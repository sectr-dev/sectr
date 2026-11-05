//! API key management (`/api-keys`) + `/me` — the control-plane surface.
//! All platform-principal routes; shapes mirror
//! `schema/public-api.yaml` (ApiKeyMeta / Me).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use sectr_common::registry::ApiKeyKind;

use crate::auth::keys;
use crate::state::AppState;

/// `schema/public-api.yaml` ApiKeyMeta (prefix dropped — the store keeps
/// only hashes, so no display prefix exists; `name` identifies keys).
#[derive(Debug, Serialize)]
pub struct ApiKeyMeta {
  pub id: Uuid,
  pub name: String,
  pub kind: ApiKeyKind,
  /// NULL for platform keys.
  pub app_id: Option<Uuid>,
  pub created_at: DateTime<Utc>,
  pub last_used_at: Option<DateTime<Utc>>,
  pub revoked_at: Option<DateTime<Utc>>,
  /// Hard expiry (dashboard-session keys); NULL = never expires.
  pub expires_at: Option<DateTime<Utc>>,
}

fn meta(key: sectr_common::registry::ApiKey) -> ApiKeyMeta {
  ApiKeyMeta {
    id: key.id,
    name: key.name,
    kind: key.kind,
    app_id: key.app_id,
    created_at: key.created_at,
    last_used_at: key.last_used_at,
    revoked_at: key.revoked_at,
    expires_at: key.expires_at,
  }
}

/// `GET /me` — current principal + org (sidebar / account menus). The MVP
/// has no user records (paste-a-platform-key login), so `user` is omitted.
pub async fn me(
  State(state): State<AppState>,
  principal: crate::auth::PlatformPrincipal,
) -> Response {
  let org = match state.registry().get_org(principal.0.org_id()).await {
    Ok(org) => org,
    Err(e) => return internal_error(e),
  };
  Json(json!({
      "org": { "id": org.id, "name": org.name },
      "dashboard_url": state.config().dashboard_url,
  }))
  .into_response()
}

#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
  pub name: String,
  pub kind: ApiKeyKind,
  /// Required for invocation keys (the app being scoped to); must be
  /// absent for platform keys.
  pub app_id: Option<Uuid>,
}

/// `POST /api-keys` — mint a key; the full value is returned EXACTLY ONCE
/// (the store only ever sees the hash).
pub async fn create(
  State(state): State<AppState>,
  principal: crate::auth::PlatformPrincipal,
  Json(req): Json<CreateKeyRequest>,
) -> Response {
  if req.name.trim().is_empty() {
    return bad_request("name is required");
  }
  let kind = req.kind;
  // Class/app_id consistency (also enforced by the DDL CHECK; validate
  // here for a readable 400 and to check the app exists in the org).
  let app_id = match (&kind, req.app_id) {
    (ApiKeyKind::Platform, None) => None,
    (ApiKeyKind::Invocation, Some(app_id)) => match state.registry().get_app(app_id).await {
      Ok(app) if app.org_id == principal.0.org_id() => Some(app_id),
      Ok(_) => return bad_request("app belongs to a different org"),
      Err(_) => return bad_request("unknown app"),
    },
    (ApiKeyKind::Platform, Some(_)) => {
      return bad_request("platform keys must not carry an app_id");
    }
    (ApiKeyKind::Invocation, None) => {
      return bad_request("invocation keys require an app_id");
    }
    // User tokens are minted ONLY via `POST /tokens` by the
    // app's backend — never through the dashboard's control-plane mint.
    (ApiKeyKind::User, _) => {
      return bad_request("user tokens are minted via POST /tokens with an invocation key");
    }
  };

  let full_key = keys::mint(kind);
  let key_hash = keys::hash(&full_key);
  match state
    .registry()
    .create_api_key(
      principal.0.org_id(),
      app_id,
      kind,
      req.name.trim(),
      &key_hash,
    )
    .await
  {
    Ok(key) => {
      let mut body = serde_json::to_value(meta(key)).expect("serializable meta");
      body["key"] = serde_json::Value::String(full_key);
      (StatusCode::CREATED, Json(body)).into_response()
    }
    Err(e) => internal_error(e),
  }
}

/// `GET /api-keys` — org's keys, metadata only.
pub async fn list(
  State(state): State<AppState>,
  principal: crate::auth::PlatformPrincipal,
) -> Response {
  match state.registry().list_api_keys(principal.0.org_id()).await {
    Ok(keys) => Json(keys.into_iter().map(meta).collect::<Vec<_>>()).into_response(),
    Err(e) => internal_error(e),
  }
}

/// `DELETE /api-keys/{keyId}` — revoke (idempotent on already-revoked; 404
/// for unknown / other-org keys).
pub async fn revoke(
  State(state): State<AppState>,
  principal: crate::auth::PlatformPrincipal,
  Path(key_id): Path<Uuid>,
) -> Response {
  match state
    .registry()
    .revoke_api_key(principal.0.org_id(), key_id)
    .await
  {
    Ok(()) => StatusCode::NO_CONTENT.into_response(),
    Err(sectr_common::registry::RegistryError::NotFound(_)) => {
      StatusCode::NOT_FOUND.into_response()
    }
    Err(e) => internal_error(e),
  }
}

fn bad_request(message: &str) -> Response {
  (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

fn internal_error(e: impl std::fmt::Display) -> Response {
  tracing::error!(error = %e, "control-plane request failed");
  (
    StatusCode::INTERNAL_SERVER_ERROR,
    Json(json!({ "error": "internal error" })),
  )
    .into_response()
}
