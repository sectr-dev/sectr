//! [`GcpPlatform`] — the Cloud Run Jobs EXECUTION half: `spawn` =
//! ensure-job-if-absent (self-healing) + `runs.jobs.run` with per-execution
//! env overrides (the ingest URL + the per-turn ingest token), fire-and-
//! forget; `teardown` = cancel-execution, log-not-fail, idempotent.
//!
//! # Semantics
//!
//! - **No deterministic execution id**: `RunJobRequest` carries no
//!   client-settable execution id (verified against job.proto + this
//!   crate — execution ids are server-assigned, OUTPUT_ONLY). The hub's
//!   grant check is the double-dispatch protection; the restart-window
//!   second-execution case is covered by the 401-orphan self-clean path.
//! - **The execution NAME comes from the RunJob operation metadata** (the
//!   metadata type IS `Execution`, carrying its resource name as soon as
//!   the execution exists): spawn performs exactly ONE poller cycle
//!   (send + read the initial state) and does NOT poll to completion
//!   (fire-and-forget); completion arrives via ingest. The name is returned
//!   as the OPAQUE handle and rides the hub's grant for cancellation.
//! - **Self-healing ensure**: the job template IS the deployment, so a
//!   missing job is recreated from the deployment's composed manifest
//!   (`runner.image`) + this platform's config (sidecar cmd, ingest URL,
//!   runner SA) before the run.
//!
//! # Per-job concurrent-executions quota
//!
//! The ceiling for concurrent turns on one deployment is the JOB's
//! concurrent-execution quota (regional quota
//! `run.googleapis.com/jobsMaxConcurrentExecutions`, default 1000 per
//! job). Verify live with:
//! `gcloud run jobs describe <job> --region us-east4 \
//!    --format='value(runExecutionCount)'`
//! At our scale the default is far away; raise it or shard per-app if a
//! deployment ever approaches it.

use async_trait::async_trait;
use google_cloud_lro::{Poller as _, PollingResult};
use uuid::Uuid;

use google_cloud_run_v2::model as run;

use crate::dispatch::DispatchError;
use crate::platform::gcp_build::{GcpPlatform, job_id};
use crate::platform::{BuildError, BuildOutput, BuildRequest, RunnerPlatform};

/// A run-execution request for one turn: the job's resource name + the
/// per-execution env overrides (BOTH the ingest URL and the fresh
/// per-turn token — ContainerOverride env MERGES with the template's env,
/// so the URL would survive the token alone, but carrying both is
/// explicit and robust). Args are NOT touched: the job template's sidecar
/// cmd stands. Pure — unit-tested.
fn run_request(job_name: &str, ingest_url: &str, ingest_token: &str) -> run::RunJobRequest {
  let env = vec![
    run::EnvVar::default()
      .set_name("SECTR_INGEST_URL")
      .set_value(ingest_url.to_owned()),
    run::EnvVar::default()
      .set_name("SECTR_INGEST_TOKEN")
      .set_value(ingest_token.to_owned()),
  ];
  run::RunJobRequest::default()
    .set_name(job_name.to_owned())
    .set_overrides(
      run::run_job_request::Overrides::default().set_container_overrides(vec![
        run::run_job_request::overrides::ContainerOverride::default().set_env(env),
      ]),
    )
}

/// Pull the server-assigned execution resource name out of ONE poller
/// cycle's result. `InProgress` carries the Execution as metadata; a
/// (pathological) already-completed first cycle carries it as the
/// response. `None` = no name available — the caller surfaces it as a
/// spawn failure. Pure — unit-tested.
fn execution_name_from_poll(
  result: &PollingResult<run::Execution, run::Execution>,
) -> Option<String> {
  let execution = match result {
    PollingResult::InProgress(Some(execution)) => execution,
    PollingResult::Completed(Ok(execution)) => execution,
    _ => return None,
  };
  (!execution.name.is_empty()).then(|| execution.name.clone())
}

/// A cancel error on an already-finished execution (the sentinel ended
/// it, or it self-cleaned via the 401 path) — canceling then is a no-op
/// by contract (log-not-fail, idempotent). Pure — unit-tested.
fn cancel_is_noop(e: &google_cloud_gax::error::Error) -> bool {
  e.status().is_some_and(|s| {
    matches!(
      s.code,
      google_cloud_gax::error::rpc::Code::NotFound
        | google_cloud_gax::error::rpc::Code::FailedPrecondition
    )
  })
}

impl GcpPlatform {
  /// The job's resource name (one job per deployment,
  /// `sectr-app-<deployment-id>`).
  fn job_resource_name(&self, deployment_id: Uuid) -> String {
    format!(
      "projects/{}/locations/{}/jobs/{}",
      self.project_id,
      self.region,
      job_id(deployment_id)
    )
  }

  /// `runs.jobs.run` for one turn: fire-and-forget — returns the
  /// execution's resource name as soon as the execution EXISTS (the
  /// sidecar dials the ingest path; completion arrives via ingest).
  async fn run_execution(
    &self,
    job_name: &str,
    ingest_url: &str,
    ingest_token: &str,
  ) -> Result<String, DispatchError> {
    let mut poller = self
      .jobs
      .run_job()
      .with_request(run_request(job_name, ingest_url, ingest_token))
      .poller();
    // Exactly ONE cycle: sends the request and reads the initial
    // state. Dropping the poller afterwards leaves the execution
    // running — no completion polling, by design.
    let first = poller
      .poll()
      .await
      .ok_or_else(|| DispatchError::Spawn(format!("run job {job_name}: poller exhausted")))?;
    execution_name_from_poll(&first).ok_or_else(|| {
      DispatchError::Spawn(format!(
        "run job {job_name}: the operation carried no execution name \
                 (the execution may still have been created — check the console)"
      ))
    })
  }
}

#[async_trait]
impl RunnerPlatform for GcpPlatform {
  /// Model (b): the image lives in Artifact Registry under the R4 ref —
  /// that ref is the provisional build_ref. Delegates to the build half
  /// (gcp_build.rs).
  fn provisional_build_ref(&self, deployment_id: Uuid, version: i64) -> String {
    self.provisional_build_ref_impl(deployment_id, version)
  }

  async fn build(&self, req: BuildRequest) -> Result<BuildOutput, BuildError> {
    self.build_impl(req).await
  }

  async fn spawn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    _invocation_id: Uuid,
    ingest_token: &str,
  ) -> Result<Option<String>, DispatchError> {
    // Self-healing ensure (plan: ensure-if-absent before run-
    // execution). The job template IS the deployment; if it is absent,
    // recreate it from the deployment's composed manifest
    // (`runner.image` — the composed manifest carries the AR ref) plus
    // this platform's config (sidecar cmd, ingest URL, runner SA).
    let manifest = self
      .registry
      .get_deployment(deployment_id)
      .await
      .map_err(|e| DispatchError::Unavailable(format!("deployment fetch: {e}")))?
      .manifest;
    let image_ref = manifest
      .get("runner")
      .and_then(|r| r.get("image"))
      .and_then(|i| i.as_str())
      .ok_or_else(|| {
        DispatchError::Spawn(format!(
          "deployment {deployment_id} manifest has no runner.image — \
                     cannot ensure the job exists"
        ))
      })?
      .to_owned();
    self
      .ensure_job(deployment_id, &image_ref)
      .await
      .map_err(|e| DispatchError::Spawn(e.to_string()))?;

    let job_name = self.job_resource_name(deployment_id);
    let execution = self
      .run_execution(&job_name, &self.runner_server_url, ingest_token)
      .await?;
    tracing::info!(session = %session, execution = %execution, "cloud run execution started");
    // The execution name is the teardown handle; the hub carries it on
    // the turn's grant.
    Ok(Some(execution))
  }

  async fn teardown(&self, session: Uuid, handle: Option<&str>) {
    // The sentinel ends the execution by itself — teardown only fires
    // on suspension/reclaim, and canceling an already-finished
    // execution is a no-op. Log-not-fail: teardown must never fail the
    // turn-end path.
    let Some(execution) = handle else {
      tracing::debug!(session = %session, "no execution handle (spawn raced a boundary?) — nothing to cancel");
      return;
    };
    match self
      .executions
      .cancel_execution()
      .set_name(execution.to_owned())
      .send()
      .await
    {
      Ok(_) => {
        tracing::info!(session = %session, execution = %execution, "cloud run execution cancelled");
      }
      Err(e) if cancel_is_noop(&e) => {
        tracing::debug!(session = %session, execution = %execution, "execution already finished — cancel is a no-op");
      }
      Err(e) => {
        tracing::warn!(session = %session, execution = %execution, error = %e, "execution cancel failed");
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const JOB: &str = "projects/example-project-123456/locations/us-east4/jobs/sectr-app-01a0ab54-c313-72a3-a4d5-c6e38b577282";
  const EXECUTION: &str = "projects/example-project-123456/locations/us-east4/jobs/sectr-app-01a0ab54-c313-72a3-a4d5-c6e38b577282/executions/exec-123";

  #[test]
  fn run_request_carries_name_and_both_env_overrides() {
    let req = run_request(
      JOB,
      "wss://ingest.example.com/ingest",
      "sit_test-token-bytes",
    );
    assert_eq!(req.name, JOB);
    let overrides = req.overrides.expect("overrides must be set");
    let containers = overrides.container_overrides;
    assert_eq!(containers.len(), 1, "one container override");
    let get = |name: &str| {
      containers[0]
        .env
        .iter()
        .find(|e| e.name == name)
        .map(|e| match e.values.as_ref() {
          Some(run::env_var::Values::Value(s)) => s.clone(),
          other => panic!("literal env value expected, got {other:?}"),
        })
        .expect("env var must be set")
    };
    assert_eq!(get("SECTR_INGEST_URL"), "wss://ingest.example.com/ingest");
    assert_eq!(get("SECTR_INGEST_TOKEN"), "sit_test-token-bytes");
    // Args are NOT touched: the job template's sidecar cmd stands.
    assert!(containers[0].args.is_empty());
    assert!(!containers[0].clear_args, "the sidecar cmd must survive");
  }

  #[test]
  fn execution_name_from_in_progress_metadata() {
    let execution = run::Execution::default().set_name(EXECUTION.to_owned());
    let result = PollingResult::InProgress(Some(execution));
    assert_eq!(
      execution_name_from_poll(&result).as_deref(),
      Some(EXECUTION)
    );
  }

  #[test]
  fn execution_name_from_a_pathological_first_cycle_completion() {
    let execution = run::Execution::default().set_name(EXECUTION.to_owned());
    let result = PollingResult::Completed(Ok(execution));
    assert_eq!(
      execution_name_from_poll(&result).as_deref(),
      Some(EXECUTION)
    );
  }

  #[test]
  fn missing_metadata_and_errors_yield_no_name() {
    assert!(execution_name_from_poll(&PollingResult::InProgress(None)).is_none());
    let service_error =
      google_cloud_gax::error::Error::service(google_cloud_gax::error::rpc::Status::default());
    assert!(execution_name_from_poll(&PollingResult::PollingError(service_error)).is_none());
  }

  #[test]
  fn cancel_noop_covers_not_found_and_already_finished() {
    let not_found = google_cloud_gax::error::Error::service(
      google_cloud_gax::error::rpc::Status::default()
        .set_code(google_cloud_gax::error::rpc::Code::NotFound),
    );
    let finished = google_cloud_gax::error::Error::service(
      google_cloud_gax::error::rpc::Status::default()
        .set_code(google_cloud_gax::error::rpc::Code::FailedPrecondition),
    );
    let real_failure = google_cloud_gax::error::Error::service(
      google_cloud_gax::error::rpc::Status::default()
        .set_code(google_cloud_gax::error::rpc::Code::Internal),
    );
    assert!(cancel_is_noop(&not_found));
    assert!(cancel_is_noop(&finished));
    assert!(!cancel_is_noop(&real_failure));
  }
}
