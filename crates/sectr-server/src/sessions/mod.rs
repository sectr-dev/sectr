//! `ProdSessionService` — the production [`SessionService`] impl over the
//! Registry + PgJournal, mounted through the SHARED sessions router (the
//! same HTTP contract the dev gateway serves — that parity is the point).
//!
//! Turn model: prod turns are CONTAINER-BACKED, not in-process tasks. The
//! service journals the turn start under the ordering contract inherited
//! from the dev dispatch (claim lease → [approval decision] →
//! INVOCATION_STARTED → dispatch), then hands off to the
//! [`TurnDispatcher`] (spawn the runner container; its sidecar
//! dials the ingest path for the assignment; ingest doubles as lease
//! heartbeat; `NO_MORE_ACTIONS` completes the turn). A dispatch failure
//! journals `SESSION_ERROR` + releases the lease — the journal always sees
//! a well-formed turn end, and the client watches it all on the tail.
//!
//! Authz: every method receives the [`RequestScope`] the auth layer
//! derived from the principal — org boundary for platform keys, app
//! boundary for invocation keys, app + OWNER boundary for user tokens
//! (a browser token sees only its user's sessions; foreign and
//! ownerless sessions are 404s, never 403s: no existence leaks).

use std::sync::Arc;

use futures::StreamExt as _;
use uuid::Uuid;

use sectr_common::api::{
  ApprovalDecisionRequest, ListSessionsQuery, NewSessionRequest, PendingApproval, RequestScope,
  RunIntent, RunPlan, RunRequest, ServiceError, SessionInput, SessionService, SessionStatus,
  SessionSummary, StartedRun, ThreadRef, UserTokenLease, derive_session_status,
  derive_session_title, plan_run,
};
use sectr_common::events::{
  Event, EventPayload, Extra, InvocationReason, InvocationStarted, SessionError,
  ToolApprovalDecision,
};
use sectr_common::journal::{Journal, SessionId, awaiting_approval};
use sectr_common::registry::{
  Deployment, NewSession, Registry, RegistryError, Session, deployment_serves_route,
  manifest_routes,
};

use crate::analytics::{self, Analytics};
use crate::dispatch::TurnDispatcher;
use crate::limits::{self, SessionRateLimiter, TurnCapsConfig};

pub struct ProdSessionService {
  registry: Arc<dyn Registry>,
  journal: Arc<dyn Journal>,
  dispatcher: Arc<dyn TurnDispatcher>,
  lease_ttl: chrono::Duration,
  /// Product analytics (wiring): first_session / first_approval_used.
  /// Disabled by default; the composition root attaches the live client.
  analytics: Arc<Analytics>,
  /// Concurrent-turn caps (per app + platform-wide). Default: none.
  turn_caps: TurnCapsConfig,
  /// Session-creation token buckets. Default: none.
  session_limiter: SessionRateLimiter,
}

impl ProdSessionService {
  pub fn new(
    registry: Arc<dyn Registry>,
    journal: Arc<dyn Journal>,
    dispatcher: Arc<dyn TurnDispatcher>,
    lease_ttl: chrono::Duration,
  ) -> Self {
    ProdSessionService {
      registry,
      journal,
      dispatcher,
      lease_ttl,
      analytics: Arc::new(Analytics::disabled()),
      turn_caps: TurnCapsConfig::unlimited(),
      session_limiter: SessionRateLimiter::unlimited(),
    }
  }

  /// Attach the guardrails (composition root). The default
  /// constructor applies no limits, so test call sites are untouched.
  pub fn with_limits(
    mut self,
    turn_caps: TurnCapsConfig,
    session_limiter: SessionRateLimiter,
  ) -> Self {
    self.turn_caps = turn_caps;
    self.session_limiter = session_limiter;
    self
  }

  /// Attach the live analytics client (composition root; tests that
  /// exercise the analytics emissions attach a capture-endpoint client).
  pub fn with_analytics(mut self, analytics: Arc<Analytics>) -> Self {
    self.analytics = analytics;
    self
  }

  /// The full turn start under the ordering contract (see module docs and
  /// the dev dispatch, which this mirrors). `pre_invocation_event` rides
  /// between the claim and INVOCATION_STARTED (the approval decision);
  /// `extra` is journaled on INVOCATION_STARTED (forward-compat fields,
  /// e.g. the AG-UI client's message id). Returns the new invocation's id.
  async fn start_turn(
    &self,
    session: SessionId,
    route: &str,
    reason: InvocationReason,
    pre_invocation_event: Option<EventPayload>,
    extra: Extra,
  ) -> Result<Uuid, ServiceError> {
    let row = self
      .registry
      .get_session(session)
      .await
      .map_err(not_found)?;
    let deployment_id = row.deployment_id;
    let invocation_id = Uuid::now_v7();
    // The per-app cap depends on where the session's deployment runs.
    let on_endpoint = self
      .registry
      .get_deployment(deployment_id)
      .await
      .map_err(internal)?
      .endpoint_url
      .is_some();

    // 1. Claim the lease FIRST — a conflict (409) or a reached
    //    concurrency cap (429) must leave the journal exactly as
    //    it was (the error goes out, nothing to undo). The cap check and
    //    the claim are ONE transaction in the registry, so concurrent
    //    claims cannot overshoot. Approval resumes and follow-up inputs
    //    pass through here too: they spawn a sandbox, so they count.
    let lease = self
      .registry
      .claim_lease_capped(
        session,
        "sectr-server",
        Some(invocation_id),
        self.lease_ttl,
        &self.turn_caps.caps_for(row.app_id, on_endpoint),
      )
      .await
      .map_err(|e| match e {
        RegistryError::Conflict(m) => ServiceError::Conflict(m),
        RegistryError::CapExceeded { scope, limit } => limits::cap_exceeded(scope, limit),
        other => internal(other),
      })?;

    // 2. Optional pre-invocation platform event (the approval decision).
    if let Some(event) = pre_invocation_event
      && let Err(e) = self
        .journal
        .append_platform_event(session, deployment_id, event)
        .await
    {
      let _ = self.registry.release_lease(lease.id).await;
      return Err(internal(e));
    }

    // 3. INVOCATION_STARTED.
    if let Err(e) = self
      .journal
      .append_platform_event(
        session,
        deployment_id,
        EventPayload::InvocationStarted(InvocationStarted {
          invocation_id,
          route: route.to_string(),
          deployment_id,
          reason,
          extra,
        }),
      )
      .await
    {
      let _ = self.registry.release_lease(lease.id).await;
      return Err(internal(e));
    }

    // 4. Dispatch the runner. Failure journals the turn end (the runner
    //    never started) and releases the lease — well-formed turn end,
    //    recovery = the client retries the turn on the errored session.
    if let Err(e) = self
      .dispatcher
      .dispatch_turn(session, deployment_id, invocation_id, route)
      .await
    {
      tracing::warn!(%session, %invocation_id, "turn dispatch failed: {e}");
      crate::metrics::SESSION_ERRORS.add(1, &[opentelemetry::KeyValue::new("source", "dispatch")]);
      let _ = self
        .journal
        .append_platform_event(
          session,
          deployment_id,
          EventPayload::SessionError(SessionError {
            message: e.to_string(),
            extra: Default::default(),
          }),
        )
        .await;
      let _ = self.registry.release_lease(lease.id).await;
    }
    Ok(invocation_id)
  }

  /// Admission for a NEW session (`POST /sessions`, AG-UI's first
  /// run of a thread), before any row is written:
  ///
  /// 1. a non-atomic capacity pre-check, so the common rejection leaves no
  ///    empty session behind (the authoritative check is the atomic claim
  ///    in `start_turn`);
  /// 2. then the rate-limit bucket — in that order, so capacity rejections
  ///    don't burn tokens.
  ///
  /// Race (accepted, no delete path): if a concurrent create takes the last
  /// slot between this pre-check and the claim, the claim fails AFTER
  /// `open_session` — the client gets the 429 and the session exists with
  /// only `SESSION_CREATED` (idle, valid: no lease, no sandbox; `POST
  /// /input` works on it later).
  async fn admit_new_session(
    &self,
    app_id: Uuid,
    deployment: &Deployment,
    scope: &RequestScope,
  ) -> Result<(), ServiceError> {
    let caps = self
      .turn_caps
      .caps_for(app_id, deployment.endpoint_url.is_some());
    if caps.any() {
      let (app_live, total_live) = self
        .registry
        .count_live_turns(app_id)
        .await
        .map_err(internal)?;
      if let Some((cap_scope, limit)) = caps.exceeded(app_live, total_live) {
        return Err(limits::cap_exceeded(cap_scope, limit));
      }
    }
    if let Some(user) = &scope.external_user_id {
      self.session_limiter.check_user(app_id, user)
    } else if scope.origin == sectr_common::api::Origin::Api {
      self.session_limiter.check_app(app_id)
    } else {
      // Platform principal (dashboard playground / CLI): the org's own
      // developer — exempt.
      Ok(())
    }
  }

  /// Org/app/owner boundary: foreign sessions are not found (no existence
  /// leak). The session row carries no org_id (apps do), so the org check
  /// goes through one app lookup. User tokens add the owner arm:
  /// a browser token sees only sessions owned by its bound user — and
  /// ownerless sessions (backend-created via an invocation key) are never
  /// visible to user tokens (the owner-match fails on NULL).
  async fn ensure_visible(
    &self,
    session: &Session,
    scope: &RequestScope,
  ) -> Result<(), ServiceError> {
    if let Some(org_id) = scope.org_id {
      let app = self
        .registry
        .get_app(session.app_id)
        .await
        .map_err(not_found)?;
      if app.org_id != org_id {
        return Err(ServiceError::NotFound(format!("session {}", session.id)));
      }
    }
    if let Some(app_id) = scope.app_id
      && session.app_id != app_id
    {
      return Err(ServiceError::NotFound(format!("session {}", session.id)));
    }
    if let Some(owner) = &scope.external_user_id
      && session.external_user_id.as_deref() != Some(owner)
    {
      return Err(ServiceError::NotFound(format!("session {}", session.id)));
    }
    Ok(())
  }

  /// Resolve `req.app` (app id or slug) within the caller's org.
  async fn resolve_app(
    &self,
    app: &str,
    scope: &RequestScope,
  ) -> Result<sectr_common::registry::App, ServiceError> {
    let org_id = scope
      .org_id
      .ok_or_else(|| ServiceError::BadRequest("authenticated principal carries no org".into()))?;
    if let Ok(id) = app.parse::<Uuid>() {
      let found = self.registry.get_app(id).await.map_err(not_found)?;
      if found.org_id != org_id {
        return Err(ServiceError::NotFound(format!("unknown app {app:?}")));
      }
      return Ok(found);
    }
    self
      .registry
      .find_app_by_slug(org_id, app)
      .await
      .map_err(internal)?
      .ok_or_else(|| ServiceError::NotFound(format!("unknown app {app:?}")))
  }

  /// Create the session row (+ `SESSION_CREATED`, the creation metric and
  /// the `first_session` analytics event). `id` = a caller-chosen session
  /// id (AG-UI `threadId`), else a fresh UUIDv7. Shared by `create_session`
  /// and AG-UI's first run, so ownership stamping and journaling stay
  /// single-path.
  async fn open_session(
    &self,
    app: &sectr_common::registry::App,
    deployment: &Deployment,
    route: &str,
    scope: &RequestScope,
    id: Option<Uuid>,
  ) -> Result<Session, ServiceError> {
    let session = self
      .registry
      .create_session(NewSession {
        id,
        app_id: app.id,
        deployment_id: deployment.id,
        route: route.to_string(),
        origin: scope.origin,
        // User tokens stamp ownership at creation; the owner is
        // immutable thereafter. Backend/platform creators leave None.
        external_user_id: scope.external_user_id.clone(),
      })
      .await
      .map_err(internal)?;

    // SESSION_CREATED is the journal's seq 0 for the session.
    self
      .journal
      .append_platform_event(
        session.id,
        deployment.id,
        EventPayload::SessionCreated(sectr_common::events::SessionCreated {
          app_id: app.id,
          route: route.to_string(),
          external_user_id: session.external_user_id.clone(),
          extra: Default::default(),
        }),
      )
      .await
      .map_err(internal)?;

    // Metric: a session exists (the 201 is already earned).
    crate::metrics::SESSIONS_CREATED.add(
      1,
      &[opentelemetry::KeyValue::new(
        "origin",
        match scope.origin {
          sectr_common::api::Origin::Playground => "playground",
          sectr_common::api::Origin::Api => "api",
        },
      )],
    );

    // `first_session` (activation funnel) with the
    // `session_origin` prop. Emit-time firstness via the ONE org-scoped
    // count query the Registry already exposes: the new row is already
    // inserted, so an org total of 1 means this WAS the org's first
    // session. track() is fire-and-forget; the query is one indexed
    // count on the request path — a PostHog outage never delays a turn.
    if let Some(org_id) = scope.org_id
      && let Ok(counts) = self.registry.count_sessions_by_app(org_id).await
    {
      let total: i64 = counts.iter().map(|(_, count)| count).sum();
      if total == 1 {
        self.analytics.track(
          analytics::events::FIRST_SESSION,
          org_id,
          serde_json::json!({
              analytics::PROP_SESSION_ORIGIN:
                  match scope.origin {
                      sectr_common::api::Origin::Playground => "playground",
                      sectr_common::api::Origin::Api => "api",
                  }
          }),
        );
      }
    }

    Ok(session)
  }

  /// The approval decision → resumed turn; returns the resumed turn's
  /// invocation id (AG-UI streams that invocation). `extra` is journaled on
  /// its INVOCATION_STARTED (AG-UI: the run id).
  async fn decide_inner(
    &self,
    id: SessionId,
    approved: bool,
    approver: Option<String>,
    scope: &RequestScope,
    extra: Extra,
  ) -> Result<Uuid, ServiceError> {
    let session = self.registry.get_session(id).await.map_err(not_found)?;
    self.ensure_visible(&session, scope).await?;
    let events = self.events(id).await?;
    let pending = awaiting_approval(&events)
      .ok_or_else(|| ServiceError::Conflict("no pending approval on this session".into()))?;
    // Metric: the human-in-the-loop wait — journal timestamp of
    // the approval request to the decision POST landing. Only a real
    // decision records it (conflicts/errors don't count).
    let wait_ms = (chrono::Utc::now() - pending.created_at).num_milliseconds();
    if let Ok(ms) = u64::try_from(wait_ms) {
      crate::metrics::APPROVAL_DECISION_LATENCY.record(ms as f64, &[]);
    }
    let pending_call = match &pending.payload {
      EventPayload::ToolApprovalRequired(a) => a.tool_call_id.clone(),
      _ => {
        return Err(ServiceError::Internal(
          "pending tool call inconsistency".into(),
        ));
      }
    };

    // `first_approval_used` (activation funnel) — the org's
    // first approval decision. Firstness needs an org-wide scan of the
    // journals (decisions live in per-session event streams; no
    // org-scoped query exists), so it runs in a spawned task — the
    // approval POST must resume the turn immediately, and a PostHog
    // outage (or a slow scan) must never delay it. The CURRENT session
    // is excluded: its decision is journaled inside start_turn
    // concurrently with this scan. A failed fetch skips that session —
    // worst case a duplicate "first", never a false one.
    if let Some(org_id) = scope.org_id {
      let analytics = self.analytics.clone();
      let registry = self.registry.clone();
      let journal = self.journal.clone();
      let this_session = session.id;
      tokio::spawn(async move {
        if first_approval_for_org(&*registry, &*journal, org_id, this_session).await {
          analytics.track(
            analytics::events::FIRST_APPROVAL_USED,
            org_id,
            serde_json::json!({}),
          );
        }
      });
    }

    let decision = sectr_common::events::ApprovalDecision {
      approved,
      // Browser tokens: approver attribution defaults to the
      // token's bound end-user — the owner IS the human deciding.
      approver: approver.or_else(|| scope.external_user_id.clone()),
      extra: Default::default(),
    };
    // Journaled INSIDE start_turn, after the lease is secured —
    // journaling before the claim would leave a decision with no turn
    // following it when the claim conflicts (dev-dispatch lesson).
    let decision_event = if decision.approved {
      EventPayload::ToolApprovalGranted(ToolApprovalDecision {
        tool_call_id: pending_call,
        approver: decision.approver.clone(),
        extra: Default::default(),
      })
    } else {
      EventPayload::ToolApprovalDenied(ToolApprovalDecision {
        tool_call_id: pending_call,
        approver: decision.approver.clone(),
        extra: Default::default(),
      })
    };

    self
      .start_turn(
        id,
        &session.route,
        InvocationReason::ApprovalDecision {
          decision,
          extra: Default::default(),
        },
        Some(decision_event),
        extra,
      )
      .await
  }

  /// An existing thread for an AG-UI run or connect: visible to the caller
  /// (owner / app boundary), under the requested app and route.
  async fn visible_thread(
    &self,
    session: Session,
    app: &sectr_common::registry::App,
    route: &str,
    scope: &RequestScope,
  ) -> Result<Session, ServiceError> {
    self.ensure_visible(&session, scope).await?;
    if session.app_id != app.id {
      return Err(ServiceError::NotFound(format!("thread {}", session.id)));
    }
    if session.route != route {
      return Err(ServiceError::Conflict(format!(
        "thread {} belongs to route {:?}, not {:?}",
        session.id, session.route, route
      )));
    }
    Ok(session)
  }

  /// The app and active deployment an AG-UI request targets — the checks
  /// shared by runs and connect: the app (in the caller's org), the
  /// token <-> app binding, and a deployment that serves the route.
  async fn resolve_run_target(
    &self,
    app: &str,
    route: &str,
    scope: &RequestScope,
  ) -> Result<(sectr_common::registry::App, Deployment), ServiceError> {
    let found = self.resolve_app(app, scope).await?;
    // Token <-> app binding: a key/token bound to another app is refused.
    if let Some(bound) = scope.app_id
      && bound != found.id
    {
      return Err(ServiceError::Forbidden(format!(
        "this credential is not bound to app {app:?}"
      )));
    }
    let deployment: Deployment = self
      .registry
      .active_deployment(found.id)
      .await
      .map_err(internal)?
      .ok_or_else(|| {
        ServiceError::NotFound(format!("app {:?} has no active deployment", found.name))
      })?;
    if !deployment_serves_route(&deployment.manifest, route) {
      let available = manifest_routes(&deployment.manifest);
      return Err(ServiceError::NotFound(format!(
        "unknown route {:?} for app {:?}; available routes: {}",
        route,
        found.name,
        if available.is_empty() {
          "(none)".to_string()
        } else {
          available.join(", ")
        }
      )));
    }
    Ok((found, deployment))
  }

  async fn events(&self, id: SessionId) -> Result<Vec<Event>, ServiceError> {
    self.journal.fetch(id, None).await.map_err(internal)
  }

  async fn summary(&self, session: Session) -> Result<SessionSummary, ServiceError> {
    let events = self.events(session.id).await?;
    Ok(derive_summary(&session, &events))
  }
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

fn internal(e: impl std::fmt::Display) -> ServiceError {
  ServiceError::Internal(e.to_string())
}

fn not_found(e: impl std::fmt::Display) -> ServiceError {
  ServiceError::NotFound(e.to_string())
}

#[async_trait::async_trait]
impl SessionService for ProdSessionService {
  async fn create_session(
    &self,
    req: NewSessionRequest,
    scope: RequestScope,
  ) -> Result<SessionId, ServiceError> {
    let app = self.resolve_app(&req.app, &scope).await?;
    // "Latest active deployment wins" — new sessions pin to it.
    let deployment: Deployment = self
      .registry
      .active_deployment(app.id)
      .await
      .map_err(internal)?
      .ok_or_else(|| {
        ServiceError::NotFound(format!("app {:?} has no active deployment", app.name))
      })?;
    if !sectr_common::registry::deployment_serves_route(&deployment.manifest, &req.route) {
      return Err(ServiceError::NotFound(format!(
        "deployment does not serve route {:?}",
        req.route
      )));
    }

    self.admit_new_session(app.id, &deployment, &scope).await?;
    let session = self
      .open_session(&app, &deployment, &req.route, &scope, None)
      .await?;

    // Start the first turn; dispatch failures surface as SESSION_ERROR
    // on the tail (the session exists either way — 201 is correct).
    self
      .start_turn(
        session.id,
        &req.route,
        InvocationReason::UserMessage {
          input: req.input,
          extra: Default::default(),
        },
        None,
        Extra::default(),
      )
      .await?;
    Ok(session.id)
  }

  async fn list_sessions(
    &self,
    query: ListSessionsQuery,
    scope: RequestScope,
  ) -> Result<Vec<SessionSummary>, ServiceError> {
    // User tokens: the owner filter IS the token — a client-
    // supplied external_user_id is rejected (there is no way to read
    // another user's sessions through a browser token, by construction).
    let owner_filter = if scope.external_user_id.is_some() {
      if query.external_user_id.is_some() {
        return Err(ServiceError::BadRequest(
          "external_user_id is not accepted on browser tokens; sessions are always scoped to the token's user".into(),
        ));
      }
      scope.external_user_id.clone()
    } else {
      // Backend/platform admin view: optional owner filter from the query.
      query.external_user_id.clone()
    };
    // Invocation/user keys are pinned to their app regardless of the query.
    let app_id = match scope.app_id {
      Some(app_id) => Some(app_id),
      None => match &query.app_id {
        Some(app) => Some(
          self
            .resolve_app(app, &scope)
            .await
            .map_err(|_| ServiceError::NotFound(format!("unknown app {app:?}")))?
            .id,
        ),
        None => None,
      },
    };
    let rows = self
      .registry
      .list_sessions(
        scope.org_id,
        app_id,
        owner_filter.as_deref(),
        query.limit.unwrap_or(50),
        query.created_before,
      )
      .await
      .map_err(internal)?;
    let mut summaries = Vec::with_capacity(rows.len());
    for session in rows {
      let summary = self.summary(session).await?;
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
    scope: RequestScope,
  ) -> Result<SessionSummary, ServiceError> {
    let session = self.registry.get_session(id).await.map_err(not_found)?;
    self.ensure_visible(&session, &scope).await?;
    self.summary(session).await
  }

  async fn send_input(
    &self,
    id: SessionId,
    req: SessionInput,
    scope: RequestScope,
  ) -> Result<(), ServiceError> {
    let session = self.registry.get_session(id).await.map_err(not_found)?;
    self.ensure_visible(&session, &scope).await?;
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
    let events = self.events(id).await?;
    match derive_session_status(&events) {
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
    self
      .start_turn(
        id,
        &session.route,
        InvocationReason::UserMessage {
          input: req.input,
          extra: Default::default(),
        },
        None,
        Extra::default(),
      )
      .await
      .map(|_| ())
  }

  async fn decide_approval(
    &self,
    id: SessionId,
    req: ApprovalDecisionRequest,
    scope: RequestScope,
  ) -> Result<(), ServiceError> {
    self
      .decide_inner(id, req.approved, req.approver, &scope, Extra::default())
      .await
      .map(|_| ())
  }

  async fn start_run(
    &self,
    req: RunRequest,
    scope: RequestScope,
  ) -> Result<StartedRun, ServiceError> {
    let (app, deployment) = self
      .resolve_run_target(&req.app, &req.route, &scope)
      .await?;

    // Thread resolution: the threadId IS the session id. A foreign or
    // other-app session is NotFound (as on every other session route); a
    // nonexistent thread is created on the first user-message run. Unlike
    // the other session routes this IS an existence oracle (404 vs a created
    // thread) — accepted: ids are random UUIDs (schema/agui-mapping.md).
    let existing = match self.registry.get_session(req.thread_id).await {
      Ok(session) => Some(session),
      Err(RegistryError::NotFound(_)) => None,
      Err(e) => return Err(internal(e)),
    };
    let (session, created) = match existing {
      Some(session) => (
        self
          .visible_thread(session, &app, &req.route, &scope)
          .await?,
        false,
      ),
      None => {
        req.intent.check_creates_thread(req.thread_id)?;
        self.admit_new_session(app.id, &deployment, &scope).await?;
        match self
          .open_session(&app, &deployment, &req.route, &scope, Some(req.thread_id))
          .await
        {
          Ok(session) => (session, true),
          // Lost a creation race (two first POSTs for one thread — e.g.
          // a double-fired client): the thread exists now; fall through
          // to the existing-thread path (a 409 while the winner's turn is
          // live).
          Err(e) => match self.registry.get_session(req.thread_id).await {
            Ok(session) => (
              self
                .visible_thread(session, &app, &req.route, &scope)
                .await?,
              false,
            ),
            Err(_) => return Err(e),
          },
        }
      }
    };

    // Existing thread: the shared busy-thread / interrupt decision (the
    // same rule the dev gateway applies). A new thread always starts.
    let plan = if created {
      RunPlan::StartTurn
    } else {
      let live = self
        .registry
        .live_lease(session.id)
        .await
        .map_err(internal)?;
      let events = self.events(session.id).await?;
      plan_run(&events, live.as_ref(), &req.intent)?
    };

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
        let invocation_id = self
          .start_turn(
            session.id,
            &req.route,
            InvocationReason::UserMessage {
              input,
              extra: Default::default(),
            },
            None,
            sectr_common::agui::invocation_extra(
              &req.run_id,
              client_message_id,
              forwarded_props,
              context,
            ),
          )
          .await?;
        Ok(StartedRun {
          session_id: session.id,
          invocation_id,
          created,
        })
      }
      (RunPlan::ResumeApproval, RunIntent::ApprovalDecision { approved, .. }) => {
        let invocation_id = self
          .decide_inner(
            session.id,
            approved,
            None,
            &scope,
            sectr_common::agui::invocation_extra(&req.run_id, None, None, None),
          )
          .await?;
        Ok(StartedRun {
          session_id: session.id,
          invocation_id,
          created: false,
        })
      }
      _ => Err(ServiceError::Internal(
        "run plan does not match the run intent".into(),
      )),
    }
  }

  async fn resolve_thread(
    &self,
    req: ThreadRef,
    scope: RequestScope,
  ) -> Result<Option<SessionId>, ServiceError> {
    let (app, _) = self
      .resolve_run_target(&req.app, &req.route, &scope)
      .await?;
    // Read-only: a nonexistent thread is None (never created here), and an
    // existing one passes exactly the run path's visibility / route checks.
    let session = match self.registry.get_session(req.thread_id).await {
      Ok(session) => session,
      Err(RegistryError::NotFound(_)) => return Ok(None),
      Err(e) => return Err(internal(e)),
    };
    let session = self
      .visible_thread(session, &app, &req.route, &scope)
      .await?;
    Ok(Some(session.id))
  }

  async fn pending_approvals(
    &self,
    scope: RequestScope,
  ) -> Result<Vec<PendingApproval>, ServiceError> {
    let rows = self
      .registry
      .list_sessions(scope.org_id, scope.app_id, None, 200, None)
      .await
      .map_err(internal)?;
    let mut pending = Vec::new();
    for session in rows {
      let events = self.events(session.id).await?;
      let Some(event) = awaiting_approval(&events).cloned() else {
        continue;
      };
      pending.push(PendingApproval {
        session: derive_summary(&session, &events),
        event,
      });
    }
    Ok(pending)
  }

  async fn session_events(
    &self,
    id: SessionId,
    last_event_id: Option<Uuid>,
    scope: RequestScope,
  ) -> Result<sectr_common::journal::EventStream, ServiceError> {
    let session = self.registry.get_session(id).await.map_err(not_found)?;
    self.ensure_visible(&session, &scope).await?;
    let after_seq = match last_event_id {
      None => None,
      Some(event_id) => {
        // Resolve the event id → seq: replay strictly after it.
        // (O(transcript) per reconnect — fine for MVP; an id→seq
        // index is the later optimization.)
        let events = self.events(id).await?;
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
    let stream = self
      .journal
      .subscribe(id, after_seq)
      .await
      .map_err(internal)?;
    // A user token's stream must not outlive the token: end it
    // at the token's expiry or when it is revoked (incl. via its minting
    // key). The client's reconnect then gets a 401 and re-mints.
    Ok(match scope.user_token {
      Some(lease) => {
        let registry = self.registry.clone();
        Box::pin(stream.take_until(token_ended(registry, lease)))
      }
      None => stream,
    })
  }
}

/// How often an open user-token stream re-checks that its token is still
/// live (revocation; expiry is exact, not polled).
const TOKEN_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Resolves when the token has expired or been revoked. A registry error
/// is treated as "still live" (logged): a transient DB blip must not cut
/// every open stream, and the next tick re-checks.
async fn token_ended(registry: Arc<dyn Registry>, lease: UserTokenLease) {
  let expiry = async {
    // Negative (already expired) → zero.
    let remaining = (lease.expires_at - chrono::Utc::now())
      .to_std()
      .unwrap_or_default();
    tokio::time::sleep(remaining).await;
  };
  let revocation = async {
    let mut tick = tokio::time::interval(TOKEN_RECHECK_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
      tick.tick().await; // first tick is immediate: catches already-dead tokens
      match registry.api_key_is_live(lease.key_id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => tracing::warn!(error = %e, "user-token liveness check failed"),
      }
    }
  };
  tokio::select! {
    () = expiry => {}
    () = revocation => {}
  }
}

/// Analytics helper: is this the org's FIRST approval decision? Bounded scan
/// of the org's sessions (up to [`FIRST_APPROVAL_SCAN_LIMIT`]) — any prior
/// `TOOL_APPROVAL_GRANTED`/`DENIED` event in any journal ⇒ not first. Runs
/// off the request path (see the decide_approval call site).
async fn first_approval_for_org(
  registry: &dyn Registry,
  journal: &dyn Journal,
  org_id: Uuid,
  this_session: Uuid,
) -> bool {
  let Ok(sessions) = registry
    .list_sessions(Some(org_id), None, None, FIRST_APPROVAL_SCAN_LIMIT, None)
    .await
  else {
    // Can't see the org's history → don't claim a "first".
    return false;
  };
  for session in sessions {
    if session.id == this_session {
      continue;
    }
    let Ok(events) = journal.fetch(session.id, None).await else {
      continue;
    };
    if events.iter().any(|e| {
      matches!(
        e.payload,
        EventPayload::ToolApprovalGranted(_) | EventPayload::ToolApprovalDenied(_)
      )
    }) {
      return false;
    }
  }
  true
}

/// Session cap for the first-approval org scan. Alpha-scale orgs have
/// dozens of sessions; past that the scan degrades (may emit a duplicate
/// "first" — PostHog's first-time-for-user filter dedupes) rather than
/// growing unbounded.
const FIRST_APPROVAL_SCAN_LIMIT: i64 = 200;

#[cfg(test)]
mod tests;
