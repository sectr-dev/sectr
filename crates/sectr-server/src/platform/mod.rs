//! The runner-platform seam: build and runner-launch are ONE trait —
//! you cannot compose arbitrary builder×spawner pairs (a local docker tag
//! feeds only the same-host docker spawner; an AR ref feeds Cloud Run), so
//! one object per backend implements all three methods and the deploy path
//! and the dispatch path both take the same object.
//!
//! Absorbs the former `ImageBuilder` (sectr-common `drivers::build`) and
//! the former `SessionSpawner`/`SessionTeardown` seams (ingest.rs +
//! spawn.rs). Manifest composition stays in `deploy.rs` (it is
//! cloud-neutral); only HOW the image is produced, the manifest import is
//! run, containers are launched and torn down differs per backend
//! ([`docker::DockerPlatform`] on the daemon today; the model-(b) build half
//! lives in [`gcp_build::GcpPlatform`], with spawn/teardown in `gcp_run`).
//!
//! Like the other drivers, consumers are the server-role layer; the
//! turn-lifecycle core never touches this.
//!
//! Image-tag convention all platforms must follow (the R4 contract anchor):
//! `sectr-app-<deployment-id>:v<version>` for docker tags; remote registries
//! append their repo path but keep the `sectr-app-<id>:v<version>` tail.
//! Target architecture: linux/amd64 for the remote builders (Cloud Run's default,
//! and E2B's). The Docker backend builds for its daemon's architecture. The base
//! image on GHCR is amd64 and arm64, and every backend builds on it.

pub mod docker;
#[cfg(feature = "e2b")] // the E2B sandbox execution backend
pub mod e2b;
#[cfg(feature = "gcp")] // the model-(b) build half
pub mod gcp_build;
#[cfg(feature = "gcp")] // the model-(b) execution half
pub mod gcp_run;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::dispatch::DispatchError;

pub use docker::DockerPlatform;

#[cfg(feature = "e2b")]
pub use e2b::E2bPlatform;
#[cfg(feature = "gcp")]
pub use gcp_build::GcpPlatform;

/// The host a runner-server URL points at (`ws://ingest.sectr.dev/ingest`
/// → `ingest.sectr.dev`). Backends tag their runners with it.
pub(crate) fn server_host_of(runner_server_url: &str) -> anyhow::Result<String> {
  reqwest::Url::parse(runner_server_url)
    .ok()
    .and_then(|u| u.host_str().map(str::to_owned))
    .ok_or_else(|| {
      anyhow::anyhow!(
        "SECTR_RUNNER_SERVER_URL {runner_server_url:?} has no host (runners are tagged with it \
         so the orphan reaper only touches its own)"
      )
    })
}

/// The per-backend runner platform: build (deploy path) + spawn/teardown
/// (dispatch/ingest path). One object, selected by the backend flag.
#[async_trait]
pub trait RunnerPlatform: Send + Sync {
  /// Build the deployment's runner image and extract the authoritative
  /// manifest (real import, SECTR_MANIFEST=1). Returns the
  /// provider-agnostic result; the caller composes the final manifest
  /// (routes/env/runner.*/approval_tools). Model (b): the impl ALSO
  /// ensures the Cloud Run Job exists here, after the image lands in AR
  /// (a failed ensure fails the deploy before routing takes) — job
  /// lifecycle = deployment lifecycle.
  async fn build(&self, req: BuildRequest) -> Result<BuildOutput, BuildError>;
  /// The provisional in-flight-build reference to store in
  /// `deployments.build_ref` BEFORE the background build spawns — an
  /// opaque identifier the platform can later use to recognize its own
  /// in-flight build (model a: the docker tag; model b: the AR ref).
  fn provisional_build_ref(&self, deployment_id: Uuid, version: i64) -> String;
  /// Launch the turn. Model (a): run the container from the local tag.
  /// Model (b): ensure-job-if-absent (self-healing) + run-execution with
  /// per-execution env (ingest URL + the per-turn token). Fire-and-forget
  /// — the sidecar dials the ingest path; completion arrives via ingest.
  /// Returns the platform's OPAQUE handle for the running invocation:
  /// Cloud Run's execution resource name, the E2B sandbox id, or the Docker
  /// container name. No platform types leak past this seam.
  async fn spawn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    invocation_id: Uuid,
    ingest_token: &str,
  ) -> Result<Option<String>, DispatchError>;
  /// Turn-boundary teardown. `handle` = whatever [`spawn`](Self::spawn)
  /// returned for the CURRENT invocation, carried by the hub's grant
  /// (`None` when no spawn result was recorded: nothing to tear down).
  /// Model (a): kill the container the handle names. Model (b): cancel the
  /// execution — only meaningful on suspension/reclaim; the sentinel
  /// ends the execution by itself, so canceling an already-finished
  /// execution is a no-op. Log-not-fail, idempotent.
  async fn teardown(&self, session: Uuid, handle: Option<&str>);
  /// Whether the platform's runner resource ends ITSELF when the sidecar
  /// exits: a docker container dies
  /// with its main process and a Cloud Run execution ends with the
  /// sidecar — but an E2B sandbox does NOT (it bills until its timeout),
  /// so the hub's ONE boundary-policy point tears it down at every turn
  /// boundary (after the ack). Platform-capability introspection feeding
  /// the uniform policy body — not a second policy point. Default `true`.
  fn self_terminating(&self) -> bool {
    true
  }
  /// Live runners this platform (and THIS server environment) owns, for
  /// the orphan reaper. `Ok(None)` = the backend cannot list
  /// them (the reaper then exits quietly); implementations must filter to
  /// their own server's runners server-side, never list a shared account
  /// wholesale.
  async fn list_runners(&self) -> Result<Option<Vec<RunnerInfo>>, DispatchError> {
    Ok(None)
  }
}

/// One live runner as the reaper sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerInfo {
  /// The platform's opaque teardown handle (the same value `spawn`
  /// returned).
  pub handle: String,
  pub session: Uuid,
  /// The invocation the runner was spawned for, from its metadata.
  pub invocation: Option<Uuid>,
  pub started_at: chrono::DateTime<chrono::Utc>,
}

/// One build job: everything the builder needs, nothing about HOW the
/// image gets produced.
pub struct BuildRequest {
  pub deployment_id: Uuid,
  /// The image tag needs it (`sectr-app-<id>:v<version>`).
  pub version: i64,
  /// The tarball in the ArtifactStore (opaque URI — `fs:`, `gs:`, …).
  /// Pairing constraint: the URI scheme must be readable by the SELECTED
  /// platform (one backend owns both sides — artifact store + runner —
  /// so the pairing is structural, not a composition matrix).
  pub artifact_uri: String,
  /// From sectr.toml (`agent.entry`) — the authoritative-manifest import
  /// points sectr.inspect at it.
  pub entry: String,
}

pub struct BuildOutput {
  /// Pullable where the runner runs — the local docker tag today, an
  /// Artifact Registry ref for a remote builder.
  pub image_ref: String,
  /// Routes/env/python from sectr.inspect — the server composes the
  /// final manifest from this (cloud-neutral composition).
  pub imported_manifest: Value,
  /// App shipped pyproject.toml — the server composes
  /// `uv run --no-sync …` from this (with a REMOTE builder the server
  /// never sees the unpacked tree, so the builder must report it;
  /// deploy.rs derives it from the tarball today).
  pub uses_uv: bool,
}

/// Failure carries the logs — `BuildOutput` is only returned on success,
/// so putting logs there would make them unreachable exactly when they
/// matter. `message` is the human-readable failure (the deploy row's
/// `error` is computed from it); `logs_tail` is the structured tail for
/// CLI/dashboard surfacing.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct BuildError {
  pub message: String,
  pub logs_tail: String,
}

impl BuildError {
  /// A failure with no separate log tail (the message is all there is).
  pub fn bare(message: impl Into<String>) -> Self {
    BuildError {
      message: message.into(),
      logs_tail: String::new(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn server_host_is_the_runner_url_host() {
    assert_eq!(
      server_host_of("wss://ingest.sectr.dev/ingest").unwrap(),
      "ingest.sectr.dev"
    );
    assert_eq!(
      server_host_of("ws://172.17.0.1:8080/ingest").unwrap(),
      "172.17.0.1"
    );
    assert!(server_host_of("not a url").is_err());
  }
}
