//! Endpoint deployments against the real ingest hub: the wake (retries
//! before delivery only, one request in flight, the failure messages), the
//! routing platform, registration, and end-to-end turns through a real
//! serve-mode sidecar.
//!
//! The fake endpoint is strict: it answers only with what a real
//! serve-mode sidecar or a real platform can produce, forwards to a real
//! serve-mode sidecar when the script says so, and counts concurrent
//! requests.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::IntoResponse;

use super::serve::{CountingRunner, fake_runner_manifest, serve_config, start_router, temp_path};
use super::*;

use crate::config::EndpointConfig;
use sectr_worker::runner::{SubprocessRunner, SubprocessRunnerConfig};

// ---------------------------------------------------------------------------
// The fake endpoint
// ---------------------------------------------------------------------------

/// One scripted answer to `POST /wake`.
#[derive(Clone, Debug)]
enum Step {
  /// What a platform or the sidecar answers before reaching a turn.
  Status(u16, &'static str),
  /// The sidecar's `200 token_rejected` (its dial got 401).
  TokenRejected,
  /// Forward to the real serve-mode sidecar.
  Forward,
  /// Accept and never answer, never dial.
  Hang,
  /// Wait, then forward.
  Slow(Duration),
  /// Dial ingest with the token like a sidecar, take the assignment, then
  /// break the request (the instance died mid-turn).
  BreakAfterDelivery,
  /// Dial and take the assignment like `BreakAfterDelivery`, then answer
  /// with this status (a platform's time limit ended the request).
  StatusAfterDelivery(u16),
  /// A 307 elsewhere (must not be followed).
  Redirect,
}

struct Fake {
  steps: Mutex<VecDeque<Step>>,
  /// The answer once the script runs out.
  then: Step,
  /// The real serve-mode sidecar's base URL, for `Forward`.
  inner: Mutex<Option<String>>,
  /// This server's ingest, for `BreakAfterDelivery`.
  ingest: Mutex<String>,
  /// `GET /`: status and body.
  identity: Mutex<(u16, Value)>,
  /// `POST /manifest`: `None` forwards to the real sidecar.
  manifest: Mutex<Option<Value>>,
  requests: AtomicUsize,
  in_flight: AtomicUsize,
  max_in_flight: AtomicUsize,
  http: reqwest::Client,
}

struct InFlight<'a>(&'a Fake);

impl Drop for InFlight<'_> {
  fn drop(&mut self) {
    self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
  }
}

impl Fake {
  fn new(steps: Vec<Step>, then: Step) -> Arc<Self> {
    Arc::new(Fake {
      steps: Mutex::new(steps.into()),
      then,
      inner: Mutex::new(None),
      ingest: Mutex::new(String::new()),
      identity: Mutex::new((
        200,
        json!({"sectr_sidecar": {"version": env!("CARGO_PKG_VERSION"), "mode": "serve"}}),
      )),
      manifest: Mutex::new(None),
      requests: AtomicUsize::new(0),
      in_flight: AtomicUsize::new(0),
      max_in_flight: AtomicUsize::new(0),
      http: reqwest::Client::new(),
    })
  }

  fn requests(&self) -> usize {
    self.requests.load(Ordering::SeqCst)
  }

  fn inner(&self) -> String {
    self
      .inner
      .lock()
      .unwrap()
      .clone()
      .expect("a real sidecar behind the fake")
  }

  async fn forward(&self, path: &str, body: Bytes) -> axum::response::Response {
    let response = self
      .http
      .post(format!("{}/{path}", self.inner()))
      .header("Content-Type", "application/json")
      .body(body)
      .send()
      .await
      .expect("the real sidecar answers");
    let status = axum::http::StatusCode::from_u16(response.status().as_u16()).unwrap();
    (status, response.bytes().await.unwrap()).into_response()
  }
}

async fn fake_wake(State(fake): State<Arc<Fake>>, body: Bytes) -> axum::response::Response {
  fake.requests.fetch_add(1, Ordering::SeqCst);
  let now = fake.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
  fake.max_in_flight.fetch_max(now, Ordering::SeqCst);
  let _guard = InFlight(&fake);
  let step = fake
    .steps
    .lock()
    .unwrap()
    .pop_front()
    .unwrap_or_else(|| fake.then.clone());
  match step {
    Step::Status(code, text) => {
      (axum::http::StatusCode::from_u16(code).unwrap(), text).into_response()
    }
    Step::TokenRejected => axum::Json(json!({"outcome": "token_rejected"})).into_response(),
    Step::Forward => fake.forward("wake", body).await,
    Step::Hang => futures_util::future::pending().await,
    Step::Slow(delay) => {
      tokio::time::sleep(delay).await;
      fake.forward("wake", body).await
    }
    Step::Redirect => {
      axum::response::Redirect::temporary("http://169.254.169.254/").into_response()
    }
    Step::BreakAfterDelivery | Step::StatusAfterDelivery(_) => {
      let token = serde_json::from_slice::<Value>(&body).unwrap()["ingest_token"]
        .as_str()
        .unwrap()
        .to_owned();
      let ingest = fake.ingest.lock().unwrap().clone();
      let mut request = ingest.into_client_request().unwrap();
      request.headers_mut().insert(
        "Authorization",
        tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
          .unwrap(),
      );
      let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
      let _assignment = ws.next().await;
      if let Step::StatusAfterDelivery(code) = step {
        return (
          axum::http::StatusCode::from_u16(code).unwrap(),
          "Internal Server Error",
        )
          .into_response();
      }
      // The instance dies: the connection breaks without an answer.
      panic!("the fake endpoint's instance died mid-turn (expected in this test)");
    }
  }
}

async fn fake_identity(State(fake): State<Arc<Fake>>) -> axum::response::Response {
  let (code, body) = fake.identity.lock().unwrap().clone();
  (
    axum::http::StatusCode::from_u16(code).unwrap(),
    axum::Json(body),
  )
    .into_response()
}

async fn fake_manifest(State(fake): State<Arc<Fake>>, body: Bytes) -> axum::response::Response {
  let fixed = fake.manifest.lock().unwrap().clone();
  match fixed {
    Some(answer) => axum::Json(answer).into_response(),
    None => fake.forward("manifest", body).await,
  }
}

fn fake_router(fake: Arc<Fake>) -> axum::Router {
  axum::Router::new()
    .route("/", axum::routing::get(fake_identity))
    .route("/wake", axum::routing::post(fake_wake))
    .route("/manifest", axum::routing::post(fake_manifest))
    .with_state(fake)
}

async fn serve_on(listener: TcpListener, app: axum::Router) {
  tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
}

// ---------------------------------------------------------------------------
// The endpoint world
// ---------------------------------------------------------------------------

struct EndpointWorld {
  w: World,
  fake: Arc<Fake>,
  runner: Arc<CountingRunner>,
  frames: std::path::PathBuf,
  /// The real serve-mode sidecar behind the fake.
  sidecar: String,
}

fn endpoint_config(wake_deadline: Duration) -> EndpointConfig {
  EndpointConfig {
    ingest_url: None, // world_full fills in this server's own
    allow_private: true,
    wake_deadline,
    turn_timeout: Duration::from_secs(60),
  }
}

struct Options {
  lease_ttl_secs: u64,
  frames: Vec<String>,
  steps: Vec<Step>,
  then: Step,
  wake_deadline: Duration,
  /// The agent waits this long before serving.
  runner_delay_secs: u32,
  /// The endpoint only starts listening after this long.
  bind_after: Option<Duration>,
}

impl Options {
  fn new(steps: Vec<Step>, then: Step) -> Self {
    Options {
      lease_ttl_secs: 30,
      frames: vec![
        sectr_worker::testing::delta_frame("hello from the endpoint"),
        sectr_worker::testing::final_frame(),
      ],
      steps,
      then,
      wake_deadline: Duration::from_secs(10),
      runner_delay_secs: 0,
      bind_after: None,
    }
  }
}

/// A world whose deployment is an endpoint deployment: the fake endpoint,
/// with a real serve-mode sidecar and the python fake agent behind it. The
/// world's session is created (and its first turn dispatched, so woken)
/// before this returns.
async fn endpoint_world(options: Options) -> Option<EndpointWorld> {
  let frames = temp_path("frames.json");
  let pids = temp_path("pids");
  sectr_worker::testing::script_turn(&frames, &options.frames);
  let runner_port = free_port();
  let runner = Arc::new(CountingRunner {
    inner: SubprocessRunner::new(SubprocessRunnerConfig {
      port: runner_port,
      ready_deadline: Duration::from_secs(30),
      ready_poll_interval: Duration::from_millis(25),
    }),
    starts: AtomicUsize::new(0),
    stops: AtomicUsize::new(0),
  });
  let fake = Fake::new(options.steps, options.then);
  let sidecar = Arc::new(Mutex::new(String::new()));

  let make: EndpointMaker = {
    let (fake, runner, sidecar) = (fake.clone(), runner.clone(), sidecar.clone());
    let bind_after = options.bind_after;
    Box::new(move |addr| {
      Box::pin(async move {
        let ingest = format!("ws://{addr}/ingest");
        let inner = start_router(serve_config(ingest.clone(), runner_port), runner).await;
        *fake.inner.lock().unwrap() = Some(inner.clone());
        *fake.ingest.lock().unwrap() = ingest;
        *sidecar.lock().unwrap() = inner;
        let port = free_port();
        let app = fake_router(fake);
        match bind_after {
          None => {
            let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
            serve_on(listener, app).await;
          }
          Some(delay) => {
            tokio::spawn(async move {
              tokio::time::sleep(delay).await;
              let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
              axum::serve(listener, app).await.unwrap();
            });
          }
        }
        format!("http://127.0.0.1:{port}")
      })
    })
  };
  let w = world_full(WorldSetup {
    lease_ttl_secs: options.lease_ttl_secs,
    manifest: fake_runner_manifest(&frames, &pids, options.runner_delay_secs),
    endpoints: Some(endpoint_config(options.wake_deadline)),
    endpoint_url: Some(make),
  })
  .await?;
  let sidecar = sidecar.lock().unwrap().clone();
  Some(EndpointWorld {
    w,
    fake,
    runner,
    frames,
    sidecar,
  })
}

impl EndpointWorld {
  /// Wait until the sidecar has stopped every agent it started, so no
  /// agent process outlives the test.
  async fn wait_stopped(&self) {
    for _ in 0..200 {
      if self.runner.stops.load(Ordering::SeqCst) >= self.runner.starts.load(Ordering::SeqCst) {
        return;
      }
      tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the sidecar never stopped its agent");
  }
}

async fn session_error(w: &World) -> Option<String> {
  w.journal
    .fetch(w.session, None)
    .await
    .unwrap()
    .into_iter()
    .find_map(|e| match e.payload {
      EventPayload::SessionError(error) => Some(error.message),
      _ => None,
    })
}

// ---------------------------------------------------------------------------
// The wake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_wake_is_retried_until_the_endpoint_comes_up() {
  let mut options = Options::new(vec![], Step::Forward);
  options.bind_after = Some(Duration::from_millis(800));
  let Some(e) = endpoint_world(options).await else {
    return;
  };
  wait_for_tail(&e.w, &[EventType::MessageDelta, EventType::NoMoreActions]).await;
  assert_eq!(
    e.fake.requests(),
    1,
    "only the request that connected reached it"
  );
  assert_eq!(session_error(&e.w).await, None);
  wait_lease_gone(&e.w).await;
  e.wait_stopped().await;
}

#[tokio::test]
async fn platform_refusals_and_a_busy_sidecar_are_retried_one_at_a_time() {
  let Some(e) = endpoint_world(Options::new(
    vec![
      Step::Status(429, "no available instance"),
      Step::Status(500, "no available instance"),
      Step::Status(503, "no instance"),
      Step::Status(409, r#"{"error":"busy"}"#),
    ],
    Step::Forward,
  ))
  .await
  else {
    return;
  };
  wait_for_tail(&e.w, &[EventType::MessageDelta, EventType::NoMoreActions]).await;
  assert_eq!(e.fake.requests(), 5);
  assert_eq!(e.fake.max_in_flight.load(Ordering::SeqCst), 1);
  assert_eq!(e.runner.starts.load(Ordering::SeqCst), 1);
  e.wait_stopped().await;
}

#[tokio::test]
async fn other_answers_fail_the_turn_at_once() {
  for (step, needle) in [
    (
      Step::Status(404, "Not Found"),
      "answered HTTP 404: Not Found",
    ),
    (
      Step::Status(502, "boom\nwith\x07controls"),
      "answered HTTP 502: boomwithcontrols",
    ),
    (Step::Redirect, "answered HTTP 307"),
  ] {
    let Some(e) = endpoint_world(Options::new(vec![step.clone()], step.clone())).await else {
      return;
    };
    let message = session_error(&e.w).await.expect("the turn failed");
    assert!(message.starts_with("endpoint wake failed:"), "{message}");
    assert!(message.contains(needle), "{step:?}: {message}");
    assert_eq!(e.fake.requests(), 1, "{step:?}: not retried");
    assert!(
      e.w
        .registry
        .live_lease(e.w.session)
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(
      journal_types(&e.w).await.last(),
      Some(&EventType::SessionError)
    );
  }
}

#[tokio::test]
async fn a_rejected_token_before_delivery_names_this_servers_ingest_url() {
  let Some(e) = endpoint_world(Options::new(vec![], Step::TokenRejected)).await else {
    return;
  };
  let message = session_error(&e.w).await.expect("the turn failed");
  assert!(message.starts_with("endpoint wake failed:"), "{message}");
  assert!(
    message.contains(&format!("SECTR_INGEST_URL is ws://{}/ingest", e.w.addr)),
    "{message}"
  );
  assert_eq!(e.fake.requests(), 1, "never retried");
}

#[tokio::test]
async fn an_endpoint_that_never_dials_fails_at_the_deadline() {
  let mut options = Options::new(vec![], Step::Hang);
  options.wake_deadline = Duration::from_secs(1);
  let started = std::time::Instant::now();
  let Some(e) = endpoint_world(options).await else {
    return;
  };
  let message = session_error(&e.w).await.expect("the turn failed");
  assert!(
    message.contains("your compute endpoint did not start the turn within 1s"),
    "{message}"
  );
  assert!(started.elapsed() < Duration::from_secs(10));
  assert_eq!(e.fake.requests(), 1);
  // The wake task was aborted: the held request goes away.
  for _ in 0..100 {
    if e.fake.in_flight.load(Ordering::SeqCst) == 0 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
  }
  assert_eq!(
    e.fake.in_flight.load(Ordering::SeqCst),
    0,
    "the wake request was dropped"
  );
}

#[tokio::test]
async fn a_deadline_after_a_retry_names_the_timeout_not_the_old_refusal() {
  // A platform refusal, then a retry that reaches a cold start too slow
  // for the deadline: the turn failed for lack of a start, not a 429.
  let mut options = Options::new(vec![Step::Status(429, "no available instance")], Step::Hang);
  options.wake_deadline = Duration::from_secs(1);
  let Some(e) = endpoint_world(options).await else {
    return;
  };
  let message = session_error(&e.w).await.expect("the turn failed");
  assert!(
    message.contains("your compute endpoint did not start the turn within 1s"),
    "{message}"
  );
  assert!(!message.contains("429"), "{message}");
  assert_eq!(e.fake.requests(), 2);
}

#[tokio::test]
async fn a_slow_endpoint_gets_no_second_request() {
  let Some(e) = endpoint_world(Options::new(
    vec![Step::Slow(Duration::from_millis(1500))],
    Step::Forward,
  ))
  .await
  else {
    return;
  };
  wait_for_tail(&e.w, &[EventType::MessageDelta, EventType::NoMoreActions]).await;
  assert_eq!(e.fake.requests(), 1);
  assert_eq!(e.fake.max_in_flight.load(Ordering::SeqCst), 1);
  e.wait_stopped().await;
}

/// A wake that ends after delivery ends the turn at once, saying why,
/// rather than at the lease's expiry. Never re-sent.
#[tokio::test]
async fn a_wake_that_ends_after_delivery_ends_the_turn() {
  for (step, how) in [
    (Step::BreakAfterDelivery, "its connection closed"),
    (Step::StatusAfterDelivery(502), "it answered HTTP 502"),
  ] {
    let Some(e) = endpoint_world(Options::new(vec![], step.clone())).await else {
      return;
    };
    wait_for_tail(&e.w, &[EventType::SessionError, EventType::NoMoreActions]).await;
    let message = session_error(&e.w).await.expect("the turn failed");
    assert!(
      message.starts_with("your compute endpoint stopped before the turn finished: "),
      "{message}"
    );
    assert!(message.contains(how), "{step:?}: {message}");
    // Not a wake failure: the turn started.
    assert!(!message.starts_with("endpoint wake failed:"), "{message}");
    assert!(
      e.w
        .registry
        .live_lease(e.w.session)
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(e.fake.requests(), 1, "never re-sent after delivery");
    // One end, not two.
    let types = journal_types(&e.w).await;
    assert_eq!(
      types
        .iter()
        .filter(|t| **t == EventType::NoMoreActions)
        .count(),
      1,
      "{types:?}"
    );
  }
}

// ---------------------------------------------------------------------------
// End to end through a real serve-mode sidecar
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_turn_on_an_endpoint_deployment_runs_through_the_real_sidecar() {
  let Some(e) = endpoint_world(Options::new(vec![], Step::Forward)).await else {
    return;
  };
  wait_for_tail(&e.w, &[EventType::MessageDelta, EventType::NoMoreActions]).await;
  // The same shape as a managed turn.
  assert_eq!(
    journal_types(&e.w).await,
    [
      EventType::SessionCreated,
      EventType::InvocationStarted,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ]
  );
  wait_lease_gone(&e.w).await;
  assert_eq!(e.runner.starts.load(Ordering::SeqCst), 1);
  e.wait_stopped().await;
}

#[tokio::test]
async fn an_approval_on_an_endpoint_deployment_is_two_wakes() {
  let mut options = Options::new(vec![], Step::Forward);
  options.frames = vec![
    sectr_worker::testing::delta_frame("may I refund?"),
    sectr_worker::testing::approval_frame(),
  ];
  let Some(e) = endpoint_world(options).await else {
    return;
  };
  wait_for_tail(
    &e.w,
    &[EventType::MessageDelta, EventType::ToolApprovalRequired],
  )
  .await;
  wait_lease_gone(&e.w).await;
  sectr_worker::testing::script_turn(
    &e.frames,
    &[
      sectr_worker::testing::delta_frame("refunding"),
      sectr_worker::testing::final_frame(),
    ],
  );
  let (status, _) = post(
    &e.w.app,
    &format!("/sessions/{}/approval", e.w.session),
    &e.w.platform_key,
    json!({"approved": true}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED);
  wait_for_tail(
    &e.w,
    &[
      EventType::ToolApprovalGranted,
      EventType::InvocationStarted,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ],
  )
  .await;
  assert_eq!(e.fake.requests(), 2);
  e.wait_stopped().await;
}

#[tokio::test]
async fn a_reclaimed_endpoint_turn_releases_the_sidecar() {
  let mut options = Options::new(vec![], Step::Forward);
  options.lease_ttl_secs = 1;
  // The agent takes 3 s to start, so the lease expires mid-turn.
  options.runner_delay_secs = 3;
  let Some(e) = endpoint_world(options).await else {
    return;
  };
  tokio::time::sleep(Duration::from_millis(1200)).await;
  let reconciler = Reconciler::new(
    e.w.registry.clone(),
    e.w.journal.clone(),
    Duration::from_secs(15),
  )
  .with_aborter(Arc::new(crate::ingest::HubAbort(e.w.hub.clone())));
  assert_eq!(reconciler.scan_once().await.unwrap().len(), 1);

  // The sidecar doesn't hang on the dead turn: within a few seconds it
  // takes wakes again (an unknown token is rejected, not refused as busy).
  let http = reqwest::Client::new();
  let mut last = None;
  for _ in 0..100 {
    let response = http
      .post(format!("{}/wake", e.sidecar))
      .json(&json!({"ingest_token": "sit_never_minted"}))
      .send()
      .await
      .unwrap();
    let status = response.status().as_u16();
    if status == 200 {
      last = Some(response.json::<Value>().await.unwrap());
      break;
    }
    assert_eq!(status, 409);
    tokio::time::sleep(Duration::from_millis(100)).await;
  }
  assert_eq!(last, Some(json!({"outcome": "token_rejected"})));
  e.wait_stopped().await;
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// A managed platform that records what reached it.
#[derive(Default)]
struct RecordingPlatform {
  calls: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl crate::platform::RunnerPlatform for RecordingPlatform {
  async fn build(
    &self,
    _req: crate::platform::BuildRequest,
  ) -> Result<crate::platform::BuildOutput, crate::platform::BuildError> {
    self.calls.lock().unwrap().push("build".into());
    Err(crate::platform::BuildError::bare("recorded"))
  }
  fn provisional_build_ref(&self, _deployment_id: uuid::Uuid, _version: i64) -> String {
    "managed-ref".into()
  }
  async fn spawn(
    &self,
    _session: uuid::Uuid,
    _deployment_id: uuid::Uuid,
    _invocation_id: uuid::Uuid,
    _ingest_token: &str,
  ) -> Result<Option<String>, crate::dispatch::DispatchError> {
    self.calls.lock().unwrap().push("spawn".into());
    Ok(Some("managed-handle".into()))
  }
  async fn teardown(&self, _session: uuid::Uuid, handle: Option<&str>) {
    self
      .calls
      .lock()
      .unwrap()
      .push(format!("teardown {}", handle.unwrap_or("-")));
  }
}

#[tokio::test]
async fn the_routing_platform_sends_each_kind_to_its_backend() {
  use crate::platform::RunnerPlatform;
  let Some(store) = test_pg::fresh_store().await else {
    return;
  };
  let registry: Arc<dyn Registry> = Arc::new(store.registry());
  let org = registry.create_org("o", "o").await.unwrap();
  let app = registry
    .create_app(NewApp {
      org_id: org.id,
      name: "a".into(),
      slug: "a".into(),
    })
    .await
    .unwrap();
  let managed_row = registry
    .create_deployment(NewDeployment {
      app_id: app.id,
      version: 1,
      manifest: json!({}),
      endpoint_url: None,
    })
    .await
    .unwrap();
  let endpoint_row = registry
    .create_deployment(NewDeployment {
      app_id: app.id,
      version: 2,
      manifest: json!({}),
      endpoint_url: Some("https://agent.example.com/".into()),
    })
    .await
    .unwrap();
  let managed = Arc::new(RecordingPlatform::default());
  let endpoint = Arc::new(crate::platform::EndpointPlatform::new(endpoint_config(
    Duration::from_secs(5),
  )));
  let routing = crate::platform::RoutingPlatform::new(managed.clone(), endpoint, registry);

  let session = uuid::Uuid::now_v7();
  let invocation = uuid::Uuid::now_v7();
  assert_eq!(
    routing
      .spawn(session, managed_row.id, invocation, "sit_x")
      .await
      .unwrap(),
    Some("managed-handle".into())
  );
  // The endpoint half has no hub attached here: it answers, and the
  // managed platform never sees the spawn.
  assert!(matches!(
    routing
      .spawn(session, endpoint_row.id, invocation, "sit_x")
      .await,
    Err(crate::dispatch::DispatchError::Unavailable(_))
  ));
  routing.teardown(session, Some("managed-handle")).await;
  routing
    .teardown(session, Some(&format!("endpoint:{invocation}")))
    .await;
  routing.teardown(session, None).await;
  assert!(
    routing
      .build(crate::platform::BuildRequest {
        deployment_id: managed_row.id,
        version: 1,
        artifact_uri: "fs:x".into(),
        entry: "agent:app".into(),
      })
      .await
      .is_err()
  );
  assert_eq!(
    routing.provisional_build_ref(managed_row.id, 1),
    "managed-ref"
  );
  assert_eq!(
    *managed.calls.lock().unwrap(),
    ["spawn", "teardown managed-handle", "teardown -", "build"]
  );
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

const SECTR_TOML: &str = "[app]\nname = \"edge-agent\"\n\n[agent]\nentry = \"agent:app\"\n\n[deploy]\napproval_tools = [\"refund\"]\n";

/// A world with endpoint deployments on (its own deployment is managed).
async fn registration_world() -> Option<World> {
  world_full(WorldSetup {
    endpoints: Some(endpoint_config(Duration::from_secs(10))),
    ..WorldSetup::managed(
      30,
      json!({"routes": [{"path": "/assist"}], "runner": {"command": ["python3", "-m", "agent"]}}),
    )
  })
  .await
}

async fn request(
  w: &World,
  method: &str,
  uri: &str,
  body: Option<Value>,
) -> (axum::http::StatusCode, String) {
  let mut builder = axum::http::Request::builder()
    .method(method)
    .uri(uri)
    .header("Authorization", format!("Bearer {}", w.platform_key));
  let body = match body {
    Some(body) => {
      builder = builder.header("Content-Type", "application/json");
      axum::body::Body::from(body.to_string())
    }
    None => axum::body::Body::empty(),
  };
  let response = w
    .app
    .clone()
    .oneshot(builder.body(body).unwrap())
    .await
    .unwrap();
  let status = response.status();
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn register(w: &World, endpoint_url: &str) -> (axum::http::StatusCode, String) {
  request(
    w,
    "POST",
    "/endpoint-deployments",
    Some(json!({"manifest": SECTR_TOML, "endpoint_url": endpoint_url})),
  )
  .await
}

/// Poll the app's deployments until `id` leaves `building`.
async fn settled(w: &World, app_id: &str, id: &str) -> Value {
  for _ in 0..300 {
    let (_, body) = request(w, "GET", &format!("/apps/{app_id}/deployments"), None).await;
    let rows: Vec<Value> = serde_json::from_str(&body).unwrap();
    let row = rows
      .into_iter()
      .find(|r| r["id"] == id)
      .expect("the row is listed");
    if row["status"] != "building" {
      return row;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  panic!("deployment {id} never left building");
}

/// A fake endpoint with no real sidecar behind it (registration tests
/// that script the manifest answer).
async fn start_fake(fake: Arc<Fake>) -> String {
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  serve_on(listener, fake_router(fake)).await;
  format!("http://{addr}")
}

#[tokio::test]
async fn registration_activates_with_the_images_manifest() {
  let Some(python) = super::serve::sdk_python() else {
    eprintln!("skipping: no sdk-python/.venv");
    return;
  };
  let Some(w) = registration_world().await else {
    return;
  };
  // A real serve-mode sidecar with an app to import, behind the fake.
  let (app, _) = super::serve::app_dir(super::serve::AGENT_SRC, false);
  let mut config = serve_config(format!("ws://{}/ingest", w.addr), free_port());
  config.app_dir = app;
  config.python = python;
  let sidecar = start_router(config, Arc::new(sectr_worker::sidecar::ExternalRunner)).await;
  let fake = Fake::new(vec![], Step::Forward);
  *fake.inner.lock().unwrap() = Some(sidecar);
  let url = start_fake(fake).await;

  let (status, body) = register(&w, &url).await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED, "{body}");
  let dto: Value = serde_json::from_str(&body).unwrap();
  assert_eq!(dto["status"], "building");
  assert_eq!(dto["endpoint_url"], format!("{url}/"));
  assert!(dto.get("warnings").is_none(), "same version: {dto}");

  let row = settled(
    &w,
    dto["app_id"].as_str().unwrap(),
    dto["id"].as_str().unwrap(),
  )
  .await;
  assert_eq!(row["status"], "active", "{row}");
  assert_eq!(row["active"], true);
  let id: uuid::Uuid = dto["id"].as_str().unwrap().parse().unwrap();
  let manifest = w.registry.get_deployment(id).await.unwrap().manifest;
  assert_eq!(manifest["name"], "serve-agent");
  assert_eq!(manifest["routes"], json!([{"path": "/chat"}]));
  assert_eq!(manifest["env"], json!(["OPENROUTER_API_KEY"]));
  assert_eq!(manifest["runner"]["image"], Value::Null);
  assert_eq!(manifest["runner"]["workdir"], "/app");
  assert_eq!(
    manifest["runner"]["command"],
    json!(["python3", "-m", "sectr.server", "agent:app"])
  );
  assert_eq!(manifest["approval_tools"], json!(["refund"]));

  // Managed rows say so: endpoint_url is null, not absent.
  let (_, body) = request(
    &w,
    "GET",
    &format!("/apps/{}/deployments", dto["app_id"].as_str().unwrap()),
    None,
  )
  .await;
  assert!(body.contains("\"endpoint_url\""));
}

#[tokio::test]
async fn registration_needs_endpoint_deployments_on() {
  let Some(w) = world(30).await else { return };
  let (status, body) = register(&w, "https://agent.example.com").await;
  assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
  assert!(body.contains("SECTR_ENDPOINT_INGEST_URL"), "{body}");
  let (status, body) = request(&w, "GET", "/endpoint-deployments/config", None).await;
  assert_eq!(status, axum::http::StatusCode::OK);
  assert_eq!(
    serde_json::from_str::<Value>(&body).unwrap(),
    // The test config has no base image; a real one always does.
    json!({"enabled": false, "ingest_url": null, "base_image": ""})
  );

  let Some(w) = registration_world().await else {
    return;
  };
  for bad in [
    json!({"manifest": 1, "endpoint_url": "x"}),
    json!({"manifest": SECTR_TOML}),
    json!({"manifest": "not = [toml", "endpoint_url": "https://agent.example.com"}),
  ] {
    let (status, body) = request(&w, "POST", "/endpoint-deployments", Some(bad.clone())).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{bad}: {body}");
  }
  let (_, body) = request(&w, "GET", "/endpoint-deployments/config", None).await;
  assert_eq!(
    serde_json::from_str::<Value>(&body).unwrap(),
    json!({
      "enabled": true,
      "ingest_url": format!("ws://{}/ingest", w.addr),
      "base_image": "",
    })
  );
}

#[tokio::test]
async fn registration_refuses_an_endpoint_that_isnt_a_serve_mode_sidecar() {
  let Some(w) = registration_world().await else {
    return;
  };
  for (code, identity, needle) in [
    (404, json!({}), "answered HTTP 404"),
    (200, json!({"hello": "world"}), "not as a Sectr sidecar"),
    (
      200,
      json!({"sectr_sidecar": {"version": "1", "mode": "env"}}),
      "not as a Sectr sidecar",
    ),
  ] {
    let fake = Fake::new(vec![], Step::Forward);
    *fake.identity.lock().unwrap() = (code, identity);
    let url = start_fake(fake).await;
    let (status, body) = register(&w, &url).await;
    assert_eq!(
      status,
      axum::http::StatusCode::UNPROCESSABLE_ENTITY,
      "{body}"
    );
    assert!(body.contains(needle), "{body}");
    assert!(body.contains("sectr-worker --serve"), "{body}");
  }
  // Nothing listening.
  let (status, body) = register(&w, &format!("http://127.0.0.1:{}", free_port())).await;
  assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
  assert!(body.contains("could not connect"), "{body}");
  // The probe runs before the deploy start: a refused first registration
  // leaves no new app behind.
  let org_id = w
    .registry
    .get_app(
      w.registry
        .get_deployment(w.deployment_id)
        .await
        .unwrap()
        .app_id,
    )
    .await
    .unwrap()
    .org_id;
  assert!(
    w.registry
      .find_app_by_slug(org_id, "edge-agent")
      .await
      .unwrap()
      .is_none()
  );
}

#[tokio::test]
async fn a_different_sidecar_version_is_a_warning() {
  let Some(w) = registration_world().await else {
    return;
  };
  let fake = Fake::new(vec![], Step::Forward);
  *fake.identity.lock().unwrap() = (
    200,
    json!({"sectr_sidecar": {"version": "0.0.1", "mode": "serve"}}),
  );
  *fake.manifest.lock().unwrap() = Some(json!({"outcome": "token_rejected"}));
  let url = start_fake(fake).await;
  let (status, body) = register(&w, &url).await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED, "{body}");
  let dto: Value = serde_json::from_str(&body).unwrap();
  let warning = dto["warnings"][0].as_str().unwrap();
  assert!(
    warning.contains("your compute endpoint runs sidecar 0.0.1"),
    "{warning}"
  );
  assert!(warning.contains(env!("CARGO_PKG_VERSION")), "{warning}");
}

#[tokio::test]
async fn registration_refuses_the_reserved_secret_like_a_deploy() {
  let Some(w) = registration_world().await else {
    return;
  };
  let fake = Fake::new(vec![], Step::Forward);
  let url = start_fake(fake).await;
  // Create the app with the secret first (registration creates it on
  // first use otherwise).
  let app = w
    .registry
    .create_app(NewApp {
      org_id: w
        .registry
        .get_app(
          w.registry
            .get_deployment(w.deployment_id)
            .await
            .unwrap()
            .app_id,
        )
        .await
        .unwrap()
        .org_id,
      name: "edge-agent".into(),
      slug: "edge-agent".into(),
    })
    .await
    .unwrap();
  w.registry
    .set_secret(
      app.id,
      "SECTR_APPROVAL_POLICY",
      sectr_common::secret::Secret::new("[]"),
    )
    .await
    .unwrap();
  let (status, body) = register(&w, &url).await;
  assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
  assert!(
    body.contains("secret named SECTR_APPROVAL_POLICY"),
    "{body}"
  );
}

#[tokio::test]
async fn registration_failures_land_on_the_row() {
  let Some(w) = registration_world().await else {
    return;
  };
  let ingest = format!("ws://{}/ingest", w.addr);
  for (answer, needle) in [
    (
      json!({"outcome": "token_rejected"}),
      format!("doesn't know this registration. Check that its SECTR_INGEST_URL is {ingest}"),
    ),
    (
      json!({"outcome": "dial_failed", "message": "connection refused"}),
      format!(
        "couldn't reach {ingest}: connection refused. Check that your platform allows outbound"
      ),
    ),
    (
      json!({"outcome": "import_failed", "exit_code": 1, "stderr_tail": "ModuleNotFoundError: agent"}),
      "the manifest import failed in your image (exit 1): ModuleNotFoundError: agent".to_owned(),
    ),
    // A lying endpoint: a manifest, but its sidecar never dialed.
    (
      json!({"outcome": "manifest", "manifest": {"routes": []}, "uses_uv": false}),
      "its sidecar never dialed this server".to_owned(),
    ),
  ] {
    let fake = Fake::new(vec![], Step::Forward);
    *fake.manifest.lock().unwrap() = Some(answer.clone());
    let url = start_fake(fake).await;
    let (status, body) = register(&w, &url).await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED, "{body}");
    let dto: Value = serde_json::from_str(&body).unwrap();
    let row = settled(
      &w,
      dto["app_id"].as_str().unwrap(),
      dto["id"].as_str().unwrap(),
    )
    .await;
    assert_eq!(row["status"], "failed", "{answer}: {row}");
    let error = row["error"].as_str().unwrap();
    assert!(error.contains(&needle), "{answer}: {error}");
  }
}

#[tokio::test]
async fn activation_rolls_between_managed_and_endpoint_versions() {
  let Some(w) = registration_world().await else {
    return;
  };
  let deployment = w.registry.get_deployment(w.deployment_id).await.unwrap();
  let endpoint_row = w
    .registry
    .create_deployment(NewDeployment {
      app_id: deployment.app_id,
      version: 2,
      manifest: json!({"routes": [{"path": "/assist"}], "runner": {"command": ["x"]}}),
      endpoint_url: Some("https://agent.example.com/".into()),
    })
    .await
    .unwrap();
  w.registry
    .set_deployment_status(endpoint_row.id, DeploymentStatus::Retired)
    .await
    .unwrap();
  for target in [endpoint_row.id, deployment.id, endpoint_row.id] {
    let (status, body) =
      request(&w, "POST", &format!("/deployments/{target}/activate"), None).await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED, "{body}");
    assert_eq!(
      w.registry
        .active_deployment(deployment.app_id)
        .await
        .unwrap()
        .unwrap()
        .id,
      target
    );
  }
  // The world's session stays pinned to the managed deployment.
  assert_eq!(
    w.registry
      .get_session(w.session)
      .await
      .unwrap()
      .deployment_id,
    deployment.id
  );
}

#[tokio::test]
async fn a_restart_mid_registration_fails_the_row() {
  let Some(w) = registration_world().await else {
    return;
  };
  let deployment = w.registry.get_deployment(w.deployment_id).await.unwrap();
  let row = w
    .registry
    .create_deployment(NewDeployment {
      app_id: deployment.app_id,
      version: 2,
      manifest: json!({"routes": []}),
      endpoint_url: Some("https://agent.example.com/".into()),
    })
    .await
    .unwrap();
  let swept =
    crate::deploy::sweep_stale_building(&w.pool, w.registry.as_ref(), chrono::Duration::zero())
      .await
      .unwrap();
  assert_eq!(swept, 1);
  assert_eq!(
    w.registry.get_deployment(row.id).await.unwrap().status,
    DeploymentStatus::Failed
  );
}
