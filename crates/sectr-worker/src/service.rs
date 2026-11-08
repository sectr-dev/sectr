//! `DevSessionService` — the dev gateway's [`SessionService`] impl: the
//! dev server-role machinery (`DevDispatch` + SQLite journal/registry)
//! behind the shared sessions-api router.
//!
//! Process model: turns run as SPAWNED TASKS (the HTTP call returns as
//! soon as the turn is scheduled — clients follow the SSE journal tail;
//! `sessions-api.yaml`: "created; turn started"). In-flight tasks are
//! tracked so `sectr dev` can shut down turn-aware (drain, then exit).
//!
//! Session status is DERIVED from the journal at read time (see
//! `sectr_common::api::derive_session_status`) — the registry row's status
//! is lifecycle bookkeeping, not the API view. `origin` is always
//! `playground` in dev (no principals exist); prod stores it per principal
//! (Phase 5).
//!
//! Known limitation (accepted, same class as the reconciler TOCTOU): the
//! mid-turn check in `send_input` is best-effort — two racing sends on the
//! same session can both pass it; the loser's claim then conflicts inside
//! the dispatch (logged, no journal pollution — the decision ordering
//! contract keeps the journal clean), but its HTTP 202 was already sent.

use std::future::Future;
use std::sync::{Arc, Mutex};

use sectr_common::api::{
  ApprovalDecisionRequest, ListSessionsQuery, NewSessionRequest, PendingApproval, RequestScope,
  RunIntent, RunPlan, RunRequest, ServiceError, SessionInput, SessionService, SessionStatus,
  SessionSummary, StartedRun, ThreadRef, derive_session_status, derive_session_title, plan_run,
};
use sectr_common::events::Event;
use sectr_common::journal::{Journal, SessionId, awaiting_approval};
use sectr_common::registry::{Registry, RegistryError, Session};
use tokio::task::JoinHandle;

use crate::dispatch::{DevDispatch, TurnOpts};
use crate::turn::TurnResult;

/// Everything the service knows about the app it serves (from `sectr dev`'s
/// bootstrap).
#[derive(Debug, Clone)]
pub struct AppInfo {
  pub app_id: uuid::Uuid,
  pub app_name: String,
  pub deployment_id: uuid::Uuid,
  pub deployment_version: i64,
  /// The deployment manifest — route validation (`deployment_serves_route`).
  pub manifest: serde_json::Value,
}

pub struct DevSessionService {
  dispatch: Arc<DevDispatch>,
  /// Same instances as the dispatch's handles (taken from it in `new` —
  /// one wiring point; reads and writes cannot diverge onto different
  /// stores).
  registry: Arc<dyn Registry>,
  journal: Arc<dyn Journal>,
  app: AppInfo,
  /// In-flight turn tasks, drained on shutdown.
  turns: Mutex<Vec<JoinHandle<()>>>,
}

impl DevSessionService {
  /// Built FROM the dispatch: the store handles come from it, so the
  /// service and the dispatch share instances by construction (the
  /// pre-2026-09-11 constructor took parallel handles and nothing
  /// guaranteed they were the same).
  pub fn new(dispatch: Arc<DevDispatch>, app: AppInfo) -> Self {
    DevSessionService {
      registry: dispatch.registry().clone(),
      journal: dispatch.journal().clone(),
      dispatch,
      app,
      turns: Mutex::new(Vec::new()),
    }
  }

  /// Wait for all in-flight turns (graceful Ctrl-C: never kill a running
  /// turn — the journal must not be abandoned mid-write; the lease TTL +
  /// reconciler are the recovery path for a killed process anyway).
  pub async fn shutdown(&self) {
    // Take the handles out BEFORE awaiting: the std::sync::Mutex guard
    // must not be held across an await point.
    let handles = std::mem::take(&mut *self.turns.lock().unwrap());
    for handle in handles {
      let _ = handle.await;
    }
  }

  fn spawn_turn(
    &self,
    future: impl Future<
      Output = (
        SessionId,
        Result<TurnResult, crate::dispatch::DispatchError>,
      ),
    > + Send
    + 'static,
  ) {
    let handle = tokio::spawn(async move {
      let (session, result) = future.await;
      match result {
        Ok(TurnResult::Completed { errored: false }) => {}
        Ok(TurnResult::Completed { errored: true }) => {
          tracing::warn!(%session, "turn ended with a runner error")
        }
        Ok(TurnResult::AwaitingApproval) => {}
        Ok(TurnResult::Failed { message }) => {
          tracing::warn!(%session, "turn failed: {message}")
        }
        Ok(TurnResult::IngestFailed(e)) => {
          tracing::error!(%session, "journal unavailable: {e}")
        }
        Err(e) => tracing::error!(%session, "dispatch error: {e}"),
      }
    });
    self.turns.lock().unwrap().push(handle);
  }

  async fn summary(&self, session: Session) -> Result<SessionSummary, ServiceError> {
    let events = self
      .journal
      .fetch(session.id, None)
      .await
      .map_err(internal)?;
    Ok(derive_summary(&session, &events))
  }

  async fn summary_with_events(
    &self,
    session: Session,
  ) -> Result<(SessionSummary, Vec<Event>), ServiceError> {
    let events = self
      .journal
      .fetch(session.id, None)
      .await
      .map_err(internal)?;
    let summary = derive_summary(&session, &events);
    Ok((summary, events))
  }

  /// The AG-UI target checks shared by runs and connect (same as
  /// create_session): this gateway's one app, a route in its manifest.
  fn check_run_target(&self, app: &str, route: &str) -> Result<(), ServiceError> {
    if app != self.app.app_name && app != self.app.app_id.to_string() {
      return Err(ServiceError::NotFound(format!(
        "unknown app {:?} (this dev gateway serves {:?})",
        app, self.app.app_name
      )));
    }
    if !sectr_common::registry::deployment_serves_route(&self.app.manifest, route) {
      let available = sectr_common::registry::manifest_routes(&self.app.manifest);
      return Err(ServiceError::NotFound(format!(
        "unknown route {:?} for app {:?}; available routes: {}",
        route,
        self.app.app_name,
        if available.is_empty() {
          "(none)".to_string()
        } else {
          available.join(", ")
        }
      )));
    }
    Ok(())
  }
}

/// A thread belongs to its route: an AG-UI request on another is a 409.
fn check_thread_route(session: &Session, route: &str) -> Result<(), ServiceError> {
  if session.route != route {
    return Err(ServiceError::Conflict(format!(
      "thread {} belongs to route {:?}, not {:?}",
      session.id, session.route, route
    )));
  }
  Ok(())
}

fn derive_summary(session: &Session, events: &[Event]) -> SessionSummary {
  SessionSummary {
    id: session.id,
    app_id: session.app_id,
    route: session.route.clone(),
    origin: session.origin,
    deployment_id: session.deployment_id,
    status: derive_session_status(events),
    created_at: session.created_at,
    last_event_at: events.last().map(|e| e.created_at),
    event_count: events.len() as i64,
    title: derive_session_title(events),
    external_user_id: session.external_user_id.clone(),
  }
}

#[async_trait::async_trait]
impl SessionService for DevSessionService {
  async fn create_session(
    &self,
    req: NewSessionRequest,
    _scope: RequestScope,
  ) -> Result<SessionId, ServiceError> {
    // The dev gateway serves exactly ONE app (the local project's).
    if req.app != self.app.app_name && req.app != self.app.app_id.to_string() {
      return Err(ServiceError::NotFound(format!(
        "unknown app {:?} (this dev gateway serves {:?})",
        req.app, self.app.app_name
      )));
    }
    if !sectr_common::registry::deployment_serves_route(&self.app.manifest, &req.route) {
      return Err(ServiceError::NotFound(format!(
        "deployment does not serve route {:?}",
        req.route
      )));
    }
    let session = self
      .dispatch
      .create_session(&req.route)
      .await
      .map_err(internal)?;
    let dispatch = self.dispatch.clone();
    self.spawn_turn(async move {
      let result = dispatch.send_message(session, req.input).await;
      (session, result)
    });
    Ok(session)
  }

  async fn list_sessions(
    &self,
    query: ListSessionsQuery,
    _scope: RequestScope,
  ) -> Result<Vec<SessionSummary>, ServiceError> {
    let app_id = query
      .app_id
      .as_deref()
      .map(|app| {
        if app == self.app.app_name {
          Ok(self.app.app_id)
        } else {
          app
            .parse::<uuid::Uuid>()
            .map_err(|_| ServiceError::NotFound(format!("unknown app {app:?}")))
        }
      })
      .transpose()?;
    let rows = self
      .registry
      .list_sessions(
        None,
        app_id,
        query.external_user_id.as_deref(),
        query.limit.unwrap_or(50),
        query.created_before,
      )
      .await
      .map_err(internal)?;
    let mut summaries = Vec::with_capacity(rows.len());
    for session in rows {
      let (summary, _) = self.summary_with_events(session).await?;
      if let Some(status) = query.status
        && summary.status != status
      {
        continue;
      }
      summaries.push(summary);
    }
    Ok(summaries)
  }

  async fn get_session(
    &self,
    id: SessionId,
    _scope: RequestScope,
  ) -> Result<SessionSummary, ServiceError> {
    let session = self.registry.get_session(id).await.map_err(not_found)?;
    self.summary(session).await
  }

  async fn send_input(
    &self,
    id: SessionId,
    req: SessionInput,
    _scope: RequestScope,
  ) -> Result<(), ServiceError> {
    self.registry.get_session(id).await.map_err(not_found)?;
    if self
      .registry
      .live_lease(id)
      .await
      .map_err(internal)?
      .is_some()
    {
      return Err(ServiceError::Conflict(
        "session is mid-turn; wait for the current turn to finish".into(),
      ));
    }
    let events = self.journal.fetch(id, None).await.map_err(internal)?;
    match sectr_common::api::derive_session_status(&events) {
      SessionStatus::AwaitingApproval => {
        return Err(ServiceError::Conflict(
          "session is awaiting an approval decision — use the approval endpoint".into(),
        ));
      }
      SessionStatus::Ended => {
        return Err(ServiceError::Conflict("session has ended".into()));
      }
      _ => {}
    }
    let dispatch = self.dispatch.clone();
    self.spawn_turn(async move {
      let result = dispatch.send_message(id, req.input).await;
      (id, result)
    });
    Ok(())
  }

  async fn decide_approval(
    &self,
    id: SessionId,
    req: ApprovalDecisionRequest,
    _scope: RequestScope,
  ) -> Result<(), ServiceError> {
    self.registry.get_session(id).await.map_err(not_found)?;
    let events = self.journal.fetch(id, None).await.map_err(internal)?;
    if awaiting_approval(&events).is_none() {
      return Err(ServiceError::Conflict(
        "no pending approval on this session".into(),
      ));
    }
    let dispatch = self.dispatch.clone();
    self.spawn_turn(async move {
      let result = dispatch
        .resume_with_decision(
          id,
          sectr_common::events::ApprovalDecision {
            approved: req.approved,
            approver: req.approver,
            extra: Default::default(),
          },
        )
        .await;
      (id, result)
    });
    Ok(())
  }

  async fn pending_approvals(
    &self,
    _scope: RequestScope,
  ) -> Result<Vec<PendingApproval>, ServiceError> {
    let rows = self
      .registry
      .list_sessions(None, None, None, 200, None)
      .await
      .map_err(internal)?;
    let mut pending = Vec::new();
    for session in rows {
      let (summary, events) = self.summary_with_events(session).await?;
      let Some(event) = awaiting_approval(&events).cloned() else {
        continue;
      };
      pending.push(PendingApproval {
        session: summary,
        event,
      });
    }
    Ok(pending)
  }

  async fn start_run(
    &self,
    req: RunRequest,
    _scope: RequestScope,
  ) -> Result<StartedRun, ServiceError> {
    self.check_run_target(&req.app, &req.route)?;

    // Thread resolution: the threadId IS the session id; created on the
    // first user-message run. Dev has no principals, so no ownership.
    let (session, created) = match self.registry.get_session(req.thread_id).await {
      Ok(session) => {
        check_thread_route(&session, &req.route)?;
        (session.id, false)
      }
      Err(RegistryError::NotFound(_)) => {
        req.intent.check_creates_thread(req.thread_id)?;
        (
          self
            .dispatch
            .create_session_with_id(&req.route, Some(req.thread_id))
            .await
            .map_err(internal)?,
          true,
        )
      }
      Err(e) => return Err(internal(e)),
    };

    // The shared busy-thread / interrupt decision (same rule as prod).
    let plan = if created {
      RunPlan::StartTurn
    } else {
      let live = self.registry.live_lease(session).await.map_err(internal)?;
      let events = self.journal.fetch(session, None).await.map_err(internal)?;
      plan_run(&events, live.as_ref(), &req.intent)?
    };

    let invocation_id = uuid::Uuid::now_v7();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let opts = TurnOpts {
      invocation_id: Some(invocation_id),
      extra: Default::default(),
      started: Some(started_tx),
    };
    let dispatch = self.dispatch.clone();
    match (plan, req.intent) {
      (
        RunPlan::StartTurn,
        RunIntent::UserMessage {
          input,
          client_message_id,
          forwarded_props,
          context,
          ..
        },
      ) => {
        let opts = TurnOpts {
          extra: sectr_common::agui::invocation_extra(
            &req.run_id,
            client_message_id,
            forwarded_props,
            context,
          ),
          ..opts
        };
        self.spawn_turn(async move {
          let result = dispatch.send_message_with(session, input, opts).await;
          (session, result)
        });
      }
      (RunPlan::ResumeApproval, RunIntent::ApprovalDecision { approved, .. }) => {
        let opts = TurnOpts {
          extra: sectr_common::agui::invocation_extra(&req.run_id, None, None, None),
          ..opts
        };
        self.spawn_turn(async move {
          let result = dispatch
            .resume_with_decision_with(
              session,
              sectr_common::events::ApprovalDecision {
                approved,
                approver: None,
                extra: Default::default(),
              },
              opts,
            )
            .await;
          (session, result)
        });
      }
      _ => {
        return Err(ServiceError::Internal(
          "run plan does not match the run intent".into(),
        ));
      }
    }

    // The turn runs as a spawned task; the run stream needs the journaled
    // INVOCATION_STARTED, so wait for it (or for the start to fail — a
    // lost lease race is a 409, like any busy thread).
    match started_rx.await {
      Ok(Ok(())) => Ok(StartedRun {
        session_id: session,
        invocation_id,
        created,
      }),
      Ok(Err(message)) => Err(ServiceError::Conflict(message)),
      Err(_) => Err(ServiceError::Internal(
        "the turn task ended before it started".into(),
      )),
    }
  }

  async fn resolve_thread(
    &self,
    req: ThreadRef,
    _scope: RequestScope,
  ) -> Result<Option<SessionId>, ServiceError> {
    // The run path's checks, read-only: never creates the thread.
    self.check_run_target(&req.app, &req.route)?;
    match self.registry.get_session(req.thread_id).await {
      Ok(session) => {
        check_thread_route(&session, &req.route)?;
        Ok(Some(session.id))
      }
      Err(RegistryError::NotFound(_)) => Ok(None),
      Err(e) => Err(internal(e)),
    }
  }

  async fn session_events(
    &self,
    id: SessionId,
    last_event_id: Option<uuid::Uuid>,
    _scope: RequestScope,
  ) -> Result<sectr_common::journal::EventStream, ServiceError> {
    self.registry.get_session(id).await.map_err(not_found)?;
    let after_seq = match last_event_id {
      None => None,
      Some(event_id) => {
        // Resolve the event id → seq: replay strictly after it.
        // (O(transcript) per reconnect — fine for dev; prod can
        // index by id.)
        let events = self.journal.fetch(id, None).await.map_err(internal)?;
        let seq = events
          .iter()
          .find(|e| e.id == event_id)
          .map(|e| e.seq)
          .ok_or_else(|| {
            ServiceError::BadRequest(format!(
              "last_event_id {event_id} is not an event of this session"
            ))
          })?;
        Some(seq)
      }
    };
    self
      .journal
      .subscribe(id, after_seq)
      .await
      .map_err(internal)
  }
}

fn internal(e: impl std::fmt::Display) -> ServiceError {
  ServiceError::Internal(e.to_string())
}

fn not_found(e: impl std::fmt::Display) -> ServiceError {
  if e.to_string().contains("not found") {
    ServiceError::NotFound(e.to_string())
  } else {
    ServiceError::Internal(e.to_string())
  }
}
// ---------------------------------------------------------------------------
// Tests — the service over the real journal/registry + the fake runner
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dispatch::DispatchConfig;
  use crate::testing::{
    approval_frame, delta_frame, fake_runner_command, final_frame, script_turn,
  };
  use crate::turn::TurnEngine;
  use sectr_common::api::UserMessageInput;
  use sectr_common::drivers::{InMemorySecretSource, SubprocessDriver};
  use sectr_common::events::EventType;
  use sectr_common::journal::SqliteJournal;
  use sectr_common::registry::SqliteRegistry;
  use sectr_common::store::SqliteStore;
  use std::time::Duration;

  struct World {
    service: DevSessionService,
    registry: Arc<SqliteRegistry>,
    journal: Arc<SqliteJournal>,
    frames: std::path::PathBuf,
  }

  /// The fake runner (see `crate::testing`): a stdlib python HTTP server
  /// streaming the SSE frames listed in $SECTR_FRAMES_FILE (re-read per
  /// request, so later turns can be scripted by rewriting the file).
  async fn world() -> World {
    let path = std::env::temp_dir()
      .join(format!("sectr-service-{}.db", uuid::Uuid::now_v7()))
      .to_string_lossy()
      .into_owned();
    let store = Arc::new(SqliteStore::open(&path).await.unwrap());
    let org = store
      .registry()
      .create_org("test-org", "test-org")
      .await
      .unwrap();
    let app = store
      .registry()
      .create_app(sectr_common::registry::NewApp {
        org_id: org.id,
        name: "test-app".into(),
        slug: "test-app".into(),
      })
      .await
      .unwrap();
    let manifest = serde_json::json!({ "routes": [{ "path": "/test" }] });
    let deployment = store
      .registry()
      .create_deployment(sectr_common::registry::NewDeployment {
        endpoint_url: None,
        app_id: app.id,
        version: 1,
        manifest: manifest.clone(),
      })
      .await
      .unwrap();

    let journal = Arc::new(SqliteJournal::new(store.pool().clone()));
    let registry = Arc::new(SqliteRegistry::new(store.pool().clone()));
    // The frames file path goes through the runner env — the harness
    // owns the file name.
    let frames = std::env::temp_dir().join(format!("sectr-frames-{}.json", uuid::Uuid::now_v7()));
    let dispatch = Arc::new(DevDispatch::new(
      journal.clone() as Arc<dyn Journal>,
      registry.clone() as Arc<dyn Registry>,
      Arc::new(TurnEngine::new()),
      Arc::new(SubprocessDriver::new()),
      Arc::new(InMemorySecretSource::new()),
      DispatchConfig {
        lease_ttl: chrono::Duration::seconds(30),
        ready_deadline: Duration::from_secs(15),
        ready_poll_interval: Duration::from_millis(25),
        runner_command: fake_runner_command(),
        runner_workdir: None,
        runner_env: vec![(
          "SECTR_FRAMES_FILE".into(),
          frames.to_string_lossy().into_owned(),
        )],
        runner_port: None,
        approval_tools: vec![],
        env_file: None,
      },
      app.id,
      deployment.id,
    ));
    let service = DevSessionService::new(
      dispatch,
      AppInfo {
        app_id: app.id,
        app_name: "test-app".into(),
        deployment_id: deployment.id,
        deployment_version: 1,
        manifest,
      },
    );
    World {
      service,
      registry,
      journal,
      frames,
    }
  }

  /// Poll until the session's journal ends with `last_type` (turns run as
  /// spawned tasks).
  async fn wait_for(world: &World, session: uuid::Uuid, last_type: EventType) {
    use sectr_common::journal::JournalReader as _;
    for _ in 0..200 {
      let types: Vec<EventType> = world
        .journal
        .fetch(session, None)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.event_type)
        .collect();
      if types.last() == Some(&last_type) {
        return;
      }
      tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("turn did not reach {last_type:?} in time");
  }

  #[tokio::test]
  async fn create_starts_a_turn_and_serves_the_api_view() {
    let w = world().await;
    script_turn(&w.frames, &[delta_frame("hello"), final_frame()]);

    let session = w
      .service
      .create_session(
        NewSessionRequest {
          app: "test-app".into(),
          route: "/test".into(),
          input: UserMessageInput::new(serde_json::json!("hi")).unwrap(),
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap();
    wait_for(&w, session, EventType::NoMoreActions).await;

    let summary = w
      .service
      .get_session(session, RequestScope::unscoped())
      .await
      .unwrap();
    assert_eq!(summary.status, SessionStatus::Active);
    assert_eq!(summary.origin, sectr_common::api::Origin::Playground);
    assert_eq!(summary.event_count, 4); // CREATED, STARTED, DELTA, sentinel
    assert_eq!(summary.route, "/test");
    assert_eq!(summary.title.as_deref(), Some("hi"));

    let list = w
      .service
      .list_sessions(ListSessionsQuery::default(), RequestScope::unscoped())
      .await
      .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, session);

    // Filtering by a status nothing matches.
    let none = w
      .service
      .list_sessions(
        ListSessionsQuery {
          status: Some(SessionStatus::Error),
          ..Default::default()
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap();
    assert!(none.is_empty());
    w.service.shutdown().await;
  }

  #[tokio::test]
  async fn unknown_app_and_unknown_route_are_404() {
    let w = world().await;
    script_turn(&w.frames, &[final_frame()]);
    let err = w
      .service
      .create_session(
        NewSessionRequest {
          app: "other".into(),
          route: "/test".into(),
          input: UserMessageInput::new(serde_json::json!("hi")).unwrap(),
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap_err();
    assert!(matches!(err, ServiceError::NotFound(_)));
    let err = w
      .service
      .create_session(
        NewSessionRequest {
          app: "test-app".into(),
          route: "/nope".into(),
          input: UserMessageInput::new(serde_json::json!("hi")).unwrap(),
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap_err();
    assert!(matches!(err, ServiceError::NotFound(_)));
    w.service.shutdown().await;
  }

  #[tokio::test]
  async fn send_input_conflicts_while_a_lease_is_live() {
    let w = world().await;
    script_turn(&w.frames, &[delta_frame("slow"), final_frame()]);

    let session = w
      .service
      .create_session(
        NewSessionRequest {
          app: "test-app".into(),
          route: "/test".into(),
          input: UserMessageInput::new(serde_json::json!("hi")).unwrap(),
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap();
    // Let the creating turn finish and drop its own lease first: under load
    // it could otherwise claim the lease after we release ours below, and
    // the second send would rightly conflict with it.
    w.service.shutdown().await;

    // Simulate a mid-turn session: claim a lease directly.
    let lease = w
      .registry
      .claim_lease(session, "test-runner", None, chrono::Duration::seconds(30))
      .await
      .unwrap();
    let err = w
      .service
      .send_input(
        session,
        SessionInput {
          input: UserMessageInput::new(serde_json::json!("again")).unwrap(),
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap_err();
    assert!(matches!(err, ServiceError::Conflict(_)), "{err}");

    // Released → the send goes through (202 semantics are this Ok).
    w.registry.release_lease(lease.id).await.unwrap();
    w.service
      .send_input(
        session,
        SessionInput {
          input: UserMessageInput::new(serde_json::json!("again")).unwrap(),
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap();
    w.service.shutdown().await;
  }

  #[tokio::test]
  async fn approval_flow_through_the_service() {
    let w = world().await;
    script_turn(&w.frames, &[delta_frame("checking"), approval_frame()]);

    let session = w
      .service
      .create_session(
        NewSessionRequest {
          app: "test-app".into(),
          route: "/test".into(),
          input: UserMessageInput::new(serde_json::json!("refund 42")).unwrap(),
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap();
    wait_for(&w, session, EventType::ToolApprovalRequired).await;

    // Suspended: input is 409, the session shows in pending approvals.
    let err = w
      .service
      .send_input(
        session,
        SessionInput {
          input: UserMessageInput::new(serde_json::json!("hello?")).unwrap(),
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap_err();
    assert!(matches!(err, ServiceError::Conflict(_)), "{err}");
    let pending = w
      .service
      .pending_approvals(RequestScope::unscoped())
      .await
      .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].session.id, session);
    assert_eq!(pending[0].event.event_type, EventType::ToolApprovalRequired);

    // Deciding with nothing pending is 409 (wrong session id).
    let err = w
      .service
      .decide_approval(
        uuid::Uuid::now_v7(),
        ApprovalDecisionRequest {
          approved: true,
          approver: None,
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap_err();
    assert!(matches!(err, ServiceError::NotFound(_)));

    // Approve → fresh invocation → completed.
    script_turn(&w.frames, &[delta_frame("done"), final_frame()]);
    w.service
      .decide_approval(
        session,
        ApprovalDecisionRequest {
          approved: true,
          approver: Some("don".into()),
        },
        RequestScope::unscoped(),
      )
      .await
      .unwrap();
    wait_for(&w, session, EventType::NoMoreActions).await;
    let summary = w
      .service
      .get_session(session, RequestScope::unscoped())
      .await
      .unwrap();
    assert_eq!(summary.status, SessionStatus::Active);
    assert!(
      w.service
        .pending_approvals(RequestScope::unscoped())
        .await
        .unwrap()
        .is_empty()
    );
    w.service.shutdown().await;
  }

  // -------------------------------------------------------------------
  // AG-UI: the SAME endpoint as prod, served by the dev gateway
  // with no auth — over a real socket + the real fake-runner turn.
  // -------------------------------------------------------------------

  /// Serve the shared AG-UI router over `service` on an ephemeral port.
  async fn serve_agui(service: Arc<DevSessionService>, journal: Arc<SqliteJournal>) -> String {
    let router = sectr_common::api::agui::agui_router(service, journal)
      .layer(sectr_common::api::agui::agui_cors_layer());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
  }

  fn agui_body(thread: uuid::Uuid, run: &str, messages: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"threadId": thread.to_string(), "runId": run, "messages": messages})
  }

  /// POST a run and read its SSE stream to the end (it ends at the run's
  /// terminal event).
  async fn run_agui(
    url: &str,
    body: serde_json::Value,
  ) -> (reqwest::StatusCode, Vec<serde_json::Value>) {
    let response = reqwest::Client::new()
      .post(format!("{url}/ag-ui/test-app/test"))
      .json(&body)
      .send()
      .await
      .unwrap();
    let status = response.status();
    let text = tokio::time::timeout(Duration::from_secs(20), response.text())
      .await
      .expect("run stream did not end")
      .unwrap();
    let events = text
      .lines()
      .filter_map(|l| l.strip_prefix("data: "))
      .map(|d| serde_json::from_str(d).unwrap())
      .collect();
    (status, events)
  }

  #[tokio::test]
  async fn ag_ui_run_streams_a_dev_turn_without_auth() {
    let w = world().await;
    script_turn(&w.frames, &[delta_frame("hello"), final_frame()]);
    let (journal, frames) = (w.journal.clone(), w.frames.clone());
    let service = Arc::new(w.service);
    let url = serve_agui(service.clone(), journal.clone()).await;

    let thread = uuid::Uuid::now_v7();
    let (status, events) = run_agui(
      &url,
      agui_body(
        thread,
        "r1",
        serde_json::json!([{"id": "u1", "role": "user", "content": "hi"}]),
      ),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    // No Authorization header anywhere: dev has no principals.
    assert_eq!(events[0]["type"], "RUN_STARTED");
    assert_eq!(events[0]["threadId"], thread.to_string());
    assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
    assert_eq!(events.last().unwrap()["outcome"]["type"], "success");
    // The thread IS the dev session (visible through the sessions API too).
    let summary = service
      .get_session(thread, RequestScope::unscoped())
      .await
      .unwrap();
    assert_eq!(summary.route, "/test");

    // Second run on the thread: history snapshot first, own turn only.
    script_turn(&frames, &[delta_frame("again"), final_frame()]);
    let (_, events) = run_agui(
      &url,
      agui_body(
        thread,
        "r2",
        serde_json::json!([
          {"id": "u1", "role": "user", "content": "hi"},
          {"id": "u2", "role": "user", "content": "more"}
        ]),
      ),
    )
    .await;
    assert_eq!(events[1]["type"], "MESSAGES_SNAPSHOT");
    assert_eq!(events[1]["messages"][0]["id"], "u1");
    assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
    service.shutdown().await;
  }

  #[tokio::test]
  async fn ag_ui_approval_interrupts_and_resumes_in_dev() {
    use sectr_common::journal::JournalReader as _;
    let w = world().await;
    script_turn(&w.frames, &[approval_frame()]);
    let (journal, frames) = (w.journal.clone(), w.frames.clone());
    let service = Arc::new(w.service);
    let url = serve_agui(service.clone(), journal.clone()).await;

    let thread = uuid::Uuid::now_v7();
    let messages = serde_json::json!([{"id": "u1", "role": "user", "content": "refund"}]);
    let (_, events) = run_agui(&url, agui_body(thread, "r1", messages.clone())).await;
    let last = events.last().unwrap();
    assert_eq!(last["outcome"]["type"], "interrupt");
    let interrupt_id = last["outcome"]["interrupts"][0]["id"]
      .as_str()
      .unwrap()
      .to_owned();

    script_turn(&frames, &[delta_frame("done"), final_frame()]);
    let mut resume = agui_body(thread, "r2", messages);
    resume["resume"] =
      serde_json::json!([{"interruptId": interrupt_id, "status": "resolved", "payload": true}]);
    let (status, events) = run_agui(&url, resume).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(events[1]["type"], "MESSAGES_SNAPSHOT");
    assert_eq!(events.last().unwrap()["outcome"]["type"], "success");
    let journaled = journal.fetch(thread, None).await.unwrap();
    assert!(
      journaled
        .iter()
        .any(|e| e.event_type == EventType::ToolApprovalGranted)
    );
    // Each run's id is journaled on the invocation it started.
    let run_ids: Vec<_> = journaled
      .iter()
      .filter_map(|e| match &e.payload {
        sectr_common::events::EventPayload::InvocationStarted(i) => {
          Some(i.extra.map["ag_ui"]["run_id"].clone())
        }
        _ => None,
      })
      .collect();
    assert_eq!(run_ids, [serde_json::json!("r1"), serde_json::json!("r2")]);
    service.shutdown().await;
  }

  /// GET a connect and read its SSE stream to the end.
  async fn connect_agui(
    url: &str,
    app: &str,
    thread: uuid::Uuid,
  ) -> (reqwest::StatusCode, Vec<serde_json::Value>) {
    let response = reqwest::Client::new()
      .get(format!("{url}/ag-ui/{app}/test?threadId={thread}"))
      .send()
      .await
      .unwrap();
    let status = response.status();
    let text = tokio::time::timeout(Duration::from_secs(20), response.text())
      .await
      .expect("connect stream did not end")
      .unwrap();
    let events = text
      .lines()
      .filter_map(|l| l.strip_prefix("data: "))
      .map(|d| serde_json::from_str(d).unwrap_or(serde_json::Value::Null))
      .collect();
    (status, events)
  }

  #[tokio::test]
  async fn ag_ui_connect_is_served_in_dev_without_auth() {
    let w = world().await;
    script_turn(&w.frames, &[delta_frame("hello"), final_frame()]);
    let journal = w.journal.clone();
    let service = Arc::new(w.service);
    let url = serve_agui(service.clone(), journal).await;

    // A thread that doesn't exist yet: an empty stream, nothing created.
    let thread = uuid::Uuid::now_v7();
    let (status, events) = connect_agui(&url, "test-app", thread).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(events.is_empty(), "{events:?}");
    assert!(
      service
        .get_session(thread, RequestScope::unscoped())
        .await
        .is_err()
    );

    // After a run: the run is replayed under its own id, with history.
    run_agui(
      &url,
      agui_body(
        thread,
        "r1",
        serde_json::json!([{"id": "u1", "role": "user", "content": "hi"}]),
      ),
    )
    .await;
    let (status, events) = connect_agui(&url, "test-app", thread).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(events[0]["type"], "RUN_STARTED");
    assert_eq!(events[0]["runId"], "r1");
    assert_eq!(events[1]["type"], "MESSAGES_SNAPSHOT");
    assert_eq!(events[1]["messages"][0]["id"], "u1");
    assert_eq!(events.last().unwrap()["outcome"]["type"], "success");

    // The same app check as runs.
    let (status, _) = connect_agui(&url, "other-app", thread).await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    service.shutdown().await;
  }

  #[tokio::test]
  async fn ag_ui_in_dev_rejects_unknown_apps_routes_and_bad_requests() {
    let w = world().await;
    let journal = w.journal.clone();
    let service = Arc::new(w.service);
    let url = serve_agui(service.clone(), journal).await;
    let client = reqwest::Client::new();
    let body = agui_body(
      uuid::Uuid::now_v7(),
      "r1",
      serde_json::json!([{"id": "u1", "role": "user", "content": "hi"}]),
    );

    let r = client
      .post(format!("{url}/ag-ui/other-app/test"))
      .json(&body)
      .send()
      .await
      .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);
    let r = client
      .post(format!("{url}/ag-ui/test-app/missing"))
      .json(&body)
      .send()
      .await
      .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);
    assert!(r.text().await.unwrap().contains("/test"));
    let r = client
      .post(format!("{url}/ag-ui/test-app/test"))
      .body("{nope")
      .send()
      .await
      .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

    // A frontend on another origin (`next dev`): the preflight is answered
    // with a wildcard origin and POST + content-type allowed.
    let r = client
      .request(
        reqwest::Method::OPTIONS,
        format!("{url}/ag-ui/test-app/test"),
      )
      .header("origin", "http://localhost:3000")
      .header("access-control-request-method", "POST")
      .header("access-control-request-headers", "content-type")
      .send()
      .await
      .unwrap();
    assert!(r.status().is_success());
    assert_eq!(r.headers()["access-control-allow-origin"], "*");
    service.shutdown().await;
  }
}
