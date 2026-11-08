//! `sectr send` integration tests — against a REAL gateway: the shared
//! sessions-api router + `DevSessionService` over SQLite + the stdlib
//! fake runner (same harness as the worker's dispatch tests, via
//! `sectr_worker::testing`).

use super::*;
use crate::client::SseParser;
use sectr_common::api::UserMessageInput;
use sectr_common::drivers::{InMemorySecretSource, SubprocessDriver};
use sectr_common::events::EventType;
use sectr_common::journal::{JournalReader as _, SqliteJournal};
use sectr_common::registry::{Registry as _, SqliteRegistry};
use sectr_common::store::SqliteStore;
use sectr_worker::dispatch::{DevDispatch, DispatchConfig};
use sectr_worker::service::{AppInfo, DevSessionService};
use sectr_worker::testing::{
  approval_frame, delta_frame, fake_runner_command, final_frame, script_turn,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

struct Gw {
  store: Arc<SqliteStore>,
  app_id: uuid::Uuid,
  journal: Arc<SqliteJournal>,
  dispatch: Arc<DevDispatch>,
  frames: PathBuf,
  project: PathBuf,
}

/// Start a real gateway (engine + sessions-api router) on an OS-assigned
/// port and point a fake project's `.agent/dev.json` at it — exactly what
/// `sectr dev` leaves behind for send to discover.
async fn gateway(frames: &[String]) -> Gw {
  let path = std::env::temp_dir()
    .join(format!("sectr-send-{}.db", uuid::Uuid::now_v7()))
    .to_string_lossy()
    .into_owned();
  let store = Arc::new(SqliteStore::open(&path).await.unwrap());
  let org = store
    .registry()
    .create_org("test-org", "test-org")
    .await
    .unwrap();
  let app = store
    .registry()
    .create_app(sectr_common::registry::NewApp {
      org_id: org.id,
      name: "test-app".into(),
      slug: "test-app".into(),
    })
    .await
    .unwrap();
  let manifest = serde_json::json!({ "routes": [{ "path": "/test" }] });
  let deployment = store
    .registry()
    .create_deployment(sectr_common::registry::NewDeployment {
      endpoint_url: None,
      app_id: app.id,
      version: 1,
      manifest: manifest.clone(),
    })
    .await
    .unwrap();
  let journal = Arc::new(SqliteJournal::new(store.pool().clone()));
  let registry = Arc::new(SqliteRegistry::new(store.pool().clone()));
  let frames_file =
    std::env::temp_dir().join(format!("sectr-send-frames-{}.json", uuid::Uuid::now_v7()));
  script_turn(&frames_file, frames);
  let dispatch = Arc::new(DevDispatch::new(
    journal.clone() as Arc<dyn sectr_common::journal::Journal>,
    registry.clone() as Arc<dyn sectr_common::registry::Registry>,
    Arc::new(sectr_worker::turn::TurnEngine::new()),
    Arc::new(SubprocessDriver::new()),
    Arc::new(InMemorySecretSource::new()),
    DispatchConfig {
      lease_ttl: chrono::Duration::seconds(30),
      ready_deadline: Duration::from_secs(15),
      ready_poll_interval: Duration::from_millis(25),
      runner_command: fake_runner_command(),
      runner_workdir: None,
      runner_env: vec![(
        "SECTR_FRAMES_FILE".into(),
        frames_file.to_string_lossy().into_owned(),
      )],
      runner_port: None,
      approval_tools: vec![],
      env_file: None,
    },
    app.id,
    deployment.id,
  ));
  let service = Arc::new(DevSessionService::new(
    dispatch.clone(),
    AppInfo {
      app_id: app.id,
      app_name: "test-app".into(),
      deployment_id: deployment.id,
      deployment_version: 1,
      manifest,
    },
  ));

  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let port = listener.local_addr().unwrap().port();
  // /dev/status stand-in (the CLI's dev_extras_router serves the same
  // contract shape; this harness keeps send tests focused on the client).
  let status = axum::Router::new().route(
    "/dev/status",
    axum::routing::get(move || {
      let status = sectr_common::api::DevStatus {
        project: "send-test".into(),
        app: "test-app".into(),
        app_id: "0196b8f0-0000-7000-8000-000000000000".into(),
        gateway_port: port,
        framework: Some("openai-agents".into()),
        routes: vec!["/test".into()],
      };
      async move { axum::Json(status) }
    }),
  );
  tokio::spawn(async move {
    axum::serve(
      listener,
      sectr_common::api::sessions_router(service).merge(status),
    )
    .await
  });

  let project = std::env::temp_dir().join(format!("sectr-send-proj-{}", uuid::Uuid::now_v7()));
  std::fs::create_dir_all(project.join(".agent")).unwrap();
  std::fs::write(
    project.join(".agent/dev.json"),
    serde_json::json!({ "gateway_url": format!("http://127.0.0.1:{port}") }).to_string(),
  )
  .unwrap();

  Gw {
    store,
    app_id: app.id,
    journal,
    dispatch,
    frames: frames_file,
    project,
  }
}

fn args(gw: &Gw) -> SendArgs {
  SendArgs {
    path: gw.project.clone(),
    message: None,
    session: None,
    approve: false,
    deny: false,
    no_tail: false,
    json: false,
    cloud: false,
    app: None,
    route: None,
    api: None,
    key: None,
  }
}

/// Poll until the session's journal ends with `last_type`.
async fn wait_for(gw: &Gw, session: uuid::Uuid, last_type: EventType) {
  for _ in 0..400 {
    let types: Vec<EventType> = gw
      .journal
      .fetch(session, None)
      .await
      .unwrap()
      .into_iter()
      .map(|e| e.event_type)
      .collect();
    if types.last() == Some(&last_type) {
      return;
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
  }
  panic!("session {session} never reached {last_type:?}");
}

#[tokio::test]
async fn sends_message_and_streams_turn() {
  let gw = gateway(&[delta_frame("hello "), delta_frame("world"), final_frame()]).await;
  run_send(SendArgs {
    message: Some("hi".into()),
    ..args(&gw)
  })
  .await
  .unwrap();

  // One session; its turn is journaled through to NO_MORE_ACTIONS.
  let sessions = gw
    .store
    .registry()
    .list_sessions(None, Some(gw.app_id), None, 100, None)
    .await
    .unwrap();
  assert_eq!(sessions.len(), 1);
  let sid = sessions[0].id;
  let events = gw.journal.fetch(sid, None).await.unwrap();
  let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
  assert_eq!(*types.last().unwrap(), EventType::NoMoreActions);
  assert!(types.contains(&EventType::MessageDelta));
}

/// The SSE parser: id + data framing, ignoring other fields.
#[test]
fn sse_parser_frames() {
  let mut sse = SseParser::default();
  assert_eq!(sse.feed("retry: 100"), None);
  assert_eq!(sse.feed("id: abc"), None);
  assert_eq!(sse.feed("data: {\"a\":1}"), None);
  assert_eq!(
    sse.feed(""),
    Some((Some("abc".into()), "{\"a\":1}".to_string()))
  );
}

/// Mutually exclusive flags are rejected at the argument parser, not
/// silently swallowed at runtime.
#[test]
fn conflicting_flags_rejected() {
  use clap::Args as _;
  let cmd = clap::Command::new("sectr");
  let mut cmd = SendArgs::augment_args(cmd);
  for bad in [
    vec!["sectr", "--approve", "--deny"],
    vec!["sectr", "--approve", "hi"],
    vec!["sectr", "--deny", "hi"],
    vec!["sectr", "--approve"],
  ] {
    assert!(cmd.try_get_matches_from_mut(bad).is_err());
  }
  // The sane combos still parse.
  for good in [
    vec!["sectr", "hi"],
    vec!["sectr", "--session", "01900000-0000-7000-8000-000000000000"],
    vec![
      "sectr",
      "--approve",
      "--session",
      "01900000-0000-7000-8000-000000000000",
      "--no-tail",
    ],
  ] {
    assert!(cmd.try_get_matches_from_mut(good).is_ok());
  }
}

/// `sectr tail` requires --session in v1 (all-sessions tail is future
/// work) and has no --no-tail flag (that's send-only).
#[test]
fn tail_shape() {
  use clap::Args as _;
  let cmd = clap::Command::new("sectr");
  let mut cmd = crate::tail::TailArgs::augment_args(cmd);
  assert!(cmd.try_get_matches_from_mut(["sectr"]).is_err());
  assert!(
    cmd
      .try_get_matches_from_mut(["sectr", "--session", "01900000-0000-7000-8000-000000000000"])
      .is_ok()
  );
  assert!(
    cmd
      .try_get_matches_from_mut([
        "sectr",
        "--session",
        "01900000-0000-7000-8000-000000000000",
        "--no-tail",
      ])
      .is_err()
  );
}

/// The message prompt: piped stdin is read to EOF (multi-line friendly);
/// a TTY read is one line; empty input is an error.
#[test]
fn read_message_sources() {
  let piped = std::io::Cursor::new("hello\nworld\n".to_string());
  assert_eq!(
    read_message(&mut Box::new(piped), false).unwrap(),
    "hello\nworld"
  );
  let tty_line = std::io::Cursor::new("just one line\nrest ignored".to_string());
  assert_eq!(
    read_message(&mut Box::new(tty_line), true).unwrap(),
    "just one line"
  );
  let empty = std::io::Cursor::new("   \n".to_string());
  assert!(read_message(&mut Box::new(empty), false).is_err());
}

#[tokio::test]
async fn no_gateway_state_fails_fast() {
  let dir = std::env::temp_dir().join(format!("sectr-nogw-{}", uuid::Uuid::now_v7()));
  std::fs::create_dir_all(&dir).unwrap();
  let err = run_send(SendArgs {
    path: dir,
    message: Some("hi".into()),
    session: None,
    approve: false,
    deny: false,
    no_tail: false,
    json: false,
    cloud: false,
    app: None,
    route: None,
    api: None,
    key: None,
  })
  .await
  .unwrap_err();
  assert!(err.to_string().contains("sectr dev"), "{err:#}");
}

#[tokio::test]
async fn unknown_session_404_is_readable() {
  let gw = gateway(&[final_frame()]).await;
  let bogus = uuid::Uuid::now_v7();
  let err = run_send(SendArgs {
    session: Some(bogus),
    message: Some("hi".into()),
    ..args(&gw)
  })
  .await
  .unwrap_err();
  assert!(err.to_string().contains("no such session"), "{err:#}");
}

#[tokio::test]
async fn approval_flow_end_to_end() {
  // The first turn runs the runner with [approval_frame] → suspends.
  let gw = gateway(&[approval_frame()]).await;
  let sid = gw
    .dispatch
    .create_session_and_run_turn(
      "/test",
      UserMessageInput::new(serde_json::json!("refund order 42")).unwrap(),
    )
    .await
    .unwrap()
    .0;
  wait_for(&gw, sid, EventType::ToolApprovalRequired).await;

  // Script the resume, then decide through the CLIENT — the resumed
  // turn must stream and complete.
  script_turn(&gw.frames, &[delta_frame("refunded"), final_frame()]);
  run_send(SendArgs {
    session: Some(sid),
    approve: true,
    ..args(&gw)
  })
  .await
  .unwrap();

  let events = gw.journal.fetch(sid, None).await.unwrap();
  let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
  assert!(types.contains(&EventType::ToolApprovalGranted), "{types:?}");
  assert_eq!(
    *types.last().unwrap(),
    EventType::NoMoreActions,
    "{types:?}"
  );
}

#[tokio::test]
async fn deny_flow_end_to_end() {
  let gw = gateway(&[approval_frame()]).await;
  let sid = gw
    .dispatch
    .create_session_and_run_turn(
      "/test",
      UserMessageInput::new(serde_json::json!("refund order 42")).unwrap(),
    )
    .await
    .unwrap()
    .0;
  wait_for(&gw, sid, EventType::ToolApprovalRequired).await;

  script_turn(&gw.frames, &[final_frame()]);
  run_send(SendArgs {
    session: Some(sid),
    deny: true,
    ..args(&gw)
  })
  .await
  .unwrap();

  let events = gw.journal.fetch(sid, None).await.unwrap();
  let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
  assert!(types.contains(&EventType::ToolApprovalDenied), "{types:?}");
  assert_eq!(
    *types.last().unwrap(),
    EventType::NoMoreActions,
    "{types:?}"
  );
}

/// Follow-up input on an existing session.
#[tokio::test]
async fn followup_turn() {
  let gw = gateway(&[delta_frame("one"), final_frame()]).await;
  let sid = gw
    .dispatch
    .create_session_and_run_turn(
      "/test",
      UserMessageInput::new(serde_json::json!("first")).unwrap(),
    )
    .await
    .unwrap()
    .0;
  wait_for(&gw, sid, EventType::NoMoreActions).await;

  script_turn(&gw.frames, &[delta_frame("two"), final_frame()]);
  run_send(SendArgs {
    session: Some(sid),
    message: Some("second".into()),
    ..args(&gw)
  })
  .await
  .unwrap();

  let events = gw.journal.fetch(sid, None).await.unwrap();
  assert_eq!(events.last().unwrap().event_type, EventType::NoMoreActions);
  // Two invocations total (first turn + the follow-up).
  let invocations = events
    .iter()
    .filter(|e| e.event_type == EventType::InvocationStarted)
    .count();
  assert_eq!(invocations, 2);
}

/// `--no-tail`: the message is sent and send exits immediately — the turn
/// still runs server-side (tailable later).
#[tokio::test]
async fn no_tail_sends_without_following() {
  let gw = gateway(&[delta_frame("async"), final_frame()]).await;
  run_send(SendArgs {
    message: Some("hi".into()),
    no_tail: true,
    ..args(&gw)
  })
  .await
  .unwrap(); // returns long before the turn could finish streaming

  // The turn still completes in the journal.
  let sessions = gw
    .store
    .registry()
    .list_sessions(None, Some(gw.app_id), None, 100, None)
    .await
    .unwrap();
  let sid = sessions[0].id;
  wait_for(&gw, sid, EventType::NoMoreActions).await;
}
