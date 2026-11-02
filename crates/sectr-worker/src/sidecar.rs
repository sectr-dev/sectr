//! The sidecar's Contract-4 WS client: dials the server's ingest
//! endpoint, receives assignments, runs turns through the SAME engine as
//! dev mode ([`TurnEngine`] — identical stream validation and synthesis),
//! and journals via [`SidecarSink`], which ships validated events upstream
//! and awaits acks.
//!
//! Liveness: NO heartbeat frames — every journal append IS the lease
//! heartbeat (the server refreshes the lease before appending each batch).
//!
//! Exactly-once: `NewEvent.id`s are producer-minted (the engine's
//! validator) and never change across attempts, so a batch sent on a
//! connection that died mid-append is REPLAYED unchanged on the reconnect
//! — the server dedups by id and acks the original seqs. The ack carries
//! the assigned seqs; the sink returns reconstructed envelopes (the engine
//! never reads their contents — only success/failure matters to it).
//!
//! Reconnect semantics: [`SidecarClient::run`] dials forever with capped
//! exponential backoff; each connection serves assignments until the WS
//! dies. A turn that loses its WS mid-flight keeps running — its next
//! append reconnects and replays. If the server's reconciler aborted the
//! turn meanwhile, the appends are rejected (`Conflict`) and the turn ends
//! via [`TurnResult::IngestFailed`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};

use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use sectr_common::events::Event;
use sectr_common::events::NewEvent;
use sectr_common::journal::{DeploymentId, EventSink, JournalError, SessionId};
use sectr_common::sidecar::{AckEntry, Assignment, CompleteOutcome, ServerFrame, SidecarFrame};

use crate::turn::{TurnAssignment, TurnEngine};

/// Sidecar runtime configuration. `ingest_token` is session-scoped (minted
/// at the session's first dispatch, injected into the container env at
/// spawn).
#[derive(Debug, Clone)]
pub struct SidecarConfig {
  /// The server's ingest endpoint: `ws://host:port/ingest`.
  pub ingest_url: String,
  pub ingest_token: String,
  /// Base URL of the runner's invoke server (`http://127.0.0.1:$SECTR_PORT`).
  pub runner_base: String,
  /// First reconnect delay; doubles up to [`Self::max_backoff`].
  pub reconnect_backoff: Duration,
  pub max_backoff: Duration,
}

/// Spawns/stops the user process per invocation (the real implementation is
/// the in-container spawn; the sidecar dials it at [`SidecarConfig::
/// runner_base`]).
#[async_trait::async_trait]
pub trait RunnerSpawner: Send + Sync {
  /// Start the runner and wait until it serves. An `Err` is the turn's
  /// `SESSION_ERROR` message (nothing is invoked), so it says why for the
  /// user: [`crate::runner::SubprocessRunner`] ends it with the runner's
  /// stderr.
  async fn start(&self, assignment: &Assignment) -> Result<(), String>;
  async fn stop(&self);
}

/// The runner is managed externally (already running) — a no-op spawner.
pub struct ExternalRunner;

#[async_trait::async_trait]
impl RunnerSpawner for ExternalRunner {
  async fn start(&self, _assignment: &Assignment) -> Result<(), String> {
    Ok(())
  }
  async fn stop(&self) {}
}

/// The live connection handle: the current outbound queue (replaced on
/// every reconnect) and the pending-batch ack map (survives reconnects —
/// that's the replay mechanism).
type PendingMap = HashMap<uuid::Uuid, oneshot::Sender<Result<Vec<AckEntry>, String>>>;

/// What the sidecar does after a turn boundary (its invocation completed —
/// sentinel, approval suspension, or failure). The policy is a SINGLE seam so a
/// future keep-warm tier slots in at the
/// sidecar level — the server drops the grant at every boundary either way.
/// Today: always `Exit` (the process terminates immediately; the next
/// dispatch spawns a fresh one).
enum PostTurnPolicy {
  /// Exit 0 after the turn. The container/execution ends itself; the
  /// next turn is a fresh dispatch.
  Exit,
  /// Stay on the connection and wait for a follow-up assignment (the
  /// future keep-warm tier — requires dial-time token auth first).
  /// Unreachable today (the const policy is always `Exit`); kept so the
  /// seam is complete and the variant is match-exhaustive.
  #[allow(dead_code)]
  WaitForAssignment,
}

const POST_TURN_POLICY: PostTurnPolicy = PostTurnPolicy::Exit;

/// How one `serve_once` connection ended (besides errors).
/// `Unauthorized` = the server rejected the token at the upgrade (401) —
/// terminal for the container.
#[derive(Debug)]
enum ServeFlow {
  /// Re-dial and keep serving (idle connection, or keep-warm waiting).
  Reconnect,
  /// The turn-boundary policy says the process terminates. Exit 0.
  Exit,
}

#[derive(Debug)]
enum ServeError {
  Unauthorized,
  Other(String),
}

impl From<String> for ServeError {
  fn from(e: String) -> Self {
    ServeError::Other(e)
  }
}

#[derive(Clone)]
struct Conn {
  send: Arc<Mutex<Option<mpsc::Sender<String>>>>,
  pending: Arc<Mutex<PendingMap>>,
  /// Set when a turn task completes (ANY terminal outcome) — the
  /// turn-boundary signal. Flag + Notify: the flag survives reconnects
  /// (a notify can be missed between serve_once calls), the notify wakes
  /// a reader blocked on the socket.
  turn_boundary: Arc<AtomicBool>,
  boundary_notify: Arc<Notify>,
}

impl Conn {
  fn new() -> Self {
    Conn {
      send: Arc::new(Mutex::new(None)),
      pending: Arc::new(Mutex::new(HashMap::new())),
      turn_boundary: Arc::new(AtomicBool::new(false)),
      boundary_notify: Arc::new(Notify::new()),
    }
  }

  fn signal_boundary(&self) {
    self.turn_boundary.store(true, Ordering::Release);
    self.boundary_notify.notify_waiters();
  }

  fn boundary_reached(&self) -> bool {
    self.turn_boundary.load(Ordering::Acquire)
  }

  fn clear_boundary(&self) {
    self.turn_boundary.store(false, Ordering::Release);
  }

  async fn send_frame(&self, frame: &SidecarFrame) -> Result<(), String> {
    let text = serde_json::to_string(frame).map_err(|e| e.to_string())?;
    self.send_inner(&text).await
  }

  async fn send_inner(&self, text: &str) -> Result<(), String> {
    let send = self.send.lock().await.clone();
    match send {
      Some(tx) => tx.send(text.to_owned()).await.map_err(|_| "closed".into()),
      None => Err("disconnected".into()),
    }
  }

  /// Resolve every pending batch (connection lost — the sink retries).
  async fn fail_pending(&self, reason: &str) {
    let mut pending = self.pending.lock().await;
    for (_, tx) in pending.drain() {
      let _ = tx.send(Err(reason.to_owned()));
    }
  }
}

/// One sidecar process: dial → serve assignments → run turns. One
/// connection per container, reconnecting forever.
pub struct SidecarClient {
  config: SidecarConfig,
  spawner: Arc<dyn RunnerSpawner>,
}

impl SidecarClient {
  pub fn new(config: SidecarConfig, spawner: Arc<dyn RunnerSpawner>) -> Self {
    SidecarClient { config, spawner }
  }

  /// Dial → serve a turn → apply the post-turn policy. Returns when the
  /// turn-boundary policy says the process terminates (`PostTurnPolicy::
  /// Exit` — the default: every turn boundary ends the process) or on an
  /// unauthorized dial (the token is no longer valid — grant dropped:
  /// boundary, server restart, or revocation). Everything else
  /// reconnects forever (mid-turn connection losses reconnect and replay;
  /// the turn task survives reconnects via the shared `Conn`).
  pub async fn run(&self) {
    let conn = Conn::new();
    let mut backoff = self.config.reconnect_backoff;
    loop {
      // A boundary that fired while we were between connections
      // (e.g. the turn ended with IngestFailed during a reconnect
      // gap) — the policy still applies.
      if conn.boundary_reached() {
        match self.on_boundary(&conn).await {
          ServeFlow::Exit => return,
          ServeFlow::Reconnect => {}
        }
      }
      match self.serve_once(&conn).await {
        Ok(ServeFlow::Exit) => return,
        Ok(ServeFlow::Reconnect) => {
          // Clean close: dial again immediately.
          backoff = self.config.reconnect_backoff;
        }
        Err(ServeError::Unauthorized) => {
          tracing::warn!("ingest token rejected (401): container no longer wanted; exiting");
          return;
        }
        Err(ServeError::Other(e)) => {
          tracing::warn!(error = %e, "ingest connection lost; reconnecting");
          tokio::time::sleep(backoff).await;
          backoff = (backoff * 2).min(self.config.max_backoff);
        }
      }
    }
  }

  /// The turn-boundary policy (the sidecar half): ONE decision
  /// point — "the invocation completed, what does the process do?"
  /// Today always `Exit` (always-terminate: the next dispatch spawns a
  /// fresh process). The future keep-warm tier returns `Reconnect` here
  /// (stay on the connection for the follow-up assignment) — which needs
  /// dial-time token auth and an expiry reaper first. Returning from
  /// `run()` exits the binary → process exit 0 → the container/execution
  /// ends itself.
  async fn on_boundary(&self, conn: &Conn) -> ServeFlow {
    conn.clear_boundary();
    match POST_TURN_POLICY {
      PostTurnPolicy::Exit => {
        tracing::info!("turn boundary: exiting (always-terminate policy)");
        ServeFlow::Exit
      }
      PostTurnPolicy::WaitForAssignment => {
        tracing::info!("turn boundary: keep-warm — waiting on the connection");
        ServeFlow::Reconnect
      }
    }
  }

  async fn serve_once(&self, conn: &Conn) -> Result<ServeFlow, ServeError> {
    let mut request: tokio_tungstenite::tungstenite::http::Request<()> = self
      .config
      .ingest_url
      .clone()
      .into_client_request()
      .map_err(|e| ServeError::Other(e.to_string()))?;
    request.headers_mut().insert(
      "Authorization",
      HeaderValue::from_str(&format!("Bearer {}", self.config.ingest_token))
        .map_err(|e| ServeError::Other(e.to_string()))?,
    );
    let (ws, _) = tokio_tungstenite::connect_async(request)
      .await
      .map_err(|e| match e {
        tokio_tungstenite::tungstenite::Error::Http(response)
          if response.status()
            == tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED =>
        {
          ServeError::Unauthorized
        }
        e => ServeError::Other(e.to_string()),
      })?;
    tracing::info!("ingest connection established");
    let (mut write, mut read) = ws.split();

    let (tx, mut rx) = mpsc::channel::<String>(64);
    *conn.send.lock().await = Some(tx.clone());
    let _writer = tokio::spawn(async move {
      while let Some(text) = rx.recv().await {
        if write.send(Message::text(text)).await.is_err() {
          break;
        }
      }
    });

    // Ready: the assignment (if one is pending) follows immediately.
    conn.send_frame(&SidecarFrame::Ready).await?;

    loop {
      if conn.boundary_reached() {
        // The turn completed while frames were still in flight —
        // the boundary policy applies.
        return Ok(ServeFlow::Exit);
      }
      let msg = tokio::select! {
          msg = read.next() => msg,
          // The turn task completed (ANY terminal outcome): the
          // boundary policy applies.
          _ = conn.boundary_notify.notified() => {
              return Ok(ServeFlow::Exit);
          }
      };
      let Some(msg) = msg else { break };
      let Ok(msg) = msg else { break };
      let Message::Text(text) = msg else { continue };
      let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::warn!("unparseable server frame (ignored)");
        continue;
      };
      match value.get("kind").and_then(|k| k.as_str()) {
        Some("assignment") => {
          match serde_json::from_value::<Assignment>(value) {
            Ok(assignment) => {
              // A SEPARATE task: the reader loop must keep
              // processing acks while the turn runs (the sink
              // awaits them). The lease guarantees one live
              // turn per session.
              let conn = conn.clone();
              let engine = TurnEngine::new();
              let spawner = self.spawner.clone();
              let runner_base = self.config.runner_base.clone();
              tokio::spawn(async move {
                run_turn(&engine, &spawner, conn, assignment, runner_base).await;
              });
            }
            Err(e) => tracing::warn!(error = %e, "invalid assignment frame (ignored)"),
          }
        }
        Some("ack") => match serde_json::from_value::<ServerFrame>(value) {
          Ok(ServerFrame::Ack { batch_id, events }) => {
            if let Some(entry) = conn.pending.lock().await.remove(&batch_id) {
              let _ = entry.send(Ok(events));
            }
          }
          _ => tracing::warn!("invalid ack frame (ignored)"),
        },
        Some("abort") => {
          let reason = value
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("aborted")
            .to_owned();
          tracing::warn!(reason = %reason, "ingest aborted");
          conn.fail_pending(&reason).await;
          // The turn dies via its failing append; the connection
          // stays for the NEXT assignment unless the server also
          // closed it.
          continue;
        }
        // Unknown kinds are ignored (forward compat).
        _ => {}
      }
    }
    conn.send.lock().await.take();
    conn.fail_pending("connection lost").await;
    Ok(ServeFlow::Reconnect)
  }
}

/// Execute one assignment through the dev-identical engine.
async fn run_turn(
  engine: &TurnEngine,
  spawner: &Arc<dyn RunnerSpawner>,
  conn: Conn,
  assignment: Assignment,
  runner_base: String,
) {
  // One span per sidecar turn — everything the sidecar does for
  // the turn (runner start, invoke, stream interception, journal writes)
  // lands under it; the journal.append spans in sectr-common are children.
  let span = tracing::info_span!(
      "sidecar.turn",
      session_id = %assignment.session_id,
      deployment_id = %assignment.deployment_id,
      invocation_id = %assignment.invocation_id,
      route = %assignment.route,
  );
  let _guard = span.enter();
  let started = spawner
    .start(&assignment)
    .await
    .inspect_err(|e| tracing::warn!(error = %e, "runner start failed"));

  let turn_assignment = TurnAssignment {
    session_id: assignment.session_id,
    deployment_id: assignment.deployment_id,
    invocation_id: assignment.invocation_id,
    route: assignment.route.clone(),
    reason: assignment.reason.clone(),
    transcript: assignment.transcript.clone(),
    runner_base,
  };
  let sink = SidecarSink {
    conn: conn.clone(),
    retry_backoff: Duration::from_millis(250),
  };
  let result = match started {
    Ok(()) => engine.run(&sink, turn_assignment).await,
    // Why it didn't start, ending with its stderr, is the turn's
    // SESSION_ERROR: the user's only view of it.
    Err(message) => engine.fail_startup(&sink, &turn_assignment, message).await,
  };
  // The invocation completed — ANY terminal outcome is a turn boundary:
  // signal it so the connection loop applies the post-turn policy
  // (the sidecar half).
  conn.signal_boundary();
  let (outcome, message) = match result {
    crate::turn::TurnResult::Completed { .. } => (CompleteOutcome::Completed, None),
    crate::turn::TurnResult::AwaitingApproval => (CompleteOutcome::Suspended, None),
    crate::turn::TurnResult::Failed { message } => (CompleteOutcome::Failed, Some(message)),
    crate::turn::TurnResult::IngestFailed(_) => {
      // The connection is the thing that died — the complete frame
      // has nowhere to go; the server's reconciler owns the turn.
      (CompleteOutcome::IngestLost, None)
    }
  };
  let _ = conn
    .send_frame(&SidecarFrame::Complete { outcome, message })
    .await;
  spawner.stop().await;
}

/// The engine's journal sink, backed by the ingest WS: ship the batch,
/// await the ack, replay the SAME batch on reconnect (dedup makes it
/// exactly-once). Append failures map onto [`JournalError`] so the engine
/// treats them exactly like dev-mode ingest failures.
struct SidecarSink {
  conn: Conn,
  retry_backoff: Duration,
}

impl SidecarSink {
  async fn await_ack(
    &self,
    batch_id: uuid::Uuid,
    events: &[NewEvent],
  ) -> Result<Vec<AckEntry>, JournalError> {
    loop {
      let (tx, rx) = oneshot::channel();
      self.conn.pending.lock().await.insert(batch_id, tx);
      let frame = SidecarFrame::Event {
        batch_id,
        events: events.to_vec(),
      };
      if let Err(_closed) = self.conn.send_frame(&frame).await {
        tokio::time::sleep(self.retry_backoff).await;
        continue; // reconnect happened; replay the same batch
      }
      match rx.await {
        Ok(Ok(entries)) => return Ok(entries),
        Ok(Err(reason)) => {
          // The server rejected the batch (lease lost / journal
          // failure): the turn is over, no replay.
          return Err(JournalError::Conflict(reason));
        }
        Err(_sender_dropped) => {
          tokio::time::sleep(self.retry_backoff).await;
          continue; // connection died: replay the SAME batch
        }
      }
    }
  }
}

#[async_trait::async_trait]
impl EventSink for SidecarSink {
  async fn append(
    &self,
    session: SessionId,
    deployment_id: DeploymentId,
    events: Vec<NewEvent>,
  ) -> Result<Vec<Event>, JournalError> {
    if events.is_empty() {
      return Ok(Vec::new());
    }
    let batch_id = uuid::Uuid::now_v7();
    let acks = self.await_ack(batch_id, &events).await?;

    // Reconstruct stamped envelopes from the ack (the engine ignores
    // their contents — only success/failure matters to it).
    Ok(
      events
        .into_iter()
        .zip(acks)
        .map(|(new, ack)| new.into_event(session, deployment_id, ack.seq, chrono::Utc::now()))
        .collect(),
    )
  }
}
