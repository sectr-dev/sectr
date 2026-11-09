//! Infrastructure drivers — the *actions* half of the swappable-infrastructure
//! seam (stores are the *data* half). Each trait has a dev implementation and
//! a production implementation behind the same interface, so the engine and
//! server code paths are identical regardless of environment.
//!
//! Consumers are the **server-role layer**:
//! `sectr-server` in prod, the CLI's dispatch layer in dev. The
//! turn-lifecycle core never touches a driver — like the [`crate::registry`],
//! drivers stay out of the engine's `(assignment, EventSink)` world.
//!
//! - [`sandbox`] — materialize a runner: `SubprocessDriver` in dev, Docker
//!   (later Firecracker) in prod. Readiness is NOT the driver's job: in dev
//!   the engine polls the invoke-contract port; in prod the runner dials the
//!   server and its `ready` WS frame is the signal.
//! - [`secret_source`] — read-only secret resolution for env injection at
//!   runner spawn. The user-facing secret API stays write-only (values never
//!   leave the source except into a runner's env).
//! - [`artifacts`] — deployment tarball blobs. The dev in-memory impl is a
//!   fake; filesystem/object-storage impls land with the deploy flow itself
//!   (Phase 5+). `sectr dev` skips artifacts entirely (runs user code in
//!   place).
//!
//! Layout: one file per implementation — `sandbox/subprocess.rs`,
//! `sandbox/fake.rs`, `secret_source/env_file.rs`, `secret_source/in_memory.rs`,
//! `artifacts/in_memory.rs`.

pub mod artifacts;
pub mod sandbox;
pub mod secret_source;

#[cfg(feature = "gcs")]
pub use artifacts::GcsArtifactStore;
pub use artifacts::{ArtifactStore, FilesystemArtifactStore, InMemoryArtifactStore};
pub use sandbox::{
  FakeSandboxDriver, RunnerExit, RunnerHandle, RunnerSpec, SandboxDriver, StderrTail,
  SubprocessDriver, UserProcess, startup_failure_message,
};
pub use secret_source::{EnvFileSecretSource, InMemorySecretSource, SecretSource};

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
  #[error("runner spawn failed: {0}")]
  Spawn(String),

  #[error("runner failure: {0}")]
  Runner(String),

  #[error("secret file parse error at {path}:{line}: {message}")]
  EnvParse {
    path: String,
    line: usize,
    message: String,
  },

  #[error("artifact not found: {0}")]
  NotFound(String),

  /// Anything that doesn't fit a specific variant (Docker API errors,
  /// connect failures). Messages are already contextual.
  #[error("{0}")]
  Other(String),

  #[error(transparent)]
  Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DriverError>;
