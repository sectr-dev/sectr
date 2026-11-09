//! Endpoint deployments: the agent runs on the customer's own serverless
//! or containers, with the sidecar in serve mode (`sectr-worker --serve`,
//! contract in `schema/serve.schema.json`). Instead of starting a sandbox
//! for a turn, the server POSTs a wake to the deployment's endpoint, and
//! the sidecar there dials back over Contract 4 like any other sidecar.
//! Everything after the dial (leases, replay, checking, the reconciler) is
//! unchanged.
//!
//! The wake: [`EndpointPlatform::spawn`] starts a task that POSTs
//! `{"ingest_token"}` to `<endpoint>/wake` and holds the request open for
//! the turn, then waits until the hub says the sidecar has taken its
//! assignment ("delivered"). Before delivery, and only when nothing ran (a
//! connect error, the platform's "no instance available" `429`, `500` or
//! `503`, a busy sidecar's `409`), the task retries. Cloud Run answers
//! `429` when max-instances is reached and `500` when scaling can't keep
//! up. A `500` from the sidecar itself (a panic) is safe to retry too: it
//! answers only once its run has ended, and a run that took the assignment
//! has already flipped the delivery watch, which stops every retry. It
//! never sends a second wake while one is in
//! flight or after delivery: the hub hands the assignment out once, and a
//! second dial on the same token would replace the first connection. A
//! wake that isn't delivered within the deadline, or that fails before
//! delivery, fails the spawn with a message starting `endpoint wake
//! failed:`, and the existing dispatch path journals it as the turn's
//! `SESSION_ERROR` and releases the lease.
//!
//! Registration uses [`EndpointPlatform::probe`] and
//! [`EndpointPlatform::fetch_manifest`]. Every request goes through one
//! client with the address guard ([`guard`]).

pub mod guard;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use opentelemetry::KeyValue;
use reqwest::Url;
use serde_json::{Value, json};
use tokio::task::AbortHandle;
use uuid::Uuid;

use sectr_common::endpoint::{
  ANSWERED_HTTP, COULD_NOT_CONNECT, NO_ANSWER_IN_TIME, PRIVATE_ADDRESS, WAKE_TIMEOUT, WRONG_INGEST,
};
use sectr_common::registry::Registry;

use crate::config::EndpointConfig;
use crate::dispatch::DispatchError;
use crate::ingest::IngestHub;
use crate::platform::{BuildError, BuildOutput, BuildRequest, RunnerPlatform};

/// Teardown handles of endpoint turns start with this.
pub const HANDLE_PREFIX: &str = "endpoint:";

/// Every wake failure's message starts with this, so the dashboard can tell
/// them apart from other session errors.
pub use sectr_common::endpoint::WAKE_FAILED;

/// The registration probe's timeout: long enough for a cold start.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// `POST /manifest`'s timeout: a cold start, the sidecar's dial check (up
/// to about 10 s) and the import (up to 60 s).
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(90);

/// How long a torn-down turn's wake request may still take to be answered.
const TEARDOWN_GRACE: Duration = Duration::from_secs(10);

/// Response bodies are read up to this much (the manifest is capped at
/// 16 MiB by the sidecar; everything else is tiny).
const BODY_CAP: usize = 17 * 1024 * 1024;

/// Errors quote at most this much of an endpoint's response body.
const BODY_QUOTE: usize = 300;

fn record(outcome: &'static str) {
  crate::metrics::ENDPOINT_WAKES.add(1, &[KeyValue::new("outcome", outcome)]);
}

/// The endpoint backend. Built once in `main.rs` and wrapped, with the
/// managed platform, in [`super::routing::RoutingPlatform`].
pub struct EndpointPlatform {
  client: reqwest::Client,
  config: EndpointConfig,
  registry: Arc<dyn Registry>,
  /// The hub, attached after it's created (it holds the platform too, so
  /// a strong reference would be a cycle).
  hub: OnceLock<Weak<IngestHub>>,
  /// In-flight wake tasks by handle; teardown aborts them.
  inflight: Arc<Mutex<HashMap<String, AbortHandle>>>,
}

/// What the registration probe learned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
  /// The endpoint sidecar's version (`sectr_sidecar.version`).
  pub version: String,
}

/// A wake that failed before delivery: the metric outcome and the message.
struct WakeFailure {
  outcome: &'static str,
  message: String,
}

impl EndpointPlatform {
  pub fn new(config: EndpointConfig, registry: Arc<dyn Registry>) -> Self {
    EndpointPlatform {
      client: guard::client(config.allow_private),
      config,
      registry,
      hub: OnceLock::new(),
      inflight: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  /// Attach the hub (once, right after it's created).
  pub fn attach_hub(&self, hub: Weak<IngestHub>) {
    let _ = self.hub.set(hub);
  }

  pub fn config(&self) -> &EndpointConfig {
    &self.config
  }

  /// This server's ingest URL for endpoints, if endpoint deployments are on.
  pub fn ingest_url(&self) -> Option<&str> {
    self.config.ingest_url.as_deref()
  }

  fn inflight(&self) -> std::sync::MutexGuard<'_, HashMap<String, AbortHandle>> {
    self.inflight.lock().unwrap_or_else(|e| e.into_inner())
  }

  /// Parse an endpoint URL and apply the URL rules (no query; guard on:
  /// https, no userinfo or fragment, not localhost or a private IP literal).
  pub fn check_url(&self, raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("the endpoint URL {raw:?} is not a URL: {e}"))?;
    guard::check_url(&url, self.config.allow_private)?;
    Ok(url)
  }

  /// `GET <endpoint>`: is a serve-mode sidecar answering there?
  pub async fn probe(&self, url: &Url) -> Result<ProbeResult, String> {
    guard::check_url(url, self.config.allow_private)?;
    let response = self
      .client
      .get(url.clone())
      .timeout(PROBE_TIMEOUT)
      .send()
      .await
      .map_err(|e| request_failure(url, &e))?;
    let status = response.status();
    let body = read_capped(response).await.unwrap_or_default();
    let identity = serde_json::from_slice::<Value>(&body).ok();
    let sidecar = identity
      .as_ref()
      .and_then(|v| v.get("sectr_sidecar"))
      .filter(|s| s.get("mode").and_then(Value::as_str) == Some("serve"));
    match (status.as_u16(), sidecar) {
      (200, Some(sidecar)) => Ok(ProbeResult {
        version: sidecar
          .get("version")
          .and_then(Value::as_str)
          .unwrap_or_default()
          .to_owned(),
      }),
      (200, None) => Err(format!(
        "the endpoint at {url} answered, but not as a Sectr sidecar in serve mode. Check that \
         the image's CMD is `sectr-worker --serve`."
      )),
      (code, _) => Err(format!(
        "the endpoint at {url} {ANSWERED_HTTP} {code}, not a Sectr sidecar in serve mode. Check \
         that the image's CMD is `sectr-worker --serve`."
      )),
    }
  }

  /// `POST <endpoint>/manifest` with a registration token and the app's
  /// entry point. Returns the sidecar's JSON answer (an `outcome` object,
  /// see `schema/serve.schema.json`), or why there was none.
  pub async fn fetch_manifest(&self, url: &Url, token: &str, entry: &str) -> Result<Value, String> {
    guard::check_url(url, self.config.allow_private)?;
    let target = join(url, "manifest");
    let response = self
      .client
      .post(target.clone())
      .timeout(MANIFEST_TIMEOUT)
      .json(&json!({"registration_token": token, "entry": entry}))
      .send()
      .await
      .map_err(|e| {
        if e.is_timeout() {
          format!(
            "the endpoint didn't answer the manifest request within {}s",
            MANIFEST_TIMEOUT.as_secs()
          )
        } else {
          request_failure(url, &e)
        }
      })?;
    let status = response.status().as_u16();
    let body = read_capped(response)
      .await
      .map_err(|e| format!("reading the endpoint's manifest answer: {e}"))?;
    if status != 200 {
      return Err(format!(
        "the endpoint answered the manifest request with HTTP {status}: {}",
        quote(&body)
      ));
    }
    serde_json::from_slice::<Value>(&body)
      .ok()
      .filter(|v| v.get("outcome").and_then(Value::as_str).is_some())
      .ok_or_else(|| format!("the endpoint's manifest answer isn't one: {}", quote(&body)))
  }

  /// Wake the endpoint for one turn and wait until it's delivered. See
  /// the module docs.
  pub async fn wake(
    &self,
    session: Uuid,
    invocation_id: Uuid,
    token: &str,
    endpoint_url: &str,
  ) -> Result<Option<String>, DispatchError> {
    let hub = self
      .hub
      .get()
      .and_then(Weak::upgrade)
      .ok_or_else(|| DispatchError::Unavailable("the ingest hub is not attached".into()))?;
    let url = Url::parse(endpoint_url).map_err(|e| {
      record("http_error");
      DispatchError::Wake(format!("{WAKE_FAILED} the endpoint URL is not a URL: {e}"))
    })?;
    if let Err(reason) = guard::check_url(&url, self.config.allow_private) {
      record("blocked");
      return Err(DispatchError::Wake(format!("{WAKE_FAILED} {reason}")));
    }
    let Some(mut delivered) = hub.delivery_watch(session).await else {
      return Err(DispatchError::Unavailable(
        "the turn's grant is gone before its wake was sent".into(),
      ));
    };

    let handle = format!("{HANDLE_PREFIX}{invocation_id}");
    let (failed_tx, failed_rx) = tokio::sync::oneshot::channel::<WakeFailure>();
    // The failure a retry is waiting out, so a deadline that lands during
    // the backoff says why. Cleared whenever a request goes out.
    let last_retryable = Arc::new(Mutex::new(None::<WakeFailure>));
    let task = tokio::spawn(wake_task(WakeTask {
      client: self.client.clone(),
      url: url.clone(),
      token: token.to_owned(),
      delivered: delivered.clone(),
      turn_timeout: self.config.turn_timeout,
      ingest_url: self.config.ingest_url.clone().unwrap_or_default(),
      failed: failed_tx,
      last_retryable: last_retryable.clone(),
      inflight: self.inflight.clone(),
      handle: handle.clone(),
    }));
    self.inflight().insert(handle.clone(), task.abort_handle());

    let deadline = self.config.wake_deadline;
    let result = tokio::select! {
      seen = delivered.wait_for(|d| *d) => match seen {
        Ok(_) => Ok(()),
        // The grant went away first (the turn ended or was reclaimed).
        Err(_) => Err(WakeFailure {
          outcome: "deadline",
          message: format!("{WAKE_FAILED} the turn ended before the endpoint started it"),
        }),
      },
      failure = failed_rx => match failure {
        Ok(failure) => Err(failure),
        // The task ended without failing or delivering (it can't), or was
        // aborted by a teardown.
        Err(_) => Err(WakeFailure {
          outcome: "deadline",
          message: format!("{WAKE_FAILED} the wake was cancelled"),
        }),
      },
      _ = tokio::time::sleep(deadline) => {
        let retried = last_retryable.lock().unwrap_or_else(|e| e.into_inner()).take();
        Err(retried.unwrap_or(WakeFailure {
          outcome: "deadline",
          message: format!(
            "{WAKE_FAILED} the endpoint {WAKE_TIMEOUT} {}s",
            deadline.as_secs()
          ),
        }))
      }
    };
    match result {
      Ok(()) => {
        record("delivered");
        Ok(Some(handle))
      }
      Err(failure) => {
        // A delivery that raced the failure wins: the sidecar is running
        // the turn, and failing the spawn now would strand it.
        if *delivered.borrow() {
          record("delivered");
          return Ok(Some(handle));
        }
        task.abort();
        self.inflight().remove(&handle);
        record(failure.outcome);
        tracing::warn!(%session, %invocation_id, error = %failure.message, "endpoint wake failed");
        Err(DispatchError::Wake(failure.message))
      }
    }
  }
}

struct WakeTask {
  client: reqwest::Client,
  url: Url,
  token: String,
  delivered: tokio::sync::watch::Receiver<bool>,
  turn_timeout: Duration,
  ingest_url: String,
  failed: tokio::sync::oneshot::Sender<WakeFailure>,
  last_retryable: Arc<Mutex<Option<WakeFailure>>>,
  inflight: Arc<Mutex<HashMap<String, AbortHandle>>>,
  handle: String,
}

/// Send the wake (retrying only before delivery, only when nothing reached
/// a sidecar), then hold the request for the turn. One request in flight
/// at a time, always.
async fn wake_task(task: WakeTask) {
  let WakeTask {
    client,
    url,
    token,
    delivered,
    turn_timeout,
    ingest_url,
    failed,
    last_retryable,
    inflight,
    handle,
  } = task;
  let host = url.host_str().unwrap_or_default().to_owned();
  let target = join(&url, "wake");
  let mut backoff = Duration::from_millis(250);
  let fail = |failure: WakeFailure| {
    let _ = failed.send(failure);
  };
  let failure = loop {
    // A deadline that lands while this request is out is this request's
    // (a cold start, say), not the previous attempt's: the remembered
    // failure only explains a deadline during the backoff sleep.
    last_retryable
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .take();
    let sent = client
      .post(target.clone())
      .timeout(turn_timeout)
      .json(&json!({"ingest_token": token}))
      .send()
      .await;
    let was_delivered = *delivered.borrow();
    let retryable = match sent {
      Err(e) if guard::is_blocked_error(&e) => {
        break WakeFailure {
          outcome: "blocked",
          message: format!(
            "{WAKE_FAILED} {host} {PRIVATE_ADDRESS}, which this server doesn't allow"
          ),
        };
      }
      Err(e) if was_delivered => {
        record("broken_after_delivery");
        tracing::warn!(error = %e, "the endpoint's wake request broke mid-turn; the lease covers the turn");
        break_done(&inflight, &handle);
        return;
      }
      Err(e) if e.is_connect() => WakeFailure {
        outcome: "connect_error",
        message: format!(
          "{WAKE_FAILED} {COULD_NOT_CONNECT} {host} ({})",
          source_text(&e)
        ),
      },
      Err(e) => {
        break WakeFailure {
          outcome: "connect_error",
          message: format!(
            "{WAKE_FAILED} the request to {host} failed ({})",
            source_text(&e)
          ),
        };
      }
      Ok(response) => {
        let status = response.status().as_u16();
        let body = read_capped(response).await.unwrap_or_default();
        if status == 200 {
          let outcome = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|v| v.get("outcome").and_then(Value::as_str).map(str::to_owned));
          if !*delivered.borrow() {
            break match outcome.as_deref() {
              // Before delivery, a rejected token can only mean the image
              // dials some other server.
              Some("token_rejected") => WakeFailure {
                outcome: "token_rejected",
                message: format!(
                  "{WAKE_FAILED} the endpoint's sidecar {WRONG_INGEST} \
                   turn. Check that its SECTR_INGEST_URL is {ingest_url}"
                ),
              },
              // The sidecar says the turn ended, but it never took the
              // assignment from this server.
              _ => WakeFailure {
                outcome: "http_error",
                message: format!(
                  "{WAKE_FAILED} {host} answered the wake without starting the turn: {}",
                  quote(&body)
                ),
              },
            };
          }
          record(match outcome.as_deref() {
            Some("token_rejected") => "token_rejected",
            Some(_) => "turn_ended",
            None => "http_error",
          });
          tracing::info!(
            outcome = outcome.as_deref().unwrap_or("unknown"),
            "endpoint wake finished"
          );
          break_done(&inflight, &handle);
          return;
        }
        if *delivered.borrow() {
          record("http_error");
          tracing::warn!(
            status,
            "the endpoint's wake answered an error after the turn started"
          );
          break_done(&inflight, &handle);
          return;
        }
        if matches!(status, 409 | 429 | 500 | 503) {
          WakeFailure {
            outcome: "http_error",
            message: format!(
              "{WAKE_FAILED} {host} {ANSWERED_HTTP} {status}: {}",
              quote(&body)
            ),
          }
        } else {
          break WakeFailure {
            outcome: "http_error",
            message: format!(
              "{WAKE_FAILED} {host} {ANSWERED_HTTP} {status}: {}",
              quote(&body)
            ),
          };
        }
      }
    };
    // Retry: nothing reached a sidecar (or a busy one refused before
    // dialing). The spawn's deadline bounds this; it aborts the task.
    if *delivered.borrow() {
      // Delivered meanwhile by this very request's sidecar? Not possible
      // for a refusal, but never send a second wake after delivery.
      break_done(&inflight, &handle);
      return;
    }
    tracing::debug!(error = %retryable.message, "endpoint wake not delivered yet; retrying");
    *last_retryable.lock().unwrap_or_else(|e| e.into_inner()) = Some(retryable);
    tokio::time::sleep(backoff).await;
    backoff = (backoff * 2).min(Duration::from_secs(2));
  };
  fail(failure);
  break_done(&inflight, &handle);
}

/// A wake task's own end: forget its abort handle.
fn break_done(inflight: &Mutex<HashMap<String, AbortHandle>>, handle: &str) {
  inflight
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .remove(handle);
}

/// `base` treated as a directory: one `/` ensured, then `leaf` appended.
fn join(base: &Url, leaf: &str) -> Url {
  let mut dir = base.clone();
  dir.set_query(None);
  dir.set_fragment(None);
  if !dir.path().ends_with('/') {
    let path = format!("{}/", dir.path());
    dir.set_path(&path);
  }
  dir.join(leaf).expect("a relative leaf joins")
}

/// The innermost error's text (reqwest's own Display hides the cause).
fn source_text(error: &(dyn std::error::Error + 'static)) -> String {
  let mut current = error;
  while let Some(next) = current.source() {
    current = next;
  }
  current.to_string()
}

fn request_failure(url: &Url, error: &reqwest::Error) -> String {
  let host = url.host_str().unwrap_or_default();
  if guard::is_blocked_error(error) {
    format!("{host} {PRIVATE_ADDRESS}, which this server doesn't allow")
  } else if error.is_timeout() {
    format!("the endpoint at {url} {NO_ANSWER_IN_TIME}")
  } else if error.is_connect() {
    format!("{COULD_NOT_CONNECT} {host} ({})", source_text(error))
  } else {
    format!("the request to {host} failed ({})", source_text(error))
  }
}

/// The first [`BODY_QUOTE`] characters of a body, control characters
/// removed: safe to put in an error a user reads.
fn quote(body: &[u8]) -> String {
  String::from_utf8_lossy(body)
    .chars()
    .filter(|c| !c.is_control())
    .take(BODY_QUOTE)
    .collect()
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
  let mut body = Vec::new();
  while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
    if body.len() + chunk.len() > BODY_CAP {
      return Err(format!("the answer is over {BODY_CAP} bytes"));
    }
    body.extend_from_slice(&chunk);
  }
  Ok(body)
}

#[async_trait]
impl RunnerPlatform for EndpointPlatform {
  /// Never reached: endpoint deployments have no build, and the routing
  /// platform sends every build to the managed platform.
  async fn build(&self, _req: BuildRequest) -> Result<BuildOutput, BuildError> {
    Err(BuildError::bare("endpoint deployments are not built"))
  }

  fn provisional_build_ref(&self, _deployment_id: Uuid, _version: i64) -> String {
    "endpoint".into()
  }

  async fn spawn(
    &self,
    session: Uuid,
    deployment_id: Uuid,
    invocation_id: Uuid,
    ingest_token: &str,
  ) -> Result<Option<String>, DispatchError> {
    let deployment = self
      .registry
      .get_deployment(deployment_id)
      .await
      .map_err(|e| DispatchError::Unavailable(e.to_string()))?;
    let endpoint_url = deployment.endpoint_url.ok_or_else(|| {
      DispatchError::Spawn(format!("deployment {deployment_id} has no endpoint URL"))
    })?;
    self
      .wake(session, invocation_id, ingest_token, &endpoint_url)
      .await
  }

  /// Abort the server's half of the wake request, after a short grace.
  /// The customer's sidecar stops itself: its token is dead (the abort
  /// frame, a `Conflict` on its next append, or a 401 on re-dial), and it
  /// answers the wake soon after. The grace lets that answer arrive, so a
  /// teardown at a turn boundary (platforms whose own runners don't end
  /// themselves tear down at every boundary) still records how the wake
  /// ended. A wake task that ends first removes itself.
  async fn teardown(&self, session: Uuid, handle: Option<&str>) {
    let Some(handle) = handle.map(str::to_owned) else {
      return;
    };
    if !self.inflight().contains_key(&handle) {
      return;
    }
    let inflight = self.inflight.clone();
    tokio::spawn(async move {
      tokio::time::sleep(TEARDOWN_GRACE).await;
      let task = inflight
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&handle);
      if let Some(task) = task {
        tracing::debug!(%session, "endpoint teardown: aborting the wake request");
        task.abort();
      }
    });
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn join_treats_the_endpoint_as_a_directory() {
    let cases = [
      ("https://a.example", "https://a.example/wake"),
      ("https://a.example/", "https://a.example/wake"),
      ("https://a.example/agent", "https://a.example/agent/wake"),
      ("https://a.example/agent/", "https://a.example/agent/wake"),
      (
        "https://a.example/agent?x=1",
        "https://a.example/agent/wake",
      ),
    ];
    for (base, want) in cases {
      assert_eq!(
        join(&Url::parse(base).unwrap(), "wake").as_str(),
        want,
        "{base}"
      );
    }
  }

  #[test]
  fn quotes_are_short_and_clean() {
    let body = format!("bad\n\x1b[31mthing{}", "x".repeat(500));
    let quoted = quote(body.as_bytes());
    assert!(quoted.starts_with("bad[31mthing"));
    assert_eq!(quoted.chars().count(), BODY_QUOTE);
  }
}
