//! OTel metric instruments. One module so every seam references
//! the same names — dashboards key off these.
//!
//! All instruments live behind `LazyLock` statics and go through
//! `opentelemetry::global::meter`: with the OTLP gate off, the
//! global meter is a no-op and every call site below costs a match arm —
//! same dev-keeps-its-exact-behavior philosophy as the spans.
//! The meter provider's PeriodicReader collects on its own 60s cadence;
//! adding to a counter is a lock-free add on the hot path.
//!
//! Naming: OTel convention, `sectr.*` namespace, dotted lowercase.
//! Attributes are low-cardinality ONLY (outcome, platform, route shape) —
//! no session ids, no app ids (cardinality explosions kill the free tier).

use std::sync::LazyLock;

use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};

const METER_NAME: &str = "sectr-server";

fn meter() -> opentelemetry::metrics::Meter {
  global::meter(METER_NAME)
}

/// `sectr.sessions.created` — every `POST /sessions` that resolves.
pub static SESSIONS_CREATED: LazyLock<Counter<u64>> = LazyLock::new(|| {
  meter()
    .u64_counter("sectr.sessions.created")
    .with_description("Sessions created (POST /sessions)")
    .build()
});

/// `sectr.turns` — turn outcomes at the JOURNAL boundary, by `outcome`
/// (ended / suspended / reclaimed).
///
/// Found in production (2026-09-25): the original call site was the
/// sidecar's `complete` WS frame, but that frame structurally loses the
/// teardown race on non-self-terminating platforms — the sidecar journals
/// the `NoMoreActions` sentinel FIRST (the server acks it, releases the
/// lease, and tears the sandbox down), THEN sends `Complete` into a
/// already-detached socket. Prod never saw the metric even though every
/// turn completed normally. The journal sentinel is the authoritative
/// end (the UI/journal always carries it), so the metric lives where the
/// end is journaled (`ingest_batch`).
pub static TURNS: LazyLock<Counter<u64>> = LazyLock::new(|| {
  meter()
    .u64_counter("sectr.turns")
    .with_description("Turn outcomes (ended/suspended/reclaimed)")
    .build()
});

/// `sectr.session_errors` — `SessionError` events journaled: a failed turn
/// still ends with the `NoMoreActions`
/// sentinel, so the turn counter alone cannot show the completed-vs-failed
/// split — the error events are the signal.
///
/// Attr `source` says who journaled it: `runner` (the agent
/// threw — noisy, high alert threshold), `dispatch` (the platform could not
/// start the turn: an E2B outage or capacity error), or `reclaim` (the
/// reconciler abandoned an expired lease). `dispatch` and `reclaim` are
/// platform health, not user code, so alerts treat them differently.
pub static SESSION_ERRORS: LazyLock<Counter<u64>> = LazyLock::new(|| {
  meter()
    .u64_counter("sectr.session_errors")
    .with_description("SessionError events journaled (failed turns), by source")
    .build()
});

/// `sectr.ingest.failures` — sidecar ingest rejections, attr
/// `reason` (unauthorized / turn_over / lease_lost / journal). `journal`
/// means a DB write at ingest failed (the lease heartbeat or the append) —
/// the one that should page. `lease_lost` is only a not-live lease (a
/// reclaim race), and `unauthorized` is mostly orphaned sidecars told to
/// exit (every in-flight sidecar after a server restart).
pub static INGEST_FAILURES: LazyLock<Counter<u64>> = LazyLock::new(|| {
  meter()
    .u64_counter("sectr.ingest.failures")
    .with_description("Sidecar ingest failures, by reason")
    .build()
});

/// `sectr.endpoint.wakes` — wakes of endpoint deployments, by `outcome`:
/// `delivered` (the sidecar took the turn), `connect_error`,
/// `http_error`, `deadline`, `blocked` (the address guard), and once a
/// delivered wake's request ends, `turn_ended`, `token_rejected` or
/// `broken_after_delivery`. URLs and hosts are never attributes.
pub static ENDPOINT_WAKES: LazyLock<Counter<u64>> = LazyLock::new(|| {
  meter()
    .u64_counter("sectr.endpoint.wakes")
    .with_description("Endpoint deployment wakes, by outcome")
    .build()
});

/// `sectr.approval.decision_latency` — milliseconds from the
/// TOOL_APPROVAL_REQUIRED/TOOL_CALL journal timestamp to the decision POST
/// landing (the human-in-the-loop wait). f64 histogram in ms.
pub static APPROVAL_DECISION_LATENCY: LazyLock<Histogram<f64>> = LazyLock::new(|| {
  meter()
    .f64_histogram("sectr.approval.decision_latency")
    .with_description("Approval decision latency (ms from request to decision)")
    .with_unit("ms")
    .build()
});

/// `sectr.deploys` — deploy outcomes, attr `outcome` (success/failed).
pub static DEPLOYS: LazyLock<Counter<u64>> = LazyLock::new(|| {
  meter()
    .u64_counter("sectr.deploys")
    .with_description("Deployment build outcomes")
    .build()
});

/// `sectr.tail.connections` — live SSE tail connections. DEFINED in the
/// shared sessions-api (sectr-common/src/api/mod.rs), which is the seam
/// that serves both dev and prod tails — the server only references it.
/// `sectr.ingest.reconnects` — sidecar WS re-attachments with a replayed
/// batch (reconnects are the abnormal path; connects are normal churn).
pub static INGEST_RECONNECTS: LazyLock<Counter<u64>> = LazyLock::new(|| {
  meter()
    .u64_counter("sectr.ingest.reconnects")
    .with_description("Sidecar ingest reconnects (re-attach with replay)")
    .build()
});

/// `sectr.quota.rejections` — requests refused by a guardrail,
/// attr `reason` (concurrency_limit / platform_capacity / rate_limited /
/// beta_full). Low cardinality on purpose: no app ids.
pub static QUOTA_REJECTIONS: LazyLock<Counter<u64>> = LazyLock::new(|| {
  meter()
    .u64_counter("sectr.quota.rejections")
    .with_description("Requests rejected by a capacity/rate/signup guardrail")
    .build()
});

/// `sectr.runners.reaped` — orphan runners the reaper found,
/// attr `mode` (`on` = killed, `dry-run` = would have killed). Counts
/// per-scan SIGHTINGS, not distinct orphans: an orphan still listed on the
/// next 60 s scan (always, in `dry-run`; in `on`, only if the log-not-fail
/// teardown didn't take) is counted again. Alert on the rate, not the total.
pub static RUNNERS_REAPED: LazyLock<Counter<u64>> = LazyLock::new(|| {
  meter()
    .u64_counter("sectr.runners.reaped")
    .with_description("Orphan runner sandboxes reaped (or flagged, in dry-run)")
    .build()
});

/// Count one guardrail rejection (`reason` = the response `code`).
pub fn quota_rejected(reason: &'static str) {
  QUOTA_REJECTIONS.add(1, &[opentelemetry::KeyValue::new("reason", reason)]);
}

#[cfg(test)]
mod tests {
  use super::*;

  /// With no OTLP gate, the global meter is a no-op — every instrument
  /// construction and add must be safe (the dev path runs exactly this).
  #[test]
  fn instruments_are_noop_safe_without_provider() {
    let _ = &*SESSIONS_CREATED;
    let _ = &*TURNS;
    let _ = &*APPROVAL_DECISION_LATENCY;
    let _ = &*DEPLOYS;
    let _ = &*INGEST_RECONNECTS;
    let _ = &*QUOTA_REJECTIONS;
    let _ = &*RUNNERS_REAPED;
    let _ = &*SESSION_ERRORS;
    let _ = &*INGEST_FAILURES;
    let _ = &*ENDPOINT_WAKES;
    SESSIONS_CREATED.add(1, &[]);
    TURNS.add(1, &[]);
    APPROVAL_DECISION_LATENCY.record(42.0, &[]);
    DEPLOYS.add(1, &[]);
    INGEST_RECONNECTS.add(1, &[]);
    quota_rejected("concurrency_limit");
    RUNNERS_REAPED.add(1, &[]);
    SESSION_ERRORS.add(1, &[opentelemetry::KeyValue::new("source", "runner")]);
    INGEST_FAILURES.add(1, &[opentelemetry::KeyValue::new("reason", "journal")]);
    ENDPOINT_WAKES.add(1, &[opentelemetry::KeyValue::new("outcome", "delivered")]);
  }
}
