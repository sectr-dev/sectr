//! The ingest-boundary liveness mechanism (dev): an `EventSink` decorator
//! that extends the turn's lease on every journal append.
//!
//! Position matters: this is the SERVER role's hook, not the engine's and
//! not the worker's. In prod the lease is refreshed when the server's WS
//! ingest handler *receives* an event; in dev there is no wire — the engine
//! runs in-process and the `dyn EventSink` handed to it IS the ingest path —
//! so the decorator attaches the identical hook (journal-write boundary) to
//! the sink. The engine never sees it: the decorator is constructed by the
//! dispatch layer and substituted for the raw sink in `TurnEngine::run`.
//!
//! Refresh order is deliberate — **heartbeat BEFORE delegating the append**:
//! if the lease was reclaimed while the turn ran (reconciler synthesized
//! `SESSION_ERROR` + sentinel), the heartbeat fails and the append fails,
//! so no runner event can land in the journal after the reconciler's
//! sentinel. (A tiny race remains — the reconciler may read `expires_at`
//! just before our heartbeat extends it — acceptable for MVP; the reconciler
//! re-checks liveness at reclaim time.)
//!
//! `append_platform_event` is a default trait method that dispatches to the
//! overridden `append`, so platform events refresh the lease too.

use std::sync::Arc;

use async_trait::async_trait;
use sectr_common::events::{Event, NewEvent};
use sectr_common::journal::{
  DeploymentId, EventSink, JournalError, Result as JournalResult, SessionId,
};
use sectr_common::registry::Registry;

/// Extends `lease_id`'s TTL by `ttl` on every successful journal append;
/// fails the append if the lease is no longer live (reclaimed/released —
/// the turn is over, the reconciler owns it now).
pub struct LeaseRefreshingSink {
  inner: Arc<dyn EventSink>,
  registry: Arc<dyn Registry>,
  lease_id: uuid::Uuid,
  ttl: chrono::Duration,
}

impl LeaseRefreshingSink {
  pub fn new(
    inner: Arc<dyn EventSink>,
    registry: Arc<dyn Registry>,
    lease_id: uuid::Uuid,
    ttl: chrono::Duration,
  ) -> Self {
    LeaseRefreshingSink {
      inner,
      registry,
      lease_id,
      ttl,
    }
  }
}

#[async_trait]
impl EventSink for LeaseRefreshingSink {
  async fn append(
    &self,
    session: SessionId,
    deployment_id: DeploymentId,
    events: Vec<NewEvent>,
  ) -> JournalResult<Vec<Event>> {
    // Liveness check first: a reclaimed lease must reject further
    // appends (the reconciler has already synthesized the turn end).
    self
      .registry
      .heartbeat_lease(self.lease_id, self.ttl)
      .await
      .map_err(|e| JournalError::Conflict(format!("lease not live: {e}")))?;
    self.inner.append(session, deployment_id, events).await
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use sectr_common::events::{EventPayload, MessageDelta, TypedDelta};
  use sectr_common::journal::SqliteJournal;
  use sectr_common::registry::SqliteRegistry;
  use sectr_common::store::SqliteStore;

  struct World {
    pool: sqlx::SqlitePool,
    registry: SqliteRegistry,
    session: uuid::Uuid,
    deployment: uuid::Uuid,
  }

  async fn world() -> World {
    let path = std::env::temp_dir()
      .join(format!("sectr-lease-sink-{}.db", uuid::Uuid::now_v7()))
      .to_string_lossy()
      .into_owned();
    let store = SqliteStore::open(&path).await.unwrap();
    let session = uuid::Uuid::now_v7();
    let deployment = seed_session(store.pool(), session).await;
    World {
      pool: store.pool().clone(),
      registry: store.registry(),
      session,
      deployment,
    }
  }

  /// Read a lease row directly (the Registry trait has no lease read —
  /// the reconciler will add the scan method it needs).
  async fn lease_stamps(
    pool: &sqlx::SqlitePool,
    lease_id: uuid::Uuid,
  ) -> (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) {
    let (heartbeat_at, expires_at): (String, String) =
      sqlx::query_as("SELECT heartbeat_at, expires_at FROM leases WHERE id = ?")
        .bind(lease_id)
        .fetch_one(pool)
        .await
        .unwrap();
    (
      chrono::DateTime::parse_from_rfc3339(&heartbeat_at)
        .unwrap()
        .into(),
      chrono::DateTime::parse_from_rfc3339(&expires_at)
        .unwrap()
        .into(),
    )
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

  fn one_event() -> NewEvent {
    NewEvent::new(EventPayload::MessageDelta(MessageDelta {
      message_id: "m1".into(),
      delta: TypedDelta::text("hi"),
      extra: Default::default(),
    }))
  }

  #[tokio::test]
  async fn appends_delegate_and_refresh_the_lease() {
    let w = world().await;
    let ttl = chrono::Duration::seconds(60);
    let lease = w
      .registry
      .claim_lease(w.session, "runner-1", None, ttl)
      .await
      .unwrap();
    let (h_before, e_before) = lease_stamps(&w.pool, lease.id).await;
    let sink = LeaseRefreshingSink::new(
      Arc::new(SqliteJournal::new(w.pool.clone())),
      Arc::new(SqliteRegistry::new(w.pool.clone())),
      lease.id,
      ttl,
    );
    sink
      .append(w.session, w.deployment, vec![one_event()])
      .await
      .unwrap();
    let (h_after, e_after) = lease_stamps(&w.pool, lease.id).await;

    // TTL pushed out and heartbeat stamped.
    assert!(e_after > e_before);
    assert!(h_after >= h_before);
  }

  #[tokio::test]
  async fn released_lease_rejects_appends() {
    let w = world().await;
    let lease = w
      .registry
      .claim_lease(w.session, "runner-1", None, chrono::Duration::seconds(60))
      .await
      .unwrap();
    let sink = LeaseRefreshingSink::new(
      Arc::new(SqliteJournal::new(w.pool.clone())),
      Arc::new(SqliteRegistry::new(w.pool.clone())),
      lease.id,
      chrono::Duration::seconds(60),
    );
    w.registry.release_lease(lease.id).await.unwrap();

    let err = sink
      .append(w.session, w.deployment, vec![one_event()])
      .await
      .unwrap_err();
    assert!(err.to_string().contains("lease not live"));
    // Nothing landed in the journal.
    let journal = SqliteJournal::new(w.pool.clone());
    use sectr_common::journal::JournalReader;
    assert!(journal.fetch(w.session, None).await.unwrap().is_empty());
  }

  #[tokio::test]
  async fn unknown_lease_rejects_appends() {
    let w = world().await;
    let sink = LeaseRefreshingSink::new(
      Arc::new(SqliteJournal::new(w.pool.clone())),
      Arc::new(SqliteRegistry::new(w.pool.clone())),
      uuid::Uuid::now_v7(),
      chrono::Duration::seconds(60),
    );
    let err = sink
      .append(w.session, w.deployment, vec![one_event()])
      .await
      .unwrap_err();
    assert!(err.to_string().contains("lease not live"));
  }

  #[tokio::test]
  async fn default_platform_event_path_refreshes_too() {
    let w = world().await;
    let ttl = chrono::Duration::seconds(60);
    let lease = w
      .registry
      .claim_lease(w.session, "runner-1", None, ttl)
      .await
      .unwrap();
    let sink = LeaseRefreshingSink::new(
      Arc::new(SqliteJournal::new(w.pool.clone())),
      Arc::new(SqliteRegistry::new(w.pool.clone())),
      lease.id,
      ttl,
    );

    let (_, e_before) = lease_stamps(&w.pool, lease.id).await;
    sink
      .append_platform_event(
        w.session,
        w.deployment,
        EventPayload::InvocationStarted(sectr_common::events::InvocationStarted {
          invocation_id: uuid::Uuid::now_v7(),
          route: "/test".into(),
          deployment_id: w.deployment,
          reason: sectr_common::events::InvocationReason::UserMessage {
            input: "hi".to_string().try_into().unwrap(),
            extra: Default::default(),
          },
          extra: Default::default(),
        }),
      )
      .await
      .unwrap();
    let (_, e_after) = lease_stamps(&w.pool, lease.id).await;
    assert!(e_after > e_before);
  }
}
