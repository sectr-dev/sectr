//! [`SecretSource`] — read-only secret resolution for env injection at runner
//! spawn. This is the narrow, deliberate hole in the write-only secret
//! contract: values never leave the source except into a runner's env, at
//! the moment the server role spawns it.
//!
//! The name is deliberate: a *store* is mutable read/write, and this trait
//! is not — it is a provenance of values, nothing more. Mutation belongs to
//! the control plane (Registry's `set_secret`/`delete_secret` + the Phase 5
//! API); in dev the user's editor IS the mutation path (hence `.env`
//! re-read per resolve).
//!
//! Impls: `InMemorySecretSource` (tests) and `EnvFileSecretSource` (dev —
//! reads the user's `.env`). The production impl (Phase 5) lives in
//! sectr-server and reads Postgres directly — an adapter over
//! `PgRegistry::resolve_secrets` (the encrypted `secrets` table, decrypted
//! on read). The resolved pairs then ride to the runner **inside the
//! Contract-4 assignment's `env` field**: the sidecar never implements this
//! trait, it copies assignment env into the user-process env (mandatory for
//! BYOC, where the platform can't inject env at container spawn; also keeps
//! container metadata clean). Write-side CRUD is the server's HTTP API, not
//! this trait. Never user-readable over any API.

mod env_file;
mod in_memory;

pub use env_file::EnvFileSecretSource;
pub use in_memory::InMemorySecretSource;

use async_trait::async_trait;

use super::Result;
use crate::registry::AppId;
use crate::secret::Secret;

#[async_trait]
pub trait SecretSource: Send + Sync {
  /// All of the app's secrets as env pairs. Values are wrapped in
  /// [`Secret`] (redacted `Debug`/`Display`, no serde, explicit reads) so
  /// an accidental log or derive can't leak them; the one deliberate
  /// exposure is building the runner env overlay. Values are never
  /// returned over any API.
  async fn resolve(&self, app: AppId) -> Result<Vec<(String, Secret)>>;
}
