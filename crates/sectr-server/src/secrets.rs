//! Secrets — the write-only control-plane surface:
//!
//! - `GET    /secrets?app=<slug>`   → names + timestamps ONLY
//! - `PUT    /secrets/{key}?app=<slug>`  `{value}` → 204 (upsert)
//! - `DELETE /secrets/{key}?app=<slug>` → 204 (idempotent)
//!
//! Contract: values are never readable by
//! ANY client API — the only read that exposes them is
//! `Registry::resolve_secrets`, consumed by the dispatch path (assignment
//! env). At rest the values are sealed boxes under
//! `SECTR_SECRETS_MASTER_KEY` (`sectr_common::secret_cipher`); when the
//! master key is absent the registry is unwrapped and these endpoints
//! answer 503 (dev convenience; the prod compose always sets the key).
//!
//! App scoping decision: secrets are PER-APP (data-model.sql
//! `secrets.app_id`), addressed by the `app` query parameter (the app's
//! slug) — the yaml's flat `/secrets` path is kept, qualified by the param.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, routing::get};
use chrono::{DateTime, Utc};
use sectr_common::api::{ErrorBody, SecretValue};
use sectr_common::secret::Secret;
use sectr_common::sidecar::{APPROVAL_POLICY_ENV, APPROVAL_POLICY_ENV_IS_RESERVED};
use serde::{Deserialize, Serialize};

use crate::auth::PlatformPrincipal;
use crate::state::AppState;

pub fn secrets_router() -> axum::Router<AppState> {
  axum::Router::new()
    .route("/secrets", get(list))
    .route("/secrets/{key}", axum::routing::put(store).delete(delete))
}

#[derive(Deserialize)]
struct AppQuery {
  app: String,
}

/// Env-safe keys only: secrets become environment variables inside the
/// runner, so the same lexical rule applies (letter/underscore first).
fn valid_key(key: &str) -> bool {
  !key.is_empty()
    && key.len() <= 128
    && key
      .chars()
      .next()
      .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One secret's metadata, as `GET /secrets` lists it (never the value).
#[derive(Serialize)]
struct SecretDto {
  name: String,
  created_at: DateTime<Utc>,
  updated_at: Option<DateTime<Utc>>,
}

fn secrets_disabled() -> Response {
  ErrorBody::response(
    StatusCode::SERVICE_UNAVAILABLE,
    "secrets are not configured on this server (SECTR_SECRETS_MASTER_KEY missing)",
  )
}

/// Resolve the org-scoped app by slug. Foreign-org apps are 404s (no
/// existence leaks) — same authz shape as the session routes.
async fn resolve_app(
  state: &AppState,
  principal: &PlatformPrincipal,
  slug: &str,
) -> Result<sectr_common::registry::App, Box<Response>> {
  state
    .registry()
    .find_app_by_slug(principal.0.org_id(), slug)
    .await
    .map_err(internal_error)?
    .ok_or_else(|| Box::new(ErrorBody::response(StatusCode::NOT_FOUND, "app not found")))
}

/// `GET /secrets?app=<slug>` — metadata only.
async fn list(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  Query(q): Query<AppQuery>,
) -> Response {
  if !state.secrets_enabled() {
    return secrets_disabled();
  }
  let app = match resolve_app(&state, &principal, &q.app).await {
    Ok(app) => app,
    Err(resp) => return *resp,
  };
  match state.registry().list_secret_names(app.id).await {
    Ok(metas) => Json(
      metas
        .into_iter()
        .map(|m| SecretDto {
          name: m.key,
          created_at: m.created_at,
          updated_at: m.updated_at,
        })
        .collect::<Vec<_>>(),
    )
    .into_response(),
    Err(e) => internal_error(e),
  }
}

/// `PUT /secrets/{key}?app=<slug>` — create or replace; value never echoed.
async fn store(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  Path(key): Path<String>,
  Query(q): Query<AppQuery>,
  Json(body): Json<SecretValue>,
) -> Response {
  if !state.secrets_enabled() {
    return secrets_disabled();
  }
  if !valid_key(&key) {
    return ErrorBody::response(
      StatusCode::UNPROCESSABLE_ENTITY,
      "secret keys must be env-safe: [A-Za-z_][A-Za-z0-9_]* (max 128)",
    );
  }
  // Sectr sets it on every runner, over any secret: storing it would
  // silently do nothing.
  if key == APPROVAL_POLICY_ENV {
    return ErrorBody::response(
      StatusCode::UNPROCESSABLE_ENTITY,
      format!("{APPROVAL_POLICY_ENV} can't be a secret: {APPROVAL_POLICY_ENV_IS_RESERVED}"),
    );
  }
  let app = match resolve_app(&state, &principal, &q.app).await {
    Ok(app) => app,
    Err(resp) => return *resp,
  };
  match state
    .registry()
    .set_secret(app.id, &key, Secret::new(body.value))
    .await
  {
    Ok(()) => StatusCode::NO_CONTENT.into_response(),
    Err(e) => internal_error(e),
  }
}

/// `DELETE /secrets/{key}?app=<slug>` — idempotent 204.
async fn delete(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  Path(key): Path<String>,
  Query(q): Query<AppQuery>,
) -> Response {
  if !state.secrets_enabled() {
    return secrets_disabled();
  }
  let app = match resolve_app(&state, &principal, &q.app).await {
    Ok(app) => app,
    Err(resp) => return *resp,
  };
  match state.registry().delete_secret(app.id, &key).await {
    Ok(()) => StatusCode::NO_CONTENT.into_response(),
    Err(e) => internal_error(e),
  }
}

fn internal_error(e: impl std::fmt::Display) -> Response {
  tracing::error!(error = %e, "secrets request failed");
  ErrorBody::response(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

#[cfg(test)]
mod tests {
  use axum::body::Body;
  use serde_json::{Value, json};
  use std::sync::Arc;
  use tower::ServiceExt;
  use uuid::Uuid;

  use sectr_common::registry::{ApiKeyKind, EncryptedRegistry, NewApp, Registry};
  use sectr_common::secret_cipher::SecretCipher;
  use sectr_common::store::PgStore;

  use crate::config::ServerConfig;
  use crate::test_pg;
  use crate::{auth::keys, http, state::AppState};

  struct World {
    app: axum::Router,
    platform: String,
    invocation: String,
    own_app_slug: String,
    own_app_id: Uuid,
    foreign_app_slug: String,
    /// The UNWRAPPED registry handle — proves what actually hit disk.
    raw_registry: Arc<dyn Registry>,
    journal: Arc<dyn sectr_common::journal::Journal>,
  }

  fn test_config(master_key: Option<String>) -> ServerConfig {
    ServerConfig {
      endpoints: Default::default(),
      pg_url: "postgres://unused".into(),
      bind_addr: "127.0.0.1:0".parse().unwrap(),
      secrets_master_key: master_key,
      docker_socket: "/var/run/docker.sock".into(),
      bootstrap_platform_key: None,
      org_name: "test-org".into(),
      tail_pool_max: 32,
      platform: crate::config::PlatformKind::Docker,
      posthog_key: None,
      posthog_host: "https://us.i.posthog.com".into(),
      signup_webhook_url: None,
      clerk: None,
      limits: crate::limits::LimitsConfig::none(),
      artifact_backend: crate::config::ArtifactBackend::Fs,
      artifact_dir: "/tmp/sectr-test-artifacts".into(),
      gcs_bucket: None,
      gcs_endpoint: None,
      gcp_project_id: None,
      gcp_region: None,
      gcp_ar_repo: "sectr-runners".into(),
      gcp_build_sa_email: None,
      gcp_push_sa_email: None,
      gcp_runner_sa_email: None,
      e2b_api_key: None,
      e2b_domain: "e2b.app".into(),
      e2b_cpu_count: 2,
      e2b_memory_mb: 1024,
      e2b_sandbox_timeout_secs: 3600,
      e2b_puller_sa_json: None,
      lease_ttl_secs: 30,
      user_token_ttl_secs: 86_400,
      mint_rate_limit_per_hour: 240,
      runner_server_url: "ws://172.17.0.1:8080/ingest".into(),
      runner_sidecar_cmd: vec!["/usr/local/bin/sectr-worker".into()],
      runner_image: None,
      runner_base_image: None,
      runner_network: None,
      runner_resources: Default::default(),
      build_dir: "/tmp/sectr-build".into(),
      dashboard_url: "https://app.sectr.test".into(),
    }
  }

  async fn world() -> Option<World> {
    let store: PgStore = test_pg::fresh_store().await?;
    let raw_registry: Arc<dyn Registry> = Arc::new(store.registry());
    let cipher = Arc::new(SecretCipher::from_env_value(
      "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
    ));
    let registry: Arc<dyn Registry> =
      Arc::new(EncryptedRegistry::new(raw_registry.clone(), cipher));
    let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());

    let org = registry.create_org("test-org", "default").await.unwrap();
    let own = registry
      .create_app(NewApp {
        org_id: org.id,
        name: "own".into(),
        slug: "own-app".into(),
      })
      .await
      .unwrap();

    // A second org + app for authz checks.
    let foreign_org = registry.create_org("other-org", "other").await.unwrap();
    let foreign = registry
      .create_app(NewApp {
        org_id: foreign_org.id,
        name: "other".into(),
        slug: "foreign-app".into(),
      })
      .await
      .unwrap();
    let _ = foreign;

    let platform = keys::mint(ApiKeyKind::Platform);
    registry
      .create_api_key(
        org.id,
        None,
        ApiKeyKind::Platform,
        "secrets test platform",
        &keys::hash(&platform),
      )
      .await
      .unwrap();
    let invocation = keys::mint(ApiKeyKind::Invocation);
    registry
      .create_api_key(
        org.id,
        Some(own.id),
        ApiKeyKind::Invocation,
        "secrets test invocation",
        &keys::hash(&invocation),
      )
      .await
      .unwrap();

    let dispatcher = crate::dispatch::UnimplementedDispatcher;
    let service = Arc::new(crate::sessions::ProdSessionService::new(
      registry.clone(),
      journal.clone(),
      Arc::new(dispatcher),
      chrono::Duration::seconds(30),
    ));
    let hub = crate::ingest::IngestHub::new(
      registry.clone(),
      journal.clone(),
      chrono::Duration::seconds(30),
    );
    let config = test_config(Some("test-key".into()));
    let state = AppState::new(
      registry.clone(),
      journal.clone(),
      service,
      Arc::new(hub),
      config,
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      Arc::new(crate::platform::DockerPlatform::new(
        Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
        None,
        test_config(Some("test-key".into())),
        registry.clone(),
      )),
      true,
    );
    Some(World {
      app: http::router(state.clone()),
      platform,
      invocation,
      own_app_slug: own.slug,
      own_app_id: own.id,
      foreign_app_slug: "foreign-app".into(),
      raw_registry,
      journal,
    })
  }

  async fn req(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
  ) -> (axum::http::StatusCode, Value) {
    let builder = axum::http::Request::builder()
      .method(method)
      .uri(uri)
      .header("Authorization", format!("Bearer {}", token.unwrap_or("")));
    let request = builder
      .header("Content-Type", "application/json")
      .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
      .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
      .await
      .unwrap();
    let body = if bytes.is_empty() {
      Value::Null
    } else {
      serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
  }

  #[tokio::test]
  async fn put_list_delete_roundtrip_values_never_returned() {
    let Some(w) = world().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let base = format!("/secrets?app={}", w.own_app_slug);

    let (status, _) = req(
      &w.app,
      "PUT",
      &format!("/secrets/OPENAI_API_KEY?app={}", w.own_app_slug),
      Some(&w.platform),
      Some(json!({ "value": "sk-live-123" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    // Names only — no value field ever.
    let (status, body) = req(&w.app, "GET", &base, Some(&w.platform), None).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body[0]["name"], "OPENAI_API_KEY");
    assert!(body[0]["created_at"].is_string());
    assert!(body[0]["updated_at"].is_null());
    assert!(body.get("value").is_none());
    assert!(
      !serde_json::to_string(&body)
        .unwrap()
        .contains("sk-live-123")
    );

    // At rest: the UNWRAPPED registry (what the dispatcher wraps) sees
    // a v1 sealed box, never the plaintext.
    let stored = w.raw_registry.resolve_secrets(w.own_app_id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert!(stored[0].1.expose().starts_with("v1:"));
    assert!(!stored[0].1.expose().contains("sk-live-123"));

    // Replacing the value stamps updated_at; created_at stays.
    let created_at = body[0]["created_at"].clone();
    let (status, _) = req(
      &w.app,
      "PUT",
      &format!("/secrets/OPENAI_API_KEY?app={}", w.own_app_slug),
      Some(&w.platform),
      Some(json!({ "value": "sk-live-456" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let (_, body) = req(&w.app, "GET", &base, Some(&w.platform), None).await;
    assert_eq!(body[0]["created_at"], created_at);
    assert!(body[0]["updated_at"].is_string());

    // DELETE is idempotent 204.
    for _ in 0..2 {
      let (status, _) = req(
        &w.app,
        "DELETE",
        &format!("/secrets/OPENAI_API_KEY?app={}", w.own_app_slug),
        Some(&w.platform),
        None,
      )
      .await;
      assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    }
    let (_, body) = req(&w.app, "GET", &base, Some(&w.platform), None).await;
    assert_eq!(body.as_array().unwrap().len(), 0);
  }

  #[tokio::test]
  async fn foreign_app_is_a_404_and_invocation_keys_are_403() {
    let Some(w) = world().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    // Foreign org's app: 404, no existence leak.
    let (status, _) = req(
      &w.app,
      "PUT",
      &format!("/secrets/K?app={}", w.foreign_app_slug),
      Some(&w.platform),
      Some(json!({ "value": "x" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // Invocation keys are platform-class-only on control-plane routes.
    let (status, _) = req(
      &w.app,
      "PUT",
      &format!("/secrets/K?app={}", w.own_app_slug),
      Some(&w.invocation),
      Some(json!({ "value": "x" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
  }

  #[tokio::test]
  async fn unknown_app_is_404_and_bad_key_shape_is_422() {
    let Some(w) = world().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let (status, _) = req(
      &w.app,
      "PUT",
      "/secrets/K?app=nope",
      Some(&w.platform),
      Some(json!({ "value": "x" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // Not env-safe → the secret could never become an env var.
    let (status, _) = req(
      &w.app,
      "PUT",
      &format!("/secrets/9bad-key?app={}", w.own_app_slug),
      Some(&w.platform),
      Some(json!({ "value": "x" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);

    // Sectr sets the approval policy variable itself, over any secret.
    let (status, body) = req(
      &w.app,
      "PUT",
      &format!("/secrets/SECTR_APPROVAL_POLICY?app={}", w.own_app_slug),
      Some(&w.platform),
      Some(json!({ "value": "[\"refund\"]" })),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("[deploy] approval_tools"), "{error}");
  }

  #[tokio::test]
  async fn disabled_without_a_master_key() {
    let Some(w) = world().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    // Rebuild the router with secrets_enabled = false (no master key).
    // Cheap approximation: the flag is what handlers check; the
    // main()-level wiring is covered by the enabled world above.
    let base = format!("/secrets?app={}", w.own_app_slug);
    let (status, _) = req(&w.app, "GET", &base, Some(&w.platform), None).await;
    assert_eq!(status, axum::http::StatusCode::OK); // sanity: enabled world

    // The 503 path: exercise the handler-level gate directly.
    let app = disabled_state(w.raw_registry.clone(), w.journal.clone());
    for (method, uri, body) in [
      ("GET", base.as_str(), None),
      (
        "PUT",
        &format!("/secrets/K?app={}", w.own_app_slug),
        Some(json!({ "value": "v" })),
      ),
      (
        "DELETE",
        &format!("/secrets/K?app={}", w.own_app_slug),
        None,
      ),
    ] {
      let (status, err) = req(&app, method, uri, Some(&w.platform), body).await;
      assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
      assert!(err["error"].as_str().unwrap().contains("not configured"));
    }
  }

  fn disabled_state(
    raw: Arc<dyn Registry>,
    journal: Arc<dyn sectr_common::journal::Journal>,
  ) -> axum::Router {
    let service = Arc::new(crate::sessions::ProdSessionService::new(
      raw.clone(),
      journal.clone(),
      Arc::new(crate::dispatch::UnimplementedDispatcher),
      chrono::Duration::seconds(30),
    ));
    let hub =
      crate::ingest::IngestHub::new(raw.clone(), journal.clone(), chrono::Duration::seconds(30));
    let config = test_config(None);
    let state = AppState::new(
      raw.clone(),
      journal,
      service,
      Arc::new(hub),
      config,
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      Arc::new(crate::platform::DockerPlatform::new(
        Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
        None,
        test_config(None),
        raw,
      )),
      false,
    );
    http::router(state)
  }
}
