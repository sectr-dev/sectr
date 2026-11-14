//! The sidecar binary: the runner container's entrypoint. Two modes.
//!
//! **Env mode** (no arguments): every Sectr-run backend (Docker, E2B, Cloud
//! Run) starts one sidecar per invocation, with its wiring in the
//! environment (injected at spawn by the server's platform backend):
//!
//! - `SECTR_INGEST_URL` — the server's ingest endpoint (`ws://…/ingest`).
//! - `SECTR_INGEST_TOKEN` — this turn's ingest token (`sit_…`).
//! - `SECTR_RUNNER_BASE` — optional; the user process's invoke URL the
//!   engine dials (default `http://127.0.0.1:8081`, matching
//!   [`SubprocessRunnerConfig::port`]).
//!
//! It runs [`SidecarClient::run`] once and exits. Exit semantics:
//! - dial rejected with **401 → exit 0**: the token is no longer valid
//!   (grant dropped — turn end teardown, server restart, or revocation).
//!   The container is no longer wanted; exiting lets Docker reap it. This
//!   is the self-cleanup path for server restarts (the grant map is
//!   in-memory, so a restarted server 401s every orphan).
//! - a turn boundary → exit 0 (the next turn is a fresh dispatch).
//! - anything else → reconnect forever with capped backoff (network
//!   blips, server restarts mid-turn replay batches).
//!
//! **Serve mode** (`--serve`): an HTTP server on `PORT` (default 8080) for
//! the customer's own serverless or container platform. Each `POST /wake`
//! carries a turn's ingest token and runs one invocation exactly as env
//! mode does, dialing the image's own `SECTR_INGEST_URL`. See
//! [`sectr_worker::serve`].

#![cfg_attr(
  test,
  allow(
    clippy::disallowed_macros,
    reason = "tests build JSON fixtures with `json!`"
  )
)]

use std::time::Duration;

use sectr_common::telemetry;
use sectr_worker::runner::{SubprocessRunner, SubprocessRunnerConfig};
use sectr_worker::serve::ServeConfig;
use sectr_worker::sidecar::{SidecarClient, SidecarConfig};

const USAGE: &str = "usage: sectr-worker [--serve]\n\
  (no arguments)  run one invocation from SECTR_INGEST_URL + SECTR_INGEST_TOKEN\n\
  --serve         serve wake requests over HTTP on $PORT (default 8080)";

#[tokio::main]
async fn main() {
  let serve = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
    [] => false,
    [flag] if flag == "--serve" => true,
    _ => {
      eprintln!("{USAGE}");
      std::process::exit(2);
    }
  };

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

  if serve {
    run_serve_mode().await;
  } else {
    run_env_mode().await;
  }
}

async fn run_serve_mode() {
  let config = match ServeConfig::from_env() {
    Ok(config) => config,
    Err(e) => {
      tracing::error!(error = %e, "serve mode can't start");
      eprintln!("sectr-worker --serve: {e}");
      std::process::exit(1);
    }
  };
  let runner = SubprocessRunner::new(SubprocessRunnerConfig::default());
  if let Err(e) = sectr_worker::serve::serve(config, std::sync::Arc::new(runner)).await {
    tracing::error!(error = %e, "serve mode stopped");
    eprintln!("sectr-worker --serve: {e}");
    std::process::exit(1);
  }
}

async fn run_env_mode() {
  let ingest_url = std::env::var("SECTR_INGEST_URL")
    .expect("SECTR_INGEST_URL is required (server's ws://…/ingest endpoint)");
  let ingest_token = std::env::var("SECTR_INGEST_TOKEN")
    .expect("SECTR_INGEST_TOKEN is required (this turn's ingest token)");
  let runner_base =
    std::env::var("SECTR_RUNNER_BASE").unwrap_or_else(|_| "http://127.0.0.1:8081".into());

  let config = SidecarConfig {
    ingest_url,
    ingest_token,
    runner_base,
    reconnect_backoff: Duration::from_millis(250),
    max_backoff: Duration::from_secs(10),
    // The platform ends a container that never connects.
    dial_window: None,
  };
  let runner = SubprocessRunner::new(SubprocessRunnerConfig::default());
  let client = SidecarClient::new(config, std::sync::Arc::new(runner));
  // Both exits end the process with 0 (see the module docs).
  let _ = client.run().await;
}
