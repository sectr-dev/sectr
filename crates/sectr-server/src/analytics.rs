//! The PostHog capture client + the canonical event list — the
//! **product** analytics (platform debugging
//! stays in the OTel plane: `metrics.rs` + spans; Rust = Grafana, PostHog
//! = product events).
//!
//! Deliberately NO PostHog SDK: `/capture` is a ~50-line `reqwest` call
//! [`Analytics::track`] is fire-and-forget — the POST rides
//! a spawned task with a hard timeout, so a PostHog outage can never delay
//! a turn (a hard requirement). Unset `SECTR_POSTHOG_KEY` → a
//! no-op object; every call site degrades to nothing.
//!
//! Identity + property discipline:
//! - the PostHog `distinct_id` is the **org id** — never a user email, no
//!   other PII (the dashboard identifies with the same org id via its `/me`
//!   response, so server and client events correlate in PostHog);
//! - event properties are low-cardinality only (outcome, origin, app id) —
//!   the free-tier equivalent of the label-cardinality rule in metrics.rs.
//!
//! # Canonical event list
//!
//! | Event | Source | Semantics |
//! |---|---|---|
//! | [`events::ORG_CREATED`] | `clerk.rs::exchange` | top of funnel — a Clerk sign-in created a brand-new org (prop `signup: "clerk"`). Returning users never fire it. |
//! | [`events::FIRST_APP_DEPLOYED`] | `deploy.rs::activate` | the org's first deployment reached `active` |
//! | [`events::DEPLOY_SUCCESS`] / [`events::DEPLOY_FAIL`] | `deploy.rs::record_deploy_outcome` (from `run_build`, and endpoint registration with prop `kind: "endpoint"`) | every deploy outcome |
//! | [`events::FIRST_SESSION`] (+ `session_origin` prop) | `ProdSessionService::create_session` | the org's first session; origin = api \| playground |
//! | [`events::FIRST_APPROVAL_USED`] | `ProdSessionService::decide_approval` | the org's first approval decision |
//!
//! "First" semantics are detected AT EMIT TIME (cheap org-scoped store
//! checks — see each call site), so the canonical names are literal events
//! in the stream, trendable directly. PostHog's first-time-for-user
//! filters remain the fallback dedupe if an emit-time check ever degrades
//! (missed store read ⇒ a session is skipped, never a false "first").

use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::ServerConfig;

/// Canonical PostHog event names (see the module
/// docs for each one's source + semantics).
pub mod events {
  pub const ORG_CREATED: &str = "org_created";
  pub const FIRST_APP_DEPLOYED: &str = "first_app_deployed";
  pub const DEPLOY_SUCCESS: &str = "deploy_success";
  pub const DEPLOY_FAIL: &str = "deploy_fail";
  pub const FIRST_SESSION: &str = "first_session";
  pub const FIRST_APPROVAL_USED: &str = "first_approval_used";
  pub const CLI_LOGIN: &str = "cli_login";
}

/// The `first_session` property carrying the request origin (`session_origin`,
/// api vs playground). The same enum that feeds the
/// OTel `origin` attribute feeds this prop — one source of truth.
pub const PROP_SESSION_ORIGIN: &str = "session_origin";
/// The app id, on events scoped to one app. Bounded cardinality (apps per
/// org are few) — NOT session/deployment ids (unbounded).
pub const PROP_APP_ID: &str = "app_id";

/// Hard cap on the spawned capture POST — a hung PostHog must not pile up
/// tasks (an outage must never delay a turn; a zombie task must
/// not leak).
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// PostHog ingest host (`SECTR_POSTHOG_HOST`): us or eu cloud, or a
/// self-hosted instance.
const DEFAULT_POSTHOG_HOST: &str = "https://us.i.posthog.com";

#[derive(Clone)]
pub struct Analytics {
  /// `None` = disabled (no `SECTR_POSTHOG_KEY`) — every call is a no-op.
  inner: Option<Inner>,
}

#[derive(Clone)]
struct Inner {
  client: reqwest::Client,
  /// No trailing slash; `{host}/capture/`.
  host: String,
  key: String,
}

impl Analytics {
  /// Disabled: no-op analytics (tests, dev without a key).
  pub fn disabled() -> Self {
    Analytics { inner: None }
  }

  pub fn new(key: impl Into<String>, host: Option<String>) -> Self {
    let host = host
      .filter(|h| !h.trim().is_empty())
      .unwrap_or_else(|| DEFAULT_POSTHOG_HOST.into())
      .trim_end_matches('/')
      .to_owned();
    let client = reqwest::Client::builder()
      .timeout(CAPTURE_TIMEOUT)
      .build()
      .unwrap_or_default();
    Analytics {
      inner: Some(Inner {
        client,
        host,
        key: key.into(),
      }),
    }
  }

  pub fn from_config(config: &ServerConfig) -> Self {
    match &config.posthog_key {
      Some(key) if !key.trim().is_empty() => {
        Analytics::new(key.clone(), Some(config.posthog_host.clone()))
      }
      _ => Analytics::disabled(),
    }
  }

  pub fn is_enabled(&self) -> bool {
    self.inner.is_some()
  }

  /// Fire-and-forget product event. Returns immediately; the capture
  /// POST rides a spawned task (failures are logged, never propagated —
  /// nothing in the request path may wait on PostHog).
  pub fn track(&self, event: &str, org_id: Uuid, props: Value) {
    let Some(inner) = &self.inner else {
      return;
    };
    let event = event.to_owned();
    let payload = json!({
        "api_key": inner.key,
        "event": event,
        // The org IS the actor (identify by org id, never raw
        // user email). String form — PostHog distinct_ids are strings.
        "distinct_id": org_id.to_string(),
        "properties": props,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let client = inner.client.clone();
    let url = format!("{}/capture/", inner.host);
    tokio::spawn(async move {
      match tokio::time::timeout(CAPTURE_TIMEOUT, client.post(&url).json(&payload).send()).await {
        Ok(Ok(resp)) if resp.status().is_success() => {}
        Ok(Ok(resp)) => {
          tracing::warn!(status = %resp.status(), event, "posthog capture rejected")
        }
        Ok(Err(e)) => tracing::warn!(error = %e, event, "posthog capture failed"),
        Err(_) => tracing::warn!(event, "posthog capture timed out"),
      }
    });
  }
}

#[cfg(test)]
pub(crate) mod testutil {
  //! Shared test pieces: the in-process capture stub + polling helper.
  //! Other modules' tests (sessions, deploy) attach a real
  //! [`Analytics`](super::Analytics) to their harnesses and assert on
  //! captured bodies here.

  use super::*;
  use std::sync::{Arc, Mutex};

  use axum::routing::post;
  use axum::{Json, Router};

  /// In-process capture stub (same pattern as the E2B wire tests): one
  /// POST route that records bodies and answers 200 `{}`.
  pub async fn capture_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let captured: Arc<Mutex<Vec<Value>>> = Arc::default();
    let recorder = captured.clone();
    let app = Router::new().route(
      "/capture/",
      post(move |Json(body): Json<Value>| {
        let recorder = recorder.clone();
        async move {
          recorder.lock().unwrap().push(body);
          Json(json!({}))
        }
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), captured)
  }

  /// Poll until the mock server has seen `n` captures (the track POST is
  /// fire-and-forget — poll, never sleep-and-hope).
  pub async fn wait_for_captures(captured: &Arc<Mutex<Vec<Value>>>, n: usize) -> Vec<Value> {
    for _ in 0..100 {
      if captured.lock().unwrap().len() >= n {
        break;
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    captured.lock().unwrap().clone()
  }
}

#[cfg(test)]
mod tests {
  use super::testutil::{capture_server, wait_for_captures};
  use super::*;

  #[tokio::test]
  async fn track_sends_the_capture_payload() {
    let (host, captured) = capture_server().await;
    let analytics = Analytics::new("phc_test_key", Some(host));
    assert!(analytics.is_enabled());

    let org = Uuid::now_v7();
    analytics.track(
      events::FIRST_SESSION,
      org,
      json!({ PROP_SESSION_ORIGIN: "playground" }),
    );

    let captures = wait_for_captures(&captured, 1).await;
    assert_eq!(captures.len(), 1, "exactly one capture POST");
    let body = &captures[0];
    assert_eq!(body["api_key"], "phc_test_key");
    assert_eq!(body["event"], events::FIRST_SESSION);
    // Org id as distinct_id (string), never an email.
    assert_eq!(body["distinct_id"], org.to_string());
    assert_eq!(body["properties"][PROP_SESSION_ORIGIN], "playground");
    assert!(body["timestamp"].is_string());
  }

  #[tokio::test]
  async fn disabled_analytics_is_a_noop() {
    let (host, captured) = capture_server().await;
    // from_config with an unset key is exactly what prod/dev does
    // without SECTR_POSTHOG_KEY.
    let mut config = test_server_config();
    config.posthog_key = None;
    config.posthog_host = host; // would-be target — must never be hit
    let analytics = Analytics::from_config(&config);
    assert!(!analytics.is_enabled());

    analytics.track(events::DEPLOY_SUCCESS, Uuid::now_v7(), json!({}));
    analytics.track(events::ORG_CREATED, Uuid::now_v7(), json!({}));

    let captures = wait_for_captures(&captured, 0).await;
    assert!(captures.is_empty(), "disabled analytics must not POST");
  }

  fn test_server_config() -> ServerConfig {
    ServerConfig {
      endpoints: Default::default(),
      pg_url: "postgres://unused".into(),
      bind_addr: "127.0.0.1:0".parse().unwrap(),
      secrets_master_key: None,
      docker_socket: "/var/run/docker.sock".into(),
      bootstrap_platform_key: None,
      org_name: "sectr".into(),
      lease_ttl_secs: 30,
      user_token_ttl_secs: 86_400,
      mint_rate_limit_per_hour: 240,
      runner_server_url: "ws://127.0.0.1:1/ingest".into(),
      runner_sidecar_cmd: vec!["true".into()],
      runner_image: None,
      runner_base_image: None,
      runner_network: None,
      runner_resources: Default::default(),
      build_dir: "/tmp".into(),
      dashboard_url: "https://app.sectr.test".into(),
      tail_pool_max: 2,
      artifact_backend: crate::config::ArtifactBackend::Fs,
      artifact_dir: "/tmp".into(),
      gcs_bucket: None,
      gcs_endpoint: None,
      gcp_project_id: None,
      gcp_region: None,
      gcp_ar_repo: "sectr-runners".into(),
      gcp_build_sa_email: None,
      gcp_push_sa_email: None,
      gcp_runner_sa_email: None,
      e2b_api_key: None,
      e2b_domain: "e2b.app".into(),
      e2b_cpu_count: 2,
      e2b_memory_mb: 1024,
      e2b_sandbox_timeout_secs: 3600,
      e2b_puller_sa_json: None,
      platform: crate::config::PlatformKind::Docker,
      posthog_key: None,
      posthog_host: DEFAULT_POSTHOG_HOST.into(),
      signup_webhook_url: None,
      clerk: None,
      limits: crate::limits::LimitsConfig::none(),
    }
  }
}
