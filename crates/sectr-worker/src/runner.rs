//! The real in-container runner spawner: starts/stops the user
//! process per assignment — the prod counterpart of the dev dispatch's
//! `SubprocessDriver` + readiness poll.
//!
//! Dev-parity decisions carried over:
//! - **One fresh runner process per invocation** (`start` kills any prior
//!   runner; `stop` kills after the turn) — statelessness is the design.
//!   A kill is a [`UserProcess`] kill: the whole process group, so the
//!   python behind `uv run` goes too. And `start` refuses to run while the
//!   port still answers: in serve mode one sidecar runs many turns, and a
//!   leftover listener would pass the readiness poll and serve the next
//!   turn from the previous turn's process.
//! - **Readiness = TCP poll on the runner's `$SECTR_PORT`** with a
//!   deadline; the deadline is spawn-failure detection, NOT liveness.
//! - Env = `SECTR_PORT` + the assignment's resolved secrets + the
//!   manifest's runner env + `SECTR_APPROVAL_POLICY` (the JSON array of
//!   `assignment.approval_tools`; the assignment carries everything,
//!   nothing is re-read).
//! - **The runner inherits none of the sidecar's `SECTR_*` env.** That env
//!   holds the sidecar's own settings, including its ingest token. The values
//!   above are set explicitly, so they still arrive.
//! - **The runner's stderr** is forwarded to the sidecar's own (the
//!   control-plane log) through a [`StderrTail`], whose end a startup
//!   failure quotes: the sidecar journals it as the turn's `SESSION_ERROR`.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::Mutex;

use sectr_common::drivers::{RunnerExit, StderrTail, UserProcess, startup_failure_message};
use sectr_common::sidecar::{APPROVAL_POLICY_ENV, Assignment, approval_policy_env};

use crate::sidecar::RunnerSpawner;

/// In-container runner config. The port is fixed per container (no port
/// hunting — the container is network-isolated, so a constant is safe);
/// the sidecar's `runner_base` must match it.
#[derive(Debug, Clone)]
pub struct SubprocessRunnerConfig {
  /// `SECTR_PORT` handed to the user process; `runner_base` dials it.
  pub port: u16,
  /// Generous bound for the runner to bind its port (dev parity).
  pub ready_deadline: Duration,
  /// Port-poll cadence.
  pub ready_poll_interval: Duration,
}

impl Default for SubprocessRunnerConfig {
  fn default() -> Self {
    SubprocessRunnerConfig {
      port: 8081,
      ready_deadline: Duration::from_secs(30),
      ready_poll_interval: Duration::from_millis(50),
    }
  }
}

/// How long `start` waits for the previous runner's port to close.
const PORT_FREE_DEADLINE: Duration = Duration::from_secs(5);

/// Spawns the assignment's user process inside the container.
pub struct SubprocessRunner {
  config: SubprocessRunnerConfig,
  child: Mutex<Option<(UserProcess, StderrTail)>>,
}

impl SubprocessRunner {
  pub fn new(config: SubprocessRunnerConfig) -> Self {
    SubprocessRunner {
      config,
      child: Mutex::new(None),
    }
  }

  async fn kill_current(&self) {
    let mut child = self.child.lock().await;
    if let Some((mut process, _)) = child.take() {
      let _ = process.kill().await;
    }
  }

  /// Wait until nothing answers on the runner port, so the readiness poll
  /// can only find the runner `start` is about to spawn. A runner that
  /// escaped its process group (it started a new session) would still
  /// hold the port: that's a startup failure, never a turn served by it.
  async fn wait_port_free(&self) -> Result<(), String> {
    let addr: SocketAddr = ([127, 0, 0, 1], self.config.port).into();
    let deadline = tokio::time::Instant::now() + PORT_FREE_DEADLINE;
    while tokio::net::TcpStream::connect(addr).await.is_ok() {
      if tokio::time::Instant::now() >= deadline {
        return Err(format!(
          "{addr} still answers after the previous runner was stopped: a process outside the \
           runner's process group holds the port, and a new runner can't be started"
        ));
      }
      tokio::time::sleep(self.config.ready_poll_interval).await;
    }
    Ok(())
  }

  /// TCP-poll the runner port until it accepts (dev parity: the
  /// ready-deadline is spawn-failure detection, NOT liveness). The runner
  /// exiting first, or the deadline passing, is a startup failure, whose
  /// message ends with the runner's stderr.
  async fn wait_ready(&self) -> Result<(), String> {
    let addr: SocketAddr = ([127, 0, 0, 1], self.config.port).into();
    let deadline = tokio::time::Instant::now() + self.config.ready_deadline;
    loop {
      if tokio::net::TcpStream::connect(addr).await.is_ok() {
        return Ok(());
      }
      let mut child = self.child.lock().await;
      let Some((process, stderr)) = child.as_mut() else {
        return Err("the runner was stopped during startup".into());
      };
      if let Ok(Some(status)) = process.child().try_wait() {
        let what = format!(
          "the runner exited during startup ({})",
          RunnerExit::from(status)
        );
        return Err(startup_failure_message(&what, &stderr.text().await));
      }
      if tokio::time::Instant::now() >= deadline {
        let what = format!(
          "the runner didn't listen on {addr} within {:?}",
          self.config.ready_deadline
        );
        // Killed first, so its stderr closes and the tail is complete.
        let _ = process.kill().await;
        return Err(startup_failure_message(&what, &stderr.text().await));
      }
      drop(child);
      tokio::time::sleep(self.config.ready_poll_interval).await;
    }
  }
}

#[async_trait::async_trait]
impl RunnerSpawner for SubprocessRunner {
  /// Kill any previous runner, spawn a fresh one with `SECTR_PORT` +
  /// the assignment env, and wait for the port to bind.
  async fn start(&self, assignment: &Assignment) -> Result<(), String> {
    self.kill_current().await;
    self.wait_port_free().await?;
    if assignment.runner.command.is_empty() {
      return Err("assignment has no runner command".into());
    }

    let mut cmd = Command::new(&assignment.runner.command[0]);
    cmd.args(&assignment.runner.command[1..]);
    for key in std::env::vars_os().map(|(key, _)| key) {
      if key.as_encoded_bytes().starts_with(b"SECTR_") {
        cmd.env_remove(key);
      }
    }
    cmd.env("SECTR_PORT", self.config.port.to_string());
    for (k, v) in &assignment.env {
      cmd.env(k, v);
    }
    for (k, v) in &assignment.runner.env {
      cmd.env(k, v);
    }
    // Always set (even when empty) and after the env overlays, so neither an
    // inherited value nor a secret stored before the secrets API refused the
    // name can override the platform policy.
    cmd.env(
      APPROVAL_POLICY_ENV,
      approval_policy_env(&assignment.approval_tools),
    );
    if let Some(workdir) = &assignment.runner.workdir {
      cmd.current_dir(workdir);
    }
    cmd.stderr(std::process::Stdio::piped());
    let mut process = UserProcess::spawn(&mut cmd)
      .map_err(|e| format!("spawn runner {:?}: {e}", assignment.runner.command))?;
    let stderr = process.child().stderr.take().expect("stderr is piped");
    let stderr = StderrTail::tee(stderr, tokio::io::stderr());
    *self.child.lock().await = Some((process, stderr));
    self.wait_ready().await?;
    Ok(())
  }

  async fn stop(&self) {
    self.kill_current().await;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use sectr_common::sidecar::RunnerSpec;
  use std::collections::HashMap;

  fn assignment(command: Vec<String>, env: HashMap<String, String>) -> Assignment {
    Assignment {
      session_id: uuid::Uuid::now_v7(),
      deployment_id: uuid::Uuid::now_v7(),
      invocation_id: uuid::Uuid::now_v7(),
      route: "/chat".into(),
      reason: serde_json::from_value(serde_json::json!({
          "kind": "user_message", "input": "hi"
      }))
      .unwrap(),
      transcript: Vec::new(),
      env,
      runner: RunnerSpec {
        command,
        workdir: None,
        env: HashMap::new(),
      },
      approval_tools: Vec::new(),
    }
  }

  #[tokio::test]
  async fn starts_and_reads_port_env_then_stops() {
    let config = SubprocessRunnerConfig {
      port: 18099,
      ready_deadline: Duration::from_secs(10),
      ready_poll_interval: Duration::from_millis(25),
    };
    let runner = SubprocessRunner::new(config);
    let assignment = assignment(
            vec![
                "python3".into(),
                "-c".into(),
                // Reads SECTR_PORT from env and binds it (proves the env
                // overlay reaches the process).
                "import os, socket; s=socket.socket(); s.bind(('127.0.0.1', int(os.environ['SECTR_PORT']))); s.listen(1); import time; time.sleep(30)".into(),
            ],
            HashMap::new(),
        );
    runner.start(&assignment).await.expect("start + ready");
    runner.stop().await;
  }

  #[tokio::test]
  async fn missing_command_fails() {
    let runner = SubprocessRunner::new(SubprocessRunnerConfig::default());
    let assignment = assignment(Vec::new(), HashMap::new());
    assert!(runner.start(&assignment).await.is_err());
  }

  #[tokio::test]
  async fn unbindable_port_times_out() {
    let config = SubprocessRunnerConfig {
      port: 1, // unbindable/unused port
      ready_deadline: Duration::from_millis(300),
      ready_poll_interval: Duration::from_millis(50),
    };
    let runner = SubprocessRunner::new(config);
    let assignment = assignment(vec!["sleep".into(), "30".into()], HashMap::new());
    let err = runner.start(&assignment).await.unwrap_err();
    assert!(err.contains("didn't listen"), "{err}");
    runner.stop().await;
  }

  #[tokio::test]
  async fn a_runner_exiting_at_startup_fails_with_its_stderr() {
    let config = SubprocessRunnerConfig {
      port: 18095,
      ready_deadline: Duration::from_secs(30),
      ready_poll_interval: Duration::from_millis(25),
    };
    let runner = SubprocessRunner::new(config);
    let assignment = assignment(
      vec![
        "python3".into(),
        "-c".into(),
        "import sys; print('Traceback (most recent call last):', file=sys.stderr); \
         print('ValueError: nope', file=sys.stderr); sys.exit(3)"
          .into(),
      ],
      HashMap::new(),
    );
    let started = tokio::time::Instant::now();
    let err = runner.start(&assignment).await.unwrap_err();
    assert_eq!(
      err,
      "the runner exited during startup (exit code 3). The end of its stderr:\n\
       Traceback (most recent call last):\nValueError: nope"
    );
    // Noticed when it exits, not at the ready deadline.
    assert!(started.elapsed() < Duration::from_secs(10));
    runner.stop().await;
  }

  #[tokio::test]
  async fn secrets_reach_the_process_env() {
    let config = SubprocessRunnerConfig {
      port: 18098,
      ready_deadline: Duration::from_secs(10),
      ready_poll_interval: Duration::from_millis(25),
    };
    let runner = SubprocessRunner::new(config);
    let mut env = HashMap::new();
    env.insert("MY_SECRET".to_string(), "s3cr3t".to_string());
    let assignment = assignment(
            vec![
                "python3".into(),
                "-c".into(),
                "import os, socket, sys; \
                 assert os.environ['MY_SECRET'] == 's3cr3t', 'secret missing'; \
                 s=socket.socket(); s.bind(('127.0.0.1', int(os.environ['SECTR_PORT']))); s.listen(1); \
                 import time; time.sleep(30)"
                    .into(),
            ],
            env,
        );
    runner.start(&assignment).await.expect("start with secret");
    runner.stop().await;
  }

  /// Runs in a child copy of this test binary, because a test can't safely
  /// set its own process's env while others run.
  #[tokio::test]
  async fn the_runner_inherits_none_of_the_sidecars_sectr_env() {
    const CHILD: &str = "SECTR_TEST_RUNNER_ENV_CHILD";
    if std::env::var_os(CHILD).is_none() {
      let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
          "--exact",
          "runner::tests::the_runner_inherits_none_of_the_sidecars_sectr_env",
        ])
        .env(CHILD, "1")
        .env("SECTR_INGEST_TOKEN", "sit_secret")
        .env("SECTR_INGEST_URL", "ws://server/ingest")
        .env("SECTR_OTLP_HEADERS", "Authorization=Basic secret")
        .output()
        .unwrap();
      assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
      return;
    }

    let config = SubprocessRunnerConfig {
      port: 18094,
      ready_deadline: Duration::from_secs(10),
      ready_poll_interval: Duration::from_millis(25),
    };
    let runner = SubprocessRunner::new(config);
    let env = HashMap::from([
      ("MY_SECRET".to_string(), "s3cr3t".to_string()),
      ("SECTR_FAKE".to_string(), "1".to_string()),
    ]);
    let assignment = assignment(
      vec![
        "python3".into(),
        "-c".into(),
        "import os, socket, sys, time; \
         leaked = sorted(k for k in os.environ if k.startswith('SECTR_') \
           and k not in ('SECTR_PORT', 'SECTR_APPROVAL_POLICY', 'SECTR_FAKE')); \
         sys.exit(f'leaked={leaked}') if leaked else None; \
         assert os.environ['MY_SECRET'] == 's3cr3t'; \
         assert os.environ['SECTR_FAKE'] == '1'; \
         assert os.environ['SECTR_APPROVAL_POLICY'] == '[]'; \
         assert 'PATH' in os.environ; \
         s=socket.socket(); s.bind(('127.0.0.1', int(os.environ['SECTR_PORT']))); s.listen(1); \
         time.sleep(30)"
          .into(),
      ],
      env,
    );
    runner
      .start(&assignment)
      .await
      .expect("runner env is clean");
    runner.stop().await;
  }

  /// Runner that exits non-zero unless `SECTR_APPROVAL_POLICY` equals
  /// `expected`; otherwise binds `SECTR_PORT` (so `start` only succeeds
  /// if the assertion held).
  fn policy_asserting_command(expected: &str) -> Vec<String> {
    vec![
      "python3".into(),
      "-c".into(),
      format!(
        "import os, socket, sys, time; \
         got = os.environ.get('SECTR_APPROVAL_POLICY'); \
         sys.exit(f'policy={{got!r}}') if got != {expected:?} else None; \
         s=socket.socket(); s.bind(('127.0.0.1', int(os.environ['SECTR_PORT']))); s.listen(1); \
         time.sleep(30)"
      ),
    ]
  }

  fn quick_config(port: u16) -> SubprocessRunnerConfig {
    SubprocessRunnerConfig {
      port,
      ready_deadline: Duration::from_secs(3),
      ready_poll_interval: Duration::from_millis(25),
    }
  }

  #[tokio::test]
  async fn approval_policy_reaches_the_process_env() {
    let runner = SubprocessRunner::new(quick_config(18097));
    let mut a = assignment(
      policy_asserting_command(r#"["refund","delete_account"]"#),
      HashMap::new(),
    );
    a.approval_tools = vec!["refund".into(), "delete_account".into()];
    runner.start(&a).await.expect("runner sees the policy");
    runner.stop().await;
  }

  #[tokio::test]
  async fn empty_approval_policy_is_set_and_overrides_stale_values() {
    let runner = SubprocessRunner::new(quick_config(18096));
    let stale = HashMap::from([(
      "SECTR_APPROVAL_POLICY".to_string(),
      r#"["stale"]"#.to_string(),
    )]);
    let a = assignment(policy_asserting_command("[]"), stale);
    runner
      .start(&a)
      .await
      .expect("runner sees [] despite stale value");
    runner.stop().await;
  }
}
