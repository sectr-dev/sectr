//! [`GcpPlatform`] — the model-(b) [`RunnerPlatform`]: the BUILD
//! half (Cloud Build → Artifact Registry → authoritative manifest import →
//! ensure Cloud Run Job). The Cloud Run Jobs execution path is in
//! `gcp_run.rs`.
//!
//! # The build
//!
//! The deploy path uploads the app tarball to GCS (`BuildRequest.artifact_uri`
//! is a `gs:` URI — the gcs↔gcp pairing is enforced in `config.rs`), then
//! calls `build()`. This impl submits ONE Cloud Build build:
//!
//! 1. **assemble** — Cloud Build extracts the tarball into `/workspace` (the
//!    tarball members are the app tree, unpacked at its root by `deploy.rs`'s
//!    store contract); this step gathers them into `/workspace/app` so the
//!    shared Dockerfile's `COPY app /app` holds.
//! 2. **dockerfile** — writes the SAME generated Dockerfile [`generate_dockerfile`]
//!    produces for [`DockerPlatform`](super::DockerPlatform) (base64-embedded —
//!    no shell-escaping of Dockerfile content), so both platforms build
//!    byte-identical image content.
//! 3. **build** — `docker build` with `--cache-from` (base image first, then
//!    the deployment's previous version — best-effort: Cloud Build's docker
//!    layer cache is EPHEMERAL per builder VM, so a cache hit is a bonus, not
//!    a guarantee; the plan accepts first-build-after-idle at 2–5 min).
//! 4. **manifest** — runs the built image with `SECTR_MANIFEST=1` +
//!    `SECTR_MANIFEST_OUT=/workspace/manifest.json` (Cloud Build steps share
//!    `/workspace`) and records `uses_uv` (the same pyproject.toml guard
//!    `DockerPlatform` applies to pick `uv run --no-sync` — a remote builder
//!    must REPORT it, the server never sees the unpacked tree).
//!
//! The `Build.images` field performs the automatic Artifact Registry push of
//! `sectr-app-<deployment-id>:v<version>` ,
//! and the `Build.artifacts.objects` field uploads the results to the
//! deterministic `gs://<bucket>/builds/<deployment-id>/v<version>/` layout
//! (R3): `manifest.json` (the raw `sectr.inspect` output — the server-side
//! manifest composition stays in `deploy.rs`, cloud-neutral) plus `uses_uv`.
//! After the build finishes, this impl fetches both via the EXISTING
//! [`ArtifactStore`] (the same `GcsArtifactStore` the deploy path uses — no
//! second GCS client) and ensures the Cloud Run Job exists (step 4: template
//! = AR image + sidecar cmd + static ingest env; job lifecycle =
//! deployment lifecycle).
//!
//! # Auth & IAM
//!
//! - The SERVER authenticates via ADC / `GOOGLE_APPLICATION_CREDENTIALS`
//!   (the sectr-server SA key on the VPS) and needs
//!   `roles/cloudbuild.builds.editor` (builds.create/get) + the
//!   `iam.serviceAccountUser` binding on the build SA — both granted.
//! - The build RUNS AS `SECTR_GCP_BUILD_SA_EMAIL` (the `Build.service_account`
//!   field is the impersonation mechanism — no token machinery): builder SA
//!   has AR writer + bucket objectAdmin + log writer, all granted.
//! - **Known infra gap (open for the live-verification handoff)**: iam.tf
//!   grants sectr-server NO Cloud Run role, so `ensure_job` will 403 until a
//!   `roles/run.admin` (or jobs-scoped equivalent) binding for sectr-server
//!   is added (G2's runner-side infra is the natural home). Likewise raw
//!   build logs live in Cloud Logging (sectr-server has no logging.viewer),
//!   so a FAILED build's `logs_tail` is composed from the build resource's
//!   `failure_info` + per-step statuses + the console `log_url` — not the
//!   raw step output. Setting `logs_bucket` to our GCS bucket would capture
//!   raw logs but requires a storage grant to the Cloud Build service agent
//!   that iam.tf deliberately does not include.
//!
//! # Crate note (deviation from the original task text)
//!
//! Build creation uses **`google-cloud-build-v1`**, not `-v2`: the v2 crate
//! (1.13) only generates the RepositoryManager (dev-connect) API surface and
//! contains no `Build` model / `create_build` at all. The v1 client is the
//! official Cloud Build API (same family, same ADC stack) and expresses the
//! plan's Build shape (steps / images / service_account / source / artifacts)
//! verbatim.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::Value;
use uuid::Uuid;

use google_cloud_build_v1::model as cb;
use google_cloud_gax::error::rpc::Code as GaxCode;
use google_cloud_lro::Poller as _;
use google_cloud_run_v2::model as run;
use google_cloud_wkt as wkt;

use sectr_common::drivers::ArtifactStore;
use sectr_common::registry::Registry;

use crate::config::ServerConfig;
use crate::platform::docker::generate_dockerfile;
use crate::platform::{BuildError, BuildOutput, BuildRequest};

/// Cloud Build default timeout is 10 min; `uv sync` on a cold cache can
/// exceed it — cap the build at 30 min (the poll gives up shortly after).
const BUILD_TIMEOUT_SECS: i64 = 1800;
/// Hard ceiling on waiting for the create-build operation (build timeout +
/// queueing slack).
const BUILD_POLL_TIMEOUT: Duration = Duration::from_secs(BUILD_TIMEOUT_SECS as u64 + 600);
/// Hard ceiling on the ensure-job LRO (job creation is fast; slack is for
/// retry backoff).
const JOB_POLL_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Deterministic naming (R3/R4 contract anchors — pure, unit-tested)
// ---------------------------------------------------------------------------

/// The Artifact Registry image ref a deployment builds to (R4):
/// `<region>-docker.pkg.dev/<project>/<repo>/sectr-app-<deployment-id>:v<version>`.
/// Also the provisional `build_ref` (survives a server crash; a remote
/// builder re-check can find the half-built image by it).
fn ar_image_ref(
  project: &str,
  region: &str,
  repo: &str,
  deployment_id: Uuid,
  version: i64,
) -> String {
  format!("{region}-docker.pkg.dev/{project}/{repo}/sectr-app-{deployment_id}:v{version}")
}

/// The Cloud Run Job name for a deployment (one job per
/// deployment, job lifecycle = deployment lifecycle). Matches Cloud Run's
/// naming rules (lowercase alphanumerics + hyphens, ≤ 63 chars — a UUID is
/// 36, so we fit).
pub(crate) fn job_id(deployment_id: Uuid) -> String {
  format!("{TEMPLATE_NAME_PREFIX}{deployment_id}")
}

/// The deterministic resource-name prefix shared by every backend whose
/// resource identity is per-deployment: the Cloud Run job (`job_id`) and
/// the E2B template (`e2b::template_id`) — one naming discipline, two
/// providers.
pub(crate) const TEMPLATE_NAME_PREFIX: &str = "sectr-app-";

/// Deterministic gs:// layout for the build's results (R3).
fn builds_prefix(gcs_bucket: &str, deployment_id: Uuid, version: i64) -> String {
  format!("gs://{gcs_bucket}/builds/{deployment_id}/v{version}")
}

fn manifest_uri(gcs_bucket: &str, deployment_id: Uuid, version: i64) -> String {
  format!(
    "{}/manifest.json",
    builds_prefix(gcs_bucket, deployment_id, version)
  )
}

fn uses_uv_uri(gcs_bucket: &str, deployment_id: Uuid, version: i64) -> String {
  format!(
    "{}/uses_uv",
    builds_prefix(gcs_bucket, deployment_id, version)
  )
}

/// Split a `gs://<bucket>/<key>` URI (the tarball's artifact URI). Same
/// segment discipline as [`GcsArtifactStore`](sectr_common::drivers)'s own
/// parser — the two gs: readers must agree on what a valid key is.
fn parse_gs_uri(uri: &str) -> Result<(String, String), BuildError> {
  let rest = uri
    .strip_prefix("gs://")
    .ok_or_else(|| BuildError::bare(format!("not a gs: artifact uri: {uri:?}")))?;
  let (bucket, object) = rest
    .split_once('/')
    .ok_or_else(|| BuildError::bare(format!("gs: uri without an object: {uri:?}")))?;
  if bucket.is_empty() || object.is_empty() {
    return Err(BuildError::bare(format!("malformed gs: uri: {uri:?}")));
  }
  if object
    .split('/')
    .any(|seg| seg.is_empty() || seg == "." || seg == "..")
  {
    return Err(BuildError::bare(format!(
      "malformed gs: object path: {uri:?}"
    )));
  }
  Ok((bucket.to_owned(), object.to_owned()))
}

// ---------------------------------------------------------------------------
// Build-config generation (pure — the unit-tested shape)
// ---------------------------------------------------------------------------

/// Step 1: gather the extracted tarball's members into `/workspace/app`.
/// Cloud Build unpacks the source archive at `/workspace` root; the shared
/// Dockerfile expects the app at `app/`.
const ASSEMBLE_SCRIPT: &str = "mkdir -p /workspace/app && \
     find /workspace -mindepth 1 -maxdepth 1 ! -name app -exec mv -t /workspace/app {} +";

/// Step 4: the authoritative manifest import + `uses_uv` recording — the
/// SAME interpreter discipline `DockerPlatform` uses (`uv run --no-sync`
/// when the app ships a pyproject.toml, else system python3). `exec` keeps
/// the import's exit code as the step's.
const MANIFEST_SCRIPT: &str = "\
if [ -f pyproject.toml ]; then
  echo true > /workspace/uses_uv
  exec uv run --no-sync python3 -m sectr.inspect
else
  echo false > /workspace/uses_uv
  exec python3 -m sectr.inspect
fi";

/// Step 2 args: write the generated Dockerfile (base64-embedded to keep the
/// Dockerfile content out of shell-quoting reach).
fn dockerfile_write_args(dockerfile: &str) -> Vec<String> {
  let b64 = base64::engine::general_purpose::STANDARD.encode(dockerfile);
  vec![
    "-c".to_owned(),
    format!("printf '%s' '{b64}' | base64 -d > /workspace/Dockerfile"),
  ]
}

/// Step 3 args: the docker build. Cache order matters (checked by test): the
/// base image FIRST (its python/uv/sdk layers are the bulk), then the
/// deployment's previous version (best-effort; may not exist).
fn docker_build_args(image_ref: &str, base_image: &str, previous_ref: Option<&str>) -> Vec<String> {
  let mut args = vec![
    "build".to_owned(),
    // Cloud Build docker builds need the explicit named network for
    // outbound traffic (uv sync downloads deps).
    "--network=cloudbuild".to_owned(),
    // Structural arch pin (R4): Cloud Build builder VMs are amd64
    // today, but the contract must not depend on that default — an
    // arm64 image dies with 'exec format error' on Cloud Run
    // (live-verified 2026-09-16 via a locally-built arm64 base).
    "--platform=linux/amd64".to_owned(),
    "--cache-from".to_owned(),
    base_image.to_owned(),
  ];
  if let Some(prev) = previous_ref {
    args.push("--cache-from".to_owned());
    args.push(prev.to_owned());
  }
  args.extend(["-t".to_owned(), image_ref.to_owned()]);
  // The build context: /workspace (the Dockerfile + assembled app live
  // there). Cloud Build steps run in /workspace, so `.` is the context.
  // (Live-verification finding, 2026-09-16: without it docker reads
  // '-t <image>' as the missing PATH argument — 'requires exactly 1
  // argument'.)
  args.push(".".to_owned());
  args
}

/// The full Cloud Build config: steps 1–4 above, `images` = the automatic AR
/// push, `artifacts` = the deterministic result upload, `service_account` =
/// the least-privilege build identity.
fn build_config(
  source: cb::StorageSource,
  steps: Vec<cb::BuildStep>,
  images: Vec<String>,
  artifacts: cb::Artifacts,
  service_account: &str,
) -> cb::Build {
  cb::Build::default()
    .set_source(cb::Source::default().set_storage_source(source))
    .set_steps(steps)
    .set_images(images)
    .set_artifacts(artifacts)
    .set_service_account(service_account.to_owned())
    // Builds running AS a custom service account must not use the
    // default LEGACY logging (live-verification finding, 2026-09-16:
    // INVALID_ARGUMENT without one of logs_bucket /
    // REGIONAL_USER_OWNED_BUCKET / CLOUD_LOGGING_ONLY). We have no
    // logging.viewer for the server, and the module doc deliberately
    // defers raw-log access — CLOUD_LOGGING_ONLY needs zero extra IAM,
    // and failure_report surfaces failure_info + step ids + log_url.
    .set_options(
      cb::BuildOptions::default().set_logging(cb::build_options::LoggingMode::CloudLoggingOnly),
    )
    .set_timeout(wkt::Duration::clamp(BUILD_TIMEOUT_SECS, 0))
}

/// Assemble the four build steps for a deployment. Pure — unit-tested.
fn build_steps(
  base_image: &str,
  image_ref: &str,
  entry: &str,
  previous_ref: Option<&str>,
) -> Vec<cb::BuildStep> {
  let docker_builder = "gcr.io/cloud-builders/docker".to_owned();
  let dockerfile = generate_dockerfile(base_image);
  vec![
    // 1. assemble: extracted tarball members → /workspace/app
    cb::BuildStep::default()
      .set_id("assemble")
      .set_name(docker_builder.clone())
      .set_entrypoint("bash")
      .set_args(vec!["-c".to_owned(), ASSEMBLE_SCRIPT.to_owned()]),
    // 2. dockerfile: the shared generator's output
    cb::BuildStep::default()
      .set_id("dockerfile")
      .set_name(docker_builder.clone())
      .set_entrypoint("bash")
      .set_args(dockerfile_write_args(&dockerfile)),
    // 3. build (deps bake at build time — same shape as model (a))
    cb::BuildStep::default()
      .set_id("build")
      .set_name(docker_builder)
      .set_args(docker_build_args(image_ref, base_image, previous_ref)),
    // 4. manifest import: run the built image (its ENTRYPOINT is the
    //    sidecar — overridden by `entrypoint: sh`), import in /app,
    //    manifest lands in the shared /workspace.
    cb::BuildStep::default()
      .set_id("manifest")
      .set_name(image_ref.to_owned())
      .set_entrypoint("sh")
      .set_dir("/app")
      .set_env(vec![
        "SECTR_MANIFEST=1".to_owned(),
        format!("SECTR_ENTRY={entry}"),
        "SECTR_MANIFEST_OUT=/workspace/manifest.json".to_owned(),
      ])
      .set_args(vec!["-c".to_owned(), MANIFEST_SCRIPT.to_owned()]),
  ]
}

/// The results upload: `manifest.json` + `uses_uv` → the deterministic
/// prefix. Uses the Build's `artifacts` field (cleaner than a gsutil step —
/// no extra image pull, and it runs AFTER all steps succeed).
fn build_artifacts(result_prefix: &str) -> cb::Artifacts {
  cb::Artifacts::default().set_objects(
    cb::artifacts::ArtifactObjects::default()
      .set_location(format!("{result_prefix}/"))
      .set_paths(vec!["manifest.json".to_owned(), "uses_uv".to_owned()]),
  )
}

/// The Cloud Run Job template: the template IS the deployment —
/// AR image, sidecar cmd (overriding the base image's CMD via `args`, same
/// discipline as [`DockerPlatform`]), and the STATIC env (`SECTR_INGEST_URL`;
/// the per-turn token rides execution overrides — secrets must not
/// ride the job template). NO `.name`: on CreateJobRequest the name must be
/// EMPTY in the body — the identity is the `job_id` path parameter (live-
/// verification finding, 2026-09-16: 'job.name must be empty').
///
/// Two NON-DEFAULTS (reviewed against the console-rendered job YAML,
/// 2026-09-16 — Cloud Run's defaults violate our turn semantics):
/// - `maxRetries: 0` — a retried execution gets a FRESH assignment (same
///   transcript) and re-emits events with new UUIDs (ingest dedup is per
///   event UUID): duplicated journal rows and REPEATED TOOL SIDE EFFECTS.
///   A failed turn must surface through the journal, not silently re-run.
/// - `timeoutSeconds: 86400` (the max) — the default 600s would kill long
///   agent turns mid-work. Liveness is governed by the ingest heartbeat +
///   the reconciler TTL, not the task timeout (same as model (a), which
///   has no container timeout).
///
/// Pure (unit-tested).
fn job_spec(
  image_ref: &str,
  sidecar_cmd: &[String],
  ingest_url: &str,
  runner_sa_email: &str,
) -> run::Job {
  run::Job::default().set_template(
    run::ExecutionTemplate::default().set_template(
      run::TaskTemplate::default()
        .set_service_account(runner_sa_email.to_owned())
        // See the fn doc: maxRetries 0 (no silent turn re-runs —
        // repeated side effects) and the max task timeout
        // (liveness = heartbeat + reconciler, not the timeout).
        .set_max_retries(0)
        .set_timeout(wkt::Duration::clamp(86400, 0))
        .set_containers(vec![
          run::Container::default()
            .set_image(image_ref.to_owned())
            .set_args(sidecar_cmd.to_vec())
            .set_env(vec![
              run::EnvVar::default()
                .set_name("SECTR_INGEST_URL")
                .set_value(ingest_url),
            ]),
        ]),
    ),
  )
}

/// Compose the failure message + log tail from the finished build resource.
/// Without Cloud Logging read access (iam.tf grants none), this is what we
/// can honestly report: status, the failing steps, `failure_info.detail`,
/// and the console log URL. Pure — unit-tested.
fn failure_report(build: &cb::Build) -> (String, String) {
  let detail = build
    .failure_info
    .as_ref()
    .map(|f| f.detail.clone())
    .unwrap_or_default();
  let failed_steps: Vec<String> = build
    .steps
    .iter()
    .filter(|s| s.status == cb::build::Status::Failure)
    .map(|s| {
      let id = if s.id.is_empty() { &s.name } else { &s.id };
      format!("step '{id}' failed")
    })
    .collect();
  let log_url = if build.log_url.is_empty() {
    String::new()
  } else {
    format!("logs: {}", build.log_url)
  };
  let message = format!(
    "Cloud Build {} finished with status {}{}",
    build.id,
    build.status,
    if detail.is_empty() {
      String::new()
    } else {
      format!(": {detail}")
    }
  );
  let mut tail_lines = failed_steps;
  if !detail.is_empty() {
    tail_lines.push(detail);
  }
  if !log_url.is_empty() {
    tail_lines.push(log_url);
  }
  (message, tail_lines.join("\n"))
}

fn is_not_found(e: &google_cloud_gax::error::Error) -> bool {
  e.status().is_some_and(|s| s.code == GaxCode::NotFound)
}

// ---------------------------------------------------------------------------
// The platform
// ---------------------------------------------------------------------------

/// The model-(b) platform. Holds the config it needs (no ServerConfig clone —
/// the consumed keys are explicit), the artifact store (manifest result
/// fetch), and the two GCP clients. Client construction uses ADC
/// (`GOOGLE_APPLICATION_CREDENTIALS` / workload identity) — no key material
/// in code.
pub struct GcpPlatform {
  pub(crate) project_id: String,
  pub(crate) region: String,
  ar_repo: String,
  build_sa_email: String,
  /// SA the ensured jobs run AS (least-privilege runtime identity — AR
  /// reader only; the runner dials out and needs nothing else). `None`
  /// on the build-only constructor (E2B backend — it never ensures jobs;
  /// `ensure_job` fails with a clear error rather than a panic if ever
  /// reached, which it must not be).
  runner_sa_email: Option<String>,
  gcs_bucket: String,
  /// Required at build time (same failure point as [`DockerPlatform`]):
  /// the runner base image — must live in AR (Cloud Build
  /// cannot see a VPS-local tag).
  runner_base_image: Option<String>,
  pub(crate) runner_server_url: String,
  runner_sidecar_cmd: Vec<String>,
  store: Arc<dyn ArtifactStore>,
  /// Manifest resolution at spawn (self-healing ensure: the image ref
  /// lives in the composed manifest — same pattern as DockerPlatform).
  pub(crate) registry: Arc<dyn Registry>,
  builder: google_cloud_build_v1::client::CloudBuild,
  pub(crate) jobs: google_cloud_run_v2::client::Jobs,
  pub(crate) executions: google_cloud_run_v2::client::Executions,
}

impl GcpPlatform {
  /// Compose from the server config. The caller must have selected
  /// `PlatformKind::Gcp` (the config validation already fail-fasts on the
  /// pairing + required keys).
  pub async fn from_config(
    config: &ServerConfig,
    store: Arc<dyn ArtifactStore>,
    registry: Arc<dyn Registry>,
  ) -> anyhow::Result<Self> {
    Self::from_config_inner(config, store, registry, true).await
  }

  /// Build-half-only constructor: the Cloud Build half
  /// WITHOUT the Cloud Run runtime identity — the E2B backend never
  /// ensures jobs, so it never needs `SECTR_GCP_RUNNER_SA_EMAIL` (the
  /// config validation for e2b does not require it either). The Jobs/
  /// Executions clients are still constructed (cheap, ADC) but never
  /// called on this platform; a runner-SA-less `ensure_job` fails with a
  /// clear error if ever reached (it must not be).
  pub async fn build_only(
    config: &ServerConfig,
    store: Arc<dyn ArtifactStore>,
    registry: Arc<dyn Registry>,
  ) -> anyhow::Result<Self> {
    Self::from_config_inner(config, store, registry, false).await
  }

  async fn from_config_inner(
    config: &ServerConfig,
    store: Arc<dyn ArtifactStore>,
    registry: Arc<dyn Registry>,
    require_runner_sa: bool,
  ) -> anyhow::Result<Self> {
    let project_id = config
      .gcp_project_id
      .clone()
      .ok_or_else(|| anyhow::anyhow!("SECTR_GCP_PROJECT_ID is required when SECTR_PLATFORM=gcp"))?;
    let region = config
      .gcp_region
      .clone()
      .ok_or_else(|| anyhow::anyhow!("SECTR_GCP_REGION is required when SECTR_PLATFORM=gcp"))?;
    let build_sa_email = config.gcp_build_sa_email.clone().ok_or_else(|| {
      anyhow::anyhow!("SECTR_GCP_BUILD_SA_EMAIL is required when SECTR_PLATFORM=gcp")
    })?;
    let runner_sa_email = if require_runner_sa {
      Some(config.gcp_runner_sa_email.clone().ok_or_else(|| {
        anyhow::anyhow!("SECTR_GCP_RUNNER_SA_EMAIL is required when SECTR_PLATFORM=gcp")
      })?)
    } else {
      None
    };
    let gcs_bucket = config
      .gcs_bucket
      .clone()
      .ok_or_else(|| anyhow::anyhow!("SECTR_GCS_BUCKET is required when SECTR_PLATFORM=gcp"))?;
    tracing::info!(project = %project_id, region = %region, "artifact store: Cloud Build + Cloud Run clients (ADC)");
    // REGIONAL endpoint: builds must run in our region (same-region AR
    // pull + Cloud Run). The default global endpoint
    // (cloudbuild.googleapis.com) rejects a regional parent with
    // INVALID_ARGUMENT 'location ... is unsupported, service location
    // is global' — regional submissions go through
    // <region>-cloudbuild.googleapis.com. (Live-verification finding,
    // 2026-09-16.) The Cloud Run Admin API has no regional endpoints —
    // its default global endpoint is correct for Jobs.
    let builder = google_cloud_build_v1::client::CloudBuild::builder()
      .with_endpoint(format!("https://{region}-cloudbuild.googleapis.com"))
      .build()
      .await
      .map_err(|e| anyhow::anyhow!("connect to Cloud Build: {e}"))?;
    let jobs = google_cloud_run_v2::client::Jobs::builder()
      .build()
      .await
      .map_err(|e| anyhow::anyhow!("connect to Cloud Run: {e}"))?;
    let executions = google_cloud_run_v2::client::Executions::builder()
      .build()
      .await
      .map_err(|e| anyhow::anyhow!("connect to Cloud Run Executions: {e}"))?;
    Ok(GcpPlatform {
      project_id,
      region,
      ar_repo: config.gcp_ar_repo.clone(),
      build_sa_email,
      runner_sa_email,
      gcs_bucket,
      runner_base_image: config.runner_base_image.clone(),
      runner_server_url: config.runner_server_url.clone(),
      runner_sidecar_cmd: config.runner_sidecar_cmd.clone(),
      store,
      registry,
      builder,
      jobs,
      executions,
    })
  }

  fn ar_image_ref(&self, deployment_id: Uuid, version: i64) -> String {
    ar_image_ref(
      &self.project_id,
      &self.region,
      &self.ar_repo,
      deployment_id,
      version,
    )
  }

  /// Ensure the deployment's Cloud Run Job exists (plan step 4): get →
  /// create-if-absent. A hit means the job is already this deployment's
  /// (job lifecycle = deployment lifecycle — the name embeds the
  /// deployment id, so an existing job is always the right one).
  pub(crate) async fn ensure_job(
    &self,
    deployment_id: Uuid,
    image_ref: &str,
  ) -> Result<(), BuildError> {
    let parent = format!("projects/{}/locations/{}", self.project_id, self.region);
    let id = job_id(deployment_id);
    let job_name = format!("{parent}/jobs/{id}");
    match self.jobs.get_job().set_name(&job_name).send().await {
      Ok(_) => {
        tracing::debug!(job = %job_name, "Cloud Run job already exists");
        Ok(())
      }
      Err(e) if is_not_found(&e) => {
        tracing::info!(job = %job_name, image = %image_ref, "creating Cloud Run job");
        let runner_sa = self.runner_sa_email.as_deref().ok_or_else(|| {
          BuildError::bare(
            "SECTR_GCP_RUNNER_SA_EMAIL is not configured — the Cloud Run job \
                         needs its runtime identity (this platform build-only; ensure_job \
                         must not be reached)",
          )
        })?;
        let job = job_spec(
          image_ref,
          &self.runner_sidecar_cmd,
          &self.runner_server_url,
          runner_sa,
        );
        tokio::time::timeout(
          JOB_POLL_TIMEOUT,
          self
            .jobs
            .create_job()
            .set_parent(parent)
            .set_job_id(id)
            .set_job(job)
            .poller()
            .until_done(),
        )
        .await
        .map_err(|_| BuildError::bare("Cloud Run job creation did not finish within 300s"))?
        .map_err(|e| BuildError::bare(format!("create Cloud Run job {job_name}: {e}")))?;
        tracing::info!(job = %job_name, "Cloud Run job created");
        Ok(())
      }
      Err(e) => Err(BuildError::bare(format!(
        "get Cloud Run job {job_name}: {e}"
      ))),
    }
  }
}

impl GcpPlatform {
  /// Model (b): the image lives in Artifact Registry under the R4 ref —
  /// that ref is the provisional build_ref. (The `RunnerPlatform` impl
  /// lives in `gcp_run.rs` — Rust allows one impl block per trait — and
  /// delegates here: this file is the build half; that one owns the
  /// execution half.)
  pub(crate) fn provisional_build_ref_impl(&self, deployment_id: Uuid, version: i64) -> String {
    self.ar_image_ref(deployment_id, version)
  }

  pub(crate) async fn build_impl(&self, req: BuildRequest) -> Result<BuildOutput, BuildError> {
    let deployment_id = req.deployment_id;
    let output = self.cloud_build_impl(req).await?;
    // Step 4 of the plan's spec: the job IS the deployment — a failed
    // ensure fails the deploy BEFORE routing takes. (The E2B backend
    // reuses [`Self::cloud_build_impl`] verbatim and swaps this step
    // for the E2B template build.)
    self.ensure_job(deployment_id, &output.image_ref).await?;
    Ok(output)
  }

  /// The Cloud Build half WITHOUT the platform's deployment-resource step
  ///: image build → AR push → authoritative manifest →
  /// results fetch. The gcp platform adds `ensure_job` on top (above);
  /// the E2B platform adds the E2B template build instead.
  pub(crate) async fn cloud_build_impl(
    &self,
    req: BuildRequest,
  ) -> Result<BuildOutput, BuildError> {
    let base_image = self.runner_base_image.as_deref().ok_or_else(|| {
      BuildError::bare("SECTR_RUNNER_BASE_IMAGE is not configured — no base image to build on")
    })?;
    let image_ref = self.ar_image_ref(req.deployment_id, req.version);
    let (src_bucket, src_object) = parse_gs_uri(&req.artifact_uri)?;
    if src_bucket != self.gcs_bucket {
      return Err(BuildError::bare(format!(
        "artifact {src_bucket:?} is not the configured GCS bucket {:?} — the build SA \
                 only has object access there",
        self.gcs_bucket
      )));
    }

    // --cache-from: base image, then the PREVIOUS deployment's image
    // ref (platform-named, carried on the request — image refs are
    // deployment-scoped, so this cannot be derived from `version - 1`).
    // Best-effort: Cloud Build's docker cache is ephemeral per builder.
    let previous_ref = req.previous_build_ref.as_deref();

    // The serviceAccount field wants the FULL resource name (bare
    // emails fail with 'Failed to parse resource name' — live-verification
    // finding, 2026-09-16); the config value stays an email.
    let build_sa = format!(
      "projects/{}/serviceAccounts/{}",
      self.project_id, self.build_sa_email
    );
    let build = build_config(
      cb::StorageSource::default()
        .set_bucket(src_bucket)
        .set_object(src_object),
      build_steps(base_image, &image_ref, &req.entry, previous_ref),
      vec![image_ref.clone()],
      build_artifacts(&builds_prefix(
        &self.gcs_bucket,
        req.deployment_id,
        req.version,
      )),
      &build_sa,
    );

    // Submit + poll to completion. NOT the LRO poller: a FAILED build
    // arrives as an operation ERROR ('Build failed; check build logs'),
    // which would discard the Build resource that failure_report needs
    // (live-verification finding, 2026-09-16). Instead: submit once,
    // take the build id from the operation's metadata, and poll
    // get_build — which returns the full resource through terminal
    // states, failed or not.
    let op = self
      .builder
      .create_build()
      .set_parent(format!(
        "projects/{}/locations/{}",
        self.project_id, self.region
      ))
      .set_project_id(self.project_id.clone())
      .set_build(build)
      .send()
      .await
      .map_err(|e| BuildError::bare(format!("submit Cloud Build: {e}")))?;
    let build_id = op
      .metadata
      .as_ref()
      .and_then(|m| m.to_msg::<cb::BuildOperationMetadata>().ok())
      .and_then(|m| m.build)
      .map(|b| b.id)
      .filter(|id| !id.is_empty())
      .ok_or_else(|| {
        BuildError::bare(format!(
          "submit Cloud Build returned no build id (operation {})",
          op.name
        ))
      })?;
    let deadline = tokio::time::Instant::now() + BUILD_POLL_TIMEOUT;
    let finished: cb::Build = loop {
      let b = self
        .builder
        .get_build()
        .set_project_id(self.project_id.clone())
        .set_id(&build_id)
        .send()
        .await
        .map_err(|e| BuildError::bare(format!("poll build {build_id}: {e}")))?;
      if matches!(
        b.status,
        cb::build::Status::Success
          | cb::build::Status::Failure
          | cb::build::Status::InternalError
          | cb::build::Status::Timeout
          | cb::build::Status::Cancelled
          | cb::build::Status::Expired
      ) {
        break b;
      }
      if tokio::time::Instant::now() >= deadline {
        return Err(BuildError::bare(format!(
          "Cloud Build {build_id} did not finish within {}s",
          BUILD_POLL_TIMEOUT.as_secs()
        )));
      }
      tokio::time::sleep(Duration::from_secs(5)).await;
    };
    tracing::info!(build = %finished.id, status = %finished.status, "Cloud Build finished");

    if finished.status != cb::build::Status::Success {
      let (message, logs_tail) = failure_report(&finished);
      return Err(BuildError { message, logs_tail });
    }

    // Fetch the build's results from GCS via the EXISTING artifact store.
    let manifest_bytes = self
      .store
      .get(&manifest_uri(
        &self.gcs_bucket,
        req.deployment_id,
        req.version,
      ))
      .await
      .map_err(|e| BuildError::bare(format!("manifest result fetch: {e}")))?;
    let imported: Value = serde_json::from_slice(&manifest_bytes)
      .map_err(|e| BuildError::bare(format!("manifest result is not JSON: {e}")))?;
    let uses_uv = self
      .store
      .get(&uses_uv_uri(
        &self.gcs_bucket,
        req.deployment_id,
        req.version,
      ))
      .await
      .map_err(|e| BuildError::bare(format!("uses_uv result fetch: {e}")))?;
    let uses_uv = matches!(std::str::from_utf8(&uses_uv).map(str::trim), Ok("true"));

    Ok(BuildOutput {
      image_ref,
      imported_manifest: imported,
      uses_uv,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const PROJECT: &str = "example-project-123456";
  const REGION: &str = "us-east4";
  const REPO: &str = "sectr-runners";
  const BUCKET: &str = "sectr-artifacts-example-project-123456";
  const BASE: &str =
    "us-east4-docker.pkg.dev/example-project-123456/sectr-runners/sectr-runner-base:v1";
  const SA: &str = "sectr-builder@example-project-123456.iam.gserviceaccount.com";

  fn deployment() -> Uuid {
    // A fixed id: the assertions below pin exact strings.
    Uuid::parse_str("01912345-6789-7abc-def0-123456789abc").unwrap()
  }

  fn image(deployment: Uuid, version: i64) -> String {
    ar_image_ref(PROJECT, REGION, REPO, deployment, version)
  }

  fn full_build(deployment: Uuid, version: i64) -> cb::Build {
    let image_ref = image(deployment, version);
    let previous = (version > 1).then(|| image(deployment, version - 1));
    let steps = build_steps(BASE, &image_ref, "agent:agent", previous.as_deref());
    build_config(
      cb::StorageSource::default()
        .set_bucket(BUCKET)
        .set_object("org/app/1.tar.gz"),
      steps,
      vec![image_ref.clone()],
      build_artifacts(&builds_prefix(BUCKET, deployment, version)),
      &format!("projects/{PROJECT}/serviceAccounts/{SA}"),
    )
  }

  // -- naming / layout contract (R3/R4 anchors) --------------------------

  #[test]
  fn ar_tag_and_gs_layout_are_deterministic() {
    let d = deployment();
    assert_eq!(
      image(d, 3),
      format!("{REGION}-docker.pkg.dev/{PROJECT}/{REPO}/sectr-app-{d}:v3")
    );
    assert_eq!(
      image(d, 3),
      image(d, 3),
      "the ref must be a pure function of its inputs"
    );
    assert_eq!(
      manifest_uri(BUCKET, d, 3),
      format!("gs://{BUCKET}/builds/{d}/v3/manifest.json")
    );
    assert_eq!(
      uses_uv_uri(BUCKET, d, 3),
      format!("gs://{BUCKET}/builds/{d}/v3/uses_uv")
    );
    assert_eq!(job_id(d), format!("sectr-app-{d}"));
    // Cloud Run job-name rules: lowercase alphanumerics + hyphens, ≤ 63.
    let id = job_id(d);
    assert!(id.len() <= 63);
    assert!(
      id.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    );
  }

  // -- build config shape ------------------------------------------------
  // NOTE on test scope: everything testable without a live GCP call is a
  // free function over plain data — GcpPlatform's wired methods
  // (submit/poll/ensure-job) need ADC-authenticated clients and are
  // verified live. `provisional_build_ref` is a
  // one-line delegation to `ar_image_ref` (covered above) and is not
  // struct-tested, which would require constructing client instances.

  #[test]
  fn build_config_shape_matches_the_plan_spec() {
    let d = deployment();
    let build = full_build(d, 1);

    // source = the tarball already in GCS.
    let source = build.source.expect("build must carry a source");
    let storage = match source.source {
      Some(cb::source::Source::StorageSource(s)) => *s,
      other => panic!("expected a storage source, got {other:?}"),
    };
    assert_eq!(storage.bucket, BUCKET);
    assert_eq!(storage.object, "org/app/1.tar.gz");

    // Build runs AS the least-privilege build SA (the impersonation
    // mechanism — sectr-server needs serviceAccountUser
    // on exactly this SA). The Build API wants the full resource name.
    assert_eq!(
      build.service_account,
      format!("projects/{PROJECT}/serviceAccounts/{SA}")
    );

    // images field = the automatic AR push of the R4 ref.
    assert_eq!(build.images, vec![image(d, 1)]);

    // logging mode: CLOUD_LOGGING_ONLY (required with a custom build SA;
    // see build_config).
    let options = build.options.expect("build options must be set");
    assert_eq!(
      options.logging,
      cb::build_options::LoggingMode::CloudLoggingOnly
    );

    // timeout is capped (Cloud Build's default 10 min is too tight for
    // a cold uv sync).
    let timeout = build.timeout.expect("build timeout must be set");
    assert_eq!(timeout.seconds(), BUILD_TIMEOUT_SECS);

    // artifacts = the deterministic result upload.
    let objects = build.artifacts.expect("artifacts must be set").objects;
    let objects = objects.expect("artifact objects must be set");
    assert_eq!(objects.location, format!("gs://{BUCKET}/builds/{d}/v1/"));
    assert_eq!(objects.paths, vec!["manifest.json", "uses_uv"]);
  }

  #[test]
  fn build_steps_are_assemble_dockerfile_build_manifest() {
    let d = deployment();
    let build = full_build(d, 1);
    assert_eq!(build.steps.len(), 4, "exactly the plan's four steps");
    let [assemble, dockerfile, docker_build, manifest] = &build.steps[..] else {
      panic!("four steps expected");
    };

    // Step 1: assemble the extracted tarball into /workspace/app.
    assert_eq!(assemble.name, "gcr.io/cloud-builders/docker");
    assert_eq!(assemble.entrypoint, "bash");
    let script = &assemble.args[1];
    assert!(script.contains("mkdir -p /workspace/app"), "{script}");
    assert!(script.contains("mv -t /workspace/app"), "{script}");

    // Step 2: the Dockerfile written to /workspace decodes to EXACTLY
    // what the shared generator produces (byte-identical image content
    // with DockerPlatform).
    assert_eq!(dockerfile.entrypoint, "bash");
    let write_cmd = &dockerfile.args[1];
    // printf '%s' '<b64>' | base64 -d … — the 4th single-quote-delimited
    // segment is the embedded dockerfile.
    let b64 = write_cmd
      .split('\'')
      .nth(3)
      .expect("dockerfile step embeds base64 in single quotes");
    let decoded = base64::engine::general_purpose::STANDARD
      .decode(b64)
      .expect("embedded dockerfile is valid base64");
    assert_eq!(
      String::from_utf8(decoded).unwrap(),
      generate_dockerfile(BASE),
      "the Cloud Build image content must match the DockerPlatform's"
    );

    // Step 3: docker build tagged with the AR ref, context = /workspace,
    // arch pinned (R4: Cloud Run is x86_64).
    assert_eq!(docker_build.name, "gcr.io/cloud-builders/docker");
    assert!(
      docker_build
        .args
        .contains(&"--platform=linux/amd64".to_owned())
    );
    assert!(docker_build.args.contains(&"-t".to_owned()));
    assert!(docker_build.args.contains(&image(d, 1)));
    assert!(
      docker_build
        .args
        .contains(&"--network=cloudbuild".to_owned())
    );
    assert_eq!(
      docker_build.args.last(),
      Some(&".".to_owned()),
      "the build context (/workspace) must be the last arg"
    );

    // Step 4: the built image runs the import with the manifest env;
    // the step runs the image by ref (no pull — it was just built).
    assert_eq!(manifest.name, image(d, 1));
    assert_eq!(
      manifest.entrypoint, "sh",
      "overrides the sidecar ENTRYPOINT"
    );
    assert_eq!(manifest.dir, "/app");
    let env: std::collections::HashMap<&str, &str> = manifest
      .env
      .iter()
      .filter_map(|e| e.split_once('='))
      .collect();
    assert_eq!(env["SECTR_MANIFEST"], "1");
    assert_eq!(env["SECTR_ENTRY"], "agent:agent");
    assert_eq!(env["SECTR_MANIFEST_OUT"], "/workspace/manifest.json");
    assert!(
      manifest.args[1].contains("pyproject.toml"),
      "the uses_uv guard must mirror DockerPlatform's"
    );
    assert!(manifest.args[1].contains("sectr.inspect"));
    assert!(manifest.args[1].contains("uv run --no-sync"));
  }

  #[test]
  fn cache_from_orders_base_then_previous_version() {
    let d = deployment();

    // First version: only the base image is a cache source (there is no
    // previous version of the deployment to point at).
    let v1 = docker_build_args(&image(d, 1), BASE, None);
    let positions: Vec<usize> = v1
      .iter()
      .enumerate()
      .filter(|(_, a)| *a == "--cache-from")
      .map(|(i, _)| i)
      .collect();
    assert_eq!(positions.len(), 1, "first version: base image only");
    assert_eq!(v1[positions[0] + 1], BASE);
  }

  #[test]
  fn cache_from_includes_previous_version_when_one_exists() {
    let d = deployment();
    let v3 = docker_build_args(&image(d, 3), BASE, Some(&image(d, 2)));
    let positions: Vec<usize> = v3
      .iter()
      .enumerate()
      .filter(|(_, a)| *a == "--cache-from")
      .map(|(i, _)| i)
      .collect();
    assert_eq!(positions.len(), 2);
    // ORDER: base image first, previous version second.
    assert_eq!(v3[positions[0] + 1], BASE);
    assert_eq!(v3[positions[1] + 1], image(d, 2));
  }

  // -- job spec (step 4 of the plan's spec) -------------------------------

  #[test]
  fn job_template_is_image_sidecar_cmd_and_static_ingest_env_only() {
    let d = deployment();
    let job = job_spec(
      &image(d, 1),
      &["/usr/local/bin/sectr-worker".to_owned()],
      "ws://172.17.0.1:8080/ingest",
      SA,
    );
    assert!(
      job.name.is_empty(),
      "CreateJobRequest.job.name must be empty — the identity is the job_id path parameter"
    );
    let template = job.template.expect("job template must be set");
    // The run-v2 model wraps the task template in an ExecutionTemplate.
    let template = template.template.expect("task template must be set");
    assert_eq!(
      template.service_account, SA,
      "jobs run AS the least-privilege runner SA — never the default compute SA"
    );
    assert_eq!(template.containers.len(), 1, "one container: the runner");
    let c = &template.containers[0];
    assert_eq!(c.image, image(d, 1));
    assert!(
      c.command.is_empty(),
      "the base image's ENTRYPOINT (the sidecar) must stay"
    );
    assert_eq!(c.args, vec!["/usr/local/bin/sectr-worker"]);
    assert_eq!(
      c.env.len(),
      1,
      "STATIC env only — the per-turn ingest token rides execution overrides"
    );
    assert_eq!(c.env[0].name, "SECTR_INGEST_URL");
    let value = match &c.env[0].values {
      Some(run::env_var::Values::Value(v)) => v.clone(),
      other => panic!("literal env value expected, got {other:?}"),
    };
    assert_eq!(value, "ws://172.17.0.1:8080/ingest");
    // Turn semantics: no silent re-runs, no default 10-min kill.
    assert_eq!(template.max_retries(), Some(&0));
    assert_eq!(template.timeout.map(|t| t.seconds()), Some(86400));
  }

  // -- failure reporting ---------------------------------------------------

  #[test]
  fn failure_report_surfaces_status_steps_and_log_url() {
    let mut build = cb::Build::default()
      .set_id("abc-123")
      .set_status(cb::build::Status::Failure)
      .set_log_url("https://console.cloud.google.com/cloud-build/builds/abc-123");
    build.failure_info = Some(
      cb::build::FailureInfo::default()
        .set_type(cb::build::failure_info::FailureType::default())
        .set_detail("Step 3 exited with code 1"),
    );
    build.steps = vec![
      cb::BuildStep::default()
        .set_name("build")
        .set_status(cb::build::Status::Success),
      cb::BuildStep::default()
        .set_name("manifest")
        .set_status(cb::build::Status::Failure),
    ];
    let (message, tail) = failure_report(&build);
    assert!(message.contains("abc-123"), "{message}");
    assert!(message.contains("FAILURE"), "{message}");
    assert!(message.contains("Step 3 exited with code 1"), "{message}");
    assert!(tail.contains("step 'manifest' failed"), "{tail}");
    assert!(tail.contains("https://console.cloud.google.com"), "{tail}");
    assert!(
      !tail.contains("'build' failed"),
      "successful steps stay out: {tail}"
    );
  }

  #[test]
  fn success_is_not_reported_as_failure() {
    let build = cb::Build::default()
      .set_id("ok")
      .set_status(cb::build::Status::Success);
    let (message, tail) = failure_report(&build);
    assert!(message.contains("SUCCESS"), "{message}");
    assert!(tail.is_empty());
  }

  // -- gs: uri parsing ------------------------------------------------------

  #[test]
  fn parse_gs_uri_accepts_tarball_uris_and_rejects_garbage() {
    let (bucket, object) = parse_gs_uri("gs://sectr-artifacts/org/app/1.tar.gz").unwrap();
    assert_eq!(bucket, "sectr-artifacts");
    assert_eq!(object, "org/app/1.tar.gz");
    assert!(
      parse_gs_uri("fs:/tmp/sectr-artifacts/x").is_err(),
      "fs: URIs cannot feed Cloud Build"
    );
    assert!(parse_gs_uri("gs://no-object").is_err());
    assert!(parse_gs_uri("gs:///no-bucket").is_err());
    assert!(parse_gs_uri("gs://b/../escape.tar.gz").is_err());
    assert!(parse_gs_uri("gs://b/org//double-slash.tar.gz").is_err());
  }
}
