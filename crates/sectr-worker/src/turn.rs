//! The turn-lifecycle core: execute ONE invocation against a runner and
//! journal the validated stream. The engine takes an
//! [`TurnAssignment`] + `dyn EventSink` and NOTHING else — it never touches
//! the Registry, the sandbox, or the public API. All of that is the
//! **server-role layer** (sectr-server in prod, CLI dispatch in dev), which:
//!
//! - journals `INVOCATION_STARTED` (and, on resume, the
//!   `TOOL_APPROVAL_GRANTED`/`DENIED` decision) BEFORE building the
//!   assignment — so the transcript the engine receives already includes
//!   this invocation's `INVOCATION_STARTED`, exactly as the invoke contract
//!   requires ("appended to the journal before the transcript is fetched");
//! - claims/releases the lease and refreshes it ingest-driven (prod: on the
//!   server's ingest path; dev: by wrapping the sink handed to the engine);
//! - spawns the runner and waits for readiness (dev: port poll; prod: WS
//!   `ready` frame) BEFORE calling [`TurnEngine::run`];
//! - acts on the [`TurnResult`]: release lease, tear down the user process,
//!   report completion (WS `complete` frame in prod).
//!
//! What the engine owns here: building the invoke request, driving the
//! stream through the validator ([`super::invoke::StreamValidator`]),
//! journaling its directives, enforcing the turn deadline, and mapping
//! everything to a [`TurnResult`]. Two failure disciplines:
//!
//! - **Ingest failures are fatal and never synthesized**: if the sink
//!   errors, the journal cannot record anything (including a SESSION_ERROR),
//!   so the engine stops immediately with [`TurnResult::IngestFailed`] and
//!   lets the caller decide (prod: the WS sink retries internally first —
//!   only a persistently failing ingest reaches here).
//! - **Runner-side failures synthesize the turn end**: unreachable runner,
//!   non-2xx, connection reset, contract violation, turn timeout — all go
//!   through the validator's transport-failure path, which journals
//!   `SESSION_ERROR` + `NO_MORE_ACTIONS` (the journal must always see a
//!   clean turn end).
//!
//! After a conclusion the response stream is dropped mid-flight if the
//! runner keeps sending — notably, an SDK runner's own sentinel after ERROR
//! is deliberately never read (the engine already synthesized one).

use futures::StreamExt;
use sectr_common::events::{InvocationRequest, Transcript};
use sectr_common::journal::{DeploymentId, EventSink, SessionId};

use crate::invoke::{
  Directive, Failure, InvokeClient, InvokeError, StreamItem, StreamValidator, TurnOutcome,
  TurnStream,
};

/// Everything the engine needs to execute one invocation. Built by the
/// server-role layer; `transcript` MUST already include this invocation's
/// `INVOCATION_STARTED` (journalled by the caller before the transcript is
/// fetched — see the module docs).
#[derive(Debug, Clone)]
pub struct TurnAssignment {
  pub session_id: SessionId,
  pub deployment_id: DeploymentId,
  pub invocation_id: uuid::Uuid,
  pub route: String,
  /// Mirrors the INVOCATION_STARTED event's reason (user_message /
  /// approval_decision / tool_result).
  pub reason: sectr_common::events::InvocationReason,
  /// Full transcript INCLUDING this invocation's INVOCATION_STARTED.
  pub transcript: Transcript,
  /// Base URL of the runner's invoke server, `http://127.0.0.1:$SECTR_PORT`
  /// in both modes (the sidecar dials localhost; dev runs in-process).
  pub runner_base: String,
}

/// How a turn ended. Journal contents for each case are pinned by tests —
/// the caller (server role) maps this onto lease release + teardown +
/// completion reporting without re-deriving anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnResult {
  /// `NO_MORE_ACTIONS` was journaled — the turn is over. `errored` is true
  /// when it followed a runner `ERROR` (sentinel synthesized by the
  /// engine).
  Completed { errored: bool },
  /// `TOOL_APPROVAL_REQUIRED` is the final journaled event; the turn is
  /// suspended awaiting an approval decision (resume = fresh invocation
  /// with `reason: approval_decision`).
  AwaitingApproval,
  /// The turn failed; `SESSION_ERROR` (this message) + `NO_MORE_ACTIONS`
  /// were journaled.
  Failed { message: String },
  /// A journal append failed mid-turn; the display string of the
  /// [`JournalError`] is carried (the error itself is not clonable).
  /// Nothing was synthesized — the journal is unavailable; the caller
  /// escalates (prod: drop the WS and let the server reconcile via lease
  /// expiry).
  IngestFailed(String),
}

/// Executes turns against runners. Cheap to clone/share.
///
/// **No turn deadline by design** (decided 2026-09-09, after the whole-turn
/// timeout was challenged and removed): a turn runs exactly as long as its
/// runner keeps producing events; there is no max-turn-duration concept
/// anywhere. Liveness is single-mechanism — the lease, refreshed
/// ingest-driven by the server role. A stalled runner stops refreshing and
/// its lease expires; the **reconciler** (server role, mandatory) is the
/// sole synthesizer of abandoned-turn ends (`SESSION_ERROR` + sentinel) on
/// reclaim. This engine task ends naturally in that case: the sink starts
/// rejecting appends for the reclaimed lease → [`TurnResult::IngestFailed`]
/// → `run` returns and the task ends. (Prod: the server rejects appends on
/// an expired/unclaimed lease; dev: the reconciler releases the lease and
/// aborts the dispatch task.)
pub struct TurnEngine {
  client: InvokeClient,
}

impl Default for TurnEngine {
  fn default() -> Self {
    Self::new()
  }
}

impl TurnEngine {
  pub fn new() -> Self {
    TurnEngine {
      client: InvokeClient::new(),
    }
  }

  /// Execute one invocation turn: POST the assignment to the runner,
  /// validate the response stream, and journal it via `sink`. Returns the
  /// turn's outcome; journal contents are guaranteed well-formed (a clean
  /// turn end or suspension) unless [`TurnResult::IngestFailed`].
  pub async fn run(&self, sink: &dyn EventSink, assignment: TurnAssignment) -> TurnResult {
    let request = InvocationRequest {
      session_id: assignment.session_id,
      invocation_id: assignment.invocation_id,
      route: assignment.route.clone(),
      transcript: assignment.transcript.clone(),
      reason: assignment.reason.clone(),
      extra: Default::default(),
    };

    let stream = match self.client.invoke(&assignment.runner_base, &request).await {
      Ok(stream) => stream,
      // Unreachable runner / non-2xx: nothing streamed, but the
      // journal still needs a well-formed failed turn. Feed the
      // failure through the validator so synthesis has one code path.
      Err(e) => {
        return self
          .conclude_failure(sink, &assignment, invoke_failure(e))
          .await;
      }
    };

    self.drive(sink, assignment, stream).await
  }

  /// Drive the stream through the validator, journaling as directed,
  /// until the turn concludes or ingestion fails. No deadline: a stream
  /// that stays silent is the LEASE's problem (see the struct docs) —
  /// the reconciler ends the abandoned turn, and this task exits via
  /// IngestFailed when appends start being rejected for the reclaimed
  /// lease (or the dev reconciler aborts the task outright).
  async fn drive(
    &self,
    sink: &dyn EventSink,
    assignment: TurnAssignment,
    mut stream: TurnStream,
  ) -> TurnResult {
    let mut validator = StreamValidator::new();
    loop {
      let item = match stream.next().await {
        Some(item) => item,
        None => StreamItem::Ended,
      };

      let directive = validator
        .feed(item)
        .expect("engine feeds the validator only before conclusion");

      match directive {
        Directive::Append(event) => {
          if let Err(e) = sink
            .append(assignment.session_id, assignment.deployment_id, vec![event])
            .await
          {
            return TurnResult::IngestFailed(e.to_string());
          }
        }
        Directive::Conclude(outcome, events) => {
          // An empty concluding set (clean suspension) appends
          // nothing; a non-empty one (sentinel / synthesized
          // failure) is one atomic append.
          if !events.is_empty()
            && let Err(e) = sink
              .append(assignment.session_id, assignment.deployment_id, events)
              .await
          {
            return TurnResult::IngestFailed(e.to_string());
          }
          // Drop the response stream: an SDK runner's sentinel
          // after ERROR is deliberately never read.
          drop(stream);
          return TurnResult::from(outcome);
        }
      }
    }
  }

  /// End the turn of a runner that failed to start (the sidecar's spawn):
  /// `SESSION_ERROR` with `message`, then `NO_MORE_ACTIONS`, as for any
  /// runner failure. Nothing is invoked.
  pub async fn fail_startup(
    &self,
    sink: &dyn EventSink,
    assignment: &TurnAssignment,
    message: String,
  ) -> TurnResult {
    self
      .conclude_failure(sink, assignment, Failure::Startup { message })
      .await
  }

  /// Journal a failure synthesized outside the stream loop (request-level
  /// errors) through the same validator path as in-stream failures.
  async fn conclude_failure(
    &self,
    sink: &dyn EventSink,
    assignment: &TurnAssignment,
    failure: Failure,
  ) -> TurnResult {
    let mut validator = StreamValidator::new();
    let directive = validator
      .feed(StreamItem::Failed(failure))
      .expect("fresh validator cannot be concluded");
    let Directive::Conclude(outcome, events) = directive else {
      unreachable!("failure always concludes");
    };
    if let Err(e) = sink
      .append(assignment.session_id, assignment.deployment_id, events)
      .await
    {
      return TurnResult::IngestFailed(e.to_string());
    }
    TurnResult::from(outcome)
  }
}

impl From<TurnOutcome> for TurnResult {
  fn from(outcome: TurnOutcome) -> Self {
    match outcome {
      TurnOutcome::Ended { errored } => TurnResult::Completed { errored },
      TurnOutcome::Suspended => TurnResult::AwaitingApproval,
      TurnOutcome::Failed(failure) => TurnResult::Failed {
        message: failure.message(),
      },
    }
  }
}

fn invoke_failure(e: InvokeError) -> Failure {
  Failure::Transport {
    detail: e.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::body::Body;
  use axum::routing::post;
  use sectr_common::events::{
    Error, Event, EventPayload, EventType, InvocationReason, MessageStart, NoMoreActions,
    RunnerEvent,
  };
  use sectr_common::journal::{JournalReader, SqliteJournal};
  use sectr_common::store::SqliteStore;
  use std::time::Duration;

  // ---- harness -------------------------------------------------------

  struct World {
    journal: SqliteJournal,
    session: SessionId,
    deployment: DeploymentId,
  }

  async fn world() -> World {
    let path = std::env::temp_dir()
      .join(format!("sectr-turn-test-{}.db", uuid::Uuid::now_v7()))
      .to_string_lossy()
      .into_owned();
    let store = SqliteStore::open(&path).await.unwrap();
    let session = uuid::Uuid::now_v7();
    let deployment = seed_session(store.pool(), session).await;
    World {
      journal: store.journal(),
      session,
      deployment,
    }
  }

  /// Minimal registry rows so `events.session_id` FK resolves (same
  /// pattern as the journal tests in sectr-common).
  async fn seed_session(pool: &sqlx::SqlitePool, session: SessionId) -> DeploymentId {
    let org = uuid::Uuid::now_v7();
    let app = uuid::Uuid::now_v7();
    let deployment = uuid::Uuid::now_v7();
    let ts = chrono::Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO orgs (id, name, slug, created_at) VALUES (?, ?, ?, ?)")
      .bind(org)
      .bind("test-org")
      .bind("test-org")
      .bind(&ts)
      .execute(pool)
      .await
      .unwrap();
    sqlx::query("INSERT INTO apps (id, org_id, name, slug, created_at) VALUES (?, ?, ?, ?, ?)")
      .bind(app)
      .bind(org)
      .bind("test-app")
      .bind("test-app")
      .bind(&ts)
      .execute(pool)
      .await
      .unwrap();
    sqlx::query(
      "INSERT INTO deployments (id, app_id, version, manifest, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(deployment)
    .bind(app)
    .bind(1i64)
    .bind("{}")
    .bind(&ts)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
      "INSERT INTO sessions (id, app_id, deployment_id, route, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(session)
    .bind(app)
    .bind(deployment)
    .bind("/test")
    .bind(&ts)
    .execute(pool)
    .await
    .unwrap();
    deployment
  }

  impl World {
    /// The server-role prelude: journal INVOCATION_STARTED, fetch the
    /// transcript (which now includes it), build the assignment.
    async fn assignment(&self, reason: InvocationReason) -> TurnAssignment {
      let started = self
        .journal
        .append_platform_event(
          self.session,
          self.deployment,
          EventPayload::InvocationStarted(sectr_common::events::InvocationStarted {
            invocation_id: uuid::Uuid::now_v7(),
            route: "/test".into(),
            deployment_id: self.deployment,
            reason: reason.clone(),
            extra: Default::default(),
          }),
        )
        .await
        .unwrap();
      TurnAssignment {
        session_id: self.session,
        deployment_id: self.deployment,
        invocation_id: started_seq_started_invocation(&started),
        route: "/test".into(),
        reason,
        transcript: vec![started],
        runner_base: String::new(), // set per-test
      }
    }

    async fn journal_types(&self) -> Vec<EventType> {
      self
        .journal
        .fetch(self.session, None)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event_type)
        .collect()
    }

    async fn journal_events(&self) -> Vec<Event> {
      self.journal.fetch(self.session, None).await.unwrap()
    }
  }

  fn started_seq_started_invocation(e: &Event) -> uuid::Uuid {
    match &e.payload {
      EventPayload::InvocationStarted(s) => s.invocation_id,
      other => panic!("expected InvocationStarted, got {other:?}"),
    }
  }

  fn user_message() -> InvocationReason {
    InvocationReason::UserMessage {
      input: "hi".to_string().try_into().unwrap(),
      extra: Default::default(),
    }
  }

  fn frame(payload: EventPayload) -> String {
    RunnerEvent::new(payload).unwrap().to_json_string()
  }

  fn delta(text: &str) -> EventPayload {
    EventPayload::MessageDelta(sectr_common::events::MessageDelta {
      message_id: "m1".into(),
      delta: sectr_common::events::TypedDelta::text(text),
      extra: Default::default(),
    })
  }

  fn sse(frames: &[String]) -> Body {
    let payload: Vec<String> = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
    Body::from(payload.concat())
  }

  async fn spawn_runner(routes: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
      axum::serve(listener, routes).await.unwrap();
    });
    (format!("http://{addr}"), handle)
  }

  /// An EventSink whose appends always fail — for the ingest-failure path.
  #[derive(Default)]
  struct BrokenSink;

  #[async_trait::async_trait]
  impl EventSink for BrokenSink {
    async fn append(
      &self,
      _session: SessionId,
      _deployment_id: DeploymentId,
      _events: Vec<sectr_common::events::NewEvent>,
    ) -> sectr_common::journal::Result<Vec<Event>> {
      Err(sectr_common::journal::JournalError::Conflict(
        "journal unavailable".into(),
      ))
    }
  }

  // ---- tests ---------------------------------------------------------

  #[tokio::test]
  async fn happy_path_journals_the_whole_turn() {
    let w = world().await;
    let mut assignment = w.assignment(user_message()).await;
    let (base, server) = spawn_runner(axum::Router::new().route(
      "/invocations",
      post(|| async {
        sse(&[
          frame(EventPayload::MessageStart(MessageStart {
            role: "assistant".into(),
            message_id: "m1".into(),
            extra: Default::default(),
          })),
          frame(delta("hello")),
          frame(EventPayload::MessageEnd(sectr_common::events::MessageEnd {
            message_id: "m1".into(),
            finish_reason: Some("stop".into()),
            framework_state: None,
            extra: Default::default(),
          })),
          frame(EventPayload::NoMoreActions(NoMoreActions::default())),
        ])
      }),
    ))
    .await;
    assignment.runner_base = base;

    let engine = TurnEngine::new();
    let result = engine.run(&w.journal, assignment).await;
    server.abort();

    assert_eq!(result, TurnResult::Completed { errored: false });
    assert_eq!(
      w.journal_types().await,
      vec![
        EventType::InvocationStarted,
        EventType::MessageStart,
        EventType::MessageDelta,
        EventType::MessageEnd,
        EventType::NoMoreActions,
      ]
    );
    // Consecutive seqs, gap-free, including the prelude.
    let events = w.journal_events().await;
    for (i, e) in events.iter().enumerate() {
      assert_eq!(e.seq, i as i64);
    }
  }

  #[tokio::test]
  async fn approval_pause_suspends_without_sentinel() {
    let w = world().await;
    let mut assignment = w.assignment(user_message()).await;
    let (base, server) = spawn_runner(axum::Router::new().route(
      "/invocations",
      post(|| async {
        sse(&[frame(EventPayload::ToolApprovalRequired(
          sectr_common::events::ToolApprovalRequired {
            tool_call_id: "call_1".into(),
            name: "issue_refund".into(),
            arguments: serde_json::json!({}),
            reason: None,
            extra: Default::default(),
          },
        ))])
      }),
    ))
    .await;
    assignment.runner_base = base;

    let engine = TurnEngine::new();
    let result = engine.run(&w.journal, assignment).await;
    server.abort();

    assert_eq!(result, TurnResult::AwaitingApproval);
    // Suspended: approval is final, NO sentinel.
    assert_eq!(
      w.journal_types().await,
      vec![
        EventType::InvocationStarted,
        EventType::ToolApprovalRequired
      ]
    );
  }

  #[tokio::test]
  async fn runner_error_journals_error_and_one_synthesized_sentinel() {
    let w = world().await;
    let mut assignment = w.assignment(user_message()).await;
    // SDK runners emit their OWN sentinel after ERROR — the engine must
    // never read it (it already synthesized one).
    let (base, server) = spawn_runner(axum::Router::new().route(
      "/invocations",
      post(|| async {
        sse(&[
          frame(EventPayload::Error(Error {
            message: "LLM provider 503".into(),
            code: None,
            transient: None,
            extra: Default::default(),
          })),
          frame(EventPayload::NoMoreActions(NoMoreActions::default())),
        ])
      }),
    ))
    .await;
    assignment.runner_base = base;

    let engine = TurnEngine::new();
    let result = engine.run(&w.journal, assignment).await;
    server.abort();

    assert_eq!(result, TurnResult::Completed { errored: true });
    assert_eq!(
      w.journal_types().await,
      vec![
        EventType::InvocationStarted,
        EventType::Error,
        EventType::NoMoreActions,
      ]
    );
  }

  #[tokio::test]
  async fn contract_violation_journals_session_error_not_offending_frame() {
    let w = world().await;
    let mut assignment = w.assignment(user_message()).await;
    // MESSAGE_DELTA after TOOL_APPROVAL_REQUIRED: finality violation.
    let (base, server) = spawn_runner(axum::Router::new().route(
      "/invocations",
      post(|| async {
        sse(&[
          frame(EventPayload::ToolApprovalRequired(
            sectr_common::events::ToolApprovalRequired {
              tool_call_id: "call_1".into(),
              name: "issue_refund".into(),
              arguments: serde_json::json!({}),
              reason: None,
              extra: Default::default(),
            },
          )),
          frame(delta("late")),
        ])
      }),
    ))
    .await;
    assignment.runner_base = base;

    let engine = TurnEngine::new();
    let result = engine.run(&w.journal, assignment).await;
    server.abort();

    let TurnResult::Failed { message } = result else {
      panic!("expected Failed, got {result:?}");
    };
    assert!(message.contains("violated the stream contract"));
    let events = w.journal_events().await;
    assert_eq!(
      events.iter().map(|e| e.event_type).collect::<Vec<_>>(),
      vec![
        EventType::InvocationStarted,
        EventType::ToolApprovalRequired,
        EventType::SessionError,
        EventType::NoMoreActions,
      ]
    );
    // The late delta was NOT journaled.
    assert!(
      events
        .iter()
        .all(|e| e.event_type != EventType::MessageDelta)
    );
  }

  #[tokio::test]
  async fn a_startup_failure_is_the_session_error() {
    let w = world().await;
    let assignment = w.assignment(user_message()).await;
    let message = "the runner exited during startup (exit code 1). The end of its stderr:\n\
                   ValueError: nope"
      .to_string();

    let result = TurnEngine::new()
      .fail_startup(&w.journal, &assignment, message.clone())
      .await;

    assert_eq!(
      result,
      TurnResult::Failed {
        message: message.clone()
      }
    );
    let events = w.journal_events().await;
    assert_eq!(
      events.iter().map(|e| e.event_type).collect::<Vec<_>>(),
      vec![
        EventType::InvocationStarted,
        EventType::SessionError,
        EventType::NoMoreActions,
      ]
    );
    let EventPayload::SessionError(error) = &events[1].payload else {
      unreachable!()
    };
    assert_eq!(error.message, message);
  }

  #[tokio::test]
  async fn connection_reset_journals_session_error() {
    let w = world().await;
    let mut assignment = w.assignment(user_message()).await;
    // Stream one valid frame, let it flush, THEN kill the body with an
    // IO error (simulates the runner process dying mid-stream — dying
    // before the first flush aborts the whole response and no frame is
    // delivered at all).
    let (base, server) = spawn_runner(axum::Router::new().route(
      "/invocations",
      post(|| async {
        Body::from_stream(futures::stream::unfold(0u8, |state| async move {
          match state {
            0 => Some((
              Ok::<_, std::io::Error>(axum::body::Bytes::from(format!(
                "data: {}\n\n",
                frame(delta("partial"))
              ))),
              1u8,
            )),
            1 => {
              // Let the frame flush to the wire before dying.
              tokio::time::sleep(Duration::from_millis(100)).await;
              Some((
                Err(std::io::Error::new(
                  std::io::ErrorKind::ConnectionAborted,
                  "runner died",
                )),
                2u8,
              ))
            }
            _ => None,
          }
        }))
      }),
    ))
    .await;
    assignment.runner_base = base;

    let engine = TurnEngine::new();
    let result = engine.run(&w.journal, assignment).await;
    server.abort();

    let TurnResult::Failed { message } = result else {
      panic!("expected Failed, got {result:?}");
    };
    assert!(
      message.contains("runner stream failed"),
      "message was: {message}"
    );
    assert_eq!(
      w.journal_types().await,
      vec![
        EventType::InvocationStarted,
        EventType::MessageDelta,
        EventType::SessionError,
        EventType::NoMoreActions,
      ]
    );
  }

  #[tokio::test]
  async fn unreachable_runner_journals_session_error() {
    let w = world().await;
    let mut assignment = w.assignment(user_message()).await;
    assignment.runner_base = "http://127.0.0.1:1".into();

    let engine = TurnEngine::new();
    let result = engine.run(&w.journal, assignment).await;

    let TurnResult::Failed { message } = result else {
      panic!("expected Failed, got {result:?}");
    };
    assert!(message.contains("runner unreachable"));
    assert_eq!(
      w.journal_types().await,
      vec![
        EventType::InvocationStarted,
        EventType::SessionError,
        EventType::NoMoreActions,
      ]
    );
  }

  #[tokio::test]
  async fn ingest_failure_is_fatal_and_unsynthesized() {
    let w = world().await;
    let mut assignment = w.assignment(user_message()).await;
    let (base, server) = spawn_runner(
      axum::Router::new().route("/invocations", post(|| async { sse(&[frame(delta("x"))]) })),
    )
    .await;
    assignment.runner_base = base;

    let engine = TurnEngine::new();
    let result = engine.run(&BrokenSink, assignment).await;
    server.abort();

    match result {
      TurnResult::IngestFailed(message) => {
        assert!(message.contains("journal unavailable"));
      }
      other => panic!("expected IngestFailed, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn non_200_runner_journals_session_error() {
    let w = world().await;
    let mut assignment = w.assignment(user_message()).await;
    let (base, server) = spawn_runner(axum::Router::new().route(
      "/invocations",
      post(|| async { (axum::http::StatusCode::BAD_GATEWAY, "runner not ready") }),
    ))
    .await;
    assignment.runner_base = base;

    let engine = TurnEngine::new();
    let result = engine.run(&w.journal, assignment).await;
    server.abort();

    let TurnResult::Failed { message } = result else {
      panic!("expected Failed, got {result:?}");
    };
    assert!(message.contains("502"));
    assert_eq!(
      w.journal_types().await,
      vec![
        EventType::InvocationStarted,
        EventType::SessionError,
        EventType::NoMoreActions,
      ]
    );
  }
}
