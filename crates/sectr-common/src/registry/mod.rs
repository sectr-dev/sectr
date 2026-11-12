//! The `Registry` trait — the only access path to the relational data model
//! (all non-event tables of `schema/data-model.sql`), deliberately separate
//! from [`crate::journal::Journal`] so the two can sit on different stacks in
//! future cloud iterations.
//!
//! MVP scope: sessions, deployments (+ manifest/route lookup), secrets,
//! leases. Orgs/users/api_keys have row models here; their queries (auth
//! middleware, control-plane CRUD) land with the server (Phase 5).

pub mod encrypted;
pub mod pg;
pub mod sqlite;

#[cfg(test)]
mod cap_suite;

pub use encrypted::EncryptedRegistry;
pub use pg::{CliLoginDecision, CliLoginPoll, CliLoginRequest, PgRegistry};
pub use sqlite::SqliteRegistry;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::api::Origin;
use crate::secret::Secret;

pub type OrgId = Uuid;
pub type AppId = Uuid;
pub type DeploymentId = Uuid;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
  #[error("{0}")]
  NotFound(String),

  /// Live-lease contention, constraint violations, and other state
  /// conflicts.
  #[error("registry conflict: {0}")]
  Conflict(String),

  /// [`Registry::claim_lease_capped`] refused the claim because
  /// the live-turn count already reached a cap. Nothing was written.
  #[error("{scope} concurrency cap of {limit} live turns reached")]
  CapExceeded { scope: CapScope, limit: u32 },

  #[error("serialization failure: {0}")]
  Serde(#[from] serde_json::Error),

  #[error(transparent)]
  Sql(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, RegistryError>;

/// Which cap a [`RegistryError::CapExceeded`] tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapScope {
  App,
  Platform,
}

impl std::fmt::Display for CapScope {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      CapScope::App => "per-app",
      CapScope::Platform => "platform-wide",
    })
  }
}

/// Concurrency caps enforced atomically at lease claim. A "live
/// turn" is a live lease (`released_at IS NULL AND expires_at > now`);
/// `None` = unlimited. Both counts take only turns of the claim's own kind
/// (`on_endpoint`): a turn in a Sectr Run sandbox never counts against an
/// endpoint cap, nor the reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseCaps {
  /// The app the claiming session belongs to (the per-app count's key).
  pub app_id: Uuid,
  /// Whether the claiming session's deployment runs on a compute endpoint
  /// (its `endpoint_url` is set) rather than in a Sectr Run sandbox.
  pub on_endpoint: bool,
  pub per_app: Option<u32>,
  pub global: Option<u32>,
}

impl LeaseCaps {
  pub fn any(&self) -> bool {
    self.per_app.is_some() || self.global.is_some()
  }

  /// The first cap `(app_live, total_live)` already reached, if any. The
  /// per-app cap is checked first so a tenant at its own limit is told
  /// that, not "platform full".
  pub fn exceeded(&self, app_live: u64, total_live: u64) -> Option<(CapScope, u32)> {
    if let Some(limit) = self.per_app
      && app_live >= u64::from(limit)
    {
      return Some((CapScope::App, limit));
    }
    if let Some(limit) = self.global
      && total_live >= u64::from(limit)
    {
      return Some((CapScope::Platform, limit));
    }
    None
  }
}

// ---------------------------------------------------------------------------
// Row models (mirror schema/data-model.sql; snake_case wire values match the
// DDL CHECK lists)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
  Active,
  AwaitingApproval,
  Ended,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentStatus {
  Building,
  Active,
  Failed,
  Retired,
}

impl DeploymentStatus {
  /// The TEXT column representation — the SAME snake_case vocabulary serde
  /// produces (`#[serde(rename_all = "snake_case")]`), so the column, the
  /// API DTOs, and the DDL CHECK constraint share one alphabet.
  pub fn as_str(&self) -> &'static str {
    match self {
      DeploymentStatus::Building => "building",
      DeploymentStatus::Active => "active",
      DeploymentStatus::Failed => "failed",
      DeploymentStatus::Retired => "retired",
    }
  }

  /// The inverse of [`Self::as_str`]. Unknown strings are caller-visible
  /// errors — the DDL CHECK constraint makes them impossible from the DB
  /// side; this guards everything else that could synthesize a row.
  pub fn from_db(value: &str) -> Option<Self> {
    match value {
      "building" => Some(DeploymentStatus::Building),
      "active" => Some(DeploymentStatus::Active),
      "failed" => Some(DeploymentStatus::Failed),
      "retired" => Some(DeploymentStatus::Retired),
      _ => None,
    }
  }
}

/// sqlx integration for [`DeploymentStatus`]: BOTH backends treat the
/// value as its TEXT representation — deliberately NOT a Postgres enum
/// type, so the dev (SQLite) and prod (Postgres) schemas stay identical
/// (the DDL CHECK constraint is the DB-side guard on both) and a
/// vocabulary addition never becomes an `ALTER TYPE` migration. This is
/// what lets `sqlx::FromRow`/`query_as` decode `DeploymentStatus`
/// directly on either backend.
mod sqlx_text {
  use super::DeploymentStatus;
  use std::error::Error;

  impl DeploymentStatus {
    fn from_db_str(value: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
      Self::from_db(value).ok_or_else(|| format!("unknown deployment status: {value:?}").into())
    }
  }

  // --- SQLite: TEXT (the canonical DDL's documented SQLite translation)

  impl sqlx::Type<sqlx::Sqlite> for DeploymentStatus {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
      <str as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
  }

  impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for DeploymentStatus {
    fn decode(
      value: sqlx::sqlite::SqliteValueRef<'r>,
    ) -> Result<Self, Box<dyn Error + 'static + Send + Sync>> {
      let s = <&str as sqlx::Decode<sqlx::Sqlite>>::decode(value)?;
      Self::from_db_str(s)
    }
  }

  impl<'q> sqlx::Encode<'q, sqlx::Sqlite> for DeploymentStatus {
    fn encode_by_ref(
      &self,
      buf: &mut Vec<sqlx::sqlite::SqliteArgumentValue<'q>>,
    ) -> Result<sqlx::encode::IsNull, Box<dyn Error + 'static + Send + Sync>> {
      <&str as sqlx::Encode<sqlx::Sqlite>>::encode(self.as_str(), buf)
    }
  }

  // --- Postgres: TEXT (the canonical DDL; NOT a native enum — see the
  // module doc)

  impl sqlx::Type<sqlx::Postgres> for DeploymentStatus {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
      // PgTypeInfo::with_name("text") — TEXT is the canonical column type
      // (the derive-style constructors are crate-private).
      sqlx::postgres::PgTypeInfo::with_name("text")
    }
  }

  impl<'r> sqlx::Decode<'r, sqlx::Postgres> for DeploymentStatus {
    fn decode(
      value: sqlx::postgres::PgValueRef<'r>,
    ) -> Result<Self, Box<dyn Error + 'static + Send + Sync>> {
      let s = <&str as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
      Self::from_db_str(s)
    }
  }

  impl sqlx::Encode<'_, sqlx::Postgres> for DeploymentStatus {
    fn encode_by_ref(
      &self,
      buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, Box<dyn Error + 'static + Send + Sync>> {
      <&str as sqlx::Encode<sqlx::Postgres>>::encode(self.as_str(), buf)
    }
  }
}

/// The three key classes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyKind {
  /// `skp_…` — org-scoped control-plane key (CLI / dashboard / SDK).
  Platform,
  /// `ski_…` — per-app key for the app's TRUSTED BACKEND (minting
  /// credential; full data plane for its app). Never shipped to browsers
  /// in multi-user apps.
  Invocation,
  /// `sks_…` — per-user browser token minted by the app's backend
  /// (`POST /tokens`): app-scoped + bound to one `external_user_id`, short
  /// TTL. Rows carry `expires_at`; an expired token authenticates as
  /// nonexistent (same as revoked).
  User,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Org {
  pub id: Uuid,
  pub name: String,
  pub slug: String,
  pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
  pub id: Uuid,
  pub org_id: Uuid,
  pub email: String,
  pub name: Option<String>,
  pub created_at: DateTime<Utc>,
}

/// Metadata for one stored secret — the ONLY shape any list/read API gets.
/// No value field exists on purpose (write-only by contract).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretMeta {
  pub key: String,
  pub created_at: DateTime<Utc>,
  pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct App {
  pub id: Uuid,
  pub org_id: Uuid,
  pub name: String,
  pub slug: String,
  pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiKey {
  pub id: Uuid,
  pub org_id: Uuid,
  /// NULL for platform keys.
  pub app_id: Option<Uuid>,
  pub kind: ApiKeyKind,
  pub name: String,
  /// Only the hash is stored; keys are never readable back.
  pub key_hash: String,
  /// User tokens only (`kind = User`): the app-defined end-user identity
  /// the token is bound to (NULL for the other classes).
  pub external_user_id: Option<String>,
  /// User tokens only: hard expiry (mint TTL). NULL = no expiry
  /// (the long-lived key classes).
  pub expires_at: Option<DateTime<Utc>>,
  pub created_at: DateTime<Utc>,
  pub last_used_at: Option<DateTime<Utc>>,
  pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Deployment {
  pub id: Uuid,
  pub app_id: Uuid,
  pub version: i64,
  pub status: DeploymentStatus,
  /// Authoritative manifest from the SECTR_MANIFEST import (see
  /// [`deployment_serves_route`] for the MVP route-matching rule).
  pub manifest: Value,
  /// Tarball location, set once built.
  pub artifact_uri: Option<String>,
  /// Opaque builder-specific reference for the CURRENT build attempt
  /// (e.g. a Cloud Build operation id, or a docker build tag) — set while
  /// the deployment is `building` so startup can re-check in-flight builds
  /// after a crash. Not interpreted by the registry: each builder
  /// stores whatever identifier it polls. `None` once the build resolves.
  pub build_ref: Option<String>,
  /// Failure reason of a `failed` build: a truncated tail of the build /
  /// failure output (or the static stale-sweep message for rows orphaned
  /// by a server restart). `None` while `building` and for successful
  /// builds; surfaced through the public API's Deployment DTO.
  pub error: Option<String>,
  /// The customer's endpoint for an endpoint deployment (the sidecar in
  /// serve mode, woken over HTTP for each turn). `None` = a managed
  /// deployment, built and run by the server's platform. Set once at
  /// creation.
  pub endpoint_url: Option<String>,
  pub created_at: DateTime<Utc>,
}

/// status transitions: active -> awaiting_approval <-> active -> ended
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
  pub id: Uuid,
  pub app_id: Uuid,
  /// Sessions pin to the deployment they started on.
  pub deployment_id: Uuid,
  pub route: String,
  /// Which principal class created the session (api | playground).
  pub origin: Origin,
  /// Owner identity when the session was created by a user token
  ///: `(app_id, external_user_id)` namespacing. `None` =
  /// backend/platform-created (no owner; never visible to user tokens).
  pub external_user_id: Option<String>,
  pub status: SessionStatus,
  pub created_at: DateTime<Utc>,
  pub updated_at: Option<DateTime<Utc>>,
  pub ended_at: Option<DateTime<Utc>>,
}

/// A runner lease: at most one live lease per session
/// (UNIQUE partial index); claim/heartbeat policy lives in sectr-worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lease {
  pub id: Uuid,
  pub session_id: Uuid,
  pub runner_id: String,
  /// The turn the runner is currently executing — a definite statement,
  /// never a gap in information: `None` means the runner is active on the
  /// session and there is NO current invocation (idle-but-responsible,
  /// e.g. a persistent runner between turns or awaiting its first
  /// assignment). It does NOT mean "unknown".
  pub invocation_id: Option<Uuid>,
  pub claimed_at: DateTime<Utc>,
  pub heartbeat_at: DateTime<Utc>,
  pub expires_at: DateTime<Utc>,
  pub released_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Input types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct NewApp {
  pub org_id: Uuid,
  pub name: String,
  pub slug: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewDeployment {
  pub app_id: Uuid,
  pub version: i64,
  /// Starts in `building`; `artifact_uri` set once built.
  pub manifest: Value,
  /// `Some` for an endpoint deployment ([`Deployment::endpoint_url`]).
  pub endpoint_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewSession {
  /// Caller-chosen session id (the AG-UI `threadId` IS the session id).
  /// `None` = mint a fresh UUIDv7. An id that already exists is a
  /// store error; callers look the id up first.
  pub id: Option<Uuid>,
  pub app_id: Uuid,
  /// The deployment the session pins to.
  pub deployment_id: Uuid,
  pub route: String,
  /// Which principal class creates it (journaled alongside; also the API
  /// `SessionSummary.origin`).
  pub origin: Origin,
  /// Owner identity for user-token-created sessions: the
  /// `(app_id, external_user_id)` pair scopes browser-token access.
  /// `None` = backend/platform-created (no owner — invisible to user
  /// tokens, forever).
  pub external_user_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Route matching
// ---------------------------------------------------------------------------

/// MVP route matching: the manifest's `routes` array is a list of
/// `{path: "/..."}` objects (shape produced by the SDK's SECTR_MANIFEST
/// import); a deployment serves a route on exact path match. Simple, and
/// dialect-neutral — no JSON queries inside the store.
pub fn deployment_serves_route(manifest: &Value, route: &str) -> bool {
  manifest
    .get("routes")
    .and_then(Value::as_array)
    .map(|routes| {
      routes
        .iter()
        .any(|r| r.get("path").and_then(Value::as_str) == Some(route))
    })
    .unwrap_or(false)
}

/// The route paths a deployment manifest serves (`routes[].path`), in
/// manifest order — for "unknown route; available: …" errors.
pub fn manifest_routes(manifest: &Value) -> Vec<String> {
  manifest
    .get("routes")
    .and_then(Value::as_array)
    .map(|routes| {
      routes
        .iter()
        .filter_map(|r| r.get("path").and_then(Value::as_str).map(str::to_owned))
        .collect()
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
pub trait Registry: Send + Sync {
  // -- orgs / apps --------------------------------------------------------

  async fn create_org(&self, name: &str, slug: &str) -> Result<Org>;
  async fn get_org(&self, id: Uuid) -> Result<Org>;
  /// MVP single-org lookup by slug (bootstrap + control-plane reads).
  async fn find_org_by_slug(&self, slug: &str) -> Result<Option<Org>>;
  async fn create_app(&self, new: NewApp) -> Result<App>;
  async fn get_app(&self, id: Uuid) -> Result<App>;
  /// An org's apps, newest first (the console overview + sidebar).
  async fn list_apps(&self, org_id: Uuid) -> Result<Vec<App>>;
  /// Sessions-per-app counts for one org (the overview's session
  /// columns). Apps with zero sessions are ABSENT — join client-side.
  async fn count_sessions_by_app(&self, org_id: Uuid) -> Result<Vec<(Uuid, i64)>>;
  /// Name resolution for `NewSessionRequest.app` (slug; apps carry
  /// name = slug in MVP) scoped to the caller's org.
  async fn find_app_by_slug(&self, org_id: OrgId, slug: &str) -> Result<Option<App>>;

  /// User-token minting: like [`Self::create_api_key`], but the
  /// row additionally binds the end-user identity and a hard expiry.
  /// `minted_by_key_id` is the presenting invocation key: revoking it
  /// revokes the token ([`Self::revoke_api_key`] cascades), and minting
  /// against an already-revoked key is `NotFound` (closes the
  /// mint-vs-revoke race). Returns the stored row (hash only).
  async fn create_user_token(
    &self,
    org_id: OrgId,
    app_id: AppId,
    minted_by_key_id: Uuid,
    external_user_id: &str,
    key_hash: &str,
    expires_at: DateTime<Utc>,
  ) -> Result<ApiKey>;

  // -- deployments --------------------------------------------------------

  /// Insert a deployment in `building` status. The returned row is read
  /// back from the store — it is always the DB projection at creation
  /// time; later setters are not reflected until re-fetch.
  async fn create_deployment(&self, new: NewDeployment) -> Result<Deployment>;
  async fn get_deployment(&self, id: DeploymentId) -> Result<Deployment>;
  async fn set_deployment_status(&self, id: DeploymentId, status: DeploymentStatus) -> Result<()>;
  /// Set once, after the build produced the tarball.
  async fn set_artifact_uri(&self, id: DeploymentId, uri: &str) -> Result<()>;
  /// Bind or clear the builder-specific in-flight-build reference (opaque
  /// to the registry; see [`Deployment::build_ref`]). `None` is a
  /// definite statement: the build has resolved (or not started).
  async fn set_deployment_build_ref(&self, id: DeploymentId, build_ref: Option<&str>)
  -> Result<()>;
  /// Bind or clear the build's failure reason (see
  /// [`Deployment::error`]). `Some` when a build fails (truncated output
  /// tail, or the stale-sweep's static message); `None` on success.
  async fn set_deployment_build_error(&self, id: DeploymentId, error: Option<&str>) -> Result<()>;
  /// The app's deployment history, newest version first (dashboard table
  /// + `sectr deploy` status polling).
  async fn list_deployments(&self, app_id: AppId) -> Result<Vec<Deployment>>;
  /// Overwrite the manifest — the deploy build replaces the placeholder
  /// with the authoritative import result (+ platform runner fields).
  async fn set_deployment_manifest(&self, id: DeploymentId, manifest: &Value) -> Result<()>;
  /// The deployment new sessions of `app_id` route to: the most recently
  /// created `active` one (ties broken by higher version).
  async fn active_deployment(&self, app_id: AppId) -> Result<Option<Deployment>>;

  // -- sessions -----------------------------------------------------------

  /// Insert a session in `active` status. The accompanying `SESSION_CREATED`
  /// journal event is the caller's orchestration (Registry first, then
  /// Journal).
  async fn create_session(&self, new: NewSession) -> Result<Session>;
  async fn get_session(&self, id: Uuid) -> Result<Session>;
  /// List sessions newest-first, keyset-paginated on `created_at`
  /// (`created_before` = "rows strictly older than"), optional org, app
  /// and owner (`external_user_id`) filters (all None = everything — dev
  /// has no tenancy). Status FILTERING is the API layer's job — the
  /// session's API-visible status is derived from the journal (the source
  /// of truth), not from this row model.
  async fn list_sessions(
    &self,
    org_id: Option<OrgId>,
    app_id: Option<AppId>,
    external_user_id: Option<&str>,
    limit: i64,
    created_before: Option<DateTime<Utc>>,
  ) -> Result<Vec<Session>>;
  /// The session's lease if it is LIVE (`released_at IS NULL AND
  /// expires_at > now`) — the "session is mid-turn" signal for the
  /// API layer (409 on input while a turn is running).
  async fn live_lease(&self, id: Uuid) -> Result<Option<Lease>>;
  /// Setting `Ended` also stamps `ended_at`; every update stamps
  /// `updated_at`.
  async fn set_session_status(&self, id: Uuid, status: SessionStatus) -> Result<()>;

  // -- secrets (write-only by contract: values only leave the store for
  //    the worker's env injection) ----------------------------------------

  /// Upsert.
  async fn set_secret(&self, app_id: AppId, key: &str, value: Secret) -> Result<()>;
  /// Idempotent.
  async fn delete_secret(&self, app_id: AppId, key: &str) -> Result<()>;
  /// All of an app's secrets as env pairs, redacted in transit
  /// ([`Secret`]) — same contract as [`crate::drivers::SecretSource`]:
  /// values only ever leave the store into a runner env, via explicit
  /// [`Secret::expose`]/[`into_inner`](Secret::into_inner).
  async fn resolve_secrets(&self, app_id: AppId) -> Result<Vec<(String, Secret)>>;
  /// Names + timestamps ONLY — the read side of the write-only contract.
  /// No method of this trait returns a secret VALUE to a caller except
  /// [`Registry::resolve_secrets`], whose consumers are dispatch paths,
  /// never HTTP handlers.
  async fn list_secret_names(&self, app_id: AppId) -> Result<Vec<SecretMeta>>;

  // -- api keys -----------------------------------------------------------

  /// Insert a key (hash only — the full value never touches the store;
  /// the caller mints + returns it to the user exactly once).
  async fn create_api_key(
    &self,
    org_id: OrgId,
    app_id: Option<AppId>,
    kind: ApiKeyKind,
    name: &str,
    key_hash: &str,
  ) -> Result<ApiKey>;
  /// All of an org's keys, newest first (metadata only — hashes are not
  /// secrets, but they are also not display data).
  async fn list_api_keys(&self, org_id: OrgId) -> Result<Vec<ApiKey>>;
  /// Stamp `revoked_at` (auth treats revoked == nonexistent). Unknown or
  /// other-org key → NotFound; already-revoked → success (the HTTP DELETE
  /// it serves returns 204 either way).
  async fn revoke_api_key(&self, org_id: OrgId, key_id: Uuid) -> Result<()>;
  /// Auth lookup by SHA-256 of the presented key; stamps `last_used_at`.
  /// Returns None for unknown or revoked keys.
  async fn find_api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>>;
  /// Is this key still live (exists, not revoked, not expired)? Read-only
  /// — unlike [`Self::find_api_key_by_hash`] it does not stamp
  /// `last_used_at`. Used to end long-lived streams.
  async fn api_key_is_live(&self, key_id: Uuid) -> Result<bool>;
  /// Delete keys of ANY kind that expired before `expired_before` (user
  /// tokens and dashboard-session platform keys); returns how
  /// many went. Keys without an `expires_at` are never touched. Batched
  /// internally, idempotent, and safe to run from several servers at once.
  async fn purge_expired_keys(&self, expired_before: DateTime<Utc>) -> Result<u64>;

  // -- leases ------------------------------------------------------------

  /// Claim a runner lease on a session. Fails with `Conflict` if a live,
  /// unexpired lease exists; an expired one is superseded. At most one
  /// live lease per session is enforced by a partial unique index —
  /// concurrent claims for the same session cannot both succeed. (The
  /// Postgres implementation will use FOR UPDATE SKIP LOCKED inside the
  /// transaction; SQLite's BEGIN IMMEDIATE serializes writers to the same
  /// effect.)
  ///
  /// `invocation_id` is a definite statement, not a gap in information:
  /// `None` = the runner is active on the session and there is NO current
  /// invocation (idle-but-responsible). See [`Lease::invocation_id`].
  async fn claim_lease(
    &self,
    session: Uuid,
    runner_id: &str,
    invocation_id: Option<Uuid>,
    ttl: chrono::Duration,
  ) -> Result<Lease>;
  /// [`Self::claim_lease`] plus the concurrency caps, enforced
  /// atomically: the live-turn count and the claim happen in ONE
  /// transaction (Postgres serializes capped claims platform-wide under an
  /// advisory transaction lock, taken only when a cap is set), so
  /// concurrent claims cannot overshoot a cap. A reached cap is
  /// [`RegistryError::CapExceeded`] with nothing written; a session that
  /// already holds a live lease is still `Conflict`.
  async fn claim_lease_capped(
    &self,
    session: Uuid,
    runner_id: &str,
    invocation_id: Option<Uuid>,
    ttl: chrono::Duration,
    caps: &LeaseCaps,
  ) -> Result<Lease>;
  /// Live turns `(for app_id, platform-wide)` of one kind (`on_endpoint`,
  /// see [`LeaseCaps`]) — live leases joined to their session and its
  /// deployment. A non-atomic read for admission pre-checks; the
  /// authoritative check is [`Self::claim_lease_capped`].
  async fn count_live_turns(&self, app_id: Uuid, on_endpoint: bool) -> Result<(u64, u64)>;
  /// Extend a live lease: stamps `heartbeat_at` and pushes `expires_at`
  /// out by `ttl`. Fails if the lease was released or never existed.
  async fn heartbeat_lease(&self, lease_id: Uuid, ttl: chrono::Duration) -> Result<Lease>;
  /// Release a live lease (idempotent on already-released leases is NOT
  /// guaranteed — releasing an unknown id fails; releasing a released one
  /// succeeds without change).
  async fn release_lease(&self, lease_id: Uuid) -> Result<()>;

  /// Atomically release a lease IF (and only if) it is still live AND
  /// already expired — the reconciler's re-check, built into one
  /// statement so a heartbeat landing between scan and reclaim cannot
  /// resurrect a lease the reconciler is about to end. Returns the
  /// reclaimed lease, or `None` when it was released/extended meanwhile
  /// (another actor won the race; the reconciler must then do NOTHING).
  async fn reclaim_expired_lease(&self, lease_id: Uuid) -> Result<Option<Lease>>;
  /// Bind or clear a live lease's current-work annotation (persistent-
  /// runner mode: `Some(invocation)` at turn start, `None` at turn end;
  /// per-turn mode never calls this — it claims and releases instead).
  /// Like [`Lease::invocation_id`], `None` is a definite statement (idle,
  /// no current invocation), not "unknown". Fails if the lease is unknown
  /// or released. Reconciliation invariant
  /// this preserves: a live lease with `Some(inv)` must correspond to a
  /// pending invocation; a live lease with `None` is an idle-but-
  /// responsible runner — both checkable against the journal in every
  /// mode.
  async fn set_invocation(&self, lease_id: Uuid, invocation: Option<Uuid>) -> Result<Lease>;
  /// Live leases whose TTL has expired (`released_at IS NULL AND
  /// expires_at <= now`), oldest expiry first: claims whose runner stopped
  /// refreshing (stalled, crashed, or the whole worker was lost). The
  /// reconciler's scan input — the server-role layer consumes these to
  /// synthesize abandoned-turn ends; the engine never scans.
  async fn expired_leases(&self) -> Result<Vec<Lease>>;
}
