//! Clerk signup — `POST /auth/clerk` exchanges a Clerk session JWT
//! for a short-lived, dashboard-only platform key, and `POST /auth/logout`
//! revokes it.
//!
//! Clerk authenticates the HUMAN (GitHub / Google / email-code sign-in in
//! the dashboard);
//! Sectr still AUTHORIZES with platform keys. The exchanged key is an
//! ordinary `platform` row with an `expires_at`, so the auth middleware,
//! `Principal::Platform`, `GET /me`, SSE and the CLI see nothing new — expiry
//! is already part of key liveness at lookup (`find_api_key_by_hash`).
//!
//! Account resolution: a user row with the Clerk `sub` (returning user),
//! else a brand-new org + user. The Clerk `sub` is the ONLY identity key —
//! no linking by email (decided 2026-10-04: there are no alpha users
//! to carry over, and email-based linking would make the email claim an
//! account-ownership credential).
//!
//! Deliberate limits:
//! - JWTs are verified networklessly against the instance's public key
//!   (`SECTR_CLERK_JWT_KEY`); no JWKS fetch, no Clerk Backend API.
//! - The token travels in the BODY, never `Authorization`, so this route can
//!   never be confused with an API-key bearer.
//! - Failures say nothing on the wire about WHY a token was rejected (details
//!   at `debug`). The minted key rides the response once and is never logged.
//! - Storage goes through inherent [`PgRegistry`] methods (prod-Postgres-only);
//!   the SQLite dev store answers 503.
//!
//! Analytics: a brand-new account fires `org_created` with prop
//! `signup: "clerk"`. Fire-and-forget; PostHog can never fail a sign-in.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::{SubsecRound, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::json;

use sectr_common::registry::{ApiKeyKind, Org, PgRegistry, Registry, RegistryError};

use crate::analytics::events;
use crate::auth::{PlatformPrincipal, keys};
use crate::config::ClerkConfig;
use crate::state::AppState;

/// Name of the exchanged key; the dashboard hides rows with an `expires_at`
/// behind a "browser sessions" toggle.
const SESSION_KEY_NAME: &str = "dashboard session (Clerk)";

/// Clock-skew tolerance for `exp` / `nbf`.
const LEEWAY_SECS: u64 = 5;

/// Anonymous router (mounted outside the bearer middleware).
/// `POST /auth/logout` lives in the authed control router.
pub fn clerk_router() -> axum::Router<AppState> {
  axum::Router::new().route("/auth/clerk", post(exchange))
}

#[derive(Debug, Deserialize)]
struct ExchangeRequest {
  session_token: String,
}

#[derive(Debug, Deserialize)]
struct Claims {
  sub: String,
  azp: Option<String>,
  email: Option<String>,
}

/// A verified Clerk session: the stable Clerk user id and the (verified by
/// the provider or by Clerk's email code, lowercased) primary email from the
/// custom `email` claim.
#[derive(Debug, PartialEq, Eq)]
pub struct ClerkSession {
  pub sub: String,
  pub email: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
  /// Bad signature / algorithm / issuer / `azp` / expiry — a 401.
  Invalid,
  /// Authentic token without the `email` claim — an operator
  /// misconfiguration of the Clerk session-token template (a 500).
  MissingEmail,
}

/// Verify a Clerk session JWT: RS256 signature against the configured public
/// key, `iss`, `exp`/`nbf` (5 s leeway), `sub` present, `azp` in the
/// authorized parties — then the custom `email` claim. The library does not
/// check `azp`, so it is checked here. `MissingEmail` is only reported for a
/// token that is otherwise valid, so a forged token can never reach a 500.
pub fn verify_session(config: &ClerkConfig, token: &str) -> Result<ClerkSession, VerifyError> {
  let key = DecodingKey::from_rsa_pem(config.jwt_key_pem.as_bytes()).map_err(|e| {
    // Validated at startup (config.rs); unreachable unless that regresses.
    tracing::error!(error = %e, "configured Clerk JWT key does not parse");
    VerifyError::Invalid
  })?;

  let mut validation = Validation::new(Algorithm::RS256);
  validation.leeway = LEEWAY_SECS;
  validation.validate_nbf = true;
  // Clerk session tokens carry no `aud`; do not let a JWT-template `aud`
  // trip the library's audience check (we pin `azp` instead).
  validation.validate_aud = false;
  validation.set_issuer(&[config.issuer.as_str()]);
  validation.set_required_spec_claims(&["exp", "iss", "sub"]);

  let claims = jsonwebtoken::decode::<Claims>(token, &key, &validation)
    .map_err(|e| {
      tracing::debug!(error = %e, "Clerk session token rejected");
      VerifyError::Invalid
    })?
    .claims;

  match claims.azp.as_deref() {
    Some(azp) if config.authorized_parties.iter().any(|p| p == azp) => {}
    other => {
      tracing::debug!(azp = ?other, "Clerk session token azp not authorized");
      return Err(VerifyError::Invalid);
    }
  }

  let email = claims
    .email
    .map(|e| e.trim().to_lowercase())
    .filter(|e| !e.is_empty())
    .ok_or(VerifyError::MissingEmail)?;
  Ok(ClerkSession {
    sub: claims.sub,
    email,
  })
}

/// `{"error": …}` with a status — the endpoint's only error shape.
fn error(status: StatusCode, msg: &str) -> Response {
  (status, Json(json!({ "error": msg }))).into_response()
}

fn internal_error(e: impl std::fmt::Display) -> Response {
  // Deliberately vague on the wire (details are in the server log).
  tracing::error!(error = %e, "clerk sign-in failed");
  error(
    StatusCode::INTERNAL_SERVER_ERROR,
    "sign-in failed — try again",
  )
}

/// `POST /auth/clerk` — `{"session_token": "<Clerk JWT>"}` →
/// `{"key", "org_slug", "created", "expires_at"}`.
async fn exchange(State(state): State<AppState>, Json(req): Json<ExchangeRequest>) -> Response {
  let (Some(clerk), Some(pg)) = (state.config().clerk.as_ref(), state.pg_registry()) else {
    return error(
      StatusCode::SERVICE_UNAVAILABLE,
      "Clerk sign-in is unavailable on this deployment",
    );
  };

  let session = match verify_session(clerk, &req.session_token) {
    Ok(session) => session,
    Err(VerifyError::Invalid) => {
      return error(StatusCode::UNAUTHORIZED, "invalid Clerk session");
    }
    Err(VerifyError::MissingEmail) => {
      tracing::error!(
        "Clerk session token is missing the email claim; configure the session token template"
      );
      return error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "sign-in is misconfigured — contact support",
      );
    }
  };

  let (org, created) = match resolve_account(&state, pg, &session).await {
    Ok(resolved) => resolved,
    Err(response) => return response,
  };

  // Mint + store the dashboard-session key; the full value rides the
  // response exactly once (the store only sees the hash). Never logged.
  // Microseconds, as Postgres stores it — the response matches `GET /api-keys`.
  let expires_at =
    (Utc::now() + chrono::Duration::seconds(clerk.session_ttl_secs as i64)).trunc_subsecs(6);
  let full_key = keys::mint(ApiKeyKind::Platform);
  if let Err(e) = pg
    .create_api_key_expiring(
      org.id,
      ApiKeyKind::Platform,
      SESSION_KEY_NAME,
      &keys::hash(&full_key),
      Some(expires_at),
    )
    .await
  {
    return internal_error(e);
  }

  Json(json!({
      "key": full_key,
      "org_slug": org.slug,
      "created": created,
      "expires_at": expires_at,
  }))
  .into_response()
}

/// Returning Clerk user → their org; anyone else → a new account, unless
/// the signup cap is reached. `created` is true only for the new-account
/// branch. Errors come back as ready-to-return responses.
#[allow(clippy::result_large_err)] // the Err IS the ready-to-return HTTP response
async fn resolve_account(
  state: &AppState,
  pg: &PgRegistry,
  session: &ClerkSession,
) -> Result<(Org, bool), Response> {
  // Returning user — never capped.
  if let Some(org) = returning_org(pg, &session.sub)
    .await
    .map_err(internal_error)?
  {
    return Ok((org, false));
  }

  // New account. The signup cap (403 `beta_full`) gates here,
  // before the org exists.
  crate::signup::ensure_signup_open(pg, state.config().limits.signup_cap).await?;

  // Org + user are created in ONE transaction.
  let slug = slug_for_email(&session.email);
  let local = session
    .email
    .split('@')
    .next()
    .unwrap_or(&session.email)
    .to_owned();
  let org = match pg
    .create_clerk_account(
      &slug,
      &suffixed(&slug),
      &session.email,
      Some(&local),
      &session.sub,
    )
    .await
  {
    Ok(org) => org,
    Err(RegistryError::Conflict(detail)) => {
      // A concurrent first sign-in for this `sub` (two tabs finishing OAuth
      // together) committed first; ours rolled back whole. Resolve it as
      // the returning user it now is — both tabs land in the same org.
      if let Some(org) = returning_org(pg, &session.sub)
        .await
        .map_err(internal_error)?
      {
        return Ok((org, false));
      }
      return Err(internal_error(RegistryError::Conflict(detail)));
    }
    Err(e) => return Err(internal_error(e)),
  };

  // Top of the activation funnel. Fresh orgs only.
  state
    .analytics()
    .track(events::ORG_CREATED, org.id, json!({ "signup": "clerk" }));
  state
    .signup_notifier()
    .new_account(pg, state.config().limits.signup_cap);
  Ok((org, true))
}

/// The org of the user with this Clerk id, if any.
async fn returning_org(pg: &PgRegistry, sub: &str) -> Result<Option<Org>, RegistryError> {
  match pg.find_user_by_clerk_id(sub).await? {
    Some((_user_id, org_id)) => Ok(Some(pg.get_org(org_id).await?)),
    None => Ok(None),
  }
}

/// `jane@acme.dev` → `jane-acme-dev`: the new org's slug (org-per-person).
/// Not unique on its own: `a.b@x.com` and `a-b@x.com` both derive
/// `a-b-x-com`, so account creation falls back to [`suffixed`].
fn slug_for_email(email: &str) -> String {
  let (local, domain) = email.split_once('@').unwrap_or((email, ""));
  let mut slug = String::new();
  for c in format!("{local}-{domain}").chars() {
    if c.is_ascii_alphanumeric() {
      slug.push(c.to_ascii_lowercase());
    } else if !slug.ends_with('-') {
      slug.push('-');
    }
  }
  slug.trim_matches('-').to_owned()
}

/// The fallback slug when `slug` is taken: `slug-<4 random hex>`.
fn suffixed(slug: &str) -> String {
  let mut suffix = [0u8; 2];
  getrandom::getrandom(&mut suffix).expect("OS entropy");
  format!("{slug}-{:02x}{:02x}", suffix[0], suffix[1])
}

/// `POST /auth/logout` — revoke the PRESENTED key, but only if it has an
/// `expires_at` (a dashboard-session key). A pasted long-lived key is never
/// revoked by signing out. 204 for any live platform key; a repeat call with
/// the now-revoked key never gets here (the auth middleware answers 401).
pub async fn logout(State(state): State<AppState>, principal: PlatformPrincipal) -> Response {
  let org_id = principal.0.org_id();
  let key_id = principal.0.key_id();
  let keys = match state.registry().list_api_keys(org_id).await {
    Ok(keys) => keys,
    Err(e) => return internal_error(e),
  };
  if keys
    .iter()
    .any(|k| k.id == key_id && k.expires_at.is_some())
    && let Err(e) = state.registry().revoke_api_key(org_id, key_id).await
  {
    return internal_error(e);
  }
  StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
  use super::*;

  use axum::body::Body;
  use jsonwebtoken::{EncodingKey, Header};
  use sectr_common::store::{PgStore, SqliteStore};
  use serde_json::Value;
  use std::sync::Arc;
  use tower::ServiceExt;

  use crate::analytics::Analytics;
  use crate::config::ServerConfig;
  use crate::test_pg;

  use crate::testkeys::{self, TestKey};

  const ISSUER: &str = "https://clerk.sectr.test";
  const PARTY: &str = "https://app.sectr.test";

  fn clerk_config(ttl: u64) -> ClerkConfig {
    ClerkConfig {
      issuer: ISSUER.into(),
      jwt_key_pem: testkeys::primary().public_pem.clone(),
      authorized_parties: vec![PARTY.into(), "http://localhost:3000".into()],
      session_ttl_secs: ttl,
    }
  }

  fn now() -> i64 {
    Utc::now().timestamp()
  }

  /// Clerk-shaped claims; tests override single fields.
  fn claims(sub: &str, email: Option<&str>) -> Value {
    let mut c = json!({
        "sub": sub,
        "sid": "sess_1",
        "iss": ISSUER,
        "azp": PARTY,
        "iat": now(),
        "nbf": now() - 10,
        "exp": now() + 60,
    });
    if let Some(email) = email {
      c["email"] = json!(email);
    }
    c
  }

  fn sign_with(key: &TestKey, claims: &Value) -> String {
    jsonwebtoken::encode(
      &Header::new(Algorithm::RS256),
      claims,
      &EncodingKey::from_rsa_pem(key.private_pem.as_bytes()).unwrap(),
    )
    .unwrap()
  }

  fn sign(claims: &Value) -> String {
    sign_with(testkeys::primary(), claims)
  }

  #[test]
  fn slug_derivation_is_stable_and_boring() {
    assert_eq!(slug_for_email("jane@acme.dev"), "jane-acme-dev");
    assert_eq!(slug_for_email("JANE+test@acme.dev"), "jane-test-acme-dev");
    assert_eq!(slug_for_email("a@b"), "a-b");
  }

  // -- JWT verification (unit) -------------------------------------------

  #[test]
  fn valid_token_verifies_and_normalizes_the_email() {
    let token = sign(&claims("user_1", Some("  Jane@Acme.dev ")));
    assert_eq!(
      verify_session(&clerk_config(60), &token),
      Ok(ClerkSession {
        sub: "user_1".into(),
        email: "jane@acme.dev".into()
      })
    );
  }

  #[test]
  fn rejects_expired_and_not_yet_valid_tokens() {
    let cfg = clerk_config(60);
    let mut expired = claims("user_1", Some("j@a.dev"));
    expired["exp"] = json!(now() - 60);
    assert_eq!(
      verify_session(&cfg, &sign(&expired)),
      Err(VerifyError::Invalid)
    );

    let mut early = claims("user_1", Some("j@a.dev"));
    early["nbf"] = json!(now() + 3600);
    assert_eq!(
      verify_session(&cfg, &sign(&early)),
      Err(VerifyError::Invalid)
    );

    // Inside the 5 s leeway is still fine.
    let mut skewed = claims("user_1", Some("j@a.dev"));
    skewed["nbf"] = json!(now() + 2);
    assert!(verify_session(&cfg, &sign(&skewed)).is_ok());
  }

  #[test]
  fn rejects_wrong_issuer_and_unauthorized_or_missing_azp() {
    let cfg = clerk_config(60);
    let mut wrong_iss = claims("user_1", Some("j@a.dev"));
    wrong_iss["iss"] = json!("https://evil.example");
    assert_eq!(
      verify_session(&cfg, &sign(&wrong_iss)),
      Err(VerifyError::Invalid)
    );

    let mut wrong_azp = claims("user_1", Some("j@a.dev"));
    wrong_azp["azp"] = json!("https://evil.example");
    assert_eq!(
      verify_session(&cfg, &sign(&wrong_azp)),
      Err(VerifyError::Invalid)
    );

    let mut no_azp = claims("user_1", Some("j@a.dev"));
    no_azp.as_object_mut().unwrap().remove("azp");
    assert_eq!(
      verify_session(&cfg, &sign(&no_azp)),
      Err(VerifyError::Invalid)
    );
  }

  #[test]
  fn rejects_hs256_alg_confusion_wrong_key_and_tampering() {
    let cfg = clerk_config(60);
    let c = claims("user_1", Some("j@a.dev"));

    // Classic alg confusion: HS256 "signed" with the PUBLIC key as secret.
    let forged = jsonwebtoken::encode(
      &Header::new(Algorithm::HS256),
      &c,
      &EncodingKey::from_secret(testkeys::primary().public_pem.as_bytes()),
    )
    .unwrap();
    assert_eq!(verify_session(&cfg, &forged), Err(VerifyError::Invalid));

    // Valid RS256, but signed by a different key.
    assert_eq!(
      verify_session(&cfg, &sign_with(testkeys::other(), &c)),
      Err(VerifyError::Invalid)
    );

    // Payload swapped after signing.
    let token = sign(&c);
    let parts: Vec<&str> = token.split('.').collect();
    let evil = {
      use base64::Engine as _;
      let mut tampered = c.clone();
      tampered["sub"] = json!("user_admin");
      base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tampered.to_string())
    };
    let tampered_token = format!("{}.{}.{}", parts[0], evil, parts[2]);
    assert_eq!(
      verify_session(&cfg, &tampered_token),
      Err(VerifyError::Invalid)
    );

    // Garbage.
    assert_eq!(verify_session(&cfg, "not.a.jwt"), Err(VerifyError::Invalid));
    assert_eq!(verify_session(&cfg, ""), Err(VerifyError::Invalid));
  }

  #[test]
  fn missing_email_is_distinct_from_invalid() {
    let cfg = clerk_config(60);
    assert_eq!(
      verify_session(&cfg, &sign(&claims("user_1", None))),
      Err(VerifyError::MissingEmail)
    );
    assert_eq!(
      verify_session(&cfg, &sign(&claims("user_1", Some("  ")))),
      Err(VerifyError::MissingEmail)
    );
    // …but a forged token with no email stays a plain 401.
    assert_eq!(
      verify_session(&cfg, &sign_with(testkeys::other(), &claims("user_1", None))),
      Err(VerifyError::Invalid)
    );
  }

  // -- HTTP harness ------------------------------------------------------

  fn test_config(clerk: Option<ClerkConfig>) -> ServerConfig {
    ServerConfig {
      pg_url: "postgres://unused".into(),
      bind_addr: "127.0.0.1:0".parse().unwrap(),
      secrets_master_key: None,
      docker_socket: "/var/run/docker.sock".into(),
      bootstrap_platform_key: None,
      org_name: "test-org".into(),
      tail_pool_max: 32,
      platform: crate::config::PlatformKind::Docker,
      posthog_key: None,
      posthog_host: "https://us.i.posthog.com".into(),
      signup_webhook_url: None,
      clerk,
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

  fn build_state(
    registry: Arc<dyn Registry>,
    journal: Arc<dyn sectr_common::journal::Journal>,
    config: ServerConfig,
  ) -> AppState {
    let service = Arc::new(crate::sessions::ProdSessionService::new(
      registry.clone(),
      journal.clone(),
      Arc::new(crate::dispatch::UnimplementedDispatcher),
      chrono::Duration::seconds(30),
    ));
    let hub = crate::ingest::IngestHub::new(
      registry.clone(),
      journal.clone(),
      chrono::Duration::seconds(30),
    );
    AppState::new(
      registry.clone(),
      journal,
      service,
      Arc::new(hub),
      config.clone(),
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      Arc::new(crate::platform::DockerPlatform::new(
        Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
        None,
        config,
        registry,
      )),
      false,
    )
  }

  struct World {
    router: axum::Router,
    pg: PgRegistry,
    captured: Arc<std::sync::Mutex<Vec<Value>>>,
    /// What the signup notifier posted to the stand-in webhook.
    notices: Arc<std::sync::Mutex<Vec<Value>>>,
  }

  /// PG-gated: `None` = skip (no test Postgres). Analytics is wired to the
  /// in-process capture stub so `org_created` emissions are observable.
  async fn world(ttl: u64) -> Option<World> {
    world_with_signup_cap(ttl, None).await
  }

  async fn world_with_signup_cap(ttl: u64, signup_cap: Option<u32>) -> Option<World> {
    let store: PgStore = test_pg::fresh_store().await?;
    let pg = store.registry();
    let registry: Arc<dyn Registry> = Arc::new(pg.clone());
    let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
    let (host, captured) = crate::analytics::testutil::capture_server().await;
    let (webhook, notices) = crate::signup::testutil::webhook_server().await;
    let mut config = test_config(Some(clerk_config(ttl)));
    config.limits.signup_cap = signup_cap;
    let state = build_state(registry, journal, config)
      .with_analytics(Arc::new(Analytics::new("phc_test", Some(host))))
      .with_signup_notifier(Arc::new(crate::signup::SignupNotifier::new(webhook)))
      .with_pg_registry(pg.clone());
    Some(World {
      router: crate::http::router(state),
      pg,
      captured,
      notices,
    })
  }

  async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
  ) -> (StatusCode, Value) {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(key) = bearer {
      builder = builder.header("Authorization", format!("Bearer {key}"));
    }
    let request = match body {
      Some(body) => builder
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap(),
      None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
      .await
      .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
  }

  async fn exchange_token(router: &axum::Router, claims: &Value) -> (StatusCode, Value) {
    call(
      router,
      "POST",
      "/auth/clerk",
      None,
      Some(json!({ "session_token": sign(claims) })),
    )
    .await
  }

  // -- HTTP tests --------------------------------------------------------

  #[tokio::test]
  async fn unconfigured_clerk_answers_503() {
    // SQLite-backed state: no concrete PgRegistry, no Clerk config.
    let path = std::env::temp_dir()
      .join(format!("sectr-clerk-test-{}.db", uuid::Uuid::now_v7()))
      .to_string_lossy()
      .into_owned();
    let store = SqliteStore::open(&path).await.unwrap();
    let state = build_state(
      Arc::new(store.registry()),
      Arc::new(store.journal()),
      test_config(None),
    );
    let router = crate::http::router(state);
    let (status, _) = exchange_token(&router, &claims("user_1", Some("j@a.dev"))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
  }

  #[tokio::test]
  async fn invalid_session_is_a_bare_401_and_missing_email_a_500() {
    let Some(w) = world(60).await else {
      return;
    };
    let (status, body) = call(
      &w.router,
      "POST",
      "/auth/clerk",
      None,
      Some(json!({ "session_token": "garbage" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body, json!({ "error": "invalid Clerk session" }));

    let (status, _) = exchange_token(&w.router, &claims("user_1", None)).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
  }

  #[tokio::test]
  async fn new_user_gets_org_user_and_expiring_key_and_one_org_created() {
    let Some(w) = world(3600).await else {
      return;
    };
    let (status, body) = exchange_token(&w.router, &claims("user_1", Some("Jane@Acme.dev"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["created"], true);
    assert_eq!(body["org_slug"], "jane-acme-dev");
    let key = body["key"].as_str().unwrap();
    assert!(key.starts_with("skp_"), "{key}");
    assert!(body["expires_at"].is_string());

    // org + user (with the Clerk id and the normalized email) + ONE
    // expiring platform key.
    let (user_id, org_id) = w.pg.find_user_by_clerk_id("user_1").await.unwrap().unwrap();
    let (email, name): (String, Option<String>) =
      sqlx::query_as("SELECT email, name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(w.pg.pool())
        .await
        .unwrap();
    assert_eq!(email, "jane@acme.dev");
    assert_eq!(name.as_deref(), Some("jane"));
    let stored = w.pg.list_api_keys(org_id).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].key_hash, keys::hash(key));
    assert!(stored[0].expires_at.is_some());
    assert_eq!(stored[0].name, SESSION_KEY_NAME);

    // The exchanged key authenticates like any platform key.
    let (status, me) = call(&w.router, "GET", "/me", Some(key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["org"]["id"], json!(org_id));

    // `GET /api-keys` lists it with its expiry (the dashboard hides these
    // behind its "browser sessions" toggle).
    let (status, listed) = call(&w.router, "GET", "/api-keys", Some(key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed[0]["name"], SESSION_KEY_NAME);
    assert_eq!(listed[0]["expires_at"], body["expires_at"]);

    // org_created fired once, tagged signup: clerk, actor = the org.
    let captures = crate::analytics::testutil::wait_for_captures(&w.captured, 1).await;
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0]["event"], events::ORG_CREATED);
    assert_eq!(captures[0]["properties"]["signup"], "clerk");
    assert_eq!(captures[0]["distinct_id"], org_id.to_string());
  }

  #[tokio::test]
  async fn returning_user_reuses_the_org_with_a_fresh_key_and_no_second_org_created() {
    let Some(w) = world(3600).await else {
      return;
    };
    let c = claims("user_1", Some("jane@acme.dev"));
    let (_, first) = exchange_token(&w.router, &c).await;
    crate::analytics::testutil::wait_for_captures(&w.captured, 1).await;

    let (status, second) = exchange_token(&w.router, &c).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["created"], false);
    assert_eq!(second["org_slug"], first["org_slug"]);
    assert_ne!(second["key"], first["key"]);

    let (_, org_id) = w.pg.find_user_by_clerk_id("user_1").await.unwrap().unwrap();
    assert_eq!(w.pg.list_api_keys(org_id).await.unwrap().len(), 2);

    // A returning user is not a new org: still exactly one capture.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(w.captured.lock().unwrap().len(), 1);
  }

  #[tokio::test]
  async fn a_new_account_posts_a_signup_notice_without_pii() {
    let Some(w) = world_with_signup_cap(3600, Some(1000)).await else {
      return;
    };
    let c = claims("user_1", Some("jane@acme.dev"));
    let (status, body) = exchange_token(&w.router, &c).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let notices = crate::analytics::testutil::wait_for_captures(&w.notices, 1).await;
    assert_eq!(
      notices,
      vec![json!({
        "content": "New signup: account #1 of 1000",
        "allowed_mentions": { "parse": [] },
      })]
    );

    // A returning user isn't a signup.
    exchange_token(&w.router, &c).await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(w.notices.lock().unwrap().len(), 1);

    // The next account counts on.
    exchange_token(&w.router, &claims("user_2", Some("sam@example.com"))).await;
    let notices = crate::analytics::testutil::wait_for_captures(&w.notices, 2).await;
    assert_eq!(notices[1]["content"], "New signup: account #2 of 1000");
  }

  #[tokio::test]
  async fn signup_cap_refuses_new_accounts_but_never_returning_users() {
    let Some(w) = world_with_signup_cap(3600, Some(1)).await else {
      return;
    };
    // The first account fills the cap.
    let jane = claims("user_1", Some("jane@acme.dev"));
    let (status, _) = exchange_token(&w.router, &jane).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(w.pg.count_orgs().await.unwrap(), 1);
    crate::analytics::testutil::wait_for_captures(&w.captured, 1).await;

    // A new `sub` is refused with `beta_full`, and nothing is created.
    let (status, body) = exchange_token(&w.router, &claims("user_2", Some("bob@acme.dev"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "beta_full");
    assert_eq!(body["error"], crate::signup::BETA_FULL_MESSAGE);
    assert!(
      w.pg
        .find_user_by_clerk_id("user_2")
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(w.pg.count_orgs().await.unwrap(), 1, "no org");
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
      .fetch_one(w.pg.pool())
      .await
      .unwrap();
    assert_eq!(users, 1, "no user");
    let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys")
      .fetch_one(w.pg.pool())
      .await
      .unwrap();
    assert_eq!(keys, 1, "no key");

    // A returning user still gets in with a fresh key.
    let (status, body) = exchange_token(&w.router, &jane).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["created"], false);

    // Neither the refusal nor the return created an org.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(w.captured.lock().unwrap().len(), 1);
  }

  #[tokio::test]
  async fn same_email_under_a_different_sub_is_a_separate_account() {
    let Some(w) = world(3600).await else {
      return;
    };
    // No linking by email: the Clerk `sub` is the only identity key (e.g. a
    // deleted-and-recreated Clerk account gets a fresh org, not the old one).
    let (status, first) = exchange_token(&w.router, &claims("user_1", Some("jane@acme.dev"))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, second) =
      exchange_token(&w.router, &claims("user_2", Some("jane@acme.dev"))).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["created"], true);
    assert_ne!(second["org_slug"], first["org_slug"]);

    let (_, org_1) = w.pg.find_user_by_clerk_id("user_1").await.unwrap().unwrap();
    let (_, org_2) = w.pg.find_user_by_clerk_id("user_2").await.unwrap().unwrap();
    assert_ne!(org_1, org_2);
  }

  #[tokio::test]
  async fn slug_collision_retries_once_with_a_hex_suffix() {
    let Some(w) = world(3600).await else {
      return;
    };
    // `a.b@x.com` and `a-b@x.com` both derive `a-b-x-com`.
    let (_, first) = exchange_token(&w.router, &claims("user_1", Some("a.b@x.com"))).await;
    assert_eq!(first["org_slug"], "a-b-x-com");
    let (status, second) = exchange_token(&w.router, &claims("user_2", Some("a-b@x.com"))).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let slug = second["org_slug"].as_str().unwrap();
    let suffix = slug.strip_prefix("a-b-x-com-").expect(slug);
    assert_eq!(suffix.len(), 4);
    assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()), "{suffix}");
    assert_eq!(second["created"], true);
  }

  #[tokio::test]
  async fn concurrent_first_sign_ins_share_one_org_and_leave_no_orphan() {
    let Some(w) = world(3600).await else {
      return;
    };
    // Two tabs finishing OAuth together, several times over: both lookups
    // miss, both try to create the account; exactly one transaction wins
    // and the other resolves as a returning user.
    for i in 0..5 {
      let c = claims(&format!("user_{i}"), Some(&format!("u{i}@race.dev")));
      let (a, b) = tokio::join!(exchange_token(&w.router, &c), exchange_token(&w.router, &c));
      assert_eq!(a.0, StatusCode::OK, "{}", a.1);
      assert_eq!(b.0, StatusCode::OK, "{}", b.1);
      assert_eq!(a.1["org_slug"], b.1["org_slug"]);
      assert_ne!(a.1["key"], b.1["key"]);
      assert_eq!(
        [&a.1, &b.1].iter().filter(|r| r["created"] == true).count(),
        1
      );
      let orgs: i64 = sqlx::query_scalar("SELECT count(*) FROM orgs WHERE slug LIKE $1")
        .bind(format!("u{i}-race-dev%"))
        .fetch_one(w.pg.pool())
        .await
        .unwrap();
      assert_eq!(orgs, 1, "no orphan org for user_{i}");
    }
  }

  #[tokio::test]
  async fn exchanged_key_stops_working_at_expiry() {
    let Some(w) = world(1).await else {
      return;
    };
    let (_, body) = exchange_token(&w.router, &claims("user_1", Some("jane@acme.dev"))).await;
    let key = body["key"].as_str().unwrap();
    let (status, _) = call(&w.router, "GET", "/me", Some(key), None).await;
    assert_eq!(status, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let (status, _) = call(&w.router, "GET", "/me", Some(key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
  }

  #[tokio::test]
  async fn logout_revokes_an_expiring_key_but_never_a_pasted_one() {
    let Some(w) = world(3600).await else {
      return;
    };
    let (_, body) = exchange_token(&w.router, &claims("user_1", Some("jane@acme.dev"))).await;
    let session_key = body["key"].as_str().unwrap().to_owned();
    let (_, org_id) = w.pg.find_user_by_clerk_id("user_1").await.unwrap().unwrap();

    // A pasted, long-lived key in the same org.
    let pasted = keys::mint(ApiKeyKind::Platform);
    w.pg
      .create_api_key(
        org_id,
        None,
        ApiKeyKind::Platform,
        "cli",
        &keys::hash(&pasted),
      )
      .await
      .unwrap();

    // Signing out with the long-lived key: 204, key stays valid.
    let (status, _) = call(&w.router, "POST", "/auth/logout", Some(&pasted), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&w.router, "GET", "/me", Some(&pasted), None).await;
    assert_eq!(status, StatusCode::OK);

    // Signing out with the session key: 204, and the key is dead.
    let (status, _) = call(&w.router, "POST", "/auth/logout", Some(&session_key), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(&w.router, "GET", "/me", Some(&session_key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A second logout with the dead key is just an (auth) 401, and the
    // route requires authentication at all.
    let (status, _) = call(&w.router, "POST", "/auth/logout", Some(&session_key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&w.router, "POST", "/auth/logout", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
  }
}
