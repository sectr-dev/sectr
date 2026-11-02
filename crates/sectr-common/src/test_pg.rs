//! Test support for Postgres-backed tests (PgJournal, PgRegistry, the
//! Postgres migrations).
//!
//! One pgtemp server per test process (crates.io: pgtemp — a real
//! postgres, no docker): spawned lazily on the first Postgres test, and
//! each test still gets its own throwaway `sectr_test_<uuid>` database on
//! it, so parallel tests don't collide. pgtemp's data lives in a tempdir
//! wiped at shutdown, so nothing leaks across runs.
//!
//! No `initdb` on PATH → SKIP with a loud note (postgres binaries:
//! Debian/Ubuntu `postgresql` package + add `/usr/lib/postgresql/*/bin` to
//! PATH; CI does exactly that).
//!
//! Cleanup: Rust leaks statics at process exit WITHOUT running `Drop`, so
//! the postmaster would otherwise outlive the test binary and keep its
//! tempdir pinned. `libc::atexit` closes that hole: immediate-shutdown (SIGQUIT)
//! the postmaster, wait for it to exit, then reap the whole pgtemp tempdir
//! (with a detached `rm`, so the test binary exits without waiting)
//! (its own Drop would have, and never runs for a static). A hard
//! SIGKILL/abort still orphans both — that's the residual failure mode.

#![cfg(test)]

use pgtemp::PgTempDB;
use sqlx::postgres::PgConnectOptions;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::OnceLock;
use tokio::sync::OnceCell;

/// The shared pgtemp instance (created lazily on first Postgres test).
static PGTEMP: OnceCell<Option<PgTempDB>> = OnceCell::const_new();
/// Set on first use so the atexit hook can find the postmaster (matched by
/// its unique data-dir path on the command line).
static PGTEMP_DATA_DIR: OnceLock<String> = OnceLock::new();

extern "C" fn kill_pgtemp_at_exit() {
  let Some(dir) = PGTEMP_DATA_DIR.get() else {
    return;
  };
  // stdout nulled: pgrep/pkill otherwise print matching PIDs into the test
  // output (one line per poll).
  // Immediate shutdown (SIGQUIT): no checkpoint, since the data dir is
  // deleted next. A fast shutdown (SIGINT) checkpoints every per-test
  // database first, which took seconds per test binary.
  let _ = Command::new("pkill")
    .args(["-QUIT", "-f", dir])
    .stdout(Stdio::null())
    .status();
  for _ in 0..40 {
    let gone = Command::new("pgrep")
      .args(["-f", dir])
      .stdout(Stdio::null())
      .status()
      .map(|s| !s.success())
      .unwrap_or(true);
    if gone {
      break;
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
  }
  // pgtemp's TempDir is the data dir's parent — removing the parent takes
  // pg_data_dir and every scratch file with it.
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

/// The test server's base URL (admin/connect through it, then `CREATE
/// DATABASE sectr_test_*` per test). `None` means "skip".
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

fn with_db(base: &str, db: &str) -> String {
  // The test URLs are plain `postgresql://user:pass@host:port/name` —
  // swap the path segment (query params, if ever added, survive).
  match base.rsplit_once('/') {
    Some((prefix, _)) => format!("{prefix}/{db}"),
    None => format!("{base}/{db}"),
  }
}

/// Open a `PgStore` on a fresh, uniquely-named database. `None` skips the
/// test (no test Postgres available).
pub async fn fresh_store() -> Option<crate::store::PgStore> {
  let base = base_url().await?;
  let opts = PgConnectOptions::from_str(&base)
    .expect("valid pgtemp connection URI")
    .database("postgres");
  let admin = sqlx::PgPool::connect_with(opts)
    .await
    .expect("admin connect");
  let name = format!("sectr_test_{}", uuid::Uuid::now_v7().simple());
  // Leftovers from an interrupted previous run (same name can't exist,
  // but be idempotent anyway); FORCE drops dependents/connections.
  let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
    .execute(&admin)
    .await;
  sqlx::query(&format!("CREATE DATABASE {name}"))
    .execute(&admin)
    .await
    .expect("create test database");
  admin.close().await;

  let store = crate::store::PgStore::open(&with_db(&base, &name))
    .await
    .expect("migrate fresh test database");
  Some(store)
}
