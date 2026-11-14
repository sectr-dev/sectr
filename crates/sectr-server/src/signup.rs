//! Signups: the hard cap, and the operator's notice of each new one.
//!
//! The hard signup cap — a "backup precaution" against an
//! open-beta flood. The cap counts ORGS (org-per-person; the Clerk
//! exchange is the only path that creates them).
//!
//! Called from `clerk.rs::resolve_account` only where a NEW org is about
//! to be created (returning users always get in), before anything is
//! written. Count-then-create is not atomic: a concurrent burst can
//! overshoot by a few, which is accepted for a backup guard.
//!
//! [`SignupNotifier`]: a Discord message per new account
//! (`SECTR_SIGNUP_WEBHOOK_URL`), fire-and-forget like analytics.

use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use sectr_common::api::ErrorBody;
use sectr_common::registry::PgRegistry;

use crate::config::ServerConfig;

/// The `beta_full` message (the waitlist anchor is `sectr.dev/alpha`).
pub const BETA_FULL_MESSAGE: &str = "Sectr's beta is full. Join the waitlist at sectr.dev/alpha";

/// `Ok` while fewer than `cap` orgs exist (or no cap is set); otherwise
/// the ready-to-return 403 `{error, code: "beta_full"}`.
#[allow(clippy::result_large_err)] // the Err IS the ready-to-return HTTP response
pub async fn ensure_signup_open(pg: &PgRegistry, cap: Option<u32>) -> Result<(), Response> {
  let Some(cap) = cap else {
    return Ok(());
  };
  let orgs = match pg.count_orgs().await {
    Ok(n) => n,
    Err(e) => {
      tracing::error!(error = %e, "signup cap: org count failed");
      return Err(ErrorBody::response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "could not verify signup capacity — try again",
      ));
    }
  };
  if orgs >= u64::from(cap) {
    crate::metrics::quota_rejected("beta_full");
    return Err(
      (
        StatusCode::FORBIDDEN,
        Json(ErrorBody::with_code(BETA_FULL_MESSAGE, "beta_full")),
      )
        .into_response(),
    );
  }
  Ok(())
}

/// Hard cap on the spawned webhook POST: a hung Discord must not pile up
/// tasks, and nothing in the sign-in path waits on it.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// Posts "New signup: account #N" to a Discord webhook when a sign-in
/// creates an account. The message carries **no PII**: not the email,
/// name or Clerk id, and not the org slug (it's derived from the email) —
/// only the account count, and the cap when one is set. Unset
/// `SECTR_SIGNUP_WEBHOOK_URL` → a no-op object.
#[derive(Clone)]
pub struct SignupNotifier {
  /// `None` = disabled.
  inner: Option<(reqwest::Client, String)>,
}

impl SignupNotifier {
  pub fn disabled() -> Self {
    SignupNotifier { inner: None }
  }

  pub fn new(webhook_url: impl Into<String>) -> Self {
    let client = reqwest::Client::builder()
      .timeout(NOTIFY_TIMEOUT)
      .build()
      .unwrap_or_default();
    SignupNotifier {
      inner: Some((client, webhook_url.into())),
    }
  }

  pub fn from_config(config: &ServerConfig) -> Self {
    match &config.signup_webhook_url {
      Some(url) => SignupNotifier::new(url.expose()),
      None => SignupNotifier::disabled(),
    }
  }

  /// Fire-and-forget: counts the accounts and posts on a spawned task;
  /// failures are logged, never propagated.
  pub fn new_account(&self, pg: &PgRegistry, cap: Option<u32>) {
    let Some((client, url)) = self.inner.clone() else {
      return;
    };
    let pg = pg.clone();
    tokio::spawn(async move {
      let count = match pg.count_orgs().await {
        Ok(n) => Some(n),
        Err(e) => {
          tracing::warn!(error = %e, "signup notice: org count failed");
          None
        }
      };
      let payload = DiscordMessage {
        content: signup_message(count, cap),
        // Plain text: nothing in it may ping anyone.
        allowed_mentions: AllowedMentions { parse: Vec::new() },
      };
      match tokio::time::timeout(NOTIFY_TIMEOUT, client.post(&url).json(&payload).send()).await {
        Ok(Ok(resp)) if resp.status().is_success() => {}
        Ok(Ok(resp)) => tracing::warn!(status = %resp.status(), "signup notice rejected"),
        // The URL is the webhook's secret: never log it.
        Ok(Err(e)) => tracing::warn!(error = %e.without_url(), "signup notice failed"),
        Err(_) => tracing::warn!("signup notice timed out"),
      }
    });
  }
}

/// A Discord webhook message.
#[derive(Serialize)]
struct DiscordMessage {
  content: String,
  allowed_mentions: AllowedMentions,
}

/// Which mentions in `content` may ping: here, none.
#[derive(Serialize)]
struct AllowedMentions {
  parse: Vec<String>,
}

/// `New signup: account #42 of 1000`.
fn signup_message(count: Option<u64>, cap: Option<u32>) -> String {
  match (count, cap) {
    (Some(n), Some(cap)) => format!("New signup: account #{n} of {cap}"),
    (Some(n), None) => format!("New signup: account #{n}"),
    (None, _) => "New signup".to_string(),
  }
}

#[cfg(test)]
pub(crate) mod testutil {
  //! An in-process stand-in for the Discord webhook, recording bodies.

  use std::sync::{Arc, Mutex};

  use axum::routing::post;
  use axum::{Json, Router};
  use serde_json::Value;

  pub async fn webhook_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let received: Arc<Mutex<Vec<Value>>> = Arc::default();
    let recorder = received.clone();
    let app = Router::new().route(
      "/webhook",
      post(move |Json(body): Json<Value>| {
        let recorder = recorder.clone();
        async move {
          recorder.lock().unwrap().push(body);
          axum::http::StatusCode::NO_CONTENT
        }
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/webhook"), received)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_message_names_only_the_count() {
    assert_eq!(
      signup_message(Some(42), Some(1000)),
      "New signup: account #42 of 1000"
    );
    assert_eq!(signup_message(Some(42), None), "New signup: account #42");
    assert_eq!(signup_message(None, Some(1000)), "New signup");
  }
}
