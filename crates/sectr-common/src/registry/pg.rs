//! Postgres registry — the production [`Registry`] implementation. Mechanical
//! port of `SqliteRegistry` semantics
//! plus the Postgres-native lease claim:
//!
//! - **claim**: one transaction; the live-lease row (if any) is locked
//!   `FOR UPDATE SKIP LOCKED` — SKIP keeps a claimant from queueing behind a
//!   concurrent claim/supersede transaction, and the partial unique index
//!   `idx_leases_session_live` is the backstop that makes concurrent claims
//!   for an unleased session fail loudly for exactly one of them
//!   (unique-violation mapped to a friendly `Conflict`).
//! - **heartbeats/scan**: single-statement, no locks — `expired_leases` is a
//!   plain read (the reconciler's kill-then-release acts per session).

use super::{
  ApiKey, ApiKeyKind, App, AppId, Deployment, DeploymentId, DeploymentStatus, Lease, LeaseCaps,
  NewApp, NewDeployment, NewSession, Org, OrgId, Registry, RegistryError, Result, SecretMeta,
  Session, SessionStatus,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::Connection;
use sqlx::PgPool;
use uuid::Uuid;

use crate::secret::Secret;

/// Timestamp helper — truncated to microseconds to match what Postgres
/// `timestamptz` stores, so in-memory rows equal their fetched forms.
fn now() -> DateTime<Utc> {
  use chrono::SubsecRound;
  Utc::now().trunc_subsecs(6)
}

/// Postgres-backed [`Registry`]. Cheap to clone (the pool is a handle).
#[derive(Clone)]
pub struct PgRegistry {
  pool: PgPool,
}

/// `(id, app_id, deployment_id, route, origin, status, created_at, updated_at, ended_at, external_user_id)`
type SessionRow = (
  Uuid,
  Uuid,
  Uuid,
  String,
  String,
  String,
  DateTime<Utc>,
  Option<DateTime<Utc>>,
  Option<DateTime<Utc>>,
  Option<String>,
);

/// `(id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, expires_at, released_at)`
type LeaseRow = (
  Uuid,
  Uuid,
  String,
  Option<Uuid>,
  DateTime<Utc>,
  DateTime<Utc>,
  DateTime<Utc>,
  Option<DateTime<Utc>>,
);

/// Map the partial-unique-index violation from a lost concurrent claim race
/// to the same friendly conflict the fast path produces.
fn unique_violation(err: sqlx::Error) -> RegistryError {
  if err
    .as_database_error()
    .map(|e| e.code().as_deref() == Some("23505"))
    .unwrap_or(false)
  {
    RegistryError::Conflict("concurrent lease claim lost the race (live-lease unique index)".into())
  } else {
    RegistryError::Sql(err)
  }
}

impl PgRegistry {
  /// Wrap an existing pool (migrations applied — open via
  /// [`crate::store::PgStore::open`]).
  pub fn new(pool: PgPool) -> Self {
    PgRegistry { pool }
  }

  /// The underlying pool.
  pub fn pool(&self) -> &PgPool {
    &self.pool
  }

  /// Readiness probe: `SELECT 1` bounded by a 2 s timeout. The
  /// timeout covers pool acquisition too, so an exhausted or unreachable
  /// pool reads as unavailable instead of hanging the probe.
  pub async fn ping(&self) -> Result<()> {
    self.ping_within(std::time::Duration::from_secs(2)).await
  }

  /// [`Self::ping`] with an explicit bound (tests trip it cheaply).
  pub async fn ping_within(&self, timeout: std::time::Duration) -> Result<()> {
    match tokio::time::timeout(timeout, sqlx::query("SELECT 1").execute(&self.pool)).await {
      Ok(res) => res.map(|_| ()).map_err(RegistryError::Sql),
      Err(_) => Err(RegistryError::Sql(sqlx::Error::PoolTimedOut)),
    }
  }

  /// Shared by `claim_lease` and `claim_lease_capped` (`caps` = None skips
  /// the guardrail cap check entirely).
  async fn claim_lease_inner(
    &self,
    session: Uuid,
    runner_id: &str,
    invocation_id: Option<Uuid>,
    ttl: chrono::Duration,
    caps: Option<&LeaseCaps>,
  ) -> Result<Lease> {
    // Lease claim — the turn-boundary coordination point; the
    // conflict path (mid-turn 409s) is as visible as the win.
    let span = tracing::info_span!(
        "lease.claim",
        session_id = %session,
        runner_id = %runner_id,
        invocation_id = tracing::field::Empty,
        ttl_secs = ttl.num_seconds(),
        lease_id = tracing::field::Empty,
    );
    if let Some(inv) = invocation_id {
      span.record("invocation_id", inv.to_string());
    }
    let _guard = span.enter();

    // Fast, lock-free pre-check for the common "already live-leased"
    // path — friendly conflict without touching row locks.
    let existing: Option<(Uuid, DateTime<Utc>, String)> = sqlx::query_as(
      "SELECT id, expires_at, runner_id FROM leases \
             WHERE session_id = $1 AND released_at IS NULL",
    )
    .bind(session)
    .fetch_optional(&self.pool)
    .await?;
    if let Some((_, expires_at, holder)) = existing
      && expires_at > now()
    {
      return Err(RegistryError::Conflict(format!(
        "session {session} already has a live lease (runner {holder}) until {expires_at}"
      )));
    }

    let mut conn = self.pool.acquire().await?;
    let mut tx = conn.begin().await?;
    match claim_tx(&mut tx, session, runner_id, invocation_id, ttl, caps).await {
      Ok(lease) => {
        tx.commit().await.map_err(unique_violation)?;
        span.record("lease_id", lease.id.to_string());
        Ok(lease)
      }
      // The lost-race unique violation most often surfaces at the
      // INSERT inside claim_tx (the winner's committed row is what the
      // index checks against), not at commit — map it either way.
      Err(err) => {
        let _ = tx.rollback().await;
        Err(match err {
          RegistryError::Sql(e) => unique_violation(e),
          other => other,
        })
      }
    }
  }

  /// Full-row deployment read, shared by `create_deployment` (read-back
  /// invariant) and `get_deployment` — the store's projection is the only
  /// source of the returned value.
  async fn fetch_deployment(&self, id: DeploymentId) -> Result<Deployment> {
    sqlx::query_as::<_, Deployment>(
      "SELECT id, app_id, version, status, manifest, artifact_uri, build_ref, error, endpoint_url, created_at \
             FROM deployments WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&self.pool)
    .await?
    .ok_or_else(|| RegistryError::NotFound(format!("deployment {id} not found")))
  }

  fn org(row: (Uuid, String, String, DateTime<Utc>)) -> Org {
    Org {
      id: row.0,
      name: row.1,
      slug: row.2,
      created_at: row.3,
    }
  }

  fn app(row: (Uuid, Uuid, String, String, DateTime<Utc>)) -> App {
    App {
      id: row.0,
      org_id: row.1,
      name: row.2,
      slug: row.3,
      created_at: row.4,
    }
  }

  fn session(row: SessionRow) -> Result<Session> {
    Ok(Session {
      id: row.0,
      app_id: row.1,
      deployment_id: row.2,
      route: row.3,
      origin: serde_json::from_value(Value::String(row.4))?,
      status: serde_json::from_value(Value::String(row.5))?,
      created_at: row.6,
      updated_at: row.7,
      ended_at: row.8,
      external_user_id: row.9,
    })
  }

  fn lease(row: LeaseRow) -> Lease {
    Lease {
      id: row.0,
      session_id: row.1,
      runner_id: row.2,
      invocation_id: row.3,
      claimed_at: row.4,
      heartbeat_at: row.5,
      expires_at: row.6,
      released_at: row.7,
    }
  }
}

/// Rows deleted per statement by `purge_expired_keys` (bounds the
/// transaction size on a first run over a large backlog).
const PURGE_BATCH: i64 = 5000;

#[async_trait::async_trait]
impl Registry for PgRegistry {
  async fn create_org(&self, name: &str, slug: &str) -> Result<Org> {
    let id = Uuid::now_v7();
    let created_at = now();
    sqlx::query("INSERT INTO orgs (id, name, slug, created_at) VALUES ($1, $2, $3, $4)")
      .bind(id)
      .bind(name)
      .bind(slug)
      .bind(created_at)
      .execute(&self.pool)
      .await?;
    Ok(Org {
      id,
      name: name.into(),
      slug: slug.into(),
      created_at,
    })
  }

  async fn get_org(&self, id: Uuid) -> Result<Org> {
    sqlx::query_as::<_, (Uuid, String, String, DateTime<Utc>)>(
      "SELECT id, name, slug, created_at FROM orgs WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&self.pool)
    .await?
    .map(Self::org)
    .ok_or_else(|| RegistryError::NotFound(format!("org {id}")))
  }

  async fn find_org_by_slug(&self, slug: &str) -> Result<Option<Org>> {
    Ok(
      sqlx::query_as::<_, (Uuid, String, String, DateTime<Utc>)>(
        "SELECT id, name, slug, created_at FROM orgs WHERE slug = $1",
      )
      .bind(slug)
      .fetch_optional(&self.pool)
      .await?
      .map(Self::org),
    )
  }

  async fn create_app(&self, new: NewApp) -> Result<App> {
    let id = Uuid::now_v7();
    let created_at = now();
    sqlx::query(
      "INSERT INTO apps (id, org_id, name, slug, created_at) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(new.org_id)
    .bind(&new.name)
    .bind(&new.slug)
    .bind(created_at)
    .execute(&self.pool)
    .await?;
    Ok(App {
      id,
      org_id: new.org_id,
      name: new.name,
      slug: new.slug,
      created_at,
    })
  }

  async fn get_app(&self, id: Uuid) -> Result<App> {
    sqlx::query_as::<_, (Uuid, Uuid, String, String, DateTime<Utc>)>(
      "SELECT id, org_id, name, slug, created_at FROM apps WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&self.pool)
    .await?
    .map(Self::app)
    .ok_or_else(|| RegistryError::NotFound(format!("app {id}")))
  }

  async fn list_apps(&self, org_id: Uuid) -> Result<Vec<App>> {
    let rows = sqlx::query_as::<_, (Uuid, Uuid, String, String, DateTime<Utc>)>(
      "SELECT id, org_id, name, slug, created_at FROM apps \
             WHERE org_id = $1 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(&self.pool)
    .await?;
    Ok(rows.into_iter().map(Self::app).collect())
  }

  async fn count_sessions_by_app(&self, org_id: Uuid) -> Result<Vec<(Uuid, i64)>> {
    sqlx::query_as(
      "SELECT s.app_id, COUNT(*) FROM sessions s \
             JOIN apps a ON a.id = s.app_id WHERE a.org_id = $1 GROUP BY s.app_id",
    )
    .bind(org_id)
    .fetch_all(&self.pool)
    .await
    .map_err(Into::into)
  }

  async fn find_app_by_slug(&self, org_id: OrgId, slug: &str) -> Result<Option<App>> {
    Ok(
      sqlx::query_as::<_, (Uuid, Uuid, String, String, DateTime<Utc>)>(
        "SELECT id, org_id, name, slug, created_at FROM apps WHERE org_id = $1 AND slug = $2",
      )
      .bind(org_id)
      .bind(slug)
      .fetch_optional(&self.pool)
      .await?
      .map(Self::app),
    )
  }

  async fn create_deployment(&self, new: NewDeployment) -> Result<Deployment> {
    let id = Uuid::now_v7();
    sqlx::query(
      "INSERT INTO deployments (id, app_id, version, status, manifest, endpoint_url, created_at) \
             VALUES ($1, $2, $3, 'building', $4, $5, $6)",
    )
    .bind(id)
    .bind(new.app_id)
    .bind(new.version)
    .bind(new.manifest.clone())
    .bind(new.endpoint_url.as_deref())
    .bind(now())
    .execute(&self.pool)
    .await?;
    // Read-back invariant: return the store's projection (including the
    // DB-written created_at), never a caller-constructed row.
    self.fetch_deployment(id).await
  }

  async fn get_deployment(&self, id: DeploymentId) -> Result<Deployment> {
    self.fetch_deployment(id).await
  }

  async fn set_deployment_status(&self, id: DeploymentId, status: DeploymentStatus) -> Result<()> {
    let status = serde_json::to_value(&status)?;
    let status = status.as_str().expect("unit enum serializes to string");
    sqlx::query("UPDATE deployments SET status = $1 WHERE id = $2")
      .bind(status)
      .bind(id)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn set_artifact_uri(&self, id: DeploymentId, uri: &str) -> Result<()> {
    sqlx::query("UPDATE deployments SET artifact_uri = $1 WHERE id = $2")
      .bind(uri)
      .bind(id)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn set_deployment_build_ref(
    &self,
    id: DeploymentId,
    build_ref: Option<&str>,
  ) -> Result<()> {
    sqlx::query("UPDATE deployments SET build_ref = $1 WHERE id = $2")
      .bind(build_ref)
      .bind(id)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn set_deployment_build_error(&self, id: DeploymentId, error: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE deployments SET error = $1 WHERE id = $2")
      .bind(error)
      .bind(id)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn set_deployment_manifest(&self, id: DeploymentId, manifest: &Value) -> Result<()> {
    sqlx::query("UPDATE deployments SET manifest = $1 WHERE id = $2")
      .bind(manifest)
      .bind(id)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn list_deployments(&self, app_id: AppId) -> Result<Vec<Deployment>> {
    Ok(
      sqlx::query_as::<_, Deployment>(
        "SELECT id, app_id, version, status, manifest, artifact_uri, build_ref, error, endpoint_url, created_at \
             FROM deployments WHERE app_id = $1 ORDER BY version DESC, created_at DESC",
      )
      .bind(app_id)
      .fetch_all(&self.pool)
      .await?,
    )
  }

  async fn active_deployment(&self, app_id: AppId) -> Result<Option<Deployment>> {
    Ok(
      sqlx::query_as::<_, Deployment>(
        "SELECT id, app_id, version, status, manifest, artifact_uri, build_ref, error, endpoint_url, created_at \
             FROM deployments WHERE app_id = $1 AND status = 'active' \
             ORDER BY created_at DESC, version DESC LIMIT 1",
      )
      .bind(app_id)
      .fetch_optional(&self.pool)
      .await?,
    )
  }

  async fn create_session(&self, new: NewSession) -> Result<Session> {
    let id = new.id.unwrap_or_else(Uuid::now_v7);
    let created_at = now();
    sqlx::query(
      "INSERT INTO sessions (id, app_id, deployment_id, route, origin, status, created_at, external_user_id) \
             VALUES ($1, $2, $3, $4, $5, 'active', $6, $7)",
    )
    .bind(id)
    .bind(new.app_id)
    .bind(new.deployment_id)
    .bind(&new.route)
    // Bind the origin as a plain str — a serde_json::Value::String would
    // arrive as a JSON literal ("playground", quotes included) and trip
    // the CHECK constraint.
    .bind(
      serde_json::to_value(new.origin)
        .expect("Origin serializes")
        .as_str()
        .expect("unit enum")
        .to_owned(),
    )
    .bind(created_at)
    .bind(&new.external_user_id)
    .execute(&self.pool)
    .await?;
    Ok(Session {
      id,
      app_id: new.app_id,
      deployment_id: new.deployment_id,
      route: new.route,
      origin: new.origin,
      external_user_id: new.external_user_id,
      status: SessionStatus::Active,
      created_at,
      updated_at: None,
      ended_at: None,
    })
  }

  async fn delete_unstarted_session(&self, id: Uuid) -> Result<bool> {
    // Lock the row first, then delete in a second statement. A lease claim
    // in flight holds a key-share lock on the session row until it
    // commits. A lone DELETE would wait for that lock, then check its NOT
    // EXISTS against the snapshot it started with, which can't see the
    // new lease, and the cascade would remove it. Under READ COMMITTED
    // the DELETE after the lock takes a fresh snapshot that does see it.
    let mut tx = self.pool.begin().await?;
    let locked: Option<Uuid> =
      sqlx::query_scalar("SELECT id FROM sessions WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    if locked.is_none() {
      return Ok(false);
    }
    let result = sqlx::query(
      "DELETE FROM sessions WHERE id = $1 \
             AND NOT EXISTS (SELECT 1 FROM events WHERE events.session_id = sessions.id AND events.seq > 0) \
             AND NOT EXISTS (SELECT 1 FROM leases WHERE leases.session_id = sessions.id)",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
  }

  async fn get_session(&self, id: Uuid) -> Result<Session> {
    sqlx::query_as::<_, SessionRow>(
      "SELECT id, app_id, deployment_id, route, origin, status, created_at, updated_at, ended_at, external_user_id \
             FROM sessions WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&self.pool)
    .await?
    .ok_or_else(|| RegistryError::NotFound(format!("session {id} not found")))
    .and_then(Self::session)
  }

  async fn list_sessions(
    &self,
    org_id: Option<OrgId>,
    app_id: Option<AppId>,
    external_user_id: Option<&str>,
    limit: i64,
    created_before: Option<DateTime<Utc>>,
  ) -> Result<Vec<Session>> {
    let query = sqlx::query_as::<_, SessionRow>(
            "SELECT s.id, s.app_id, s.deployment_id, s.route, s.origin, s.status, s.created_at, s.updated_at, s.ended_at, s.external_user_id \
             FROM sessions s JOIN apps a ON a.id = s.app_id \
             WHERE ($1::uuid IS NULL OR a.org_id = $1) \
             AND ($2::uuid IS NULL OR s.app_id = $2) \
             AND ($5::text IS NULL OR s.external_user_id = $5) \
             AND ($3::timestamptz IS NULL OR s.created_at < $3) \
             ORDER BY s.created_at DESC LIMIT $4",
        )
        .bind(org_id)
        .bind(app_id)
        .bind(created_before)
        .bind(limit)
        .bind(external_user_id);
    let rows = query.fetch_all(&self.pool).await?;
    rows.into_iter().map(Self::session).collect()
  }

  async fn live_lease(&self, id: Uuid) -> Result<Option<Lease>> {
    let now = now();
    let row = sqlx::query_as::<_, LeaseRow>(
      "SELECT id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, \
             expires_at, released_at FROM leases \
             WHERE session_id = $1 AND released_at IS NULL AND expires_at > $2",
    )
    .bind(id)
    .bind(now)
    .fetch_optional(&self.pool)
    .await?;
    Ok(row.map(Self::lease))
  }

  async fn set_session_status(&self, id: Uuid, status: SessionStatus) -> Result<()> {
    let status_value = serde_json::to_value(&status)?;
    let status = status_value
      .as_str()
      .expect("unit enum serializes to string");
    let now = now();
    // Setting Ended also stamps ended_at; every update stamps updated_at.
    sqlx::query(
      "UPDATE sessions SET status = $1, updated_at = $2, \
             ended_at = CASE WHEN $1 = 'ended' THEN $2 ELSE ended_at END \
             WHERE id = $3",
    )
    .bind(status)
    .bind(now)
    .bind(id)
    .execute(&self.pool)
    .await?;
    Ok(())
  }

  async fn set_secret(&self, app_id: AppId, key: &str, value: Secret) -> Result<()> {
    sqlx::query(
      "INSERT INTO secrets (id, app_id, key, value_encrypted, created_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (app_id, key) DO UPDATE SET \
             value_encrypted = excluded.value_encrypted, updated_at = $6",
    )
    .bind(Uuid::now_v7())
    .bind(app_id)
    .bind(key)
    .bind(value.expose())
    .bind(now())
    .bind(now())
    .execute(&self.pool)
    .await?;
    Ok(())
  }

  async fn delete_secret(&self, app_id: AppId, key: &str) -> Result<()> {
    // Idempotent by design: the HTTP DELETE it serves returns 204 either way.
    sqlx::query("DELETE FROM secrets WHERE app_id = $1 AND key = $2")
      .bind(app_id)
      .bind(key)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn resolve_secrets(&self, app_id: AppId) -> Result<Vec<(String, Secret)>> {
    let rows: Vec<(String, String)> =
      sqlx::query_as("SELECT key, value_encrypted FROM secrets WHERE app_id = $1")
        .bind(app_id)
        .fetch_all(&self.pool)
        .await?;
    Ok(
      rows
        .into_iter()
        .map(|(key, value)| (key, Secret::new(value)))
        .collect(),
    )
  }

  async fn list_secret_names(&self, app_id: AppId) -> Result<Vec<SecretMeta>> {
    let rows: Vec<(String, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>)> = sqlx::query_as(
      "SELECT key, created_at, updated_at FROM secrets \
                 WHERE app_id = $1 ORDER BY key",
    )
    .bind(app_id)
    .fetch_all(&self.pool)
    .await?;
    Ok(
      rows
        .into_iter()
        .map(|(key, created_at, updated_at)| SecretMeta {
          key,
          created_at,
          updated_at,
        })
        .collect(),
    )
  }

  async fn create_api_key(
    &self,
    org_id: OrgId,
    app_id: Option<AppId>,
    kind: ApiKeyKind,
    name: &str,
    key_hash: &str,
  ) -> Result<ApiKey> {
    let id = Uuid::now_v7();
    let created_at = now();
    let kind_value = serde_json::to_value(kind)?;
    let kind = kind_value.as_str().expect("unit enum serializes to string");
    sqlx::query(
      "INSERT INTO api_keys (id, org_id, app_id, kind, name, key_hash, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(org_id)
    .bind(app_id)
    .bind(kind)
    .bind(name)
    .bind(key_hash)
    .bind(created_at)
    .execute(&self.pool)
    .await?;
    Ok(ApiKey {
      id,
      org_id,
      app_id,
      kind: serde_json::from_value(serde_json::Value::String(kind.into()))?,
      name: name.into(),
      key_hash: key_hash.into(),
      external_user_id: None,
      expires_at: None,
      created_at,
      last_used_at: None,
      revoked_at: None,
    })
  }

  async fn api_key_is_live(&self, key_id: Uuid) -> Result<bool> {
    Ok(
      sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM api_keys WHERE id = $1 AND revoked_at IS NULL \
             AND (expires_at IS NULL OR expires_at > $2))",
      )
      .bind(key_id)
      .bind(now())
      .fetch_one(&self.pool)
      .await?,
    )
  }

  async fn purge_expired_keys(&self, expired_before: DateTime<Utc>) -> Result<u64> {
    let mut total = 0;
    loop {
      let deleted = sqlx::query(
        "DELETE FROM api_keys WHERE id IN ( \
               SELECT id FROM api_keys WHERE expires_at < $1 LIMIT $2)",
      )
      .bind(expired_before)
      .bind(PURGE_BATCH)
      .execute(&self.pool)
      .await?
      .rows_affected();
      total += deleted;
      if (deleted as i64) < PURGE_BATCH {
        return Ok(total);
      }
    }
  }

  async fn create_user_token(
    &self,
    org_id: OrgId,
    app_id: AppId,
    minted_by_key_id: Uuid,
    external_user_id: &str,
    key_hash: &str,
    expires_at: DateTime<Utc>,
  ) -> Result<ApiKey> {
    let id = Uuid::now_v7();
    let created_at = now();
    // Conditional on the minter still being live: a revoke that landed
    // between the caller's authentication and this insert must not leave
    // a live token behind (revoke_api_key's cascade has already run).
    let result = sqlx::query(
      "INSERT INTO api_keys (id, org_id, app_id, kind, name, key_hash, external_user_id, expires_at, created_at, minted_by_key_id) \
             SELECT $1, $2, $3, 'user', $4, $5, $6, $7, $8, $9 \
             WHERE EXISTS (SELECT 1 FROM api_keys WHERE id = $9 AND org_id = $2 AND kind = 'invocation' AND revoked_at IS NULL)",
    )
    .bind(id)
    .bind(org_id)
    .bind(app_id)
    // The name identifies the row (owner first, then a short id tail —
    // several tokens for one user may coexist across page loads).
    .bind(format!("user:{external_user_id}"))
    .bind(key_hash)
    .bind(external_user_id)
    .bind(expires_at)
    .bind(created_at)
    .bind(minted_by_key_id)
    .execute(&self.pool)
    .await?;
    if result.rows_affected() == 0 {
      return Err(RegistryError::NotFound(format!(
        "live invocation key {minted_by_key_id}"
      )));
    }
    Ok(ApiKey {
      id,
      org_id,
      app_id: Some(app_id),
      kind: ApiKeyKind::User,
      name: format!("user:{external_user_id}"),
      key_hash: key_hash.into(),
      external_user_id: Some(external_user_id.into()),
      expires_at: Some(expires_at),
      created_at,
      last_used_at: None,
      revoked_at: None,
    })
  }

  async fn list_api_keys(&self, org_id: OrgId) -> Result<Vec<ApiKey>> {
    // User tokens are deliberately EXCLUDED: they are ephemeral
    // mint-per-page-load credentials with no management UI — listing them
    // would be noise on the dashboard's settings page.
    let rows: Vec<(
      Uuid,
      Uuid,
      Option<Uuid>,
      String,
      String,
      String,
      Option<String>,
      Option<DateTime<Utc>>,
      DateTime<Utc>,
      Option<DateTime<Utc>>,
      Option<DateTime<Utc>>,
    )> = sqlx::query_as(
      "SELECT id, org_id, app_id, kind, name, key_hash, external_user_id, expires_at, created_at, last_used_at, revoked_at \
             FROM api_keys WHERE org_id = $1 AND kind != 'user' ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(&self.pool)
    .await?;
    rows
      .into_iter()
      .map(
        |(
          id,
          org_id,
          app_id,
          kind,
          name,
          key_hash,
          external_user_id,
          expires_at,
          created_at,
          last_used_at,
          revoked_at,
        )| {
          Ok(ApiKey {
            id,
            org_id,
            app_id,
            kind: serde_json::from_value(Value::String(kind))?,
            name,
            key_hash,
            external_user_id,
            expires_at,
            created_at,
            last_used_at,
            revoked_at,
          })
        },
      )
      .collect()
  }

  async fn revoke_api_key(&self, org_id: OrgId, key_id: Uuid) -> Result<()> {
    // org scoping: an org can only revoke its own keys; already-revoked
    // is a success (idempotent 204).
    // One transaction: the key and every user token it minted go together
    // (tokens are only as trustworthy as their minting key).
    let revoked_at = now();
    let mut tx = self.pool.begin().await?;
    let result = sqlx::query(
      "UPDATE api_keys SET revoked_at = $1 \
             WHERE id = $2 AND org_id = $3 AND revoked_at IS NULL",
    )
    .bind(revoked_at)
    .bind(key_id)
    .bind(org_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
      "UPDATE api_keys SET revoked_at = $1 \
             WHERE minted_by_key_id = $2 AND org_id = $3 AND revoked_at IS NULL",
    )
    .bind(revoked_at)
    .bind(key_id)
    .bind(org_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if result.rows_affected() == 0 {
      // Distinguish unknown/not-owned (404) from already-revoked (ok).
      let known: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM api_keys WHERE id = $1")
        .bind(key_id)
        .fetch_optional(&self.pool)
        .await?;
      if known != Some(org_id) {
        return Err(RegistryError::NotFound(format!("api key {key_id}")));
      }
    }
    Ok(())
  }

  async fn find_api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>> {
    // One atomic statement: stamp `last_used_at` and return the live row
    // (revoked keys update nothing -> None; expired user tokens likewise
    // — expiry is part of liveness, same treatment as revocation).
    // RETURNING keeps read-back and stamp race-free.
    let row: Option<(
            Uuid,
            Uuid,
            Option<Uuid>,
            String,
            String,
            String,
            Option<String>,
            Option<DateTime<Utc>>,
            DateTime<Utc>,
            Option<DateTime<Utc>>,
            Option<DateTime<Utc>>,
        )> = sqlx::query_as(
            "UPDATE api_keys SET last_used_at = $1 WHERE key_hash = $2 AND revoked_at IS NULL \
             AND (expires_at IS NULL OR expires_at > $1) \
             RETURNING id, org_id, app_id, kind, name, key_hash, external_user_id, expires_at, created_at, last_used_at, revoked_at",
        )
        .bind(now())
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await?;
    let Some((
      id,
      org_id,
      app_id,
      kind,
      name,
      key_hash,
      external_user_id,
      expires_at,
      created_at,
      last_used_at,
      revoked_at,
    )) = row
    else {
      return Ok(None);
    };
    Ok(Some(ApiKey {
      id,
      org_id,
      app_id,
      kind: serde_json::from_value(Value::String(kind))?,
      name,
      key_hash,
      external_user_id,
      expires_at,
      created_at,
      last_used_at,
      revoked_at,
    }))
  }

  async fn claim_lease(
    &self,
    session: Uuid,
    runner_id: &str,
    invocation_id: Option<Uuid>,
    ttl: chrono::Duration,
  ) -> Result<Lease> {
    self
      .claim_lease_inner(session, runner_id, invocation_id, ttl, None)
      .await
  }

  async fn claim_lease_capped(
    &self,
    session: Uuid,
    runner_id: &str,
    invocation_id: Option<Uuid>,
    ttl: chrono::Duration,
    caps: &LeaseCaps,
  ) -> Result<Lease> {
    self
      .claim_lease_inner(session, runner_id, invocation_id, ttl, Some(caps))
      .await
  }

  async fn count_live_turns(&self, app_id: Uuid, on_endpoint: bool) -> Result<(u64, u64)> {
    count_live_turns_on(&self.pool, app_id, on_endpoint).await
  }

  async fn heartbeat_lease(&self, lease_id: Uuid, ttl: chrono::Duration) -> Result<Lease> {
    let now = now();
    let expires_at = now + ttl;
    let updated = sqlx::query(
      "UPDATE leases SET heartbeat_at = $1, expires_at = $2 \
             WHERE id = $3 AND released_at IS NULL",
    )
    .bind(now)
    .bind(expires_at)
    .bind(lease_id)
    .execute(&self.pool)
    .await?;
    if updated.rows_affected() == 0 {
      return Err(RegistryError::Conflict(format!(
        "lease {lease_id} is not live (released or never existed)"
      )));
    }
    sqlx::query_as::<_, LeaseRow>(
      "SELECT id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, \
             expires_at, released_at FROM leases WHERE id = $1",
    )
    .bind(lease_id)
    .fetch_optional(&self.pool)
    .await?
    .map(Self::lease)
    .ok_or_else(|| RegistryError::NotFound(format!("lease {lease_id}")))
  }

  async fn release_lease(&self, lease_id: Uuid) -> Result<()> {
    let span = tracing::info_span!("lease.release", lease_id = %lease_id);
    let _guard = span.enter();
    let updated =
      sqlx::query("UPDATE leases SET released_at = $1 WHERE id = $2 AND released_at IS NULL")
        .bind(now())
        .bind(lease_id)
        .execute(&self.pool)
        .await?;
    if updated.rows_affected() == 0 {
      return Err(RegistryError::NotFound(format!("live lease {lease_id}")));
    }
    Ok(())
  }

  async fn reclaim_expired_lease(&self, lease_id: Uuid) -> Result<Option<Lease>> {
    let span = tracing::info_span!(
        "lease.reclaim",
        lease_id = %lease_id,
        session_id = tracing::field::Empty,
        reclaimed = tracing::field::Empty,
    );
    let _guard = span.enter();
    let row = sqlx::query_as::<_, LeaseRow>(
            "UPDATE leases SET released_at = $1              WHERE id = $2 AND released_at IS NULL AND expires_at <= $1              RETURNING id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at,                        expires_at, released_at",
        )
        .bind(now())
        .bind(lease_id)
        .fetch_optional(&self.pool)
        .await?;
    // pg's LeaseRow is a tuple: (id, session_id, runner_id, ...)
    match &row {
      Some(lease) => {
        span.record("session_id", lease.1.to_string());
        span.record("reclaimed", true);
      }
      None => {
        span.record("reclaimed", false);
      }
    }
    Ok(row.map(Self::lease))
  }

  async fn set_invocation(&self, lease_id: Uuid, invocation: Option<Uuid>) -> Result<Lease> {
    let updated =
      sqlx::query("UPDATE leases SET invocation_id = $1 WHERE id = $2 AND released_at IS NULL")
        .bind(invocation)
        .bind(lease_id)
        .execute(&self.pool)
        .await?;
    if updated.rows_affected() == 0 {
      return Err(RegistryError::NotFound(format!("live lease {lease_id}")));
    }
    sqlx::query_as::<_, LeaseRow>(
      "SELECT id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, \
             expires_at, released_at FROM leases WHERE id = $1",
    )
    .bind(lease_id)
    .fetch_optional(&self.pool)
    .await?
    .map(Self::lease)
    .ok_or_else(|| RegistryError::NotFound(format!("lease {lease_id}")))
  }

  async fn expired_leases(&self) -> Result<Vec<Lease>> {
    let rows = sqlx::query_as::<_, LeaseRow>(
      "SELECT id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, \
             expires_at, released_at FROM leases \
             WHERE released_at IS NULL AND expires_at <= $1 ORDER BY expires_at",
    )
    .bind(now())
    .fetch_all(&self.pool)
    .await?;
    Ok(rows.into_iter().map(Self::lease).collect())
  }
}

/// Advisory-lock key (two-int form) serializing capped lease claims.
const LEASE_CAPS_LOCK: (i32, i32) = (0x5EC7_0075, 1);

/// `(live leases of app_id, live leases platform-wide)`.
async fn count_live_turns_on<'e, E>(
  executor: E,
  app_id: Uuid,
  on_endpoint: bool,
) -> Result<(u64, u64)>
where
  E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
  let (app, total): (i64, i64) = sqlx::query_as(
    "SELECT count(*) FILTER (WHERE s.app_id = $1), count(*) \
         FROM leases l JOIN sessions s ON s.id = l.session_id \
         JOIN deployments d ON d.id = s.deployment_id \
         WHERE l.released_at IS NULL AND l.expires_at > now() \
         AND (d.endpoint_url IS NOT NULL) = $2",
  )
  .bind(app_id)
  .bind(on_endpoint)
  .fetch_one(executor)
  .await?;
  Ok((app.max(0) as u64, total.max(0) as u64))
}

/// One claim = one transaction. `FOR UPDATE SKIP LOCKED` on the live-lease
/// row: SKIP keeps a concurrent claimant/superseder from being waited on —
/// when the row is locked, this claimant falls through to the INSERT and the
/// partial unique index (`idx_leases_session_live`) decides the race loudly
/// (unique violation mapped to `Conflict`). An expired row we do win the
/// lock on is superseded (released, then re-claimed).
async fn claim_tx(
  tx: &mut sqlx::PgConnection,
  session: Uuid,
  runner_id: &str,
  invocation_id: Option<Uuid>,
  ttl: chrono::Duration,
  caps: Option<&LeaseCaps>,
) -> Result<Lease> {
  // Capped claims serialize platform-wide on one advisory xact
  // lock (released at commit/rollback), so the count below sees every
  // earlier capped claim's committed lease and concurrent claims cannot
  // overshoot. Claims are once-per-turn; serializing them is free at beta
  // scale. Uncapped claims (caps unset) never touch the lock.
  if let Some(caps) = caps
    && caps.any()
  {
    // The two-int form is a separate keyspace from the one-bigint form the
    // journal's per-session locks use (`hashtextextended`), so the two can
    // never collide.
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
      .bind(LEASE_CAPS_LOCK.0)
      .bind(LEASE_CAPS_LOCK.1)
      .execute(&mut *tx)
      .await?;
    let (app_live, total_live) =
      count_live_turns_on(&mut *tx, caps.app_id, caps.on_endpoint).await?;
    if let Some((scope, limit)) = caps.exceeded(app_live, total_live) {
      return Err(RegistryError::CapExceeded { scope, limit });
    }
  }

  let locked: Option<(Uuid, DateTime<Utc>, String)> = sqlx::query_as(
    "SELECT id, expires_at, runner_id FROM leases \
         WHERE session_id = $1 AND released_at IS NULL \
         FOR UPDATE SKIP LOCKED",
  )
  .bind(session)
  .fetch_optional(&mut *tx)
  .await?;

  if let Some((lease_id, expires_at, holder)) = locked {
    // Re-check under the lock — expiry may have changed since the
    // pre-check, and a live lease still conflicts.
    if expires_at > now() {
      return Err(RegistryError::Conflict(format!(
        "session {session} already has a live lease (runner {holder}) until {expires_at}"
      )));
    }
    // Expired: supersede.
    sqlx::query("UPDATE leases SET released_at = $1 WHERE id = $2")
      .bind(now())
      .bind(lease_id)
      .execute(&mut *tx)
      .await?;
  }

  let id = Uuid::now_v7();
  let now = now();
  let expires_at = now + ttl;
  sqlx::query(
        "INSERT INTO leases (id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(session)
    .bind(runner_id)
    .bind(invocation_id)
    .bind(now)
    .bind(now)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;

  Ok(Lease {
    id,
    session_id: session,
    runner_id: runner_id.into(),
    invocation_id,
    claimed_at: now,
    heartbeat_at: now,
    expires_at,
    released_at: None,
  })
}

// --------------------------------------------------------------------------
// Clerk signup helpers (PgRegistry ONLY — deliberately NOT on the
// `Registry` trait; signup is a prod-Postgres-only surface and the SQLite dev
// store has no use for it). They replaced the alpha allowlist helpers.
// --------------------------------------------------------------------------

impl PgRegistry {
  /// A returning Clerk user: `(user_id, org_id)` for the Clerk `sub`.
  pub async fn find_user_by_clerk_id(&self, sub: &str) -> Result<Option<(Uuid, OrgId)>> {
    Ok(
      sqlx::query_as("SELECT id, org_id FROM users WHERE clerk_user_id = $1")
        .bind(sub)
        .fetch_optional(&self.pool)
        .await?,
    )
  }

  /// Total orgs — the guardrails' signup cap counts orgs (org-per-person; the
  /// Clerk exchange is the only path that creates them). Inherent like
  /// the other Clerk helpers: signups are prod-Postgres-only.
  pub async fn count_orgs(&self) -> Result<u64> {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM orgs")
      .fetch_one(&self.pool)
      .await?;
    Ok(n.max(0) as u64)
  }

  /// Create a brand-new Clerk account — the org AND its user row (email,
  /// name, Clerk id) — in ONE transaction, so a failure between the two
  /// can never leave an org without a user.
  ///
  /// The org gets `slug`, or `fallback_slug` when `slug` is taken (distinct
  /// emails can derive the same slug). Conflicts are detected with
  /// `ON CONFLICT DO NOTHING`, never a failed statement, because any error
  /// aborts a Postgres transaction. Returns `Conflict` — with nothing
  /// written — when this Clerk id already has a user row (a concurrent
  /// first sign-in committed first; the caller re-resolves it as a
  /// returning user) or when both slugs are taken.
  pub async fn create_clerk_account(
    &self,
    slug: &str,
    fallback_slug: &str,
    email: &str,
    name: Option<&str>,
    sub: &str,
  ) -> Result<Org> {
    let org_id = Uuid::now_v7();
    let created_at = now();
    let mut tx = self.pool.begin().await?;

    let mut chosen = None;
    for candidate in [slug, fallback_slug] {
      let inserted = sqlx::query(
        "INSERT INTO orgs (id, name, slug, created_at) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (slug) DO NOTHING",
      )
      .bind(org_id)
      .bind(candidate)
      .bind(candidate)
      .bind(created_at)
      .execute(&mut *tx)
      .await?;
      if inserted.rows_affected() == 1 {
        chosen = Some(candidate);
        break;
      }
    }
    let Some(chosen) = chosen else {
      return Err(RegistryError::Conflict(format!(
        "org slugs {slug} and {fallback_slug} are both taken"
      )));
    };

    let inserted = sqlx::query(
      "INSERT INTO users (id, org_id, email, name, clerk_user_id, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (clerk_user_id) WHERE clerk_user_id IS NOT NULL DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(org_id)
    .bind(email)
    .bind(name)
    .bind(sub)
    .bind(created_at)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
      // Dropping `tx` rolls the org back too.
      return Err(RegistryError::Conflict(format!(
        "clerk user {sub} already has an account"
      )));
    }

    tx.commit().await?;
    Ok(Org {
      id: org_id,
      name: chosen.into(),
      slug: chosen.into(),
      created_at,
    })
  }

  /// [`Registry::create_api_key`] with an optional hard expiry — the
  /// dashboard-session platform keys (`expires_at` is enforced for every
  /// key kind at lookup; the DDL CHECK allows it on platform keys).
  pub async fn create_api_key_expiring(
    &self,
    org_id: OrgId,
    kind: ApiKeyKind,
    name: &str,
    key_hash: &str,
    expires_at: Option<DateTime<Utc>>,
  ) -> Result<ApiKey> {
    let id = Uuid::now_v7();
    let created_at = now();
    let kind_value = serde_json::to_value(kind)?;
    let kind_str = kind_value.as_str().expect("unit enum serializes to string");
    sqlx::query(
      "INSERT INTO api_keys (id, org_id, kind, name, key_hash, expires_at, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(org_id)
    .bind(kind_str)
    .bind(name)
    .bind(key_hash)
    .bind(expires_at)
    .bind(created_at)
    .execute(&self.pool)
    .await?;
    Ok(ApiKey {
      id,
      org_id,
      app_id: None,
      kind,
      name: name.into(),
      key_hash: key_hash.into(),
      external_user_id: None,
      expires_at,
      created_at,
      last_used_at: None,
      revoked_at: None,
    })
  }
}

// --------------------------------------------------------------------------
// Browser login (device flow) for `sectr auth login`. PgRegistry
// ONLY, like the Clerk helpers above. The device code never reaches the
// store in plaintext (callers pass its hash); the minted key is created
// by the consuming poll, never by the approval.
// --------------------------------------------------------------------------

/// A pending-or-decided CLI login request, as the approval page sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliLoginRequest {
  pub user_code: String,
  pub client_name: String,
  pub status: String,
  pub created_at: DateTime<Utc>,
  pub expires_at: DateTime<Utc>,
}

/// Outcome of approving / denying a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliLoginDecision {
  Done,
  /// Unknown or expired.
  NotFound,
  /// Already decided or collected; carries the current status.
  NotPending(String),
}

/// Outcome of the CLI's poll.
#[derive(Debug, Clone, PartialEq)]
pub enum CliLoginPoll {
  Pending,
  Denied,
  /// Unknown, consumed or past-TTL: deliberately indistinguishable.
  Expired,
  /// The consuming call: the key row now exists.
  Approved {
    key_id: Uuid,
    org: Org,
  },
}

impl PgRegistry {
  /// Insert a pending request expiring `ttl_secs` from now (negative =
  /// already expired; tests inject the TTL). `Ok(false)` when `user_code`
  /// collides (the caller retries once with a new code).
  pub async fn create_cli_login(
    &self,
    device_code_hash: &str,
    user_code: &str,
    client_name: &str,
    ttl_secs: i64,
  ) -> Result<bool> {
    let inserted = sqlx::query(
      "INSERT INTO cli_logins (id, device_code_hash, user_code, client_name, expires_at) \
             VALUES ($1, $2, $3, $4, now() + $5 * interval '1 second') \
             ON CONFLICT DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(device_code_hash)
    .bind(user_code)
    .bind(client_name)
    .bind(ttl_secs as f64)
    .execute(&self.pool)
    .await?;
    Ok(inserted.rows_affected() == 1)
  }

  /// The unexpired request with this (normalized) user code.
  pub async fn get_cli_login(&self, user_code: &str) -> Result<Option<CliLoginRequest>> {
    #[allow(clippy::type_complexity)] // one-off row tuple
    let row: Option<(String, String, String, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
      "SELECT user_code, client_name, status, created_at, expires_at FROM cli_logins \
             WHERE user_code = $1 AND expires_at > now()",
    )
    .bind(user_code)
    .fetch_optional(&self.pool)
    .await?;
    Ok(row.map(
      |(user_code, client_name, status, created_at, expires_at)| CliLoginRequest {
        user_code,
        client_name,
        status,
        created_at,
        expires_at,
      },
    ))
  }

  /// Approve (`Some((org, approving key))`) or deny (`None`) a pending,
  /// unexpired request. Approval only records who approved; no key exists
  /// until the CLI collects it.
  pub async fn decide_cli_login(
    &self,
    user_code: &str,
    approval: Option<(OrgId, Uuid)>,
  ) -> Result<CliLoginDecision> {
    let (status, org_id, approved_by) = match approval {
      Some((org, key)) => ("approved", Some(org), Some(key)),
      None => ("denied", None, None),
    };
    let updated = sqlx::query(
      "UPDATE cli_logins SET status = $2, org_id = $3, approved_by = $4 \
             WHERE user_code = $1 AND status = 'pending' AND expires_at > now()",
    )
    .bind(user_code)
    .bind(status)
    .bind(org_id)
    .bind(approved_by)
    .execute(&self.pool)
    .await?;
    if updated.rows_affected() == 1 {
      return Ok(CliLoginDecision::Done);
    }
    Ok(match self.get_cli_login(user_code).await? {
      Some(req) => CliLoginDecision::NotPending(req.status),
      None => CliLoginDecision::NotFound,
    })
  }

  /// The CLI's poll. When the request is approved and live, atomically
  /// marks it `consumed` and inserts the platform key (`key_hash`, named
  /// `CLI (<client_name>)`, no expiry) in one transaction: concurrent
  /// polls can mint at most one key.
  pub async fn poll_cli_login(
    &self,
    device_code_hash: &str,
    key_hash: &str,
  ) -> Result<CliLoginPoll> {
    let mut tx = self.pool.begin().await?;
    let key_id = Uuid::now_v7();
    let consumed: Option<(OrgId, String)> = sqlx::query_as(
      "UPDATE cli_logins SET status = 'consumed' \
             WHERE device_code_hash = $1 AND status = 'approved' AND expires_at > now() \
             RETURNING org_id, client_name",
    )
    .bind(device_code_hash)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((org_id, client_name)) = consumed {
      sqlx::query(
        "INSERT INTO api_keys (id, org_id, kind, name, key_hash, created_at) \
                 VALUES ($1, $2, 'platform', $3, $4, $5)",
      )
      .bind(key_id)
      .bind(org_id)
      .bind(format!("CLI ({client_name})"))
      .bind(key_hash)
      .bind(now())
      .execute(&mut *tx)
      .await?;
      sqlx::query("UPDATE cli_logins SET key_id = $2 WHERE device_code_hash = $1")
        .bind(device_code_hash)
        .bind(key_id)
        .execute(&mut *tx)
        .await?;
      tx.commit().await?;
      let org = self.get_org(org_id).await?;
      return Ok(CliLoginPoll::Approved { key_id, org });
    }
    tx.rollback().await?;

    let status: Option<String> = sqlx::query_scalar(
      "SELECT status FROM cli_logins WHERE device_code_hash = $1 AND expires_at > now()",
    )
    .bind(device_code_hash)
    .fetch_optional(&self.pool)
    .await?;
    Ok(match status.as_deref() {
      Some("pending") => CliLoginPoll::Pending,
      Some("denied") => CliLoginPoll::Denied,
      _ => CliLoginPoll::Expired,
    })
  }

  /// Delete requests that expired before `expired_before`. Never touches
  /// the keys they minted (`key_id` points from the request to the key).
  pub async fn purge_cli_logins(&self, expired_before: DateTime<Utc>) -> Result<u64> {
    Ok(
      sqlx::query("DELETE FROM cli_logins WHERE expires_at < $1")
        .bind(expired_before)
        .execute(&self.pool)
        .await?
        .rows_affected(),
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::registry::deployment_serves_route;
  use crate::store::PgStore;
  use crate::test_pg::fresh_store;
  use serde_json::json;
  use std::time::Duration;

  /// `None` = skip (no test Postgres configured).
  async fn registry() -> Option<(PgRegistry, Org, App, Deployment)> {
    let store: PgStore = fresh_store().await?;
    let registry = store.registry();
    let org = registry.create_org("test-org", "test-org").await.unwrap();
    let app = registry
      .create_app(NewApp {
        org_id: org.id,
        name: "test-app".into(),
        slug: "test-app".into(),
      })
      .await
      .unwrap();
    let deployment = registry
      .create_deployment(NewDeployment {
        endpoint_url: None,
        app_id: app.id,
        version: 1,
        manifest: json!({"routes": [{"path": "/test"}]}),
      })
      .await
      .unwrap();
    Some((registry, org, app, deployment))
  }

  #[tokio::test]
  async fn org_and_app_round_trip() {
    let Some((registry, org, app, _deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    assert_eq!(registry.get_org(org.id).await.unwrap(), org);
    assert_eq!(registry.get_app(app.id).await.unwrap(), app);
    assert!(matches!(
      registry.get_app(Uuid::now_v7()).await.unwrap_err(),
      RegistryError::NotFound(_)
    ));
  }

  #[tokio::test]
  async fn create_deployment_returns_store_projection() {
    let Some((registry, _org, app, _deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let created = registry
      .create_deployment(NewDeployment {
        endpoint_url: None,
        app_id: app.id,
        version: 7,
        manifest: json!({"routes": [{"path": "/roundtrip"}]}),
      })
      .await
      .unwrap();
    assert_eq!(created.status, DeploymentStatus::Building);
    assert_eq!(created.artifact_uri, None);
    assert_eq!(created.build_ref, None);
    // Field-identical to a fresh read — every field, including the
    // created_at round-trip through the store.
    assert_eq!(
      registry.get_deployment(created.id).await.unwrap(),
      created,
      "create_deployment must return the store's row, not a pre-insert guess"
    );
  }

  #[tokio::test]
  async fn deployment_lifecycle_and_active_lookup() {
    let Some((registry, _org, app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    assert_eq!(registry.active_deployment(app.id).await.unwrap(), None);
    registry
      .set_deployment_status(deployment.id, DeploymentStatus::Active)
      .await
      .unwrap();
    let v2 = registry
      .create_deployment(NewDeployment {
        endpoint_url: None,
        app_id: app.id,
        version: 2,
        manifest: json!({"routes": [{"path": "/test"}]}),
      })
      .await
      .unwrap();
    assert_eq!(
      registry
        .active_deployment(app.id)
        .await
        .unwrap()
        .unwrap()
        .id,
      deployment.id
    );
    registry
      .set_deployment_status(v2.id, DeploymentStatus::Active)
      .await
      .unwrap();
    assert_eq!(
      registry
        .active_deployment(app.id)
        .await
        .unwrap()
        .unwrap()
        .id,
      v2.id
    );

    // Retire v2 -> falls back to v1.
    registry
      .set_deployment_status(v2.id, DeploymentStatus::Retired)
      .await
      .unwrap();
    assert_eq!(
      registry
        .active_deployment(app.id)
        .await
        .unwrap()
        .unwrap()
        .id,
      deployment.id
    );

    registry
      .set_artifact_uri(deployment.id, "file:///tmp/app.tgz")
      .await
      .unwrap();
    assert_eq!(
      registry
        .get_deployment(deployment.id)
        .await
        .unwrap()
        .artifact_uri,
      Some("file:///tmp/app.tgz".into())
    );

    // History: newest version first, all statuses included.
    let listed = registry.list_deployments(app.id).await.unwrap();
    let versions: Vec<i64> = listed.iter().map(|d| d.version).collect();
    assert_eq!(versions, vec![2, 1], "newest first");
    assert_eq!(listed[0].status, DeploymentStatus::Retired);
    assert_eq!(listed[1].artifact_uri, Some("file:///tmp/app.tgz".into()));
  }

  #[tokio::test]
  async fn deployment_build_ref_set_overwrite_clear() {
    let Some((registry, _org, _app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    // Fresh rows carry no build_ref.
    assert_eq!(deployment.build_ref, None);
    assert_eq!(
      registry
        .get_deployment(deployment.id)
        .await
        .unwrap()
        .build_ref,
      None
    );

    // Set, overwrite, clear — the read-back goes through get_deployment,
    // so the SELECT projections are covered too.
    registry
      .set_deployment_build_ref(deployment.id, Some("op-abc-123"))
      .await
      .unwrap();
    assert_eq!(
      registry
        .get_deployment(deployment.id)
        .await
        .unwrap()
        .build_ref,
      Some("op-abc-123".into())
    );

    registry
      .set_deployment_build_ref(deployment.id, Some("op-def-456"))
      .await
      .unwrap();
    assert_eq!(
      registry
        .get_deployment(deployment.id)
        .await
        .unwrap()
        .build_ref,
      Some("op-def-456".into())
    );

    // Clearing is a definite statement (build resolved or not started).
    registry
      .set_deployment_build_ref(deployment.id, None)
      .await
      .unwrap();
    assert_eq!(
      registry
        .get_deployment(deployment.id)
        .await
        .unwrap()
        .build_ref,
      None
    );
  }

  #[tokio::test]
  async fn deployment_build_error_set_and_clear() {
    let Some((registry, _org, _app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    // Fresh rows carry no error.
    assert_eq!(deployment.error, None);

    // A failed build stores its reason; the read-back goes through
    // get_deployment, so the SELECT projections are covered too.
    registry
      .set_deployment_build_error(deployment.id, Some("import failed (exit 1): Traceback …"))
      .await
      .unwrap();
    assert_eq!(
      registry.get_deployment(deployment.id).await.unwrap().error,
      Some("import failed (exit 1): Traceback …".into())
    );
    // The setter mirrors set_deployment_build_ref: list + active
    // projections carry the column as well.
    assert!(
      registry.list_deployments(deployment.app_id).await.unwrap()[0]
        .error
        .is_some()
    );

    // Clearing is a definite statement (the retry build succeeded).
    registry
      .set_deployment_build_error(deployment.id, None)
      .await
      .unwrap();
    assert_eq!(
      registry.get_deployment(deployment.id).await.unwrap().error,
      None
    );
  }

  #[tokio::test]
  async fn session_lifecycle() {
    let Some((registry, _org, app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let session = registry
      .create_session(NewSession {
        id: None,
        app_id: app.id,
        deployment_id: deployment.id,
        route: "/test".into(),

        origin: crate::api::Origin::Playground,
        external_user_id: None,
      })
      .await
      .unwrap();
    assert_eq!(session.status, SessionStatus::Active);
    assert_eq!(session.deployment_id, deployment.id);
    assert_eq!(registry.get_session(session.id).await.unwrap(), session);

    registry
      .set_session_status(session.id, SessionStatus::AwaitingApproval)
      .await
      .unwrap();
    assert_eq!(
      registry.get_session(session.id).await.unwrap().status,
      SessionStatus::AwaitingApproval
    );
    registry
      .set_session_status(session.id, SessionStatus::Ended)
      .await
      .unwrap();
    let ended = registry.get_session(session.id).await.unwrap();
    assert_eq!(ended.status, SessionStatus::Ended);
    assert!(ended.ended_at.is_some());
    assert!(ended.updated_at.is_some());
  }

  #[tokio::test]
  async fn secrets_upsert_resolve_delete() {
    let Some((registry, _org, app, _deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    registry
      .set_secret(app.id, "OPENAI_API_KEY", Secret::new("sk-1"))
      .await
      .unwrap();
    registry
      .set_secret(app.id, "OPENAI_API_KEY", Secret::new("sk-2"))
      .await
      .unwrap();
    registry
      .set_secret(app.id, "OTHER", Secret::new("x"))
      .await
      .unwrap();

    let mut resolved = registry.resolve_secrets(app.id).await.unwrap();
    resolved.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
      resolved,
      vec![
        ("OPENAI_API_KEY".to_string(), Secret::new("sk-2")),
        ("OTHER".to_string(), Secret::new("x")),
      ]
    );
    assert!(!format!("{resolved:?}").contains("sk-2"));
    // An overwrite stamps updated_at; a first write leaves it unset.
    let metas = registry.list_secret_names(app.id).await.unwrap();
    assert_eq!(metas[0].key, "OPENAI_API_KEY");
    assert!(metas[0].updated_at.is_some());
    assert_eq!(metas[1].key, "OTHER");
    assert!(metas[1].updated_at.is_none());

    registry.delete_secret(app.id, "OTHER").await.unwrap();
    registry.delete_secret(app.id, "OTHER").await.unwrap();
    assert_eq!(registry.resolve_secrets(app.id).await.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn secret_names_list_metadata_only() {
    let Some((registry, _org, app, _deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    registry
      .set_secret(app.id, "B_KEY", Secret::new("v-b"))
      .await
      .unwrap();
    registry
      .set_secret(app.id, "A_KEY", Secret::new("v-a"))
      .await
      .unwrap();
    let names = registry.list_secret_names(app.id).await.unwrap();
    assert_eq!(
      names.iter().map(|m| m.key.as_str()).collect::<Vec<_>>(),
      vec!["A_KEY", "B_KEY"] // ordered by key
    );
    assert!(names[0].updated_at.is_none());
    // Upsert stamps updated_at.
    registry
      .set_secret(app.id, "A_KEY", Secret::new("v-a2"))
      .await
      .unwrap();
    let names = registry.list_secret_names(app.id).await.unwrap();
    assert!(
      names
        .iter()
        .find(|m| m.key == "A_KEY")
        .unwrap()
        .updated_at
        .is_some()
    );
    // Delete removes from the list.
    registry.delete_secret(app.id, "A_KEY").await.unwrap();
    let names = registry.list_secret_names(app.id).await.unwrap();
    assert_eq!(names.len(), 1);
  }

  #[tokio::test]
  async fn lease_invocation_binding() {
    let Some((registry, _org, app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let session = registry
      .create_session(NewSession {
        id: None,
        app_id: app.id,
        deployment_id: deployment.id,
        route: "/test".into(),

        origin: crate::api::Origin::Playground,
        external_user_id: None,
      })
      .await
      .unwrap();

    let lease = registry
      .claim_lease(session.id, "runner-1", None, chrono::Duration::seconds(30))
      .await
      .unwrap();
    assert_eq!(lease.invocation_id, None);

    let invocation = Uuid::now_v7();
    let bound = registry
      .set_invocation(lease.id, Some(invocation))
      .await
      .unwrap();
    assert_eq!(bound.invocation_id, Some(invocation));

    let cleared = registry.set_invocation(lease.id, None).await.unwrap();
    assert_eq!(cleared.invocation_id, None);

    registry.release_lease(lease.id).await.unwrap();
    let err = registry
      .set_invocation(lease.id, Some(invocation))
      .await
      .unwrap_err();
    assert!(matches!(err, RegistryError::NotFound(_)));
  }

  #[tokio::test]
  async fn lease_claim_heartbeat_release() {
    let Some((registry, _org, app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let session = registry
      .create_session(NewSession {
        id: None,
        app_id: app.id,
        deployment_id: deployment.id,
        route: "/test".into(),

        origin: crate::api::Origin::Playground,
        external_user_id: None,
      })
      .await
      .unwrap();

    let ttl = chrono::Duration::seconds(30);
    let lease = registry
      .claim_lease(session.id, "runner-1", None, ttl)
      .await
      .unwrap();
    assert_eq!(lease.session_id, session.id);
    assert!(lease.released_at.is_none());

    // Second claim while live conflicts.
    let err = registry
      .claim_lease(session.id, "runner-2", None, ttl)
      .await
      .unwrap_err();
    assert!(
      matches!(err, RegistryError::Conflict(ref m) if m.contains("already has a live lease")),
      "expected live-lease conflict, got {err:?}"
    );

    // Heartbeat extends.
    let heartbeated = registry.heartbeat_lease(lease.id, ttl).await.unwrap();
    assert!(heartbeated.expires_at > lease.expires_at);

    // Release, then reclaim works.
    registry.release_lease(lease.id).await.unwrap();
    let again = registry
      .claim_lease(session.id, "runner-2", None, ttl)
      .await
      .unwrap();
    assert_ne!(again.id, lease.id);

    // Heartbeat on a released lease fails.
    assert!(matches!(
      registry.heartbeat_lease(lease.id, ttl).await.unwrap_err(),
      RegistryError::Conflict(_)
    ));
  }

  #[tokio::test]
  async fn expired_lease_is_superseded() {
    let Some((registry, _org, app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let session = registry
      .create_session(NewSession {
        id: None,
        app_id: app.id,
        deployment_id: deployment.id,
        route: "/test".into(),

        origin: crate::api::Origin::Playground,
        external_user_id: None,
      })
      .await
      .unwrap();

    let lease = registry
      .claim_lease(session.id, "runner-1", None, chrono::Duration::zero())
      .await
      .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;

    // Expired lease does not conflict — the new claim supersedes it.
    let next = registry
      .claim_lease(session.id, "runner-2", None, chrono::Duration::seconds(30))
      .await
      .unwrap();
    assert_ne!(next.id, lease.id);

    let expired = registry.expired_leases().await.unwrap();
    assert!(expired.is_empty());

    // A zero-TTL claim shows up in the scan; a heartbeat revives it.
    registry.release_lease(next.id).await.unwrap();
    let dying = registry
      .claim_lease(session.id, "runner-3", None, chrono::Duration::zero())
      .await
      .unwrap();
    let scanned = registry.expired_leases().await.unwrap();
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].id, dying.id);
    assert!(scanned[0].released_at.is_none());

    registry
      .heartbeat_lease(dying.id, chrono::Duration::seconds(30))
      .await
      .unwrap();
    assert!(registry.expired_leases().await.unwrap().is_empty());
  }

  /// Concurrent claims for the same session: exactly one wins, the losers
  /// get a loud conflict (the partial unique index decides).
  #[tokio::test]
  async fn api_key_lifecycle_and_auth_lookup() {
    let Some((registry, org, app, _deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    let platform_hash = "hash-platform";
    let created = registry
      .create_api_key(org.id, None, ApiKeyKind::Platform, "cli", platform_hash)
      .await
      .unwrap();
    assert_eq!(created.kind, ApiKeyKind::Platform);
    assert_eq!(created.app_id, None);
    assert_eq!(created.key_hash, platform_hash);
    assert!(created.revoked_at.is_none());

    let invocation = registry
      .create_api_key(
        org.id,
        Some(app.id),
        ApiKeyKind::Invocation,
        "end-clients",
        "hash-invocation",
      )
      .await
      .unwrap();
    assert_eq!(invocation.kind, ApiKeyKind::Invocation);
    assert_eq!(invocation.app_id, Some(app.id));

    // List is org-scoped, newest first.
    let listed = registry.list_api_keys(org.id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, invocation.id);
    // Another org sees nothing.
    let other = registry.create_org("other", "other").await.unwrap();
    assert!(registry.list_api_keys(other.id).await.unwrap().is_empty());

    // Auth lookup by hash works and stamps last_used_at.
    let found = registry
      .find_api_key_by_hash(platform_hash)
      .await
      .unwrap()
      .unwrap();
    assert_eq!(found.id, created.id);
    assert!(found.last_used_at.is_some());
    assert!(
      registry
        .find_api_key_by_hash("hash-unknown")
        .await
        .unwrap()
        .is_none()
    );

    // Revoke: org-scoped, idempotent, and revoked keys vanish from auth.
    registry.revoke_api_key(org.id, created.id).await.unwrap();
    registry.revoke_api_key(org.id, created.id).await.unwrap(); // 204 again
    assert!(
      registry
        .find_api_key_by_hash(platform_hash)
        .await
        .unwrap()
        .is_none()
    );
    // Cross-org revoke is NotFound (other org doesn't own it).
    assert!(matches!(
      registry
        .revoke_api_key(other.id, invocation.id)
        .await
        .unwrap_err(),
      RegistryError::NotFound(_)
    ));
    assert!(matches!(
      registry
        .revoke_api_key(org.id, Uuid::now_v7())
        .await
        .unwrap_err(),
      RegistryError::NotFound(_)
    ));
  }

  #[tokio::test]
  async fn concurrent_lease_claims_have_exactly_one_winner() {
    let Some((registry, _org, app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let session = registry
      .create_session(NewSession {
        id: None,
        app_id: app.id,
        deployment_id: deployment.id,
        route: "/test".into(),

        origin: crate::api::Origin::Playground,
        external_user_id: None,
      })
      .await
      .unwrap();

    let claims: Vec<_> = (0..8)
      .map(|i| {
        let registry = registry.clone();
        tokio::spawn(async move {
          (
            i,
            registry
              .claim_lease(
                session.id,
                &format!("runner-{i}"),
                None,
                chrono::Duration::seconds(30),
              )
              .await,
          )
        })
      })
      .collect();

    let mut winners = 0;
    let mut losers = 0;
    for claim in claims {
      let (_, result) = claim.await.unwrap();
      match result {
        Ok(_) => winners += 1,
        Err(RegistryError::Conflict(_)) => losers += 1,
        Err(err) => panic!("unexpected error: {err:?}"),
      }
    }
    assert_eq!(winners, 1, "exactly one claim wins");
    assert_eq!(losers, 7, "every loser conflicts loudly");
  }

  #[tokio::test]
  async fn lease_cap_per_app() {
    let Some((registry, org, ..)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    crate::registry::cap_suite::per_app_cap(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_global_across_apps() {
    let Some((registry, org, ..)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    crate::registry::cap_suite::global_cap_across_apps(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_ignores_expired_and_released() {
    let Some((registry, org, ..)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    crate::registry::cap_suite::expired_and_released_leases_do_not_count(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_counts_kinds_apart() {
    let Some((registry, org, ..)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    crate::registry::cap_suite::kinds_are_counted_apart(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_uncapped_and_conflict() {
    let Some((registry, org, ..)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    crate::registry::cap_suite::uncapped_and_conflict_semantics(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_concurrent_claims_respect_cap() {
    let Some((registry, org, ..)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    crate::registry::cap_suite::concurrent_claims_respect_cap(
      std::sync::Arc::new(registry),
      org.id,
    )
    .await;
  }

  #[tokio::test]
  async fn delete_unstarted_session_only_removes_unstarted() {
    let Some(store) = fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let registry = store.registry();
    let org = registry.create_org("test-org", "test-org").await.unwrap();
    crate::registry::cap_suite::delete_unstarted_session_only_removes_unstarted(
      &registry,
      &store.journal(),
      org.id,
    )
    .await;
  }

  /// A lease claim that commits while the delete waits on it keeps the
  /// session (and the lease): the delete must not judge "never held a
  /// lease" from a snapshot taken before the claim committed.
  #[tokio::test]
  async fn delete_unstarted_session_keeps_a_session_whose_claim_commits_meanwhile() {
    let Some(store) = fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let registry = store.registry();
    let org = registry.create_org("test-org", "test-org").await.unwrap();
    let (app, deployment) =
      crate::registry::cap_suite::app_with_deployment(&registry, org.id, "race").await;
    let session = crate::registry::cap_suite::session_of(&registry, &app, &deployment).await;

    // The claim: its lease inserted, not yet committed.
    let mut claim = registry.pool.begin().await.unwrap();
    let lease_id = Uuid::now_v7();
    sqlx::query(
      "INSERT INTO leases (id, session_id, runner_id, expires_at) \
             VALUES ($1, $2, 'test', now() + interval '30 seconds')",
    )
    .bind(lease_id)
    .bind(session.id)
    .execute(&mut *claim)
    .await
    .unwrap();

    let deleting = tokio::spawn({
      let registry = registry.clone();
      async move { registry.delete_unstarted_session(session.id).await }
    });
    // Commit the claim only once the delete is waiting on its lock.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
      let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE NOT granted")
        .fetch_one(&registry.pool)
        .await
        .unwrap();
      if waiting > 0 {
        break;
      }
      assert!(
        tokio::time::Instant::now() < deadline,
        "the delete never waited on the claim's lock"
      );
      tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    claim.commit().await.unwrap();

    assert!(!deleting.await.unwrap().unwrap());
    registry.get_session(session.id).await.unwrap();
    let kept: Option<Uuid> = sqlx::query_scalar("SELECT id FROM leases WHERE id = $1")
      .bind(lease_id)
      .fetch_optional(&registry.pool)
      .await
      .unwrap();
    assert_eq!(kept, Some(lease_id));
  }

  #[tokio::test]
  async fn list_sessions_filters_and_paginates() {
    let Some((registry, _org, app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let other_app = {
      let org = registry.create_org("other", "other").await.unwrap();
      registry
        .create_app(NewApp {
          org_id: org.id,
          name: "other-app".into(),
          slug: "other-app".into(),
        })
        .await
        .unwrap()
    };

    let mut ids = Vec::new();
    for i in 0..5 {
      let app_id = if i == 4 { other_app.id } else { app.id };
      let session = registry
        .create_session(NewSession {
          id: None,
          app_id,
          deployment_id: deployment.id,
          route: "/test".into(),

          origin: crate::api::Origin::Playground,
          external_user_id: None,
        })
        .await
        .unwrap();
      // created_at has microsecond precision; force distinct ordering.
      tokio::time::sleep(Duration::from_millis(2)).await;
      ids.push(session.id);
    }

    let all = registry
      .list_sessions(None, None, None, 100, None)
      .await
      .unwrap();
    assert_eq!(all.len(), 5);
    assert!(all.windows(2).all(|w| w[0].created_at >= w[1].created_at));

    let by_app = registry
      .list_sessions(None, Some(app.id), None, 100, None)
      .await
      .unwrap();
    assert_eq!(by_app.len(), 4);

    let page = registry
      .list_sessions(None, None, None, 2, None)
      .await
      .unwrap();
    assert_eq!(page.len(), 2);

    // Keyset: `created_before` = rows strictly older than the given
    // timestamp. Feed it the 2nd-newest session's created_at -> the
    // remaining 3 come back, all strictly older.
    let all = registry
      .list_sessions(None, None, None, 100, None)
      .await
      .unwrap();
    let second_newest = all[1].created_at;
    let older = registry
      .list_sessions(None, None, None, 100, Some(second_newest))
      .await
      .unwrap();
    assert_eq!(older.len(), 3);
    assert!(older.iter().all(|s| s.created_at < second_newest));
  }

  #[tokio::test]
  async fn user_token_lifecycle_expiry_and_owner_filter() {
    let Some((registry, org, app, deployment)) = registry().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    // The minting credential: an invocation key for the app.
    let minter = registry
      .create_api_key(
        org.id,
        Some(app.id),
        ApiKeyKind::Invocation,
        "backend",
        "hash-minter",
      )
      .await
      .unwrap();

    // Mint (hash-only, like every key class) + row shape.
    let token = registry
      .create_user_token(
        org.id,
        app.id,
        minter.id,
        "user_1",
        "hash-user-1",
        Utc::now() + chrono::Duration::hours(1),
      )
      .await
      .unwrap();
    assert_eq!(token.kind, ApiKeyKind::User);
    assert_eq!(token.app_id, Some(app.id));
    assert_eq!(token.external_user_id.as_deref(), Some("user_1"));
    assert!(token.expires_at.is_some());
    assert!(token.revoked_at.is_none());

    // Live lookup.
    let found = registry
      .find_api_key_by_hash("hash-user-1")
      .await
      .unwrap()
      .unwrap();
    assert_eq!(found.id, token.id);
    assert_eq!(found.external_user_id.as_deref(), Some("user_1"));

    // An EXPIRED token authenticates as nonexistent (same as revoked).
    let expired = registry
      .create_user_token(
        org.id,
        app.id,
        minter.id,
        "user_2",
        "hash-user-2",
        Utc::now() - chrono::Duration::hours(1),
      )
      .await
      .unwrap();
    assert!(
      registry
        .find_api_key_by_hash("hash-user-2")
        .await
        .unwrap()
        .is_none()
    );

    // The DDL CHECK forbids 'user'-kind rows without identity + expiry —
    // the plain create_api_key path cannot mint user tokens.
    assert!(
      registry
        .create_api_key(org.id, Some(app.id), ApiKeyKind::User, "x", "hash-user-3")
        .await
        .is_err()
    );

    // list_api_keys excludes the ephemeral user rows (no management UI).
    let listed = registry.list_api_keys(org.id).await.unwrap();
    assert!(listed.iter().all(|k| k.kind != ApiKeyKind::User));

    // Liveness (read-only re-check used by open SSE streams): a live token
    // is live, an expired one is not.
    assert!(registry.api_key_is_live(token.id).await.unwrap());
    assert!(!registry.api_key_is_live(expired.id).await.unwrap());

    // The purge deletes only expired user tokens (here: the one expired
    // token) — the live token and the invocation key are untouched.
    assert_eq!(registry.purge_expired_keys(Utc::now()).await.unwrap(), 1);
    assert_eq!(
      registry.purge_expired_keys(Utc::now()).await.unwrap(),
      0,
      "idempotent"
    );
    assert!(registry.api_key_is_live(token.id).await.unwrap());
    assert!(registry.api_key_is_live(minter.id).await.unwrap());

    // Revoking the minting key revokes its live tokens…
    assert!(
      registry
        .find_api_key_by_hash("hash-user-1")
        .await
        .unwrap()
        .is_some()
    );
    registry.revoke_api_key(org.id, minter.id).await.unwrap();
    assert!(
      registry
        .find_api_key_by_hash("hash-user-1")
        .await
        .unwrap()
        .is_none(),
      "token must die with its minting key"
    );
    assert!(!registry.api_key_is_live(token.id).await.unwrap());
    // …and a mint racing the revoke cannot leave a live token behind.
    assert!(
      registry
        .create_user_token(
          org.id,
          app.id,
          minter.id,
          "user_1",
          "hash-user-late",
          Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .is_err()
    );

    // Owner filter on list_sessions: owned vs ownerless sessions.
    let owned = registry
      .create_session(NewSession {
        id: None,
        app_id: app.id,
        deployment_id: deployment.id,
        route: "/test".into(),
        origin: crate::api::Origin::Api,
        external_user_id: Some("user_1".into()),
      })
      .await
      .unwrap();
    let _ownerless = registry
      .create_session(NewSession {
        id: None,
        app_id: app.id,
        deployment_id: deployment.id,
        route: "/test".into(),
        origin: crate::api::Origin::Api,
        external_user_id: None,
      })
      .await
      .unwrap();

    let rows = registry
      .list_sessions(Some(org.id), Some(app.id), Some("user_1"), 100, None)
      .await
      .unwrap();
    assert_eq!(
      rows.iter().map(|s| s.id).collect::<Vec<_>>(),
      vec![owned.id]
    );
    assert_eq!(rows[0].external_user_id.as_deref(), Some("user_1"));

    // A user with no sessions sees nothing; ownerless rows match no owner.
    assert!(
      registry
        .list_sessions(Some(org.id), Some(app.id), Some("user_2"), 100, None)
        .await
        .unwrap()
        .is_empty()
    );
  }

  #[tokio::test]
  async fn route_matching_is_exact_against_manifest() {
    let manifest = json!({"routes": [{"path": "/customer-support"}]});
    assert!(deployment_serves_route(&manifest, "/customer-support"));
    assert!(!deployment_serves_route(&manifest, "/customer"));
    assert!(!deployment_serves_route(
      &manifest,
      "/customer-support/extra"
    ));
    assert!(!deployment_serves_route(&json!({}), "/customer-support"));
  }

  // -- Clerk signup helpers ------------------------------------

  async fn org_count(registry: &PgRegistry) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM orgs")
      .fetch_one(registry.pool())
      .await
      .unwrap()
  }

  #[tokio::test]
  async fn clerk_account_is_org_plus_user_in_one_transaction() {
    let Some((registry, ..)) = registry().await else {
      return;
    };

    assert!(
      registry
        .find_user_by_clerk_id("user_1")
        .await
        .unwrap()
        .is_none()
    );
    let org = registry
      .create_clerk_account(
        "jane-acme-dev",
        "jane-acme-dev-0a1b",
        "jane@acme.dev",
        Some("jane"),
        "user_1",
      )
      .await
      .unwrap();
    assert_eq!(org.slug, "jane-acme-dev");
    let (user_id, org_id) = registry
      .find_user_by_clerk_id("user_1")
      .await
      .unwrap()
      .unwrap();
    assert_eq!(org_id, org.id);
    let (email, name): (String, Option<String>) =
      sqlx::query_as("SELECT email, name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(registry.pool())
        .await
        .unwrap();
    assert_eq!(
      (email.as_str(), name.as_deref()),
      ("jane@acme.dev", Some("jane"))
    );

    // A taken slug falls back to the suffixed one.
    let bob = registry
      .create_clerk_account(
        "jane-acme-dev",
        "jane-acme-dev-0a1b",
        "jane@acme.dev",
        None,
        "user_2",
      )
      .await
      .unwrap();
    assert_eq!(bob.slug, "jane-acme-dev-0a1b");

    // A Clerk id that already has an account: Conflict, and the org insert
    // is rolled back with it — nothing is left behind.
    let before = org_count(&registry).await;
    assert!(matches!(
      registry
        .create_clerk_account(
          "fresh-slug",
          "fresh-slug-ffff",
          "jane@acme.dev",
          None,
          "user_1"
        )
        .await,
      Err(RegistryError::Conflict(_))
    ));
    assert_eq!(org_count(&registry).await, before, "no orphan org");

    // Both slugs taken: Conflict, nothing written.
    assert!(matches!(
      registry
        .create_clerk_account(
          "jane-acme-dev",
          "jane-acme-dev-0a1b",
          "x@acme.dev",
          None,
          "user_3"
        )
        .await,
      Err(RegistryError::Conflict(_))
    ));
    assert_eq!(org_count(&registry).await, before);
    assert!(
      registry
        .find_user_by_clerk_id("user_3")
        .await
        .unwrap()
        .is_none()
    );
  }

  #[tokio::test]
  async fn expiring_platform_keys_expire_and_are_purged_after_grace() {
    let Some((registry, org, ..)) = registry().await else {
      return;
    };
    let now = Utc::now();
    let mk = |name: &'static str, hash: &'static str, expires: Option<DateTime<Utc>>| {
      let registry = registry.clone();
      async move {
        registry
          .create_api_key_expiring(org.id, ApiKeyKind::Platform, name, hash, expires)
          .await
          .unwrap()
      }
    };
    let old = mk("old", "h-old", Some(now - chrono::Duration::days(8))).await;
    mk("recent", "h-recent", Some(now - chrono::Duration::hours(1))).await;
    mk("live", "h-live", Some(now + chrono::Duration::days(1))).await;
    mk("forever", "h-forever", None).await;

    // Expiry is part of liveness for platform keys too.
    assert!(
      registry
        .find_api_key_by_hash("h-old")
        .await
        .unwrap()
        .is_none()
    );
    assert!(
      registry
        .find_api_key_by_hash("h-recent")
        .await
        .unwrap()
        .is_none()
    );
    assert!(!registry.api_key_is_live(old.id).await.unwrap());
    let live = registry
      .find_api_key_by_hash("h-live")
      .await
      .unwrap()
      .unwrap();
    assert!(live.expires_at.is_some());
    assert!(
      registry
        .find_api_key_by_hash("h-forever")
        .await
        .unwrap()
        .is_some()
    );

    // GC: the grace window protects the recently expired; live and
    // non-expiring keys are never purged.
    let cutoff = now - chrono::Duration::days(7);
    assert_eq!(registry.purge_expired_keys(cutoff).await.unwrap(), 1);
    assert_eq!(registry.purge_expired_keys(cutoff).await.unwrap(), 0);
    assert_eq!(registry.purge_expired_keys(Utc::now()).await.unwrap(), 1);
    assert!(
      registry
        .find_api_key_by_hash("h-live")
        .await
        .unwrap()
        .is_some()
    );
    assert!(
      registry
        .find_api_key_by_hash("h-forever")
        .await
        .unwrap()
        .is_some()
    );
  }
}
