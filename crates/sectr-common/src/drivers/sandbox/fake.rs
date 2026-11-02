//! In-memory [`SandboxDriver`] for engine tests: records every [`RunnerSpec`]
//! and returns handles that stay alive until the test resolves them (via
//! [`FakeSandboxDriver::resolve`]) or kills them. No real processes.

use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;

use super::{RunnerExit, RunnerHandle, RunnerSpec, SandboxDriver};
use crate::drivers::Result;

/// In-memory [`SandboxDriver`] for engine tests.
#[derive(Default)]
pub struct FakeSandboxDriver {
  state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
  specs: Vec<RunnerSpec>,
  handles: Vec<Weak<FakeShared>>,
}

struct FakeShared {
  id: String,
  /// The test resolves the "process" here (or `kill` does).
  exit: Mutex<Option<RunnerExit>>,
  notify: tokio::sync::Notify,
}

impl FakeSandboxDriver {
  pub fn new() -> Self {
    FakeSandboxDriver::default()
  }

  /// Every spec passed to [`SandboxDriver::spawn`], in spawn order.
  pub fn recorded_specs(&self) -> Vec<RunnerSpec> {
    self.state.lock().unwrap().specs.clone()
  }

  /// Make runner `n` (0-based spawn order) exit on its own with `exit`.
  /// Panics if that runner no longer exists (all handles dropped).
  pub fn resolve(&self, n: usize, exit: RunnerExit) {
    let shared = {
      let state = self.state.lock().unwrap();
      state.handles[n]
        .upgrade()
        .expect("runner handle dropped; nothing to resolve")
    };
    *shared.exit.lock().unwrap() = Some(exit);
    shared.notify.notify_waiters();
  }
}

#[async_trait]
impl SandboxDriver for FakeSandboxDriver {
  async fn spawn(&self, spec: RunnerSpec) -> Result<Box<dyn RunnerHandle>> {
    let shared = {
      let mut state = self.state.lock().unwrap();
      state.specs.push(spec);
      let shared = Arc::new(FakeShared {
        id: format!("fake-runner-{}", state.handles.len()),
        exit: Mutex::new(None),
        notify: tokio::sync::Notify::new(),
      });
      state.handles.push(Arc::downgrade(&shared));
      shared
    };
    Ok(Box::new(FakeHandle { shared }))
  }
}

struct FakeHandle {
  shared: Arc<FakeShared>,
}

#[async_trait]
impl RunnerHandle for FakeHandle {
  fn id(&self) -> String {
    self.shared.id.clone()
  }

  async fn wait(&mut self) -> Result<RunnerExit> {
    loop {
      // Register interest BEFORE checking, so a notify fired between
      // the check and the await cannot be missed.
      let notified = self.shared.notify.notified();
      if let Some(exit) = *self.shared.exit.lock().unwrap() {
        return Ok(exit);
      }
      notified.await;
    }
  }

  async fn kill(&mut self) -> Result<()> {
    {
      let mut exit = self.shared.exit.lock().unwrap();
      if exit.is_none() {
        *exit = Some(RunnerExit {
          code: None,
          signal: Some(9),
        });
      }
    }
    self.shared.notify.notify_waiters();
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn records_specs_and_resolves_exits() {
    let driver = FakeSandboxDriver::new();
    let mut h1 = driver
      .spawn(RunnerSpec {
        command: vec!["fake".into(), "one".into()],
        workdir: None,
        env: vec![],
      })
      .await
      .unwrap();
    let mut h2 = driver
      .spawn(RunnerSpec {
        command: vec!["fake".into(), "two".into()],
        workdir: None,
        env: vec![],
      })
      .await
      .unwrap();
    assert_eq!(driver.recorded_specs().len(), 2);
    assert_eq!(driver.recorded_specs()[1].command, vec!["fake", "two"]);
    assert_eq!(h2.id(), "fake-runner-1");

    // wait stays pending until resolved.
    let pending = tokio::time::timeout(std::time::Duration::from_millis(20), h2.wait()).await;
    assert!(pending.is_err(), "unresolved fake runner must stay alive");

    driver.resolve(
      1,
      RunnerExit {
        code: Some(3),
        signal: None,
      },
    );
    assert_eq!(h2.wait().await.unwrap().code, Some(3));

    // Kill resolves the other one with a signal exit.
    h1.kill().await.unwrap();
    assert_eq!(h1.wait().await.unwrap().signal, Some(9));
  }
}
