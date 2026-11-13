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

use sectr_common::registry::{Deployment, DeploymentStatus};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::PlatformPrincipal;
use crate::deploy::DeploymentDto;
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
    // The active deployment is the one with status active
    // (`Registry::active_deployment`: the newest, should there be two).
    let active_id = deployments
      .iter()
      .filter(|d| d.status == DeploymentStatus::Active)
      .max_by_key(|d| (d.created_at, d.version))
      .map(|d| d.id);
    // Routes come from it, or else from the newest row (an app
    // mid-first-build has no active deployment yet: its routes render
    // empty, honestly).
    let serving = deployments
      .iter()
      .find(|d| Some(d.id) == active_id)
      .or_else(|| deployments.first());

    let (routes, framework) = match serving {
      Some(d) => routes_from_manifest(&d.manifest),
      None => (Vec::new(), None),
    };

    let dto = |d: &Deployment| DeploymentDto::new(d, Some(d.id) == active_id);
    let latest = deployments.first().map(dto);

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
      summary["deployments"] = json!(deployments.iter().map(dto).collect::<Vec<_>>());
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

  async fn world() -> Option<(axum::Router, String, Uuid, Uuid, Arc<dyn Registry>)> {
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
        endpoint_url: None,
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
        endpoint_url: None,
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
      endpoints: Default::default(),
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
        registry.clone(),
      )),
      false,
    );
    Some((http::router(state), platform, app.id, org.id, registry))
  }

  fn test_config() -> ServerConfig {
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
    let Some((app, key, app_id, _org, _registry)) = world().await else {
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
    let Some((app, key, app_id, _org, _registry)) = world().await else {
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

  /// Where an app runs comes from its deployments' `endpoint_url`: both
  /// routes give the same Deployment as the deploy routes.
  #[tokio::test]
  async fn app_routes_give_the_full_deployment() {
    let Some((app, key, _app_id, org, registry)) = world().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let endpoint_app = registry
      .create_app(NewApp {
        org_id: org,
        name: "byo".into(),
        slug: "byo".into(),
      })
      .await
      .unwrap();
    let url = "https://byo-xyz.a.run.app/";
    let d = registry
      .create_deployment(NewDeployment {
        endpoint_url: Some(url.into()),
        app_id: endpoint_app.id,
        version: 1,
        manifest: serde_json::json!({"routes": [{"path": "/chat"}]}),
      })
      .await
      .unwrap();
    registry
      .set_deployment_status(d.id, DeploymentStatus::Active)
      .await
      .unwrap();

    let (_, detail) = req(
      &app,
      "GET",
      &format!("/apps/{}", endpoint_app.id),
      Some(&key),
    )
    .await;
    assert_eq!(detail["latest_deployment"]["endpoint_url"], url);
    assert_eq!(detail["latest_deployment"]["active"], true);
    assert_eq!(detail["deployments"][0]["endpoint_url"], url);
    // The same as the deploy routes' DTO.
    let (_, history) = req(
      &app,
      "GET",
      &format!("/apps/{}/deployments", endpoint_app.id),
      Some(&key),
    )
    .await;
    assert_eq!(detail["deployments"], history);

    let (_, list) = req(&app, "GET", "/apps", Some(&key)).await;
    let byo = list
      .as_array()
      .unwrap()
      .iter()
      .find(|a| a["id"] == endpoint_app.id.to_string())
      .unwrap();
    assert_eq!(byo["latest_deployment"]["endpoint_url"], url);
    // A managed deployment has `endpoint_url: null`, not a missing field.
    let managed = list
      .as_array()
      .unwrap()
      .iter()
      .find(|a| a["id"] != endpoint_app.id.to_string())
      .unwrap();
    assert!(managed["latest_deployment"]["endpoint_url"].is_null());
    assert!(
      managed["latest_deployment"]
        .as_object()
        .unwrap()
        .contains_key("endpoint_url")
    );
  }

  /// An app whose only deployment is still building has no active one.
  #[tokio::test]
  async fn a_building_first_deployment_is_not_active() {
    let Some((app, key, _app_id, org, registry)) = world().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let new_app = registry
      .create_app(NewApp {
        org_id: org,
        name: "new".into(),
        slug: "new".into(),
      })
      .await
      .unwrap();
    registry
      .create_deployment(NewDeployment {
        endpoint_url: None,
        app_id: new_app.id,
        version: 1,
        manifest: serde_json::json!({"routes": [{"path": "/assist"}]}),
      })
      .await
      .unwrap();
    let (_, detail) = req(&app, "GET", &format!("/apps/{}", new_app.id), Some(&key)).await;
    assert_eq!(detail["latest_deployment"]["status"], "building");
    assert_eq!(detail["latest_deployment"]["active"], false);
    assert_eq!(detail["deployments"][0]["active"], false);
    // Its routes still show, from the newest deployment.
    assert_eq!(detail["routes"][0]["path"], "/assist");
  }
}
