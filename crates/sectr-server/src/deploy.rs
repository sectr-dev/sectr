//! The deploy path — tarball upload → artifact store → image build →
//! authoritative manifest → activation.
//!
//! Flow:
//! 1. `POST /deployments` (multipart: tarball + `sectr.toml` contents,
//!    platform key): resolve-or-create the app (CLI-first creation),
//!    version = max+1, tarball → artifact store, deployment row in
//!    `building`, provisional `build_ref` (the docker tag the build will
//!    produce) → answer **202 immediately** with the row in `building`.
//! 2. The build runs in a background tokio task (builds never ride the
//!    request — Cloudflare's ~100s proxy limit and build-time growth are both
//!    off the table); the CLI polls
//!    `GET /apps/{id}/deployments` (endpoint already exists):
//!    - unpack the tarball into the build dir,
//!    - `docker build`: `FROM $SECTR_RUNNER_BASE_IMAGE` + `COPY app /app`
//!      (+ `uv sync` when the app ships a pyproject.toml — deps bake at
//!      BUILD time so the runner never needs the network),
//!    - authoritative manifest: run the image once with
//!      `SECTR_MANIFEST=1` — a REAL import (no AST parsing; the app's own
//!      interpreter inside the image). `sectr.inspect` writes the JSON via
//!      `SECTR_MANIFEST_OUT` to a bind-mounted file (the user module's
//!      import may print to stdout),
//!    - the platform composes the final manifest: the import result +
//!      `runner.{image,command,workdir}` (what the dispatcher +
//!      spawner consume) + `approval_tools` from sectr.toml.
//!    - row → `active` (previous active → `retired`), or `failed` with
//!      the error logged. Failed deploys never take routing:
//!      `active_deployment` keeps serving the last good one.
//!    - in both outcomes the row's `build_ref` is cleared (the build has
//!      resolved). A server crash mid-build leaves the row `building`
//!      with its `build_ref` — the startup sweep
//!      ([`sweep_stale_building`]) fails those rows (in-process builds
//!      die with the process; a remote-builder re-check lands with the
//!      RunnerPlatform-seam refactor).
//! 3. `POST /deployments/{id}/activate` — dashboard rollback / promote.
//!    Sessions keep their pinned deployments.
//!
//! # MVP caveats
//!
//! - [`sweep_stale_building`] assumes a SINGLE server instance: at startup
//!   every `building` row is by definition orphaned. Multi-instance needs a
//!   leader lock (or a remote re-check of in-flight builds).
//! - Deployment version allocation is max+1 and races under concurrent
//!   deploys of the same app — accepted at MVP concurrency; the store's
//!   UNIQUE (app_id, version) constraint surfaces the loser as 422.
//!   Same-family caveat (R3 finding): the tarball key shares the version
//!   (`{org}/{app}/{version}.tar.gz`) and `put` runs BEFORE the row insert,
//!   so two concurrent deploys can overwrite each other's tarball before
//!   the loser 422s — the winner's row then points at the loser's bytes.
//!   Fix shape (deliberate change, not a drive-by): create the row first
//!   and put to a deployment-id-scoped key.
//! - The terminal write sequence in [`run_build`] (error → status →
//!   build_ref clear) is not atomic: a crash mid-sequence can leave a
//!   `failed` row with its `build_ref` still set (harmless — forensic) or
//!   a cleared-ref `building` row (the startup sweep repairs it to
//!   `failed`).
//! - The provisional-build_ref write-before-spawn ordering is enforced only
//!   by the code path (deploy() sets it before `tokio::spawn`), not by any
//!   deterministic test — the 202 test asserts end states only, since the
//!   dockerless build resolves faster than a mid-flight assertion can
//!   observe (R1 follow-up). A real-docker E2E could read the row while
//!   `building` if this invariant ever needs a regression test.

use chrono::Utc;
use sectr_common::registry::RegistryError;
use sqlx::PgPool;

use axum::extract::{DefaultBodyLimit, Multipart, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use opentelemetry::KeyValue;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use sectr_common::registry::{Deployment, DeploymentStatus, Registry};
use sectr_common::sidecar::{APPROVAL_POLICY_ENV, APPROVAL_POLICY_ENV_IS_RESERVED};

use crate::auth::PlatformPrincipal;
use crate::platform::{BuildOutput, BuildRequest};
use crate::state::AppState;

/// Multipart cap: tarballs are small (source code), but a 2 MB default
/// would be artificial — 64 MB covers any MVP app with assets.
const DEPLOY_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn deploy_router() -> Router<AppState> {
  Router::new()
    .route("/deployments", post(upload_deployment))
    .route(
      "/deployments/{deploymentId}/activate",
      post(activate_deployment),
    )
    .route("/apps/{appId}/deployments", get(list_deployments))
    .layer(DefaultBodyLimit::max(DEPLOY_BODY_LIMIT))
}

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// The public Deployment shape (public-api.yaml): registry row + the
/// computed `active` flag (is this the app's routing target right now).
/// `error` is absent (not null) unless the build failed — the yaml schema
/// keeps it optional to match.
#[derive(Serialize)]
struct DeploymentDto {
  id: Uuid,
  app_id: Uuid,
  version: i64,
  status: DeploymentStatus,
  created_at: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<String>,
  active: bool,
}

async fn deployment_dto(row: Deployment, registry: &dyn Registry) -> Result<DeploymentDto, String> {
  let active = registry
    .active_deployment(row.app_id)
    .await
    .map_err(|e| e.to_string())?
    .is_some_and(|d| d.id == row.id);
  Ok(DeploymentDto {
    id: row.id,
    app_id: row.app_id,
    version: row.version,
    status: row.status,
    created_at: row.created_at.to_rfc3339(),
    error: row.error,
    active,
  })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn upload_deployment(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  mut multipart: Multipart,
) -> Response {
  let org_id = principal.0.org_id();

  let mut tarball: Option<Vec<u8>> = None;
  let mut manifest_toml: Option<String> = None;
  while let Ok(Some(field)) = multipart.next_field().await {
    match field.name().unwrap_or_default() {
      "tarball" => match field.bytes().await {
        Ok(bytes) => tarball = Some(bytes.to_vec()),
        Err(e) => return bad_request(format!("tarball unreadable: {e}")),
      },
      "manifest" => match field.text().await {
        Ok(text) => manifest_toml = Some(text),
        Err(e) => return bad_request(format!("manifest unreadable: {e}")),
      },
      // Unknown fields ignored (forward compat).
      _ => {}
    }
  }
  let (Some(tarball), Some(manifest_toml)) = (tarball, manifest_toml) else {
    return bad_request("multipart body requires 'tarball' and 'manifest' fields".into());
  };

  let parsed = match parse_sectr_toml(&manifest_toml) {
    Ok(parsed) => parsed,
    Err(e) => return bad_request(e),
  };

  // 202 (not 201): the deployment exists but is not resolved — the build
  // runs off-request. Validation errors above stay
  // synchronous 4xx.
  match deploy(&state, org_id, &parsed, tarball).await {
    Ok(dto) => (StatusCode::ACCEPTED, Json(dto)).into_response(),
    Err(e) => {
      tracing::error!(error = %e, "deploy rejected");
      (StatusCode::UNPROCESSABLE_ENTITY, e).into_response()
    }
  }
}

/// Dashboard rollback / promote: retire the current active, activate the
/// target. Sessions keep their pinned deployments.
async fn activate_deployment(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  AxumPath(deployment_id): AxumPath<Uuid>,
) -> Response {
  let org_id = principal.0.org_id();
  let registry = state.registry();
  let Ok(deployment) = registry.get_deployment(deployment_id).await else {
    return StatusCode::NOT_FOUND.into_response();
  };
  // Org boundary: the deployment's app must belong to the caller's org.
  let Ok(app) = registry.get_app(deployment.app_id).await else {
    return StatusCode::NOT_FOUND.into_response();
  };
  if app.org_id != org_id {
    return StatusCode::NOT_FOUND.into_response();
  }
  // Activation guard: only a row whose build RESOLVED may take routing.
  // Activating a `building` row would retire the currently-active
  // deployment and route to a placeholder manifest; a `failed` row has
  // no usable manifest at all. `retired` → activate is the rollback path.
  // (The shared [`activate`] is NOT guarded: the background build calls
  // it with a `building` row by design.)
  if !matches!(
    deployment.status,
    DeploymentStatus::Active | DeploymentStatus::Retired
  ) {
    return (
      StatusCode::CONFLICT,
      format!(
        "deployment {deployment_id} is not in a ready state \
                 (status: {:?}) — only `active` or `retired` rows can be activated",
        deployment.status
      ),
    )
      .into_response();
  }
  if let Err(e) = activate(&state, deployment_id).await {
    return (StatusCode::UNPROCESSABLE_ENTITY, e).into_response();
  }
  StatusCode::ACCEPTED.into_response()
}

async fn list_deployments(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  AxumPath(app_id): AxumPath<Uuid>,
) -> Response {
  let org_id = principal.0.org_id();
  let registry = state.registry();
  let app = match registry.get_app(app_id).await {
    Ok(a) if a.org_id == org_id => a,
    _ => return StatusCode::NOT_FOUND.into_response(),
  };
  match registry.list_deployments(app.id).await {
    Ok(rows) => {
      let mut dtos = Vec::with_capacity(rows.len());
      for row in rows {
        match deployment_dto(row, registry.as_ref()).await {
          Ok(dto) => dtos.push(dto),
          Err(e) => return (StatusCode::UNPROCESSABLE_ENTITY, e).into_response(),
        }
      }
      Json(dtos).into_response()
    }
    Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()).into_response(),
  }
}

fn bad_request(message: String) -> Response {
  (StatusCode::BAD_REQUEST, message).into_response()
}

// ---------------------------------------------------------------------------
// sectr.toml (the uploaded `manifest` field)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DeployManifest {
  pub app_name: String,
  pub entry: String,
  pub approval_tools: Vec<String>,
}

fn parse_sectr_toml(text: &str) -> Result<DeployManifest, String> {
  let value: toml::Value =
    toml::from_str(text).map_err(|e| format!("sectr.toml is not valid TOML: {e}"))?;
  let entry = value
    .get("agent")
    .and_then(|a| a.get("entry"))
    .and_then(|e| e.as_str())
    .ok_or("sectr.toml is missing agent.entry")?
    .to_owned();
  if entry.is_empty() {
    return Err("agent.entry is empty".into());
  }
  let module = entry.split(':').next().unwrap_or(&entry);
  let app_name = value
    .get("app")
    .and_then(|a| a.get("name"))
    .and_then(|n| n.as_str())
    .map(str::to_owned)
    .unwrap_or_else(|| module.to_owned());
  let approval_tools = value
    .get("deploy")
    .and_then(|d| d.get("approval_tools"))
    .and_then(|t| t.as_array())
    .map(|a| {
      a.iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect()
    })
    .unwrap_or_default();
  Ok(DeployManifest {
    app_name,
    entry,
    approval_tools,
  })
}

/// App-name → slug (matches `create_app`'s naming: apps carry name = slug
/// in MVP, so the deploy-created slug must be stable across redeploys).
fn slugify(name: &str) -> String {
  let slug: String = name
    .chars()
    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
    .collect::<String>()
    .to_lowercase();
  let slug = slug.trim_matches('-').to_owned();
  if slug.is_empty() { "app".into() } else { slug }
}

// ---------------------------------------------------------------------------
// The deploy pipeline
// ---------------------------------------------------------------------------

async fn deploy(
  state: &AppState,
  org_id: Uuid,
  parsed: &DeployManifest,
  tarball: Vec<u8>,
) -> Result<DeploymentDto, String> {
  let registry = state.registry();
  let slug = slugify(&parsed.app_name);
  let reg_err = |e: sectr_common::registry::RegistryError| e.to_string();

  // CLI-first creation: the first deploy of an unknown app name creates
  // the app in the caller's org.
  let app = match registry.find_app_by_slug(org_id, &slug).await {
    Ok(Some(app)) => app,
    Ok(None) => registry
      .create_app(sectr_common::registry::NewApp {
        org_id,
        name: parsed.app_name.clone(),
        slug: slug.clone(),
      })
      .await
      .map_err(reg_err)?,
    Err(e) => return Err(reg_err(e)),
  };

  // A secret stored under this name before the secrets API refused it:
  // the sidecar sets the variable over it, so the policy it meant to set
  // would silently not apply.
  let secrets = registry.list_secret_names(app.id).await.map_err(reg_err)?;
  if secrets.iter().any(|s| s.key == APPROVAL_POLICY_ENV) {
    return Err(format!(
      "the app has a secret named {APPROVAL_POLICY_ENV}, which this deployment would ignore: \
       {APPROVAL_POLICY_ENV_IS_RESERVED}, then remove the secret \
       (`sectr secrets rm {APPROVAL_POLICY_ENV}`)"
    ));
  }

  let version = registry
    .list_deployments(app.id)
    .await
    .map_err(reg_err)?
    .iter()
    .map(|d| d.version)
    .max()
    .unwrap_or(0)
    + 1;

  // Tarball → artifact store (the URI rides the row).
  let key = format!("{}/{}/{}.tar.gz", org_id, app.id, version);
  let artifact_uri = state
    .artifact_store()
    .put(&key, tarball)
    .await
    .map_err(|e| format!("artifact store: {e}"))?;

  let deployment = registry
    .create_deployment(sectr_common::registry::NewDeployment {
      app_id: app.id,
      version,
      // Placeholder until the authoritative import lands.
      manifest: json!({"routes": []}),
    })
    .await
    .map_err(reg_err)?;
  registry
    .set_artifact_uri(deployment.id, &artifact_uri)
    .await
    .map_err(reg_err)?;

  // Provisional in-flight-build ref: a platform-owned opaque identifier
  // for the build about to run (model a: the docker tag; model b: the AR
  // ref). Survives a server crash so forensics (and a remote-builder
  // re-check) can find the half-built image; cleared when the build
  // resolves.
  let tag = state
    .platform()
    .provisional_build_ref(deployment.id, deployment.version);
  registry
    .set_deployment_build_ref(deployment.id, Some(&tag))
    .await
    .map_err(reg_err)?;

  // Re-fetch: the task needs the row WITH artifact_uri + build_ref (the
  // create_deployment return predates both setters).
  let deployment = registry
    .get_deployment(deployment.id)
    .await
    .map_err(reg_err)?;

  // The build is off-request: hand the owned state handles to a spawned
  // task and answer immediately. Only the registry is shared state —
  // `AppState` is a bag of Arcs, so the clone is cheap and safe.
  let task_state = state.clone();
  let task_parsed = parsed.clone();
  tokio::spawn(run_build(task_state, deployment.clone(), task_parsed));

  // The row is `building`, never the app's active deployment — active
  // flags only flip in the background task on success.
  deployment_dto(deployment, registry.as_ref()).await
}

/// Background build task: resolves the `building` row to `active` (build
/// success → authoritative manifest → activate) or `failed` (with the
/// failure reason stored on the row for CLI/dashboard surfacing), and
/// clears the `build_ref` either way. Fully self-contained: owns its
/// `AppState` clone, touches nothing shared beyond the registry.
async fn run_build(state: AppState, deployment: Deployment, parsed: DeployManifest) {
  // The background build is a long off-request operation — a
  // dedicated span (app_id/version for context; the platform's own build
  // impl may add child spans), + the outcome counter for dashboards.
  let span = tracing::info_span!(
      "deploy.build",
      deployment_id = %deployment.id,
      app_id = %deployment.app_id,
      version = deployment.version,
      outcome = tracing::field::Empty,
  );
  let _guard = span.enter();
  let registry = state.registry();
  // Product analytics need the org (deployment rows don't carry
  // org_id; apps do). One lookup per build — a failed lookup just skips
  // emission (log-and-degrade, like every other analytics site).
  let org_id = match registry.get_app(deployment.app_id).await {
    Ok(app) => Some(app.org_id),
    Err(e) => {
      tracing::debug!(app = %deployment.app_id, error = %e, "deploy analytics: app lookup failed");
      None
    }
  };
  let deploy_props =
    || serde_json::json!({ crate::analytics::PROP_APP_ID: deployment.app_id.to_string() });
  if let Err(e) = build_and_activate(&state, &deployment, &parsed).await {
    span.record("outcome", "failed");
    crate::metrics::DEPLOYS.add(1, &[KeyValue::new("outcome", "failed")]);
    if let Some(org_id) = org_id {
      state.analytics().track(
        crate::analytics::events::DEPLOY_FAIL,
        org_id,
        deploy_props(),
      );
    }
    tracing::error!(deployment = %deployment.id, error = %e, "background deploy build failed");
    let tail = error_tail(&e);
    if let Err(e) = registry
      .set_deployment_build_error(deployment.id, Some(&tail))
      .await
    {
      tracing::error!(deployment = %deployment.id, error = %e, "failed to store build error");
    }
    if let Err(e) = registry
      .set_deployment_status(deployment.id, DeploymentStatus::Failed)
      .await
    {
      tracing::error!(deployment = %deployment.id, error = %e, "failed to mark deployment failed");
    }
  } else {
    span.record("outcome", "success");
    crate::metrics::DEPLOYS.add(1, &[KeyValue::new("outcome", "success")]);
    if let Some(org_id) = org_id {
      state.analytics().track(
        crate::analytics::events::DEPLOY_SUCCESS,
        org_id,
        deploy_props(),
      );
    }
    if let Err(e) = registry
      .set_deployment_build_error(deployment.id, None)
      .await
    {
      // Success clears any stale error from an earlier failed attempt.
      tracing::error!(deployment = %deployment.id, error = %e, "failed to clear build error");
    }
  }
  // The build has resolved (either way) — clear the in-flight ref.
  if let Err(e) = registry.set_deployment_build_ref(deployment.id, None).await {
    tracing::error!(deployment = %deployment.id, error = %e, "failed to clear build_ref");
  }
}

/// Cap on the stored build-failure tail (bytes).
const BUILD_ERROR_TAIL_CAP: usize = 2 * 1024;
/// Max trailing lines of the build/failure output kept in the row.
const BUILD_ERROR_TAIL_LINES: usize = 10;

/// The failed row's `error` payload: the LAST `BUILD_ERROR_TAIL_LINES`
/// lines of the failure output, further truncated to the last
/// `BUILD_ERROR_TAIL_CAP` bytes (tail, not head — the failure reason is
/// almost always at the end of a build log).
fn error_tail(output: &str) -> String {
  let tail = output
    .lines()
    .rev()
    .take(BUILD_ERROR_TAIL_LINES)
    .collect::<Vec<_>>()
    .into_iter()
    .rev()
    .collect::<Vec<_>>()
    .join("\n");
  if tail.len() <= BUILD_ERROR_TAIL_CAP {
    return tail;
  }
  // Walk back from the end to the smallest char boundary within the cap,
  // so the TAIL (not the head) survives and no UTF-8 char is split.
  let start = tail
    .char_indices()
    .rev()
    .take_while(|(i, _)| tail.len() - i <= BUILD_ERROR_TAIL_CAP)
    .map(|(i, _)| i)
    .last()
    .unwrap_or(tail.len());
  tail[start..].to_owned()
}

/// The task's success-or-fail body, split out so `run_build` stays a flat
/// log-and-mark wrapper.
async fn build_and_activate(
  state: &AppState,
  deployment: &Deployment,
  parsed: &DeployManifest,
) -> Result<(), String> {
  let artifact_uri = deployment
    .artifact_uri
    .as_deref()
    .ok_or("deployment has no artifact_uri — cannot build")?;
  // Layer-cache hint for the remote builder: the previous deployment's
  // image ref, computed THROUGH the platform (`provisional_build_ref` is
  // the platform's own deterministic naming — none of it leaks here).
  // Image refs are deployment-scoped while the version counter is
  // app-scoped, so the previous image belongs to the previous row's id —
  // not to `version - 1` under this one.
  let previous_build_ref = match state.registry().list_deployments(deployment.app_id).await {
    Ok(rows) => rows
      .iter()
      .filter(|d| d.version < deployment.version)
      .max_by_key(|d| d.version)
      .map(|prev| {
        state
          .platform()
          .provisional_build_ref(prev.id, prev.version)
      }),
    // Only a cache hint — degrade, but leave a trace (a silent None
    // would hide a registry problem behind 'cold build every time').
    Err(e) => {
      tracing::warn!(deployment = %deployment.id, error = %e,
                "could not list deployments for the previous-image cache hint");
      None
    }
  };
  // The build is behind the RunnerPlatform seam: the ONE platform object shared with the ingest
  // hub/dispatcher — the same task, whichever backend it is.
  let output = state
    .platform()
    .build(BuildRequest {
      deployment_id: deployment.id,
      version: deployment.version,
      artifact_uri: artifact_uri.to_owned(),
      entry: parsed.entry.clone(),
      previous_build_ref,
    })
    .await
    // The row's `error` tail: message + the builder's own log tail (the
    // latter is the part worth reading — the BuildError carries the
    // failing step's output; error_tail in run_build trims the union).
    .map_err(|e| {
      if e.logs_tail.trim().is_empty() {
        e.message
      } else {
        format!("{}\n--- build log tail ---\n{}", e.message, e.logs_tail)
      }
    })?;
  let manifest = compose_manifest(&output, parsed);
  state
    .registry()
    .set_deployment_manifest(deployment.id, &manifest)
    .await
    .map_err(|e| e.to_string())?;
  activate(state, deployment.id).await
}

/// Compose the final manifest from the builder's output: what the runtime
/// consumes (routes for `deployment_serves_route`, env declarations for WS
/// G, runner.* for the spawner + dispatcher). Cloud-neutral — the
/// builder returns the raw import; this composition stays server-side.
fn compose_manifest(output: &BuildOutput, parsed: &DeployManifest) -> Value {
  let imported = &output.imported_manifest;
  let mut runner_command = if output.uses_uv {
    vec![
      "uv".into(),
      "run".into(),
      "--no-sync".into(),
      "python3".into(),
      "-m".into(),
      "sectr.server".into(),
    ]
  } else {
    vec!["python3".into(), "-m".into(), "sectr.server".into()]
  };
  runner_command.push(parsed.entry.clone());
  json!({
      "name": imported.get("name").unwrap_or(&json!(parsed.app_name)).clone(),
      "entry": parsed.entry,
      "python": imported.get("python").cloned().unwrap_or(json!(null)),
      "routes": imported.get("routes").cloned().unwrap_or(json!([])),
      "env": imported.get("env").cloned().unwrap_or(json!([])),
      "runner": {
          "image": output.image_ref,
          "command": runner_command,
          "workdir": "/app",
      },
      "approval_tools": parsed.approval_tools,
  })
}

/// Routing handover: retire the current active, activate `deployment_id`.
async fn activate(state: &AppState, deployment_id: Uuid) -> Result<(), String> {
  let registry = state.registry();
  let deployment = registry
    .get_deployment(deployment_id)
    .await
    .map_err(|e| e.to_string())?;
  // Whether this app already had an active deployment BEFORE the swap —
  // an app re-activating itself (deploy over deploy, rollback, promote)
  // can never be the org's FIRST active app (analytics emission below).
  // `Some(this row)` ⇒ activate is a routing no-op: return early, emit
  // nothing.
  let mut was_app_active = false;
  match registry
    .active_deployment(deployment.app_id)
    .await
    .map_err(|e| e.to_string())?
  {
    Some(current) if current.id == deployment_id => return Ok(()),
    Some(current) => {
      was_app_active = true;
      registry
        .set_deployment_status(current.id, DeploymentStatus::Retired)
        .await
        .map_err(|e| e.to_string())?;
    }
    None => {}
  }
  registry
    .set_deployment_status(deployment_id, DeploymentStatus::Active)
    .await
    .map_err(|e| e.to_string())?;

  // `first_app_deployed` — the org's first deployment ever to
  // reach `active` (the funnel's middle step). Emit-time firstness: this
  // app had no prior active AND no OTHER org app has one right now. The
  // checks are indexed store reads on the build/human-paced activate
  // paths; track() itself is fire-and-forget.
  if !was_app_active
    && let Ok(app) = registry.get_app(deployment.app_id).await
    && let Ok(apps) = registry.list_apps(app.org_id).await
  {
    let mut first = true;
    for other in &apps {
      if other.id == deployment.app_id {
        continue;
      }
      let other_active = registry
        .active_deployment(other.id)
        .await
        .map(|d| d.is_some())
        .unwrap_or(false);
      if other_active {
        first = false;
        break;
      }
    }
    if first {
      state.analytics().track(
        crate::analytics::events::FIRST_APP_DEPLOYED,
        app.org_id,
        serde_json::json!({
            crate::analytics::PROP_APP_ID: deployment.app_id.to_string()
        }),
      );
    }
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// Startup sweep: orphaned `building` rows
// ---------------------------------------------------------------------------

/// What swept rows say about themselves — there is no real output to keep
/// (the process that built died), so this is a definite statement instead.
const STALE_BUILDING_ERROR: &str = "build did not complete: server restarted mid-build";

/// Fail every `building` deployment older than `older_than`. Called once at
/// server startup: a row still in `building` when the process starts is by
/// definition orphaned — the background tokio task that would have resolved
/// it died with the previous process (prod `older_than` is zero: no build
/// of this process can exist yet). The row's `build_ref` is preserved for
/// forensics ([`Registry::set_deployment_status`] doesn't touch it). Remote
/// builders (Cloud Build) get a proper re-check instead with the
/// RunnerPlatform refactor.
///
/// The id enumeration rides the store pool directly (the `Registry` trait
/// has no "list by status" — by design; this is a one-shot startup
/// maintenance query, not an API concern) and the marking goes through the
/// registry so the transition rules stay in one place. Returns the number
/// of rows swept.
pub async fn sweep_stale_building(
  pool: &PgPool,
  registry: &dyn Registry,
  older_than: chrono::Duration,
) -> Result<usize, RegistryError> {
  let ids: Vec<Uuid> =
    sqlx::query_scalar("SELECT id FROM deployments WHERE status = 'building' AND created_at <= $1")
      .bind(Utc::now() - older_than)
      .fetch_all(pool)
      .await?;
  for id in &ids {
    registry
      .set_deployment_status(*id, DeploymentStatus::Failed)
      .await?;
    registry
      .set_deployment_build_error(*id, Some(STALE_BUILDING_ERROR))
      .await?;
  }
  if !ids.is_empty() {
    tracing::warn!(count = ids.len(), ids = ?ids, "swept stale building deployments to failed");
  }
  Ok(ids.len())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  use axum::body::Body;
  use tower::ServiceExt;

  use sectr_common::registry::{ApiKeyKind, NewApp, NewDeployment};
  use sectr_common::store::PgStore;

  use crate::auth::keys;
  use crate::config::ServerConfig;
  use crate::dispatch::UnimplementedDispatcher;
  use crate::http;
  use crate::ingest::IngestHub;
  use crate::sessions::ProdSessionService;
  use crate::state::AppState;
  use crate::test_pg;

  async fn test_state(store: &PgStore) -> AppState {
    test_state_with(store, Arc::new(crate::analytics::Analytics::disabled())).await
  }

  /// The state with an ATTACHED analytics client — the analytics emission
  /// tests wire one to the in-process capture stub.
  async fn test_state_with(
    store: &PgStore,
    analytics: Arc<crate::analytics::Analytics>,
  ) -> AppState {
    let registry: Arc<dyn Registry> = Arc::new(store.registry());
    let journal: Arc<dyn sectr_common::journal::Journal> = Arc::new(store.journal());
    let service = Arc::new(ProdSessionService::new(
      registry.clone(),
      journal.clone(),
      Arc::new(UnimplementedDispatcher),
      chrono::Duration::seconds(30),
    ));
    let hub = IngestHub::new(
      registry.clone(),
      journal.clone(),
      chrono::Duration::seconds(30),
    );
    let config = test_config();
    AppState::new(
      registry.clone(),
      journal,
      service,
      Arc::new(hub),
      config.clone(),
      Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
      // Dockerless on purpose: the background build fails fast and
      // the test asserts the failed lifecycle (no daemon in CI).
      Arc::new(crate::platform::DockerPlatform::new(
        Arc::new(sectr_common::drivers::InMemoryArtifactStore::default()),
        None,
        config,
        registry,
      )),
      false,
    )
    .with_analytics(analytics)
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
      // None → the background build fails at the first step (no base
      // image) without needing the tarball unpacked or docker reached.
      runner_base_image: None,
      build_dir: "/tmp/sectr-build".into(),
      dashboard_url: "https://app.sectr.test".into(),
    }
  }

  /// Poll the registry until the deployment leaves `building` AND its
  /// build_ref is cleared (the build task is unsynchronized by design:
  /// the failed-marking and the ref-clear are two registry writes).
  async fn wait_resolved(registry: &dyn Registry, id: Uuid) -> Deployment {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
      let row = registry.get_deployment(id).await.unwrap();
      if row.status != DeploymentStatus::Building && row.build_ref.is_none() {
        return row;
      }
      assert!(
        tokio::time::Instant::now() < deadline,
        "deployment {id} never left building"
      );
      tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
  }

  #[tokio::test]
  async fn deploy_is_202_and_background_build_resolves_to_failed() {
    let Some(store) = test_pg::fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let state = test_state(&store).await;
    let registry = state.registry();
    let org = registry.create_org("test-org", "default").await.unwrap();

    let parsed = DeployManifest {
      app_name: "poll-app".into(),
      entry: "agent:agent".into(),
      approval_tools: vec![],
    };
    // Tarball bytes are irrelevant: the build fails at the base-image
    // check, before the artifact is ever unpacked.
    let dto = deploy(&state, org.id, &parsed, b"fake-tarball".to_vec())
      .await
      .expect("upload path must succeed synchronously");

    // 202 semantics: the row exists in `building`, not yet routed.
    assert_eq!(dto.status, DeploymentStatus::Building);
    assert!(!dto.active);
    assert_eq!(
      registry.active_deployment(dto.app_id).await.unwrap(),
      None,
      "a building deployment must not take routing"
    );
    // NOTE: do NOT assert the provisional build_ref here. It is written
    // before the spawn (deploy(), build_tag) and cleared when the build
    // resolves — but the dockerless build fails in milliseconds, so the
    // task has usually already cleared it by the time this test fetches
    // the row. Asserting a transient mid-flight state is a race (this
    // exact assertion flaked when a refactor shifted timing).
    // The end-state assertions below cover the invariant deterministically.

    // The background task fails fast (no base image) → `failed`,
    // build_ref cleared, manifest untouched (placeholder), and the row
    // carries the failure reason for the CLI/dashboard.
    let row = wait_resolved(registry.as_ref(), dto.id).await;
    assert_eq!(row.status, DeploymentStatus::Failed);
    assert_eq!(row.build_ref, None);
    assert_eq!(row.manifest, json!({"routes": []}));
    let error = row.error.expect("failed row must carry an error");
    assert!(!error.trim().is_empty());
    assert!(
      error.contains("SECTR_RUNNER_BASE_IMAGE"),
      "error must name the actual failure: {error}"
    );
  }

  #[tokio::test]
  async fn deploy_rejects_an_app_with_an_approval_policy_secret() {
    let Some(store) = test_pg::fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let state = test_state(&store).await;
    let registry = state.registry();
    let org = registry.create_org("test-org", "default").await.unwrap();
    let app = registry
      .create_app(sectr_common::registry::NewApp {
        org_id: org.id,
        name: "policy-app".into(),
        slug: "policy-app".into(),
      })
      .await
      .unwrap();
    // Stored before the secrets API refused the name.
    registry
      .set_secret(
        app.id,
        APPROVAL_POLICY_ENV,
        sectr_common::secret::Secret::new("[\"refund\"]"),
      )
      .await
      .unwrap();

    let parsed = DeployManifest {
      app_name: "policy-app".into(),
      entry: "agent:agent".into(),
      approval_tools: vec![],
    };
    let Err(err) = deploy(&state, org.id, &parsed, b"fake-tarball".to_vec()).await else {
      panic!("the secret must be refused");
    };
    assert!(err.contains("secret named SECTR_APPROVAL_POLICY"), "{err}");
    assert!(err.contains("[deploy] approval_tools"), "{err}");
    assert!(registry.list_deployments(app.id).await.unwrap().is_empty());

    registry
      .delete_secret(app.id, APPROVAL_POLICY_ENV)
      .await
      .unwrap();
    deploy(&state, org.id, &parsed, b"fake-tarball".to_vec())
      .await
      .expect("deploys once the secret is gone");
  }

  #[tokio::test]
  async fn analytics_emits_deploy_outcomes_and_first_app_deployed() {
    let Some(store) = test_pg::fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let (host, captured) = crate::analytics::testutil::capture_server().await;
    let state = test_state_with(
      &store,
      Arc::new(crate::analytics::Analytics::new("phc_test", Some(host))),
    )
    .await;
    let registry = state.registry();
    let org = registry.create_org("test-org", "default").await.unwrap();
    let app = registry
      .create_app(NewApp {
        org_id: org.id,
        name: "app".into(),
        slug: "app".into(),
      })
      .await
      .unwrap();
    let deployment = registry
      .create_deployment(NewDeployment {
        app_id: app.id,
        version: 1,
        manifest: json!({"routes": [{"path": "/test"}]}),
      })
      .await
      .unwrap();
    registry
      .set_artifact_uri(deployment.id, "in-memory://tarball")
      .await
      .unwrap();

    // Dockerless build fails fast → exactly one deploy_fail, no
    // deploy_success, no first_app_deployed (the build never reached
    // activation).
    let parsed = DeployManifest {
      app_name: "app".into(),
      entry: "agent:agent".into(),
      approval_tools: vec![],
    };
    run_build(state.clone(), deployment.clone(), parsed).await;
    let captures = crate::analytics::testutil::wait_for_captures(&captured, 1).await;
    let fails: Vec<_> = captures
      .iter()
      .filter(|c| c["event"] == "deploy_fail")
      .collect();
    assert_eq!(fails.len(), 1, "exactly one deploy_fail capture");
    assert_eq!(fails[0]["distinct_id"], org.id.to_string());
    assert_eq!(fails[0]["properties"]["app_id"], app.id.to_string());
    assert!(
      !captures.iter().any(|c| c["event"] == "deploy_success"),
      "a failed build never emits deploy_success"
    );
    assert!(
      !captures.iter().any(|c| c["event"] == "first_app_deployed"),
      "a failed build never activates — no first_app_deployed"
    );

    // Activation routes through `activate` for BOTH the deploy path and
    // the dashboard's promote/rollback — first org app to go active
    // emits first_app_deployed exactly once.
    activate(&state, deployment.id).await.unwrap();
    let captures = crate::analytics::testutil::wait_for_captures(&captured, 2).await;
    let firsts: Vec<_> = captures
      .iter()
      .filter(|c| c["event"] == "first_app_deployed")
      .collect();
    assert_eq!(firsts.len(), 1);
    assert_eq!(firsts[0]["distinct_id"], org.id.to_string());

    // Re-activating the already-active deployment is a routing no-op —
    // no duplicate emission.
    activate(&state, deployment.id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let captures = captured.lock().unwrap().clone();
    assert_eq!(
      captures
        .iter()
        .filter(|c| c["event"] == "first_app_deployed")
        .count(),
      1
    );

    // A second app in the SAME org activating does NOT emit again —
    // the org already has its first active app.
    let app_b = registry
      .create_app(NewApp {
        org_id: org.id,
        name: "app-b".into(),
        slug: "app-b".into(),
      })
      .await
      .unwrap();
    let deployment_b = registry
      .create_deployment(NewDeployment {
        app_id: app_b.id,
        version: 1,
        manifest: json!({"routes": [{"path": "/test"}]}),
      })
      .await
      .unwrap();
    activate(&state, deployment_b.id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let captures = captured.lock().unwrap().clone();
    assert_eq!(
      captures
        .iter()
        .filter(|c| c["event"] == "first_app_deployed")
        .count(),
      1,
      "first_app_deployed fires once per org"
    );
  }

  /// The generated Dockerfile is the single source of the runner-image
  /// shape (docker builder today; the CloudBuild builder must reuse it
  /// verbatim). One Dockerfile covers BOTH app shapes: the
  /// `if [ -f pyproject.toml ]` guard keeps dependency-free apps
  /// building, and dep-bearing ones get the `../../sdk-python` →
  /// `/sdk-python` rewrite in pyproject.toml AND uv.lock, then a frozen
  /// `uv sync` (bakes deps at build time).
  #[test]
  fn dockerfile_generation_keeps_sdk_rewrite_and_uv_guard() {
    let dockerfile = crate::platform::docker::generate_dockerfile("sectr/runner:latest");
    assert_eq!(
      dockerfile.lines().next().unwrap(),
      "FROM sectr/runner:latest",
      "base image comes first"
    );
    assert!(dockerfile.contains("COPY app /app\nWORKDIR /app\n"));
    // The /sdk-python symlink (the examples' editable dep resolves there).
    assert!(dockerfile.contains("RUN ln -s /opt/sdk-python /sdk-python\n"));
    let guard = dockerfile
      .lines()
      .find(|l| l.contains("pyproject.toml"))
      .expect("uv guard line must be present");
    assert!(
      guard.contains("sed -i 's#\\.\\./\\.\\./sdk-python#/sdk-python#g' pyproject.toml uv.lock"),
      "sdk-python rewrite must hit pyproject.toml AND uv.lock: {guard}"
    );
    assert!(guard.contains("uv sync --frozen"), "{guard}");
  }

  #[test]
  fn error_tail_keeps_the_last_lines_within_the_cap() {
    // Short output passes through verbatim.
    assert_eq!(error_tail("boom"), "boom");

    // Last 10 lines only.
    let output = (0..15)
      .map(|i| format!("line-{i}"))
      .collect::<Vec<_>>()
      .join("\n");
    let tail = error_tail(&output);
    assert!(!tail.contains("line-4"));
    assert!(tail.starts_with("line-5"));
    assert!(tail.ends_with("line-14"));

    // Byte cap: the tail (not the head) survives a huge single line,
    // and the truncation never splits a UTF-8 char.
    let long = format!("{}\u{00e9}{}", "x".repeat(5000), "|FINAL CAUSE|".repeat(3));
    let tail = error_tail(&long);
    assert!(tail.len() <= 2 * 1024);
    assert!(tail.ends_with("|FINAL CAUSE||FINAL CAUSE||FINAL CAUSE|"));
  }

  #[tokio::test]
  async fn sweep_fails_stale_building_rows_and_preserves_build_ref() {
    let Some(store) = test_pg::fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let registry: Arc<dyn Registry> = Arc::new(store.registry());
    let org = registry.create_org("test-org", "default").await.unwrap();
    let app = registry
      .create_app(NewApp {
        org_id: org.id,
        name: "sweep-app".into(),
        slug: "sweep-app".into(),
      })
      .await
      .unwrap();
    // An orphaned build: created `building` (as create_deployment does)
    // with a build_ref, and nothing will ever resolve it.
    let stuck = registry
      .create_deployment(sectr_common::registry::NewDeployment {
        app_id: app.id,
        version: 1,
        manifest: json!({"routes": []}),
      })
      .await
      .unwrap();
    registry
      .set_deployment_build_ref(stuck.id, Some("sectr-app-x:v1"))
      .await
      .unwrap();

    // Grace larger than the row's age: untouched.
    let swept = sweep_stale_building(
      store.pool(),
      registry.as_ref(),
      chrono::Duration::minutes(5),
    )
    .await
    .unwrap();
    assert_eq!(swept, 0);
    assert_eq!(
      registry.get_deployment(stuck.id).await.unwrap().status,
      DeploymentStatus::Building
    );

    // Zero grace (prod startup): the row fails with the static
    // message, build_ref preserved.
    let swept = sweep_stale_building(store.pool(), registry.as_ref(), chrono::Duration::zero())
      .await
      .unwrap();
    assert_eq!(swept, 1);
    let row = registry.get_deployment(stuck.id).await.unwrap();
    assert_eq!(row.status, DeploymentStatus::Failed);
    assert_eq!(row.build_ref.as_deref(), Some("sectr-app-x:v1"));
    assert_eq!(
      row.error.as_deref(),
      Some("build did not complete: server restarted mid-build"),
      "swept rows must carry the stale-build error"
    );

    // Idempotent: a failed row is not `building` anymore.
    let swept = sweep_stale_building(store.pool(), registry.as_ref(), chrono::Duration::zero())
      .await
      .unwrap();
    assert_eq!(swept, 0);
  }

  async fn activate_req(app: &axum::Router, id: Uuid, key: &str) -> axum::http::StatusCode {
    let request = axum::http::Request::builder()
      .method("POST")
      .uri(format!("/deployments/{id}/activate"))
      .header("Authorization", format!("Bearer {key}"))
      .body(Body::empty())
      .unwrap();
    app.clone().oneshot(request).await.unwrap().status()
  }

  /// The activation guard: `building` and `failed` rows must not take
  /// routing (409) — an unguarded activate would retire the live routing
  /// target and point new sessions at a placeholder or failed manifest.
  /// `retired` → activate is the rollback path and keeps working.
  #[tokio::test]
  async fn activate_rejects_building_and_failed_but_rolls_back_retired() {
    let Some(store) = test_pg::fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let registry: Arc<dyn Registry> = Arc::new(store.registry());
    let org = registry.create_org("test-org", "default").await.unwrap();
    let app = registry
      .create_app(NewApp {
        org_id: org.id,
        name: "guard-app".into(),
        slug: "guard-app".into(),
      })
      .await
      .unwrap();
    let new_row = |version: i64| sectr_common::registry::NewDeployment {
      app_id: app.id,
      version,
      manifest: json!({"routes": []}),
    };

    // A currently-active row (the routing target a bad activate would
    // have retired) + the three candidate states.
    let live = registry.create_deployment(new_row(1)).await.unwrap();
    registry
      .set_deployment_status(live.id, DeploymentStatus::Active)
      .await
      .unwrap();
    let building = registry.create_deployment(new_row(2)).await.unwrap();
    let failed = registry.create_deployment(new_row(3)).await.unwrap();
    registry
      .set_deployment_status(failed.id, DeploymentStatus::Failed)
      .await
      .unwrap();
    let retired = registry.create_deployment(new_row(4)).await.unwrap();
    registry
      .set_deployment_status(retired.id, DeploymentStatus::Active)
      .await
      .unwrap();
    registry
      .set_deployment_status(retired.id, DeploymentStatus::Retired)
      .await
      .unwrap();

    let state = test_state(&store).await;
    let platform = keys::mint(ApiKeyKind::Platform);
    registry
      .create_api_key(
        org.id,
        None,
        ApiKeyKind::Platform,
        "deploy guard test",
        &keys::hash(&platform),
      )
      .await
      .unwrap();
    let router = http::router(state);

    // `building` target: 409, and the live routing target is untouched.
    assert_eq!(
      activate_req(&router, building.id, &platform).await,
      axum::http::StatusCode::CONFLICT,
      "activating a `building` row must be rejected"
    );
    assert_eq!(
      registry
        .active_deployment(app.id)
        .await
        .unwrap()
        .map(|d| d.id),
      Some(live.id),
      "a rejected activate must not retire the current active"
    );

    // `failed` target: 409 too.
    assert_eq!(
      activate_req(&router, failed.id, &platform).await,
      axum::http::StatusCode::CONFLICT,
      "activating a `failed` row must be rejected"
    );

    // `retired` target: rollback still works (202, takes routing).
    assert_eq!(
      activate_req(&router, retired.id, &platform).await,
      axum::http::StatusCode::ACCEPTED,
      "retired → activate is the rollback path"
    );
    assert_eq!(
      registry.get_deployment(retired.id).await.unwrap().status,
      DeploymentStatus::Active
    );
    assert_eq!(
      registry.get_deployment(live.id).await.unwrap().status,
      DeploymentStatus::Retired,
      "the previous active is retired by the successful rollback"
    );
  }
}
