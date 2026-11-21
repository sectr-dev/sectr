//! Usage pings from the real `sectr` binary to a real HTTP stub: what a user's
//! machine does, end to end. The unit tests in `src/telemetry.rs` cover the
//! decision table. This covers what they can't: that `main` lets a ping
//! through on its way out (even when the command fails), and that a collector
//! that never answers doesn't hold the command up.

#![allow(
  clippy::disallowed_macros,
  reason = "tests build JSON fixtures with `json!`"
)]

use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::Value;

type Seen = Arc<Mutex<Vec<Value>>>;

/// A collector stub: records each ping, then answers 204 after `delay`.
async fn collector(delay: Duration) -> (String, Seen) {
  let seen: Seen = Arc::default();
  let recorder = seen.clone();
  let app = Router::new().route(
    "/v1/ping",
    post(move |axum::Json(body): axum::Json<Value>| {
      let recorder = recorder.clone();
      async move {
        recorder.lock().unwrap().push(body);
        tokio::time::sleep(delay).await;
        StatusCode::NO_CONTENT
      }
    }),
  );
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();
  tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
  (format!("http://{addr}/v1/ping"), seen)
}

struct Machine {
  /// `XDG_CONFIG_HOME`, and an empty project directory to run in.
  home: tempfile::TempDir,
  project: tempfile::TempDir,
  collector: String,
}

impl Machine {
  fn new(collector: &str) -> Self {
    Machine {
      home: tempfile::tempdir().unwrap(),
      project: tempfile::tempdir().unwrap(),
      collector: collector.to_owned(),
    }
  }

  fn state_file(&self) -> std::path::PathBuf {
    self.home.path().join("sectr").join("telemetry.toml")
  }

  /// Run `sectr <args>` with pings on (whatever this test run's own
  /// environment says) and pointed at the stub.
  async fn sectr(&self, args: &[&str], env: &[(&str, &str)]) -> (Output, Duration) {
    let started = Instant::now();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_sectr"))
      .args(args)
      .current_dir(self.project.path())
      .env("XDG_CONFIG_HOME", self.home.path())
      .env("SECTR_TELEMETRY_URL", &self.collector)
      .env_remove("SECTR_TELEMETRY")
      .env_remove("DO_NOT_TRACK")
      .envs(env.iter().copied())
      .output()
      .await
      .expect("run the sectr binary");
    (output, started.elapsed())
  }

  /// A real command that fails fast: no `sectr.toml` in the project.
  async fn command(&self, env: &[(&str, &str)]) -> (String, Duration) {
    let (output, elapsed) = self.sectr(&["secrets", "list"], env).await;
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("sectr secrets: "), "{stderr}");
    (stderr, elapsed)
  }
}

const NOTICE: &str = "Sectr sends anonymous telemetry at most once per day";

fn install_id(path: &Path) -> String {
  let state: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
  state["install_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_fresh_install_pings_once_on_its_second_command() {
  let (url, seen) = collector(Duration::ZERO).await;
  let machine = Machine::new(&url);

  // Run 1: the notice, and no ping.
  let (stderr, _) = machine.command(&[]).await;
  assert!(stderr.contains(NOTICE), "{stderr}");
  assert!(
    seen.lock().unwrap().is_empty(),
    "the notice run sends nothing"
  );
  let id = install_id(&machine.state_file());

  // Run 2: one ping, with this binary's version and the id from the file.
  let (stderr, _) = machine.command(&[]).await;
  assert!(!stderr.contains(NOTICE), "the notice shows once: {stderr}");
  assert_eq!(
    *seen.lock().unwrap(),
    vec![serde_json::json!({
      "program": "cli",
      "version": env!("CARGO_PKG_VERSION"),
      "install_id": id,
    })]
  );

  // Run 3, the same day: nothing more.
  machine.command(&[]).await;
  assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn json_output_never_carries_the_notice() {
  let (url, seen) = collector(Duration::ZERO).await;
  let machine = Machine::new(&url);
  let session = "0190d7a0-0000-7000-8000-000000000000";
  let json_runs: [&[&str]; 2] = [
    &["send", "--json", "hello"],
    &["tail", "--json", "--session", session],
  ];

  // On a fresh machine, `--json` runs print nothing about pings, even on
  // stderr, which a caller may merge into the JSON it parses. They fail
  // fast here (no `sectr dev` running), which is all this needs.
  for args in json_runs {
    let (output, _) = machine.sectr(args, &[]).await;
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains(NOTICE), "{args:?}: {stderr}");
    assert!(output.stdout.is_empty(), "{args:?}");
  }
  // Nor does a bad switch get a warning there.
  let (output, _) = machine
    .sectr(json_runs[0], &[("SECTR_TELEMETRY", "maybe")])
    .await;
  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(!stderr.contains("SECTR_TELEMETRY"), "{stderr}");
  assert!(
    !machine.state_file().exists(),
    "a fresh install is left alone"
  );

  // The first ordinary command shows the notice, as on any fresh machine.
  let (stderr, _) = machine.command(&[]).await;
  assert!(stderr.contains(NOTICE), "{stderr}");
  assert!(seen.lock().unwrap().is_empty());

  // After that, a `--json` run sends the day's ping, and still prints
  // nothing about it.
  let (output, _) = machine.sectr(json_runs[0], &[]).await;
  let stderr = String::from_utf8(output.stderr).unwrap();
  assert!(!stderr.contains(NOTICE), "{stderr}");
  assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_off_switch_sends_nothing_and_creates_no_file() {
  let (url, seen) = collector(Duration::ZERO).await;
  for switch in [("SECTR_TELEMETRY", "off"), ("DO_NOT_TRACK", "1")] {
    let machine = Machine::new(&url);
    for _ in 0..2 {
      let (stderr, _) = machine.command(&[switch]).await;
      assert!(!stderr.contains(NOTICE), "{stderr}");
    }
    assert!(!machine.state_file().exists(), "{switch:?}");
  }
  assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_telemetry_command_and_version_never_ping() {
  let (url, seen) = collector(Duration::ZERO).await;
  let machine = Machine::new(&url);

  let (output, _) = machine.sectr(&["--version"], &[]).await;
  assert!(output.status.success());
  let (output, _) = machine.sectr(&["telemetry", "status"], &[]).await;
  assert!(output.status.success());
  let stdout = String::from_utf8(output.stdout).unwrap();
  assert!(
    stdout.starts_with("Telemetry is on (the default)."),
    "{stdout}"
  );
  assert!(!machine.state_file().exists(), "status writes nothing");

  // `on` makes the id, so the next real command would ping, but `status`
  // and `off` still don't.
  for args in [
    ["telemetry", "on"],
    ["telemetry", "status"],
    ["telemetry", "off"],
  ] {
    let (output, _) = machine.sectr(&args, &[]).await;
    assert!(output.status.success());
    assert!(output.stderr.is_empty(), "no notice from `sectr telemetry`");
  }
  let state = std::fs::read_to_string(machine.state_file()).unwrap();
  assert!(state.contains("enabled = false"), "{state}");
  assert!(!state.contains("install_id"), "{state}");

  // Off in the file: a real command sends nothing either.
  machine.command(&[]).await;
  assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_collector_that_hangs_costs_half_a_second_once() {
  let (url, seen) = collector(Duration::from_secs(5)).await;
  let machine = Machine::new(&url);
  let (_, baseline) = machine.command(&[]).await; // the notice run

  // The day's first try waits for the ping, but only briefly.
  let (_, first) = machine.command(&[]).await;
  assert_eq!(seen.lock().unwrap().len(), 1);
  assert!(
    first < baseline + Duration::from_secs(1),
    "the command took {first:?}, against {baseline:?} without a ping"
  );

  assert!(first >= Duration::from_millis(450), "it waited: {first:?}");

  // It never got an answer, so the next command tries again, this time
  // without waiting: a collector that can't be reached costs the wait once a
  // day, not on every command.
  let (_, again) = machine.command(&[]).await;
  assert!(
    again < first,
    "the second try took {again:?}, the first {first:?}"
  );
  let state = std::fs::read_to_string(machine.state_file()).unwrap();
  assert!(state.contains("last_attempt"), "{state}");
  assert!(!state.contains("last_ping"), "{state}");
}
