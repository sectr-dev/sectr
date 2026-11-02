//! End-to-end tests for `POST /ag-ui/{app}/{route}` over a real Postgres
//! journal (pgtemp; skipped when no Postgres is available), through the
//! full HTTP stack (CORS → request span → auth → handler →
//! `ProdSessionService::start_run` → journal tail → AG-UI projection), with
//! a scripted turn dispatcher standing in for the runner path.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use sectr_common::events::{
  EventPayload, EventType, Extra, MessageDelta, MessageEnd, MessageStart, NoMoreActions,
  ToolApprovalRequired, ToolResult, TypedDelta,
};
use sectr_common::journal::Journal;
use sectr_common::registry::{ApiKeyKind, DeploymentStatus, NewApp, NewDeployment, Registry};
use sectr_common::store::PgStore;

use crate::config::ServerConfig;
use crate::dispatch::{DispatchError, TurnDispatcher};
use crate::http;
use crate::state::AppState;
use crate::test_pg;

// ---------------------------------------------------------------------------
// Scripted dispatcher (the runner stand-in)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
  /// Reply "Hello" and end the turn (resume turns answer the pending call).
  Complete,
  /// Reply, then suspend on a gated tool (TOOL_APPROVAL_REQUIRED).
  Suspend,
  /// Start streaming and keep the turn live (lease held) until `finish_held`.
  Hold,
  Fail,
}

struct Scripted {
  journal: Arc<dyn Journal>,
  registry: Arc<dyn Registry>,
  mode: Mutex<Mode>,
}

impl Scripted {
  fn set(&self, mode: Mode) {
    *self.mode.lock().unwrap() = mode;
  }

  async fn append(&self, session: Uuid, deployment: Uuid, payload: EventPayload) {
    self
      .journal
      .append_platform_event(session, deployment, payload)
      .await
      .unwrap();
  }

  async fn message(&self, session: Uuid, deployment: Uuid, id: &str, text: &str, end: bool) {
    self
      .append(
        session,
        deployment,
        EventPayload::MessageStart(MessageStart {
          role: "assistant".into(),
          message_id: id.into(),
          extra: Extra::default(),
        }),
      )
      .await;
    self
      .append(
        session,
        deployment,
        EventPayload::MessageDelta(MessageDelta {
          message_id: id.into(),
          delta: TypedDelta::text(text),
          extra: Extra::default(),
        }),
      )
      .await;
    if end {
      self
        .append(
          session,
          deployment,
          EventPayload::MessageEnd(MessageEnd {
            message_id: id.into(),
            finish_reason: None,
            framework_state: None,
            extra: Extra::default(),
          }),
        )
        .await;
    }
  }

  async fn release(&self, session: Uuid) {
    if let Some(lease) = self.registry.live_lease(session).await.unwrap() {
      self.registry.release_lease(lease.id).await.unwrap();
    }
  }

  /// End a held turn: the rest of the message, the sentinel, lease release.
  async fn finish_held(&self, session: Uuid) {
    let deployment = self.journal.fetch(session, None).await.unwrap()[0].deployment_id;
    let events = self.journal.fetch(session, None).await.unwrap();
    let id = events
      .iter()
      .rev()
      .find_map(|e| match &e.payload {
        EventPayload::MessageStart(m) => Some(m.message_id.clone()),
        _ => None,
      })
      .unwrap();
    self
      .append(
        session,
        deployment,
        EventPayload::MessageDelta(MessageDelta {
          message_id: id.clone(),
          delta: TypedDelta::text("meow"),
          extra: Extra::default(),
        }),
      )
      .await;
    self
      .append(
        session,
        deployment,
        EventPayload::MessageEnd(MessageEnd {
          message_id: id,
          finish_reason: None,
          framework_state: None,
          extra: Extra::default(),
        }),
      )
      .await;
    self
      .append(
        session,
        deployment,
        EventPayload::NoMoreActions(NoMoreActions::default()),
      )
      .await;
    self.release(session).await;
  }
}

#[async_trait::async_trait]
impl TurnDispatcher for Scripted {
  async fn dispatch_turn(
    &self,
    session: Uuid,
    deployment: Uuid,
    _invocation_id: Uuid,
    _route: &str,
  ) -> Result<(), DispatchError> {
    let mode = *self.mode.lock().unwrap();
    let message_id = format!("m-{}", Uuid::now_v7().simple());
    match mode {
      Mode::Complete => {
        let events = self.journal.fetch(session, None).await.unwrap();
        // An approval-resume turn executes the approved tool first.
        if let Some(pending) = sectr_common::journal::pending_tool_call(&events) {
          let tool_call_id = match &pending.payload {
            EventPayload::ToolCall(c) => c.tool_call_id.clone(),
            EventPayload::ToolApprovalRequired(a) => a.tool_call_id.clone(),
            _ => unreachable!(),
          };
          self
            .append(
              session,
              deployment,
              EventPayload::ToolResult(ToolResult {
                tool_call_id,
                result: json!({"refunded": 42}),
                extra: Extra::default(),
              }),
            )
            .await;
        }
        self
          .message(session, deployment, &message_id, "Hello", true)
          .await;
        self
          .append(
            session,
            deployment,
            EventPayload::NoMoreActions(NoMoreActions::default()),
          )
          .await;
        self.release(session).await;
        Ok(())
      }
      Mode::Suspend => {
        self
          .message(session, deployment, &message_id, "Refunding.", true)
          .await;
        self
          .append(
            session,
            deployment,
            EventPayload::ToolApprovalRequired(ToolApprovalRequired {
              tool_call_id: "call_1".into(),
              name: "refund".into(),
              arguments: json!({"amount": 42}),
              reason: Some("needs a human".into()),
              extra: Extra::default(),
            }),
          )
          .await;
        self.release(session).await;
        Ok(())
      }
      Mode::Hold => {
        self
          .message(session, deployment, &message_id, "Cats are ", false)
          .await;
        Ok(())
      }
      Mode::Fail => Err(DispatchError::Spawn("container gone".into())),
    }
  }
}

// ---------------------------------------------------------------------------
// World
// ---------------------------------------------------------------------------

struct World {
  app: axum::Router,
  registry: Arc<dyn Registry>,
  journal: Arc<dyn Journal>,
  dispatcher: Arc<Scripted>,
  org_id: Uuid,
  app_a: Uuid,
  platform_key: String,
  inv_a: String,
  inv_a_id: Uuid,
  alice: String,
  bob: String,
  carol_app_b: String,
}

async fn user_token(
  registry: &dyn Registry,
  org: Uuid,
  app: Uuid,
  minter: Uuid,
  user: &str,
  ttl: chrono::Duration,
) -> String {
  let token = crate::auth::keys::mint(ApiKeyKind::User);
  registry
    .create_user_token(
      org,
      app,
      minter,
      user,
      &crate::auth::keys::hash(&token),
      chrono::Utc::now() + ttl,
    )
    .await
    .unwrap();
  token
}

async fn world() -> Option<World> {
  world_limited(crate::limits::LimitsConfig::none()).await
}

async fn world_limited(limits: crate::limits::LimitsConfig) -> Option<World> {
  let store: PgStore = test_pg::fresh_store().await?;
  let registry = store.registry();
  let org = registry.create_org("test-org", "default").await.unwrap();

  let mut apps = Vec::new();
  for name in ["app-a", "app-b"] {
    let app = registry
      .create_app(NewApp {
        org_id: org.id,
        name: name.into(),
        slug: name.into(),
      })
      .await
      .unwrap();
    let deployment = registry
      .create_deployment(NewDeployment {
        app_id: app.id,
        version: 1,
        manifest: json!({"routes": [{"path": "/chat"}, {"path": "/other"}]}),
      })
      .await
      .unwrap();
    registry
      .set_deployment_status(deployment.id, DeploymentStatus::Active)
      .await
      .unwrap();
    apps.push(app);
  }
  let (app_a, app_b) = (apps[0].id, apps[1].id);

  let platform_key = crate::auth::keys::mint(ApiKeyKind::Platform);
  registry
    .create_api_key(
      org.id,
      None,
      ApiKeyKind::Platform,
      "platform",
      &crate::auth::keys::hash(&platform_key),
    )
    .await
    .unwrap();
  let inv_a = crate::auth::keys::mint(ApiKeyKind::Invocation);
  let inv_a_row = registry
    .create_api_key(
      org.id,
      Some(app_a),
      ApiKeyKind::Invocation,
      "backend-a",
      &crate::auth::keys::hash(&inv_a),
    )
    .await
    .unwrap();
  let inv_b = crate::auth::keys::mint(ApiKeyKind::Invocation);
  let inv_b_row = registry
    .create_api_key(
      org.id,
      Some(app_b),
      ApiKeyKind::Invocation,
      "backend-b",
      &crate::auth::keys::hash(&inv_b),
    )
    .await
    .unwrap();
  let day = chrono::Duration::days(1);
  let alice = user_token(&registry, org.id, app_a, inv_a_row.id, "alice", day).await;
  let bob = user_token(&registry, org.id, app_a, inv_a_row.id, "bob", day).await;
  let carol_app_b = user_token(&registry, org.id, app_b, inv_b_row.id, "carol", day).await;

  let registry: Arc<dyn Registry> = Arc::new(registry);
  let journal: Arc<dyn Journal> = Arc::new(store.journal());
  let dispatcher = Arc::new(Scripted {
    journal: journal.clone(),
    registry: registry.clone(),
    mode: Mutex::new(Mode::Complete),
  });
  let service = Arc::new(
    crate::sessions::ProdSessionService::new(
      registry.clone(),
      journal.clone(),
      dispatcher.clone(),
      chrono::Duration::seconds(30),
    )
    .with_limits(
      crate::limits::TurnCapsConfig::from_config(&limits),
      crate::limits::SessionRateLimiter::from_config(&limits),
    ),
  );
  let hub = crate::ingest::IngestHub::new(
    registry.clone(),
    journal.clone(),
    chrono::Duration::seconds(30),
  );
  let state = AppState::new(
    registry.clone(),
    journal.clone(),
    service,
    Arc::new(hub),
    test_config(),
    Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
    Arc::new(crate::platform::DockerPlatform::new(
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      None,
      test_config(),
      registry.clone(),
    )),
    false,
  );
  Some(World {
    app: http::router(state),
    registry,
    journal,
    dispatcher,
    org_id: org.id,
    app_a,
    platform_key,
    inv_a,
    inv_a_id: inv_a_row.id,
    alice,
    bob,
    carol_app_b,
  })
}

fn test_config() -> ServerConfig {
  ServerConfig {
    pg_url: "postgres://unused".into(),
    bind_addr: "127.0.0.1:0".parse().unwrap(),
    secrets_master_key: None,
    docker_socket: "/var/run/docker.sock".into(),
    bootstrap_platform_key: None,
    org_name: "test-org".into(),
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
    lease_ttl_secs: 30,
    user_token_ttl_secs: 86_400,
    mint_rate_limit_per_hour: 240,
    runner_server_url: "ws://172.17.0.1:8080/ingest".into(),
    runner_sidecar_cmd: vec!["/usr/local/bin/sectr-worker".into()],
    runner_image: None,
    runner_base_image: None,
    build_dir: "/tmp/sectr-build".into(),
    dashboard_url: "https://app.sectr.test".into(),
  }
}

macro_rules! world {
  () => {
    match world().await {
      Some(w) => w,
      None => {
        eprintln!("skipping: no test Postgres (initdb not on PATH)");
        return;
      }
    }
  };
}

// ---------------------------------------------------------------------------
// HTTP + SSE helpers
// ---------------------------------------------------------------------------

fn user_msg(id: &str, text: &str) -> Value {
  json!({"id": id, "role": "user", "content": text})
}

fn run_body(thread: Uuid, run: &str, messages: Vec<Value>) -> Value {
  json!({"threadId": thread.to_string(), "runId": run, "messages": messages, "tools": [], "context": []})
}

async fn post(app: &axum::Router, path: &str, token: Option<&str>, body: Value) -> Response {
  post_raw(app, path, token, body.to_string()).await
}

async fn post_raw(app: &axum::Router, path: &str, token: Option<&str>, body: String) -> Response {
  let mut req = Request::builder()
    .method("POST")
    .uri(path)
    .header("content-type", "application/json");
  if let Some(t) = token {
    req = req.header("Authorization", format!("Bearer {t}"));
  }
  app
    .clone()
    .oneshot(req.body(Body::from(body)).unwrap())
    .await
    .unwrap()
}

async fn json_body(response: Response) -> (StatusCode, Value) {
  let status = response.status();
  let bytes = response.into_body().collect().await.unwrap().to_bytes();
  (
    status,
    serde_json::from_slice(&bytes).unwrap_or(Value::Null),
  )
}

/// Incremental SSE reader: the AG-UI events (`data:` frames) of a response.
struct Sse {
  body: Body,
  buf: String,
}

impl Sse {
  fn new(response: Response) -> Self {
    assert_eq!(response.status(), StatusCode::OK, "expected an SSE 200");
    assert_eq!(
      response.headers()["content-type"],
      "text/event-stream",
      "AG-UI responses are text/event-stream"
    );
    Sse {
      body: response.into_body(),
      buf: String::new(),
    }
  }

  /// The next AG-UI event; `None` when the stream ends.
  async fn next(&mut self) -> Option<Value> {
    loop {
      if let Some(end) = self.buf.find("\n\n") {
        let frame: String = self.buf.drain(..end + 2).collect();
        if let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: ")) {
          return Some(serde_json::from_str(data).expect("data frame is JSON"));
        }
        continue; // keep-alive comment
      }
      let frame = tokio::time::timeout(Duration::from_secs(10), self.body.frame())
        .await
        .expect("timed out waiting for an SSE frame")?
        .expect("body error");
      if let Ok(data) = frame.into_data() {
        self.buf.push_str(std::str::from_utf8(&data).unwrap());
      }
    }
  }

  async fn take(&mut self, n: usize) -> Vec<Value> {
    let mut out = Vec::new();
    for _ in 0..n {
      out.push(self.next().await.expect("stream ended early"));
    }
    out
  }

  /// Everything until the stream ends.
  async fn all(mut self) -> Vec<Value> {
    let mut out = Vec::new();
    while let Some(e) = self.next().await {
      out.push(e);
    }
    out
  }
}

fn types(events: &[Value]) -> Vec<&str> {
  events.iter().map(|e| e["type"].as_str().unwrap()).collect()
}

async fn journal_types(w: &World, session: Uuid) -> Vec<EventType> {
  w.journal
    .fetch(session, None)
    .await
    .unwrap()
    .iter()
    .map(|e| e.event_type)
    .collect()
}

async fn invocation_count(w: &World, session: Uuid) -> usize {
  journal_types(w, session)
    .await
    .into_iter()
    .filter(|t| *t == EventType::InvocationStarted)
    .count()
}

const CHAT: &str = "/ag-ui/app-a/chat";

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_turn_on_a_new_thread_streams_run_started_to_run_finished() {
  let w = world!();
  let thread = Uuid::now_v7();
  let response = post(
    &w.app,
    CHAT,
    Some(&w.alice),
    run_body(thread, "r1", vec![user_msg("u1", "hi")]),
  )
  .await;
  let events = Sse::new(response).all().await;

  // New thread: no history, so no snapshot.
  assert_eq!(
    types(&events),
    [
      "RUN_STARTED",
      "TEXT_MESSAGE_START",
      "TEXT_MESSAGE_CONTENT",
      "TEXT_MESSAGE_END",
      "RUN_FINISHED"
    ]
  );
  assert_eq!(events[0]["threadId"], thread.to_string());
  assert_eq!(events[0]["runId"], "r1");
  assert_eq!(events[0]["protocolVersion"], "1.0");
  assert_eq!(events[0]["input"]["messages"][0]["content"], "hi");
  assert_eq!(events[2]["delta"], "Hello");
  assert_eq!(events[4]["outcome"]["type"], "success");
  // Every event carries the journal seq and timestamp.
  assert!(events.iter().all(|e| e["metadata"]["seq"].is_i64()));
  assert!(events.iter().all(|e| e["timestamp"].is_i64()));

  // threadId IS the session id; the token's user owns it; the client's
  // message id and the run id rode along on INVOCATION_STARTED (the
  // invocation id itself is server-minted).
  let session = w.registry.get_session(thread).await.unwrap();
  assert_eq!(session.external_user_id.as_deref(), Some("alice"));
  assert_eq!(session.route, "/chat");
  let journal = w.journal.fetch(thread, None).await.unwrap();
  let started = journal
    .iter()
    .find_map(|e| match &e.payload {
      EventPayload::InvocationStarted(i) => Some(i),
      _ => None,
    })
    .unwrap();
  assert_eq!(started.extra.map["ag_ui"]["client_message_id"], "u1");
  assert_eq!(started.extra.map["ag_ui"]["run_id"], "r1");
  assert_eq!(started.invocation_id.get_version_num(), 7);
  match &started.reason {
    sectr_common::events::InvocationReason::UserMessage { input, .. } => {
      assert_eq!(input.as_str(), "hi")
    }
    other => panic!("unexpected reason {other:?}"),
  }
}

#[tokio::test]
async fn second_run_begins_with_a_snapshot_and_streams_only_its_own_turn() {
  let w = world!();
  let thread = Uuid::now_v7();
  let first = post(
    &w.app,
    CHAT,
    Some(&w.alice),
    run_body(thread, "r1", vec![user_msg("u1", "hi")]),
  )
  .await;
  let first_events = Sse::new(first).all().await;
  let assistant_id = first_events[1]["messageId"].as_str().unwrap().to_owned();

  let second = post(
    &w.app,
    CHAT,
    Some(&w.alice),
    run_body(
      thread,
      "r2",
      vec![
        user_msg("u1", "hi"),
        json!({"id": assistant_id, "role": "assistant", "content": "Hello"}),
        user_msg("u2", "again"),
      ],
    ),
  )
  .await;
  let events = Sse::new(second).all().await;
  assert_eq!(events[0]["type"], "RUN_STARTED");
  assert_eq!(events[0]["runId"], "r2");
  assert_eq!(events[1]["type"], "MESSAGES_SNAPSHOT");
  let snapshot = events[1]["messages"].as_array().unwrap();
  let roles: Vec<_> = snapshot
    .iter()
    .map(|m| m["role"].as_str().unwrap())
    .collect();
  assert_eq!(roles, ["user", "assistant", "user"]);
  // The client's own ids come back (snapshots reconcile by id).
  assert_eq!(snapshot[0]["id"], "u1");
  assert_eq!(snapshot[1]["id"], assistant_id);
  assert_eq!(snapshot[2]["id"], "u2");
  assert_eq!(snapshot[1]["content"], "Hello");
  // Run 2's stream is the run's invocation only — not run 1's replay.
  let rest = &types(&events)[2..];
  assert_eq!(
    rest,
    [
      "TEXT_MESSAGE_START",
      "TEXT_MESSAGE_CONTENT",
      "TEXT_MESSAGE_END",
      "RUN_FINISHED"
    ]
  );
  assert_ne!(events[2]["messageId"], assistant_id.as_str());
}

#[tokio::test]
async fn approval_suspends_as_an_interrupt_and_resumes_via_resume_entries() {
  let w = world!();
  w.dispatcher.set(Mode::Suspend);
  let thread = Uuid::now_v7();
  let events = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "refund me")]),
    )
    .await,
  )
  .all()
  .await;
  let last = events.last().unwrap();
  assert_eq!(last["type"], "RUN_FINISHED");
  assert_eq!(last["outcome"]["type"], "interrupt");
  let interrupt = &last["outcome"]["interrupts"][0];
  assert_eq!(interrupt["reason"], "tool_approval");
  assert_eq!(interrupt["message"], "needs a human");
  assert_eq!(interrupt["toolCallId"], "call_1");
  assert_eq!(interrupt["responseSchema"], json!({"type": "boolean"}));
  assert_eq!(interrupt["metadata"]["toolName"], "refund");

  // The interrupt id IS the journal id of the TOOL_APPROVAL_REQUIRED row.
  let journal = w.journal.fetch(thread, None).await.unwrap();
  let approval = journal
    .iter()
    .find(|e| e.event_type == EventType::ToolApprovalRequired)
    .unwrap();
  assert_eq!(interrupt["id"], approval.id.to_string());
  // Suspended, not over: no sentinel was journaled.
  assert!(
    !journal
      .iter()
      .any(|e| e.event_type == EventType::NoMoreActions)
  );

  // Resume run: resolved/true grants.
  w.dispatcher.set(Mode::Complete);
  let mut body = run_body(thread, "r2", vec![user_msg("u1", "refund me")]);
  body["resume"] =
    json!([{"interruptId": approval.id.to_string(), "status": "resolved", "payload": true}]);
  let resumed = Sse::new(post(&w.app, CHAT, Some(&w.alice), body).await)
    .all()
    .await;
  assert_eq!(
    types(&resumed),
    [
      "RUN_STARTED",
      "MESSAGES_SNAPSHOT",
      "TOOL_CALL_RESULT",
      "TEXT_MESSAGE_START",
      "TEXT_MESSAGE_CONTENT",
      "TEXT_MESSAGE_END",
      "RUN_FINISHED"
    ]
  );
  assert_eq!(resumed[0]["runId"], "r2");
  assert_eq!(resumed[2]["toolCallId"], "call_1");
  assert_eq!(resumed[2]["content"], "{\"refunded\":42}");
  assert_eq!(resumed.last().unwrap()["outcome"]["type"], "success");

  // The decision is journaled through the existing approval path, with
  // approver = the token's external_user_id; then the resumed invocation.
  let after = w.journal.fetch(thread, None).await.unwrap();
  let granted = after
    .iter()
    .find_map(|e| match &e.payload {
      EventPayload::ToolApprovalGranted(d) => Some(d),
      _ => None,
    })
    .expect("TOOL_APPROVAL_GRANTED journaled");
  assert_eq!(granted.approver.as_deref(), Some("alice"));
  assert_eq!(granted.tool_call_id, "call_1");
  let resume_start = after
    .iter()
    .filter_map(|e| match &e.payload {
      EventPayload::InvocationStarted(i) => Some(i),
      _ => None,
    })
    .next_back()
    .unwrap();
  assert!(matches!(
    resume_start.reason,
    sectr_common::events::InvocationReason::ApprovalDecision { .. }
  ));
  // The resume run's id is journaled on the invocation it started.
  assert_eq!(resume_start.extra.map["ag_ui"]["run_id"], "r2");
}

#[tokio::test]
async fn cancelled_and_false_resume_entries_deny() {
  for (entry_status, payload) in [("cancelled", None), ("resolved", Some(false))] {
    let w = world!();
    w.dispatcher.set(Mode::Suspend);
    let thread = Uuid::now_v7();
    Sse::new(
      post(
        &w.app,
        CHAT,
        Some(&w.alice),
        run_body(thread, "r1", vec![user_msg("u1", "refund me")]),
      )
      .await,
    )
    .all()
    .await;
    let approval = w
      .journal
      .fetch(thread, None)
      .await
      .unwrap()
      .into_iter()
      .find(|e| e.event_type == EventType::ToolApprovalRequired)
      .unwrap();
    w.dispatcher.set(Mode::Complete);
    let mut entry = json!({"interruptId": approval.id.to_string(), "status": entry_status});
    if let Some(p) = payload {
      entry["payload"] = json!(p);
    }
    let mut body = run_body(thread, "r2", vec![]);
    body["resume"] = json!([entry]);
    Sse::new(post(&w.app, CHAT, Some(&w.alice), body).await)
      .all()
      .await;
    let types = journal_types(&w, thread).await;
    assert!(
      types.contains(&EventType::ToolApprovalDenied),
      "{entry_status}: {types:?}"
    );
    assert!(!types.contains(&EventType::ToolApprovalGranted));
  }
}

#[tokio::test]
async fn bad_resume_entries_are_run_errors_not_http_errors() {
  let w = world!();
  w.dispatcher.set(Mode::Suspend);
  let thread = Uuid::now_v7();
  Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "refund me")]),
    )
    .await,
  )
  .all()
  .await;
  let approval = w
    .journal
    .fetch(thread, None)
    .await
    .unwrap()
    .into_iter()
    .find(|e| e.event_type == EventType::ToolApprovalRequired)
    .unwrap();
  w.dispatcher.set(Mode::Complete);

  let cases = [
    // unknown interrupt id
    json!([{"interruptId": Uuid::now_v7().to_string(), "status": "resolved", "payload": true}]),
    json!([{"interruptId": "not-a-uuid", "status": "resolved", "payload": true}]),
    // resolved without a boolean payload
    json!([{"interruptId": approval.id.to_string(), "status": "resolved", "payload": "yes"}]),
    json!([{"interruptId": approval.id.to_string(), "status": "resolved"}]),
    // more than one entry: one of them can't exist
    json!([
      {"interruptId": approval.id.to_string(), "status": "resolved", "payload": true},
      {"interruptId": Uuid::now_v7().to_string(), "status": "resolved", "payload": true}
    ]),
  ];
  for (i, resume) in cases.into_iter().enumerate() {
    let mut body = run_body(thread, &format!("r-bad-{i}"), vec![]);
    body["resume"] = resume;
    let events = Sse::new(post(&w.app, CHAT, Some(&w.alice), body).await)
      .all()
      .await;
    assert_eq!(types(&events), ["RUN_STARTED", "RUN_ERROR"], "case {i}");
    assert_eq!(events[0]["runId"], format!("r-bad-{i}"));
  }
  // None of them journaled a decision: the thread is still awaiting one.
  let journaled = journal_types(&w, thread).await;
  assert!(!journaled.contains(&EventType::ToolApprovalGranted));
  assert!(!journaled.contains(&EventType::ToolApprovalDenied));

  // A resume with nothing pending (fresh thread, completed turn).
  let done = Uuid::now_v7();
  Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(done, "r1", vec![user_msg("u1", "hi")]),
    )
    .await,
  )
  .all()
  .await;
  let mut body = run_body(done, "r2", vec![]);
  body["resume"] =
    json!([{"interruptId": approval.id.to_string(), "status": "resolved", "payload": true}]);
  let events = Sse::new(post(&w.app, CHAT, Some(&w.alice), body).await)
    .all()
    .await;
  assert_eq!(types(&events), ["RUN_STARTED", "RUN_ERROR"]);

  // Resume on a thread that does not exist: 404 (nothing to resume).
  let mut body = run_body(Uuid::now_v7(), "r2", vec![]);
  body["resume"] =
    json!([{"interruptId": approval.id.to_string(), "status": "resolved", "payload": true}]);
  let (status, _) = json_body(post(&w.app, CHAT, Some(&w.alice), body).await).await;
  assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn busy_thread_is_409_before_the_stream_starts_even_for_the_same_input() {
  let w = world!();
  w.dispatcher.set(Mode::Hold);
  let thread = Uuid::now_v7();
  let mut first = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "cats")]),
    )
    .await,
  );
  first.take(1).await;

  // No adoption of the live turn: a re-send of the SAME message and a
  // second tab asking something else are both refused while it runs.
  for (run, messages) in [
    ("r2", vec![user_msg("u1", "cats")]),
    ("r3", vec![user_msg("u2", "dogs")]),
  ] {
    let (status, body) = json_body(
      post(
        &w.app,
        CHAT,
        Some(&w.alice),
        run_body(thread, run, messages),
      )
      .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{run}: {body}");
    assert!(body["error"].as_str().unwrap().contains("mid-turn"));
  }
  // Still exactly one invocation.
  let n = journal_types(&w, thread)
    .await
    .into_iter()
    .filter(|t| *t == EventType::InvocationStarted)
    .count();
  assert_eq!(n, 1);

  // The original run is undisturbed and ends normally.
  w.dispatcher.finish_held(thread).await;
  let rest = first.all().await;
  assert_eq!(rest.last().unwrap()["type"], "RUN_FINISHED");
}

#[tokio::test]
async fn a_user_message_on_a_thread_awaiting_approval_is_409() {
  let w = world!();
  w.dispatcher.set(Mode::Suspend);
  let thread = Uuid::now_v7();
  Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "refund me")]),
    )
    .await,
  )
  .all()
  .await;
  let (status, body) = json_body(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r2", vec![user_msg("u2", "never mind")]),
    )
    .await,
  )
  .await;
  assert_eq!(status, StatusCode::CONFLICT);
  assert!(body["error"].as_str().unwrap().contains("resume"));
}

#[tokio::test]
async fn session_error_ends_the_run_with_run_error_and_the_thread_recovers() {
  let w = world!();
  w.dispatcher.set(Mode::Fail);
  let thread = Uuid::now_v7();
  let events = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "hi")]),
    )
    .await,
  )
  .all()
  .await;
  assert_eq!(types(&events), ["RUN_STARTED", "RUN_ERROR"]);
  assert!(
    events[1]["message"]
      .as_str()
      .unwrap()
      .contains("container gone")
  );

  // An errored thread accepts a new turn (the sessions-API recovery path),
  // and its snapshot shows the earlier user message.
  w.dispatcher.set(Mode::Complete);
  let retry = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(
        thread,
        "r2",
        vec![user_msg("u1", "hi"), user_msg("u2", "hi again")],
      ),
    )
    .await,
  )
  .all()
  .await;
  assert_eq!(retry[1]["type"], "MESSAGES_SNAPSHOT");
  assert_eq!(retry[1]["messages"].as_array().unwrap().len(), 2);
  assert_eq!(retry.last().unwrap()["type"], "RUN_FINISHED");
}

#[tokio::test]
async fn the_stream_ends_with_run_error_when_the_user_token_expires() {
  let w = world!();
  w.dispatcher.set(Mode::Hold);
  let short = user_token(
    &*w.registry,
    w.org_id,
    w.app_a,
    w.inv_a_id,
    "dave",
    chrono::Duration::seconds(2),
  )
  .await;
  let mut sse = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&short),
      run_body(Uuid::now_v7(), "r1", vec![user_msg("u1", "cats")]),
    )
    .await,
  );
  sse.take(3).await;
  // The tail is cut at the token's expiry; the client still gets a terminal
  // event (then re-mints and re-sends).
  let rest = sse.all().await;
  let last = rest.last().expect("a terminal event");
  assert_eq!(last["type"], "RUN_ERROR");
  assert_eq!(last["code"], "stream_ended");
  // The open message was closed before the error.
  assert_eq!(types(&rest), ["TEXT_MESSAGE_END", "RUN_ERROR"]);
}

// ---------------------------------------------------------------------------
// Auth, resolution, validation, CORS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn credentials_are_bearer_only_and_class_gated() {
  let w = world!();
  let body = || run_body(Uuid::now_v7(), "r1", vec![user_msg("u1", "hi")]);

  let (status, _) = json_body(post(&w.app, CHAT, None, body()).await).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
  let (status, _) = json_body(post(&w.app, CHAT, Some("sks_garbage_garbage"), body()).await).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);

  // No ?token= fallback: AG-UI clients are fetch-based.
  let response = post(&w.app, &format!("{CHAT}?token={}", w.alice), None, body()).await;
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

  // Platform keys are not an AG-UI credential class.
  let (status, body_json) =
    json_body(post(&w.app, CHAT, Some(&w.platform_key), body()).await).await;
  assert_eq!(status, StatusCode::FORBIDDEN, "{body_json}");

  // ski_ (backend-mediated) is accepted; its sessions are ownerless.
  let thread = Uuid::now_v7();
  let events = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.inv_a),
      run_body(thread, "r1", vec![user_msg("u1", "hi")]),
    )
    .await,
  )
  .all()
  .await;
  assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
  assert!(
    w.registry
      .get_session(thread)
      .await
      .unwrap()
      .external_user_id
      .is_none()
  );
}

#[tokio::test]
async fn threads_are_ownership_scoped_and_rotation_invisible() {
  let w = world!();
  let thread = Uuid::now_v7();
  Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "hi")]),
    )
    .await,
  )
  .all()
  .await;

  // Another user of the same app: 404, no existence leak.
  let (status, _) = json_body(
    post(
      &w.app,
      CHAT,
      Some(&w.bob),
      run_body(thread, "r2", vec![user_msg("u2", "hello?")]),
    )
    .await,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);

  // The rejected run started NOTHING on alice's thread (no turn on the
  // foreign thread's behalf — the check precedes the turn start).
  assert_eq!(invocation_count(&w, thread).await, 1);

  // The app's trusted backend (ski_) sees every thread of its app; a token
  // bound to ANOTHER app sees none of them.
  let (status, _) = json_body(
    post(
      &w.app,
      "/ag-ui/app-b/chat",
      Some(&w.carol_app_b),
      run_body(thread, "r2", vec![user_msg("u2", "hello?")]),
    )
    .await,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  assert_eq!(invocation_count(&w, thread).await, 1);

  // Rotating the token (a fresh sks_ for the same user) is invisible to
  // existing threads: ownership is by external_user_id, not by token.
  let rotated = user_token(
    &*w.registry,
    w.org_id,
    w.app_a,
    w.inv_a_id,
    "alice",
    chrono::Duration::days(1),
  )
  .await;
  let events = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&rotated),
      run_body(
        thread,
        "r3",
        vec![user_msg("u1", "hi"), user_msg("u3", "again")],
      ),
    )
    .await,
  )
  .all()
  .await;
  assert_eq!(events[1]["type"], "MESSAGES_SNAPSHOT");
  assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
}

#[tokio::test]
async fn token_app_binding_and_route_resolution() {
  let w = world!();
  let body = || run_body(Uuid::now_v7(), "r1", vec![user_msg("u1", "hi")]);

  // A token bound to app-b cannot run app-a's agent.
  let (status, _) = json_body(post(&w.app, CHAT, Some(&w.carol_app_b), body()).await).await;
  assert_eq!(status, StatusCode::FORBIDDEN);
  // …but its own app works (route in ITS manifest).
  let events = Sse::new(post(&w.app, "/ag-ui/app-b/chat", Some(&w.carol_app_b), body()).await)
    .all()
    .await;
  assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");

  // Unknown app → 404.
  let (status, _) = json_body(post(&w.app, "/ag-ui/nope/chat", Some(&w.alice), body()).await).await;
  assert_eq!(status, StatusCode::NOT_FOUND);

  // Unknown route → 404 naming the available routes.
  let (status, err) =
    json_body(post(&w.app, "/ag-ui/app-a/missing", Some(&w.alice), body()).await).await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  let message = err["error"].as_str().unwrap();
  assert!(
    message.contains("/chat") && message.contains("/other"),
    "{message}"
  );

  // The app resolves by id as well as by slug.
  let events = Sse::new(
    post(
      &w.app,
      &format!("/ag-ui/{}/chat", w.app_a),
      Some(&w.alice),
      body(),
    )
    .await,
  )
  .all()
  .await;
  assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");

  // A thread belongs to its route.
  let thread = Uuid::now_v7();
  Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "hi")]),
    )
    .await,
  )
  .all()
  .await;
  let (status, err) = json_body(
    post(
      &w.app,
      "/ag-ui/app-a/other",
      Some(&w.alice),
      run_body(thread, "r2", vec![user_msg("u2", "hi")]),
    )
    .await,
  )
  .await;
  assert_eq!(status, StatusCode::CONFLICT);
  assert!(err["error"].as_str().unwrap().contains("/chat"));
}

#[tokio::test]
async fn malformed_requests_are_readable_4xx_before_any_stream() {
  let w = world!();
  let ok_thread = Uuid::now_v7();

  let (status, err) =
    json_body(post_raw(&w.app, CHAT, Some(&w.alice), "{not json".into()).await).await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert!(err["error"].as_str().unwrap().contains("RunAgentInput"));

  // threadId/runId/messages are required.
  for missing in ["threadId", "runId", "messages"] {
    let mut body = run_body(ok_thread, "r1", vec![user_msg("u1", "hi")]);
    body.as_object_mut().unwrap().remove(missing);
    let (status, _) = json_body(post(&w.app, CHAT, Some(&w.alice), body).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "missing {missing}");
  }

  // threadId is the session id: it must be a UUID.
  let mut body = run_body(ok_thread, "r1", vec![user_msg("u1", "hi")]);
  body["threadId"] = json!("thread-1");
  let (status, err) = json_body(post(&w.app, CHAT, Some(&w.alice), body).await).await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert!(err["error"].as_str().unwrap().contains("UUID"));

  // Unknown future major versions are refused readably; any 1.x and absent pass.
  let mut body = run_body(ok_thread, "r1", vec![user_msg("u1", "hi")]);
  body["protocolVersion"] = json!("2.0");
  let (status, err) = json_body(post(&w.app, CHAT, Some(&w.alice), body).await).await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert!(err["error"].as_str().unwrap().contains("protocolVersion"));
  let mut body = run_body(ok_thread, "r1", vec![user_msg("u1", "hi")]);
  body["protocolVersion"] = json!("1.1");
  let events = Sse::new(post(&w.app, CHAT, Some(&w.alice), body).await)
    .all()
    .await;
  assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");

  // The trailing message must be a user message.
  for messages in [
    vec![],
    vec![json!({"id": "a1", "role": "assistant", "content": "hi"})],
    vec![json!({"id": "u1", "role": "user"})],
  ] {
    let (status, _) = json_body(
      post(
        &w.app,
        CHAT,
        Some(&w.alice),
        run_body(Uuid::now_v7(), "r1", messages),
      )
      .await,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
  }

  // A NEW thread can't import client-side history: refused, nothing created
  // (an existing thread's history is fine — the journal is its history).
  let thread = Uuid::now_v7();
  let (status, err) = json_body(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(
        thread,
        "r1",
        vec![
          user_msg("u0", "earlier"),
          json!({"id": "a0", "role": "assistant", "content": "reply"}),
          user_msg("u1", "hi"),
        ],
      ),
    )
    .await,
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  assert!(err["error"].as_str().unwrap().contains("history"), "{err}");
  assert!(w.registry.get_session(thread).await.is_err());
  // System/developer prompts are not history.
  let events = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(
        Uuid::now_v7(),
        "r1",
        vec![
          json!({"id": "s", "role": "system", "content": "be brief"}),
          user_msg("u1", "hi"),
        ],
      ),
    )
    .await,
  )
  .all()
  .await;
  assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
}

#[tokio::test]
async fn forwarded_props_tools_and_context_are_accepted_and_echoed() {
  let w = world!();
  let mut body = run_body(Uuid::now_v7(), "r1", vec![user_msg("u1", "hi")]);
  body["forwardedProps"] = json!({"locale": "en"});
  body["context"] = json!([{"description": "page", "value": "/pricing"}]);
  body["tools"] =
    json!([{"name": "frontend_tool", "description": "d", "parameters": {"type": "object"}}]);
  body["unknownTopLevelField"] = json!(1);
  let events = Sse::new(post(&w.app, CHAT, Some(&w.alice), body).await)
    .all()
    .await;
  assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
  let echoed = &events[0]["input"];
  assert_eq!(echoed["forwardedProps"], json!({"locale": "en"}));
  assert_eq!(echoed["context"][0]["value"], "/pricing");
  assert_eq!(echoed["tools"][0]["name"], "frontend_tool");
  // forwardedProps/context ride on INVOCATION_STARTED's forward-compat
  // extras (the runner reads them from its transcript); `input` stays the
  // bare message content. Frontend `tools` are accepted but unused (v1).
  let thread: Uuid = events[0]["threadId"].as_str().unwrap().parse().unwrap();
  let started = w
    .journal
    .fetch(thread, None)
    .await
    .unwrap()
    .into_iter()
    .find_map(|e| match e.payload {
      EventPayload::InvocationStarted(i) => Some(i),
      _ => None,
    })
    .unwrap();
  let ag_ui = &started.extra.map["ag_ui"];
  assert_eq!(ag_ui["forwarded_props"], json!({"locale": "en"}));
  assert_eq!(ag_ui["context"][0]["value"], "/pricing");
  assert!(ag_ui.get("tools").is_none());
  // Nothing AG-UI-specific outside the namespace.
  assert_eq!(started.extra.map.len(), 1);
  match started.reason {
    sectr_common::events::InvocationReason::UserMessage { input, .. } => {
      assert_eq!(input.as_str(), "hi")
    }
    other => panic!("unexpected reason {other:?}"),
  }
  // The schema is closed: fields it does not declare are not echoed back.
  assert!(echoed.get("unknownTopLevelField").is_none());
}

#[tokio::test]
async fn cors_preflight_is_answered_before_auth_for_any_origin() {
  let w = world!();
  let preflight = Request::builder()
    .method("OPTIONS")
    .uri(CHAT)
    .header("origin", "https://customer-frontend.example")
    .header("access-control-request-method", "POST")
    .header(
      "access-control-request-headers",
      "authorization,content-type",
    )
    .body(Body::empty())
    .unwrap();
  let response = w.app.clone().oneshot(preflight).await.unwrap();
  assert!(response.status().is_success(), "{}", response.status());
  let h = response.headers();
  assert_eq!(h["access-control-allow-origin"], "*");
  assert!(
    h["access-control-allow-methods"]
      .to_str()
      .unwrap()
      .contains("POST")
  );
  let allowed = h["access-control-allow-headers"]
    .to_str()
    .unwrap()
    .to_lowercase();
  assert!(allowed.contains("authorization") && allowed.contains("content-type"));

  // The actual response carries the allow-origin header too.
  let mut req = Request::builder()
    .method("POST")
    .uri(CHAT)
    .header("content-type", "application/json")
    .header("origin", "https://customer-frontend.example")
    .header("Authorization", format!("Bearer {}", w.alice));
  req = req.header("accept", "text/event-stream");
  let response = w
    .app
    .clone()
    .oneshot(
      req
        .body(Body::from(
          run_body(Uuid::now_v7(), "r1", vec![user_msg("u1", "hi")]).to_string(),
        ))
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.headers()["access-control-allow-origin"], "*");
  Sse::new(response).all().await;
}

#[tokio::test]
async fn platform_sse_is_still_the_session_tail_path() {
  // The AG-UI endpoint must not disturb the sessions API: the raw journal
  // tail still serves the whole session (all invocations) to its owner.
  let w = world!();
  let thread = Uuid::now_v7();
  for run in ["r1", "r2"] {
    Sse::new(
      post(
        &w.app,
        CHAT,
        Some(&w.alice),
        run_body(thread, run, vec![user_msg("u1", "hi")]),
      )
      .await,
    )
    .all()
    .await;
  }
  let response = w
    .app
    .clone()
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/sessions/{thread}"))
        .header("Authorization", format!("Bearer {}", w.alice))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  let (status, summary) = json_body(response).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(summary["id"], thread.to_string());
  assert_eq!(summary["external_user_id"], "alice");
}

// ---------------------------------------------------------------------------
// A 429 from start_run is a pre-stream HTTP error (like 404/409)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_thread_at_the_concurrency_cap_is_a_429_before_the_stream() {
  let Some(w) = world_limited(crate::limits::LimitsConfig {
    max_turns_per_app: 1,
    ..crate::limits::LimitsConfig::none()
  })
  .await
  else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  w.dispatcher.set(Mode::Hold);
  let first_thread = Uuid::now_v7();
  let mut first = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(first_thread, "r1", vec![user_msg("u1", "cats")]),
    )
    .await,
  );
  first.take(1).await; // the only slot is now held by a live turn

  let second_thread = Uuid::now_v7();
  let response = post(
    &w.app,
    CHAT,
    Some(&w.bob),
    run_body(second_thread, "r2", vec![user_msg("u2", "dogs")]),
  )
  .await;
  assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
  assert_eq!(response.headers()["retry-after"], "5");
  assert_ne!(
    response.headers()["content-type"],
    "text/event-stream",
    "a plain JSON error, not a stream"
  );
  let (_, body) = json_body(response).await;
  assert_eq!(body["code"], "concurrency_limit", "{body}");
  assert!(
    w.registry.get_session(second_thread).await.is_err(),
    "the rejected thread was never created"
  );

  // The first run is undisturbed; once it ends the second thread starts.
  w.dispatcher.finish_held(first_thread).await;
  assert_eq!(first.all().await.last().unwrap()["type"], "RUN_FINISHED");
  w.dispatcher.set(Mode::Complete);
  let retry = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.bob),
      run_body(second_thread, "r3", vec![user_msg("u2", "dogs")]),
    )
    .await,
  );
  assert_eq!(retry.all().await.last().unwrap()["type"], "RUN_FINISHED");
}

// ---------------------------------------------------------------------------
// Connect — `GET /ag-ui/{app}/{route}?threadId=` re-streams the
// thread's latest run, read-only
// ---------------------------------------------------------------------------

async fn get(app: &axum::Router, path: &str, token: Option<&str>) -> Response {
  let mut req = Request::builder()
    .method("GET")
    .uri(path)
    .header("accept", "text/event-stream");
  if let Some(t) = token {
    req = req.header("Authorization", format!("Bearer {t}"));
  }
  app
    .clone()
    .oneshot(req.body(Body::empty()).unwrap())
    .await
    .unwrap()
}

fn connect_path(route: &str, thread: Uuid) -> String {
  format!("{route}?threadId={thread}")
}

/// A thread with one finished run ("hi" → "Hello"), owned by alice.
async fn finished_thread(w: &World) -> Uuid {
  let thread = Uuid::now_v7();
  Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "hi")]),
    )
    .await,
  )
  .all()
  .await;
  thread
}

#[tokio::test]
async fn connect_replays_the_latest_run_with_its_original_run_id() {
  let w = world!();
  let thread = finished_thread(&w).await;
  Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r2", vec![user_msg("u2", "again")]),
    )
    .await,
  )
  .all()
  .await;

  let events = Sse::new(get(&w.app, &connect_path(CHAT, thread), Some(&w.alice)).await)
    .all()
    .await;
  assert_eq!(
    types(&events),
    [
      "RUN_STARTED",
      "MESSAGES_SNAPSHOT",
      "TEXT_MESSAGE_START",
      "TEXT_MESSAGE_CONTENT",
      "TEXT_MESSAGE_END",
      "RUN_FINISHED"
    ]
  );
  // The latest run, under its original id; no request to echo.
  assert_eq!(events[0]["threadId"], thread.to_string());
  assert_eq!(events[0]["runId"], "r2");
  assert!(events[0].get("input").is_none());
  // History through the latest run's own user message — not its reply.
  let snapshot = events[1]["messages"].as_array().unwrap();
  let ids: Vec<_> = snapshot
    .iter()
    .map(|m| m["role"].as_str().unwrap())
    .collect();
  assert_eq!(ids, ["user", "assistant", "user"]);
  assert_eq!(snapshot[2]["id"], "u2");
  assert_eq!(events.last().unwrap()["outcome"]["type"], "success");
  // Read-only: still exactly two invocations.
  assert_eq!(invocation_count(&w, thread).await, 2);
}

#[tokio::test]
async fn connect_mid_turn_reattaches_to_the_live_tail_alongside_the_run() {
  let w = world!();
  w.dispatcher.set(Mode::Hold);
  let thread = Uuid::now_v7();
  let mut run = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "cats")]),
    )
    .await,
  );
  run.take(3).await; // RUN_STARTED, TEXT_MESSAGE_START, "Cats are "

  // A reloaded page connects mid-turn: replay so far, then the live tail.
  let mut connect = Sse::new(get(&w.app, &connect_path(CHAT, thread), Some(&w.alice)).await);
  let replay = connect.take(4).await;
  assert_eq!(
    types(&replay),
    [
      "RUN_STARTED",
      "MESSAGES_SNAPSHOT",
      "TEXT_MESSAGE_START",
      "TEXT_MESSAGE_CONTENT"
    ]
  );
  assert_eq!(replay[0]["runId"], "r1");
  assert_eq!(replay[3]["delta"], "Cats are ");

  w.dispatcher.finish_held(thread).await;
  // Both readers get the rest of the turn, independently.
  let rest = connect.all().await;
  assert_eq!(
    types(&rest),
    ["TEXT_MESSAGE_CONTENT", "TEXT_MESSAGE_END", "RUN_FINISHED"]
  );
  assert_eq!(rest[0]["delta"], "meow");
  assert_eq!(run.all().await.last().unwrap()["type"], "RUN_FINISHED");
  // Connect started no turn of its own.
  assert_eq!(invocation_count(&w, thread).await, 1);
}

#[tokio::test]
async fn connect_restores_a_pending_approval_and_the_resume_still_works() {
  let w = world!();
  w.dispatcher.set(Mode::Suspend);
  let thread = Uuid::now_v7();
  let original = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "refund me")]),
    )
    .await,
  )
  .all()
  .await;
  let interrupt = original.last().unwrap()["outcome"]["interrupts"][0].clone();

  let events = Sse::new(get(&w.app, &connect_path(CHAT, thread), Some(&w.alice)).await)
    .all()
    .await;
  let last = events.last().unwrap();
  assert_eq!(last["type"], "RUN_FINISHED");
  assert_eq!(last["outcome"]["type"], "interrupt");
  // The same interrupt the original run ended in.
  assert_eq!(last["outcome"]["interrupts"][0], interrupt);

  // The reloaded client answers it with a normal resume run.
  w.dispatcher.set(Mode::Complete);
  let mut body = run_body(thread, "r2", vec![]);
  body["resume"] = json!([{"interruptId": interrupt["id"], "status": "resolved", "payload": true}]);
  let resumed = Sse::new(post(&w.app, CHAT, Some(&w.alice), body).await)
    .all()
    .await;
  assert_eq!(resumed.last().unwrap()["outcome"]["type"], "success");

  // Connect after the resume replays the resumed invocation.
  let after = Sse::new(get(&w.app, &connect_path(CHAT, thread), Some(&w.alice)).await)
    .all()
    .await;
  assert_eq!(after[0]["runId"], "r2");
  assert_eq!(after[2]["type"], "TOOL_CALL_RESULT");
  assert_eq!(after.last().unwrap()["outcome"]["type"], "success");
}

#[tokio::test]
async fn connect_replays_a_last_turn_error_faithfully() {
  let w = world!();
  w.dispatcher.set(Mode::Fail);
  let thread = Uuid::now_v7();
  Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&w.alice),
      run_body(thread, "r1", vec![user_msg("u1", "hi")]),
    )
    .await,
  )
  .all()
  .await;
  let events = Sse::new(get(&w.app, &connect_path(CHAT, thread), Some(&w.alice)).await)
    .all()
    .await;
  assert_eq!(
    types(&events),
    ["RUN_STARTED", "MESSAGES_SNAPSHOT", "RUN_ERROR"]
  );
  assert!(
    events[2]["message"]
      .as_str()
      .unwrap()
      .contains("container gone")
  );
}

#[tokio::test]
async fn connect_on_a_thread_without_a_run_is_an_empty_stream_and_creates_nothing() {
  let w = world!();
  // A thread that doesn't exist: 200, no events, nothing created.
  let fresh = Uuid::now_v7();
  let events = Sse::new(get(&w.app, &connect_path(CHAT, fresh), Some(&w.alice)).await)
    .all()
    .await;
  assert!(events.is_empty(), "{events:?}");
  assert!(w.registry.get_session(fresh).await.is_err());

  // A session with only SESSION_CREATED (e.g. the guardrails' 429 after
  // open_session): same as no thread.
  let deployment = w
    .registry
    .active_deployment(w.app_a)
    .await
    .unwrap()
    .unwrap();
  let session = w
    .registry
    .create_session(sectr_common::registry::NewSession {
      id: None,
      app_id: w.app_a,
      deployment_id: deployment.id,
      route: "/chat".into(),
      origin: sectr_common::api::Origin::Api,
      external_user_id: Some("alice".into()),
    })
    .await
    .unwrap();
  w.journal
    .append_platform_event(
      session.id,
      deployment.id,
      EventPayload::SessionCreated(sectr_common::events::SessionCreated {
        app_id: w.app_a,
        route: "/chat".into(),
        external_user_id: Some("alice".into()),
        extra: Extra::default(),
      }),
    )
    .await
    .unwrap();
  let events = Sse::new(get(&w.app, &connect_path(CHAT, session.id), Some(&w.alice)).await)
    .all()
    .await;
  assert!(events.is_empty(), "{events:?}");
  assert_eq!(
    journal_types(&w, session.id).await,
    [EventType::SessionCreated]
  );
}

#[tokio::test]
async fn connect_applies_the_run_paths_auth_and_resolution() {
  let w = world!();
  let thread = finished_thread(&w).await;
  let path = connect_path(CHAT, thread);

  // Another user of the same app: 404, no existence leak.
  let (status, _) = json_body(get(&w.app, &path, Some(&w.bob)).await).await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  // A token bound to another app: 403.
  let (status, _) = json_body(get(&w.app, &path, Some(&w.carol_app_b)).await).await;
  assert_eq!(status, StatusCode::FORBIDDEN);
  // …and on its own app, the other app's thread is not found.
  let (status, _) = json_body(
    get(
      &w.app,
      &connect_path("/ag-ui/app-b/chat", thread),
      Some(&w.carol_app_b),
    )
    .await,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  // Another route: 409, as for runs.
  let (status, err) = json_body(
    get(
      &w.app,
      &connect_path("/ag-ui/app-a/other", thread),
      Some(&w.alice),
    )
    .await,
  )
  .await;
  assert_eq!(status, StatusCode::CONFLICT);
  assert!(err["error"].as_str().unwrap().contains("/chat"));
  // Unknown route: 404 naming the available ones.
  let (status, err) = json_body(
    get(
      &w.app,
      &connect_path("/ag-ui/app-a/missing", thread),
      Some(&w.alice),
    )
    .await,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  assert!(err["error"].as_str().unwrap().contains("/other"));

  // Credentials: Bearer only, data-plane classes only.
  let (status, _) = json_body(get(&w.app, &path, None).await).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
  let (status, _) = json_body(get(&w.app, &format!("{path}&token={}", w.alice), None).await).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
  let (status, _) = json_body(get(&w.app, &path, Some(&w.platform_key)).await).await;
  assert_eq!(status, StatusCode::FORBIDDEN);
  // The app's backend key sees its app's threads.
  let events = Sse::new(get(&w.app, &path, Some(&w.inv_a)).await)
    .all()
    .await;
  assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");

  // threadId: required, and a UUID.
  for bad in [CHAT.to_string(), format!("{CHAT}?threadId=thread-1")] {
    let (status, err) = json_body(get(&w.app, &bad, Some(&w.alice)).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    assert!(err["error"].as_str().unwrap().contains("UUID"), "{err}");
  }
}

#[tokio::test]
async fn connect_ends_with_run_error_when_the_user_token_expires_mid_tail() {
  let w = world!();
  w.dispatcher.set(Mode::Hold);
  let short = user_token(
    &*w.registry,
    w.org_id,
    w.app_a,
    w.inv_a_id,
    "dave",
    chrono::Duration::seconds(2),
  )
  .await;
  let thread = Uuid::now_v7();
  let mut run = Sse::new(
    post(
      &w.app,
      CHAT,
      Some(&short),
      run_body(thread, "r1", vec![user_msg("u1", "cats")]),
    )
    .await,
  );
  run.take(3).await;
  let mut connect = Sse::new(get(&w.app, &connect_path(CHAT, thread), Some(&short)).await);
  connect.take(4).await;
  let rest = connect.all().await;
  assert_eq!(types(&rest), ["TEXT_MESSAGE_END", "RUN_ERROR"]);
  assert_eq!(rest[1]["code"], "stream_ended");
}

#[tokio::test]
async fn connect_preflight_allows_get() {
  let w = world!();
  let preflight = Request::builder()
    .method("OPTIONS")
    .uri(CHAT)
    .header("origin", "https://customer-frontend.example")
    .header("access-control-request-method", "GET")
    .header("access-control-request-headers", "authorization")
    .body(Body::empty())
    .unwrap();
  let response = w.app.clone().oneshot(preflight).await.unwrap();
  assert!(response.status().is_success(), "{}", response.status());
  assert_eq!(response.headers()["access-control-allow-origin"], "*");
  assert!(
    response.headers()["access-control-allow-methods"]
      .to_str()
      .unwrap()
      .contains("GET")
  );
}

#[tokio::test]
async fn connect_is_not_subject_to_the_session_rate_limit() {
  let Some(w) = world_limited(crate::limits::LimitsConfig {
    session_rate_per_user_hour: 1,
    ..crate::limits::LimitsConfig::none()
  })
  .await
  else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  // alice's one new thread for the hour…
  let thread = finished_thread(&w).await;
  // …so a second new thread is rate-limited…
  let response = post(
    &w.app,
    CHAT,
    Some(&w.alice),
    run_body(Uuid::now_v7(), "r2", vec![user_msg("u2", "more")]),
  )
  .await;
  assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
  // …but connect is a read: it never answers 429, existing thread or not.
  for _ in 0..3 {
    let events = Sse::new(get(&w.app, &connect_path(CHAT, thread), Some(&w.alice)).await)
      .all()
      .await;
    assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
    let empty = Sse::new(get(&w.app, &connect_path(CHAT, Uuid::now_v7()), Some(&w.alice)).await)
      .all()
      .await;
    assert!(empty.is_empty());
  }
}
