//! Shared handler state: the Postgres-backed store handles + the session
//! service + config. One `PgStore` open (migrations applied), trait objects
//! constructed from it — same pattern as the dev gateway's `SqliteStore`.

use std::sync::Arc;

use sectr_common::api::SessionService;
use sectr_common::journal::Journal;
use sectr_common::registry::{PgRegistry, Registry};

use crate::analytics::Analytics;
use crate::config::{ArtifactBackend, ServerConfig};
use crate::http::tokens::MintLimiter;
use crate::ingest::IngestHub;
use crate::platform::RunnerPlatform;
use crate::signup::SignupNotifier;

/// Build the deploy-path artifact store from config:
/// `SECTR_ARTIFACT_BACKEND=fs` (default, model a) → the filesystem store on
/// the VPS artifact volume; `gcs` (model b) → [`GcsArtifactStore`], only
/// when this crate was built with the `gcs` feature (config.rs already
/// fail-fasts otherwise, so the unreachable arm is belt-and-braces).
pub async fn build_artifact_store(
  config: &ServerConfig,
) -> anyhow::Result<Arc<dyn sectr_common::drivers::ArtifactStore>> {
  match config.artifact_backend {
    ArtifactBackend::Fs => Ok(Arc::new(
      sectr_common::drivers::FilesystemArtifactStore::new(&config.artifact_dir),
    )),
    #[cfg(feature = "gcs")]
    ArtifactBackend::Gcs => {
      let bucket = config
        .gcs_bucket
        .as_deref()
        .expect("config refuses backend=gcs without SECTR_GCS_BUCKET");
      let store = match &config.gcs_endpoint {
        Some(endpoint) => {
          tracing::info!(bucket, endpoint, "artifact store: GCS (emulator mode)");
          sectr_common::drivers::GcsArtifactStore::new_with_endpoint(bucket, endpoint).await?
        }
        None => {
          tracing::info!(bucket, "artifact store: GCS");
          sectr_common::drivers::GcsArtifactStore::new(bucket).await?
        }
      };
      Ok(Arc::new(store))
    }
    #[cfg(not(feature = "gcs"))]
    ArtifactBackend::Gcs => {
      unreachable!("config.rs fail-fasts on SECTR_ARTIFACT_BACKEND=gcs without the `gcs` feature")
    }
  }
}

#[derive(Clone)]
pub struct AppState {
  registry: Arc<dyn Registry>,
  journal: Arc<dyn Journal>,
  service: Arc<dyn SessionService>,
  hub: Arc<IngestHub>,
  config: Arc<ServerConfig>,
  /// Deploy-path drivers: tarball store + the runner platform
  /// (build + spawn + teardown — one object per backend, shared with the
  /// ingest hub/dispatcher).
  artifact_store: Arc<dyn sectr_common::drivers::ArtifactStore>,
  platform: Arc<dyn RunnerPlatform>,
  /// Whether secret values are encrypted at rest (a master key was
  /// configured). False → the secrets endpoints answer 503.
  secrets_enabled: bool,
  /// Product analytics — shared by the deploy path; no-op
  /// without `SECTR_POSTHOG_KEY` (disabled default, composition root
  /// attaches the live client from config).
  analytics: Arc<Analytics>,
  /// The operator's Discord notice per new account; no-op without
  /// `SECTR_SIGNUP_WEBHOOK_URL` (disabled default, like `analytics`).
  signup_notifier: Arc<SignupNotifier>,
  /// The CONCRETE Postgres registry: Clerk signup helpers live
  /// on PgRegistry only (not the Registry trait — signup is
  /// prod-Postgres-only). None in SQLite-backed test states →
  /// `/auth/clerk` answers 503.
  pg: Option<PgRegistry>,
  /// User-token mint rate limiter, keyed per (invocation key,
  /// external_user_id); sized from config.
  mint_limiter: Arc<MintLimiter>,
  /// `POST /auth/cli` token bucket.
  cli_login_limiter: Arc<crate::cli_login::StartLimiter>,
}

impl AppState {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    registry: Arc<dyn Registry>,
    journal: Arc<dyn Journal>,
    service: Arc<dyn SessionService>,
    hub: Arc<IngestHub>,
    config: ServerConfig,
    artifact_store: Arc<dyn sectr_common::drivers::ArtifactStore>,
    platform: Arc<dyn RunnerPlatform>,
    secrets_enabled: bool,
  ) -> Self {
    let mint_limiter = Arc::new(MintLimiter::new(config.mint_rate_limit_per_hour));
    AppState {
      registry,
      journal,
      service,
      hub,
      config: Arc::new(config),
      artifact_store,
      platform,
      secrets_enabled,
      analytics: Arc::new(Analytics::disabled()),
      signup_notifier: Arc::new(SignupNotifier::disabled()),
      pg: None,
      mint_limiter,
      cli_login_limiter: Arc::new(crate::cli_login::StartLimiter::standard()),
    }
  }

  /// Attach the live analytics client (composition root; tests that
  /// exercise the analytics emissions attach a capture-endpoint client).
  pub fn with_analytics(mut self, analytics: Arc<Analytics>) -> Self {
    self.analytics = analytics;
    self
  }

  /// Attach the live signup notifier (composition root; tests attach one
  /// pointed at a stand-in webhook).
  pub fn with_signup_notifier(mut self, notifier: Arc<SignupNotifier>) -> Self {
    self.signup_notifier = notifier;
    self
  }

  /// The mint rate limiter, built from config at construction.
  pub fn mint_limiter(&self) -> &Arc<MintLimiter> {
    &self.mint_limiter
  }

  /// The CLI-login start rate limiter.
  pub fn cli_login_limiter(&self) -> &Arc<crate::cli_login::StartLimiter> {
    &self.cli_login_limiter
  }

  /// Attach the concrete Postgres registry (composition root).
  pub fn with_pg_registry(mut self, pg: PgRegistry) -> Self {
    self.pg = Some(pg);
    self
  }

  pub fn registry(&self) -> &Arc<dyn Registry> {
    &self.registry
  }

  pub fn journal(&self) -> &Arc<dyn Journal> {
    &self.journal
  }

  pub fn hub(&self) -> &Arc<IngestHub> {
    &self.hub
  }

  pub fn service(&self) -> &Arc<dyn SessionService> {
    &self.service
  }

  #[allow(dead_code)] // consumed by the ingest and spawn paths
  pub fn config(&self) -> &ServerConfig {
    &self.config
  }

  pub fn artifact_store(&self) -> &Arc<dyn sectr_common::drivers::ArtifactStore> {
    &self.artifact_store
  }

  /// The runner platform: ONE object shared by the deploy path (build)
  /// and the ingest hub/dispatcher (spawn/teardown).
  pub fn platform(&self) -> &Arc<dyn RunnerPlatform> {
    &self.platform
  }

  pub fn secrets_enabled(&self) -> bool {
    self.secrets_enabled
  }

  /// The analytics handle (no-op without `SECTR_POSTHOG_KEY`). Shared by
  /// the deploy path; the session service holds its own clone.
  pub fn analytics(&self) -> &Arc<Analytics> {
    &self.analytics
  }

  /// The signup notifier (no-op without `SECTR_SIGNUP_WEBHOOK_URL`).
  pub fn signup_notifier(&self) -> &Arc<SignupNotifier> {
    &self.signup_notifier
  }

  /// The concrete Postgres registry, when this state is Postgres-backed
  /// (Clerk signup surface — None ⇒ 503).
  pub fn pg_registry(&self) -> Option<&PgRegistry> {
    self.pg.as_ref()
  }
}
