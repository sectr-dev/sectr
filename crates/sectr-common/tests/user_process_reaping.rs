//! `UserProcess` reaps the killed group when its members are this
//! process's children, which they become when the sidecar is PID 1 in a
//! customer image with no init. Linux only: the test stands in for PID 1
//! by becoming a child subreaper, which inherits orphans the same way.
//! Its own test binary, so the subreaper setting reaches no other test.
#![cfg(target_os = "linux")]

use std::io;
use std::process::Stdio;
use std::time::Duration;

use sectr_common::drivers::UserProcess;
use tokio::process::Command;

/// A wrapper that starts its real work as its own child, the way `uv run`
/// starts python.
fn wrapper() -> Command {
  let mut cmd = Command::new("sh");
  cmd
    .args(["-c", "sleep 59.1251 & wait $!"])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
  cmd
}

fn become_subreaper() {
  // SAFETY: prctl with integer arguments only.
  let set = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
  assert_eq!(set, 0, "PR_SET_CHILD_SUBREAPER");
}

/// Whether any member of the group is still this process's child, alive or
/// a zombie. WNOHANG: 0 = a child is still running, > 0 = a zombie
/// (collected by this call), ECHILD = none.
fn group_has_children(pgid: i32) -> bool {
  let mut status = 0;
  // SAFETY: waitpid with a valid out-pointer to a local.
  let found = unsafe { libc::waitpid(-pgid, &mut status, libc::WNOHANG) };
  !(found == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
}

/// The pids of every process in group `pgid`, zombies included, from
/// `/proc/<pid>/stat` (the group is the 3rd field after the `(comm)`).
fn group_members(pgid: i32) -> Vec<String> {
  let mut members = Vec::new();
  for entry in std::fs::read_dir("/proc").unwrap().flatten() {
    let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
      continue;
    };
    let Some((_, rest)) = stat.rsplit_once(") ") else {
      continue;
    };
    if rest.split_whitespace().nth(2) == Some(pgid.to_string().as_str()) {
      members.push(entry.file_name().to_string_lossy().into_owned());
    }
  }
  members
}

#[tokio::test]
async fn a_killed_or_dropped_group_leaves_no_zombie_for_pid_1() {
  become_subreaper();

  // kill(): reaped by the time it returns.
  let mut process = UserProcess::spawn(&mut wrapper()).unwrap();
  let pgid = process.id().unwrap() as i32;
  tokio::time::sleep(Duration::from_millis(200)).await;
  process.kill().await.unwrap();
  assert!(!group_has_children(pgid), "kill left a zombie");
  drop(process);

  // drop: reaped by a task shortly after.
  let process = UserProcess::spawn(&mut wrapper()).unwrap();
  let pgid = process.id().unwrap() as i32;
  tokio::time::sleep(Duration::from_millis(200)).await;
  drop(process);
  let mut reaped = false;
  for _ in 0..40 {
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Look without collecting: a waitpid here would itself reap a zombie.
    if group_members(pgid).is_empty() {
      reaped = true;
      break;
    }
  }
  assert!(reaped, "drop left a zombie");
}
