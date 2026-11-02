//! Embedded migrations for the sectr database — the whole data model
//! (`schema/data-model.sql`), not journal-specific: `Journal` and `Registry`
//! are implemented over the same DB in MVP, so one migration set serves both.
//! When the stores diverge onto separate stacks, each stack takes its own
//! migration set from its own store's schema.
//!
//! Versioned, checksummed, and idempotent: applied versions are recorded in
//! a `sectr_migrations` table together with the SHA-256 of the migration's
//! SQL, so running [`migrate_sqlite`] on an existing database is a no-op —
//! this is what makes `sectr dev` safe across restarts. Schema evolution =
//! add a new numbered file; next open applies it. Re-applying a version
//! whose content changed after being applied fails loudly (checksum
//! mismatch) instead of silently keeping a stale schema.

use crate::journal::{JournalError, Result};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

/// The ordered SQLite migration set: `(version, sql)`.
pub const SQLITE_MIGRATIONS: &[(i64, &str)] = &[
  (1, include_str!("sqlite/0001_initial.sql")),
  (2, include_str!("sqlite/0002_session_origin.sql")),
  (3, include_str!("sqlite/0003_deployment_build_ref.sql")),
  (4, include_str!("sqlite/0004_deployment_error.sql")),
  (5, include_str!("sqlite/0005_alpha_allowlist.sql")),
  (6, include_str!("sqlite/0006_user_tokens.sql")),
  (7, include_str!("sqlite/0007_clerk_users.sql")),
  (8, include_str!("sqlite/0008_cli_logins.sql")),
];

/// The ordered Postgres migration set: `(version, sql)` — a verbatim port of
/// `schema/data-model.sql` (canonical dialect), minus BEGIN/COMMIT (the
/// runner supplies one transaction per version).
pub const POSTGRES_MIGRATIONS: &[(i64, &str)] = &[
  (1, include_str!("postgres/0001_initial.sql")),
  (2, include_str!("postgres/0002_session_origin.sql")),
  (3, include_str!("postgres/0003_deployment_build_ref.sql")),
  (4, include_str!("postgres/0004_deployment_error.sql")),
  (5, include_str!("postgres/0005_alpha_allowlist.sql")),
  (6, include_str!("postgres/0006_user_tokens.sql")),
  (7, include_str!("postgres/0007_clerk_users.sql")),
  (8, include_str!("postgres/0008_cli_logins.sql")),
];

/// Checksums of migrations from before their leading comments were reworded.
/// No statement changed, so databases that recorded one of these stay valid.
const COMMENT_ONLY_EDITS: &[(i64, &str)] = &[
  (
    5,
    "2731f23ec61d98e9a55bcf48774c9a33dfc7d43b973f0decbe64aba97c5f3e2f",
  ),
  (
    5,
    "235ab32d8f59412d0e1726df836e09b0669b0dcab33f319cd8fe806b63c87bb5",
  ),
  (
    6,
    "2d4e823558f5dd845734fe6d20cb1f162f3848e7a5f0c2e3b45e2bb98f707d03",
  ),
  (
    6,
    "ef105c87b6da648699e821d51f5ec0df583c8f5c6f4e2f92b9ac777d89407c6a",
  ),
  (
    7,
    "deea7fac63f6dfd6525825ebd1e87a8588ac35ebee9806483a27cfacb88ba299",
  ),
  (
    8,
    "0ea004a6eac281a63af5111aee725ed7684405ade915802f3e2ae630082c2162",
  ),
];

/// Whether a recorded checksum still matches this version's migration.
fn checksum_matches(version: i64, recorded: &str, expected: &str) -> bool {
  recorded == expected
    || COMMENT_ONLY_EDITS
      .iter()
      .any(|(v, sum)| *v == version && *sum == recorded)
}

/// Hex SHA-256 of a migration's SQL — what "this version was applied with
/// exactly this content" is pinned to.
fn checksum(sql: &str) -> String {
  let mut hasher = Sha256::new();
  hasher.update(sql.as_bytes());
  format!("{:x}", hasher.finalize())
}

/// Create/open a SQLite database at `path` and bring the schema up to date.
/// Idempotent: applied versions are recorded in `sectr_migrations`; a
/// current database is untouched. Foreign keys + WAL are set
/// per-connection (PRAGMAs live in connect options, not in the SQL files).
pub async fn migrate_sqlite(path: &str) -> Result<SqlitePool> {
  migrate_with(path, SQLITE_MIGRATIONS).await
}

async fn migrate_with(path: &str, migrations: &[(i64, &str)]) -> Result<SqlitePool> {
  let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
    .expect("valid sqlite path")
    .create_if_missing(true)
    .foreign_keys(true)
    .journal_mode(SqliteJournalMode::Wal);
  let pool = SqlitePoolOptions::new()
    .max_connections(5)
    .connect_with(opts)
    .await?;

  sqlx::query(
    "CREATE TABLE IF NOT EXISTS sectr_migrations (
            version    INTEGER PRIMARY KEY,
            checksum   TEXT NOT NULL,
            applied_at TEXT NOT NULL
        )",
  )
  .execute(&pool)
  .await?;

  for (version, sql) in migrations {
    let expected = checksum(sql);
    let recorded: Option<String> =
      sqlx::query_scalar("SELECT checksum FROM sectr_migrations WHERE version = ?")
        .bind(version)
        .fetch_optional(&pool)
        .await?;
    match recorded {
      // Already applied: same content is the normal restart path;
      // different content means the file was edited after being
      // applied — fail loudly instead of silently keeping a stale
      // schema.
      Some(recorded) if checksum_matches(*version, &recorded, &expected) => continue,
      Some(recorded) => {
        return Err(JournalError::Conflict(format!(
          "migration {version} was already applied with a different checksum                      (recorded {recorded}, file now {expected}) — the file changed after                      being applied; this needs a new numbered migration"
        )));
      }
      None => {}
    }

    // One transaction per migration: all statements or none.
    let mut conn = pool.acquire().await?;
    let result = async {
      sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
      sqlx::raw_sql(sql).execute(&mut *conn).await?;
      sqlx::query("INSERT INTO sectr_migrations (version, checksum, applied_at) VALUES (?, ?, ?)")
        .bind(version)
        .bind(&expected)
        .bind(chrono::Utc::now())
        .execute(&mut *conn)
        .await?;
      sqlx::query("COMMIT").execute(&mut *conn).await
    }
    .await;
    if let Err(err) = result {
      let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
      return Err(JournalError::Conflict(format!(
        "migration {version} failed: {err}"
      )));
    }
  }

  Ok(pool)
}

/// Connect to a Postgres database and bring the schema up to date. Same
/// contract as [`migrate_sqlite`]: versioned, checksummed, idempotent —
/// applied versions live in `sectr_migrations`; a changed file under an
/// applied version fails loudly.
pub async fn migrate_postgres(url: &str) -> Result<PgPool> {
  let opts = PgConnectOptions::from_str(url)?;
  let pool = PgPoolOptions::new()
    .max_connections(10)
    .connect_with(opts)
    .await?;
  migrate_postgres_with(&pool, POSTGRES_MIGRATIONS).await?;
  Ok(pool)
}

/// Run migrations over an existing Postgres pool (test helper; production
/// opens via [`migrate_postgres`]).
pub(crate) async fn migrate_postgres_with(pool: &PgPool, migrations: &[(i64, &str)]) -> Result<()> {
  sqlx::query(
    "CREATE TABLE IF NOT EXISTS sectr_migrations (
            version    BIGINT PRIMARY KEY,
            checksum   TEXT NOT NULL,
            applied_at timestamptz NOT NULL DEFAULT now()
        )",
  )
  .execute(pool)
  .await?;

  for (version, sql) in migrations {
    let expected = checksum(sql);
    let recorded: Option<String> =
      sqlx::query_scalar("SELECT checksum FROM sectr_migrations WHERE version = $1")
        .bind(version)
        .fetch_optional(pool)
        .await?;
    match recorded {
      Some(recorded) if checksum_matches(*version, &recorded, &expected) => continue,
      Some(recorded) => {
        return Err(JournalError::Conflict(format!(
          "migration {version} was already applied with a different checksum                      (recorded {recorded}, file now {expected}) — the file changed after                      being applied; this needs a new numbered migration"
        )));
      }
      None => {}
    }

    // One transaction per migration (Postgres DDL is transactional):
    // all statements or none.
    let mut tx = pool.begin().await?;
    sqlx::raw_sql(sql).execute(&mut *tx).await?;
    sqlx::query(
      "INSERT INTO sectr_migrations (version, checksum, applied_at) VALUES ($1, $2, now())",
    )
    .bind(version)
    .bind(&expected)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::{BTreeMap, BTreeSet};

  fn temp_db() -> String {
    std::env::temp_dir()
      .join(format!("sectr-migrations-test-{}.db", uuid::Uuid::now_v7()))
      .to_string_lossy()
      .into_owned()
  }

  /// Extract `CREATE TABLE <name> (...)` blocks and their column names from
  /// a DDL file, plus `ALTER TABLE <name> ADD COLUMN <col>` additions
  /// (later migrations evolve the initial tables — columns compare as
  /// sets, see the parity tests).
  fn parse_tables(sql: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut tables: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut rest = sql;
    while let Some(pos) = rest.find("CREATE TABLE ") {
      let after = &rest[pos + "CREATE TABLE ".len()..];
      let name = after.split([' ', '(']).next().unwrap().trim().to_string();
      let body = after.split_once('(').unwrap().1;
      let body = body.split("\n)").next().unwrap();
      let columns = tables.entry(name).or_default();
      for line in body.lines() {
        let line = line.trim();
        if line.is_empty()
          || line.starts_with("--")
          || line.starts_with("UNIQUE")
          || line.starts_with("CHECK")
          || line.starts_with("PRIMARY")
          || line.starts_with("FOREIGN")
          || line.starts_with("CONSTRAINT")
        {
          continue;
        }
        if let Some(col) = line.split_whitespace().next() {
          columns.insert(col.trim_end_matches(',').to_string());
        }
      }
      rest = &rest[pos + 13..];
    }
    // ALTER TABLE <name> ADD COLUMN <col> ... (evolution migrations).
    let mut rest = sql;
    while let Some(pos) = rest.find("ALTER TABLE ") {
      let after = &rest[pos + "ALTER TABLE ".len()..];
      let name = after.split_whitespace().next().unwrap().to_string();
      // Only THIS statement: an ALTER without ADD COLUMN (DROP CONSTRAINT,
      // RENAME) must not pick up a later migration's ADD COLUMN.
      let statement = after.split(';').next().unwrap();
      if let Some((_prefix, added)) = statement.split_once("ADD COLUMN ") {
        let col = added.split_whitespace().next().unwrap().to_string();
        tables.entry(name).or_default().insert(col);
      }
      rest = &rest[pos + 12..];
    }
    tables
  }

  /// The events.type CHECK list (quoted values) from a DDL file.
  fn parse_event_check(sql: &str) -> Vec<String> {
    let start = sql.split("type IN (").nth(1).expect("events type CHECK");
    let block = &start[..start.find(')').unwrap()];
    let mut values = Vec::new();
    for line in block.lines() {
      let line = line.split("--").next().unwrap().trim();
      if line.starts_with('\'') {
        values.push(line.trim_matches(',').trim_matches('\'').to_string());
      }
    }
    values
  }

  fn all_migration_sql() -> String {
    SQLITE_MIGRATIONS
      .iter()
      .map(|(_, sql)| *sql)
      .collect::<Vec<_>>()
      .join("\n")
  }

  fn all_postgres_migration_sql() -> String {
    POSTGRES_MIGRATIONS
      .iter()
      .map(|(_, sql)| *sql)
      .collect::<Vec<_>>()
      .join("\n")
  }

  #[test]
  fn postgres_migrations_are_in_sync_with_canonical_ddl() {
    // The canonical dialect IS Postgres — the port must stay a verbatim
    // table/column/index mirror.
    let canonical = std::fs::read_to_string(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../../schema/data-model.sql"
    ))
    .unwrap();
    let canonical_tables = parse_tables(&canonical);
    let pg_tables = parse_tables(&all_postgres_migration_sql());

    assert_eq!(
      canonical_tables.keys().collect::<Vec<_>>(),
      pg_tables.keys().collect::<Vec<_>>(),
      "table sets diverge"
    );
    for (name, canonical_cols) in &canonical_tables {
      assert_eq!(
        canonical_cols,
        pg_tables.get(name).unwrap(),
        "columns of {name} diverge (canonical must translate mechanically; \
                 compared as sets — evolution migrations ALTER in columns)"
      );
    }
    assert_eq!(
      parse_event_check(&canonical),
      parse_event_check(&all_postgres_migration_sql())
    );
  }

  #[tokio::test]
  async fn postgres_migrations_actually_run() {
    let Some(store) = crate::test_pg::fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };

    // Every table in the migration set exists for real.
    let names: Vec<(String,)> = sqlx::query_as(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' ORDER BY table_name",
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
    let names: Vec<String> = names.into_iter().map(|(n,)| n).collect();
    for table in parse_tables(&all_postgres_migration_sql()).keys() {
      assert!(
        names.contains(table),
        "table {table} missing after migration"
      );
    }

    // The version was recorded.
    let versions: Vec<(i64,)> =
      sqlx::query_as("SELECT version FROM sectr_migrations ORDER BY version")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert_eq!(
      versions,
      POSTGRES_MIGRATIONS
        .iter()
        .map(|(v, _)| (*v,))
        .collect::<Vec<_>>()
    );
  }

  #[tokio::test]
  async fn postgres_migration_is_idempotent_and_preserves_data() {
    let Some(store) = crate::test_pg::fresh_store().await else {
      eprintln!("skipping: no test Postgres (initdb not on PATH)");
      return;
    };
    sqlx::query("INSERT INTO orgs (id, name, slug) VALUES ($1, $2, $3)")
      .bind(uuid::Uuid::now_v7())
      .bind("dev-org")
      .bind("dev-org")
      .execute(store.pool())
      .await
      .unwrap();

    // Re-open (the server-restart path): no error, data intact, nothing
    // re-applied.
    let store = crate::store::PgStore::open(store.url()).await.unwrap();
    let (orgs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM orgs")
      .fetch_one(store.pool())
      .await
      .unwrap();
    assert_eq!(orgs, 1);
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sectr_migrations")
      .fetch_one(store.pool())
      .await
      .unwrap();
    assert_eq!(count, POSTGRES_MIGRATIONS.len() as i64);
  }

  #[test]
  fn sqlite_migrations_are_in_sync_with_canonical_ddl() {
    let canonical = std::fs::read_to_string(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../../schema/data-model.sql"
    ))
    .unwrap();
    let canonical_tables = parse_tables(&canonical);
    let sqlite_tables = parse_tables(&all_migration_sql());

    assert_eq!(
      canonical_tables.keys().collect::<Vec<_>>(),
      sqlite_tables.keys().collect::<Vec<_>>(),
      "table sets diverge"
    );
    for (name, canonical_cols) in &canonical_tables {
      assert_eq!(
        canonical_cols,
        sqlite_tables.get(name).unwrap(),
        "columns of {name} diverge (canonical must translate mechanically;                  compared as sets — evolution migrations ALTER in columns)"
      );
    }

    // events.type CHECK list must match exactly (it mirrors the
    // eventType enum, which the conformance suite already checks).
    assert_eq!(
      parse_event_check(&canonical),
      parse_event_check(&all_migration_sql())
    );
  }

  #[tokio::test]
  async fn migration_actually_runs_on_sqlite() {
    let path = temp_db();
    let pool = migrate_sqlite(&path).await.unwrap();

    // Every table in the migration set exists for real.
    let names: Vec<(String,)> =
      sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .fetch_all(&pool)
        .await
        .unwrap();
    let names: Vec<String> = names.into_iter().map(|(n,)| n).collect();
    for table in parse_tables(&all_migration_sql()).keys() {
      assert!(
        names.contains(table),
        "table {table} missing after migration"
      );
    }

    // The version was recorded.
    let versions: Vec<(i64,)> =
      sqlx::query_as("SELECT version FROM sectr_migrations ORDER BY version")
        .fetch_all(&pool)
        .await
        .unwrap();
    let versions: Vec<i64> = versions.into_iter().map(|(v,)| v).collect();
    assert_eq!(
      versions,
      SQLITE_MIGRATIONS
        .iter()
        .map(|(v, _)| *v)
        .collect::<Vec<_>>()
    );
  }

  #[test]
  fn a_recorded_checksum_from_before_a_comment_edit_still_matches() {
    let (version, old) = COMMENT_ONLY_EDITS[0];
    assert!(checksum_matches(version, old, "current"));
    // Only for its own version, and nothing else gets through.
    assert!(!checksum_matches(version + 1, old, "current"));
    assert!(!checksum_matches(version, "other", "current"));
    assert!(checksum_matches(version, "current", "current"));
  }

  #[tokio::test]
  async fn editing_an_applied_migration_fails_loudly() {
    let path = temp_db();
    let v1_a = "CREATE TABLE tamper_test (id TEXT PRIMARY KEY);";
    migrate_with(&path, &[(1, v1_a)]).await.unwrap();

    // Same version, different content: the file changed after being
    // applied — must refuse, not silently keep the stale schema.
    let v1_b = "CREATE TABLE tamper_test (id TEXT PRIMARY KEY, extra TEXT);";
    let err = migrate_with(&path, &[(1, v1_b)]).await.unwrap_err();
    assert!(
      matches!(err, JournalError::Conflict(ref m) if m.contains("different checksum")),
      "expected checksum conflict, got {err:?}"
    );
  }

  #[tokio::test]
  async fn new_version_applies_over_untouched_older_one() {
    let path = temp_db();
    let v1 = "CREATE TABLE upgrade_test (id TEXT PRIMARY KEY);";
    migrate_with(&path, &[(1, v1)]).await.unwrap();

    // A 0002 lands: applied on the next open; 0001 is skipped (checksum
    // unchanged).
    let v2 = "CREATE TABLE upgrade_test_2 (id TEXT PRIMARY KEY);";
    let pool = migrate_with(&path, &[(1, v1), (2, v2)]).await.unwrap();

    let versions: Vec<(i64,)> =
      sqlx::query_as("SELECT version FROM sectr_migrations ORDER BY version")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(versions, vec![(1,), (2,)]);
    let (tables,): (i64,) = sqlx::query_as(
      "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'upgrade_test_2'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tables, 1);
  }

  #[tokio::test]
  async fn migration_is_idempotent_and_preserves_data() {
    let path = temp_db();
    let pool = migrate_sqlite(&path).await.unwrap();

    // Write something a dev DB would hold between `sectr dev` runs.
    sqlx::query("INSERT INTO orgs (id, name, slug, created_at) VALUES (?, ?, ?, ?)")
      .bind(uuid::Uuid::now_v7())
      .bind("dev-org")
      .bind("dev-org")
      .bind(chrono::Utc::now())
      .execute(&pool)
      .await
      .unwrap();
    drop(pool);

    // Re-open (the `sectr dev` restart path): no error, data intact.
    let pool = migrate_sqlite(&path).await.unwrap();
    let (orgs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM orgs")
      .fetch_one(&pool)
      .await
      .unwrap();
    assert_eq!(orgs, 1);

    // And the recorded versions are unchanged (nothing re-applied).
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sectr_migrations")
      .fetch_one(&pool)
      .await
      .unwrap();
    assert_eq!(count, SQLITE_MIGRATIONS.len() as i64);
  }
}
