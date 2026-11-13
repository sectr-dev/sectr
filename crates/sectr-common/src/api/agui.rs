//! `/ag-ui/{app}/{route}` — the AG-UI 1.0 HTTP surface, shared by
//! the prod server and the `sectr dev` gateway exactly like
//! [`super::sessions_router`]: auth-AGNOSTIC (the composing server layers
//! auth, the credential-class gate and request spans; dev layers nothing).
//!
//! - `POST` — a run: starts one turn and streams it.
//! - `GET ?threadId=` — connect: re-streams the thread's latest
//!   run, read-only. AG-UI 1.0 has no reconnect; this is the server half of
//!   an SDK `connect()` (the starter's `SectrRunner`), so a reloaded page
//!   gets its history, a live turn or a pending approval back from Sectr.
//!
//! Every deployed agent route is a first-class AG-UI endpoint, served as
//! another VIEW over the journal: both streams are a FILTERED journal read
//! (one invocation's events, via [`crate::agui::Projector`]), never the
//! session tail and never a runner proxy. One AG-UI run = one Sectr turn:
//! `threadId` is the session id, `runId` the invocation.
//!
//! The endpoint adds NO session logic of its own — thread resolution,
//! ownership, journaling and quotas are [`SessionService::start_run`] (and
//! [`SessionService::resolve_thread`] for connect). What lives here is only
//! the wire: request validation, the SSE response, and the CORS layer
//! ([`agui_cors_layer`], applied OUTSIDE auth by the composer so preflights
//! are answered before credentials are required).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

use crate::agui::{
  AgUiEvent, PROTOCOL_VERSION, Projector, ResumeStatus, RunAgentInput, RunContext, connect_context,
  fold_messages, supports_protocol_version,
};
use crate::events::{Event, EventPayload};
use crate::journal::{EventStream, JournalReader, SessionId};

use super::{
  RequestScope, RunIntent, RunRequest, ServiceError, SessionService, StartedRun, ThreadRef,
};

/// Request body cap: CopilotKit-style clients re-send the whole message
/// history with every run.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Keep-alive comment cadence (≤20s: Cloudflare-proven, same discipline as
/// platform SSE).
const KEEP_ALIVE: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct AgUiState {
  service: Arc<dyn SessionService>,
  journal: Arc<dyn JournalReader>,
}

/// The AG-UI route. Compose with [`agui_cors_layer`] (outermost) and, in
/// prod, the auth layers.
pub fn agui_router(service: Arc<dyn SessionService>, journal: Arc<dyn JournalReader>) -> Router {
  Router::new()
    .route("/ag-ui/{app}/{*route}", post(run).get(connect))
    .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
    .with_state(AgUiState { service, journal })
}

/// CORS for `/ag-ui/*`: the direct path is a cross-origin browser `fetch`
/// with `content-type: application/json` (preflighted) from arbitrary
/// frontend origins (a customer's site, `next dev` on localhost). Auth is
/// the Bearer header (no cookies, no credentialed requests), so a wildcard
/// origin is safe — and required: external frontends can't be enumerated.
pub fn agui_cors_layer() -> CorsLayer {
  CorsLayer::new()
    .allow_origin(Any)
    .allow_methods([
      axum::http::Method::GET,
      axum::http::Method::POST,
      axum::http::Method::OPTIONS,
    ])
    .allow_headers([
      axum::http::header::AUTHORIZATION,
      axum::http::header::CONTENT_TYPE,
      axum::http::header::ACCEPT,
    ])
    .max_age(Duration::from_secs(600))
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
  super::ErrorBody::response(status, message)
}

async fn run(
  State(state): State<AgUiState>,
  scope: Option<axum::Extension<RequestScope>>,
  Path((app, route)): Path<(String, String)>,
  body: Bytes,
) -> Response {
  let scope = scope
    .map(|axum::Extension(s)| s)
    .unwrap_or_else(RequestScope::unscoped);

  let input: RunAgentInput = match serde_json::from_slice(&body) {
    Ok(input) => input,
    Err(e) => {
      return error_response(
        StatusCode::BAD_REQUEST,
        format!("invalid RunAgentInput: {e}"),
      );
    }
  };
  if let Some(version) = &input.protocol_version
    && !supports_protocol_version(version)
  {
    return error_response(
      StatusCode::BAD_REQUEST,
      format!(
        "unsupported protocolVersion {version:?}: this endpoint speaks AG-UI {PROTOCOL_VERSION} (accepts 1.x)"
      ),
    );
  }
  let thread_id: Uuid = match input.thread_id.parse() {
    Ok(id) => id,
    Err(_) => {
      return error_response(
        StatusCode::BAD_REQUEST,
        "threadId must be a UUID: it is the Sectr session id",
      );
    }
  };
  let echo = serde_json::to_value(&input).expect("RunAgentInput serializes");
  let route = format!("/{route}");

  let intent = if input.is_resume() {
    match resume_intent(&input) {
      Ok(intent) => intent,
      // Resume-answer problems are RUN_ERRORs (the protocol's channel for
      // "this run cannot proceed"), not HTTP errors.
      Err(message) => return run_error_response(&input, echo, &message),
    }
  } else {
    match input.trailing_user_input() {
      Ok(user) => RunIntent::UserMessage {
        input: user.input,
        client_message_id: user.message_id,
        has_history: user.has_history,
        forwarded_props: input.forwarded_props.clone(),
        context: input.context.clone(),
      },
      Err(message) => return error_response(StatusCode::BAD_REQUEST, message),
    }
  };
  let is_resume = matches!(intent, RunIntent::ApprovalDecision { .. });

  let started = match state
    .service
    .start_run(
      RunRequest {
        app,
        route,
        thread_id,
        run_id: input.run_id.clone(),
        intent,
      },
      scope.clone(),
    )
    .await
  {
    Ok(started) => started,
    // For a resume run a BadRequest is an unknown/mismatched interrupt:
    // RUN_ERROR per the spec.
    Err(ServiceError::BadRequest(message)) if is_resume => {
      return run_error_response(&input, echo, &message);
    }
    Err(e) => return e.into_response(),
  };

  match stream_run(&state, scope, &input, echo, started).await {
    Ok(response) => response,
    Err(e) => e.into_response(),
  }
}

/// `resume[]` → the approval decision (grant/deny). Exactly one entry: a
/// thread has at most one pending interrupt, so anything else names an
/// interrupt that does not exist. `cancelled` ⇒ deny.
fn resume_intent(input: &RunAgentInput) -> Result<RunIntent, String> {
  let entries = input.resume.as_deref().unwrap_or_default();
  let [entry] = entries else {
    return Err(format!(
      "resume must answer exactly one interrupt (a thread has at most one pending); got {}",
      entries.len()
    ));
  };
  let approved = match (entry.status, &entry.payload) {
    (ResumeStatus::Cancelled, _) => false,
    (ResumeStatus::Resolved, Some(serde_json::Value::Bool(approved))) => *approved,
    (ResumeStatus::Resolved, _) => {
      return Err(
        "a resolved tool-approval interrupt needs a boolean payload (true = approve, false = deny)"
          .into(),
      );
    }
  };
  Ok(RunIntent::ApprovalDecision {
    interrupt_id: entry.interrupt_id.clone(),
    approved,
  })
}

/// A run that fails before it starts: `RUN_STARTED` + `RUN_ERROR`, over a
/// normal 200 SSE response.
fn run_error_response(input: &RunAgentInput, echo: serde_json::Value, message: &str) -> Response {
  let mut projector = Projector::new(RunContext {
    thread_id: input.thread_id.clone(),
    run_id: input.run_id.clone(),
    input: Some(echo),
    snapshot: None,
  });
  let events = projector.abort(message, "invalid_run");
  sse_response(futures::stream::iter(
    events.into_iter().map(|e| Ok::<_, Infallible>(frame(e))),
  ))
}

/// The run's SSE stream: a filtered journal read of the started
/// invocation, projected to AG-UI.
async fn stream_run(
  state: &AgUiState,
  scope: RequestScope,
  input: &RunAgentInput,
  echo: serde_json::Value,
  started: StartedRun,
) -> Result<Response, ServiceError> {
  let history = fetch_history(state, started.session_id).await?;
  let idx = history
    .iter()
    .position(|e| {
      matches!(&e.payload, EventPayload::InvocationStarted(i) if i.invocation_id == started.invocation_id)
    })
    .ok_or_else(|| {
      ServiceError::Internal("started invocation is missing from the journal".into())
    })?;

  // Every run on an EXISTING thread begins with the history folded from
  // the journal (through this run's own user message) — the run stream is
  // self-contained, and tool-call results are present by construction.
  let snapshot = (!started.created).then(|| fold_messages(&history[..=idx]));

  let ctx = RunContext {
    thread_id: input.thread_id.clone(),
    run_id: input.run_id.clone(),
    input: Some(echo),
    snapshot,
  };
  stream_invocation(state, scope, started.session_id, &history, idx, ctx).await
}

/// One invocation's AG-UI stream, shared by runs and connect: the journal
/// tail from `history[idx]` (that invocation's `INVOCATION_STARTED`)
/// through its terminal event, projected with `ctx`.
async fn stream_invocation(
  state: &AgUiState,
  scope: RequestScope,
  session_id: SessionId,
  history: &[Event],
  idx: usize,
  ctx: RunContext,
) -> Result<Response, ServiceError> {
  // SESSION_CREATED is seq 0, so an invocation never starts the journal.
  let prev = history
    .get(idx.wrapping_sub(1))
    .ok_or_else(|| ServiceError::Internal("invocation has no preceding event".into()))?;

  // The tail resumes strictly after the event preceding INVOCATION_STARTED,
  // so the stream carries exactly this invocation's events (already
  // journaled ones replayed, the rest live) — via the service, so the
  // visibility check and the user-token lifetime cutoff apply.
  let events = state
    .service
    .session_events(session_id, Some(prev.id), scope)
    .await?;
  Ok(sse_response(project_stream(events, Projector::new(ctx))))
}

async fn fetch_history(
  state: &AgUiState,
  session_id: SessionId,
) -> Result<Vec<Event>, ServiceError> {
  state
    .journal
    .fetch(session_id, None)
    .await
    .map_err(|e| ServiceError::Internal(e.to_string()))
}

#[derive(Debug, Deserialize)]
struct ConnectQuery {
  #[serde(rename = "threadId")]
  thread_id: Option<String>,
}

/// `GET /ag-ui/{app}/{route}?threadId=` — connect. Re-streams the
/// thread's latest run: `RUN_STARTED` (the original run id), a
/// `MESSAGES_SNAPSHOT` through that run's `INVOCATION_STARTED`, then its
/// events, replayed and then live, to its terminal event. Read-only: starts
/// no turn, creates nothing, writes nothing. A thread that does not exist,
/// or has no turn yet, is an empty stream (CopilotKit connects on every page
/// load, fresh threads included).
async fn connect(
  State(state): State<AgUiState>,
  scope: Option<axum::Extension<RequestScope>>,
  Path((app, route)): Path<(String, String)>,
  query: Result<Query<ConnectQuery>, QueryRejection>,
) -> Response {
  let scope = scope
    .map(|axum::Extension(s)| s)
    .unwrap_or_else(RequestScope::unscoped);
  let thread_id = query
    .ok()
    .and_then(|Query(q)| q.thread_id)
    .and_then(|id| id.parse::<Uuid>().ok());
  let Some(thread_id) = thread_id else {
    return error_response(
      StatusCode::BAD_REQUEST,
      "threadId query parameter must be a UUID: it is the Sectr session id",
    );
  };
  let req = ThreadRef {
    app,
    route: format!("/{route}"),
    thread_id,
  };
  let session_id = match state.service.resolve_thread(req, scope.clone()).await {
    Ok(Some(id)) => id,
    Ok(None) => return empty_stream(),
    Err(e) => return e.into_response(),
  };
  let result = async {
    let history = fetch_history(&state, session_id).await?;
    match connect_context(&thread_id.to_string(), &history) {
      Some((idx, ctx)) => stream_invocation(&state, scope, session_id, &history, idx, ctx).await,
      None => Ok(empty_stream()),
    }
  }
  .await;
  result.unwrap_or_else(|e| e.into_response())
}

/// A `200` SSE response with no events: connect on a thread with no run.
fn empty_stream() -> Response {
  sse_response(futures::stream::empty())
}

struct RunStream {
  events: EventStream,
  projector: Projector,
  queue: VecDeque<AgUiEvent>,
  finished: bool,
}

/// Journal tail → AG-UI SSE frames. Ends after the projector's terminal
/// event; a tail that ends or fails first still gets a terminal `RUN_ERROR`
/// (token expiry cuts the tail; the client re-mints and re-sends).
fn project_stream(
  events: EventStream,
  projector: Projector,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send {
  futures::stream::unfold(
    RunStream {
      events,
      projector,
      queue: VecDeque::new(),
      finished: false,
    },
    |mut s| async move {
      loop {
        if let Some(event) = s.queue.pop_front() {
          return Some((Ok(frame(event)), s));
        }
        if s.finished {
          return None;
        }
        let produced = match s.events.next().await {
          Some(Ok(event)) => s.projector.push(&event),
          Some(Err(e)) => {
            tracing::warn!(error = %e, "ag-ui: journal tail error; ending run");
            s.projector
              .abort("the journal read failed; retry the run", "journal_error")
          }
          None => s.projector.abort(
            "the run stream ended before the run finished (the token may have expired)",
            "stream_ended",
          ),
        };
        s.queue.extend(produced);
        s.finished = s.projector.is_done();
      }
    },
  )
}

fn frame(event: AgUiEvent) -> SseEvent {
  SseEvent::default().data(event.to_json())
}

fn sse_response<S>(stream: S) -> Response
where
  S: Stream<Item = Result<SseEvent, Infallible>> + Send + 'static,
{
  Sse::new(stream.boxed())
    .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
    .into_response()
}
