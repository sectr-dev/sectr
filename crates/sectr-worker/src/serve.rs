//! Serve mode (`sectr-worker --serve`): the sidecar as an HTTP server, for
//! platforms that start a container per incoming request (AWS Lambda
//! through the Lambda Web Adapter, Google Cloud Run and similar).
//!
//! - `GET /` identifies the sidecar (`{"sectr_sidecar": {version, mode}}`).
//!   It's at `/` because platform readiness checks use `/`.
//! - `POST /wake` `{"ingest_token": "sit_…"}` runs ONE invocation: a fresh
//!   [`SidecarClient`] dials this image's own `SECTR_INGEST_URL` with the
//!   token, receives the assignment and runs the turn exactly as env mode
//!   does. The request is held open until the turn ends, because
//!   serverless platforms freeze or throttle an instance once its response
//!   is sent.
//! - `POST /manifest` `{"registration_token", "entry"}` proves the loop
//!   (dials ingest with the token) and only then imports the app the same
//!   way managed builds do, returning the manifest.
//!
//! The ingest URL comes only from the image's configuration, never from a
//! request: endpoint URLs are public, and a sidecar that dialed a URL the
//! request named could be pointed at someone else's server, which could
//! then run the agent with its owner's keys. With the destination fixed, a
//! forged wake only makes the sidecar dial Sectr with a token Sectr
//! rejects.
//!
//! One turn at a time per instance: a wake or manifest request while
//! another runs gets `409` at once and is never queued. The sidecar never
//! answers `429` or `503` itself; the server treats those as the platform
//! saying the request never reached a container, which is what makes its
//! retry safe.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use sectr_common::inspect;
use sectr_common::sidecar::INGEST_TOKEN_PREFIX;

use crate::runner::SubprocessRunnerConfig;
use crate::sidecar::{RunExit, RunnerSpawner, SidecarClient, SidecarConfig};

/// The platform's port when `PORT` is unset: Cloud Run sets `PORT`, and
/// the Lambda Web Adapter assumes 8080.
pub const DEFAULT_PORT: u16 = 8080;

/// Request bodies are small JSON objects: anything bigger is refused (413).
pub const BODY_LIMIT: usize = 4 * 1024;

/// Serve-mode configuration. [`ServeConfig::from_env`] fills it for the
/// binary; tests build it directly.
#[derive(Debug, Clone)]
pub struct ServeConfig {
  pub port: u16,
  /// The only server this sidecar dials (`SECTR_INGEST_URL`).
  pub ingest_url: String,
  /// The agent's invoke URL (`SECTR_RUNNER_BASE`), as in env mode.
  pub runner_base: String,
  /// Where the app lives in the image: the manifest import runs here.
  pub app_dir: PathBuf,
  /// The interpreter the manifest import runs (`python3` in the image).
  pub python: String,
  /// How long `POST /manifest` retries a dial that fails for a reason
  /// other than a rejected token.
  pub dial_retry_window: Duration,
  /// How long the manifest import may run before it's killed.
  pub import_timeout: Duration,
}

impl ServeConfig {
  /// The binary's configuration, from the process environment.
  pub fn from_env() -> anyhow::Result<Self> {
    Self::from_lookup(|key| std::env::var(key).ok())
  }

  /// [`Self::from_env`] over any variable lookup. Fails on a missing or
  /// unusable `SECTR_INGEST_URL`, an unparsable `PORT`, or a `PORT` that
  /// collides with the agent's own port.
  pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
    let ingest_url = lookup("SECTR_INGEST_URL")
      .filter(|url| !url.is_empty())
      .ok_or_else(|| {
        anyhow::anyhow!(
          "SECTR_INGEST_URL is required in serve mode: set it in the image to your Sectr \
           server's ingest URL (for hosted Sectr, wss://ingest.sectr.dev/ingest)"
        )
      })?;
    if !(ingest_url.starts_with("ws://") || ingest_url.starts_with("wss://"))
      || ingest_url.as_str().into_client_request().is_err()
    {
      anyhow::bail!("SECTR_INGEST_URL {ingest_url:?} is not a ws:// or wss:// URL");
    }

    let agent_port = SubprocessRunnerConfig::default().port;
    let port = match lookup("PORT").filter(|p| !p.is_empty()) {
      None => DEFAULT_PORT,
      Some(raw) => raw
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| anyhow::anyhow!("PORT {raw:?} is not a port number"))?,
    };
    if port == agent_port {
      anyhow::bail!(
        "PORT {port} is the agent's own port inside the container; serve the sidecar on another \
         port (the default is {DEFAULT_PORT})"
      );
    }

    let runner_base = lookup("SECTR_RUNNER_BASE")
      .filter(|b| !b.is_empty())
      .unwrap_or_else(|| format!("http://127.0.0.1:{agent_port}"));

    Ok(ServeConfig {
      port,
      ingest_url,
      runner_base,
      app_dir: PathBuf::from("/app"),
      python: "python3".into(),
      dial_retry_window: Duration::from_secs(10),
      import_timeout: Duration::from_secs(60),
    })
  }
}

struct ServeState {
  config: ServeConfig,
  spawner: Arc<dyn RunnerSpawner>,
  /// One permit: one turn (or manifest import) per instance.
  permit: Arc<Semaphore>,
}

/// The serve-mode routes. `spawner` is shared across wakes: one
/// [`crate::runner::SubprocessRunner`] in the binary, which kills any
/// leftover agent process on `start`.
pub fn router(config: ServeConfig, spawner: Arc<dyn RunnerSpawner>) -> Router {
  let state = Arc::new(ServeState {
    config,
    spawner,
    permit: Arc::new(Semaphore::new(1)),
  });
  Router::new()
    .route("/", get(identify))
    .route("/wake", post(wake))
    .route("/manifest", post(manifest))
    .layer(DefaultBodyLimit::max(BODY_LIMIT))
    .with_state(state)
}

/// Serve on `0.0.0.0:<port>` until the process ends.
pub async fn serve(config: ServeConfig, spawner: Arc<dyn RunnerSpawner>) -> anyhow::Result<()> {
  let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.port));
  let listener = tokio::net::TcpListener::bind(addr)
    .await
    .map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
  tracing::info!(%addr, "sidecar serving (serve mode)");
  axum::serve(listener, router(config, spawner)).await?;
  Ok(())
}

async fn identify() -> Json<Value> {
  Json(json!({
    "sectr_sidecar": {"version": env!("CARGO_PKG_VERSION"), "mode": "serve"}
  }))
}

/// A refused request: its status, `error` code and message.
struct Refusal {
  status: StatusCode,
  code: &'static str,
  message: String,
}

impl IntoResponse for Refusal {
  fn into_response(self) -> Response {
    (
      self.status,
      Json(json!({"error": self.code, "message": self.message})),
    )
      .into_response()
  }
}

fn bad_request(message: impl Into<String>) -> Refusal {
  Refusal {
    status: StatusCode::BAD_REQUEST,
    code: "bad_request",
    message: message.into(),
  }
}

fn busy() -> Response {
  Refusal {
    status: StatusCode::CONFLICT,
    code: "busy",
    message: "this instance is already running a turn".into(),
  }
  .into_response()
}

fn outcome(body: Value) -> Response {
  (StatusCode::OK, Json(body)).into_response()
}

/// The JSON object body, or the response refusing it (413 over the limit,
/// 400 for anything that isn't a JSON object).
fn object_body(
  body: Result<Bytes, BytesRejection>,
) -> Result<serde_json::Map<String, Value>, Refusal> {
  let bytes = body.map_err(|rejection| {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
      Refusal {
        status: StatusCode::PAYLOAD_TOO_LARGE,
        code: "too_large",
        message: format!("request bodies are limited to {BODY_LIMIT} bytes"),
      }
    } else {
      bad_request(rejection.body_text())
    }
  })?;
  match serde_json::from_slice::<Value>(&bytes) {
    Ok(Value::Object(object)) => Ok(object),
    Ok(_) => Err(bad_request("the body must be a JSON object")),
    Err(e) => Err(bad_request(format!("the body is not JSON: {e}"))),
  }
}

/// A `sit_…` token field that can travel as a bearer header (a token that
/// can't would make every dial fail and the sidecar retry forever).
fn token_field(object: &serde_json::Map<String, Value>, field: &str) -> Result<String, Refusal> {
  let token = match object.get(field) {
    Some(Value::String(token)) => token,
    Some(_) => return Err(bad_request(format!("`{field}` must be a string"))),
    None => return Err(bad_request(format!("`{field}` is required"))),
  };
  if !token.starts_with(INGEST_TOKEN_PREFIX)
    || HeaderValue::from_str(&format!("Bearer {token}")).is_err()
  {
    return Err(bad_request(format!(
      "`{field}` is not a Sectr ingest token ({INGEST_TOKEN_PREFIX}…)"
    )));
  }
  Ok(token.clone())
}

/// `POST /wake`: run one invocation, holding the request until it ends.
async fn wake(
  State(state): State<Arc<ServeState>>,
  body: Result<Bytes, BytesRejection>,
) -> Response {
  let object = match object_body(body) {
    Ok(object) => object,
    Err(refusal) => return refusal.into_response(),
  };
  // Unknown fields (an `ingest_url` from an older server included) are
  // ignored: the URL is the image's, never the request's.
  let ingest_token = match token_field(&object, "ingest_token") {
    Ok(token) => token,
    Err(refusal) => return refusal.into_response(),
  };
  let Ok(permit) = state.permit.clone().try_acquire_owned() else {
    tracing::info!("wake refused: a turn is already running");
    return busy();
  };

  // The invocation runs in its own task, which owns the permit: if the
  // caller drops the request (the server tore the turn down, or the
  // platform cancelled it), the turn still ends only the way the protocol
  // ends it (a boundary, or a 401 once its token is dead), with the agent
  // stopped. Cancelling it with the request would leave the agent running,
  // and free the permit while it was.
  let started = Instant::now();
  // Today's env-mode backoff values (main.rs).
  let client = SidecarClient::new(
    SidecarConfig {
      ingest_url: state.config.ingest_url.clone(),
      ingest_token,
      runner_base: state.config.runner_base.clone(),
      reconnect_backoff: Duration::from_millis(250),
      max_backoff: Duration::from_secs(10),
    },
    state.spawner.clone(),
  );
  let run = tokio::spawn(async move {
    let _permit = permit;
    let outcome_name = match client.run().await {
      RunExit::Boundary => "turn_ended",
      RunExit::Unauthorized => "token_rejected",
    };
    tracing::info!(
      outcome = outcome_name,
      duration_ms = started.elapsed().as_millis() as u64,
      "wake finished"
    );
    outcome_name
  });
  match run.await {
    Ok(outcome_name) => outcome(json!({"outcome": outcome_name})),
    // Only a panic in the turn's own code gets here.
    Err(e) => {
      tracing::error!(error = %e, "wake task failed");
      (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal"})),
      )
        .into_response()
    }
  }
}

/// `POST /manifest`: dial ingest with the registration token, and only if
/// Sectr accepts it, import the app and return its manifest.
async fn manifest(
  State(state): State<Arc<ServeState>>,
  body: Result<Bytes, BytesRejection>,
) -> Response {
  let object = match object_body(body) {
    Ok(object) => object,
    Err(refusal) => return refusal.into_response(),
  };
  let token = match token_field(&object, "registration_token") {
    Ok(token) => token,
    Err(refusal) => return refusal.into_response(),
  };
  let entry = match object.get("entry") {
    Some(Value::String(entry)) if !entry.is_empty() && !entry.contains('\0') => entry.clone(),
    Some(Value::String(_)) => {
      return bad_request("`entry` must be a module:attribute string").into_response();
    }
    Some(_) => return bad_request("`entry` must be a string").into_response(),
    None => return bad_request("`entry` is required").into_response(),
  };
  let Ok(_permit) = state.permit.clone().try_acquire_owned() else {
    tracing::info!("manifest request refused: a turn is already running");
    return busy();
  };

  let started = Instant::now();
  let response = match dial_check(&state.config, &token).await {
    DialCheck::Accepted => import_manifest(&state.config, &entry).await,
    DialCheck::Rejected => json!({"outcome": "token_rejected"}),
    DialCheck::Failed(message) => json!({"outcome": "dial_failed", "message": message}),
  };
  tracing::info!(
    outcome = response["outcome"].as_str().unwrap_or_default(),
    duration_ms = started.elapsed().as_millis() as u64,
    "manifest request finished"
  );
  outcome(response)
}

enum DialCheck {
  Accepted,
  Rejected,
  Failed(String),
}

/// The same WebSocket upgrade a turn makes, closed as soon as it's
/// accepted: no frames are sent. A 401 is final; anything else is retried
/// for the configured window.
async fn dial_check(config: &ServeConfig, token: &str) -> DialCheck {
  use tokio_tungstenite::tungstenite::{Error, http::StatusCode as WsStatus};

  let deadline = Instant::now() + config.dial_retry_window;
  let mut backoff = Duration::from_millis(250);
  loop {
    let mut request = match config.ingest_url.as_str().into_client_request() {
      Ok(request) => request,
      Err(e) => return DialCheck::Failed(format!("SECTR_INGEST_URL is unusable: {e}")),
    };
    request.headers_mut().insert(
      "Authorization",
      HeaderValue::from_str(&format!("Bearer {token}")).expect("checked by token_field"),
    );
    let remaining = deadline.saturating_duration_since(Instant::now());
    let failure = match tokio::time::timeout(
      remaining.max(Duration::from_millis(100)),
      tokio_tungstenite::connect_async(request),
    )
    .await
    {
      Ok(Ok((mut ws, _))) => {
        let _ = ws.close(None).await;
        return DialCheck::Accepted;
      }
      Ok(Err(Error::Http(response))) if response.status() == WsStatus::UNAUTHORIZED => {
        return DialCheck::Rejected;
      }
      Ok(Err(e)) => format!("dial {}: {e}", config.ingest_url),
      Err(_) => format!("dial {}: timed out", config.ingest_url),
    };
    if Instant::now() + backoff >= deadline {
      tracing::warn!(error = %failure, "manifest dial check failed");
      return DialCheck::Failed(failure);
    }
    tokio::time::sleep(backoff).await;
    backoff = (backoff * 2).min(Duration::from_secs(2));
  }
}

/// Import the app the way managed builds do and shape the result as a
/// `/manifest` outcome.
async fn import_manifest(config: &ServeConfig, entry: &str) -> Value {
  let uses_uv = inspect::uses_uv(&config.app_dir);
  let command = inspect::import_command(uses_uv, &config.python);
  let result = inspect::run_import(
    &command,
    &config.app_dir,
    entry,
    Some(config.import_timeout),
    tokio::io::stderr(),
  )
  .await;
  match result {
    Ok(manifest) => json!({"outcome": "manifest", "manifest": manifest, "uses_uv": uses_uv}),
    Err(failure) => {
      tracing::warn!(
        exit_code = failure.exit_code,
        message = failure.message.as_deref().unwrap_or_default(),
        "manifest import failed"
      );
      let mut body = json!({
        "outcome": "import_failed",
        "exit_code": failure.exit_code,
        "stderr_tail": failure.stderr_tail,
      });
      if let Some(message) = failure.message {
        body["message"] = Value::String(message);
      }
      body
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashMap;

  fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let vars: HashMap<String, String> = vars
      .iter()
      .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
      .collect();
    move |key| vars.get(key).cloned()
  }

  #[test]
  fn config_needs_an_ingest_url() {
    let err = ServeConfig::from_lookup(lookup(&[]))
      .unwrap_err()
      .to_string();
    assert!(err.contains("SECTR_INGEST_URL is required"), "{err}");
    let err = ServeConfig::from_lookup(lookup(&[("SECTR_INGEST_URL", "https://x/ingest")]))
      .unwrap_err()
      .to_string();
    assert!(err.contains("not a ws:// or wss:// URL"), "{err}");
  }

  #[test]
  fn config_refuses_the_agents_port() {
    let err = ServeConfig::from_lookup(lookup(&[
      ("SECTR_INGEST_URL", "wss://ingest.sectr.dev/ingest"),
      ("PORT", "8081"),
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("the agent's own port"), "{err}");
    let err = ServeConfig::from_lookup(lookup(&[
      ("SECTR_INGEST_URL", "wss://ingest.sectr.dev/ingest"),
      ("PORT", "eighty"),
    ]))
    .unwrap_err()
    .to_string();
    assert!(err.contains("not a port number"), "{err}");
  }

  #[test]
  fn config_defaults() {
    let config = ServeConfig::from_lookup(lookup(&[(
      "SECTR_INGEST_URL",
      "wss://ingest.sectr.dev/ingest",
    )]))
    .unwrap();
    assert_eq!(config.port, 8080);
    assert_eq!(config.runner_base, "http://127.0.0.1:8081");
    assert_eq!(config.app_dir, PathBuf::from("/app"));
    let config = ServeConfig::from_lookup(lookup(&[
      ("SECTR_INGEST_URL", "ws://127.0.0.1:9/ingest"),
      ("PORT", "9000"),
      ("SECTR_RUNNER_BASE", "http://127.0.0.1:7000"),
    ]))
    .unwrap();
    assert_eq!(config.port, 9000);
    assert_eq!(config.runner_base, "http://127.0.0.1:7000");
  }
}
