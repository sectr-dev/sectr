//! App read APIs — the console overview + app pages:
//!
//! - `GET /apps`        → AppSummary[] (org-scoped)
//! - `GET /apps/{id}`   → AppDetail (AppSummary + deployments[])
//!
//! The dashboard composes its UI App from these (see
//! dashboard/src/lib/transport/control-plane.ts). Routes/framework come
//! from the ACTIVE deployment's manifest (what actually serves); url is
//! null in the MVP (no per-app public hostnames yet); description is ""
//! (nothing derives an honest one server-side).
//!
//! Authz mirrors the session routes: org-scoped by the principal; foreign
//! orgs get 404 (no existence leaks).

use sectr_common::registry::DeploymentStatus;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::PlatformPrincipal;
use crate::state::AppState;

pub fn apps_router() -> axum::Router<AppState> {
  axum::Router::new()
    .route("/apps", get(list_apps))
    .route("/apps/{appId}", get(get_app))
}

/// `GET /apps`
async fn list_apps(State(state): State<AppState>, principal: PlatformPrincipal) -> Response {
  compose_list(&state, principal.0.org_id(), None).await
}

/// `GET /apps/{appId}`
async fn get_app(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  Path(app_id): Path<Uuid>,
) -> Response {
  compose_list(&state, principal.0.org_id(), Some(app_id)).await
}

/// Both endpoints share one composition: apps (+ counts) from the
/// registry, route/framework projection from each app's ACTIVE
/// deployment manifest. `only` narrows to a single app (the detail
/// endpoint) — its row becomes AppDetail (deployments[] attached).
async fn compose_list(state: &AppState, org_id: Uuid, only: Option<Uuid>) -> Response {
  let registry = state.registry();

  let apps = match only {
    Some(id) => match registry.get_app(id).await {
      Ok(app) if app.org_id == org_id => vec![app],
      Ok(_) | Err(_) => {
        return (
          StatusCode::NOT_FOUND,
          Json(json!({ "error": "app not found" })),
        )
          .into_response();
      }
    },
    None => match registry.list_apps(org_id).await {
      Ok(apps) => apps,
      Err(e) => return internal_error(e),
    },
  };

  let counts: std::collections::HashMap<Uuid, i64> =
    match registry.count_sessions_by_app(org_id).await {
      Ok(pairs) => pairs.into_iter().collect(),
      Err(e) => return internal_error(e),
    };

  let mut out = Vec::with_capacity(apps.len());
  for app in &apps {
    let deployments = match registry.list_deployments(app.id).await {
      Ok(d) => d,
      Err(e) => return internal_error(e),
    };
    let active = deployments
      .iter()
      .find(|d| d.status == DeploymentStatus::Active)
      // Fall back to the newest row (an app mid-first-build has no
      // active deployment yet — its routes render empty, honestly).
      .or_else(|| deployments.first());

    let (routes, framework) = match active {
      Some(d) => routes_from_manifest(&d.manifest),
      None => (Vec::new(), None),
    };

    let latest = deployments.first().map(|d| {
      json!({
          "id": d.id,
          "app_id": d.app_id,
          "version": d.version,
          "status": serde_json::to_value(&d.status).unwrap_or(Value::Null),
          "created_at": d.created_at,
          "active": active.is_some_and(|a| a.id == d.id),
      })
    });

    let mut summary = json!({
        "id": app.id,
        "name": app.name,
        "slug": app.slug,
        "framework": framework,
        "description": "",
        "url": null,
        "routes": routes,
        "sessions_count": counts.get(&app.id).copied().unwrap_or(0),
        "latest_deployment": latest,
    });
    if only.is_some() {
      summary["deployments"] = Value::Array(
        deployments
          .iter()
          .map(|d| {
            json!({
                "id": d.id,
                "app_id": d.app_id,
                "version": d.version,
                "status": serde_json::to_value(&d.status).unwrap_or(Value::Null),
                "created_at": d.created_at,
                "active": active.is_some_and(|a| a.id == d.id),
            })
          })
          .collect(),
      );
    }
    out.push(summary);
  }

  // Detail endpoint: a single AppDetail object; summary endpoint: the
  // org's AppSummary array.
  match only {
    Some(_) => match out.into_iter().next() {
      Some(one) => Json(one).into_response(),
      None => (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "app not found" })),
      )
        .into_response(),
    },
    None => Json(out).into_response(),
  }
}

/// The UI-facing route list + framework marker from a manifest: the
/// import result carries `routes[]` (`{path, framework?}`) and
/// `deployment_serves_route` matches those paths.
fn routes_from_manifest(manifest: &Value) -> (Vec<Value>, Option<String>) {
  let mut routes = Vec::new();
  let mut framework = None;
  if let Some(list) = manifest.get("routes").and_then(|r| r.as_array()) {
    for r in list {
      let path = r.get("path").and_then(|p| p.as_str()).unwrap_or("");
      if !path.is_empty() {
        routes.push(json!({ "path": path, "description": "" }));
      }
      if framework.is_none() {
        framework = r
          .get("framework")
          .and_then(|f| f.as_str())
          .map(str::to_owned);
      }
    }
  }
  (routes, framework)
}

fn internal_error(e: impl std::fmt::Display) -> Response {
  tracing::error!(error = %e, "apps request failed");
  (
    StatusCode::INTERNAL_SERVER_ERROR,
    Json(json!({ "error": "internal error" })),
  )
    .into_response()
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::body::Body;
  use serde_json::Value;
  use std::sync::Arc;
  use tower::ServiceExt;

  use sectr_common::registry::{ApiKeyKind, NewApp, NewDeployment, Registry};
  use sectr_common::store::PgStore;

  use crate::auth::keys;
  use crate::config::ServerConfig;
  use crate::test_pg;
  use crate::{http, state::AppState};

  async fn world() -> Option<(axum::Router, String, Uuid, Uuid)> {
    let store: PgStore = test_pg::fresh_store().await?;
    let registry: Arc<dyn Registry> = Arc::new(store.registry());
    let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
    let org = registry.create_org("test-org", "default").await.unwrap();
    let app = registry
      .create_app(NewApp {
        org_id: org.id,
        name: "my-agent".into(),
        slug: "my-agent".into(),
      })
      .await
      .unwrap();
    // One active + one retired deployment (history order matters).
    let d1 = registry
      .create_deployment(NewDeployment {
        app_id: app.id,
        version: 1,
        manifest: serde_json::json!({
            "routes": [
                {"path": "/assist", "framework": "openai-agents"},
                {"path": "/echo"}
            ]
        }),
      })
      .await
      .unwrap();
    let _d2 = registry
      .create_deployment(NewDeployment {
        app_id: app.id,
        version: 2,
        manifest: serde_json::json!({
            "routes": [{"path": "/assist"}]
        }),
      })
      .await
      .unwrap();
    registry
      .set_deployment_status(d1.id, DeploymentStatus::Active)
      .await
      .unwrap();

    let platform = keys::mint(ApiKeyKind::Platform);
    registry
      .create_api_key(
        org.id,
        None,
        ApiKeyKind::Platform,
        "apps test",
        &keys::hash(&platform),
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
    let config = ServerConfig {
      secrets_master_key: None,
      ..test_config()
    };
    let state = AppState::new(
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
    );
    Some((http::router(state), platform, app.id, org.id))
  }

  fn test_config() -> ServerConfig {
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
  ) -> (axum::http::StatusCode, Value) {
    let request = axum::http::Request::builder()
      .method(method)
      .uri(uri)
      .header("Authorization", format!("Bearer {}", token.unwrap_or("")))
      .body(Body::empty())
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
  async fn apps_list_composes_manifest_routes_and_active_flag() {
    let Some((app, key, app_id, _org)) = world().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let (status, body) = req(&app, "GET", "/apps", Some(&key)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    let a = &body[0];
    assert_eq!(a["id"], app_id.to_string());
    assert_eq!(a["framework"], "openai-agents");
    // Routes project from the ACTIVE deployment's manifest.
    let routes: Vec<&str> = a["routes"]
      .as_array()
      .unwrap()
      .iter()
      .map(|r| r["path"].as_str().unwrap())
      .collect();
    assert_eq!(routes, vec!["/assist", "/echo"]);
    assert_eq!(a["latest_deployment"]["version"], 2);
    assert_eq!(a["latest_deployment"]["active"], false);
    assert_eq!(a["sessions_count"], 0);
    assert!(a["url"].is_null());
  }

  #[tokio::test]
  async fn app_detail_includes_deployment_history_and_404s_foreign() {
    let Some((app, key, app_id, _org)) = world().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let (status, body) = req(&app, "GET", &format!("/apps/{app_id}"), Some(&key)).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let deployments = body["deployments"].as_array().unwrap();
    assert_eq!(deployments.len(), 2);
    // Newest first (registry list order); v1 carries the active flag.
    assert_eq!(deployments[0]["version"], 2);
    assert_eq!(deployments[1]["version"], 1);
    assert_eq!(deployments[1]["active"], true);

    // Unknown / foreign app ids are plain 404s.
    let missing = uuid::Uuid::now_v7();
    let (status, _) = req(&app, "GET", &format!("/apps/{missing}"), Some(&key)).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // No token → 401 (auth middleware), not a data leak.
    let (status, _) = req(&app, "GET", "/apps", None).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
  }
}
