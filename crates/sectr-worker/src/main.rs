//! The sidecar binary: the runner container's entrypoint. Reads its
//! wiring from the environment (injected at spawn by the server's
//! `SessionSpawner`):
//!
//! - `SECTR_INGEST_URL` — the server's ingest endpoint (`ws://…/ingest`).
//! - `SECTR_INGEST_TOKEN` — the session-scoped ingest token (`sit_…`).
//! - `SECTR_RUNNER_BASE` — optional; the user process's invoke URL the
//!   engine dials (default `http://127.0.0.1:8081`, matching
//!   [`SubprocessRunnerConfig::port`]).
//!
//! Runs [`SidecarClient::run`] forever. Exit semantics:
//! - dial rejected with **401 → exit 0**: the token is no longer valid
//!   (grant dropped — turn end teardown, server restart, or revocation).
//!   The container is no longer wanted; exiting lets Docker reap it. This
//!   is the self-cleanup path for server restarts (the grant map is
//!   in-memory, so a restarted server 401s every orphan).
//! - anything else → reconnect forever with capped backoff (network
//!   blips, server restarts mid-turn replay batches).

use std::time::Duration;

use sectr_common::telemetry;
use sectr_worker::runner::{SubprocessRunner, SubprocessRunnerConfig};
use sectr_worker::sidecar::{SidecarClient, SidecarConfig};

#[tokio::main]
async fn main() {
  // Resolve the rustls process-level CryptoProvider BEFORE anything can
  // need it (the wss:// ingest dial resolves it lazily). The workspace
  // graph enables BOTH ring and aws-lc-rs on the unified rustls crate, so
  // rustls 0.23 refuses to auto-select and panics at the first dial
  // without this (found live, E2B prod: sidecar exited 101 at startup
  // 2026-09-28; dev mode was unaffected — sectr-cli's dev.rs installs the
  // same provider itself). Same call as dev.rs; `let _` because a second
  // install attempt after a successful one is a benign no-op error.
  let _ = rustls::crypto::ring::default_provider().install_default();

  // Same pipeline as sectr-server (sectr_common::telemetry): JSON fmt
  // layer always (`SECTR_LOG_FORMAT=text` for humans), OTel layer only
  // when SECTR_OTLP_ENDPOINT is set. Hold the guard for the process
  // lifetime — dropping it flushes the exporter at exit.
  let _telemetry = telemetry::init("sectr-worker").expect("telemetry init failed");

  let ingest_url = std::env::var("SECTR_INGEST_URL")
    .expect("SECTR_INGEST_URL is required (server's ws://…/ingest endpoint)");
  let ingest_token = std::env::var("SECTR_INGEST_TOKEN")
    .expect("SECTR_INGEST_TOKEN is required (session-scoped ingest token)");
  let runner_base =
    std::env::var("SECTR_RUNNER_BASE").unwrap_or_else(|_| "http://127.0.0.1:8081".into());

  let config = SidecarConfig {
    ingest_url,
    ingest_token,
    runner_base,
    reconnect_backoff: Duration::from_millis(250),
    max_backoff: Duration::from_secs(10),
  };
  let runner = SubprocessRunner::new(SubprocessRunnerConfig::default());
  let client = SidecarClient::new(config, std::sync::Arc::new(runner));
  client.run().await;
}
