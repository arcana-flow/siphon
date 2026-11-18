//! Versioned SQLite schema migrations for Siphon.

use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};

use crate::domain::Timestamp;
use crate::error::StorageError;

pub(crate) const BUSY_RETRY_WINDOW: Duration = Duration::from_secs(5);

/// Definition of a single forward-only SQLite schema migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    /// Positive sequential schema version starting at `1`.
    pub version: i64,
    /// Stable human-readable identifier for this migration.
    pub name: &'static str,
    /// SQL statements executed atomically within the migration transaction.
    pub sql: &'static str,
}

/// Metadata recorded in `schema_migrations` for an applied migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMigration {
    /// Applied migration version.
    pub version: i64,
    /// Migration name recorded when applied.
    pub name: String,
    /// UTC timestamp when the migration was applied.
    pub applied_at: Timestamp,
}

const MIGRATION_001_INITIAL_SCHEMA: &str = r#"
CREATE TABLE creators (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    platform TEXT NOT NULL CHECK (length(platform) > 0),
    platform_creator_id TEXT NOT NULL CHECK (length(platform_creator_id) > 0),
    display_name TEXT NOT NULL,
    UNIQUE (platform, platform_creator_id)
);

CREATE TABLE follows (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    creator_id INTEGER NOT NULL UNIQUE REFERENCES creators(id) ON DELETE RESTRICT,
    followed_at INTEGER NOT NULL,
    refresh_checkpoint INTEGER NOT NULL
);

CREATE TABLE uploads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    creator_id INTEGER NOT NULL REFERENCES creators(id) ON DELETE RESTRICT,
    platform TEXT NOT NULL CHECK (length(platform) > 0),
    platform_upload_id TEXT NOT NULL CHECK (length(platform_upload_id) > 0),
    url TEXT NOT NULL,
    title TEXT NOT NULL,
    published_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'downloaded', 'ignored')),
    UNIQUE (platform, platform_upload_id)
);

CREATE INDEX idx_uploads_status_id ON uploads (status, id);
CREATE INDEX idx_uploads_creator_id ON uploads (creator_id);

CREATE TABLE download_presets (
    name TEXT PRIMARY KEY CHECK (length(name) > 0 AND name <> 'video')
);

CREATE TABLE download_preset_args (
    preset_name TEXT NOT NULL REFERENCES download_presets(name) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    arg TEXT NOT NULL,
    PRIMARY KEY (preset_name, position)
);

CREATE TABLE download_preferences (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    default_preset_name TEXT NOT NULL CHECK (length(default_preset_name) > 0)
);

INSERT INTO download_preferences (id, default_preset_name)
VALUES (1, 'video');

CREATE TABLE refresh_preferences (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    cookie_source_kind TEXT CHECK (cookie_source_kind IN ('file', 'browser_profile')),
    cookie_source_value TEXT,
    CHECK (
        (cookie_source_kind IS NULL AND cookie_source_value IS NULL)
        OR (
            cookie_source_kind IS NOT NULL
            AND cookie_source_value IS NOT NULL
            AND length(cookie_source_value) > 0
        )
    )
);

INSERT INTO refresh_preferences (id, cookie_source_kind, cookie_source_value)
VALUES (1, NULL, NULL);
"#;

/// Ordered list of built-in Siphon SQLite migrations.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial_schema",
    sql: MIGRATION_001_INITIAL_SCHEMA,
}];

/// Highest schema version supported by this build of Siphon.
pub const LATEST_SCHEMA_VERSION: i64 = 1;

/// Applies all built-in Siphon migrations to `conn` if not already applied.
pub fn apply(conn: &mut Connection) -> Result<(), StorageError> {
    apply_migrations(conn, MIGRATIONS)
}

/// Applies an explicit migration sequence to `conn` inside an immediate transaction.
///
/// Validates that already-applied versions match `migrations` sequentially without gaps and
/// rejects databases created by newer schema versions.
pub fn apply_migrations(
    conn: &mut Connection,
    migrations: &[Migration],
) -> Result<(), StorageError> {
    validate_migration_definitions(migrations)?;

    retry_storage_on_busy("failed to apply SQLite schema migrations", || {
        apply_migrations_once(conn, migrations)
    })
}

fn apply_migrations_once(
    conn: &mut Connection,
    migrations: &[Migration],
) -> Result<(), MigrationAttemptError> {
    if schema_migrations_exists(conn).map_err(MigrationAttemptError::Sqlite)? {
        let applied = read_applied_migrations(conn).map_err(MigrationAttemptError::Sqlite)?;
        let pending_start =
            validate_applied(&applied, migrations).map_err(MigrationAttemptError::Storage)?;
        if pending_start == migrations.len() {
            return Ok(());
        }
    }

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(MigrationAttemptError::Sqlite)?;

    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        );",
    )
    .map_err(MigrationAttemptError::Sqlite)?;

    let applied = read_applied_migrations(&tx).map_err(MigrationAttemptError::Sqlite)?;
    let pending_start =
        validate_applied(&applied, migrations).map_err(MigrationAttemptError::Storage)?;

    let applied_at = Timestamp::now().as_unix_seconds();
    for migration in &migrations[pending_start..] {
        tx.execute_batch(migration.sql).map_err(|err| {
            if is_sqlite_busy(&err) {
                MigrationAttemptError::Sqlite(err)
            } else {
                MigrationAttemptError::Storage(StorageError::Backend(format!(
                    "failed to apply migration v{} ('{}'): {err}",
                    migration.version, migration.name
                )))
            }
        })?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            params![migration.version, migration.name, applied_at],
        )
        .map_err(MigrationAttemptError::Sqlite)?;
    }

    tx.commit().map_err(MigrationAttemptError::Sqlite)
}

/// Returns all migrations currently recorded in `schema_migrations`, ordered by version.
pub fn applied_migrations(conn: &Connection) -> Result<Vec<AppliedMigration>, StorageError> {
    if !schema_migrations_exists(conn).map_err(map_err)? {
        return Ok(Vec::new());
    }
    read_applied_migrations(conn).map_err(map_err)
}

/// Returns the highest applied migration version, or `0` if uninitialized.
pub fn current_version(conn: &Connection) -> Result<i64, StorageError> {
    Ok(applied_migrations(conn)?
        .last()
        .map_or(0, |entry| entry.version))
}

fn validate_migration_definitions(migrations: &[Migration]) -> Result<(), StorageError> {
    if migrations.is_empty() {
        return Err(StorageError::Backend(
            "migration sequence cannot be empty".to_owned(),
        ));
    }
    for (idx, migration) in migrations.iter().enumerate() {
        let expected_version = i64::try_from(idx + 1)
            .map_err(|_| StorageError::Backend("migration version overflowed i64".to_owned()))?;
        if migration.version != expected_version {
            return Err(StorageError::Backend(format!(
                "invalid migration definition order: expected version {expected_version}, found {}",
                migration.version
            )));
        }
        if migration.name.trim().is_empty() {
            return Err(StorageError::Backend(format!(
                "migration v{} must have a non-empty name",
                migration.version
            )));
        }
    }
    Ok(())
}

fn schema_migrations_exists(conn: &Connection) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
        [],
        |_| Ok(true),
    )
    .optional()
    .map(|opt| opt.unwrap_or(false))
}

fn read_applied_migrations(conn: &Connection) -> Result<Vec<AppliedMigration>, rusqlite::Error> {
    let mut stmt = conn
        .prepare("SELECT version, name, applied_at FROM schema_migrations ORDER BY version ASC")?;
    let rows = stmt.query_map([], |row| {
        let version: i64 = row.get(0)?;
        let name: String = row.get(1)?;
        let applied_at: i64 = row.get(2)?;
        Ok(AppliedMigration {
            version,
            name,
            applied_at: Timestamp::from_unix_seconds(applied_at),
        })
    })?;

    let mut applied = Vec::new();
    for row in rows {
        applied.push(row?);
    }
    Ok(applied)
}

enum MigrationAttemptError {
    Sqlite(rusqlite::Error),
    Storage(StorageError),
}

fn is_sqlite_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(ffi_err, _)
            if matches!(
                ffi_err.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            )
    )
}

pub(crate) fn retry_sqlite_on_busy<T, F>(context: &str, mut op: F) -> Result<T, StorageError>
where
    F: FnMut() -> Result<T, rusqlite::Error>,
{
    retry_storage_on_busy(context, || op().map_err(MigrationAttemptError::Sqlite))
}

fn retry_storage_on_busy<T, F>(context: &str, mut op: F) -> Result<T, StorageError>
where
    F: FnMut() -> Result<T, MigrationAttemptError>,
{
    let deadline = Instant::now() + BUSY_RETRY_WINDOW;
    let mut backoff = Duration::from_millis(5);
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(MigrationAttemptError::Sqlite(err))
                if is_sqlite_busy(&err) && Instant::now() < deadline =>
            {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(50));
            }
            Err(MigrationAttemptError::Sqlite(err)) => {
                return Err(StorageError::Backend(format!("{context}: {err}")));
            }
            Err(MigrationAttemptError::Storage(err)) => return Err(err),
        }
    }
}

fn validate_applied(
    applied: &[AppliedMigration],
    migrations: &[Migration],
) -> Result<usize, StorageError> {
    let latest_version = migrations.last().map_or(0, |migration| migration.version);

    if let Some(last) = applied.last()
        && last.version > latest_version
    {
        return Err(StorageError::Backend(format!(
            "unsupported database schema version {} (latest supported version is {latest_version})",
            last.version
        )));
    }

    for (idx, record) in applied.iter().enumerate() {
        let expected = migrations.get(idx).ok_or_else(|| {
            StorageError::CorruptedData(format!(
                "unexpected migration entry v{} ('{}') in schema_migrations",
                record.version, record.name
            ))
        })?;
        if record.version != expected.version {
            return Err(StorageError::CorruptedData(format!(
                "expected migration version {} in schema_migrations, found {}",
                expected.version, record.version
            )));
        }
        if record.name != expected.name {
            return Err(StorageError::CorruptedData(format!(
                "migration v{} name mismatch: expected '{}', found '{}'",
                expected.version, expected.name, record.name
            )));
        }
    }

    Ok(applied.len())
}

fn map_err(err: rusqlite::Error) -> StorageError {
    StorageError::Backend(err.to_string())
}
