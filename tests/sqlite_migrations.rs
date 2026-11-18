use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use rusqlite::Connection;
use siphon::domain::{
    BUILTIN_VIDEO_PRESET_NAME, CreatorId, Platform, Timestamp, UploadId, UploadStatus,
    builtin_video_preset,
};
use siphon::error::StorageError;
use siphon::storage::migrations::{
    LATEST_SCHEMA_VERSION, MIGRATIONS, Migration, applied_migrations, apply_migrations,
    current_version,
};
use siphon::storage::{FollowCreatorRequest, SqliteStorage, Storage};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "siphon-sqlite-migrations-{}-{}-{id}",
            label,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self { path: dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn table_names(conn: &Connection) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name ASC",
        )
        .unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn index_names(conn: &Connection) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'index' AND name NOT LIKE 'sqlite_%'
             ORDER BY name ASC",
        )
        .unwrap();
    stmt.query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn fresh_database_creates_tables_indexes_pragmas_and_initial_defaults() {
    let temp = TempDir::new("fresh");
    let db_path = temp.path().join("nested").join("dir").join("siphon.db");

    let storage = SqliteStorage::open(&db_path).unwrap();
    assert!(db_path.exists());
    assert_eq!(storage.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    assert_eq!(
        storage.get_default_preset().unwrap(),
        builtin_video_preset()
    );
    assert_eq!(storage.get_default_refresh_cookies().unwrap(), None);
    drop(storage);

    let conn = Connection::open(&db_path).unwrap();
    let expected_tables: BTreeSet<String> = [
        "creators",
        "download_preferences",
        "download_preset_args",
        "download_presets",
        "follows",
        "jobs",
        "operation_reservations",
        "ownership_sequence",
        "refresh_preferences",
        "schema_migrations",
        "uploads",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(table_names(&conn), expected_tables);

    let expected_indexes: BTreeSet<String> = [
        "idx_jobs_kind_id",
        "idx_jobs_state_id",
        "idx_uploads_creator_id",
        "idx_uploads_status_id",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(index_names(&conn), expected_indexes);

    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

    let applied = applied_migrations(&conn).unwrap();
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0].version, 1);
    assert_eq!(applied[0].name, "initial_schema");
    assert_eq!(applied[1].version, 2);
    assert_eq!(applied[1].name, "add_jobs_and_operation_reservations");
}

#[test]
fn rejects_empty_database_path() {
    let err = SqliteStorage::open(Path::new("")).unwrap_err();
    assert!(matches!(err, StorageError::Backend(_)));
    assert!(err.to_string().contains("empty"));
}

#[test]
fn migrations_are_idempotent_across_reopens_and_from_connection() {
    let temp = TempDir::new("idempotent");
    let db_path = temp.path().join("siphon.db");

    let first = SqliteStorage::open(&db_path).unwrap();
    first
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "12345".to_owned(),
            display_name: "Alice".to_owned(),
            followed_at: Timestamp::from_unix_seconds(1_700_000_000),
        })
        .unwrap();
    drop(first);

    let raw_conn = Connection::open(&db_path).unwrap();
    let initial_applied = applied_migrations(&raw_conn).unwrap();
    assert_eq!(initial_applied.len(), 2);
    drop(raw_conn);

    let second = SqliteStorage::open(&db_path).unwrap();
    let followed = second.list_followed_creators().unwrap();
    assert_eq!(followed.len(), 1);
    assert_eq!(followed[0].creator.id, CreatorId::new(1).unwrap());
    drop(second);

    let raw_conn = Connection::open(&db_path).unwrap();
    let wrapped = SqliteStorage::from_connection(raw_conn).unwrap();
    assert_eq!(wrapped.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
    assert_eq!(wrapped.list_followed_creators().unwrap().len(), 1);
}

#[test]
fn incremental_migration_preserves_existing_data() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();

    apply_migrations(&mut conn, &MIGRATIONS[..1]).unwrap();
    assert_eq!(current_version(&conn).unwrap(), 1);

    conn.execute(
        "INSERT INTO creators (platform, platform_creator_id, display_name) VALUES ('bilibili', '99', 'Bob')",
        [],
    )
    .unwrap();

    let v2 = [
        MIGRATIONS[0],
        Migration {
            version: 2,
            name: "add_creator_note_and_audit",
            sql: "ALTER TABLE creators ADD COLUMN note TEXT NOT NULL DEFAULT '';
                  CREATE TABLE migration_audit (id INTEGER PRIMARY KEY, label TEXT NOT NULL);",
        },
    ];
    apply_migrations(&mut conn, &v2).unwrap();
    assert_eq!(current_version(&conn).unwrap(), 2);

    let applied = applied_migrations(&conn).unwrap();
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0].version, 1);
    assert_eq!(applied[1].version, 2);
    assert_eq!(applied[1].name, "add_creator_note_and_audit");

    let (display_name, note): (String, String) = conn
        .query_row(
            "SELECT display_name, note FROM creators WHERE platform_creator_id = '99'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(display_name, "Bob");
    assert_eq!(note, "");
}

#[test]
fn rejects_newer_schema_version_and_corrupted_migration_history() {
    let temp = TempDir::new("reject-bad-migrations");

    // Newer database version than binary supports.
    let newer_db = temp.path().join("newer.db");
    {
        let storage = SqliteStorage::open(&newer_db).unwrap();
        drop(storage);
        let conn = Connection::open(&newer_db).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (99, 'future_schema', 1700000000)",
            [],
        )
        .unwrap();
    }
    let err = SqliteStorage::open(&newer_db).unwrap_err();
    assert!(matches!(err, StorageError::Backend(_)));
    assert!(err.to_string().contains("99"));

    // Name mismatch in schema_migrations.
    let mismatch_db = temp.path().join("mismatch.db");
    {
        let storage = SqliteStorage::open(&mismatch_db).unwrap();
        drop(storage);
        let conn = Connection::open(&mismatch_db).unwrap();
        conn.execute(
            "UPDATE schema_migrations SET name = 'tampered' WHERE version = 1",
            [],
        )
        .unwrap();
    }
    let err = SqliteStorage::open(&mismatch_db).unwrap_err();
    assert!(matches!(err, StorageError::CorruptedData(_)));

    // Unexpected extra version 0 row in schema_migrations (bounds-safety regression).
    let extra_row_db = temp.path().join("extra_row.db");
    {
        let storage = SqliteStorage::open(&extra_row_db).unwrap();
        drop(storage);
        let conn = Connection::open(&extra_row_db).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES (0, 'bogus', 1700000000)",
            [],
        )
        .unwrap();
    }
    let err = SqliteStorage::open(&extra_row_db).unwrap_err();
    assert!(matches!(err, StorageError::CorruptedData(_)));
}

#[test]
fn schema_constraints_and_autoincrement_follow_ids_are_enforced_at_sql_level() {
    let temp = TempDir::new("constraints");
    let db_path = temp.path().join("constraints.db");

    // Initialize via SqliteStorage, then verify follow IDs never reuse across close/reopen.
    let storage = SqliteStorage::open(&db_path).unwrap();
    let first = storage
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "500".to_owned(),
            display_name: "Creator 500".to_owned(),
            followed_at: Timestamp::from_unix_seconds(1_700_000_000),
        })
        .unwrap();
    assert_eq!(first.follow.id.as_i64(), 1);
    storage.unfollow_creator(first.creator.id).unwrap();
    drop(storage);

    let storage = SqliteStorage::open(&db_path).unwrap();
    let second = storage
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "500".to_owned(),
            display_name: "Creator 500".to_owned(),
            followed_at: Timestamp::from_unix_seconds(1_700_000_000),
        })
        .unwrap();
    assert_eq!(second.creator.id, first.creator.id);
    assert!(second.follow.id > first.follow.id);
    drop(storage);

    let conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();

    // Unique (platform, platform_creator_id)
    assert!(
        conn.execute(
            "INSERT INTO creators (platform, platform_creator_id, display_name) VALUES ('bilibili', '500', 'Dup')",
            [],
        )
        .is_err()
    );

    // One active follow per creator
    assert!(
        conn.execute(
            "INSERT INTO follows (creator_id, followed_at, refresh_checkpoint) VALUES (?1, 1, 1)",
            [second.creator.id.as_i64()],
        )
        .is_err()
    );

    // Foreign key on follows.creator_id
    assert!(
        conn.execute(
            "INSERT INTO follows (creator_id, followed_at, refresh_checkpoint) VALUES (9999, 1, 1)",
            [],
        )
        .is_err()
    );

    // ON DELETE RESTRICT protects followed creators from accidental deletion
    assert!(
        conn.execute(
            "DELETE FROM creators WHERE id = ?1",
            [second.creator.id.as_i64()],
        )
        .is_err()
    );

    // Upload status CHECK constraint and unique (platform, platform_upload_id)
    assert!(
        conn.execute(
            "INSERT INTO uploads (creator_id, platform, platform_upload_id, url, title, published_at, status)
             VALUES (?1, 'bilibili', 'BV1', 'https://example.com/1', 'Title', 100, 'invalid_status')",
            [second.creator.id.as_i64()],
        )
        .is_err()
    );
    conn.execute(
        "INSERT INTO uploads (creator_id, platform, platform_upload_id, url, title, published_at, status)
         VALUES (?1, 'bilibili', 'BV1', 'https://example.com/1', 'Title', 100, 'pending')",
        [second.creator.id.as_i64()],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO uploads (creator_id, platform, platform_upload_id, url, title, published_at, status)
             VALUES (?1, 'bilibili', 'BV1', 'https://example.com/2', 'Title 2', 200, 'pending')",
            [second.creator.id.as_i64()],
        )
        .is_err()
    );
    let upload_id = UploadId::new(conn.last_insert_rowid()).unwrap();
    assert_eq!(upload_id.as_i64(), 1);
    assert_eq!(UploadStatus::Pending.as_str(), "pending");

    // Reserve 'video' in download_presets
    assert!(
        conn.execute(
            "INSERT INTO download_presets (name) VALUES (?1)",
            [BUILTIN_VIDEO_PRESET_NAME],
        )
        .is_err()
    );

    // ON DELETE CASCADE for download_preset_args
    conn.execute("INSERT INTO download_presets (name) VALUES ('audio')", [])
        .unwrap();
    conn.execute(
        "INSERT INTO download_preset_args (preset_name, position, arg) VALUES ('audio', 0, '-x')",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM download_presets WHERE name = 'audio'", [])
        .unwrap();
    let remaining_args: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM download_preset_args WHERE preset_name = 'audio'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining_args, 0);

    // Singleton constraints on download_preferences and refresh_preferences
    assert!(
        conn.execute(
            "INSERT INTO download_preferences (id, default_preset_name) VALUES (2, 'video')",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO refresh_preferences (id, cookie_source_kind, cookie_source_value) VALUES (2, NULL, NULL)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE refresh_preferences SET cookie_source_kind = 'file', cookie_source_value = NULL WHERE id = 1",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE refresh_preferences SET cookie_source_kind = 'invalid', cookie_source_value = 'x' WHERE id = 1",
            [],
        )
        .is_err()
    );
}

#[test]
fn concurrent_open_calls_on_new_database_succeed_cleanly() {
    let temp = TempDir::new("concurrent-open");
    let db_path = temp.path().join("concurrent.db");
    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = db_path.clone();
            let start = Arc::clone(&barrier);
            thread::spawn(move || {
                start.wait();
                let storage = SqliteStorage::open(&path).unwrap();
                assert_eq!(storage.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
                assert_eq!(
                    storage.get_default_preset().unwrap(),
                    builtin_video_preset()
                );
                assert_eq!(storage.get_default_refresh_cookies().unwrap(), None);
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(applied_migrations(&conn).unwrap().len(), 2);
}

#[test]
fn migrates_from_released_v1_schema_preserving_all_existing_data() {
    use siphon::domain::{CookieSource, DiscoveredUpload, PresetName};
    use siphon::jobs::{JobState, JobStore, JobTarget, ProcessOwner, StartJobRequest};
    use siphon::storage::CommitRefreshRequest;

    let temp = TempDir::new("migrate-v1-to-v2");
    let db_path = temp.path().join("released_v1.db");

    // Seed an exact v1 released database populated with v1 domain records.
    {
        let mut conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        apply_migrations(&mut conn, &MIGRATIONS[..1]).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 1);

        conn.execute(
            "INSERT INTO creators (platform, platform_creator_id, display_name)
             VALUES ('bilibili', '1001', 'Creator V1')",
            [],
        )
        .unwrap();
        let creator_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO follows (creator_id, followed_at, refresh_checkpoint)
             VALUES (?1, 1700000000, 1700000100)",
            [creator_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO uploads (creator_id, platform, platform_upload_id, url, title, published_at, status)
             VALUES (?1, 'bilibili', 'BV1v1', 'https://www.bilibili.com/video/BV1v1', 'V1 Upload', 1700000050, 'pending')",
            [creator_id],
        )
        .unwrap();
        conn.execute("INSERT INTO download_presets (name) VALUES ('audio')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO download_preset_args (preset_name, position, arg) VALUES ('audio', 0, '-x')",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE download_preferences SET default_preset_name = 'audio' WHERE id = 1",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE refresh_preferences
             SET cookie_source_kind = 'browser_profile', cookie_source_value = 'firefox'
             WHERE id = 1",
            [],
        )
        .unwrap();
    }

    // Reopen through SqliteStorage, which must migrate v1 -> v2 automatically.
    let storage = SqliteStorage::open(&db_path).unwrap();
    assert_eq!(storage.schema_version().unwrap(), LATEST_SCHEMA_VERSION);

    let followed = storage.list_followed_creators().unwrap();
    assert_eq!(followed.len(), 1);
    assert_eq!(followed[0].creator.display_name, "Creator V1");
    assert_eq!(
        followed[0].follow.refresh_checkpoint,
        Timestamp::from_unix_seconds(1_700_000_100)
    );

    let pending = storage.list_pending_uploads().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].platform_upload_id, "BV1v1");

    let default_preset = storage.get_default_preset().unwrap();
    assert_eq!(default_preset.name, PresetName::new("audio").unwrap());
    assert_eq!(default_preset.args, vec!["-x"]);
    assert_eq!(
        storage.get_default_refresh_cookies().unwrap(),
        Some(CookieSource::BrowserProfile("firefox".to_owned()))
    );

    // Existing v1 operations still work post-migration.
    let commit = storage
        .commit_refresh(CommitRefreshRequest {
            creator_id: followed[0].creator.id,
            expected_follow_id: followed[0].follow.id,
            expected_follow_time: followed[0].follow.followed_at,
            expected_checkpoint: followed[0].follow.refresh_checkpoint,
            new_checkpoint: Timestamp::from_unix_seconds(1_700_000_200),
            discovered_uploads: vec![DiscoveredUpload {
                platform: Platform::Bilibili,
                platform_upload_id: "BV1v2".to_owned(),
                url: "https://www.bilibili.com/video/BV1v2".to_owned(),
                title: "V2 Upload".to_owned(),
                published_at: Timestamp::from_unix_seconds(1_700_000_150),
            }],
        })
        .unwrap();
    assert_eq!(commit.new_uploads_inserted, 1);

    // New v2 JobStore functionality works on the migrated database.
    let started = storage
        .start_background_job(StartJobRequest {
            target: JobTarget::Refresh { creator_id: None },
            owner: ProcessOwner::current_pid(),
            started_at: Timestamp::from_unix_seconds(1_700_000_300),
            initial_progress: None,
        })
        .unwrap();
    assert_eq!(started.job.state, JobState::Running);
    assert_eq!(storage.get_job(started.job.id).unwrap(), started.job);
}

#[test]
fn v2_job_and_reservation_sql_constraints_are_enforced() {
    let temp = TempDir::new("v2-constraints");
    let db_path = temp.path().join("v2_constraints.db");
    let storage = SqliteStorage::open(&db_path).unwrap();
    drop(storage);

    let conn = Connection::open(&db_path).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();

    // ownership_sequence singleton and positive token checks
    assert!(
        conn.execute(
            "INSERT INTO ownership_sequence (id, next_token) VALUES (2, 1)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "UPDATE ownership_sequence SET next_token = 0 WHERE id = 1",
            []
        )
        .is_err()
    );

    // jobs target/state/owner CHECK constraints
    assert!(
        conn.execute(
            "INSERT INTO jobs (kind, state, started_at, updated_at, owner_token, owner_pid)
             VALUES ('unknown', 'running', 1, 1, 1, 100)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO jobs (kind, target_url, state, started_at, updated_at, owner_token, owner_pid)
             VALUES ('refresh', 'https://example.com', 'running', 1, 1, 1, 100)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO jobs (kind, state, started_at, updated_at, owner_token, owner_pid)
             VALUES ('upload_download', 'running', 1, 1, 1, 100)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO jobs (kind, state, started_at, updated_at, owner_token, owner_pid)
             VALUES ('direct_download', 'running', 1, 1, 1, 100)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO jobs (kind, state, started_at, updated_at, finished_at, owner_token, owner_pid)
             VALUES ('refresh', 'running', 1, 1, 2, 1, 100)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO jobs (kind, state, started_at, updated_at, finished_at, outcome_json, owner_token, owner_pid)
             VALUES ('refresh', 'succeeded', 1, 1, NULL, '{}', 1, 100)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO jobs (kind, state, started_at, updated_at, owner_token, owner_pid)
             VALUES ('refresh', 'running', 1, 1, 1, 0)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO jobs (kind, state, started_at, updated_at, owner_token, owner_pid, owner_process_started_at)
             VALUES ('refresh', 'running', 1, 1, 1, 100, '   ')",
            [],
        )
        .is_err()
    );

    // Valid job insert + AUTOINCREMENT never reuses IDs after deletion
    conn.execute(
        "INSERT INTO jobs (kind, state, started_at, updated_at, owner_token, owner_pid)
         VALUES ('refresh', 'running', 1, 1, 1, 100)",
        [],
    )
    .unwrap();
    let first_job_id = conn.last_insert_rowid();
    assert_eq!(first_job_id, 1);

    // operation_reservations constraints
    assert!(
        conn.execute(
            "INSERT INTO operation_reservations
             (slot_key, operation_kind, upload_id, execution_mode, job_id, owner_token, owner_pid, acquired_at)
             VALUES ('refresh', 'refresh', NULL, 'foreground', ?1, 1, 100, 1)",
            [first_job_id],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO operation_reservations
             (slot_key, operation_kind, upload_id, execution_mode, job_id, owner_token, owner_pid, acquired_at)
             VALUES ('refresh', 'refresh', NULL, 'background', NULL, 1, 100, 1)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO operation_reservations
             (slot_key, operation_kind, upload_id, execution_mode, job_id, owner_token, owner_pid, acquired_at)
             VALUES ('wrong_key', 'upload_download', 7, 'foreground', NULL, 1, 100, 1)",
            [],
        )
        .is_err()
    );
    conn.execute(
        "INSERT INTO operation_reservations
         (slot_key, operation_kind, upload_id, execution_mode, job_id, owner_token, owner_pid, acquired_at)
         VALUES ('upload_download:7', 'upload_download', 7, 'background', ?1, 1, 100, 1)",
        [first_job_id],
    )
    .unwrap();

    // ON DELETE RESTRICT protects jobs referenced by operation_reservations
    assert!(
        conn.execute("DELETE FROM jobs WHERE id = ?1", [first_job_id])
            .is_err()
    );
    conn.execute(
        "DELETE FROM operation_reservations WHERE slot_key = 'upload_download:7'",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM jobs WHERE id = ?1", [first_job_id])
        .unwrap();

    conn.execute(
        "INSERT INTO jobs (kind, state, started_at, updated_at, owner_token, owner_pid)
         VALUES ('refresh', 'running', 2, 2, 2, 101)",
        [],
    )
    .unwrap();
    assert!(conn.last_insert_rowid() > first_job_id);
}
