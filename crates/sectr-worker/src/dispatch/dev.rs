//! The dev server-role turn orchestration: everything around
//! `TurnEngine::run` that "the server does" in prod, executed in-process —
//! session create → lease claim → INVOCATION_STARTED → sandbox spawn +
//! readiness → engine → teardown.
//!
//! Ownership map:
//! - HERE (server role): Registry rows, journal platform events
//!   (`SESSION_CREATED` / `INVOCATION_STARTED` / approval decisions),
//!   lease claim/release, secret resolution into the runner env overlay,
//!   sandbox spawn + readiness + teardown, the debug event tail.
//! - ENGINE (`crate::turn`): invoke, validate, journal the stream. Never
//!   sees leases, sandboxes, or secrets sources.
//!
//! Process model (MVP): a FRESH runner process per invocation — process
//! statelessness is the transcript-first correctness invariant. Readiness =
//! TCP poll on the runner's `$SECTR_PORT` (prod: the WS `ready` frame). The
//! ready-deadline is spawn-failure detection for dev UX, NOT liveness —
//! liveness is the lease (ingest-refreshed), and a runner that binds and
//! then stalls is the reconciler's problem, not this module's.
//!
//! Transcript rule: `INVOCATION_STARTED` is journaled BEFORE the transcript
//! is fetched, so the engine receives a transcript that already includes
//! this invocation's start (invoke-contract requirement).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sectr_common::api::UserMessageInput;
use sectr_common::drivers::{
  DriverError, RunnerSpec, SandboxDriver, SecretSource, startup_failure_message,
};
use sectr_common::events::{
  ApprovalDecision, EventPayload, Extra, InvocationReason, InvocationStarted, NewEvent,
  NoMoreActions, SessionCreated, SessionError, ToolApprovalDecision,
};
use sectr_common::journal::{Journal, JournalError, SessionId, awaiting_approval};
use sectr_common::registry::{NewSession, Registry, RegistryError};
use sectr_common::sidecar::{
  APPROVAL_POLICY_ENV, APPROVAL_POLICY_ENV_IS_RESERVED, approval_policy_env,
};

use super::LeaseRefreshingSink;
use crate::turn::{TurnAssignment, TurnEngine, TurnResult};

/// Dev dispatch configuration. The CLI (step 4) fills this from `sectr.toml`
/// + CLI flags.
#[derive(Debug, Clone)]
pub struct DispatchConfig {
  /// Lease TTL — the only liveness bound in the system (refreshed on
  /// every journal append via [`LeaseRefreshingSink`]).
  pub lease_ttl: chrono::Duration,
  /// Generous bound for the runner to bind its port. Spawn-failure
  /// detection only (see module doc) — NOT liveness.
  pub ready_deadline: Duration,
  /// Port-poll cadence.
  pub ready_poll_interval: Duration,
  /// Runner argv, resolved by the CLI (no shell interpolation).
  pub runner_command: Vec<String>,
  /// Runner working directory.
  pub runner_workdir: Option<PathBuf>,
  /// Extra env for the runner beyond SECTR_PORT + secrets.
  pub runner_env: Vec<(String, String)>,
  /// `[deploy] approval_tools` from sectr.toml: passed to the runner as
  /// `SECTR_APPROVAL_POLICY`, as the sidecar does for a deployment.
  pub approval_tools: Vec<String>,
  /// The port passed to the runner as `SECTR_PORT`, and polled for
  /// readiness. `None`: the dispatch reserves a free ephemeral one per
  /// spawn. A caller running many dispatches at once (the conformance
  /// harness) passes its own, so no other process can take the port
  /// between the reservation and the runner's bind.
  pub runner_port: Option<u16>,
  /// `.env` path for secret resolution; `None` = no secrets. In dev the
  /// file is re-read per resolve, so edits apply on the next turn.
  pub env_file: Option<PathBuf>,
}

/// Optional per-turn knobs for callers that need to know the invocation
/// BEFORE the turn finishes (the AG-UI endpoint streams that invocation's
/// journal events while the turn runs). `TurnOpts::default()` is the plain
/// turn.
#[derive(Debug, Default)]
pub struct TurnOpts {
  /// Use this invocation id instead of minting one.
  pub invocation_id: Option<uuid::Uuid>,
  /// Forward-compat fields journaled on `INVOCATION_STARTED`.
  pub extra: Extra,
  /// Fired once `INVOCATION_STARTED` is journaled (`Ok`) — or with the
  /// failure when the turn could not start (e.g. the lease claim
  /// conflicted), so a waiting caller never hangs.
  pub started: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
  #[error(transparent)]
  Registry(#[from] RegistryError),
  #[error(transparent)]
  Journal(#[from] JournalError),
  #[error(transparent)]
  Driver(#[from] DriverError),
  #[error(transparent)]
  Io(#[from] std::io::Error),
  #[error("invalid session state: {0}")]
  InvalidState(String),
  /// The runner didn't start: the message says why, with the end of its
  /// stderr ([`startup_failure_message`]).
  #[error("{0}")]
  RunnerStartup(String),
}

pub type Result<T> = std::result::Result<T, DispatchError>;

/// Dev server-role dispatch: one per `sectr dev` process, pinned to the
/// single bootstrapped app + its active deployment.
pub struct DevDispatch {
  journal: Arc<dyn Journal>,
  registry: Arc<dyn Registry>,
  engine: Arc<TurnEngine>,
  driver: Arc<dyn SandboxDriver>,
  secret_source: Arc<dyn SecretSource>,
  config: DispatchConfig,
  app_id: uuid::Uuid,
  deployment_id: uuid::Uuid,
}

impl DevDispatch {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    journal: Arc<dyn Journal>,
    registry: Arc<dyn Registry>,
    engine: Arc<TurnEngine>,
    driver: Arc<dyn SandboxDriver>,
    secret_source: Arc<dyn SecretSource>,
    config: DispatchConfig,
    app_id: uuid::Uuid,
    deployment_id: uuid::Uuid,
  ) -> Self {
    DevDispatch {
      journal,
      registry,
      engine,
      driver,
      secret_source,
      config,
      app_id,
      deployment_id,
    }
  }

  /// The journal handle — the single source for read-path consumers
  /// built on this dispatch (e.g. `DevSessionService` takes its store
  /// handles from here, so reads and writes are provably the same
  /// instances).
  pub fn journal(&self) -> &Arc<dyn Journal> {
    &self.journal
  }

  pub fn registry(&self) -> &Arc<dyn Registry> {
    &self.registry
  }

  /// Create a session (Registry row + `SESSION_CREATED` as the journal's
  /// seq 0) WITHOUT running a turn — lets callers (the CLI) start the
  /// event tail before the first turn runs.
  pub async fn create_session(&self, route: &str) -> Result<SessionId> {
    self.create_session_with_id(route, None).await
  }

  /// [`Self::create_session`] under a caller-chosen session id (the AG-UI
  /// `threadId`).
  pub async fn create_session_with_id(
    &self,
    route: &str,
    id: Option<uuid::Uuid>,
  ) -> Result<SessionId> {
    let session = self
      .registry
      .create_session(NewSession {
        id,
        app_id: self.app_id,
        deployment_id: self.deployment_id,
        route: route.to_string(),
        origin: sectr_common::api::Origin::Playground,
        // Dev has no principals — no owner (backend-created semantics).
        external_user_id: None,
      })
      .await?;
    self
      .journal
      .append_platform_event(
        session.id,
        self.deployment_id,
        EventPayload::SessionCreated(SessionCreated {
          app_id: self.app_id,
          route: route.to_string(),
          external_user_id: None,
          extra: Default::default(),
        }),
      )
      .await?;
    Ok(session.id)
  }

  /// Create a session and run its first turn with a user message.
  pub async fn create_session_and_run_turn(
    &self,
    route: &str,
    input: UserMessageInput,
  ) -> Result<(SessionId, TurnResult)> {
    let session = self.create_session(route).await?;
    let result = self
      .start_turn(
        session,
        route,
        InvocationReason::UserMessage {
          input,
          extra: Default::default(),
        },
        None,
        TurnOpts::default(),
      )
      .await?;
    Ok((session, result))
  }

  /// Follow-up user message on an existing session (new turn, same
  /// session — the transcript carries the context).
  pub async fn send_message(
    &self,
    session: SessionId,
    input: UserMessageInput,
  ) -> Result<TurnResult> {
    self
      .send_message_with(session, input, TurnOpts::default())
      .await
  }

  /// [`Self::send_message`] with per-turn options.
  pub async fn send_message_with(
    &self,
    session: SessionId,
    input: UserMessageInput,
    opts: TurnOpts,
  ) -> Result<TurnResult> {
    let route = self.registry.get_session(session).await?.route;
    self
      .start_turn(
        session,
        &route,
        InvocationReason::UserMessage {
          input,
          extra: Default::default(),
        },
        None,
        opts,
      )
      .await
  }

  /// Resume a turn suspended by `TOOL_APPROVAL_REQUIRED`: journal the
  /// decision (`TOOL_APPROVAL_GRANTED`/`DENIED`), then run a fresh
  /// invocation with `reason: approval_decision`.
  pub async fn resume_with_decision(
    &self,
    session: SessionId,
    decision: ApprovalDecision,
  ) -> Result<TurnResult> {
    self
      .resume_with_decision_with(session, decision, TurnOpts::default())
      .await
  }

  /// [`Self::resume_with_decision`] with per-turn options.
  pub async fn resume_with_decision_with(
    &self,
    session: SessionId,
    decision: ApprovalDecision,
    opts: TurnOpts,
  ) -> Result<TurnResult> {
    let events = self.journal.fetch(session, None).await?;
    let Some(EventPayload::ToolApprovalRequired(pending)) =
      awaiting_approval(&events).map(|e| &e.payload)
    else {
      return Err(DispatchError::InvalidState(format!(
        "last event is {}, expected TOOL_APPROVAL_REQUIRED",
        events
          .last()
          .map_or("none".into(), |e| e.event_type.to_string())
      )));
    };
    let decision_event = if decision.approved {
      EventPayload::ToolApprovalGranted(ToolApprovalDecision {
        tool_call_id: pending.tool_call_id.clone(),
        approver: decision.approver.clone(),
        extra: Default::default(),
      })
    } else {
      EventPayload::ToolApprovalDenied(ToolApprovalDecision {
        tool_call_id: pending.tool_call_id.clone(),
        approver: decision.approver.clone(),
        extra: Default::default(),
      })
    };
    let route = self.registry.get_session(session).await?.route;
    self
      .start_turn(
        session,
        &route,
        InvocationReason::ApprovalDecision {
          decision,
          extra: Default::default(),
        },
        // Journaled INSIDE start_turn, after the lease is secured —
        // journaling it before the claim would leave a decision with no
        // turn following it when the claim conflicts (and a retry would
        // then fail the TOOL_APPROVAL_REQUIRED check above, stranding
        // the session).
        Some(decision_event),
        opts,
      )
      .await
  }

  /// The full turn flow for one invocation. `pre_invocation_event` is an
  /// optional platform event journaled AFTER the lease is secured but
  /// BEFORE INVOCATION_STARTED (and thus included in the transcript the
  /// runner receives) — the approval decision rides this.
  ///
  /// Ordering contract, load-bearing end to end:
  /// claim lease → [optional decision] → INVOCATION_STARTED → transcript
  /// fetch (already includes the above). The claim MUST precede any
  /// journaling: a claim conflict must leave the journal exactly as it
  /// was, so a retry is possible.
  async fn start_turn(
    &self,
    session: SessionId,
    route: &str,
    reason: InvocationReason,
    pre_invocation_event: Option<EventPayload>,
    opts: TurnOpts,
  ) -> Result<TurnResult> {
    let mut opts = opts;
    let invocation_id = *opts.invocation_id.get_or_insert_with(uuid::Uuid::now_v7);

    // 1. Claim the lease FIRST. Failing here (live lease exists) errors
    //    up BEFORE anything is journaled — nothing to undo.
    let lease = match self
      .registry
      .claim_lease(
        session,
        "dev-runner",
        Some(invocation_id),
        self.config.lease_ttl,
      )
      .await
    {
      Ok(lease) => lease,
      Err(e) => {
        if let Some(tx) = opts.started.take() {
          let _ = tx.send(Err(e.to_string()));
        }
        return Err(e.into());
      }
    };

    // Everything between claim and teardown must release the lease on
    // an Err exit (a mid-flow journal/registry failure must not strand
    // the claim). Ok exits release in the flow itself.
    let result = self
      .run_turn_inner(
        session,
        route,
        reason,
        &lease,
        pre_invocation_event,
        &mut opts,
      )
      .await;
    if let Err(e) = &result {
      let _ = self.registry.release_lease(lease.id).await;
      // The turn never reached INVOCATION_STARTED: unblock a waiting caller.
      if let Some(tx) = opts.started.take() {
        let _ = tx.send(Err(e.to_string()));
      }
    }
    result
  }

  /// Post-claim flow. Lease release obligations: spawn-failure path and
  /// normal teardown release here; only an Err return leaves it to the
  /// caller (start_turn).
  async fn run_turn_inner(
    &self,
    session: SessionId,
    route: &str,
    reason: InvocationReason,
    lease: &sectr_common::registry::Lease,
    pre_invocation_event: Option<EventPayload>,
    opts: &mut TurnOpts,
  ) -> Result<TurnResult> {
    let invocation_id = opts
      .invocation_id
      .expect("start_turn sets the invocation id before the flow");
    let sink = LeaseRefreshingSink::new(
      self.journal.clone(),
      self.registry.clone(),
      lease.id,
      self.config.lease_ttl,
    );
    // 2. Optional pre-invocation platform event (approval decision) —
    //    journaled after the claim, so it can never dangle on a lost
    //    race, and before INVOCATION_STARTED, so the transcript the
    //    runner receives carries it.
    if let Some(event) = pre_invocation_event {
      self
        .journal
        .append_platform_event(session, self.deployment_id, event)
        .await?;
    }
    let started = self
      .journal
      .append_platform_event(
        session,
        self.deployment_id,
        EventPayload::InvocationStarted(InvocationStarted {
          invocation_id,
          route: route.to_string(),
          deployment_id: self.deployment_id,
          reason: reason.clone(),
          extra: std::mem::take(&mut opts.extra),
        }),
      )
      .await?;
    if let Some(tx) = opts.started.take() {
      let _ = tx.send(Ok(()));
    }
    let transcript = self.journal.fetch(session, None).await?;
    debug_assert!(transcript.len() >= 2); // SESSION_CREATED + INVOCATION_STARTED
    debug_assert!(matches!(
        &started.payload,
        EventPayload::InvocationStarted(is) if is.invocation_id == invocation_id
    ));

    // 2. Spawn the runner (secrets + SECTR_PORT via env overlay) and
    //    wait for it to bind.
    let (mut runner, port) = match self.spawn_runner().await {
      Ok(ok) => ok,
      Err(e) => {
        // Pre-invocation failure: this module journals the turn end
        // (the engine never started). The journal always sees a
        // well-formed turn end — same rule as the engine's paths.
        let _ = self.registry.release_lease(lease.id).await;
        return Ok(self.synthesize_failure(session, &e.to_string()).await);
      }
    };

    // 3. Run the turn.
    let assignment = TurnAssignment {
      session_id: session,
      deployment_id: self.deployment_id,
      invocation_id,
      route: route.to_string(),
      reason,
      transcript,
      runner_base: format!("http://127.0.0.1:{port}"),
    };
    let result = self.engine.run(&sink, assignment).await;

    // 4. Teardown: fresh process per invocation — kill + reap, release.
    let _ = runner.kill().await;
    let _ = self.registry.release_lease(lease.id).await;
    Ok(result)
  }

  /// Pick the port, resolve secrets into the env overlay, spawn the
  /// runner, wait for it to bind. Returns the handle and port.
  async fn spawn_runner(
    &self,
  ) -> std::result::Result<(Box<dyn sectr_common::drivers::RunnerHandle>, u16), DispatchError> {
    let port = match self.config.runner_port {
      Some(port) => port,
      None => reserve_port()?,
    };
    let mut env = vec![("SECTR_PORT".to_string(), port.to_string())];
    env.extend(self.config.runner_env.clone());
    for (key, secret) in self.secret_source.resolve(self.app_id).await? {
      // `.env` is re-read every turn, so `sectr dev`'s startup check
      // doesn't cover a line added since.
      if key == APPROVAL_POLICY_ENV {
        return Err(DispatchError::InvalidState(format!(
          "the dev secrets (.env) set {APPROVAL_POLICY_ENV}: {APPROVAL_POLICY_ENV_IS_RESERVED}"
        )));
      }
      env.push((key, secret.into_inner()));
    }
    // Last, so it wins over an inherited value.
    env.push((
      APPROVAL_POLICY_ENV.to_string(),
      approval_policy_env(&self.config.approval_tools),
    ));
    let mut runner = self
      .driver
      .spawn(RunnerSpec {
        command: self.config.runner_command.clone(),
        workdir: self.config.runner_workdir.clone(),
        env,
      })
      .await?;

    // Readiness poll. The process exiting while we poll is ground-truth
    // spawn failure — catch it at poll granularity so the error carries
    // the real exit status instead of a generic "did not bind".
    let deadline = tokio::time::Instant::now() + self.config.ready_deadline;
    loop {
      if tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
      {
        return Ok((runner, port));
      }
      if tokio::time::Instant::now() >= deadline {
        let what = format!(
          "the runner didn't listen on port {port} within {:?}",
          self.config.ready_deadline
        );
        // Killed first, so its stderr closes and the tail is complete.
        let _ = runner.kill().await;
        let tail = runner.stderr_tail().await.unwrap_or_default();
        return Err(DispatchError::RunnerStartup(startup_failure_message(
          &what, &tail,
        )));
      }
      if let Ok(Ok(exit)) =
        tokio::time::timeout(self.config.ready_poll_interval, runner.wait()).await
      {
        let what = format!("the runner exited during startup ({exit})");
        let tail = runner.stderr_tail().await.unwrap_or_default();
        return Err(DispatchError::RunnerStartup(startup_failure_message(
          &what, &tail,
        )));
      }
    }
  }

  /// Journal a synthesized turn failure for pre-invocation spawn failures
  /// (the engine owns everything after INVOCATION_STARTED).
  async fn synthesize_failure(&self, session: SessionId, message: &str) -> TurnResult {
    match self
      .journal
      .append(
        session,
        self.deployment_id,
        vec![
          NewEvent::new(EventPayload::SessionError(SessionError {
            message: message.to_string(),
            extra: Default::default(),
          })),
          NewEvent::new(EventPayload::NoMoreActions(NoMoreActions::default())),
        ],
      )
      .await
    {
      Ok(_) => TurnResult::Failed {
        message: message.to_string(),
      },
      Err(e) => TurnResult::IngestFailed(e.to_string()),
    }
  }
}

/// Reserve an ephemeral port for the runner; the listener is dropped
/// immediately (small TOCTOU window, fine for dev).
fn reserve_port() -> std::result::Result<u16, DispatchError> {
  let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
  let port = listener.local_addr()?.port();
  Ok(port)
}

#[cfg(test)]
mod tests {
  use super::*;
  use sectr_common::drivers::{InMemorySecretSource, SubprocessDriver};
  use sectr_common::events::{EventType, MessageDelta, ToolApprovalRequired, TypedDelta};
  use sectr_common::journal::{JournalReader as _, SqliteJournal};
  use sectr_common::registry::SqliteRegistry;
  use sectr_common::store::SqliteStore;

  // ---- harness -------------------------------------------------------

  struct World {
    dispatch: DevDispatch,
    journal: Arc<SqliteJournal>,
    secrets: Arc<InMemorySecretSource>,
    app_id: uuid::Uuid,
  }

  async fn world(frames_file: &std::path::Path) -> World {
    world_with_policy(frames_file, vec![]).await
  }

  async fn world_with_policy(frames_file: &std::path::Path, approval_tools: Vec<String>) -> World {
    let path = std::env::temp_dir()
      .join(format!("sectr-dispatch-{}.db", uuid::Uuid::now_v7()))
      .to_string_lossy()
      .into_owned();
    let s = SqliteStore::open(&path).await.unwrap();
    let org = s
      .registry()
      .create_org("test-org", "test-org")
      .await
      .unwrap();
    let app = s
      .registry()
      .create_app(sectr_common::registry::NewApp {
        org_id: org.id,
        name: "test-app".into(),
        slug: "test-app".into(),
      })
      .await
      .unwrap();
    let deployment = s
      .registry()
      .create_deployment(sectr_common::registry::NewDeployment {
        endpoint_url: None,
        app_id: app.id,
        version: 1,
        manifest: serde_json::json!({ "routes": [{ "path": "/test" }] }),
      })
      .await
      .unwrap();

    let secrets = Arc::new(InMemorySecretSource::new());
    let journal = Arc::new(SqliteJournal::new(s.pool().clone()));
    let registry = Arc::new(SqliteRegistry::new(s.pool().clone()));
    let journal_dyn: Arc<dyn Journal> = journal.clone();
    let registry_dyn: Arc<dyn Registry> = registry.clone();
    let dispatch = DevDispatch::new(
      journal_dyn,
      registry_dyn,
      Arc::new(TurnEngine::new()),
      Arc::new(SubprocessDriver::new()),
      secrets.clone(),
      DispatchConfig {
        lease_ttl: chrono::Duration::seconds(30),
        ready_deadline: Duration::from_secs(15),
        ready_poll_interval: Duration::from_millis(25),
        runner_command: fake_runner_command(),
        runner_workdir: None,
        runner_env: vec![(
          "SECTR_FRAMES_FILE".into(),
          frames_file.to_string_lossy().into_owned(),
        )],
        approval_tools,
        runner_port: None,
        env_file: None,
      },
      app.id,
      deployment.id,
    );
    World {
      dispatch,
      journal,
      secrets,
      app_id: app.id,
    }
  }

  /// A fake runner: a stdlib HTTP server in python3 that reads the
  /// invocation and streams the SSE frames listed in the file named by
  /// $SECTR_FRAMES_FILE (JSON array of runner-event objects — re-read per
  /// request so tests can script turn 2 by rewriting the file).
  fn fake_runner_command() -> Vec<String> {
    let script =
      std::env::temp_dir().join(format!("sectr-fake-runner-{}.py", uuid::Uuid::now_v7()));
    std::fs::write(
      &script,
      r#"
import http.server, json, os

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        with open(os.environ["SECTR_FRAMES_FILE"]) as f:
            frames = json.load(f)
        # The policy the runner was given, for the tests that check it.
        policy = json.dumps(os.environ.get("SECTR_APPROVAL_POLICY", "unset"))[1:-1]
        frames = [frame.replace("@POLICY@", policy) for frame in frames]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for frame in frames:
            # Each entry is already a runner-event JSON document.
            self.wfile.write(b"data: " + frame.encode() + b"\n\n")
        self.wfile.flush()

http.server.HTTPServer(("127.0.0.1", int(os.environ["SECTR_PORT"])), Handler).serve_forever()
"#,
    )
    .unwrap();
    vec!["python3".into(), script.to_string_lossy().into_owned()]
  }

  fn frame_json(payload: EventPayload) -> String {
    sectr_common::events::RunnerEvent::new(payload)
      .unwrap()
      .to_json_string()
  }

  fn delta_frame(text: &str) -> String {
    frame_json(EventPayload::MessageDelta(MessageDelta {
      message_id: "m1".into(),
      delta: TypedDelta::text(text),
      extra: Default::default(),
    }))
  }

  fn approval_frame() -> String {
    frame_json(EventPayload::ToolApprovalRequired(ToolApprovalRequired {
      tool_call_id: "call_1".into(),
      name: "issue_refund".into(),
      arguments: serde_json::json!({}),
      reason: None,
      extra: Default::default(),
    }))
  }

  /// Write the fake runner's script for one turn.
  fn script_turn(path: &std::path::Path, frames: &[String]) {
    std::fs::write(path, serde_json::to_string(&frames).unwrap()).unwrap();
  }

  async fn journal_types(journal: &SqliteJournal, session: uuid::Uuid) -> Vec<EventType> {
    journal
      .fetch(session, None)
      .await
      .unwrap()
      .into_iter()
      .map(|e| e.event_type)
      .collect()
  }

  // ---- tests ---------------------------------------------------------

  #[tokio::test]
  async fn the_runner_gets_approval_tools_as_the_policy() {
    let frames = std::env::temp_dir().join(format!("sectr-frames-{}.json", uuid::Uuid::now_v7()));
    script_turn(
      &frames,
      &[
        delta_frame("policy=@POLICY@"),
        frame_json(EventPayload::NoMoreActions(NoMoreActions::default())),
      ],
    );
    let w = world_with_policy(&frames, vec!["refund".into()]).await;

    let (session, result) = w
      .dispatch
      .create_session_and_run_turn(
        "/test",
        UserMessageInput::new(serde_json::json!("hi")).unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(result, TurnResult::Completed { errored: false });
    let events = w.journal.fetch(session, None).await.unwrap();
    let text = events
      .iter()
      .find_map(|e| match &e.payload {
        EventPayload::MessageDelta(d) => Some(serde_json::to_value(&d.delta).unwrap()),
        _ => None,
      })
      .unwrap();
    assert_eq!(text["text"], "policy=[\"refund\"]");
  }

  #[tokio::test]
  async fn an_approval_policy_in_the_dev_secrets_fails_the_turn() {
    let frames = std::env::temp_dir().join(format!("sectr-frames-{}.json", uuid::Uuid::now_v7()));
    script_turn(
      &frames,
      &[frame_json(EventPayload::NoMoreActions(
        NoMoreActions::default(),
      ))],
    );
    let w = world_with_policy(&frames, vec!["refund".into()]).await;
    w.secrets.set(
      w.app_id,
      vec![("SECTR_APPROVAL_POLICY".into(), "[]".into())],
    );

    let (session, result) = w
      .dispatch
      .create_session_and_run_turn(
        "/test",
        UserMessageInput::new(serde_json::json!("hi")).unwrap(),
      )
      .await
      .unwrap();

    let TurnResult::Failed { message } = result else {
      panic!("expected a failed turn, got {result:?}");
    };
    assert!(message.contains("[deploy] approval_tools"), "{message}");
    let events = w.journal.fetch(session, None).await.unwrap();
    let error = events
      .iter()
      .find_map(|e| match &e.payload {
        EventPayload::SessionError(err) => Some(err.message.clone()),
        _ => None,
      })
      .expect("the turn ends with SESSION_ERROR");
    assert_eq!(error, message);
  }

  #[tokio::test]
  async fn first_turn_end_to_end() {
    let frames = std::env::temp_dir().join(format!("sectr-frames-{}.json", uuid::Uuid::now_v7()));
    script_turn(
      &frames,
      &[
        delta_frame("hello"),
        frame_json(EventPayload::NoMoreActions(NoMoreActions::default())),
      ],
    );
    let w = world(&frames).await;

    let (session, result) = w
      .dispatch
      .create_session_and_run_turn(
        "/test",
        UserMessageInput::new(serde_json::json!("hi")).unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(result, TurnResult::Completed { errored: false });
    assert_eq!(
      journal_types(&w.journal, session).await,
      vec![
        EventType::SessionCreated,
        EventType::InvocationStarted,
        EventType::MessageDelta,
        EventType::NoMoreActions,
      ]
    );
    // Lease released at teardown.
    assert!(
      w.dispatch
        .registry
        .expired_leases()
        .await
        .unwrap()
        .is_empty()
    );
    let (live,): (i64,) =
      sqlx::query_as("SELECT COUNT(*) FROM leases WHERE session_id = ? AND released_at IS NULL")
        .bind(session)
        .fetch_one(w.journal.pool())
        .await
        .unwrap();
    assert_eq!(live, 0);
  }

  #[tokio::test]
  async fn a_configured_runner_port_is_the_one_used() {
    let frames = std::env::temp_dir().join(format!("sectr-frames-{}.json", uuid::Uuid::now_v7()));
    script_turn(
      &frames,
      &[
        delta_frame("hello"),
        frame_json(EventPayload::NoMoreActions(NoMoreActions::default())),
      ],
    );
    let mut w = world(&frames).await;
    let port = reserve_port().unwrap();
    w.dispatch.config.runner_port = Some(port);
    // The runner's command reports the port it was given, then runs.
    let mut command = vec![
      "/bin/sh".into(),
      "-c".into(),
      r#"echo "$SECTR_PORT" >"$SECTR_FRAMES_FILE.port"; exec "$0" "$@""#.into(),
    ];
    command.extend(fake_runner_command());
    w.dispatch.config.runner_command = command;

    let (_, result) = w
      .dispatch
      .create_session_and_run_turn(
        "/test",
        UserMessageInput::new(serde_json::json!("hi")).unwrap(),
      )
      .await
      .unwrap();

    assert_eq!(result, TurnResult::Completed { errored: false });
    let given = std::fs::read_to_string(frames.with_extension("json.port")).unwrap();
    assert_eq!(given.trim(), port.to_string());
  }

  #[tokio::test]
  async fn approval_resume_flow() {
    let frames = std::env::temp_dir().join(format!("sectr-frames-{}.json", uuid::Uuid::now_v7()));
    // Turn 1: suspend on approval.
    script_turn(&frames, &[delta_frame("checking"), approval_frame()]);
    let w = world(&frames).await;

    let (session, result) = w
      .dispatch
      .create_session_and_run_turn(
        "/test",
        UserMessageInput::new(serde_json::json!("refund order 42")).unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(result, TurnResult::AwaitingApproval);
    assert_eq!(
      journal_types(&w.journal, session).await.last(),
      Some(&EventType::ToolApprovalRequired)
    );

    // Turn 2: approve — fresh runner, scripted differently.
    script_turn(
      &frames,
      &[
        delta_frame("refund issued"),
        frame_json(EventPayload::NoMoreActions(NoMoreActions::default())),
      ],
    );
    let result = w
      .dispatch
      .resume_with_decision(
        session,
        ApprovalDecision {
          approved: true,
          approver: None,
          extra: Default::default(),
        },
      )
      .await
      .unwrap();
    assert_eq!(result, TurnResult::Completed { errored: false });

    assert_eq!(
      journal_types(&w.journal, session).await,
      vec![
        EventType::SessionCreated,
        EventType::InvocationStarted,
        EventType::MessageDelta,
        EventType::ToolApprovalRequired,
        EventType::ToolApprovalGranted,
        EventType::InvocationStarted,
        EventType::MessageDelta,
        EventType::NoMoreActions,
      ]
    );
  }

  #[tokio::test]
  async fn spawn_failure_journals_session_error() {
    let frames = std::env::temp_dir().join(format!("sectr-frames-{}.json", uuid::Uuid::now_v7()));
    std::fs::write(&frames, "[]").unwrap();
    let mut w = world(&frames).await;
    // A runner that fails at startup, saying why on stderr (as an adapter
    // rejecting the agent does).
    w.dispatch.config.runner_command = vec![
      "python3".into(),
      "-c".into(),
      "raise SystemExit('ValueError: structured output is not supported on Sectr yet')".into(),
    ];

    let (session, result) = w
      .dispatch
      .create_session_and_run_turn(
        "/test",
        UserMessageInput::new(serde_json::json!("hi")).unwrap(),
      )
      .await
      .unwrap();

    let TurnResult::Failed { message } = result else {
      panic!("expected Failed, got {result:?}");
    };
    assert_eq!(
      message,
      "the runner exited during startup (exit code 1). The end of its stderr:\n\
       ValueError: structured output is not supported on Sectr yet"
    );
    let events = w.journal.fetch(session, None).await.unwrap();
    let journaled = events.iter().find_map(|e| match &e.payload {
      EventPayload::SessionError(err) => Some(err.message.as_str()),
      _ => None,
    });
    assert_eq!(journaled, Some(message.as_str()));
    assert_eq!(
      journal_types(&w.journal, session).await,
      vec![
        EventType::SessionCreated,
        EventType::InvocationStarted,
        EventType::SessionError,
        EventType::NoMoreActions,
      ]
    );
  }

  #[tokio::test]
  async fn resume_claim_conflict_leaves_the_journal_untouched() {
    // Regression: the approval decision used to be journaled BEFORE the
    // lease claim — a claim conflict would leave a decision with no turn
    // following it, and a retry would then fail the
    // TOOL_APPROVAL_REQUIRED check (session stranded).
    let frames = std::env::temp_dir().join(format!("sectr-frames-{}.json", uuid::Uuid::now_v7()));
    script_turn(&frames, &[delta_frame("checking"), approval_frame()]);
    let w = world(&frames).await;

    let (session, result) = w
      .dispatch
      .create_session_and_run_turn(
        "/test",
        UserMessageInput::new(serde_json::json!("refund order 42")).unwrap(),
      )
      .await
      .unwrap();
    assert_eq!(result, TurnResult::AwaitingApproval);

    // Simulate a concurrent turn holder: a live lease on the session.
    let squatter = w
      .dispatch
      .registry
      .claim_lease(session, "other-runner", None, chrono::Duration::seconds(60))
      .await
      .unwrap();
    let _ = &squatter;

    let err = w
      .dispatch
      .resume_with_decision(
        session,
        ApprovalDecision {
          approved: true,
          approver: None,
          extra: Default::default(),
        },
      )
      .await
      .unwrap_err();
    assert!(
      err.to_string().contains("already has a live lease"),
      "expected claim conflict, got {err}"
    );

    // The decision was NOT journaled — the last event is still the
    // suspension, so a retry after the squatter's lease expires works.
    let types = journal_types(&w.journal, session).await;
    assert_eq!(types.last(), Some(&EventType::ToolApprovalRequired));
    assert!(!types.contains(&EventType::ToolApprovalGranted));
  }

  #[tokio::test]
  async fn resume_without_suspension_is_invalid_state() {
    let frames = std::env::temp_dir().join(format!("sectr-frames-{}.json", uuid::Uuid::now_v7()));
    script_turn(
      &frames,
      &[
        delta_frame("hello"),
        frame_json(EventPayload::NoMoreActions(NoMoreActions::default())),
      ],
    );
    let w = world(&frames).await;

    let (session, _) = w
      .dispatch
      .create_session_and_run_turn(
        "/test",
        UserMessageInput::new(serde_json::json!("hi")).unwrap(),
      )
      .await
      .unwrap();

    let err = w
      .dispatch
      .resume_with_decision(
        session,
        ApprovalDecision {
          approved: true,
          approver: None,
          extra: Default::default(),
        },
      )
      .await
      .unwrap_err();
    assert!(matches!(err, DispatchError::InvalidState(_)));
  }
}
