//! [`E2bPlatform`] — the E2B runner backend: the build half is CLOUD BUILD (reused from
//! [`GcpPlatform`](super::gcp_build::GcpPlatform) verbatim — the gcs↔gcp
//! pairing is structural), the execution half is E2B sandboxes
//! (Firecracker microVMs) driven by E2B's platform REST API. E2B has no
//! Rust SDK, so this module carries a small `reqwest` client.
//!
//! # Semantics
//!
//! - **The sidecar is NOT the template's startCmd process.** E2B's start
//!   command runs ONCE at template-build time and is snapshot-captured —
//!   it never re-runs on sandbox creation, and `Sandbox.create()` env vars
//!   are NOT available to it (docs.e2b.dev/template/start-ready-command).
//!   A snapshot-restored sidecar would carry build-time env — no per-turn
//!   `SECTR_INGEST_TOKEN` — and die at its env validation. Templates are
//!   therefore built as pure `fromImage` (no startCmd/readyCmd), and
//!   `spawn()` starts the sidecar post-create via envd's
//!   `process.Process/Start` Connect RPC with the per-turn env in
//!   `ProcessConfig.envs`. Process persistence across client disconnect is
//!   E2B-documented (commands/background.md: a background command "keeps
//!   running inside the sandbox even after the SDK disconnects"); the
//!   server drains the Start stream in a background task (safe whether or
//!   not dropping would kill the process).
//! - **`self_terminating() = false`**: a sandbox does NOT end itself when
//!   the sidecar exits — it bills until its timeout. The hub's ONE
//!   boundary-policy point consults this and calls the
//!   existing `teardown_session` at every boundary (after the ack — kill
//!   races nothing: the sidecar has already finished journaling and exits
//!   itself; an E2B kill has no CANCELLED status semantics).
//! - **ensure-template at spawn is an existence CHECK, not self-healing**:
//!   a template build is minutes (a Cloud Run job create is milliseconds),
//!   so a missing template fails with an explicit "redeploy" error — an
//!   accepted asymmetry vs GCP's ensure-job.
//!
//! # Template lifecycle (build = deployment version)
//!
//! `build()` = `gcp.cloud_build_impl()` (Cloud Build → AR → manifest, all
//! unchanged) **+ the E2B template build**: create-or-reuse the template
//! named `sectr-app-<deployment-id>` (name pattern `^[a-z0-9-_]+$` ≤128 —
//! project-local, one template per deployment, mirrors the Cloud Run
//! one-job-per-deployment decision), then start a build with
//! `fromImage` = the just-landed AR ref and `fromImageRegistry` = the
//! dedicated `sectr-e2b-puller` SA JSON (artifactregistry.reader on the
//! runners repo ONLY — a third-party-held credential, scoped + rotatable).
//! A failed template build fails the deploy before routing — the same
//! "step 4" discipline as `ensure_job`.
//!
//! # Auth
//!
//! Platform API (`https://api.<domain>`): `X-API-Key` = the account key
//! (`SECTR_E2B_API_KEY` — a server secret). envd (`https://sandbox.<domain>`):
//! `X-Access-Token` = the per-sandbox `envdAccessToken` returned at create
//! (`secure: true` — without it anyone with the sandbox ID could drive the
//! sandbox's envd). The access token lives only in memory for the duration
//! of the spawn call; it is never journaled.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::dispatch::DispatchError;
use crate::platform::gcp_build::{GcpPlatform, TEMPLATE_NAME_PREFIX};
use crate::platform::{
  BuildError, BuildOutput, BuildRequest, RunnerInfo, RunnerPlatform, server_host_of,
};

/// envd's in-sandbox listen port (default per E2B's API reference — the
/// `E2b-Sandbox-Port` header routes through the shared sandbox host).
const SANDBOX_ENVD_PORT: u32 = 49983;

/// E2B template builds are minutes (image pull + snapshot); cap the poll
/// generously (Cloud Build itself is capped at 30 min upstream of this).
const TEMPLATE_BUILD_POLL_TIMEOUT: Duration = Duration::from_secs(2400);
const TEMPLATE_BUILD_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Sandbox creation involves a placement round; the docs' own SDK uses a
/// similar order of magnitude. 503/504 (`sandbox_capacity_unavailable`,
/// `sandbox_placement_timeout`) are known transient semantic error codes.
/// Cap for UNARY platform calls (create/kill/status/poll).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const KILL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the envd Start stream's FIRST event (the
/// StartEvent carrying the pid) before declaring the sidecar launch
/// failed. The sidecar binary is in the image; startup is fast.
const PROCESS_START_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Deterministic naming
// ---------------------------------------------------------------------------

/// The E2B template name for a deployment (template = deployment,
/// mirrors `gcp_build::job_id`). Matches E2B's name rules
/// (`^[a-z0-9-_]+$`, ≤128 — a UUID is 36).
pub(crate) fn template_id(deployment_id: Uuid) -> String {
  format!("{TEMPLATE_NAME_PREFIX}{deployment_id}")
}

/// Metadata key naming the Sectr server environment that spawned the
/// sandbox: the reaper lists ONLY its own server's sandboxes, so
/// a local e2b-mode server sharing the E2B team is never reaped by prod.
const METADATA_SERVER: &str = "sectr.server";

/// The sandbox create request's metadata (observability + the orphan
/// reaper's filter keys). `server_host` = the host of
/// `SECTR_RUNNER_SERVER_URL` (prod: `ingest.sectr.dev`).
fn sandbox_metadata(
  session: Uuid,
  deployment_id: Uuid,
  invocation_id: Uuid,
  server_host: &str,
) -> BTreeMap<String, String> {
  BTreeMap::from([
    ("sectr.session".to_owned(), session.to_string()),
    ("sectr.deployment".to_owned(), deployment_id.to_string()),
    ("sectr.invocation".to_owned(), invocation_id.to_string()),
    (METADATA_SERVER.to_owned(), server_host.to_owned()),
  ])
}

/// The `metadata` query value for `GET /v2/sandboxes`: E2B parses it as an
/// inner query string (`k=v&k2=v2`, each key and value URL-encoded); the
/// HTTP client then encodes the whole parameter once more.
fn metadata_filter(key: &str, value: &str) -> String {
  reqwest::Url::parse_with_params("http://unused/", [(key, value)])
    .ok()
    .and_then(|u| u.query().map(str::to_owned))
    .unwrap_or_default()
}

/// Pages followed per reaper listing (100 sandboxes each); a truncated
/// listing is logged and the rest is picked up on a later tick.
const LIST_MAX_PAGES: usize = 20;

/// The user-visible failure when E2B refuses a create with 429 (account
/// concurrency limit): friendly, and no raw provider body.
fn create_sandbox_error(template: &str, e: &E2bError) -> DispatchError {
  if e.status == 429 {
    return DispatchError::Spawn(
      "Sectr is at capacity right now; retry in a moment (runner backend 429)".into(),
    );
  }
  DispatchError::Spawn(format!("create sandbox from {template}: {e}"))
}

/// Whether a listed entry is tagged with OUR `sectr.server` host. The
/// server-side `metadata` filter should already guarantee it; checking again
/// here means a filter E2B ignored or misread can never hand the reaper
/// another environment's sandboxes.
fn listed_on_server(entry: &Value, server_host: &str) -> bool {
  entry
    .get("metadata")
    .and_then(|m| m.get(METADATA_SERVER))
    .and_then(Value::as_str)
    == Some(server_host)
}

/// One `GET /v2/sandboxes` entry → a [`RunnerInfo`]. Entries we can't
/// interpret (no/invalid session or start time) are skipped: the reaper
/// must never act on uncertainty.
fn runner_from_listed(entry: &Value) -> Option<RunnerInfo> {
  let handle = entry.get("sandboxID")?.as_str()?.to_owned();
  let started_at = entry
    .get("startedAt")?
    .as_str()?
    .parse::<chrono::DateTime<chrono::Utc>>()
    .ok()?;
  let metadata = entry.get("metadata")?;
  let session = metadata.get("sectr.session")?.as_str()?.parse().ok()?;
  let invocation = metadata
    .get("sectr.invocation")
    .and_then(Value::as_str)
    .and_then(|s| s.parse().ok());
  Some(RunnerInfo {
    handle,
    session,
    invocation,
    started_at,
  })
}

/// Split the sidecar command into envd's cmd/args (the first token is the
/// binary, the rest are arguments — the same whitespace-split the config
/// performs). `None` on an empty command (a misconfiguration the config
/// default never produces).
fn split_sidecar_cmd(cmd: &[String]) -> Option<(String, Vec<String>)> {
  let (first, rest) = cmd.split_first()?;
  Some((first.clone(), rest.to_vec()))
}

// ---------------------------------------------------------------------------
// Request/response shapes (pinned against docs.e2b.dev — pure, unit-tested)
// ---------------------------------------------------------------------------

/// `POST /v3/templates` — create-or-reuse the template (same-name call is
/// the live-verify item; see [`E2bPlatform::ensure_template`]).
fn create_template_body(name: &str, cpu_count: i32, memory_mb: i32) -> Value {
  serde_json::json!({
      "name": name,
      "cpuCount": cpu_count,
      "memoryMB": memory_mb,
  })
}

/// `POST /v2/templates/{tid}/builds/{bid}` — start the build from the AR
/// image. NO `steps`, NO `startCmd`/`readyCmd` (deviation 1: the snapshot
/// must not capture a sidecar with build-time env).
fn start_build_body(from_image: &str, service_account_json: &str) -> Value {
  serde_json::json!({
      "fromImage": from_image,
      "fromImageRegistry": {
          "type": "gcp",
          "serviceAccountJson": service_account_json,
      },
  })
}

/// `POST /sandboxes` — one sandbox per turn. `timeout` is SECONDS (the API
/// default is 15!). `secure: true` so the sandbox's envd requires the
/// per-sandbox access token. Env is NOT passed here (deviation 1: global
/// envs reach envd-run commands only — the sidecar's env rides the Start
/// call).
fn create_sandbox_body(
  template_name: &str,
  timeout_secs: i64,
  metadata: &BTreeMap<String, String>,
) -> Value {
  serde_json::json!({
      "templateID": template_name,
      "timeout": timeout_secs,
      "secure": true,
      "metadata": metadata,
  })
}

/// The envd `process.Process/Start` request (Connect JSON). `stdin: false`
/// — nothing writes to the sidecar's stdin.
fn start_process_body(
  cmd: &str,
  args: &[String],
  envs: &BTreeMap<String, String>,
  cwd: &str,
) -> Value {
  serde_json::json!({
      "process": {
          "cmd": cmd,
          "args": args,
          "envs": envs,
          "cwd": cwd,
      },
      "stdin": false,
  })
}

/// Connect stream-RPC end-of-stream flag (5th... rather, FIRST header
/// byte, bit 1): the frame carries terminal metadata/error JSON instead of
/// a message (live finding 2026-09-17 #2: envd reports in-band failures —
/// e.g. an envd-level auth rejection — as a 200 + EOS error frame, NOT a
/// non-200 status; ignoring the flag made those look like an empty stream).
const CONNECT_FLAG_END_STREAM: u8 = 0b0000_0010;

/// Parse ONE Connect streaming envelope: a 5-byte header (1 flag byte +
/// 4-byte big-endian payload length) followed by the JSON payload.
/// Returns the flag byte + payload bytes, or `None` when the buffer
/// doesn't hold a complete message.
fn parse_connect_envelope(buf: &[u8]) -> Option<(u8, &[u8])> {
  if buf.len() < 5 {
    return None;
  }
  let flags = buf[0];
  let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
  if buf.len() < 5 + len {
    return None;
  }
  Some((flags, &buf[5..5 + len]))
}

/// Decode a Connect end-of-stream frame's payload into an error string
/// (the `error` object carries code/message; an empty payload or `{}` is a
/// clean end of stream).
fn connect_eos_error(payload: &[u8]) -> String {
  if payload.iter().all(|b| b.is_ascii_whitespace()) {
    return "envd end-of-stream frame (clean EOF)".into();
  }
  match serde_json::from_slice::<Value>(payload) {
    Ok(v) => {
      let code = v
        .pointer("/error/code")
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".into());
      match v.pointer("/error/message").and_then(Value::as_str) {
        Some(message) => format!("envd Connect error {code}: {message}"),
        None => format!("envd end-of-stream frame without an error (clean EOF): {v}"),
      }
    }
    Err(_) => "envd end-of-stream frame (unparseable)".into(),
  }
}

/// Frame a Connect streaming REQUEST (the stream-RPC request body is
/// enveloped exactly like the response — E2B-GATE finding 1: a raw JSON
/// body would be rejected/mis-parsed by envd): one envelope carrying the
/// request message (flags 0) followed by the end-of-stream frame (flags
/// 0b10). Live finding 2026-09-17 #3: the EOS payload must be `{}` (an
/// empty EndStreamResponse object) — a ZERO-length payload makes envd
/// reject the whole request with `unmarshal end stream message:
/// unexpected end of JSON input`.
fn connect_request_frame(payload: &[u8]) -> Vec<u8> {
  let mut out = Vec::with_capacity(payload.len() + 12);
  out.push(0u8);
  out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
  out.extend_from_slice(payload);
  out.push(CONNECT_FLAG_END_STREAM);
  out.extend_from_slice(&2u32.to_be_bytes());
  out.extend_from_slice(b"{}");
  out
}

/// The build-status view (subset of `GET /templates/{tid}/builds/{bid}/status`
/// we consume). `status` is E2B's enum: `building|waiting|ready|error|uploaded`.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct BuildStatusView {
  pub status: String,
  #[serde(default)]
  pub logs: Vec<String>,
  #[serde(default)]
  pub reason: Option<BuildStatusReason>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct BuildStatusReason {
  #[serde(default)]
  pub message: String,
  #[serde(default)]
  pub step: Option<String>,
}

/// Compose the BuildError from a failed template build: the reason message
/// (+ failing step, when named) as the message, the last ~20 log lines as
/// the tail. Pure — unit-tested.
fn build_error_from_status(status: &BuildStatusView) -> BuildError {
  let reason = status
    .reason
    .as_ref()
    .map(|r| {
      let step = r
        .step
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" (step '{s}')"))
        .unwrap_or_default();
      format!("{}{step}", r.message)
    })
    .unwrap_or_else(|| format!("template build finished with status {}", status.status));
  let tail: Vec<&str> = status.logs.iter().map(String::as_str).collect();
  let tail_start = tail.len().saturating_sub(20);
  BuildError {
    message: reason,
    logs_tail: tail[tail_start..].join("\n"),
  }
}

/// Is the build-status terminal? `ready` = success; `error` = failure.
/// `uploaded` is the manual-upload path's state — not ours, treated as
/// non-terminal (the poll times out loudly if it never resolves).
fn status_is_terminal(status: &str) -> Option<bool> {
  match status {
    "ready" => Some(true),
    "error" => Some(false),
    _ => None,
  }
}

// ---------------------------------------------------------------------------
// The REST client
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[error("E2B API {status}: {message}")]
pub(crate) struct E2bError {
  pub status: u16,
  pub message: String,
}

impl E2bError {
  // Not-found discrimination happens per-call (template_exists returns
  // bool; kill_sandbox maps 404 to "already gone") — no shared helper.
}

type E2bResult<T> = Result<T, E2bError>;

/// A created sandbox's identity: the opaque handle (the sandbox id) plus
/// the in-memory-only envd access token (`secure: true` sandboxes require
/// it for the Start call). The manual Debug MASKS the token — it must
/// never reach logs (E2B-GATE finding 3).
#[derive(Clone)]
pub(crate) struct SandboxCreated {
  pub sandbox_id: String,
  pub envd_access_token: Option<String>,
}

impl std::fmt::Debug for SandboxCreated {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SandboxCreated")
      .field("sandbox_id", &self.sandbox_id)
      .field(
        "envd_access_token",
        &self.envd_access_token.as_ref().map(|_| "<masked>"),
      )
      .finish()
  }
}

/// The E2B REST client: platform API + envd, X-API-Key
/// auth, hand-written serde types pinned by unit tests against the
/// documented schemas. No SDK — deliberately thin.
pub(crate) struct E2bClient {
  http: reqwest::Client,
  api_base: String,
  sandbox_base: String,
  api_key: String,
  puller_sa_json: String,
  cpu_count: i32,
  memory_mb: i32,
  sandbox_timeout_secs: i64,
}

impl E2bClient {
  pub(crate) fn from_config(config: &ServerConfig, puller_sa_json: String) -> anyhow::Result<Self> {
    let api_key = config
      .e2b_api_key
      .clone()
      .ok_or_else(|| anyhow::anyhow!("SECTR_E2B_API_KEY is required when SECTR_PLATFORM=e2b"))?;
    let http = reqwest::Client::builder()
      // rustls (the workspace reqwest spec) needs no native roots; the
      // default timeout is per-request — each call wraps its own.
      .build()
      .map_err(|e| anyhow::anyhow!("build E2B HTTP client: {e}"))?;
    Ok(E2bClient {
      api_base: format!("https://api.{}", config.e2b_domain),
      sandbox_base: format!("https://sandbox.{}", config.e2b_domain),
      http,
      api_key,
      puller_sa_json,
      cpu_count: config.e2b_cpu_count,
      memory_mb: config.e2b_memory_mb,
      sandbox_timeout_secs: config.e2b_sandbox_timeout_secs as i64,
    })
  }

  /// Create-or-reuse the template (POST /v3/templates → 202
  /// `{templateID, buildID}`).
  pub(crate) async fn create_template(&self, name: &str) -> E2bResult<(String, String)> {
    let resp = self
      .http
      .post(format!("{}/v3/templates", self.api_base))
      .header("X-API-Key", &self.api_key)
      .json(&create_template_body(name, self.cpu_count, self.memory_mb))
      .timeout(REQUEST_TIMEOUT)
      .send()
      .await
      .map_err(|e| E2bError {
        status: 0,
        message: format!("POST /v3/templates: {e}"),
      })?;
    let status = resp.status().as_u16();
    if status != 202 {
      return Err(E2bError {
        status,
        message: truncate_body(resp).await,
      });
    }
    #[derive(serde::Deserialize)]
    struct Created {
      #[serde(rename = "templateID")]
      template_id: String,
      #[serde(rename = "buildID")]
      build_id: String,
    }
    let created: Created = resp.json().await.map_err(|e| E2bError {
      status,
      message: format!("template create response: {e}"),
    })?;
    Ok((created.template_id, created.build_id))
  }

  /// Start the build (POST /v2/templates/{tid}/builds/{bid} → 202).
  pub(crate) async fn start_template_build(
    &self,
    template_id: &str,
    build_id: &str,
    from_image: &str,
  ) -> E2bResult<()> {
    let resp = self
      .http
      .post(format!(
        "{}/v2/templates/{}/builds/{}",
        self.api_base, template_id, build_id
      ))
      .header("X-API-Key", &self.api_key)
      .json(&start_build_body(from_image, &self.puller_sa_json))
      .timeout(REQUEST_TIMEOUT)
      .send()
      .await
      .map_err(|e| E2bError {
        status: 0,
        message: format!("POST template build: {e}"),
      })?;
    let status = resp.status().as_u16();
    if status != 202 {
      return Err(E2bError {
        status,
        message: truncate_body(resp).await,
      });
    }
    Ok(())
  }

  /// Poll the build status (GET …/status).
  pub(crate) async fn build_status(
    &self,
    template_id: &str,
    build_id: &str,
  ) -> E2bResult<BuildStatusView> {
    let resp = self
      .http
      .get(format!(
        "{}/templates/{}/builds/{}/status",
        self.api_base, template_id, build_id
      ))
      .header("X-API-Key", &self.api_key)
      .timeout(REQUEST_TIMEOUT)
      .send()
      .await
      .map_err(|e| E2bError {
        status: 0,
        message: format!("GET build status: {e}"),
      })?;
    let status = resp.status().as_u16();
    if status != 200 {
      return Err(E2bError {
        status,
        message: truncate_body(resp).await,
      });
    }
    resp.json().await.map_err(|e| E2bError {
      status,
      message: format!("build status response: {e}"),
    })
  }

  /// The ensure-template existence check (GET /templates/aliases/{name}):
  /// `Some(templateID)` = exists, `None` = absent (spawn refuses with
  /// "redeploy"). The templateID rides along so an ensure_template
  /// fallback can NAME the existing template in its error (E2B-GATE
  /// finding 5).
  pub(crate) async fn template_exists(&self, name: &str) -> E2bResult<Option<String>> {
    let resp = self
      .http
      .get(format!("{}/templates/aliases/{}", self.api_base, name))
      .header("X-API-Key", &self.api_key)
      .timeout(REQUEST_TIMEOUT)
      .send()
      .await
      .map_err(|e| E2bError {
        status: 0,
        message: format!("GET template alias: {e}"),
      })?;
    match resp.status().as_u16() {
      200 => {
        #[derive(serde::Deserialize)]
        struct Alias {
          #[serde(rename = "templateID")]
          template_id: String,
        }
        let alias: Alias = resp.json().await.map_err(|e| E2bError {
          status: 200,
          message: format!("alias response: {e}"),
        })?;
        Ok(Some(alias.template_id))
      }
      404 => Ok(None),
      status => Err(E2bError {
        status,
        message: truncate_body(resp).await,
      }),
    }
  }

  /// Create the turn's sandbox (POST /sandboxes → 201). `metadata` keys
  /// are our `sectr.*` observability surface.
  pub(crate) async fn create_sandbox(
    &self,
    template_name: &str,
    metadata: &BTreeMap<String, String>,
  ) -> E2bResult<SandboxCreated> {
    let resp = self
      .http
      .post(format!("{}/sandboxes", self.api_base))
      .header("X-API-Key", &self.api_key)
      .json(&create_sandbox_body(
        template_name,
        self.sandbox_timeout_secs,
        metadata,
      ))
      .timeout(REQUEST_TIMEOUT)
      .send()
      .await
      .map_err(|e| E2bError {
        status: 0,
        message: format!("POST /sandboxes: {e}"),
      })?;
    let status = resp.status().as_u16();
    if status != 201 {
      return Err(E2bError {
        status,
        message: truncate_body(resp).await,
      });
    }
    #[derive(serde::Deserialize)]
    struct Created {
      #[serde(rename = "sandboxID")]
      sandbox_id: String,
      #[serde(rename = "envdAccessToken")]
      envd_access_token: Option<String>,
    }
    let created: Created = resp.json().await.map_err(|e| E2bError {
      status,
      message: format!("sandbox create response: {e}"),
    })?;
    Ok(SandboxCreated {
      sandbox_id: created.sandbox_id,
      envd_access_token: created.envd_access_token,
    })
  }

  /// Running sandboxes spawned by `server_host` (`GET /v2/sandboxes`,
  /// filtered server-side on `sectr.server`), following `X-Next-Token`
  /// pagination up to [`LIST_MAX_PAGES`] pages. The filter is mandatory:
  /// an unfiltered list would cover every sandbox on the E2B team. Every
  /// returned entry is also checked client-side ([`listed_on_server`]), so
  /// correctness never rests on E2B honouring the filter.
  pub(crate) async fn list_sandboxes(&self, server_host: &str) -> E2bResult<Vec<RunnerInfo>> {
    let filter = metadata_filter(METADATA_SERVER, server_host);
    let mut runners = Vec::new();
    let mut next_token: Option<String> = None;
    for page in 0..LIST_MAX_PAGES {
      let mut query = vec![
        ("state", "running".to_owned()),
        ("metadata", filter.clone()),
        ("limit", "100".to_owned()),
      ];
      if let Some(token) = &next_token {
        query.push(("nextToken", token.clone()));
      }
      let resp = self
        .http
        .get(format!("{}/v2/sandboxes", self.api_base))
        .header("X-API-Key", &self.api_key)
        .query(&query)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| E2bError {
          status: 0,
          message: format!("GET /v2/sandboxes: {e}"),
        })?;
      let status = resp.status().as_u16();
      if status != 200 {
        return Err(E2bError {
          status,
          message: truncate_body(resp).await,
        });
      }
      next_token = resp
        .headers()
        .get("X-Next-Token")
        .and_then(|v| v.to_str().ok())
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
      let entries: Vec<Value> = resp.json().await.map_err(|e| E2bError {
        status,
        message: format!("sandbox list response: {e}"),
      })?;
      for entry in &entries {
        if !listed_on_server(entry, server_host) {
          // Should be impossible with the filter applied — loud, because
          // it means E2B ignored or misread the `metadata` filter.
          tracing::warn!(
            entry = %entry,
            server = server_host,
            "sandbox listing returned an entry not tagged with this server; skipping it"
          );
          continue;
        }
        match runner_from_listed(entry) {
          Some(runner) => runners.push(runner),
          None => {
            tracing::debug!(entry = %entry, "skipping an uninterpretable sandbox listing entry")
          }
        }
      }
      if next_token.is_none() {
        return Ok(runners);
      }
      if page + 1 == LIST_MAX_PAGES {
        tracing::warn!(
          pages = LIST_MAX_PAGES,
          "sandbox listing truncated; the remainder is picked up on a later reaper tick"
        );
      }
    }
    Ok(runners)
  }

  /// Kill the sandbox (DELETE → 204). Returns whether it was running
  /// (404 → false — already gone; kill is idempotent by contract).
  pub(crate) async fn kill_sandbox(&self, sandbox_id: &str) -> E2bResult<bool> {
    let resp = self
      .http
      .delete(format!("{}/sandboxes/{}", self.api_base, sandbox_id))
      .header("X-API-Key", &self.api_key)
      .timeout(KILL_TIMEOUT)
      .send()
      .await
      .map_err(|e| E2bError {
        status: 0,
        message: format!("DELETE sandbox: {e}"),
      })?;
    match resp.status().as_u16() {
      204 => Ok(true),
      404 => Ok(false),
      status => Err(E2bError {
        status,
        message: truncate_body(resp).await,
      }),
    }
  }

  /// Start the sidecar in the sandbox (envd `process.Process/Start`,
  /// Connect JSON over the shared sandbox host). Reads the stream until
  /// the StartEvent (pid) or an EndEvent (launch failure) with a startup
  /// deadline — the first message is not guaranteed to be the start —
  /// then spawns a task that logs the sidecar's stdout/stderr (data
  /// events) at info for the process's lifetime. envd supervises the
  /// process independently; holding the stream is the conservative
  /// choice (one held HTTP/2 stream per live turn). Returns the pid.
  pub(crate) async fn start_process(
    &self,
    sandbox_id: &str,
    envd_access_token: Option<&str>,
    cmd: &str,
    args: &[String],
    envs: &BTreeMap<String, String>,
    cwd: &str,
  ) -> E2bResult<i64> {
    // The request body is ENVELOPED (Connect stream-RPC framing —
    // E2B-GATE finding 1): the request message + end-of-stream marker.
    let mut req = self
      .http
      .post(format!("{}/process.Process/Start", self.sandbox_base))
      .header("E2b-Sandbox-Id", sandbox_id)
      .header("E2b-Sandbox-Port", SANDBOX_ENVD_PORT)
      .header("Connect-Protocol-Version", "1")
      .header("Content-Type", "application/connect+json")
      .body(connect_request_frame(
        start_process_body(cmd, args, envs, cwd)
          .to_string()
          .as_bytes(),
      ));
    if let Some(token) = envd_access_token {
      req = req.header("X-Access-Token", token);
    }
    // NO request-wide .timeout(): the response is a stream that lives
    // as long as the sidecar process. The SEND is bounded separately
    // (a hung TLS/TCP connect must not block spawn — E2B-GATE finding
    // 4); the startup deadline below bounds the FIRST event; the drain
    // task reads indefinitely.
    let mut resp = tokio::time::timeout(PROCESS_START_TIMEOUT, req.send())
      .await
      .map_err(|_| E2bError {
        status: 0,
        message: format!(
          "envd Start send did not complete within {}s",
          PROCESS_START_TIMEOUT.as_secs()
        ),
      })?
      .map_err(|e| E2bError {
        status: 0,
        message: format!("envd Start: {e}"),
      })?;
    let status = resp.status().as_u16();
    if status != 200 {
      return Err(E2bError {
        status,
        message: truncate_body(resp).await,
      });
    }

    // Read envelopes until the actual StartEvent: the first message is
    // NOT guaranteed to be the start (a keepalive or the sidecar's own
    // early output can precede it — live finding 2026-09-17: the first
    // invocation logged pid 0 because exactly this happened and the
    // single-message read mistook it for success). Any pre-start data
    // event is the sidecar's stdout/stderr — logged at info, since a
    // boot error (env validation, import failure) is exactly what we
    // need to see. An EndEvent before the start is a launch failure.
    let outcome = tokio::time::timeout(
      PROCESS_START_TIMEOUT,
      read_start_event(sandbox_id, &mut resp),
    )
    .await
    .map_err(|_| E2bError {
      status: 200,
      message: format!(
        "envd Start stream carried no start event within {}s",
        PROCESS_START_TIMEOUT.as_secs()
      ),
    })?
    .map_err(|e| E2bError {
      status: 200,
      message: e,
    })?;
    let pid = match outcome {
      StartOutcome::Started(pid) => pid,
      StartOutcome::Ended(status) => {
        // The pre-start stderr (env validation, import failure, …)
        // was already logged by read_start_event — the dispatch
        // error carries the exit status.
        return Err(E2bError {
          status: 200,
          message: format!("sidecar failed to start: {status}"),
        });
      }
    };

    // Drain the remaining stream for the process's lifetime, logging
    // the sidecar's stdout/stderr at info — this is the ONLY place the
    // sidecar's output is visible (the runner has no log driver here;
    // journalctl on the control plane is the diagnostic surface).
    let sandbox = sandbox_id.to_owned();
    tokio::spawn(async move {
      let mut buf: Vec<u8> = Vec::new();
      loop {
        match resp.chunk().await {
          Ok(Some(chunk)) => {
            buf.extend_from_slice(&chunk);
            while let Some((flags, payload)) = take_envelope(&mut buf) {
              if flags & CONNECT_FLAG_END_STREAM != 0 {
                tracing::info!(sandbox = %sandbox, status = %connect_eos_error(&payload), "envd stream terminated");
                return;
              }
              log_stream_event(&sandbox, &payload);
            }
          }
          Ok(None) => {
            tracing::debug!(sandbox = %sandbox, "envd Start stream ended (sidecar exited)");
            break;
          }
          Err(e) => {
            tracing::debug!(sandbox = %sandbox, error = %e, "envd Start stream error");
            break;
          }
        }
      }
    });
    tracing::info!(sandbox = %sandbox_id, pid, "sidecar process started");
    Ok(pid)
  }
}

/// Consume ONE complete Connect frame from the buffer (the first message
/// may span chunks; multiple messages may arrive in one chunk). `None` =
/// no complete frame yet.
fn take_envelope(buf: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
  let (flags, payload) = parse_connect_envelope(buf)?;
  let payload = payload.to_vec();
  buf.drain(..5 + payload.len());
  Some((flags, payload))
}

/// The outcome of waiting for the envd Start stream's StartEvent.
enum StartOutcome {
  Started(i64),
  /// The process ended before (or instead of) starting — the payload is
  /// envd's status/error string (e.g. `exit status 1`).
  Ended(String),
}

/// Read frames until the StartEvent (pid) or an EndEvent, logging every
/// other event (keepalives AND the sidecar's pre-start stdout/stderr —
/// the boot-error surface) at info. An in-band Connect error frame, a
/// clean EOS before the start, or a raw stream end are all errors — the
/// first two carry envd's actual complaint in the message.
async fn read_start_event(
  sandbox_id: &str,
  resp: &mut reqwest::Response,
) -> Result<StartOutcome, String> {
  let mut buf: Vec<u8> = Vec::new();
  let mut bytes_seen = 0usize;
  loop {
    match resp.chunk().await {
      Ok(Some(chunk)) => {
        bytes_seen += chunk.len();
        buf.extend_from_slice(&chunk);
        while let Some((flags, payload)) = take_envelope(&mut buf) {
          if flags & CONNECT_FLAG_END_STREAM != 0 {
            return Err(connect_eos_error(&payload));
          }
          let event: Value = serde_json::from_slice(&payload)
            .map_err(|e| format!("envd Start message is not JSON: {e}"))?;
          if let Some(end) = event.pointer("/event/end") {
            let status = end_event_status(end);
            tracing::info!(sandbox = %sandbox_id, status = %status, "sidecar process ended before reporting start");
            return Ok(StartOutcome::Ended(status));
          }
          if event.pointer("/event/start").is_some() {
            let pid = event
              .pointer("/event/start/pid")
              .and_then(Value::as_i64)
              .unwrap_or(0);
            return Ok(StartOutcome::Started(pid));
          }
          // keepalive / data (the sidecar's early output) — log
          // and keep waiting. UNRECOGNIZED shapes log at warn
          // with the raw payload: that is how an unexpected envd
          // dialect gets caught instead of silently EOFing.
          if !log_stream_event(sandbox_id, &payload) {
            tracing::warn!(
                sandbox = %sandbox_id,
                raw = %String::from_utf8_lossy(&payload),
                "envd Start: unrecognized pre-start message"
            );
          }
        }
      }
      Ok(None) => {
        return Err(format!(
          "envd Start stream ended before the start event (read {bytes_seen} bytes)"
        ));
      }
      Err(e) => return Err(format!("envd Start stream read: {e}")),
    }
  }
}

/// envd's EndEvent status string (falls back through `status` → `error` →
/// "unknown" — the exact field that carries the failure has varied).
fn end_event_status(end: &Value) -> String {
  end
    .get("status")
    .or_else(|| end.get("error"))
    .and_then(Value::as_str)
    .unwrap_or("unknown")
    .to_owned()
}

/// Log one envd stream frame for a sandbox. Data events are the SIDECAR'S
/// stdout/stderr (base64-encoded per the Connect JSON byte format) — the
/// only visibility into a runner boot failure — so they log at INFO; end
/// events at info; keepalives at trace. Returns whether the frame was a
/// recognized event message (false = caller logs the raw payload).
fn log_stream_event(sandbox_id: &str, payload: &[u8]) -> bool {
  use base64::Engine as _;
  let Ok(event) = serde_json::from_slice::<Value>(payload) else {
    return false;
  };
  let Some(data) = event.pointer("/event/data") else {
    if let Some(end) = event.pointer("/event/end") {
      tracing::info!(sandbox = %sandbox_id, status = %end_event_status(end), "sidecar process ended");
      return true;
    }
    if event.pointer("/event/start").is_some() || event.pointer("/event/keepalive").is_some() {
      tracing::trace!(sandbox = %sandbox_id, "envd stream control event");
      return true;
    }
    return false;
  };
  for (stream, key) in [("stdout", "stdout"), ("stderr", "stderr")] {
    let encoded = data.get(key).and_then(Value::as_str);
    let Some(encoded) = encoded else { continue };
    let decoded = base64::engine::general_purpose::STANDARD
      .decode(encoded)
      .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
      .unwrap_or_else(|_| encoded.to_owned());
    tracing::info!(sandbox = %sandbox_id, stream, "{}", decoded.trim_end());
  }
  true
}

/// Read an error response's body for the message (bounded — the platform's
/// error bodies are small JSON; truncation respects char boundaries —
/// E2B-GATE finding 2).
async fn truncate_body(resp: reqwest::Response) -> String {
  let text = resp.text().await.unwrap_or_default();
  const MAX: usize = 500;
  if text.len() <= MAX {
    return text;
  }
  // Byte 500 may fall inside a multi-byte char — floor to a boundary.
  let cut = text
    .char_indices()
    .map(|(i, _)| i)
    .take_while(|i| *i <= MAX)
    .last()
    .unwrap_or(0);
  format!("{}…", &text[..cut])
}

// ---------------------------------------------------------------------------
// The platform
// ---------------------------------------------------------------------------

/// The E2B platform: Cloud Build build half (shared with the
/// gcp platform) + E2B sandbox execution. ONE object, selected by
/// `SECTR_PLATFORM=e2b` — builder and runner cannot disagree.
pub struct E2bPlatform {
  gcp: GcpPlatform,
  client: E2bClient,
  runner_server_url: String,
  /// Host of `runner_server_url` — the `sectr.server` sandbox tag.
  server_host: String,
  runner_sidecar_cmd: Vec<String>,
}

impl E2bPlatform {
  /// Compose from the server config. The caller must have selected
  /// `PlatformKind::E2b` (the config validation fail-fasts on the gcs
  /// pairing + the E2B keys, and on the shared Cloud Build keys).
  pub async fn from_config(config: &ServerConfig, gcp: GcpPlatform) -> anyhow::Result<Self> {
    tracing::info!(domain = %config.e2b_domain, "artifact store: Cloud Build (shared with gcp) + E2B sandboxes");
    let puller_sa_json = config.e2b_puller_sa_json.clone().ok_or_else(|| {
      anyhow::anyhow!("SECTR_E2B_PULLER_SA_JSON is required when SECTR_PLATFORM=e2b")
    })?;
    let puller_sa_json = tokio::fs::read_to_string(&puller_sa_json)
      .await
      .map_err(|e| anyhow::anyhow!("read SECTR_E2B_PULLER_SA_JSON ({}): {e}", puller_sa_json))?;
    Ok(E2bPlatform {
      gcp,
      client: E2bClient::from_config(config, puller_sa_json)?,
      runner_server_url: config.runner_server_url.clone(),
      server_host: server_host_of(&config.runner_server_url)?,
      runner_sidecar_cmd: config.runner_sidecar_cmd.clone(),
    })
  }

  /// Ensure the deployment's E2B template exists and is built from the
  /// deployment's image (the build-path "step 4"): create-or-reuse the
  /// template, start a build from the AR ref, poll to terminal. A failed
  /// build fails the deploy before routing takes.
  async fn ensure_template(&self, deployment_id: Uuid, image_ref: &str) -> Result<(), BuildError> {
    let name = template_id(deployment_id);
    let (template, build) = match self.client.create_template(&name).await {
      Ok(ids) => ids,
      Err(e) => {
        // Live-verify note (deviation 3): same-name reuse semantics
        // of POST /v3/templates are the one under-pinned API
        // behavior. If the name already resolves to a template,
        // NAME it in the error (the alias-GET's templateID —
        // E2B-GATE finding 5) rather than improvising a second
        // write path.
        let existing = self.client.template_exists(&name).await.ok().flatten();
        return Err(BuildError::bare(match existing {
          Some(id) => format!(
            "create E2B template {name}: {e} — a template with this name \
                         already exists (templateID {id}); E2B's same-name rebuild \
                         semantics are a live-verify item"
          ),
          None => format!("create E2B template {name}: {e}"),
        }));
      }
    };
    self
      .client
      .start_template_build(&template, &build, image_ref)
      .await
      .map_err(|e| BuildError::bare(format!("start E2B template build {template}/{build}: {e}")))?;
    tracing::info!(template = %template, build = %build, image = %image_ref, "E2B template build started");

    let deadline = tokio::time::Instant::now() + TEMPLATE_BUILD_POLL_TIMEOUT;
    loop {
      let status = self
        .client
        .build_status(&template, &build)
        .await
        .map_err(|e| {
          BuildError::bare(format!("poll E2B template build {template}/{build}: {e}"))
        })?;
      match status_is_terminal(&status.status) {
        Some(true) => {
          tracing::info!(template = %template, build = %build, "E2B template build ready");
          return Ok(());
        }
        Some(false) => return Err(build_error_from_status(&status)),
        // building | waiting | uploaded — keep polling.
        None => {}
      }
      if tokio::time::Instant::now() >= deadline {
        return Err(BuildError::bare(format!(
          "E2B template build {template}/{build} did not finish within {}s",
          TEMPLATE_BUILD_POLL_TIMEOUT.as_secs()
        )));
      }
      tokio::time::sleep(TEMPLATE_BUILD_POLL_INTERVAL).await;
    }
  }

  /// Start the sidecar in the sandbox with the per-turn env (deviation
  /// 1). On failure the sandbox is killed before returning — a created
  /// sandbox must never orphan on our own error path (it would bill
  /// until its timeout).
  async fn start_sidecar(
    &self,
    created: &SandboxCreated,
    ingest_token: &str,
  ) -> Result<i64, DispatchError> {
    let (cmd, args) = split_sidecar_cmd(&self.runner_sidecar_cmd)
      .ok_or_else(|| DispatchError::Spawn("SECTR_RUNNER_SIDECAR_CMD is empty".into()))?;
    let envs = BTreeMap::from([
      (
        "SECTR_INGEST_URL".to_owned(),
        self.runner_server_url.clone(),
      ),
      ("SECTR_INGEST_TOKEN".to_owned(), ingest_token.to_owned()),
    ]);
    let pid = self
      .client
      .start_process(
        &created.sandbox_id,
        created.envd_access_token.as_deref(),
        &cmd,
        &args,
        &envs,
        "/app",
      )
      .await
      .map_err(|e| DispatchError::Spawn(format!("start sidecar in sandbox: {e}")))?;
    Ok(pid)
  }
}

#[async_trait]
impl RunnerPlatform for E2bPlatform {
  /// The Cloud Build half verbatim + the E2B template build as the
  /// deployment-resource step (the E2B analog of gcp's ensure-job).
  async fn build(&self, req: BuildRequest) -> Result<BuildOutput, BuildError> {
    let deployment_id = req.deployment_id;
    let output = self.gcp.cloud_build_impl(req).await?;
    self
      .ensure_template(deployment_id, &output.image_ref)
      .await?;
    Ok(output)
  }

  /// The AR ref — the canonical image identity; the template is derived
  /// from it (the template build consumes the AR ref, never
  /// produces a parallel identity).
  fn provisional_build_ref(&self, deployment_id: Uuid, version: i64) -> String {
    self.gcp.provisional_build_ref_impl(deployment_id, version)
  }

  async fn spawn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    invocation_id: Uuid,
    ingest_token: &str,
  ) -> Result<Option<String>, DispatchError> {
    // Ensure-template: an existence CHECK only (not self-healing — a template build is minutes, so a
    // missing template is a redeploy, never a silent rebuild).
    let name = template_id(deployment_id);
    match self.client.template_exists(&name).await {
      Ok(Some(_)) => {}
      Ok(None) => {
        return Err(DispatchError::Spawn(format!(
          "E2B template {name} for deployment {deployment_id} does not exist — \
                     redeploy the app (spawn never builds templates)"
        )));
      }
      Err(e) => {
        return Err(DispatchError::Spawn(format!(
          "check E2B template {name}: {e}"
        )));
      }
    }

    let metadata = sandbox_metadata(session, deployment_id, invocation_id, &self.server_host);
    let created = self
      .client
      .create_sandbox(&name, &metadata)
      .await
      .map_err(|e| create_sandbox_error(&name, &e))?;

    match self.start_sidecar(&created, ingest_token).await {
      Ok(pid) => {
        tracing::info!(
            session = %session,
            sandbox = %created.sandbox_id,
            pid,
            "E2B sandbox started with sidecar"
        );
        // The sandbox id is the OPAQUE teardown handle — the hub
        // carries it on the turn's grant (mirrors the Cloud Run
        // execution name).
        Ok(Some(created.sandbox_id))
      }
      Err(e) => {
        // Never orphan a sandbox on our own failure: created but
        // sidecar-less means idle billing until the timeout.
        tracing::warn!(session = %session, sandbox = %created.sandbox_id, error = %e, "sidecar start failed — killing the sandbox");
        if let Err(kill_err) = self.client.kill_sandbox(&created.sandbox_id).await {
          tracing::warn!(session = %session, sandbox = %created.sandbox_id, error = %kill_err, "sandbox cleanup after failed sidecar start also failed — it self-terminates at its timeout");
        }
        Err(e)
      }
    }
  }

  async fn teardown(&self, session: Uuid, handle: Option<&str>) {
    let Some(sandbox_id) = handle else {
      tracing::debug!(
          session = %session,
          "no sandbox handle (spawn raced a boundary or a restart dropped the grant) — nothing to kill; an orphaned sandbox self-terminates at its timeout"
      );
      return;
    };
    match self.client.kill_sandbox(sandbox_id).await {
      Ok(true) => {
        tracing::info!(session = %session, sandbox = %sandbox_id, "E2B sandbox killed");
      }
      Ok(false) => {
        tracing::debug!(session = %session, sandbox = %sandbox_id, "sandbox already gone — kill is a no-op");
      }
      Err(e) => {
        // Log-not-fail: teardown must never fail the turn-end path.
        tracing::warn!(session = %session, sandbox = %sandbox_id, error = %e, "sandbox kill failed");
      }
    }
  }

  /// The sandbox does NOT end itself when the sidecar exits — it bills
  /// until its timeout. The hub's boundary policy (the ONE
  /// policy point) consults this and tears the sandbox down at every
  /// turn boundary, after the ack.
  fn self_terminating(&self) -> bool {
    false
  }

  async fn list_runners(&self) -> Result<Option<Vec<RunnerInfo>>, DispatchError> {
    self
      .client
      .list_sandboxes(&self.server_host)
      .await
      .map(Some)
      .map_err(|e| DispatchError::Unavailable(format!("list sandboxes: {e}")))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn deployment() -> Uuid {
    // A fixed id: the assertions below pin exact strings.
    Uuid::parse_str("01912345-6789-7abc-def0-123456789abc").unwrap()
  }

  // -- naming / metadata contract --------------------------------------

  #[test]
  fn template_name_is_deterministic_and_fits_e2b_rules() {
    let d = deployment();
    let name = template_id(d);
    assert_eq!(name, format!("sectr-app-{d}"));
    assert!(name.len() <= 128);
    assert!(
      name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    );
  }

  #[test]
  fn sandbox_metadata_carries_the_sectr_keys() {
    let session = Uuid::now_v7();
    let invocation = Uuid::now_v7();
    let md = sandbox_metadata(session, deployment(), invocation, "ingest.sectr.dev");
    assert_eq!(md["sectr.session"], session.to_string());
    assert_eq!(md["sectr.deployment"], deployment().to_string());
    assert_eq!(md["sectr.invocation"], invocation.to_string());
    assert_eq!(md["sectr.server"], "ingest.sectr.dev");
    assert_eq!(md.len(), 4);
  }

  #[test]
  fn metadata_filter_encodes_key_and_value() {
    assert_eq!(
      metadata_filter("sectr.server", "ingest.sectr.dev"),
      "sectr.server=ingest.sectr.dev"
    );
    // Inner-query specials are escaped so one value can't smuggle a
    // second filter pair.
    assert_eq!(metadata_filter("k", "a&b=c"), "k=a%26b%3Dc");
  }

  #[test]
  fn non_429_create_errors_keep_their_detail() {
    let e = E2bError {
      status: 500,
      message: "boom".into(),
    };
    let shown = create_sandbox_error("sectr-app-x", &e).to_string();
    assert!(shown.contains("create sandbox from sectr-app-x"), "{shown}");
    assert!(shown.contains("boom"), "{shown}");
  }

  // -- request shapes (pinned against the documented schemas) -----------

  #[test]
  fn create_template_body_carries_name_cpu_memory() {
    let body = create_template_body("sectr-app-x", 2, 1024);
    assert_eq!(body["name"], "sectr-app-x");
    assert_eq!(body["cpuCount"], 2);
    assert_eq!(body["memoryMB"], 1024);
  }

  #[test]
  fn start_build_body_is_fromimage_with_gcp_registry_and_no_steps() {
    let body = start_build_body(
      "us-east4-docker.pkg.dev/p/repo/sectr-app-x:v1",
      "{\"project_id\":\"p\"}",
    );
    assert_eq!(
      body["fromImage"],
      "us-east4-docker.pkg.dev/p/repo/sectr-app-x:v1"
    );
    assert_eq!(body["fromImageRegistry"]["type"], "gcp");
    assert_eq!(
      body["fromImageRegistry"]["serviceAccountJson"],
      "{\"project_id\":\"p\"}"
    );
    // Deviation 1: no steps, no startCmd/readyCmd — the snapshot must
    // not capture a sidecar with build-time env.
    assert!(body.get("steps").is_none());
    assert!(body.get("startCmd").is_none());
    assert!(body.get("readyCmd").is_none());
  }

  #[test]
  fn create_sandbox_body_carries_timeout_secure_and_metadata_only() {
    let md = sandbox_metadata(
      Uuid::now_v7(),
      deployment(),
      Uuid::now_v7(),
      "ingest.sectr.dev",
    );
    let body = create_sandbox_body("sectr-app-x", 3600, &md);
    assert_eq!(body["templateID"], "sectr-app-x");
    assert_eq!(
      body["timeout"], 3600,
      "the API default is 15s — always pass ours"
    );
    assert_eq!(body["secure"], true);
    assert_eq!(body["metadata"]["sectr.session"], md["sectr.session"]);
    // Deviation 1: env is NOT passed here — the sidecar's env rides
    // the Start call (global envs reach envd-run commands only).
    assert!(body.get("envVars").is_none());
  }

  #[test]
  fn start_process_body_carries_cmd_args_envs_and_no_stdin() {
    let envs = BTreeMap::from([
      (
        "SECTR_INGEST_URL".to_owned(),
        "wss://example/ingest".to_owned(),
      ),
      ("SECTR_INGEST_TOKEN".to_owned(), "sit_tok".to_owned()),
    ]);
    let body = start_process_body(
      "/usr/local/bin/sectr-worker",
      &["--flag".to_owned()],
      &envs,
      "/app",
    );
    assert_eq!(body["process"]["cmd"], "/usr/local/bin/sectr-worker");
    assert_eq!(body["process"]["args"], serde_json::json!(["--flag"]));
    assert_eq!(
      body["process"]["envs"]["SECTR_INGEST_URL"],
      "wss://example/ingest"
    );
    assert_eq!(body["process"]["envs"]["SECTR_INGEST_TOKEN"], "sit_tok");
    assert_eq!(
      body["process"]["envs"].as_object().unwrap().len(),
      2,
      "ingest URL + token ONLY — secrets ride the WS-D assignment"
    );
    assert_eq!(body["process"]["cwd"], "/app");
    assert_eq!(body["stdin"], false);
  }

  #[test]
  fn sidecar_cmd_splits_on_the_first_token() {
    let (cmd, args) =
      split_sidecar_cmd(&["/usr/local/bin/sectr-worker".to_owned(), "--x".to_owned()]).unwrap();
    assert_eq!(cmd, "/usr/local/bin/sectr-worker");
    assert_eq!(args, vec!["--x"]);
    assert!(split_sidecar_cmd(&[]).is_none());
    let (cmd, args) = split_sidecar_cmd(&["bin".to_owned()]).unwrap();
    assert_eq!(cmd, "bin");
    assert!(args.is_empty());
  }

  // -- connect envelope ---------------------------------------------------

  #[test]
  fn connect_envelope_parses_the_5_byte_header() {
    // flags(1) + len(4 BE) + payload
    let payload = b"{\"event\":{\"start\":{\"pid\":42}}}";
    let mut buf = vec![0u8];
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    assert_eq!(parse_connect_envelope(&buf), Some((0u8, &payload[..])));
    // Incomplete buffers yield None.
    assert_eq!(parse_connect_envelope(&buf[..4]), None);
    assert_eq!(parse_connect_envelope(&buf[..5 + payload.len() - 1]), None);
    // The empty-payload envelope is valid.
    let mut empty = vec![0u8];
    empty.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(parse_connect_envelope(&empty), Some((0u8, &[][..])));
    // The EOS flag byte round-trips (in-band Connect error frames).
    let mut eos = vec![0b10u8];
    eos.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    eos.extend_from_slice(payload);
    assert_eq!(parse_connect_envelope(&eos), Some((0b10, &payload[..])));
  }

  // -- build-status handling ------------------------------------------------

  #[test]
  fn terminal_status_classification() {
    assert_eq!(status_is_terminal("ready"), Some(true));
    assert_eq!(status_is_terminal("error"), Some(false));
    assert_eq!(status_is_terminal("building"), None);
    assert_eq!(status_is_terminal("waiting"), None);
    // `uploaded` is the manual-upload path — not ours; never terminal.
    assert_eq!(status_is_terminal("uploaded"), None);
  }

  #[test]
  fn build_error_surfaces_reason_step_and_log_tail() {
    let status = BuildStatusView {
      status: "error".into(),
      logs: (0..30).map(|i| format!("log {i}")).collect(),
      reason: Some(BuildStatusReason {
        message: "step failed".into(),
        step: Some("dockerfile".into()),
      }),
    };
    let err = build_error_from_status(&status);
    assert_eq!(err.message, "step failed (step 'dockerfile')");
    // Last ~20 lines only.
    assert!(err.logs_tail.starts_with("log 10"));
    assert!(err.logs_tail.ends_with("log 29"));
  }

  #[test]
  fn build_error_without_reason_falls_back_to_the_status() {
    let status = BuildStatusView {
      status: "error".into(),
      logs: vec![],
      reason: None,
    };
    let err = build_error_from_status(&status);
    assert_eq!(err.message, "template build finished with status error");
    assert!(err.logs_tail.is_empty());
  }
}

// ---------------------------------------------------------------------------
// HTTP-mocked client tests (E2B-GATE finding 6): the pure body-shape tests
// above pin WHAT is sent; these pin THAT it reaches a wire-shaped HTTP
// surface correctly — most importantly the Connect REQUEST framing
// (finding 1) end-to-end. Platform-level spawn self-cleanup (kill after a
// failed Start) still needs a live E2B round-trip.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod http_tests {
  use super::*;
  use axum::Router;
  use axum::response::IntoResponse;
  use axum::routing::{delete, get, post};

  fn client_at(addr: std::net::SocketAddr) -> E2bClient {
    E2bClient {
      http: reqwest::Client::new(),
      api_base: format!("http://{addr}"),
      sandbox_base: format!("http://{addr}"),
      api_key: "test-key".into(),
      puller_sa_json: "{\"project_id\":\"p\"}".into(),
      cpu_count: 2,
      memory_mb: 1024,
      sandbox_timeout_secs: 3600,
    }
  }

  async fn spawn_server(router: Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
  }

  fn test_router() -> Router {
    async fn create_template() -> impl axum::response::IntoResponse {
      (
        axum::http::StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({"templateID": "t1", "buildID": "b1"})),
      )
    }
    async fn start_build() -> impl axum::response::IntoResponse {
      (
        axum::http::StatusCode::ACCEPTED,
        axum::Json(serde_json::json!({})),
      )
    }
    async fn build_status() -> axum::Json<serde_json::Value> {
      axum::Json(serde_json::json!({"status": "ready", "logs": ["layer ok"]}))
    }
    async fn alias(
      axum::extract::Path(name): axum::extract::Path<String>,
    ) -> impl axum::response::IntoResponse {
      if name == "sectr-app-exists" {
        (
          axum::http::StatusCode::OK,
          axum::Json(serde_json::json!({"templateID": "t-exists", "public": false})),
        )
          .into_response()
      } else {
        (
          axum::http::StatusCode::NOT_FOUND,
          axum::Json(serde_json::json!({"code": 5, "message": "not found"})),
        )
          .into_response()
      }
    }
    async fn create_sandbox(axum::Json(body): axum::Json<Value>) -> axum::response::Response {
      // The over-limit shape (docs.e2b.dev/billing): 429 + Retry-After.
      if body["templateID"] == "sectr-app-429" {
        return (
          axum::http::StatusCode::TOO_MANY_REQUESTS,
          [(axum::http::header::RETRY_AFTER, "7")],
          "concurrent sandbox limit of 20 reached for team t-123",
        )
          .into_response();
      }
      (
        axum::http::StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "sandboxID": "sbx-1",
            "clientID": "c",
            "envdVersion": "0.2.0",
            "envdAccessToken": "envd-tok",
            "templateID": "t1"
        })),
      )
        .into_response()
    }
    // GET /v2/sandboxes — STRICT like the real API's filter contract: the
    // `metadata` param is REQUIRED (400 without it), so a client that
    // forgets the filter can't pass tests while listing the whole team.
    // Two pages for the prod host (X-Next-Token), one malformed entry.
    async fn list_sandboxes(
      axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    ) -> axum::response::Response {
      let Some(metadata) = q.get("metadata") else {
        return (
          axum::http::StatusCode::BAD_REQUEST,
          "metadata filter is required",
        )
          .into_response();
      };
      assert_eq!(q.get("state").map(String::as_str), Some("running"));
      assert_eq!(q.get("limit").map(String::as_str), Some("100"));
      // The inner query string, decoded once by axum's Query.
      if metadata != "sectr.server=ingest.sectr.dev" {
        return axum::Json(serde_json::json!([])).into_response();
      }
      let sbx = |id: &str, session: &str, invocation: Option<&str>| {
        let mut md = serde_json::json!({
            "sectr.session": session,
            "sectr.server": "ingest.sectr.dev",
        });
        if let Some(inv) = invocation {
          md["sectr.invocation"] = inv.into();
        }
        serde_json::json!({
            "sandboxID": id,
            "templateID": "t1",
            "startedAt": "2026-10-03T12:00:00Z",
            "state": "running",
            "metadata": md,
        })
      };
      match q.get("nextToken").map(String::as_str) {
        None => (
          [("X-Next-Token", "tok2")],
          axum::Json(serde_json::json!([
            sbx(
              "sbx-a",
              "01912345-6789-7abc-def0-123456789abc",
              Some("01912345-6789-7abc-def0-123456789abd")
            ),
            // Malformed: session is not a UUID -> skipped.
            sbx("sbx-bad", "not-a-uuid", None),
            // Well-formed, but another environment's (as if E2B had ignored
            // the filter) -> the client-side `sectr.server` check skips it.
            {"sandboxID": "sbx-foreign", "startedAt": "2026-10-03T12:00:00Z", "metadata": {
                "sectr.session": "01912345-6789-7abc-def0-123456789ac0",
                "sectr.invocation": "01912345-6789-7abc-def0-123456789ac1",
                "sectr.server": "localhost",
            }},
            // Untagged (pre-`sectr.server` sandbox) -> skipped as well.
            {"sandboxID": "sbx-untagged", "startedAt": "2026-10-03T12:00:00Z", "metadata": {
                "sectr.session": "01912345-6789-7abc-def0-123456789ac2",
            }},
          ])),
        )
          .into_response(),
        Some("tok2") => axum::Json(serde_json::json!([
          // No invocation tag -> invocation None.
          sbx("sbx-b", "01912345-6789-7abc-def0-123456789abe", None),
          // Malformed: no startedAt -> skipped.
          {"sandboxID": "sbx-nostart", "metadata": {
              "sectr.session": "01912345-6789-7abc-def0-123456789abf",
              "sectr.server": "ingest.sectr.dev",
          }},
        ]))
        .into_response(),
        Some(other) => (
          axum::http::StatusCode::BAD_REQUEST,
          format!("unknown nextToken {other}"),
        )
          .into_response(),
      }
    }
    async fn kill(
      axum::extract::Path(id): axum::extract::Path<String>,
    ) -> impl axum::response::IntoResponse {
      if id == "sbx-gone" {
        axum::http::StatusCode::NOT_FOUND
      } else {
        axum::http::StatusCode::NO_CONTENT
      }
    }
    // The finding-1 assertion lives HERE: the Start request body must
    // be ENVELOPED (request message + end-of-stream marker), not raw
    // JSON. The response covers the 2026-09-17 live finding: the first
    // stream message is NOT guaranteed to be the StartEvent — keepalive
    // first, then start (the loop must skip and keep waiting). A
    // sandbox with id `sbx-dead` instead emits the sidecar's stderr +
    // an immediate EndEvent (the boot-failure shape).
    async fn process_start(
      headers: axum::http::HeaderMap,
      body: axum::body::Bytes,
    ) -> impl axum::response::IntoResponse {
      assert_eq!(
        headers.get("content-type").unwrap(),
        "application/connect+json"
      );
      let routed_id = headers
        .get("e2b-sandbox-id")
        .expect("sandbox id header")
        .to_str()
        .unwrap();
      assert!(
        routed_id == "sbx-1" || routed_id == "sbx-dead" || routed_id == "sbx-authfail",
        "the shared sandbox host routes by id header"
      );
      // The authfail test deliberately sends a WRONG token — envd
      // would reject it in-band; here we just require its presence.
      assert!(
        !headers
          .get("x-access-token")
          .expect("access token")
          .is_empty(),
        "secure sandboxes require the X-Access-Token"
      );
      // Request message envelope: flags 0, BE length, JSON payload.
      let (req_flags, payload) =
        parse_connect_envelope(&body).expect("request body must carry a complete Connect envelope");
      assert_eq!(req_flags, 0, "request message is not an EOS frame");
      let msg: Value = serde_json::from_slice(payload).unwrap();
      assert_eq!(msg["process"]["cmd"], "/usr/local/bin/sectr-worker");
      assert_eq!(msg["process"]["envs"]["SECTR_INGEST_TOKEN"], "sit_tok");
      // End-of-stream frame: flags 0b10, `{}` EndStreamResponse.
      let payload_len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
      let tail = &body[5 + payload_len..];
      assert_eq!(tail.len(), 7, "the EOS frame follows the message");
      assert_eq!(tail[0] & 0b10, 0b10, "EOS flag set");
      assert_eq!(&tail[1..5], &[0, 0, 0, 2], "empty-object payload");
      assert_eq!(&tail[5..7], b"{}");
      // Respond with enveloped events: keepalive, [stderr + end for
      // the dead sandbox | start for the live one].
      let mut resp: Vec<u8> = Vec::new();
      let push = |event: &Value, resp: &mut Vec<u8>| {
        let payload = event.to_string().into_bytes();
        resp.push(0u8);
        resp.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        resp.extend_from_slice(&payload);
      };
      push(&serde_json::json!({"event": {"keepalive": {}}}), &mut resp);
      if routed_id == "sbx-authfail" {
        // The live finding's shape: HTTP 200 + an in-band Connect
        // EOS ERROR frame (flags 0b10) — envd-level failures do not
        // use non-200 statuses.
        let error = serde_json::json!({
            "error": {"code": "unauthenticated", "message": "invalid access token"}
        })
        .to_string()
        .into_bytes();
        resp.push(CONNECT_FLAG_END_STREAM);
        resp.extend_from_slice(&(error.len() as u32).to_be_bytes());
        resp.extend_from_slice(&error);
      } else if routed_id == "sbx-dead" {
        use base64::Engine as _;
        let stderr = base64::engine::general_purpose::STANDARD
          .encode("env validation failed: SECTR_INGEST_TOKEN missing\n");
        push(
          &serde_json::json!({"event": {"data": {"stderr": stderr}}}),
          &mut resp,
        );
        push(
          &serde_json::json!({"event": {"end": {"status": "exit status 1", "error": null}}}),
          &mut resp,
        );
      } else {
        push(
          &serde_json::json!({"event": {"start": {"pid": 7}}}),
          &mut resp,
        );
      }
      (
        [(axum::http::header::CONTENT_TYPE, "application/connect+json")],
        resp,
      )
    }
    Router::new()
      .route("/v3/templates", post(create_template))
      .route("/v2/templates/{tid}/builds/{bid}", post(start_build))
      .route("/templates/{tid}/builds/{bid}/status", get(build_status))
      .route("/templates/aliases/{name}", get(alias))
      .route("/sandboxes", post(create_sandbox))
      .route("/v2/sandboxes", get(list_sandboxes))
      .route("/sandboxes/{id}", delete(kill))
      .route("/process.Process/Start", post(process_start))
  }

  #[tokio::test]
  async fn build_flow_reaches_a_wire_shaped_api() {
    let addr = spawn_server(test_router()).await;
    let client = client_at(addr);
    let (tid, bid) = client.create_template("sectr-app-x").await.unwrap();
    assert_eq!((tid.as_str(), bid.as_str()), ("t1", "b1"));
    client
      .start_template_build(&tid, &bid, "registry/img:v1")
      .await
      .unwrap();
    let status = client.build_status(&tid, &bid).await.unwrap();
    assert_eq!(status.status, "ready");
    assert_eq!(status_is_terminal(&status.status), Some(true));
  }

  #[tokio::test]
  async fn list_sandboxes_filters_pages_and_skips_malformed_entries() {
    let addr = spawn_server(test_router()).await;
    let client = client_at(addr);
    let runners = client.list_sandboxes("ingest.sectr.dev").await.unwrap();
    assert_eq!(
      runners
        .iter()
        .map(|r| r.handle.as_str())
        .collect::<Vec<_>>(),
      ["sbx-a", "sbx-b"],
      "both pages followed; malformed, foreign-host and untagged entries skipped"
    );
    assert_eq!(
      runners[0].session,
      Uuid::parse_str("01912345-6789-7abc-def0-123456789abc").unwrap()
    );
    assert_eq!(
      runners[0].invocation,
      Some(Uuid::parse_str("01912345-6789-7abc-def0-123456789abd").unwrap())
    );
    assert_eq!(runners[1].invocation, None);
    assert_eq!(
      runners[0].started_at.to_rfc3339(),
      "2026-10-03T12:00:00+00:00"
    );

    // The server-side filter: another environment's host lists nothing.
    assert!(client.list_sandboxes("localhost").await.unwrap().is_empty());
  }

  #[tokio::test]
  async fn the_mock_refuses_an_unfiltered_listing() {
    // Guards the fake itself: no `metadata` param -> 400, so the client
    // tests above can't pass by accident against a team-wide listing.
    let addr = spawn_server(test_router()).await;
    let resp = reqwest::Client::new()
      .get(format!("http://{addr}/v2/sandboxes?state=running"))
      .send()
      .await
      .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
  }

  #[tokio::test]
  async fn sandbox_create_429_becomes_the_friendly_capacity_message() {
    let addr = spawn_server(test_router()).await;
    let client = client_at(addr);
    let err = client
      .create_sandbox(
        "sectr-app-429",
        &sandbox_metadata(Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), "h"),
      )
      .await
      .unwrap_err();
    assert_eq!(err.status, 429);
    let shown = create_sandbox_error("sectr-app-429", &err).to_string();
    assert!(shown.contains("Sectr is at capacity right now"), "{shown}");
    assert!(shown.contains("runner backend 429"), "{shown}");
    assert!(
      !shown.contains("team t-123"),
      "raw provider body leaked: {shown}"
    );
  }

  #[tokio::test]
  async fn template_exists_maps_200_and_404() {
    let addr = spawn_server(test_router()).await;
    let client = client_at(addr);
    assert_eq!(
      client.template_exists("sectr-app-exists").await.unwrap(),
      Some("t-exists".to_owned())
    );
    assert_eq!(
      client.template_exists("sectr-app-missing").await.unwrap(),
      None
    );
  }

  #[tokio::test]
  async fn sandbox_create_kill_and_start_process_round_trip() {
    let addr = spawn_server(test_router()).await;
    let client = client_at(addr);
    let created = client
      .create_sandbox(
        "sectr-app-x",
        &sandbox_metadata(Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), "h"),
      )
      .await
      .unwrap();
    assert_eq!(created.sandbox_id, "sbx-1");
    assert_eq!(created.envd_access_token.as_deref(), Some("envd-tok"));

    // The kill-404 no-op contract (already-gone → Ok(false)).
    assert!(!client.kill_sandbox("sbx-gone").await.unwrap());
    assert!(client.kill_sandbox(&created.sandbox_id).await.unwrap());

    // Happy-path sidecar start: the handler asserts the envelope
    // framing (finding 1) AND that the keepalive-first stream is
    // skipped (the 2026-09-17 live finding — pid 0 meant the old
    // single-message read mistook a keepalive for the start event);
    // we still get the real pid back.
    let envs = BTreeMap::from([
      (
        "SECTR_INGEST_URL".to_owned(),
        "wss://example/ingest".to_owned(),
      ),
      ("SECTR_INGEST_TOKEN".to_owned(), "sit_tok".to_owned()),
    ]);
    let pid = client
      .start_process(
        &created.sandbox_id,
        created.envd_access_token.as_deref(),
        "/usr/local/bin/sectr-worker",
        &[],
        &envs,
        "/app",
      )
      .await
      .unwrap();
    assert_eq!(pid, 7);
  }

  #[tokio::test]
  async fn boot_failure_surfaces_the_sidecars_stderr_and_exit_status() {
    let addr = spawn_server(test_router()).await;
    let client = client_at(addr);
    let envs = BTreeMap::from([
      (
        "SECTR_INGEST_URL".to_owned(),
        "wss://example/ingest".to_owned(),
      ),
      ("SECTR_INGEST_TOKEN".to_owned(), "sit_tok".to_owned()),
    ]);
    // The `sbx-dead` handler emits stderr + EndEvent before any start:
    // the launch must FAIL (not "succeed" with pid 0 like the live
    // finding) and carry the exit status.
    let err = client
      .start_process(
        "sbx-dead",
        Some("envd-tok"),
        "/usr/local/bin/sectr-worker",
        &[],
        &envs,
        "/app",
      )
      .await
      .unwrap_err();
    assert!(err.message.contains("exit status 1"), "{err}");
  }

  #[tokio::test]
  async fn in_band_connect_error_frame_surfaces_envds_complaint() {
    // The 2026-09-17 live finding #2: an envd-level failure (e.g. auth)
    // arrives as HTTP 200 + an EOS error frame, NOT a non-200 status —
    // the old flags-blind parser turned that into a bare "stream ended
    // before the start event" with zero diagnostic content.
    let addr = spawn_server(test_router()).await;
    let client = client_at(addr);
    let envs = BTreeMap::from([
      (
        "SECTR_INGEST_URL".to_owned(),
        "wss://example/ingest".to_owned(),
      ),
      ("SECTR_INGEST_TOKEN".to_owned(), "sit_tok".to_owned()),
    ]);
    let err = client
      .start_process(
        "sbx-authfail",
        Some("wrong-token"),
        "/usr/local/bin/sectr-worker",
        &[],
        &envs,
        "/app",
      )
      .await
      .unwrap_err();
    assert!(err.message.contains("invalid access token"), "{err}");
    assert!(err.message.contains("unauthenticated"), "{err}");
  }

  #[test]
  fn connect_request_frame_is_enveloped_with_eos() {
    let frame = connect_request_frame(br#"{"a":1}"#);
    assert_eq!(frame[0], 0);
    assert_eq!(&frame[1..5], &7u32.to_be_bytes());
    assert_eq!(&frame[5..12], br#"{"a":1}"#);
    // End-of-stream frame: flags 0b10 + `{}` (the empty
    // EndStreamResponse — a zero-length payload fails envd's JSON
    // unmarshal; live finding 2026-09-17 #3).
    assert_eq!(frame.len(), 19);
    assert_eq!(frame[12] & 0b10, 0b10, "EOS flag");
    assert_eq!(&frame[13..17], &[0, 0, 0, 2], "{{}} payload length");
    assert_eq!(&frame[17..19], b"{}");
    assert_eq!(
      parse_connect_envelope(&frame),
      Some((0u8, &br#"{"a":1}"#[..]))
    );
    // The trailing EOS frame parses as an end-of-stream frame.
    assert_eq!(
      parse_connect_envelope(&frame[12..]),
      Some((CONNECT_FLAG_END_STREAM, &br#"{}"#[..]))
    );
  }

  #[test]
  fn sandbox_created_debug_masks_the_envd_token() {
    let created = SandboxCreated {
      sandbox_id: "sbx-1".into(),
      envd_access_token: Some("secret-token".into()),
    };
    let rendered = format!("{created:?}");
    assert!(!rendered.contains("secret-token"), "{rendered}");
    assert!(rendered.contains("<masked>"), "{rendered}");
  }
}
