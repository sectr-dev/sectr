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
use serde::Serialize;

use crate::state::AppState;

/// Both routes' answer: `status` (`ok` | `degraded`), the database's state
/// (`/readyz` only: `ok` | `skipped` | `unavailable`) and the server's
/// version.
#[derive(Serialize)]
pub struct Health {
  status: &'static str,
  #[serde(skip_serializing_if = "Option::is_none")]
  db: Option<&'static str>,
  version: &'static str,
}

impl Health {
  fn new(status: &'static str, db: Option<&'static str>) -> Self {
    Health {
      status,
      db,
      version: env!("CARGO_PKG_VERSION"),
    }
  }
}

pub async fn healthz() -> Json<Health> {
  Json(Health::new("ok", None))
}

pub async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
  let Some(pg) = state.pg_registry() else {
    return (StatusCode::OK, Json(Health::new("ok", Some("skipped"))));
  };
  match pg.ping().await {
    Ok(()) => (StatusCode::OK, Json(Health::new("ok", Some("ok")))),
    Err(e) => {
      tracing::warn!(error = %e, "readyz: database ping failed");
      (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(Health::new("degraded", Some("unavailable"))),
      )
    }
  }
}
