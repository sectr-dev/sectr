//! sectr-server — the public gateway + control plane.
//!
//! One binary on the VPS compose stack: config from env, Postgres (Neon
//! direct URL — the journal tail is LISTEN/NOTIFY), auth middleware with
//! two bearer key classes, `/healthz`, key management, and the shared
//! sessions-api (journal-backed SSE) served by `ProdSessionService`.

use chrono::Duration;
use std::sync::Arc;

use clap::Parser;

use sectr_common::reconcile::Reconciler;
use sectr_common::store::PgStore;
use sectr_server::analytics::Analytics;
use sectr_server::config::{PlatformKind, ServerConfig};
use sectr_server::dispatch::TurnDispatcher;
use sectr_server::ingest::{HubAbort, IngestHub, SidecarDispatcher};
use sectr_server::platform::{DockerPlatform, RunnerPlatform};
use sectr_server::sessions::ProdSessionService;
use sectr_server::state::AppState;

#[derive(clap::Subcommand, Debug)]
enum Command {
  /// Serve HTTP (the only mode of the compose deployment; exists for
  /// scripting clarity).
  Serve,
}

#[derive(Parser, Debug)]
#[command(name = "sectr-server")]
struct Args {
  #[command(subcommand)]
  command: Command,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  let args = Args::parse();

  // Observability pipeline: fmt layer always (JSON by
  // default; `SECTR_LOG_FORMAT=text` for humans), OTLP exporter env-gated
  // on SECTR_OTLP_ENDPOINT — unset = stdout-only, dev behavior unchanged.
  // First, so startup failures are traceable; the guard flushes the
  // tracer provider at graceful-shutdown exit.
  let _telemetry = sectr_common::telemetry::init("sectr-server")?;

  match args.command {
    Command::Serve => serve().await,
  }
}

async fn serve() -> anyhow::Result<()> {
  let config = ServerConfig::from_env()?;
  tracing::info!(bind = %config.bind_addr, "starting sectr-server");

  let store = PgStore::open_with_tail_max(&config.pg_url, config.tail_pool_max).await?;
  let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());

  // Secrets: with a master key, secret values are sealed boxes at
  // rest (EncryptedRegistry wraps the store); without one the registry is
  // unwrapped and the secrets endpoints answer 503.
  let secrets_enabled = config.secrets_master_key.is_some();
  let registry: Arc<dyn sectr_common::registry::Registry> = match &config.secrets_master_key {
    Some(raw) => Arc::new(sectr_common::registry::EncryptedRegistry::new(
      Arc::new(store.registry()),
      Arc::new(sectr_common::secret_cipher::SecretCipher::from_env_value(
        raw,
      )),
    )),
    None => {
      tracing::warn!("SECTR_SECRETS_MASTER_KEY not set — secrets endpoints disabled (503)");
      Arc::new(store.registry())
    }
  };

  let ttl = Duration::seconds(config.lease_ttl_secs as i64);

  // The concrete Postgres registry handle for the alpha
  // provisioning surface (inherent PgRegistry methods — see alpha.rs).
  let pg_registry = store.registry();

  // Contract-4 ingest: grants + sidecar connections. The dispatcher
  // registers turns here; the DockerPlatform spawns the runner
  // container on first dispatch and kills it at turn end / reclaim.
  //
  // The runner platform:
  let artifact_store = sectr_server::state::build_artifact_store(&config).await?;
  // ONE object — build (deploy path) + spawn/teardown (dispatch path) —
  // constructed here and shared by both consumers. Backend selection:
  // SECTR_PLATFORM.
  let platform: Arc<dyn RunnerPlatform> = match config.platform {
    PlatformKind::Docker => {
      let docker = Arc::new(
        sectr_common::drivers::sandbox::DockerSandboxDriver::connect_with_socket(
          &config.docker_socket,
        )?,
      );
      docker.ping().await?;
      Arc::new(DockerPlatform::new(
        artifact_store.clone(),
        Some(docker),
        config.clone(),
        registry.clone(),
      ))
    }
    #[cfg(feature = "gcp")]
    PlatformKind::Gcp => {
      // Model (b): the full RunnerPlatform (build +
      // execution). Auth = ADC: on the control-plane VM the attached
      // sectr-server SA provides it via the metadata server (no key
      // material anywhere); locally via GOOGLE_APPLICATION_CREDENTIALS.
      Arc::new(
        sectr_server::platform::gcp_build::GcpPlatform::from_config(
          &config,
          artifact_store.clone(),
          registry.clone(),
        )
        .await?,
      )
    }
    #[cfg(not(feature = "gcp"))]
    PlatformKind::Gcp => anyhow::bail!(
      "SECTR_PLATFORM=gcp requires the sectr-server `gcp` cargo feature \
             (rebuild with: cargo build --features gcp) "
    ),
    #[cfg(feature = "e2b")]
    PlatformKind::E2b => {
      // The E2B backend: the Cloud Build build half is
      // SHARED with the gcp platform (build_only — no Cloud Run
      // runtime identity needed); only spawn/teardown differ (E2B
      // sandboxes instead of job executions).
      let gcp = sectr_server::platform::gcp_build::GcpPlatform::build_only(
        &config,
        artifact_store.clone(),
        registry.clone(),
      )
      .await?;
      Arc::new(sectr_server::platform::E2bPlatform::from_config(&config, gcp).await?)
    }
    #[cfg(not(feature = "e2b"))]
    PlatformKind::E2b => anyhow::bail!(
      "SECTR_PLATFORM=e2b requires the sectr-server `e2b` cargo feature \
             (rebuild with: cargo build --features e2b)"
    ),
  };
  // Endpoint deployments run on the customer's own endpoint: the routing
  // platform sends their spawns (wakes) and teardowns there, and
  // everything else to the managed platform above. It's the ONE platform
  // object every consumer below shares.
  let endpoints = Arc::new(sectr_server::platform::EndpointPlatform::new(
    config.endpoints.clone(),
  ));
  let platform: Arc<dyn RunnerPlatform> = Arc::new(sectr_server::platform::RoutingPlatform::new(
    platform,
    endpoints.clone(),
    registry.clone(),
  ));
  if let Some(ingest_url) = config.endpoints.ingest_url.as_deref() {
    tracing::info!(
      ingest_url,
      allow_private = config.endpoints.allow_private,
      "endpoint deployments on"
    );
  }
  let hub = Arc::new(IngestHub::new(registry.clone(), journal.clone(), ttl));
  hub.set_teardown(platform.clone());
  // A weak reference: the hub already holds the platform.
  endpoints.attach_hub(Arc::downgrade(&hub));
  let dispatcher: Arc<dyn TurnDispatcher> = Arc::new(SidecarDispatcher::new(
    hub.clone(),
    registry.clone(),
    journal.clone(),
    platform.clone(),
  ));
  let analytics = Arc::new(Analytics::from_config(&config));
  let service = Arc::new(
    ProdSessionService::new(registry.clone(), journal.clone(), dispatcher, ttl)
      .with_analytics(analytics.clone())
      // Concurrent-turn caps + per-user/per-app session-creation
      // rate limits (server policy; fail-fast env parse in the config).
      .with_limits(
        sectr_server::limits::TurnCapsConfig::from_config(&config.limits),
        sectr_server::limits::SessionRateLimiter::from_config(&config.limits),
      ),
  );
  let reaper_platform = platform.clone();

  let state = AppState::new(
    registry.clone(),
    journal.clone(),
    service,
    hub.clone(),
    config.clone(),
    artifact_store,
    platform,
    secrets_enabled,
  )
  .with_pg_registry(pg_registry)
  .with_endpoints(endpoints)
  .with_analytics(analytics)
  .with_signup_notifier(Arc::new(sectr_server::signup::SignupNotifier::from_config(
    &config,
  )));

  // Orphaned builds: a `building` row at startup is by definition dead —
  // its background build task died with the previous process. Fail them
  // (build_ref preserved for forensics). Zero grace: no build of THIS
  // process can exist yet.
  let swept =
    sectr_server::deploy::sweep_stale_building(store.pool(), registry.as_ref(), Duration::zero())
      .await?;
  if swept > 0 {
    tracing::info!(swept, "startup sweep marked orphaned builds failed");
  }

  // The reconciler: ends turns abandoned by lost sidecars (lease expiry).
  // Prod abort hook = the ingest hub's `abort` frame.
  let reconciler = Reconciler::new(
    registry.clone(),
    journal.clone(),
    std::time::Duration::from_secs(15),
  )
  .with_aborter(Arc::new(HubAbort(hub.clone())));
  tokio::spawn(reconciler.run());

  // Orphan-runner reaper: kills sandboxes no live lease owns (a
  // server restart can't leave us billed for idle sandboxes until their
  // timeout). SECTR_RUNNER_REAPER=off|dry-run|on (default on). Idempotent,
  // so several instances are fine (same note as run_expiry_gc).
  if config.limits.reaper != sectr_server::reaper::ReaperMode::Off {
    tokio::spawn(sectr_server::reaper::run_runner_reaper(
      reaper_platform,
      registry.clone(),
      config.limits.reaper,
      sectr_server::reaper::REAP_INTERVAL,
      sectr_server::reaper::REAP_GRACE,
    ));
  } else {
    tracing::info!("runner reaper disabled (SECTR_RUNNER_REAPER=off)");
  }

  // Expired user tokens accumulate one row per mint, and CLI login
  // requests one per attempt; purge hourly.
  tokio::spawn(sectr_server::http::tokens::run_expiry_gc(
    registry.clone(),
    state.pg_registry().cloned(),
  ));

  // Bootstrap the first platform key out-of-band (idempotent; skip when
  // unset — keys can then only come from direct DB provisioning).
  if let Some(key) = &config.bootstrap_platform_key {
    sectr_server::auth::ensure_bootstrap_key(&state, key, &config.org_name).await?;
  }

  let app = sectr_server::http::router(state);
  let listener = tokio::net::TcpListener::bind(config.bind_addr).await?;
  tracing::info!(bind = %config.bind_addr, "listening");

  axum::serve(listener, app)
    .with_graceful_shutdown(async {
      let _ = tokio::signal::ctrl_c().await;
      tracing::info!("shutting down");
    })
    .await?;
  Ok(())
}
