//! Endpoint deployments: a deployment that runs on the customer's own
//! endpoint (the sidecar in serve mode), with no image build.
//!
//! `POST /endpoint-deployments` (platform key, JSON `{manifest, endpoint_url}`)
//! answers `202` like a deploy:
//!
//! 1. Synchronously, with a 4xx on failure: parse `sectr.toml`, apply the
//!    URL rules, probe the endpoint (`GET /` must be a serve-mode sidecar),
//!    then the shared deploy start ([`super::prepare_deploy`]). The probe
//!    comes first because it can take a cold start's time: the deploy
//!    start creates the app on first use and picks the next version, which
//!    the row then takes at once. The row is created in `building` with
//!    its `endpoint_url`.
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
use serde::Serialize;

use sectr_common::endpoint::{
  DIAL_FAILED, DISABLED, EndpointDeploymentsConfig, IMPORT_FAILED, ManifestAnswer,
  RegisterEndpoint, WRONG_INGEST,
};
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

fn unprocessable(message: impl Into<String>) -> Response {
  (StatusCode::UNPROCESSABLE_ENTITY, message.into()).into_response()
}

/// `GET /endpoint-deployments/config`.
pub(super) async fn endpoint_config(
  State(state): State<AppState>,
  _principal: PlatformPrincipal,
) -> Json<EndpointDeploymentsConfig> {
  let ingest_url = state.endpoints().ingest_url();
  // Always set: the configured base image, or this version's published one.
  let base_image = state.config().runner_base_image.clone().unwrap_or_default();
  Json(EndpointDeploymentsConfig {
    enabled: ingest_url.is_some(),
    ingest_url: ingest_url.map(str::to_owned),
    base_image: Some(base_image),
  })
}

/// The registration warning when the endpoint's sidecar isn't this
/// server's version: it names the base image to rebuild on.
fn version_warning(theirs: &str, ours: &str, base_image: Option<&str>) -> Option<String> {
  if theirs == ours {
    return None;
  }
  let theirs = if theirs.is_empty() {
    "of an unknown version"
  } else {
    theirs
  };
  let rebuild = match base_image.filter(|image| !image.is_empty()) {
    Some(image) => format!("Rebuild the image FROM {image}."),
    None => "Rebuild the image on the matching base image.".to_owned(),
  };
  Some(format!(
    "your compute endpoint runs sidecar {theirs}; this server is {ours}. {rebuild}"
  ))
}

/// `POST /endpoint-deployments`.
pub(super) async fn register_endpoint(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  body: Result<Json<RegisterEndpoint>, JsonRejection>,
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
  // Before the deploy start: a failed probe leaves no new app behind, and
  // the version the deploy start picks isn't held through a cold start.
  let probe = match endpoints.probe(&url).await {
    Ok(probe) => probe,
    Err(e) => return unprocessable(e),
  };
  let (app, version) = match prepare_deploy(&state, org_id, &parsed).await {
    Ok(prepared) => prepared,
    Err(e) => {
      tracing::warn!(error = %e, "endpoint deployment rejected");
      return unprocessable(e);
    }
  };
  let warnings: Vec<String> = version_warning(
    &probe.version,
    env!("CARGO_PKG_VERSION"),
    state.config().runner_base_image.as_deref(),
  )
  .into_iter()
  .collect();

  let registry = state.registry();
  let deployment = match registry
    .create_deployment(NewDeployment {
      app_id: app.id,
      version,
      // Placeholder until the endpoint's manifest lands.
      manifest: super::placeholder_manifest(),
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
  (
    StatusCode::ACCEPTED,
    Json(Registered {
      deployment: dto,
      warnings,
    }),
  )
    .into_response()
}

/// `POST /endpoint-deployments`'s 202: the deployment, plus any warnings
/// about the endpoint.
#[derive(Serialize)]
struct Registered {
  #[serde(flatten)]
  deployment: super::DeploymentDto,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  warnings: Vec<String>,
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
  let outcome = super::record_deploy_outcome(&state, &deployment, result, Some("endpoint")).await;
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
  let (imported, uses_uv) = match answer {
    ManifestAnswer::Manifest { manifest, uses_uv } => {
      // The endpoint's word isn't enough: this server's own ingest must
      // have seen the dial.
      if !*dialed.borrow() {
        return Err(
          "your compute endpoint answered with a manifest, but its sidecar never dialed this server with \
           the registration token"
            .into(),
        );
      }
      if !manifest.is_object() {
        return Err("your compute endpoint's manifest answer has no manifest object".into());
      }
      if manifest.get("routes").is_some_and(|r| !r.is_array()) {
        return Err("your compute endpoint's manifest has a `routes` that isn't a list".into());
      }
      (manifest, uses_uv)
    }
    ManifestAnswer::TokenRejected => {
      return Err(format!(
        "your compute endpoint's sidecar {WRONG_INGEST} registration. Check that \
         its SECTR_INGEST_URL is {ingest_url}."
      ));
    }
    ManifestAnswer::DialFailed { message } => {
      return Err(format!(
        "your compute endpoint's sidecar {DIAL_FAILED} {ingest_url}: {message}. Check that your platform \
         allows outbound connections."
      ));
    }
    ManifestAnswer::ImportFailed {
      exit_code,
      stderr_tail,
      message,
    } => {
      let exit = match exit_code {
        Some(code) => format!("exit {code}"),
        None => message.unwrap_or_default(),
      };
      return Err(format!("{IMPORT_FAILED} ({exit}): {stderr_tail}"));
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

#[cfg(test)]
mod tests {
  use super::version_warning;

  #[test]
  fn a_version_warning_names_the_base_image() {
    assert_eq!(version_warning("1.2.3", "1.2.3", Some("img:v1.2.3")), None);
    assert_eq!(
      version_warning(
        "1.2.2",
        "1.2.3",
        Some("ghcr.io/sectr-dev/sectr-runner-base:v1.2.3")
      )
      .as_deref(),
      Some(
        "your compute endpoint runs sidecar 1.2.2; this server is 1.2.3. Rebuild the image FROM \
         ghcr.io/sectr-dev/sectr-runner-base:v1.2.3."
      )
    );
    let unknown = version_warning("", "1.2.3", None).unwrap();
    assert!(
      unknown.starts_with("your compute endpoint runs sidecar of an unknown version;"),
      "{unknown}"
    );
    assert!(
      unknown.ends_with("on the matching base image."),
      "{unknown}"
    );
  }
}
