//! Auth middleware + extractors Three bearer-token classes, distinguished by the `api_keys.kind`
//! row they hash to:
//!
//! - **platform** (`skp_…`): control-plane principal (CLI, dashboard, SDK) —
//!   org-scoped.
//! - **invocation** (`ski_…`): per-app principal — the app's TRUSTED
//!   BACKEND credential (the minting credential; never shipped to browsers in multi-user
//!   apps).
//! - **user** (`sks_…`): per-user BROWSER token minted by the
//!   backend (`POST /tokens`) — app-scoped + bound to one
//!   `external_user_id`; sees/creates only that user's sessions. Short
//!   TTL; expired tokens authenticate as nonexistent.
//!
//! The middleware authenticates once (hash → `find_api_key_by_hash`, which
//! also stamps `last_used_at`) and inserts a [`Principal`] extension; typed
//! extractors then enforce the class per route (`PlatformPrincipal` for the
//! control plane, `InvocationPrincipal` for minting, raw `Principal` where
//! several classes are allowed — the session routes). No JWTs on
//! requests: opaque rows + hash lookup (revocation and
//! expiry are a single SQL predicate). A Clerk session JWT is
//! verified once, at `POST /auth/clerk` (clerk.rs), and swapped for an
//! expiring platform key — this middleware never sees it. No cookies
//! either — the dashboard keeps its platform key in localStorage.
//!
//! One header exception: the SSE tail endpoint ALSO accepts
//! `?token=<key>` — `EventSource` cannot set headers (`Last-Event-ID`
//! already has a query-param fallback). The parameter is accepted ONLY on
//! `/sessions/{id}/events`; access logs never carry it (the request span
//! logs the route PATTERN, not the URI — see span_middleware).

pub mod keys;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderValue, header::AUTHORIZATION};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{extract::State, http::StatusCode};
use uuid::Uuid;

use sectr_common::api::{Origin, RequestScope, UserTokenLease};
use sectr_common::registry::ApiKeyKind;

use crate::state::AppState;

/// The authenticated caller.
#[derive(Debug, Clone)]
pub enum Principal {
  /// Control-plane principal (CLI / dashboard / SDK) — org-scoped.
  Platform { org_id: Uuid, key_id: Uuid },
  /// The app's trusted backend — full data plane for its app. The minting
  /// credential (`POST /tokens`); never shipped
  /// to browsers in multi-user apps.
  Invocation {
    org_id: Uuid,
    key_id: Uuid,
    // Consumed by the per-session authorization (the session routes
    // check the invocation key's app against the session's app).
    app_id: Uuid,
  },
  /// End-user browser token: app-scoped AND owner-scoped. The
  /// scope the session service derives from it filters every read to the
  /// bound user's own sessions — cross-user isolation is by construction.
  User {
    org_id: Uuid,
    key_id: Uuid,
    app_id: Uuid,
    external_user_id: String,
    /// Hard expiry (carried so long-lived streams can end with the token).
    expires_at: chrono::DateTime<chrono::Utc>,
  },
}

impl Principal {
  pub fn org_id(&self) -> Uuid {
    match self {
      Principal::Platform { org_id, .. }
      | Principal::Invocation { org_id, .. }
      | Principal::User { org_id, .. } => *org_id,
    }
  }

  /// The api_keys row id (rate limiting, attribution).
  pub fn key_id(&self) -> Uuid {
    match self {
      Principal::Platform { key_id, .. }
      | Principal::Invocation { key_id, .. }
      | Principal::User { key_id, .. } => *key_id,
    }
  }

  pub fn kind(&self) -> ApiKeyKind {
    match self {
      Principal::Platform { .. } => ApiKeyKind::Platform,
      Principal::Invocation { .. } => ApiKeyKind::Invocation,
      Principal::User { .. } => ApiKeyKind::User,
    }
  }
}

/// Uniform auth error body: `{"error": "..."}` with 401/403.
pub struct AuthError {
  status: StatusCode,
  message: &'static str,
}

impl IntoResponse for AuthError {
  fn into_response(self) -> Response {
    let mut resp = sectr_common::api::ErrorBody::response(self.status, self.message);
    if self.status == StatusCode::UNAUTHORIZED {
      resp.headers_mut().insert(
        "WWW-Authenticate",
        HeaderValue::from_static("Bearer realm=\"sectr\""),
      );
    }
    resp
  }
}

/// Authenticate the request: `Authorization: Bearer <key>` → hash → live key
/// row → [`Principal`] extension. 401 on anything missing/unknown/revoked.
pub async fn auth_middleware(
  State(state): State<AppState>,
  mut req: axum::extract::Request,
  next: Next,
) -> Response {
  let token = extract_token(&req);
  let principal = match token {
    Some(token) => authenticate(&state, &token).await,
    None => Err(unauthorized()),
  };
  match principal {
    Ok(principal) => {
      // The scope the shared sessions router consumes: org boundary
      // for platform keys, app boundary + api origin for invocation
      // keys, app + OWNER boundary for user tokens.
      let scope = match &principal {
        Principal::Platform { org_id, .. } => RequestScope {
          org_id: Some(*org_id),
          app_id: None,
          origin: Origin::Playground,
          external_user_id: None,
          user_token: None,
        },
        Principal::Invocation { org_id, app_id, .. } => RequestScope {
          org_id: Some(*org_id),
          app_id: Some(*app_id),
          origin: Origin::Api,
          external_user_id: None,
          user_token: None,
        },
        Principal::User {
          org_id,
          app_id,
          external_user_id,
          key_id,
          expires_at,
          ..
        } => RequestScope {
          org_id: Some(*org_id),
          app_id: Some(*app_id),
          origin: Origin::Api,
          external_user_id: Some(external_user_id.clone()),
          user_token: Some(UserTokenLease {
            key_id: *key_id,
            expires_at: *expires_at,
          }),
        },
      };
      req.extensions_mut().insert(principal);
      req.extensions_mut().insert(scope.clone());
      // Stamp the principal-derived scope onto the enclosing
      // `http.request` span (fields declared Empty by the middleware
      // — the principal resolves INSIDE the span, so the middleware
      // cannot see them upfront).
      let span = tracing::Span::current();
      if let Some(org_id) = scope.org_id {
        span.record("org_id", tracing::field::display(org_id));
      }
      if let Some(app_id) = scope.app_id {
        span.record("app_id", tracing::field::display(app_id));
      }
      next.run(req).await
    }
    Err(e) => e.into_response(),
  }
}

/// Token extraction: `Authorization: Bearer <key>`; PLUS the
/// `?token=<key>` query parameter on the SSE tail path only —
/// `EventSource` cannot set headers. The Authorization header wins when
/// both are present.
fn extract_token(req: &axum::extract::Request) -> Option<String> {
  if let Some(token) = bearer_token(req.headers()) {
    return Some(token.to_owned());
  }
  if !is_events_path(req.uri().path()) {
    return None;
  }
  req.uri().query().and_then(|query| {
    query.split('&').find_map(|pair| {
      pair
        .strip_prefix("token=")
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
    })
  })
}

/// The SSE tail route pattern (`/sessions/{session_id}/events`) — the ONLY
/// path where the `?token=` fallback applies.
fn is_events_path(path: &str) -> bool {
  path.starts_with("/sessions/") && path.ends_with("/events")
}

fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
  let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
  let token = value
    .strip_prefix("Bearer ")
    .or_else(|| value.strip_prefix("bearer "))?;
  (!token.is_empty()).then_some(token)
}

async fn authenticate(state: &AppState, token: &str) -> Result<Principal, AuthError> {
  let key_hash = crate::auth::keys::hash(token);
  let key = state
    .registry()
    .find_api_key_by_hash(&key_hash)
    .await
    .map_err(|e| {
      tracing::error!(error = %e, "api key lookup failed");
      AuthError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "internal error",
      }
    })?
    .ok_or(unauthorized())?;

  match key.kind {
    ApiKeyKind::Platform => Ok(Principal::Platform {
      org_id: key.org_id,
      key_id: key.id,
    }),
    ApiKeyKind::Invocation => {
      // DDL guarantees app_id NOT NULL for invocation keys; treat a
      // violation as corrupt data, loudly.
      let app_id = key.app_id.ok_or(AuthError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "invocation key without app_id",
      })?;
      Ok(Principal::Invocation {
        org_id: key.org_id,
        key_id: key.id,
        app_id,
      })
    }
    ApiKeyKind::User => {
      // DDL guarantees app_id + external_user_id NOT NULL for user
      // tokens (expires_at too; expiry is filtered in SQL — an expired
      // token is None above). Treat a violation as corrupt data, loudly.
      let app_id = key.app_id.ok_or(AuthError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "user token without app_id",
      })?;
      let external_user_id = key.external_user_id.ok_or(AuthError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "user token without external_user_id",
      })?;
      let expires_at = key.expires_at.ok_or(AuthError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "user token without expires_at",
      })?;
      Ok(Principal::User {
        org_id: key.org_id,
        key_id: key.id,
        app_id,
        external_user_id,
        expires_at,
      })
    }
  }
}

fn unauthorized() -> AuthError {
  AuthError {
    status: StatusCode::UNAUTHORIZED,
    message: "missing, invalid, or revoked key",
  }
}

/// Extractor enforcing the platform class (control-plane routes). An
/// invocation key gets 403 — authenticated, but not this class.
#[derive(Debug, Clone)]
pub struct PlatformPrincipal(pub Principal);

impl FromRequestParts<AppState> for PlatformPrincipal {
  type Rejection = AuthError;

  async fn from_request_parts(
    parts: &mut Parts,
    _state: &AppState,
  ) -> Result<Self, Self::Rejection> {
    let principal = parts
      .extensions
      .get::<Principal>()
      .cloned()
      .ok_or(AuthError {
        status: StatusCode::UNAUTHORIZED,
        message: "missing, invalid, or revoked key",
      })?;
    match principal.kind() {
      ApiKeyKind::Platform => Ok(PlatformPrincipal(principal)),
      ApiKeyKind::Invocation => Err(AuthError {
        status: StatusCode::FORBIDDEN,
        message: "invocation keys cannot access the control plane",
      }),
      ApiKeyKind::User => Err(AuthError {
        status: StatusCode::FORBIDDEN,
        message: "user tokens cannot access the control plane",
      }),
    }
  }
}

/// Extractor enforcing the invocation class (the minting route). A
/// platform key gets 403 — the dashboard has no business minting browser
/// tokens (its session origins are playground principals); a user token
/// likewise. Authenticated, but not this class.
#[derive(Debug, Clone)]
pub struct InvocationPrincipal(pub Principal);

impl FromRequestParts<AppState> for InvocationPrincipal {
  type Rejection = AuthError;

  async fn from_request_parts(
    parts: &mut Parts,
    _state: &AppState,
  ) -> Result<Self, Self::Rejection> {
    let principal = parts
      .extensions
      .get::<Principal>()
      .cloned()
      .ok_or(AuthError {
        status: StatusCode::UNAUTHORIZED,
        message: "missing, invalid, or revoked key",
      })?;
    match principal.kind() {
      ApiKeyKind::Invocation => Ok(InvocationPrincipal(principal)),
      _ => Err(AuthError {
        status: StatusCode::FORBIDDEN,
        message: "only invocation keys may mint user tokens",
      }),
    }
  }
}

/// Bootstrap: register a pre-provisioned platform key (and the single MVP
/// org) if not already present. Idempotent: safe on every start. A key
/// that was registered and then revoked stays revoked; the server logs it
/// and starts without it.
pub async fn ensure_bootstrap_key(
  state: &AppState,
  key_value: &str,
  org_name: &str,
) -> anyhow::Result<()> {
  const ORG_SLUG: &str = "default";
  let key_hash = crate::auth::keys::hash(key_value);

  // Single MVP org: fixed slug, env-configured name.
  let org = match state.registry().find_org_by_slug(ORG_SLUG).await? {
    Some(org) => org,
    None => state.registry().create_org(org_name, ORG_SLUG).await?,
  };
  // Look the key up among all of the org's keys, revoked ones included:
  // `find_api_key_by_hash` only finds live keys, and inserting a revoked
  // key's hash again breaks the table's unique hash.
  let existing = state
    .registry()
    .list_api_keys(org.id)
    .await?
    .into_iter()
    .find(|k| k.key_hash == key_hash);
  match existing {
    Some(key) if key.revoked_at.is_some() => {
      tracing::warn!(
        key_id = %key.id,
        "the key in SECTR_BOOTSTRAP_PLATFORM_KEY is revoked; remove that line from the server's environment"
      );
    }
    Some(_) => {
      tracing::info!(org = %org_name, "bootstrap platform key already registered");
    }
    None => {
      state
        .registry()
        .create_api_key(org.id, None, ApiKeyKind::Platform, "bootstrap", &key_hash)
        .await?;
      tracing::info!(org_id = %org.id, "bootstrap platform key registered");
    }
  }
  Ok(())
}
