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
//! Killed processes are also reaped. A killed group's members other than
//! the leader (the python under `uv`) are orphans, and the system hands
//! orphans to PID 1. In a customer's image with no init, PID 1 is
//! `sectr-worker` itself, which only ever waited for its own children:
//! each turn left a zombie behind, counting against the process limit. So
//! after a kill, the rest of the group is collected with `waitpid(-pgid)`
//! when it has become ours. Where something else is PID 1 (Lambda's init,
//! a terminal), that answers ECHILD at once and the real init reaps them.
//!
//! A side effect: a terminal's Ctrl-C doesn't reach the group. Stopping
//! user code is the caller's job.

use std::io;
use std::time::Duration;

use tokio::process::{Child, Command};

/// How long [`UserProcess::kill`] waits for each cleanup step before
/// leaving it to the background.
pub const CLEANUP_DEADLINE: Duration = Duration::from_secs(5);

/// A running user-code process, leader of its own process group. Killing
/// or dropping it kills, and reaps, the whole group.
pub struct UserProcess {
  /// `None` only once `Drop` has taken it.
  child: Option<Child>,
  /// The group's id (the leader's pid), kept after the leader is reaped:
  /// its children can outlive it.
  pgid: Option<i32>,
  /// Set once [`UserProcess::kill`] has killed and reaped the group.
  reaped: bool,
}

impl UserProcess {
  /// Spawn `cmd` as the leader of a new process group.
  pub fn spawn(cmd: &mut Command) -> io::Result<UserProcess> {
    let child = cmd.process_group(0).kill_on_drop(true).spawn()?;
    let pgid = child.id().and_then(|pid| i32::try_from(pid).ok());
    Ok(UserProcess {
      child: Some(child),
      pgid,
      reaped: false,
    })
  }

  /// The direct child's pid, until it's reaped.
  pub fn id(&self) -> Option<u32> {
    self.child.as_ref().and_then(Child::id)
  }

  /// The direct child: its pipes, `wait` and `try_wait`.
  pub fn child(&mut self) -> &mut Child {
    self.child.as_mut().expect("only Drop takes the child")
  }

  /// SIGKILL every process in the group, reap the direct child, then reap
  /// the rest of the group where it's ours to reap. An already-exited
  /// child is fine: its group may still have members.
  ///
  /// Waits at most [`CLEANUP_DEADLINE`] for each step. A process stuck in
  /// the kernel (uninterruptible sleep, say on a hung network filesystem)
  /// can't die until it's unstuck, and a caller blocked on it would never
  /// take the next turn. Past the deadline it logs a warning, and the
  /// cleanup finishes in the background, reaping the process whenever it
  /// does die.
  pub async fn kill(&mut self) -> io::Result<()> {
    kill_group(self.pgid);
    let killed = match tokio::time::timeout(CLEANUP_DEADLINE, self.child().kill()).await {
      Err(_) => {
        tracing::warn!(
          pgid = self.pgid,
          "user process didn't exit within {CLEANUP_DEADLINE:?} of SIGKILL; reaping it in the background"
        );
        // Not reaped: Drop's task waits for the leader, then the group.
        return Ok(());
      }
      // InvalidInput: the child had already exited and been reaped.
      Ok(Err(e)) if e.kind() != io::ErrorKind::InvalidInput => Err(e),
      Ok(_) => Ok(()),
    };
    if let Some(reaping) = reap_group(self.pgid)
      && tokio::time::timeout(CLEANUP_DEADLINE, reaping)
        .await
        .is_err()
    {
      // The blocking loop carries on by itself until the group is gone.
      tracing::warn!(
        pgid = self.pgid,
        "a killed user process didn't exit within {CLEANUP_DEADLINE:?}; reaping it in the background"
      );
    }
    self.reaped = true;
    killed
  }
}

impl Drop for UserProcess {
  fn drop(&mut self) {
    // After kill() the group is gone: signalling its id again could only
    // reach a process that reused it.
    if self.reaped {
      return;
    }
    kill_group(self.pgid);
    // Drop can't wait, so a task does: the leader through its own handle
    // first (the group loop must not take it from under tokio), then the
    // rest. With no runtime, kill_on_drop still ends the leader.
    let (Some(mut child), Ok(runtime)) = (self.child.take(), tokio::runtime::Handle::try_current())
    else {
      return;
    };
    let pgid = self.pgid;
    runtime.spawn(async move {
      // No deadline here: nothing waits on this task.
      let _ = child.kill().await;
      if let Some(reaping) = reap_group(pgid) {
        let _ = reaping.await;
      }
    });
  }
}

fn kill_group(pgid: Option<i32>) {
  #[cfg(unix)]
  if let Some(pgid) = pgid {
    // SAFETY: a plain syscall with no memory arguments. A group with no
    // members left answers ESRCH, which is fine.
    unsafe {
      libc::kill(-pgid, libc::SIGKILL);
    }
  }
}

/// Wait for every member of the (already SIGKILLed) group that is this
/// process's child: orphans adopted because this process is PID 1. Only
/// called once the leader is reaped, so it never takes a child tokio
/// tracks; every other user process has a group of its own. It ends:
/// every member was sent SIGKILL, and ECHILD means none is ours. Runs on a
/// blocking thread, which carries on if the returned handle is dropped.
fn reap_group(pgid: Option<i32>) -> Option<tokio::task::JoinHandle<()>> {
  #[cfg(unix)]
  if let Some(pgid) = pgid {
    return Some(tokio::task::spawn_blocking(move || {
      loop {
        // SAFETY: waitpid with a valid out-pointer to a local.
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(-pgid, &mut status, 0) };
        if reaped > 0 {
          continue;
        }
        if reaped == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
          continue;
        }
        break;
      }
    }));
  }
  None
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
