//! `/ag-ui/{app}/{route}` on the prod server: `POST` runs and
//! `GET ?threadId=` connect.
//!
//! The endpoint itself — request validation, run / connect streaming, the
//! journal → AG-UI projection — is shared with the `sectr dev` gateway:
//! [`sectr_common::api::agui`]. This module only composes the PROD layers
//! around it, outermost first:
//!
//! 1. CORS ([`agui_cors_layer`]) — answers the preflight before auth ever
//!    runs; merged in `http::router` AFTER the optional console CORS layer
//!    (which only allows GET).
//! 2. the request span
//! 3. bearer auth ([`auth::auth_middleware`]) — `sks_` (browser) and `ski_`
//!    (backend) tokens via `Authorization: Bearer`; no `?token=` fallback
//!    (that is allowed only on the platform SSE tail)
//! 4. the credential-class gate: platform keys are not an AG-UI credential
//!    class (spec: no new auth shape — just the two data-plane classes)
//!
//! Thread resolution, ownership, token↔app binding and quotas live in
//! `ProdSessionService::start_run` (and `resolve_thread`, its read-only twin
//! for connect): the single authorization path.

use axum::Router;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use sectr_common::api::agui::{agui_cors_layer, agui_router};

use crate::auth::{self, Principal};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
  agui_router(state.service().clone(), state.journal().clone())
    .layer(middleware::from_fn(reject_platform_keys))
    .layer(middleware::from_fn_with_state(
      state.clone(),
      auth::auth_middleware,
    ))
    .layer(middleware::from_fn(
      crate::http::span_middleware::request_span,
    ))
    // Outermost: answers the preflight before auth ever runs.
    .layer(agui_cors_layer())
}

async fn reject_platform_keys(req: axum::extract::Request, next: Next) -> Response {
  if matches!(
    req.extensions().get::<Principal>(),
    Some(Principal::Platform { .. })
  ) {
    return (
      StatusCode::FORBIDDEN,
      axum::Json(serde_json::json!({
        "error": "the AG-UI endpoint accepts invocation keys (ski_) and user tokens (sks_), not platform keys"
      })),
    )
      .into_response();
  }
  next.run(req).await
}

#[cfg(test)]
mod tests;
