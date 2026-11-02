//! Orphan runner reaper: kills runner sandboxes nobody owns, so a
//! server restart (or any lost teardown) can't leave us billed for idle
//! sandboxes until their ~1 h timeout. The known "E2B orphan billing"
//! caveat, closed.
//!
//! Ownership rule: a runner is OWNED iff its session has a live lease
//! (`released_at IS NULL AND expires_at > now`, i.e. `Registry::live_lease`)
//! whose `invocation_id` equals the runner's `sectr.invocation` tag.
//! Anything else older than the grace period is an orphan and is killed via
//! the platform's own (idempotent, log-not-fail) `teardown`.
//!
//! Fail safe: a listing or registry error skips the tick — the reaper never
//! kills on uncertainty. The platform only lists ITS OWN server's runners
//! (see `RunnerPlatform::list_runners`), so a shared account's other
//! environments are never touched.
//!
//! The reaper is idempotent, so several server instances running it is
//! redundant, never wrong. It never talks to a runner and never writes the
//! journal.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use sectr_common::registry::Registry;

use crate::platform::{RunnerInfo, RunnerPlatform};

/// How often the reaper scans.
pub const REAP_INTERVAL: Duration = Duration::from_secs(60);
/// A runner younger than this is never judged: spawn creates the sandbox
/// moments after the lease claim, and the grace also covers clock skew and
/// the claim→spawn window.
pub const REAP_GRACE: Duration = Duration::from_secs(120);

/// `SECTR_RUNNER_REAPER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaperMode {
  /// Don't run (the kill switch).
  Off,
  /// Log and count what would be killed; kill nothing.
  DryRun,
  /// Kill orphans.
  On,
}

impl ReaperMode {
  pub fn parse(raw: &str) -> anyhow::Result<Self> {
    match raw.trim().to_ascii_lowercase().as_str() {
      "off" => Ok(ReaperMode::Off),
      "dry-run" | "dry_run" | "dryrun" => Ok(ReaperMode::DryRun),
      "on" => Ok(ReaperMode::On),
      other => Err(anyhow::anyhow!(
        "SECTR_RUNNER_REAPER={other:?} is not one of off | dry-run | on"
      )),
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      ReaperMode::Off => "off",
      ReaperMode::DryRun => "dry-run",
      ReaperMode::On => "on",
    }
  }
}

/// Why a runner is an orphan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanReason {
  /// The session has no live lease at all.
  NoLiveLease,
  /// The session is leased, but for a different invocation.
  InvocationMismatch,
}

impl OrphanReason {
  fn as_str(self) -> &'static str {
    match self {
      OrphanReason::NoLiveLease => "no_live_lease",
      OrphanReason::InvocationMismatch => "invocation_mismatch",
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
  pub handle: String,
  pub session: Uuid,
  pub reason: OrphanReason,
}

/// What one scan did.
#[derive(Debug, PartialEq, Eq)]
pub enum ReapOutcome {
  /// The platform can't list runners; the reaper task should stop.
  Unsupported,
  /// A listing or registry error — nothing further was killed this tick.
  Skipped,
  /// The scan completed. `orphans` were killed in `On` mode, only
  /// reported in `DryRun`.
  Scanned {
    examined: usize,
    orphans: Vec<Orphan>,
  },
}

/// One reaper pass at time `now`.
pub async fn reap_once(
  platform: &dyn RunnerPlatform,
  registry: &dyn Registry,
  mode: ReaperMode,
  grace: Duration,
  now: DateTime<Utc>,
) -> ReapOutcome {
  if mode == ReaperMode::Off {
    return ReapOutcome::Scanned {
      examined: 0,
      orphans: Vec::new(),
    };
  }
  let runners = match platform.list_runners().await {
    Ok(Some(runners)) => runners,
    Ok(None) => return ReapOutcome::Unsupported,
    Err(e) => {
      tracing::warn!(error = %e, "runner reaper: listing runners failed; skipping this tick");
      return ReapOutcome::Skipped;
    }
  };
  let grace = chrono::Duration::from_std(grace).unwrap_or(chrono::Duration::seconds(120));
  let mut orphans = Vec::new();
  let examined = runners.len();
  for runner in runners {
    if now - runner.started_at < grace {
      continue;
    }
    let reason = match classify(registry, &runner).await {
      Ok(None) => continue,
      Ok(Some(reason)) => reason,
      Err(e) => {
        tracing::warn!(session = %runner.session, error = %e, "runner reaper: lease lookup failed; skipping this tick");
        return ReapOutcome::Skipped;
      }
    };
    tracing::warn!(
      session = %runner.session,
      sandbox = %runner.handle,
      reason = reason.as_str(),
      mode = mode.as_str(),
      started_at = %runner.started_at,
      "runner reaper: orphan runner"
    );
    crate::metrics::RUNNERS_REAPED.add(1, &[opentelemetry::KeyValue::new("mode", mode.as_str())]);
    if mode == ReaperMode::On {
      platform
        .teardown(runner.session, Some(&runner.handle))
        .await;
    }
    orphans.push(Orphan {
      handle: runner.handle,
      session: runner.session,
      reason,
    });
  }
  ReapOutcome::Scanned { examined, orphans }
}

/// `None` = owned.
async fn classify(
  registry: &dyn Registry,
  runner: &RunnerInfo,
) -> Result<Option<OrphanReason>, sectr_common::registry::RegistryError> {
  Ok(match registry.live_lease(runner.session).await? {
    None => Some(OrphanReason::NoLiveLease),
    Some(lease) if lease.invocation_id == runner.invocation => None,
    Some(_) => Some(OrphanReason::InvocationMismatch),
  })
}

/// Run until the platform says it can't list runners (then once, quietly).
pub async fn run_runner_reaper(
  platform: Arc<dyn RunnerPlatform>,
  registry: Arc<dyn Registry>,
  mode: ReaperMode,
  interval: Duration,
  grace: Duration,
) {
  tracing::info!(mode = mode.as_str(), "runner reaper started");
  let mut ticker = tokio::time::interval(interval);
  ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
  loop {
    ticker.tick().await;
    match reap_once(&*platform, &*registry, mode, grace, Utc::now()).await {
      ReapOutcome::Unsupported => {
        tracing::info!("runner reaper: this platform cannot list runners; reaper disabled");
        return;
      }
      ReapOutcome::Skipped => {}
      ReapOutcome::Scanned { orphans, .. } if !orphans.is_empty() => {
        tracing::info!(
          orphans = orphans.len(),
          killed = mode == ReaperMode::On,
          "runner reaper pass complete"
        );
      }
      ReapOutcome::Scanned { .. } => {}
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Mutex;

  use sectr_common::registry::{NewApp, NewDeployment, NewSession, Registry};
  use sectr_common::store::PgStore;

  use crate::dispatch::DispatchError;
  use crate::platform::{BuildError, BuildOutput, BuildRequest};
  use crate::test_pg;

  /// Records teardowns; lists a scripted set of runners (or errors, or
  /// reports "unsupported").
  struct FakePlatform {
    listing: Mutex<Option<Result<Option<Vec<RunnerInfo>>, String>>>,
    torn_down: Mutex<Vec<(Uuid, Option<String>)>>,
  }

  impl FakePlatform {
    fn listing(runners: Vec<RunnerInfo>) -> Self {
      FakePlatform {
        listing: Mutex::new(Some(Ok(Some(runners)))),
        torn_down: Mutex::new(Vec::new()),
      }
    }
    fn with(result: Result<Option<Vec<RunnerInfo>>, String>) -> Self {
      FakePlatform {
        listing: Mutex::new(Some(result)),
        torn_down: Mutex::new(Vec::new()),
      }
    }
    fn killed(&self) -> Vec<String> {
      self
        .torn_down
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(_, h)| h.clone())
        .collect()
    }
  }

  #[async_trait::async_trait]
  impl RunnerPlatform for FakePlatform {
    async fn build(&self, _req: BuildRequest) -> Result<BuildOutput, BuildError> {
      Err(BuildError::bare("fake"))
    }
    fn provisional_build_ref(&self, _deployment_id: Uuid, _version: i64) -> String {
      "fake".into()
    }
    async fn spawn(
      &self,
      _session: Uuid,
      _deployment_id: Uuid,
      _invocation_id: Uuid,
      _ingest_token: &str,
    ) -> Result<Option<String>, DispatchError> {
      Ok(None)
    }
    async fn teardown(&self, session: Uuid, handle: Option<&str>) {
      self
        .torn_down
        .lock()
        .unwrap()
        .push((session, handle.map(str::to_owned)));
    }
    async fn list_runners(&self) -> Result<Option<Vec<RunnerInfo>>, DispatchError> {
      match self.listing.lock().unwrap().clone().expect("scripted") {
        Ok(runners) => Ok(runners),
        Err(e) => Err(DispatchError::Unavailable(e)),
      }
    }
  }

  struct Fixture {
    registry: sectr_common::registry::PgRegistry,
    app: sectr_common::registry::App,
    deployment: sectr_common::registry::Deployment,
  }

  async fn fixture() -> Option<Fixture> {
    let store: PgStore = test_pg::fresh_store().await?;
    let registry = store.registry();
    let org = registry.create_org("test-org", "default").await.unwrap();
    let app = registry
      .create_app(NewApp {
        org_id: org.id,
        name: "app".into(),
        slug: "app".into(),
      })
      .await
      .unwrap();
    let deployment = registry
      .create_deployment(NewDeployment {
        app_id: app.id,
        version: 1,
        manifest: serde_json::json!({"routes": [{"path": "/test"}]}),
      })
      .await
      .unwrap();
    Some(Fixture {
      registry,
      app,
      deployment,
    })
  }

  impl Fixture {
    async fn session(&self) -> Uuid {
      self
        .registry
        .create_session(NewSession {
          id: None,
          app_id: self.app.id,
          deployment_id: self.deployment.id,
          route: "/test".into(),
          origin: sectr_common::api::Origin::Api,
          external_user_id: None,
        })
        .await
        .unwrap()
        .id
    }

    /// A session holding a live lease for `invocation`.
    async fn leased(&self, invocation: Uuid) -> Uuid {
      let session = self.session().await;
      self
        .registry
        .claim_lease(
          session,
          "sectr-server",
          Some(invocation),
          chrono::Duration::seconds(300),
        )
        .await
        .unwrap();
      session
    }
  }

  fn runner(handle: &str, session: Uuid, invocation: Option<Uuid>, age_secs: i64) -> RunnerInfo {
    RunnerInfo {
      handle: handle.into(),
      session,
      invocation,
      started_at: Utc::now() - chrono::Duration::seconds(age_secs),
    }
  }

  const GRACE: Duration = Duration::from_secs(120);

  #[tokio::test]
  async fn owned_runners_are_kept_and_orphans_killed() {
    let Some(f) = fixture().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let inv = Uuid::now_v7();
    let owned = f.leased(inv).await;
    let no_lease = f.session().await;
    let mismatch = f.leased(Uuid::now_v7()).await;
    let fresh_orphan = f.session().await;
    // An expired-but-unreleased lease is not live either.
    let expired = f.session().await;
    f.registry
      .claim_lease(expired, "sectr-server", Some(inv), chrono::Duration::zero())
      .await
      .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let platform = FakePlatform::listing(vec![
      runner("sbx-owned", owned, Some(inv), 600),
      runner("sbx-nolease", no_lease, Some(inv), 600),
      runner("sbx-mismatch", mismatch, Some(inv), 600),
      runner("sbx-fresh", fresh_orphan, Some(inv), 30), // within grace
      runner("sbx-expired", expired, Some(inv), 600),
    ]);
    let outcome = reap_once(&platform, &f.registry, ReaperMode::On, GRACE, Utc::now()).await;

    let ReapOutcome::Scanned { examined, orphans } = outcome else {
      panic!("expected a completed scan");
    };
    assert_eq!(examined, 5);
    let reasons: Vec<_> = orphans
      .iter()
      .map(|o| (o.handle.as_str(), o.reason))
      .collect();
    assert_eq!(
      reasons,
      [
        ("sbx-nolease", OrphanReason::NoLiveLease),
        ("sbx-mismatch", OrphanReason::InvocationMismatch),
        ("sbx-expired", OrphanReason::NoLiveLease),
      ]
    );
    assert_eq!(
      platform.killed(),
      ["sbx-nolease", "sbx-mismatch", "sbx-expired"],
      "owned and within-grace runners survive"
    );
  }

  #[tokio::test]
  async fn dry_run_reports_but_kills_nothing() {
    let Some(f) = fixture().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let orphan = f.session().await;
    let platform = FakePlatform::listing(vec![runner("sbx-orphan", orphan, None, 600)]);
    let outcome = reap_once(
      &platform,
      &f.registry,
      ReaperMode::DryRun,
      GRACE,
      Utc::now(),
    )
    .await;
    let ReapOutcome::Scanned { orphans, .. } = outcome else {
      panic!("expected a completed scan");
    };
    assert_eq!(orphans.len(), 1, "counted");
    assert!(platform.killed().is_empty(), "but not killed");
  }

  #[tokio::test]
  async fn off_mode_does_not_even_list() {
    let Some(f) = fixture().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    // A listing error would surface as Skipped if it were consulted.
    let platform = FakePlatform::with(Err("must not be called".into()));
    let outcome = reap_once(&platform, &f.registry, ReaperMode::Off, GRACE, Utc::now()).await;
    assert_eq!(
      outcome,
      ReapOutcome::Scanned {
        examined: 0,
        orphans: Vec::new()
      }
    );
  }

  #[tokio::test]
  async fn a_listing_error_kills_nothing() {
    let Some(f) = fixture().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let platform = FakePlatform::with(Err("E2B 500".into()));
    let outcome = reap_once(&platform, &f.registry, ReaperMode::On, GRACE, Utc::now()).await;
    assert_eq!(outcome, ReapOutcome::Skipped);
    assert!(platform.killed().is_empty());
  }

  #[tokio::test]
  async fn unsupported_platforms_stop_the_reaper() {
    let Some(f) = fixture().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let platform = FakePlatform::with(Ok(None));
    assert_eq!(
      reap_once(&platform, &f.registry, ReaperMode::On, GRACE, Utc::now()).await,
      ReapOutcome::Unsupported
    );
  }

  #[tokio::test]
  async fn a_registry_error_skips_the_tick_without_further_kills() {
    // An unknown-to-the-registry session is NOT an error (live_lease is
    // None -> orphan); a closed pool is.
    let Some(f) = fixture().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    let orphan = f.session().await;
    let platform = FakePlatform::listing(vec![runner("sbx-1", orphan, None, 600)]);
    f.registry.pool().close().await;
    let outcome = reap_once(&platform, &f.registry, ReaperMode::On, GRACE, Utc::now()).await;
    assert_eq!(outcome, ReapOutcome::Skipped);
    assert!(platform.killed().is_empty());
  }

  #[test]
  fn mode_parsing() {
    assert_eq!(ReaperMode::parse("on").unwrap(), ReaperMode::On);
    assert_eq!(ReaperMode::parse(" OFF ").unwrap(), ReaperMode::Off);
    assert_eq!(ReaperMode::parse("dry-run").unwrap(), ReaperMode::DryRun);
    assert!(ReaperMode::parse("yes").is_err());
  }
}
