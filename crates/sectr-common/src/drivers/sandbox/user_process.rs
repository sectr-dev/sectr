//! [`UserProcess`]: THE way to run user code as a local process. Every
//! place that starts the developer's code on a machine Sectr's code runs
//! on goes through it: `sectr dev`'s runner ([`super::SubprocessDriver`]),
//! the sidecar's runner (`sectr-worker`'s `SubprocessRunner`, env and
//! serve mode) and the manifest import ([`crate::inspect`]).
//!
//! The rule it enforces: stopping user code stops ALL of it. The command
//! usually wraps the real process (`uv run python …` starts python as its
//! own child, it doesn't `exec`), and killing only the direct child leaves
//! that python running, orphaned. So the process starts as the leader of
//! its own process group, and every kill, including the one when the
//! handle is dropped, is a SIGKILL to the whole group.
//!
//! Why it matters:
//! - `sectr dev` (2026-09-10): a surviving runner held a stdout pipe open
//!   and hung the CLI.
//! - Serve mode: one sidecar process runs many turns. A surviving agent
//!   keeps the agent port, so the next turn's readiness check connects to
//!   it, and that turn runs in the previous turn's process, with its
//!   environment and memory.
//!
//! A side effect: a terminal's Ctrl-C doesn't reach the group. Stopping
//! user code is the caller's job.

use std::io;

use tokio::process::{Child, Command};

/// A running user-code process, leader of its own process group. Killing
/// or dropping it kills the whole group.
pub struct UserProcess {
  child: Child,
  /// The group's id (the leader's pid), kept after the leader is reaped:
  /// its children can outlive it.
  pgid: Option<i32>,
}

impl UserProcess {
  /// Spawn `cmd` as the leader of a new process group.
  pub fn spawn(cmd: &mut Command) -> io::Result<UserProcess> {
    let child = cmd.process_group(0).kill_on_drop(true).spawn()?;
    let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
    Ok(UserProcess { child, pgid })
  }

  /// The direct child's pid, until it's reaped.
  pub fn id(&self) -> Option<u32> {
    self.child.id()
  }

  /// The direct child: its pipes, `wait` and `try_wait`.
  pub fn child(&mut self) -> &mut Child {
    &mut self.child
  }

  /// SIGKILL every process in the group, then reap the direct child. An
  /// already-exited child is fine: its group may still have members.
  pub async fn kill(&mut self) -> io::Result<()> {
    self.kill_group();
    match self.child.kill().await {
      // InvalidInput: the child had already exited and been reaped.
      Err(e) if e.kind() != io::ErrorKind::InvalidInput => Err(e),
      _ => Ok(()),
    }
  }

  fn kill_group(&self) {
    #[cfg(unix)]
    if let Some(pgid) = self.pgid {
      // SAFETY: a plain syscall with no memory arguments. A group with no
      // members left answers ESRCH, which is fine.
      unsafe {
        libc::kill(-pgid, libc::SIGKILL);
      }
    }
  }
}

impl Drop for UserProcess {
  fn drop(&mut self) {
    // kill_on_drop covers the direct child; this covers the rest.
    self.kill_group();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  /// Whether a process whose command line ends with `needle` is running.
  fn running(needle: &str) -> bool {
    let out = std::process::Command::new("ps")
      .args(["-axo", "command="])
      .output()
      .unwrap();
    String::from_utf8_lossy(&out.stdout)
      .lines()
      .any(|line| line.trim().ends_with(needle))
  }

  /// Polls: signals are delivered asynchronously.
  async fn gone(needle: &str) -> bool {
    for _ in 0..40 {
      if !running(needle) {
        return true;
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
  }

  /// `sh` starts `sleep` as its own child and waits, the way `uv run`
  /// starts python: killing sh alone would leave sleep running.
  fn wrapper(seconds: &str) -> Command {
    detached(&format!("sleep {seconds} & wait $!"))
  }

  /// `sh -c script` with no inherited stdio: a survivor (when the code
  /// under test is broken) mustn't hold the test harness's pipes open.
  fn detached(script: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd
      .args(["-c", script])
      .stdin(std::process::Stdio::null())
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null());
    cmd
  }

  #[tokio::test]
  async fn kill_stops_the_whole_group() {
    let mut process = UserProcess::spawn(&mut wrapper("59.1231")).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(running("sleep 59.1231"), "the grandchild started");
    process.kill().await.unwrap();
    assert!(gone("sleep 59.1231").await, "the grandchild survived kill");
  }

  #[tokio::test]
  async fn dropping_stops_the_whole_group() {
    let process = UserProcess::spawn(&mut wrapper("59.1232")).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(running("sleep 59.1232"), "the grandchild started");
    drop(process);
    assert!(
      gone("sleep 59.1232").await,
      "the grandchild survived the drop"
    );
  }

  #[tokio::test]
  async fn kill_after_the_leader_exited_still_stops_its_children() {
    // The leader exits at once and leaves its child behind, in the group.
    let mut process = UserProcess::spawn(&mut detached("sleep 59.1233 &")).unwrap();
    process.child().wait().await.unwrap();
    assert!(running("sleep 59.1233"), "the orphan is running");
    process.kill().await.unwrap();
    assert!(gone("sleep 59.1233").await, "the orphan survived kill");
  }
}
