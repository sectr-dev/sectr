//! [`DockerPlatform`] — the model-(a) [`RunnerPlatform`]: everything runs
//! on the same host's docker daemon. `build` unpacks the
//! tarball from the artifact store into the build workspace, generates the
//! Dockerfile (`FROM $SECTR_RUNNER_BASE_IMAGE` + `COPY app /app`, deps
//! baked via `uv sync` when the app ships a pyproject.toml), `docker
//! build`, then gets the authoritative manifest from a one-shot
//! `SECTR_MANIFEST=1` import inside the built image, copying the result out
//! of the container. `spawn`/`teardown`: one runner container per turn,
//! spawned via [`DockerSandboxDriver`] (bollard). All-docker; the only host
//! requirement is the socket.
//!
//! The swap-point discipline: everything that
//! makes this Docker-specific lives behind the [`RunnerPlatform`] seam — a
//! serverless driver (model (b)) implements the same methods against
//! its provider's API and the control plane is unchanged. The GcpPlatform
//! reuses [`generate_dockerfile`] verbatim and swaps the daemon
//! calls for the Cloud Build API / the Cloud Run Jobs API.
//!
//! A runner behaves like an E2B sandbox wherever that isn't specific to E2B:
//! a per-turn handle, the same four tags, an orphan reaper listing, a maximum
//! lifetime, the same CPU and memory defaults, uid 1000, and an explicit
//! error when a deployment's image is gone. The turn-boundary policy is
//! shared. Only `self_terminating` differs, because it describes the
//! platform: a container ends with its sidecar, and a sandbox doesn't.
//!
//! Container contents & env (spawn):
//! - image: the deployment manifest's `runner.image`, falling back to the
//!   server's `SECTR_RUNNER_IMAGE` (the build path pins per-deployment
//!   images; the fallback keeps dev/testing possible).
//! - cmd: the sidecar command (`SECTR_RUNNER_SIDECAR_CMD`) under `timeout`.
//! - env: `SECTR_INGEST_URL` (the server as seen from the container) +
//!   `SECTR_INGEST_TOKEN` (per-turn: the grant dies at every
//!   turn boundary and the next dispatch spawns a fresh process with a
//!   fresh token) + `HOME`. Secrets do NOT ride the
//!   container env: they arrive via the assignment, resolved per turn.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use sectr_common::drivers::ArtifactStore;
use sectr_common::drivers::sandbox::{
  ContainerRun, DockerSandboxDriver, ListedContainer, OneShotRun,
};
use sectr_common::inspect;
use sectr_common::registry::Registry;

use crate::config::ServerConfig;
use crate::dispatch::DispatchError;
use crate::platform::{
  BuildError, BuildOutput, BuildRequest, RunnerInfo, RunnerPlatform, server_host_of,
};

const LABEL_SESSION: &str = "sectr.session";
const LABEL_DEPLOYMENT: &str = "sectr.deployment";
const LABEL_INVOCATION: &str = "sectr.invocation";
/// Names the server that owns a runner: the reaper lists only its own.
const LABEL_SERVER: &str = "sectr.server";

/// Numeric, so the base image needn't define the user. E2B runs its
/// processes as uid 1000 too.
const RUNNER_USER: &str = "1000:1000";
/// uid 1000 has no home directory in the image, and uv needs one for its
/// cache.
const RUNNER_HOME: &str = "/tmp";
/// Where the import writes the manifest inside its one-shot container.
const MANIFEST_OUT: &str = "/tmp/sectr-manifest.json";
/// How long a runner gets to stop after its lifetime runs out, before it's
/// killed.
const STOP_GRACE_SECS: u64 = 10;

/// The docker tag a deployment's image builds to (also the provisional
/// `build_ref` — see `deploy.rs`).
pub(crate) fn image_tag(deployment_id: Uuid, version: i64) -> String {
  format!("sectr-app-{}:v{}", deployment_id, version)
}

/// The runner image's Dockerfile: the base image + the app tree. The
/// `if [ -f pyproject.toml ]` guard keeps dependency-free apps building,
/// while deps bake at BUILD time for dep-bearing ones (the runner then
/// never needs the network).
///
/// The examples' editable path dep (`sectr = ../../sdk-python` from
/// /app) resolves to /sdk-python inside the image — but uv refuses to
/// NORMALIZE a relative path beyond the workspace root ("cannot
/// normalize a relative path beyond the base directory"), so a
/// symlink alone never gets a chance: rewrite the reference to the
/// absolute in-image path in BOTH pyproject.toml and uv.lock (uv
/// --frozen validates that the two agree). No-op for PyPI-dep apps.
///
/// Shared with the GcpPlatform, which must produce the
/// same image content.
pub(crate) fn generate_dockerfile(base_image: &str) -> String {
  // The app's lines are shared with endpoint images; only the SDK path
  // rewrite is this server's own.
  let steps = sectr_common::dockerfile::app_build_steps(
    "app",
    "RUN ln -s /opt/sdk-python /sdk-python\n",
    "sed -i 's#\\.\\./\\.\\./sdk-python#/sdk-python#g' pyproject.toml uv.lock; ",
  );
  format!("FROM {base_image}\n{steps}")
}

/// One name per turn: a new turn can't collide with, or remove, another
/// turn's container.
pub fn runner_container_name(session: Uuid, invocation: Uuid) -> String {
  format!("sectr-runner-{}-{}", session.simple(), invocation.simple())
}

/// The `sectr.server` label value: this server's host and port, plus its
/// runner network when it has one. One daemon can host several Sectr
/// installs, which can share a host name (`sectr-server` in every compose
/// stack). Each reaper must see only its own runners, or it would find no
/// lease for the others' and kill them.
pub(crate) fn runner_server_tag(config: &ServerConfig) -> anyhow::Result<String> {
  let host = server_host_of(&config.runner_server_url)?;
  let authority = match reqwest::Url::parse(&config.runner_server_url)
    .ok()
    .and_then(|url| url.port_or_known_default())
  {
    Some(port) => format!("{host}:{port}"),
    None => host,
  };
  Ok(match &config.runner_network {
    Some(network) => format!("{authority}@{network}"),
    None => authority,
  })
}

/// Resolve the runner image from the manifest. Formal manifest schema is
/// the open decision; until then `runner.image` (string) is the opt-in
/// per-deployment image.
fn image_from_manifest(manifest: &serde_json::Value) -> Option<String> {
  manifest
    .get("runner")
    .and_then(|r| r.get("image"))
    .and_then(|i| i.as_str())
    .map(str::to_owned)
}

/// Build the container spec for one turn's runner. Pure — unit-tested.
pub fn container_run_for(
  session: Uuid,
  deployment_id: Uuid,
  invocation_id: Uuid,
  ingest_token: &str,
  manifest: &serde_json::Value,
  config: &ServerConfig,
) -> Result<ContainerRun, DispatchError> {
  let image = image_from_manifest(manifest)
    .or_else(|| config.runner_image.clone())
    .ok_or_else(|| {
      DispatchError::Spawn(
        "no runner image: manifest.runner.image missing and SECTR_RUNNER_IMAGE unset".into(),
      )
    })?;
  if config.runner_sidecar_cmd.is_empty() {
    return Err(DispatchError::Spawn(
      "SECTR_RUNNER_SIDECAR_CMD is empty".into(),
    ));
  }
  let server_tag = runner_server_tag(config).map_err(|e| DispatchError::Spawn(e.to_string()))?;
  let env = vec![
    (
      "SECTR_INGEST_URL".to_owned(),
      config.runner_server_url.clone(),
    ),
    ("SECTR_INGEST_TOKEN".to_owned(), ingest_token.to_owned()),
    ("HOME".to_owned(), RUNNER_HOME.to_owned()),
  ];
  let resources = config.runner_resources;
  // Maximum lifetime, as an E2B sandbox has: at expiry the container stops,
  // and the reconciler ends the turn when its lease expires. The image's
  // coreutils provide `timeout`; it stops the sidecar and everything it
  // started, and the container ends with it.
  let mut cmd = vec![
    "timeout".to_owned(),
    "-k".to_owned(),
    STOP_GRACE_SECS.to_string(),
    resources.timeout_secs.to_string(),
  ];
  cmd.extend(config.runner_sidecar_cmd.iter().cloned());
  Ok(ContainerRun {
    image,
    // Auto-remove: the sidecar exits itself at the turn boundary
    // (the sidecar policy) and nothing on the prod path waits
    // for its exit code — the daemon must sweep the husk.
    auto_remove: true,
    name: Some(runner_container_name(session, invocation_id)),
    labels: [
      (LABEL_SESSION.to_owned(), session.to_string()),
      (LABEL_DEPLOYMENT.to_owned(), deployment_id.to_string()),
      (LABEL_INVOCATION.to_owned(), invocation_id.to_string()),
      (LABEL_SERVER.to_owned(), server_tag),
    ]
    .into_iter()
    .collect(),
    env,
    cmd,
    network: config.runner_network.clone(),
    memory: Some(resources.memory_mb * 1024 * 1024),
    nano_cpus: Some((resources.cpu_count * 1e9) as i64),
    user: Some(RUNNER_USER.to_owned()),
  })
}

/// One listed container → a [`RunnerInfo`]. Entries we can't interpret are
/// skipped: the reaper must never act on uncertainty.
fn runner_from_listed(container: &ListedContainer) -> Option<RunnerInfo> {
  let session = container.labels.get(LABEL_SESSION)?.parse().ok()?;
  let invocation = container
    .labels
    .get(LABEL_INVOCATION)
    .and_then(|v| v.parse().ok());
  let started_at = chrono::DateTime::from_timestamp(container.created, 0)?;
  Some(RunnerInfo {
    handle: container.name.clone(),
    session,
    invocation,
    started_at,
  })
}

/// The `docker build` context: the Dockerfile and the app. Symlinks stay
/// symlinks. Following them would read whatever they point at on the server
/// (an uploaded `leak -> /proc/self/environ` would put the server's secrets
/// in the image).
fn build_context(workdir: &Path) -> Result<Vec<u8>, BuildError> {
  let context_err = |e: std::io::Error| BuildError::bare(format!("context tar: {e}"));
  let mut context = tar::Builder::new(Vec::new());
  context.follow_symlinks(false);
  context
    .append_path_with_name(workdir.join("Dockerfile"), "Dockerfile")
    .map_err(context_err)?;
  context
    .append_dir_all("app", workdir.join("app"))
    .map_err(context_err)?;
  context.into_inner().map_err(context_err)
}

/// The structured log tail inside `DockerSandboxDriver::build_image`'s
/// error strings (`…\n--- build log tail ---\n<tail>`); empty when the
/// error carries no tail.
fn build_log_tail(message: &str) -> String {
  message
    .split_once("--- build log tail ---\n")
    .map(|(_, tail)| tail.to_owned())
    .unwrap_or_default()
}

/// The docker-backed platform. Holds exactly what the docker path needs:
/// the artifact store (fetch + unpack the tarball), the daemon handle,
/// the server config (base image + build workspace + runner wiring), and
/// the registry (manifest resolution at spawn).
pub struct DockerPlatform {
  store: Arc<dyn ArtifactStore>,
  /// `None` when no daemon is reachable — the build fails at the same
  /// step the pre-merge code did (after unpack, before `docker build`).
  docker: Option<Arc<DockerSandboxDriver>>,
  config: ServerConfig,
  registry: Arc<dyn Registry>,
}

impl DockerPlatform {
  pub fn new(
    store: Arc<dyn ArtifactStore>,
    docker: Option<Arc<DockerSandboxDriver>>,
    config: ServerConfig,
    registry: Arc<dyn Registry>,
  ) -> Self {
    DockerPlatform {
      store,
      docker,
      config,
      registry,
    }
  }

  /// The underlying docker driver (test/ops access for existence checks).
  pub fn driver(&self) -> &DockerSandboxDriver {
    self
      .docker
      .as_ref()
      .expect("DockerPlatform::driver() on a dockerless platform")
  }

  /// The build's steps, in the deployment's workspace.
  async fn build_in(&self, workdir: &Path, req: &BuildRequest) -> Result<BuildOutput, BuildError> {
    let base_image = self.config.runner_base_image.as_deref().ok_or_else(|| {
      BuildError::bare("SECTR_RUNNER_BASE_IMAGE is not configured — no base image to build on")
    })?;
    let tag = image_tag(req.deployment_id, req.version);

    // 1. Unpack the tarball into the build workspace.
    tokio::fs::create_dir_all(workdir)
      .await
      .map_err(|e| BuildError::bare(format!("mkdir build dir: {e}")))?;
    unpack_tarball(self.store.as_ref(), &req.artifact_uri, workdir).await?;

    // 2. docker build (deps bake at build time when the app ships
    //    a pyproject.toml — the `if [ -f …]` guard keeps dependency-free
    //    apps building; the runner then never needs the network).
    let dockerfile = generate_dockerfile(base_image);
    tokio::fs::write(workdir.join("Dockerfile"), dockerfile)
      .await
      .map_err(|e| BuildError::bare(format!("write Dockerfile: {e}")))?;
    let context = build_context(workdir)?;
    let docker = self
      .docker
      .as_ref()
      .ok_or_else(|| BuildError::bare("no docker daemon available — deploy requires one"))?;
    docker
      .build_image(&tag, "Dockerfile", context)
      .await
      .map_err(|e| BuildError {
        logs_tail: build_log_tail(&e),
        message: e,
      })?;

    // 3. Authoritative manifest: a real import inside the built image.
    //    The manifest comes back in a file copied out of the container
    //    (import may print to stdout). Import uses the SAME interpreter
    //    and user as the runner (`deploy.rs` composes the runner's command
    //    from `uses_uv`, below): `uv run --no-sync` when the app ships a
    //    pyproject — the deps were `uv sync`ed into /app/.venv at build,
    //    which system python3 cannot see (this is why dep-bearing apps
    //    422'd on first deploy).
    let has_pyproject = inspect::uses_uv(&workdir.join("app"));
    let import_cmd = inspect::import_command(has_pyproject, "python3");
    let import = docker
      .run_once(OneShotRun {
        image: tag.clone(),
        env: vec![
          "SECTR_MANIFEST=1".into(),
          format!("SECTR_ENTRY={}", req.entry),
          format!("SECTR_MANIFEST_OUT={MANIFEST_OUT}"),
          format!("HOME={RUNNER_HOME}"),
        ],
        cmd: import_cmd,
        working_dir: Some("/app".into()),
        user: Some(RUNNER_USER.into()),
        output_path: MANIFEST_OUT.into(),
      })
      .await
      .map_err(BuildError::bare)?;
    if import.exit_code != 0 {
      return Err(BuildError {
        message: format!(
          "authoritative manifest import failed (exit {}): {}",
          import.exit_code,
          import
            .logs
            .lines()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .join(" | ")
        ),
        logs_tail: import.logs,
      });
    }
    let output = import.output_file.ok_or_else(|| {
      BuildError::bare(format!(
        "manifest output missing: the import exited 0 but wrote no file at {MANIFEST_OUT}"
      ))
    })?;
    let imported: Value = serde_json::from_slice(&output)
      .map_err(|e| BuildError::bare(format!("manifest output is not JSON: {e}")))?;

    Ok(BuildOutput {
      image_ref: tag,
      imported_manifest: imported,
      uses_uv: has_pyproject,
    })
  }
}

#[async_trait]
impl RunnerPlatform for DockerPlatform {
  /// Model (a): the built image lives in the local daemon under the
  /// deterministic tag — that tag is the provisional build_ref.
  fn provisional_build_ref(&self, deployment_id: Uuid, version: i64) -> String {
    image_tag(deployment_id, version)
  }

  async fn build(&self, req: BuildRequest) -> Result<BuildOutput, BuildError> {
    let workdir: PathBuf =
      Path::new(&self.config.build_dir).join(format!("build-{}", req.deployment_id));
    let result = self.build_in(&workdir, &req).await;
    // The build workspace is ephemeral (the image is the durable artifact;
    // the tarball stays in the artifact store).
    tokio::fs::remove_dir_all(&workdir).await.ok();
    result
  }

  async fn spawn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    invocation_id: Uuid,
    ingest_token: &str,
  ) -> Result<Option<String>, DispatchError> {
    let manifest = self
      .registry
      .get_deployment(deployment_id)
      .await
      .map_err(|e| DispatchError::Unavailable(format!("deployment fetch: {e}")))?
      .manifest;
    let run = container_run_for(
      session,
      deployment_id,
      invocation_id,
      ingest_token,
      &manifest,
      &self.config,
    )?;
    let docker = self.docker.as_ref().ok_or_else(|| {
      DispatchError::Spawn("no docker daemon available — spawn requires one".into())
    })?;
    // An existence CHECK, as on E2B. Images get pruned, and a build is too
    // slow to run silently here.
    match docker.image_exists(&run.image).await {
      Ok(true) => {}
      Ok(false) => {
        return Err(DispatchError::Spawn(format!(
          "image {} for deployment {deployment_id} does not exist: redeploy the app",
          run.image
        )));
      }
      Err(e) => return Err(DispatchError::Spawn(e.to_string())),
    }
    let name = run.name.clone();
    docker
      .spawn_container(run)
      .await
      .map_err(|e| DispatchError::Spawn(e.to_string()))?;
    tracing::info!(session = %session, container = name.as_deref().unwrap_or_default(), "runner container spawned");
    Ok(name)
  }

  async fn teardown(&self, session: Uuid, handle: Option<&str>) {
    let Some(name) = handle else {
      tracing::debug!(
        session = %session,
        "no container handle (spawn raced a boundary or a restart dropped the grant): nothing to kill; a leftover runner gets a 401 and exits, or the reaper removes it"
      );
      return;
    };
    let Some(docker) = self.docker.as_ref() else {
      tracing::debug!(session = %session, "dockerless platform: teardown is a no-op");
      return;
    };
    if let Err(e) = docker.kill(name).await {
      // A never-started container (spawn failed earlier) is 404 —
      // kill is idempotent; anything else is worth a warning only:
      // teardown failures must never fail the turn-end path.
      tracing::warn!(session = %session, container = %name, error = %e, "container teardown failed");
    } else {
      tracing::info!(session = %session, container = %name, "runner container torn down");
    }
  }

  async fn list_runners(&self) -> Result<Option<Vec<RunnerInfo>>, DispatchError> {
    let Some(docker) = self.docker.as_ref() else {
      return Ok(None);
    };
    let server_tag = runner_server_tag(&self.config)
      .map_err(|e| DispatchError::Unavailable(format!("list containers: {e}")))?;
    let listed = docker
      .list_by_label(LABEL_SERVER, &server_tag)
      .await
      .map_err(|e| DispatchError::Unavailable(e.to_string()))?;
    let mut runners = Vec::new();
    for container in &listed {
      // The daemon's filter should already guarantee this. Correctness never
      // rests on it, because a wrong answer here would kill another server's
      // runners.
      if container.labels.get(LABEL_SERVER) != Some(&server_tag) {
        tracing::warn!(
          container = %container.name,
          server = %server_tag,
          "container listing returned an entry not tagged with this server; skipping it"
        );
        continue;
      }
      match runner_from_listed(container) {
        Some(runner) => runners.push(runner),
        None => {
          tracing::debug!(container = %container.name, "skipping an uninterpretable container listing entry")
        }
      }
    }
    Ok(Some(runners))
  }
}

/// Pull the tarball from the artifact store and unpack it under
/// `workdir/app/` (the docker build context).
async fn unpack_tarball(
  store: &dyn ArtifactStore,
  artifact_uri: &str,
  workdir: &Path,
) -> Result<(), BuildError> {
  let bytes = store
    .get(artifact_uri)
    .await
    .map_err(|e| BuildError::bare(format!("artifact: {e}")))?;
  let app_dir = workdir.join("app");
  tokio::fs::create_dir_all(&app_dir)
    .await
    .map_err(|e| BuildError::bare(format!("mkdir: {e}")))?;
  let gz = flate2::read::GzDecoder::new(&bytes[..]);
  let mut archive = tar::Archive::new(gz);
  archive.set_overwrite(true);
  archive
    .unpack(&app_dir)
    .map_err(|e| BuildError::bare(format!("tarball unpack: {e}")))?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn config(image: Option<&str>) -> ServerConfig {
    ServerConfig {
      endpoints: Default::default(),
      pg_url: "postgres://x".into(),
      bind_addr: "0.0.0.0:8080".parse().unwrap(),
      secrets_master_key: None,
      docker_socket: "/var/run/docker.sock".into(),
      bootstrap_platform_key: None,
      lease_ttl_secs: 120,
      runner_server_url: "ws://172.17.0.1:8080/ingest".into(),
      runner_sidecar_cmd: vec!["/usr/local/bin/sectr-worker".into()],
      runner_image: image.map(str::to_owned),
      runner_base_image: None,
      runner_network: None,
      runner_resources: Default::default(),
      build_dir: "/tmp/sectr-build".into(),
      dashboard_url: "https://app.sectr.test".into(),
      org_name: "sectr".into(),
      tail_pool_max: 32,
      platform: crate::config::PlatformKind::Docker,
      posthog_key: None,
      posthog_host: "https://us.i.posthog.com".into(),
      signup_webhook_url: None,
      clerk: None,
      limits: crate::limits::LimitsConfig::none(),
      artifact_backend: crate::config::ArtifactBackend::Fs,
      artifact_dir: "/tmp/sectr-test-artifacts".into(),
      gcs_bucket: None,
      gcs_endpoint: None,
      gcp_project_id: None,
      gcp_region: None,
      gcp_ar_repo: "sectr-runners".into(),
      gcp_build_sa_email: None,
      gcp_push_sa_email: None,
      gcp_runner_sa_email: None,
      e2b_api_key: None,
      e2b_domain: "e2b.app".into(),
      e2b_cpu_count: 2,
      e2b_memory_mb: 1024,
      e2b_sandbox_timeout_secs: 3600,
      e2b_puller_sa_json: None,
      user_token_ttl_secs: 86_400,
      mint_rate_limit_per_hour: 240,
    }
  }

  fn run_for(config: &ServerConfig, manifest: &Value) -> Result<ContainerRun, DispatchError> {
    container_run_for(
      Uuid::now_v7(),
      Uuid::now_v7(),
      Uuid::now_v7(),
      "sit_x",
      manifest,
      config,
    )
  }

  /// Sharing the app's lines with endpoint images must not change a managed
  /// build's Dockerfile by one byte.
  #[test]
  fn the_managed_dockerfile_is_unchanged() {
    assert_eq!(
      generate_dockerfile("sectr/runner:latest"),
      "FROM sectr/runner:latest\nCOPY app /app\nWORKDIR /app\n\
       RUN ln -s /opt/sdk-python /sdk-python\n\
       RUN if [ -f pyproject.toml ]; then sed -i 's#\\.\\./\\.\\./sdk-python#/sdk-python#g' pyproject.toml uv.lock; uv sync --frozen; fi\n\
       RUN chmod -R a+rX /app\n"
    );
  }

  #[test]
  fn manifest_image_wins_over_default() {
    let config = config(Some("fallback:latest"));
    let manifest = serde_json::json!({"runner": {"image": "app:42", "command": ["python"]}});
    assert_eq!(run_for(&config, &manifest).unwrap().image, "app:42");
  }

  #[test]
  fn default_image_used_without_manifest_image() {
    let config = config(Some("fallback:latest"));
    let manifest = serde_json::json!({"runner": {"command": ["python"]}});
    assert_eq!(
      run_for(&config, &manifest).unwrap().image,
      "fallback:latest"
    );
  }

  #[test]
  fn no_image_anywhere_is_a_spawn_error() {
    let config = config(None);
    let manifest = serde_json::json!({"runner": {"command": ["python"]}});
    let err = run_for(&config, &manifest).unwrap_err();
    assert!(err.to_string().contains("no runner image"), "{err}");
  }

  #[test]
  fn an_empty_sidecar_command_is_a_spawn_error() {
    let mut config = config(Some("img:1"));
    config.runner_sidecar_cmd = Vec::new();
    let err = run_for(&config, &serde_json::json!({})).unwrap_err();
    assert!(
      err.to_string().contains("SECTR_RUNNER_SIDECAR_CMD"),
      "{err}"
    );
  }

  #[test]
  fn env_holds_only_the_ingest_wiring_and_home() {
    let config = config(Some("img:1"));
    let run = run_for(&config, &serde_json::json!({})).unwrap();
    let env: std::collections::HashMap<&str, &str> = run
      .env
      .iter()
      .map(|(k, v)| (k.as_str(), v.as_str()))
      .collect();
    assert_eq!(env["SECTR_INGEST_URL"], "ws://172.17.0.1:8080/ingest");
    assert_eq!(env["SECTR_INGEST_TOKEN"], "sit_x");
    assert_eq!(env["HOME"], "/tmp");
    assert_eq!(
      env.len(),
      3,
      "secrets and platform credentials must NOT ride the container env"
    );
  }

  #[test]
  fn each_turn_gets_its_own_name_and_all_four_labels() {
    let config = config(Some("img:1"));
    let session = Uuid::now_v7();
    let deployment = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let run = container_run_for(
      session,
      deployment,
      first,
      "sit_x",
      &serde_json::json!({}),
      &config,
    )
    .unwrap();
    assert_eq!(
      run.name.as_deref(),
      Some(format!("sectr-runner-{}-{}", session.simple(), first.simple()).as_str())
    );
    assert_ne!(
      runner_container_name(session, first),
      runner_container_name(session, second),
      "a new turn must not collide with another turn's container"
    );
    assert_eq!(run.labels[LABEL_SESSION], session.to_string());
    assert_eq!(run.labels[LABEL_DEPLOYMENT], deployment.to_string());
    assert_eq!(run.labels[LABEL_INVOCATION], first.to_string());
    assert_eq!(run.labels[LABEL_SERVER], "172.17.0.1:8080");
    assert_eq!(run.labels.len(), 4);
  }

  #[test]
  fn network_user_and_limits_pass_through() {
    let mut config = config(Some("img:1"));
    config.runner_network = Some("sectr-runners".into());
    config.runner_resources = crate::config::RunnerResources {
      cpu_count: 0.5,
      memory_mb: 2048,
      timeout_secs: 60,
    };
    let run = run_for(&config, &serde_json::json!({})).unwrap();
    assert_eq!(run.network.as_deref(), Some("sectr-runners"));
    assert_eq!(run.user.as_deref(), Some("1000:1000"));
    assert_eq!(run.memory, Some(2048 * 1024 * 1024));
    assert_eq!(run.nano_cpus, Some(500_000_000));
    assert_eq!(
      run.cmd,
      ["timeout", "-k", "10", "60", "/usr/local/bin/sectr-worker"]
    );
    assert!(run.auto_remove);
  }

  #[test]
  fn no_network_setting_keeps_the_default_bridge() {
    let config = config(Some("img:1"));
    assert_eq!(
      run_for(&config, &serde_json::json!({})).unwrap().network,
      None
    );
  }

  #[test]
  fn two_installs_on_one_daemon_get_different_server_tags() {
    let mut one = config(Some("img:1"));
    one.runner_server_url = "ws://sectr-server:8080/ingest".into();
    one.runner_network = Some("one-runners".into());
    let mut two = one.clone();
    two.runner_network = Some("two-runners".into());
    assert_eq!(
      runner_server_tag(&one).unwrap(),
      "sectr-server:8080@one-runners"
    );
    assert_ne!(
      runner_server_tag(&one).unwrap(),
      runner_server_tag(&two).unwrap()
    );

    // Two local servers on one host differ by port.
    let mut local = config(Some("img:1"));
    local.runner_server_url = "ws://host.docker.internal:8080/ingest".into();
    let mut other = local.clone();
    other.runner_server_url = "ws://host.docker.internal:8081/ingest".into();
    assert_ne!(
      runner_server_tag(&local).unwrap(),
      runner_server_tag(&other).unwrap()
    );

    let mut bad = config(Some("img:1"));
    bad.runner_server_url = "not a url".into();
    assert!(runner_server_tag(&bad).is_err());
  }

  fn listed(labels: &[(&str, &str)]) -> ListedContainer {
    ListedContainer {
      name: "sectr-runner-x".into(),
      labels: labels
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect(),
      created: 1_700_000_000,
    }
  }

  #[test]
  fn listed_containers_become_runner_info() {
    let session = Uuid::now_v7();
    let invocation = Uuid::now_v7();
    let runner = runner_from_listed(&listed(&[
      (LABEL_SESSION, &session.to_string()),
      (LABEL_INVOCATION, &invocation.to_string()),
    ]))
    .unwrap();
    assert_eq!(runner.handle, "sectr-runner-x");
    assert_eq!(runner.session, session);
    assert_eq!(runner.invocation, Some(invocation));
    assert_eq!(runner.started_at.timestamp(), 1_700_000_000);

    // A bad invocation is still a runner: the reaper's ownership check
    // treats it as not matching any lease.
    let runner = runner_from_listed(&listed(&[
      (LABEL_SESSION, &session.to_string()),
      (LABEL_INVOCATION, "garbage"),
    ]))
    .unwrap();
    assert_eq!(runner.invocation, None);

    // No usable session: never acted on.
    assert_eq!(runner_from_listed(&listed(&[])), None);
    assert_eq!(
      runner_from_listed(&listed(&[(LABEL_SESSION, "garbage")])),
      None
    );
  }

  #[test]
  fn the_build_context_keeps_symlinks_and_never_reads_their_targets() {
    let dir = std::env::temp_dir().join(format!("sectr-context-{}", Uuid::now_v7()));
    std::fs::create_dir_all(dir.join("app")).unwrap();
    std::fs::write(dir.join("Dockerfile"), "FROM x\n").unwrap();
    std::fs::write(dir.join("app/main.py"), "print(1)\n").unwrap();
    let secret = dir.join("outside-the-app");
    std::fs::write(&secret, "server-secret").unwrap();
    std::os::unix::fs::symlink(&secret, dir.join("app/leak")).unwrap();

    let context = build_context(&dir).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();

    let contents = String::from_utf8_lossy(&context);
    assert!(
      !contents.contains("server-secret"),
      "the symlink's target must not enter the build context"
    );
    let mut archive = tar::Archive::new(&context[..]);
    let leak = archive
      .entries()
      .unwrap()
      .map(Result::unwrap)
      .find(|entry| entry.path().unwrap().ends_with("leak"))
      .expect("the symlink itself is kept");
    assert!(leak.header().entry_type().is_symlink());
  }

  #[tokio::test]
  async fn a_none_handle_is_a_no_op_and_needs_no_daemon() {
    let Some(store) = crate::test_pg::fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let platform = DockerPlatform::new(
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      None,
      config(Some("img:1")),
      Arc::new(store.registry()),
    );
    platform.teardown(Uuid::now_v7(), None).await;
    assert_eq!(platform.list_runners().await.unwrap(), None);
  }
}
