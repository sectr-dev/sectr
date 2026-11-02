//! `GET /healthz` and `GET /readyz` — the anonymous operator routes
//! (external uptime monitoring, compose healthchecks).
//!
//! `/healthz` is pure liveness: no DB access, never fails for a downstream
//! reason (the dashboard badge polls it every 5 s per tab and `deploy.sh`
//! smoke-checks it). `/readyz` adds a bounded `SELECT 1` against
//! Postgres and is what the external uptime check targets.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::json;

use crate::state::AppState;

pub async fn healthz() -> Json<serde_json::Value> {
  Json(json!({
      "status": "ok",
      "version": env!("CARGO_PKG_VERSION"),
  }))
}

pub async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
  let version = env!("CARGO_PKG_VERSION");
  let Some(pg) = state.pg_registry() else {
    return (
      StatusCode::OK,
      Json(json!({ "status": "ok", "db": "skipped", "version": version })),
    );
  };
  match pg.ping().await {
    Ok(()) => (
      StatusCode::OK,
      Json(json!({ "status": "ok", "db": "ok", "version": version })),
    ),
    Err(e) => {
      tracing::warn!(error = %e, "readyz: database ping failed");
      (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "status": "degraded", "db": "unavailable", "version": version })),
      )
    }
  }
}
