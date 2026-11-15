//! Anonymous usage pings from the CLI, and `sectr telemetry on|off|status`.
//!
//! At most once a day the CLI sends its version and a random install id
//! (`sectr_common::usage_ping::Ping`, the whole body) so installs can be
//! counted by version. The docs page `reference/telemetry` is the public
//! description, and this module must not send anything that page doesn't list.
//!
//! State: `~/.config/sectr/telemetry.toml`, beside the credentials file:
//!
//! ```toml
//! enabled = true
//! install_id = "3f0e9c1a-6b1d-4c57-9a52-0d6f1c2b7e88"
//! last_attempt = "2026-11-14"   # the last day a ping was tried
//! last_ping = "2026-11-14"      # the last day one was accepted
//! ```
//!
//! The rules ([`decide`]):
//! - An off switch in the environment creates nothing: no file, no id, no
//!   notice.
//! - The run that creates the install id prints the notice and sends nothing,
//!   so everyone sees the notice before a ping goes out. The next run pings.
//! - No ping once one was accepted today (UTC). A failed ping is tried again
//!   on the next run.
//! - A file that can't be written means no notice and no ping: a notice that
//!   can't be recorded would repeat on every run.
//!
//! A ping never slows a command by more than [`EXIT_WAIT`], and only on the
//! day's first try. It runs on its own thread with its own runtime, so a slow
//! collector or resolver can't hold up the command or the process's exit.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use sectr_common::usage_ping::{self, EnvSwitch, Ping, Program};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How long the CLI waits at exit for the day's first ping to finish.
pub const EXIT_WAIT: Duration = Duration::from_millis(500);

const FILE_NAME: &str = "telemetry.toml";

fn enabled_by_default() -> bool {
  true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct State {
  #[serde(default = "enabled_by_default")]
  enabled: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  install_id: Option<Uuid>,
  /// The last UTC day a ping was tried, whether or not it got through.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  last_attempt: Option<NaiveDate>,
  /// The last UTC day the collector accepted a ping.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  last_ping: Option<NaiveDate>,
}

impl State {
  fn on() -> Self {
    State {
      enabled: true,
      install_id: Some(usage_ping::new_install_id()),
      last_attempt: None,
      last_ping: None,
    }
  }

  fn off() -> Self {
    State {
      enabled: false,
      install_id: None,
      last_attempt: None,
      last_ping: None,
    }
  }
}

enum Loaded {
  Missing,
  Malformed,
  State(State),
}

fn load(path: &Path) -> Loaded {
  match std::fs::read_to_string(path) {
    Ok(raw) => toml::from_str(&raw).map_or(Loaded::Malformed, Loaded::State),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded::Missing,
    // Unreadable is treated like unparseable: pings off, and say so.
    Err(_) => Loaded::Malformed,
  }
}

/// Write the state file whole: a temp file beside it, then a rename, so a
/// concurrent `sectr` never reads half a file. 0600 like the credentials.
fn store(path: &Path, state: &State) -> anyhow::Result<()> {
  let dir = path.parent().context("the state file has no directory")?;
  std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
  let body = format!(
    "# Sectr's anonymous usage pings. `sectr telemetry status` explains this file.\n# {}\n{}",
    usage_ping::DOCS_URL,
    toml::to_string(state).context("serialize the usage ping state")?
  );
  let tmp = dir.join(format!("{FILE_NAME}.{}.tmp", std::process::id()));
  let written = write_private(&tmp, &body).and_then(|()| std::fs::rename(&tmp, path));
  if written.is_err() {
    let _ = std::fs::remove_file(&tmp);
  }
  written.with_context(|| format!("write {}", path.display()))
}

fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
  use std::io::Write as _;
  let mut options = std::fs::OpenOptions::new();
  options.write(true).create(true).truncate(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  options.open(path)?.write_all(body.as_bytes())
}

/// What one run of the CLI does about pinging.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
  /// Send nothing and say nothing.
  Skip,
  /// Send nothing, and print this line.
  Warn(String),
  /// The install id was just created: print the notice, send nothing.
  Notice,
  /// Send a ping. `wait` is whether the CLI waits for it at exit: only on
  /// the day's first try, so a collector that can't be reached costs a wait
  /// once a day, not on every command.
  Ping { install_id: Uuid, wait: bool },
}

/// Decide, and record the decision in the state file where it needs to be.
fn decide(path: &Path, switch: &EnvSwitch, today: NaiveDate) -> Decision {
  match switch {
    EnvSwitch::Unset => {}
    EnvSwitch::Off(_) => return Decision::Skip,
    EnvSwitch::Invalid(value) => {
      return Decision::Warn(format!(
        "sectr: {}={value:?} is not on or off, so usage pings are off for this run",
        usage_ping::SWITCH_ENV
      ));
    }
  }
  let mut state = match load(path) {
    Loaded::State(state) => state,
    Loaded::Missing => return first_run(path, State::on()),
    Loaded::Malformed => {
      return Decision::Warn(format!(
        "sectr: {} couldn't be read, so usage pings are off. Run `sectr telemetry on` to replace \
         it, or delete it.",
        path.display()
      ));
    }
  };
  if !state.enabled {
    return Decision::Skip;
  }
  let Some(install_id) = state.install_id else {
    // On, but with no id (a hand-edited file): the same as a first run.
    return first_run(path, State::on());
  };
  if state.last_ping == Some(today) {
    return Decision::Skip;
  }
  let wait = state.last_attempt != Some(today);
  if wait {
    state.last_attempt = Some(today);
    if store(path, &state).is_err() {
      return Decision::Skip;
    }
  }
  Decision::Ping { install_id, wait }
}

fn first_run(path: &Path, state: State) -> Decision {
  match store(path, &state) {
    Ok(()) => Decision::Notice,
    Err(_) => Decision::Skip,
  }
}

/// Record that the collector accepted a ping, unless the user turned pings
/// off or reset the id while it was in flight.
fn record_ping(path: &Path, install_id: Uuid, today: NaiveDate) {
  if let Loaded::State(mut state) = load(path)
    && state.enabled
    && state.install_id == Some(install_id)
  {
    state.last_ping = Some(today);
    let _ = store(path, &state);
  }
}

fn notice() -> String {
  format!(
    "Sectr sends an anonymous usage ping at most once a day: the CLI version and a random install \
     id, nothing else. Turn it off with `sectr telemetry off` or `{}=off`. Details: {}",
    usage_ping::SWITCH_ENV,
    usage_ping::DOCS_URL
  )
}

fn state_path() -> anyhow::Result<PathBuf> {
  Ok(crate::auth::config_dir()?.join(FILE_NAME))
}

fn today() -> NaiveDate {
  chrono::Utc::now().date_naive()
}

fn ping_for(install_id: Uuid) -> Ping {
  Ping {
    program: Program::Cli,
    version: env!("CARGO_PKG_VERSION"),
    install_id,
  }
}

/// A ping this run should send.
pub struct PendingPing {
  path: PathBuf,
  url: String,
  install_id: Uuid,
  wait: bool,
}

/// A ping on its way.
pub struct SentPing {
  done: tokio::sync::oneshot::Receiver<()>,
  wait: bool,
}

/// Decide whether this run pings, printing the first-run notice or a warning
/// to stderr when there is one.
pub fn prepare() -> Option<PendingPing> {
  let path = state_path().ok()?;
  let switch = usage_ping::env_switch(usage_ping::process_env);
  match decide(&path, &switch, today()) {
    Decision::Skip => None,
    Decision::Warn(line) => {
      eprintln!("{line}");
      None
    }
    Decision::Notice => {
      eprintln!("{}", notice());
      None
    }
    Decision::Ping { install_id, wait } => Some(PendingPing {
      path,
      url: usage_ping::url(usage_ping::process_env),
      install_id,
      wait,
    }),
  }
}

/// Send the ping in the background, on a thread of its own. The command's
/// runtime never sees it: when the process exits, the thread just ends.
pub fn spawn(pending: PendingPing) -> SentPing {
  let (finished, done) = tokio::sync::oneshot::channel();
  let wait = pending.wait;
  std::thread::spawn(move || {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
      .enable_all()
      .build()
    else {
      return;
    };
    let sent = runtime.block_on(async {
      let client = reqwest::Client::new();
      usage_ping::send(&client, &pending.url, &ping_for(pending.install_id)).await
    });
    if sent.is_ok() {
      record_ping(&pending.path, pending.install_id, today());
    }
    let _ = finished.send(());
  });
  SentPing { done, wait }
}

impl SentPing {
  /// Give the ping a moment to finish before the process exits. Returns at
  /// once when it already has, or when this isn't the day's first try.
  pub async fn finish(self) {
    if self.wait {
      let _ = tokio::time::timeout(EXIT_WAIT, self.done).await;
    }
  }
}

#[derive(Parser, Debug)]
pub struct TelemetryArgs {
  #[command(subcommand)]
  command: TelemetryCommand,
}

#[derive(Subcommand, Debug)]
enum TelemetryCommand {
  /// Turn usage pings on, with a new install id.
  On,
  /// Turn usage pings off and delete the install id.
  Off,
  /// Show whether usage pings are on, and exactly what one sends.
  Status,
}

/// `sectr telemetry`. Never sends a ping itself.
pub fn run_telemetry(args: TelemetryArgs) -> anyhow::Result<()> {
  let path = state_path()?;
  let switch = usage_ping::env_switch(usage_ping::process_env);
  match args.command {
    TelemetryCommand::On => {
      store(&path, &State::on())?;
      println!("Usage pings are on.");
      if let Some(reason) = env_off_reason(&switch) {
        println!("They stay off in this environment: {reason}.");
      }
    }
    TelemetryCommand::Off => {
      store(&path, &State::off())?;
      println!("Usage pings are off.");
    }
    TelemetryCommand::Status => {
      print!(
        "{}",
        status(&path, &switch, &usage_ping::url(usage_ping::process_env))
      );
    }
  }
  Ok(())
}

fn env_off_reason(switch: &EnvSwitch) -> Option<String> {
  match switch {
    EnvSwitch::Unset => None,
    EnvSwitch::Off(var) => Some(format!("{var} is set")),
    EnvSwitch::Invalid(value) => Some(format!(
      "{}={value:?} is not on or off",
      usage_ping::SWITCH_ENV
    )),
  }
}

#[derive(Serialize)]
struct PingPreview<'a> {
  program: Program,
  version: &'a str,
  install_id: &'a str,
}

/// The text of `sectr telemetry status`. Reads the state file and writes
/// nothing.
fn status(path: &Path, switch: &EnvSwitch, url: &str) -> String {
  let loaded = load(path);
  let (on, why) = match (&loaded, env_off_reason(switch)) {
    (_, Some(reason)) => (false, reason),
    (Loaded::Missing, None) => (true, "the default".to_owned()),
    (Loaded::Malformed, None) => (false, format!("{} couldn't be read", path.display())),
    (Loaded::State(state), None) if state.enabled => (true, format!("set in {}", path.display())),
    (Loaded::State(_), None) => (false, format!("turned off in {}", path.display())),
  };
  let install_id = match &loaded {
    Loaded::State(State {
      enabled: true,
      install_id: Some(id),
      ..
    }) => Some(*id),
    _ => None,
  };
  let mut out = format!(
    "Usage pings are {} ({why}).\nCollector: {url}\n",
    if on { "on" } else { "off" }
  );
  let body = match install_id {
    Some(id) => serde_json::to_string_pretty(&ping_for(id)),
    None => serde_json::to_string_pretty(&PingPreview {
      program: Program::Cli,
      version: env!("CARGO_PKG_VERSION"),
      install_id: "<a random id>",
    }),
  }
  .unwrap_or_default();
  out.push_str(match (on, install_id) {
    (true, Some(_)) => "A ping sends exactly this, at most once a day:\n",
    (true, None) => {
      "No install id yet: the next command creates one. A ping sends exactly this, at most once a \
       day:\n"
    }
    (false, _) => "Nothing is sent. With pings on, one would send exactly this:\n",
  });
  out.push_str(&body);
  out.push_str(&format!("\nDetails: {}\n", usage_ping::DOCS_URL));
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
  }

  fn dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    // The `sectr` directory doesn't exist yet, as on a fresh machine.
    let path = dir.path().join("sectr").join(FILE_NAME);
    (dir, path)
  }

  fn state(path: &Path) -> State {
    match load(path) {
      Loaded::State(state) => state,
      Loaded::Missing => panic!("no state file"),
      Loaded::Malformed => panic!("malformed state file"),
    }
  }

  fn expect_ping(decision: Decision) -> (Uuid, bool) {
    match decision {
      Decision::Ping { install_id, wait } => (install_id, wait),
      other => panic!("expected a ping, got {other:?}"),
    }
  }

  #[test]
  fn fresh_install_shows_the_notice_then_pings_on_the_next_run() {
    let (_dir, path) = dir();
    let on = EnvSwitch::Unset;

    // Run 1: the notice, a random id, no ping.
    assert_eq!(decide(&path, &on, day(14)), Decision::Notice);
    let created = state(&path);
    assert!(created.enabled);
    let id = created.install_id.expect("an install id");
    assert_eq!(id.get_version_num(), 4);
    assert_eq!(created.last_ping, None);
    assert_eq!(created.last_attempt, None);

    // Run 2, the same day: pings, and waits for it at exit.
    assert_eq!(expect_ping(decide(&path, &on, day(14))), (id, true));
    record_ping(&path, id, day(14));
    assert_eq!(state(&path).last_ping, Some(day(14)));

    // Run 3, the same day: nothing.
    assert_eq!(decide(&path, &on, day(14)), Decision::Skip);

    // The next day: pings again, with the same id.
    assert_eq!(expect_ping(decide(&path, &on, day(15))), (id, true));
  }

  #[test]
  fn a_failed_ping_is_tried_again_without_waiting() {
    let (_dir, path) = dir();
    let on = EnvSwitch::Unset;
    assert_eq!(decide(&path, &on, day(14)), Decision::Notice);

    // Run 2's ping fails: nothing records a success.
    let (id, wait) = expect_ping(decide(&path, &on, day(14)));
    assert!(wait);
    assert_eq!(state(&path).last_ping, None);

    // Run 3 tries again, but no longer holds up the command's exit.
    assert_eq!(expect_ping(decide(&path, &on, day(14))), (id, false));
    // The next day's first try waits again.
    assert_eq!(expect_ping(decide(&path, &on, day(15))), (id, true));
  }

  #[test]
  fn an_off_switch_in_the_environment_creates_nothing() {
    let (dir, path) = dir();
    for switch in [
      EnvSwitch::Off(usage_ping::SWITCH_ENV),
      EnvSwitch::Off(usage_ping::DO_NOT_TRACK_ENV),
    ] {
      assert_eq!(decide(&path, &switch, day(14)), Decision::Skip);
    }
    let warned = decide(&path, &EnvSwitch::Invalid("maybe".into()), day(14));
    assert!(
      matches!(&warned, Decision::Warn(line) if line.contains("SECTR_TELEMETRY=\"maybe\"")),
      "{warned:?}"
    );
    assert!(!path.exists(), "no state file");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
  }

  #[test]
  fn ci_in_the_environment_changes_nothing() {
    // There is no CI check: the switch reads two variables and no others.
    let switch = usage_ping::env_switch(|name| (name == "CI").then(|| "true".to_owned()));
    assert_eq!(switch, EnvSwitch::Unset);
    let (_dir, path) = dir();
    assert_eq!(decide(&path, &switch, day(14)), Decision::Notice);
    assert!(matches!(
      decide(&path, &switch, day(14)),
      Decision::Ping { .. }
    ));
  }

  #[test]
  fn turned_off_in_the_file_sends_nothing() {
    let (_dir, path) = dir();
    store(&path, &State::off()).unwrap();
    assert_eq!(decide(&path, &EnvSwitch::Unset, day(14)), Decision::Skip);
    assert_eq!(state(&path), State::off());
  }

  #[test]
  fn a_malformed_file_turns_pings_off_with_a_warning() {
    let (_dir, path) = dir();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "enabled = maybe\n").unwrap();
    let decision = decide(&path, &EnvSwitch::Unset, day(14));
    assert!(
      matches!(&decision, Decision::Warn(line)
        if line.contains(&path.display().to_string()) && line.contains("sectr telemetry on")),
      "{decision:?}"
    );
    // The file is left as it was for the user to look at.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "enabled = maybe\n");
  }

  #[test]
  fn on_without_an_id_is_a_first_run() {
    let (_dir, path) = dir();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "enabled = true\n").unwrap();
    assert_eq!(decide(&path, &EnvSwitch::Unset, day(14)), Decision::Notice);
    assert!(state(&path).install_id.is_some());
  }

  #[cfg(unix)]
  #[test]
  fn a_read_only_directory_means_no_notice_and_no_ping() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, path) = dir();
    let read_only = std::fs::Permissions::from_mode(0o555);
    std::fs::set_permissions(dir.path(), read_only).unwrap();
    if std::fs::write(dir.path().join("probe"), "").is_ok() {
      // Running as root: permissions don't bind, so there is nothing to test.
      return;
    }
    assert_eq!(decide(&path, &EnvSwitch::Unset, day(14)), Decision::Skip);
    assert!(!path.exists());

    // An existing file that can't be rewritten: the try can't be recorded,
    // so no ping either.
    let (dir, path) = self::dir();
    store(&path, &State::on()).unwrap();
    std::fs::set_permissions(
      path.parent().unwrap(),
      std::fs::Permissions::from_mode(0o555),
    )
    .unwrap();
    assert_eq!(decide(&path, &EnvSwitch::Unset, day(14)), Decision::Skip);
    std::fs::set_permissions(
      path.parent().unwrap(),
      std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    drop(dir);
  }

  #[cfg(unix)]
  #[test]
  fn the_state_file_is_private_and_leaves_no_temp_file() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, path) = dir();
    store(&path, &State::on()).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
      .unwrap()
      .map(|e| e.unwrap().file_name())
      .collect();
    assert_eq!(names, [FILE_NAME]);
    assert!(
      std::fs::read_to_string(&path)
        .unwrap()
        .starts_with("# Sectr's anonymous usage pings.")
    );
  }

  #[test]
  fn off_then_on_makes_a_different_id() {
    let (_dir, path) = dir();
    assert_eq!(decide(&path, &EnvSwitch::Unset, day(14)), Decision::Notice);
    let first = state(&path).install_id.unwrap();

    store(&path, &State::off()).unwrap();
    let off = std::fs::read_to_string(&path).unwrap();
    assert!(off.contains("enabled = false"));
    assert!(!off.contains("install_id"), "the id is deleted: {off}");

    store(&path, &State::on()).unwrap();
    let second = state(&path).install_id.unwrap();
    assert_ne!(first, second);
    // Turned on by hand: no notice run, the next command pings.
    assert_eq!(
      expect_ping(decide(&path, &EnvSwitch::Unset, day(14))),
      (second, true)
    );
  }

  #[test]
  fn a_ping_in_flight_doesn_t_undo_turning_pings_off() {
    let (_dir, path) = dir();
    store(&path, &State::on()).unwrap();
    let id = state(&path).install_id.unwrap();

    store(&path, &State::off()).unwrap();
    record_ping(&path, id, day(14));
    assert_eq!(state(&path), State::off());

    // Nor does it stamp a new id with the old id's ping.
    store(&path, &State::on()).unwrap();
    record_ping(&path, id, day(14));
    assert_eq!(state(&path).last_ping, None);
  }

  #[test]
  fn status_says_why_and_shows_the_body() {
    let (_dir, path) = dir();
    let url = usage_ping::DEFAULT_URL;

    let fresh = status(&path, &EnvSwitch::Unset, url);
    assert!(
      fresh.starts_with("Usage pings are on (the default).\n"),
      "{fresh}"
    );
    assert!(fresh.contains("No install id yet"), "{fresh}");
    assert!(!path.exists(), "status writes nothing");

    store(&path, &State::on()).unwrap();
    let id = state(&path).install_id.unwrap();
    let on = status(&path, &EnvSwitch::Unset, url);
    assert!(on.contains(&format!("Collector: {url}\n")), "{on}");
    let start = on.find('{').unwrap();
    let end = on.rfind('}').unwrap();
    let body: serde_json::Value = serde_json::from_str(&on[start..=end]).unwrap();
    assert_eq!(
      body,
      serde_json::json!({
        "program": "cli",
        "version": env!("CARGO_PKG_VERSION"),
        "install_id": id.to_string(),
      })
    );

    let env_off = status(&path, &EnvSwitch::Off(usage_ping::DO_NOT_TRACK_ENV), url);
    assert!(
      env_off.starts_with("Usage pings are off (DO_NOT_TRACK is set).\n"),
      "{env_off}"
    );
    assert!(env_off.contains("Nothing is sent."), "{env_off}");

    store(&path, &State::off()).unwrap();
    let file_off = status(&path, &EnvSwitch::Unset, url);
    assert!(
      file_off.starts_with("Usage pings are off (turned off in "),
      "{file_off}"
    );
    assert!(file_off.contains("Nothing is sent."), "{file_off}");
  }

  #[test]
  fn the_notice_names_both_switches_and_the_docs() {
    let notice = notice();
    assert!(notice.contains("`sectr telemetry off`"));
    assert!(notice.contains("`SECTR_TELEMETRY=off`"));
    assert!(notice.ends_with("https://docs.sectr.dev/reference/telemetry/"));
  }
}
