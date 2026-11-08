//! Endpoint deployments: a deployment that runs on the customer's own
//! endpoint (the sidecar in serve mode), with no image build.
//!
//! `POST /endpoint-deployments` (platform key, JSON `{manifest, endpoint_url}`)
//! answers `202` like a deploy:
//!
//! 1. Synchronously, with a 4xx on failure: parse `sectr.toml`, apply the
//!    URL rules, the shared deploy start ([`super::prepare_deploy`]), and
//!    probe the endpoint (`GET /` must be a serve-mode sidecar). The row is
//!    created in `building` with its `endpoint_url`.
//! 2. In the background: mint a registration token, ask the endpoint for
//!    its manifest (`POST /manifest`, which dials this server with the
//!    token before importing the app in the deployed image), and require
//!    BOTH the `manifest` answer and the hub's own record of the dial. Then
//!    compose the manifest (`runner.image: null`) and activate, or fail the
//!    row with an error the CLI and dashboard show.
//!
//! `GET /endpoint-deployments/config` tells the CLI whether endpoint
//! deployments are on, which ingest URL the image must dial, and which base
//! image it builds from.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};

use sectr_common::endpoint::{DIAL_FAILED, DISABLED, WRONG_INGEST};
use sectr_common::registry::{Deployment, NewDeployment};

use super::{DeployManifest, compose_manifest, deployment_dto, parse_sectr_toml, prepare_deploy};
use crate::auth::PlatformPrincipal;
use crate::state::AppState;

/// The registration body: `sectr.toml` and a URL, so small; the cap only
/// bounds a hostile request.
pub(super) const REGISTER_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// How long a registration token stays valid: the endpoint's cold start,
/// its dial and nothing more.
const REGISTRATION_TOKEN_TTL: Duration = Duration::from_secs(120);

#[derive(Deserialize)]
pub(super) struct RegisterBody {
  /// `sectr.toml`, as text.
  manifest: String,
  endpoint_url: String,
}

fn unprocessable(message: impl Into<String>) -> Response {
  (StatusCode::UNPROCESSABLE_ENTITY, message.into()).into_response()
}

/// `GET /endpoint-deployments/config`.
pub(super) async fn endpoint_config(
  State(state): State<AppState>,
  _principal: PlatformPrincipal,
) -> Json<Value> {
  let ingest_url = state.endpoints().ingest_url();
  // Always set: the configured base image, or this version's published one.
  let base_image = state.config().runner_base_image.clone().unwrap_or_default();
  Json(json!({
    "enabled": ingest_url.is_some(),
    "ingest_url": ingest_url,
    "base_image": base_image,
  }))
}

/// `POST /endpoint-deployments`.
pub(super) async fn register_endpoint(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  body: Result<Json<RegisterBody>, JsonRejection>,
) -> Response {
  let org_id = principal.0.org_id();
  let Json(body) = match body {
    Ok(body) => body,
    // Not JSON, or not this shape: all 400, as the API spec says.
    Err(rejection) => {
      return (StatusCode::BAD_REQUEST, rejection.body_text()).into_response();
    }
  };
  let endpoints = state.endpoints().clone();
  if !endpoints.config().enabled() {
    return unprocessable(format!(
      "this server {DISABLED} (its SECTR_ENDPOINT_INGEST_URL is not set)"
    ));
  }
  let parsed = match parse_sectr_toml(&body.manifest) {
    Ok(parsed) => parsed,
    Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
  };
  let url = match endpoints.check_url(body.endpoint_url.trim()) {
    Ok(url) => url,
    Err(e) => return unprocessable(e),
  };
  let (app, version) = match prepare_deploy(&state, org_id, &parsed).await {
    Ok(prepared) => prepared,
    Err(e) => {
      tracing::warn!(error = %e, "endpoint deployment rejected");
      return unprocessable(e);
    }
  };
  let probe = match endpoints.probe(&url).await {
    Ok(probe) => probe,
    Err(e) => return unprocessable(e),
  };
  let mut warnings = Vec::new();
  let ours = env!("CARGO_PKG_VERSION");
  if probe.version != ours {
    warnings.push(format!(
      "the endpoint runs sidecar {}; this server is {ours}. Rebuild the image on the matching \
       base image.",
      if probe.version.is_empty() {
        "of an unknown version"
      } else {
        &probe.version
      }
    ));
  }

  let registry = state.registry();
  let deployment = match registry
    .create_deployment(NewDeployment {
      app_id: app.id,
      version,
      // Placeholder until the endpoint's manifest lands.
      manifest: json!({"routes": []}),
      endpoint_url: Some(url.as_str().to_owned()),
    })
    .await
  {
    Ok(deployment) => deployment,
    Err(e) => return unprocessable(e.to_string()),
  };
  tokio::spawn(run_registration(
    state.clone(),
    deployment.clone(),
    parsed,
    url,
  ));

  let dto = match deployment_dto(deployment, registry.as_ref()).await {
    Ok(dto) => dto,
    Err(e) => return unprocessable(e),
  };
  let mut body = serde_json::to_value(dto).expect("the DTO serializes");
  if !warnings.is_empty() {
    body["warnings"] = json!(warnings);
  }
  (StatusCode::ACCEPTED, Json(body)).into_response()
}

/// The background half: resolve the `building` row to `active` or
/// `failed`.
async fn run_registration(
  state: AppState,
  deployment: Deployment,
  parsed: DeployManifest,
  url: Url,
) {
  let span = tracing::info_span!(
      "deploy.endpoint",
      deployment_id = %deployment.id,
      app_id = %deployment.app_id,
      version = deployment.version,
      outcome = tracing::field::Empty,
  );
  let result = register_and_activate(&state, &deployment, &parsed, &url).await;
  let outcome =
    super::record_deploy_outcome(&state, &deployment, result, json!({"kind": "endpoint"})).await;
  span.record("outcome", outcome);
}

async fn register_and_activate(
  state: &AppState,
  deployment: &Deployment,
  parsed: &DeployManifest,
  url: &Url,
) -> Result<(), String> {
  let endpoints = state.endpoints();
  let ingest_url = endpoints.ingest_url().unwrap_or_default().to_owned();
  let (token, dialed) = state.hub().register_probe(REGISTRATION_TOKEN_TTL).await;
  let answer = endpoints.fetch_manifest(url, &token, &parsed.entry).await?;
  let field = |name: &str| answer.get(name).and_then(Value::as_str).unwrap_or_default();
  let (imported, uses_uv) = match field("outcome") {
    "manifest" => {
      // The endpoint's word isn't enough: this server's own ingest must
      // have seen the dial.
      if !*dialed.borrow() {
        return Err(
          "the endpoint answered with a manifest, but its sidecar never dialed this server with \
           the registration token"
            .into(),
        );
      }
      let imported = answer
        .get("manifest")
        .filter(|m| m.is_object())
        .ok_or("the endpoint's manifest answer has no manifest object")?;
      if imported.get("routes").is_some_and(|r| !r.is_array()) {
        return Err("the endpoint's manifest has a `routes` that isn't a list".into());
      }
      let uses_uv = answer
        .get("uses_uv")
        .and_then(Value::as_bool)
        .ok_or("the endpoint's manifest answer has no uses_uv")?;
      (imported.clone(), uses_uv)
    }
    "token_rejected" => {
      return Err(format!(
        "the endpoint's sidecar {WRONG_INGEST} registration. Check that \
         its SECTR_INGEST_URL is {ingest_url}."
      ));
    }
    "dial_failed" => {
      return Err(format!(
        "the endpoint's sidecar {DIAL_FAILED} {ingest_url}: {}. Check that your platform allows \
         outbound connections.",
        field("message")
      ));
    }
    "import_failed" => {
      let exit = match answer.get("exit_code").and_then(Value::as_i64) {
        Some(code) => format!("exit {code}"),
        None => field("message").to_owned(),
      };
      return Err(format!(
        "the manifest import failed in your image ({exit}): {}",
        field("stderr_tail")
      ));
    }
    other => {
      return Err(format!(
        "the endpoint answered the manifest request with an unknown outcome {other:?}"
      ));
    }
  };

  let manifest = compose_manifest(None, &imported, uses_uv, parsed);
  state
    .registry()
    .set_deployment_manifest(deployment.id, &manifest)
    .await
    .map_err(|e| e.to_string())?;
  super::activate(state, deployment.id).await
}
