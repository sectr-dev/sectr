//! The sessions-api HTTP contract server (`schema/sessions-api.yaml`) —
//! shared by the dev gateway (`sectr dev`) and the prod public API
//! (`sectr-server`).
//!
//! Layering mirrors the contract (`schema/README.md`, "API contract
//! layering"): this module is auth-AGNOSTIC — paths
//! carry no security knowledge; the composing server layers auth as
//! middleware (prod) or not at all (dev, localhost-only). The routes
//! speak to a [`SessionService`] implemented per deployment:
//!
//! - dev: `sectr_worker::dispatch::service::DevSessionService` (in-process
//!   engine + SQLite journal/registry)
//! - prod (Phase 5): sectr-server's impl over PgJournal + the sidecar
//!
//! Types are mirrored from `schema/sessions-api.yaml` (mirrored-types +
//! golden-fixture policy — no codegen); conformance tests pin the shapes.
//! The journal is the source of truth: the SSE endpoint is a journal tail
//! (`Last-Event-ID` resumption), never a proxy of a running runner.

pub mod cli_login;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router, http::StatusCode, routing::post};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod agui;

use crate::journal::{EventStream, SessionId};

/// `sectr.tail.connections` — live SSE tail connections. Lives
/// HERE (not in the server) because this is the shared sessions-api seam:
/// the dev gateway's tails count too. Up-down around the stream lifetime
/// (guarded increment/decrement below). No-op without the OTLP gate.
static TAIL_CONNECTIONS: std::sync::LazyLock<opentelemetry::metrics::UpDownCounter<i64>> =
  std::sync::LazyLock::new(|| {
    opentelemetry::global::meter("sectr")
      .i64_up_down_counter("sectr.tail.connections")
      .with_description("Live SSE tail connections")
      .build()
  });

/// Decrement the tail gauge when the stream drops (any exit path).
struct TailGuard;

impl Drop for TailGuard {
  fn drop(&mut self) {
    TAIL_CONNECTIONS.add(-1, &[]);
  }
}

// ---------------------------------------------------------------------------
// Types — mirrored from schema/sessions-api.yaml
// ---------------------------------------------------------------------------

/// The API view of a session's state (`sessions-api.yaml` Session.status —
/// four values). NOTE: distinct from `registry::SessionStatus`, which is
/// the DDL row model (`active`/`awaiting_approval`/`ended`) — `error` is
/// DERIVED at this layer from the journal (a failed last turn) rather than
/// stored: the journal is the source of truth, and an errored session can
/// still accept new turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
  Active,
  AwaitingApproval,
  Ended,
  Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
  Api,
  Playground,
}

/// What the authenticated caller may see and how their sessions are
/// stamped. The composing server derives it from its principal (prod: the
/// auth middleware; dev: [`RequestScope::unscoped`]) and the shared router
/// passes it to the service. Auth itself stays outside this module —
/// this is deployment-neutral visibility, not credentials.
#[derive(Debug, Clone)]
pub struct RequestScope {
  /// Tenant boundary; `None` = unscoped (dev, single-tenant).
  pub org_id: Option<Uuid>,
  /// Invocation/user keys are app-scoped; `None` = whole org (platform).
  pub app_id: Option<Uuid>,
  /// Stamped on sessions this principal creates.
  pub origin: Origin,
  /// User tokens only: the bound end-user identity — the owner
  /// filter on every read and the owner stamped on created sessions.
  /// `None` for the other classes (and dev).
  pub external_user_id: Option<String>,
  /// User tokens only: the credential behind this request, so a
  /// long-lived response (the SSE tail) can end when the token expires or
  /// is revoked instead of outliving it. `None` for the other classes
  /// (and dev).
  pub user_token: Option<UserTokenLease>,
}

/// Identity + hard expiry of the user token a request authenticated with.
#[derive(Debug, Clone)]
pub struct UserTokenLease {
  /// The `api_keys` row id (liveness re-check: revocation).
  pub key_id: Uuid,
  pub expires_at: DateTime<Utc>,
}

impl RequestScope {
  /// Dev / no-auth: sees everything, creates playground sessions.
  pub fn unscoped() -> Self {
    RequestScope {
      org_id: None,
      app_id: None,
      origin: Origin::Playground,
      external_user_id: None,
      user_token: None,
    }
  }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
  pub id: Uuid,
  pub app_id: Uuid,
  pub route: String,
  pub origin: Origin,
  pub deployment_id: Uuid,
  pub status: SessionStatus,
  pub created_at: DateTime<Utc>,
  pub last_event_at: Option<DateTime<Utc>>,
  pub event_count: i64,
  /// Display label ([`derive_session_title`]); absent when there's no text
  /// to show.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub title: Option<String>,
  /// Owner identity: the app-defined end-user the session
  /// belongs to. Absent for backend/platform-created sessions.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub external_user_id: Option<String>,
}

pub use crate::events::{InvalidInput, UserMessageInput};

impl From<InvalidInput> for ServiceError {
  fn from(e: InvalidInput) -> Self {
    ServiceError::BadRequest(e.to_string())
  }
}

#[derive(Debug)]
pub struct NewSessionRequest {
  /// App id or name — the dev gateway serves exactly one app and rejects
  /// others; the prod API resolves by either.
  pub app: String,
  pub route: String,
  pub input: UserMessageInput,
}

#[derive(Debug)]
pub struct SessionInput {
  pub input: UserMessageInput,
}

/// `POST /sessions`'s body, before its input is checked.
#[derive(Debug, Serialize, Deserialize)]
pub struct NewSessionBody {
  pub app: String,
  pub route: String,
  pub input: serde_json::Value,
}

/// `POST /sessions`'s 201 answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCreated {
  pub session_id: SessionId,
}

/// `POST /sessions/{id}/input`'s body, before its input is checked.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInputBody {
  pub input: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApprovalDecisionRequest {
  pub approved: bool,
  /// Omitted: a user token's decision is attributed to its end user
  /// (`external_user_id`); other callers (and the dev gateway) record none.
  pub approver: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListSessionsQuery {
  pub app_id: Option<String>,
  pub status: Option<SessionStatus>,
  pub limit: Option<i64>,
  /// RFC 3339; keyset pagination.
  pub created_before: Option<DateTime<Utc>>,
  /// Owner filter for backend/platform listing views. IGNORED
  /// (rejected, 400) for user tokens — their owner filter comes from the
  /// token itself; there is no way to read another user's sessions
  /// through a browser token, by construction.
  pub external_user_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct EventsQuery {
  /// First-connect resumption alternative to the `Last-Event-ID` header.
  pub last_event_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingApproval {
  pub session: SessionSummary,
  /// The TOOL_APPROVAL_REQUIRED envelope (or the TOOL_CALL that awaits a
  /// decision when the approval event is absent from the transcript).
  pub event: crate::events::Event,
}

/// One AG-UI run, as the turn-level request the AG-UI endpoint hands the
/// service. The service owns thread resolution, visibility,
/// the busy-thread 409 and the turn start, so ownership, journaling and quotas
/// stay on the single `SessionService` authorization path.
#[derive(Debug, Clone)]
pub struct RunRequest {
  /// App id or slug (the URL's `{app}`).
  pub app: String,
  /// Route path with its leading slash (the URL's `{route}`).
  pub route: String,
  /// The AG-UI `threadId`, which IS the Sectr session id.
  pub thread_id: SessionId,
  /// The AG-UI `runId`, journaled on the run's `INVOCATION_STARTED` as
  /// `ag_ui.run_id`. The invocation id itself is always server-minted.
  pub run_id: String,
  pub intent: RunIntent,
}

/// One AG-UI connect: the thread a read-only re-stream is for,
/// under the URL's app and route. See [`SessionService::resolve_thread`].
#[derive(Debug, Clone)]
pub struct ThreadRef {
  /// App id or slug (the URL's `{app}`).
  pub app: String,
  /// Route path with its leading slash (the URL's `{route}`).
  pub route: String,
  /// The AG-UI `threadId`, which IS the Sectr session id.
  pub thread_id: SessionId,
}

#[derive(Debug, Clone)]
pub enum RunIntent {
  /// A user message: starts a turn (new thread → session created under
  /// `thread_id`). A thread with a live turn is a 409.
  UserMessage {
    /// Journaled as `INVOCATION_STARTED.reason.input`.
    input: UserMessageInput,
    /// The client's id for the message, journaled for snapshot
    /// reconciliation.
    client_message_id: Option<String>,
    /// The request carried conversation history before this message (see
    /// `agui::UserInput::has_history`): fine on an existing thread (the
    /// journal is its history), refused when the run would create one.
    has_history: bool,
    /// AG-UI `forwardedProps` / `context`, journaled on
    /// `INVOCATION_STARTED` (forward-compat extras the runner reads from
    /// its transcript). `input` stays the bare message content.
    forwarded_props: Option<serde_json::Value>,
    context: Option<Vec<serde_json::Value>>,
  },
  /// A resume run answering the thread's pending approval interrupt.
  ApprovalDecision {
    /// The interrupt id (the journal id of the `TOOL_APPROVAL_REQUIRED` row).
    interrupt_id: String,
    approved: bool,
  },
}

impl RunIntent {
  /// May this run CREATE thread `thread_id` (it does not exist yet)? Shared
  /// by the prod and dev [`SessionService::start_run`] impls:
  ///
  /// - a resume run: no — there is no interrupt to answer (`NotFound`)
  /// - a user message carrying prior conversation history: no (`BadRequest`)
  ///   — a Sectr thread starts empty and history import is not supported,
  ///   so accepting it would silently run the agent cold while the client
  ///   shows a conversation the agent never saw
  pub fn check_creates_thread(&self, thread_id: SessionId) -> Result<(), ServiceError> {
    match self {
      RunIntent::UserMessage {
        has_history: true, ..
      } => Err(ServiceError::BadRequest(format!(
        "thread {thread_id} does not exist, but `messages` carries earlier conversation \
         history; a new thread must start with only the user message (importing history \
         is not supported)"
      ))),
      RunIntent::UserMessage { .. } => Ok(()),
      RunIntent::ApprovalDecision { .. } => Err(ServiceError::NotFound(format!(
        "thread {thread_id} not found"
      ))),
    }
  }
}

/// What [`SessionService::start_run`] started.
#[derive(Debug, Clone)]
pub struct StartedRun {
  pub session_id: SessionId,
  /// The invocation whose journal events ARE the run's stream.
  pub invocation_id: Uuid,
  /// The thread was created by this run (no prior history to snapshot).
  pub created: bool,
}

/// What a run on an EXISTING thread does (see [`plan_run`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunPlan {
  /// Start a new user-message turn.
  StartTurn,
  /// Journal the approval decision and resume the suspended turn.
  ResumeApproval,
}

/// The busy-thread / pending-interrupt decision for a run on an existing
/// thread, from the thread's journal and its live lease — ONE rule shared
/// by the prod and dev [`SessionService::start_run`] impls:
///
/// - user message while a turn is live: 409 (retry after the turn ends).
///   No adoption of the live turn: a reloaded page reattaches through the
///   read-only AG-UI connect, so a run is only ever a new
///   question — and a second tab / client must not be handed another
///   question's answer.
/// - user message while awaiting an approval decision: 409 (use `resume`)
/// - resume: only against the thread's pending `TOOL_APPROVAL_REQUIRED`
///   (interrupt id = its journal id); a live turn is a 409, anything else an
///   unknown interrupt (`BadRequest`, surfaced as `RUN_ERROR` by the handler)
pub fn plan_run(
  events: &[crate::events::Event],
  live_lease: Option<&crate::registry::Lease>,
  intent: &RunIntent,
) -> Result<RunPlan, ServiceError> {
  let status = derive_session_status(events);
  match intent {
    RunIntent::UserMessage { .. } => {
      if live_lease.is_some() {
        return Err(ServiceError::Conflict(
          "thread is mid-turn; retry after the current run finishes".into(),
        ));
      }
      match status {
        SessionStatus::AwaitingApproval => Err(ServiceError::Conflict(
          "thread is awaiting an approval decision — answer the interrupt with `resume`".into(),
        )),
        SessionStatus::Ended => Err(ServiceError::Conflict("thread has ended".into())),
        _ => Ok(RunPlan::StartTurn),
      }
    }
    RunIntent::ApprovalDecision { interrupt_id, .. } => {
      if live_lease.is_some() {
        return Err(ServiceError::Conflict(
          "thread is mid-turn; wait for the current turn to finish".into(),
        ));
      }
      // Only a TOOL_APPROVAL_REQUIRED row is an interrupt (its journal id
      // IS the interrupt id).
      let pending = crate::journal::awaiting_approval(events);
      match pending {
        Some(p) if interrupt_id.parse::<Uuid>().ok() == Some(p.id) => Ok(RunPlan::ResumeApproval),
        Some(_) => Err(ServiceError::BadRequest(format!(
          "unknown interrupt {interrupt_id:?}: it does not match this thread's pending interrupt"
        ))),
        None => Err(ServiceError::BadRequest(
          "this thread has no pending interrupt to resume".into(),
        )),
      }
    }
  }
}

#[derive(Debug, Deserialize)]
pub struct DevStatusRequest {}

/// Mirror of `dev-api.yaml` `DevStatus` — what `GET /dev/status` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevStatus {
  pub project: String,
  pub app: String,
  /// Registry id sessions are stamped with — clients join session rows
  /// to the app by this, not by name.
  pub app_id: String,
  pub gateway_port: u16,
  /// Framework adapter from the SDK inspector's manifest (absent when the
  /// handler carries no `sectr_framework` marker).
  #[serde(skip_serializing_if = "Option::is_none")]
  pub framework: Option<String>,
  /// Manifest routes — clients need one to create a session.
  pub routes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Service trait — the seam between the HTTP contract and an engine
// ---------------------------------------------------------------------------

/// An org, as the API names it to a caller: its id and name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgRef {
  pub id: Uuid,
  pub name: String,
}

/// `GET /me`'s answer: the caller's org and the server's dashboard origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
  pub org: OrgRef,
  pub dashboard_url: String,
}

/// `PUT /secrets/{key}`'s body: the secret's value, never echoed back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretValue {
  pub value: String,
}

/// Every API error's body: `{error}`, plus a machine-readable `code` where
/// clients branch on the kind of refusal (429s, the beta cap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
  pub error: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub code: Option<String>,
}

impl ErrorBody {
  pub fn new(error: impl Into<String>) -> Self {
    ErrorBody {
      error: error.into(),
      code: None,
    }
  }

  pub fn with_code(error: impl Into<String>, code: impl Into<String>) -> Self {
    ErrorBody {
      error: error.into(),
      code: Some(code.into()),
    }
  }

  /// A `{error}` response with `status`.
  pub fn response(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(Self::new(error))).into_response()
  }
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
  #[error("not found: {0}")]
  NotFound(String),
  #[error("conflict: {0}")]
  Conflict(String),
  #[error("bad request: {0}")]
  BadRequest(String),
  /// Authenticated but not privileged (e.g. the platform-only approvals
  /// inbox asked for by an app-scoped principal).
  #[error("forbidden: {0}")]
  Forbidden(String),
  #[error("internal: {0}")]
  Internal(String),
  /// A concurrency / capacity / rate limit (server policy; the
  /// dev gateway never returns it). 429 + `Retry-After` + `{error, code}`.
  /// `code` ∈ `concurrency_limit` | `platform_capacity` | `rate_limited`.
  #[error("too many requests: {message}")]
  TooManyRequests {
    code: &'static str,
    message: String,
    retry_after_secs: u64,
  },
}

impl IntoResponse for ServiceError {
  fn into_response(self) -> Response {
    let (status, message) = match &self {
      ServiceError::NotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
      ServiceError::Conflict(_) => (StatusCode::CONFLICT, self.to_string()),
      ServiceError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
      ServiceError::Forbidden(_) => (StatusCode::FORBIDDEN, self.to_string()),
      ServiceError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
      ServiceError::TooManyRequests {
        code,
        message,
        retry_after_secs,
      } => {
        return (
          StatusCode::TOO_MANY_REQUESTS,
          [(
            axum::http::header::RETRY_AFTER,
            retry_after_secs.to_string(),
          )],
          Json(ErrorBody::with_code(message.clone(), *code)),
        )
          .into_response();
      }
    };
    ErrorBody::response(status, message)
  }
}

#[async_trait::async_trait]
pub trait SessionService: Send + Sync {
  /// Create a session and START its first turn (`input` as a
  /// user_message reason). Returns before the turn produces anything —
  /// clients follow the SSE tail. 201 with the session id.
  async fn create_session(
    &self,
    req: NewSessionRequest,
    scope: RequestScope,
  ) -> Result<SessionId, ServiceError>;

  async fn list_sessions(
    &self,
    query: ListSessionsQuery,
    scope: RequestScope,
  ) -> Result<Vec<SessionSummary>, ServiceError>;

  async fn get_session(
    &self,
    id: SessionId,
    scope: RequestScope,
  ) -> Result<SessionSummary, ServiceError>;

  /// Start a follow-up turn. 202 once the turn is scheduled. 409 when
  /// the session is mid-turn (live lease) or awaiting an approval
  /// decision; accepted after an errored turn (that's the recovery path).
  async fn send_input(
    &self,
    id: SessionId,
    req: SessionInput,
    scope: RequestScope,
  ) -> Result<(), ServiceError>;

  /// Decide a pending approval; resumes the suspended turn. 202.
  /// 409 when nothing is pending.
  async fn decide_approval(
    &self,
    id: SessionId,
    req: ApprovalDecisionRequest,
    scope: RequestScope,
  ) -> Result<(), ServiceError>;

  async fn pending_approvals(
    &self,
    scope: RequestScope,
  ) -> Result<Vec<PendingApproval>, ServiceError>;

  /// Start the turn behind one AG-UI run. Services
  /// that do not serve AG-UI keep the default.
  async fn start_run(
    &self,
    _req: RunRequest,
    _scope: RequestScope,
  ) -> Result<StartedRun, ServiceError> {
    Err(ServiceError::Internal(
      "AG-UI runs are not supported by this service".into(),
    ))
  }

  /// Resolve an AG-UI thread for a READ (connect): the same app /
  /// token-binding / route / ownership checks as [`Self::start_run`], but
  /// no turn and no creation. `Ok(None)` = no such thread (connect answers
  /// an empty stream, so a fresh thread id reveals nothing new).
  async fn resolve_thread(
    &self,
    _req: ThreadRef,
    _scope: RequestScope,
  ) -> Result<Option<SessionId>, ServiceError> {
    Err(ServiceError::Internal(
      "AG-UI connect is not supported by this service".into(),
    ))
  }

  /// The journal tail: replays from seq 0 (or after `last_event_id`),
  /// then holds open. The stream never terminates on its own; client
  /// disconnect drops it.
  async fn session_events(
    &self,
    id: SessionId,
    last_event_id: Option<Uuid>,
    scope: RequestScope,
  ) -> Result<EventStream, ServiceError>;
}

// ---------------------------------------------------------------------------
// Router — the shared axum surface (dev gateway and prod server mount this)
// ---------------------------------------------------------------------------

pub fn sessions_router(service: Arc<dyn SessionService>) -> Router {
  Router::new()
    .route("/sessions", post(create_session).get(list_sessions))
    .route("/sessions/{session_id}", get(get_session))
    .route("/sessions/{session_id}/events", get(events))
    .route("/sessions/{session_id}/input", post(input))
    .route("/sessions/{session_id}/approval", post(approval))
    .route("/approvals", get(pending_approvals))
    .with_state(service)
}

async fn create_session(
  State(service): State<Arc<dyn SessionService>>,
  scope: Option<axum::Extension<RequestScope>>,
  Json(body): Json<NewSessionBody>,
) -> Response {
  let req = match UserMessageInput::new(body.input) {
    Ok(input) => NewSessionRequest {
      app: body.app,
      route: body.route,
      input,
    },
    Err(e) => return ServiceError::from(e).into_response(),
  };
  let scope = scope
    .map(|axum::Extension(s)| s)
    .unwrap_or_else(RequestScope::unscoped);
  match service.create_session(req, scope).await {
    Ok(id) => (StatusCode::CREATED, Json(SessionCreated { session_id: id })).into_response(),
    Err(e) => e.into_response(),
  }
}

async fn list_sessions(
  State(service): State<Arc<dyn SessionService>>,
  scope: Option<axum::Extension<RequestScope>>,
  Query(query): Query<ListSessionsQuery>,
) -> Response {
  let scope = scope
    .map(|axum::Extension(s)| s)
    .unwrap_or_else(RequestScope::unscoped);
  match service.list_sessions(query, scope).await {
    Ok(list) => Json(list).into_response(),
    Err(e) => e.into_response(),
  }
}

async fn get_session(
  State(service): State<Arc<dyn SessionService>>,
  scope: Option<axum::Extension<RequestScope>>,
  Path(id): Path<SessionId>,
) -> Response {
  let scope = scope
    .map(|axum::Extension(s)| s)
    .unwrap_or_else(RequestScope::unscoped);
  match service.get_session(id, scope).await {
    Ok(summary) => Json(summary).into_response(),
    Err(e) => e.into_response(),
  }
}

async fn events(
  State(service): State<Arc<dyn SessionService>>,
  scope: Option<axum::Extension<RequestScope>>,
  Path(id): Path<SessionId>,
  headers: axum::http::HeaderMap,
  Query(query): Query<EventsQuery>,
) -> Response {
  let scope = scope
    .map(|axum::Extension(s)| s)
    .unwrap_or_else(RequestScope::unscoped);
  // `Last-Event-ID` header wins over the query-param alternative.
  let last_event_id = headers
    .get("last-event-id")
    .and_then(|v| v.to_str().ok())
    .and_then(|v| Uuid::parse_str(v).ok())
    .or(query.last_event_id);
  match service.session_events(id, last_event_id, scope).await {
    Ok(stream) => {
      // One span per tail connection — the span (and the
      // guard's -1) live until the stream drops, i.e. until the
      // connection ends on ANY path (client close, journal error,
      // consumer cancel). Frame-level debug events enter it per
      // frame so each frame is attributable to its tail connection.
      let tail_span = tracing::info_span!(
          "sse.tail",
          session_id = %id,
          after_last_event_id = tracing::field::Empty,
      );
      if let Some(last) = last_event_id {
        tail_span.record("after_last_event_id", last.to_string());
      }
      let _setup_guard = tail_span.enter();
      TAIL_CONNECTIONS.add(1, &[]);
      let tail_guard = TailGuard;
      // Journal read errors END the stream: the client reconnects
      // (with Last-Event-ID) rather than seeing a corrupt frame.
      //
      // Per-frame tracing lives at DEBUG level (target `sectr_sse`):
      // enable with `RUST_LOG=sectr_common=debug` when the console
      // and the server disagree about journal delivery (M1 run's
      // nondeterministic prefix bug) — it timestamps exactly when the
      // server emitted each frame; the browser's `[sse]` console
      // trace is the other half of the comparison.
      let started = std::time::Instant::now();
      // Own the span + gauge guard for the STREAM's lifetime: the
      // span closes (and the gauge drops) when the response body
      // ends. Per-frame debug events enter the span so each frame is
      // attributable to its tail connection.
      let frame_span = tail_span.clone();
      let mapped = stream
        .filter_map(move |item| {
          let _ = &tail_guard; // keep the -1 guard alive with the stream
          let frame_span = frame_span.clone();
          async move {
            // Enter per frame: each debug event is attributable
            // to this tail connection's span.
            let _frame_guard = frame_span.enter();
            match item {
              Ok(event) => {
                tracing::debug!(
                    target: "sectr_sse",
                    session = %event.session_id,
                    seq = event.seq,
                    event_type = %event.event_type,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "sse frame emitted"
                );
                match sse_frame(&event) {
                  Ok(frame) => Some(Ok::<_, std::convert::Infallible>(frame)),
                  Err(e) => {
                    tracing::error!(error = %e, "sse: failed to serialize event");
                    None
                  }
                }
              }
              Err(e) => {
                tracing::warn!(error = %e, "sse: journal tail error; ending stream");
                None
              }
            }
          }
        })
        .boxed();
      Sse::new(mapped)
        // 500ms heartbeat: Safari (WebKit) sometimes pauses delivery
        // of an in-flight SSE body after a page RELOAD (~50% of
        // reloads, arbitrary prefix — M1 run) and only resumes when
        // the next bytes arrive; the heartbeat IS what flushes it.
        // With a 500ms interval the stall is bounded to ≤500ms and
        // imperceptible (6×3-byte comments/sec — nothing). Firefox
        // and Chrome never stall. The console's tail additionally
        // drops a connection that delivers NO bytes for >4s and
        // re-opens with last_event_id (exact resume) — that path
        // covers total stalls regardless of cause.
        .keep_alive(KeepAlive::new().interval(Duration::from_millis(500)))
        .into_response()
    }
    Err(e) => e.into_response(),
  }
}

fn sse_frame(
  event: &crate::events::Event,
) -> Result<axum::response::sse::Event, serde_json::Error> {
  Ok(
    axum::response::sse::Event::default()
      .id(event.id.to_string())
      .data(serde_json::to_string(event)?),
  )
}

async fn input(
  State(service): State<Arc<dyn SessionService>>,
  scope: Option<axum::Extension<RequestScope>>,
  Path(id): Path<SessionId>,
  Json(body): Json<SessionInputBody>,
) -> Response {
  let req = match UserMessageInput::new(body.input) {
    Ok(input) => SessionInput { input },
    Err(e) => return ServiceError::from(e).into_response(),
  };
  let scope = scope
    .map(|axum::Extension(s)| s)
    .unwrap_or_else(RequestScope::unscoped);
  match service.send_input(id, req, scope).await {
    Ok(()) => StatusCode::ACCEPTED.into_response(),
    Err(e) => e.into_response(),
  }
}

async fn approval(
  State(service): State<Arc<dyn SessionService>>,
  scope: Option<axum::Extension<RequestScope>>,
  Path(id): Path<SessionId>,
  Json(req): Json<ApprovalDecisionRequest>,
) -> Response {
  let scope = scope
    .map(|axum::Extension(s)| s)
    .unwrap_or_else(RequestScope::unscoped);
  match service.decide_approval(id, req, scope).await {
    Ok(()) => StatusCode::ACCEPTED.into_response(),
    Err(e) => e.into_response(),
  }
}

async fn pending_approvals(
  State(service): State<Arc<dyn SessionService>>,
  scope: Option<axum::Extension<RequestScope>>,
) -> Response {
  let scope = scope
    .map(|axum::Extension(s)| s)
    .unwrap_or_else(RequestScope::unscoped);
  // Platform-only: the inbox is a CROSS-app view. App-scoped
  // principals — invocation keys and user tokens — are authenticated but
  // not this class (their per-session approval DECISIONS ride the
  // session routes). Prod platform principals carry app_id = None;
  // dev unscoped passes.
  if scope.app_id.is_some() {
    return ServiceError::Forbidden(
      "the approvals inbox is platform-key-only; decide approvals on the session routes".into(),
    )
    .into_response();
  }
  match service.pending_approvals(scope).await {
    Ok(list) => Json(list).into_response(),
    Err(e) => e.into_response(),
  }
}

// ---------------------------------------------------------------------------
// Derivation — journal → the API view (shared by dev and prod services)
// ---------------------------------------------------------------------------

/// Derive the session's API status from its journal. The journal is the
/// source of truth; the rules:
///
/// - journal ENDS on `TOOL_APPROVAL_REQUIRED` (suspended — no decision
///   journaled after it) → `awaiting_approval`
/// - the last turn (from its `user_message` invocation on) holds an `ERROR`
///   or a `SESSION_ERROR` → `error`. Errors end the turn, so the session
///   takes a new one, and once that starts it's `active` again.
/// - otherwise → `active` (mid-turn, idle between turns, or turn ended)
///
/// `ended` is never derived: it's an explicit session-lifecycle state
/// (registry-driven; nothing sets it in MVP).
pub fn derive_session_status(events: &[crate::events::Event]) -> SessionStatus {
  use crate::events::{EventPayload, EventType, InvocationReason};
  if events.last().map(|e| e.event_type) == Some(EventType::ToolApprovalRequired) {
    return SessionStatus::AwaitingApproval;
  }
  let turn_start = events
    .iter()
    .rposition(|e| {
      matches!(
        &e.payload,
        EventPayload::InvocationStarted(inv)
          if matches!(inv.reason, InvocationReason::UserMessage { .. })
      )
    })
    .unwrap_or(0);
  let failed = events[turn_start..]
    .iter()
    .any(|e| matches!(e.event_type, EventType::Error | EventType::SessionError));
  if failed {
    SessionStatus::Error
  } else {
    SessionStatus::Active
  }
}

/// The most characters a [`SessionSummary::title`] keeps, before the `…`.
pub const SESSION_TITLE_MAX_CHARS: usize = 80;

/// Derive the session's display title from its journal: today, the text of
/// its first user message, whitespace collapsed and truncated to
/// [`SESSION_TITLE_MAX_CHARS`]. `None` when no user message is journaled
/// yet.
///
/// Server-computed so the derivation can change (a summary, say) without
/// touching clients — they display it, never parse it.
pub fn derive_session_title(events: &[crate::events::Event]) -> Option<String> {
  use crate::events::{EventPayload, InvocationReason};
  let text = events.iter().find_map(|e| match &e.payload {
    EventPayload::InvocationStarted(inv) => match &inv.reason {
      InvocationReason::UserMessage { input, .. } => Some(input),
      _ => None,
    },
    _ => None,
  })?;
  let words = text
    .as_str()
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ");
  if words.is_empty() {
    return None;
  }
  if words.chars().count() <= SESSION_TITLE_MAX_CHARS {
    return Some(words);
  }
  let cut: String = words.chars().take(SESSION_TITLE_MAX_CHARS).collect();
  Some(format!("{}…", cut.trim_end()))
}

// ---------------------------------------------------------------------------
// Tests — router semantics over a stub service (HTTP contract level)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::events::{EventPayload, SessionCreated};

  mod status {
    use super::*;
    use crate::events::{
      ApprovalDecision, Error, Event, InvocationReason, InvocationStarted, NoMoreActions,
      SessionError, ToolApprovalRequired,
    };

    fn journal(payloads: Vec<EventPayload>) -> Vec<Event> {
      payloads
        .into_iter()
        .enumerate()
        .map(|(seq, payload)| {
          Event::new(
            uuid::Uuid::now_v7(),
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            seq as i64,
            chrono::Utc::now(),
            payload,
          )
        })
        .collect()
    }

    fn started(reason: InvocationReason) -> EventPayload {
      EventPayload::InvocationStarted(InvocationStarted {
        invocation_id: uuid::Uuid::now_v7(),
        route: "/chat".into(),
        deployment_id: uuid::Uuid::nil(),
        reason,
        extra: Default::default(),
      })
    }
    fn user() -> EventPayload {
      started(InvocationReason::UserMessage {
        input: "hi".to_string().try_into().unwrap(),
        extra: Default::default(),
      })
    }
    fn resume() -> EventPayload {
      started(InvocationReason::ApprovalDecision {
        decision: ApprovalDecision {
          approved: true,
          approver: None,
          extra: Default::default(),
        },
        extra: Default::default(),
      })
    }
    fn end() -> EventPayload {
      EventPayload::NoMoreActions(NoMoreActions::default())
    }
    fn session_error() -> EventPayload {
      EventPayload::SessionError(SessionError {
        message: "endpoint wake failed: …".into(),
        extra: Default::default(),
      })
    }
    fn runner_error() -> EventPayload {
      EventPayload::Error(Error {
        message: "status_code: 401".into(),
        code: None,
        transient: None,
        extra: Default::default(),
      })
    }
    fn approval() -> EventPayload {
      EventPayload::ToolApprovalRequired(ToolApprovalRequired {
        tool_call_id: "c1".into(),
        name: "refund".into(),
        arguments: serde_json::Value::Null,
        reason: None,
        extra: Default::default(),
      })
    }

    #[test]
    fn the_last_turn_decides() {
      use SessionStatus::*;
      let cases: Vec<(Vec<EventPayload>, SessionStatus)> = vec![
        (vec![], Active),
        (vec![user()], Active),
        (vec![user(), end()], Active),
        (vec![user(), approval()], AwaitingApproval),
        // A turn that failed, whichever way, ends in error.
        (vec![user(), session_error(), end()], Error),
        (vec![user(), runner_error(), end()], Error),
        (vec![user(), session_error()], Error),
        (
          vec![user(), approval(), resume(), session_error(), end()],
          Error,
        ),
        // A new turn after a failed one recovers the session, from its
        // start.
        (vec![user(), session_error(), end(), user()], Active),
        (vec![user(), session_error(), end(), user(), end()], Active),
        // An earlier turn's error doesn't follow an approval pause.
        (
          vec![user(), runner_error(), end(), user(), approval()],
          AwaitingApproval,
        ),
      ];
      for (payloads, want) in cases {
        let types: Vec<_> = payloads.iter().map(|p| p.event_type()).collect();
        assert_eq!(derive_session_status(&journal(payloads)), want, "{types:?}");
      }
    }
  }
  use axum::body::Body;
  use axum::http::{Request, StatusCode};
  use futures::stream;
  use tower::ServiceExt;

  #[derive(Default)]
  struct Stub {
    fail_get: bool,
  }

  fn envelope(seq: i64) -> crate::events::Event {
    crate::events::Event::new(
      uuid::Uuid::now_v7(),
      uuid::Uuid::now_v7(),
      uuid::Uuid::now_v7(),
      seq,
      chrono::Utc::now(),
      EventPayload::SessionCreated(SessionCreated {
        app_id: uuid::Uuid::now_v7(),
        route: "/chat".into(),
        extra: Default::default(),
        external_user_id: None,
      }),
    )
  }

  #[async_trait::async_trait]
  impl SessionService for Stub {
    async fn create_session(
      &self,
      _req: NewSessionRequest,
      _scope: RequestScope,
    ) -> Result<SessionId, ServiceError> {
      Ok(uuid::Uuid::now_v7())
    }
    async fn list_sessions(
      &self,
      _query: ListSessionsQuery,
      _scope: RequestScope,
    ) -> Result<Vec<SessionSummary>, ServiceError> {
      Ok(vec![])
    }
    async fn get_session(
      &self,
      _id: SessionId,
      _scope: RequestScope,
    ) -> Result<SessionSummary, ServiceError> {
      if self.fail_get {
        return Err(ServiceError::NotFound("nope".into()));
      }
      Err(ServiceError::Internal("unused in these tests".into()))
    }
    async fn send_input(
      &self,
      _id: SessionId,
      _req: SessionInput,
      _scope: RequestScope,
    ) -> Result<(), ServiceError> {
      Err(ServiceError::Conflict("session is mid-turn".into()))
    }
    async fn decide_approval(
      &self,
      _id: SessionId,
      _req: ApprovalDecisionRequest,
      _scope: RequestScope,
    ) -> Result<(), ServiceError> {
      Err(ServiceError::Conflict("no pending approval".into()))
    }
    async fn pending_approvals(
      &self,
      _scope: RequestScope,
    ) -> Result<Vec<PendingApproval>, ServiceError> {
      Ok(vec![])
    }
    async fn session_events(
      &self,
      _id: SessionId,
      _last_event_id: Option<uuid::Uuid>,
      _scope: RequestScope,
    ) -> Result<EventStream, ServiceError> {
      let a = envelope(0);
      let b = envelope(1);
      Ok(Box::pin(stream::iter(vec![Ok(a), Ok(b)])))
    }
  }

  fn app() -> Router {
    sessions_router(Arc::new(Stub::default()))
  }

  #[tokio::test]
  async fn create_returns_201_with_session_id() {
    let response = app()
      .oneshot(
        Request::builder()
          .method("POST")
          .uri("/sessions")
          .header("content-type", "application/json")
          .body(Body::from(
            r#"{"app": "a", "route": "/chat", "input": "hi"}"#,
          ))
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
      .await
      .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["session_id"].is_string());
  }

  /// A non-string or whitespace-only input is rejected with 400 before
  /// the service sees it: some framework and provider pairs can't
  /// send an empty user message. (The stub would answer 201 or 409.)
  #[test]
  fn user_message_input_accepts_only_a_non_empty_string() {
    use serde_json::json;
    for empty in [json!(""), json!(" \n\t")] {
      assert_eq!(
        UserMessageInput::new(empty.clone()),
        Err(InvalidInput::Empty),
        "{empty}"
      );
    }
    for input in [
      json!({"message": "hi"}),
      json!({"text": "hi"}),
      json!([{"type": "text", "text": "hi"}]),
      json!(0),
    ] {
      assert_eq!(
        UserMessageInput::new(input.clone()),
        Err(InvalidInput::NotAString),
        "{input}"
      );
    }
    assert_eq!(UserMessageInput::new(json!("hi")).unwrap().as_str(), "hi");
  }

  #[tokio::test]
  async fn an_invalid_input_is_rejected() {
    for (input, expected) in [
      ("\"\"", "empty"),
      ("\"  \\n\\t\"", "empty"),
      ("[]", "must be a string"),
      (r#"{"message": "hi"}"#, "must be a string"),
      (r#"[{"type": "text", "text": "hi"}]"#, "must be a string"),
    ] {
      for (uri, body) in [
        (
          "/sessions",
          format!(r#"{{"app": "a", "route": "/chat", "input": {input}}}"#),
        ),
        (
          "/sessions/0192f3a4-0000-7000-8000-000000000001/input",
          format!(r#"{{"input": {input}}}"#),
        ),
      ] {
        let response = app()
          .oneshot(
            Request::builder()
              .method("POST")
              .uri(uri)
              .header("content-type", "application/json")
              .body(Body::from(body))
              .unwrap(),
          )
          .await
          .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri} {input}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
          .await
          .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains(expected), "{json}");
      }
    }
  }

  #[tokio::test]
  async fn conflict_maps_to_409() {
    let response = app()
      .oneshot(
        Request::builder()
          .method("POST")
          .uri("/sessions/0192f3a4-0000-7000-8000-000000000001/input")
          .header("content-type", "application/json")
          .body(Body::from(r#"{"input": "hi"}"#))
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
  }

  #[tokio::test]
  async fn not_found_maps_to_404() {
    let app = sessions_router(Arc::new(Stub { fail_get: true }));
    let response = app
      .oneshot(
        Request::builder()
          .method("GET")
          .uri("/sessions/0192f3a4-0000-7000-8000-000000000001")
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  async fn events_sse_carries_id_and_envelope_data() {
    let response = app()
      .oneshot(
        Request::builder()
          .method("GET")
          .uri("/sessions/0192f3a4-0000-7000-8000-000000000001/events")
          .body(Body::empty())
          .unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
      .await
      .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    // Two frames, each with an SSE id and an envelope-shaped data line.
    let frames: Vec<&str> = text.trim().split("\n\n").collect();
    assert_eq!(frames.len(), 2, "two events -> two frames: {text}");
    for frame in &frames {
      assert!(frame.starts_with("id: "), "frame: {frame}");
      let data = frame.lines().find(|l| l.starts_with("data: ")).unwrap();
      let envelope: serde_json::Value =
        serde_json::from_str(&data[6..]).expect("data is an event envelope JSON");
      assert!(envelope["seq"].is_i64());
      assert_eq!(envelope["type"], "SESSION_CREATED");
    }
    // The ids are distinct (Last-Event-ID resumption keys on them).
    let id0 = frames[0].lines().next().unwrap()[4..].to_string();
    let id1 = frames[1].lines().next().unwrap()[4..].to_string();
    assert_ne!(id0, id1);
  }

  // -- plan_run: the busy-thread / interrupt rule shared by prod + dev ---

  fn ev(seq: i64, payload: EventPayload) -> crate::events::Event {
    crate::events::Event::new(
      uuid::Uuid::now_v7(),
      uuid::Uuid::nil(),
      uuid::Uuid::nil(),
      seq,
      chrono::Utc::now(),
      payload,
    )
  }

  fn started(seq: i64, invocation_id: Uuid, input: &str) -> crate::events::Event {
    ev(
      seq,
      EventPayload::InvocationStarted(crate::events::InvocationStarted {
        invocation_id,
        route: "/chat".into(),
        deployment_id: Uuid::nil(),
        reason: crate::events::InvocationReason::UserMessage {
          input: input.to_string().try_into().unwrap(),
          extra: Default::default(),
        },
        extra: Default::default(),
      }),
    )
  }

  fn lease(invocation_id: Option<Uuid>) -> crate::registry::Lease {
    let now = chrono::Utc::now();
    crate::registry::Lease {
      id: Uuid::now_v7(),
      session_id: Uuid::nil(),
      runner_id: "r".into(),
      invocation_id,
      claimed_at: now,
      heartbeat_at: now,
      expires_at: now,
      released_at: None,
    }
  }

  fn user(input: &str) -> RunIntent {
    RunIntent::UserMessage {
      input: UserMessageInput::new(serde_json::json!(input)).unwrap(),
      client_message_id: None,
      has_history: false,
      forwarded_props: None,
      context: None,
    }
  }

  fn approval_required(seq: i64) -> crate::events::Event {
    ev(
      seq,
      EventPayload::ToolApprovalRequired(crate::events::ToolApprovalRequired {
        tool_call_id: "c1".into(),
        name: "t".into(),
        arguments: serde_json::json!({}),
        reason: None,
        extra: Default::default(),
      }),
    )
  }

  #[test]
  fn plan_run_refuses_any_user_message_while_a_turn_is_live() {
    let inv = Uuid::now_v7();
    let events = vec![started(1, inv, "cats")];
    // Same input or not, a live turn is busy: no adoption.
    for input in ["cats", "dogs"] {
      assert!(matches!(
        plan_run(&events, Some(&lease(Some(inv))), &user(input)),
        Err(ServiceError::Conflict(_))
      ));
    }
    // An idle lease (no current invocation) is simply busy.
    assert!(matches!(
      plan_run(&events, Some(&lease(None)), &user("cats")),
      Err(ServiceError::Conflict(_))
    ));
    // No live lease: a new turn.
    assert_eq!(
      plan_run(&events, None, &user("cats")).unwrap(),
      RunPlan::StartTurn
    );
  }

  #[test]
  fn plan_run_resume_needs_the_pending_interrupt() {
    let approval = approval_required(2);
    let events = vec![started(1, Uuid::now_v7(), "refund"), approval.clone()];
    let resume = |id: String| RunIntent::ApprovalDecision {
      interrupt_id: id,
      approved: true,
    };
    // A user message on a thread awaiting approval is a conflict.
    assert!(matches!(
      plan_run(&events, None, &user("hi")),
      Err(ServiceError::Conflict(_))
    ));
    assert_eq!(
      plan_run(&events, None, &resume(approval.id.to_string())).unwrap(),
      RunPlan::ResumeApproval
    );
    // Unknown / malformed interrupt ids → BadRequest (RUN_ERROR upstream).
    for id in [Uuid::now_v7().to_string(), "nope".to_string()] {
      assert!(matches!(
        plan_run(&events, None, &resume(id)),
        Err(ServiceError::BadRequest(_))
      ));
    }
    // A live turn blocks resume; no pending interrupt is a BadRequest.
    assert!(matches!(
      plan_run(
        &events,
        Some(&lease(None)),
        &resume(approval.id.to_string())
      ),
      Err(ServiceError::Conflict(_))
    ));
    let done = vec![started(1, Uuid::now_v7(), "hi")];
    assert!(matches!(
      plan_run(&done, None, &resume(approval.id.to_string())),
      Err(ServiceError::BadRequest(_))
    ));
  }

  fn started_with(seq: i64, input: String) -> crate::events::Event {
    ev(
      seq,
      EventPayload::InvocationStarted(crate::events::InvocationStarted {
        invocation_id: Uuid::now_v7(),
        route: "/chat".into(),
        deployment_id: Uuid::nil(),
        reason: crate::events::InvocationReason::UserMessage {
          input: input.try_into().unwrap(),
          extra: Default::default(),
        },
        extra: Default::default(),
      }),
    )
  }

  #[test]
  fn session_title_is_the_first_user_message_text() {
    // The first user message wins; whitespace collapses.
    assert_eq!(
      derive_session_title(&[
        envelope(0),
        started(1, Uuid::now_v7(), "  refund\n order   42 "),
        started(2, Uuid::now_v7(), "later"),
      ]),
      Some("refund order 42".into())
    );
    assert_eq!(derive_session_title(&[envelope(0)]), None);
  }

  #[test]
  fn session_title_truncates_on_a_char_boundary() {
    let fits = "é".repeat(SESSION_TITLE_MAX_CHARS);
    assert_eq!(
      derive_session_title(&[started_with(0, fits.clone())]),
      Some(fits.clone())
    );
    let long = format!("{} tail", "é".repeat(SESSION_TITLE_MAX_CHARS - 1));
    assert_eq!(
      derive_session_title(&[started_with(0, long)]),
      Some(format!("{}…", "é".repeat(SESSION_TITLE_MAX_CHARS - 1)))
    );
  }
}
