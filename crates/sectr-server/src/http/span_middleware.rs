//! Request-span middleware: one `http.request` span per HTTP
//! request, so the flat info/warn/error events emitted all over the server
//! become children of the request that caused them — request_id correlates
//! logs, the trace view shows one tree per request.
//!
//! Attributes:
//! * `request_id` — inbound `X-Request-Id` honored (proxy/frontend chaining),
//!   else a fresh UUIDv7; always echoed back on the response
//! * `method`, `route` — method + the matched path pattern (axum
//!   `MatchedPath`, available to `Router::layer` middleware)
//! * `session_id`, `app_id` — recorded here when they are PATH params of
//!   the matched route; `org_id`, `app_id` — recorded by the auth
//!   middleware (they come from the principal, which resolves inside the
//!   span, so they're declared `Empty` here and recorded there)
//! * `status` — recorded on close
//!
//! The span is fire-and-forget by construction: the OTel layer
//! feeds the batch processor, so nothing here blocks on export.

use axum::extract::{FromRequestParts as _, MatchedPath, RawPathParams};
use axum::http::{HeaderValue, Request, header::HeaderName};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

/// Response header echoing the request id back to the caller (and the
/// accepted inbound form of the header for chaining).
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Wrap one request in an `http.request` span. Outermost layer, so auth
/// and every handler event inherit it.
pub async fn request_span(req: Request<axum::body::Body>, next: Next) -> Response {
  let (mut parts, body) = req.into_parts();

  let request_id = inbound_request_id(&parts).unwrap_or_else(|| Uuid::now_v7().to_string());
  let route = parts
    .extensions
    .get::<MatchedPath>()
    .map(|p| p.as_str().to_owned());
  // Path params: `session_id` on the sessions data plane, `appId`/
  // `deploymentId` on the control plane (schema naming). Only what the
  // middleware can see without the body/principal — org_id and the
  // body-derived app ids are recorded later by auth / child spans.
  let params = RawPathParams::from_request_parts(&mut parts, &())
    .await
    .ok();
  let param = |names: &[&str]| {
    params.as_ref().and_then(|ps| {
      ps.iter()
        .find(|(k, _)| names.contains(k))
        .map(|(_, v)| (*v).to_owned())
    })
  };
  let session_id = param(&["session_id", "sessionId"]);
  let app_id = param(&["app_id", "appId"]);

  let span = tracing::info_span!(
      "http.request",
      request_id = %request_id,
      method = %parts.method,
      route = route.as_deref().unwrap_or("<unmatched>"),
      status = tracing::field::Empty,
      session_id = tracing::field::Empty,
      org_id = tracing::field::Empty,
      app_id = tracing::field::Empty,
  );
  if let Some(id) = session_id {
    span.record("session_id", id.as_str());
  }
  if let Some(id) = app_id {
    span.record("app_id", id.as_str());
  }

  let _guard = span.enter();
  let mut response = next.run(Request::from_parts(parts, body)).await;
  drop(_guard);

  span.record("status", response.status().as_u16());
  if let Ok(value) = HeaderValue::from_str(&request_id) {
    response.headers_mut().insert(REQUEST_ID_HEADER, value);
  }
  response
}

/// Inbound `X-Request-Id` (chaining), only when it is a plain UUID.
fn inbound_request_id(parts: &axum::http::request::Parts) -> Option<String> {
  parts
    .headers
    .get(&REQUEST_ID_HEADER)
    .and_then(|v| v.to_str().ok())
    .filter(|v| Uuid::parse_str(v).is_ok())
    .map(str::to_owned)
}

#[cfg(test)]
mod tests;
