//! [`EncryptedRegistry`] — the at-rest boundary for secret values.
//!
//! A [`Registry`] decorator that encrypts on write / decrypts on read via a
//! [`SecretCipher`]; everything else is delegated verbatim. The inner
//! registry NEVER sees plaintext secret values, and the stored
//! `value_encrypted` column contains only `v1:` sealed boxes — the
//! decorator's own test asserts this against the raw database.
//!
//! Wired in the server's `main`: master key configured → the PgRegistry is
//! wrapped and the secrets endpoints enable; no master key → the registry
//! stays unwrapped and the endpoints answer 503 (a dev convenience, not a
//! prod posture — the compose stack always sets the key).

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use super::{
  ApiKey, ApiKeyKind, App, Deployment, DeploymentId, DeploymentStatus, Lease, LeaseCaps, NewApp,
  NewDeployment, NewSession, Org, Registry, RegistryError, Result, SecretMeta, Session,
  SessionStatus,
};
use crate::secret::Secret;
use crate::secret_cipher::SecretCipher;
use chrono::{DateTime, Utc};

/// Encrypts secret values at rest; a pure pass-through for everything else.
pub struct EncryptedRegistry {
  inner: Arc<dyn Registry>,
  cipher: Arc<SecretCipher>,
}

impl EncryptedRegistry {
  pub fn new(inner: Arc<dyn Registry>, cipher: Arc<SecretCipher>) -> Self {
    EncryptedRegistry { inner, cipher }
  }

  pub fn cipher(&self) -> &Arc<SecretCipher> {
    &self.cipher
  }
}

fn cipher_error(e: crate::secret_cipher::CipherError) -> RegistryError {
  RegistryError::Conflict(format!("secret cipher: {e}"))
}

#[async_trait]
impl Registry for EncryptedRegistry {
  async fn set_secret(&self, app_id: Uuid, key: &str, value: Secret) -> Result<()> {
    let encrypted = self.cipher.encrypt(value.expose()).map_err(cipher_error)?;
    self
      .inner
      .set_secret(app_id, key, Secret::new(encrypted))
      .await
  }

  async fn resolve_secrets(&self, app_id: Uuid) -> Result<Vec<(String, Secret)>> {
    let stored = self.inner.resolve_secrets(app_id).await?;
    stored
      .into_iter()
      .map(|(key, value)| {
        self
          .cipher
          .decrypt(value.expose())
          .map(|plain| (key, plain))
          .map_err(cipher_error)
      })
      .collect()
  }

  async fn list_secret_names(&self, app_id: Uuid) -> Result<Vec<SecretMeta>> {
    self.inner.list_secret_names(app_id).await
  }

  async fn delete_secret(&self, app_id: Uuid, key: &str) -> Result<()> {
    self.inner.delete_secret(app_id, key).await
  }

  // -- everything below is verbatim delegation ---------------------------

  async fn create_org(&self, name: &str, slug: &str) -> Result<Org> {
    self.inner.create_org(name, slug).await
  }
  async fn get_org(&self, id: Uuid) -> Result<Org> {
    self.inner.get_org(id).await
  }
  async fn find_org_by_slug(&self, slug: &str) -> Result<Option<Org>> {
    self.inner.find_org_by_slug(slug).await
  }
  async fn create_app(&self, new: NewApp) -> Result<App> {
    self.inner.create_app(new).await
  }
  async fn get_app(&self, id: Uuid) -> Result<App> {
    self.inner.get_app(id).await
  }
  async fn list_apps(&self, org_id: Uuid) -> Result<Vec<App>> {
    self.inner.list_apps(org_id).await
  }
  async fn count_sessions_by_app(&self, org_id: Uuid) -> Result<Vec<(Uuid, i64)>> {
    self.inner.count_sessions_by_app(org_id).await
  }
  async fn find_app_by_slug(&self, org_id: Uuid, slug: &str) -> Result<Option<App>> {
    self.inner.find_app_by_slug(org_id, slug).await
  }
  async fn create_deployment(&self, new: NewDeployment) -> Result<Deployment> {
    self.inner.create_deployment(new).await
  }
  async fn get_deployment(&self, id: DeploymentId) -> Result<Deployment> {
    self.inner.get_deployment(id).await
  }
  async fn set_deployment_status(&self, id: DeploymentId, status: DeploymentStatus) -> Result<()> {
    self.inner.set_deployment_status(id, status).await
  }
  async fn set_artifact_uri(&self, id: DeploymentId, uri: &str) -> Result<()> {
    self.inner.set_artifact_uri(id, uri).await
  }
  async fn set_deployment_build_ref(
    &self,
    id: DeploymentId,
    build_ref: Option<&str>,
  ) -> Result<()> {
    self.inner.set_deployment_build_ref(id, build_ref).await
  }
  async fn set_deployment_build_error(&self, id: DeploymentId, error: Option<&str>) -> Result<()> {
    self.inner.set_deployment_build_error(id, error).await
  }
  async fn list_deployments(&self, app_id: Uuid) -> Result<Vec<Deployment>> {
    self.inner.list_deployments(app_id).await
  }
  async fn set_deployment_manifest(
    &self,
    id: DeploymentId,
    manifest: &serde_json::Value,
  ) -> Result<()> {
    self.inner.set_deployment_manifest(id, manifest).await
  }
  async fn active_deployment(&self, app_id: Uuid) -> Result<Option<Deployment>> {
    self.inner.active_deployment(app_id).await
  }
  async fn create_session(&self, new: NewSession) -> Result<Session> {
    self.inner.create_session(new).await
  }
  async fn delete_unstarted_session(&self, id: Uuid) -> Result<bool> {
    self.inner.delete_unstarted_session(id).await
  }
  async fn get_session(&self, id: Uuid) -> Result<Session> {
    self.inner.get_session(id).await
  }
  async fn list_sessions(
    &self,
    org_id: Option<Uuid>,
    app_id: Option<Uuid>,
    external_user_id: Option<&str>,
    limit: i64,
    created_before: Option<DateTime<Utc>>,
  ) -> Result<Vec<Session>> {
    self
      .inner
      .list_sessions(org_id, app_id, external_user_id, limit, created_before)
      .await
  }
  async fn live_lease(&self, id: Uuid) -> Result<Option<Lease>> {
    self.inner.live_lease(id).await
  }
  async fn set_session_status(&self, id: Uuid, status: SessionStatus) -> Result<()> {
    self.inner.set_session_status(id, status).await
  }
  async fn create_api_key(
    &self,
    org_id: Uuid,
    app_id: Option<Uuid>,
    kind: ApiKeyKind,
    name: &str,
    key_hash: &str,
  ) -> Result<ApiKey> {
    self
      .inner
      .create_api_key(org_id, app_id, kind, name, key_hash)
      .await
  }
  async fn list_api_keys(&self, org_id: Uuid) -> Result<Vec<ApiKey>> {
    self.inner.list_api_keys(org_id).await
  }
  async fn revoke_api_key(&self, org_id: Uuid, key_id: Uuid) -> Result<()> {
    self.inner.revoke_api_key(org_id, key_id).await
  }
  async fn find_api_key_by_hash(&self, key_hash: &str) -> Result<Option<ApiKey>> {
    self.inner.find_api_key_by_hash(key_hash).await
  }
  async fn api_key_is_live(&self, key_id: Uuid) -> Result<bool> {
    self.inner.api_key_is_live(key_id).await
  }
  async fn purge_expired_keys(&self, expired_before: DateTime<Utc>) -> Result<u64> {
    self.inner.purge_expired_keys(expired_before).await
  }
  async fn create_user_token(
    &self,
    org_id: Uuid,
    app_id: Uuid,
    minted_by_key_id: Uuid,
    external_user_id: &str,
    key_hash: &str,
    expires_at: DateTime<Utc>,
  ) -> Result<ApiKey> {
    self
      .inner
      .create_user_token(
        org_id,
        app_id,
        minted_by_key_id,
        external_user_id,
        key_hash,
        expires_at,
      )
      .await
  }
  async fn claim_lease(
    &self,
    session: Uuid,
    runner_id: &str,
    invocation_id: Option<Uuid>,
    ttl: chrono::Duration,
  ) -> Result<Lease> {
    self
      .inner
      .claim_lease(session, runner_id, invocation_id, ttl)
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
      .inner
      .claim_lease_capped(session, runner_id, invocation_id, ttl, caps)
      .await
  }
  async fn count_live_turns(&self, app_id: Uuid, on_endpoint: bool) -> Result<(u64, u64)> {
    self.inner.count_live_turns(app_id, on_endpoint).await
  }
  async fn heartbeat_lease(&self, lease_id: Uuid, ttl: chrono::Duration) -> Result<Lease> {
    self.inner.heartbeat_lease(lease_id, ttl).await
  }
  async fn release_lease(&self, lease_id: Uuid) -> Result<()> {
    self.inner.release_lease(lease_id).await
  }
  async fn reclaim_expired_lease(&self, lease_id: Uuid) -> Result<Option<Lease>> {
    self.inner.reclaim_expired_lease(lease_id).await
  }
  async fn set_invocation(&self, lease_id: Uuid, invocation: Option<Uuid>) -> Result<Lease> {
    self.inner.set_invocation(lease_id, invocation).await
  }
  async fn expired_leases(&self) -> Result<Vec<Lease>> {
    self.inner.expired_leases().await
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::store::SqliteStore;

  const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

  async fn world() -> (Arc<dyn Registry>, String, uuid::Uuid) {
    let path = std::env::temp_dir()
      .join(format!("sectr-enc-registry-{}.db", Uuid::now_v7()))
      .to_string_lossy()
      .into_owned();
    let store = SqliteStore::open(&path).await.unwrap();
    let inner: Arc<dyn Registry> = Arc::new(store.registry());
    let org = inner.create_org("o", "o").await.unwrap();
    let app = inner
      .create_app(NewApp {
        org_id: org.id,
        name: "a".into(),
        slug: "a".into(),
      })
      .await
      .unwrap();
    let cipher = Arc::new(SecretCipher::from_env_value(KEY));
    let wrapped: Arc<dyn Registry> = Arc::new(EncryptedRegistry::new(inner, cipher));
    (wrapped, path, app.id)
  }

  /// The at-rest guarantee, asserted against the RAW database: the
  /// decorator's inner registry never sees plaintext.
  #[tokio::test]
  async fn values_never_reach_the_database_as_plaintext() {
    let (registry, path, app_id) = world().await;
    registry
      .set_secret(app_id, "OPENAI_API_KEY", Secret::new("sk-super-secret"))
      .await
      .unwrap();

    let pool = sqlx::sqlite::SqlitePool::connect(&format!("sqlite://{path}?mode=rw"))
      .await
      .unwrap();
    let (raw,): (String,) =
      sqlx::query_as("SELECT value_encrypted FROM secrets WHERE key = 'OPENAI_API_KEY'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!raw.contains("sk-super-secret"), "plaintext at rest: {raw}");
    assert!(raw.starts_with("v1:"), "missing version tag: {raw}");

    // Names list works; the value side decrypts back.
    let names = registry.list_secret_names(app_id).await.unwrap();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].key, "OPENAI_API_KEY");
    let resolved = registry.resolve_secrets(app_id).await.unwrap();
    assert_eq!(resolved[0].1.expose(), "sk-super-secret");

    // A different master key cannot read the stored value.
    let inner: Arc<dyn Registry> = Arc::new(SqliteStore::open(&path).await.unwrap().registry());
    let wrong = Arc::new(EncryptedRegistry::new(
      inner,
      Arc::new(SecretCipher::from_env_value(
        "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
      )),
    ));
    let err = wrong.resolve_secrets(app_id).await;
    assert!(err.is_err(), "wrong key must fail");
  }
}
