//! Test support: same policy as sectr-common's test_pg — fresh throwaway
//! database per test, on the shared pgtemp instance; duplicated (rather
//! than shared) because sectr-common's helper is #[cfg(test)]-gated and a
//! dependency crate's cfg(test) items are invisible to consumers.

#![cfg(test)]

use pgtemp::PgTempDB;
use sqlx::postgres::PgConnectOptions;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::OnceLock;
use tokio::sync::OnceCell;

static PGTEMP: OnceCell<Option<PgTempDB>> = OnceCell::const_new();
static PGTEMP_DATA_DIR: OnceLock<String> = OnceLock::new();

extern "C" fn kill_pgtemp_at_exit() {
  let Some(dir) = PGTEMP_DATA_DIR.get() else {
    return;
  };
  // Immediate shutdown (SIGQUIT): no checkpoint, since the data dir is
  // deleted next.
  let _ = Command::new("pkill").args(["-QUIT", "-f", dir]).status();
  for _ in 0..40 {
    let gone = Command::new("pgrep")
      .args(["-f", dir])
      .status()
      .map(|s| !s.success())
      .unwrap_or(true);
    if gone {
      break;
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
  }
  // Deleted by a detached `rm`, so the test binary doesn't wait for it:
  // one database per test makes it hundreds of MB (over a second to
  // delete). The rm outlives this process.
  if let Some(root) = std::path::Path::new(dir).parent() {
    let _ = Command::new("rm")
      .arg("-rf")
      .arg(root)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn();
  }
}

async fn base_url() -> Option<String> {
  let db = PGTEMP
    .get_or_init(|| async {
      let probe = Command::new("initdb").arg("--version").output();
      let usable = matches!(&probe, Ok(out) if out.status.success());
      if !usable {
        println!(
          "SKIP-REASON: no usable initdb on PATH — Postgres-backed tests \
                     are SKIPPED (install postgresql and put its bin dir on PATH \
                     to run them)"
        );
        return None;
      }
      Some(PgTempDB::async_new().await)
    })
    .await
    .as_ref();
  let db = db?;
  if PGTEMP_DATA_DIR
    .set(db.data_dir().to_string_lossy().into_owned())
    .is_ok()
  {
    unsafe {
      libc::atexit(kill_pgtemp_at_exit as extern "C" fn());
    }
  }
  Some(db.connection_uri())
}

pub async fn fresh_store() -> Option<PgStore> {
  let base = base_url().await?;
  let opts = PgConnectOptions::from_str(&base)
    .expect("valid pgtemp connection URI")
    .database("postgres");
  let admin = sqlx::PgPool::connect_with(opts)
    .await
    .expect("admin connect");
  let name = format!("sectr_test_{}", uuid::Uuid::now_v7().simple());
  let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
    .execute(&admin)
    .await;
  sqlx::query(&format!("CREATE DATABASE {name}"))
    .execute(&admin)
    .await
    .expect("create test database");
  admin.close().await;

  Some(
    PgStore::open(&with_db(&base, &name))
      .await
      .expect("migrate fresh test database"),
  )
}

/// A fresh throwaway database URL WITHOUT opening a store — for tests that
/// hand the URL to another process (the binary-level restart E2E:
/// the spawned sectr-server opens its own pool).
pub async fn fresh_db_url() -> Option<String> {
  let base = base_url().await?;
  let opts = PgConnectOptions::from_str(&base)
    .expect("valid pgtemp connection URI")
    .database("postgres");
  let admin = sqlx::PgPool::connect_with(opts)
    .await
    .expect("admin connect");
  let name = format!("sectr_test_{}", uuid::Uuid::now_v7().simple());
  let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
    .execute(&admin)
    .await;
  sqlx::query(&format!("CREATE DATABASE {name}"))
    .execute(&admin)
    .await
    .expect("create test database");
  admin.close().await;

  Some(with_db(&base, &name))
}

use sectr_common::store::PgStore;

fn with_db(base: &str, db: &str) -> String {
  match base.rsplit_once('/') {
    Some((prefix, _)) => format!("{prefix}/{db}"),
    None => format!("{base}/{db}"),
  }
}
