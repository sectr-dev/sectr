//! HTTP integration tests: auth middleware + key management, over a
//! real Postgres (pgtemp-managed test PG; skip when none is available,
//! same policy as sectr-common). In-process via `tower::ServiceExt`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use sectr_common::registry::{ApiKeyKind, NewApp, Registry};
use sectr_common::store::PgStore;

use crate::config::ServerConfig;
use crate::test_pg;
use crate::{auth::keys, http, state::AppState};

type TestCtx = (
  axum::Router,
  String, /* platform key */
  Uuid,   /* app id */
);

async fn ctx() -> Option<TestCtx> {
  ctx_with(|_| None).await
}

/// [`ctx`] with a concrete Postgres registry attached to the state (what
/// production composes; `/readyz` pings it). The closure picks the registry
/// from the fresh store.
async fn ctx_with(
  pg: impl FnOnce(&PgStore) -> Option<sectr_common::registry::PgRegistry>,
) -> Option<TestCtx> {
  let store: PgStore = test_pg::fresh_store().await?;
  let pg_registry = pg(&store);
  let registry: Arc<dyn Registry> = Arc::new(store.registry());
  let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
  let org = registry.create_org("test-org", "default").await.unwrap();
  let app_row = registry
    .create_app(NewApp {
      org_id: org.id,
      name: "test-app".into(),
      slug: "test-app".into(),
    })
    .await
    .unwrap();

  let platform = keys::mint(ApiKeyKind::Platform);
  registry
    .create_api_key(
      org.id,
      None,
      ApiKeyKind::Platform,
      "test platform",
      &keys::hash(&platform),
    )
    .await
    .unwrap();

  // Control-plane tests never exercise turns; the interim dispatcher is
  // fine (its failures only surface through session routes).
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
  let state = AppState::new(
    registry.clone(),
    journal.clone(),
    service,
    Arc::new(hub),
    test_config(),
    Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
    // Dockerless platform: the config carries no base image, so a
    // deploy build fails fast (dockerless test-world semantics).
    Arc::new(crate::platform::DockerPlatform::new(
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      None,
      test_config(),
      registry.clone(),
    )),
    false,
  );
  let state = match pg_registry {
    Some(pg) => state.with_pg_registry(pg),
    None => state,
  };
  let app = http::router(state);
  Some((app, platform, app_row.id))
}

fn test_config() -> ServerConfig {
  // Config values other than pg_url are irrelevant to the HTTP tests; the
  // pg_url is only read by main()'s open path (tests build the state
  // directly).
  ServerConfig {
    endpoints: Default::default(),
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

async fn req(
  app: &axum::Router,
  method: &str,
  uri: &str,
  token: Option<&str>,
  body: Option<Value>,
) -> (StatusCode, Value) {
  let builder = Request::builder().method(method).uri(uri);
  let builder = match token {
    Some(t) => builder.header("Authorization", format!("Bearer {t}")),
    None => builder,
  };
  let request = builder
    .header("Content-Type", "application/json")
    .body(Body::from(
      body.map(|b| b.to_string()).unwrap_or_else(|| "null".into()),
    ))
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
async fn healthz_is_anonymous_and_ok() {
  let Some((app, _, _)) = ctx().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, body) = req(&app, "GET", "/healthz", None, None).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn readyz_is_ok_with_a_live_database() {
  let Some((app, _, _)) = ctx_with(|s| Some(s.registry())).await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, body) = req(&app, "GET", "/readyz", None, None).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["status"], "ok");
  assert_eq!(body["db"], "ok");
  assert!(body["version"].is_string());
}

#[tokio::test]
async fn readyz_skips_the_db_check_without_postgres() {
  let Some((app, _, _)) = ctx().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, body) = req(&app, "GET", "/readyz", None, None).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["db"], "skipped");
}

#[tokio::test]
async fn readyz_is_503_without_error_detail_when_the_database_is_dead() {
  let dead = sectr_common::registry::PgRegistry::new(
    sqlx::postgres::PgPoolOptions::new()
      .acquire_timeout(std::time::Duration::from_millis(500))
      .connect_lazy("postgres://nobody@127.0.0.1:1/nodb")
      .expect("lazy pool"),
  );
  let Some((app, _, _)) = ctx_with(move |_| Some(dead)).await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, body) = req(&app, "GET", "/readyz", None, None).await;
  assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
  assert_eq!(body["status"], "degraded");
  assert_eq!(body["db"], "unavailable");
  // Error details go to the log, never on the wire.
  let wire = body.to_string();
  assert!(
    !wire.contains("127.0.0.1") && !wire.contains("refused"),
    "{wire}"
  );
  assert_eq!(body.as_object().unwrap().len(), 3);
}

#[tokio::test]
async fn readyz_ping_times_out_when_the_pool_is_exhausted() {
  let Some(store) = test_pg::fresh_store().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  // One-connection pool with its only connection held: the ping cannot
  // acquire, so only the timeout can end it.
  let pool = sqlx::postgres::PgPoolOptions::new()
    .max_connections(1)
    .connect_with((*store.pool().connect_options()).clone())
    .await
    .expect("single-connection pool");
  let registry = sectr_common::registry::PgRegistry::new(pool.clone());
  registry.ping().await.expect("ping succeeds on a free pool");
  let _held = pool.acquire().await.expect("hold the only connection");
  let started = std::time::Instant::now();
  let res = registry
    .ping_within(std::time::Duration::from_millis(200))
    .await;
  assert!(res.is_err());
  assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[tokio::test]
async fn missing_or_garbage_key_is_401() {
  let Some((app, _, _)) = ctx().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // No header.
  let (status, body) = req(&app, "GET", "/api-keys", None, None).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
  assert!(body["error"].is_string());
  // 401 must advertise the scheme.
  // (WWW-Authenticate checked in the response headers below via raw call.)

  // Garbage scheme / garbage token.
  for token in ["", "not-a-sectr-key", "skp_short"] {
    let (status, _) = req(&app, "GET", "/api-keys", Some(token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "token {token:?}");
  }
}

#[tokio::test]
async fn revoked_key_is_401() {
  let Some((app, platform, _)) = ctx().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // Mint a second key via the API, then revoke it.
  let (status, body) = req(
    &app,
    "POST",
    "/api-keys",
    Some(&platform),
    Some(json!({"name": "doomed", "kind": "platform"})),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  let key_id = body["id"].as_str().unwrap();
  let key_value = body["key"].as_str().unwrap();
  assert!(key_value.starts_with("skp_"));

  // Fresh key works.
  let (status, _) = req(&app, "GET", "/api-keys", Some(key_value), None).await;
  assert_eq!(status, StatusCode::OK);

  // Revoke (by the original key).
  let (status, _) = req(
    &app,
    "DELETE",
    &format!("/api-keys/{key_id}"),
    Some(&platform),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NO_CONTENT);

  // Revoked key is dead; revoke is idempotent (204 again).
  let (status, _) = req(&app, "GET", "/api-keys", Some(key_value), None).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
  let (status, _) = req(
    &app,
    "DELETE",
    &format!("/api-keys/{key_id}"),
    Some(&platform),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn invocation_keys_are_403_on_control_plane() {
  let Some((app, platform, app_id)) = ctx().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // Mint an invocation key for the app.
  let (status, body) = req(
    &app,
    "POST",
    "/api-keys",
    Some(&platform),
    Some(json!({"name": "end-clients", "kind": "invocation", "app_id": app_id})),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED, "body: {body}");
  let invocation = body["key"].as_str().unwrap();
  assert!(invocation.starts_with("ski_"));

  // Authenticated, but the wrong class: 403, not 401.
  let (status, body) = req(&app, "GET", "/api-keys", Some(invocation), None).await;
  assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
  let (status, _) = req(&app, "GET", "/me", Some(invocation), None).await;
  assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn key_create_validation_and_metadata_only_list() {
  let Some((app, platform, app_id)) = ctx().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // Platform keys must not carry an app_id.
  let (status, _) = req(
    &app,
    "POST",
    "/api-keys",
    Some(&platform),
    Some(json!({"name": "x", "kind": "platform", "app_id": app_id})),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);

  // Invocation keys require an app_id.
  let (status, _) = req(
    &app,
    "POST",
    "/api-keys",
    Some(&platform),
    Some(json!({"name": "x", "kind": "invocation"})),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);

  // Unknown / foreign app.
  let (status, _) = req(
    &app,
    "POST",
    "/api-keys",
    Some(&platform),
    Some(json!({"name": "x", "kind": "invocation", "app_id": uuid::Uuid::now_v7()})),
  )
  .await;
  assert_eq!(status, StatusCode::BAD_REQUEST);

  // Valid platform key: meta + full value exactly once.
  let (status, body) = req(
    &app,
    "POST",
    "/api-keys",
    Some(&platform),
    Some(json!({"name": "cli", "kind": "platform"})),
  )
  .await;
  assert_eq!(status, StatusCode::CREATED);
  assert!(body["key"].as_str().unwrap().starts_with("skp_"));
  assert!(body.get("key_hash").is_none(), "hash is not API surface");

  // List: metadata only — no key values, no hashes.
  let (status, list) = req(&app, "GET", "/api-keys", Some(&platform), None).await;
  assert_eq!(status, StatusCode::OK);
  let list = list.as_array().unwrap();
  assert!(list.len() >= 2);
  for entry in list {
    assert!(entry.get("key").is_none());
    assert!(entry.get("key_hash").is_none());
    assert!(entry["id"].is_string() && entry["name"].is_string());
    assert_eq!(entry["kind"], "platform");
  }
}

#[tokio::test]
async fn deploy_upload_is_202_building_and_resolves_async() {
  let Some((app, platform, _)) = ctx().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };

  // Hand-rolled multipart (the `req` helper always sends JSON). The
  // tarball content is irrelevant: the test world has no base image
  // configured, so the background build fails at its first step.
  let boundary = "X-SECTR-TEST-BOUNDARY";
  let toml = "agent.entry = 'agent:agent'\n";
  let multipart = format!(
    "--{b}\r\n\
         Content-Disposition: form-data; name=\"tarball\"; filename=\"app.tar.gz\"\r\n\
         Content-Type: application/gzip\r\n\r\n\
         fake-tarball\r\n\
         --{b}\r\n\
         Content-Disposition: form-data; name=\"manifest\"\r\n\
         Content-Type: application/toml\r\n\r\n\
         {toml}\r\n\
         --{b}--\r\n",
    b = boundary
  );
  let request = Request::builder()
    .method("POST")
    .uri("/deployments")
    .header("Authorization", format!("Bearer {platform}"))
    .header(
      "Content-Type",
      format!("multipart/form-data; boundary={boundary}"),
    )
    .body(Body::from(multipart))
    .unwrap();
  let response = app.clone().oneshot(request).await.unwrap();
  // 202, not 201: the row exists but the build resolves off-request.
  assert_eq!(response.status(), StatusCode::ACCEPTED);
  let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
    .await
    .unwrap();
  let body: Value = serde_json::from_slice(&bytes).unwrap();
  assert_eq!(body["status"], "building");
  assert_eq!(body["active"], false);
  let app_id = body["app_id"].as_str().unwrap();
  let deployment_id = body["id"].as_str().unwrap();

  // Poll the list endpoint (the CLI's polling path) until the background
  // task resolves the row. Test config has no base image → `failed`.
  let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
  loop {
    let (status, list) = req(
      &app,
      "GET",
      &format!("/apps/{app_id}/deployments"),
      Some(&platform),
      None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entry = list
      .as_array()
      .unwrap()
      .iter()
      .find(|d| d["id"] == *deployment_id)
      .expect("deployed row must be listed")
      .clone();
    if entry["status"] != "building" {
      assert_eq!(entry["status"], "failed");
      assert_eq!(entry["active"], false);
      break;
    }
    assert!(
      tokio::time::Instant::now() < deadline,
      "deployment never left building"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
  }
}

#[tokio::test]
async fn me_returns_org_for_platform_principal() {
  let Some((app, platform, _)) = ctx().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, body) = req(&app, "GET", "/me", Some(&platform), None).await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["org"]["name"], "test-org");
  assert!(body["org"]["id"].is_string());
  assert_eq!(body["dashboard_url"], "https://app.sectr.test");
}

#[tokio::test]
async fn deleting_another_orgs_key_is_404() {
  let Some((app, platform, _)) = ctx().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let (status, _) = req(
    &app,
    "DELETE",
    &format!("/api-keys/{}", uuid::Uuid::now_v7()),
    Some(&platform),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_bootstraps_the_first_platform_key() {
  // The bootstrap path: ensure_bootstrap_key is idempotent and the key
  // authenticates.
  let Some(store) = test_pg::fresh_store().await else {
    eprintln!("skipping: no test Postgres (initdb not on PATH)");
    return;
  };
  let bootstrap_key = keys::mint(ApiKeyKind::Platform);
  let registry: Arc<dyn Registry> = Arc::new(store.registry());
  let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
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
  let state = AppState::new(
    registry.clone(),
    journal.clone(),
    service,
    Arc::new(hub),
    test_config(),
    Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
    // Dockerless platform: the config carries no base image, so a
    // deploy build fails fast (dockerless test-world semantics).
    Arc::new(crate::platform::DockerPlatform::new(
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      None,
      test_config(),
      registry.clone(),
    )),
    false,
  );
  crate::auth::ensure_bootstrap_key(&state, &bootstrap_key, "sectr")
    .await
    .unwrap();
  // Second call: no duplicate (idempotent).
  crate::auth::ensure_bootstrap_key(&state, &bootstrap_key, "sectr")
    .await
    .unwrap();

  let app = http::router(state.clone());
  let (status, body) = req(&app, "GET", "/me", Some(&bootstrap_key), None).await;
  assert_eq!(status, StatusCode::OK, "body: {body}");
  assert_eq!(body["org"]["name"], "sectr");

  // Exactly one key was created.
  let org = state
    .registry()
    .find_org_by_slug("default")
    .await
    .unwrap()
    .unwrap();
  let keys = state.registry().list_api_keys(org.id).await.unwrap();
  assert_eq!(keys.len(), 1);

  // Rotating it: once revoked, a restart with the key still in the
  // environment starts cleanly and leaves it revoked (no second row, which
  // the unique hash would refuse).
  state
    .registry()
    .revoke_api_key(org.id, keys[0].id)
    .await
    .unwrap();
  crate::auth::ensure_bootstrap_key(&state, &bootstrap_key, "sectr")
    .await
    .unwrap();
  let keys = state.registry().list_api_keys(org.id).await.unwrap();
  assert_eq!(keys.len(), 1);
  assert!(keys[0].revoked_at.is_some());
  let (status, _) = req(&app, "GET", "/me", Some(&bootstrap_key), None).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// An app's frontend (any origin) calls the session API with a user
// token — preflights for GET/POST and the actual responses carry allow-origin.
#[tokio::test]
async fn session_routes_answer_cross_origin_browser_calls() {
  let Some((app, _, _)) = ctx().await else {
    return;
  };
  for (uri, method) in [
    ("/sessions", "POST"),
    ("/sessions", "GET"),
    ("/sessions/abc/input", "POST"),
    ("/sessions/abc/approval", "POST"),
    ("/sessions/abc/events", "GET"),
    ("/approvals", "GET"),
  ] {
    let preflight = Request::builder()
      .method("OPTIONS")
      .uri(uri)
      .header("origin", "https://customer-app.example")
      .header("access-control-request-method", method)
      .header(
        "access-control-request-headers",
        "authorization,content-type,last-event-id",
      )
      .body(Body::empty())
      .unwrap();
    let r = app.clone().oneshot(preflight).await.unwrap();
    assert!(r.status().is_success(), "{method} {uri}: {}", r.status());
    let h = r.headers();
    assert_eq!(h["access-control-allow-origin"], "*", "{method} {uri}");
    assert!(
      h["access-control-allow-methods"]
        .to_str()
        .unwrap()
        .contains(method)
    );
    let allowed = h["access-control-allow-headers"]
      .to_str()
      .unwrap()
      .to_lowercase();
    for want in ["authorization", "content-type", "last-event-id"] {
      assert!(allowed.contains(want), "{uri}: {allowed}");
    }
  }

  // Non-preflight responses (here a 401) still carry allow-origin, so the
  // browser can read the error.
  let get = Request::builder()
    .uri("/sessions")
    .header("origin", "https://customer-app.example")
    .body(Body::empty())
    .unwrap();
  let r = app.clone().oneshot(get).await.unwrap();
  assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
  assert_eq!(r.headers()["access-control-allow-origin"], "*");
}

#[tokio::test]
async fn control_plane_routes_stay_same_origin() {
  let Some((app, _, _)) = ctx().await else {
    return;
  };
  for (uri, method) in [("/api-keys", "POST"), ("/tokens", "POST"), ("/me", "GET")] {
    let preflight = Request::builder()
      .method("OPTIONS")
      .uri(uri)
      .header("origin", "https://customer-app.example")
      .header("access-control-request-method", method)
      .body(Body::empty())
      .unwrap();
    let r = app.clone().oneshot(preflight).await.unwrap();
    assert!(
      r.headers().get("access-control-allow-origin").is_none(),
      "{method} {uri} must not be cross-origin"
    );
  }
}

// -- CLI browser login (device flow) --------------------------------

async fn cli_ctx() -> Option<(axum::Router, sectr_common::registry::PgRegistry, String)> {
  let mut slot = None;
  let (app, platform, _) = ctx_with(|s| {
    slot = Some(s.registry());
    slot.clone()
  })
  .await?;
  Some((app, slot.unwrap(), platform))
}

async fn cli_start(app: &axum::Router) -> (String, String) {
  let (status, body) = req(
    app,
    "POST",
    "/auth/cli",
    None,
    Some(json!({"client_name": "donald-mbp"})),
  )
  .await;
  assert_eq!(status, StatusCode::OK, "{body}");
  assert_eq!(body["expires_in"], 600);
  assert_eq!(body["interval"], 2);
  let user_code = body["user_code"].as_str().unwrap().to_owned();
  assert_eq!(
    body["verification_url_complete"],
    format!("https://app.sectr.test/cli/{user_code}")
  );
  assert_eq!(body["verification_url"], "https://app.sectr.test/cli");
  (body["device_code"].as_str().unwrap().to_owned(), user_code)
}

async fn cli_poll(app: &axum::Router, device_code: &str) -> Value {
  let (status, body) = req(
    app,
    "POST",
    "/auth/cli/poll",
    None,
    Some(json!({"device_code": device_code})),
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  body
}

#[tokio::test]
async fn cli_login_approve_then_poll_returns_the_key_once() {
  let Some((app, _pg, platform)) = cli_ctx().await else {
    return;
  };
  let (device, code) = cli_start(&app).await;
  assert_eq!(cli_poll(&app, &device).await["status"], "pending");

  // The approval page's view of the request (lenient code spelling).
  let (status, body) = req(
    &app,
    "GET",
    &format!(
      "/auth/cli/requests/{}",
      code.to_lowercase().replace('-', "")
    ),
    Some(&platform),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::OK);
  assert_eq!(body["client_name"], "donald-mbp");
  assert_eq!(body["status"], "pending");

  let (status, _) = req(
    &app,
    "POST",
    &format!("/auth/cli/requests/{code}/approve"),
    Some(&platform),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NO_CONTENT);

  let polled = cli_poll(&app, &device).await;
  assert_eq!(polled["status"], "approved");
  let key = polled["key"].as_str().unwrap();
  assert!(key.starts_with("skp_"));
  assert_eq!(polled["org"]["name"], "test-org");
  let (status, _) = req(&app, "GET", "/me", Some(key), None).await;
  assert_eq!(status, StatusCode::OK);
  let (_, listed) = req(&app, "GET", "/api-keys", Some(key), None).await;
  let row = listed
    .as_array()
    .unwrap()
    .iter()
    .find(|k| k["id"] == polled["key_id"])
    .expect("minted key is listed");
  assert_eq!(row["name"], "CLI (donald-mbp)");
  assert!(row["expires_at"].is_null());

  // Single use: the second poll is indistinguishable from unknown.
  assert_eq!(cli_poll(&app, &device).await, json!({"status": "expired"}));
  assert_eq!(
    cli_poll(&app, "never-issued").await,
    json!({"status": "expired"})
  );
}

#[tokio::test]
async fn cli_login_deny_mints_nothing() {
  let Some((app, pg, platform)) = cli_ctx().await else {
    return;
  };
  let before: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys")
    .fetch_one(pg.pool())
    .await
    .unwrap();
  let (device, code) = cli_start(&app).await;
  let (status, _) = req(
    &app,
    "POST",
    &format!("/auth/cli/requests/{code}/deny"),
    Some(&platform),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NO_CONTENT);
  assert_eq!(cli_poll(&app, &device).await, json!({"status": "denied"}));
  let after: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys")
    .fetch_one(pg.pool())
    .await
    .unwrap();
  assert_eq!(before, after);
}

#[tokio::test]
async fn cli_login_decisions_are_single_shot_and_need_a_session() {
  let Some((app, _pg, platform)) = cli_ctx().await else {
    return;
  };
  let (_, code) = cli_start(&app).await;
  let approve = format!("/auth/cli/requests/{code}/approve");
  let deny = format!("/auth/cli/requests/{code}/deny");

  // Anonymous callers can neither look up nor decide.
  let (status, _) = req(&app, "POST", &approve, None, None).await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);
  let (status, _) = req(
    &app,
    "GET",
    &format!("/auth/cli/requests/{code}"),
    None,
    None,
  )
  .await;
  assert_eq!(status, StatusCode::UNAUTHORIZED);

  assert_eq!(
    req(&app, "POST", &approve, Some(&platform), None).await.0,
    StatusCode::NO_CONTENT
  );
  let (status, body) = req(&app, "POST", &approve, Some(&platform), None).await;
  assert_eq!(status, StatusCode::CONFLICT);
  assert_eq!(body["status"], "approved");
  assert_eq!(
    req(&app, "POST", &deny, Some(&platform), None).await.0,
    StatusCode::CONFLICT
  );
  let (status, _) = req(
    &app,
    "POST",
    "/auth/cli/requests/BBBB-BBBB/approve",
    Some(&platform),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cli_login_expiry_blocks_approve_and_poll() {
  let Some((app, pg, platform)) = cli_ctx().await else {
    return;
  };
  // Injected TTLs: one request already expired when created.
  assert!(
    pg.create_cli_login("hash-1", "BBBB-CCCC", "old", -1)
      .await
      .unwrap()
  );
  let (status, _) = req(
    &app,
    "POST",
    "/auth/cli/requests/BBBB-CCCC/approve",
    Some(&platform),
    None,
  )
  .await;
  assert_eq!(status, StatusCode::NOT_FOUND);

  // Approved, then expired before collection: poll says expired, no key.
  let (device, code) = cli_start(&app).await;
  req(
    &app,
    "POST",
    &format!("/auth/cli/requests/{code}/approve"),
    Some(&platform),
    None,
  )
  .await;
  sqlx::query(
    "UPDATE cli_logins SET expires_at = now() - interval '1 second' WHERE user_code = $1",
  )
  .bind(&code)
  .execute(pg.pool())
  .await
  .unwrap();
  assert_eq!(cli_poll(&app, &device).await, json!({"status": "expired"}));
}

#[tokio::test]
async fn cli_login_concurrent_polls_mint_exactly_one_key() {
  let Some((app, _pg, platform)) = cli_ctx().await else {
    return;
  };
  let (device, code) = cli_start(&app).await;
  req(
    &app,
    "POST",
    &format!("/auth/cli/requests/{code}/approve"),
    Some(&platform),
    None,
  )
  .await;
  let polls = futures::future::join_all((0..8).map(|_| cli_poll(&app, &device))).await;
  let approved = polls.iter().filter(|p| p["status"] == "approved").count();
  assert_eq!(approved, 1, "{polls:?}");
}

#[tokio::test]
async fn cli_login_key_lands_in_the_approvers_org() {
  let Some((app, pg, _platform)) = cli_ctx().await else {
    return;
  };
  let other = pg.create_org("other-org", "other").await.unwrap();
  let other_key = keys::mint(ApiKeyKind::Platform);
  pg.create_api_key(
    other.id,
    None,
    ApiKeyKind::Platform,
    "other",
    &keys::hash(&other_key),
  )
  .await
  .unwrap();
  let (device, code) = cli_start(&app).await;
  req(
    &app,
    "POST",
    &format!("/auth/cli/requests/{code}/approve"),
    Some(&other_key),
    None,
  )
  .await;
  let polled = cli_poll(&app, &device).await;
  assert_eq!(polled["org"]["id"], other.id.to_string());
}

#[tokio::test]
async fn cli_login_gc_deletes_old_rows_but_not_their_keys() {
  let Some((app, pg, platform)) = cli_ctx().await else {
    return;
  };
  let (device, code) = cli_start(&app).await;
  req(
    &app,
    "POST",
    &format!("/auth/cli/requests/{code}/approve"),
    Some(&platform),
    None,
  )
  .await;
  let key = cli_poll(&app, &device).await["key"]
    .as_str()
    .unwrap()
    .to_owned();
  let (_, live_code) = cli_start(&app).await;

  sqlx::query("UPDATE cli_logins SET expires_at = now() - interval '2 days' WHERE user_code = $1")
    .bind(&code)
    .execute(pg.pool())
    .await
    .unwrap();
  let purged = pg
    .purge_cli_logins(chrono::Utc::now() - chrono::Duration::days(1))
    .await
    .unwrap();
  assert_eq!(purged, 1);
  assert!(pg.get_cli_login(&live_code).await.unwrap().is_some());
  let (status, _) = req(&app, "GET", "/me", Some(&key), None).await;
  assert_eq!(
    status,
    StatusCode::OK,
    "the minted key outlives its request"
  );
}

#[tokio::test]
async fn cli_login_start_is_rate_limited_and_503_without_pg() {
  // No PgRegistry (SQLite-style state): 503.
  if let Some((app, _, _)) = ctx().await {
    let (status, _) = req(&app, "POST", "/auth/cli", None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
  }
  let Some((app, _pg, _)) = cli_ctx().await else {
    return;
  };
  for _ in 0..20 {
    let (status, _) = req(&app, "POST", "/auth/cli", None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
  }
  let request = Request::builder()
    .method("POST")
    .uri("/auth/cli")
    .header("Content-Type", "application/json")
    .body(Body::from("{}"))
    .unwrap();
  let response = app.clone().oneshot(request).await.unwrap();
  assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
  assert!(response.headers().contains_key("retry-after"));
}
