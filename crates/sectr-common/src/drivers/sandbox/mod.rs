//! [`SandboxDriver`] — materialize a runner: `SubprocessDriver` in dev, a
//! Docker driver in prod (MVP), Firecracker post-MVP. All implement the same
//! spawn/wait/kill surface, so dispatch code is environment-agnostic.
//!
//! **Readiness is NOT the driver's job.** The driver returns as soon as the
//! process exists. Waiting for the runner to actually serve is the engine's:
//! in dev it polls the invoke-contract port; in prod the runner dials the
//! server over the Contract 4 WebSocket and its `ready` frame is the signal.

mod docker;
mod fake;
mod stderr_tail;
mod subprocess;

pub use docker::{ContainerRun, DockerSandboxDriver};
pub use fake::FakeSandboxDriver;
pub use stderr_tail::{STDERR_TAIL_BYTES, StderrTail, startup_failure_message};
pub use subprocess::SubprocessDriver;

use async_trait::async_trait;

use super::Result;

/// Everything the server-role layer hands the driver to materialize one
/// runner instance.
#[derive(Debug, Clone)]
pub struct RunnerSpec {
  /// argv, resolved by the caller — no shell interpolation anywhere.
  pub command: Vec<String>,
  /// Working directory for the runner process.
  pub workdir: Option<std::path::PathBuf>,
  /// Environment overlay on top of the inherited parent env: platform
  /// vars (`SECTR_PORT`, `SECTR_*`), injected secrets, user vars.
  pub env: Vec<(String, String)>,
}

/// How a runner process ended. `code: None` means terminated by a signal
/// (or otherwise no exit code); `signal` carries the unix signal if known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerExit {
  pub code: Option<i32>,
  pub signal: Option<i32>,
}

/// A live runner, created by [`SandboxDriver::spawn`]. Dropping the handle
/// does NOT stop the runner (kill-on-drop is the impl's choice — the
/// subprocess impl kills on drop as teardown hygiene); call
/// [`RunnerHandle::kill`] for a deliberate stop.
#[async_trait]
pub trait RunnerHandle: Send {
  /// Opaque runner identity for logs/diagnostics (pid, container id, …).
  fn id(&self) -> String;
  /// Wait for the runner to exit.
  async fn wait(&mut self) -> Result<RunnerExit>;
  /// Kill the runner. Idempotence is NOT guaranteed — killing an already
  /// exited runner may error or succeed depending on the impl.
  async fn kill(&mut self) -> Result<()>;
  /// The end of the runner's stderr ([`StderrTail::text`]), for a startup
  /// failure's message. `None` when the driver doesn't keep it.
  async fn stderr_tail(&mut self) -> Option<String> {
    None
  }
}

#[async_trait]
pub trait SandboxDriver: Send + Sync {
  /// Spawn a runner. Returns as soon as the process exists (not when it
  /// is ready to serve — see the module doc).
  async fn spawn(&self, spec: RunnerSpec) -> Result<Box<dyn RunnerHandle>>;
}
