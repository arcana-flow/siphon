use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use clap::Parser;
use rusqlite::Connection;
use siphon::app::{AppServices, execute};
use siphon::cli::Cli;
use siphon::domain::{
    CookieSource, Creator, CreatorId, DiscoveredUpload, FollowId, Platform, PresetName,
    PresetSummary, Timestamp, UploadId, UploadStatus, builtin_video_preset,
};
use siphon::download::{DownloadOutcome, DownloadRequest, Downloader};
use siphon::error::{AppError, DownloadError, RefreshError, StorageError};
use siphon::refresh::{RefreshFeedResult, RefreshSource, ResolvedCreator};
use siphon::storage::{
    CommitRefreshRequest, FollowCreatorRequest, RemovePresetOutcome, SqliteStorage, Storage,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "siphon-sqlite-storage-{}-{}-{id}",
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

fn sample_upload(platform_upload_id: &str, url: &str, title: &str, ts: i64) -> DiscoveredUpload {
    DiscoveredUpload {
        platform: Platform::Bilibili,
        platform_upload_id: platform_upload_id.to_owned(),
        url: url.to_owned(),
        title: title.to_owned(),
        published_at: Timestamp::from_unix_seconds(ts),
    }
}

#[test]
fn creator_follow_unfollow_refollow_preserves_stable_id_and_uploads() {
    let temp = TempDir::new("creator-lifecycle");
    let db_path = temp.path().join("siphon.db");

    let t1 = Timestamp::from_unix_seconds(1_760_000_000);
    let t2 = Timestamp::from_unix_seconds(1_760_000_500);
    let t3 = Timestamp::from_unix_seconds(1_760_001_000);

    let storage = SqliteStorage::open(&db_path).unwrap();
    let f_b = storage
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "200".to_owned(),
            display_name: "Creator B".to_owned(),
            followed_at: t1,
        })
        .unwrap();
    let f_a = storage
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "100".to_owned(),
            display_name: "Creator A".to_owned(),
            followed_at: t1,
        })
        .unwrap();

    assert_eq!(f_b.creator.id, CreatorId::new(1).unwrap());
    assert_eq!(f_a.creator.id, CreatorId::new(2).unwrap());
    assert_eq!(f_b.follow.id, FollowId::new(1).unwrap());
    assert_eq!(f_a.follow.id, FollowId::new(2).unwrap());

    // Duplicate active follow must fail and preserve existing display name.
    let dup = storage
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "200".to_owned(),
            display_name: "Changed B".to_owned(),
            followed_at: t2,
        })
        .unwrap_err();
    assert_eq!(
        dup,
        StorageError::CreatorAlreadyFollowed {
            creator_id: f_b.creator.id,
            display_name: "Creator B".to_owned(),
        }
    );

    // Deterministic ordering by CreatorId.
    let listed = storage.list_followed_creators().unwrap();
    assert_eq!(listed, vec![f_b.clone(), f_a.clone()]);

    // Commit uploads for Creator B (pending + ignored + downloaded).
    let outcome = storage
        .commit_refresh(CommitRefreshRequest {
            creator_id: f_b.creator.id,
            expected_follow_id: f_b.follow.id,
            expected_follow_time: t1,
            expected_checkpoint: t1,
            new_checkpoint: t2,
            discovered_uploads: vec![
                sample_upload("BV-1", "https://bili/1", "Pending One", 1_760_000_100),
                sample_upload("BV-2", "https://bili/2", "Ignored Two", 1_760_000_200),
                sample_upload("BV-3", "https://bili/3", "Downloaded Three", 1_760_000_300),
            ],
        })
        .unwrap();
    assert_eq!(outcome.new_uploads_inserted, 3);
    assert_eq!(outcome.existing_uploads_seen, 0);

    storage
        .mark_upload_ignored(UploadId::new(2).unwrap())
        .unwrap();
    storage
        .mark_upload_downloaded(UploadId::new(3).unwrap())
        .unwrap();

    // Unfollow Creator B; uploads remain accessible and Creator B is excluded from followed list.
    let unfollowed = storage.unfollow_creator(f_b.creator.id).unwrap();
    assert_eq!(unfollowed, f_b.creator);
    assert_eq!(
        storage.unfollow_creator(f_b.creator.id).unwrap_err(),
        StorageError::CreatorNotFollowed {
            creator_id: f_b.creator.id
        }
    );
    assert_eq!(
        storage.get_followed_creator(f_b.creator.id).unwrap_err(),
        StorageError::CreatorNotFollowed {
            creator_id: f_b.creator.id
        }
    );
    assert_eq!(
        storage
            .get_followed_creator(CreatorId::new(999).unwrap())
            .unwrap_err(),
        StorageError::CreatorNotFound {
            creator_id: CreatorId::new(999).unwrap()
        }
    );
    assert_eq!(storage.list_followed_creators().unwrap(), vec![f_a]);
    assert_eq!(storage.list_pending_uploads().unwrap().len(), 1);
    assert_eq!(
        storage
            .get_upload(UploadId::new(2).unwrap())
            .unwrap()
            .status,
        UploadStatus::Ignored
    );
    assert_eq!(
        storage
            .get_upload(UploadId::new(3).unwrap())
            .unwrap()
            .status,
        UploadStatus::Downloaded
    );
    drop(storage);

    // Reopen database and re-follow Creator B: same CreatorId(1), updated display_name, fresh FollowId(3),
    // checkpoint initialized to t3, and old upload statuses preserved.
    let reopened = SqliteStorage::open(&db_path).unwrap();
    let refollowed = reopened
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "200".to_owned(),
            display_name: "Creator B Renamed".to_owned(),
            followed_at: t3,
        })
        .unwrap();
    assert_eq!(refollowed.creator.id, f_b.creator.id);
    assert_eq!(refollowed.creator.display_name, "Creator B Renamed");
    assert_eq!(refollowed.follow.id, FollowId::new(3).unwrap());
    assert_eq!(refollowed.follow.followed_at, t3);
    assert_eq!(refollowed.follow.refresh_checkpoint, t3);

    assert_eq!(
        reopened
            .get_upload(UploadId::new(1).unwrap())
            .unwrap()
            .status,
        UploadStatus::Pending
    );
    assert_eq!(
        reopened
            .get_upload(UploadId::new(2).unwrap())
            .unwrap()
            .status,
        UploadStatus::Ignored
    );
    assert_eq!(
        reopened
            .get_upload(UploadId::new(3).unwrap())
            .unwrap()
            .status,
        UploadStatus::Downloaded
    );
}

#[test]
fn upload_identity_url_updates_and_status_transitions_are_preserved() {
    let storage = SqliteStorage::open_in_memory().unwrap();
    let t0 = Timestamp::from_unix_seconds(1_760_000_000);
    let t1 = Timestamp::from_unix_seconds(1_760_000_100);
    let t2 = Timestamp::from_unix_seconds(1_760_000_200);

    let followed = storage
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "700".to_owned(),
            display_name: "Creator 700".to_owned(),
            followed_at: t0,
        })
        .unwrap();

    // First refresh includes a duplicate platform_upload_id within the same batch.
    let first = storage
        .commit_refresh(CommitRefreshRequest {
            creator_id: followed.creator.id,
            expected_follow_id: followed.follow.id,
            expected_follow_time: t0,
            expected_checkpoint: t0,
            new_checkpoint: t1,
            discovered_uploads: vec![
                sample_upload("BV-same", "https://bili/old", "Old Title", 1_760_000_010),
                sample_upload("BV-same", "https://bili/new", "New Title", 1_760_000_010),
                sample_upload("BV-keep", "https://bili/keep", "Keep Me", 1_760_000_020),
            ],
        })
        .unwrap();
    assert_eq!(first.new_uploads_inserted, 2);
    assert_eq!(first.existing_uploads_seen, 1);

    let u1 = storage.get_upload(UploadId::new(1).unwrap()).unwrap();
    let u2 = storage.get_upload(UploadId::new(2).unwrap()).unwrap();
    assert_eq!(u1.url, "https://bili/new");
    assert_eq!(u1.title, "New Title");
    assert_eq!(u2.platform_upload_id, "BV-keep");

    // Mark u1 ignored and u2 downloaded; subsequent non-pending transitions fail cleanly.
    storage.mark_upload_ignored(u1.id).unwrap();
    storage.mark_upload_downloaded(u2.id).unwrap();
    assert_eq!(
        storage.mark_upload_downloaded(u1.id).unwrap_err(),
        StorageError::UploadNotPending {
            upload_id: u1.id,
            status: UploadStatus::Ignored,
        }
    );
    assert_eq!(
        storage.mark_upload_ignored(u2.id).unwrap_err(),
        StorageError::UploadNotPending {
            upload_id: u2.id,
            status: UploadStatus::Downloaded,
        }
    );
    assert_eq!(
        storage
            .mark_upload_ignored(UploadId::new(999).unwrap())
            .unwrap_err(),
        StorageError::UploadNotFound {
            upload_id: UploadId::new(999).unwrap(),
        }
    );

    // Second refresh encounters known uploads with changed URLs plus a re-upload with a new ID.
    let second = storage
        .commit_refresh(CommitRefreshRequest {
            creator_id: followed.creator.id,
            expected_follow_id: followed.follow.id,
            expected_follow_time: t0,
            expected_checkpoint: t1,
            new_checkpoint: t2,
            discovered_uploads: vec![
                sample_upload(
                    "BV-same",
                    "https://bili/v2",
                    "Updated Ignored",
                    1_760_000_010,
                ),
                sample_upload(
                    "BV-keep",
                    "https://bili/keep-v2",
                    "Updated Downloaded",
                    1_760_000_020,
                ),
                sample_upload("BV-reupload", "https://bili/re", "Reupload", 1_760_000_150),
            ],
        })
        .unwrap();
    assert_eq!(second.new_uploads_inserted, 1);
    assert_eq!(second.existing_uploads_seen, 2);

    let refreshed_u1 = storage.get_upload(u1.id).unwrap();
    assert_eq!(refreshed_u1.status, UploadStatus::Ignored);
    assert_eq!(refreshed_u1.url, "https://bili/v2");
    assert_eq!(refreshed_u1.title, "Updated Ignored");

    let refreshed_u2 = storage.get_upload(u2.id).unwrap();
    assert_eq!(refreshed_u2.status, UploadStatus::Downloaded);
    assert_eq!(refreshed_u2.url, "https://bili/keep-v2");
    assert_eq!(refreshed_u2.title, "Updated Downloaded");

    let pending = storage.list_pending_uploads().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, UploadId::new(3).unwrap());
    assert_eq!(pending[0].platform_upload_id, "BV-reupload");

    // Empty refresh at unchanged checkpoint commits cleanly.
    let noop = storage
        .commit_refresh(CommitRefreshRequest {
            creator_id: followed.creator.id,
            expected_follow_id: followed.follow.id,
            expected_follow_time: t0,
            expected_checkpoint: t2,
            new_checkpoint: t2,
            discovered_uploads: Vec::new(),
        })
        .unwrap();
    assert_eq!(noop.new_uploads_inserted, 0);
    assert_eq!(noop.existing_uploads_seen, 0);
    assert_eq!(noop.new_checkpoint, t2);
}

#[test]
fn two_handle_same_second_unfollow_and_refollow_rejects_stale_refresh() {
    let temp = TempDir::new("two-handle-same-second");
    let db_path = temp.path().join("shared.db");
    let same_second = Timestamp::from_unix_seconds(1_760_000_000);
    let next_checkpoint = Timestamp::from_unix_seconds(1_760_000_120);

    let handle_a = SqliteStorage::open(&db_path).unwrap();
    let handle_b = SqliteStorage::open(&db_path).unwrap();

    // Handle A follows creator at `same_second` and snapshots follow state before fetching.
    let snapshot_a = handle_a
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "301".to_owned(),
            display_name: "Creator 301".to_owned(),
            followed_at: same_second,
        })
        .unwrap();
    assert_eq!(snapshot_a.follow.id, FollowId::new(1).unwrap());

    // Handle B unfollows and immediately re-follows in the EXACT same second.
    handle_b.unfollow_creator(snapshot_a.creator.id).unwrap();
    let snapshot_b = handle_b
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "301".to_owned(),
            display_name: "Creator 301".to_owned(),
            followed_at: same_second,
        })
        .unwrap();
    assert_eq!(snapshot_b.creator.id, snapshot_a.creator.id);
    assert_eq!(snapshot_b.follow.id, FollowId::new(2).unwrap());
    assert_eq!(snapshot_b.follow.followed_at, same_second);
    assert_eq!(snapshot_b.follow.refresh_checkpoint, same_second);

    // Handle A's stale commit must fail and insert zero uploads despite identical timestamps.
    let stale_err = handle_a
        .commit_refresh(CommitRefreshRequest {
            creator_id: snapshot_a.creator.id,
            expected_follow_id: snapshot_a.follow.id,
            expected_follow_time: snapshot_a.follow.followed_at,
            expected_checkpoint: snapshot_a.follow.refresh_checkpoint,
            new_checkpoint: next_checkpoint,
            discovered_uploads: vec![sample_upload(
                "BV-stale",
                "https://bili/stale",
                "Stale Upload",
                1_760_000_050,
            )],
        })
        .unwrap_err();
    assert_eq!(
        stale_err,
        StorageError::StaleRefreshCheckpoint {
            creator_id: snapshot_a.creator.id,
        }
    );
    assert!(handle_b.list_pending_uploads().unwrap().is_empty());
    assert_eq!(
        handle_b
            .get_followed_creator(snapshot_b.creator.id)
            .unwrap()
            .follow,
        snapshot_b.follow
    );

    // Handle B commits cleanly using the new FollowId(2).
    let committed = handle_b
        .commit_refresh(CommitRefreshRequest {
            creator_id: snapshot_b.creator.id,
            expected_follow_id: snapshot_b.follow.id,
            expected_follow_time: snapshot_b.follow.followed_at,
            expected_checkpoint: snapshot_b.follow.refresh_checkpoint,
            new_checkpoint: next_checkpoint,
            discovered_uploads: vec![sample_upload(
                "BV-fresh",
                "https://bili/fresh",
                "Fresh Upload",
                1_760_000_080,
            )],
        })
        .unwrap();
    assert_eq!(committed.new_uploads_inserted, 1);
    assert_eq!(handle_a.list_pending_uploads().unwrap().len(), 1);
    assert_eq!(
        handle_a
            .get_followed_creator(snapshot_b.creator.id)
            .unwrap()
            .follow
            .refresh_checkpoint,
        next_checkpoint
    );
}

#[test]
fn two_handle_checkpoint_conflict_and_mid_batch_error_rollback_atomically() {
    let temp = TempDir::new("rollback");
    let db_path = temp.path().join("rollback.db");
    let t0 = Timestamp::from_unix_seconds(1_760_000_000);
    let t1 = Timestamp::from_unix_seconds(1_760_000_100);
    let t2 = Timestamp::from_unix_seconds(1_760_000_200);

    let handle_a = SqliteStorage::open(&db_path).unwrap();
    let handle_b = SqliteStorage::open(&db_path).unwrap();

    let followed = handle_a
        .follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: "808".to_owned(),
            display_name: "Creator 808".to_owned(),
            followed_at: t0,
        })
        .unwrap();

    // Handle B advances checkpoint first.
    handle_b
        .commit_refresh(CommitRefreshRequest {
            creator_id: followed.creator.id,
            expected_follow_id: followed.follow.id,
            expected_follow_time: t0,
            expected_checkpoint: t0,
            new_checkpoint: t1,
            discovered_uploads: vec![sample_upload(
                "BV-winner",
                "https://bili/win",
                "Winner",
                1_760_000_050,
            )],
        })
        .unwrap();

    // Handle A still expects t0 -> rejected without inserting BV-loser.
    let conflict = handle_a
        .commit_refresh(CommitRefreshRequest {
            creator_id: followed.creator.id,
            expected_follow_id: followed.follow.id,
            expected_follow_time: t0,
            expected_checkpoint: t0,
            new_checkpoint: t2,
            discovered_uploads: vec![sample_upload(
                "BV-loser",
                "https://bili/lose",
                "Loser",
                1_760_000_060,
            )],
        })
        .unwrap_err();
    assert_eq!(
        conflict,
        StorageError::StaleRefreshCheckpoint {
            creator_id: followed.creator.id,
        }
    );

    // Mid-batch DB failure (empty platform_upload_id violates CHECK) rolls back prior upload AND checkpoint.
    let bad_batch = handle_b
        .commit_refresh(CommitRefreshRequest {
            creator_id: followed.creator.id,
            expected_follow_id: followed.follow.id,
            expected_follow_time: t0,
            expected_checkpoint: t1,
            new_checkpoint: t2,
            discovered_uploads: vec![
                sample_upload(
                    "BV-valid-1",
                    "https://bili/v1",
                    "Should Rollback",
                    1_760_000_120,
                ),
                sample_upload("", "https://bili/bad", "Invalid Empty ID", 1_760_000_130),
            ],
        })
        .unwrap_err();
    assert!(matches!(bad_batch, StorageError::Backend(_)));

    let pending = handle_a.list_pending_uploads().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].platform_upload_id, "BV-winner");
    assert_eq!(
        handle_a
            .get_followed_creator(followed.creator.id)
            .unwrap()
            .follow
            .refresh_checkpoint,
        t1
    );
}

#[test]
fn presets_defaults_and_refresh_cookies_persist_across_reopen() {
    let temp = TempDir::new("prefs-and-presets");
    let db_path = temp.path().join("prefs.db");

    let video = PresetName::builtin_video();
    let audio = PresetName::new("audio").unwrap();
    let subs = PresetName::new("subs").unwrap();

    let cookie_file_path = temp.path().join("siphon cookies").join("netscape.txt");
    let cookie_file_path_reopen = cookie_file_path.clone();

    {
        let storage = SqliteStorage::open(&db_path).unwrap();

        // Initial defaults.
        assert_eq!(
            storage.get_default_preset().unwrap(),
            builtin_video_preset()
        );
        assert_eq!(
            storage.list_presets().unwrap(),
            vec![PresetSummary {
                preset: builtin_video_preset(),
                is_default: true,
            }]
        );
        assert_eq!(storage.get_default_refresh_cookies().unwrap(), None);

        // Built-in preset protections.
        assert_eq!(
            storage
                .add_preset(video.clone(), vec!["-f".to_owned()])
                .unwrap_err(),
            StorageError::CannotModifyBuiltinPreset {
                name: video.clone()
            }
        );
        assert_eq!(
            storage.remove_preset(&video).unwrap_err(),
            StorageError::CannotModifyBuiltinPreset {
                name: video.clone()
            }
        );
        assert_eq!(storage.get_preset(&video).unwrap(), builtin_video_preset());

        // Add custom presets (added out of alphabetical order) and preserve exact arg order.
        let subs_args = vec![
            "--write-subs".to_owned(),
            "--sub-langs".to_owned(),
            "en,zh-Hans".to_owned(),
            "--skip-download".to_owned(),
            "arg with spaces & 'quotes'".to_owned(),
        ];
        let audio_args = vec![
            "-x".to_owned(),
            "--audio-format".to_owned(),
            "mp3".to_owned(),
        ];

        let added_subs = storage.add_preset(subs.clone(), subs_args.clone()).unwrap();
        assert_eq!(added_subs.args, subs_args);
        assert!(!added_subs.is_builtin);

        storage
            .add_preset(audio.clone(), audio_args.clone())
            .unwrap();
        assert_eq!(
            storage.add_preset(audio.clone(), vec![]).unwrap_err(),
            StorageError::PresetAlreadyExists {
                name: audio.clone()
            }
        );

        // Set default to audio and persist refresh cookies.
        let default_audio = storage.set_default_preset(&audio).unwrap();
        assert_eq!(default_audio.name, audio);
        assert_eq!(default_audio.args, audio_args);
        assert_eq!(
            storage
                .set_default_preset(&PresetName::new("missing").unwrap())
                .unwrap_err(),
            StorageError::PresetNotFound {
                name: PresetName::new("missing").unwrap()
            }
        );

        let file_cookies = CookieSource::File(cookie_file_path.clone());
        storage
            .set_default_refresh_cookies(Some(&file_cookies))
            .unwrap();
        assert_eq!(
            storage.get_default_refresh_cookies().unwrap(),
            Some(file_cookies)
        );
    }

    // Reopen database and verify persisted state.
    let reopened = SqliteStorage::open(&db_path).unwrap();
    assert_eq!(reopened.get_default_preset().unwrap().name, audio);
    assert_eq!(
        reopened.get_default_refresh_cookies().unwrap(),
        Some(CookieSource::File(cookie_file_path_reopen))
    );

    let listed = reopened.list_presets().unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].preset.name, video);
    assert!(!listed[0].is_default);
    assert_eq!(listed[1].preset.name, audio);
    assert!(listed[1].is_default);
    assert_eq!(listed[2].preset.name, subs);
    assert!(!listed[2].is_default);
    assert_eq!(
        listed[2].preset.args,
        vec![
            "--write-subs",
            "--sub-langs",
            "en,zh-Hans",
            "--skip-download",
            "arg with spaces & 'quotes'",
        ]
    );

    // Removing non-default preset leaves `audio` as default.
    assert_eq!(
        reopened.remove_preset(&subs).unwrap(),
        RemovePresetOutcome {
            removed: subs.clone(),
            reset_default_to_builtin: false,
        }
    );
    assert_eq!(reopened.get_default_preset().unwrap().name, audio);
    assert_eq!(
        reopened.get_preset(&subs).unwrap_err(),
        StorageError::PresetNotFound { name: subs }
    );

    // Removing active default resets default back to `video` atomically.
    assert_eq!(
        reopened.remove_preset(&audio).unwrap(),
        RemovePresetOutcome {
            removed: audio.clone(),
            reset_default_to_builtin: true,
        }
    );
    assert_eq!(
        reopened.get_default_preset().unwrap(),
        builtin_video_preset()
    );

    // Switch default back and forth and save/clear browser profile cookies.
    reopened
        .add_preset(audio.clone(), vec!["-x".to_owned()])
        .unwrap();
    reopened.set_default_preset(&audio).unwrap();
    assert_eq!(
        reopened.set_default_preset(&video).unwrap(),
        builtin_video_preset()
    );
    assert_eq!(
        reopened.get_default_preset().unwrap(),
        builtin_video_preset()
    );

    let browser_cookies = CookieSource::BrowserProfile("firefox:work".to_owned());
    reopened
        .set_default_refresh_cookies(Some(&browser_cookies))
        .unwrap();
    assert_eq!(
        reopened.get_default_refresh_cookies().unwrap(),
        Some(browser_cookies)
    );
    reopened.set_default_refresh_cookies(None).unwrap();
    assert_eq!(reopened.get_default_refresh_cookies().unwrap(), None);
}

#[test]
fn corrupted_preference_rows_surface_corrupted_data_errors() {
    let temp = TempDir::new("corrupted");
    let db_path = temp.path().join("corrupted.db");

    {
        let storage = SqliteStorage::open(&db_path).unwrap();
        drop(storage);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE download_preferences SET default_preset_name = 'ghost' WHERE id = 1",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM refresh_preferences WHERE id = 1", [])
            .unwrap();
    }

    let storage = SqliteStorage::open(&db_path).unwrap();
    assert!(matches!(
        storage.get_default_preset().unwrap_err(),
        StorageError::CorruptedData(_)
    ));
    assert!(matches!(
        storage.list_presets().unwrap_err(),
        StorageError::CorruptedData(_)
    ));
    assert!(matches!(
        storage.get_default_refresh_cookies().unwrap_err(),
        StorageError::CorruptedData(_)
    ));
    assert!(matches!(
        storage.set_default_refresh_cookies(None).unwrap_err(),
        StorageError::CorruptedData(_)
    ));
}

struct StubRefresh {
    seen_cookies: Mutex<Vec<Option<CookieSource>>>,
}

impl StubRefresh {
    fn new() -> Self {
        Self {
            seen_cookies: Mutex::new(Vec::new()),
        }
    }
}

impl RefreshSource for StubRefresh {
    fn resolve_creator(
        &self,
        url: &str,
        _cookies: Option<&CookieSource>,
    ) -> Result<ResolvedCreator, RefreshError> {
        let mid = url.rsplit('/').next().unwrap();
        Ok(ResolvedCreator {
            platform: Platform::Bilibili,
            platform_creator_id: mid.to_owned(),
            display_name: format!("Creator {mid}"),
        })
    }

    fn fetch_uploads_since(
        &self,
        creator: &Creator,
        checkpoint: Timestamp,
        cookies: Option<&CookieSource>,
    ) -> Result<RefreshFeedResult, RefreshError> {
        self.seen_cookies.lock().unwrap().push(cookies.cloned());
        let published_at = Timestamp::from_unix_seconds(checkpoint.as_unix_seconds() + 60);
        Ok(RefreshFeedResult {
            uploads: vec![DiscoveredUpload {
                platform: Platform::Bilibili,
                platform_upload_id: format!(
                    "BV-{}-{}",
                    creator.platform_creator_id,
                    published_at.as_unix_seconds()
                ),
                url: format!(
                    "https://www.bilibili.com/video/BV-{}-1",
                    creator.platform_creator_id
                ),
                title: format!("Video by {}", creator.display_name),
                published_at,
            }],
            new_checkpoint: published_at,
        })
    }
}

struct StubDownloader {
    requests: Mutex<Vec<DownloadRequest>>,
}

impl StubDownloader {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl Downloader for StubDownloader {
    fn download(&self, request: &DownloadRequest) -> Result<DownloadOutcome, DownloadError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(DownloadOutcome {
            succeeded: true,
            confirmed_artifacts: vec![PathBuf::from("video.mp4")],
        })
    }
}

fn run_cli(services: &AppServices<'_>, argv: &[&str]) -> Result<String, AppError> {
    let cli = Cli::try_parse_from(argv).unwrap();
    execute(cli, services).map(|out| out.stdout)
}

#[test]
fn saved_relative_cookie_file_resolves_against_cwd_without_canonicalization_or_existence() {
    let temp = TempDir::new("relative-cookie-path");
    let db_path = temp.path().join("cookies.db");

    // Relative path to a nonexistent directory/file must resolve against CWD on save.
    let rel_path = PathBuf::from("./nonexistent-cookie-dir/cookies.txt");
    let expected_abs = std::path::absolute(&rel_path).unwrap();
    assert!(expected_abs.is_absolute());
    assert!(!expected_abs.exists());

    {
        let storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .set_default_refresh_cookies(Some(&CookieSource::File(rel_path)))
            .unwrap();
        assert_eq!(
            storage.get_default_refresh_cookies().unwrap(),
            Some(CookieSource::File(expected_abs.clone()))
        );
    }

    // Verify raw SQLite value and persisted reload across reopen.
    {
        let conn = Connection::open(&db_path).unwrap();
        let stored_value: String = conn
            .query_row(
                "SELECT cookie_source_value FROM refresh_preferences WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(PathBuf::from(stored_value), expected_abs);
    }

    let storage = SqliteStorage::open(&db_path).unwrap();
    assert_eq!(
        storage.get_default_refresh_cookies().unwrap(),
        Some(CookieSource::File(expected_abs.clone()))
    );

    // Symlink paths must not be canonicalized when saved.
    #[cfg(unix)]
    {
        let symlink_path = temp.path().join("symlink-cookies.txt");
        let missing_target = temp.path().join("real-target-does-not-exist.txt");
        std::os::unix::fs::symlink(&missing_target, &symlink_path).unwrap();

        storage
            .set_default_refresh_cookies(Some(&CookieSource::File(symlink_path.clone())))
            .unwrap();
        assert_eq!(
            storage.get_default_refresh_cookies().unwrap(),
            Some(CookieSource::File(symlink_path))
        );

        // Restore expected_abs as the saved default before testing one-off invocations.
        storage
            .set_default_refresh_cookies(Some(&CookieSource::File(expected_abs.clone())))
            .unwrap();
    }

    // One-off refresh and download cookie inputs remain unchanged (relative) and leave saved default intact.
    let refresh = StubRefresh::new();
    let downloader = StubDownloader::new();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    run_cli(
        &services,
        &["siphon", "creator", "add", "https://space.bilibili.com/42"],
    )
    .unwrap();
    run_cli(
        &services,
        &["siphon", "refresh", "--cookies", "./oneoff-refresh.txt"],
    )
    .unwrap();
    run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1test",
            "--cookies",
            "./oneoff-download.txt",
        ],
    )
    .unwrap();
    run_cli(&services, &["siphon", "refresh"]).unwrap();

    let seen_refresh_cookies = refresh.seen_cookies.lock().unwrap().clone();
    assert_eq!(
        seen_refresh_cookies,
        vec![
            Some(CookieSource::File(PathBuf::from("./oneoff-refresh.txt"))),
            Some(CookieSource::File(expected_abs.clone())),
        ]
    );

    let dl_reqs = downloader.requests.lock().unwrap();
    assert_eq!(
        dl_reqs[0].cookies,
        Some(CookieSource::File(PathBuf::from("./oneoff-download.txt")))
    );
    drop(dl_reqs);

    // Saving a relative path via `refresh --save-cookies` stores the resolved absolute path.
    run_cli(
        &services,
        &[
            "siphon",
            "refresh",
            "--cookies",
            "saved-via-cli/cookies.txt",
            "--save-cookies",
        ],
    )
    .unwrap();
    let expected_cli_abs = std::path::absolute(Path::new("saved-via-cli/cookies.txt")).unwrap();
    assert_eq!(
        storage.get_default_refresh_cookies().unwrap(),
        Some(CookieSource::File(expected_cli_abs))
    );
}

#[test]
fn app_services_work_end_to_end_with_sqlite_storage_across_reopen() {
    let temp = TempDir::new("e2e-app");
    let db_path = temp.path().join("app.db");
    let refresh = StubRefresh::new();

    // Session 1: follow creator, save preset & default, save cookies, refresh.
    {
        let storage = SqliteStorage::open(&db_path).unwrap();
        let downloader = StubDownloader::new();
        let services = AppServices {
            storage: &storage,
            refresh_source: &refresh,
            downloader: &downloader,
        };

        run_cli(
            &services,
            &[
                "siphon",
                "creator",
                "add",
                "https://space.bilibili.com/9001",
            ],
        )
        .unwrap();
        run_cli(
            &services,
            &[
                "siphon",
                "preset",
                "add",
                "audio",
                "--",
                "-x",
                "--audio-format",
                "opus",
            ],
        )
        .unwrap();
        run_cli(&services, &["siphon", "preset", "set-default", "audio"]).unwrap();
        run_cli(
            &services,
            &[
                "siphon",
                "refresh",
                "--cookies-from-browser",
                "firefox:default",
                "--save-cookies",
            ],
        )
        .unwrap();
    }

    // Session 2: verify saved uploads, preset default, and cookie isolation on download.
    {
        let storage = SqliteStorage::open(&db_path).unwrap();
        let downloader = StubDownloader::new();
        let services = AppServices {
            storage: &storage,
            refresh_source: &refresh,
            downloader: &downloader,
        };

        let pending_out = run_cli(&services, &["siphon", "upload", "list"]).unwrap();
        assert!(pending_out.contains("Video by Creator 9001"));

        let dl_out = run_cli(&services, &["siphon", "download", "--upload", "1"]).unwrap();
        assert!(dl_out.contains("marked as downloaded"));

        let reqs = downloader.requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].cookies, None);
        assert_eq!(
            reqs[0].preset_args,
            vec![
                "-x".to_owned(),
                "--audio-format".to_owned(),
                "opus".to_owned()
            ]
        );

        assert_eq!(
            run_cli(&services, &["siphon", "upload", "list"]).unwrap(),
            "No pending uploads.\n"
        );
    }
}
