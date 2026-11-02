//! Ingest tests: the Contract-4 loop over a REAL WebSocket (axum
//! server bound to an ephemeral port, tokio-tungstenite client playing the
//! sidecar) against docker Postgres — assignment delivery, seq stamping +
//! acks, replay dedup, the live-lease gate, and the reconciler's abort.
//! The sidecar client is exercised at the protocol level here; the full
//! client loop (with the real engine) is the integration test.

use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use sectr_common::drivers::sandbox::DockerSandboxDriver;
use sectr_common::events::{EventPayload, EventType, MessageDelta, NewEvent, TypedDelta};
use sectr_common::reconcile::Reconciler;
use sectr_common::registry::{ApiKeyKind, DeploymentStatus, NewApp, NewDeployment, Registry};
use sectr_common::store::PgStore;

use crate::auth::keys;
use crate::ingest::{AcceptedSpawner, IngestHub};
use crate::sessions::ProdSessionService;
use crate::state::AppState;
use crate::test_pg;
use tower::util::ServiceExt;

// ---------------------------------------------------------------------------
// World: the full app (auth + sessions + ingest) on a real TCP listener,
// with a spawner that accepts every spawn and captures the ingest token.
// ---------------------------------------------------------------------------

type SidecarWs = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct World {
  app: axum::Router,
  hub: Arc<IngestHub>,
  spawner: Arc<AcceptedSpawner>,
  platform_key: String,
  registry: Arc<dyn Registry>,
  journal: Arc<dyn sectr_common::journal::Journal>,
  /// The store's pool (closing it simulates a database outage).
  pool: sqlx::PgPool,
  session: uuid::Uuid,
  deployment_id: uuid::Uuid,
  addr: std::net::SocketAddr,
}

fn test_config(lease_ttl_secs: u64) -> crate::config::ServerConfig {
  crate::config::ServerConfig {
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
    user_token_ttl_secs: 86_400,
    mint_rate_limit_per_hour: 240,
    lease_ttl_secs,
    runner_server_url: "ws://172.17.0.1:8080/ingest".into(),
    runner_sidecar_cmd: vec!["/usr/local/bin/sectr-worker".into()],
    runner_image: None,
    runner_base_image: None,
    build_dir: "/tmp/sectr-build".into(),
    dashboard_url: "https://app.sectr.test".into(),
  }
}

async fn world(lease_ttl_secs: u64) -> Option<World> {
  let store: PgStore = test_pg::fresh_store().await?;
  let registry: Arc<dyn Registry> = Arc::new(store.registry());
  let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
  let org = registry.create_org("test-org", "default").await.unwrap();
  let app = registry
    .create_app(NewApp {
      org_id: org.id,
      name: "test-app".into(),
      slug: "test-app".into(),
    })
    .await
    .unwrap();
  let deployment = registry
    .create_deployment(NewDeployment {
      app_id: app.id,
      version: 1,
      manifest: json!({
          "routes": [{"path": "/assist"}],
          "runner": {"command": ["python3", "-m", "agent"]},
          "approval_tools": ["refund"]
      }),
    })
    .await
    .unwrap();
  registry
    .set_deployment_status(deployment.id, DeploymentStatus::Active)
    .await
    .unwrap();
  registry
    .set_secret(
      app.id,
      "OPENAI_API_KEY",
      sectr_common::secret::Secret::new("sk-test"),
    )
    .await
    .unwrap();

  let platform = keys::mint(ApiKeyKind::Platform);
  registry
    .create_api_key(
      org.id,
      None,
      ApiKeyKind::Platform,
      "test platform",
      &keys::hash(&platform),
    )
    .await
    .unwrap();

  let ttl = chrono::Duration::seconds(lease_ttl_secs as i64);
  let hub = Arc::new(IngestHub::new(registry.clone(), journal.clone(), ttl));
  let spawner = Arc::new(AcceptedSpawner::default());
  let dispatcher = crate::ingest::SidecarDispatcher::new(
    hub.clone(),
    registry.clone(),
    journal.clone(),
    spawner.clone(),
  );
  let service = Arc::new(ProdSessionService::new(
    registry.clone(),
    journal.clone(),
    Arc::new(dispatcher),
    ttl,
  ));
  let config = test_config(lease_ttl_secs);
  let state = AppState::new(
    registry.clone(),
    journal.clone(),
    service,
    hub.clone(),
    config.clone(),
    Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
    Arc::new(crate::platform::DockerPlatform::new(
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      DockerSandboxDriver::connect_env().ok().map(Arc::new),
      config,
      registry.clone(),
    )),
    false,
  );

  let router = crate::http::router(state);
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  {
    let server_router = router.clone();
    tokio::spawn(async move { axum::serve(listener, server_router).await.expect("server") });
  }

  // Create the session over the HTTP surface (dispatch happens inside:
  // lease → INVOCATION_STARTED → SidecarDispatcher → AcceptedSpawner).
  let request = axum::http::Request::builder()
    .method("POST")
    .uri("/sessions")
    .header("Authorization", format!("Bearer {platform}"))
    .header("Content-Type", "application/json")
    .body(axum::body::Body::from(
      json!({"app": "test-app", "route": "/assist", "input": "hi"}).to_string(),
    ))
    .unwrap();
  let response = router.clone().oneshot(request).await.unwrap();
  assert_eq!(response.status(), axum::http::StatusCode::CREATED);
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  let body: Value = serde_json::from_slice(&bytes).unwrap();
  let session: uuid::Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  let deployment_id = registry.get_session(session).await.unwrap().deployment_id;

  Some(World {
    app: router,
    hub,
    spawner,
    platform_key: platform,
    registry,
    journal,
    pool: store.pool().clone(),
    session,
    deployment_id,
    addr,
  })
}

/// The token minted for the session's first dispatch.
async fn ingest_token(w: &World) -> String {
  w.spawner
    .next_token()
    .await
    .expect("dispatch must have minted an ingest token")
}

async fn dial(w: &World, token: &str) -> SidecarWs {
  let uri = format!("ws://{}/ingest", w.addr);
  let mut request: tokio_tungstenite::tungstenite::http::Request<()> =
    uri.into_client_request().unwrap();
  request.headers_mut().insert(
    "Authorization",
    tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
      .unwrap(),
  );
  let (ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
  ws
}

async fn send_json(ws: &mut SidecarWs, value: &Value) {
  ws.send(WsMessage::text(serde_json::to_string(value).unwrap()))
    .await
    .unwrap();
}

/// Next text frame as JSON (5s deadline — the server must be responsive).
async fn recv_json(ws: &mut SidecarWs, label: &str) -> Value {
  let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
    .expect("connection closed while waiting for {label}")
    .expect("ws error");
  serde_json::from_str(&msg.into_text().expect("text frame")).expect("server frame is JSON")
}

/// Await a text frame and assert it is an `abort` with a reason containing
/// `needle`.
async fn expect_abort(ws: &mut SidecarWs, needle: &str) {
  let frame = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
    .await
    .expect("timed out waiting for the abort frame")
    .expect("connection closed before the abort arrived")
    .expect("ws error");
  let value: Value = serde_json::from_str(&frame.into_text().unwrap()).unwrap();
  assert_eq!(value["kind"], "abort", "expected abort, got {value}");
  assert!(
    value["reason"].as_str().unwrap().contains(needle),
    "abort reason {value} lacks {needle:?}"
  );
}

fn delta(payload_text: &str) -> NewEvent {
  NewEvent::new(EventPayload::MessageDelta(MessageDelta {
    message_id: "msg_1".into(),
    delta: TypedDelta::Text {
      text: payload_text.to_owned(),
      extra: Default::default(),
    },
    extra: Default::default(),
  }))
}

fn approval_required() -> NewEvent {
  NewEvent::new(EventPayload::ToolApprovalRequired(
    sectr_common::events::ToolApprovalRequired {
      tool_call_id: "call_1".into(),
      name: "issue_refund".into(),
      arguments: json!({"amount": 10}),
      reason: None,
      extra: Default::default(),
    },
  ))
}

fn no_more_actions() -> NewEvent {
  NewEvent::new(EventPayload::NoMoreActions(Default::default()))
}

/// An event batch frame: (batch_id, frame JSON).
fn batch(events: Vec<NewEvent>) -> (uuid::Uuid, Value) {
  let batch_id = uuid::Uuid::now_v7();
  (
    batch_id,
    serde_json::to_value(crate::ingest::SidecarFrame::Event { batch_id, events }).unwrap(),
  )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The full happy loop: dispatch → dial → assignment (transcript with
/// INVOCATION_STARTED, secrets, runner spec, approval tools) → event batch →
/// ack with server-stamped seqs → journal contents → complete → lease
/// released.
#[tokio::test]
async fn assignment_delivery_ingest_acks_and_complete() {
  let Some(w) = world(30).await else { return };

  // Dispatch happened inside create_session; the sidecar dials now.
  let token = ingest_token(&w).await;
  let mut ws = dial(&w, &token).await;

  // First frame: the pending assignment.
  let assignment = recv_json(&mut ws, "assignment").await;
  assert_eq!(assignment["kind"], "assignment");
  assert_eq!(assignment["session_id"], w.session.to_string());
  assert_eq!(assignment["deployment_id"], w.deployment_id.to_string());
  assert_eq!(assignment["route"], "/assist");
  assert_eq!(
    assignment["runner"]["command"],
    json!(["python3", "-m", "agent"])
  );
  assert_eq!(assignment["approval_tools"], json!(["refund"]));
  assert_eq!(assignment["env"]["OPENAI_API_KEY"], "sk-test");
  let transcript = assignment["transcript"].as_array().unwrap();
  let transcript_types: Vec<&str> = transcript
    .iter()
    .map(|e| e["type"].as_str().unwrap())
    .collect();
  assert_eq!(
    transcript_types,
    ["SESSION_CREATED", "INVOCATION_STARTED"],
    "the assignment transcript includes this invocation's INVOCATION_STARTED"
  );

  // ready, then one batch of runner events.
  send_json(&mut ws, &json!({"kind": "ready"})).await;
  let e1 = delta("hello");
  let e2 = delta("world");
  let ids = [e1.id, e2.id];
  let (batch_id, frame) = batch(vec![e1, e2]);
  send_json(&mut ws, &frame).await;

  let ack = recv_json(&mut ws, "ack").await;
  assert_eq!(ack["kind"], "ack");
  assert_eq!(ack["batch_id"], json!(batch_id.to_string()));
  let entries = ack["events"].as_array().unwrap();
  assert_eq!(entries.len(), 2);
  assert_eq!(entries[0]["id"], json!(ids[0].to_string()));
  assert_eq!(
    entries[0]["seq"], 2,
    "0=SESSION_CREATED, 1=INVOCATION_STARTED"
  );
  assert_eq!(entries[0]["duplicate"], false);
  assert_eq!(entries[1]["seq"], 3);

  // Journal: the stamped events are exactly-once, gap-free.
  let events = w.journal.fetch(w.session, None).await.unwrap();
  assert_eq!(events.len(), 4);
  assert_eq!(events[2].seq, 2);
  assert_eq!(events[2].id, ids[0]);
  assert_eq!(events[3].id, ids[1]);

  // complete → lease released (the turn is over).
  send_json(
    &mut ws,
    &json!({"kind": "complete", "outcome": "completed"}),
  )
  .await;
  tokio::time::sleep(std::time::Duration::from_millis(100)).await;
  assert!(w.registry.live_lease(w.session).await.unwrap().is_none());
}

/// Reconnect semantics: replaying already-acked events (same producer-
/// minted ids) acks duplicate:true with the original seqs and changes
/// nothing in the journal.
#[tokio::test]
async fn replayed_events_are_deduped_by_id() {
  let Some(w) = world(30).await else { return };
  let token = ingest_token(&w).await;
  let mut ws = dial(&w, &token).await;
  let _assignment = recv_json(&mut ws, "assignment").await;
  send_json(&mut ws, &json!({"kind": "ready"})).await;

  let e1 = delta("hello");
  let id = e1.id;
  let (_, frame) = batch(vec![e1]);
  send_json(&mut ws, &frame).await;
  let ack = recv_json(&mut ws, "batch ack").await;
  assert_eq!(ack["events"][0]["seq"], 2);
  assert_eq!(ack["events"][0]["duplicate"], false);

  // Replay the SAME event id in a new batch.
  let batch_id = uuid::Uuid::now_v7();
  let frame = serde_json::to_value(crate::ingest::SidecarFrame::Event {
    batch_id,
    events: vec![NewEvent::with_id(delta("hello"), id)],
  })
  .unwrap();
  let (batch_id, frame) = (batch_id, frame);
  send_json(&mut ws, &frame).await;
  let replay_ack = recv_json(&mut ws, "replay ack").await;
  assert_eq!(replay_ack["batch_id"], json!(batch_id.to_string()));
  assert_eq!(replay_ack["events"][0]["duplicate"], true);
  assert_eq!(replay_ack["events"][0]["seq"], 2, "original seq preserved");

  // Nothing was double-journaled.
  let events = w.journal.fetch(w.session, None).await.unwrap();
  assert_eq!(events.len(), 3);
}

/// After the turn ends (complete), a further event batch hits the
/// live-lease gate: abort + connection close. This is what keeps runner
/// events from landing after the turn's end.
#[tokio::test]
async fn ingest_after_turn_end_aborts() {
  let Some(w) = world(30).await else { return };
  let token = ingest_token(&w).await;
  let mut ws = dial(&w, &token).await;
  let _assignment = recv_json(&mut ws, "assignment").await;
  send_json(&mut ws, &json!({"kind": "ready"})).await;

  // End the turn cleanly.
  send_json(
    &mut ws,
    &json!({"kind": "complete", "outcome": "completed"}),
  )
  .await;
  tokio::time::sleep(std::time::Duration::from_millis(100)).await;

  // A late batch: the lease is gone → abort.
  let (_, frame) = batch(vec![delta("late")]);
  send_json(&mut ws, &frame).await;
  expect_abort(&mut ws, "no longer live").await;
}

/// A database failure at ingest (here the heartbeat, the first
/// DB write of a batch) is a `journal` failure, not a lost lease — so
/// `ingest.failures{reason="journal"}` (CRIT rule 6) sees a DB outage.
#[tokio::test]
async fn db_failure_at_ingest_is_a_journal_error_not_a_lost_lease() {
  let Some(w) = world(30).await else { return };
  let token = ingest_token(&w).await;
  let mut ws = dial(&w, &token).await;
  let _assignment = recv_json(&mut ws, "assignment").await;
  send_json(&mut ws, &json!({"kind": "ready"})).await;

  w.pool.close().await;
  let (_, frame) = batch(vec![delta("during the outage")]);
  send_json(&mut ws, &frame).await;
  expect_abort(&mut ws, "journal").await;
}

/// Boundary policy (refined live 2026-09-17): BOTH
/// boundary kinds just release the grant (the token dies; the sidecar
/// exits itself per its post-turn policy) — the platform teardown hook
/// never fires on the happy path (a cancel there races the sidecar's
/// exit). The approval resume is a fresh dial with a fresh token
/// receiving the resume assignment.
#[tokio::test]
async fn turn_boundaries_release_the_grant_and_the_sidecar_exits_itself() {
  let Some(w) = world(30).await else { return };

  struct Recording(Arc<tokio::sync::Mutex<Vec<uuid::Uuid>>>);
  #[async_trait::async_trait]
  impl crate::platform::RunnerPlatform for Recording {
    async fn build(
      &self,
      _req: crate::platform::BuildRequest,
    ) -> Result<crate::platform::BuildOutput, crate::platform::BuildError> {
      Err(crate::platform::BuildError::bare(
        "test fake — never builds",
      ))
    }

    fn provisional_build_ref(&self, _deployment_id: uuid::Uuid, _version: i64) -> String {
      String::new()
    }

    async fn spawn(
      &self,
      _session: uuid::Uuid,
      _deployment_id: uuid::Uuid,
      _invocation_id: uuid::Uuid,
      _ingest_token: &str,
    ) -> Result<Option<String>, crate::dispatch::DispatchError> {
      Ok(None)
    }

    async fn teardown(&self, session: uuid::Uuid, _handle: Option<&str>) {
      self.0.lock().await.push(session);
    }
  }
  let killed = Arc::new(tokio::sync::Mutex::new(Vec::new()));
  w.hub.set_teardown(Arc::new(Recording(killed.clone())));

  let token1 = ingest_token(&w).await;

  // --- suspension: the boundary policy TERMINATES ---
  let mut ws = dial(&w, &token1).await;
  let _ = recv_json(&mut ws, "initial assignment").await;
  send_json(&mut ws, &json!({"kind": "ready"})).await;
  let (_, frame) = batch(vec![approval_required()]);
  send_json(&mut ws, &frame).await;
  let ack = recv_json(&mut ws, "suspension ack").await;
  assert_eq!(ack["kind"], "ack");
  wait_lease_gone(&w).await;
  for _ in 0..50 {
    if !killed.lock().await.is_empty() {
      break;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  tokio::time::sleep(std::time::Duration::from_millis(100)).await;
  assert!(
    killed.lock().await.is_empty(),
    "the boundary policy releases the grant — no platform teardown call"
  );
  // The torn-down token is invalid (401 on the next dial).
  let mut request: tokio_tungstenite::tungstenite::http::Request<()> =
    format!("ws://{}/ingest", w.addr)
      .into_client_request()
      .unwrap();
  request.headers_mut().insert(
    "Authorization",
    tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {token1}"))
      .unwrap(),
  );
  let err = tokio_tungstenite::connect_async(request).await.unwrap_err();
  assert!(
    matches!(
        &err,
        tokio_tungstenite::tungstenite::Error::Http(response)
            if response.status() == tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED
    ),
    "the torn-down token is invalid (401), got: {err:?}"
  );

  // The resume is a FRESH dispatch: a new token is minted and the
  // process re-spawns (no live connection to ride).
  let (status, _) = post(
    &w.app,
    &format!("/sessions/{}/approval", w.session),
    &w.platform_key,
    json!({"approved": true}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED);
  let token2 = ingest_token(&w).await;
  assert_ne!(token1, token2, "every turn mints a fresh token");

  // The fresh process dials with the fresh token and receives the
  // resume assignment.
  let mut ws2 = dial(&w, &token2).await;
  let resume = recv_json(&mut ws2, "resume assignment").await;
  assert_eq!(
    resume["kind"], "assignment",
    "the resume assignment rides the new dial"
  );

  // --- turn end: the sentinel tears down too ---
  let (_, frame) = batch(vec![no_more_actions()]);
  send_json(&mut ws2, &frame).await;
  let ack2 = recv_json(&mut ws2, "sentinel ack").await;
  assert_eq!(ack2["kind"], "ack");
  wait_lease_gone(&w).await;
  tokio::time::sleep(std::time::Duration::from_millis(100)).await;
  assert!(
    killed.lock().await.is_empty(),
    "the sentinel also just releases the grant"
  );

  // The next dispatch re-mints again (fresh token → fresh container).
  let (status, _) = post(
    &w.app,
    &format!("/sessions/{}/input", w.session),
    &w.platform_key,
    json!({"input": "one more turn"}),
  )
  .await;
  assert_eq!(
    status,
    axum::http::StatusCode::ACCEPTED,
    "a torn-down session can start a new turn (cold re-invoke)"
  );
  let token3 = ingest_token(&w).await;
  assert_ne!(token2, token3);
}

/// The lease expired and the reconciler reclaimed it while the sidecar
/// was connected: the reclaim SYNTHESIZES the turn end (no sidecar events
/// ever arrived) and the hub's abort hook kills the sidecar's connection
/// (abort frame + close). End to end through the reconciler's real code.
#[tokio::test]
async fn reclaim_synthesizes_the_turn_end_and_aborts_the_sidecar() {
  let Some(w) = world(1).await else { return };
  let token = ingest_token(&w).await;
  tokio::time::sleep(std::time::Duration::from_millis(1100)).await; // lease expires

  let mut ws = dial(&w, &token).await;
  let _assignment = recv_json(&mut ws, "assignment").await;
  send_json(&mut ws, &json!({"kind": "ready"})).await;

  // No events were ever ingested (the heartbeat-before-append race is
  // not in play) — the reconciler owns the turn now.
  let reconciler = Reconciler::new(
    w.registry.clone(),
    w.journal.clone(),
    std::time::Duration::from_secs(15),
  )
  .with_aborter(Arc::new(crate::ingest::HubAbort(w.hub.clone())));
  let reclaimed = reconciler.scan_once().await.unwrap();
  assert_eq!(reclaimed.len(), 1, "the expired lease is reclaimed");
  assert!(w.registry.live_lease(w.session).await.unwrap().is_none());

  // The sidecar sees the abort frame and the connection close.
  expect_abort(&mut ws, "lease expired").await;

  // The journal got the reconciler's synthesized turn end.
  let events = w.journal.fetch(w.session, None).await.unwrap();
  let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
  assert_eq!(
    types[2..],
    [EventType::SessionError, EventType::NoMoreActions]
  );
}

/// The heartbeat-before-append race won by the sidecar: a batch arriving
/// after TTL expiry but BEFORE the reconciler's scan extends the lease and
/// is accepted (an expired-but-not-reclaimed turn lives — dev-parity
/// semantics; the atomic reclaim closes the opposite ordering).
#[tokio::test]
async fn expired_but_unreclaimed_lease_accepts_a_late_batch() {
  let Some(w) = world(1).await else { return };
  let token = ingest_token(&w).await;
  tokio::time::sleep(std::time::Duration::from_millis(1100)).await; // expired, unreclaimed

  let mut ws = dial(&w, &token).await;
  let _assignment = recv_json(&mut ws, "assignment").await;
  send_json(&mut ws, &json!({"kind": "ready"})).await;

  let (_, frame) = batch(vec![delta("late but alive")]);
  send_json(&mut ws, &frame).await;
  let ack = recv_json(&mut ws, "ack").await;
  assert_eq!(
    ack["kind"], "ack",
    "heartbeat-before-append extends the expired lease"
  );
  assert_eq!(ack["events"][0]["seq"], 2);
}

/// A wrong token is rejected at the upgrade (401), and a foreign token
/// never reaches a connection.
#[tokio::test]
async fn bad_ingest_token_is_rejected() {
  let Some(w) = world(30).await else { return };
  let _ = ingest_token(&w).await; // ensure a grant exists

  let uri = format!("ws://{}/ingest", w.addr);
  let mut request: tokio_tungstenite::tungstenite::http::Request<()> =
    uri.into_client_request().unwrap();
  request.headers_mut().insert(
    "Authorization",
    tokio_tungstenite::tungstenite::http::HeaderValue::from_str(
      "Bearer sit_wrong_token_value_0000000000000000000000000",
    )
    .unwrap(),
  );
  let result = tokio_tungstenite::connect_async(request).await;
  assert!(result.is_err(), "the upgrade must be rejected (401)");
}

/// Unknown frame kinds are ignored (forward compat): the connection stays
/// open and later frames still work.
#[tokio::test]
async fn unknown_frame_kind_is_ignored() {
  let Some(w) = world(30).await else { return };
  let token = ingest_token(&w).await;
  let mut ws = dial(&w, &token).await;
  let _assignment = recv_json(&mut ws, "_assignment").await;

  send_json(&mut ws, &json!({"kind": "from_the_future", "stuff": 1})).await;
  // Still alive: the complete frame is processed normally.
  send_json(
    &mut ws,
    &json!({"kind": "complete", "outcome": "completed"}),
  )
  .await;
  tokio::time::sleep(std::time::Duration::from_millis(100)).await;
  assert!(w.registry.live_lease(w.session).await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// E2E: the REAL sidecar client loop + the fake runner — everything but the
// container (the client speaks the protocol; the runner serves the invoke
// contract over HTTP). Turn 1 runs; turn 2 rides the SAME connection
// (one connection per container).
// ---------------------------------------------------------------------------

fn free_port() -> u16 {
  std::net::TcpListener::bind("127.0.0.1:0")
    .unwrap()
    .local_addr()
    .unwrap()
    .port()
}

async fn post(
  app: &axum::Router,
  uri: &str,
  token: &str,
  body: Value,
) -> (axum::http::StatusCode, Value) {
  let request = axum::http::Request::builder()
    .method("POST")
    .uri(uri)
    .header("Authorization", format!("Bearer {token}"))
    .header("Content-Type", "application/json")
    .body(axum::body::Body::from(body.to_string()))
    .unwrap();
  let response = app.clone().oneshot(request).await.unwrap();
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  let value = if bytes.is_empty() {
    Value::Null
  } else {
    serde_json::from_slice(&bytes).unwrap()
  };
  (status, value)
}

async fn journal_types(w: &World) -> Vec<EventType> {
  w.journal
    .fetch(w.session, None)
    .await
    .unwrap()
    .iter()
    .map(|e| e.event_type)
    .collect()
}

/// Poll until the journal tail matches `want` (1s deadline) — the turn ran.
async fn wait_for_tail(w: &World, want: &[EventType]) {
  for _ in 0..40 {
    if journal_types(w).await.ends_with(want) {
      return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  panic!(
    "journal tail never reached {want:?} — got {:?}",
    journal_types(w).await
  );
}

/// Wait for the fake runner to bind its port (dev mode has the same
/// readiness gate — SECTR_PORT poll; prod: the WS `ready` frame).
async fn wait_runner_ready(port: u16) {
  for _ in 0..100 {
    if tokio::net::TcpStream::connect(("127.0.0.1", port))
      .await
      .is_ok()
    {
      return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
  }
  panic!("fake runner never bound its port");
}

async fn wait_lease_gone(w: &World) {
  for _ in 0..40 {
    if w.registry.live_lease(w.session).await.unwrap().is_none() {
      return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  panic!("lease never released");
}

#[tokio::test]
async fn full_loop_real_sidecar_client_and_fake_runner() {
  // Quiet by default (sqlx logs every statement at DEBUG); opt in with
  // e.g. RUST_LOG=debug cargo test -- --nocapture.
  let _ = tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
    )
    .with_test_writer()
    .try_init();
  let Some(w) = world(30).await else { return };

  // The fake runner (python3 stdlib SSE server), scripted for turn 1.
  let port = free_port();
  let frames_file =
    std::env::temp_dir().join(format!("sectr-e2e-frames-{}.json", uuid::Uuid::now_v7()));
  sectr_worker::testing::script_turn(
    &frames_file,
    &[
      sectr_worker::testing::delta_frame("checking the order"),
      sectr_worker::testing::delta_frame("done"),
      sectr_worker::testing::final_frame(),
    ],
  );
  let script_path = sectr_worker::testing::fake_runner_command()[1].clone();
  let mut runner = tokio::process::Command::new("python3")
    .arg(&script_path)
    .env("SECTR_PORT", port.to_string())
    .env("SECTR_FRAMES_FILE", &frames_file)
    .spawn()
    .expect("spawn fake runner");

  wait_runner_ready(port).await;
  let token = ingest_token(&w).await;
  let sidecar = sectr_worker::sidecar::SidecarClient::new(
    sectr_worker::sidecar::SidecarConfig {
      ingest_url: format!("ws://{}/ingest", w.addr),
      ingest_token: token,
      runner_base: format!("http://127.0.0.1:{port}"),
      reconnect_backoff: std::time::Duration::from_millis(100),
      max_backoff: std::time::Duration::from_millis(500),
    },
    Arc::new(sectr_worker::sidecar::ExternalRunner),
  );
  tokio::spawn(async move { sidecar.run().await });

  // Turn 1 runs to completion through the whole stack: server dispatch →
  // WS assignment → sidecar engine → fake runner → validated events →
  // journal writes → complete → lease release.
  wait_for_tail(
    &w,
    &[
      EventType::MessageDelta,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ],
  )
  .await;
  wait_lease_gone(&w).await;

  // Turn 2 = a FRESH container (teardown policy: the sentinel at
  // turn 1's end killed the container; the old sidecar client 401-exits
  // on its dead token). Rewrite the fake runner's script, submit the
  // follow-up, and spawn the replacement sidecar with the new token.
  sectr_worker::testing::script_turn(
    &frames_file,
    &[
      sectr_worker::testing::delta_frame("again"),
      sectr_worker::testing::final_frame(),
    ],
  );
  let (status, _) = post(
    &w.app,
    &format!("/sessions/{}/input", w.session),
    &w.platform_key,
    json!({"input": "and now?"}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED);
  let token2 = ingest_token(&w).await;
  let sidecar2 = sectr_worker::sidecar::SidecarClient::new(
    sectr_worker::sidecar::SidecarConfig {
      ingest_url: format!("ws://{}/ingest", w.addr),
      ingest_token: token2,
      runner_base: format!("http://127.0.0.1:{port}"),
      reconnect_backoff: std::time::Duration::from_millis(100),
      max_backoff: std::time::Duration::from_millis(500),
    },
    Arc::new(sectr_worker::sidecar::ExternalRunner),
  );
  tokio::spawn(async move { sidecar2.run().await });
  wait_for_tail(
    &w,
    &[
      EventType::InvocationStarted,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ],
  )
  .await;
  wait_lease_gone(&w).await;

  runner.kill().await.ok();
}

/// Approval E2E: the fake runner suspends the turn (TOOL_APPROVAL_REQUIRED
/// final), the platform approves over HTTP, and the resume turn executes
/// the approved tool — all through the real sidecar client.
#[tokio::test]
async fn approval_flow_through_the_real_sidecar() {
  let Some(w) = world(30).await else { return };

  let port = free_port();
  let frames_file =
    std::env::temp_dir().join(format!("sectr-e2e-frames-{}.json", uuid::Uuid::now_v7()));
  sectr_worker::testing::script_turn(
    &frames_file,
    &[
      sectr_worker::testing::delta_frame("may I refund?"),
      sectr_worker::testing::approval_frame(),
    ],
  );
  let script_path = sectr_worker::testing::fake_runner_command()[1].clone();
  let mut runner = tokio::process::Command::new("python3")
    .arg(&script_path)
    .env("SECTR_PORT", port.to_string())
    .env("SECTR_FRAMES_FILE", &frames_file)
    .spawn()
    .unwrap();

  wait_runner_ready(port).await;
  let token = ingest_token(&w).await;
  let sidecar = sectr_worker::sidecar::SidecarClient::new(
    sectr_worker::sidecar::SidecarConfig {
      ingest_url: format!("ws://{}/ingest", w.addr),
      ingest_token: token.clone(),
      runner_base: format!("http://127.0.0.1:{port}"),
      reconnect_backoff: std::time::Duration::from_millis(100),
      max_backoff: std::time::Duration::from_millis(500),
    },
    Arc::new(sectr_worker::sidecar::ExternalRunner),
  );
  tokio::spawn(async move { sidecar.run().await });

  // Turn 1 suspends on the approval.
  wait_for_tail(
    &w,
    &[EventType::MessageDelta, EventType::ToolApprovalRequired],
  )
  .await;
  wait_lease_gone(&w).await; // suspension releases the lease

  // Approve: the boundary policy TERMINATED the first sidecar's grant
  // (its connection got dropped and its token died), so the
  // resume dispatch mints a FRESH token and spawns a FRESH sidecar. The
  // test mirrors production: dial a new SidecarClient with the new
  // token, which receives the pending resume assignment on attach.
  sectr_worker::testing::script_turn(
    &frames_file,
    &[
      sectr_worker::testing::delta_frame("refunding"),
      sectr_worker::testing::final_frame(),
    ],
  );
  let (status, _) = post(
    &w.app,
    &format!("/sessions/{}/approval", w.session),
    &w.platform_key,
    json!({"approved": true}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED);

  let token2 = ingest_token(&w).await;
  assert_ne!(token, token2, "every turn mints a fresh token");
  let sidecar2 = sectr_worker::sidecar::SidecarClient::new(
    sectr_worker::sidecar::SidecarConfig {
      ingest_url: format!("ws://{}/ingest", w.addr),
      ingest_token: token2,
      runner_base: format!("http://127.0.0.1:{port}"),
      reconnect_backoff: std::time::Duration::from_millis(100),
      max_backoff: std::time::Duration::from_millis(500),
    },
    Arc::new(sectr_worker::sidecar::ExternalRunner),
  );
  tokio::spawn(async move { sidecar2.run().await });

  wait_for_tail(
    &w,
    &[
      EventType::ToolApprovalGranted,
      EventType::InvocationStarted,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ],
  )
  .await;
  wait_lease_gone(&w).await;

  runner.kill().await.ok();
}

// ---------------------------------------------------------------------------
// The real-container E2E (docker-gated — skips without a daemon)
// ---------------------------------------------------------------------------

/// The runner image used by the container E2E: ubuntu 26.04 (the sandbox's
/// glibc — the built binaries run unmodified) + the sidecar binary + the
/// fake runner binary. Built once, then cached by the daemon.
const E2E_IMAGE: &str = "sectr-runner-e2e:latest";

fn copy_dir(src: &std::path::Path, dst: &std::path::Path, skip: &[&str]) {
  std::fs::create_dir_all(dst).unwrap();
  for entry in std::fs::read_dir(src).unwrap() {
    let entry = entry.unwrap();
    let name = entry.file_name().to_string_lossy().into_owned();
    if skip
      .iter()
      .any(|s| name == *s || (s.ends_with('*') && name.ends_with(&s[..s.len() - 1])))
    {
      continue;
    }
    let to = dst.join(&name);
    if entry.file_type().unwrap().is_dir() {
      copy_dir(&entry.path(), &to, skip);
    } else {
      std::fs::copy(entry.path(), &to).unwrap();
    }
  }
}

/// Build the E2E image if absent: `cargo build` the worker bins, then
/// `docker build` a two-binary image. Panics on failure — a broken build
/// is exactly what the test is for.
fn ensure_e2e_image() -> bool {
  // These tests copy the HOST-built worker binaries into a Linux image and
  // reach the host via the Linux docker0 gateway (172.17.0.1), so they can
  // only work on a Linux host. `just test` also opts out explicitly.
  if !cfg!(target_os = "linux") || std::env::var_os("SECTR_SKIP_DOCKER_E2E").is_some() {
    return false;
  }
  let Ok(docker) = std::process::Command::new("docker").arg("version").output() else {
    return false;
  };
  if !docker.status.success() {
    return false;
  }
  // ALWAYS build: docker's layer cache makes a no-op rebuild cheap, and
  // an early-return-on-exists shipped STALE worker binaries when the
  // sidecar changed (found live 2026-09-17 — the boundary-exit policy
  // never reached the cached image).
  let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .and_then(std::path::Path::parent)
    .expect("workspace root")
    .to_path_buf();
  let status = std::process::Command::new("cargo")
    .args(["build", "-p", "sectr-worker", "--bins"])
    .current_dir(&workspace)
    .status()
    .expect("cargo");
  assert!(
    status.success(),
    "cargo build -p sectr-worker --bins failed"
  );

  let ctx = std::env::temp_dir().join(format!("sectr-e2e-img-{}", uuid::Uuid::now_v7()));
  std::fs::create_dir_all(&ctx).unwrap();
  // Same target dir the inner `cargo build` above writes (honors
  // CARGO_TARGET_DIR — the bins must come from the build that JUST ran,
  // not a stale sibling target).
  let target_debug = std::env::var("CARGO_TARGET_DIR")
    .map(std::path::PathBuf::from)
    .unwrap_or_else(|_| workspace.join("target"))
    .join("debug");
  for bin in ["sectr-worker", "fake-runner"] {
    std::fs::copy(target_debug.join(bin), ctx.join(bin)).expect("copy binary into build context");
  }
  // The SDK goes into the image (pip-installed) — the authoritative
  // import and the agent runtime are real python.
  copy_dir(
    &workspace.join("sdk-python"),
    &ctx.join("sdk-python"),
    &["__pycache__", ".venv", "*.egg-info"],
  );
  std::fs::write(
        ctx.join("Dockerfile"),
        // The deploy path needs a real interpreter: python3 (apt) + the
        // SDK pip-installed (the authoritative import + the agent runtime).
        "FROM ubuntu:26.04\n\
         RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-pip && rm -rf /var/lib/apt/lists/*\n\
         COPY sectr-worker /usr/local/bin/sectr-worker\n\
         COPY fake-runner /usr/local/bin/fake-runner\n\
         COPY sdk-python /opt/sdk-python\n\
         RUN pip install --break-system-packages --no-cache-dir /opt/sdk-python\n",
    )
    .unwrap();
  let build = std::process::Command::new("docker")
    .args(["build", "-t", E2E_IMAGE, "."])
    .current_dir(&ctx)
    .output()
    .expect("docker build");
  // The build context is ~230 MB per run (SDK + two binaries) and this
  // helper runs on every E2E invocation — never clean up = 74 dirs /
  // 17 GB filled /tmp and killed the disk (found 2026-09-17). Best-
  // effort removal after the build; docker has already digested the
  // context into image layers.
  std::fs::remove_dir_all(&ctx).ok();
  assert!(
    build.status.success(),
    "docker build failed: {}",
    String::from_utf8_lossy(&build.stderr)
  );
  true
}

/// The docker world: like [`world`], but the server binds 0.0.0.0 (the
/// container dials the bridge gateway), the spawner is the REAL
/// [`DockerPlatform`], and the manifest carries the in-container
/// runner command + frames env.
async fn world_docker(
  lease_ttl_secs: u64,
) -> Option<(World, Arc<crate::platform::DockerPlatform>)> {
  let store: PgStore = test_pg::fresh_store().await?;
  let registry: Arc<dyn Registry> = Arc::new(store.registry());
  let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
  let org = registry.create_org("test-org", "default").await.unwrap();
  let app = registry
    .create_app(NewApp {
      org_id: org.id,
      name: "docker-app".into(),
      slug: "docker-app".into(),
    })
    .await
    .unwrap();

  let frames = json!([
    sectr_worker::testing::delta_frame("from inside the container"),
    sectr_worker::testing::delta_frame("twice"),
    sectr_worker::testing::final_frame(),
  ]);
  let deployment = registry
    .create_deployment(NewDeployment {
      app_id: app.id,
      version: 1,
      manifest: json!({
          "routes": [{"path": "/assist"}],
          "runner": {
              "image": E2E_IMAGE,
              "command": ["/usr/local/bin/fake-runner"],
              "env": {"SECTR_FRAMES_JSON": frames.to_string()},
          },
          "approval_tools": ["refund"],
      }),
    })
    .await
    .unwrap();
  registry
    .set_deployment_status(deployment.id, DeploymentStatus::Active)
    .await
    .unwrap();

  let platform = keys::mint(ApiKeyKind::Platform);
  registry
    .create_api_key(
      org.id,
      None,
      ApiKeyKind::Platform,
      "docker test platform",
      &keys::hash(&platform),
    )
    .await
    .unwrap();

  // Bind 0.0.0.0 FIRST — the port feeds the runner server URL (what the
  // container dials), so the config/spawner must be built after this.
  let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
  let addr = listener.local_addr().unwrap();

  let ttl = chrono::Duration::seconds(lease_ttl_secs as i64);
  let driver = DockerSandboxDriver::connect_env().expect("docker");
  let mut config = test_config(lease_ttl_secs);
  config.runner_image = Some(E2E_IMAGE.to_owned());
  config.runner_server_url = format!("ws://172.17.0.1:{}/ingest", addr.port());
  let artifact_store: Arc<dyn sectr_common::drivers::ArtifactStore> =
    Arc::new(sectr_common::drivers::FilesystemArtifactStore::new(
      std::env::temp_dir().join(format!("sectr-artifacts-{}", uuid::Uuid::now_v7())),
    ));
  let spawner = Arc::new(crate::platform::DockerPlatform::new(
    artifact_store.clone(),
    Some(Arc::new(driver.clone())),
    config.clone(),
    registry.clone(),
  ));
  let hub = Arc::new(IngestHub::new(registry.clone(), journal.clone(), ttl));
  hub.set_teardown(spawner.clone());
  let dispatcher = crate::ingest::SidecarDispatcher::new(
    hub.clone(),
    registry.clone(),
    journal.clone(),
    spawner.clone() as Arc<dyn crate::platform::RunnerPlatform>,
  );
  let service = Arc::new(ProdSessionService::new(
    registry.clone(),
    journal.clone(),
    Arc::new(dispatcher),
    ttl,
  ));
  let state = AppState::new(
    registry.clone(),
    journal.clone(),
    service,
    hub.clone(),
    config.clone(),
    artifact_store,
    spawner.clone(),
    false,
  );
  let router = crate::http::router(state);

  {
    let server_router = router.clone();
    tokio::spawn(async move { axum::serve(listener, server_router).await.expect("server") });
  }

  // Create the session over HTTP (dispatch spawns the container).
  let request = axum::http::Request::builder()
    .method("POST")
    .uri("/sessions")
    .header("Authorization", format!("Bearer {platform}"))
    .header("Content-Type", "application/json")
    .body(axum::body::Body::from(
      json!({"app": "docker-app", "route": "/assist", "input": "hi"}).to_string(),
    ))
    .unwrap();
  let response = router.clone().oneshot(request).await.unwrap();
  assert_eq!(response.status(), axum::http::StatusCode::CREATED);
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  let body: Value = serde_json::from_slice(&bytes).unwrap();
  let session: uuid::Uuid = body["session_id"].as_str().unwrap().parse().unwrap();

  Some((
    World {
      app: router,
      hub,
      spawner: Arc::new(AcceptedSpawner::default()),
      platform_key: platform,
      registry,
      journal,
      pool: store.pool().clone(),
      session,
      deployment_id: deployment.id,
      addr,
    },
    spawner,
  ))
}

/// Wait for the journal tail to end with `want` (journal only — no World).
async fn wait_tail(
  journal: &Arc<dyn sectr_common::journal::Journal>,
  session: uuid::Uuid,
  want: &[EventType],
) {
  for _ in 0..120 {
    let types: Vec<EventType> = journal
      .fetch(session, None)
      .await
      .unwrap()
      .iter()
      .map(|e| e.event_type)
      .collect();
    if types.ends_with(want) {
      return;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  panic!("journal tail never reached {want:?}");
}

/// The real-container E2E: a REAL runner container (built image, real
/// sidecar binary) dials the REAL server over the docker bridge, picks up
/// the assignment, spawns the in-container runner, and streams validated
/// events through ingest — then the sentinel tears the container down and a
/// follow-up turn cold-spawns a fresh one (teardown-at-turn-end policy).
#[tokio::test]
async fn real_container_runs_turns_end_to_end() {
  if !ensure_e2e_image() {
    eprintln!("skipping: no docker daemon");
    return;
  }
  let Some((w, spawner)) = world_docker(30).await else {
    return;
  };
  let driver = spawner.driver();
  let name = crate::platform::docker::runner_container_name(w.session);

  // Turn 1: the container spawned, dialed in, ran the turn in-container.
  wait_tail(
    &w.journal,
    w.session,
    &[
      EventType::MessageDelta,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ],
  )
  .await;
  wait_lease_gone(&w).await;
  assert!(
    driver.exists(&name).await.unwrap(),
    "container alive mid-session"
  );
  // ...torn down by the sentinel:
  for _ in 0..60 {
    if !driver.exists(&name).await.unwrap() {
      break;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  assert!(
    !driver.exists(&name).await.unwrap(),
    "sentinel must tear the container down"
  );

  // Turn 2: cold re-invoke — a fresh container, same session.
  let (status, _) = post(
    &w.app,
    &format!("/sessions/{}/input", w.session),
    &w.platform_key,
    json!({"input": "one more turn"}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED);
  wait_tail(
    &w.journal,
    w.session,
    &[
      EventType::NoMoreActions,
      EventType::InvocationStarted,
      EventType::MessageDelta,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ],
  )
  .await;
  for _ in 0..60 {
    if !driver.exists(&name).await.unwrap() {
      break;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  assert!(
    !driver.exists(&name).await.unwrap(),
    "turn 2's container torn down too"
  );
}

// ---------------------------------------------------------------------------
// The deploy-path E2E — the REAL pipeline end to end
// ---------------------------------------------------------------------------

/// A minimal dependency-free agent (the baked SDK is enough): one route
/// that streams a single delta and exhausts (the driver emits the
/// sentinel). The whole point: this source goes in the tarball and the
/// IMAGE runs it through the SDK's FastAPI adapter.
const E2E_APP_MAIN: &str = r#"
from collections.abc import AsyncIterator
from uuid import uuid4

from sectr import AgentApp, SessionContext
from sectr.events import MessageDelta, MessageEnd, MessageStart, RunnerEvent

app = AgentApp(name="deploy-e2e-app")


@app.agent_route("/assist")
async def assist(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
    msg_id = uuid4().hex[:16]
    yield MessageStart(role="assistant", message_id=msg_id)
    yield MessageDelta(message_id=msg_id, delta={"type": "text", "text": "hello from the built image"})
    yield MessageEnd(message_id=msg_id)
"#;

const E2E_APP_TOML: &str = r#"
[app]
name = "deploy-e2e-app"

[agent]
entry = "main:app"
python = "3.13"
"#;

/// Tar+gzip a directory of (relative path, content) pairs — the test's
/// version of `sectr deploy`'s packaging.
fn package_app(files: &[(&str, &str)]) -> Vec<u8> {
  let mut tar = tar::Builder::new(Vec::new());
  for (path, content) in files {
    let mut header = tar::Header::new_gnu();
    let data = content.as_bytes();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, path, data).unwrap();
  }
  let tarball = tar.into_inner().unwrap();
  let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
  std::io::Write::write_all(&mut gz, &tarball).unwrap();
  gz.finish().unwrap()
}

/// Hand-built multipart body (fixed boundary) for `POST /deployments`.
fn multipart_body(tarball: &[u8], manifest_toml: &str) -> (String, Vec<u8>) {
  let boundary = "sectr-e2e-boundary";
  let mut body = Vec::new();
  body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
  body.extend_from_slice(
    b"Content-Disposition: form-data; name=\"tarball\"; filename=\"app.tar.gz\"\r\n\r\n",
  );
  body.extend_from_slice(tarball);
  body.extend_from_slice(b"\r\n");
  body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
  body.extend_from_slice(b"Content-Disposition: form-data; name=\"manifest\"\r\n\r\n");
  body.extend_from_slice(manifest_toml.as_bytes());
  body.extend_from_slice(b"\r\n");
  body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
  (boundary.to_owned(), body)
}

/// The deploy world: like [`world_docker`], but nothing is pre-created —
/// the test drives the REAL deploy pipeline (multipart upload → build →
/// activate) and then runs a session against the built image.
async fn world_deploy(lease_ttl_secs: u64) -> Option<World> {
  let store: PgStore = test_pg::fresh_store().await?;
  let registry: Arc<dyn Registry> = Arc::new(store.registry());
  let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
  let org = registry.create_org("test-org", "default").await.unwrap();
  let platform = keys::mint(ApiKeyKind::Platform);
  registry
    .create_api_key(
      org.id,
      None,
      ApiKeyKind::Platform,
      "deploy test platform",
      &keys::hash(&platform),
    )
    .await
    .unwrap();

  // Bind 0.0.0.0 FIRST (the port feeds the runner server URL).
  let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
  let addr = listener.local_addr().unwrap();

  let ttl = chrono::Duration::seconds(lease_ttl_secs as i64);
  let driver = DockerSandboxDriver::connect_env().expect("docker");
  let mut config = test_config(lease_ttl_secs);
  config.runner_base_image = Some(E2E_IMAGE.to_owned());
  config.runner_server_url = format!("ws://172.17.0.1:{}/ingest", addr.port());
  config.build_dir = std::env::temp_dir()
    .join(format!("sectr-build-{}", uuid::Uuid::now_v7()))
    .to_string_lossy()
    .into_owned();

  let artifact_store: Arc<dyn sectr_common::drivers::ArtifactStore> =
    Arc::new(sectr_common::drivers::FilesystemArtifactStore::new(
      std::env::temp_dir().join(format!("sectr-artifacts-{}", uuid::Uuid::now_v7())),
    ));
  let spawner = Arc::new(crate::platform::DockerPlatform::new(
    artifact_store.clone(),
    Some(Arc::new(driver)),
    config.clone(),
    registry.clone(),
  ));
  let hub = Arc::new(IngestHub::new(registry.clone(), journal.clone(), ttl));
  hub.set_teardown(spawner.clone());
  let dispatcher = crate::ingest::SidecarDispatcher::new(
    hub.clone(),
    registry.clone(),
    journal.clone(),
    spawner.clone() as Arc<dyn crate::platform::RunnerPlatform>,
  );
  let service = Arc::new(ProdSessionService::new(
    registry.clone(),
    journal.clone(),
    Arc::new(dispatcher),
    ttl,
  ));
  let state = AppState::new(
    registry.clone(),
    journal.clone(),
    service,
    hub.clone(),
    config.clone(),
    artifact_store,
    spawner.clone(),
    false,
  );
  let router = crate::http::router(state);
  {
    let server_router = router.clone();
    tokio::spawn(async move { axum::serve(listener, server_router).await.expect("server") });
  }

  Some(World {
    app: router,
    hub,
    spawner: Arc::new(AcceptedSpawner::default()),
    platform_key: platform,
    registry,
    journal,
    pool: store.pool().clone(),
    session: uuid::Uuid::nil(), // set by the test after creation
    deployment_id: uuid::Uuid::nil(),
    addr,
  })
}

/// Deploy E2E: `POST /deployments` (multipart tarball + sectr.toml) runs the
/// REAL build pipeline — unpack, `docker build` on the E2E base, the
/// authoritative import in a one-shot container — and activates; a session
/// then runs the deployed app IN the built container through the full
/// spawn path; the sentinel tears the container down.
#[tokio::test]
async fn deploy_builds_an_image_and_runs_it() {
  if !ensure_e2e_image() {
    eprintln!("skipping: no docker daemon");
    return;
  }
  let Some(mut w) = world_deploy(30).await else {
    return;
  };

  // 1. Deploy: multipart upload of the packaged app.
  let tarball = package_app(&[("main.py", E2E_APP_MAIN)]);
  let (boundary, body) = multipart_body(&tarball, E2E_APP_TOML);
  let request = axum::http::Request::builder()
    .method("POST")
    .uri("/deployments")
    .header("Authorization", format!("Bearer {}", w.platform_key))
    .header(
      "Content-Type",
      format!("multipart/form-data; boundary={boundary}"),
    )
    .body(axum::body::Body::from(body))
    .unwrap();
  let response = w.app.clone().oneshot(request).await.unwrap();
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  // 202 semantics: the upload returns immediately with the row in
  // `building`; the build resolves in a background task.
  assert_eq!(
    status,
    axum::http::StatusCode::ACCEPTED,
    "{}",
    String::from_utf8_lossy(&bytes)
  );
  let deployed: Value = serde_json::from_slice(&bytes).unwrap();
  assert_eq!(deployed["status"], "building", "{deployed}");
  assert_eq!(deployed["active"], false, "{deployed}");
  assert_eq!(deployed["version"], 1);
  let app_id: uuid::Uuid = deployed["app_id"].as_str().unwrap().parse().unwrap();
  let deployment_id: uuid::Uuid = deployed["id"].as_str().unwrap().parse().unwrap();

  // Poll the registry (as the CLI polls the list endpoint) until the
  // background build activates the deployment.
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
  let row = loop {
    let r = w.registry.get_deployment(deployment_id).await.unwrap();
    if r.status != DeploymentStatus::Building {
      break r;
    }
    assert!(
      tokio::time::Instant::now() < deadline,
      "background build never resolved"
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
  };
  assert_eq!(
    row.status,
    DeploymentStatus::Active,
    "background build failed: {:?}",
    row.error
  );

  // Routing history reflects the activation.
  let request = axum::http::Request::builder()
    .method("GET")
    .uri(format!("/apps/{app_id}/deployments"))
    .header("Authorization", format!("Bearer {}", w.platform_key))
    .body(axum::body::Body::empty())
    .unwrap();
  let response = w.app.clone().oneshot(request).await.unwrap();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  let history: Value = serde_json::from_slice(&bytes).unwrap();
  assert_eq!(history.as_array().unwrap().len(), 1, "{history}");
  assert_eq!(history[0]["active"], true, "{history}");

  // 2. A session runs the deployed app IN the built container.
  let request = axum::http::Request::builder()
    .method("POST")
    .uri("/sessions")
    .header("Authorization", format!("Bearer {}", w.platform_key))
    .header("Content-Type", "application/json")
    .body(axum::body::Body::from(
      json!({"app": "deploy-e2e-app", "route": "/assist", "input": "hi"}).to_string(),
    ))
    .unwrap();
  let response = w.app.clone().oneshot(request).await.unwrap();
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  assert_eq!(
    status,
    axum::http::StatusCode::CREATED,
    "{}",
    String::from_utf8_lossy(&bytes)
  );
  let created: Value = serde_json::from_slice(&bytes).unwrap();
  w.session = created["session_id"].as_str().unwrap().parse().unwrap();
  w.deployment_id = deployment_id;

  wait_tail(
    &w.journal,
    w.session,
    &[
      EventType::MessageDelta,
      EventType::MessageEnd,
      EventType::NoMoreActions,
    ],
  )
  .await;
  wait_lease_gone(&w).await;

  // 3. The sentinel tore the container down.
  let driver = DockerSandboxDriver::connect_env().unwrap();
  let name = crate::platform::docker::runner_container_name(w.session);
  for _ in 0..60 {
    if !driver.exists(&name).await.unwrap() {
      break;
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
  assert!(
    !driver.exists(&name).await.unwrap(),
    "the built container must be torn down at the sentinel"
  );
}

// ---------------------------------------------------------------------------
// Restart durability — the REAL binary, SIGKILLed and respawned
// ---------------------------------------------------------------------------

/// The restore test at the binary level (user-approved shape): spawn
/// `sectr-server` against a throwaway PG database, run three sessions into
/// distinct states (completed / awaiting approval / mid-turn with a hung
/// runner), SIGKILL the process, respawn it on the same port, and assert
/// every recovery path:
///   1. journal + registry rows survive (reads, SSE `Last-Event-ID` replay
///      through the RESTARTED binary, follow-up turns);
///   2. an approval decision across the restart re-spawns a fresh container
///      (the in-memory grant map died with the process — the turn's
///      token is gone, the deterministic container name collides with the
///      orphan, `remove-if-exists` wins);
///   3. a mid-turn crash is reclaimed by the reconciler (the hung runner
///      holds its SSE open — no stream end, so the ENGINE cannot synthesize
///      the turn end; only the lease expiry can) and the session stays
///      usable for follow-up turns.
#[tokio::test]
async fn server_restart_durability_end_to_end() {
  use sectr_common::registry::{NewApp, NewDeployment};

  if !ensure_e2e_image() {
    eprintln!("skipping: no docker daemon");
    return;
  }
  let Some(base_url) = crate::test_pg::fresh_db_url().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // Locate the built server binary: <target>/debug/deps/<this test> →
  // <target>/debug/sectr-server.
  let server_bin = std::env::current_exe()
    .unwrap()
    .ancestors()
    .nth(2)
    .unwrap()
    .join("sectr-server");
  assert!(
    server_bin.exists(),
    "server binary not built at {server_bin:?}"
  );

  // Reserve a port for BOTH server runs (kernel frees it on SIGKILL).
  let port = {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
  };
  let api = format!("http://127.0.0.1:{port}");
  let bootstrap = "skp_restart_e2e_key_0000000000000000000000000";

  /// Kills the child even when the test panics — a leaked server holds PG
  /// pools + a 15s reconciler loop, which poisons every LATER test run
  /// (found the hard way: 10 orphans made the 10s tail-timeout flake).
  struct KillOnDrop(std::process::Child);
  impl Drop for KillOnDrop {
    fn drop(&mut self) {
      let _ = self.0.kill();
      let _ = self.0.wait();
    }
  }

  let spawn_server = |run: u8| {
    let log = std::fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(format!("/tmp/restart-e2e-server-{run}.log"))
      .unwrap();
    std::process::Command::new(&server_bin)
      .arg("serve")
      .env("SECTR_PG_URL", &base_url)
      .env("SECTR_BIND_ADDR", format!("0.0.0.0:{port}"))
      .env("SECTR_BOOTSTRAP_PLATFORM_KEY", bootstrap)
      .env("SECTR_RUNNER_IMAGE", E2E_IMAGE)
      .env(
        "SECTR_RUNNER_SERVER_URL",
        format!("ws://172.17.0.1:{port}/ingest"),
      )
      // Short TTL: leg 3's reclaim must fit the test budget.
      .env("SECTR_LEASE_TTL_SECS", "3")
      .env("RUST_LOG", "warn")
      .stdout(std::process::Stdio::null())
      .stderr(log)
      .spawn()
      .map(KillOnDrop)
      .expect("spawn sectr-server")
  };

  async fn wait_health(client: &reqwest::Client, api: &str, run: u8) {
    for _ in 0..100 {
      if let Ok(resp) = client.get(format!("{api}/healthz")).send().await
        && resp.status().is_success()
      {
        return;
      }
      tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // Server stderr goes to a file CI never uploads; surface it here.
    let log = std::fs::read_to_string(format!("/tmp/restart-e2e-server-{run}.log"))
      .unwrap_or_else(|e| format!("<no log: {e}>"));
    let tail: Vec<&str> = log.lines().rev().take(40).collect();
    panic!(
      "server run {run} never became healthy at {api}; log tail:\n{}",
      tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    );
  }

  async fn journal_types(
    journal: &Arc<dyn sectr_common::journal::Journal>,
    session: uuid::Uuid,
  ) -> Vec<EventType> {
    journal
      .fetch(session, None)
      .await
      .unwrap()
      .into_iter()
      .map(|e| e.event_type)
      .collect()
  }

  /// Poll until the journal's event-type list CONTAINS `want` (in order,
  /// not necessarily contiguously) — for states the turn blows past.
  async fn wait_journal_contains(
    journal: &Arc<dyn sectr_common::journal::Journal>,
    session: uuid::Uuid,
    want: &[EventType],
    timeout: std::time::Duration,
  ) {
    let start = std::time::Instant::now();
    loop {
      let types = journal_types(journal, session).await;
      let mut i = 0;
      for t in &types {
        if i < want.len() && t == &want[i] {
          i += 1;
        }
      }
      if i == want.len() {
        return;
      }
      assert!(
        start.elapsed() < timeout,
        "journal never contained {want:?} (have {types:?})"
      );
      tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
  }

  /// Poll until the journal's event-type list ENDS with `want`.
  async fn wait_journal(
    journal: &Arc<dyn sectr_common::journal::Journal>,
    session: uuid::Uuid,
    want: &[EventType],
    timeout: std::time::Duration,
  ) {
    let start = std::time::Instant::now();
    loop {
      let types = journal_types(journal, session).await;
      if types.len() >= want.len() && types[types.len() - want.len()..] == *want {
        return;
      }
      assert!(
        start.elapsed() < timeout,
        "journal never reached {want:?} (have {types:?})"
      );
      tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
  }

  let start = std::time::Instant::now();
  // --- server #1 ---------------------------------------------------------
  let mut server = spawn_server(1);
  let client = reqwest::Client::new();
  wait_health(&client, &api, 1).await;

  // Our own handles for seeding + journal assertions (same DB; the
  // binary has its own pools).
  let store = PgStore::open(&base_url).await.unwrap();
  let registry = store.registry();
  let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
  let org = registry
    .find_org_by_slug("default")
    .await
    .unwrap()
    .expect("bootstrap created the org");

  // Three apps, one per scenario (frame scripts are deployment-scoped).
  let complete_frames = serde_json::json!([
    sectr_worker::testing::delta_frame("alpha"),
    sectr_worker::testing::delta_frame("beta"),
    sectr_worker::testing::final_frame(),
  ]);
  let approve_frames = serde_json::json!([
    sectr_worker::testing::delta_frame("working"),
    sectr_worker::testing::approval_frame(),
  ]);
  let hang_frames = serde_json::json!([sectr_worker::testing::delta_frame("stalling"),]);
  let mut sessions = std::collections::HashMap::new();
  for (slug, frames_env) in [
    ("dur-complete", complete_frames.clone()),
    ("dur-approve", approve_frames.clone()),
    ("dur-hang", hang_frames.clone()),
  ] {
    let app = registry
      .create_app(NewApp {
        org_id: org.id,
        name: slug.into(),
        slug: slug.into(),
      })
      .await
      .unwrap();
    let mut env = serde_json::json!({ "SECTR_FRAMES_JSON": frames_env.to_string() });
    if slug == "dur-hang" {
      // The runner goes SILENT but holds the SSE open — the engine
      // sees no stream end; only the lease expiry can end the turn.
      env["SECTR_HANG_SECS"] = serde_json::json!("300");
    }
    let deployment = registry
      .create_deployment(NewDeployment {
        app_id: app.id,
        version: 1,
        manifest: serde_json::json!({
            "routes": [{"path": "/assist"}],
            "runner": {
                "image": E2E_IMAGE,
                "command": ["/usr/local/bin/fake-runner"],
                "env": env,
            },
        }),
      })
      .await
      .unwrap();
    registry
      .set_deployment_status(deployment.id, DeploymentStatus::Active)
      .await
      .unwrap();

    let resp = client
      .post(format!("{api}/sessions"))
      .bearer_auth(bootstrap)
      .json(&serde_json::json!({"app": slug, "route": "/assist", "input": "go"}))
      .send()
      .await
      .unwrap();
    assert_eq!(resp.status(), 201, "{slug} session create");
    let sid: uuid::Uuid = resp.json::<serde_json::Value>().await.unwrap()["session_id"]
      .as_str()
      .unwrap()
      .parse()
      .unwrap();
    sessions.insert(slug, sid);
  }
  let s_complete = sessions["dur-complete"];
  let s_approve = sessions["dur-approve"];
  let s_hang = sessions["dur-hang"];

  // Reach the three states.
  wait_journal(
    &journal,
    s_complete,
    &[EventType::MessageDelta, EventType::NoMoreActions],
    std::time::Duration::from_secs(30),
  )
  .await;
  wait_journal(
    &journal,
    s_approve,
    &[EventType::ToolApprovalRequired],
    std::time::Duration::from_secs(30),
  )
  .await;
  wait_journal_contains(
    &journal,
    s_hang,
    &[EventType::MessageDelta],
    std::time::Duration::from_secs(30),
  )
  .await;

  // The durable reference point: s_complete's events (for the SSE
  // resume assertion after the restart).
  let before: Vec<sectr_common::events::Event> = journal.fetch(s_complete, None).await.unwrap();
  let resume_from = before[1].id.to_string(); // seq 1
  let before_count = before.len();

  eprintln!(
    "[restart-e2e] phase 1 (pre-restart) done at {:?}",
    start.elapsed()
  );
  // --- SIGKILL ------------------------------------------------------------
  // (kill + wait on the guard's child; the guard would do it on drop too)
  server.0.kill().unwrap();
  server.0.wait().unwrap();

  // --- server #2 (same DB, same port) ------------------------------------
  let server = spawn_server(2);
  wait_health(&client, &api, 2).await;

  eprintln!(
    "[restart-e2e] phase 2 (post-restart reads) done at {:?}",
    start.elapsed()
  );
  // 1a. Rows survived: the completed session is listed and complete.
  let resp = client
    .get(format!("{api}/sessions/{s_complete}"))
    .bearer_auth(bootstrap)
    .send()
    .await
    .unwrap();
  assert_eq!(resp.status(), 200);
  let body: serde_json::Value = resp.json().await.unwrap();
  assert_eq!(body["event_count"].as_i64().unwrap(), before_count as i64);

  // 1b. SSE resume through the RESTARTED binary: Last-Event-ID replays
  //     exactly the events after seq 1.
  {
    use futures_util::StreamExt;
    let resp = client
      .get(format!("{api}/sessions/{s_complete}/events"))
      .bearer_auth(bootstrap)
      .header("Last-Event-ID", resume_from.clone())
      .send()
      .await
      .unwrap();
    assert_eq!(resp.status(), 200);
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut replayed_seqs: Vec<i64> = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), async {
      while let Some(Ok(chunk)) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buf.find("\n\n") {
          let frame = String::from_utf8_lossy(&buf.as_bytes()[..pos]).to_string();
          buf.drain(..pos + 2);
          if let Some(data) = frame.lines().find_map(|l| l.strip_prefix("data: "))
            && let Ok(ev) = serde_json::from_str::<serde_json::Value>(data)
          {
            replayed_seqs.push(ev["seq"].as_i64().unwrap());
          }
        }
        if replayed_seqs.len() >= before_count - 2 {
          break;
        }
      }
    })
    .await;
    // before_count events total (seq 0..N-1); resuming from seq 1
    // replays seq 2..N-1.
    let expected: Vec<i64> = (2..before_count as i64).collect();
    assert_eq!(replayed_seqs, expected, "resume after restart");
  }

  // 1c. Follow-up turn on the completed session: works after the restart.
  client
    .post(format!("{api}/sessions/{s_complete}/input"))
    .bearer_auth(bootstrap)
    .json(&serde_json::json!({"input": "again"}))
    .send()
    .await
    .unwrap();
  wait_journal(
    &journal,
    s_complete,
    &[
      EventType::InvocationStarted,
      EventType::MessageDelta,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ],
    std::time::Duration::from_secs(30),
  )
  .await;

  eprintln!(
    "[restart-e2e] phase 3 (follow-up) done at {:?}",
    start.elapsed()
  );
  // 2. Approval decision ACROSS the restart: the grant map died with the
  //    process; the decision must still resume — journal the decision,
  //    spawn a FRESH container (same deterministic name as the orphan),
  //    and stream the resume turn's deltas.
  client
    .post(format!("{api}/sessions/{s_approve}/approval"))
    .bearer_auth(bootstrap)
    .json(&serde_json::json!({"approved": true}))
    .send()
    .await
    .unwrap();
  // The resume re-suspends on the static approval frame — the suffix is
  // not stable; assert the decision → fresh invocation → delta IN ORDER.
  wait_journal_contains(
    &journal,
    s_approve,
    &[
      EventType::ToolApprovalGranted,
      EventType::InvocationStarted,
      EventType::MessageDelta,
    ],
    std::time::Duration::from_secs(30),
  )
  .await;

  eprintln!(
    "[restart-e2e] phase 4 (approval resume) done at {:?}",
    start.elapsed()
  );
  // 3. Mid-turn crash recovery: the hung runner's lease expired, the
  //    reconciler synthesized the abandoned turn's end (SESSION_ERROR +
  //    sentinel), and the session stays usable.
  wait_journal(
    &journal,
    s_hang,
    &[EventType::SessionError, EventType::NoMoreActions],
    std::time::Duration::from_secs(40),
  )
  .await;
  client
    .post(format!("{api}/sessions/{s_hang}/input"))
    .bearer_auth(bootstrap)
    .json(&serde_json::json!({"input": "after crash"}))
    .send()
    .await
    .unwrap();
  // The follow-up ALSO dies by lease expiry (the hang frames hang every
  // invocation) — the assertion is that the session is USABLE: a second
  // invocation ran and produced a delta after the reconciled crash.
  let start = std::time::Instant::now();
  loop {
    let types = journal_types(&journal, s_hang).await;
    let second_invocation = types
      .iter()
      .rposition(|t| *t == EventType::InvocationStarted)
      .expect("at least one invocation");
    let ran = types[second_invocation..].contains(&EventType::MessageDelta);
    assert!(
      ran || start.elapsed() < std::time::Duration::from_secs(30),
      "follow-up after the crash never ran (have {types:?})"
    );
    if ran {
      break;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
  }

  // NOTE: container end-state is a POLICY outcome and is
  // unit-tested at the hub's boundary policy — the E2E asserts only
  // durability outcomes (journal integrity + the follow-up ran).
  // Best-effort cleanup of the three runners, without asserting:
  let driver = DockerSandboxDriver::connect_env().unwrap();
  for s in [s_complete, s_approve, s_hang] {
    let _ = driver
      .kill(&crate::platform::docker::runner_container_name(s))
      .await;
  }

  drop(server); // the guard kills + reaps
}

/// Concurrent turns on ONE deployment : two sessions
/// dispatch independently, both spawn their own process (per-turn tokens),
/// both dial, and both stream into the journal with independent seqs —
/// no cross-talk, no grant collision (this is the per-job concurrent-
/// executions shape; the hub never serializes across sessions).
#[tokio::test]
async fn two_concurrent_turns_on_one_deployment() {
  let Some(w) = world(30).await else { return };
  let token_a = ingest_token(&w).await; // session A: created by world()

  // Session B on the SAME deployment, over the HTTP surface.
  let (status, body) = post(
    &w.app,
    "/sessions",
    &w.platform_key,
    json!({"app": "test-app", "route": "/assist", "input": "from B"}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::CREATED);
  let session_b: uuid::Uuid = body["session_id"].as_str().unwrap().parse().unwrap();
  assert_ne!(session_b, w.session);
  let token_b = ingest_token(&w).await; // session B's dispatch spawned too

  // Both grants coexist — two live executions of one deployment's job.
  let mut ws_a = dial(&w, &token_a).await;
  let mut ws_b = dial(&w, &token_b).await;
  let a = recv_json(&mut ws_a, "A assignment").await;
  let b = recv_json(&mut ws_b, "B assignment").await;
  assert_eq!(a["kind"], "assignment");
  assert_eq!(b["kind"], "assignment");
  assert_ne!(
    a["session_id"], b["session_id"],
    "each execution carries its own session (assignment fields are flattened)"
  );
  assert_eq!(a["session_id"], json!(w.session.to_string()));
  assert_eq!(b["session_id"], json!(session_b.to_string()));

  // Both sides ready; one event batch each, interleaved.
  send_json(&mut ws_a, &json!({"kind": "ready"})).await;
  send_json(&mut ws_b, &json!({"kind": "ready"})).await;
  let (_, frame_a) = batch(vec![delta("from A")]);
  let (_, frame_b) = batch(vec![delta("from B")]);
  send_json(&mut ws_a, &frame_a).await;
  send_json(&mut ws_b, &frame_b).await;
  let ack_a = recv_json(&mut ws_a, "A ack").await;
  let ack_b = recv_json(&mut ws_b, "B ack").await;
  assert_eq!(ack_a["events"][0]["seq"], 2);
  assert_eq!(ack_b["events"][0]["seq"], 2, "independent per-session seqs");

  // The journal holds BOTH turns under their own sessions.
  let a_events = w.journal.fetch(w.session, None).await.unwrap();
  let b_events = w.journal.fetch(session_b, None).await.unwrap();
  assert_eq!(a_events.len(), 3);
  assert_eq!(b_events.len(), 3);
  assert_eq!(a_events[2].seq, 2);
  assert_eq!(b_events[2].seq, 2);
}
