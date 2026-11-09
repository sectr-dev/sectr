//! `GET /deployments/{id}/wakes?limit=20`: how a deployment's recent turns
//! started, for the dashboard's health line on an endpoint deployment.
//!
//! A turn is an `INVOCATION_STARTED` on the deployment. It failed to wake
//! when the first `SESSION_ERROR` after it in the same session, before that
//! session's next turn, starts with [`WAKE_FAILED`]. Other session errors (an
//! agent that raised, a reclaimed lease) aren't wake failures. Read-only over
//! the journal; nothing is written.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use sectr_common::endpoint::WAKE_FAILED;
use sectr_common::journal::TurnRow;

use crate::auth::PlatformPrincipal;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

#[derive(Deserialize)]
pub(super) struct WakesQuery {
  limit: Option<i64>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct WakeSummary {
  /// Turns counted (at most the limit).
  pub turns: usize,
  /// Of those, the ones whose wake failed.
  pub failed: usize,
  /// The newest wake failure among them.
  pub latest_failure: Option<WakeFailure>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct WakeFailure {
  pub message: String,
  pub session_id: Uuid,
  pub created_at: DateTime<Utc>,
}

/// `GET /deployments/{deploymentId}/wakes`.
pub(super) async fn deployment_wakes(
  State(state): State<AppState>,
  principal: PlatformPrincipal,
  AxumPath(deployment_id): AxumPath<Uuid>,
  Query(query): Query<WakesQuery>,
) -> Response {
  let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
  if !(1..=MAX_LIMIT).contains(&limit) {
    return (
      StatusCode::BAD_REQUEST,
      format!("limit must be between 1 and {MAX_LIMIT}"),
    )
      .into_response();
  }
  // Org boundary, as activate checks it.
  let registry = state.registry();
  let Ok(deployment) = registry.get_deployment(deployment_id).await else {
    return StatusCode::NOT_FOUND.into_response();
  };
  match registry.get_app(deployment.app_id).await {
    Ok(app) if app.org_id == principal.0.org_id() => {}
    _ => return StatusCode::NOT_FOUND.into_response(),
  }
  match state
    .journal()
    .deployment_turn_starts(deployment_id, limit)
    .await
  {
    Ok(rows) => Json(summarize(rows, limit as usize)).into_response(),
    Err(e) => {
      tracing::error!(error = %e, "reading a deployment's turns failed");
      StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
  }
}

/// Pair each turn with its first following session error, then count the
/// newest `limit` turns. `rows` come newest first, as the journal returns
/// them.
pub(crate) fn summarize(rows: Vec<TurnRow>, limit: usize) -> WakeSummary {
  // Per session, oldest first.
  let mut by_session: HashMap<Uuid, Vec<TurnRow>> = HashMap::new();
  for row in rows {
    by_session.entry(row.session_id).or_default().push(row);
  }
  // (turn start, the wake failure that ended it if any)
  let mut turns: Vec<(TurnRow, Option<TurnRow>)> = Vec::new();
  for mut session_rows in by_session.into_values() {
    session_rows.sort_by_key(|row| row.seq);
    // The current turn, and whether a session error already answered it.
    let mut current: Option<(TurnRow, Option<TurnRow>, bool)> = None;
    for row in session_rows {
      match &row.error {
        None => {
          if let Some((start, failure, _)) = current.take() {
            turns.push((start, failure));
          }
          current = Some((row, None, false));
        }
        Some(message) => {
          // An error before any counted turn belongs to an older one.
          if let Some((_, failure, answered)) = current.as_mut()
            && !*answered
          {
            *answered = true;
            if message.starts_with(WAKE_FAILED) {
              *failure = Some(row);
            }
          }
        }
      }
    }
    if let Some((start, failure, _)) = current {
      turns.push((start, failure));
    }
  }
  // Newest first, then the limit.
  turns.sort_by(|(a, _), (b, _)| {
    (b.created_at, b.seq, b.session_id).cmp(&(a.created_at, a.seq, a.session_id))
  });
  turns.truncate(limit);
  let failures: Vec<&TurnRow> = turns.iter().filter_map(|(_, f)| f.as_ref()).collect();
  WakeSummary {
    turns: turns.len(),
    failed: failures.len(),
    latest_failure: failures
      .iter()
      .max_by_key(|f| (f.created_at, f.seq))
      .map(|f| WakeFailure {
        message: f.error.clone().unwrap_or_default(),
        session_id: f.session_id,
        created_at: f.created_at,
      }),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Rows built in time order; `summarize` gets them newest first.
  struct Journal {
    rows: Vec<TurnRow>,
    seqs: HashMap<Uuid, i64>,
    clock: i64,
  }

  impl Journal {
    fn new() -> Self {
      Journal {
        rows: Vec::new(),
        seqs: HashMap::new(),
        clock: 0,
      }
    }
    fn push(&mut self, session: Uuid, error: Option<&str>) -> &mut Self {
      let seq = self.seqs.entry(session).or_insert(0);
      *seq += 1;
      self.clock += 1;
      self.rows.push(TurnRow {
        session_id: session,
        seq: *seq,
        error: error.map(str::to_owned),
        created_at: DateTime::from_timestamp(1_800_000_000 + self.clock, 0).unwrap(),
      });
      self
    }
    fn turn(&mut self, session: Uuid) -> &mut Self {
      self.push(session, None)
    }
    fn error(&mut self, session: Uuid, message: &str) -> &mut Self {
      self.push(session, Some(message))
    }
    fn summary(&self, limit: usize) -> WakeSummary {
      summarize(self.rows.iter().rev().cloned().collect(), limit)
    }
  }

  fn wake_failed(detail: &str) -> String {
    format!("{WAKE_FAILED} {detail}")
  }

  #[test]
  fn twenty_turns_three_wake_failures_two_other_errors() {
    let sessions: Vec<Uuid> = (0..4).map(|_| Uuid::now_v7()).collect();
    let mut j = Journal::new();
    for i in 0..20 {
      let s = sessions[i % 4];
      j.turn(s);
      match i {
        3 | 9 => {
          j.error(s, &wake_failed(&format!("turn {i}")));
        }
        14 => {
          j.error(s, &wake_failed("newest"));
        }
        5 | 11 => {
          j.error(s, "the agent raised ValueError");
        }
        _ => {}
      }
    }
    let summary = j.summary(20);
    assert_eq!((summary.turns, summary.failed), (20, 3));
    let latest = summary.latest_failure.unwrap();
    assert_eq!(latest.message, wake_failed("newest"));
    assert_eq!(latest.session_id, sessions[14 % 4]);
  }

  #[test]
  fn a_failed_turn_followed_by_a_good_one_counts_once() {
    let s = Uuid::now_v7();
    let mut j = Journal::new();
    j.turn(s).error(s, &wake_failed("403")).turn(s);
    let summary = j.summary(20);
    assert_eq!((summary.turns, summary.failed), (2, 1));
  }

  #[test]
  fn only_the_first_error_after_a_turn_counts() {
    let s = Uuid::now_v7();
    let mut j = Journal::new();
    // An agent error, then (later, same turn) a wake-failure-shaped message:
    // the turn started, so it isn't a wake failure.
    j.turn(s)
      .error(s, "lease expired")
      .error(s, &wake_failed("x"));
    assert_eq!(j.summary(20).failed, 0);
  }

  #[test]
  fn the_limit_keeps_the_newest_turns() {
    let a = Uuid::now_v7();
    let b = Uuid::now_v7();
    let mut j = Journal::new();
    j.turn(a).error(a, &wake_failed("old")); // outside the limit
    j.turn(b).turn(a).turn(b);
    let summary = j.summary(3);
    assert_eq!((summary.turns, summary.failed), (3, 0));
    assert_eq!(summary.latest_failure, None);
    // An error whose turn is outside the rows (the journal cut it off).
    let mut cut = Journal::new();
    cut.turn(a).error(a, &wake_failed("x")).turn(a);
    let rows: Vec<TurnRow> = cut.rows.iter().skip(1).rev().cloned().collect();
    assert_eq!(summarize(rows, 20).failed, 0);
  }

  #[test]
  fn no_turns_is_all_zero() {
    assert_eq!(
      summarize(Vec::new(), 20),
      WakeSummary {
        turns: 0,
        failed: 0,
        latest_failure: None
      }
    );
  }
}
