//! Store tests for `claim_lease_capped` / `count_live_turns`, written
//! once against `dyn Registry` and run by BOTH stores (Registry parity).

use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use super::{
  App, Deployment, LeaseCaps, NewApp, NewDeployment, NewSession, Registry, RegistryError, Session,
};
use super::{CapScope, OrgId};

const TTL: i64 = 30;

pub(crate) async fn app_with_deployment(
  registry: &dyn Registry,
  org: OrgId,
  slug: &str,
) -> (App, Deployment) {
  let app = registry
    .create_app(NewApp {
      org_id: org,
      name: slug.into(),
      slug: slug.into(),
    })
    .await
    .unwrap();
  let deployment = registry
    .create_deployment(NewDeployment {
      endpoint_url: None,
      app_id: app.id,
      version: 1,
      manifest: json!({"routes": [{"path": "/test"}]}),
    })
    .await
    .unwrap();
  (app, deployment)
}

pub(crate) async fn session_of(
  registry: &dyn Registry,
  app: &App,
  deployment: &Deployment,
) -> Session {
  registry
    .create_session(NewSession {
      id: None,
      app_id: app.id,
      deployment_id: deployment.id,
      route: "/test".into(),
      origin: crate::api::Origin::Playground,
      external_user_id: None,
    })
    .await
    .unwrap()
}

async fn claim(
  registry: &dyn Registry,
  session: &Session,
  caps: &LeaseCaps,
  ttl: chrono::Duration,
) -> Result<super::Lease, RegistryError> {
  registry
    .claim_lease_capped(session.id, "test", Some(Uuid::now_v7()), ttl, caps)
    .await
}

fn caps(app: &App, per_app: Option<u32>, global: Option<u32>) -> LeaseCaps {
  LeaseCaps {
    app_id: app.id,
    on_endpoint: false,
    per_app,
    global,
  }
}

/// N live leases → the N+1th is `CapExceeded{App}`; other apps unaffected;
/// a released lease frees a slot.
pub(crate) async fn per_app_cap(registry: &dyn Registry, org: OrgId) {
  let (a, a_dep) = app_with_deployment(registry, org, "app-a").await;
  let (b, b_dep) = app_with_deployment(registry, org, "app-b").await;
  let ttl = chrono::Duration::seconds(TTL);
  let a_caps = caps(&a, Some(2), None);
  let b_caps = caps(&b, Some(2), None);

  let a1 = session_of(registry, &a, &a_dep).await;
  let a2 = session_of(registry, &a, &a_dep).await;
  let a3 = session_of(registry, &a, &a_dep).await;
  let first = claim(registry, &a1, &a_caps, ttl).await.unwrap();
  claim(registry, &a2, &a_caps, ttl).await.unwrap();
  match claim(registry, &a3, &a_caps, ttl).await {
    Err(RegistryError::CapExceeded { scope, limit }) => {
      assert_eq!(scope, CapScope::App);
      assert_eq!(limit, 2);
    }
    other => panic!("expected CapExceeded{{App}}, got {other:?}"),
  }
  // The rejected claim wrote nothing.
  assert!(registry.live_lease(a3.id).await.unwrap().is_none());

  // Another app is unaffected by A being at its cap.
  let b1 = session_of(registry, &b, &b_dep).await;
  claim(registry, &b1, &b_caps, ttl).await.unwrap();

  // A released lease frees the slot.
  registry.release_lease(first.id).await.unwrap();
  claim(registry, &a3, &a_caps, ttl).await.unwrap();
  assert_eq!(
    registry.count_live_turns(a.id, false).await.unwrap(),
    (2, 3)
  );
}

/// The platform-wide cap spans apps and reports `Platform`.
pub(crate) async fn global_cap_across_apps(registry: &dyn Registry, org: OrgId) {
  let (a, a_dep) = app_with_deployment(registry, org, "app-a").await;
  let (b, b_dep) = app_with_deployment(registry, org, "app-b").await;
  let ttl = chrono::Duration::seconds(TTL);

  let a1 = session_of(registry, &a, &a_dep).await;
  let b1 = session_of(registry, &b, &b_dep).await;
  let b2 = session_of(registry, &b, &b_dep).await;
  claim(registry, &a1, &caps(&a, None, Some(2)), ttl)
    .await
    .unwrap();
  claim(registry, &b1, &caps(&b, None, Some(2)), ttl)
    .await
    .unwrap();
  match claim(registry, &b2, &caps(&b, None, Some(2)), ttl).await {
    Err(RegistryError::CapExceeded { scope, limit }) => {
      assert_eq!(scope, CapScope::Platform);
      assert_eq!(limit, 2);
    }
    other => panic!("expected CapExceeded{{Platform}}, got {other:?}"),
  }
  assert_eq!(
    registry.count_live_turns(a.id, false).await.unwrap(),
    (1, 2)
  );
  assert_eq!(
    registry.count_live_turns(b.id, false).await.unwrap(),
    (1, 2)
  );

  // When both caps trip, the tenant is told about its own limit first.
  let both = caps(&b, Some(1), Some(2));
  match claim(registry, &b2, &both, ttl).await {
    Err(RegistryError::CapExceeded { scope, .. }) => assert_eq!(scope, CapScope::App),
    other => panic!("expected CapExceeded{{App}}, got {other:?}"),
  }
}

/// Released leases and expired-but-unreleased leases don't count.
pub(crate) async fn expired_and_released_leases_do_not_count(registry: &dyn Registry, org: OrgId) {
  let (a, dep) = app_with_deployment(registry, org, "app-a").await;
  let one = caps(&a, Some(1), Some(1));

  let s1 = session_of(registry, &a, &dep).await;
  let s2 = session_of(registry, &a, &dep).await;
  let s3 = session_of(registry, &a, &dep).await;

  // Zero TTL: live in the table (released_at NULL) but already expired.
  claim(registry, &s1, &one, chrono::Duration::zero())
    .await
    .unwrap();
  tokio::time::sleep(std::time::Duration::from_millis(20)).await;
  assert_eq!(
    registry.count_live_turns(a.id, false).await.unwrap(),
    (0, 0)
  );

  let live = claim(registry, &s2, &one, chrono::Duration::seconds(TTL))
    .await
    .unwrap();
  assert!(matches!(
    claim(registry, &s3, &one, chrono::Duration::seconds(TTL)).await,
    Err(RegistryError::CapExceeded { .. })
  ));

  registry.release_lease(live.id).await.unwrap();
  assert_eq!(
    registry.count_live_turns(a.id, false).await.unwrap(),
    (0, 0)
  );
  claim(registry, &s3, &one, chrono::Duration::seconds(TTL))
    .await
    .unwrap();
}

/// No caps behaves exactly like `claim_lease`; a session that already holds
/// a live lease is still a `Conflict` (not a cap error).
pub(crate) async fn uncapped_and_conflict_semantics(registry: &dyn Registry, org: OrgId) {
  let (a, dep) = app_with_deployment(registry, org, "app-a").await;
  let ttl = chrono::Duration::seconds(TTL);
  let none = caps(&a, None, None);

  let s1 = session_of(registry, &a, &dep).await;
  claim(registry, &s1, &none, ttl).await.unwrap();
  assert!(matches!(
    claim(registry, &s1, &none, ttl).await,
    Err(RegistryError::Conflict(_))
  ));
  // Same with a (generous) cap set.
  let roomy = caps(&a, Some(10), Some(10));
  assert!(matches!(
    claim(registry, &s1, &roomy, ttl).await,
    Err(RegistryError::Conflict(_))
  ));
  // Plenty of sessions beyond any cap when uncapped.
  for _ in 0..5 {
    let s = session_of(registry, &a, &dep).await;
    claim(registry, &s, &none, ttl).await.unwrap();
  }
  assert_eq!(
    registry.count_live_turns(a.id, false).await.unwrap(),
    (6, 6)
  );
}

/// 16 concurrent claims against a cap of 3 → exactly 3 win (proves the
/// Postgres advisory lock; SQLite's BEGIN IMMEDIATE gives the same).
pub(crate) async fn concurrent_claims_respect_cap(registry: Arc<dyn Registry>, org: OrgId) {
  let (a, dep) = app_with_deployment(&*registry, org, "app-a").await;
  let cap = caps(&a, Some(3), None);
  let mut sessions = Vec::new();
  for _ in 0..16 {
    sessions.push(session_of(&*registry, &a, &dep).await);
  }
  let tasks: Vec<_> = sessions
    .into_iter()
    .map(|s| {
      let registry = registry.clone();
      tokio::spawn(async move { claim(&*registry, &s, &cap, chrono::Duration::seconds(TTL)).await })
    })
    .collect();
  let (mut won, mut capped) = (0, 0);
  for t in tasks {
    match t.await.unwrap() {
      Ok(_) => won += 1,
      Err(RegistryError::CapExceeded { .. }) => capped += 1,
      Err(e) => panic!("unexpected error: {e:?}"),
    }
  }
  assert_eq!(won, 3, "exactly the cap's worth of claims win");
  assert_eq!(capped, 13);
  assert_eq!(
    registry.count_live_turns(a.id, false).await.unwrap(),
    (3, 3)
  );
}

/// Sandbox turns and endpoint turns are counted apart: a claim counts only
/// turns of its own kind, for the app and platform-wide.
pub(crate) async fn kinds_are_counted_apart(registry: &dyn Registry, org: OrgId) {
  let (a, sandbox) = app_with_deployment(registry, org, "app-a").await;
  let endpoint = registry
    .create_deployment(NewDeployment {
      endpoint_url: Some("https://app-a-xyz.a.run.app/".into()),
      app_id: a.id,
      version: 2,
      manifest: json!({"routes": [{"path": "/test"}]}),
    })
    .await
    .unwrap();
  let ttl = chrono::Duration::seconds(TTL);
  let sandbox_caps = caps(&a, Some(1), Some(1));
  let endpoint_caps = LeaseCaps {
    on_endpoint: true,
    ..caps(&a, Some(2), None)
  };

  // The sandbox turn fills both sandbox caps...
  let s1 = session_of(registry, &a, &sandbox).await;
  claim(registry, &s1, &sandbox_caps, ttl).await.unwrap();
  let s2 = session_of(registry, &a, &sandbox).await;
  assert!(matches!(
    claim(registry, &s2, &sandbox_caps, ttl).await,
    Err(RegistryError::CapExceeded { .. })
  ));
  // ...and none of the endpoint's.
  let e1 = session_of(registry, &a, &endpoint).await;
  let e2 = session_of(registry, &a, &endpoint).await;
  let e3 = session_of(registry, &a, &endpoint).await;
  claim(registry, &e1, &endpoint_caps, ttl).await.unwrap();
  claim(registry, &e2, &endpoint_caps, ttl).await.unwrap();
  match claim(registry, &e3, &endpoint_caps, ttl).await {
    Err(RegistryError::CapExceeded { scope, limit }) => {
      assert_eq!(scope, CapScope::App);
      assert_eq!(limit, 2);
    }
    other => panic!("expected CapExceeded{{App}}, got {other:?}"),
  }
  assert_eq!(
    registry.count_live_turns(a.id, false).await.unwrap(),
    (1, 1)
  );
  assert_eq!(registry.count_live_turns(a.id, true).await.unwrap(), (2, 2));
}
