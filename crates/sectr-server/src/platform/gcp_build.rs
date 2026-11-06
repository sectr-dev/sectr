//! [`GcpPlatform`] — the model-(b) [`RunnerPlatform`]: the BUILD
//! half (Cloud Build → Artifact Registry → authoritative manifest import →
//! ensure Cloud Run Job). The Cloud Run Jobs execution path is in
//! `gcp_run.rs`.
//!
//! # The build
//!
//! The deploy path uploads the app tarball to GCS (`BuildRequest.artifact_uri`
//! is a `gs:` URI — the gcs↔gcp pairing is enforced in `config.rs`), then
//! calls `build()`. This impl runs TWO Cloud Builds, because the app's own
//! code runs during a build (dependency install, the manifest import), and
//! every build step can use the build identity's credentials.
//!
//! **The user-code build** holds nothing worth taking. It runs AS
//! `SECTR_GCP_BUILD_SA_EMAIL`, which can only write logs, and sets no
//! `source`, `images` or `artifacts`. Its files move only through V4 URLs the
//! server signs, each for one method on one object:
//!
//! 1. **fetch** — downloads the app tarball by a signed GET and unpacks it
//!    into `/workspace/app`, where the shared Dockerfile's `COPY app /app`
//!    expects it.
//! 2. **dockerfile** — writes the SAME generated Dockerfile [`generate_dockerfile`]
//!    produces for [`DockerPlatform`](super::DockerPlatform) (base64-embedded —
//!    no shell-escaping of Dockerfile content), so both platforms build
//!    byte-identical image content.
//! 3. **build** — `docker build` on the public base image (GHCR, pulled with
//!    no credentials), caching from it. Not from the deployment's previous
//!    image: that would need registry read, which covers every org's images.
//! 4. **manifest** — runs the built image with `SECTR_MANIFEST=1` +
//!    `SECTR_MANIFEST_OUT=/workspace/manifest.json` (Cloud Build steps share
//!    `/workspace`) and records `uses_uv` (the same pyproject.toml guard
//!    `DockerPlatform` applies to pick `uv run --no-sync` — a remote builder
//!    must REPORT it, the server never sees the unpacked tree).
//! 5. **save** — `docker save` writes the image to `/workspace/image.tar`.
//! 6. **upload** — PUTs `image.tar`, `manifest.json` and `uses_uv` to their
//!    signed URLs.
//!
//! We assume the app's code controls this whole build, so no step in it is a
//! boundary, and everything it uploads is hostile. The boundaries are where
//! it lands. Each upload URL is create-only and size-capped (both signed
//! headers, enforced by Cloud Storage), and every attempt writes under a
//! fresh prefix, `builds/<deployment-id>/v<version>/<attempt>/`. The server
//! reads `manifest.json` and `uses_uv` back through the [`ArtifactStore`]
//! with capped reads (the raw `sectr.inspect` output — the server-side
//! manifest composition stays in `deploy.rs`, cloud-neutral).
//!
//! **The push build** runs no app code. It runs AS `SECTR_GCP_PUSH_SA_EMAIL`,
//! the one identity that can write images, on a fresh VM (Cloud Build gives
//! every build its own). It downloads `image.tar` by a signed GET and runs
//! `crane push` (gcrane, pinned by digest) to
//! `sectr-app-<deployment-id>:v<version>`. Not `docker load`: crane uploads
//! the layers without unpacking them, ignores any names the tar declares, and
//! refuses a tar holding more than one image. Then the server deletes
//! `image.tar` (a bucket lifecycle rule catches any it misses). The gcp
//! platform then ensures the Cloud Run Job (its template is the AR image,
//! the sidecar cmd and the static ingest env; job lifecycle = deployment
//! lifecycle); the E2B platform builds its template instead.
//!
//! # Auth & IAM
//!
//! - The SERVER authenticates via ADC / `GOOGLE_APPLICATION_CREDENTIALS`
//!   (the sectr-server SA key on the VPS). It needs
//!   `roles/cloudbuild.builds.editor`, `iam.serviceAccountUser` on both build
//!   identities, and objectAdmin on the bucket, which is what its signed URLs
//!   grant. The key signs them locally, with no network call.
//! - The user-code build runs AS the build SA: `roles/logging.logWriter`
//!   only. With a URL, the app's code can read its own tarball and create
//!   its own three result files once; nothing else.
//! - The push build runs AS the push SA: writer on the `sectr-runners` repo,
//!   plus `roles/logging.logWriter` so a failed push leaves logs.
//! - The `Build.service_account` field is the impersonation mechanism (no
//!   token machinery).
//! - **Known infra gap**: iam.tf grants sectr-server NO Cloud Run role, so
//!   `ensure_job` will 403 until a `roles/run.admin` (or jobs-scoped
//!   equivalent) binding for sectr-server is added. Likewise raw build logs
//!   live in Cloud Logging (sectr-server has no logging.viewer), so a FAILED
//!   build's `logs_tail` is composed from the build resource's
//!   `failure_info` + per-step statuses + the console `log_url` — not the
//!   raw step output, which also means one org's build can't forge another's
//!   error message.
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
use google_cloud_storage::builder::storage::SignedUrlBuilder;
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
/// The push build only downloads one tar and pushes it: 20 min covers the
/// largest tar the user-code build may upload.
const PUSH_TIMEOUT_SECS: i64 = 1200;
const PUSH_POLL_TIMEOUT: Duration = Duration::from_secs(PUSH_TIMEOUT_SECS as u64 + 600);
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

/// The object prefix of one build attempt's files (R3, plus the attempt):
/// `builds/<deployment-id>/v<version>/<attempt>`. Every attempt gets a fresh
/// id, so no two builds, retries of one version included, share an object.
/// The create-only uploads depend on that.
fn attempt_prefix(deployment_id: Uuid, version: i64, attempt: Uuid) -> String {
  format!("builds/{deployment_id}/v{version}/{attempt}")
}

/// The files the user-code build uploads, under its attempt prefix. The
/// bucket's lifecycle rule deletes leftover `builds/**/image.tar` objects
/// after a day, so the name is part of the ops contract.
const IMAGE_TAR: &str = "image.tar";
const MANIFEST_JSON: &str = "manifest.json";
const USES_UV: &str = "uses_uv";

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

/// Step 1: download the app tarball by its signed URL (`$0`) and unpack it
/// into `/workspace/app`, where the shared Dockerfile expects it. The
/// tarball's members are the app tree.
const FETCH_SOURCE_SCRIPT: &str = "set -eu
curl -fsS -o /workspace/source.tar.gz \"$0\"
mkdir -p /workspace/app
tar -xzf /workspace/source.tar.gz -C /workspace/app
rm /workspace/source.tar.gz";

/// Push step 1: download the saved image by its signed URL (`$0`).
const FETCH_IMAGE_SCRIPT: &str = "set -eu
curl -fsS -o /workspace/image.tar \"$0\"";

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

/// The most the server reads back of each build result. The app's code writes
/// both, so neither is trusted: a manifest is a few KB, and `uses_uv` is one
/// word.
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_USES_UV_BYTES: usize = 64;
/// The largest saved image the user-code build may upload.
const MAX_IMAGE_TAR_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// What the user-code build uploads, in order, and each one's size cap.
const UPLOADS: [(&str, u64); 3] = [
  (IMAGE_TAR, MAX_IMAGE_TAR_BYTES),
  (MANIFEST_JSON, MAX_MANIFEST_BYTES as u64),
  (USES_UV, MAX_USES_UV_BYTES as u64),
];

/// The image every step but the import and the push runs in. It ships curl.
const DOCKER_BUILDER: &str = "gcr.io/cloud-builders/docker";
/// The push step's image, pinned by digest: it's the one step that can
/// write images, so it runs exactly the code we tested.
const GCRANE: &str = "gcr.io/go-containerregistry/gcrane:v0.22.1@sha256:67b9993e083198db3d30ae6d2bb685cc67a45494a3c485ef2fa9994715bee9ff";

// ---------------------------------------------------------------------------
// Signed URLs (the specs are pure — unit-tested; signing needs the key)
// ---------------------------------------------------------------------------

/// One V4 signed URL to make: a method on one object, plus the headers the
/// request must send. A signed header is part of the signature, so a
/// request that drops or changes it is refused.
#[derive(Debug, Clone, PartialEq)]
struct UrlSpec {
  method: google_cloud_storage::http::Method,
  object: String,
  headers: Vec<(&'static str, String)>,
  expiry: Duration,
}

impl UrlSpec {
  fn get(object: String, expiry: Duration) -> Self {
    UrlSpec {
      method: google_cloud_storage::http::Method::GET,
      object,
      headers: Vec::new(),
      expiry,
    }
  }

  /// An upload that can create the object once and never replace it, at
  /// most `max_bytes` long. Cloud Storage enforces both conditions, even
  /// for a chunked upload that sends no length.
  fn create(object: String, max_bytes: u64, expiry: Duration) -> Self {
    UrlSpec {
      method: google_cloud_storage::http::Method::PUT,
      object,
      headers: vec![
        ("x-goog-if-generation-match", "0".to_owned()),
        ("x-goog-content-length-range", format!("0,{max_bytes}")),
      ],
      expiry,
    }
  }

  fn delete(object: String) -> Self {
    UrlSpec {
      method: google_cloud_storage::http::Method::DELETE,
      object,
      headers: Vec::new(),
      expiry: Duration::from_secs(300),
    }
  }
}

/// The user-code build's uploads, in [`UPLOADS`] order. The URLs outlive
/// the build's own timeout, with slack for queueing.
fn upload_specs(prefix: &str) -> Vec<UrlSpec> {
  UPLOADS
    .iter()
    .map(|(file, max_bytes)| {
      UrlSpec::create(format!("{prefix}/{file}"), *max_bytes, BUILD_POLL_TIMEOUT)
    })
    .collect()
}

/// The last user-code step: upload each file in [`UPLOADS`] to its signed
/// URL (`$0`, `$1`, `$2`), sending exactly the headers the URL was signed
/// with.
fn upload_script(specs: &[UrlSpec]) -> String {
  let mut lines = vec!["set -eu".to_owned()];
  for (i, ((file, _), spec)) in UPLOADS.iter().zip(specs).enumerate() {
    let headers: String = spec
      .headers
      .iter()
      .map(|(k, v)| format!(" -H '{k}: {v}'"))
      .collect();
    lines.push(format!(
      "curl -fsS -X PUT{headers} --upload-file /workspace/{file} \"${i}\""
    ));
  }
  lines.join("\n")
}

/// Step 2 args: write the generated Dockerfile (base64-embedded to keep the
/// Dockerfile content out of shell-quoting reach).
fn dockerfile_write_args(dockerfile: &str) -> Vec<String> {
  let b64 = base64::engine::general_purpose::STANDARD.encode(dockerfile);
  vec![
    "-c".to_owned(),
    format!("printf '%s' '{b64}' | base64 -d > /workspace/Dockerfile"),
  ]
}

/// Step 3 args: the docker build, caching from the base image (its
/// python/uv/sdk layers are the bulk). Not from the deployment's previous
/// image: reading it would need registry access, which this build must not
/// have.
fn docker_build_args(image_ref: &str, base_image: &str) -> Vec<String> {
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
    "-t".to_owned(),
    image_ref.to_owned(),
  ];
  // The build context: /workspace (the Dockerfile + assembled app live
  // there). Cloud Build steps run in /workspace, so `.` is the context.
  // (Live-verification finding, 2026-09-16: without it docker reads
  // '-t <image>' as the missing PATH argument — 'requires exactly 1
  // argument'.)
  args.push(".".to_owned());
  args
}

/// A Cloud Build config: `steps`, run AS `service_account`. Neither build
/// sets `source`, `images` or `artifacts`: each moves files only through the
/// signed URLs in its steps, so neither depends on what its identity can
/// read or write in the bucket.
fn build_config(steps: Vec<cb::BuildStep>, service_account: &str, timeout_secs: i64) -> cb::Build {
  cb::Build::default()
    .set_steps(escape_substitutions(steps))
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
    .set_timeout(wkt::Duration::clamp(timeout_secs, 0))
}

/// Cloud Build reads `$NAME` in step args and env as its own substitution,
/// and refuses a build that names one it doesn't know. We use none, so every
/// `$` is meant for the shell: `$$` passes it through.
fn escape_substitutions(steps: Vec<cb::BuildStep>) -> Vec<cb::BuildStep> {
  let escape = |v: &mut Vec<String>| {
    for a in v.iter_mut() {
      *a = a.replace('$', "$$");
    }
  };
  steps
    .into_iter()
    .map(|mut step| {
      escape(&mut step.args);
      escape(&mut step.env);
      step
    })
    .collect()
}

/// The user-code build's six steps. Everything from step 3 on runs the
/// app's code or could be changed by it, so nothing here is a boundary:
/// what it uploads is checked where it's received. Pure — unit-tested.
fn build_steps(
  base_image: &str,
  image_ref: &str,
  entry: &str,
  source_url: &str,
  upload_specs: &[UrlSpec],
  upload_urls: &[String],
) -> Vec<cb::BuildStep> {
  let dockerfile = generate_dockerfile(base_image);
  let mut upload_args = vec!["-c".to_owned(), upload_script(upload_specs)];
  upload_args.extend(upload_urls.iter().cloned());
  vec![
    // 1. fetch: the app tarball, by its signed URL → /workspace/app
    cb::BuildStep::default()
      .set_id("fetch")
      .set_name(DOCKER_BUILDER)
      .set_entrypoint("bash")
      .set_args(vec![
        "-c".to_owned(),
        FETCH_SOURCE_SCRIPT.to_owned(),
        source_url.to_owned(),
      ]),
    // 2. dockerfile: the shared generator's output
    cb::BuildStep::default()
      .set_id("dockerfile")
      .set_name(DOCKER_BUILDER)
      .set_entrypoint("bash")
      .set_args(dockerfile_write_args(&dockerfile)),
    // 3. build (deps bake at build time — same shape as model (a))
    cb::BuildStep::default()
      .set_id("build")
      .set_name(DOCKER_BUILDER)
      .set_args(docker_build_args(image_ref, base_image)),
    // 4. manifest import: run the built image (its ENTRYPOINT is the
    //    sidecar — overridden by `entrypoint: sh`), import in /app,
    //    manifest lands in the shared /workspace. Cloud Build runs the
    //    image it just built, by its tag; it never pulls it.
    cb::BuildStep::default()
      .set_id("manifest")
      .set_name(image_ref.to_owned())
      .set_entrypoint("sh")
      .set_dir("/app")
      .set_env(vec![
        "SECTR_MANIFEST=1".to_owned(),
        format!("SECTR_ENTRY={entry}"),
        format!("SECTR_MANIFEST_OUT=/workspace/{MANIFEST_JSON}"),
      ])
      .set_args(vec!["-c".to_owned(), MANIFEST_SCRIPT.to_owned()]),
    // 5. save: the image, as one file for the push build
    cb::BuildStep::default()
      .set_id("save")
      .set_name(DOCKER_BUILDER)
      .set_args(vec![
        "save".to_owned(),
        "-o".to_owned(),
        format!("/workspace/{IMAGE_TAR}"),
        image_ref.to_owned(),
      ]),
    // 6. upload: each file to its create-only, size-capped signed URL
    cb::BuildStep::default()
      .set_id("upload")
      .set_name(DOCKER_BUILDER)
      .set_entrypoint("bash")
      .set_args(upload_args),
  ]
}

/// The push build's two steps. It runs no app code: it downloads the saved
/// image and pushes it under the one name the server chose. `crane push`
/// uploads the tar's layers without unpacking them, ignores any names the
/// tar declares, and refuses a tar holding more than one image. It
/// authenticates to Artifact Registry as the build's identity. Pure —
/// unit-tested.
fn push_steps(image_tar_url: &str, image_ref: &str) -> Vec<cb::BuildStep> {
  vec![
    cb::BuildStep::default()
      .set_id("fetch")
      .set_name(DOCKER_BUILDER)
      .set_entrypoint("bash")
      .set_args(vec![
        "-c".to_owned(),
        FETCH_IMAGE_SCRIPT.to_owned(),
        image_tar_url.to_owned(),
      ]),
    cb::BuildStep::default()
      .set_id("push")
      .set_name(GCRANE)
      .set_args(vec![
        "push".to_owned(),
        format!("/workspace/{IMAGE_TAR}"),
        image_ref.to_owned(),
      ]),
  ]
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
  /// The user-code build runs AS this. It needs only to write logs.
  build_sa_email: String,
  /// The push build runs AS this: the one build that can write images.
  push_sa_email: String,
  /// SA the ensured jobs run AS (least-privilege runtime identity — AR
  /// reader only; the runner dials out and needs nothing else). `None`
  /// on the build-only constructor (E2B backend — it never ensures jobs;
  /// `ensure_job` fails with a clear error rather than a panic if ever
  /// reached, which it must not be).
  runner_sa_email: Option<String>,
  gcs_bucket: String,
  /// Required at build time (same failure point as [`DockerPlatform`]):
  /// the runner base image. Public (GHCR by default): the user-code build
  /// pulls it with no credentials.
  runner_base_image: Option<String>,
  pub(crate) runner_server_url: String,
  runner_sidecar_cmd: Vec<String>,
  store: Arc<dyn ArtifactStore>,
  /// Signs the builds' URLs, as the server's own identity (the
  /// `sectr-server` key signs locally, with no network call).
  signer: google_cloud_auth::signer::Signer,
  /// Deletes the saved image after its push, by signed URL.
  http: reqwest::Client,
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
    let push_sa_email = config.gcp_push_sa_email.clone().ok_or_else(|| {
      anyhow::anyhow!("SECTR_GCP_PUSH_SA_EMAIL is required when SECTR_PLATFORM=gcp")
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
    let signer = google_cloud_auth::credentials::Builder::default()
      .build_signer()
      .map_err(|e| anyhow::anyhow!("make a URL signer from the server's credentials: {e}"))?;
    Ok(GcpPlatform {
      project_id,
      region,
      ar_repo: config.gcp_ar_repo.clone(),
      build_sa_email,
      push_sa_email,
      runner_sa_email,
      gcs_bucket,
      runner_base_image: config.runner_base_image.clone(),
      runner_server_url: config.runner_server_url.clone(),
      runner_sidecar_cmd: config.runner_sidecar_cmd.clone(),
      store,
      signer,
      http: reqwest::Client::new(),
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

  /// The Cloud Build half WITHOUT the platform's deployment-resource step:
  /// the user-code build, its results, then the push build. The gcp
  /// platform adds `ensure_job` on top (above); the E2B platform adds the
  /// E2B template build instead.
  pub(crate) async fn cloud_build_impl(
    &self,
    req: BuildRequest,
  ) -> Result<BuildOutput, BuildError> {
    let attempt = Uuid::new_v4();
    let prefix = attempt_prefix(req.deployment_id, req.version, attempt);
    let outcome = self.build_and_push(&req, &prefix).await;
    // The saved image only carries the build to the push. Delete it
    // whatever happened: the app's code may have uploaded it even if its
    // build then failed. The bucket's lifecycle rule catches a miss.
    self.delete_quietly(format!("{prefix}/{IMAGE_TAR}")).await;
    outcome
  }

  async fn build_and_push(
    &self,
    req: &BuildRequest,
    prefix: &str,
  ) -> Result<BuildOutput, BuildError> {
    let base_image = self.runner_base_image.as_deref().ok_or_else(|| {
      BuildError::bare("SECTR_RUNNER_BASE_IMAGE is not configured — no base image to build on")
    })?;
    let image_ref = self.ar_image_ref(req.deployment_id, req.version);
    let (src_bucket, src_object) = parse_gs_uri(&req.artifact_uri)?;
    if src_bucket != self.gcs_bucket {
      return Err(BuildError::bare(format!(
        "artifact {src_bucket:?} is not the configured GCS bucket {:?} — the server signs \
                 URLs for that bucket only",
        self.gcs_bucket
      )));
    }

    // 1. The user-code build. Its identity can only write logs; its files
    //    move through URLs signed for exactly those objects.
    let source_url = self
      .sign(&UrlSpec::get(src_object, BUILD_POLL_TIMEOUT))
      .await?;
    let uploads = upload_specs(prefix);
    let mut upload_urls = Vec::with_capacity(uploads.len());
    for spec in &uploads {
      upload_urls.push(self.sign(spec).await?);
    }
    let build = build_config(
      build_steps(
        base_image,
        &image_ref,
        &req.entry,
        &source_url,
        &uploads,
        &upload_urls,
      ),
      &self.sa_resource(&self.build_sa_email),
      BUILD_TIMEOUT_SECS,
    );
    let finished = self.run_build(build, BUILD_POLL_TIMEOUT).await?;
    if finished.status != cb::build::Status::Success {
      let (message, logs_tail) = failure_report(&finished);
      return Err(BuildError { message, logs_tail });
    }

    // 2. Its results. The app's code wrote them, so the reads are capped.
    let result_uri = |file: &str| format!("gs://{}/{prefix}/{file}", self.gcs_bucket);
    let manifest_bytes = self
      .store
      .get_at_most(&result_uri(MANIFEST_JSON), MAX_MANIFEST_BYTES)
      .await
      .map_err(|e| BuildError::bare(format!("manifest result fetch: {e}")))?;
    let imported: Value = serde_json::from_slice(&manifest_bytes)
      .map_err(|e| BuildError::bare(format!("manifest result is not JSON: {e}")))?;
    let uses_uv = self
      .store
      .get_at_most(&result_uri(USES_UV), MAX_USES_UV_BYTES)
      .await
      .map_err(|e| BuildError::bare(format!("uses_uv result fetch: {e}")))?;
    let uses_uv = matches!(std::str::from_utf8(&uses_uv).map(str::trim), Ok("true"));

    // 3. The push build: no app code, and the only build that can write
    //    images.
    let tar_url = self
      .sign(&UrlSpec::get(
        format!("{prefix}/{IMAGE_TAR}"),
        PUSH_POLL_TIMEOUT,
      ))
      .await?;
    let push = build_config(
      push_steps(&tar_url, &image_ref),
      &self.sa_resource(&self.push_sa_email),
      PUSH_TIMEOUT_SECS,
    );
    let finished = self.run_build(push, PUSH_POLL_TIMEOUT).await?;
    if finished.status != cb::build::Status::Success {
      let (message, logs_tail) = failure_report(&finished);
      return Err(BuildError {
        message: format!("pushing the image: {message}"),
        logs_tail,
      });
    }

    Ok(BuildOutput {
      image_ref,
      imported_manifest: imported,
      uses_uv,
    })
  }

  /// The serviceAccount field wants the FULL resource name (bare emails
  /// fail with 'Failed to parse resource name' — live-verification finding,
  /// 2026-09-16); the config values stay emails.
  fn sa_resource(&self, email: &str) -> String {
    format!("projects/{}/serviceAccounts/{email}", self.project_id)
  }

  /// Sign one URL. The error never includes a URL: a signed URL is a
  /// credential until it expires.
  async fn sign(&self, spec: &UrlSpec) -> Result<String, BuildError> {
    let mut builder = SignedUrlBuilder::for_object(
      format!("projects/_/buckets/{}", self.gcs_bucket),
      spec.object.clone(),
    )
    .with_method(spec.method.clone())
    .with_expiration(spec.expiry);
    for (name, value) in &spec.headers {
      builder = builder.with_header(*name, value.clone());
    }
    builder
      .sign_with(&self.signer)
      .await
      .map_err(|e| BuildError::bare(format!("sign a URL for {:?}: {e}", spec.object)))
  }

  /// Delete one object, logging (not failing) on error.
  async fn delete_quietly(&self, object: String) {
    let url = match self.sign(&UrlSpec::delete(object.clone())).await {
      Ok(url) => url,
      Err(e) => {
        tracing::warn!(%object, error = %e.message, "could not delete a saved build image");
        return;
      }
    };
    match self.http.delete(url).send().await {
      Ok(r) if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND => {}
      Ok(r) => {
        tracing::warn!(%object, status = %r.status(), "could not delete a saved build image")
      }
      Err(e) => {
        let e = e.without_url();
        tracing::warn!(%object, error = %e, "could not delete a saved build image");
      }
    }
  }

  /// Submit a build and poll it to a terminal state. NOT the LRO poller: a
  /// FAILED build arrives as an operation ERROR ('Build failed; check build
  /// logs'), which would discard the Build resource that failure_report
  /// needs (live-verification finding, 2026-09-16). Instead: submit once,
  /// take the build id from the operation's metadata, and poll get_build —
  /// which returns the full resource through terminal states, failed or not.
  async fn run_build(
    &self,
    build: cb::Build,
    poll_timeout: Duration,
  ) -> Result<cb::Build, BuildError> {
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
    let deadline = tokio::time::Instant::now() + poll_timeout;
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
          poll_timeout.as_secs()
        )));
      }
      tokio::time::sleep(Duration::from_secs(5)).await;
    };
    tracing::info!(build = %finished.id, status = %finished.status, "Cloud Build finished");
    Ok(finished)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const PROJECT: &str = "example-project-123456";
  const REGION: &str = "us-east4";
  const REPO: &str = "sectr-runners";
  const BASE: &str = "ghcr.io/sectr-dev/sectr-runner-base:v1.2.3";
  const SA: &str = "sectr-builder@example-project-123456.iam.gserviceaccount.com";
  const PUSH_SA: &str = "sectr-pusher@example-project-123456.iam.gserviceaccount.com";
  const SOURCE_URL: &str = "https://signed.example/org/app/1.tar.gz";
  const TAR_URL: &str = "https://signed.example/image.tar";

  fn deployment() -> Uuid {
    // A fixed id: the assertions below pin exact strings.
    Uuid::parse_str("01912345-6789-7abc-def0-123456789abc").unwrap()
  }

  fn image(deployment: Uuid, version: i64) -> String {
    ar_image_ref(PROJECT, REGION, REPO, deployment, version)
  }

  fn attempt() -> Uuid {
    Uuid::parse_str("0b5e7a1c-2d3e-4f50-8a6b-7c8d9e0f1a2b").unwrap()
  }

  fn sa(email: &str) -> String {
    format!("projects/{PROJECT}/serviceAccounts/{email}")
  }

  /// A stand-in URL per spec: the signature itself needs the key.
  fn urls(specs: &[UrlSpec]) -> Vec<String> {
    specs
      .iter()
      .map(|s| format!("https://signed.example/{}", s.object))
      .collect()
  }

  fn user_build(deployment: Uuid, version: i64) -> cb::Build {
    let specs = upload_specs(&attempt_prefix(deployment, version, attempt()));
    let steps = build_steps(
      BASE,
      &image(deployment, version),
      "agent:agent",
      SOURCE_URL,
      &specs,
      &urls(&specs),
    );
    build_config(steps, &sa(SA), BUILD_TIMEOUT_SECS)
  }

  fn push_build(deployment: Uuid, version: i64) -> cb::Build {
    build_config(
      push_steps(TAR_URL, &image(deployment, version)),
      &sa(PUSH_SA),
      PUSH_TIMEOUT_SECS,
    )
  }

  fn step<'a>(build: &'a cb::Build, id: &str) -> &'a cb::BuildStep {
    build
      .steps
      .iter()
      .find(|s| s.id == id)
      .unwrap_or_else(|| panic!("no step {id:?}"))
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
      attempt_prefix(d, 3, attempt()),
      format!("builds/{d}/v3/{}", attempt())
    );
    // The bucket's lifecycle rule deletes `builds/**/image.tar` (ops repo,
    // storage.tf): the saved image must keep that name and place.
    let tar = format!("{}/{IMAGE_TAR}", attempt_prefix(d, 3, attempt()));
    assert!(
      tar.starts_with("builds/") && tar.ends_with("/image.tar"),
      "{tar}"
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
  fn the_user_code_build_holds_no_source_registry_or_bucket_access() {
    let d = deployment();
    let build = user_build(d, 1);

    // Files move only through the signed URLs in its steps.
    assert!(
      build.source.is_none(),
      "no `source`: it would read the bucket as the build SA"
    );
    assert!(
      build.images.is_empty(),
      "no `images`: the user-code build must never push"
    );
    assert!(
      build.artifacts.is_none(),
      "no `artifacts`: it would write the bucket as the build SA"
    );

    // Runs AS the build SA, which only writes logs. The Build API wants
    // the full resource name.
    assert_eq!(build.service_account, sa(SA));
    let options = build.options.expect("build options must be set");
    assert_eq!(
      options.logging,
      cb::build_options::LoggingMode::CloudLoggingOnly
    );
    assert_eq!(
      build.timeout.expect("build timeout must be set").seconds(),
      BUILD_TIMEOUT_SECS
    );
  }

  #[test]
  fn user_code_build_steps_fetch_build_import_save_upload() {
    let d = deployment();
    let build = user_build(d, 1);
    let ids: Vec<&str> = build.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
      ids,
      ["fetch", "dockerfile", "build", "manifest", "save", "upload"]
    );

    // fetch: the tarball by its signed URL, unpacked into /workspace/app.
    let fetch = step(&build, "fetch");
    assert_eq!(fetch.name, DOCKER_BUILDER);
    assert_eq!(fetch.args[2], SOURCE_URL);
    assert!(fetch.args[1].contains("curl -fsS"), "{}", fetch.args[1]);
    assert!(
      fetch.args[1].contains("-C /workspace/app"),
      "{}",
      fetch.args[1]
    );

    // dockerfile: decodes to EXACTLY what the shared generator produces
    // (byte-identical image content with DockerPlatform).
    let write_cmd = &step(&build, "dockerfile").args[1];
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

    // build: tagged with the AR ref, arch pinned (R4), context last, and
    // cached from the base image only.
    let docker_build = &step(&build, "build").args;
    assert!(docker_build.contains(&"--platform=linux/amd64".to_owned()));
    assert!(docker_build.contains(&"--network=cloudbuild".to_owned()));
    assert!(docker_build.contains(&image(d, 1)));
    assert_eq!(docker_build.last(), Some(&".".to_owned()));
    let cache_from: Vec<&String> = docker_build
      .iter()
      .zip(docker_build.iter().skip(1))
      .filter(|(flag, _)| *flag == "--cache-from")
      .map(|(_, v)| v)
      .collect();
    assert_eq!(
      cache_from,
      [BASE],
      "no previous image: reading it needs registry access"
    );

    // manifest: runs the image just built, by its tag.
    let manifest = step(&build, "manifest");
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
    assert!(manifest.args[1].contains("uv run --no-sync"));
    assert!(manifest.args[1].contains("sectr.inspect"));

    // save: the image as one file.
    assert_eq!(
      step(&build, "save").args,
      ["save", "-o", "/workspace/image.tar", image(d, 1).as_str()]
    );
  }

  #[test]
  fn uploads_are_create_only_capped_and_under_one_attempt_prefix() {
    let d = deployment();
    let prefix = attempt_prefix(d, 1, attempt());
    let specs = upload_specs(&prefix);
    let objects: Vec<&str> = specs.iter().map(|s| s.object.as_str()).collect();
    assert_eq!(
      objects,
      [
        format!("{prefix}/image.tar"),
        format!("{prefix}/manifest.json"),
        format!("{prefix}/uses_uv"),
      ]
    );
    for (spec, cap) in specs.iter().zip([8u64 << 30, 16 << 20, 64]) {
      assert_eq!(spec.method, google_cloud_storage::http::Method::PUT);
      assert_eq!(
        spec.headers,
        [
          ("x-goog-if-generation-match", "0".to_owned()),
          ("x-goog-content-length-range", format!("0,{cap}")),
        ],
        "{}",
        spec.object
      );
      assert!(
        spec.expiry.as_secs() > BUILD_TIMEOUT_SECS as u64,
        "a URL must outlive the build that uses it"
      );
    }

    // The upload step sends each URL exactly the headers it was signed
    // with (a missing or changed signed header is refused), in order.
    let build = user_build(d, 1);
    let upload = step(&build, "upload");
    assert_eq!(&upload.args[2..], urls(&specs).as_slice());
    let script = &upload.args[1];
    for (i, spec) in specs.iter().enumerate() {
      let file = spec.object.rsplit('/').next().unwrap();
      let line = script
        .lines()
        .find(|l| l.contains(&format!("/workspace/{file} ")))
        .unwrap_or_else(|| panic!("no upload of {file}: {script}"));
      assert!(line.ends_with(&format!("\"$${i}\"")), "{line}");
      for (name, value) in &spec.headers {
        assert!(line.contains(&format!("-H '{name}: {value}'")), "{line}");
      }
    }
  }

  #[test]
  fn every_shell_dollar_is_escaped_from_cloud_build_substitution() {
    for build in [user_build(deployment(), 1), push_build(deployment(), 1)] {
      for s in &build.steps {
        for value in s.args.iter().chain(&s.env) {
          assert!(
            !value.replace("$$", "").contains('$'),
            "step {:?} has an unescaped $ (Cloud Build would refuse the build): {value}",
            s.id
          );
        }
      }
    }
    // And the shell still gets its `$0`.
    let build = user_build(deployment(), 1);
    assert!(step(&build, "fetch").args[1].contains("\"$$0\""));
  }

  #[test]
  fn the_push_build_runs_no_app_code_and_pushes_one_fixed_name() {
    let d = deployment();
    let build = push_build(d, 1);
    assert_eq!(build.service_account, sa(PUSH_SA));
    assert!(build.source.is_none() && build.images.is_empty() && build.artifacts.is_none());
    assert_eq!(
      build.timeout.expect("push timeout must be set").seconds(),
      PUSH_TIMEOUT_SECS
    );
    let ids: Vec<&str> = build.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["fetch", "push"]);

    let fetch = step(&build, "fetch");
    assert_eq!(fetch.name, DOCKER_BUILDER);
    assert_eq!(fetch.args[2], TAR_URL);

    // crane push: no `docker load` (it would unpack the app's layers
    // where the build can write images), and the name is the server's.
    let push = step(&build, "push");
    assert_eq!(push.name, GCRANE);
    assert!(
      GCRANE.contains("@sha256:"),
      "the push image is pinned by digest"
    );
    assert_eq!(
      push.args,
      ["push", "/workspace/image.tar", image(d, 1).as_str()]
    );
    for s in &build.steps {
      assert_ne!(
        s.name,
        image(d, 1),
        "the push build never runs the app's image"
      );
      assert!(
        !s.args.iter().any(|a| a.contains("docker load")),
        "no docker load"
      );
    }
  }

  /// The regression guard: the build that runs the app's code must never
  /// push or run as the push identity. (The live IAM is checked by the ops
  /// repo's doctor.)
  #[test]
  fn only_the_push_build_can_write_images() {
    let d = deployment();
    let user = user_build(d, 1);
    let push = push_build(d, 1);
    assert_ne!(user.service_account, push.service_account);
    assert!(user.images.is_empty());
    assert!(!user.steps.iter().any(|s| s.name == GCRANE));
    assert!(
      !user
        .steps
        .iter()
        .any(|s| s.args.first().is_some_and(|a| a == "push")),
      "no docker push in the user-code build"
    );
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
