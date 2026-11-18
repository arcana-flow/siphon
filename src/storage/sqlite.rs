//! SQLite implementation of Siphon's [`Storage`] contract.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::domain::{
    BUILTIN_VIDEO_PRESET_NAME, CookieSource, Creator, CreatorId, DownloadPreset, Follow, FollowId,
    FollowedCreator, Platform, PresetName, PresetSummary, Timestamp, Upload, UploadId,
    UploadStatus, builtin_video_preset,
};
use crate::error::StorageError;
use crate::storage::migrations;
use crate::storage::{
    CommitRefreshOutcome, CommitRefreshRequest, FollowCreatorRequest, RemovePresetOutcome, Storage,
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Local SQLite-backed implementation of [`Storage`].
///
/// Wraps a [`Connection`] in a [`Mutex`] for `Send + Sync` access, enables foreign keys and WAL
/// journaling (for file-backed databases), runs schema migrations on open, and serializes writes
/// using `BEGIN IMMEDIATE` transactions.
#[derive(Debug)]
pub struct SqliteStorage {
    conn: Mutex<Connection>,
}

impl SqliteStorage {
    /// Opens (or creates) a SQLite database at `path`, creating missing parent directories and
    /// applying any pending schema migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() {
            return Err(StorageError::Backend(
                "database path cannot be empty".to_owned(),
            ));
        }

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| {
                StorageError::Backend(format!(
                    "failed to create database directory '{}': {err}",
                    parent.display()
                ))
            })?;
        }

        let mut conn = Connection::open(path).map_err(|err| {
            StorageError::Backend(format!(
                "failed to open SQLite database '{}': {err}",
                path.display()
            ))
        })?;
        configure_connection(&conn, true)?;
        migrations::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Opens a fresh in-memory SQLite database with all Siphon schema migrations applied.
    pub fn open_in_memory() -> Result<Self, StorageError> {
        let mut conn = Connection::open_in_memory().map_err(map_sqlite_err)?;
        configure_connection(&conn, false)?;
        migrations::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Wraps an existing SQLite [`Connection`], configuring pragmas and applying pending migrations.
    pub fn from_connection(mut conn: Connection) -> Result<Self, StorageError> {
        configure_connection(&conn, false)?;
        migrations::apply(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Returns the highest schema migration version applied to this database.
    pub fn schema_version(&self) -> Result<i64, StorageError> {
        let conn = self.lock_conn()?;
        migrations::current_version(&conn)
    }

    fn lock_conn(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.conn
            .lock()
            .map_err(|_| StorageError::Backend("SQLite storage mutex poisoned".to_owned()))
    }
}

impl Storage for SqliteStorage {
    fn follow_creator(
        &self,
        request: FollowCreatorRequest,
    ) -> Result<FollowedCreator, StorageError> {
        if request.platform_creator_id.is_empty() {
            return Err(StorageError::Backend(
                "platform creator ID cannot be empty".to_owned(),
            ));
        }

        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let existing = tx
            .query_row(
                "SELECT id, display_name FROM creators WHERE platform = ?1 AND platform_creator_id = ?2",
                params![request.platform.as_str(), request.platform_creator_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(map_sqlite_err)?;

        let creator = match existing {
            Some((raw_id, existing_display_name)) => {
                let creator_id = creator_id_from_i64(raw_id)?;
                let already_followed = tx
                    .query_row(
                        "SELECT 1 FROM follows WHERE creator_id = ?1",
                        [creator_id.as_i64()],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(map_sqlite_err)?
                    .is_some();

                if already_followed {
                    return Err(StorageError::CreatorAlreadyFollowed {
                        creator_id,
                        display_name: existing_display_name,
                    });
                }

                tx.execute(
                    "UPDATE creators SET display_name = ?1 WHERE id = ?2",
                    params![request.display_name, creator_id.as_i64()],
                )
                .map_err(map_sqlite_err)?;

                Creator {
                    id: creator_id,
                    platform: request.platform,
                    platform_creator_id: request.platform_creator_id,
                    display_name: request.display_name,
                }
            }
            None => {
                tx.execute(
                    "INSERT INTO creators (platform, platform_creator_id, display_name) VALUES (?1, ?2, ?3)",
                    params![
                        request.platform.as_str(),
                        request.platform_creator_id,
                        request.display_name
                    ],
                )
                .map_err(map_sqlite_err)?;
                let creator_id = creator_id_from_i64(tx.last_insert_rowid())?;
                Creator {
                    id: creator_id,
                    platform: request.platform,
                    platform_creator_id: request.platform_creator_id,
                    display_name: request.display_name,
                }
            }
        };

        let timestamp = request.followed_at.as_unix_seconds();
        tx.execute(
            "INSERT INTO follows (creator_id, followed_at, refresh_checkpoint) VALUES (?1, ?2, ?3)",
            params![creator.id.as_i64(), timestamp, timestamp],
        )
        .map_err(map_sqlite_err)?;
        let follow_id = follow_id_from_i64(tx.last_insert_rowid())?;

        tx.commit().map_err(map_sqlite_err)?;

        Ok(FollowedCreator {
            creator: creator.clone(),
            follow: Follow {
                id: follow_id,
                creator_id: creator.id,
                followed_at: request.followed_at,
                refresh_checkpoint: request.followed_at,
            },
        })
    }

    fn list_followed_creators(&self) -> Result<Vec<FollowedCreator>, StorageError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT c.id, c.platform, c.platform_creator_id, c.display_name,
                        f.id, f.followed_at, f.refresh_checkpoint
                 FROM follows f
                 INNER JOIN creators c ON c.id = f.creator_id
                 ORDER BY c.id ASC",
            )
            .map_err(map_sqlite_err)?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(map_sqlite_err)?;

        let mut followed = Vec::new();
        for row in rows {
            let (cid, platform, pcid, display_name, fid, followed_at, checkpoint) =
                row.map_err(map_sqlite_err)?;
            let creator = creator_from_parts(cid, platform, pcid, display_name)?;
            let follow = follow_from_parts(fid, creator.id, followed_at, checkpoint)?;
            followed.push(FollowedCreator { creator, follow });
        }
        Ok(followed)
    }

    fn get_followed_creator(&self, creator_id: CreatorId) -> Result<FollowedCreator, StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        let creator = find_creator(&tx, creator_id)?;
        let follow =
            find_follow(&tx, creator_id)?.ok_or(StorageError::CreatorNotFollowed { creator_id })?;
        Ok(FollowedCreator { creator, follow })
    }

    fn unfollow_creator(&self, creator_id: CreatorId) -> Result<Creator, StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let creator = find_creator(&tx, creator_id)?;
        let deleted = tx
            .execute(
                "DELETE FROM follows WHERE creator_id = ?1",
                [creator_id.as_i64()],
            )
            .map_err(map_sqlite_err)?;

        if deleted == 0 {
            return Err(StorageError::CreatorNotFollowed { creator_id });
        }

        tx.commit().map_err(map_sqlite_err)?;
        Ok(creator)
    }

    fn commit_refresh(
        &self,
        request: CommitRefreshRequest,
    ) -> Result<CommitRefreshOutcome, StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let creator = find_creator(&tx, request.creator_id)?;
        let follow =
            find_follow(&tx, request.creator_id)?.ok_or(StorageError::StaleRefreshCheckpoint {
                creator_id: request.creator_id,
            })?;

        if follow.id != request.expected_follow_id
            || follow.followed_at != request.expected_follow_time
            || follow.refresh_checkpoint != request.expected_checkpoint
        {
            return Err(StorageError::StaleRefreshCheckpoint {
                creator_id: request.creator_id,
            });
        }

        let mut new_uploads_inserted = 0usize;
        let mut existing_uploads_seen = 0usize;

        {
            let mut find_upload_stmt = tx
                .prepare_cached(
                    "SELECT id FROM uploads WHERE platform = ?1 AND platform_upload_id = ?2",
                )
                .map_err(map_sqlite_err)?;
            let mut update_upload_stmt = tx
                .prepare_cached("UPDATE uploads SET url = ?1, title = ?2 WHERE id = ?3")
                .map_err(map_sqlite_err)?;
            let mut insert_upload_stmt = tx
                .prepare_cached(
                    "INSERT INTO uploads (creator_id, platform, platform_upload_id, url, title, published_at, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
                )
                .map_err(map_sqlite_err)?;

            for item in request.discovered_uploads {
                let existing_id = find_upload_stmt
                    .query_row(
                        params![item.platform.as_str(), item.platform_upload_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(map_sqlite_err)?;

                if let Some(upload_id) = existing_id {
                    existing_uploads_seen += 1;
                    update_upload_stmt
                        .execute(params![item.url, item.title, upload_id])
                        .map_err(map_sqlite_err)?;
                } else {
                    new_uploads_inserted += 1;
                    insert_upload_stmt
                        .execute(params![
                            request.creator_id.as_i64(),
                            item.platform.as_str(),
                            item.platform_upload_id,
                            item.url,
                            item.title,
                            item.published_at.as_unix_seconds()
                        ])
                        .map_err(map_sqlite_err)?;
                }
            }
        }

        let updated = tx
            .execute(
                "UPDATE follows
                 SET refresh_checkpoint = ?1
                 WHERE id = ?2
                   AND creator_id = ?3
                   AND followed_at = ?4
                   AND refresh_checkpoint = ?5",
                params![
                    request.new_checkpoint.as_unix_seconds(),
                    request.expected_follow_id.as_i64(),
                    request.creator_id.as_i64(),
                    request.expected_follow_time.as_unix_seconds(),
                    request.expected_checkpoint.as_unix_seconds(),
                ],
            )
            .map_err(map_sqlite_err)?;

        if updated != 1 {
            return Err(StorageError::StaleRefreshCheckpoint {
                creator_id: request.creator_id,
            });
        }

        tx.commit().map_err(map_sqlite_err)?;

        Ok(CommitRefreshOutcome {
            creator_id: request.creator_id,
            display_name: creator.display_name,
            new_uploads_inserted,
            existing_uploads_seen,
            previous_checkpoint: request.expected_checkpoint,
            new_checkpoint: request.new_checkpoint,
        })
    }

    fn list_pending_uploads(&self) -> Result<Vec<Upload>, StorageError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, creator_id, platform, platform_upload_id, url, title, published_at, status
                 FROM uploads
                 WHERE status = 'pending'
                 ORDER BY id ASC",
            )
            .map_err(map_sqlite_err)?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .map_err(map_sqlite_err)?;

        let mut uploads = Vec::new();
        for row in rows {
            let (id, cid, platform, puid, url, title, published_at, status) =
                row.map_err(map_sqlite_err)?;
            uploads.push(upload_from_parts(
                id,
                cid,
                platform,
                puid,
                url,
                title,
                published_at,
                status,
            )?);
        }
        Ok(uploads)
    }

    fn get_upload(&self, upload_id: UploadId) -> Result<Upload, StorageError> {
        let conn = self.lock_conn()?;
        fetch_upload(&conn, upload_id)
    }

    fn mark_upload_ignored(&self, upload_id: UploadId) -> Result<Upload, StorageError> {
        self.transition_upload_status(upload_id, UploadStatus::Ignored)
    }

    fn mark_upload_downloaded(&self, upload_id: UploadId) -> Result<Upload, StorageError> {
        self.transition_upload_status(upload_id, UploadStatus::Downloaded)
    }

    fn add_preset(
        &self,
        name: PresetName,
        args: Vec<String>,
    ) -> Result<DownloadPreset, StorageError> {
        if name.is_builtin_video() {
            return Err(StorageError::CannotModifyBuiltinPreset { name });
        }

        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let exists = tx
            .query_row(
                "SELECT 1 FROM download_presets WHERE name = ?1",
                [name.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_err)?
            .is_some();

        if exists {
            return Err(StorageError::PresetAlreadyExists { name });
        }

        tx.execute(
            "INSERT INTO download_presets (name) VALUES (?1)",
            [name.as_str()],
        )
        .map_err(map_sqlite_err)?;

        {
            let mut arg_stmt = tx
                .prepare_cached(
                    "INSERT INTO download_preset_args (preset_name, position, arg) VALUES (?1, ?2, ?3)",
                )
                .map_err(map_sqlite_err)?;
            for (idx, arg) in args.iter().enumerate() {
                let position = i64::try_from(idx).map_err(|_| {
                    StorageError::Backend("preset argument count overflowed i64".to_owned())
                })?;
                arg_stmt
                    .execute(params![name.as_str(), position, arg])
                    .map_err(map_sqlite_err)?;
            }
        }

        tx.commit().map_err(map_sqlite_err)?;

        Ok(DownloadPreset {
            name,
            args,
            is_builtin: false,
        })
    }

    fn list_presets(&self) -> Result<Vec<PresetSummary>, StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;

        let default_name = read_default_preset_name(&tx)?;
        let custom_names = load_custom_preset_names(&tx)?;
        let mut args_by_name = load_all_custom_preset_args(&tx)?;

        if default_name != BUILTIN_VIDEO_PRESET_NAME
            && !custom_names
                .iter()
                .any(|name| name.as_str() == default_name)
        {
            return Err(StorageError::CorruptedData(format!(
                "default preset '{default_name}' does not exist in download_presets"
            )));
        }

        let builtin = builtin_video_preset();
        let mut summaries = Vec::with_capacity(custom_names.len() + 1);
        summaries.push(PresetSummary {
            is_default: default_name == builtin.name.as_str(),
            preset: builtin,
        });

        for name in custom_names {
            let args = args_by_name.remove(name.as_str()).unwrap_or_default();
            let is_default = default_name == name.as_str();
            summaries.push(PresetSummary {
                preset: DownloadPreset {
                    name,
                    args,
                    is_builtin: false,
                },
                is_default,
            });
        }

        Ok(summaries)
    }

    fn get_preset(&self, name: &PresetName) -> Result<DownloadPreset, StorageError> {
        if name.is_builtin_video() {
            return Ok(builtin_video_preset());
        }

        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        resolve_custom_preset(&tx, name)?
            .ok_or_else(|| StorageError::PresetNotFound { name: name.clone() })
    }

    fn remove_preset(&self, name: &PresetName) -> Result<RemovePresetOutcome, StorageError> {
        if name.is_builtin_video() {
            return Err(StorageError::CannotModifyBuiltinPreset { name: name.clone() });
        }

        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let current_default = read_default_preset_name(&tx)?;
        let deleted = tx
            .execute(
                "DELETE FROM download_presets WHERE name = ?1",
                [name.as_str()],
            )
            .map_err(map_sqlite_err)?;

        if deleted == 0 {
            return Err(StorageError::PresetNotFound { name: name.clone() });
        }

        let reset_default_to_builtin = current_default == name.as_str();
        if reset_default_to_builtin {
            let updated = tx
                .execute(
                    "UPDATE download_preferences SET default_preset_name = ?1 WHERE id = 1",
                    [BUILTIN_VIDEO_PRESET_NAME],
                )
                .map_err(map_sqlite_err)?;
            if updated != 1 {
                return Err(StorageError::CorruptedData(
                    "download_preferences singleton row is missing".to_owned(),
                ));
            }
        }

        tx.commit().map_err(map_sqlite_err)?;

        Ok(RemovePresetOutcome {
            removed: name.clone(),
            reset_default_to_builtin,
        })
    }

    fn set_default_preset(&self, name: &PresetName) -> Result<DownloadPreset, StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let preset = if name.is_builtin_video() {
            builtin_video_preset()
        } else {
            resolve_custom_preset(&tx, name)?
                .ok_or_else(|| StorageError::PresetNotFound { name: name.clone() })?
        };

        let updated = tx
            .execute(
                "UPDATE download_preferences SET default_preset_name = ?1 WHERE id = 1",
                [preset.name.as_str()],
            )
            .map_err(map_sqlite_err)?;
        if updated != 1 {
            return Err(StorageError::CorruptedData(
                "download_preferences singleton row is missing".to_owned(),
            ));
        }

        tx.commit().map_err(map_sqlite_err)?;
        Ok(preset)
    }

    fn get_default_preset(&self) -> Result<DownloadPreset, StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;

        let default_name = read_default_preset_name(&tx)?;
        if default_name == BUILTIN_VIDEO_PRESET_NAME {
            return Ok(builtin_video_preset());
        }

        let parsed_name = PresetName::new(default_name.clone()).map_err(|err| {
            StorageError::CorruptedData(format!(
                "invalid default preset name '{default_name}' in download_preferences: {err}"
            ))
        })?;

        resolve_custom_preset(&tx, &parsed_name)?.ok_or_else(|| {
            StorageError::CorruptedData(format!(
                "default preset '{parsed_name}' in download_preferences does not exist"
            ))
        })
    }

    fn get_default_refresh_cookies(&self) -> Result<Option<CookieSource>, StorageError> {
        let conn = self.lock_conn()?;
        let row = conn
            .query_row(
                "SELECT cookie_source_kind, cookie_source_value FROM refresh_preferences WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_err)?
            .ok_or_else(|| {
                StorageError::CorruptedData(
                    "refresh_preferences singleton row is missing".to_owned(),
                )
            })?;

        decode_cookie_source(row.0, row.1)
    }

    fn set_default_refresh_cookies(
        &self,
        source: Option<&CookieSource>,
    ) -> Result<(), StorageError> {
        let (kind, value) = encode_cookie_source(source)?;

        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let updated = tx
            .execute(
                "UPDATE refresh_preferences
                 SET cookie_source_kind = ?1, cookie_source_value = ?2
                 WHERE id = 1",
                params![kind, value],
            )
            .map_err(map_sqlite_err)?;
        if updated != 1 {
            return Err(StorageError::CorruptedData(
                "refresh_preferences singleton row is missing".to_owned(),
            ));
        }

        tx.commit().map_err(map_sqlite_err)
    }
}

impl SqliteStorage {
    fn transition_upload_status(
        &self,
        upload_id: UploadId,
        next_status: UploadStatus,
    ) -> Result<Upload, StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_err)?;

        let mut upload = fetch_upload(&tx, upload_id)?;
        if upload.status != UploadStatus::Pending {
            return Err(StorageError::UploadNotPending {
                upload_id,
                status: upload.status,
            });
        }

        let updated = tx
            .execute(
                "UPDATE uploads SET status = ?1 WHERE id = ?2 AND status = 'pending'",
                params![next_status.as_str(), upload_id.as_i64()],
            )
            .map_err(map_sqlite_err)?;
        if updated != 1 {
            return Err(StorageError::CorruptedData(format!(
                "expected 1 upload row updated for upload {upload_id}, updated {updated}"
            )));
        }

        tx.commit().map_err(map_sqlite_err)?;
        upload.status = next_status;
        Ok(upload)
    }
}

fn configure_connection(conn: &Connection, enable_wal: bool) -> Result<(), StorageError> {
    conn.busy_timeout(BUSY_TIMEOUT).map_err(|err| {
        StorageError::Backend(format!("failed to configure SQLite busy_timeout: {err}"))
    })?;
    migrations::retry_sqlite_on_busy("failed to configure SQLite connection pragmas", || {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        if enable_wal {
            let current_mode: String =
                conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
            if !current_mode.eq_ignore_ascii_case("wal") {
                conn.pragma_update(None, "journal_mode", "WAL")?;
            }
        }
        Ok(())
    })
}

fn find_creator(conn: &Connection, creator_id: CreatorId) -> Result<Creator, StorageError> {
    let row = conn
        .query_row(
            "SELECT id, platform, platform_creator_id, display_name FROM creators WHERE id = ?1",
            [creator_id.as_i64()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(map_sqlite_err)?
        .ok_or(StorageError::CreatorNotFound { creator_id })?;

    creator_from_parts(row.0, row.1, row.2, row.3)
}

fn find_follow(conn: &Connection, creator_id: CreatorId) -> Result<Option<Follow>, StorageError> {
    let row = conn
        .query_row(
            "SELECT id, followed_at, refresh_checkpoint FROM follows WHERE creator_id = ?1",
            [creator_id.as_i64()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(map_sqlite_err)?;

    row.map(|(id, followed_at, checkpoint)| {
        follow_from_parts(id, creator_id, followed_at, checkpoint)
    })
    .transpose()
}

fn fetch_upload(conn: &Connection, upload_id: UploadId) -> Result<Upload, StorageError> {
    let row = conn
        .query_row(
            "SELECT id, creator_id, platform, platform_upload_id, url, title, published_at, status
             FROM uploads
             WHERE id = ?1",
            [upload_id.as_i64()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        )
        .optional()
        .map_err(map_sqlite_err)?
        .ok_or(StorageError::UploadNotFound { upload_id })?;

    upload_from_parts(row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7)
}

fn read_default_preset_name(tx: &Transaction<'_>) -> Result<String, StorageError> {
    tx.query_row(
        "SELECT default_preset_name FROM download_preferences WHERE id = 1",
        [],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(map_sqlite_err)?
    .ok_or_else(|| {
        StorageError::CorruptedData("download_preferences singleton row is missing".to_owned())
    })
}

fn load_custom_preset_names(tx: &Transaction<'_>) -> Result<Vec<PresetName>, StorageError> {
    let mut stmt = tx
        .prepare("SELECT name FROM download_presets ORDER BY name ASC")
        .map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(map_sqlite_err)?;

    let mut names = Vec::new();
    for row in rows {
        let raw = row.map_err(map_sqlite_err)?;
        let parsed = PresetName::new(raw.clone()).map_err(|err| {
            StorageError::CorruptedData(format!(
                "invalid preset name '{raw}' in download_presets: {err}"
            ))
        })?;
        if parsed.is_builtin_video() {
            return Err(StorageError::CorruptedData(
                "reserved preset name 'video' found in download_presets".to_owned(),
            ));
        }
        names.push(parsed);
    }
    Ok(names)
}

fn load_all_custom_preset_args(
    tx: &Transaction<'_>,
) -> Result<BTreeMap<String, Vec<String>>, StorageError> {
    let mut stmt = tx
        .prepare("SELECT preset_name, arg FROM download_preset_args ORDER BY preset_name ASC, position ASC")
        .map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(map_sqlite_err)?;

    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        let (preset_name, arg) = row.map_err(map_sqlite_err)?;
        map.entry(preset_name).or_default().push(arg);
    }
    Ok(map)
}

fn resolve_custom_preset(
    tx: &Transaction<'_>,
    name: &PresetName,
) -> Result<Option<DownloadPreset>, StorageError> {
    let exists = tx
        .query_row(
            "SELECT 1 FROM download_presets WHERE name = ?1",
            [name.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(map_sqlite_err)?
        .is_some();

    if !exists {
        return Ok(None);
    }

    let mut stmt = tx
        .prepare(
            "SELECT arg FROM download_preset_args WHERE preset_name = ?1 ORDER BY position ASC",
        )
        .map_err(map_sqlite_err)?;
    let rows = stmt
        .query_map([name.as_str()], |row| row.get::<_, String>(0))
        .map_err(map_sqlite_err)?;

    let mut args = Vec::new();
    for row in rows {
        args.push(row.map_err(map_sqlite_err)?);
    }

    Ok(Some(DownloadPreset {
        name: name.clone(),
        args,
        is_builtin: false,
    }))
}

fn creator_id_from_i64(id: i64) -> Result<CreatorId, StorageError> {
    CreatorId::new(id)
        .map_err(|err| StorageError::CorruptedData(format!("invalid creator ID in SQLite: {err}")))
}

fn follow_id_from_i64(id: i64) -> Result<FollowId, StorageError> {
    FollowId::new(id)
        .map_err(|err| StorageError::CorruptedData(format!("invalid follow ID in SQLite: {err}")))
}

fn upload_id_from_i64(id: i64) -> Result<UploadId, StorageError> {
    UploadId::new(id)
        .map_err(|err| StorageError::CorruptedData(format!("invalid upload ID in SQLite: {err}")))
}

fn creator_from_parts(
    id: i64,
    platform: String,
    platform_creator_id: String,
    display_name: String,
) -> Result<Creator, StorageError> {
    let id = creator_id_from_i64(id)?;
    let platform = Platform::from_str(&platform).map_err(|err| {
        StorageError::CorruptedData(format!("invalid creator platform '{platform}': {err}"))
    })?;
    Ok(Creator {
        id,
        platform,
        platform_creator_id,
        display_name,
    })
}

fn follow_from_parts(
    id: i64,
    creator_id: CreatorId,
    followed_at: i64,
    refresh_checkpoint: i64,
) -> Result<Follow, StorageError> {
    Ok(Follow {
        id: follow_id_from_i64(id)?,
        creator_id,
        followed_at: Timestamp::from_unix_seconds(followed_at),
        refresh_checkpoint: Timestamp::from_unix_seconds(refresh_checkpoint),
    })
}

#[allow(clippy::too_many_arguments)]
fn upload_from_parts(
    id: i64,
    creator_id: i64,
    platform: String,
    platform_upload_id: String,
    url: String,
    title: String,
    published_at: i64,
    status: String,
) -> Result<Upload, StorageError> {
    let id = upload_id_from_i64(id)?;
    let creator_id = creator_id_from_i64(creator_id)?;
    let platform = Platform::from_str(&platform).map_err(|err| {
        StorageError::CorruptedData(format!("invalid upload platform '{platform}': {err}"))
    })?;
    let status = UploadStatus::from_str(&status).map_err(|err| {
        StorageError::CorruptedData(format!("invalid upload status '{status}': {err}"))
    })?;
    Ok(Upload {
        id,
        creator_id,
        platform,
        platform_upload_id,
        url,
        title,
        published_at: Timestamp::from_unix_seconds(published_at),
        status,
    })
}

fn encode_cookie_source(
    source: Option<&CookieSource>,
) -> Result<(Option<&'static str>, Option<String>), StorageError> {
    match source {
        None => Ok((None, None)),
        Some(CookieSource::File(path)) => {
            if path.as_os_str().is_empty() {
                return Err(StorageError::Backend(
                    "cookie file path cannot be empty".to_owned(),
                ));
            }
            // Resolve relative paths against the caller's working directory on save so later
            // refreshes from other directories target the same file without requiring existence
            // or symlink canonicalization.
            let resolved = std::path::absolute(path).map_err(|err| {
                StorageError::Backend(format!(
                    "failed to resolve cookie file path '{}': {err}",
                    path.display()
                ))
            })?;
            let as_str = resolved.into_os_string().into_string().map_err(|_| {
                StorageError::Backend("cookie file path must be valid UTF-8".to_owned())
            })?;
            Ok((Some("file"), Some(as_str)))
        }
        Some(CookieSource::BrowserProfile(spec)) => {
            if spec.trim().is_empty() {
                return Err(StorageError::Backend(
                    "browser profile selector cannot be empty".to_owned(),
                ));
            }
            Ok((Some("browser_profile"), Some(spec.clone())))
        }
    }
}

fn decode_cookie_source(
    kind: Option<String>,
    value: Option<String>,
) -> Result<Option<CookieSource>, StorageError> {
    match (kind, value) {
        (None, None) => Ok(None),
        (Some(kind), Some(value)) if !value.is_empty() => match kind.as_str() {
            "file" => Ok(Some(CookieSource::File(PathBuf::from(value)))),
            "browser_profile" => {
                if value.trim().is_empty() {
                    return Err(StorageError::CorruptedData(
                        "refresh_preferences browser profile selector cannot be blank".to_owned(),
                    ));
                }
                Ok(Some(CookieSource::BrowserProfile(value)))
            }
            other => Err(StorageError::CorruptedData(format!(
                "unknown cookie_source_kind '{other}' in refresh_preferences"
            ))),
        },
        (kind, value) => Err(StorageError::CorruptedData(format!(
            "inconsistent refresh_preferences cookie columns: kind={kind:?}, value={value:?}"
        ))),
    }
}

fn map_sqlite_err(err: rusqlite::Error) -> StorageError {
    StorageError::Backend(err.to_string())
}
