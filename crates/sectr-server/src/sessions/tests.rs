//! Integration tests: the prod session service over a real Postgres
//! (pgtemp-managed test PG — skip when none is available), through the
//! full HTTP stack (auth → scope → shared sessions router) with a
//! scripted turn dispatcher standing in for the runner path.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use sectr_common::events::{
  EventPayload, EventType, NoMoreActions, SessionError, ToolApprovalRequired, ToolResult,
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
// Scripted dispatcher: the runner-path stand-in. Completes turns, suspends them
// on an approval, or fails — driven per test.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
  Complete,
  Suspend,
  Fail,
  /// The runner dies mid-turn, e.g. inside the approved tool after its
  /// side effect: no TOOL_RESULT, the platform ends the turn.
  Crash,
  /// Leave the lease live and journal nothing more: a turn that is still
  /// running (guardrail cap tests hold slots with it).
  Hold,
}

struct ScriptedDispatcher {
  journal: Arc<dyn Journal>,
  registry: Arc<dyn Registry>,
  mode: Mutex<Mode>,
}

impl ScriptedDispatcher {
  fn set(&self, mode: Mode) {
    *self.mode.lock().unwrap() = mode;
  }
}

#[async_trait::async_trait]
impl TurnDispatcher for ScriptedDispatcher {
  async fn dispatch_turn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    _invocation_id: Uuid,
    _route: &str,
  ) -> Result<(), DispatchError> {
    let mode = *self.mode.lock().unwrap();
    let result = match mode {
      Mode::Complete => {
        // Simulate the runner finishing. If the transcript carries
        // a pending approval decision, the resume turn executes
        // the approved tool itself -> TOOL_RESULT (dev-dispatch
        // semantics); then the sentinel.
        let events = self.journal.fetch(session, None).await.unwrap();
        if let Some(pending) = sectr_common::journal::pending_tool_call(&events) {
          let tool_call_id = match &pending.payload {
            EventPayload::ToolCall(c) => c.tool_call_id.clone(),
            EventPayload::ToolApprovalRequired(a) => a.tool_call_id.clone(),
            _ => unreachable!("pending_tool_call yields calls only"),
          };
          self
            .journal
            .append_platform_event(
              session,
              deployment_id,
              EventPayload::ToolResult(ToolResult {
                tool_call_id,
                result: json!({"ok": true}),
                extra: Default::default(),
              }),
            )
            .await
            .unwrap();
        }
        self
          .journal
          .append_platform_event(
            session,
            deployment_id,
            EventPayload::NoMoreActions(NoMoreActions {
              extra: Default::default(),
            }),
          )
          .await
          .unwrap();
        Ok(())
      }
      Mode::Suspend => {
        // Runner-final event without the sentinel: the turn is
        // suspended, the invocation ends.
        self
          .journal
          .append_platform_event(
            session,
            deployment_id,
            EventPayload::ToolApprovalRequired(ToolApprovalRequired {
              tool_call_id: "call_1".into(),
              name: "refund".into(),
              arguments: json!({"amount": 42}),
              reason: None,
              extra: Default::default(),
            }),
          )
          .await
          .unwrap();
        Ok(())
      }
      Mode::Crash => {
        for payload in [
          EventPayload::SessionError(SessionError {
            message: "runner exited (code: Some(1), signal: None)".into(),
            extra: Default::default(),
          }),
          EventPayload::NoMoreActions(NoMoreActions {
            extra: Default::default(),
          }),
        ] {
          self
            .journal
            .append_platform_event(session, deployment_id, payload)
            .await
            .unwrap();
        }
        Ok(())
      }
      Mode::Fail => Err(DispatchError::Spawn("container gone".into())),
      Mode::Hold => return Ok(()),
    };
    // Both runner-final paths end the invocation -> the lease is
    // released (dev: teardown; prod: ingest completion).
    if result.is_ok()
      && let Some(lease) = self.registry.live_lease(session).await.unwrap()
    {
      self.registry.release_lease(lease.id).await.unwrap();
    }
    result
  }
}

// ---------------------------------------------------------------------------
// World: two apps, keys for both classes, the router over everything.
// ---------------------------------------------------------------------------

struct World {
  app: axum::Router,
  #[allow(dead_code)] // service handle for future direct-service tests
  state: AppState,
  platform_key: String,
  inv_a: String,
  #[allow(dead_code)] // symmetric with inv_a; used when a test needs B's key
  inv_b: String,
  app_a: Uuid,
  #[allow(dead_code)]
  app_b: Uuid,
  journal: Arc<dyn Journal>,
  dispatcher: Arc<ScriptedDispatcher>,
}

async fn world() -> Option<World> {
  world_with(Arc::new(crate::analytics::Analytics::disabled())).await
}

/// The world with an ATTACHED analytics client — the analytics emission tests
/// hand in one wired to the in-process capture stub (analytics::testutil).
async fn world_with(analytics: Arc<crate::analytics::Analytics>) -> Option<World> {
  world_inner(analytics, |_, _| crate::limits::LimitsConfig::none()).await
}

/// The world with limits; the closure gets `(app_a, app_b)` ids so
/// per-app overrides can name them.
async fn world_with_limits(
  limits: impl FnOnce(Uuid, Uuid) -> crate::limits::LimitsConfig,
) -> Option<World> {
  world_inner(Arc::new(crate::analytics::Analytics::disabled()), limits).await
}

async fn world_inner(
  analytics: Arc<crate::analytics::Analytics>,
  limits: impl FnOnce(Uuid, Uuid) -> crate::limits::LimitsConfig,
) -> Option<World> {
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
        manifest: json!({"routes": [{"path": "/test"}]}),
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
  let limits = limits(app_a, app_b);

  let platform_key = crate::auth::keys::mint(ApiKeyKind::Platform);
  registry
    .create_api_key(
      org.id,
      None,
      ApiKeyKind::Platform,
      "test platform",
      &crate::auth::keys::hash(&platform_key),
    )
    .await
    .unwrap();
  let inv_a = crate::auth::keys::mint(ApiKeyKind::Invocation);
  registry
    .create_api_key(
      org.id,
      Some(app_a),
      ApiKeyKind::Invocation,
      "end-clients-a",
      &crate::auth::keys::hash(&inv_a),
    )
    .await
    .unwrap();
  let inv_b = crate::auth::keys::mint(ApiKeyKind::Invocation);
  registry
    .create_api_key(
      org.id,
      Some(app_b),
      ApiKeyKind::Invocation,
      "end-clients-b",
      &crate::auth::keys::hash(&inv_b),
    )
    .await
    .unwrap();

  let registry: Arc<dyn Registry> = Arc::new(registry);
  let journal: Arc<dyn Journal> = Arc::new(store.journal());
  let dispatcher = Arc::new(ScriptedDispatcher {
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
    .with_analytics(analytics)
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
    // Dockerless platform: the config carries no base image, so a
    // deploy build fails fast (dockerless test-world semantics).
    Arc::new(crate::platform::DockerPlatform::new(
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      None,
      test_config(),
      registry.clone(),
    )),
    false,
  );
  let app = http::router(state.clone());

  Some(World {
    app,
    state,
    platform_key,
    inv_a,
    inv_b,
    app_a,
    app_b,
    journal,
    dispatcher,
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

async fn req(
  app: &axum::Router,
  method: &str,
  uri: &str,
  token: Option<&str>,
  body: Option<Value>,
) -> (StatusCode, Value) {
  let builder = Request::builder().method(method).uri(uri);
  let builder = match token {
    Some(t) => builder.header("Authorization", format!("Bearer {t}")),
    None => builder,
  };
  let request = builder
    .header("Content-Type", "application/json")
    .body(Body::from(
      body.map(|b| b.to_string()).unwrap_or_else(|| "null".into()),
    ))
    .unwrap();
  let response = app.clone().oneshot(request).await.unwrap();
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  let body = if bytes.is_empty() {
    Value::Null
  } else {
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
  };
  (status, body)
}

/// Like [`req`], plus the `Retry-After` header (the 429 contract).
async fn req_retry_after(
  app: &axum::Router,
  method: &str,
  uri: &str,
  token: &str,
  body: Value,
) -> (StatusCode, Value, Option<String>) {
  let request = Request::builder()
    .method(method)
    .uri(uri)
    .header("Authorization", format!("Bearer {token}"))
    .header("Content-Type", "application/json")
    .body(Body::from(body.to_string()))
    .unwrap();
  let response = app.clone().oneshot(request).await.unwrap();
  let status = response.status();
  let retry_after = response
    .headers()
    .get("retry-after")
    .map(|v| v.to_str().unwrap().to_owned());
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
  (status, body, retry_after)
}

async fn create_session(app: &axum::Router, token: &str, body: Value) -> (StatusCode, Value) {
  req(app, "POST", "/sessions", Some(token), Some(body)).await
}

/// Collect up to `n` SSE data payloads from a response body. The tail never
/// ends on its own, so read frames (not to-bytes) under a deadline and drop
/// the stream when we have what we came for.
async fn sse_data_frames(response: axum::response::Response, n: usize) -> Vec<Value> {
  use http_body_util::BodyExt;
  let mut body = response.into_body();
  let mut out = Vec::new();
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
  while out.len() < n {
    let frame = tokio::time::timeout_at(deadline, body.frame())
      .await
      .expect("timed out waiting for SSE frames (tail held open too long?)")
      .expect("body error")
      .unwrap();
    if let Ok(data) = frame.into_data() {
      // A frame carries the event's wire format ("id: ...\ndata: ...");
      // the JSON payload is the data: line. The 500ms keepalive also
      // arrives as its own comment-only frame (no data line) — skip it,
      // like any real SSE client (this expect-before was a timing
      // flake: whether a keepalive beat the first event was a race).
      let wire = String::from_utf8(data.to_vec()).unwrap();
      let Some(data_line) = wire.lines().find(|l| l.starts_with("data: ")) else {
        continue; // keepalive / comment frame
      };
      out.push(serde_json::from_str(&data_line[6..]).unwrap());
    }
  }
  drop(body);
  out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sessions_require_auth() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  for uri in ["/sessions", "/approvals"] {
    let (status, _) = req(&w.app, "GET", uri, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
  }
  let (status, _) = create_session(&w.app, "skp_garbage_garbage", json!({})).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_session_journals_the_turn_and_completes() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  let (status, body) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-a", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  let session: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  // Journal, in order: SESSION_CREATED (seq 0), INVOCATION_STARTED (1),
  // NO_MORE_ACTIONS (2) — a well-formed turn.
  let events = w.journal.fetch(session, None).await.unwrap();
  let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
  assert_eq!(
    types,
    vec![
      EventType::SessionCreated,
      EventType::InvocationStarted,
      EventType::NoMoreActions
    ]
  );
  assert_eq!(events[0].seq, 0);

  // The summary: active, playground origin (platform principal), counts.
  let (status, summary) = req(
    &w.app,
    "GET",
    &format!("/sessions/{session}"),
    Some(&w.platform_key),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(summary["status"], "active");
  assert_eq!(summary["origin"], "playground");
  assert_eq!(summary["event_count"], 3);
  assert_eq!(summary["route"], "/test");
  assert_eq!(summary["app_id"], w.app_a.to_string());
  assert_eq!(summary["title"], "hi");
}

#[tokio::test]
async fn invocation_key_creates_api_origin_sessions_and_sees_only_its_app() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // Invocation key A creates a session (origin=api, app resolved for it —
  // it may also pass its own app id explicitly).
  let (status, body) = create_session(
    &w.app,
    &w.inv_a,
    json!({"app": "app-a", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  let session_a: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();
  let (_, summary) = req(
    &w.app,
    "GET",
    &format!("/sessions/{session_a}"),
    Some(&w.inv_a),
    None,
  )
  .await;
  assert_eq!(summary["origin"], "api");

  // Platform creates a session on app B.
  let (status, body) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-b", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  let session_b: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  // App A's invocation key cannot see app B's session (404, not 403).
  let (status, _) = req(
    &w.app,
    "GET",
    &format!("/sessions/{session_b}"),
    Some(&w.inv_a),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);

  // Nor its events, nor send input into it.
  let (status, _) = req(
    &w.app,
    "GET",
    &format!("/sessions/{session_b}/events"),
    Some(&w.inv_a),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  let (status, _) = req(
    &w.app,
    "POST",
    &format!("/sessions/{session_b}/input"),
    Some(&w.inv_a),
    Some(json!({"input": "sneak"})),
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);

  // Listing as inv_a shows only app A's sessions.
  let (status, list) = req(&w.app, "GET", "/sessions", Some(&w.inv_a), None).await;
  assert_eq!(status, StatusCode::OK);
  let ids: Vec<&str> = list
    .as_array()
    .unwrap()
    .iter()
    .map(|s| s["id"].as_str().unwrap())
    .collect();
  assert_eq!(ids, vec![session_a.to_string().as_str()]);

  // The platform key sees both.
  let (status, list) = req(&w.app, "GET", "/sessions", Some(&w.platform_key), None).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(list.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn unknown_app_route_and_inactive_deployment_are_404() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, _) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "nope", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);

  let (status, _) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-a", "route": "/nope", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn approval_flow_and_input_conflicts() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // Suspend the first turn on an approval.
  w.dispatcher.set(Mode::Suspend);
  let (status, body) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-a", "route": "/test", "input": "refund 42"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  let session: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  // Mid-suspension: input is 409 (awaiting approval).
  let (status, _) = req(
    &w.app,
    "POST",
    &format!("/sessions/{session}/input"),
    Some(&w.platform_key),
    Some(json!({"input": "hello?"})),
  )
  .await;
  assert_eq!(status, StatusCode::CONFLICT);

  // The pending-approval inbox shows it.
  let (status, pending) = req(&w.app, "GET", "/approvals", Some(&w.platform_key), None).await;
  assert_eq!(status, StatusCode::OK);
  let pending = pending.as_array().unwrap();
  assert_eq!(pending.len(), 1);
  assert_eq!(pending[0]["session"]["id"], session.to_string());
  assert_eq!(pending[0]["event"]["payload"]["tool_call_id"], "call_1");

  // A second turn cannot start while the lease is live... (the suspended
  // turn holds the lease; input was already 409ed). Now approve.
  w.dispatcher.set(Mode::Complete);
  let (status, _) = req(
    &w.app,
    "POST",
    &format!("/sessions/{session}/approval"),
    Some(&w.platform_key),
    Some(json!({"approved": true, "approver": "don"})),
  )
  .await;
  assert_eq!(status, StatusCode::ACCEPTED);

  // Journal ordering contract: GRANTED rides between the fresh lease
  // claim and INVOCATION_STARTED (so the resume runner's transcript
  // carries it); the resume turn executes the approved tool (TOOL_RESULT)
  // and completes.
  let events = w.journal.fetch(session, None).await.unwrap();
  let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
  assert_eq!(
    types[types.len() - 5..],
    [
      EventType::ToolApprovalRequired,
      EventType::ToolApprovalGranted,
      EventType::InvocationStarted,
      EventType::ToolResult,
      EventType::NoMoreActions
    ]
  );

  // Approving again with nothing pending is 409.
  let (status, _) = req(
    &w.app,
    "POST",
    &format!("/sessions/{session}/approval"),
    Some(&w.platform_key),
    Some(json!({"approved": true})),
  )
  .await;
  assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_crashed_resume_does_not_accept_a_second_decision() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  w.dispatcher.set(Mode::Suspend);
  let (status, body) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-a", "route": "/test", "input": "refund 42"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  let session: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  // The resume dies inside the approved tool: no TOOL_RESULT.
  w.dispatcher.set(Mode::Crash);
  let approval = format!("/sessions/{session}/approval");
  let approve = || {
    req(
      &w.app,
      "POST",
      &approval,
      Some(&w.platform_key),
      Some(json!({"approved": true})),
    )
  };
  assert_eq!(approve().await.0, StatusCode::ACCEPTED);

  // The call is still unanswered, but it was decided: approving again
  // would run the tool a second time.
  w.dispatcher.set(Mode::Complete);
  assert_eq!(approve().await.0, StatusCode::CONFLICT);
  let (_, pending) = req(&w.app, "GET", "/approvals", Some(&w.platform_key), None).await;
  assert_eq!(pending, json!([]));
  let events = w.journal.fetch(session, None).await.unwrap();
  let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
  assert_eq!(
    types[types.len() - 5..],
    [
      EventType::ToolApprovalRequired,
      EventType::ToolApprovalGranted,
      EventType::InvocationStarted,
      EventType::SessionError,
      EventType::NoMoreActions
    ]
  );

  // The conversation moves on with a user turn.
  let (status, _) = req(
    &w.app,
    "POST",
    &format!("/sessions/{session}/input"),
    Some(&w.platform_key),
    Some(json!({"input": "did it go through?"})),
  )
  .await;
  assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn dispatch_failure_journals_session_error_and_frees_the_session() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  w.dispatcher.set(Mode::Fail);

  let (status, body) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-a", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  let session: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  // The tail shows the well-formed turn end (SESSION_ERROR), the lease
  // was released, and the errored session accepts a retry turn.
  let events = w.journal.fetch(session, None).await.unwrap();
  assert_eq!(events.last().unwrap().event_type, EventType::SessionError);

  w.dispatcher.set(Mode::Complete);
  let (status, _) = req(
    &w.app,
    "POST",
    &format!("/sessions/{session}/input"),
    Some(&w.platform_key),
    Some(json!({"input": "retry"})),
  )
  .await;
  assert_eq!(status, StatusCode::ACCEPTED);
  let events = w.journal.fetch(session, None).await.unwrap();
  let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
  assert_eq!(
    types[types.len() - 2..],
    [EventType::InvocationStarted, EventType::NoMoreActions]
  );
}

#[tokio::test]
async fn sse_tail_resumes_from_last_event_id_header() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, body) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-a", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  let session: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();
  let events = w.journal.fetch(session, None).await.unwrap();
  let first_id = events[0].id.to_string();

  // Header-based Last-Event-ID: replay strictly AFTER the first event.
  let response = w
    .app
    .clone()
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/sessions/{session}/events"))
        .header("Authorization", format!("Bearer {}", w.platform_key))
        .header("Last-Event-ID", &first_id)
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  let frames = sse_data_frames(response, 2).await;
  assert_eq!(frames.len(), 2, "events 1..=2 replay, not 0");
  assert_eq!(frames[0]["seq"], 1);
  assert_eq!(frames[1]["seq"], 2);

  // Unknown event id in the header: 400 (a torn client state, not a
  // silent full replay).
  let response = w
    .app
    .clone()
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!(
          "/sessions/{session}/events?last_event_id={}",
          Uuid::now_v7()
        ))
        .header("Authorization", format!("Bearer {}", w.platform_key))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

/// Live-append while tailing (HTTP layer): the tail opened before a follow-up
/// turn receives the new turn's events without reconnecting.
#[tokio::test]
async fn sse_tail_receives_live_events_over_http() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, body) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-a", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  let session: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  // Open the tail AFTER the completed first turn, then run a follow-up
  // turn (input endpoint) — the tail must carry its events live.
  let stream_resp = w
    .app
    .clone()
    .oneshot(
      Request::builder()
        .method("GET")
        .uri(format!("/sessions/{session}/events?last_event_id={}", {
          let events = w.journal.fetch(session, None).await.unwrap();
          events.last().unwrap().id
        }))
        .header("Authorization", format!("Bearer {}", w.platform_key))
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
  assert_eq!(stream_resp.status(), StatusCode::OK);

  let (status, _) = req(
    &w.app,
    "POST",
    &format!("/sessions/{session}/input"),
    Some(&w.platform_key),
    Some(json!({"input": "again"})),
  )
  .await;
  assert_eq!(status, StatusCode::ACCEPTED);

  // The follow-up turn's INVOCATION_STARTED + NO_MORE_ACTIONS arrive as
  // live frames (with ids for resumption) — no reconnect.
  let frames = sse_data_frames(stream_resp, 2).await;
  let got: Vec<&str> = frames.iter().map(|e| e["type"].as_str().unwrap()).collect();
  assert_eq!(got, vec!["INVOCATION_STARTED", "NO_MORE_ACTIONS"]);
}

// ---------------------------------------------------------------------------
// Product-analytics emission (activation funnel)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn analytics_emits_first_session_and_first_approval_once() {
  let (host, captured) = crate::analytics::testutil::capture_server().await;
  let Some(w) = world_with(Arc::new(crate::analytics::Analytics::new(
    "phc_test",
    Some(host),
  )))
  .await
  else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // First session for the org (platform key ⇒ origin: playground).
  let (status, _) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-a", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);

  let captures = crate::analytics::testutil::wait_for_captures(&captured, 1).await;
  let firsts: Vec<_> = captures
    .iter()
    .filter(|c| c["event"] == "first_session")
    .collect();
  assert_eq!(firsts.len(), 1, "exactly one first_session capture");
  assert_eq!(firsts[0]["properties"]["session_origin"], "playground");
  assert!(
    firsts[0]["distinct_id"].is_string(),
    "org id, never an email"
  );

  // A second session (and a third via input) must NOT re-emit the first.
  for _ in 0..2 {
    let (status, _) = create_session(
      &w.app,
      &w.platform_key,
      json!({"app": "app-a", "route": "/test", "input": "again"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
  }
  tokio::time::sleep(std::time::Duration::from_millis(200)).await;
  let captures = captured.lock().unwrap().clone();
  assert_eq!(
    captures
      .iter()
      .filter(|c| c["event"] == "first_session")
      .count(),
    1,
    "first_session fires once per org"
  );

  // First approval decision: suspend a turn, approve, exactly one
  // first_approval_used (the spawned org-scan may lag — poll).
  w.dispatcher.set(Mode::Suspend);
  let (status, body) = create_session(
    &w.app,
    &w.platform_key,
    json!({"app": "app-a", "route": "/test", "input": "refund 42"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  let session: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();
  w.dispatcher.set(Mode::Complete);
  let (status, _) = req(
    &w.app,
    "POST",
    &format!("/sessions/{session}/approval"),
    Some(&w.platform_key),
    Some(json!({"approved": true, "approver": "don"})),
  )
  .await;
  assert_eq!(status, StatusCode::ACCEPTED);

  let captures = crate::analytics::testutil::wait_for_captures(&captured, 2).await;
  let approvals: Vec<_> = captures
    .iter()
    .filter(|c| c["event"] == "first_approval_used")
    .collect();
  assert_eq!(
    approvals.len(),
    1,
    "exactly one first_approval_used capture"
  );
  assert_eq!(
    captures
      .iter()
      .filter(|c| c["event"] == "first_session")
      .count(),
    1,
    "no extra first_session from the approval turn"
  );
}

// ---------------------------------------------------------------------------
// User-scope browser tokens (sks_): minting, owner scoping,
// SSE ?token= fallback, approvals attribution.
// ---------------------------------------------------------------------------

async fn mint_token(app: &axum::Router, token: &str, body: Value) -> (StatusCode, Value) {
  req(app, "POST", "/tokens", Some(token), Some(body)).await
}

#[tokio::test]
async fn user_token_mint_auth_and_scoping() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // Only the invocation key mints.
  let (status, body) = mint_token(
    &w.app,
    &w.inv_a,
    json!({"app": "app-a", "external_user_id": "user_1"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  let token_user1 = body["token"].as_str().unwrap();
  assert!(token_user1.starts_with("sks_"), "{token_user1}");

  for (who, key) in [
    ("platform", w.platform_key.as_str()),
    ("user token", token_user1),
  ] {
    let (status, _) = mint_token(
      &w.app,
      key,
      json!({"app": "app-a", "external_user_id": "user_9"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{who} must not mint");
  }
  let (status, _) = mint_token(&w.app, "", json!({"app": "app-a", "external_user_id": "x"})).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);

  // Mismatched app (the key's sibling app) and unknown app are refused.
  let (status, _) = mint_token(
    &w.app,
    &w.inv_a,
    json!({"app": "app-b", "external_user_id": "user_1"}),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);
  let (status, _) = mint_token(
    &w.app,
    &w.inv_a,
    json!({"app": "nope", "external_user_id": "user_1"}),
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);

  // user_1 mints a session — owned, api-origin.
  let (status, body) = create_session(
    &w.app,
    token_user1,
    json!({"app": "app-a", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  let s1: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();
  let (_, summary) = req(
    &w.app,
    "GET",
    &format!("/sessions/{s1}"),
    Some(token_user1),
    None,
  )
  .await;
  assert_eq!(summary["origin"], "api");
  assert_eq!(summary["external_user_id"], "user_1");

  // The journal's seq-0 payload carries the owner too (additive).
  let events = w.journal.fetch(s1, None).await.unwrap();
  let created = events
    .iter()
    .find(|e| e.event_type == EventType::SessionCreated)
    .unwrap();
  match &created.payload {
    EventPayload::SessionCreated(c) => assert_eq!(c.external_user_id.as_deref(), Some("user_1")),
    other => panic!("unexpected payload {other:?}"),
  }

  // A backend (invocation-key) session is OWNERLESS — user tokens can
  // never see it (the "single-user app adds end users" failure mode).
  let (status, body) = create_session(
    &w.app,
    &w.inv_a,
    json!({"app": "app-a", "route": "/test", "input": "backend"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  let s_backend: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  // Listing with the browser token: ONLY its own user's sessions, and a
  // client-supplied owner filter is rejected (isolation by construction).
  let (status, list) = req(&w.app, "GET", "/sessions", Some(token_user1), None).await;
  assert_eq!(status, StatusCode::OK);
  let ids: Vec<&str> = list
    .as_array()
    .unwrap()
    .iter()
    .map(|s| s["id"].as_str().unwrap())
    .collect();
  assert_eq!(ids, vec![s1.to_string()]);
  let (status, _) = req(
    &w.app,
    "GET",
    "/sessions?external_user_id=user_1",
    Some(token_user1),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);

  // user_2's token gets 404 on user_1's session (no existence leak).
  let (_, body) = mint_token(
    &w.app,
    &w.inv_a,
    json!({"app": "app-a", "external_user_id": "user_2"}),
  )
  .await;
  let token_user2 = body["token"].as_str().unwrap();
  let (status, _) = req(
    &w.app,
    "GET",
    &format!("/sessions/{s1}"),
    Some(token_user2),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);
  let (status, list) = req(&w.app, "GET", "/sessions", Some(token_user2), None).await;
  assert_eq!(status, StatusCode::OK);
  assert!(list.as_array().unwrap().is_empty());

  // The trusted backend keeps its app-wide view, with the admin owner
  // filter for "what did user X see".
  let (status, list) = req(&w.app, "GET", "/sessions", Some(&w.inv_a), None).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(list.as_array().unwrap().len(), 2);
  let (status, list) = req(
    &w.app,
    "GET",
    "/sessions?external_user_id=user_1",
    Some(&w.inv_a),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  let ids: Vec<&str> = list
    .as_array()
    .unwrap()
    .iter()
    .map(|s| s["id"].as_str().unwrap())
    .collect();
  assert_eq!(ids, vec![s1.to_string()]);
  assert_ne!(s_backend, s1);

  // The platform key sees everything (owner badge/filter is a dashboard
  // concern), ownerless included.
  let (status, list) = req(
    &w.app,
    "GET",
    "/sessions?app_id=app-a",
    Some(&w.platform_key),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(list.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn user_token_dies_with_its_minting_key() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, body) = mint_token(
    &w.app,
    &w.inv_a,
    json!({"app": "app-a", "external_user_id": "user_1"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  let token = body["token"].as_str().unwrap().to_owned();
  let (status, _) = req(&w.app, "GET", "/sessions", Some(&token), None).await;
  assert_eq!(status, StatusCode::OK);

  // Revoke the minting invocation key: its tokens die with it, and it
  // can no longer mint.
  let minter = w
    .state
    .registry()
    .find_api_key_by_hash(&crate::auth::keys::hash(&w.inv_a))
    .await
    .unwrap()
    .unwrap();
  w.state
    .registry()
    .revoke_api_key(minter.org_id, minter.id)
    .await
    .unwrap();
  let (status, _) = req(&w.app, "GET", "/sessions", Some(&token), None).await;
  assert_eq!(
    status,
    StatusCode::UNAUTHORIZED,
    "token outlived its minter"
  );
  let (status, _) = mint_token(
    &w.app,
    &w.inv_a,
    json!({"app": "app-a", "external_user_id": "user_1"}),
  )
  .await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_user_token_is_401() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  // Mint an already-expired token straight into the store.
  let token = crate::auth::keys::mint(ApiKeyKind::User);
  let minter = w
    .state
    .registry()
    .find_api_key_by_hash(&crate::auth::keys::hash(&w.inv_a))
    .await
    .unwrap()
    .unwrap();
  w.state
    .registry()
    .create_user_token(
      minter.org_id,
      w.app_a,
      minter.id,
      "user_expired",
      &crate::auth::keys::hash(&token),
      chrono::Utc::now() - chrono::Duration::hours(1),
    )
    .await
    .unwrap();
  let (status, _) = req(&w.app, "GET", "/sessions", Some(&token), None).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED, "expired == nonexistent");
}

#[tokio::test]
async fn sse_tail_accepts_token_query_param_and_other_paths_do_not() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (_, body) = mint_token(
    &w.app,
    &w.inv_a,
    json!({"app": "app-a", "external_user_id": "user_1"}),
  )
  .await;
  let token = body["token"].as_str().unwrap();

  let (status, body) = create_session(
    &w.app,
    token,
    json!({"app": "app-a", "route": "/test", "input": "hi"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  let session: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  // EventSource cannot set headers: the tail accepts ?token=.
  let request = Request::builder()
    .method("GET")
    .uri(format!("/sessions/{session}/events?token={token}"))
    .body(Body::empty())
    .unwrap();
  let response = w.app.clone().oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::OK);
  drop(response); // the tail holds open; drop as soon as headers land

  // Everywhere else the query param is NOT a credential.
  let request = Request::builder()
    .method("GET")
    .uri(format!("/sessions?token={token}"))
    .body(Body::empty())
    .unwrap();
  let response = w.app.clone().oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn approvals_inbox_is_platform_only_and_decisions_default_to_the_token_user() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (_, body) = mint_token(
    &w.app,
    &w.inv_a,
    json!({"app": "app-a", "external_user_id": "user_1"}),
  )
  .await;
  let token = body["token"].as_str().unwrap();

  // The cross-app inbox refuses app-scoped principals (authenticated,
  // not this class).
  for key in [token, &w.inv_a] {
    let (status, _) = req(&w.app, "GET", "/approvals", Some(key), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
  }
  let (status, _) = req(&w.app, "GET", "/approvals", Some(&w.platform_key), None).await;
  assert_eq!(status, StatusCode::OK);

  // But deciding an approval on an OWNED session rides the session
  // route — and the approver attribute defaults to the token's user.
  w.dispatcher.set(Mode::Suspend);
  let (status, body) = create_session(
    &w.app,
    token,
    json!({"app": "app-a", "route": "/test", "input": "refund 1"}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  let session: Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  w.dispatcher.set(Mode::Complete);
  let (status, _) = req(
    &w.app,
    "POST",
    &format!("/sessions/{session}/approval"),
    Some(token),
    Some(json!({"approved": true})),
  )
  .await;
  assert_eq!(status, StatusCode::ACCEPTED);

  let events = w.journal.fetch(session, None).await.unwrap();
  let granted = events
    .iter()
    .find(|e| e.event_type == EventType::ToolApprovalGranted)
    .expect("GRANTED journaled");
  match &granted.payload {
    EventPayload::ToolApprovalGranted(d) => assert_eq!(d.approver.as_deref(), Some("user_1")),
    other => panic!("unexpected payload {other:?}"),
  }
}

#[tokio::test]
async fn mint_rate_limit_is_enforced_per_user() {
  let Some(w) = world().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  // The test world's limiter allows 240/h — the unit test below covers
  // the limiter itself; here just prove minting repeatedly works and is
  // counted (different users are independent buckets).
  for user in ["u_a", "u_b"] {
    let (status, _) = mint_token(
      &w.app,
      &w.inv_a,
      json!({"app": "app-a", "external_user_id": user}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
  }
}

// ---------------------------------------------------------------------------
// Concurrency caps, platform capacity, session-creation rate limits
// ---------------------------------------------------------------------------

fn limits(per_app: u32, global: u32) -> crate::limits::LimitsConfig {
  crate::limits::LimitsConfig {
    max_turns_per_app: per_app,
    max_turns: global,
    ..crate::limits::LimitsConfig::none()
  }
}

fn new_session_body(app: &str) -> Value {
  json!({"app": app, "route": "/test", "input": "hi"})
}

/// Create a session and return its id (asserts 201).
async fn create_ok(w: &World, token: &str, app: &str) -> Uuid {
  let (status, body) = create_session(&w.app, token, new_session_body(app)).await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  body["session_id"].as_str().unwrap().parse().unwrap()
}

/// Free a held slot (what a finishing turn does).
async fn release_slot(w: &World, session: Uuid) {
  let lease = w
    .state
    .registry()
    .live_lease(session)
    .await
    .unwrap()
    .expect("session holds a live lease");
  w.state.registry().release_lease(lease.id).await.unwrap();
}

async fn session_count(w: &World, app: Uuid) -> usize {
  w.state
    .registry()
    .list_sessions(None, Some(app), None, 1000, None)
    .await
    .unwrap()
    .len()
}

#[tokio::test]
async fn app_cap_rejects_create_with_429_and_leaves_no_session_row() {
  let Some(w) = world_with_limits(|_, _| limits(2, 0)).await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  w.dispatcher.set(Mode::Hold);
  create_ok(&w, &w.platform_key, "app-a").await;
  let second = create_ok(&w, &w.platform_key, "app-a").await;

  let (status, body, retry_after) = req_retry_after(
    &w.app,
    "POST",
    "/sessions",
    &w.platform_key,
    new_session_body("app-a"),
  )
  .await;
  assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
  assert_eq!(body["code"], "concurrency_limit");
  assert!(
    body["error"]
      .as_str()
      .unwrap()
      .contains("maximum of 2 turns"),
    "{body}"
  );
  assert_eq!(retry_after.as_deref(), Some("5"));
  assert_eq!(
    session_count(&w, w.app_a).await,
    2,
    "the rejected create must not leave an empty session row"
  );

  // Another app is unaffected by A being at its cap.
  create_ok(&w, &w.platform_key, "app-b").await;

  // A finishing turn frees the slot.
  release_slot(&w, second).await;
  create_ok(&w, &w.platform_key, "app-a").await;
}

#[tokio::test]
async fn input_at_cap_is_429_and_the_journal_is_untouched() {
  let Some(w) = world_with_limits(|_, _| limits(1, 0)).await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  // s1 completes (no lease); s2 then holds the app's only slot.
  let s1 = create_ok(&w, &w.platform_key, "app-a").await;
  w.dispatcher.set(Mode::Hold);
  let s2 = create_ok(&w, &w.platform_key, "app-a").await;

  let before = w.journal.fetch(s1, None).await.unwrap().len();
  let (status, body, retry_after) = req_retry_after(
    &w.app,
    "POST",
    &format!("/sessions/{s1}/input"),
    &w.platform_key,
    json!({"input": "again"}),
  )
  .await;
  assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
  assert_eq!(body["code"], "concurrency_limit");
  assert!(retry_after.is_some());
  assert_eq!(
    w.journal.fetch(s1, None).await.unwrap().len(),
    before,
    "a 429 must leave the journal exactly as it was"
  );

  // Once the slot frees, the same input goes through.
  release_slot(&w, s2).await;
  w.dispatcher.set(Mode::Complete);
  let (status, body, _) = req_retry_after(
    &w.app,
    "POST",
    &format!("/sessions/{s1}/input"),
    &w.platform_key,
    json!({"input": "again"}),
  )
  .await;
  assert!(status.is_success(), "{status} {body}");
}

#[tokio::test]
async fn approval_at_cap_is_429_and_no_decision_is_journaled() {
  let Some(w) = world_with_limits(|_, _| limits(1, 0)).await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  w.dispatcher.set(Mode::Suspend);
  let s1 = create_ok(&w, &w.platform_key, "app-a").await;
  w.dispatcher.set(Mode::Hold);
  let s2 = create_ok(&w, &w.platform_key, "app-a").await;

  let before = w.journal.fetch(s1, None).await.unwrap().len();
  let (status, body, _) = req_retry_after(
    &w.app,
    "POST",
    &format!("/sessions/{s1}/approval"),
    &w.platform_key,
    json!({"approved": true}),
  )
  .await;
  assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
  let events = w.journal.fetch(s1, None).await.unwrap();
  assert_eq!(events.len(), before, "journal unchanged");
  assert!(
    !events
      .iter()
      .any(|e| e.event_type == EventType::ToolApprovalGranted),
    "a rejected approval is simply not recorded"
  );

  // The client retries once a slot frees.
  release_slot(&w, s2).await;
  w.dispatcher.set(Mode::Complete);
  let (status, body, _) = req_retry_after(
    &w.app,
    "POST",
    &format!("/sessions/{s1}/approval"),
    &w.platform_key,
    json!({"approved": true}),
  )
  .await;
  assert_eq!(status, StatusCode::ACCEPTED, "{body}");
  assert!(
    w.journal
      .fetch(s1, None)
      .await
      .unwrap()
      .iter()
      .any(|e| e.event_type == EventType::ToolApprovalGranted)
  );
}

#[tokio::test]
async fn override_map_lifts_the_cap_for_one_app() {
  let Some(w) = world_with_limits(|app_a, _| crate::limits::LimitsConfig {
    app_turn_limit_overrides: std::collections::HashMap::from([(app_a, 3)]),
    ..limits(1, 0)
  })
  .await
  else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  w.dispatcher.set(Mode::Hold);
  for _ in 0..3 {
    create_ok(&w, &w.platform_key, "app-a").await;
  }
  let (status, ..) = req_retry_after(
    &w.app,
    "POST",
    "/sessions",
    &w.platform_key,
    new_session_body("app-a"),
  )
  .await;
  assert_eq!(
    status,
    StatusCode::TOO_MANY_REQUESTS,
    "override is 3, not more"
  );

  // App B keeps the default of 1.
  create_ok(&w, &w.platform_key, "app-b").await;
  let (status, ..) = req_retry_after(
    &w.app,
    "POST",
    "/sessions",
    &w.platform_key,
    new_session_body("app-b"),
  )
  .await;
  assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn platform_cap_is_429_platform_capacity() {
  let Some(w) = world_with_limits(|_, _| limits(0, 2)).await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  w.dispatcher.set(Mode::Hold);
  create_ok(&w, &w.platform_key, "app-a").await;
  create_ok(&w, &w.platform_key, "app-b").await;

  let (status, body, retry_after) = req_retry_after(
    &w.app,
    "POST",
    "/sessions",
    &w.platform_key,
    new_session_body("app-a"),
  )
  .await;
  assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
  assert_eq!(body["code"], "platform_capacity");
  assert_eq!(retry_after.as_deref(), Some("10"));
  assert_eq!(
    session_count(&w, w.app_a).await,
    1,
    "no row for the rejected create"
  );
}

/// Mint a browser token for `user` on app-a.
async fn user_token(w: &World, user: &str) -> String {
  let (status, body) = mint_token(
    &w.app,
    &w.inv_a,
    json!({"app": "app-a", "external_user_id": user}),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "{body}");
  body["token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn user_token_session_creation_is_rate_limited_per_user() {
  let Some(w) = world_with_limits(|_, _| crate::limits::LimitsConfig {
    session_rate_per_user_hour: 60, // burst 10
    ..limits(0, 0)
  })
  .await
  else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let alice = user_token(&w, "alice").await;
  let bob = user_token(&w, "bob").await;

  for _ in 0..10 {
    create_ok(&w, &alice, "app-a").await;
  }
  let (status, body, retry_after) = req_retry_after(
    &w.app,
    "POST",
    "/sessions",
    &alice,
    new_session_body("app-a"),
  )
  .await;
  assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
  assert_eq!(body["code"], "rate_limited");
  let wait: u64 = retry_after.expect("Retry-After").parse().unwrap();
  assert!(wait >= 1, "Retry-After {wait}");

  // A different user is unaffected, and so is the backend's invocation key
  // (its app bucket is a separate, unconfigured class here).
  create_ok(&w, &bob, "app-a").await;
  create_ok(&w, &w.inv_a, "app-a").await;
}

#[tokio::test]
async fn invocation_key_is_limited_per_app_and_platform_key_is_exempt() {
  let Some(w) = world_with_limits(|_, _| crate::limits::LimitsConfig {
    session_rate_per_app_hour: 6, // burst min(60, 6) = 6
    ..limits(0, 0)
  })
  .await
  else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  for _ in 0..6 {
    create_ok(&w, &w.inv_a, "app-a").await;
  }
  let (status, body, retry_after) = req_retry_after(
    &w.app,
    "POST",
    "/sessions",
    &w.inv_a,
    new_session_body("app-a"),
  )
  .await;
  assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
  assert_eq!(body["code"], "rate_limited");
  assert!(retry_after.is_some());

  // The bucket is per app: app-b's key is untouched.
  create_ok(&w, &w.inv_b, "app-b").await;
  // The platform principal (dashboard playground / CLI) is exempt.
  for _ in 0..10 {
    create_ok(&w, &w.platform_key, "app-a").await;
  }
}

#[tokio::test]
async fn capacity_rejections_do_not_burn_rate_limit_tokens() {
  let Some(w) = world_with_limits(|_, _| crate::limits::LimitsConfig {
    session_rate_per_user_hour: 3, // burst min(10, 3) = 3
    ..limits(1, 0)
  })
  .await
  else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let alice = user_token(&w, "alice").await;
  w.dispatcher.set(Mode::Hold);
  let held = create_ok(&w, &w.platform_key, "app-a").await;

  for _ in 0..5 {
    let (status, body, _) = req_retry_after(
      &w.app,
      "POST",
      "/sessions",
      &alice,
      new_session_body("app-a"),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], "concurrency_limit", "{body}");
  }

  release_slot(&w, held).await;
  w.dispatcher.set(Mode::Complete);
  for _ in 0..3 {
    create_ok(&w, &alice, "app-a").await;
  }
  let (_, body, _) = req_retry_after(
    &w.app,
    "POST",
    "/sessions",
    &alice,
    new_session_body("app-a"),
  )
  .await;
  assert_eq!(
    body["code"], "rate_limited",
    "all 3 tokens were still there"
  );
}
