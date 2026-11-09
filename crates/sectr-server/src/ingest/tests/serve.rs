//! The sidecar's serve mode (`sectr-worker --serve`) against the real
//! ingest hub: each test runs the serve router on a free port with a real
//! `SubprocessRunner` (the assignment's runner command is the python fake
//! runner), takes the turn token the dispatch minted, and POSTs `/wake` or
//! `/manifest` the way the server's endpoint backend will.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::*;

use sectr_common::sidecar::Assignment;
use sectr_worker::runner::{SubprocessRunner, SubprocessRunnerConfig};
use sectr_worker::serve::{ServeConfig, router};
use sectr_worker::sidecar::RunnerSpawner;

/// A `SubprocessRunner` that counts its starts and stops.
struct CountingRunner {
  inner: SubprocessRunner,
  starts: AtomicUsize,
  stops: AtomicUsize,
}

#[async_trait::async_trait]
impl RunnerSpawner for CountingRunner {
  async fn start(&self, assignment: &Assignment) -> Result<(), String> {
    self.starts.fetch_add(1, Ordering::SeqCst);
    self.inner.start(assignment).await
  }
  async fn stop(&self) {
    self.inner.stop().await;
    self.stops.fetch_add(1, Ordering::SeqCst);
  }
}

/// One serve-mode test: the world, the serve router's base URL, the
/// runner, and the fake runner's frame script and pid log.
struct Serve {
  w: World,
  base: String,
  runner: Arc<CountingRunner>,
  frames: std::path::PathBuf,
  pids: std::path::PathBuf,
  http: reqwest::Client,
}

fn temp_path(what: &str) -> std::path::PathBuf {
  std::env::temp_dir().join(format!("sectr-serve-{what}-{}", uuid::Uuid::now_v7()))
}

/// The deployment's runner: the python fake runner, started by a wrapper
/// shell as ITS child, the way `uv run` starts python (no `exec`): killing
/// only the direct child would leave the agent running. The shell logs
/// the agent's pid and, with `delay_secs`, waits before starting it (so a
/// turn is still running while a test sends a second request).
fn fake_runner_manifest(
  frames: &std::path::Path,
  pids: &std::path::Path,
  delay_secs: u32,
) -> Value {
  let script = sectr_worker::testing::fake_runner_command()[1].clone();
  json!({
    "routes": [{"path": "/assist"}],
    "runner": {
      "command": [
        "sh", "-c",
        format!(
          "sleep {delay_secs}; python3 '{script}' & echo $! >> '{}'; wait $!",
          pids.display()
        ),
      ],
      "env": {"SECTR_FRAMES_FILE": frames.to_string_lossy()},
    },
    "approval_tools": ["refund"],
  })
}

fn serve_config(ingest_url: String, runner_port: u16) -> ServeConfig {
  ServeConfig {
    port: 0,
    ingest_url,
    runner_base: format!("http://127.0.0.1:{runner_port}"),
    app_dir: std::env::temp_dir(),
    python: "python3".into(),
    dial_retry_window: Duration::from_secs(10),
    import_timeout: Duration::from_secs(60),
  }
}

/// Bind the serve router on a free port; returns its base URL.
async fn start_router(config: ServeConfig, spawner: Arc<dyn RunnerSpawner>) -> String {
  let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  let app = router(config, spawner);
  tokio::spawn(async move { axum::serve(listener, app).await.expect("serve router") });
  format!("http://{addr}")
}

async fn serve_world(delay_secs: u32, frames: &[String]) -> Option<Serve> {
  serve_world_via(delay_secs, frames, None).await
}

/// [`serve_world`], with the sidecar dialing ingest through `relay` when
/// one is given.
async fn serve_world_via(
  delay_secs: u32,
  frames: &[String],
  relay: Option<&Relay>,
) -> Option<Serve> {
  let frames_file = temp_path("frames.json");
  let pids = temp_path("pids");
  sectr_worker::testing::script_turn(&frames_file, frames);
  let w = world_with_manifest(30, fake_runner_manifest(&frames_file, &pids, delay_secs)).await?;
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
  if let Some(relay) = relay {
    relay.target.set(w.addr).expect("one world per relay");
  }
  let ingest = relay.map_or(w.addr, |r| r.addr);
  let base = start_router(
    serve_config(format!("ws://{ingest}/ingest"), runner_port),
    runner.clone(),
  )
  .await;
  Some(Serve {
    w,
    base,
    runner,
    frames: frames_file,
    pids,
    http: reqwest::Client::new(),
  })
}

/// A TCP relay in front of ingest whose open connections a test can cut,
/// the way a server restart drops them.
struct Relay {
  addr: std::net::SocketAddr,
  target: Arc<std::sync::OnceLock<std::net::SocketAddr>>,
  conns: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl Relay {
  async fn start() -> Relay {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let target = Arc::new(std::sync::OnceLock::<std::net::SocketAddr>::new());
    let conns = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (to, open) = (target.clone(), conns.clone());
    tokio::spawn(async move {
      while let Ok((mut inbound, _)) = listener.accept().await {
        let Some(&target) = to.get() else { continue };
        let relay = tokio::spawn(async move {
          if let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await {
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
          }
        });
        open.lock().unwrap().push(relay.abort_handle());
      }
    });
    Relay {
      addr,
      target,
      conns,
    }
  }

  /// Drop every open connection (new ones still go through).
  fn cut(&self) {
    for conn in self.conns.lock().unwrap().drain(..) {
      conn.abort();
    }
  }
}

/// A test's agents are killed when it ends, pass or fail: a survivor
/// (when the code under test is broken) would hold cargo's pipes open and
/// hang the run.
impl Drop for Serve {
  fn drop(&mut self) {
    for pid in self.pids() {
      let _ = std::process::Command::new("kill")
        .args(["-9", &pid])
        .status();
    }
  }
}

/// Whether the process `pid` has ended (a zombie counts: it no longer
/// runs or holds a port). Polls, since a SIGKILL lands asynchronously.
async fn agent_gone(pid: &str) -> bool {
  for _ in 0..40 {
    let out = std::process::Command::new("ps")
      .args(["-o", "stat=", "-p", pid])
      .output()
      .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    if stat.trim().is_empty() || stat.trim().starts_with('Z') {
      return true;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
  }
  false
}

async fn post_raw(
  http: &reqwest::Client,
  url: String,
  body: impl Into<reqwest::Body>,
) -> (u16, Value) {
  let response = http
    .post(url)
    .header("Content-Type", "application/json")
    .body(body)
    .send()
    .await
    .expect("serve router answers");
  let status = response.status().as_u16();
  let body = response.json::<Value>().await.unwrap_or(Value::Null);
  (status, body)
}

impl Serve {
  async fn wake(&self, body: Value) -> (u16, Value) {
    post_raw(&self.http, format!("{}/wake", self.base), body.to_string()).await
  }

  fn pids(&self) -> Vec<String> {
    std::fs::read_to_string(&self.pids)
      .unwrap_or_default()
      .lines()
      .map(str::to_owned)
      .collect()
  }
}

#[tokio::test]
async fn the_root_identifies_a_serve_mode_sidecar() {
  let base = start_router(
    serve_config("ws://127.0.0.1:9/ingest".into(), free_port()),
    Arc::new(sectr_worker::sidecar::ExternalRunner),
  )
  .await;
  let response = reqwest::get(format!("{base}/")).await.unwrap();
  assert_eq!(response.status().as_u16(), 200);
  let body: Value = response.json().await.unwrap();
  assert_eq!(
    body,
    json!({"sectr_sidecar": {"version": env!("CARGO_PKG_VERSION"), "mode": "serve"}})
  );
}

#[tokio::test]
async fn a_wake_runs_the_turn_and_answers_when_it_ends() {
  let Some(s) = serve_world(
    0,
    &[
      sectr_worker::testing::delta_frame("checking the order"),
      sectr_worker::testing::delta_frame("done"),
      sectr_worker::testing::final_frame(),
    ],
  )
  .await
  else {
    return;
  };
  let token = ingest_token(&s.w).await;
  let (status, body) = s.wake(json!({"ingest_token": token})).await;
  assert_eq!((status, body), (200, json!({"outcome": "turn_ended"})));

  // Held until the turn ended: the journal is complete as the response
  // arrives, and the runner has been started and stopped.
  assert!(
    journal_types(&s.w).await.ends_with(&[
      EventType::MessageDelta,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ]),
    "journal: {:?}",
    journal_types(&s.w).await
  );
  assert_eq!(s.runner.starts.load(Ordering::SeqCst), 1);
  assert_eq!(s.runner.stops.load(Ordering::SeqCst), 1);
  wait_lease_gone(&s.w).await;
}

#[tokio::test]
async fn two_wakes_in_a_row_run_two_turns_in_fresh_runners() {
  let Some(s) = serve_world(
    0,
    &[
      sectr_worker::testing::delta_frame("first"),
      sectr_worker::testing::final_frame(),
    ],
  )
  .await
  else {
    return;
  };
  let token = ingest_token(&s.w).await;
  let (status, body) = s.wake(json!({"ingest_token": token})).await;
  assert_eq!((status, body), (200, json!({"outcome": "turn_ended"})));
  wait_lease_gone(&s.w).await;
  // The agent itself is gone, not just its wrapper: a survivor would keep
  // the port and serve the next turn.
  let first = s.pids();
  assert_eq!(first.len(), 1, "{first:?}");
  assert!(agent_gone(&first[0]).await, "turn 1's agent survived");

  sectr_worker::testing::script_turn(
    &s.frames,
    &[
      sectr_worker::testing::delta_frame("second"),
      sectr_worker::testing::final_frame(),
    ],
  );
  let (status, _) = post(
    &s.w.app,
    &format!("/sessions/{}/input", s.w.session),
    &s.w.platform_key,
    json!({"input": "and now?"}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED);
  let token2 = ingest_token(&s.w).await;
  // A URL in the body (as an older server might send) is ignored: the
  // sidecar dials only its own configured ingest URL.
  let (status, body) = s
    .wake(json!({"ingest_token": token2, "ingest_url": "ws://elsewhere.invalid/ingest"}))
    .await;
  assert_eq!((status, body), (200, json!({"outcome": "turn_ended"})));
  assert!(
    journal_types(&s.w).await.ends_with(&[
      EventType::InvocationStarted,
      EventType::MessageDelta,
      EventType::NoMoreActions,
    ]),
    "journal: {:?}",
    journal_types(&s.w).await
  );

  let pids = s.pids();
  assert_eq!(pids.len(), 2, "one runner per turn: {pids:?}");
  assert_ne!(pids[0], pids[1], "the second turn ran a fresh runner");
  assert!(agent_gone(&pids[1]).await, "turn 2's agent survived");
  assert_eq!(s.runner.starts.load(Ordering::SeqCst), 2);
  assert_eq!(s.runner.stops.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_wake_during_a_turn_is_refused_and_the_turn_completes() {
  // The runner waits 2 s before serving, so the turn is still running.
  let Some(s) = serve_world(
    2,
    &[
      sectr_worker::testing::delta_frame("slow"),
      sectr_worker::testing::final_frame(),
    ],
  )
  .await
  else {
    return;
  };
  let token = ingest_token(&s.w).await;
  let first = {
    let (http, url, body) = (
      s.http.clone(),
      format!("{}/wake", s.base),
      json!({"ingest_token": token}).to_string(),
    );
    tokio::spawn(async move { post_raw(&http, url, body).await })
  };
  // Wait until the first wake holds the turn (its runner has started).
  for _ in 0..200 {
    if s.runner.starts.load(Ordering::SeqCst) == 1 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
  }
  assert_eq!(s.runner.starts.load(Ordering::SeqCst), 1);

  let started = Instant::now();
  let (status, body) = s.wake(json!({"ingest_token": "sit_another"})).await;
  assert_eq!(status, 409);
  assert_eq!(body["error"], "busy");
  assert!(
    started.elapsed() < Duration::from_secs(1),
    "refused at once"
  );

  // So is a manifest request.
  let (status, body) = post_raw(
    &s.http,
    format!("{}/manifest", s.base),
    json!({"registration_token": "sit_x", "entry": "agent:app"}).to_string(),
  )
  .await;
  assert_eq!(status, 409);
  assert_eq!(body["error"], "busy");

  let (status, body) = first.await.unwrap();
  assert_eq!((status, body), (200, json!({"outcome": "turn_ended"})));
  assert!(
    journal_types(&s.w)
      .await
      .ends_with(&[EventType::MessageDelta, EventType::NoMoreActions,])
  );
}

/// The server dropping a held wake (or the platform cancelling it) doesn't
/// cancel the turn: it runs to its end, the agent is stopped, and only then
/// does the instance take another wake.
#[tokio::test]
async fn a_dropped_wake_request_still_finishes_its_turn() {
  let Some(s) = serve_world(
    1,
    &[
      sectr_worker::testing::delta_frame("still here"),
      sectr_worker::testing::final_frame(),
    ],
  )
  .await
  else {
    return;
  };
  let token = ingest_token(&s.w).await;
  let wake = {
    let (http, url, body) = (
      s.http.clone(),
      format!("{}/wake", s.base),
      json!({"ingest_token": token}).to_string(),
    );
    tokio::spawn(async move { post_raw(&http, url, body).await })
  };
  for _ in 0..200 {
    if s.runner.starts.load(Ordering::SeqCst) == 1 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
  }
  wake.abort();
  wait_for_tail(&s.w, &[EventType::MessageDelta, EventType::NoMoreActions]).await;
  for _ in 0..200 {
    if s.runner.stops.load(Ordering::SeqCst) == 1 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
  }
  assert_eq!(
    s.runner.stops.load(Ordering::SeqCst),
    1,
    "the agent was stopped"
  );
  let (status, body) = s.wake(json!({"ingest_token": "sit_never_minted"})).await;
  assert_eq!((status, body), (200, json!({"outcome": "token_rejected"})));
}

/// The server losing a turn while its wake is held (a restart: the
/// connection drops and the grant is gone): the sidecar's redial gets a
/// 401, but a turn had started, so the wake answers `turn_ended`, never
/// `token_rejected` ("nothing ran"). The agent is stopped and the
/// instance takes the next wake. (A reclaim can end the same way, when the
/// connection closes before the turn task sees its failed append.)
#[tokio::test]
async fn a_turn_lost_mid_wake_answers_turn_ended() {
  let relay = Relay::start().await;
  // The runner waits 5 s before serving: the turn is still starting when
  // the server loses it.
  let Some(s) = serve_world_via(5, &[sectr_worker::testing::final_frame()], Some(&relay)).await
  else {
    return;
  };
  let token = ingest_token(&s.w).await;
  let wake = {
    let (http, url, body) = (
      s.http.clone(),
      format!("{}/wake", s.base),
      json!({"ingest_token": token}).to_string(),
    );
    tokio::spawn(async move { post_raw(&http, url, body).await })
  };
  for _ in 0..200 {
    if s.runner.starts.load(Ordering::SeqCst) == 1 {
      break;
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
  }
  assert_eq!(
    s.runner.starts.load(Ordering::SeqCst),
    1,
    "the turn started"
  );

  // The restart: the grant is gone and the connection drops.
  s.w.hub.teardown_session(s.w.session).await;
  relay.cut();

  // Well before the runner would serve: this is the 401 on the redial.
  let (status, body) = tokio::time::timeout(Duration::from_secs(3), wake)
    .await
    .expect("the wake answers once the redial is refused")
    .unwrap();
  assert_eq!((status, body), (200, json!({"outcome": "turn_ended"})));
  assert_eq!(
    s.runner.stops.load(Ordering::SeqCst),
    1,
    "the agent was stopped"
  );

  // The instance is free again.
  let (status, body) = s.wake(json!({"ingest_token": "sit_never_minted"})).await;
  assert_eq!((status, body), (200, json!({"outcome": "token_rejected"})));
}

#[tokio::test]
async fn malformed_wakes_are_bad_requests() {
  let base = start_router(
    serve_config("ws://127.0.0.1:9/ingest".into(), free_port()),
    Arc::new(sectr_worker::sidecar::ExternalRunner),
  )
  .await;
  let http = reqwest::Client::new();
  for body in [
    json!({"ingest_token": "not-a-sectr-token"}).to_string(),
    json!({"ingest_token": "sit_has\nnewline"}).to_string(),
    json!({"ingest_token": 7}).to_string(),
    json!({}).to_string(),
    json!(["sit_x"]).to_string(),
    "not json".to_owned(),
  ] {
    let (status, response) = post_raw(&http, format!("{base}/wake"), body.clone()).await;
    assert_eq!(status, 400, "{body}: {response}");
    assert_eq!(response["error"], "bad_request", "{body}: {response}");
  }
  for body in [
    json!({"entry": "agent:app"}),
    json!({"registration_token": "sit_x"}),
    json!({"registration_token": "sit_x", "entry": 1}),
    json!({"registration_token": "sit_x", "entry": ""}),
  ] {
    let (status, response) = post_raw(&http, format!("{base}/manifest"), body.to_string()).await;
    assert_eq!(status, 400, "{body}: {response}");
  }
}

#[tokio::test]
async fn a_body_over_4_kib_is_refused() {
  let base = start_router(
    serve_config("ws://127.0.0.1:9/ingest".into(), free_port()),
    Arc::new(sectr_worker::sidecar::ExternalRunner),
  )
  .await;
  let http = reqwest::Client::new();
  let big = json!({"ingest_token": format!("sit_{}", "x".repeat(5000))}).to_string();
  for path in ["wake", "manifest"] {
    let (status, body) = post_raw(&http, format!("{base}/{path}"), big.clone()).await;
    assert_eq!(status, 413, "{path}: {body}");
    assert_eq!(body["error"], "too_large");
  }
}

#[tokio::test]
async fn a_token_the_hub_doesnt_know_is_rejected_quickly() {
  let Some(s) = serve_world(0, &[sectr_worker::testing::final_frame()]).await else {
    return;
  };
  let started = Instant::now();
  let (status, body) = s
    .wake(json!({"ingest_token": "sit_this_token_was_never_minted"}))
    .await;
  assert_eq!((status, body), (200, json!({"outcome": "token_rejected"})));
  assert!(started.elapsed() < Duration::from_secs(2));
  assert_eq!(s.runner.starts.load(Ordering::SeqCst), 0, "nothing ran");
}

#[tokio::test]
async fn an_approval_suspension_ends_the_wake_and_the_resume_is_a_second_wake() {
  let Some(s) = serve_world(
    0,
    &[
      sectr_worker::testing::delta_frame("may I refund?"),
      sectr_worker::testing::approval_frame(),
    ],
  )
  .await
  else {
    return;
  };
  let token = ingest_token(&s.w).await;
  let (status, body) = s.wake(json!({"ingest_token": token})).await;
  assert_eq!((status, body), (200, json!({"outcome": "turn_ended"})));
  assert!(
    journal_types(&s.w)
      .await
      .ends_with(&[EventType::MessageDelta, EventType::ToolApprovalRequired,])
  );
  wait_lease_gone(&s.w).await;

  sectr_worker::testing::script_turn(
    &s.frames,
    &[
      sectr_worker::testing::delta_frame("refunding"),
      sectr_worker::testing::final_frame(),
    ],
  );
  let (status, _) = post(
    &s.w.app,
    &format!("/sessions/{}/approval", s.w.session),
    &s.w.platform_key,
    json!({"approved": true}),
  )
  .await;
  assert_eq!(status, axum::http::StatusCode::ACCEPTED);
  let token2 = ingest_token(&s.w).await;
  let (status, body) = s.wake(json!({"ingest_token": token2})).await;
  assert_eq!((status, body), (200, json!({"outcome": "turn_ended"})));
  assert!(journal_types(&s.w).await.ends_with(&[
    EventType::ToolApprovalGranted,
    EventType::InvocationStarted,
    EventType::MessageDelta,
    EventType::NoMoreActions,
  ]));
}

// ---------------------------------------------------------------------------
// POST /manifest
// ---------------------------------------------------------------------------

const AGENT_SRC: &str = r#"
import os
from sectr import AgentApp

app = AgentApp(name="serve-agent", env=["OPENROUTER_API_KEY"])

@app.agent_route("/chat")
async def chat(ctx):
    yield []
"#;

/// The SDK's interpreter (the repo's `sdk-python/.venv`), or `None` to
/// skip: the import needs `sectr` installed.
fn sdk_python() -> Option<String> {
  // Not canonicalized: the venv's python3 is a symlink to the base
  // interpreter, and only the venv path finds the venv's packages.
  let python = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()?
    .parent()?
    .join("sdk-python/.venv/bin/python3");
  python
    .exists()
    .then(|| python.to_string_lossy().into_owned())
}

/// An app directory holding `agent.py` (and `pyproject.toml` if asked),
/// plus a marker file the module creates when it's imported.
fn app_dir(source: &str, pyproject: bool) -> (std::path::PathBuf, std::path::PathBuf) {
  let dir = temp_path("app");
  std::fs::create_dir_all(&dir).unwrap();
  let marker = dir.join("imported");
  std::fs::write(
    dir.join("agent.py"),
    format!(
      "open({:?}, 'w').close()\n{source}",
      marker.to_string_lossy()
    ),
  )
  .unwrap();
  if pyproject {
    std::fs::write(
      dir.join("pyproject.toml"),
      "[project]\nname = \"serve-agent\"\nversion = \"0\"\nrequires-python = \">=3.10\"\n",
    )
    .unwrap();
  }
  (dir, marker)
}

async fn manifest_router(ingest_url: String, app: &std::path::Path, python: String) -> String {
  let mut config = serve_config(ingest_url, free_port());
  config.app_dir = app.to_owned();
  config.python = python;
  config.dial_retry_window = Duration::from_secs(1);
  start_router(config, Arc::new(sectr_worker::sidecar::ExternalRunner)).await
}

async fn post_manifest(base: &str, token: &str) -> (u16, Value) {
  post_raw(
    &reqwest::Client::new(),
    format!("{base}/manifest"),
    json!({"registration_token": token, "entry": "agent:app"}).to_string(),
  )
  .await
}

#[tokio::test]
async fn the_manifest_is_the_in_image_import_once_the_dial_is_accepted() {
  let Some(python) = sdk_python() else {
    eprintln!("skipping: no sdk-python/.venv");
    return;
  };
  let Some(w) = world(30).await else { return };
  let (app, marker) = app_dir(AGENT_SRC, false);
  let base = manifest_router(format!("ws://{}/ingest", w.addr), &app, python.clone()).await;
  // A turn token is enough for the sidecar's side: the hub accepts the
  // upgrade (registration tokens are the server's own concern).
  let token = ingest_token(&w).await;
  let (status, body) = post_manifest(&base, &token).await;
  assert_eq!(status, 200, "{body}");
  assert_eq!(body["outcome"], "manifest", "{body}");
  assert_eq!(body["uses_uv"], false);
  assert!(marker.exists());

  // The same manifest the SDK's import gives for this app.
  let direct = std::process::Command::new(&python)
    .args(["-m", "sectr.inspect", "agent:app"])
    .current_dir(&app)
    .output()
    .unwrap();
  assert!(direct.status.success());
  let direct: Value = serde_json::from_slice(&direct.stdout).unwrap();
  assert_eq!(body["manifest"], direct);
  assert_eq!(body["manifest"]["name"], "serve-agent");
  assert_eq!(body["manifest"]["routes"], json!([{"path": "/chat"}]));
  assert_eq!(body["manifest"]["env"], json!(["OPENROUTER_API_KEY"]));

  // With a pyproject.toml the import runs under `uv run --no-sync`, like
  // the managed build, and says so.
  if std::process::Command::new("uv")
    .arg("--version")
    .output()
    .is_ok()
  {
    let (app, _) = app_dir(AGENT_SRC, true);
    let base = manifest_router(format!("ws://{}/ingest", w.addr), &app, python).await;
    let (status, body) = post_manifest(&base, &token).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["outcome"], "manifest", "{body}");
    assert_eq!(body["uses_uv"], true);
    assert_eq!(body["manifest"]["name"], "serve-agent");
  } else {
    eprintln!("skipping the uv half: no uv on PATH");
  }
}

#[tokio::test]
async fn an_unknown_registration_token_never_runs_the_import() {
  // The SDK's interpreter, so the import WOULD reach the app (and touch
  // the marker) if the dial check didn't stop it.
  let Some(python) = sdk_python() else {
    eprintln!("skipping: no sdk-python/.venv");
    return;
  };
  let Some(w) = world(30).await else { return };
  let (app, marker) = app_dir(AGENT_SRC, false);
  let base = manifest_router(format!("ws://{}/ingest", w.addr), &app, python).await;
  let (status, body) = post_manifest(&base, "sit_never_minted").await;
  assert_eq!((status, body), (200, json!({"outcome": "token_rejected"})));
  assert!(!marker.exists(), "the app was imported");
}

#[tokio::test]
async fn an_unreachable_ingest_is_a_dial_failure_without_an_import() {
  // The SDK's interpreter, as above: the marker proves the dial check.
  let Some(python) = sdk_python() else {
    eprintln!("skipping: no sdk-python/.venv");
    return;
  };
  let (app, marker) = app_dir(AGENT_SRC, false);
  // Nothing listens on this port.
  let closed = format!("ws://127.0.0.1:{}/ingest", free_port());
  let base = manifest_router(closed, &app, python).await;
  let started = Instant::now();
  let (status, body) = post_manifest(&base, "sit_whatever").await;
  assert_eq!(status, 200);
  assert_eq!(body["outcome"], "dial_failed", "{body}");
  assert!(body["message"].as_str().unwrap().contains("dial"), "{body}");
  // Retried for the window (1 s here; 10 s by default), not forever.
  let elapsed = started.elapsed();
  assert!(
    elapsed >= Duration::from_millis(500) && elapsed < Duration::from_secs(5),
    "{elapsed:?}"
  );
  assert!(!marker.exists(), "the app was imported");
}

#[tokio::test]
async fn an_import_that_raises_is_reported_with_its_stderr() {
  let Some(python) = sdk_python() else {
    eprintln!("skipping: no sdk-python/.venv");
    return;
  };
  let Some(w) = world(30).await else { return };
  let (app, _) = app_dir("raise RuntimeError('boom at import')\n", false);
  let base = manifest_router(format!("ws://{}/ingest", w.addr), &app, python).await;
  let token = ingest_token(&w).await;
  let (status, body) = post_manifest(&base, &token).await;
  assert_eq!(status, 200);
  assert_eq!(body["outcome"], "import_failed", "{body}");
  assert_eq!(body["exit_code"], 1, "{body}");
  assert!(
    body["stderr_tail"]
      .as_str()
      .unwrap()
      .contains("boom at import"),
    "{body}"
  );
}
