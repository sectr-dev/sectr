//! Browser login for `sectr auth login` — the OAuth 2.0 Device
//! Authorization Grant (RFC 8628) shape with Sectr's own endpoints.
//!
//! The CLI starts a request (`POST /auth/cli`), shows the user a URL + code,
//! and polls (`POST /auth/cli/poll`). A signed-in dashboard session looks the
//! request up by its user code and approves or denies it. The key is minted
//! by the first poll AFTER approval (never at approval), returned once, and
//! only its hash is stored — it is an ordinary non-expiring platform key
//! named `CLI (<hostname>)`, so every cloud command works unchanged.
//!
//! Deliberate limits:
//! - No auto-approve: approving takes a click on a page that shows the code,
//!   the machine name and the request age (device-code phishing defense).
//! - Requests live 10 minutes and are single-use. Poll answers `expired` for
//!   unknown, consumed and past-TTL codes alike, revealing nothing.
//! - Storage is inherent [`PgRegistry`] methods; the SQLite dev store
//!   answers 503 (like `/auth/clerk`).

use std::sync::Mutex;
use std::time::Instant;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use serde::Serialize;

use sectr_common::api::cli_login::{
  ApprovedLogin, DeviceGrant, PollAnswer, PollRequest, StartRequest,
};
use sectr_common::api::{ErrorBody, OrgRef};

use sectr_common::registry::{ApiKeyKind, CliLoginDecision, CliLoginPoll, PgRegistry};

use crate::analytics::{Props, events};
use crate::auth::{PlatformPrincipal, keys};
use crate::state::AppState;

/// How long a login request can be approved and collected.
pub const REQUEST_TTL_SECS: i64 = 600;
/// Seconds the CLI should wait between polls.
pub const POLL_INTERVAL_SECS: u64 = 2;
/// Request rows are kept this long past expiry (debugging), then purged.
pub const PURGE_GRACE_DAYS: i64 = 1;

const CLIENT_NAME_MAX: usize = 64;
const DEFAULT_CLIENT_NAME: &str = "unknown machine";
/// RFC 8628's consonant alphabet: no vowels (no accidental words), no 0/O/1/I.
const USER_CODE_ALPHABET: &[u8; 20] = b"BCDFGHJKLMNPQRSTVWXZ";

/// Anonymous router (mounted outside the bearer middleware).
pub fn cli_login_router() -> axum::Router<AppState> {
  axum::Router::new()
    .route("/auth/cli", post(start))
    .route("/auth/cli/poll", post(poll))
}

/// Authenticated (platform principal) router: the dashboard's approval page.
pub fn cli_login_authed_router() -> axum::Router<AppState> {
  axum::Router::new()
    .route(
      "/auth/cli/requests/{userCode}",
      axum::routing::get(get_request),
    )
    .route("/auth/cli/requests/{userCode}/approve", post(approve))
    .route("/auth/cli/requests/{userCode}/deny", post(deny))
}

// ---------------------------------------------------------------------------
// Rate limit: a global token bucket on `POST /auth/cli` (60/min, burst 20).
// No per-IP limiting — the server has no client-IP plumbing yet.
// ---------------------------------------------------------------------------

pub struct StartLimiter {
  burst: f64,
  refill_per_sec: f64,
  state: Mutex<(f64, Instant)>,
}

impl StartLimiter {
  pub fn new(per_minute: u32, burst: u32) -> Self {
    StartLimiter {
      burst: f64::from(burst),
      refill_per_sec: f64::from(per_minute) / 60.0,
      state: Mutex::new((f64::from(burst), Instant::now())),
    }
  }

  pub fn standard() -> Self {
    Self::new(60, 20)
  }

  /// Take one token; `Err(retry_after_secs)` when the bucket is empty.
  pub fn check(&self) -> Result<(), u64> {
    self.check_at(Instant::now())
  }

  fn check_at(&self, now: Instant) -> Result<(), u64> {
    let mut state = self.state.lock().unwrap();
    let elapsed = now.saturating_duration_since(state.1).as_secs_f64();
    state.0 = (state.0 + elapsed * self.refill_per_sec).min(self.burst);
    state.1 = now;
    if state.0 >= 1.0 {
      state.0 -= 1.0;
      Ok(())
    } else {
      Err(((1.0 - state.0) / self.refill_per_sec).ceil().max(1.0) as u64)
    }
  }
}

// ---------------------------------------------------------------------------
// Codes and names
// ---------------------------------------------------------------------------

/// 8 random characters from the consonant alphabet, `XXXX-XXXX`. Rejection
/// sampling keeps the distribution uniform (256 is not a multiple of 20).
fn generate_user_code() -> String {
  let mut code = String::with_capacity(9);
  let mut count = 0;
  while count < 8 {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("OS entropy");
    for b in buf {
      if b < 240 && count < 8 {
        if count == 4 {
          code.push('-');
        }
        code.push(USER_CODE_ALPHABET[(b % 20) as usize] as char);
        count += 1;
      }
    }
  }
  code
}

/// Lookup form of a user code: uppercase, anything but letters/digits
/// stripped (`wdjb mjht`, `WDJB-MJHT` and `wdjbmjht` are the same code).
pub fn normalize_user_code(raw: &str) -> String {
  raw
    .chars()
    .filter(|c| c.is_ascii_alphanumeric())
    .map(|c| c.to_ascii_uppercase())
    .collect()
}

/// `WDJBMJHT` → `WDJB-MJHT` (the stored and displayed form).
fn display_user_code(normalized: &str) -> String {
  match normalized.len() {
    8 => format!("{}-{}", &normalized[..4], &normalized[4..]),
    _ => normalized.to_owned(),
  }
}

/// Display-only machine name: control characters stripped, trimmed, at most
/// 64 characters; empty → "unknown machine".
pub fn sanitize_client_name(raw: &str) -> String {
  let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
  let name: String = cleaned.trim().chars().take(CLIENT_NAME_MAX).collect();
  let name = name.trim().to_owned();
  if name.is_empty() {
    DEFAULT_CLIENT_NAME.into()
  } else {
    name
  }
}

fn generate_device_code() -> String {
  let mut buf = [0u8; 32];
  getrandom::getrandom(&mut buf).expect("OS entropy");
  base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn error(status: StatusCode, msg: &str) -> Response {
  ErrorBody::response(status, msg)
}

fn internal_error(e: impl std::fmt::Display) -> Response {
  tracing::error!(error = %e, "CLI login failed");
  error(StatusCode::INTERNAL_SERVER_ERROR, "CLI login failed")
}

#[allow(clippy::result_large_err)] // the Err IS the ready-to-return HTTP response
fn pg(state: &AppState) -> Result<&PgRegistry, Response> {
  state.pg_registry().ok_or_else(|| {
    error(
      StatusCode::SERVICE_UNAVAILABLE,
      "browser login is unavailable on this deployment",
    )
  })
}

/// `POST /auth/cli` — `{"client_name"}` → the device code (secret), the
/// user code and the URLs to show.
async fn start(State(state): State<AppState>, Json(req): Json<StartRequest>) -> Response {
  let pg = match pg(&state) {
    Ok(pg) => pg,
    Err(response) => return response,
  };
  if let Err(retry_after) = state.cli_login_limiter().check() {
    let mut response = error(
      StatusCode::TOO_MANY_REQUESTS,
      "too many login attempts; retry shortly",
    );
    response.headers_mut().insert(
      header::RETRY_AFTER,
      HeaderValue::from_str(&retry_after.to_string()).expect("digits"),
    );
    return response;
  }

  let client_name = sanitize_client_name(&req.client_name);
  let device_code = generate_device_code();
  let device_hash = keys::hash(&device_code);

  // A user-code collision is a 1-in-2.6e10 event per live pair; retry once.
  let mut user_code = generate_user_code();
  for attempt in 0..2 {
    match pg
      .create_cli_login(&device_hash, &user_code, &client_name, REQUEST_TTL_SECS)
      .await
    {
      Ok(true) => break,
      Ok(false) if attempt == 0 => user_code = generate_user_code(),
      Ok(false) => return internal_error("user code collision"),
      Err(e) => return internal_error(e),
    }
  }

  let base = &state.config().dashboard_url;
  Json(DeviceGrant {
    device_code,
    verification_url: format!("{base}/cli"),
    verification_url_complete: format!("{base}/cli/{user_code}"),
    user_code,
    expires_in: REQUEST_TTL_SECS.unsigned_abs(),
    interval: POLL_INTERVAL_SECS,
  })
  .into_response()
}

/// `POST /auth/cli/poll` — `{"device_code"}` → `pending` | `denied` |
/// `expired` | `approved` + the key (only on the consuming call).
async fn poll(State(state): State<AppState>, Json(req): Json<PollRequest>) -> Response {
  let pg = match pg(&state) {
    Ok(pg) => pg,
    Err(response) => return response,
  };
  // Minted up front, stored only if this call wins the consume.
  let full_key = keys::mint(ApiKeyKind::Platform);
  match pg
    .poll_cli_login(&keys::hash(&req.device_code), &keys::hash(&full_key))
    .await
  {
    Ok(CliLoginPoll::Pending) => Json(PollAnswer::Pending).into_response(),
    Ok(CliLoginPoll::Denied) => Json(PollAnswer::Denied).into_response(),
    Ok(CliLoginPoll::Expired) => Json(PollAnswer::Expired).into_response(),
    Ok(CliLoginPoll::Approved { key_id, org }) => {
      state
        .analytics()
        .track(events::CLI_LOGIN, org.id, Props::default());
      Json(PollAnswer::Approved(ApprovedLogin {
        key: full_key,
        key_id,
        org: OrgRef {
          id: org.id,
          name: org.name,
        },
      }))
      .into_response()
    }
    Err(e) => internal_error(e),
  }
}

/// What the approval page shows of a login request. Its own type, so a
/// column added to the row isn't published without a decision.
#[derive(Serialize)]
struct LoginRequestView {
  user_code: String,
  client_name: String,
  created_at: chrono::DateTime<chrono::Utc>,
  expires_at: chrono::DateTime<chrono::Utc>,
  status: String,
}

/// `GET /auth/cli/requests/{userCode}` — what the approval page shows.
async fn get_request(
  State(state): State<AppState>,
  _principal: PlatformPrincipal,
  Path(user_code): Path<String>,
) -> Response {
  let pg = match pg(&state) {
    Ok(pg) => pg,
    Err(response) => return response,
  };
  let code = display_user_code(&normalize_user_code(&user_code));
  match pg.get_cli_login(&code).await {
    Ok(Some(req)) => Json(LoginRequestView {
      user_code: req.user_code,
      client_name: req.client_name,
      created_at: req.created_at,
      expires_at: req.expires_at,
      status: req.status,
    })
    .into_response(),
    Ok(None) => error(StatusCode::NOT_FOUND, "login request not found or expired"),
    Err(e) => internal_error(e),
  }
}

async fn approve(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  Path(user_code): Path<String>,
) -> Response {
  let approval = Some((principal.0.org_id(), principal.0.key_id()));
  decide(&state, &user_code, approval).await
}

async fn deny(
  State(state): State<AppState>,
  _principal: PlatformPrincipal,
  Path(user_code): Path<String>,
) -> Response {
  decide(&state, &user_code, None).await
}

/// A decision on a request that isn't pending: the error and its current
/// status.
#[derive(Serialize)]
struct AlreadyDecided {
  error: &'static str,
  status: String,
}

async fn decide(
  state: &AppState,
  user_code: &str,
  approval: Option<(uuid::Uuid, uuid::Uuid)>,
) -> Response {
  let pg = match pg(state) {
    Ok(pg) => pg,
    Err(response) => return response,
  };
  let code = display_user_code(&normalize_user_code(user_code));
  match pg.decide_cli_login(&code, approval).await {
    Ok(CliLoginDecision::Done) => StatusCode::NO_CONTENT.into_response(),
    Ok(CliLoginDecision::NotFound) => {
      error(StatusCode::NOT_FOUND, "login request not found or expired")
    }
    Ok(CliLoginDecision::NotPending(status)) => (
      StatusCode::CONFLICT,
      Json(AlreadyDecided {
        error: "login request was already decided",
        status,
      }),
    )
      .into_response(),
    Err(e) => internal_error(e),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  #[test]
  fn user_codes_use_the_consonant_alphabet_and_format() {
    for _ in 0..200 {
      let code = generate_user_code();
      assert_eq!(code.len(), 9);
      assert_eq!(code.as_bytes()[4], b'-');
      assert!(
        code
          .chars()
          .filter(|c| *c != '-')
          .all(|c| USER_CODE_ALPHABET.contains(&(c as u8)))
      );
    }
  }

  #[test]
  fn user_codes_normalize_on_lookup() {
    for raw in ["wdjb mjht", "WDJB-MJHT", " wdjb-mjht ", "wdjbmjht"] {
      assert_eq!(
        display_user_code(&normalize_user_code(raw)),
        "WDJB-MJHT",
        "{raw:?}"
      );
    }
  }

  #[test]
  fn client_names_are_sanitized() {
    assert_eq!(sanitize_client_name("  donald-mbp \n"), "donald-mbp");
    assert_eq!(sanitize_client_name("a\x1b[31mb\0c"), "a[31mbc");
    assert_eq!(sanitize_client_name(""), "unknown machine");
    assert_eq!(sanitize_client_name(" \t\x07"), "unknown machine");
    assert_eq!(sanitize_client_name(&"x".repeat(200)).chars().count(), 64);
  }

  #[test]
  fn start_limiter_is_a_token_bucket_with_retry_after() {
    let limiter = StartLimiter::new(60, 20);
    let t0 = Instant::now();
    for _ in 0..20 {
      assert!(limiter.check_at(t0).is_ok());
    }
    let retry = limiter.check_at(t0).unwrap_err();
    assert_eq!(retry, 1, "60/min refills a token per second");
    assert!(limiter.check_at(t0 + Duration::from_secs(1)).is_ok());
    assert!(limiter.check_at(t0 + Duration::from_secs(1)).is_err());
  }
}
