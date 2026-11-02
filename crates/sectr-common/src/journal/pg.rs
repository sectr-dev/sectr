//! Postgres journal — the production [`Journal`] implementation. Mirror of
//! `SqliteJournal` semantics with the
//! Postgres-native tail:
//!
//! - **seq assignment**: one append = one transaction holding a per-session
//!   advisory xact lock (`pg_advisory_xact_lock(hashtextextended(session))`)
//!   — the Postgres analog of SQLite's `BEGIN IMMEDIATE` writer
//!   serialization. `MAX(seq) + 1 …` inside the lock is gap-free;
//!   `UNIQUE(session_id, seq)` is the backstop.
//! - **dedup**: producer-minted UUIDv7 ids are the dedup key —
//!   `ON CONFLICT (id) DO NOTHING` + read-back returns originally stamped
//!   rows on replay (exactly-once reconnect replay), no seq movement.
//! - **tail**: `LISTEN sectr_events` (payload = session uuid) wakes a
//!   catch-up query; a slow fallback poll self-heals a missed/broken
//!   notification. Race design: the listener registers BEFORE the first
//!   catch-up query, so (a) any append committed before the query is in the
//!   query result, and (b) any append after the query commits its NOTIFY
//!   after registration — delivered and re-fetched. The two overlap windows
//!   are closed by dedup on `last_seq`; the append-between-notify-and-query
//!   race therefore yields exactly-once delivery (deliberately tested).
//! - Neon note: LISTEN/NOTIFY needs a DIRECT connection — poolers
//!   (PgBouncer transaction mode) break it. Open via the direct endpoint.

use super::record_stamped;
use super::{
  DeploymentId, EventSink, EventStream, JournalError, JournalReader, PG_NOTIFY_CHANNEL, Result,
  SessionId,
};
use crate::events::{Event, EventType, Extra, NewEvent};
use chrono::{DateTime, Utc};
use sqlx::postgres::PgListener;
use sqlx::{Connection, PgPool};
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

/// Fallback re-poll interval: a wake even if no NOTIFY arrives (protects a
/// dropped/expired listener connection; cost is one indexed query per tick
/// per active tail).
const FALLBACK_POLL: Duration = Duration::from_secs(1);

/// Postgres-backed [`Journal`]. Cheap to clone (the pool is a handle).
#[derive(Clone)]
pub struct PgJournal {
  pool: PgPool,
  /// Dedicated pool for journal TAILS: each `subscribe` holds one
  /// connection for `PgListener` for the stream's entire lifetime. This
  /// MUST NOT share the request pool — a handful of open tails (one per
  /// dashboard tab) would drain it and hang every authed request
  /// (found live in the M1 run: `pool timed out` WARNs, requests stuck).
  /// Drains (catch-up queries) still use the request pool — they are
  /// short-lived acquires, and the fallback poll bounds their rate at
  /// one per tail per second.
  tail_pool: PgPool,
}

impl PgJournal {
  /// Wrap existing pools — migrations already applied (open via
  /// [`crate::store::PgStore::open`]).
  pub fn new(pool: PgPool, tail_pool: PgPool) -> Self {
    PgJournal { pool, tail_pool }
  }

  /// The underlying pool (shared with [`crate::registry::PgRegistry`]).
  pub fn pool(&self) -> &PgPool {
    &self.pool
  }

  async fn fetch_inner(
    e: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
    session: SessionId,
    after_seq: Option<i64>,
  ) -> Result<Vec<Event>> {
    let rows = sqlx::query_as::<_, EventRow>(
      "SELECT id, session_id, deployment_id, seq, type, payload, extra, created_at \
             FROM events WHERE session_id = $1 AND seq > COALESCE($2::bigint, -1) ORDER BY seq ASC",
    )
    .bind(session)
    .bind(after_seq)
    .fetch_all(e)
    .await?;
    rows.into_iter().map(map_row).collect()
  }

  /// Append batch body — must run inside a transaction holding the
  /// session's advisory xact lock.
  async fn append_tx(
    conn: &mut sqlx::PgConnection,
    session: SessionId,
    deployment_id: DeploymentId,
    events: Vec<NewEvent>,
  ) -> Result<Vec<Event>> {
    // Serialize appends per session so consecutive `MAX(seq)+1` is
    // gap-free under concurrency (SQLite's BEGIN IMMEDIATE analog).
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))")
      .bind(session.to_string())
      .execute(&mut *conn)
      .await?;

    let head: Option<i64> = sqlx::query_scalar("SELECT MAX(seq) FROM events WHERE session_id = $1")
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

      let mut payload_value = serde_json::to_value(&payload)?;
      if !payload_value.is_object() {
        return Err(JournalError::Conflict(format!(
          "payload must serialize to an object, got {payload_value}"
        )));
      }
      escape_nul(&mut payload_value);
      let mut extra_value = serde_json::to_value(&extra.map)?;
      escape_nul(&mut extra_value);

      let inserted = sqlx::query(
        "INSERT INTO events (id, session_id, deployment_id, seq, type, payload, extra, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (id) DO NOTHING",
      )
      .bind(id)
      .bind(session)
      .bind(deployment_id)
      .bind(next_seq)
      .bind(event_type.name())
      .bind(payload_value)
      .bind(extra_value)
      .bind(created_at)
      .execute(&mut *conn)
      .await?;

      if inserted.rows_affected() == 0 {
        // Already journaled (reconnect replay): return the originally
        // stamped row — exactly-once, no duplicate, no seq movement.
        let existing = sqlx::query_as::<_, EventRow>(
          "SELECT id, session_id, deployment_id, seq, type, payload, extra, created_at \
                     FROM events WHERE id = $1",
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

  /// One fetch-and-drain step of the tail loop: catch-up query, then send
  /// everything past `last_seq`. Returns the advanced seq.
  /// Catch-up poll. Returns `false` only when the consumer dropped the
  /// tail. A BUSY request pool (no connection free right now) is not an
  /// error — the tail stays live (the listener still buffers NOTIFYs and
  /// the fallback poll retries); ending the stream here turned transient
  /// pool pressure into a full tail teardown cascade (M1 run finding).
  async fn drain(
    pool: &PgPool,
    session: SessionId,
    last_seq: &mut i64,
    tx: &tokio::sync::mpsc::Sender<Result<Event>>,
  ) -> bool {
    // sqlx 0.8: try_acquire is Option-valued (None = no connection
    // free right now).
    let mut conn = match pool.try_acquire() {
      Some(conn) => conn,
      None => return true, // pool busy — skip this poll; next wake retries
    };
    match Self::fetch_inner(&mut *conn, session, Some(*last_seq)).await {
      Ok(events) => {
        for event in events {
          *last_seq = event.seq;
          if tx.send(Ok(event)).await.is_err() {
            return false; // consumer dropped the tail
          }
        }
        true
      }
      Err(err) => {
        let _ = tx.send(Err(err)).await;
        false
      }
    }
  }
}

#[async_trait::async_trait]
impl EventSink for PgJournal {
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
    let mut tx = conn.begin().await?;
    let result = Self::append_tx(&mut tx, session, deployment_id, events).await;
    match result {
      Ok(stamped) => {
        tx.commit().await?;
        record_stamped(&span, &stamped);
        // Wake tails after the commit is visible. pg_notify inside
        // the transaction would fire at commit anyway, but doing it
        // after keeps the ordering unambiguous (never notify for an
        // uncommitted batch).
        sqlx::query("SELECT pg_notify($1, $2)")
          .bind(PG_NOTIFY_CHANNEL)
          .bind(session.to_string())
          .execute(&mut *conn)
          .await?;
        Ok(stamped)
      }
      Err(err) => {
        let _ = tx.rollback().await;
        Err(err)
      }
    }
  }
}

#[async_trait::async_trait]
impl JournalReader for PgJournal {
  async fn fetch(&self, session: SessionId, after_seq: Option<i64>) -> Result<Vec<Event>> {
    Self::fetch_inner(&self.pool, session, after_seq).await
  }

  async fn subscribe(&self, session: SessionId, after_seq: Option<i64>) -> Result<EventStream> {
    let tail_pool = self.tail_pool.clone();
    let pool = self.pool.clone();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event>>(64);

    tokio::spawn(async move {
      // Dedicated connection for LISTEN (PgListener holds its own);
      // register BEFORE the first catch-up query — that ordering is
      // what closes the notify-vs-catchup race (see module docs).
      let mut listener = match PgListener::connect_with(&tail_pool).await {
        Ok(listener) => listener,
        Err(err) => {
          let _ = tx.send(Err(err.into())).await;
          return;
        }
      };
      if let Err(err) = listener.listen(PG_NOTIFY_CHANNEL).await {
        let _ = tx.send(Err(err.into())).await;
        return;
      }

      let mut last_seq = after_seq.unwrap_or(-1);
      // A wake that fires even without NOTIFY: self-heals a lost
      // listener and bounds staleness. Delayed ticks don't stack up.
      let mut fallback =
        tokio::time::interval_at(tokio::time::Instant::now() + FALLBACK_POLL, FALLBACK_POLL);
      fallback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

      loop {
        if !Self::drain(&pool, session, &mut last_seq, &tx).await {
          return;
        }
        tokio::select! {
            // Consumer dropped (client disconnected)? Release the
            // listener NOW. Without this branch a QUIET tail (turn
            // ended, journal silent) never sends again, never
            // notices the drop, and holds its connection forever —
            // one leaked PG connection per disconnected client,
            // which drained the pool over a debugging session
            // (M1 run: one tab + one curl at a time still leaked).
            biased;
            _ = tx.closed() => return,
            notification = listener.recv() => {
                match notification {
                    // Filter to our session: one channel serves all
                    // sessions' tails; the payload is the uuid.
                    Ok(notification)
                        if notification.payload() == session.to_string() => {}
                    Ok(_) => {}
                    Err(err) => {
                        let _ = tx.send(Err(err.into())).await;
                        return;
                    }
                }
            }
            _ = fallback.tick() => {}
        }
      }
    });

    Ok(Box::pin(ReceiverStream::new(rx)))
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
  let (id, session_id, deployment_id, seq, type_str, mut payload, mut extra, created_at) = row;
  unescape_nul(&mut payload);
  unescape_nul(&mut extra);
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

/// `jsonb` can't hold U+0000 in a string, and user messages, model output
/// and tool results can all contain one. So every string and object key in
/// `payload` and `extra` is stored escaped: NUL as `ESC 0`, and `ESC` itself
/// as `ESC ESC`. `fetch` returns them unchanged. `ESC` is a Unicode
/// noncharacter (reserved for a program's internal use), so almost every
/// string is stored as it is.
const ESC: char = '\u{FDD0}';
const ESC_LEN: usize = ESC.len_utf8();

fn escape_nul(value: &mut serde_json::Value) {
  rewrite_strings(value, &['\0', ESC], escape_str);
}

fn unescape_nul(value: &mut serde_json::Value) {
  rewrite_strings(value, &[ESC], unescape_str);
}

/// Rewrites, in place with `f`, every string and object key in `value`
/// that contains one of `special`.
fn rewrite_strings(value: &mut serde_json::Value, special: &[char], f: fn(&mut String)) {
  use serde_json::Value;
  match value {
    Value::String(s) => {
      if s.contains(special) {
        f(s);
      }
    }
    Value::Array(items) => {
      for item in items {
        rewrite_strings(item, special, f);
      }
    }
    Value::Object(map) => {
      let keys: Vec<String> = map
        .keys()
        .filter(|key| key.contains(special))
        .cloned()
        .collect();
      // All out before any goes back in: a rewritten key can equal one
      // that's still waiting to be rewritten.
      let entries: Vec<(String, Value)> = keys
        .into_iter()
        .map(|key| {
          let item = map.remove(&key).expect("a listed key");
          (key, item)
        })
        .collect();
      for (mut key, item) in entries {
        f(&mut key);
        map.insert(key, item);
      }
      for item in map.values_mut() {
        rewrite_strings(item, special, f);
      }
    }
    _ => {}
  }
}

fn escape_str(s: &mut String) {
  let mut from = 0;
  while let Some(found) = s[from..].find(['\0', ESC]) {
    let at = from + found;
    if s[at..].starts_with('\0') {
      s.replace_range(at..at + 1, "\u{FDD0}0");
      from = at + ESC_LEN + 1;
    } else {
      s.insert(at, ESC);
      from = at + 2 * ESC_LEN;
    }
  }
}

/// Every `ESC` starts an escape: `ESC 0` or `ESC ESC`.
fn unescape_str(s: &mut String) {
  let mut from = 0;
  while let Some(found) = s[from..].find(ESC) {
    let at = from + found;
    if s[at + ESC_LEN..].starts_with('0') {
      s.replace_range(at..at + ESC_LEN + 1, "\0");
      from = at + 1;
    } else {
      s.replace_range(at..at + ESC_LEN, "");
      from = at + ESC_LEN;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::events::{
    EventPayload, InvocationReason, InvocationStarted, MessageDelta, SessionCreated, SessionError,
    TypedDelta,
  };
  use crate::registry::Registry as _;
  use crate::test_pg::fresh_store;
  use serde_json::json;
  use tokio_stream::StreamExt;

  /// A fresh migrated store with org/app/deployment/session seeded via the
  /// Registry (the prod orchestration order). `None` = skip (no test PG).
  async fn journal() -> Option<(PgJournal, SessionId, DeploymentId)> {
    let store = fresh_store().await?;
    let registry = store.registry();
    let org = registry.create_org("test-org", "test-org").await.unwrap();
    let app = registry
      .create_app(crate::registry::NewApp {
        org_id: org.id,
        name: "test-app".into(),
        slug: "test-app".into(),
      })
      .await
      .unwrap();
    let deployment = registry
      .create_deployment(crate::registry::NewDeployment {
        app_id: app.id,
        version: 1,
        manifest: json!({"routes": [{"path": "/test"}]}),
      })
      .await
      .unwrap();
    let session = registry
      .create_session(crate::registry::NewSession {
        id: None,
        app_id: app.id,
        deployment_id: deployment.id,
        route: "/test".into(),
        origin: crate::api::Origin::Playground,
        external_user_id: None,
      })
      .await
      .unwrap();
    Some((store.journal(), session.id, deployment.id))
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
  async fn append_stamps_and_round_trips() {
    let Some((journal, session, deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    let stamped = journal.append(session, deployment, events).await.unwrap();

    assert_eq!(stamped.len(), 2);
    assert_eq!(stamped[0].seq, 0);
    assert_eq!(stamped[1].seq, 1);
    assert_eq!(stamped[0].deployment_id, deployment);
    for e in &stamped {
      assert_eq!(e.id.get_version_num(), 7);
    }

    let fetched = journal.fetch(session, None).await.unwrap();
    assert_eq!(fetched, stamped);
  }

  /// User messages, model replies and tool results can contain a NUL,
  /// which `jsonb` can't hold: it's stored escaped, and so is the escape
  /// character, wherever either appears.
  #[tokio::test]
  async fn nul_survives_the_round_trip() {
    let Some((journal, session, deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let tricky = json!({
      "key\u{0}with nul": "value\u{0}",
      "key\u{FDD0}with esc": ["\u{FDD0}0", "\u{FDD0}\u{FDD0}", "\u{FDD0}", "\u{0}\u{FDD0}\u{0}"],
    });
    let mut payloads = sample_payloads();
    payloads.truncate(1);
    payloads.push(EventPayload::MessageDelta(MessageDelta {
      message_id: "m1".into(),
      delta: TypedDelta::text("before\u{0}after"),
      extra: Extra {
        map: tricky.as_object().unwrap().clone(),
      },
    }));
    let mut events: Vec<NewEvent> = payloads.into_iter().map(NewEvent::new).collect();
    events[1].extra = Extra {
      map: tricky.as_object().unwrap().clone(),
    };
    let stamped = journal
      .append(session, deployment, events.clone())
      .await
      .unwrap();
    assert_eq!(journal.fetch(session, None).await.unwrap(), stamped);

    // A replayed append reads the stored row back, unescaped.
    let replayed = journal.append(session, deployment, events).await.unwrap();
    assert_eq!(replayed, stamped);
  }

  #[test]
  fn escaping_round_trips_any_string() {
    for s in [
      "",
      "plain",
      "\u{0}",
      "\u{FDD0}",
      "\u{FDD0}0",
      "\u{FDD0}\u{FDD0}",
      "a\u{0}\u{0}b\u{FDD0}c",
    ] {
      let mut value = json!({ s: [s, { s: s }] });
      let original = value.clone();
      escape_nul(&mut value);
      assert!(
        !value.to_string().contains("\\u0000"),
        "{s:?} left a NUL: {value}"
      );
      unescape_nul(&mut value);
      assert_eq!(value, original, "{s:?}");
    }
  }

  #[test]
  fn keys_that_escape_onto_each_other_survive() {
    // `a NUL` escapes to what `a ESC 0` was, which escapes on in turn.
    let original = json!({ "a\u{0}": 1, "a\u{FDD0}0": 2, "a\u{FDD0}\u{FDD0}0": 3 });
    let mut value = original.clone();
    escape_nul(&mut value);
    assert_eq!(
      value,
      json!({
        "a\u{FDD0}0": 1,
        "a\u{FDD0}\u{FDD0}0": 2,
        "a\u{FDD0}\u{FDD0}\u{FDD0}\u{FDD0}0": 3,
      })
    );
    unescape_nul(&mut value);
    assert_eq!(value, original);
  }

  #[tokio::test]
  async fn append_continues_seq_across_batches() {
    let Some((journal, session, deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

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

    let all = journal.fetch(session, None).await.unwrap();
    assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![0, 1, 2]);
  }

  #[tokio::test]
  async fn append_to_unknown_session_fails_loudly() {
    let Some((journal, _session, _deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let session = Uuid::now_v7();
    let err = journal
      .append(
        session,
        Uuid::now_v7(),
        vec![NewEvent::new(sample_payloads().pop().unwrap())],
      )
      .await
      .unwrap_err();
    // The events FK is what makes this loud — the journal itself does no
    // relational lookups.
    assert!(
      matches!(err, JournalError::Sql(_)),
      "expected an FK failure, got {err:?}"
    );
  }

  #[tokio::test]
  async fn replayed_events_are_deduped_by_id() {
    let Some((journal, session, deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    let first = journal
      .append(session, deployment, events.clone())
      .await
      .unwrap();

    // Simulate a reconnect replay of the SAME unacked events.
    let replayed = journal.append(session, deployment, events).await.unwrap();
    assert_eq!(replayed, first);

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
  async fn envelope_extras_survive_storage() {
    let Some((journal, session, deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    let mut event = NewEvent::new(EventPayload::MessageDelta(MessageDelta {
      message_id: "m1".into(),
      delta: TypedDelta::text("hi"),
      extra: Extra::default(),
    }));
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
  async fn fetch_after_seq_filters() {
    let Some((journal, session, deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    journal.append(session, deployment, events).await.unwrap();

    let tail = journal.fetch(session, Some(0)).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, 1);
  }

  #[tokio::test]
  async fn subscribe_replays_history_then_tail() {
    let Some((journal, session, deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    journal.append(session, deployment, events).await.unwrap();

    let mut stream = journal.subscribe(session, None).await.unwrap();

    // History arrives promptly.
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
      .await
      .expect("history event 0")
      .unwrap()
      .unwrap();
    assert_eq!(first.seq, 0);
    let second = tokio::time::timeout(Duration::from_secs(5), stream.next())
      .await
      .expect("history event 1")
      .unwrap()
      .unwrap();
    assert_eq!(second.seq, 1);

    // Live tail via NOTIFY — no polling help.
    journal
      .append_platform_event(
        session,
        deployment,
        EventPayload::SessionError(SessionError {
          message: "boom".into(),
          extra: Extra::default(),
        }),
      )
      .await
      .unwrap();
    let live = tokio::time::timeout(Duration::from_secs(5), stream.next())
      .await
      .expect("live event")
      .unwrap()
      .unwrap();
    assert_eq!(live.seq, 2);
    assert_eq!(live.event_type, EventType::SessionError);
  }

  #[tokio::test]
  async fn subscribe_after_seq_starts_at_head() {
    let Some((journal, session, deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let events: Vec<NewEvent> = sample_payloads().into_iter().map(NewEvent::new).collect();
    journal.append(session, deployment, events).await.unwrap();

    // Last-Event-ID semantics: seq 1 already seen -> nothing replayed.
    let mut stream = journal.subscribe(session, Some(1)).await.unwrap();
    journal
      .append_platform_event(
        session,
        deployment,
        EventPayload::SessionError(SessionError {
          message: "late".into(),
          extra: Extra::default(),
        }),
      )
      .await
      .unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), stream.next())
      .await
      .expect("live event")
      .unwrap()
      .unwrap();
    assert_eq!(next.seq, 2);
  }

  /// THE race test: an event appended while the tail is
  /// between its catch-up query and its recv must arrive exactly once.
  /// Deterministically unknowable as a single instant, so: hammer it — a
  /// tail subscribed with no history, then bursts of appends from several
  /// concurrent writers while the tail is still in its first
  /// query/recv cycles. Exactly-once, in seq order, is the contract.
  #[tokio::test]
  async fn tail_survives_append_during_catchup_exactly_once() {
    for _ in 0..10 {
      let Some((journal, session, deployment)) = journal().await else {
        eprintln!("skipping: no test Postgres (initdb not on PATH)");
        return;
      };

      // Subscribe BEFORE any append: first catch-up query sees nothing,
      // then appends race against the query/recv loop.
      let mut stream = journal.subscribe(session, None).await.unwrap();

      let writers: Vec<_> = (0..4)
        .map(|w| {
          let journal = journal.clone();
          tokio::spawn(async move {
            for i in 0..10 {
              journal
                .append_platform_event(
                  session,
                  deployment,
                  EventPayload::MessageDelta(MessageDelta {
                    message_id: format!("w{w}-m{i}"),
                    delta: TypedDelta::text("x"),
                    extra: Extra::default(),
                  }),
                )
                .await
                .unwrap();
            }
          })
        })
        .collect();

      // Drain exactly 40 events (seqs 0..=39), each exactly once.
      let mut seqs = Vec::new();
      while seqs.len() < 40 {
        let event = tokio::time::timeout(Duration::from_secs(10), stream.next())
          .await
          .expect("expected an event within 10s")
          .unwrap()
          .unwrap();
        seqs.push(event.seq);
      }
      let expected: Vec<i64> = (0..40).collect();
      assert_eq!(
        seqs, expected,
        "tail must deliver 0..=39 in order, exactly once"
      );

      for writer in writers {
        writer.await.unwrap();
      }
    }
  }

  /// Concurrent appends under the advisory lock stay gap-free and the
  /// unique(session_id, seq) constraint never trips.
  #[tokio::test]
  async fn concurrent_appends_are_gap_free() {
    let Some((journal, session, deployment)) = journal().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    let writers: Vec<_> = (0..8)
      .map(|w| {
        let journal = journal.clone();
        tokio::spawn(async move {
          for i in 0..25 {
            journal
              .append_platform_event(
                session,
                deployment,
                EventPayload::MessageDelta(MessageDelta {
                  message_id: format!("w{w}-m{i}"),
                  delta: TypedDelta::text("x"),
                  extra: Extra::default(),
                }),
              )
              .await
              .unwrap();
          }
        })
      })
      .collect();
    for writer in writers {
      writer.await.unwrap();
    }

    let all = journal.fetch(session, None).await.unwrap();
    let expected: Vec<i64> = (0..200).collect();
    assert_eq!(
      all.iter().map(|e| e.seq).collect::<Vec<_>>(),
      expected,
      "seqs must be 0..=199, gap-free"
    );
  }

  /// Workstream A's Neon verification: LISTEN/NOTIFY works
  /// on Neon's DIRECT endpoint (DATABASE_URL_UNPOOLED — poolers break
  /// LISTEN). `#[ignore]`d so it never runs in CI/default suites; run
  /// explicitly:
  ///
  ///   SECTR_NEON_URL=<unpooled-url> cargo test -p sectr-common -- --ignored neon
  ///
  /// Runs migrations on the branch database (idempotent), seeds via the
  /// Registry, proves the tail wakes on NOTIFY (not the 1s fallback poll,
  /// via a latency bound) and cleans up its rows.
  #[tokio::test]
  #[ignore]
  async fn neon_direct_endpoint_wakes_the_tail() {
    use crate::store::PgStore;

    let Some(url) = std::env::var("SECTR_NEON_URL")
      .ok()
      .filter(|s| !s.is_empty())
    else {
      eprintln!("skipping: SECTR_NEON_URL not set (Neon DATABASE_URL_UNPOOLED)");
      return;
    };

    let store = PgStore::open(&url).await.unwrap();
    let registry = store.registry();
    let org = registry
      .create_org("neon-verify", "neon-verify")
      .await
      .unwrap();
    let app = registry
      .create_app(crate::registry::NewApp {
        org_id: org.id,
        name: "neon-verify".into(),
        slug: "neon-verify".into(),
      })
      .await
      .unwrap();
    let deployment = registry
      .create_deployment(crate::registry::NewDeployment {
        app_id: app.id,
        version: 1,
        manifest: json!({"routes": [{"path": "/test"}]}),
      })
      .await
      .unwrap();
    let session = registry
      .create_session(crate::registry::NewSession {
        id: None,
        app_id: app.id,
        deployment_id: deployment.id,
        route: "/test".into(),
        origin: crate::api::Origin::Playground,
        external_user_id: None,
      })
      .await
      .unwrap();

    let journal = store.journal();
    let mut stream = journal.subscribe(session.id, None).await.unwrap();

    // Append AFTER subscribing (empty history — the wake must come from
    // NOTIFY, not from the initial catch-up query).
    let stamped = journal
      .append_platform_event(
        session.id,
        deployment.id,
        EventPayload::SessionError(SessionError {
          message: "neon verify".into(),
          extra: Extra::default(),
        }),
      )
      .await
      .unwrap();

    // Latency is measured from append completion: if NOTIFY did not
    // drive the wake, the 1s fallback poll would — anything under ~700ms
    // is notify-driven even on high-RTT links.
    let start = tokio::time::Instant::now();
    let received = tokio::time::timeout(Duration::from_secs(10), stream.next())
      .await
      .expect("tail never woke within 10s — LISTEN/NOTIFY is broken here")
      .unwrap()
      .unwrap();
    let wake = start.elapsed();
    assert_eq!(received.seq, stamped.seq);
    assert_eq!(received.id, stamped.id);
    println!("Neon direct LISTEN/NOTIFY wake latency: {wake:?}");
    assert!(
      wake < Duration::from_millis(700),
      "wake took {wake:?} — that is fallback-poll territory (1s), NOTIFY did not drive it"
    );

    // Cleanup: the session first (cascades its events), then the org
    // (cascades apps/deployments; no sessions left referencing them).
    sqlx::query("DELETE FROM sessions WHERE id = $1")
      .bind(session.id)
      .execute(store.pool())
      .await
      .unwrap();
    sqlx::query("DELETE FROM orgs WHERE id = $1")
      .bind(org.id)
      .execute(store.pool())
      .await
      .unwrap();
  }
}
