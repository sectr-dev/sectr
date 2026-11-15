//! Anonymous usage pings: what the `sectr` CLI and `sectr-server` send to
//! count active installs by version, and the rule that turns them off.
//!
//! A ping is the [`Ping`] struct and nothing else: which program, its version
//! and a random install id. The docs page `reference/telemetry` lists the same
//! fields, and a test here fails if the two drift apart. The CLI and the
//! server each decide when to send and where the id lives. This module holds
//! what they share.
//!
//! Not to be confused with [`crate::telemetry`], the server's own traces,
//! metrics and logs, which go only where its operator points them.

use std::time::Duration;

use serde::Serialize;
use uuid::Uuid;

/// Where pings go: a collector that drops the sender's IP address and
/// forwards the three fields.
pub const DEFAULT_URL: &str = "https://telemetry.sectr.dev/v1/ping";
/// The page that lists what is sent and how to turn it off.
pub const DOCS_URL: &str = "https://docs.sectr.dev/reference/telemetry/";

/// Turns pings off when set to `off`, `false`, `0` or `no`.
pub const SWITCH_ENV: &str = "SECTR_TELEMETRY";
/// The cross-tool convention: any value but empty or `0` turns pings off.
pub const DO_NOT_TRACK_ENV: &str = "DO_NOT_TRACK";
/// Sends pings to another collector, for tests and for anyone who wants to
/// look at them.
pub const URL_ENV: &str = "SECTR_TELEMETRY_URL";

/// How long one ping may take, start to finish.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Which program is pinging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Program {
  Cli,
  Server,
}

/// The whole body of a ping. A field added here is a field sent: change the
/// docs page and the collector in the same breath.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ping {
  pub program: Program,
  /// The program's own version, e.g. `env!("CARGO_PKG_VERSION")`.
  pub version: &'static str,
  /// Random (UUIDv4), so it says nothing about the machine or when the id
  /// was made.
  pub install_id: Uuid,
}

/// A fresh install id. v4, not the v7 used elsewhere: a v7 id carries its
/// creation time.
pub fn new_install_id() -> Uuid {
  Uuid::new_v4()
}

/// What the environment says about pings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvSwitch {
  /// Nothing turns pings off.
  Unset,
  /// Off, and the name of the variable that says so.
  Off(&'static str),
  /// `SECTR_TELEMETRY` holds this value, which is neither on nor off. The
  /// server refuses to start. The CLI warns and sends nothing.
  Invalid(String),
}

/// The process environment, as the getter [`env_switch`] and [`url`] take.
pub fn process_env(name: &str) -> Option<String> {
  std::env::var(name).ok()
}

/// The one opt-out rule for both programs. `get` reads an environment
/// variable ([`process_env`] outside tests). Either variable can turn pings
/// off, and neither can turn them back on over the other.
pub fn env_switch(get: impl Fn(&str) -> Option<String>) -> EnvSwitch {
  let switch = get(SWITCH_ENV).map(|v| v.trim().to_ascii_lowercase());
  match switch.as_deref() {
    None | Some("" | "on" | "true" | "1" | "yes") => {}
    Some("off" | "false" | "0" | "no") => return EnvSwitch::Off(SWITCH_ENV),
    Some(_) => return EnvSwitch::Invalid(get(SWITCH_ENV).unwrap_or_default()),
  }
  match get(DO_NOT_TRACK_ENV).as_deref().map(str::trim) {
    None | Some("" | "0") => EnvSwitch::Unset,
    Some(_) => EnvSwitch::Off(DO_NOT_TRACK_ENV),
  }
}

/// The collector's URL: `SECTR_TELEMETRY_URL` when set, else [`DEFAULT_URL`].
pub fn url(get: impl Fn(&str) -> Option<String>) -> String {
  get(URL_ENV)
    .map(|u| u.trim().to_owned())
    .filter(|u| !u.is_empty())
    .unwrap_or_else(|| DEFAULT_URL.to_owned())
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
  #[error("the collector answered {0}")]
  Status(reqwest::StatusCode),
  #[error(transparent)]
  Http(#[from] reqwest::Error),
}

/// Send one ping. Only a 2xx answer counts, within [`SEND_TIMEOUT`].
pub async fn send(client: &reqwest::Client, url: &str, ping: &Ping) -> Result<(), SendError> {
  let response = client
    .post(url)
    .json(ping)
    .timeout(SEND_TIMEOUT)
    .send()
    .await?;
  if response.status().is_success() {
    Ok(())
  } else {
    Err(SendError::Status(response.status()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::BTreeSet;
  use std::sync::{Arc, Mutex};

  use axum::Router;
  use axum::http::StatusCode;
  use axum::routing::post;
  use serde_json::Value;

  fn switch(vars: &[(&str, &str)]) -> EnvSwitch {
    env_switch(|name| {
      vars
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| (*v).to_owned())
    })
  }

  #[test]
  fn env_switch_table() {
    assert_eq!(switch(&[]), EnvSwitch::Unset);
    for on in ["", "  ", "on", "true", "1", "yes", "ON", " On "] {
      assert_eq!(switch(&[(SWITCH_ENV, on)]), EnvSwitch::Unset, "{on:?}");
    }
    for off in ["off", "false", "0", "no", "OFF", "False", " off "] {
      assert_eq!(
        switch(&[(SWITCH_ENV, off)]),
        EnvSwitch::Off(SWITCH_ENV),
        "{off:?}"
      );
    }
    for set in ["1", "true", "yes", "anything"] {
      assert_eq!(
        switch(&[(DO_NOT_TRACK_ENV, set)]),
        EnvSwitch::Off(DO_NOT_TRACK_ENV),
        "{set:?}"
      );
    }
    for unset in ["", "0", " 0 "] {
      assert_eq!(
        switch(&[(DO_NOT_TRACK_ENV, unset)]),
        EnvSwitch::Unset,
        "{unset:?}"
      );
    }
    assert_eq!(
      switch(&[(SWITCH_ENV, "maybe")]),
      EnvSwitch::Invalid("maybe".into())
    );
  }

  #[test]
  fn either_variable_turns_pings_off() {
    // `on` doesn't override DO_NOT_TRACK, and a typo is reported even when
    // DO_NOT_TRACK would have turned pings off anyway.
    assert_eq!(
      switch(&[(SWITCH_ENV, "on"), (DO_NOT_TRACK_ENV, "1")]),
      EnvSwitch::Off(DO_NOT_TRACK_ENV)
    );
    assert_eq!(
      switch(&[(SWITCH_ENV, "off"), (DO_NOT_TRACK_ENV, "0")]),
      EnvSwitch::Off(SWITCH_ENV)
    );
    assert_eq!(
      switch(&[(SWITCH_ENV, "offf"), (DO_NOT_TRACK_ENV, "1")]),
      EnvSwitch::Invalid("offf".into())
    );
  }

  #[test]
  fn url_defaults_and_overrides() {
    assert_eq!(url(|_| None), DEFAULT_URL);
    assert_eq!(url(|_| Some("  ".into())), DEFAULT_URL);
    assert_eq!(
      url(|name| (name == URL_ENV).then(|| "http://127.0.0.1:9/v1/ping".into())),
      "http://127.0.0.1:9/v1/ping"
    );
  }

  fn keys(value: &Value) -> BTreeSet<String> {
    value
      .as_object()
      .expect("a JSON object")
      .keys()
      .cloned()
      .collect()
  }

  /// The first ```json block of the docs page: the example ping.
  fn docs_example() -> Value {
    let path = concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../../docs-site/src/content/docs/reference/telemetry.md"
    );
    let page = std::fs::read_to_string(path).expect("read the usage pings docs page");
    let start = page.find("```json\n").expect("a json block in the page") + "```json\n".len();
    let end = start + page[start..].find("```").expect("the block's end");
    serde_json::from_str(&page[start..end]).expect("the docs example parses")
  }

  #[test]
  fn ping_body_is_exactly_three_fields() {
    let id = new_install_id();
    assert_eq!(id.get_version_num(), 4, "random, with no timestamp in it");
    let ping = Ping {
      program: Program::Cli,
      version: "1.2.3",
      install_id: id,
    };
    let body = serde_json::to_value(&ping).unwrap();
    assert_eq!(
      body,
      serde_json::json!({"program": "cli", "version": "1.2.3", "install_id": id.to_string()})
    );
    let server = serde_json::to_value(Ping {
      program: Program::Server,
      ..ping
    })
    .unwrap();
    assert_eq!(server["program"], "server");

    // The docs page shows the same fields, no more and no fewer.
    let example = docs_example();
    assert_eq!(keys(&example), keys(&body));
    assert!(
      example["install_id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .is_ok()
    );
  }

  /// A collector stub answering every ping with `status`, after `delay`.
  async fn stub(status: StatusCode, delay: Duration) -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let recorder = seen.clone();
    let app = Router::new().route(
      "/v1/ping",
      post(move |axum::Json(body): axum::Json<Value>| {
        let recorder = recorder.clone();
        async move {
          recorder.lock().unwrap().push(body);
          tokio::time::sleep(delay).await;
          status
        }
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1/ping"), seen)
  }

  fn ping() -> Ping {
    Ping {
      program: Program::Server,
      version: "1.2.3",
      install_id: new_install_id(),
    }
  }

  #[tokio::test]
  async fn send_succeeds_on_2xx_only() {
    let client = reqwest::Client::new();
    let ping = ping();

    let (url, seen) = stub(StatusCode::NO_CONTENT, Duration::ZERO).await;
    send(&client, &url, &ping).await.expect("204 is success");
    assert_eq!(
      *seen.lock().unwrap(),
      vec![serde_json::to_value(&ping).unwrap()],
      "the body is the ping and nothing else"
    );

    let (url, _) = stub(StatusCode::INTERNAL_SERVER_ERROR, Duration::ZERO).await;
    assert!(matches!(
      send(&client, &url, &ping).await,
      Err(SendError::Status(StatusCode::INTERNAL_SERVER_ERROR))
    ));
  }

  #[tokio::test]
  async fn send_gives_up_on_a_collector_that_never_answers() {
    let (url, _) = stub(StatusCode::NO_CONTENT, Duration::from_secs(30)).await;
    let started = std::time::Instant::now();
    let result = send(&reqwest::Client::new(), &url, &ping()).await;
    assert!(matches!(result, Err(SendError::Http(e)) if e.is_timeout()));
    assert!(started.elapsed() < SEND_TIMEOUT + Duration::from_secs(2));
  }
}
