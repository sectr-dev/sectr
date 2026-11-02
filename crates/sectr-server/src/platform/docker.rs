//! [`DockerPlatform`] — the model-(a) [`RunnerPlatform`]: everything runs
//! on the same host's docker daemon. `build` unpacks the
//! tarball from the artifact store into the build workspace, generates the
//! Dockerfile (`FROM $SECTR_RUNNER_BASE_IMAGE` + `COPY app /app`, deps
//! baked via `uv sync` when the app ships a pyproject.toml), `docker
//! build`, then gets the authoritative manifest via a one-shot `SECTR_MANIFEST=1`
//! import inside the built image. `spawn`/`teardown`: a
//! per-session runner container spawned via [`DockerSandboxDriver`]
//! (bollard), killed by deterministic name at turn end. All-docker; the
//! only host requirement is the socket.
//!
//! The swap-point discipline: everything that
//! makes this Docker-specific lives behind the [`RunnerPlatform`] seam — a
//! serverless driver (model (b)) implements the same three methods against
//! its provider's API and the control plane is unchanged. The GcpPlatform
//! reuses [`generate_dockerfile`] verbatim and swaps the daemon
//! calls for the Cloud Build API / the Cloud Run Jobs API.
//!
//! Container contents & env (spawn):
//! - image: the deployment manifest's `runner.image`, falling back to the
//!   server's `SECTR_RUNNER_IMAGE` (the build path pins per-deployment
//!   images; the fallback keeps dev/testing possible).
//! - cmd: the sidecar command (`SECTR_RUNNER_SIDECAR_CMD`; the base
//!   image's ENTRYPOINT is expected to be the sidecar — this overrides
//!   CMD).
//! - env: `SECTR_INGEST_URL` (the server as seen from the container) +
//!   `SECTR_INGEST_TOKEN` (per-turn: the grant dies at every
//!   turn boundary and the next dispatch spawns a fresh process with a
//!   fresh token). Secrets do NOT ride the
//!   container env: they arrive via the assignment, resolved per turn.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use sectr_common::drivers::ArtifactStore;
use sectr_common::drivers::sandbox::{ContainerRun, DockerSandboxDriver};
use sectr_common::registry::Registry;

use crate::config::ServerConfig;
use crate::dispatch::DispatchError;
use crate::platform::{BuildError, BuildOutput, BuildRequest, RunnerPlatform};

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
  format!(
    "FROM {base_image}\nCOPY app /app\nWORKDIR /app\n\
         RUN ln -s /opt/sdk-python /sdk-python\n\
         RUN if [ -f pyproject.toml ]; then sed -i 's#\\.\\./\\.\\./sdk-python#/sdk-python#g' pyproject.toml uv.lock; uv sync --frozen; fi\n\
         RUN chmod -R a+rX /app\n"
  )
}

/// Container naming: deterministic per session (idempotent re-spawn,
/// name-addressable teardown).
pub fn runner_container_name(session: Uuid) -> String {
  format!("sectr-runner-{}", session.simple())
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

/// Build the container spec for a session's runner. Pure — unit-tested.
pub fn container_run_for(
  session: Uuid,
  deployment_id: Uuid,
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
  let env = vec![
    (
      "SECTR_INGEST_URL".to_owned(),
      config.runner_server_url.clone(),
    ),
    ("SECTR_INGEST_TOKEN".to_owned(), ingest_token.to_owned()),
  ];
  Ok(ContainerRun {
    image,
    // Auto-remove: the sidecar exits itself at the turn boundary
    // (the sidecar policy) and nothing on the prod path waits
    // for its exit code — the daemon must sweep the husk.
    auto_remove: true,
    name: Some(runner_container_name(session)),
    labels: [
      ("sectr.session".to_owned(), session.to_string()),
      ("sectr.deployment".to_owned(), deployment_id.to_string()),
    ]
    .into_iter()
    .collect(),
    env,
    cmd: config.runner_sidecar_cmd.clone(),
  })
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
}

#[async_trait]
impl RunnerPlatform for DockerPlatform {
  /// Model (a): the built image lives in the local daemon under the
  /// deterministic tag — that tag is the provisional build_ref.
  fn provisional_build_ref(&self, deployment_id: Uuid, version: i64) -> String {
    image_tag(deployment_id, version)
  }

  async fn build(&self, req: BuildRequest) -> Result<BuildOutput, BuildError> {
    let base_image = self.config.runner_base_image.as_deref().ok_or_else(|| {
      BuildError::bare("SECTR_RUNNER_BASE_IMAGE is not configured — no base image to build on")
    })?;
    let tag = image_tag(req.deployment_id, req.version);

    // 1. Unpack the tarball into the build workspace. The manifest
    //    extraction bind-mounts a dir here — in compose, `build_dir` must
    //    be a volume shared between the server container and the daemon
    //    host (bind paths resolve on the daemon host).
    let workdir: PathBuf =
      Path::new(&self.config.build_dir).join(format!("build-{}", req.deployment_id));
    tokio::fs::create_dir_all(&workdir)
      .await
      .map_err(|e| BuildError::bare(format!("mkdir build dir: {e}")))?;
    unpack_tarball(self.store.as_ref(), &req.artifact_uri, &workdir).await?;

    // 2. docker build (deps bake at build time when the app ships
    //    a pyproject.toml — the `if [ -f …]` guard keeps dependency-free
    //    apps building; the runner then never needs the network).
    let dockerfile = generate_dockerfile(base_image);
    tokio::fs::write(workdir.join("Dockerfile"), dockerfile)
      .await
      .map_err(|e| BuildError::bare(format!("write Dockerfile: {e}")))?;
    let mut context = tar::Builder::new(Vec::new());
    context
      .append_path_with_name(workdir.join("Dockerfile"), "Dockerfile")
      .map_err(|e| BuildError::bare(format!("context tar: {e}")))?;
    context
      .append_dir_all("app", workdir.join("app"))
      .map_err(|e| BuildError::bare(format!("context tar: {e}")))?;
    let context = context
      .into_inner()
      .map_err(|e| BuildError::bare(format!("context tar: {e}")))?;
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
    //    The manifest comes back via a bind-mounted file (import may print
    //    to stdout); `build_dir` must be daemon-visible for the bind.
    //    Import uses the SAME interpreter discipline as the runner (which
    //    `deploy.rs` composes from `uses_uv`, below): `uv run --no-sync`
    //    when the app ships a pyproject — the deps were `uv sync`ed into
    //    /app/.venv at build, which system python3 cannot see (this is
    //    why dep-bearing apps 422'd on first deploy).
    let has_pyproject = workdir.join("app/pyproject.toml").is_file();
    let mut import_cmd = if has_pyproject {
      vec![
        "uv".into(),
        "run".into(),
        "--no-sync".into(),
        "python3".into(),
      ]
    } else {
      vec!["python3".into()]
    };
    import_cmd.extend(["-m".into(), "sectr.inspect".into()]);
    let manifest_out = workdir.join("manifest.json");
    let _ = tokio::fs::remove_file(&manifest_out).await;
    let binds = vec![format!(
      "{}:/out",
      manifest_out.parent().unwrap_or(Path::new("/tmp")).display()
    )];
    let (exit, logs) = docker
      .run_once(
        &tag,
        &[
          "SECTR_MANIFEST=1".into(),
          format!("SECTR_ENTRY={}", req.entry),
          "SECTR_MANIFEST_OUT=/out/manifest.json".into(),
        ],
        &import_cmd,
        &binds,
        Some("/app".into()),
      )
      .await
      .map_err(BuildError::bare)?;
    if exit != 0 {
      return Err(BuildError {
        logs_tail: logs.clone(),
        message: format!(
          "authoritative manifest import failed (exit {exit}): {}",
          logs.lines().rev().take(10).collect::<Vec<_>>().join(" | ")
        ),
      });
    }
    let imported: Value = tokio::fs::read_to_string(&manifest_out)
      .await
      .map_err(|e| BuildError::bare(format!("manifest output missing: {e}")))?
      .parse()
      .map_err(|e| BuildError::bare(format!("manifest output is not JSON: {e}")))?;

    // The build workspace is ephemeral (the image is the durable artifact;
    // the tarball stays in the artifact store).
    tokio::fs::remove_dir_all(&workdir).await.ok();
    Ok(BuildOutput {
      image_ref: tag,
      imported_manifest: imported,
      uses_uv: has_pyproject,
    })
  }

  async fn spawn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    _invocation_id: Uuid,
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
      ingest_token,
      &manifest,
      &self.config,
    )?;
    let docker = self.docker.as_ref().ok_or_else(|| {
      DispatchError::Spawn("no docker daemon available — spawn requires one".into())
    })?;
    docker
      .spawn_container(run)
      .await
      .map_err(|e| DispatchError::Spawn(e.to_string()))?;
    tracing::info!(session = %session, "runner container spawned");
    // DockerPlatform manages its own lifecycle (deterministic container
    // names) — no opaque handle for the hub to carry.
    Ok(None)
  }

  async fn teardown(&self, session: Uuid, _handle: Option<&str>) {
    let Some(docker) = self.docker.as_ref() else {
      tracing::debug!(session = %session, "dockerless platform: teardown is a no-op");
      return;
    };
    let name = runner_container_name(session);
    if let Err(e) = docker.kill(&name).await {
      // A never-started container (spawn failed earlier) is 404 —
      // kill is idempotent; anything else is worth a warning only:
      // teardown failures must never fail the turn-end path.
      tracing::warn!(session = %session, error = %e, "container teardown failed");
    } else {
      tracing::info!(session = %session, "runner container torn down");
    }
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

  #[test]
  fn manifest_image_wins_over_default() {
    let config = config(Some("fallback:latest"));
    let manifest = serde_json::json!({"runner": {"image": "app:42", "command": ["python"]}});
    let run = container_run_for(
      uuid::Uuid::now_v7(),
      uuid::Uuid::now_v7(),
      "sit_x",
      &manifest,
      &config,
    )
    .unwrap();
    assert_eq!(run.image, "app:42");
  }

  #[test]
  fn default_image_used_without_manifest_image() {
    let config = config(Some("fallback:latest"));
    let manifest = serde_json::json!({"runner": {"command": ["python"]}});
    let run = container_run_for(
      uuid::Uuid::now_v7(),
      uuid::Uuid::now_v7(),
      "sit_x",
      &manifest,
      &config,
    )
    .unwrap();
    assert_eq!(run.image, "fallback:latest");
  }

  #[test]
  fn no_image_anywhere_is_a_spawn_error() {
    let config = config(None);
    let manifest = serde_json::json!({"runner": {"command": ["python"]}});
    let err = container_run_for(
      uuid::Uuid::now_v7(),
      uuid::Uuid::now_v7(),
      "sit_x",
      &manifest,
      &config,
    )
    .unwrap_err();
    assert!(err.to_string().contains("no runner image"), "{err}");
  }

  #[test]
  fn env_carries_ingest_wiring_and_name_is_deterministic() {
    let config = config(Some("img:1"));
    let manifest = serde_json::json!({"runner": {"image": "app:1"}});
    let session = uuid::Uuid::now_v7();
    let deployment = uuid::Uuid::now_v7();
    let run = container_run_for(session, deployment, "sit_tok", &manifest, &config).unwrap();
    assert_eq!(run.name, Some(runner_container_name(session)));
    assert_eq!(
      runner_container_name(session),
      runner_container_name(session)
    );
    let env: std::collections::HashMap<&str, &str> = run
      .env
      .iter()
      .map(|(k, v)| (k.as_str(), v.as_str()))
      .collect();
    assert_eq!(env["SECTR_INGEST_URL"], "ws://172.17.0.1:8080/ingest");
    assert_eq!(env["SECTR_INGEST_TOKEN"], "sit_tok");
    assert_eq!(
      env.len(),
      2,
      "secrets and platform credentials must NOT ride the container env"
    );
    assert_eq!(run.labels["sectr.session"], session.to_string());
    assert_eq!(run.labels["sectr.deployment"], deployment.to_string());
    assert_eq!(run.cmd, vec!["/usr/local/bin/sectr-worker"]);
  }
}
