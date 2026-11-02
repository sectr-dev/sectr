//! The sectr SQLite database — open it once, construct the traits from it.
//!
//! In MVP, `Journal` and `Registry` live on the same database file, so the
//! schema (and its migrations) belong to the *database*, not to either
//! component: [`SqliteStore::open`] is the only thing that runs migrations,
//! and the trait implementations are constructed from the store. When the
//! stores diverge onto separate stacks,
//! this is the seam that splits.

use crate::journal::Result;
use crate::journal::{PgJournal, SqliteJournal};
use crate::migrations::{migrate_postgres, migrate_sqlite};
use crate::registry::{PgRegistry, SqliteRegistry};
use sqlx::PgPool;
use sqlx::sqlite::SqlitePool;
use std::time::Duration;

/// Default journal tail-pool size (the cap on concurrent journal tails;
/// each open tail holds one dedicated connection for LISTEN).
pub const DEFAULT_TAIL_POOL_MAX: u32 = 32;

/// The sectr SQLite database file.
pub struct SqliteStore {
  pool: SqlitePool,
}

impl SqliteStore {
  /// Open (or create) the database at `path` and bring the schema up to
  /// date. Idempotent — safe to call on every `sectr dev` start.
  pub async fn open(path: &str) -> Result<Self> {
    Ok(SqliteStore {
      pool: migrate_sqlite(path).await?,
    })
  }

  /// The underlying pool.
  pub fn pool(&self) -> &SqlitePool {
    &self.pool
  }

  /// The journal over this database.
  pub fn journal(&self) -> SqliteJournal {
    SqliteJournal::new(self.pool.clone())
  }

  /// The registry over this database.
  pub fn registry(&self) -> SqliteRegistry {
    SqliteRegistry::new(self.pool.clone())
  }
}

/// The sectr Postgres database — the production stack. Mirror of
/// [`SqliteStore`]: one open, migrations
/// applied, both store traits constructed from it.
pub struct PgStore {
  url: String,
  pool: PgPool,
  tail_pool: PgPool,
}

impl PgStore {
  /// Connect and bring the schema up to date. Idempotent — safe to call
  /// on every server start. The journal tail pool gets the default size
  /// ([`DEFAULT_TAIL_POOL_MAX`]); use [`PgStore::open_with_tail_max`] to
  /// override.
  pub async fn open(url: &str) -> Result<Self> {
    Self::open_with_tail_max(url, DEFAULT_TAIL_POOL_MAX).await
  }

  /// As [`PgStore::open`], with an explicit journal tail-pool size. Each
  /// open journal tail holds ONE connection for its lifetime (LISTEN),
  /// so this is the cap on concurrent tails; requests never compete
  /// with it (separate pool — see `PgJournal`).
  pub async fn open_with_tail_max(url: &str, tail_pool_max: u32) -> Result<Self> {
    let pool = migrate_postgres(url).await?;
    let tail_pool = sqlx::postgres::PgPoolOptions::new()
      .max_connections(tail_pool_max)
      // A saturated tail pool must fail a tail FAST (the client
      // retries) rather than hang 30s on the default.
      .acquire_timeout(Duration::from_secs(10))
      .connect(url)
      .await?;
    Ok(PgStore {
      url: url.to_string(),
      pool,
      tail_pool,
    })
  }

  /// The connection URL (server restart / re-open paths). Neon URLs carry
  /// credentials — treat as secret-adjacent; never log.
  pub fn url(&self) -> &str {
    &self.url
  }

  /// The underlying pool.
  pub fn pool(&self) -> &PgPool {
    &self.pool
  }

  /// The journal over this database.
  pub fn journal(&self) -> PgJournal {
    PgJournal::new(self.pool.clone(), self.tail_pool.clone())
  }

  /// The registry over this database.
  pub fn registry(&self) -> PgRegistry {
    PgRegistry::new(self.pool.clone())
  }
}
