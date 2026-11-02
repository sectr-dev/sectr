//! `POST /tokens` — user-token minting. Invocation-key
//! authenticated (server-to-server): the app's backend asserts the
//! end-user identity (`external_user_id`) its own auth established, and
//! gets a short-TTL browser token (`sks_…`) bound to (app, user). The
//! browser then talks to the data plane directly — this is the ONE new
//! endpoint and the minimal trusted touchpoint (NOT a BFF).
//!
//! Rate limiting: per (invocation key, external_user_id), fixed one-hour
//! window, in-memory (a server restart resets it — the cheap guard is
//! deliberately cheap; the real gate is the app's own auth in front of
//! the mint call). 429 when the window is exhausted.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use sectr_common::registry::{ApiKeyKind, RegistryError};

use crate::auth::{InvocationPrincipal, Principal, keys};
use crate::state::AppState;

/// The mint rate limit: max mints per (key_id, external_user_id) per
/// one-hour fixed window.
pub struct MintLimiter {
  max_per_window: u32,
  /// One hour.
  window: Duration,
  windows: Mutex<HashMap<(Uuid, String), Window>>,
}

struct Window {
  start: Instant,
  count: u32,
}

impl MintLimiter {
  pub fn new(max_per_window: u32) -> Self {
    MintLimiter {
      max_per_window,
      window: Duration::from_secs(3600),
      windows: Mutex::new(HashMap::new()),
    }
  }

  /// Allow one mint for `(key, user)`? Records the attempt on Ok.
  fn check(&self, key_id: Uuid, external_user_id: &str) -> Result<(), ()> {
    let mut windows = self.windows.lock().unwrap();
    let now = Instant::now();
    // Opportunistic sweep when the map grows — distinct (key, user)
    // pairs per hour is bounded in practice; this keeps it bounded in
    // theory too.
    if windows.len() > 10_000 {
      windows.retain(|_, w| now.duration_since(w.start) < self.window);
    }
    let entry = windows
      .entry((key_id, external_user_id.to_owned()))
      .or_insert(Window {
        start: now,
        count: 0,
      });
    if now.duration_since(entry.start) >= self.window {
      entry.start = now;
      entry.count = 0;
    }
    if entry.count >= self.max_per_window {
      return Err(());
    }
    entry.count += 1;
    Ok(())
  }
}

/// How long an expired key's row is kept (debugging: "why was this
/// token rejected"), and how often the purge runs.
const PURGE_GRACE_DAYS: i64 = 7;
const PURGE_INTERVAL: Duration = Duration::from_secs(3600);

/// Run forever: hourly, delete keys of any kind that expired more than
/// [`PURGE_GRACE_DAYS`] ago (user tokens and dashboard-session
/// platform keys) and, with Postgres, CLI login requests past
/// their own grace — never the keys those requests minted. Errors are
/// logged and retried. Idempotent, so several server instances running it
/// is only redundant, never wrong.
pub async fn run_expiry_gc(
  registry: std::sync::Arc<dyn sectr_common::registry::Registry>,
  pg: Option<sectr_common::registry::PgRegistry>,
) {
  let mut ticker = tokio::time::interval(PURGE_INTERVAL);
  loop {
    ticker.tick().await;
    let cutoff = Utc::now() - chrono::Duration::days(PURGE_GRACE_DAYS);
    match registry.purge_expired_keys(cutoff).await {
      Ok(0) => {}
      Ok(purged) => tracing::info!(purged, "purged expired keys"),
      Err(e) => tracing::warn!(error = %e, "expired-key purge failed; retrying"),
    }
    if let Some(pg) = &pg {
      let cutoff = Utc::now() - chrono::Duration::days(crate::cli_login::PURGE_GRACE_DAYS);
      match pg.purge_cli_logins(cutoff).await {
        Ok(0) => {}
        Ok(purged) => tracing::info!(purged, "purged expired CLI login requests"),
        Err(e) => tracing::warn!(error = %e, "CLI login purge failed; retrying"),
      }
    }
  }
}

#[derive(Debug, Deserialize)]
pub struct MintTokenRequest {
  /// The app to mint for — must be the invocation key's own app (id or
  /// slug; a mismatch is refused, the key cannot mint for other apps).
  pub app: String,
  /// The app-defined end-user identity asserted by the app's OWN auth.
  /// Opaque to Sectr: `(app_id, external_user_id)` namespacing only.
  pub external_user_id: String,
}

/// `POST /tokens` — mint a user-scope browser token. 201 with the full
/// value, shown once (the store keeps the hash; TTL from config, 24h
/// default).
pub async fn mint(
  State(state): State<AppState>,
  principal: InvocationPrincipal,
  Json(req): Json<MintTokenRequest>,
) -> Response {
  let Principal::Invocation {
    org_id,
    key_id,
    app_id,
  } = principal.0
  else {
    // The extractor already enforces the class; unreachable.
    return internal_error("non-invocation principal reached minting");
  };

  let external_user_id = req.external_user_id.trim();
  if external_user_id.is_empty() {
    return bad_request("external_user_id is required");
  }
  if external_user_id.len() > 512 {
    return bad_request("external_user_id too long (max 512 chars)");
  }

  // The app must resolve — and be the KEY's app (mints are app-scoped by
  // the presenting key; the request field is the backend stating whose
  // app it is, verified against the credential).
  let resolved = match req.app.parse::<Uuid>() {
    Ok(id) => match state.registry().get_app(id).await {
      Ok(app) if app.org_id == org_id => Some(app.id),
      Ok(_) => None, // other org's app → treated like unknown (no leak)
      Err(RegistryError::NotFound(_)) => None,
      Err(e) => return internal_error(e),
    },
    Err(_) => match state.registry().find_app_by_slug(org_id, &req.app).await {
      Ok(app) => app.map(|app| app.id),
      Err(e) => return internal_error(e),
    },
  };
  match resolved {
    Some(id) if id == app_id => {}
    Some(_) => {
      return bad_request("app does not match the invocation key's app");
    }
    None => return not_found(format!("unknown app {:?}", req.app)),
  }

  // Cheap mint-farm guard, per (key, user).
  if state
    .mint_limiter()
    .check(key_id, external_user_id)
    .is_err()
  {
    return (
      StatusCode::TOO_MANY_REQUESTS,
      Json(json!({ "error": "mint rate limit exceeded for this user; retry later" })),
    )
      .into_response();
  }

  let ttl = chrono::Duration::seconds(state.config().user_token_ttl_secs as i64);
  let expires_at = Utc::now() + ttl;
  let full_token = keys::mint(ApiKeyKind::User);
  let key_hash = keys::hash(&full_token);
  match state
    .registry()
    .create_user_token(
      org_id,
      app_id,
      key_id,
      external_user_id,
      &key_hash,
      expires_at,
    )
    .await
  {
    Ok(_) => (StatusCode::CREATED, Json(json!({ "token": full_token }))).into_response(),
    // The presenting key was revoked between authentication and insert.
    Err(RegistryError::NotFound(_)) => (
      StatusCode::UNAUTHORIZED,
      Json(json!({ "error": "missing, invalid, or revoked key" })),
    )
      .into_response(),
    Err(e) => internal_error(e),
  }
}

fn bad_request(message: &str) -> Response {
  (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

fn not_found(message: String) -> Response {
  (StatusCode::NOT_FOUND, Json(json!({ "error": message }))).into_response()
}

fn internal_error(e: impl std::fmt::Display) -> Response {
  tracing::error!(error = %e, "token minting failed");
  (
    StatusCode::INTERNAL_SERVER_ERROR,
    Json(json!({ "error": "internal error" })),
  )
    .into_response()
}
