use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use clap::Parser;
use siphon::app::{AppServices, execute};
use siphon::cli::Cli;
use siphon::domain::{
    CookieSource, Creator, CreatorId, DownloadPreset, Follow, FollowId, FollowedCreator, Platform,
    PresetName, PresetSummary, Timestamp, Upload, UploadId, UploadStatus, builtin_video_preset,
};
use siphon::download::{DownloadTarget, Downloader, YtDlpDownloader};
use siphon::error::{AppError, DownloadError, RefreshError, StorageError};
use siphon::refresh::{RefreshFeedResult, RefreshSource, ResolvedCreator};
use siphon::storage::{
    CommitRefreshOutcome, CommitRefreshRequest, FollowCreatorRequest, RemovePresetOutcome, Storage,
};

static NEXT_TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempWorkspace {
    root: PathBuf,
}

impl TempWorkspace {
    fn new(label: &str) -> Self {
        let seq = NEXT_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!(
            "siphon-dl-test-{}-{}-{}",
            std::process::id(),
            seq,
            label
        ));
        fs::create_dir_all(&root).expect("create temp test dir");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn compiled_stub_binary() -> &'static PathBuf {
    static STUB_BIN: OnceLock<PathBuf> = OnceLock::new();
    STUB_BIN.get_or_init(|| {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest_dir
            .join("tests")
            .join("fixtures")
            .join("yt_dlp_stub.rs");
        let build_dir = env::temp_dir().join(format!("siphon-stub-bin-{}", std::process::id()));
        fs::create_dir_all(&build_dir).expect("create stub build dir");
        let out_bin = build_dir.join(format!("yt-dlp-stub{}", env::consts::EXE_SUFFIX));

        let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = Command::new(rustc)
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&out_bin)
            .output()
            .expect("invoke rustc to compile offline yt-dlp stub");
        assert!(
            output.status.success(),
            "failed to compile yt-dlp stub:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        out_bin
    })
}

fn fixture_cookies_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("cookies.txt")
}

#[derive(Debug, PartialEq, Eq)]
struct LoggedInvocation {
    cwd: String,
    args: Vec<String>,
}

fn unescape_field(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn read_stub_log(log_path: &Path) -> Vec<LoggedInvocation> {
    let Ok(contents) = fs::read_to_string(log_path) else {
        return Vec::new();
    };
    let mut invocations = Vec::new();
    let mut current_cwd = None;
    let mut current_args = Vec::new();

    for line in contents.lines() {
        if line == "INVOCATION" {
            current_cwd = None;
            current_args = Vec::new();
        } else if let Some(cwd) = line.strip_prefix("CWD=") {
            current_cwd = Some(unescape_field(cwd));
        } else if let Some(rest) = line.strip_prefix("ARG[")
            && let Some((_, val)) = rest.split_once("]=")
        {
            current_args.push(unescape_field(val));
        } else if line == "END" {
            invocations.push(LoggedInvocation {
                cwd: current_cwd.take().unwrap_or_default(),
                args: std::mem::take(&mut current_args),
            });
        }
    }
    invocations
}

fn write_stub_config(work_dir: &Path, entries: &[(&str, &str)]) {
    let mut body = String::new();
    for (k, v) in entries {
        body.push_str(k);
        body.push('=');
        body.push_str(v);
        body.push('\n');
    }
    fs::write(work_dir.join(".siphon-stub.cfg"), body).expect("write stub config");
}

#[derive(Default)]
struct MemoryState {
    next_creator_id: i64,
    next_follow_id: i64,
    next_upload_id: i64,
    creators: BTreeMap<CreatorId, Creator>,
    follows: BTreeMap<CreatorId, Follow>,
    uploads: BTreeMap<UploadId, Upload>,
    presets: BTreeMap<PresetName, DownloadPreset>,
    default_preset: Option<PresetName>,
    refresh_cookies: Option<CookieSource>,
}

struct TestStorage {
    state: Mutex<MemoryState>,
}

impl TestStorage {
    fn new() -> Self {
        Self {
            state: Mutex::new(MemoryState {
                next_creator_id: 1,
                next_follow_id: 1,
                next_upload_id: 1,
                ..MemoryState::default()
            }),
        }
    }

    fn seed_pending_upload(&self, platform_upload_id: &str, url: &str, title: &str) -> Upload {
        let mut guard = self.state.lock().unwrap();
        let creator_id = CreatorId::new(guard.next_creator_id).unwrap();
        guard.next_creator_id += 1;
        guard.creators.insert(
            creator_id,
            Creator {
                id: creator_id,
                platform: Platform::Bilibili,
                platform_creator_id: format!("mid-{creator_id}"),
                display_name: format!("Creator {creator_id}"),
            },
        );
        let id = UploadId::new(guard.next_upload_id).unwrap();
        guard.next_upload_id += 1;
        let upload = Upload {
            id,
            creator_id,
            platform: Platform::Bilibili,
            platform_upload_id: platform_upload_id.to_owned(),
            url: url.to_owned(),
            title: title.to_owned(),
            published_at: Timestamp::from_unix_seconds(1_700_000_000),
            status: UploadStatus::Pending,
        };
        guard.uploads.insert(id, upload.clone());
        upload
    }
}

impl Storage for TestStorage {
    fn follow_creator(
        &self,
        request: FollowCreatorRequest,
    ) -> Result<FollowedCreator, StorageError> {
        let mut guard = self.state.lock().unwrap();
        let id = CreatorId::new(guard.next_creator_id).unwrap();
        guard.next_creator_id += 1;
        let creator = Creator {
            id,
            platform: request.platform,
            platform_creator_id: request.platform_creator_id,
            display_name: request.display_name,
        };
        guard.creators.insert(id, creator.clone());
        let follow_id = FollowId::new(guard.next_follow_id).unwrap();
        guard.next_follow_id += 1;
        let follow = Follow {
            id: follow_id,
            creator_id: id,
            followed_at: request.followed_at,
            refresh_checkpoint: request.followed_at,
        };
        guard.follows.insert(id, follow.clone());
        Ok(FollowedCreator { creator, follow })
    }

    fn list_followed_creators(&self) -> Result<Vec<FollowedCreator>, StorageError> {
        Ok(Vec::new())
    }

    fn get_followed_creator(&self, creator_id: CreatorId) -> Result<FollowedCreator, StorageError> {
        Err(StorageError::CreatorNotFound { creator_id })
    }

    fn unfollow_creator(&self, creator_id: CreatorId) -> Result<Creator, StorageError> {
        Err(StorageError::CreatorNotFound { creator_id })
    }

    fn commit_refresh(
        &self,
        request: CommitRefreshRequest,
    ) -> Result<CommitRefreshOutcome, StorageError> {
        Err(StorageError::CreatorNotFound {
            creator_id: request.creator_id,
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
        self.state
            .lock()
            .unwrap()
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
        Ok(vec![PresetSummary {
            preset: builtin_video_preset(),
            is_default: true,
        }])
    }

    fn get_preset(&self, name: &PresetName) -> Result<DownloadPreset, StorageError> {
        if name.is_builtin_video() {
            return Ok(builtin_video_preset());
        }
        self.state
            .lock()
            .unwrap()
            .presets
            .get(name)
            .cloned()
            .ok_or_else(|| StorageError::PresetNotFound { name: name.clone() })
    }

    fn remove_preset(&self, name: &PresetName) -> Result<RemovePresetOutcome, StorageError> {
        Err(StorageError::PresetNotFound { name: name.clone() })
    }

    fn set_default_preset(&self, name: &PresetName) -> Result<DownloadPreset, StorageError> {
        if name.is_builtin_video() {
            self.state.lock().unwrap().default_preset = None;
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

struct NoopRefreshSource;

impl RefreshSource for NoopRefreshSource {
    fn resolve_creator(
        &self,
        _url: &str,
        _cookies: Option<&CookieSource>,
    ) -> Result<ResolvedCreator, RefreshError> {
        Err(RefreshError::NotImplemented)
    }

    fn fetch_uploads_since(
        &self,
        _creator: &Creator,
        _checkpoint: Timestamp,
        _cookies: Option<&CookieSource>,
    ) -> Result<RefreshFeedResult, RefreshError> {
        Err(RefreshError::NotImplemented)
    }
}

fn run_cli(services: &AppServices<'_>, argv: &[&str]) -> Result<String, AppError> {
    let cli = Cli::try_parse_from(argv).expect("CLI argv must parse");
    execute(cli, services).map(|out| out.stdout)
}

fn assert_manifest_prefix_and_tail(args: &[String], expected_tail: &[&str]) {
    assert!(
        args.len() >= 6,
        "expected manifest --print-to-file prefix in {args:?}"
    );
    assert_eq!(args[0], "--print-to-file");
    assert!(args[1].starts_with("after_move:"));
    assert_eq!(args[3], "--print-to-file");
    assert!(args[4].starts_with("after_video:"));
    assert_eq!(args[2], args[5]);
    assert_eq!(&args[6..], expected_tail);
}

#[cfg(unix)]
struct UnreadableDirGuard {
    path: PathBuf,
}

#[cfg(unix)]
impl UnreadableDirGuard {
    fn new(path: PathBuf) -> Self {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(&path).expect("create locked subdir");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod 000");
        Self { path }
    }
}

#[cfg(unix)]
impl Drop for UnreadableDirGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn preserves_argv_order_no_preset_and_shell_metacharacters_without_shell() {
    let ws = TempWorkspace::new("argv-order");
    let work_dir = ws.path().join("work");
    let log_path = ws.path().join("stub.log");
    fs::create_dir_all(&work_dir).unwrap();
    write_stub_config(
        &work_dir,
        &[("SIPHON_STUB_LOG", &log_path.to_string_lossy())],
    );

    let storage = TestStorage::new();
    let refresh = NoopRefreshSource;
    let downloader =
        YtDlpDownloader::with_binary(compiled_stub_binary()).with_working_dir(&work_dir);
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

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
            "mp3",
        ],
    )
    .unwrap();
    run_cli(&services, &["siphon", "preset", "set-default", "audio"]).unwrap();

    let tricky_arg = "say $(touch pwned); 'single' & \"double\" | %PATH%";
    let tricky_url = "https://www.bilibili.com/video/BV1order?p=2&ref=a;b";
    run_cli(
        &services,
        &[
            "siphon",
            "download",
            tricky_url,
            "--",
            "--ppa",
            tricky_arg,
            "--no-overwrites",
        ],
    )
    .unwrap();

    // Ensure no shell ran `touch pwned`.
    assert!(!work_dir.join("pwned").exists());

    // `--no-preset` omits preset args while keeping extra args in exact order.
    run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1nopreset",
            "--no-preset",
            "--",
            "-f",
            "worst",
        ],
    )
    .unwrap();

    // `--help` passthrough omits `--print-to-file` injection and avoids polluting stdout with a summary.
    let help_out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1help",
            "--no-preset",
            "--",
            "--help",
        ],
    )
    .unwrap();
    assert!(help_out.is_empty());

    let logs = read_stub_log(&log_path);
    assert_eq!(logs.len(), 3);
    assert_manifest_prefix_and_tail(
        &logs[0].args,
        &[
            "-x",
            "--audio-format",
            "mp3",
            "--ppa",
            tricky_arg,
            "--no-overwrites",
            "--",
            tricky_url,
        ],
    );
    assert_manifest_prefix_and_tail(
        &logs[1].args,
        &[
            "-f",
            "worst",
            "--",
            "https://www.bilibili.com/video/BV1nopreset",
        ],
    );
    assert_eq!(
        logs[2].args,
        vec!["--help", "--", "https://www.bilibili.com/video/BV1help",]
    );
}

#[test]
fn respects_explicit_cookies_and_ignores_saved_refresh_default() {
    let ws = TempWorkspace::new("cookies");
    let work_dir = ws.path().join("work");
    let log_path = ws.path().join("stub.log");
    fs::create_dir_all(&work_dir).unwrap();
    write_stub_config(
        &work_dir,
        &[("SIPHON_STUB_LOG", &log_path.to_string_lossy())],
    );

    let storage = TestStorage::new();
    let cookie_file = fixture_cookies_path();
    storage
        .set_default_refresh_cookies(Some(&CookieSource::File(cookie_file.clone())))
        .unwrap();
    let refresh = NoopRefreshSource;
    let downloader =
        YtDlpDownloader::with_binary(compiled_stub_binary()).with_working_dir(&work_dir);
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    // 1. Download without explicit cookie options must NOT inherit refresh default.
    run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1nocookie",
        ],
    )
    .unwrap();

    // 2. Explicit --cookies file is verified and forwarded.
    let cookie_str = cookie_file.to_string_lossy().into_owned();
    run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1filecookie",
            "--cookies",
            &cookie_str,
        ],
    )
    .unwrap();

    // 3. Explicit --cookies-from-browser is forwarded.
    run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1browser",
            "--cookies-from-browser",
            "firefox:default",
        ],
    )
    .unwrap();

    // 3b. Explicit invocation cookie flag overrides any cookie flag inside a preset.
    run_cli(
        &services,
        &[
            "siphon",
            "preset",
            "add",
            "with_chrome",
            "--",
            "--cookies-from-browser",
            "chrome",
        ],
    )
    .unwrap();
    run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1overridecookie",
            "--preset",
            "with_chrome",
            "--cookies",
            &cookie_str,
        ],
    )
    .unwrap();

    // 4. Missing cookie file fails before spawning child process.
    let missing_cookie = ws.path().join("does-not-exist.txt");
    let err = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1missingcookie",
            "--cookies",
            &missing_cookie.to_string_lossy(),
        ],
    )
    .unwrap_err();
    assert!(matches!(
        err,
        AppError::Download(DownloadError::CookieSourceInvalid { .. })
    ));

    let logs = read_stub_log(&log_path);
    assert_eq!(logs.len(), 4);
    assert_manifest_prefix_and_tail(
        &logs[0].args,
        &[
            "-f",
            "bv*+ba/b",
            "--",
            "https://www.bilibili.com/video/BV1nocookie",
        ],
    );
    assert_manifest_prefix_and_tail(
        &logs[1].args,
        &[
            "-f",
            "bv*+ba/b",
            "--cookies",
            &cookie_str,
            "--",
            "https://www.bilibili.com/video/BV1filecookie",
        ],
    );
    assert_manifest_prefix_and_tail(
        &logs[2].args,
        &[
            "-f",
            "bv*+ba/b",
            "--cookies-from-browser",
            "firefox:default",
            "--",
            "https://www.bilibili.com/video/BV1browser",
        ],
    );
    assert_manifest_prefix_and_tail(
        &logs[3].args,
        &[
            "--cookies-from-browser",
            "chrome",
            "--cookies",
            &cookie_str,
            "--",
            "https://www.bilibili.com/video/BV1overridecookie",
        ],
    );
}

#[test]
fn isolates_tracked_uploads_from_playlist_config_and_multi_target_args() {
    let ws = TempWorkspace::new("isolation");
    let work_dir = ws.path().join("work");
    let log_path = ws.path().join("stub.log");
    fs::create_dir_all(&work_dir).unwrap();
    write_stub_config(
        &work_dir,
        &[("SIPHON_STUB_LOG", &log_path.to_string_lossy())],
    );

    // Place an ambient yt-dlp.conf attempting to inject a second URL and playlist mode.
    fs::write(
        work_dir.join("yt-dlp.conf"),
        "--yes-playlist\nhttps://evil.example/ambient-config\n",
    )
    .unwrap();

    let batch_file = ws.path().join("batch.txt");
    fs::write(&batch_file, "https://example.com/batch-1\n").unwrap();
    let custom_conf = ws.path().join("extra.conf");
    fs::write(&custom_conf, "https://example.com/from-custom-conf\n").unwrap();

    let storage = TestStorage::new();
    let upload = storage.seed_pending_upload(
        "BV1playlist",
        "https://www.bilibili.com/video/BV1playlist?list=PL99",
        "Playlist Tracked Upload",
    );
    let refresh = NoopRefreshSource;
    let downloader =
        YtDlpDownloader::with_binary(compiled_stub_binary()).with_working_dir(&work_dir);
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    // Preset enabling playlists must still isolate `--upload <ID>` to 1 video and ignore yt-dlp.conf.
    run_cli(
        &services,
        &[
            "siphon",
            "preset",
            "add",
            "playlisty",
            "--",
            "--yes-playlist",
        ],
    )
    .unwrap();

    let out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "--upload",
            &upload.id.to_string(),
            "--preset",
            "playlisty",
            "--",
            "--referer",
            "https://www.bilibili.com/",
        ],
    )
    .unwrap();
    assert!(out.contains("marked as downloaded"));
    assert!(!work_dir.join("Stub Video [ambient-config].mp4").exists());
    assert!(
        !work_dir
            .join("Stub Video Part 2 [BV1playlist_2].mp4")
            .exists()
    );
    assert!(work_dir.join("Stub Video [BV1playlist].mp4").is_file());

    // Direct URL download is NOT restricted: --yes-playlist expands playlist, --batch-file,
    // --config-locations, --alias, and yt-dlp optparse prefix abbreviations all pass through.
    let direct_batch_file = ws.path().join("direct-batch.txt");
    fs::write(&direct_batch_file, "https://example.com/batch-abbrev\n").unwrap();
    let direct_conf = ws.path().join("direct-abbrev.conf");
    fs::write(&direct_conf, "https://example.com/conf-abbrev\n").unwrap();
    let direct_info = ws.path().join("direct-info.json");
    fs::write(&direct_info, "https://example.com/info-abbrev\n").unwrap();

    let direct_out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1directpl?list=PL1",
            "--preset",
            "playlisty",
            "--",
            "--batch-file",
            &batch_file.to_string_lossy(),
            "--config-locations",
            &custom_conf.to_string_lossy(),
            "--alias",
            "--aliased-url",
            "https://example.com/from-alias",
            "--aliased-url",
            &format!("--batch={}", direct_batch_file.display()),
            &format!("--config={}", direct_conf.display()),
            &format!("--load-info={}", direct_info.display()),
        ],
    )
    .unwrap();
    assert!(direct_out.contains("Downloaded direct URL"));
    assert!(
        work_dir
            .join("Stub Video Part 1 [BV1directpl_1].mp4")
            .is_file()
    );
    assert!(
        work_dir
            .join("Stub Video Part 2 [BV1directpl_2].mp4")
            .is_file()
    );
    assert!(work_dir.join("Stub Video [batch-1].mp4").is_file());
    assert!(work_dir.join("Stub Video [from-custom-conf].mp4").is_file());
    assert!(work_dir.join("Stub Video [from-alias].mp4").is_file());
    assert!(work_dir.join("Stub Video [batch-abbrev].mp4").is_file());
    assert!(work_dir.join("Stub Video [conf-abbrev].mp4").is_file());
    assert!(work_dir.join("Stub Video [info-abbrev].mp4").is_file());

    // Multi-target / target-displacing args, aliases, abbreviations, and unknown flags on a
    // pending upload are all rejected pre-spawn.
    let guarded = storage.seed_pending_upload(
        "BV1guarded",
        "https://www.bilibili.com/video/BV1guarded",
        "Guarded Upload",
    );
    let log_count_before = read_stub_log(&log_path).len();
    let batch_str = batch_file.to_string_lossy().into_owned();
    let batch_eq = format!("--batch={batch_str}");
    let conf_str = custom_conf.to_string_lossy().into_owned();
    let conf_eq = format!("--config={conf_str}");
    let load_info_eq = format!("--load-info={}", direct_info.display());
    let rejected_cases: Vec<Vec<&str>> = vec![
        vec!["--", "https://other.example/video"],
        vec!["https://other.example/video"],
        vec!["-f", "best", "https://other.example/video"],
        vec!["--batch-file", &batch_str],
        vec!["--batch", &batch_str],
        vec![&batch_eq],
        vec!["-a", &batch_str],
        vec!["-qa", &batch_str],
        vec!["--config-locations", &conf_str],
        vec!["--config-loc", &conf_str],
        vec![&conf_eq],
        vec!["--load-info-json", "other.info.json"],
        vec![&load_info_eq],
        vec!["--alias", "--x", "--batch-file batch.txt"],
        vec!["--ali", "--x", "https://other.example/video"],
        vec!["--unknown-flag"],
        vec!["--unknown-flag=val"],
        vec!["--simulate=yes"],
        vec!["-z"],
        vec!["-f"],
    ];

    for extra in rejected_cases {
        let mut argv = vec!["siphon", "download", "--upload", "2", "--no-preset", "--"];
        argv.extend(extra.iter().copied());
        let err = run_cli(&services, &argv).unwrap_err();
        assert!(
            matches!(
                err,
                AppError::Download(DownloadError::InvalidArguments { .. })
            ),
            "expected InvalidArguments for {extra:?}, got {err:?}"
        );
    }

    assert_eq!(
        storage.get_upload(guarded.id).unwrap().status,
        UploadStatus::Pending
    );
    // Stub process was never spawned for any of the rejected invocations.
    assert_eq!(read_stub_log(&log_path).len(), log_count_before);
}

#[test]
fn subtitle_only_downloads_confirm_subtitles_and_reject_missing_subs_or_thumbnails() {
    let ws = TempWorkspace::new("subtitles");
    let work_dir = ws.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();

    let storage = TestStorage::new();
    let upload_ok = storage.seed_pending_upload(
        "BV1subok",
        "https://www.bilibili.com/video/BV1subok",
        "Subtitle Video",
    );
    let upload_missing = storage.seed_pending_upload(
        "BV1subnone",
        "https://www.bilibili.com/video/BV1subnone",
        "No Subtitles Video",
    );
    let upload_thumb = storage.seed_pending_upload(
        "BV1thumbonly",
        "https://www.bilibili.com/video/BV1thumbonly",
        "Thumbnail Only",
    );
    let refresh = NoopRefreshSource;
    let downloader =
        YtDlpDownloader::with_binary(compiled_stub_binary()).with_working_dir(&work_dir);
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    // 1. Quiet subtitle-only download that saves `.en.srt` marks upload as Downloaded via
    // downloader-reported manifest paths.
    let out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "--upload",
            &upload_ok.id.to_string(),
            "--no-preset",
            "--",
            "-q",
            "--skip-download",
            "--write-subs",
            "--convert-subs",
            "srt",
        ],
    )
    .unwrap();
    assert!(out.contains("marked as downloaded"));
    assert!(work_dir.join("Stub Video [BV1subok].en.srt").is_file());
    assert_eq!(
        storage.get_upload(upload_ok.id).unwrap().status,
        UploadStatus::Downloaded
    );

    // 2. Subtitle-only download when no subtitle exists leaves upload Pending.
    write_stub_config(&work_dir, &[("SIPHON_STUB_MODE", "subtitle_missing")]);
    let missing_out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "--upload",
            &upload_missing.id.to_string(),
            "--no-preset",
            "--",
            "--skip-download",
            "--write-subs",
        ],
    )
    .unwrap();
    assert!(missing_out.contains("remains pending"));
    assert_eq!(
        storage.get_upload(upload_missing.id).unwrap().status,
        UploadStatus::Pending
    );

    // 3. `--skip-download --write-thumbnail` writes only a thumbnail (.jpg), not media or subtitles.
    write_stub_config(&work_dir, &[("SIPHON_STUB_MODE", "normal")]);
    let thumb_out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "--upload",
            &upload_thumb.id.to_string(),
            "--no-preset",
            "--",
            "--skip-download",
            "--write-thumbnail",
        ],
    )
    .unwrap();
    assert!(thumb_out.contains("remains pending"));
    assert!(work_dir.join("Stub Video [BV1thumbonly].jpg").is_file());
    assert_eq!(
        storage.get_upload(upload_thumb.id).unwrap().status,
        UploadStatus::Pending
    );
}

#[test]
fn simulation_leaves_upload_pending_even_when_old_file_exists_on_disk() {
    let ws = TempWorkspace::new("simulate");
    let work_dir = ws.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();

    // Pre-existing file on disk matching the upload ID must NOT fool a simulation run.
    fs::write(work_dir.join("Stub Video [BV1sim].mp4"), b"stale-file").unwrap();

    let storage = TestStorage::new();
    let upload = storage.seed_pending_upload(
        "BV1sim",
        "https://www.bilibili.com/video/BV1sim",
        "Simulated Video",
    );
    let refresh = NoopRefreshSource;
    let downloader =
        YtDlpDownloader::with_binary(compiled_stub_binary()).with_working_dir(&work_dir);
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    for flag in ["--simulate", "-s", "-sq", "--dump-json"] {
        let out = run_cli(
            &services,
            &[
                "siphon",
                "download",
                "--upload",
                &upload.id.to_string(),
                "--",
                flag,
            ],
        )
        .unwrap();
        if flag == "--dump-json" {
            assert!(out.is_empty());
        } else {
            assert!(out.contains("remains pending"));
        }
        assert_eq!(
            storage.get_upload(upload.id).unwrap().status,
            UploadStatus::Pending
        );
    }

    // `--skip-download` alone (no subtitles) also leaves upload pending despite pre-existing file.
    let skip_out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "--upload",
            &upload.id.to_string(),
            "--",
            "--skip-download",
        ],
    )
    .unwrap();
    assert!(skip_out.contains("remains pending"));
    assert_eq!(
        storage.get_upload(upload.id).unwrap().status,
        UploadStatus::Pending
    );

    // `--simulate --no-simulate` executes the real download and marks Downloaded.
    let real_out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "--upload",
            &upload.id.to_string(),
            "--",
            "--simulate",
            "--no-simulate",
        ],
    )
    .unwrap();
    assert!(real_out.contains("marked as downloaded"));
    assert_eq!(
        storage.get_upload(upload.id).unwrap().status,
        UploadStatus::Downloaded
    );
}

#[test]
fn mismatched_or_absent_artifacts_and_direct_url_keep_upload_pending() {
    let ws = TempWorkspace::new("artifact-matching");
    let work_dir = ws.path().join("work");
    let custom_dir = ws.path().join("custom-out");
    fs::create_dir_all(&work_dir).unwrap();

    let storage = TestStorage::new();
    let upload = storage.seed_pending_upload(
        "BV1target",
        "https://www.bilibili.com/video/BV1target",
        "Tracked Target",
    );
    let refresh = NoopRefreshSource;
    let downloader =
        YtDlpDownloader::with_binary(compiled_stub_binary()).with_working_dir(&work_dir);
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    // 1. Direct download of the exact pending upload URL leaves upload status unchanged.
    run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1target",
        ],
    )
    .unwrap();
    assert_eq!(
        storage.get_upload(upload.id).unwrap().status,
        UploadStatus::Pending
    );

    // 2. Stub creates file for a different upload ID (`BV999wrong`) -> rejected as non-matching.
    write_stub_config(&work_dir, &[("SIPHON_STUB_MODE", "wrong_id")]);
    let wrong_out = run_cli(
        &services,
        &["siphon", "download", "--upload", &upload.id.to_string()],
    )
    .unwrap();
    assert!(wrong_out.contains("remains pending"));
    assert_eq!(
        storage.get_upload(upload.id).unwrap().status,
        UploadStatus::Pending
    );

    // 3. Stub exits 0 silently without writing any artifact -> stays pending, EVEN when an
    // unrelated concurrent file matching a custom `-o` prefix exists on disk.
    fs::create_dir_all(&custom_dir).unwrap();
    fs::write(custom_dir.join("custom_clip_concurrent.mp4"), b"concurrent").unwrap();
    fs::write(custom_dir.join("custom_clip.mp4"), b"preexisting-custom").unwrap();
    write_stub_config(&work_dir, &[("SIPHON_STUB_MODE", "silent_no_artifact")]);
    let empty_out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "--upload",
            &upload.id.to_string(),
            "--",
            "-P",
            &custom_dir.to_string_lossy(),
            "-o",
            "custom_clip.%(ext)s",
        ],
    )
    .unwrap();
    assert!(empty_out.contains("remains pending"));
    assert_eq!(
        storage.get_upload(upload.id).unwrap().status,
        UploadStatus::Pending
    );

    // 4. Quiet download (`-q`) with `-P <dir>` and `-o custom_clip.%(ext)s` confirms artifact via
    // manifest attribution even with an unreadable subdirectory inside `work_dir`.
    #[cfg(unix)]
    let _locked_dir = UnreadableDirGuard::new(work_dir.join("locked-unrelated-subdir"));
    write_stub_config(&work_dir, &[("SIPHON_STUB_MODE", "normal")]);
    let quiet_out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "--upload",
            &upload.id.to_string(),
            "--",
            "-q",
            "-P",
            &custom_dir.to_string_lossy(),
            "-o",
            "custom_clip.%(ext)s",
        ],
    )
    .unwrap();
    assert!(quiet_out.contains("marked as downloaded"));
    assert!(custom_dir.join("custom_clip.mp4").is_file());
    assert_eq!(
        storage.get_upload(upload.id).unwrap().status,
        UploadStatus::Downloaded
    );

    // Direct URL download also succeeds when an unrelated unreadable subdirectory exists.
    let direct_locked = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1lockedok",
        ],
    )
    .unwrap();
    assert!(direct_locked.contains("Downloaded direct URL"));

    // 5. Existing matching file reported via `--no-overwrites` (`has already been downloaded`)
    // also confirms and marks a new pending upload as Downloaded.
    let existing_upload = storage.seed_pending_upload(
        "BV1already",
        "https://www.bilibili.com/video/BV1already",
        "Already Present",
    );
    fs::write(work_dir.join("Stub Video [BV1already].mp4"), b"existing").unwrap();
    let already_out = run_cli(
        &services,
        &[
            "siphon",
            "download",
            "--upload",
            &existing_upload.id.to_string(),
            "--",
            "--no-overwrites",
        ],
    )
    .unwrap();
    assert!(already_out.contains("marked as downloaded"));
    assert_eq!(
        storage.get_upload(existing_upload.id).unwrap().status,
        UploadStatus::Downloaded
    );
}

#[test]
fn process_failure_and_missing_binary_leave_upload_pending() {
    let ws = TempWorkspace::new("failures");
    let work_dir = ws.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    write_stub_config(
        &work_dir,
        &[
            ("SIPHON_STUB_MODE", "fail"),
            ("SIPHON_STUB_EXIT_CODE", "42"),
            (
                "SIPHON_STUB_STDERR",
                "ERROR: HTTP Error 412: Precondition Failed",
            ),
            ("SIPHON_STUB_CREATE_PART", "1"),
        ],
    );

    let storage = TestStorage::new();
    let upload = storage.seed_pending_upload(
        "BV1fail",
        "https://www.bilibili.com/video/BV1fail",
        "Failing Upload",
    );
    let refresh = NoopRefreshSource;
    let failing_downloader =
        YtDlpDownloader::with_binary(compiled_stub_binary()).with_working_dir(&work_dir);
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &failing_downloader,
    };

    let err = run_cli(
        &services,
        &["siphon", "download", "--upload", &upload.id.to_string()],
    )
    .unwrap_err();
    match err {
        AppError::Download(DownloadError::ProcessFailed { exit_code, stderr }) => {
            assert_eq!(exit_code, Some(42));
            assert!(stderr.contains("412"));
        }
        other => panic!("expected ProcessFailed, got {other:?}"),
    }
    assert!(work_dir.join("Stub Video [BV1fail].mp4.part").is_file());
    assert_eq!(
        storage.get_upload(upload.id).unwrap().status,
        UploadStatus::Pending
    );

    // Missing binary returns DownloaderUnavailable and leaves upload pending.
    let missing_bin = ws.path().join("nonexistent-yt-dlp-binary");
    let missing_downloader = YtDlpDownloader::with_binary(&missing_bin).with_working_dir(&work_dir);
    let req = siphon::download::DownloadRequest {
        target: DownloadTarget::PendingUpload {
            upload: upload.clone(),
        },
        preset_name: None,
        preset_args: Vec::new(),
        extra_args: Vec::new(),
        cookies: None,
    };
    let unavail_err = missing_downloader.download(&req).unwrap_err();
    assert!(matches!(
        unavail_err,
        DownloadError::DownloaderUnavailable { .. }
    ));
    assert_eq!(
        storage.get_upload(upload.id).unwrap().status,
        UploadStatus::Pending
    );
}

#[test]
fn child_cli_passthrough_harness() {
    let Ok(harness_dir_raw) = env::var("SIPHON_PASSTHROUGH_HARNESS_DIR") else {
        return;
    };
    let harness_dir = PathBuf::from(harness_dir_raw);
    let work_dir = harness_dir.join("work");
    let raw_argv = fs::read_to_string(harness_dir.join("argv.txt")).expect("read harness argv");
    let argv: Vec<String> = raw_argv.lines().map(str::to_owned).collect();

    let storage = TestStorage::new();
    let seed_path = harness_dir.join("seed_upload.txt");
    if let Ok(seed) = fs::read_to_string(&seed_path) {
        let parts: Vec<&str> = seed.trim().split('\t').collect();
        if parts.len() == 3 {
            let _ = storage.seed_pending_upload(parts[0], parts[1], parts[2]);
        }
    }

    let refresh = NoopRefreshSource;
    let downloader =
        YtDlpDownloader::with_binary(compiled_stub_binary()).with_working_dir(&work_dir);
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    let cli = Cli::try_parse_from(&argv).expect("harness argv must parse");
    println!("<<<SIPHON_BEGIN>>>");
    let _ = std::io::stdout().flush();
    match execute(cli, &services) {
        Ok(output) => {
            print!("{}", output.stdout);
            let _ = std::io::stdout().flush();
            println!("<<<SIPHON_END>>>");
            let _ = std::io::stdout().flush();
        }
        Err(err) => {
            println!("<<<SIPHON_END>>>");
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }

    if seed_path.is_file() {
        let upload = storage.get_upload(UploadId::new(1).unwrap()).unwrap();
        fs::write(
            harness_dir.join("upload_status.txt"),
            upload.status.as_str(),
        )
        .unwrap();
    }
}

fn invoke_harness_cli(
    ws: &TempWorkspace,
    work_dir: &Path,
    argv: &[&str],
    seed_upload: Option<(&str, &str, &str)>,
) -> (String, String, Option<UploadStatus>) {
    let mut joined = String::new();
    for arg in argv {
        joined.push_str(arg);
        joined.push('\n');
    }
    fs::write(ws.path().join("argv.txt"), joined).unwrap();
    let seed_file = ws.path().join("seed_upload.txt");
    let status_file = ws.path().join("upload_status.txt");
    let _ = fs::remove_file(&seed_file);
    let _ = fs::remove_file(&status_file);
    if let Some((id, url, title)) = seed_upload {
        fs::write(&seed_file, format!("{id}\t{url}\t{title}")).unwrap();
    }

    let current_exe = env::current_exe().expect("current test binary");
    let output = Command::new(current_exe)
        .args(["--exact", "child_cli_passthrough_harness", "--nocapture"])
        .env("SIPHON_PASSTHROUGH_HARNESS_DIR", ws.path())
        .output()
        .expect("spawn child harness");
    assert!(
        output.status.success(),
        "child harness failed for {argv:?}:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let full_stdout = String::from_utf8_lossy(&output.stdout);
    let after_begin = full_stdout
        .split("<<<SIPHON_BEGIN>>>\n")
        .nth(1)
        .or_else(|| full_stdout.split("<<<SIPHON_BEGIN>>>\r\n").nth(1))
        .expect("missing SIPHON_BEGIN marker");
    let captured_stdout = after_begin
        .split("<<<SIPHON_END>>>")
        .next()
        .expect("missing SIPHON_END marker")
        .to_owned();
    let captured_stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let final_status = fs::read_to_string(&status_file)
        .ok()
        .and_then(|s| s.trim().parse().ok());
    let _ = work_dir;
    (captured_stdout, captured_stderr, final_status)
}

#[test]
fn preserves_machine_readable_stdout_and_streams_normal_stdout_and_stderr_progress() {
    let ws = TempWorkspace::new("passthrough-stdio");
    let work_dir = ws.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();

    // 1. Direct passthrough with `-J` yields pure JSON on stdout with zero Siphon summary.
    write_stub_config(
        &work_dir,
        &[(
            "SIPHON_STUB_STDERR_PROGRESS",
            "WARNING: simulated extractor notice",
        )],
    );
    let (json_stdout, json_stderr, _) = invoke_harness_cli(
        &ws,
        &work_dir,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1json",
            "--",
            "-J",
        ],
        None,
    );
    let json_trimmed = json_stdout.trim();
    assert!(json_trimmed.starts_with('{') && json_trimmed.ends_with('}'));
    assert!(json_trimmed.contains("\"id\":\"BV1json\""));
    assert!(!json_stdout.contains("Finished direct URL"));
    assert!(!json_stdout.contains("Downloaded direct URL"));
    assert!(json_stderr.contains("WARNING: simulated extractor notice"));

    // 2. Direct passthrough with `--print`, `--list-formats`, and `--help` stays clean on stdout.
    let (print_stdout, _, _) = invoke_harness_cli(
        &ws,
        &work_dir,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1print",
            "--",
            "--print",
            "title",
        ],
        None,
    );
    assert_eq!(print_stdout.trim(), "Stub Video");

    let (formats_stdout, _, _) = invoke_harness_cli(
        &ws,
        &work_dir,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1formats",
            "--",
            "--list-formats",
        ],
        None,
    );
    assert!(formats_stdout.contains("Available formats for BV1formats"));
    assert!(!formats_stdout.contains("Finished direct URL"));

    let (help_stdout, _, _) = invoke_harness_cli(
        &ws,
        &work_dir,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1help",
            "--",
            "--help",
        ],
        None,
    );
    assert!(help_stdout.contains("yt-dlp stub"));
    assert!(!help_stdout.contains("Finished direct URL"));

    // 3. Normal direct download preserves yt-dlp stdout + stderr progress AND appends Siphon summary.
    write_stub_config(
        &work_dir,
        &[(
            "SIPHON_STUB_STDERR_PROGRESS",
            "[download] 100% progress on stderr",
        )],
    );
    let (direct_stdout, direct_stderr, _) = invoke_harness_cli(
        &ws,
        &work_dir,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1normaldirect",
        ],
        None,
    );
    assert!(direct_stdout.contains("[download] Destination:"));
    assert!(direct_stdout.contains("Downloaded direct URL"));
    assert!(direct_stderr.contains("[download] 100% progress on stderr"));

    // 4. Normal tracked upload streams yt-dlp stdout + stderr progress AND marks upload Downloaded.
    let (tracked_stdout, tracked_stderr, tracked_status) = invoke_harness_cli(
        &ws,
        &work_dir,
        &["siphon", "download", "--upload", "1"],
        Some((
            "BV1tracked",
            "https://www.bilibili.com/video/BV1tracked",
            "Tracked Stream",
        )),
    );
    assert!(tracked_stdout.contains("[download] Destination:"));
    assert!(tracked_stdout.contains("marked as downloaded"));
    assert!(tracked_stderr.contains("[download] 100% progress on stderr"));
    assert_eq!(tracked_status, Some(UploadStatus::Downloaded));

    // 5. Tracked upload with `--print after_move:filepath` marks Downloaded without Siphon stdout summary.
    let (tracked_print_stdout, _, tracked_print_status) = invoke_harness_cli(
        &ws,
        &work_dir,
        &[
            "siphon",
            "download",
            "--upload",
            "1",
            "--",
            "--print",
            "after_move:filepath",
        ],
        Some((
            "BV1trackedprint",
            "https://www.bilibili.com/video/BV1trackedprint",
            "Tracked Print",
        )),
    );
    assert!(
        tracked_print_stdout
            .trim()
            .ends_with("Stub Video [BV1trackedprint].mp4")
    );
    assert!(!tracked_print_stdout.contains("marked as downloaded"));
    assert_eq!(tracked_print_status, Some(UploadStatus::Downloaded));
}
