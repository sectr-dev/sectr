//! SQLite registry — the `sectr dev` / platform-dev implementation of
//! [`Registry`], over the same database as the journal (open it via
//! [`crate::store::SqliteStore`]). Production is `PgRegistry` (Phase 5; lease
//! claim will use FOR UPDATE SKIP LOCKED).

use super::{
  ApiKey, ApiKeyKind, App, AppId, Deployment, DeploymentId, DeploymentStatus, Lease, LeaseCaps,
  NewApp, NewDeployment, NewSession, Org, OrgId, Registry, RegistryError, Result, SecretMeta,
  Session, SessionStatus,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use crate::secret::Secret;

/// SQLite-backed [`Registry`].
pub struct SqliteRegistry {
  pool: SqlitePool,
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

impl SqliteRegistry {
  /// Wrap an existing pool (migrations applied, foreign keys enabled —
  /// open via [`crate::store::SqliteStore::open`]).
  pub fn new(pool: SqlitePool) -> Self {
    SqliteRegistry { pool }
  }

  /// The underlying pool.
  pub fn pool(&self) -> &SqlitePool {
    &self.pool
  }

  /// Full-row deployment read, shared by `create_deployment` (read-back
  /// invariant) and `get_deployment` — the store's projection is the only
  /// source of the returned value.
  async fn fetch_deployment(&self, id: DeploymentId) -> Result<Deployment> {
    sqlx::query_as::<_, Deployment>(
      "SELECT id, app_id, version, status, manifest, artifact_uri, build_ref, error, endpoint_url, created_at \
             FROM deployments WHERE id = ?",
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

    let mut conn = self.pool.acquire().await?;
    let result = claim_tx(&mut conn, session, runner_id, invocation_id, ttl, caps).await;
    match result {
      Ok(lease) => {
        sqlx::query("COMMIT").execute(&mut *conn).await?;
        span.record("lease_id", lease.id.to_string());
        Ok(lease)
      }
      Err(err) => {
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        Err(err)
      }
    }
  }
}

/// Rows deleted per statement by `purge_expired_keys` (bounds the
/// transaction size on a first run over a large backlog).
const PURGE_BATCH: i64 = 5000;

#[async_trait::async_trait]
impl Registry for SqliteRegistry {
  async fn create_org(&self, name: &str, slug: &str) -> Result<Org> {
    let id = Uuid::now_v7();
    let created_at = Utc::now();
    sqlx::query("INSERT INTO orgs (id, name, slug, created_at) VALUES (?, ?, ?, ?)")
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
      "SELECT id, name, slug, created_at FROM orgs WHERE id = ?",
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
        "SELECT id, name, slug, created_at FROM orgs WHERE slug = ?",
      )
      .bind(slug)
      .fetch_optional(&self.pool)
      .await?
      .map(Self::org),
    )
  }

  async fn create_app(&self, new: NewApp) -> Result<App> {
    let id = Uuid::now_v7();
    let created_at = Utc::now();
    sqlx::query("INSERT INTO apps (id, org_id, name, slug, created_at) VALUES (?, ?, ?, ?, ?)")
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
      "SELECT id, org_id, name, slug, created_at FROM apps WHERE id = ?",
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
             WHERE org_id = ? ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(&self.pool)
    .await?;
    Ok(rows.into_iter().map(Self::app).collect())
  }

  async fn count_sessions_by_app(&self, org_id: Uuid) -> Result<Vec<(Uuid, i64)>> {
    sqlx::query_as(
      "SELECT s.app_id, COUNT(*) FROM sessions s \
             JOIN apps a ON a.id = s.app_id WHERE a.org_id = ? GROUP BY s.app_id",
    )
    .bind(org_id)
    .fetch_all(&self.pool)
    .await
    .map_err(Into::into)
  }

  async fn find_app_by_slug(&self, org_id: OrgId, slug: &str) -> Result<Option<App>> {
    Ok(
      sqlx::query_as::<_, (Uuid, Uuid, String, String, DateTime<Utc>)>(
        "SELECT id, org_id, name, slug, created_at FROM apps WHERE org_id = ? AND slug = ?",
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
             VALUES (?, ?, ?, 'building', ?, ?, ?)",
    )
    .bind(id)
    .bind(new.app_id)
    .bind(new.version)
    .bind(new.manifest.to_string())
    .bind(new.endpoint_url.as_deref())
    .bind(Utc::now())
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
    sqlx::query("UPDATE deployments SET status = ? WHERE id = ?")
      .bind(status)
      .bind(id)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn set_artifact_uri(&self, id: DeploymentId, uri: &str) -> Result<()> {
    sqlx::query("UPDATE deployments SET artifact_uri = ? WHERE id = ?")
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
    sqlx::query("UPDATE deployments SET build_ref = ? WHERE id = ?")
      .bind(build_ref)
      .bind(id)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn set_deployment_build_error(&self, id: DeploymentId, error: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE deployments SET error = ? WHERE id = ?")
      .bind(error)
      .bind(id)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn set_deployment_manifest(&self, id: DeploymentId, manifest: &Value) -> Result<()> {
    sqlx::query("UPDATE deployments SET manifest = ? WHERE id = ?")
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
             FROM deployments WHERE app_id = ? ORDER BY version DESC, created_at DESC",
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
             FROM deployments WHERE app_id = ? AND status = 'active' \
             ORDER BY created_at DESC, version DESC LIMIT 1",
      )
      .bind(app_id)
      .fetch_optional(&self.pool)
      .await?,
    )
  }

  async fn create_session(&self, new: NewSession) -> Result<Session> {
    let id = new.id.unwrap_or_else(Uuid::now_v7);
    let created_at = Utc::now();
    sqlx::query(
      "INSERT INTO sessions (id, app_id, deployment_id, route, origin, status, created_at, external_user_id) \
             VALUES (?, ?, ?, ?, ?, 'active', ?, ?)",
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

  async fn get_session(&self, id: Uuid) -> Result<Session> {
    sqlx::query_as::<_, SessionRow>(
      "SELECT id, app_id, deployment_id, route, origin, status, created_at, updated_at, ended_at, external_user_id \
             FROM sessions WHERE id = ?",
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
             WHERE (?1 IS NULL OR a.org_id = ?1) \
             AND (?2 IS NULL OR s.app_id = ?2) \
             AND (?5 IS NULL OR s.external_user_id = ?5) \
             AND (?3 IS NULL OR s.created_at < ?3) \
             ORDER BY s.created_at DESC LIMIT ?4",
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
    let now = Utc::now();
    let row = sqlx::query_as::<_, LeaseRow>(
      "SELECT id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, \
             expires_at, released_at FROM leases \
             WHERE session_id = ? AND released_at IS NULL AND expires_at > ?",
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
    let now = Utc::now();
    // Setting Ended also stamps ended_at; every update stamps updated_at.
    sqlx::query(
      "UPDATE sessions SET status = ?, updated_at = ?, \
             ended_at = CASE WHEN ? = 'ended' THEN ? ELSE ended_at END \
             WHERE id = ?",
    )
    .bind(status)
    .bind(now)
    .bind("ended")
    .bind(now)
    .bind(id)
    .execute(&self.pool)
    .await?;
    Ok(())
  }

  async fn set_secret(&self, app_id: AppId, key: &str, value: Secret) -> Result<()> {
    sqlx::query(
      "INSERT INTO secrets (id, app_id, key, value_encrypted, created_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (app_id, key) DO UPDATE SET \
             value_encrypted = excluded.value_encrypted, updated_at = ?",
    )
    .bind(Uuid::now_v7())
    .bind(app_id)
    .bind(key)
    .bind(value.expose())
    .bind(Utc::now())
    .bind(Utc::now())
    .execute(&self.pool)
    .await?;
    Ok(())
  }

  async fn delete_secret(&self, app_id: AppId, key: &str) -> Result<()> {
    // Idempotent by design: the HTTP DELETE it serves returns 204 either way.
    sqlx::query("DELETE FROM secrets WHERE app_id = ? AND key = ?")
      .bind(app_id)
      .bind(key)
      .execute(&self.pool)
      .await?;
    Ok(())
  }

  async fn resolve_secrets(&self, app_id: AppId) -> Result<Vec<(String, Secret)>> {
    let rows: Vec<(String, String)> =
      sqlx::query_as("SELECT key, value_encrypted FROM secrets WHERE app_id = ?")
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
                 WHERE app_id = ? ORDER BY key",
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
    let created_at = Utc::now();
    let kind_value = serde_json::to_value(kind)?;
    let kind = kind_value.as_str().expect("unit enum serializes to string");
    sqlx::query(
      "INSERT INTO api_keys (id, org_id, app_id, kind, name, key_hash, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
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
        "SELECT EXISTS (SELECT 1 FROM api_keys WHERE id = ? AND revoked_at IS NULL \
             AND (expires_at IS NULL OR expires_at > ?))",
      )
      .bind(key_id)
      .bind(Utc::now())
      .fetch_one(&self.pool)
      .await?,
    )
  }

  async fn purge_expired_keys(&self, expired_before: DateTime<Utc>) -> Result<u64> {
    let mut total = 0;
    loop {
      let deleted = sqlx::query(
        "DELETE FROM api_keys WHERE id IN ( \
               SELECT id FROM api_keys WHERE expires_at < ? LIMIT ?)",
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
    let created_at = Utc::now();
    // Conditional on the minter still being live: a revoke that landed
    // between the caller's authentication and this insert must not leave
    // a live token behind (revoke_api_key's cascade has already run).
    let result = sqlx::query(
      "INSERT INTO api_keys (id, org_id, app_id, kind, name, key_hash, external_user_id, expires_at, created_at, minted_by_key_id) \
             SELECT ?, ?, ?, 'user', ?, ?, ?, ?, ?, ? \
             WHERE EXISTS (SELECT 1 FROM api_keys WHERE id = ? AND org_id = ? AND kind = 'invocation' AND revoked_at IS NULL)",
    )
    .bind(id)
    .bind(org_id)
    .bind(app_id)
    // The name identifies the row (owner first — several tokens for one
    // user may coexist across page loads).
    .bind(format!("user:{external_user_id}"))
    .bind(key_hash)
    .bind(external_user_id)
    .bind(expires_at)
    .bind(created_at)
    .bind(minted_by_key_id)
    .bind(minted_by_key_id)
    .bind(org_id)
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
             FROM api_keys WHERE org_id = ? AND kind != 'user' ORDER BY created_at DESC",
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
    let revoked_at = Utc::now();
    let mut tx = self.pool.begin().await?;
    let result = sqlx::query(
      "UPDATE api_keys SET revoked_at = ? \
             WHERE id = ? AND org_id = ? AND revoked_at IS NULL",
    )
    .bind(revoked_at)
    .bind(key_id)
    .bind(org_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
      "UPDATE api_keys SET revoked_at = ? \
             WHERE minted_by_key_id = ? AND org_id = ? AND revoked_at IS NULL",
    )
    .bind(revoked_at)
    .bind(key_id)
    .bind(org_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if result.rows_affected() == 0 {
      // Distinguish unknown/not-owned (404) from already-revoked (ok).
      let known: Option<Uuid> = sqlx::query_scalar("SELECT org_id FROM api_keys WHERE id = ?")
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
    // (revoked keys update nothing -> None). RETURNING keeps read-back
    // and stamp race-free.
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
            "UPDATE api_keys SET last_used_at = ? WHERE key_hash = ? AND revoked_at IS NULL \
             AND (expires_at IS NULL OR expires_at > ?) \
             RETURNING id, org_id, app_id, kind, name, key_hash, external_user_id, expires_at, created_at, last_used_at, revoked_at",
        )
        .bind(Utc::now())
        .bind(key_hash)
        .bind(Utc::now())
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
    let mut conn = self.pool.acquire().await?;
    count_live_turns_on(&mut conn, app_id, on_endpoint).await
  }

  async fn heartbeat_lease(&self, lease_id: Uuid, ttl: chrono::Duration) -> Result<Lease> {
    let now = Utc::now();
    let expires_at = now + ttl;
    let updated = sqlx::query(
      "UPDATE leases SET heartbeat_at = ?, expires_at = ? \
             WHERE id = ? AND released_at IS NULL",
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
             expires_at, released_at FROM leases WHERE id = ?",
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
      sqlx::query("UPDATE leases SET released_at = ? WHERE id = ? AND released_at IS NULL")
        .bind(Utc::now())
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
      "UPDATE leases SET released_at = ?1 \
             WHERE id = ?2 AND released_at IS NULL AND expires_at <= ?1 \
             RETURNING id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, \
                       expires_at, released_at",
    )
    .bind(Utc::now())
    .bind(lease_id)
    .fetch_optional(&self.pool)
    .await?;
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
      sqlx::query("UPDATE leases SET invocation_id = ? WHERE id = ? AND released_at IS NULL")
        .bind(invocation)
        .bind(lease_id)
        .execute(&self.pool)
        .await?;
    if updated.rows_affected() == 0 {
      return Err(RegistryError::NotFound(format!("live lease {lease_id}")));
    }
    sqlx::query_as::<_, LeaseRow>(
      "SELECT id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, \
             expires_at, released_at FROM leases WHERE id = ?",
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
             WHERE released_at IS NULL AND expires_at <= ? ORDER BY expires_at",
    )
    .bind(Utc::now())
    .fetch_all(&self.pool)
    .await?;
    Ok(rows.into_iter().map(Self::lease).collect())
  }
}

/// `(live leases of app_id, live leases platform-wide)`, of one kind
/// (`on_endpoint`).
async fn count_live_turns_on(
  conn: &mut sqlx::SqliteConnection,
  app_id: Uuid,
  on_endpoint: bool,
) -> Result<(u64, u64)> {
  let (app, total): (Option<i64>, i64) = sqlx::query_as(
    "SELECT sum(s.app_id = ?1), count(*) \
         FROM leases l JOIN sessions s ON s.id = l.session_id \
         JOIN deployments d ON d.id = s.deployment_id \
         WHERE l.released_at IS NULL AND l.expires_at > ?2 \
         AND (d.endpoint_url IS NOT NULL) = ?3",
  )
  .bind(app_id)
  .bind(Utc::now())
  .bind(on_endpoint)
  .fetch_one(&mut *conn)
  .await?;
  Ok((app.unwrap_or(0).max(0) as u64, total.max(0) as u64))
}

/// One claim = one IMMEDIATE transaction. A live, unexpired lease conflicts;
/// an expired one is superseded (released, then re-claimed). The partial
/// UNIQUE index is the backstop against any race escaping the transaction.
async fn claim_tx(
  conn: &mut sqlx::SqliteConnection,
  session: Uuid,
  runner_id: &str,
  invocation_id: Option<Uuid>,
  ttl: chrono::Duration,
  caps: Option<&LeaseCaps>,
) -> Result<Lease> {
  sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

  // BEGIN IMMEDIATE already serializes writers, so the count and
  // the claim below are atomic without any extra lock.
  if let Some(caps) = caps
    && caps.any()
  {
    let (app_live, total_live) =
      count_live_turns_on(&mut *conn, caps.app_id, caps.on_endpoint).await?;
    if let Some((scope, limit)) = caps.exceeded(app_live, total_live) {
      return Err(RegistryError::CapExceeded { scope, limit });
    }
  }

  let existing: Option<(Uuid, DateTime<Utc>, String)> = sqlx::query_as(
    "SELECT id, expires_at, runner_id FROM leases \
         WHERE session_id = ? AND released_at IS NULL",
  )
  .bind(session)
  .fetch_optional(&mut *conn)
  .await?;

  if let Some((lease_id, expires_at, holder)) = existing {
    if expires_at > Utc::now() {
      return Err(RegistryError::Conflict(format!(
        "session {session} already has a live lease (runner {holder}) until {expires_at}"
      )));
    }
    // Expired: supersede.
    sqlx::query("UPDATE leases SET released_at = ? WHERE id = ?")
      .bind(Utc::now())
      .bind(lease_id)
      .execute(&mut *conn)
      .await?;
  }

  let id = Uuid::now_v7();
  let now = Utc::now();
  let expires_at = now + ttl;
  sqlx::query(
        "INSERT INTO leases (id, session_id, runner_id, invocation_id, claimed_at, heartbeat_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(session)
    .bind(runner_id)
    .bind(invocation_id)
    .bind(now)
    .bind(now)
    .bind(expires_at)
    .execute(&mut *conn)
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::registry::deployment_serves_route;
  use crate::store::SqliteStore;
  use serde_json::json;
  use std::time::Duration;

  async fn registry() -> (SqliteRegistry, Org, App, Deployment, String) {
    let path = std::env::temp_dir()
      .join(format!("sectr-registry-test-{}.db", Uuid::now_v7()))
      .to_string_lossy()
      .into_owned();
    let store = SqliteStore::open(&path).await.unwrap();
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
    (registry, org, app, deployment, path)
  }

  #[tokio::test]
  async fn lease_cap_per_app() {
    let (registry, org, ..) = registry().await;
    crate::registry::cap_suite::per_app_cap(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_global_across_apps() {
    let (registry, org, ..) = registry().await;
    crate::registry::cap_suite::global_cap_across_apps(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_ignores_expired_and_released() {
    let (registry, org, ..) = registry().await;
    crate::registry::cap_suite::expired_and_released_leases_do_not_count(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_counts_kinds_apart() {
    let (registry, org, ..) = registry().await;
    crate::registry::cap_suite::kinds_are_counted_apart(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_uncapped_and_conflict() {
    let (registry, org, ..) = registry().await;
    crate::registry::cap_suite::uncapped_and_conflict_semantics(&registry, org.id).await;
  }

  #[tokio::test]
  async fn lease_cap_concurrent_claims_respect_cap() {
    let (registry, org, ..) = registry().await;
    crate::registry::cap_suite::concurrent_claims_respect_cap(
      std::sync::Arc::new(registry),
      org.id,
    )
    .await;
  }

  #[tokio::test]
  async fn org_and_app_round_trip() {
    let (registry, org, app, _deployment, _path) = registry().await;
    assert_eq!(registry.get_org(org.id).await.unwrap(), org);
    assert_eq!(registry.get_app(app.id).await.unwrap(), app);
    assert!(matches!(
      registry.get_app(Uuid::now_v7()).await.unwrap_err(),
      RegistryError::NotFound(_)
    ));
  }

  #[tokio::test]
  async fn create_deployment_returns_store_projection() {
    let (registry, _org, app, _deployment, _path) = registry().await;
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
    let (registry, _org, app, deployment, _path) = registry().await;

    // Newest active wins; ties break by higher version.
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

    // Artifact URI is set-once from the build.
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
    let (registry, _org, _app, deployment, _path) = registry().await;

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
    let (registry, _org, _app, deployment, _path) = registry().await;

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
    let (registry, _org, app, deployment, _path) = registry().await;
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

    // Approval pause + resume + end.
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
    let (registry, _org, app, _deployment, _path) = registry().await;
    registry
      .set_secret(app.id, "OPENAI_API_KEY", Secret::new("sk-1"))
      .await
      .unwrap();
    // Upsert overwrites.
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
    // Redacted if the whole list gets logged by accident.
    assert!(!format!("{resolved:?}").contains("sk-2"));
    // An overwrite stamps updated_at; a first write leaves it unset.
    let metas = registry.list_secret_names(app.id).await.unwrap();
    assert_eq!(metas[0].key, "OPENAI_API_KEY");
    assert!(metas[0].updated_at.is_some());
    assert_eq!(metas[1].key, "OTHER");
    assert!(metas[1].updated_at.is_none());

    registry.delete_secret(app.id, "OTHER").await.unwrap();
    // Idempotent delete.
    registry.delete_secret(app.id, "OTHER").await.unwrap();
    assert_eq!(registry.resolve_secrets(app.id).await.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn secret_names_list_metadata_only() {
    let (registry, _org, app, _deployment, _path) = registry().await;
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
  async fn api_key_lifecycle_and_auth_lookup() {
    let (registry, org, app, _deployment, _path) = registry().await;

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

    let listed = registry.list_api_keys(org.id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, invocation.id);
    let other = registry.create_org("other", "other").await.unwrap();
    assert!(registry.list_api_keys(other.id).await.unwrap().is_empty());

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

    registry.revoke_api_key(org.id, created.id).await.unwrap();
    registry.revoke_api_key(org.id, created.id).await.unwrap(); // 204 again
    assert!(
      registry
        .find_api_key_by_hash(platform_hash)
        .await
        .unwrap()
        .is_none()
    );
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
  async fn lease_invocation_binding() {
    let (registry, _org, app, deployment, _path) = registry().await;
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

    // Persistent-runner mode: claim without a work annotation, bind at
    // turn start, clear at turn end.
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

    // A released lease can't be rebound.
    registry.release_lease(lease.id).await.unwrap();
    let err = registry
      .set_invocation(lease.id, Some(invocation))
      .await
      .unwrap_err();
    assert!(matches!(err, RegistryError::NotFound(_)));
  }

  #[tokio::test]
  async fn lease_claim_heartbeat_release() {
    let (registry, _org, app, deployment, _path) = registry().await;
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
      matches!(err, RegistryError::Conflict(ref m) if m.contains("already has a live lease"))
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
    let (registry, _org, app, deployment, _path) = registry().await;
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

    // Zero TTL: expires immediately.
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

    // The reconciler's scan: superseded lease is released, live one is
    // not expired.
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

  #[tokio::test]
  async fn user_token_lifecycle_expiry_and_owner_filter() {
    let (registry, org, app, deployment, _path) = registry().await;

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
  async fn purge_expired_keys_covers_expiring_platform_keys() {
    let (registry, org, ..) = registry().await;

    // dashboard-session keys are platform keys WITH an expiry; the
    // trait has no expiring-create (PgRegistry-inherent), so insert directly.
    async fn insert(
      registry: &SqliteRegistry,
      org: Uuid,
      hash: &str,
      expires: Option<DateTime<Utc>>,
    ) {
      sqlx::query(
        "INSERT INTO api_keys (id, org_id, kind, name, key_hash, expires_at, created_at) \
             VALUES (?, ?, 'platform', 'dashboard session (Clerk)', ?, ?, ?)",
      )
      .bind(Uuid::now_v7())
      .bind(org)
      .bind(hash)
      .bind(expires)
      .bind(Utc::now())
      .execute(registry.pool())
      .await
      .unwrap();
    }
    let now = Utc::now();
    insert(
      &registry,
      org.id,
      "h-old",
      Some(now - chrono::Duration::days(8)),
    )
    .await;
    insert(
      &registry,
      org.id,
      "h-recent",
      Some(now - chrono::Duration::hours(1)),
    )
    .await;
    insert(
      &registry,
      org.id,
      "h-live",
      Some(now + chrono::Duration::days(1)),
    )
    .await;
    insert(&registry, org.id, "h-forever", None).await;

    // Grace window: only the key expired > 7 days ago goes.
    let cutoff = now - chrono::Duration::days(7);
    assert_eq!(registry.purge_expired_keys(cutoff).await.unwrap(), 1);
    assert_eq!(registry.purge_expired_keys(cutoff).await.unwrap(), 0);
    // Past the grace, the recently expired one goes too; live and
    // non-expiring keys are never touched.
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
}
