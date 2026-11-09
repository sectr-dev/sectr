//! Dev sandbox: the user's code as a plain subprocess in a workspace dir.
//! Inherits the parent environment and overlays `spec.env` on top — right
//! for dev (the runner needs `PATH`, `HOME`, toolchains to run the user's
//! code) and leak-free (secrets arrive via `spec.env`, never via the
//! parent). The future Docker driver passes an explicit env only.
//!
//! The runner's stderr goes through a [`StderrTail`]: forwarded to this
//! process's stderr (the `sectr dev` terminal), or appended to a log file,
//! with its end kept for a startup failure's message.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{RunnerExit, RunnerHandle, RunnerSpec, SandboxDriver, StderrTail, UserProcess};
use crate::drivers::{DriverError, Result};

/// Dev [`SandboxDriver`]: spawn the user's code as a plain subprocess.
pub struct SubprocessDriver {
  /// Where the runner's stderr goes: `None` = this process's stderr.
  stderr_log: Option<PathBuf>,
}

impl SubprocessDriver {
  pub fn new() -> Self {
    SubprocessDriver { stderr_log: None }
  }

  /// Append the runners' stderr to `path` instead of this process's.
  pub fn with_stderr_log(path: impl Into<PathBuf>) -> Self {
    SubprocessDriver {
      stderr_log: Some(path.into()),
    }
  }
}

impl Default for SubprocessDriver {
  fn default() -> Self {
    Self::new()
  }
}

#[async_trait]
impl SandboxDriver for SubprocessDriver {
  async fn spawn(&self, spec: RunnerSpec) -> Result<Box<dyn RunnerHandle>> {
    if spec.command.is_empty() {
      return Err(DriverError::Spawn("empty command".into()));
    }
    let mut cmd = tokio::process::Command::new(&spec.command[0]);
    cmd.args(&spec.command[1..]);
    if let Some(workdir) = &spec.workdir {
      cmd.current_dir(workdir);
    }
    for (key, value) in &spec.env {
      cmd.env(key, value);
    }
    cmd.stderr(std::process::Stdio::piped());
    let forward: Box<dyn tokio::io::AsyncWrite + Unpin + Send> = match &self.stderr_log {
      None => Box::new(tokio::io::stderr()),
      Some(path) => Box::new(
        tokio::fs::OpenOptions::new()
          .create(true)
          .append(true)
          .open(path)
          .await?,
      ),
    };
    // Its own process group, killed whole (and on drop): see UserProcess.
    let mut process = UserProcess::spawn(&mut cmd)
      .map_err(|e| DriverError::Spawn(format!("{}: {e}", spec.command.join(" "))))?;
    let stderr = process.child().stderr.take().expect("stderr is piped");
    let stderr = StderrTail::tee(stderr, forward);
    Ok(Box::new(SubprocessHandle { process, stderr }))
  }
}

struct SubprocessHandle {
  process: UserProcess,
  stderr: StderrTail,
}

#[async_trait]
impl RunnerHandle for SubprocessHandle {
  fn id(&self) -> String {
    self
      .process
      .id()
      .map(|pid| format!("pid-{pid}"))
      .unwrap_or_else(|| "pid-<exited>".into())
  }

  async fn wait(&mut self) -> Result<RunnerExit> {
    Ok(self.process.child().wait().await?.into())
  }

  async fn kill(&mut self) -> Result<()> {
    // The whole group: the python behind `uv run …` too. Only the direct
    // child would leave it orphaned, which is how a surviving runner
    // pinned a stdout pipe open and hung the CLI (2026-09-10).
    self.process.kill().await?;
    Ok(())
  }

  async fn stderr_tail(&mut self) -> Option<String> {
    Some(self.stderr.text().await)
  }
}

impl From<std::process::ExitStatus> for RunnerExit {
  fn from(status: std::process::ExitStatus) -> Self {
    RunnerExit {
      code: status.code(),
      signal: signal_of(status),
    }
  }
}

#[cfg(unix)]
fn signal_of(status: std::process::ExitStatus) -> Option<i32> {
  use std::os::unix::process::ExitStatusExt;
  status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: std::process::ExitStatus) -> Option<i32> {
  None
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;
  use uuid::Uuid;

  fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("sectr-sandbox-test-{name}-{}", Uuid::now_v7()))
  }

  #[tokio::test]
  async fn spawn_wait_exit_code() {
    let driver = SubprocessDriver::new();
    let mut handle = driver
      .spawn(RunnerSpec {
        command: vec!["sh".into(), "-c".into(), "exit 7".into()],
        workdir: None,
        env: vec![],
      })
      .await
      .unwrap();
    assert!(handle.id().starts_with("pid-"));
    let exit = handle.wait().await.unwrap();
    assert_eq!(exit.code, Some(7));
  }

  #[tokio::test]
  async fn stderr_reaches_the_log_and_its_tail_is_kept() {
    let log = temp_path("stderr-log");
    std::fs::write(&log, "earlier output\n").unwrap();
    let driver = SubprocessDriver::with_stderr_log(&log);
    let mut handle = driver
      .spawn(RunnerSpec {
        command: vec![
          "sh".into(),
          "-c".into(),
          "echo out; echo 'ValueError: nope' >&2; exit 1".into(),
        ],
        workdir: None,
        env: vec![],
      })
      .await
      .unwrap();
    let exit = handle.wait().await.unwrap();
    assert_eq!(exit.to_string(), "exit code 1");
    assert_eq!(handle.stderr_tail().await.unwrap(), "ValueError: nope");
    // Appended, after what the log held; stdout isn't the driver's.
    assert_eq!(
      std::fs::read_to_string(&log).unwrap(),
      "earlier output\nValueError: nope\n"
    );
  }

  #[tokio::test]
  async fn env_overlay() {
    let driver = SubprocessDriver::new();
    let out_file = temp_path("env-out");
    let mut handle = driver
      .spawn(RunnerSpec {
        command: vec![
          "sh".into(),
          "-c".into(),
          format!("printf %s \"$SECTR_TEST_VAR\" > {}", out_file.display()),
        ],
        workdir: None,
        env: vec![("SECTR_TEST_VAR".into(), "hello-env".into())],
      })
      .await
      .unwrap();
    let exit = handle.wait().await.unwrap();
    assert_eq!(exit.code, Some(0));
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "hello-env");
    let _ = std::fs::remove_file(&out_file);
  }

  #[tokio::test]
  async fn kill_terminates() {
    let driver = SubprocessDriver::new();
    let mut handle = driver
      .spawn(RunnerSpec {
        command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
        workdir: None,
        env: vec![],
      })
      .await
      .unwrap();
    handle.kill().await.unwrap();
    let exit = handle.wait().await.unwrap();
    assert!(exit.signal.is_some() || exit.code.is_none());
  }

  #[tokio::test]
  async fn kill_tears_down_the_whole_process_group() {
    // Regression (2026-09-10): `uv run python -m sectr.server` survives
    // as an orphaned grandchild when only the direct child is killed.
    let driver = SubprocessDriver::new();
    let mut handle = driver
      .spawn(RunnerSpec {
        // `sh` forks `sleep 1234` as a GRANDCHILD, then waits —
        // killing sh alone must leave sleep running; killing the
        // group must not.
        command: vec!["sh".into(), "-c".into(), "sleep 1234 & wait $!".into()],
        workdir: None,
        env: vec![],
      })
      .await
      .unwrap();
    handle.kill().await.unwrap();
    let _ = handle.wait().await;
    // The distinctive grandchild must be gone (poll: signals are async).
    let mut gone = false;
    for _ in 0..20 {
      let out = std::process::Command::new("ps")
        .arg("-axo")
        .arg("command=")
        .output()
        .unwrap();
      let found = String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim().ends_with("sleep 1234"));
      if !found {
        gone = true;
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(gone, "orphaned grandchild survived the group kill");
  }

  #[tokio::test]
  async fn empty_command_is_spawn_error() {
    let driver = SubprocessDriver::new();
    let err = match driver
      .spawn(RunnerSpec {
        command: vec![],
        workdir: None,
        env: vec![],
      })
      .await
    {
      Err(err) => err,
      Ok(_) => panic!("empty command must not spawn"),
    };
    assert!(matches!(err, DriverError::Spawn(_)));
  }
}
