//! UNIT tests of the turn-boundary POLICY itself — not the WS
//! flow: both boundary kinds terminate, the grant's opaque execution handle
//! is handed to the platform teardown, and the grant dies with the turn
//! (the next dispatch mints a fresh token). The integration tests in
//! `tests.rs` assert the WS flow works end-to-end; THESE assert the policy
//! decision at the policy point (`IngestHub::on_turn_boundary`).

use std::sync::Arc;
use uuid::Uuid;

use sectr_common::registry::Registry;
use sectr_common::sidecar::{Assignment, RunnerSpec};

use crate::ingest::{IngestHub, TurnBoundary};
use crate::test_pg;

type TeardownCalls = Arc<std::sync::Mutex<Vec<(Uuid, Option<String>)>>>;

struct RecordingTeardown(
  TeardownCalls,
  /// Platform-capability introspection: does this fake's runtime end
  /// itself when the sidecar exits? (The E2B
  /// backend reports false; docker/gcp keep the default true.)
  bool,
);

impl RecordingTeardown {
  fn self_terminating(calls: TeardownCalls) -> Self {
    RecordingTeardown(calls, true)
  }

  /// The E2B shape: the sandbox outlives the sidecar.
  fn non_self_terminating(calls: TeardownCalls) -> Self {
    RecordingTeardown(calls, false)
  }
}

#[async_trait::async_trait]
impl crate::platform::RunnerPlatform for RecordingTeardown {
  async fn build(
    &self,
    _req: crate::platform::BuildRequest,
  ) -> Result<crate::platform::BuildOutput, crate::platform::BuildError> {
    Err(crate::platform::BuildError::bare(
      "test fake — never builds",
    ))
  }

  fn provisional_build_ref(&self, _deployment_id: Uuid, _version: i64) -> String {
    String::new()
  }

  async fn spawn(
    &self,
    _session: Uuid,
    _deployment_id: Uuid,
    _invocation_id: Uuid,
    _ingest_token: &str,
  ) -> Result<Option<String>, crate::dispatch::DispatchError> {
    Ok(Some("exec-from-spawn".into()))
  }

  async fn teardown(&self, session: Uuid, handle: Option<&str>) {
    self
      .0
      .lock()
      .unwrap()
      .push((session, handle.map(str::to_owned)));
  }

  fn self_terminating(&self) -> bool {
    self.1
  }
}

/// The boundary path is pure in-memory grant bookkeeping + the platform
/// hook — the registry/journal are never touched by it (fresh PG store
/// only because the hub is typed over them).
async fn unit_hub() -> Option<Arc<IngestHub>> {
  let store = test_pg::fresh_store().await?;
  let registry: Arc<dyn Registry> = Arc::new(store.registry());
  let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
  Some(Arc::new(IngestHub::new(
    registry,
    journal,
    chrono::Duration::seconds(30),
  )))
}

fn assignment() -> Assignment {
  Assignment {
    session_id: Uuid::now_v7(),
    deployment_id: Uuid::now_v7(),
    invocation_id: Uuid::now_v7(),
    route: "agent".into(),
    reason: sectr_common::events::InvocationReason::UserMessage {
      input: "hi".to_string().try_into().unwrap(),
      extra: Default::default(),
    },
    transcript: vec![],
    env: Default::default(),
    runner: RunnerSpec {
      command: vec!["true".into()],
      workdir: None,
      env: Default::default(),
    },
    approval_tools: vec![],
  }
}

#[tokio::test]
async fn boundary_policy_terminates_and_hands_the_handle_to_the_platform() {
  let Some(hub) = unit_hub().await else { return };
  let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
  hub.set_teardown(Arc::new(RecordingTeardown::self_terminating(calls.clone())));

  // Turn 1: fresh grant + a recorded execution handle (the spawn
  // returns the execution resource name; the dispatcher stores it).
  let session1 = Uuid::now_v7();
  let token1 = hub
    .register_turn(session1, Uuid::now_v7(), Uuid::now_v7(), assignment())
    .await
    .expect("fresh registration mints a token");
  assert!(token1.starts_with("sit_"));
  hub.store_execution(session1, Some("exec-abc".into())).await;

  // SUSPENSION boundary: the SIDECAR terminates itself (its post-turn
  // policy) — the server only releases the grant. NO platform call (a
  // cancel here would race the sidecar's exit and flip the execution to
  // CANCELLED).
  hub
    .on_turn_boundary(session1, TurnBoundary::Suspended)
    .await;
  assert!(
    calls.lock().unwrap().is_empty(),
    "boundary release never calls the platform teardown"
  );

  // Turn 2: the grant is gone — a FRESH token is minted.
  let session2 = Uuid::now_v7();
  let token2 = hub
    .register_turn(session2, Uuid::now_v7(), Uuid::now_v7(), assignment())
    .await
    .expect("the boundary dropped the grant — fresh mint");
  assert_ne!(token1, token2, "tokens are per-turn");
  hub.store_execution(session2, Some("exec-def".into())).await;

  // TURN-ENDED boundary: same uniform release — no platform call.
  hub
    .on_turn_boundary(session2, TurnBoundary::TurnEnded)
    .await;
  assert!(calls.lock().unwrap().is_empty());
  let token3 = hub
    .register_turn(session2, Uuid::now_v7(), Uuid::now_v7(), assignment())
    .await
    .expect("the sentinel dropped the grant — fresh mint");
  assert_ne!(token2, token3);
}

#[tokio::test]
async fn reclaim_still_hands_the_handle_to_the_platform() {
  // Platform teardown (cancel/kill) is RESERVED for exceptional paths —
  // reconciler reclaim via HubAbort → teardown_session. The boundary
  // policy never calls it; this pins the handle handover there.
  let Some(hub) = unit_hub().await else { return };
  let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
  hub.set_teardown(Arc::new(RecordingTeardown::self_terminating(calls.clone())));

  let session = Uuid::now_v7();
  hub
    .register_turn(session, Uuid::now_v7(), Uuid::now_v7(), assignment())
    .await
    .expect("fresh registration mints a token");
  hub.store_execution(session, Some("exec-live".into())).await;

  // Reclaim = teardown_session directly (HubAbort::on_reclaim), NOT the
  // boundary policy — the platform gets the execution handle.
  hub.teardown_session(session).await;
  assert_eq!(
    *calls.lock().unwrap(),
    vec![(session, Some("exec-live".into()))],
    "reclaim hands the execution handle to the platform"
  );
}

#[tokio::test]
async fn non_self_terminating_platform_is_torn_down_at_every_boundary() {
  // A platform whose runner resource does NOT end
  // itself when the sidecar exits (the E2B sandbox bills until its
  // timeout) gets torn down at the boundary — the SAME policy point, fed
  // by self_terminating(). BOTH boundary kinds; the ack precedes the
  // boundary, so the kill races nothing.
  for boundary in [TurnBoundary::TurnEnded, TurnBoundary::Suspended] {
    let Some(hub) = unit_hub().await else { return };
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    hub.set_teardown(Arc::new(RecordingTeardown::non_self_terminating(
      calls.clone(),
    )));

    let session = Uuid::now_v7();
    hub
      .register_turn(session, Uuid::now_v7(), Uuid::now_v7(), assignment())
      .await
      .expect("fresh registration mints a token");
    hub.store_execution(session, Some("sbx-123".into())).await;

    hub.on_turn_boundary(session, boundary).await;
    assert_eq!(
      *calls.lock().unwrap(),
      vec![(session, Some("sbx-123".into()))],
      "the boundary hands the sandbox handle to the platform teardown"
    );

    // The grant died with the boundary — the next turn mints fresh.
    let token = hub
      .register_turn(session, Uuid::now_v7(), Uuid::now_v7(), assignment())
      .await
      .expect("the boundary dropped the grant — fresh mint");
    assert!(token.starts_with("sit_"));
  }
}

#[tokio::test]
async fn no_hook_attached_defaults_to_self_terminating() {
  // Dev/tests without a platform hook: the boundary releases the grant
  // and touches nothing else (the historical behavior — the default is
  // self-terminating, never a teardown call).
  let Some(hub) = unit_hub().await else { return };
  let session = Uuid::now_v7();
  hub
    .register_turn(session, Uuid::now_v7(), Uuid::now_v7(), assignment())
    .await
    .expect("fresh registration mints a token");
  hub.on_turn_boundary(session, TurnBoundary::TurnEnded).await;
  let token = hub
    .register_turn(session, Uuid::now_v7(), Uuid::now_v7(), assignment())
    .await
    .expect("the boundary dropped the grant — fresh mint");
  assert!(token.starts_with("sit_"));
}
