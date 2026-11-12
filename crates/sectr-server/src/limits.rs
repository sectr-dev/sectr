//! guardrails: the concurrent-turn caps, the session-creation rate
//! limiter, and the env-parsed knobs behind both (plus the signup cap and
//! reaper mode, which live in [`LimitsConfig`] so there is one place for
//! "what is this deployment's policy").
//!
//! All of it is SERVER POLICY: the dev gateway never returns these 429s.
//! Nothing here writes to the journal.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use uuid::Uuid;

use sectr_common::api::ServiceError;
use sectr_common::registry::{CapScope, LeaseCaps};

use crate::reaper::ReaperMode;

/// `Retry-After` for a per-app concurrency rejection (a turn usually
/// finishes within seconds).
const APP_RETRY_SECS: u64 = 5;
/// `Retry-After` for a platform-capacity rejection.
const PLATFORM_RETRY_SECS: u64 = 10;

/// User-token (`sks_`) bucket burst; the refill rate is the configured
/// per-hour figure.
const USER_BURST: u32 = 10;
/// Invocation-key (`ski_`) bucket burst.
const APP_BURST: u32 = 60;

/// Opportunistic sweep threshold (same as `MintLimiter`).
const SWEEP_ABOVE: usize = 10_000;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// The guardrail env knobs, parsed fail-fast (a typo'd cap must not silently
/// become "unlimited"). `0` on a cap / rate means unlimited / off.
#[derive(Debug, Clone)]
pub struct LimitsConfig {
  /// `SECTR_MAX_CONCURRENT_SANDBOX_TURNS_PER_APP` (formerly
  /// `SECTR_MAX_CONCURRENT_TURNS_PER_APP`), default 3: an app's turns in
  /// Sectr Run sandboxes, which bound our sandbox spend.
  pub max_sandbox_turns_per_app: u32,
  /// `SECTR_MAX_CONCURRENT_ENDPOINT_TURNS_PER_APP`, default 20: an app's
  /// turns on its own compute endpoint. Those run on the customer's
  /// compute, and cost the server only a held request, the sidecar's
  /// connection and journal writes.
  pub max_endpoint_turns_per_app: u32,
  /// `SECTR_MAX_CONCURRENT_SANDBOX_TURNS` (formerly
  /// `SECTR_MAX_CONCURRENT_TURNS`), default 0 = unlimited: turns in Sectr
  /// Run sandboxes platform-wide, the sandbox platform's capacity. Endpoint
  /// turns have no platform-wide cap.
  pub max_sandbox_turns: u32,
  /// `SECTR_APP_TURN_LIMIT_OVERRIDES` = `<app-uuid>=<n>,…` (n = 0 lifts the
  /// per-app cap for that app). An override is the app's per-app cap for
  /// both kinds of turn.
  pub app_turn_limit_overrides: HashMap<Uuid, u32>,
  /// `SECTR_SESSION_RATE_PER_USER_HOUR`, default 60 (burst 10).
  pub session_rate_per_user_hour: u32,
  /// `SECTR_SESSION_RATE_PER_APP_HOUR`, default 600 (burst 60).
  pub session_rate_per_app_hour: u32,
  /// `SECTR_SIGNUP_CAP`: max orgs; unset = no cap. `0` closes signups.
  pub signup_cap: Option<u32>,
  /// `SECTR_RUNNER_REAPER`, default `on`.
  pub reaper: ReaperMode,
}

impl LimitsConfig {
  /// No caps, no rate limits, reaper off — the shared test fixtures use
  /// this so existing tests are unaffected.
  pub fn none() -> Self {
    LimitsConfig {
      max_sandbox_turns_per_app: 0,
      max_endpoint_turns_per_app: 0,
      max_sandbox_turns: 0,
      app_turn_limit_overrides: HashMap::new(),
      session_rate_per_user_hour: 0,
      session_rate_per_app_hour: 0,
      signup_cap: None,
      reaper: ReaperMode::Off,
    }
  }

  pub fn from_env() -> anyhow::Result<Self> {
    Self::from_lookup(|key| std::env::var(key).ok())
  }

  fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
    fn value(get: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
      get(key)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
    }
    fn number(get: &impl Fn(&str) -> Option<String>, key: &str) -> anyhow::Result<Option<u32>> {
      match value(get, key) {
        None => Ok(None),
        Some(v) => v
          .parse::<u32>()
          .map(Some)
          .map_err(|_| anyhow::anyhow!("{key}={v:?} is not a non-negative integer")),
      }
    }
    // A renamed knob still reads its old name, so an existing env file
    // keeps its cap rather than silently losing it. Both set differently
    // is refused: which one was meant can't be told.
    fn renamed(
      get: &impl Fn(&str) -> Option<String>,
      key: &str,
      old: &str,
    ) -> anyhow::Result<Option<u32>> {
      match (number(get, key)?, number(get, old)?) {
        (Some(new), Some(was)) if new != was => {
          anyhow::bail!(
            "{old}={was} and {key}={new} are set: {old} is {key}'s old name, so set only {key}"
          )
        }
        (Some(new), _) => Ok(Some(new)),
        (None, Some(was)) => {
          tracing::warn!("{old} is deprecated: rename it to {key}");
          Ok(Some(was))
        }
        (None, None) => Ok(None),
      }
    }
    Ok(LimitsConfig {
      max_sandbox_turns_per_app: renamed(
        &get,
        "SECTR_MAX_CONCURRENT_SANDBOX_TURNS_PER_APP",
        "SECTR_MAX_CONCURRENT_TURNS_PER_APP",
      )?
      .unwrap_or(3),
      max_endpoint_turns_per_app: number(&get, "SECTR_MAX_CONCURRENT_ENDPOINT_TURNS_PER_APP")?
        .unwrap_or(20),
      max_sandbox_turns: renamed(
        &get,
        "SECTR_MAX_CONCURRENT_SANDBOX_TURNS",
        "SECTR_MAX_CONCURRENT_TURNS",
      )?
      .unwrap_or(0),
      app_turn_limit_overrides: match value(&get, "SECTR_APP_TURN_LIMIT_OVERRIDES") {
        Some(v) => parse_overrides(&v)?,
        None => HashMap::new(),
      },
      session_rate_per_user_hour: number(&get, "SECTR_SESSION_RATE_PER_USER_HOUR")?.unwrap_or(60),
      session_rate_per_app_hour: number(&get, "SECTR_SESSION_RATE_PER_APP_HOUR")?.unwrap_or(600),
      signup_cap: number(&get, "SECTR_SIGNUP_CAP")?,
      reaper: match value(&get, "SECTR_RUNNER_REAPER") {
        Some(v) => ReaperMode::parse(&v)?,
        None => ReaperMode::On,
      },
    })
  }
}

/// `"<app-uuid>=<n>,<app-uuid>=<n>"`; any malformed entry is an error
/// (startup fails rather than silently dropping a limit).
fn parse_overrides(raw: &str) -> anyhow::Result<HashMap<Uuid, u32>> {
  let mut out = HashMap::new();
  for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
    let (id, n) = entry.split_once('=').ok_or_else(|| {
      anyhow::anyhow!("SECTR_APP_TURN_LIMIT_OVERRIDES entry {entry:?} is not <app-uuid>=<n>")
    })?;
    let id: Uuid = id.trim().parse().map_err(|_| {
      anyhow::anyhow!("SECTR_APP_TURN_LIMIT_OVERRIDES entry {entry:?}: {id:?} is not a UUID")
    })?;
    let n: u32 = n.trim().parse().map_err(|_| {
      anyhow::anyhow!(
        "SECTR_APP_TURN_LIMIT_OVERRIDES entry {entry:?}: {n:?} is not a non-negative integer"
      )
    })?;
    out.insert(id, n);
  }
  Ok(out)
}

// ---------------------------------------------------------------------------
// Concurrent-turn caps
// ---------------------------------------------------------------------------

/// Resolved cap policy: per-app defaults (by where the turn runs) +
/// per-app overrides + the platform-wide sandbox cap.
#[derive(Debug, Clone, Default)]
pub struct TurnCapsConfig {
  per_app_default: Option<u32>,
  endpoint_per_app_default: Option<u32>,
  overrides: HashMap<Uuid, Option<u32>>,
  global: Option<u32>,
}

impl TurnCapsConfig {
  /// No caps (the default constructor of the service: test call sites are
  /// untouched).
  pub fn unlimited() -> Self {
    Self::default()
  }

  pub fn from_config(limits: &LimitsConfig) -> Self {
    let cap = |n: u32| (n > 0).then_some(n);
    TurnCapsConfig {
      per_app_default: cap(limits.max_sandbox_turns_per_app),
      endpoint_per_app_default: cap(limits.max_endpoint_turns_per_app),
      overrides: limits
        .app_turn_limit_overrides
        .iter()
        .map(|(app, n)| (*app, cap(*n)))
        .collect(),
      global: cap(limits.max_sandbox_turns),
    }
  }

  /// The caps that apply to a claim for `app_id`, whose turn runs on a
  /// compute endpoint when `on_endpoint` and in a Sectr Run sandbox
  /// otherwise. Each cap counts only turns of the claim's kind. An app's
  /// override applies to both kinds; the platform-wide cap only to
  /// sandboxes.
  pub fn caps_for(&self, app_id: Uuid, on_endpoint: bool) -> LeaseCaps {
    LeaseCaps {
      app_id,
      on_endpoint,
      per_app: match self.overrides.get(&app_id) {
        Some(over) => *over,
        None if on_endpoint => self.endpoint_per_app_default,
        None => self.per_app_default,
      },
      global: (!on_endpoint).then_some(self.global).flatten(),
    }
  }
}

/// The 429 for a tripped concurrency cap (counts the rejection).
pub fn cap_exceeded(scope: CapScope, limit: u32) -> ServiceError {
  match scope {
    CapScope::App => {
      crate::metrics::quota_rejected("concurrency_limit");
      ServiceError::TooManyRequests {
        code: "concurrency_limit",
        message: format!("this app is running its maximum of {limit} turns at once; retry shortly"),
        retry_after_secs: APP_RETRY_SECS,
      }
    }
    CapScope::Platform => {
      crate::metrics::quota_rejected("platform_capacity");
      ServiceError::TooManyRequests {
        code: "platform_capacity",
        message: "Sectr is at capacity right now; retry shortly".into(),
        retry_after_secs: PLATFORM_RETRY_SECS,
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Session-creation rate limiter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BucketKey {
  /// `sks_` user token: `(app, external_user_id)`.
  User(Uuid, String),
  /// `ski_` invocation key: the whole app (a public page holding a `ski_`
  /// shares one bucket across all its visitors).
  App(Uuid),
}

#[derive(Debug, Clone, Copy)]
struct BucketSpec {
  capacity: f64,
  refill_per_sec: f64,
}

impl BucketSpec {
  /// `per_hour` = 0 → no limit. Burst is capped by the hourly rate so a
  /// tiny configured rate can't be exceeded by its own burst.
  fn new(per_hour: u32, burst: u32) -> Option<Self> {
    (per_hour > 0).then(|| BucketSpec {
      capacity: f64::from(burst.min(per_hour)),
      refill_per_sec: f64::from(per_hour) / 3600.0,
    })
  }
}

struct Bucket {
  tokens: f64,
  last: Instant,
}

/// In-memory token buckets over session creation. Per process, resets on
/// restart, deliberately cheap — the `MintLimiter` precedent; a
/// Postgres-backed limiter waits for multi-instance.
pub struct SessionRateLimiter {
  user: Option<BucketSpec>,
  app: Option<BucketSpec>,
  buckets: Mutex<HashMap<BucketKey, Bucket>>,
}

impl SessionRateLimiter {
  pub fn unlimited() -> Self {
    Self::new(0, 0)
  }

  /// `*_per_hour` = 0 disables that bucket class.
  pub fn new(user_per_hour: u32, app_per_hour: u32) -> Self {
    SessionRateLimiter {
      user: BucketSpec::new(user_per_hour, USER_BURST),
      app: BucketSpec::new(app_per_hour, APP_BURST),
      buckets: Mutex::new(HashMap::new()),
    }
  }

  pub fn from_config(limits: &LimitsConfig) -> Self {
    Self::new(
      limits.session_rate_per_user_hour,
      limits.session_rate_per_app_hour,
    )
  }

  /// Take one token for `(app, user)` (user-token callers).
  pub fn check_user(&self, app_id: Uuid, external_user_id: &str) -> Result<(), ServiceError> {
    self.check_user_at(app_id, external_user_id, Instant::now())
  }

  /// Take one token for the whole `app` (invocation-key callers).
  pub fn check_app(&self, app_id: Uuid) -> Result<(), ServiceError> {
    self.check_app_at(app_id, Instant::now())
  }

  fn check_user_at(
    &self,
    app_id: Uuid,
    external_user_id: &str,
    now: Instant,
  ) -> Result<(), ServiceError> {
    match self.user {
      Some(spec) => self.take(
        BucketKey::User(app_id, external_user_id.to_owned()),
        spec,
        now,
      ),
      None => Ok(()),
    }
  }

  fn check_app_at(&self, app_id: Uuid, now: Instant) -> Result<(), ServiceError> {
    match self.app {
      Some(spec) => self.take(BucketKey::App(app_id), spec, now),
      None => Ok(()),
    }
  }

  fn take(&self, key: BucketKey, spec: BucketSpec, now: Instant) -> Result<(), ServiceError> {
    let mut buckets = self.buckets.lock().unwrap();
    if buckets.len() > SWEEP_ABOVE {
      // A bucket that has refilled to full is indistinguishable from a
      // missing one — dropping it loses nothing.
      buckets.retain(|k, b| {
        let spec = match k {
          BucketKey::User(..) => self.user,
          BucketKey::App(_) => self.app,
        };
        match spec {
          Some(spec) => refilled(b, spec, now) < spec.capacity,
          None => false,
        }
      });
    }
    let bucket = buckets.entry(key).or_insert(Bucket {
      tokens: spec.capacity,
      last: now,
    });
    bucket.tokens = refilled(bucket, spec, now);
    bucket.last = now;
    if bucket.tokens >= 1.0 {
      bucket.tokens -= 1.0;
      return Ok(());
    }
    let wait = ((1.0 - bucket.tokens) / spec.refill_per_sec)
      .ceil()
      .max(1.0) as u64;
    crate::metrics::quota_rejected("rate_limited");
    Err(ServiceError::TooManyRequests {
      code: "rate_limited",
      message: "too many sessions created recently; retry later".into(),
      retry_after_secs: wait,
    })
  }
}

/// Tokens in `bucket` as of `now` (refill since `last`, capped).
fn refilled(bucket: &Bucket, spec: BucketSpec, now: Instant) -> f64 {
  let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
  (bucket.tokens + elapsed * spec.refill_per_sec).min(spec.capacity)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  fn retry_after(e: ServiceError) -> (&'static str, u64) {
    match e {
      ServiceError::TooManyRequests {
        code,
        retry_after_secs,
        ..
      } => (code, retry_after_secs),
      other => panic!("expected TooManyRequests, got {other:?}"),
    }
  }

  #[test]
  fn user_bucket_bursts_then_limits_then_refills() {
    // 60/hour = one token per 60 s; burst 10.
    let limiter = SessionRateLimiter::new(60, 0);
    let app = Uuid::now_v7();
    let t0 = Instant::now();
    for _ in 0..10 {
      limiter.check_user_at(app, "alice", t0).unwrap();
    }
    let (code, wait) = retry_after(limiter.check_user_at(app, "alice", t0).unwrap_err());
    assert_eq!(code, "rate_limited");
    assert_eq!(wait, 60, "empty bucket: a full token interval");

    // Another user, and another app's same-named user, are unaffected.
    limiter.check_user_at(app, "bob", t0).unwrap();
    limiter.check_user_at(Uuid::now_v7(), "alice", t0).unwrap();

    // Half an interval later: still empty, retry in the remaining 30 s.
    let (_, wait) = retry_after(
      limiter
        .check_user_at(app, "alice", t0 + Duration::from_secs(30))
        .unwrap_err(),
    );
    assert_eq!(wait, 30);

    // A full interval: exactly one token back, then empty again.
    let t1 = t0 + Duration::from_secs(60);
    limiter.check_user_at(app, "alice", t1).unwrap();
    assert!(limiter.check_user_at(app, "alice", t1).is_err());
  }

  #[test]
  fn refill_is_capped_at_the_burst() {
    let limiter = SessionRateLimiter::new(60, 0);
    let app = Uuid::now_v7();
    let t0 = Instant::now();
    limiter.check_user_at(app, "alice", t0).unwrap();
    // A day idle refills to the burst (10), not beyond.
    let later = t0 + Duration::from_secs(86_400);
    for _ in 0..10 {
      limiter.check_user_at(app, "alice", later).unwrap();
    }
    assert!(limiter.check_user_at(app, "alice", later).is_err());
  }

  #[test]
  fn app_bucket_is_shared_by_all_callers_of_the_app() {
    let limiter = SessionRateLimiter::new(0, 600);
    let app = Uuid::now_v7();
    let t0 = Instant::now();
    for _ in 0..60 {
      limiter.check_app_at(app, t0).unwrap();
    }
    let (code, wait) = retry_after(limiter.check_app_at(app, t0).unwrap_err());
    assert_eq!(code, "rate_limited");
    assert_eq!(wait, 6, "600/h = a token every 6 s");
    limiter.check_app_at(Uuid::now_v7(), t0).unwrap();
  }

  #[test]
  fn zero_rate_disables_the_bucket_class() {
    let limiter = SessionRateLimiter::unlimited();
    let app = Uuid::now_v7();
    let t0 = Instant::now();
    for _ in 0..1000 {
      limiter.check_user_at(app, "alice", t0).unwrap();
      limiter.check_app_at(app, t0).unwrap();
    }
  }

  #[test]
  fn burst_never_exceeds_the_hourly_rate() {
    let limiter = SessionRateLimiter::new(3, 0);
    let app = Uuid::now_v7();
    let t0 = Instant::now();
    for _ in 0..3 {
      limiter.check_user_at(app, "alice", t0).unwrap();
    }
    assert!(limiter.check_user_at(app, "alice", t0).is_err());
  }

  #[test]
  fn sweep_drops_only_refilled_buckets() {
    let limiter = SessionRateLimiter::new(60, 0);
    let t0 = Instant::now();
    let hot = Uuid::now_v7();
    for _ in 0..10 {
      limiter.check_user_at(hot, "hot", t0).unwrap();
    }
    for i in 0..SWEEP_ABOVE + 5 {
      limiter
        .check_user_at(Uuid::now_v7(), &format!("u{i}"), t0)
        .unwrap();
    }
    // Next take sweeps: the 1-token-used buckets are not full yet at t0, so
    // jump forward past their refill (60 s) but not the hot bucket's (10
    // tokens = 600 s).
    let t1 = t0 + Duration::from_secs(120);
    limiter
      .check_user_at(Uuid::now_v7(), "trigger", t1)
      .unwrap();
    let remaining = limiter.buckets.lock().unwrap().len();
    assert!(remaining < 100, "refilled buckets swept, {remaining} left");
    assert!(
      limiter.check_user_at(hot, "hot", t1).is_ok(),
      "the hot bucket survived with its (partially refilled) state"
    );
  }

  #[test]
  fn overrides_lift_or_replace_the_per_app_cap() {
    let vip = Uuid::now_v7();
    let unlimited = Uuid::now_v7();
    let other = Uuid::now_v7();
    let limits = LimitsConfig {
      max_sandbox_turns_per_app: 3,
      max_endpoint_turns_per_app: 20,
      max_sandbox_turns: 8,
      app_turn_limit_overrides: HashMap::from([(vip, 6), (unlimited, 0)]),
      ..LimitsConfig::none()
    };
    let caps = TurnCapsConfig::from_config(&limits);
    assert_eq!(caps.caps_for(other, false).per_app, Some(3));
    assert_eq!(caps.caps_for(other, true).per_app, Some(20));
    // The platform-wide cap is the sandbox platform's.
    assert_eq!(caps.caps_for(other, false).global, Some(8));
    assert_eq!(caps.caps_for(other, true).global, None);
    // An override is the app's, wherever its turns run.
    for on_endpoint in [false, true] {
      assert_eq!(caps.caps_for(vip, on_endpoint).per_app, Some(6));
      assert_eq!(caps.caps_for(unlimited, on_endpoint).per_app, None);
      assert_eq!(caps.caps_for(other, on_endpoint).on_endpoint, on_endpoint);
      assert_eq!(
        TurnCapsConfig::unlimited()
          .caps_for(other, on_endpoint)
          .per_app,
        None
      );
    }
  }

  fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |k| {
      pairs
        .iter()
        .find(|(key, _)| *key == k)
        .map(|(_, v)| (*v).to_owned())
    }
  }

  #[test]
  fn env_defaults() {
    let l = LimitsConfig::from_lookup(lookup(&[])).unwrap();
    assert_eq!(l.max_sandbox_turns_per_app, 3);
    assert_eq!(l.max_endpoint_turns_per_app, 20);
    assert_eq!(l.max_sandbox_turns, 0);
    assert!(l.app_turn_limit_overrides.is_empty());
    assert_eq!(l.session_rate_per_user_hour, 60);
    assert_eq!(l.session_rate_per_app_hour, 600);
    assert_eq!(l.signup_cap, None);
    assert_eq!(l.reaper, ReaperMode::On);
  }

  #[test]
  fn env_parses_every_knob() {
    let l = LimitsConfig::from_lookup(lookup(&[
      ("SECTR_MAX_CONCURRENT_SANDBOX_TURNS_PER_APP", "4"),
      ("SECTR_MAX_CONCURRENT_ENDPOINT_TURNS_PER_APP", "0"),
      ("SECTR_MAX_CONCURRENT_SANDBOX_TURNS", "8"),
      (
        "SECTR_APP_TURN_LIMIT_OVERRIDES",
        "11111111-1111-1111-1111-111111111111=6, 22222222-2222-2222-2222-222222222222=0",
      ),
      ("SECTR_SESSION_RATE_PER_USER_HOUR", "30"),
      ("SECTR_SESSION_RATE_PER_APP_HOUR", "300"),
      ("SECTR_SIGNUP_CAP", "1000"),
      ("SECTR_RUNNER_REAPER", "dry-run"),
    ]))
    .unwrap();
    assert_eq!(l.max_sandbox_turns_per_app, 4);
    assert_eq!(l.max_endpoint_turns_per_app, 0);
    assert_eq!(l.max_sandbox_turns, 8);
    assert_eq!(l.app_turn_limit_overrides.len(), 2);
    assert_eq!(l.session_rate_per_user_hour, 30);
    assert_eq!(l.session_rate_per_app_hour, 300);
    assert_eq!(l.signup_cap, Some(1000));
    assert_eq!(l.reaper, ReaperMode::DryRun);
  }

  /// An env file from before the rename keeps its caps.
  #[test]
  fn the_old_names_still_set_the_sandbox_caps() {
    let l = LimitsConfig::from_lookup(lookup(&[
      ("SECTR_MAX_CONCURRENT_TURNS_PER_APP", "0"),
      ("SECTR_MAX_CONCURRENT_TURNS", "10"),
    ]))
    .unwrap();
    assert_eq!(l.max_sandbox_turns_per_app, 0);
    assert_eq!(l.max_sandbox_turns, 10);
    // Both names, the same value: fine.
    let l = LimitsConfig::from_lookup(lookup(&[
      ("SECTR_MAX_CONCURRENT_TURNS", "8"),
      ("SECTR_MAX_CONCURRENT_SANDBOX_TURNS", "8"),
    ]))
    .unwrap();
    assert_eq!(l.max_sandbox_turns, 8);
    // Different values: refused.
    let err = LimitsConfig::from_lookup(lookup(&[
      ("SECTR_MAX_CONCURRENT_TURNS", "8"),
      ("SECTR_MAX_CONCURRENT_SANDBOX_TURNS", "10"),
    ]))
    .unwrap_err()
    .to_string();
    assert!(
      err.contains("set only SECTR_MAX_CONCURRENT_SANDBOX_TURNS"),
      "{err}"
    );
  }

  #[test]
  fn env_malformed_values_fail_startup() {
    for bad in [
      ("SECTR_MAX_CONCURRENT_SANDBOX_TURNS", "eight"),
      ("SECTR_MAX_CONCURRENT_SANDBOX_TURNS_PER_APP", "-1"),
      // The old names are still checked.
      ("SECTR_MAX_CONCURRENT_TURNS", "eight"),
      ("SECTR_MAX_CONCURRENT_TURNS_PER_APP", "-1"),
      ("SECTR_MAX_CONCURRENT_ENDPOINT_TURNS_PER_APP", "lots"),
      ("SECTR_SIGNUP_CAP", "1k"),
      ("SECTR_SESSION_RATE_PER_USER_HOUR", "1.5"),
      ("SECTR_RUNNER_REAPER", "maybe"),
      ("SECTR_APP_TURN_LIMIT_OVERRIDES", "not-a-uuid=3"),
      (
        "SECTR_APP_TURN_LIMIT_OVERRIDES",
        "11111111-1111-1111-1111-111111111111",
      ),
      (
        "SECTR_APP_TURN_LIMIT_OVERRIDES",
        "11111111-1111-1111-1111-111111111111=many",
      ),
    ] {
      let pairs: &'static [(&str, &str)] = Box::leak(Box::new([bad]));
      assert!(
        LimitsConfig::from_lookup(lookup(pairs)).is_err(),
        "{bad:?} should fail"
      );
    }
  }

  #[test]
  fn empty_env_values_are_unset() {
    let l = LimitsConfig::from_lookup(lookup(&[
      ("SECTR_SIGNUP_CAP", ""),
      ("SECTR_MAX_CONCURRENT_SANDBOX_TURNS", "  "),
      ("SECTR_RUNNER_REAPER", ""),
    ]))
    .unwrap();
    assert_eq!(l.signup_cap, None);
    assert_eq!(l.max_sandbox_turns, 0);
    assert_eq!(l.reaper, ReaperMode::On);
  }
}
