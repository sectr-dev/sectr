//! HTTP surface composition.
//!
//! Path layout follows `schema/public-api.yaml` and
//! `schema/sessions-api.yaml` (no `/v1` prefix: the yaml is authoritative
//! and the CLI/dashboard clients already speak it). Anonymous routes —
//! `/healthz`, `/readyz`, the sidecar ingest WS, and `POST /auth/clerk`
//! — live OUTSIDE the auth middleware. Everything else authenticates and
//! carries a `RequestScope` (org for platform keys, app for invocation keys).

#[cfg(test)]
mod tests;

pub mod api_keys;
pub mod health;
pub mod span_middleware;
pub mod tokens;

use crate::ingest;

use axum::Router;
use axum::middleware;
use axum::routing::{delete, get, post};
use sectr_common::api::sessions_router;

use crate::auth;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
  // Control plane: platform principals only (extractor-enforced).
  let control: Router<AppState> = Router::new()
    .route("/me", get(api_keys::me))
    .route("/api-keys", get(api_keys::list).post(api_keys::create))
    .route("/api-keys/{keyId}", delete(api_keys::revoke))
    // Revoke the presented dashboard-session key (platform principal).
    .route("/auth/logout", post(crate::clerk::logout))
    // The dashboard's CLI-login approval page.
    .merge(crate::cli_login::cli_login_authed_router())
    // Minting: invocation-key authenticated — the app's trusted
    // backend, not the dashboard (PlatformPrincipal would 403 here; the
    // handler's own extractor enforces the class).
    .route("/tokens", post(tokens::mint))
    .merge(crate::deploy::deploy_router())
    .merge(crate::apps::apps_router())
    .merge(crate::secrets::secrets_router());

  // Data plane: the shared sessions contract, served by the prod
  // service. Both key classes; the scope (derived in the auth
  // middleware) differentiates.
  let sessions: Router<()> = sessions_router(state.service().clone());

  // CORS: any origin, session routes + `/approvals` only. User
  // tokens (`sks_`) are the browser credential — an app's frontend on its
  // own domain calls these directly (create/list/get sessions, input,
  // approvals, the `EventSource` events stream). Auth is the bearer token
  // (header or `?token=`), never cookies, so there is nothing for an origin
  // list to protect. The layer sits OUTSIDE auth so preflights are answered
  // before it; it also covers the dashboard's direct SSE tail. The control
  // plane (platform keys, minting, deploys, secrets) stays same-origin.
  let sessions = sessions
    .layer(middleware::from_fn_with_state(
      state.clone(),
      auth::auth_middleware,
    ))
    .layer(
      tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([
          axum::http::Method::GET,
          axum::http::Method::POST,
          axum::http::Method::OPTIONS,
        ])
        .allow_headers([
          axum::http::header::AUTHORIZATION,
          axum::http::header::CONTENT_TYPE,
          axum::http::header::ACCEPT,
          axum::http::header::HeaderName::from_static("last-event-id"),
        ])
        .max_age(std::time::Duration::from_secs(600)),
    );

  let authed: Router<()> = control
    .with_state(state.clone())
    .layer(middleware::from_fn_with_state(
      state.clone(),
      auth::auth_middleware,
    ))
    .merge(sessions);

  // Anonymous routes: /healthz, the sidecar ingest WS (its own token
  // class, verified in the upgrade handler — see crate::ingest), and
  // Clerk session JWT → dashboard-session platform key exchange.
  let anon: Router<AppState> = Router::new()
    .route("/healthz", get(health::healthz))
    .route("/readyz", get(health::readyz))
    .route("/ingest", get(ingest::ingest_ws))
    .merge(crate::clerk::clerk_router())
    .merge(crate::cli_login::cli_login_router());

  let app = anon.with_state(state.clone()).merge(authed);

  // Request-span middleware — added LAST so it is OUTERMOST and
  // wraps auth + every handler (MatchedPath is available: the router
  // matches before the layer stack runs).
  let app = app.layer(middleware::from_fn(span_middleware::request_span));

  // AG-UI: The AG-UI router carries its own auth, request-span and CORS layers.
  app.merge(crate::agui::router(state))
}
