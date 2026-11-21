//! The server's anonymous usage ping: its version and a random install id,
//! once a day, so self-hosted servers can be counted by version. The body is
//! `sectr_common::usage_ping::Ping` and nothing else, and the docs page
//! `reference/telemetry` is the public description of it.
//!
//! On unless `SECTR_TELEMETRY=off` or `DO_NOT_TRACK` is set (`config.rs`).
//! The install id lives in the database (`Registry::server_install_id`), and
//! only this task asks for it, so a server with pings off never has one.
//!
//! A background task like the reaper and the token GC: it touches no journal,
//! runner or lease, nothing waits on it, and it ends with the process.

use std::sync::Arc;
use std::time::Duration;

use sectr_common::registry::Registry;
use sectr_common::usage_ping::{self, Ping, Program};

/// How long after startup the first ping goes out. A server that crashes and
/// restarts in a loop never gets this far, so it doesn't ping once per restart.
pub const FIRST_PING_DELAY: Duration = Duration::from_secs(60);
/// One ping a day while the server runs.
pub const PING_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Log whether pings are on, then, if they are, send one after `first_delay`
/// and one every `interval` after that. Runs for as long as the server does.
pub async fn run(
  registry: Arc<dyn Registry>,
  enabled: bool,
  url: String,
  first_delay: Duration,
  interval: Duration,
) {
  if !enabled {
    tracing::info!("anonymous telemetry is off");
    return;
  }
  tracing::info!(
    "anonymous telemetry is on: the server version and a random install id, once a day, to \
     {url}. Set {}=off to turn it off: {}",
    usage_ping::SWITCH_ENV,
    usage_ping::DOCS_URL
  );
  let client = reqwest::Client::new();
  tokio::time::sleep(first_delay).await;
  loop {
    // `debug`, not `warn`: a server with no route to the collector (an
    // air-gapped install) would otherwise log a warning every day.
    if let Err(error) = ping(registry.as_ref(), &client, &url).await {
      tracing::debug!(%error, "usage ping not sent; trying again at the next interval");
    }
    tokio::time::sleep(interval).await;
  }
}

async fn ping(registry: &dyn Registry, client: &reqwest::Client, url: &str) -> anyhow::Result<()> {
  let ping = Ping {
    program: Program::Server,
    version: env!("CARGO_PKG_VERSION"),
    install_id: registry.server_install_id().await?,
  };
  usage_ping::send(client, url, &ping).await?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Mutex;

  use axum::Router;
  use axum::http::StatusCode;
  use axum::routing::post;
  use sectr_common::store::SqliteStore;
  use serde_json::{Value, json};

  type Seen = Arc<Mutex<Vec<Value>>>;

  /// A collector stub: records each ping and answers with `status`.
  async fn collector(status: StatusCode) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let recorder = seen.clone();
    let app = Router::new().route(
      "/v1/ping",
      post(move |axum::Json(body): axum::Json<Value>| {
        let recorder = recorder.clone();
        async move {
          recorder.lock().unwrap().push(body);
          status
        }
      }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1/ping"), seen)
  }

  async fn store() -> SqliteStore {
    let path = std::env::temp_dir()
      .join(format!("sectr-usage-ping-test-{}.db", uuid::Uuid::now_v7()))
      .to_string_lossy()
      .into_owned();
    SqliteStore::open(&path).await.unwrap()
  }

  async fn install_ids(store: &SqliteStore) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM server_instance")
      .fetch_one(store.pool())
      .await
      .unwrap()
  }

  async fn wait_for(seen: &Seen, n: usize) -> Vec<Value> {
    for _ in 0..200 {
      if seen.lock().unwrap().len() >= n {
        break;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    seen.lock().unwrap().clone()
  }

  #[tokio::test]
  async fn on_sends_the_version_and_the_registry_s_id() {
    let (url, seen) = collector(StatusCode::NO_CONTENT).await;
    let store = store().await;
    let registry: Arc<dyn Registry> = Arc::new(store.registry());
    let task = tokio::spawn(run(
      registry.clone(),
      true,
      url,
      Duration::ZERO,
      Duration::from_millis(50),
    ));

    let pings = wait_for(&seen, 2).await;
    task.abort();
    let id = registry.server_install_id().await.unwrap();
    assert_eq!(
      pings[0],
      json!({
        "program": "server",
        "version": env!("CARGO_PKG_VERSION"),
        "install_id": id.to_string(),
      })
    );
    // The next interval's ping carries the same id.
    assert_eq!(pings[1], pings[0]);
    assert_eq!(install_ids(&store).await, 1);
  }

  #[tokio::test]
  async fn off_sends_nothing_and_creates_no_id() {
    let (url, seen) = collector(StatusCode::NO_CONTENT).await;
    let store = store().await;
    let registry: Arc<dyn Registry> = Arc::new(store.registry());

    // Off, the task returns straight away, however short the delays.
    run(registry, false, url, Duration::ZERO, Duration::ZERO).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(install_ids(&store).await, 0);
  }

  #[tokio::test]
  async fn the_first_ping_waits_for_the_startup_delay() {
    let (url, seen) = collector(StatusCode::NO_CONTENT).await;
    let store = store().await;
    let registry: Arc<dyn Registry> = Arc::new(store.registry());
    let task = tokio::spawn(run(
      registry,
      true,
      url,
      Duration::from_secs(60),
      PING_INTERVAL,
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    task.abort();
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(install_ids(&store).await, 0, "no id before the first ping");
  }

  #[tokio::test]
  async fn a_refused_ping_is_tried_again_at_the_next_interval() {
    let (url, seen) = collector(StatusCode::INTERNAL_SERVER_ERROR).await;
    let store = store().await;
    let registry: Arc<dyn Registry> = Arc::new(store.registry());
    let task = tokio::spawn(run(
      registry,
      true,
      url,
      Duration::ZERO,
      Duration::from_millis(20),
    ));
    let pings = wait_for(&seen, 3).await;
    task.abort();
    assert!(pings.len() >= 3, "the task survives a failed ping");
  }
}
