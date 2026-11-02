//! The expired-lease reconciler — the SOLE synthesizer of abandoned-turn
//! ends. When a runner stalls or dies, its ingest-driven refreshes stop and
//! its lease simply expires; this component is what notices, and it must
//! exist no matter how healthy the engine is, because the runner AND the
//! sidecar can be lost together (then nothing engine-side ever fires).
//!
//! Server-role, mode-neutral: dev runs it inside `sectr dev`; prod runs it
//! inside sectr-server with a [`ReclaimAbort`] hook wired to the ingest hub
//! (the WS `abort` frame). Lives here because it is generic over
//! `Registry`/`Journal` and every mode constructs the identical logic.
//!
//! Reclaim (there is no turn timeout, so this duty is not split anywhere else):
//! 1. **atomic release-if-expired** (`Registry::reclaim_expired_lease`) —
//!    the re-check that closes the heartbeat race: a lease extended between
//!    scan and reclaim is left alone (we lost the race; the turn lives);
//! 2. unless the journal already shows the turn ended (last event is
//!    `NO_MORE_ACTIONS` or `TOOL_APPROVAL_REQUIRED` — e.g. the sidecar's
//!    final batch raced the lease expiry), journal `SESSION_ERROR`
//!    ("turn abandoned: lease expired") + `NO_MORE_ACTIONS` via the
//!    journal-append path — the abandoned turn gets a clean end. The
//!    reconciler holds the RAW journal, deliberately not any
//!    lease-refreshing wrapper: it must never extend a lease.
//! 3. abort the turn holder: the [`ReclaimAbort`] hook (prod: `abort` frame
//!    to the sidecar's ingest connection — its in-flight append then fails
//!    and the engine task exits via `IngestFailed`; dev: nothing — the
//!    reconciled lease is gone, the engine's next append fails on the
//!    refresh gate).
//!
//! Session status is NOT touched: a failed turn does not end a session.
//! Liveness: a healthy turn never appears here — its events ARE the
//! heartbeat (ingest traffic refreshes the lease server-side).

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::events::{EventPayload, EventType, NewEvent, NoMoreActions, SessionError};
use crate::journal::Journal;

/// Notified when the reconciler takes a turn away from its holder. Prod:
/// the ingest hub sends the WS `abort` frame and drops the connection.
/// The hook must be cheap and non-blocking (fire-and-forget).
pub trait ReclaimAbort: Send + Sync {
  fn on_reclaim(&self, session_id: Uuid, lease_id: Uuid);
}

/// Scans for expired live leases and reclaims them. One per process (dev:
/// inside `sectr dev`; prod: one per server).
pub struct Reconciler {
  registry: Arc<dyn crate::registry::Registry>,
  /// The raw journal — the reconciler is the server role, so deliberately
  /// NOT a lease-refreshing wrapper (it must never extend a lease).
  journal: Arc<dyn Journal>,
  interval: Duration,
  aborter: Option<Arc<dyn ReclaimAbort>>,
}

impl Reconciler {
  pub fn new(
    registry: Arc<dyn crate::registry::Registry>,
    journal: Arc<dyn Journal>,
    interval: Duration,
  ) -> Self {
    Reconciler {
      registry,
      journal,
      interval,
      aborter: None,
    }
  }

  /// Attach the turn-holder abort hook (prod: ingest hub).
  pub fn with_aborter(mut self, aborter: Arc<dyn ReclaimAbort>) -> Self {
    self.aborter = Some(aborter);
    self
  }

  /// One scan pass; returns the expired leases found. Reclaim per the
  /// module doc.
  pub async fn scan_once(&self) -> crate::registry::Result<Vec<crate::registry::Lease>> {
    let expired = self.registry.expired_leases().await?;
    for lease in &expired {
      self.reclaim(lease).await;
    }
    Ok(expired)
  }

  /// Reclaim one expired lease (module doc for the full contract).
  async fn reclaim(&self, lease: &crate::registry::Lease) {
    // 1. Atomic re-check + release: the heartbeat race closer. Losing
    //    (None) means the lease was extended or released meanwhile —
    //    the turn lives; do nothing.
    let Some(lease) = (match self.registry.reclaim_expired_lease(lease.id).await {
      Ok(l) => l,
      Err(e) => {
        tracing::warn!(lease_id = %lease.id, error = %e, "reclaim release failed");
        return;
      }
    }) else {
      tracing::debug!(lease_id = %lease.id, "reclaim lost the race — lease still live");
      return;
    };

    // 2. Synthesize the abandoned turn's end unless the journal already
    //    has it (final batch raced the expiry).
    let last = self.journal.fetch(lease.session_id, None).await.ok();
    let already_ended = matches!(
      last
        .as_ref()
        .and_then(|events| events.last().map(|e| e.event_type)),
      Some(EventType::NoMoreActions | EventType::ToolApprovalRequired)
    );
    if !already_ended {
      let deployment_id = match last
        .as_ref()
        .and_then(|events| events.last().map(|e| e.deployment_id))
      {
        Some(d) => d,
        None => {
          tracing::warn!(
              lease_id = %lease.id,
              session = %lease.session_id,
              "abandoned lease has no journal events; cannot synthesize turn end"
          );
          return;
        }
      };
      if let Err(e) = self
        .journal
        .append(
          lease.session_id,
          deployment_id,
          vec![
            NewEvent::new(EventPayload::SessionError(SessionError {
              message: "turn abandoned: lease expired".into(),
              extra: Default::default(),
            })),
            NewEvent::new(EventPayload::NoMoreActions(NoMoreActions {
              extra: Default::default(),
            })),
          ],
        )
        .await
      {
        tracing::warn!(
            session = %lease.session_id,
            error = %e,
            "failed to journal the abandoned turn's end"
        );
      }
    }

    // 3. Abort the turn holder.
    if let Some(hook) = &self.aborter {
      hook.on_reclaim(lease.session_id, lease.id);
    }
    tracing::warn!(
        lease_id = %lease.id,
        session = %lease.session_id,
        runner = %lease.runner_id,
        "reclaimed expired lease (turn ended by the reconciler)"
    );
  }

  /// Run forever: scan every `interval`. Errors are logged and retried —
  /// the reconciler must never die because one scan failed.
  pub async fn run(self) {
    let mut ticker = tokio::time::interval(self.interval);
    loop {
      ticker.tick().await;
      match self.scan_once().await {
        Ok(leases) if leases.is_empty() => {}
        Ok(leases) => {
          tracing::warn!(count = leases.len(), "reclaimed expired leases");
        }
        Err(e) => {
          tracing::warn!(error = %e, "reconciler scan failed; retrying");
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::events::Event;
  use crate::journal::{EventSink, JournalReader, SqliteJournal};
  use crate::registry::{Registry, SqliteRegistry};
  use crate::store::SqliteStore;
  use std::sync::Mutex;

  struct World {
    pool: sqlx::SqlitePool,
    registry: SqliteRegistry,
    journal: SqliteJournal,
    session: uuid::Uuid,
    deployment: uuid::Uuid,
  }

  async fn world() -> World {
    let path = std::env::temp_dir()
      .join(format!("sectr-reconcile-{}.db", uuid::Uuid::now_v7()))
      .to_string_lossy()
      .into_owned();
    let store = SqliteStore::open(&path).await.unwrap();
    let session = uuid::Uuid::now_v7();
    let deployment = seed_session(store.pool(), session).await;
    World {
      pool: store.pool().clone(),
      registry: SqliteRegistry::new(store.pool().clone()),
      journal: SqliteJournal::new(store.pool().clone()),
      session,
      deployment,
    }
  }

  async fn seed_session(pool: &sqlx::SqlitePool, session: uuid::Uuid) -> uuid::Uuid {
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
            "INSERT INTO sessions (id, app_id, deployment_id, route, origin, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(session)
        .bind(app)
        .bind(deployment)
        .bind("/test")
        .bind("playground")
        .bind(&ts)
        .execute(pool)
        .await
        .unwrap();
    deployment
  }

  #[derive(Default)]
  struct AbortLog(Mutex<Vec<(uuid::Uuid, uuid::Uuid)>>);
  impl ReclaimAbort for AbortLog {
    fn on_reclaim(&self, session_id: Uuid, lease_id: Uuid) {
      self.0.lock().unwrap().push((session_id, lease_id));
    }
  }

  fn reconciler(w: &World, aborts: &std::sync::Arc<AbortLog>) -> Reconciler {
    Reconciler::new(
      Arc::new(SqliteRegistry::new(w.pool.clone())),
      Arc::new(SqliteJournal::new(w.pool.clone())),
      Duration::from_secs(15),
    )
    .with_aborter(aborts.clone())
  }

  /// Seed seq 0 (SESSION_CREATED) and claim a zero-TTL (already expired)
  /// lease on the session.
  async fn abandoned_turn(w: &World) -> crate::registry::Lease {
    w.journal
      .append_platform_event(
        w.session,
        w.deployment,
        EventPayload::SessionCreated(crate::events::SessionCreated {
          app_id: uuid::Uuid::now_v7(),
          route: "/test".into(),
          extra: Default::default(),
          external_user_id: None,
        }),
      )
      .await
      .unwrap();
    w.registry
      .claim_lease(w.session, "runner-1", None, chrono::Duration::zero())
      .await
      .unwrap()
  }

  #[tokio::test]
  async fn reclaim_ends_the_abandoned_turn_and_aborts_the_holder() {
    let w = world().await;
    let lease = abandoned_turn(&w).await;
    let aborts = Arc::new(AbortLog::default());
    let reconciler = reconciler(&w, &aborts);

    let found = reconciler.scan_once().await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, lease.id);

    // Journal: SESSION_ERROR + sentinel synthesized, lease released,
    // holder aborted.
    let events: Vec<Event> = JournalReader::fetch(&w.journal, w.session, None)
      .await
      .unwrap();
    let types: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
    assert_eq!(
      types[1..],
      [EventType::SessionError, EventType::NoMoreActions]
    );
    assert!(w.registry.live_lease(w.session).await.unwrap().is_none());
    assert_eq!(*aborts.0.lock().unwrap(), vec![(w.session, lease.id)]);
  }

  /// The heartbeat race the atomic `reclaim_expired_lease` closes: a
  /// lease extended between the scan and the reclaim must NOT be
  /// released (registry-level test — the interleaving is not reachable
  /// through `scan_once`). PG parity lives in registry::pg tests.
  #[tokio::test]
  async fn reclaim_release_is_atomic_over_expiry_and_liveness() {
    let w = world().await;
    w.journal
      .append_platform_event(
        w.session,
        w.deployment,
        EventPayload::SessionCreated(crate::events::SessionCreated {
          app_id: uuid::Uuid::now_v7(),
          route: "/test".into(),
          extra: Default::default(),
          external_user_id: None,
        }),
      )
      .await
      .unwrap();

    // Live (unexpired): untouched.
    let live = w
      .registry
      .claim_lease(w.session, "r", None, chrono::Duration::seconds(60))
      .await
      .unwrap();
    assert!(
      w.registry
        .reclaim_expired_lease(live.id)
        .await
        .unwrap()
        .is_none()
    );

    // Expired: reclaimed; a second attempt (already released): None.
    w.registry.release_lease(live.id).await.unwrap();
    let dying = w
      .registry
      .claim_lease(w.session, "r2", None, chrono::Duration::zero())
      .await
      .unwrap();
    assert!(
      w.registry
        .reclaim_expired_lease(dying.id)
        .await
        .unwrap()
        .is_some()
    );
    assert!(
      w.registry
        .reclaim_expired_lease(dying.id)
        .await
        .unwrap()
        .is_none()
    );

    // Expired-then-heartbeated: extended means NOT reclaimable.
    let extended = w
      .registry
      .claim_lease(w.session, "r3", None, chrono::Duration::zero())
      .await
      .unwrap();
    w.registry
      .heartbeat_lease(extended.id, chrono::Duration::seconds(30))
      .await
      .unwrap();
    assert!(
      w.registry
        .reclaim_expired_lease(extended.id)
        .await
        .unwrap()
        .is_none()
    );
    assert!(w.registry.live_lease(w.session).await.unwrap().is_some());
  }

  #[tokio::test]
  async fn reclaim_skips_synthesis_when_the_journal_already_ended_the_turn() {
    let w = world().await;
    let lease = abandoned_turn(&w).await;
    // The sidecar's final batch raced the expiry: the sentinel is
    // already in the journal.
    w.journal
      .append_platform_event(
        w.session,
        w.deployment,
        EventPayload::NoMoreActions(NoMoreActions {
          extra: Default::default(),
        }),
      )
      .await
      .unwrap();
    let aborts = Arc::new(AbortLog::default());
    let reconciler = reconciler(&w, &aborts);

    reconciler.scan_once().await.unwrap();
    let events: Vec<Event> = JournalReader::fetch(&w.journal, w.session, None)
      .await
      .unwrap();
    assert_eq!(events.len(), 2, "no duplicate sentinel synthesized");
    assert_eq!(*aborts.0.lock().unwrap(), vec![(w.session, lease.id)]);
  }
}
