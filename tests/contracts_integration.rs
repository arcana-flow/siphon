use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use clap::Parser;
use siphon::app::{AppServices, execute};
use siphon::cli::Cli;
use siphon::domain::{
    CookieSource, Creator, CreatorId, DiscoveredUpload, DownloadPreset, Follow, FollowedCreator,
    Platform, PresetName, PresetSummary, Timestamp, Upload, UploadId, UploadStatus,
    builtin_video_preset,
};
use siphon::download::{DownloadOutcome, DownloadRequest, Downloader};
use siphon::error::{AppError, DownloadError, RefreshError, StorageError};
use siphon::refresh::{RefreshFeedResult, RefreshSource, ResolvedCreator};
use siphon::storage::{
    CommitRefreshOutcome, CommitRefreshRequest, FollowCreatorRequest, RemovePresetOutcome, Storage,
};

#[derive(Default)]
struct MemoryState {
    next_creator_id: i64,
    next_upload_id: i64,
    creators: BTreeMap<CreatorId, Creator>,
    follows: BTreeMap<CreatorId, Follow>,
    uploads: BTreeMap<UploadId, Upload>,
    presets: BTreeMap<PresetName, DownloadPreset>,
    default_preset: Option<PresetName>,
    refresh_cookies: Option<CookieSource>,
}

struct InMemoryStorage {
    state: Mutex<MemoryState>,
}

impl InMemoryStorage {
    fn new() -> Self {
        Self {
            state: Mutex::new(MemoryState {
                next_creator_id: 1,
                next_upload_id: 1,
                ..MemoryState::default()
            }),
        }
    }

    fn set_checkpoint_direct(&self, creator_id: CreatorId, checkpoint: Timestamp) {
        let mut guard = self.state.lock().unwrap();
        guard
            .follows
            .get_mut(&creator_id)
            .unwrap()
            .refresh_checkpoint = checkpoint;
    }

    fn uploads_snapshot(&self) -> Vec<Upload> {
        let guard = self.state.lock().unwrap();
        guard.uploads.values().cloned().collect()
    }
}

impl Storage for InMemoryStorage {
    fn follow_creator(
        &self,
        request: FollowCreatorRequest,
    ) -> Result<FollowedCreator, StorageError> {
        let mut guard = self.state.lock().unwrap();
        let existing_id = guard
            .creators
            .values()
            .find(|c| {
                c.platform == request.platform
                    && c.platform_creator_id == request.platform_creator_id
            })
            .map(|c| c.id);

        let creator = if let Some(id) = existing_id {
            if guard.follows.contains_key(&id) {
                let display_name = guard.creators[&id].display_name.clone();
                return Err(StorageError::CreatorAlreadyFollowed {
                    creator_id: id,
                    display_name,
                });
            }
            let entry = guard.creators.get_mut(&id).unwrap();
            entry.display_name = request.display_name;
            entry.clone()
        } else {
            let id = CreatorId::new(guard.next_creator_id).unwrap();
            guard.next_creator_id += 1;
            let creator = Creator {
                id,
                platform: request.platform,
                platform_creator_id: request.platform_creator_id,
                display_name: request.display_name,
            };
            guard.creators.insert(id, creator.clone());
            creator
        };

        let follow = Follow {
            creator_id: creator.id,
            followed_at: request.followed_at,
            refresh_checkpoint: request.followed_at,
        };
        guard.follows.insert(creator.id, follow.clone());
        Ok(FollowedCreator { creator, follow })
    }

    fn list_followed_creators(&self) -> Result<Vec<FollowedCreator>, StorageError> {
        let guard = self.state.lock().unwrap();
        Ok(guard
            .follows
            .iter()
            .map(|(id, follow)| FollowedCreator {
                creator: guard.creators[id].clone(),
                follow: follow.clone(),
            })
            .collect())
    }

    fn get_followed_creator(&self, creator_id: CreatorId) -> Result<FollowedCreator, StorageError> {
        let guard = self.state.lock().unwrap();
        let creator = guard
            .creators
            .get(&creator_id)
            .cloned()
            .ok_or(StorageError::CreatorNotFound { creator_id })?;
        let follow = guard
            .follows
            .get(&creator_id)
            .cloned()
            .ok_or(StorageError::CreatorNotFollowed { creator_id })?;
        Ok(FollowedCreator { creator, follow })
    }

    fn unfollow_creator(&self, creator_id: CreatorId) -> Result<Creator, StorageError> {
        let mut guard = self.state.lock().unwrap();
        let creator = guard
            .creators
            .get(&creator_id)
            .cloned()
            .ok_or(StorageError::CreatorNotFound { creator_id })?;
        if guard.follows.remove(&creator_id).is_none() {
            return Err(StorageError::CreatorNotFollowed { creator_id });
        }
        Ok(creator)
    }

    fn commit_refresh(
        &self,
        request: CommitRefreshRequest,
    ) -> Result<CommitRefreshOutcome, StorageError> {
        let mut guard = self.state.lock().unwrap();
        let creator = guard.creators.get(&request.creator_id).cloned().ok_or(
            StorageError::CreatorNotFound {
                creator_id: request.creator_id,
            },
        )?;
        let follow = guard.follows.get(&request.creator_id).cloned().ok_or(
            StorageError::StaleRefreshCheckpoint {
                creator_id: request.creator_id,
            },
        )?;

        if follow.followed_at != request.expected_follow_time
            || follow.refresh_checkpoint != request.expected_checkpoint
        {
            return Err(StorageError::StaleRefreshCheckpoint {
                creator_id: request.creator_id,
            });
        }

        let mut new_uploads_inserted = 0usize;
        let mut existing_uploads_seen = 0usize;
        for item in request.discovered_uploads {
            let existing_id = guard
                .uploads
                .values()
                .find(|u| {
                    u.platform == item.platform && u.platform_upload_id == item.platform_upload_id
                })
                .map(|u| u.id);

            if let Some(id) = existing_id {
                existing_uploads_seen += 1;
                let existing = guard.uploads.get_mut(&id).unwrap();
                existing.url = item.url;
                existing.title = item.title;
            } else {
                new_uploads_inserted += 1;
                let id = UploadId::new(guard.next_upload_id).unwrap();
                guard.next_upload_id += 1;
                guard.uploads.insert(
                    id,
                    Upload {
                        id,
                        creator_id: request.creator_id,
                        platform: item.platform,
                        platform_upload_id: item.platform_upload_id,
                        url: item.url,
                        title: item.title,
                        published_at: item.published_at,
                        status: UploadStatus::Pending,
                    },
                );
            }
        }

        guard
            .follows
            .get_mut(&request.creator_id)
            .unwrap()
            .refresh_checkpoint = request.new_checkpoint;

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
        let guard = self.state.lock().unwrap();
        Ok(guard
            .uploads
            .values()
            .filter(|u| u.status == UploadStatus::Pending)
            .cloned()
            .collect())
    }

    fn get_upload(&self, upload_id: UploadId) -> Result<Upload, StorageError> {
        let guard = self.state.lock().unwrap();
        guard
            .uploads
            .get(&upload_id)
            .cloned()
            .ok_or(StorageError::UploadNotFound { upload_id })
    }

    fn mark_upload_ignored(&self, upload_id: UploadId) -> Result<Upload, StorageError> {
        let mut guard = self.state.lock().unwrap();
        let upload = guard
            .uploads
            .get_mut(&upload_id)
            .ok_or(StorageError::UploadNotFound { upload_id })?;
        if upload.status != UploadStatus::Pending {
            return Err(StorageError::UploadNotPending {
                upload_id,
                status: upload.status,
            });
        }
        upload.status = UploadStatus::Ignored;
        Ok(upload.clone())
    }

    fn mark_upload_downloaded(&self, upload_id: UploadId) -> Result<Upload, StorageError> {
        let mut guard = self.state.lock().unwrap();
        let upload = guard
            .uploads
            .get_mut(&upload_id)
            .ok_or(StorageError::UploadNotFound { upload_id })?;
        if upload.status != UploadStatus::Pending {
            return Err(StorageError::UploadNotPending {
                upload_id,
                status: upload.status,
            });
        }
        upload.status = UploadStatus::Downloaded;
        Ok(upload.clone())
    }

    fn add_preset(
        &self,
        name: PresetName,
        args: Vec<String>,
    ) -> Result<DownloadPreset, StorageError> {
        if name.is_builtin_video() {
            return Err(StorageError::CannotModifyBuiltinPreset { name });
        }
        let mut guard = self.state.lock().unwrap();
        if guard.presets.contains_key(&name) {
            return Err(StorageError::PresetAlreadyExists { name });
        }
        let preset = DownloadPreset {
            name: name.clone(),
            args,
            is_builtin: false,
        };
        guard.presets.insert(name, preset.clone());
        Ok(preset)
    }

    fn list_presets(&self) -> Result<Vec<PresetSummary>, StorageError> {
        let guard = self.state.lock().unwrap();
        let active = guard
            .default_preset
            .clone()
            .unwrap_or_else(PresetName::builtin_video);
        let builtin = builtin_video_preset();
        let mut list = vec![PresetSummary {
            is_default: active == builtin.name,
            preset: builtin,
        }];
        for preset in guard.presets.values() {
            list.push(PresetSummary {
                is_default: active == preset.name,
                preset: preset.clone(),
            });
        }
        Ok(list)
    }

    fn get_preset(&self, name: &PresetName) -> Result<DownloadPreset, StorageError> {
        if name.is_builtin_video() {
            return Ok(builtin_video_preset());
        }
        let guard = self.state.lock().unwrap();
        guard
            .presets
            .get(name)
            .cloned()
            .ok_or_else(|| StorageError::PresetNotFound { name: name.clone() })
    }

    fn remove_preset(&self, name: &PresetName) -> Result<RemovePresetOutcome, StorageError> {
        if name.is_builtin_video() {
            return Err(StorageError::CannotModifyBuiltinPreset { name: name.clone() });
        }
        let mut guard = self.state.lock().unwrap();
        if guard.presets.remove(name).is_none() {
            return Err(StorageError::PresetNotFound { name: name.clone() });
        }
        let reset_default_to_builtin = if guard.default_preset.as_ref() == Some(name) {
            guard.default_preset = None;
            true
        } else {
            false
        };
        Ok(RemovePresetOutcome {
            removed: name.clone(),
            reset_default_to_builtin,
        })
    }

    fn set_default_preset(&self, name: &PresetName) -> Result<DownloadPreset, StorageError> {
        if name.is_builtin_video() {
            let mut guard = self.state.lock().unwrap();
            guard.default_preset = None;
            return Ok(builtin_video_preset());
        }
        let mut guard = self.state.lock().unwrap();
        let preset = guard
            .presets
            .get(name)
            .cloned()
            .ok_or_else(|| StorageError::PresetNotFound { name: name.clone() })?;
        guard.default_preset = Some(name.clone());
        Ok(preset)
    }

    fn get_default_preset(&self) -> Result<DownloadPreset, StorageError> {
        let guard = self.state.lock().unwrap();
        match &guard.default_preset {
            Some(name) => guard
                .presets
                .get(name)
                .cloned()
                .ok_or_else(|| StorageError::PresetNotFound { name: name.clone() }),
            None => Ok(builtin_video_preset()),
        }
    }

    fn get_default_refresh_cookies(&self) -> Result<Option<CookieSource>, StorageError> {
        Ok(self.state.lock().unwrap().refresh_cookies.clone())
    }

    fn set_default_refresh_cookies(
        &self,
        source: Option<&CookieSource>,
    ) -> Result<(), StorageError> {
        self.state.lock().unwrap().refresh_cookies = source.cloned();
        Ok(())
    }
}

struct StubRefreshSource {
    fail_for_mid: Option<String>,
    mutate_checkpoint_for: Option<(CreatorId, Timestamp)>,
    seen_cookies: Mutex<Vec<Option<CookieSource>>>,
    storage_ref: Option<*const InMemoryStorage>,
}

// Safety: test helper pointer is only used synchronously inside single-threaded tests.
unsafe impl Sync for StubRefreshSource {}
unsafe impl Send for StubRefreshSource {}

impl StubRefreshSource {
    fn new() -> Self {
        Self {
            fail_for_mid: None,
            mutate_checkpoint_for: None,
            seen_cookies: Mutex::new(Vec::new()),
            storage_ref: None,
        }
    }
}

impl RefreshSource for StubRefreshSource {
    fn resolve_creator(
        &self,
        url: &str,
        _cookies: Option<&CookieSource>,
    ) -> Result<ResolvedCreator, RefreshError> {
        let mid = url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| RefreshError::InvalidCreatorUrl {
                url: url.to_owned(),
                reason: "missing creator ID".to_owned(),
            })?;
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
        if let Some((target_id, new_cp)) = self.mutate_checkpoint_for
            && creator.id == target_id
            && let Some(storage_ptr) = self.storage_ref
        {
            // Safety: test keeps storage alive across call.
            unsafe { &*storage_ptr }.set_checkpoint_direct(target_id, new_cp);
        }
        if self.fail_for_mid.as_deref() == Some(creator.platform_creator_id.as_str()) {
            return Err(RefreshError::Network("simulated 502".to_owned()));
        }
        let published = Timestamp::from_unix_seconds(checkpoint.as_unix_seconds() + 100);
        Ok(RefreshFeedResult {
            uploads: vec![DiscoveredUpload {
                platform: Platform::Bilibili,
                platform_upload_id: format!("BV-{}-1", creator.platform_creator_id),
                url: format!(
                    "https://www.bilibili.com/video/BV-{}-1",
                    creator.platform_creator_id
                ),
                title: format!("Upload from {}", creator.display_name),
                published_at: published,
            }],
            new_checkpoint: published,
        })
    }
}

struct StubDownloader {
    requests: Mutex<Vec<DownloadRequest>>,
    outcome: Result<DownloadOutcome, DownloadError>,
}

impl StubDownloader {
    fn succeeding_with_artifact(path: &str) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            outcome: Ok(DownloadOutcome {
                succeeded: true,
                confirmed_artifacts: vec![PathBuf::from(path)],
            }),
        }
    }

    fn simulation() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            outcome: Ok(DownloadOutcome {
                succeeded: true,
                confirmed_artifacts: Vec::new(),
            }),
        }
    }

    fn failing() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            outcome: Err(DownloadError::ProcessFailed {
                exit_code: Some(1),
                stderr: "network timeout".to_owned(),
            }),
        }
    }
}

impl Downloader for StubDownloader {
    fn download(&self, request: &DownloadRequest) -> Result<DownloadOutcome, DownloadError> {
        self.requests.lock().unwrap().push(request.clone());
        self.outcome.clone()
    }
}

fn run_cmd(services: &AppServices<'_>, argv: &[&str]) -> Result<String, AppError> {
    let cli = Cli::try_parse_from(argv).expect("test argv must parse");
    execute(cli, services).map(|out| out.stdout)
}

#[test]
fn follow_unfollow_refollow_and_independent_refresh_isolation() {
    let storage = InMemoryStorage::new();
    let mut refresh = StubRefreshSource::new();
    refresh.fail_for_mid = Some("100".to_owned());
    let downloader = StubDownloader::simulation();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    let out1 = run_cmd(
        &services,
        &["siphon", "creator", "add", "https://space.bilibili.com/100"],
    )
    .unwrap();
    assert!(out1.contains("Followed creator 1"));

    let dup = run_cmd(
        &services,
        &["siphon", "creator", "add", "https://space.bilibili.com/100"],
    )
    .unwrap_err();
    assert!(matches!(
        dup,
        AppError::Storage(StorageError::CreatorAlreadyFollowed { .. })
    ));

    run_cmd(
        &services,
        &["siphon", "creator", "add", "https://space.bilibili.com/200"],
    )
    .unwrap();

    let cp1_before = storage
        .get_followed_creator(CreatorId::new(1).unwrap())
        .unwrap()
        .follow
        .refresh_checkpoint;
    let cp2_before = storage
        .get_followed_creator(CreatorId::new(2).unwrap())
        .unwrap()
        .follow
        .refresh_checkpoint;

    let err = run_cmd(&services, &["siphon", "refresh"]).unwrap_err();
    match err {
        AppError::RefreshPartialFailure {
            succeeded,
            failures,
        } => {
            assert_eq!(succeeded, 1);
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].creator_id, CreatorId::new(1).unwrap());
        }
        other => panic!("expected partial refresh error, got {other:?}"),
    }

    // Creator 1 checkpoint stayed unchanged; creator 2 advanced and saved upload.
    let cp1_after = storage
        .get_followed_creator(CreatorId::new(1).unwrap())
        .unwrap()
        .follow
        .refresh_checkpoint;
    let cp2_after = storage
        .get_followed_creator(CreatorId::new(2).unwrap())
        .unwrap()
        .follow
        .refresh_checkpoint;
    assert_eq!(cp1_after, cp1_before);
    assert!(cp2_after > cp2_before);
    assert_eq!(storage.list_pending_uploads().unwrap().len(), 1);

    // Unfollow keeps creator 2's uploads; re-following reuses CreatorId(2).
    run_cmd(&services, &["siphon", "creator", "remove", "2"]).unwrap();
    assert_eq!(storage.list_pending_uploads().unwrap().len(), 1);
    let refollow = run_cmd(
        &services,
        &["siphon", "creator", "add", "https://space.bilibili.com/200"],
    )
    .unwrap();
    assert!(refollow.contains("Followed creator 2"));
}

#[test]
fn commit_refresh_aborts_when_checkpoint_changed_during_fetch() {
    let storage = InMemoryStorage::new();
    let mut refresh = StubRefreshSource::new();
    refresh.mutate_checkpoint_for = Some((
        CreatorId::new(1).unwrap(),
        Timestamp::from_unix_seconds(9_999_999_999),
    ));
    refresh.storage_ref = Some(&storage as *const InMemoryStorage);
    let downloader = StubDownloader::simulation();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    run_cmd(
        &services,
        &["siphon", "creator", "add", "https://space.bilibili.com/300"],
    )
    .unwrap();

    let err = run_cmd(&services, &["siphon", "refresh", "1"]).unwrap_err();
    match err {
        AppError::RefreshPartialFailure { failures, .. } => {
            assert_eq!(failures.len(), 1);
            assert!(failures[0].error.contains("checkpoint changed"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert!(storage.list_pending_uploads().unwrap().is_empty());
}

#[test]
fn cookie_precedence_and_download_isolation() {
    let storage = InMemoryStorage::new();
    let refresh = StubRefreshSource::new();
    let downloader = StubDownloader::succeeding_with_artifact("video.mp4");
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    run_cmd(
        &services,
        &["siphon", "creator", "add", "https://space.bilibili.com/400"],
    )
    .unwrap();

    // Save refresh default cookie source.
    run_cmd(
        &services,
        &[
            "siphon",
            "refresh",
            "--cookies",
            "saved.txt",
            "--save-cookies",
        ],
    )
    .unwrap();
    // Subsequent refresh uses saved default.
    run_cmd(&services, &["siphon", "refresh"]).unwrap();
    // --no-cookies suppresses saved default without clearing it.
    run_cmd(&services, &["siphon", "refresh", "--no-cookies"]).unwrap();
    // Saved default still present.
    assert_eq!(
        storage.get_default_refresh_cookies().unwrap(),
        Some(CookieSource::File(PathBuf::from("saved.txt")))
    );

    // Download NEVER inherits refresh's default cookie source.
    run_cmd(
        &services,
        &["siphon", "download", "https://www.bilibili.com/video/BV1xx"],
    )
    .unwrap();
    let recorded = downloader.requests.lock().unwrap();
    assert_eq!(recorded[0].cookies, None);
    drop(recorded);

    // Clear refresh default cookies.
    run_cmd(&services, &["siphon", "refresh", "--clear-cookies"]).unwrap();
    assert_eq!(storage.get_default_refresh_cookies().unwrap(), None);

    let seen = refresh.seen_cookies.lock().unwrap();
    assert_eq!(
        seen.as_slice(),
        &[
            Some(CookieSource::File(PathBuf::from("saved.txt"))),
            Some(CookieSource::File(PathBuf::from("saved.txt"))),
            None,
            None,
        ]
    );
}

#[test]
fn preset_lifecycle_and_download_argument_forwarding() {
    let storage = InMemoryStorage::new();
    let refresh = StubRefreshSource::new();
    let downloader = StubDownloader::succeeding_with_artifact("audio.mp3");
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    // Built-in `video` preset cannot be added or removed.
    let add_builtin = run_cmd(
        &services,
        &["siphon", "preset", "add", "video", "--", "-f", "best"],
    )
    .unwrap_err();
    assert!(add_builtin.to_string().contains("reserved"));

    let remove_builtin = run_cmd(&services, &["siphon", "preset", "remove", "video"]).unwrap_err();
    assert!(remove_builtin.to_string().contains("reserved"));

    // Add custom preset, set as default, verify ordering with extra args.
    run_cmd(
        &services,
        &[
            "siphon",
            "preset",
            "add",
            "audio",
            "--",
            "-x",
            "--audio-format",
            "mp3",
        ],
    )
    .unwrap();
    run_cmd(&services, &["siphon", "preset", "set-default", "audio"]).unwrap();

    run_cmd(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1test",
            "--",
            "--embed-thumbnail",
            "--no-overwrites",
        ],
    )
    .unwrap();

    let reqs = downloader.requests.lock().unwrap();
    assert_eq!(
        reqs[0].build_yt_dlp_args(),
        vec![
            "-x",
            "--audio-format",
            "mp3",
            "--embed-thumbnail",
            "--no-overwrites",
            "--",
            "https://www.bilibili.com/video/BV1test",
        ]
    );

    // --no-preset omits preset args while retaining extra args.
    drop(reqs);
    run_cmd(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1test",
            "--no-preset",
            "--",
            "-f",
            "worst",
        ],
    )
    .unwrap();
    let reqs = downloader.requests.lock().unwrap();
    assert_eq!(
        reqs[1].build_yt_dlp_args(),
        vec![
            "-f",
            "worst",
            "--",
            "https://www.bilibili.com/video/BV1test"
        ]
    );
    drop(reqs);

    // Removing the active default preset resets default back to `video`.
    let removed_out = run_cmd(&services, &["siphon", "preset", "remove", "audio"]).unwrap();
    assert!(removed_out.contains("reset to 'video'"));
    assert_eq!(
        storage.get_default_preset().unwrap().name,
        PresetName::builtin_video()
    );
}

#[test]
fn download_status_updates_only_on_confirmed_upload_artifact() {
    let storage = InMemoryStorage::new();
    let refresh = StubRefreshSource::new();
    let sim_downloader = StubDownloader::simulation();
    let sim_services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &sim_downloader,
    };

    run_cmd(
        &sim_services,
        &["siphon", "creator", "add", "https://space.bilibili.com/500"],
    )
    .unwrap();
    run_cmd(&sim_services, &["siphon", "refresh"]).unwrap();
    let upload_id = storage.list_pending_uploads().unwrap()[0].id;
    let upload_url = storage.list_pending_uploads().unwrap()[0].url.clone();

    // Simulation leaves upload pending.
    let sim_out = run_cmd(
        &sim_services,
        &[
            "siphon",
            "download",
            "--upload",
            &upload_id.to_string(),
            "--",
            "--simulate",
        ],
    )
    .unwrap();
    assert!(sim_out.contains("remains pending"));
    assert_eq!(
        storage.get_upload(upload_id).unwrap().status,
        UploadStatus::Pending
    );

    // Direct URL download leaves matching pending upload untouched even with confirmed artifact.
    let ok_downloader = StubDownloader::succeeding_with_artifact("subs.srt");
    let ok_services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &ok_downloader,
    };
    let direct_ok = run_cmd(&ok_services, &["siphon", "download", &upload_url]).unwrap();
    assert!(direct_ok.contains("Downloaded direct URL"));
    assert!(direct_ok.contains("1 artifact(s) confirmed"));
    assert_eq!(
        storage.get_upload(upload_id).unwrap().status,
        UploadStatus::Pending
    );

    // Direct URL simulation / run without confirmed artifacts uses neutral completion message.
    let direct_sim = run_cmd(
        &sim_services,
        &["siphon", "download", &upload_url, "--", "--simulate"],
    )
    .unwrap();
    assert!(!direct_sim.to_ascii_lowercase().contains("downloaded"));
    assert!(direct_sim.contains("without confirmed output artifact"));
    assert_eq!(
        storage.get_upload(upload_id).unwrap().status,
        UploadStatus::Pending
    );

    // Ok(DownloadOutcome { succeeded: false, ... }) surfaces as nonzero AppError for direct & upload runs.
    let unsuccessful_direct = StubDownloader {
        requests: Mutex::new(Vec::new()),
        outcome: Ok(DownloadOutcome {
            succeeded: false,
            confirmed_artifacts: Vec::new(),
        }),
    };
    let unsuccessful_direct_services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &unsuccessful_direct,
    };
    let direct_unsuccessful_err = run_cmd(
        &unsuccessful_direct_services,
        &["siphon", "download", &upload_url],
    )
    .unwrap_err();
    assert!(matches!(direct_unsuccessful_err, AppError::Download(_)));

    let unsuccessful_with_artifact = StubDownloader {
        requests: Mutex::new(Vec::new()),
        outcome: Ok(DownloadOutcome {
            succeeded: false,
            confirmed_artifacts: vec![PathBuf::from("partial.mp4")],
        }),
    };
    let unsuccessful_upload_services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &unsuccessful_with_artifact,
    };
    let upload_unsuccessful_err = run_cmd(
        &unsuccessful_upload_services,
        &["siphon", "download", "--upload", &upload_id.to_string()],
    )
    .unwrap_err();
    assert!(matches!(upload_unsuccessful_err, AppError::Download(_)));
    assert_eq!(
        storage.get_upload(upload_id).unwrap().status,
        UploadStatus::Pending
    );

    // Failed download leaves upload pending.
    let fail_downloader = StubDownloader::failing();
    let fail_services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &fail_downloader,
    };
    let err = run_cmd(
        &fail_services,
        &["siphon", "download", "--upload", &upload_id.to_string()],
    )
    .unwrap_err();
    assert!(matches!(err, AppError::Download(_)));
    assert_eq!(
        storage.get_upload(upload_id).unwrap().status,
        UploadStatus::Pending
    );

    // Confirmed upload download marks it Downloaded and passes `--no-playlist`.
    let ok_out = run_cmd(
        &ok_services,
        &["siphon", "download", "--upload", &upload_id.to_string()],
    )
    .unwrap();
    assert!(ok_out.contains("marked as downloaded"));
    assert_eq!(
        storage.get_upload(upload_id).unwrap().status,
        UploadStatus::Downloaded
    );
    let recorded = ok_downloader.requests.lock().unwrap();
    assert!(
        recorded[1]
            .build_yt_dlp_args()
            .contains(&"--no-playlist".to_owned())
    );

    // Downloading or ignoring a non-pending upload fails cleanly.
    let second_try = run_cmd(
        &ok_services,
        &["siphon", "download", "--upload", &upload_id.to_string()],
    )
    .unwrap_err();
    assert!(matches!(
        second_try,
        AppError::Storage(StorageError::UploadNotPending { .. })
    ));

    let ignore_downloaded = run_cmd(
        &ok_services,
        &["siphon", "upload", "ignore", &upload_id.to_string()],
    )
    .unwrap_err();
    assert!(matches!(
        ignore_downloaded,
        AppError::Storage(StorageError::UploadNotPending { .. })
    ));

    // Refreshing again preserves Downloaded status even if we keep the upload in storage.
    assert_eq!(storage.uploads_snapshot().len(), 1);
    run_cmd(&ok_services, &["siphon", "refresh"]).unwrap();
    assert_eq!(
        storage.get_upload(upload_id).unwrap().status,
        UploadStatus::Downloaded
    );
}

#[test]
fn list_and_ignore_commands_render_human_readable_output() {
    let storage = InMemoryStorage::new();
    let refresh = StubRefreshSource::new();
    let downloader = StubDownloader::succeeding_with_artifact("out.mp4");
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    assert_eq!(
        run_cmd(&services, &["siphon", "creator", "list"]).unwrap(),
        "No followed creators.\n"
    );
    assert_eq!(
        run_cmd(&services, &["siphon", "upload", "list"]).unwrap(),
        "No pending uploads.\n"
    );

    run_cmd(
        &services,
        &["siphon", "creator", "add", "https://space.bilibili.com/777"],
    )
    .unwrap();
    let creators_out = run_cmd(&services, &["siphon", "creator", "list"]).unwrap();
    assert!(creators_out.contains("Creator 777"));
    assert!(creators_out.contains("bilibili"));

    run_cmd(&services, &["siphon", "refresh"]).unwrap();
    let uploads_out = run_cmd(&services, &["siphon", "upload", "list"]).unwrap();
    assert!(uploads_out.contains("Upload from Creator 777"));

    let ignore_out = run_cmd(&services, &["siphon", "upload", "ignore", "1"]).unwrap();
    assert!(ignore_out.contains("Ignored upload 1"));
    assert_eq!(
        run_cmd(&services, &["siphon", "upload", "list"]).unwrap(),
        "No pending uploads.\n"
    );

    let presets_out = run_cmd(&services, &["siphon", "preset", "list"]).unwrap();
    assert!(presets_out.contains("* video"));
    assert!(presets_out.contains("[built-in]"));

    // Explicit download cookie option is forwarded to yt-dlp.
    run_cmd(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1cookie",
            "--cookies-from-browser",
            "firefox",
        ],
    )
    .unwrap();
    let reqs = downloader.requests.lock().unwrap();
    assert_eq!(
        reqs[0].build_yt_dlp_args(),
        vec![
            "-f",
            "bv*+ba/b",
            "--cookies-from-browser",
            "firefox",
            "--",
            "https://www.bilibili.com/video/BV1cookie",
        ]
    );
}

#[test]
fn binary_reports_usage_and_unimplemented_backend_errors_cleanly() {
    let bin = env!("CARGO_BIN_EXE_siphon");

    let help = Command::new(bin).arg("--help").output().unwrap();
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("creator"));
    assert!(stdout.contains("refresh"));
    assert!(stdout.contains("upload"));
    assert!(stdout.contains("download"));
    assert!(stdout.contains("preset"));

    let missing_args = Command::new(bin).output().unwrap();
    assert_eq!(missing_args.status.code(), Some(2));

    let unimpl = Command::new(bin)
        .args(["creator", "list"])
        .output()
        .unwrap();
    assert_eq!(unimpl.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&unimpl.stderr);
    assert!(stderr.contains("error:"));
    assert!(stderr.contains("not implemented yet"));
}
