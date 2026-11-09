//! SQLite journal — the `sectr dev` / platform-dev implementation of
//! [`Journal`]. The tail is polling (acceptable for dev); production is
//! `PgJournal` with LISTEN/NOTIFY (Phase 5).

use super::record_stamped;
use super::{DeploymentId, EventSink, EventStream, JournalError, JournalReader, Result, SessionId};
use crate::events::{Event, EventType, Extra, NewEvent};
use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

/// How often the polling tail re-queries for new events.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// SQLite-backed [`Journal`]. Cheap to clone via `Arc` by callers.
pub struct SqliteJournal {
  pool: SqlitePool,
}

impl SqliteJournal {
  /// Wrap an existing pool — migrations already applied and foreign keys
  /// enabled (open via [`crate::store::SqliteStore::open`]).
  pub fn new(pool: SqlitePool) -> Self {
    SqliteJournal { pool }
  }

  /// The underlying pool (shared with the future `Registry` impl).
  pub fn pool(&self) -> &SqlitePool {
    &self.pool
  }

  async fn fetch_inner(
    pool: &SqlitePool,
    session: SessionId,
    after_seq: Option<i64>,
  ) -> Result<Vec<Event>> {
    let rows = sqlx::query_as::<_, EventRow>(
      "SELECT id, session_id, deployment_id, seq, type, payload, extra, created_at \
             FROM events WHERE session_id = ? AND seq > COALESCE(?, -1) ORDER BY seq ASC",
    )
    .bind(session)
    .bind(after_seq)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(map_row).collect()
  }
}

#[async_trait::async_trait]
impl EventSink for SqliteJournal {
  async fn append(
    &self,
    session: SessionId,
    deployment_id: DeploymentId,
    events: Vec<NewEvent>,
  ) -> Result<Vec<Event>> {
    // One span per journal write batch — the writer's own view
    // of what landed (seq range + types), parented under whatever
    // caused the append (request span, ingest span, turn span).
    let span = tracing::info_span!(
        "journal.append",
        session_id = %session,
        count = events.len(),
        event_types = tracing::field::Empty,
        first_seq = tracing::field::Empty,
        last_seq = tracing::field::Empty,
    );
    span.record(
      "event_types",
      events
        .iter()
        .map(|e| e.event_type.to_string())
        .collect::<Vec<_>>()
        .join(","),
    );
    let _guard = span.enter();
    if events.is_empty() {
      return Ok(Vec::new());
    }

    let mut conn = self.pool.acquire().await?;
    let result = append_tx(&mut conn, session, deployment_id, events).await;
    match result {
      Ok(stamped) => {
        sqlx::query("COMMIT").execute(&mut *conn).await?;
        record_stamped(&span, &stamped);
        Ok(stamped)
      }
      Err(err) => {
        // Roll back; a rollback error must not mask the original.
        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        Err(err)
      }
    }
  }
}

#[async_trait::async_trait]
impl JournalReader for SqliteJournal {
  async fn fetch(&self, session: SessionId, after_seq: Option<i64>) -> Result<Vec<Event>> {
    Self::fetch_inner(&self.pool, session, after_seq).await
  }

  async fn subscribe(&self, session: SessionId, after_seq: Option<i64>) -> Result<EventStream> {
    let pool = self.pool.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event>>(64);

    tokio::spawn(async move {
      let mut last_seq = after_seq.unwrap_or(-1);
      loop {
        match Self::fetch_inner(&pool, session, Some(last_seq)).await {
          Ok(events) => {
            for event in events {
              last_seq = event.seq;
              if tx.send(Ok(event)).await.is_err() {
                return; // consumer dropped the tail
              }
            }
          }
          Err(err) => {
            let _ = tx.send(Err(err)).await;
            return;
          }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
      }
    });

    Ok(Box::pin(ReceiverStream::new(rx)))
  }

  async fn deployment_turn_starts(
    &self,
    deployment: DeploymentId,
    limit: i64,
  ) -> Result<Vec<super::TurnRow>> {
    // Both queries use idx_events_turn_starts.
    let since: Option<DateTime<Utc>> = sqlx::query_scalar(
      "SELECT MIN(created_at) FROM (SELECT created_at FROM events \
       WHERE deployment_id = ? AND type = 'INVOCATION_STARTED' \
       ORDER BY created_at DESC LIMIT ?) AS recent",
    )
    .bind(deployment)
    .bind(limit)
    .fetch_one(&self.pool)
    .await?;
    let Some(since) = since else {
      return Ok(Vec::new());
    };
    let rows = sqlx::query_as::<_, super::TurnRowParts>(
      "SELECT session_id, seq, type, payload, created_at FROM events \
       WHERE deployment_id = ? AND type IN ('INVOCATION_STARTED', 'SESSION_ERROR') \
       AND created_at >= ? ORDER BY created_at DESC, seq DESC",
    )
    .bind(deployment)
    .bind(since)
    .fetch_all(&self.pool)
    .await?;
    Ok(rows.into_iter().map(super::turn_row).collect())
  }
}

type EventRow = (
  Uuid,              // id
  Uuid,              // session_id
  Uuid,              // deployment_id
  i64,               // seq
  String,            // type
  serde_json::Value, // payload
  serde_json::Value, // extra
  DateTime<Utc>,     // created_at
);

fn map_row(row: EventRow) -> Result<Event> {
  let (id, session_id, deployment_id, seq, type_str, payload, extra, created_at) = row;
  let event_type: EventType = serde_json::from_value(serde_json::Value::String(type_str))
    .map_err(|e| JournalError::Corrupt(format!("unknown event type: {e}")))?;
  let payload =
    crate::events::EventPayload::from_parts(event_type, payload).map_err(JournalError::Corrupt)?;
  let extra = match extra {
    serde_json::Value::Object(map) => Extra { map },
    other => {
      return Err(JournalError::Corrupt(format!(
        "envelope extra is not an object: {other}"
      )));
    }
  };
  Ok(Event {
    id,
    session_id,
    deployment_id,
    seq,
    event_type,
    payload,
    created_at,
    extra,
  })
}

/// One append = one IMMEDIATE transaction: head read, then inserts with
/// strictly consecutive seqs. Producer-minted event ids are the dedup key —
/// an already-journaled id (reconnect replay) returns its originally stamped
/// row instead of a duplicate. UNIQUE(session_id, seq) makes any race fail
/// loudly; the events FK (foreign_keys ON) makes an unknown session fail
/// loudly. The journal performs no relational lookups.
async fn append_tx(
  conn: &mut sqlx::SqliteConnection,
  session: SessionId,
  deployment_id: DeploymentId,
  events: Vec<NewEvent>,
) -> Result<Vec<Event>> {
  sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;

  let head: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM events WHERE session_id = ?")
    .bind(session)
    .fetch_one(&mut *conn)
    .await?;
  let mut next_seq = head.map_or(0, |s| s + 1);
  let created_at = super::now();

  let mut stamped = Vec::with_capacity(events.len());
  for event in events {
    let NewEvent {
      id,
      event_type,
      payload,
      extra,
    } = event;

    if id.get_version_num() != 7 {
      return Err(JournalError::Conflict(format!(
        "event id must be UUIDv7 (producer-minted), got {id}"
      )));
    }

    let payload_value = serde_json::to_value(&payload)?;
    if !payload_value.is_object() {
      return Err(JournalError::Conflict(format!(
        "payload must serialize to an object, got {payload_value}"
      )));
    }

    let inserted = sqlx::query(
      "INSERT INTO events (id, session_id, deployment_id, seq, type, payload, extra, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO NOTHING",
    )
    .bind(id)
    .bind(session)
    .bind(deployment_id)
    .bind(next_seq)
    .bind(event_type.name())
    .bind(payload_value.to_string())
    .bind(serde_json::to_string(&extra.map)?)
    .bind(created_at)
    .execute(&mut *conn)
    .await?;

    if inserted.rows_affected() == 0 {
      // Already journaled (reconnect replay): return the originally
      // stamped row — exactly-once, no duplicate, no seq movement.
      let existing = sqlx::query_as::<_, EventRow>(
        "SELECT id, session_id, deployment_id, seq, type, payload, extra, created_at \
                 FROM events WHERE id = ?",
      )
      .bind(id)
      .fetch_one(&mut *conn)
      .await?;
      let existing = map_row(existing)?;
      if existing.session_id != session {
        return Err(JournalError::Conflict(format!(
          "event id {id} is already journaled under a different session"
        )));
      }
      stamped.push(existing);
    } else {
      stamped.push(Event {
        id,
        session_id: session,
        deployment_id,
        seq: next_seq,
        event_type,
        payload,
        created_at,
        extra,
      });
      next_seq += 1;
    }
  }
  Ok(stamped)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::events::InvocationReason;
  use crate::events::{EventPayload, InvocationStarted, MessageDelta, SessionCreated, TypedDelta};
  use serde_json::json;
  use tokio_stream::StreamExt;

  /// Fresh temp DB path (cleaned up lazily by the OS temp dir; WAL files
  /// are siblings).
  fn temp_db() -> String {
    std::env::temp_dir()
      .join(format!("sectr-journal-test-{}.db", Uuid::now_v7()))
      .to_string_lossy()
      .into_owned()
  }

  async fn journal() -> (SqliteJournal, String) {
    let path = temp_db();
    let store = crate::store::SqliteStore::open(&path).await.unwrap();
    (store.journal(), path)
  }

  /// Minimal registry rows so `events.session_id` FK resolves. The Registry
  /// implementation (next milestone) will replace the raw SQL here.
  #[tokio::test]
  async fn deployment_turn_starts_meets_the_contract() {
    let (journal, _path) = journal().await;
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    seed_session(&journal.pool, a).await;
    sqlx::query(
      "INSERT INTO sessions (id, app_id, deployment_id, route, created_at) \
       SELECT ?, app_id, deployment_id, route, created_at FROM sessions WHERE id = ?",
    )
    .bind(b)
    .bind(a)
    .execute(&journal.pool)
    .await
    .unwrap();
    crate::journal::turn_starts_contract(&journal, a, b).await;
  }

  async fn seed_session(pool: &SqlitePool, session: SessionId) -> Uuid {
    let org = Uuid::now_v7();
    let app = Uuid::now_v7();
    let deployment = Uuid::now_v7();
    let ts = Utc::now().to_rfc3339();
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

  fn sample_payloads() -> Vec<EventPayload> {
    vec![
      EventPayload::SessionCreated(SessionCreated {
        app_id: Uuid::now_v7(),
        route: "/test".into(),
        extra: Extra::default(),
        external_user_id: None,
      }),
      EventPayload::MessageDelta(MessageDelta {
        message_id: "m1".into(),
        delta: TypedDelta::text("hi"),
        extra: Extra::default(),
      }),
    ]
  }

  #[tokio::test]
  async fn migrations_create_all_tables() {
    let (journal, _path) = journal().await;
    let tables: Vec<(String,)> =
      sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .fetch_all(journal.pool())
        .await
        .unwrap();
    let names: Vec<&str> = tables.iter().map(|(n,)| n.as_str()).collect();
    for expected in [
      "orgs",
      "users",
      "apps",
      "api_keys",
      "deployments",
      "secrets",
      "sessions",
      "events",
      "leases",
    ] {
      assert!(names.contains(&expected), "missing table {expected}");
    }
  }

  #[tokio::test]
  async fn append_stamps_and_round_trips() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let deployment = seed_session(journal.pool(), session).await;

    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    let stamped = journal.append(session, deployment, events).await.unwrap();

    assert_eq!(stamped.len(), 2);
    assert_eq!(stamped[0].seq, 0);
    assert_eq!(stamped[1].seq, 1);
    assert_eq!(stamped[0].deployment_id, deployment);
    assert_ne!(stamped[0].id, stamped[1].id);
    for e in &stamped {
      assert_eq!(e.id.get_version_num(), 7);
    }

    // Full-envelope round trip through storage.
    let fetched = journal.fetch(session, None).await.unwrap();
    assert_eq!(fetched, stamped);
  }

  #[tokio::test]
  async fn append_continues_seq_across_batches() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let deployment = seed_session(journal.pool(), session).await;

    let payloads = sample_payloads();
    journal
      .append(
        session,
        deployment,
        vec![
          NewEvent::new(payloads[0].clone()),
          NewEvent::new(payloads[1].clone()),
        ],
      )
      .await
      .unwrap();

    let next = journal
      .append_platform_event(
        session,
        deployment,
        EventPayload::InvocationStarted(InvocationStarted {
          invocation_id: Uuid::now_v7(),
          route: "/test".into(),
          deployment_id: deployment,
          reason: InvocationReason::UserMessage {
            input: "hi".to_string().try_into().unwrap(),
            extra: Extra::default(),
          },
          extra: Extra::default(),
        }),
      )
      .await
      .unwrap();
    assert_eq!(next.seq, 2);
    assert_eq!(next.event_type, EventType::InvocationStarted);

    let all = journal.fetch(session, None).await.unwrap();
    assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![0, 1, 2]);
  }

  #[tokio::test]
  async fn append_to_unknown_session_fails_loudly() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let err = journal
      .append(
        session,
        Uuid::now_v7(),
        vec![NewEvent::new(sample_payloads().pop().unwrap())],
      )
      .await
      .unwrap_err();
    // The events FK (foreign_keys ON) is what makes this loud — the
    // journal itself does no relational lookups.
    assert!(
      matches!(err, JournalError::Sql(_)),
      "expected an FK failure, got {err:?}"
    );
  }

  #[tokio::test]
  async fn empty_append_is_ok() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let deployment = seed_session(journal.pool(), session).await;
    assert!(
      journal
        .append(session, deployment, vec![])
        .await
        .unwrap()
        .is_empty()
    );
  }

  #[tokio::test]
  async fn fetch_after_seq_filters() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let deployment = seed_session(journal.pool(), session).await;
    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    journal.append(session, deployment, events).await.unwrap();

    let tail = journal.fetch(session, Some(0)).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, 1);
  }

  #[tokio::test]
  async fn envelope_extras_survive_storage() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let deployment = seed_session(journal.pool(), session).await;

    let mut event = NewEvent::new(EventPayload::MessageDelta(MessageDelta {
      message_id: "m1".into(),
      delta: TypedDelta::text("hi"),
      extra: Extra::default(),
    }));
    // A future platform adds an envelope-level field; older readers must
    // round-trip it verbatim.
    event
      .extra
      .map
      .insert("future_envelope_field".into(), json!({"v": 1}));
    let stamped = journal
      .append(session, deployment, vec![event])
      .await
      .unwrap();
    let fetched = journal.fetch(session, None).await.unwrap();
    assert_eq!(fetched, stamped);
    assert_eq!(fetched[0].extra.map["future_envelope_field"]["v"], 1);
  }

  #[tokio::test]
  async fn replayed_events_are_deduped_by_id() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let deployment = seed_session(journal.pool(), session).await;

    // Fixed ids, as the producer mints them.
    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    let first = journal
      .append(session, deployment, events.clone())
      .await
      .unwrap();
    assert_eq!(first.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![0, 1]);

    // Simulate a reconnect replay of the SAME unacked events.
    let replayed = journal.append(session, deployment, events).await.unwrap();
    assert_eq!(replayed, first); // originally stamped rows, unchanged

    // No duplicates, no seq movement.
    let all = journal.fetch(session, None).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![0, 1]);

    // New events after a replay continue the sequence cleanly.
    let next = journal
      .append(
        session,
        deployment,
        vec![NewEvent::new(sample_payloads()[0].clone())],
      )
      .await
      .unwrap();
    assert_eq!(next[0].seq, 2);
  }

  #[tokio::test]
  async fn non_v7_producer_id_is_rejected() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let deployment = seed_session(journal.pool(), session).await;

    let v4 = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let event = NewEvent::new(sample_payloads()[0].clone()).with_id(v4);
    let err = journal
      .append(session, deployment, vec![event])
      .await
      .unwrap_err();
    assert!(
      matches!(err, JournalError::Conflict(ref m) if m.contains("UUIDv7")),
      "expected UUIDv7 rejection, got {err:?}"
    );
  }

  #[tokio::test]
  async fn subscribe_replays_history_then_tail() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let deployment = seed_session(journal.pool(), session).await;

    // History before subscribing.
    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    journal.append(session, deployment, events).await.unwrap();

    let mut stream = journal.subscribe(session, None).await.unwrap();

    // History arrives promptly.
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
      .await
      .expect("history event 0")
      .unwrap()
      .unwrap();
    assert_eq!(first.seq, 0);
    let second = tokio::time::timeout(Duration::from_secs(2), stream.next())
      .await
      .expect("history event 1")
      .unwrap()
      .unwrap();
    assert_eq!(second.seq, 1);

    // Live tail: append after subscribing, event arrives without polling help.
    tokio::time::sleep(POLL_INTERVAL * 3).await;
    journal
      .append_platform_event(
        session,
        deployment,
        EventPayload::SessionError(crate::events::SessionError {
          message: "boom".into(),
          extra: Extra::default(),
        }),
      )
      .await
      .unwrap();
    let live = tokio::time::timeout(Duration::from_secs(2), stream.next())
      .await
      .expect("live event")
      .unwrap()
      .unwrap();
    assert_eq!(live.seq, 2);
    assert_eq!(live.event_type, EventType::SessionError);
  }

  #[tokio::test]
  async fn subscribe_after_seq_starts_at_head() {
    let (journal, _path) = journal().await;
    let session = Uuid::now_v7();
    let deployment = seed_session(journal.pool(), session).await;
    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    journal.append(session, deployment, events).await.unwrap();

    // Last-Event-ID semantics: seq 1 already seen -> nothing replayed.
    let mut stream = journal.subscribe(session, Some(1)).await.unwrap();
    journal
      .append_platform_event(
        session,
        deployment,
        EventPayload::SessionError(crate::events::SessionError {
          message: "late".into(),
          extra: Extra::default(),
        }),
      )
      .await
      .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
      .await
      .expect("live event")
      .unwrap()
      .unwrap();
    assert_eq!(next.seq, 2);
  }
}
