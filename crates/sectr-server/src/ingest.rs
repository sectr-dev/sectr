//! Contract-4 ingest: the sidecar-dialed WebSocket, the ingest hub,
//! and the [`SidecarDispatcher`] that fills the dispatch seam.
//!
//! Protocol (`schema/sidecar.schema.json`):
//! one connection per sidecar container, dialed by the sidecar with a
//! per-turn ingest token on the upgrade request. No heartbeat frames
//! — **ingest traffic IS the lease heartbeat**: every `event` batch is
//! gated on `heartbeat_lease` BEFORE the journal append (the prod mirror
//! of `LeaseRefreshingSink`'s hook point), so a reclaimed lease rejects
//! the batch with `abort` and no runner event can land after the
//! reconciler's sentinel. Turn end follows the single journal rule:
//! appending a sentinel / `TOOL_APPROVAL_REQUIRED` releases the lease
//! right there — `complete` is bookkeeping, not the end signal.
//!
//! The ingest token is PER-TURN (invocation-scoped, from the always-terminate
//! policy): every
//! turn boundary drops the grant, so each dispatch mints a fresh token and
//! spawns a fresh process. MID-TURN reconnects replay with the SAME
//! turn's token (the grant lives while the turn is live), so exactly-once
//! replay is unaffected. The token never leaves the container env and
//! dies with the turn boundary.
//!
//! Reconnect semantics: the sidecar re-dials with the same token, sends
//! `ready`, and replays its unacked in-flight batch (same producer-minted
//! ids); the journal dedups by id and the ack carries the originally
//! stamped seqs with `duplicate: true`. The hub keeps a per-session
//! acked-id→seq map as the fast path (in-memory, MVP); the journal's own
//! dedup is the correctness backstop.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};

use axum::http::{HeaderMap, StatusCode, header::AUTHORIZATION};
use axum::response::{IntoResponse, Response};
use base64::Engine;

use tokio::sync::Mutex;
use tokio::sync::mpsc;
use uuid::Uuid;

use sectr_common::events::{Event, EventPayload, EventType};
use sectr_common::journal::Journal;
use sectr_common::reconcile::ReclaimAbort;
use sectr_common::registry::{Registry, RegistryError};
use sectr_common::sidecar::{
  AckEntry, Assignment, CompleteOutcome, INGEST_TOKEN_PREFIX, RunnerSpec, ServerFrame, SidecarFrame,
};
use tracing::Instrument;

use crate::auth::keys;
use crate::dispatch::DispatchError;
use crate::state::AppState;

fn mint_ingest_token() -> String {
  let mut buf = [0u8; 32];
  getrandom::getrandom(&mut buf).expect("OS entropy");
  format!(
    "{INGEST_TOKEN_PREFIX}{}",
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
  )
}

// ---------------------------------------------------------------------------
// IngestHub
// ---------------------------------------------------------------------------

/// The server-side Contract-4 state: grants (token → pending turn), live
/// sidecar connections, and the per-session acked-id memory for reconnect
/// replay. One hub per process (in-memory: a server restart orphans all
/// containers — their leases expire and the reconciler ends their turns;
/// accepted for now).
pub struct IngestHub {
  registry: Arc<dyn Registry>,
  journal: Arc<dyn Journal>,
  /// Lease TTL applied by ingest-driven heartbeats.
  ttl: chrono::Duration,
  inner: Mutex<HubInner>,
  /// Optional container teardown. `None` in tests that don't care.
  teardown: std::sync::RwLock<Option<std::sync::Arc<dyn crate::platform::RunnerPlatform>>>,
}

struct HubInner {
  /// One grant per session: the ingest token hash, the CURRENT turn's
  /// lease (None between turns), and an undelivered assignment.
  grants: HashMap<Uuid, Grant>,
  by_token: HashMap<String, Uuid>,
  /// Live sidecar connections (the handler's inbound frame queue).
  conns: HashMap<Uuid, mpsc::Sender<ServerFrame>>,
  /// session → (event id → stamped seq): reconnect-replay fast path.
  acked: HashMap<Uuid, HashMap<Uuid, i64>>,
  /// Per-session ingest serialization (batch order guarantee).
  locks: HashMap<Uuid, Arc<Mutex<()>>>,
  /// Registration tokens (token hash → probe): single-use, short-lived,
  /// belonging to no session. See [`IngestHub::register_probe`].
  probes: HashMap<String, Probe>,
}

/// An outstanding registration token: the dial an endpoint's sidecar makes
/// when asked for its manifest, which proves it reaches this server.
struct Probe {
  expires_at: tokio::time::Instant,
  dialed: tokio::sync::watch::Sender<bool>,
}

struct Grant {
  token_hash: String,
  deployment_id: Uuid,
  lease_id: Option<Uuid>,
  assignment: Option<Assignment>,
  /// The platform's OPAQUE handle for the CURRENT invocation's process
  /// (Cloud Run: the execution resource name; docker: the container name).
  /// Consumed by the boundary teardown. Lives exactly as long as the turn.
  execution: Option<String>,
  /// Set once the sidecar has dialed and taken this grant's assignment.
  /// The endpoint platform waits on it to know a wake was delivered.
  delivered: tokio::sync::watch::Sender<bool>,
}

/// A platform that accepts every spawn and streams the minted ingest
/// tokens in dispatch order — test support (the `DockerPlatform` is
/// the prod impl; protocol/e2e tests use this to play the container).
/// With the teardown-at-turn-end policy, a multi-turn session is
/// SPAWNED AGAIN per turn — consumers take tokens sequentially. Build and
/// teardown are stubs: these tests never build and use their own
/// recording teardown where it matters.
#[derive(Clone)]
pub struct AcceptedSpawner {
  tokens_tx: tokio::sync::mpsc::UnboundedSender<String>,
  tokens_rx: Arc<tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>>,
}

impl Default for AcceptedSpawner {
  fn default() -> Self {
    Self::new()
  }
}

impl AcceptedSpawner {
  pub fn new() -> Self {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    AcceptedSpawner {
      tokens_tx: tx,
      tokens_rx: Arc::new(tokio::sync::Mutex::new(Some(rx))),
    }
  }

  /// The next minted token, in dispatch order.
  pub async fn next_token(&self) -> Option<String> {
    self.tokens_rx.lock().await.as_mut()?.recv().await
  }
}

#[async_trait::async_trait]
impl crate::platform::RunnerPlatform for AcceptedSpawner {
  async fn build(
    &self,
    _req: crate::platform::BuildRequest,
  ) -> Result<crate::platform::BuildOutput, crate::platform::BuildError> {
    Err(crate::platform::BuildError::bare(
      "AcceptedSpawner is a test fake — it never builds",
    ))
  }

  fn provisional_build_ref(&self, _deployment_id: Uuid, _version: i64) -> String {
    String::new()
  }

  async fn spawn(
    &self,
    _session: Uuid,
    _deployment_id: Uuid,
    _invocation_id: Uuid,
    ingest_token: &str,
  ) -> Result<Option<String>, DispatchError> {
    let _ = self.tokens_tx.send(ingest_token.to_owned());
    Ok(None)
  }

  async fn teardown(&self, _session: Uuid, _handle: Option<&str>) {}
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
  #[error("ingest token invalid")]
  Unauthorized,
  #[error("the session's turn is not live (never started, ended, or reclaimed)")]
  TurnOver,
  #[error("lease lost (reclaimed or released): {0}")]
  LeaseLost(String),
  #[error("journal: {0}")]
  Journal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum IngestBatchError {
  #[error(transparent)]
  Ingest(#[from] IngestError),
}

/// The journal carried a turn boundary. Classification only — the
/// TERMINATION POLICY is [`IngestHub::on_turn_boundary`]
/// (both kinds terminate immediately; a future keep-warm policy swaps
/// that body, and THIS enum is what it switches on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnBoundary {
  /// `NO_MORE_ACTIONS` journaled — the turn ended, nothing pending.
  TurnEnded,
  /// `TOOL_APPROVAL_REQUIRED` journaled — the turn suspended on a gated
  /// tool; the resume is a fresh invocation with the decision as reason.
  Suspended,
}

impl IngestHub {
  pub fn new(
    registry: Arc<dyn Registry>,
    journal: Arc<dyn Journal>,
    ttl: chrono::Duration,
  ) -> Self {
    IngestHub {
      registry,
      journal,
      ttl,
      teardown: std::sync::RwLock::new(None),
      inner: Mutex::new(HubInner {
        grants: HashMap::new(),
        by_token: HashMap::new(),
        conns: HashMap::new(),
        acked: HashMap::new(),
        locks: HashMap::new(),
        probes: HashMap::new(),
      }),
    }
  }

  /// Attach the container-teardown hook. Idempotent setter.
  pub fn set_teardown(&self, teardown: std::sync::Arc<dyn crate::platform::RunnerPlatform>) {
    *self.teardown.write().expect("teardown lock") = Some(teardown);
  }

  /// The session's process is no longer wanted: drop the grant (the
  /// token dies with it — a lingering sidecar dials 401 and exits: the
  /// orphan self-cleanup path), detach any connection, and tear down the
  /// execution/container (the platform gets the grant's opaque handle —
  /// the Cloud Run execution name — for platforms that need it).
  pub async fn teardown_session(&self, session: Uuid) {
    let span = tracing::info_span!(
        "runner.teardown",
        session_id = %session,
        had_handle = tracing::field::Empty,
    );
    let _guard = span.enter();
    let handle = self.release_grant_and_connection(session).await;
    span.record("had_handle", handle.is_some());
    let hook = self.teardown.read().expect("teardown lock").clone();
    if let Some(hook) = hook {
      hook.teardown(session, handle.as_deref()).await;
    }
  }

  /// Drop the grant + connection WITHOUT the platform teardown call —
  /// the turn-boundary policy uses this where the process ends itself
  /// (sentinel) and no cancel should race it.
  async fn release_grant_and_connection(&self, session: Uuid) -> Option<String> {
    let mut inner = self.inner.lock().await;
    let removed = inner.grants.remove(&session);
    let handle = removed.as_ref().and_then(|g| g.execution.clone());
    if let Some(grant) = &removed {
      inner.by_token.remove(&grant.token_hash);
    }
    inner.conns.remove(&session);
    handle
  }

  /// THE turn-boundary policy (refined live, 2026-09-17):
  /// ONE decision point — "the invocation completed; what happens to the
  /// process?" The answer is UNIFORM across boundary kinds: the SIDECAR
  /// terminates itself (its post-turn policy — see
  /// sectr-worker/src/sidecar.rs `PostTurnPolicy` — exits at every
  /// boundary), so the server's default action is to drop the grant: the
  /// token dies, the connection detaches. NO platform cancel for
  /// SELF-TERMINATING platforms — canceling races the sidecar's natural
  /// exit and flips the Cloud Run execution to CANCELLED even though the
  /// turn completed (live-verified). For platforms whose runner resource
  /// does NOT end itself ([`RunnerPlatform::self_terminating`] = false —
  /// the E2B sandbox outlives its sidecar and bills until its timeout),
  /// the boundary policy additionally tears the resource down via the
  /// existing `teardown_session` (kill races
  /// nothing — the ack precedes this, and an E2B kill has no CANCELLED
  /// status semantics). Platform teardown otherwise remains for the
  /// EXCEPTIONAL paths: reconciler reclaim (HubAbort — a reclaimed lease
  /// always terminates) and first-spawn failure.
  /// A future keep-warm-for-X policy is a swap of this body (keep the
  /// grant, signal the sidecar to wait) + the sidecar's PostTurnPolicy
  /// (WaitForAssignment) + dial-time token auth.
  pub async fn on_turn_boundary(&self, session: Uuid, _boundary: TurnBoundary) {
    // Platform-capability introspection feeding the ONE policy body:
    // self-terminating runtimes only need the grant dropped; the rest
    // also get their resource torn down. No hook attached (dev/tests)
    // = default to self-terminating.
    let self_terminates = self
      .teardown
      .read()
      .expect("teardown lock")
      .as_ref()
      .is_none_or(|platform| platform.self_terminating());
    if self_terminates {
      tracing::debug!(session = %session, "turn boundary: releasing the grant");
      self.release_grant_and_connection(session).await;
    } else {
      tracing::info!(
          session = %session,
          "turn boundary: platform resource is not self-terminating — tearing it down"
      );
      self.teardown_session(session).await;
    }
  }

  /// Register a turn's dispatch: mint a FRESH session token when no
  /// LIVE grant exists (per-turn tokens: grants die at
  /// every turn boundary, so every turn is a fresh registration; the
  /// spawner injects the token into the process env), or update
  /// the grant for an existing LIVE one (double-dispatch protection /
  /// assignment replacement — no re-spawn). Returns the token when the
  /// caller must spawn (fresh registration OR a STALE leftover — see
  /// below).
  ///
  /// Stale-grant rule: a grant whose lease is already released (the
  /// boundary raced a dispatch — the lease is released inside
  /// [`Self::ingest_batch`] a moment before the grant is torn down) is
  /// a LEFTOVER, not a live container: the assignment would be pushed
  /// to a dying connection and stranded until lease expiry. Re-spawn
  /// over it instead.
  pub async fn register_turn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    lease_id: Uuid,
    assignment: Assignment,
  ) -> Option<String> {
    let mut inner = self.inner.lock().await;
    match inner.grants.get_mut(&session) {
      Some(grant) if grant.lease_id.is_some() => {
        grant.deployment_id = deployment_id;
        grant.lease_id = Some(lease_id);
        grant.assignment = Some(assignment);
        None
      }
      // No grant, or a stale leftover: fresh registration — mint a
      // new token and let the dispatcher spawn.
      _ => {
        let token = mint_ingest_token();
        let token_hash = keys::hash(&token);
        inner.by_token.insert(token_hash.clone(), session);
        inner.grants.insert(
          session,
          Grant {
            token_hash,
            deployment_id,
            lease_id: Some(lease_id),
            assignment: Some(assignment),
            execution: None,
            delivered: tokio::sync::watch::Sender::new(false),
          },
        );
        Some(token)
      }
    }
  }

  /// Record the platform handle returned by [`RunnerPlatform::spawn`]
  /// (Cloud Run: the execution resource name) on the CURRENT turn's
  /// grant — the boundary teardown consumes it. No-op when the grant is
  /// already gone (spawn raced a boundary: the execution gets orphaned
  /// and self-cleans via the 401 path).
  pub async fn store_execution(&self, session: Uuid, handle: Option<String>) {
    let mut inner = self.inner.lock().await;
    if let Some(grant) = inner.grants.get_mut(&session) {
      grant.execution = handle;
    }
  }

  /// A first-dispatch spawn failed: drop the grant so the next dispatch
  /// re-mints the token and retries the spawn (the container never
  /// started; the old token must not linger valid).
  pub async fn fail_first_spawn(&self, session: Uuid) {
    let mut inner = self.inner.lock().await;
    if let Some(grant) = inner.grants.remove(&session) {
      inner.by_token.remove(&grant.token_hash);
    }
  }

  /// Verify an ingest token and attach the connection. Replaces any
  /// existing connection for the session (the old one is stale — the
  /// sidecar reconnects after a drop).
  pub async fn attach(
    &self,
    token: &str,
    tx: mpsc::Sender<ServerFrame>,
  ) -> Result<(Uuid, bool), IngestError> {
    let hash = keys::hash(token);
    let mut inner = self.inner.lock().await;
    let session = *inner.by_token.get(&hash).ok_or(IngestError::Unauthorized)?;
    // Reconnect = a live connection is being replaced (the sidecar
    // redials after a drop and replays its in-flight batch).
    let reconnect = inner.conns.contains_key(&session);
    inner.conns.insert(session, tx);
    Ok((session, reconnect))
  }

  /// Drop the session's connection IF it is still `tx`'s — a replaced
  /// connection's exiting loop must not detach its replacement (the
  /// reconnect race: the old loop's exit fires after the new attach).
  pub async fn detach_if_current(&self, session: Uuid, tx: &mpsc::Sender<ServerFrame>) {
    let mut inner = self.inner.lock().await;
    if inner
      .conns
      .get(&session)
      .is_some_and(|current| current.same_channel(tx))
    {
      inner.conns.remove(&session);
    }
  }

  /// Pop the undelivered assignment (delivered once per dispatch — a
  /// reconnecting sidecar already knows what it was running; it replays
  /// events instead). Taking it is the delivery point: the grant's
  /// delivery watch flips to true.
  pub async fn take_assignment(&self, session: Uuid) -> Option<Assignment> {
    let mut inner = self.inner.lock().await;
    let grant = inner.grants.get_mut(&session)?;
    let assignment = grant.assignment.take()?;
    grant.delivered.send_replace(true);
    Some(assignment)
  }

  /// The delivery watch of the session's CURRENT grant, if it has one:
  /// true once a sidecar has dialed and taken the assignment. The sender
  /// dies with the grant, so a waiter sees the channel close if the turn
  /// ends or the grant is replaced first.
  pub async fn delivery_watch(&self, session: Uuid) -> Option<tokio::sync::watch::Receiver<bool>> {
    let inner = self.inner.lock().await;
    inner.grants.get(&session).map(|g| g.delivered.subscribe())
  }

  /// Wait until the session's current turn is delivered. False at the
  /// deadline, or when there is no grant or it goes away first.
  pub async fn wait_delivered(&self, session: Uuid, deadline: std::time::Duration) -> bool {
    let Some(mut watch) = self.delivery_watch(session).await else {
      return false;
    };
    matches!(
      tokio::time::timeout(deadline, watch.wait_for(|delivered| *delivered)).await,
      Ok(Ok(_))
    )
  }

  /// Mint a registration token: a `sit_` token that belongs to no session,
  /// is valid for `ttl` and works for ONE dial. A sidecar dialing with it
  /// is accepted and closed at once, and the returned watch flips to true.
  /// Endpoint registration hands it to the endpoint's `POST /manifest`, so
  /// the server learns from its own ingest, not the endpoint's word, that
  /// the endpoint's sidecar reaches this server.
  pub async fn register_probe(
    &self,
    ttl: std::time::Duration,
  ) -> (String, tokio::sync::watch::Receiver<bool>) {
    let token = mint_ingest_token();
    let (dialed, watch) = tokio::sync::watch::channel(false);
    let now = tokio::time::Instant::now();
    let mut inner = self.inner.lock().await;
    inner.probes.retain(|_, probe| probe.expires_at > now);
    inner.probes.insert(
      keys::hash(&token),
      Probe {
        expires_at: now + ttl,
        dialed,
      },
    );
    (token, watch)
  }

  /// Consume a registration token if `token` is one that is still valid:
  /// records the dial and removes it (single use). False for anything
  /// else, including an expired or already-used registration token.
  pub async fn take_probe(&self, token: &str) -> bool {
    let mut inner = self.inner.lock().await;
    match inner.probes.remove(&keys::hash(token)) {
      Some(probe) if probe.expires_at > tokio::time::Instant::now() => {
        probe.dialed.send_replace(true);
        true
      }
      _ => false,
    }
  }

  /// Push a frame to the sidecar's connection, if one is attached.
  /// A failed send (closed queue) detaches the connection.
  pub async fn push(&self, session: Uuid, frame: ServerFrame) {
    let tx = { self.inner.lock().await.conns.get(&session).cloned() };
    if let Some(tx) = tx
      && tx.send(frame).await.is_err()
    {
      self.inner.lock().await.conns.remove(&session);
    }
  }

  /// Ingest one `event` batch: heartbeat the lease FIRST (live-lease
  /// gate — ingest IS the heartbeat), then journal with journal-side
  /// dedup, then ack. Serialized per session (protocol: one batch in
  /// flight; a reconnect replay may race the original connection's
  /// in-flight batch).
  /// Returns `(ack_entries, boundary)` — `boundary` = `Some(kind)` when
  /// the journal now carries a turn boundary (the SENTINEL, or
  /// TOOL_APPROVAL_REQUIRED). The caller must run
  /// [`Self::on_turn_boundary`] AFTER the ack has been pushed (tearing
  /// down first would drop the ack: the conn lookup sees the emptied
  /// map).
  pub async fn ingest_batch(
    &self,
    session: Uuid,
    batch_id: Uuid,
    events: Vec<sectr_common::events::NewEvent>,
  ) -> Result<(Vec<AckEntry>, Option<TurnBoundary>), IngestError> {
    // One span per sidecar batch — the replay/dedup outcome is
    // the interesting part (duplicates vs fresh), plus the boundary it
    // produced (turn end / suspension).
    let span = tracing::info_span!(
        "ingest.batch",
        session_id = %session,
        batch_id = %batch_id,
        count = events.len(),
        duplicates = tracing::field::Empty,
        fresh = tracing::field::Empty,
        boundary = tracing::field::Empty,
    );
    let _span_guard = span.enter();
    // Snapshot under the lock: grant state + which events are known
    // replays. DB work happens outside the hub lock (per-session
    // serialization below), so concurrent sessions never queue on it.
    let (lease_id, deployment_id, fresh, mut ack_entries, lock) = {
      let mut inner = self.inner.lock().await;
      let (lease_id, deployment_id) = match inner.grants.get(&session) {
        Some(grant) => match grant.lease_id {
          Some(l) => (l, grant.deployment_id),
          None => return Err(IngestError::TurnOver),
        },
        None => return Err(IngestError::TurnOver),
      };
      let mut fresh = Vec::new();
      let mut ack_entries: Vec<AckEntry> = Vec::new();
      {
        let acked = inner.acked.entry(session).or_default();
        for event in events {
          match acked.get(&event.id) {
            Some(seq) => ack_entries.push(AckEntry {
              id: event.id,
              seq: *seq,
              duplicate: true,
            }),
            None => fresh.push(event),
          }
        }
      }
      let lock = inner.locks.entry(session).or_default().clone();
      (lease_id, deployment_id, fresh, ack_entries, lock)
    };
    span.record("duplicates", ack_entries.len());
    span.record("fresh", fresh.len());
    let _guard = lock.lock().await;

    // Heartbeat BEFORE append (LeaseRefreshingSink semantics): a lost
    // lease fails here — nothing lands after the reconciler's sentinel.
    // Only a not-live lease is `LeaseLost`; a DB failure (Neon down, pool
    // timeout) is `Journal`, so `ingest.failures{reason="journal"}` sees
    // an outage that trips the heartbeat before the append.
    self
      .registry
      .heartbeat_lease(lease_id, self.ttl)
      .await
      .map_err(|e| match e {
        RegistryError::Conflict(_) | RegistryError::NotFound(_) => {
          IngestError::LeaseLost(e.to_string())
        }
        other => IngestError::Journal(other.to_string()),
      })?;

    let stamped = if fresh.is_empty() {
      Vec::new()
    } else {
      self
        .journal
        .append(session, deployment_id, fresh)
        .await
        .map_err(|e| IngestError::Journal(e.to_string()))?
    };

    let mut boundary = None;
    let mut new_acks = Vec::with_capacity(stamped.len());
    for event in &stamped {
      // Turn end per the single journal rule — release the lease the
      // moment the journal carries it (a post-sentinel batch then
      // fails the heartbeat gate above).
      //
      // Boundary kinds (the TERMINATION POLICY lives in
      // [`Self::on_turn_boundary`]: BOTH kinds terminate
      // immediately; this fn only classifies):
      // - `NoMoreActions`: the turn ENDED (nothing pending).
      // - `ToolApprovalRequired`: the turn SUSPENDED on a gated tool
      //   (a pending action — the journal carries it; the resume is
      //   a fresh invocation with the decision as reason).
      //
      // Metric, moved here from `hub.complete`: the sidecar's `complete` frame loses the teardown
      // race — see the `TURNS` doc. The journal is the truth.
      if event.event_type == EventType::NoMoreActions {
        boundary = Some(TurnBoundary::TurnEnded);
        crate::metrics::TURNS.add(1, &[opentelemetry::KeyValue::new("outcome", "ended")]);
      } else if event.event_type == EventType::ToolApprovalRequired {
        boundary = Some(TurnBoundary::Suspended);
        crate::metrics::TURNS.add(1, &[opentelemetry::KeyValue::new("outcome", "suspended")]);
      }
      if event.event_type == EventType::SessionError {
        crate::metrics::SESSION_ERRORS.add(1, &[opentelemetry::KeyValue::new("source", "runner")]);
      }
      new_acks.push(AckEntry {
        id: event.id,
        seq: event.seq,
        duplicate: false,
      });
    }
    span.record(
      "boundary",
      match boundary {
        Some(TurnBoundary::TurnEnded) => "turn_ended",
        Some(TurnBoundary::Suspended) => "suspended",
        None => "none",
      },
    );

    {
      let mut inner = self.inner.lock().await;
      // Unconditional: every stamped event updates the replay fast
      // path (a replay is only detectable if the original ack is
      // recorded — not just at turn boundaries).
      let acked = inner.acked.entry(session).or_default();
      for event in &stamped {
        acked.insert(event.id, event.seq);
      }
      if boundary.is_some()
        && let Some(grant) = inner.grants.get_mut(&session)
      {
        grant.lease_id = None;
      }
    }
    if boundary.is_some()
      && let Err(e) = self.registry.release_lease(lease_id).await
    {
      tracing::warn!(session = %session, error = %e, "turn-end lease release failed");
    }

    ack_entries.extend(new_acks);
    tracing::debug!(session = %session, batch = %batch_id, count = ack_entries.len(), "batch ingested");
    Ok((ack_entries, boundary))
  }

  /// `complete` frame: the turn ended (the journal already carries the
  /// end — this is bookkeeping). Release the lease if it is somehow
  /// still live (idempotent safety net).
  ///
  /// Found in production: the `TURNS` metric used to live here, but
  /// this frame structurally loses the teardown race on non-
  /// self-terminating platforms (the sentinel ack detaches the socket
  /// and tears the sandbox down BEFORE the sidecar sends this frame) —
  /// prod never saw the metric. The metric now lives at the journal
  /// boundary in [`Self::ingest_batch`]; this handler keeps the lease
  /// safety net and the log (which only fires when the connection
  /// survives — dev/docker).
  pub async fn complete(&self, session: Uuid, outcome: CompleteOutcome, message: Option<String>) {
    let lease_id = {
      let mut inner = self.inner.lock().await;
      inner
        .grants
        .get_mut(&session)
        .and_then(|g| g.lease_id.take())
    };
    if let Some(lease_id) = lease_id
      && let Err(e) = self.registry.release_lease(lease_id).await
    {
      tracing::debug!(session = %session, error = %e, "complete: lease already released");
    }
    // Metric: see ingest_batch — moved to the journal boundary;
    // the frame cannot be trusted to arrive.
    tracing::info!(session = %session, outcome = ?outcome, message = message.as_deref(), "turn complete");
  }
}

/// The reconciler's turn-holder abort (the [`ReclaimAbort`] hook): tell the
/// sidecar its turn is gone and close the connection (its in-flight append
/// fails and the engine task exits via IngestFailed). Fire-and-forget — the
/// hook is sync. Newtype because `ReclaimAbort` must be implemented on a
/// local type, and the hub always lives in an `Arc`.
pub struct HubAbort(pub Arc<IngestHub>);

impl ReclaimAbort for HubAbort {
  fn on_reclaim(&self, session_id: Uuid, _lease_id: Uuid) {
    // Deliberately bypasses on_turn_boundary: a reclaimed lease ALWAYS
    // terminates (there is no keep-warm reading of a reclaim — the
    // turn is already abandoned), so the policy indirection would
    // only obscure that.
    //
    // Found in production: the sidecar's `complete` frame never
    // covers this path — the frame is skipped when the connection is
    // lost (IngestLost) and a lease expiry fires here, not at the
    // sidecar. Without this count, every abnormal turn ending was
    // invisible in `sectr.turns` (prod, 2026-09-25: the series was
    // absent entirely even though approvals ran).
    crate::metrics::TURNS.add(1, &[opentelemetry::KeyValue::new("outcome", "reclaimed")]);
    // The reconciler journals a SessionError for this reclaim
    // (sectr-common reconcile.rs); count it where the reclaim is observed.
    crate::metrics::SESSION_ERRORS.add(1, &[opentelemetry::KeyValue::new("source", "reclaim")]);
    let hub = self.0.clone();
    let reason = "turn abandoned: lease expired (reconciler reclaim)".to_string();
    tokio::spawn(async move {
      hub.push(session_id, ServerFrame::Abort { reason }).await;
      // The container is orphaned (its lease expired) — kill it.
      hub.teardown_session(session_id).await;
    });
  }
}

// ---------------------------------------------------------------------------
// WS endpoint
// ---------------------------------------------------------------------------

/// `GET /ingest` — the sidecar dials here with `Authorization: Bearer
/// sit_...`. Outside the platform/invocation auth middleware: the ingest
/// token is its own credential class.
pub async fn ingest_ws(
  State(state): State<AppState>,
  headers: HeaderMap,
  upgrade: WebSocketUpgrade,
) -> Response {
  let token = headers
    .get(AUTHORIZATION)
    .and_then(|v| v.to_str().ok())
    .and_then(|v| v.strip_prefix("Bearer "))
    .unwrap_or("");
  let hub = state.hub().clone();
  // A registration token: the dial IS the whole exchange. Accept the
  // upgrade (the sidecar must see it accepted) and close at once.
  if token.starts_with(INGEST_TOKEN_PREFIX) && hub.take_probe(token).await {
    tracing::info!("endpoint registration dial accepted");
    return upgrade.on_upgrade(|mut socket| async move {
      let _ = socket.send(Message::Close(None)).await;
    });
  }
  let (tx, rx) = mpsc::channel(64);
  let (session, reconnect) = match hub.attach(token, tx.clone()).await {
    Ok((session, reconnect)) if token.starts_with(INGEST_TOKEN_PREFIX) => (session, reconnect),
    _ => {
      ingest_failure("unauthorized");
      return StatusCode::UNAUTHORIZED.into_response();
    }
  };
  // Reconnects are the abnormal path (the sidecar redials and
  // replays); connects are normal churn per dial.
  if reconnect {
    crate::metrics::INGEST_RECONNECTS.add(1, &[]);
  }
  let connection_span = tracing::info_span!(
      "ingest.connection",
      session_id = %session,
      reconnect = reconnect,
  );

  // Deliver a pending assignment first (first dispatch before dial, or a
  // dispatch that landed while the sidecar was disconnected).
  if let Some(assignment) = hub.take_assignment(session).await {
    hub
      .push(
        session,
        ServerFrame::Assignment {
          assignment: Box::new(assignment),
        },
      )
      .await;
  }

  upgrade
    .on_upgrade(move |socket| connection(hub, session, socket, rx, tx).instrument(connection_span))
}

/// One connection's message loop: server frames out (assignment/ack/abort),
/// sidecar frames in. Exits on close/error or when the hub drops the
/// inbound queue (replaced by a reconnecting sidecar, or reconciler abort).
async fn connection(
  hub: Arc<IngestHub>,
  session: Uuid,
  mut socket: WebSocket,
  mut rx: mpsc::Receiver<ServerFrame>,
  tx: mpsc::Sender<ServerFrame>,
) {
  loop {
    tokio::select! {
        outbound = rx.recv() => match outbound {
            // Queue closed: the hub replaced or aborted this connection.
            None => break,
            Some(frame) => {
                let text = serde_json::to_string(&frame)
                    .expect("server frames serialize");
                if socket.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
        },
        inbound = socket.recv() => match inbound {
            None | Some(Err(_)) => break,
            Some(Ok(Message::Close(_))) => break,
            Some(Ok(Message::Text(text))) => {
                match handle_sidecar_frame(&hub, session, &text).await {
                    Flow::Continue => {}
                    Flow::Abort(reason) => {
                        // Directly on the socket — the loop (and with it
                        // the hub queue) ends right after.
                        let frame = serde_json::to_string(&ServerFrame::Abort { reason })
                            .expect("server frames serialize");
                        let _ = socket.send(Message::text(frame)).await;
                        break;
                    }
                }
            }
            Some(Ok(_)) => {} // ping/pong/binary: nothing to do
        },
    }
  }
  hub.detach_if_current(session, &tx).await;
}

enum Flow {
  Continue,
  /// Reject the batch and close (the sidecar's turn task ends via its
  /// failing append).
  Abort(String),
}

fn ingest_failure(reason: &'static str) {
  crate::metrics::INGEST_FAILURES.add(1, &[opentelemetry::KeyValue::new("reason", reason)]);
}

/// Unknown `kind` values are ignored (forward compat, per the schema).
async fn handle_sidecar_frame(hub: &IngestHub, session: Uuid, text: &str) -> Flow {
  let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
    tracing::warn!(session = %session, "unparseable sidecar frame (ignored)");
    return Flow::Continue;
  };
  match value.get("kind").and_then(|k| k.as_str()) {
    Some("ready") => Flow::Continue,
    Some("event") => match serde_json::from_value::<SidecarFrame>(value) {
      Ok(SidecarFrame::Event { batch_id, events }) => {
        match hub.ingest_batch(session, batch_id, events).await {
          Ok((entries, boundary)) => {
            hub
              .push(
                session,
                ServerFrame::Ack {
                  batch_id,
                  events: entries,
                },
              )
              .await;
            // Ack first, THEN apply the boundary policy:
            // teardown drops the grant and the
            // conn — inverted order silently eats the ack.
            if let Some(boundary) = boundary {
              hub.on_turn_boundary(session, boundary).await;
            }
            Flow::Continue
          }
          Err(IngestError::TurnOver) => {
            ingest_failure("turn_over");
            Flow::Abort("turn no longer live (ended, released, or reclaimed)".into())
          }
          Err(IngestError::LeaseLost(_)) => {
            ingest_failure("lease_lost");
            Flow::Abort("turn no longer live (ended, released, or reclaimed)".into())
          }
          Err(e @ IngestError::Journal(_)) => {
            ingest_failure("journal");
            Flow::Abort(e.to_string())
          }
          Err(IngestError::Unauthorized) => {
            ingest_failure("unauthorized");
            Flow::Abort("unauthorized".into())
          }
        }
      }
      // Malformed event frame or a kind mismatch: ignore (forward compat).
      _ => {
        tracing::warn!(session = %session, "invalid event frame (ignored)");
        Flow::Continue
      }
    },
    Some("complete") => match serde_json::from_value::<SidecarFrame>(value) {
      Ok(SidecarFrame::Complete { outcome, message }) => {
        hub.complete(session, outcome, message).await;
        Flow::Continue
      }
      // Malformed: ignore (forward compat).
      _ => {
        tracing::warn!(session = %session, "invalid complete frame (ignored)");
        Flow::Continue
      }
    },
    _ => Flow::Continue,
  }
}

// ---------------------------------------------------------------------------
// Platform seam
// ---------------------------------------------------------------------------

// The teardown + spawn seams are [`crate::platform::RunnerPlatform`] (one
// object per backend). The
// hub holds an `Arc<dyn RunnerPlatform>` for turn-boundary teardown and the
// dispatcher spawns through the same object. Test fakes below implement the
// trait instead of two separate seams.

/// The prod dispatcher: builds the assignment (transcript — already
/// including INVOCATION_STARTED per the ordering contract — resolved
/// secrets, runner spec from the manifest), registers the grant, spawns on
/// first dispatch, and delivers the assignment to a connected sidecar (or
/// leaves it pending for the dial).
pub struct SidecarDispatcher {
  hub: Arc<IngestHub>,
  registry: Arc<dyn Registry>,
  journal: Arc<dyn Journal>,
  platform: Arc<dyn crate::platform::RunnerPlatform>,
}

impl SidecarDispatcher {
  pub fn new(
    hub: Arc<IngestHub>,
    registry: Arc<dyn Registry>,
    journal: Arc<dyn Journal>,
    platform: Arc<dyn crate::platform::RunnerPlatform>,
  ) -> Self {
    SidecarDispatcher {
      hub,
      registry,
      journal,
      platform,
    }
  }
}

#[derive(Debug, thiserror::Error)]
enum AssignmentBuildError {
  #[error("manifest has no runner command (manifest.runner.command)")]
  NoRunnerCommand,
  #[error("registry: {0}")]
  Registry(String),
  #[error("journal: {0}")]
  Journal(String),
}

impl From<sectr_common::registry::RegistryError> for AssignmentBuildError {
  fn from(e: sectr_common::registry::RegistryError) -> Self {
    AssignmentBuildError::Registry(e.to_string())
  }
}

impl From<sectr_common::journal::JournalError> for AssignmentBuildError {
  fn from(e: sectr_common::journal::JournalError) -> Self {
    AssignmentBuildError::Journal(e.to_string())
  }
}

/// Extract the runner spec from the deployment manifest. Formal manifest
/// schema is the open decision; until then: `runner.command` (required),
/// `runner.workdir`, `runner.env`, `approval_tools` (all optional except
/// command).
fn assignment_parts_from_manifest(
  manifest: &serde_json::Value,
) -> Result<(RunnerSpec, Vec<String>), AssignmentBuildError> {
  let command: Vec<String> = manifest
    .get("runner")
    .and_then(|r| r.get("command"))
    .and_then(|c| serde_json::from_value(c.clone()).ok())
    .ok_or(AssignmentBuildError::NoRunnerCommand)?;
  let workdir = manifest
    .get("runner")
    .and_then(|r| r.get("workdir"))
    .and_then(|w| w.as_str())
    .map(str::to_owned);
  let env = manifest
    .get("runner")
    .and_then(|r| r.get("env"))
    .and_then(|e| serde_json::from_value(e.clone()).unwrap_or_default())
    .unwrap_or_default();
  let approval_tools = manifest
    .get("approval_tools")
    .and_then(|t| serde_json::from_value(t.clone()).unwrap_or_default())
    .unwrap_or_default();
  Ok((
    RunnerSpec {
      command,
      workdir,
      env,
    },
    approval_tools,
  ))
}

/// The INVOCATION_STARTED reason for this invocation (the transcript
/// carries it — journaled before dispatch).
fn invocation_reason(
  transcript: &[Event],
  invocation_id: Uuid,
) -> Option<sectr_common::events::InvocationReason> {
  transcript
    .iter()
    .rev()
    .find(|e| e.event_type == EventType::InvocationStarted)
    .filter(|e| match &e.payload {
      EventPayload::InvocationStarted(s) => s.invocation_id == invocation_id,
      _ => false,
    })
    .and_then(|e| match &e.payload {
      EventPayload::InvocationStarted(s) => Some(s.reason.clone()),
      _ => None,
    })
}

#[async_trait::async_trait]
impl crate::dispatch::TurnDispatcher for SidecarDispatcher {
  async fn dispatch_turn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    invocation_id: Uuid,
    route: &str,
  ) -> Result<(), DispatchError> {
    // The dispatch seam — assignment build + runner spawn +
    // grant registration, one span per turn dispatch (child of the
    // request span; parent of runner.spawn).
    let span = tracing::info_span!(
        "dispatch.turn",
        session_id = %session,
        deployment_id = %deployment_id,
        invocation_id = %invocation_id,
        route = %route,
        outcome = tracing::field::Empty,
    );
    let _guard = span.enter();
    let build = async {
      let transcript = self.journal.fetch(session, None).await?;
      let session_row = self.registry.get_session(session).await?;
      let deployment = self.registry.get_deployment(deployment_id).await?;
      let (runner, approval_tools) = assignment_parts_from_manifest(&deployment.manifest)?;
      let env: HashMap<String, String> = self
        .registry
        .resolve_secrets(session_row.app_id)
        .await?
        .into_iter()
        .map(|(k, v)| (k, v.expose().to_owned()))
        .collect();
      let reason = invocation_reason(&transcript, invocation_id).ok_or_else(|| {
        AssignmentBuildError::Journal(format!(
          "no INVOCATION_STARTED for invocation {invocation_id}"
        ))
      })?;
      let lease =
        self
          .registry
          .live_lease(session)
          .await?
          .ok_or(AssignmentBuildError::Registry(
            "no live lease at dispatch time".into(),
          ))?;
      Ok::<_, AssignmentBuildError>((
        Assignment {
          session_id: session,
          deployment_id,
          invocation_id,
          route: route.to_owned(),
          reason,
          transcript,
          env,
          runner,
          approval_tools,
        },
        lease.id,
      ))
    };
    let (assignment, lease_id) = build.await.map_err(|e| match e {
      AssignmentBuildError::NoRunnerCommand => {
        span.record("outcome", "spawn");
        DispatchError::Spawn(e.to_string())
      }
      other => {
        span.record("outcome", "unavailable");
        DispatchError::Unavailable(other.to_string())
      }
    })?;

    match self
      .hub
      .register_turn(session, deployment_id, lease_id, assignment.clone())
      .await
    {
      // Fresh registration (every turn: the grant
      // died at the last boundary): spawn the process — it dials
      // back and picks up the pending assignment.
      Some(token) => {
        let spawn_span = tracing::info_span!(
            "runner.spawn",
            session_id = %session,
            deployment_id = %deployment_id,
            invocation_id = %invocation_id,
            outcome = tracing::field::Empty,
        );
        let _spawn_guard = spawn_span.enter();
        let handle = self
          .platform
          .spawn(session, deployment_id, invocation_id, &token)
          .await;
        match handle {
          Ok(handle) => {
            spawn_span.record("outcome", "ok");
            // Carry the platform's opaque handle on the grant
            // (Cloud Run: the execution name; docker: the container
            // name — the boundary teardown uses it).
            self.hub.store_execution(session, handle).await;
          }
          Err(e) => {
            spawn_span.record("outcome", "failed");
            self.hub.fail_first_spawn(session).await;
            return Err(e);
          }
        }
        drop(_spawn_guard);
        span.record("outcome", "spawned");
      }
      // Existing grant (double-dispatch of a live turn): deliver
      // immediately if connected (else it stays pending for the
      // sidecar's next dial/ready).
      None => {
        self
          .hub
          .push(
            session,
            ServerFrame::Assignment {
              assignment: Box::new(assignment),
            },
          )
          .await;
        span.record("outcome", "delivered");
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod policy_tests;
#[cfg(test)]
mod tests;
