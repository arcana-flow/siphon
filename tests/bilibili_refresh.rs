use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use clap::Parser;
use rusqlite::Connection;
use siphon::app::{AppServices, execute};
use siphon::cli::Cli;
use siphon::domain::{
    CookieSource, Creator, CreatorId, DownloadPreset, Follow, FollowId, FollowedCreator, Platform,
    PresetName, PresetSummary, Timestamp, Upload, UploadId, UploadStatus, builtin_video_preset,
};
use siphon::download::{DownloadOutcome, DownloadRequest, Downloader};
use siphon::error::{AppError, DownloadError, RefreshError, StorageError};
use siphon::platform::bilibili::{BilibiliClientConfig, BilibiliRefreshSource};
use siphon::refresh::{RefreshCookiePolicy, RefreshSource, follow_creator, refresh_creators};
use siphon::storage::{
    CommitRefreshOutcome, CommitRefreshRequest, FollowCreatorRequest, RemovePresetOutcome, Storage,
};
use tempfile::TempDir;
use url::Url;

fn fixture_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(rel)
}

fn read_fixture(rel: &str) -> String {
    fs::read_to_string(fixture_path(rel)).expect("fixture file must be readable")
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

    fn follow_at(&self, mid: &str, display_name: &str, followed_at: Timestamp) -> FollowedCreator {
        self.follow_creator(FollowCreatorRequest {
            platform: Platform::Bilibili,
            platform_creator_id: mid.to_owned(),
            display_name: display_name.to_owned(),
            followed_at,
        })
        .unwrap()
    }

    fn all_uploads(&self) -> Vec<Upload> {
        self.state
            .lock()
            .unwrap()
            .uploads
            .values()
            .cloned()
            .collect()
    }
}

impl Storage for TestStorage {
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

        let follow_id = FollowId::new(guard.next_follow_id).unwrap();
        guard.next_follow_id += 1;
        let follow = Follow {
            id: follow_id,
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
        self.state
            .lock()
            .unwrap()
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

struct RecordingDownloader {
    requests: Mutex<Vec<DownloadRequest>>,
}

impl RecordingDownloader {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl Downloader for RecordingDownloader {
    fn download(&self, request: &DownloadRequest) -> Result<DownloadOutcome, DownloadError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(DownloadOutcome {
            succeeded: true,
            confirmed_artifacts: vec![PathBuf::from("artifact.mp4")],
        })
    }
}

#[derive(Debug, Clone)]
struct RecordedHttpRequest {
    path: String,
    query: HashMap<String, String>,
    cookie_header: Option<String>,
}

#[derive(Debug, Clone)]
struct MockHttpResponse {
    status: u16,
    body: String,
    location: Option<String>,
}

impl MockHttpResponse {
    fn ok_json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            location: None,
        }
    }

    fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            location: None,
        }
    }

    fn redirect(status: u16, location: impl Into<String>) -> Self {
        Self {
            status,
            body: String::new(),
            location: Some(location.into()),
        }
    }
}

type MockRouter = dyn Fn(&RecordedHttpRequest) -> MockHttpResponse + Send + Sync + 'static;

struct MockBilibiliServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    recorded: Arc<Mutex<Vec<RecordedHttpRequest>>>,
    handle: Option<JoinHandle<()>>,
}

impl MockBilibiliServer {
    fn start<F>(handler: F) -> Self
    where
        F: Fn(&RecordedHttpRequest) -> MockHttpResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let router: Arc<MockRouter> = Arc::new(handler);

        let stop_clone = Arc::clone(&stop);
        let recorded_clone = Arc::clone(&recorded);
        let handle = thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                if stop_clone.load(Ordering::SeqCst) {
                    break;
                }
                if let Some(req) = read_http_request(&mut stream) {
                    recorded_clone.lock().unwrap().push(req.clone());
                    let resp = router(&req);
                    let reason = match resp.status {
                        200 => "OK",
                        302 => "Found",
                        307 => "Temporary Redirect",
                        404 => "Not Found",
                        500 => "Internal Server Error",
                        502 => "Bad Gateway",
                        _ => "Status",
                    };
                    let location_hdr = resp
                        .location
                        .as_deref()
                        .map(|loc| format!("Location: {loc}\r\n"))
                        .unwrap_or_default();
                    let raw = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp.status,
                        reason,
                        location_hdr,
                        resp.body.len(),
                        resp.body
                    );
                    let _ = stream.write_all(raw.as_bytes());
                    let _ = stream.flush();
                }
            }
        });

        Self {
            addr,
            stop,
            recorded,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn client(&self, page_size: u32) -> BilibiliRefreshSource {
        BilibiliRefreshSource::with_config(
            BilibiliClientConfig::default()
                .with_api_base_url(self.base_url())
                .with_page_size(page_size)
                .with_guest_buvid(false),
        )
    }

    fn requests(&self) -> Vec<RecordedHttpRequest> {
        self.recorded.lock().unwrap().clone()
    }
}

impl Drop for MockBilibiliServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<RecordedHttpRequest> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let req_line = lines.next()?;
    let target = req_line.split_whitespace().nth(1)?;
    let full = format!("http://localhost{target}");
    let parsed = Url::parse(&full).ok()?;
    let query = parsed
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let mut cookie_header = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("cookie")
        {
            cookie_header = Some(value.trim().to_owned());
        }
    }
    Some(RecordedHttpRequest {
        path: parsed.path().to_owned(),
        query,
        cookie_header,
    })
}

fn run_cmd(services: &AppServices<'_>, argv: &[&str]) -> Result<String, AppError> {
    let cli = Cli::try_parse_from(argv).expect("test argv must parse");
    execute(cli, services).map(|out| out.stdout)
}

#[test]
fn creator_lookup_uses_sanitized_fixtures_and_maps_remote_errors() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_ok = read_fixture("bilibili/acc_info_946974.json");
    let acc_404 = read_fixture("bilibili/acc_info_not_found.json");
    let acc_352 = read_fixture("bilibili/acc_info_risk_control.json");

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/acc/info" => {
            assert!(req.query.contains_key("w_rid"));
            assert!(req.query.contains_key("wts"));
            match req.query.get("mid").map(String::as_str) {
                Some("946974") => MockHttpResponse::ok_json(&acc_ok),
                Some("404040") => MockHttpResponse::ok_json(&acc_404),
                Some("352352") => MockHttpResponse::ok_json(&acc_352),
                Some("500500") => MockHttpResponse::status(500, "upstream failure"),
                _ => MockHttpResponse::status(404, "not found"),
            }
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let storage = TestStorage::new();
    let refresh = server.client(30);
    let downloader = RecordingDownloader::new();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    let followed = follow_creator(
        &storage,
        &refresh,
        "https://space.bilibili.com/946974/video?spm_id_from=333",
        Timestamp::from_unix_seconds(1_700_000_000),
    )
    .unwrap();
    assert_eq!(followed.creator.platform, Platform::Bilibili);
    assert_eq!(followed.creator.platform_creator_id, "946974");
    assert_eq!(followed.creator.display_name, "Sanitized Creator Alpha");

    let not_found = run_cmd(
        &services,
        &[
            "siphon",
            "creator",
            "add",
            "https://space.bilibili.com/404040",
        ],
    )
    .unwrap_err();
    assert!(matches!(
        not_found,
        AppError::Refresh(RefreshError::CreatorNotFoundRemote { .. })
    ));

    let risk = run_cmd(
        &services,
        &[
            "siphon",
            "creator",
            "add",
            "https://space.bilibili.com/352352",
        ],
    )
    .unwrap_err();
    match risk {
        AppError::Refresh(RefreshError::PlatformResponse(msg)) => {
            assert!(msg.contains("-352"));
            assert!(msg.contains("cookies"));
        }
        other => panic!("expected PlatformResponse for -352, got {other:?}"),
    }

    let http_500 = run_cmd(
        &services,
        &[
            "siphon",
            "creator",
            "add",
            "https://space.bilibili.com/500500",
        ],
    )
    .unwrap_err();
    assert!(matches!(
        http_500,
        AppError::Refresh(RefreshError::PlatformResponse(_))
    ));

    let invalid_url = run_cmd(
        &services,
        &[
            "siphon",
            "creator",
            "add",
            "https://www.bilibili.com/video/BV1xx",
        ],
    )
    .unwrap_err();
    assert!(matches!(
        invalid_url,
        AppError::Refresh(RefreshError::InvalidCreatorUrl { .. })
    ));

    // Unreachable local port surfaces as RefreshError::Network.
    let dead_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_addr = dead_listener.local_addr().unwrap();
    drop(dead_listener);
    let dead_client = BilibiliRefreshSource::with_config(
        BilibiliClientConfig::default()
            .with_api_base_url(format!("http://{dead_addr}"))
            .with_guest_buvid(false),
    );
    let net_err = dead_client
        .resolve_creator("https://space.bilibili.com/946974", None)
        .unwrap_err();
    assert!(matches!(net_err, RefreshError::Network(_)));
}

#[test]
fn paginated_refresh_persists_all_pages_and_stops_at_initial_follow_boundary() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let p1 = read_fixture("bilibili/arc_search_creator1_p1.json");
    let p2 = read_fixture("bilibili/arc_search_creator1_p2.json");
    let p3 = read_fixture("bilibili/arc_search_creator1_p3.json");

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/arc/search" => {
            assert!(req.query.contains_key("w_rid"));
            match req.query.get("pn").map(String::as_str) {
                Some("1") => MockHttpResponse::ok_json(&p1),
                Some("2") => MockHttpResponse::ok_json(&p2),
                Some("3") => MockHttpResponse::ok_json(&p3),
                _ => MockHttpResponse::status(404, "unexpected page"),
            }
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });

    // Follow at 1_700_001_000 (which equals BV1san000002's created timestamp on page 2).
    // Must discover BV1san000005, BV1san000004, BV1san000003, exclude BV1san000002, and never fetch page 3.
    let storage = TestStorage::new();
    let followed = storage.follow_at(
        "946974",
        "Sanitized Creator Alpha",
        Timestamp::from_unix_seconds(1_700_001_000),
    );
    let refresh = server.client(2);
    let summary = refresh_creators(
        &storage,
        &refresh,
        Some(followed.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap();
    summary.check_failures().unwrap();

    let pending = storage.list_pending_uploads().unwrap();
    let ids: Vec<&str> = pending
        .iter()
        .map(|u| u.platform_upload_id.as_str())
        .collect();
    assert_eq!(ids, vec!["BV1san000005", "BV1san000004", "BV1san000003"]);
    assert_eq!(
        storage
            .get_followed_creator(followed.creator.id)
            .unwrap()
            .follow
            .refresh_checkpoint,
        Timestamp::from_unix_seconds(1_700_002_000)
    );

    let arc_pages: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/x/space/wbi/arc/search")
        .filter_map(|r| r.query.get("pn").cloned())
        .collect();
    assert_eq!(arc_pages, vec!["1", "2"]);

    // Following earlier at 1_700_000_000 fetches all 3 pages (5 uploads).
    let storage_full = TestStorage::new();
    let followed_full = storage_full.follow_at(
        "946974",
        "Sanitized Creator Alpha",
        Timestamp::from_unix_seconds(1_700_000_000),
    );
    refresh_creators(
        &storage_full,
        &refresh,
        Some(followed_full.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap()
    .check_failures()
    .unwrap();
    assert_eq!(storage_full.list_pending_uploads().unwrap().len(), 5);
}

#[test]
fn same_second_checkpoint_replay_preserves_existing_status_and_discovers_ties() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let tie_initial = read_fixture("bilibili/arc_search_tie_initial.json");
    let tie_p1 = read_fixture("bilibili/arc_search_tie_replay_p1.json");
    let tie_p2 = read_fixture("bilibili/arc_search_tie_replay_p2.json");

    let phase = Arc::new(Mutex::new(1u8));
    let phase_clone = Arc::clone(&phase);

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/arc/search" => {
            let current_phase = *phase_clone.lock().unwrap();
            let pn = req.query.get("pn").map(String::as_str);
            match (current_phase, pn) {
                (1, Some("1")) => MockHttpResponse::ok_json(&tie_initial),
                (2, Some("1")) => MockHttpResponse::ok_json(&tie_p1),
                (2, Some("2")) => MockHttpResponse::ok_json(&tie_p2),
                _ => MockHttpResponse::status(404, "unexpected phase/page"),
            }
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let storage = TestStorage::new();
    let followed = storage.follow_at(
        "946974",
        "Sanitized Creator Alpha",
        Timestamp::from_unix_seconds(1_700_002_000),
    );
    let refresh = server.client(2);

    // Phase 1: discovers BV1tieB00002 (1_700_003_000), excludes BV1old000001 (1_700_002_000 == followed_at).
    refresh_creators(
        &storage,
        &refresh,
        Some(followed.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap()
    .check_failures()
    .unwrap();

    let first_uploads = storage.list_pending_uploads().unwrap();
    assert_eq!(first_uploads.len(), 1);
    assert_eq!(first_uploads[0].platform_upload_id, "BV1tieB00002");

    // Mark BV1tieB00002 as Downloaded before phase 2 replay.
    storage.mark_upload_downloaded(first_uploads[0].id).unwrap();

    // Phase 2: page 1 has BV1newer0004 (1_700_004_000) & BV1tieC00003 (1_700_003_000),
    // page 2 has BV1tieB00002 (1_700_003_000) & BV1old000001 (1_700_002_000).
    *phase.lock().unwrap() = 2;
    let summary = refresh_creators(
        &storage,
        &refresh,
        Some(followed.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap();
    summary.check_failures().unwrap();

    let all = storage.all_uploads();
    assert_eq!(all.len(), 3);
    let by_bvid: HashMap<&str, &Upload> = all
        .iter()
        .map(|u| (u.platform_upload_id.as_str(), u))
        .collect();
    // Same-second newly visible upload on page 1 AND replayed upload on page 2:
    assert_eq!(by_bvid["BV1tieB00002"].status, UploadStatus::Downloaded);
    assert_eq!(
        by_bvid["BV1tieB00002"].title,
        "Upload At 1700003000 First (Updated Title)"
    );
    assert_eq!(by_bvid["BV1tieC00003"].status, UploadStatus::Pending);
    assert_eq!(by_bvid["BV1newer0004"].status, UploadStatus::Pending);
    assert!(!by_bvid.contains_key("BV1old000001"));
    assert_eq!(
        storage
            .get_followed_creator(followed.creator.id)
            .unwrap()
            .follow
            .refresh_checkpoint,
        Timestamp::from_unix_seconds(1_700_004_000)
    );
}

#[test]
fn partial_page_failure_preserves_checkpoint_and_status_while_other_creators_succeed() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let c1_p1 = read_fixture("bilibili/arc_search_creator1_p1.json");
    let c1_p2_rate_limited = read_fixture("bilibili/arc_search_rate_limited.json");
    let c2_p1 = read_fixture("bilibili/arc_search_creator2_p1.json");

    let fail_mode = Arc::new(Mutex::new("http_502"));
    let fail_mode_clone = Arc::clone(&fail_mode);

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/arc/search" => {
            let mid = req.query.get("mid").map(String::as_str);
            let pn = req.query.get("pn").map(String::as_str);
            match (mid, pn) {
                (Some("946974"), Some("1")) => MockHttpResponse::ok_json(&c1_p1),
                (Some("946974"), Some("2")) => match *fail_mode_clone.lock().unwrap() {
                    "http_502" => MockHttpResponse::status(502, "bad gateway"),
                    "rate_limit" => MockHttpResponse::ok_json(&c1_p2_rate_limited),
                    _ => MockHttpResponse::ok_json("{not valid json"),
                },
                (Some("888001"), Some("1")) => MockHttpResponse::ok_json(&c2_p1),
                _ => MockHttpResponse::status(404, "unexpected"),
            }
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let storage = TestStorage::new();
    let c1 = storage.follow_at(
        "946974",
        "Sanitized Creator Alpha",
        Timestamp::from_unix_seconds(1_700_000_000),
    );
    // Pre-commit an existing upload for Creator 1 and mark it Ignored, advancing checkpoint to 1_700_001_000.
    storage
        .commit_refresh(CommitRefreshRequest {
            creator_id: c1.creator.id,
            expected_follow_id: c1.follow.id,
            expected_follow_time: c1.follow.followed_at,
            expected_checkpoint: c1.follow.refresh_checkpoint,
            new_checkpoint: Timestamp::from_unix_seconds(1_700_001_000),
            discovered_uploads: vec![siphon::domain::DiscoveredUpload {
                platform: Platform::Bilibili,
                platform_upload_id: "BV1existing0".to_owned(),
                url: "https://www.bilibili.com/video/BV1existing0".to_owned(),
                title: "Pre-existing Ignored Upload".to_owned(),
                published_at: Timestamp::from_unix_seconds(1_700_001_000),
            }],
        })
        .unwrap();
    let existing_id = storage.list_pending_uploads().unwrap()[0].id;
    storage.mark_upload_ignored(existing_id).unwrap();

    let c2 = storage.follow_at(
        "888001",
        "Sanitized Creator Beta",
        Timestamp::from_unix_seconds(1_700_000_000),
    );

    let refresh = server.client(2);
    let downloader = RecordingDownloader::new();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    // Page 1 for creator 1 succeeds, page 2 fails with HTTP 502; creator 2 succeeds.
    let err = run_cmd(&services, &["siphon", "refresh"]).unwrap_err();
    match err {
        AppError::RefreshPartialFailure {
            succeeded,
            failures,
        } => {
            assert_eq!(succeeded, 1);
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].creator_id, c1.creator.id);
        }
        other => panic!("expected partial refresh failure, got {other:?}"),
    }

    // Creator 1 checkpoint stayed at 1_700_001_000, pre-existing upload stayed Ignored,
    // and NONE of creator 1's page-1 uploads were persisted!
    assert_eq!(
        storage
            .get_followed_creator(c1.creator.id)
            .unwrap()
            .follow
            .refresh_checkpoint,
        Timestamp::from_unix_seconds(1_700_001_000)
    );
    assert_eq!(
        storage.get_upload(existing_id).unwrap().status,
        UploadStatus::Ignored
    );
    let c1_uploads: Vec<Upload> = storage
        .all_uploads()
        .into_iter()
        .filter(|u| u.creator_id == c1.creator.id)
        .collect();
    assert_eq!(c1_uploads.len(), 1);

    // Creator 2 committed its upload and advanced its checkpoint independently.
    assert_eq!(
        storage
            .get_followed_creator(c2.creator.id)
            .unwrap()
            .follow
            .refresh_checkpoint,
        Timestamp::from_unix_seconds(1_700_001_800)
    );
    let pending = storage.list_pending_uploads().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].platform_upload_id, "BV1beta00001");

    // Verify Bilibili code -799 and malformed JSON on page 2 also abort atomically.
    for mode in ["rate_limit", "bad_json"] {
        *fail_mode.lock().unwrap() = mode;
        let single_err = run_cmd(
            &services,
            &["siphon", "refresh", &c1.creator.id.to_string()],
        )
        .unwrap_err();
        assert!(matches!(
            single_err,
            AppError::RefreshPartialFailure { succeeded: 0, .. }
        ));
        assert_eq!(
            storage
                .get_followed_creator(c1.creator.id)
                .unwrap()
                .follow
                .refresh_checkpoint,
            Timestamp::from_unix_seconds(1_700_001_000)
        );
        assert_eq!(storage.list_pending_uploads().unwrap().len(), 1);
    }
}

#[test]
fn netscape_cookie_file_authenticates_refresh_and_does_not_leak_to_download() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let c2_p1 = read_fixture("bilibili/arc_search_creator2_p1.json");
    let cookie_file = fixture_path("cookies/bilibili_netscape.txt");
    let cookie_path_str = cookie_file.to_str().unwrap();

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/arc/search" => MockHttpResponse::ok_json(&c2_p1),
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let storage = TestStorage::new();
    let c = storage.follow_at(
        "888001",
        "Sanitized Creator Beta",
        Timestamp::from_unix_seconds(1_700_000_000),
    );
    let refresh = server.client(30);
    let downloader = RecordingDownloader::new();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    run_cmd(
        &services,
        &[
            "siphon",
            "refresh",
            &c.creator.id.to_string(),
            "--cookies",
            cookie_path_str,
            "--save-cookies",
        ],
    )
    .unwrap();

    let reqs = server.requests();
    let arc_req = reqs
        .iter()
        .rfind(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap();
    let sent_cookie = arc_req.cookie_header.as_deref().unwrap();
    assert!(sent_cookie.contains("SESSDATA=sanitized-sessdata-token"));
    assert!(sent_cookie.contains("bili_jct=sanitized-bili-jct"));
    assert!(sent_cookie.contains("buvid3=SAN-BUVID3-0001"));
    assert!(!sent_cookie.contains("expired_cookie"));
    assert!(!sent_cookie.contains("foreign_cookie"));

    // Subsequent refresh uses saved default Netscape file; --no-cookies sends no Cookie header.
    let before_count = server.requests().len();
    run_cmd(&services, &["siphon", "refresh", "--no-cookies"]).unwrap();
    let after_reqs = server.requests();
    let no_cookie_req = after_reqs[before_count..]
        .iter()
        .find(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap();
    assert_eq!(no_cookie_req.cookie_header, None);

    // Download NEVER inherits refresh's saved default cookie source.
    run_cmd(
        &services,
        &[
            "siphon",
            "download",
            "https://www.bilibili.com/video/BV1beta00001",
        ],
    )
    .unwrap();
    assert_eq!(downloader.requests.lock().unwrap()[0].cookies, None);

    // Missing Netscape cookie file surfaces as CookieSourceInvalid without touching checkpoint.
    let missing = cookie_file.with_file_name("does_not_exist.txt");
    let err = run_cmd(
        &services,
        &[
            "siphon",
            "refresh",
            &c.creator.id.to_string(),
            "--cookies",
            missing.to_str().unwrap(),
        ],
    )
    .unwrap_err();
    match err {
        AppError::RefreshPartialFailure { failures, .. } => {
            assert!(failures[0].error.contains("cookie error"));
        }
        other => panic!("expected CookieSourceInvalid failure, got {other:?}"),
    }
}

fn create_firefox_profile_fixture(dir: &std::path::Path) {
    fs::copy(
        fixture_path("cookies/containers.json"),
        dir.join("containers.json"),
    )
    .unwrap();

    let conn = Connection::open(dir.join("cookies.sqlite")).unwrap();
    conn.execute_batch(
        "PRAGMA user_version = 15;
         CREATE TABLE moz_cookies (
             id INTEGER PRIMARY KEY,
             originAttributes TEXT NOT NULL DEFAULT '',
             name TEXT,
             value TEXT,
             host TEXT,
             path TEXT,
             expiry INTEGER,
             lastAccessed INTEGER,
             creationTime INTEGER,
             isSecure INTEGER,
             isHttpOnly INTEGER,
             inBrowserElement INTEGER DEFAULT 0,
             sameSite INTEGER DEFAULT 0
         );
         INSERT INTO moz_cookies (originAttributes, name, value, host, path, expiry, lastAccessed, creationTime, isSecure, isHttpOnly)
         VALUES
           ('', 'SESSDATA', 'ff-default-sessdata', '.bilibili.com', '/', 4102444800, 1700000000000000, 1700000000000000, 1, 1),
           ('', 'bili_jct', 'ff-default-jct', '.bilibili.com', '/', 4102444800, 1700000000000000, 1700000000000000, 1, 0),
           ('', 'expired_ff', 'stale', '.bilibili.com', '/', 1000000000, 1700000000000000, 1700000000000000, 1, 0),
           ('', 'foreign_ff', 'nope', '.example.com', '/', 4102444800, 1700000000000000, 1700000000000000, 1, 0),
           ('^userContextId=1', 'SESSDATA', 'ff-personal-sessdata', '.bilibili.com', '/', 4102444800, 1700000000000000, 1700000000000000, 1, 1),
           ('^userContextId=2', 'SESSDATA', 'ff-work-sessdata', '.bilibili.com', '/', 4102444800, 1700000000000000, 1700000000000000, 1, 1);",
    )
    .unwrap();
}

fn create_chromium_plaintext_profile_fixture(dir: &std::path::Path) {
    let conn = Connection::open(dir.join("Cookies")).unwrap();
    conn.execute_batch(
        "CREATE TABLE meta (key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR);
         INSERT INTO meta (key, value) VALUES ('version', '24');
         CREATE TABLE cookies (
             creation_utc INTEGER NOT NULL,
             host_key TEXT NOT NULL,
             top_frame_site_key TEXT NOT NULL DEFAULT '',
             name TEXT NOT NULL,
             value TEXT NOT NULL,
             encrypted_value BLOB NOT NULL DEFAULT X'',
             path TEXT NOT NULL,
             expires_utc INTEGER NOT NULL,
             is_secure INTEGER NOT NULL,
             is_httponly INTEGER NOT NULL,
             last_access_utc INTEGER NOT NULL,
             has_expires INTEGER NOT NULL DEFAULT 1,
             is_persistent INTEGER NOT NULL DEFAULT 1,
             priority INTEGER NOT NULL DEFAULT 1,
             samesite INTEGER NOT NULL DEFAULT -1,
             source_scheme INTEGER NOT NULL DEFAULT 2,
             source_port INTEGER NOT NULL DEFAULT 443,
             has_cross_site_ancestor INTEGER NOT NULL DEFAULT 0
         );
         INSERT INTO cookies (creation_utc, host_key, name, value, encrypted_value, path, expires_utc, is_secure, is_httponly, last_access_utc)
         VALUES
           (13344000000000000, '.bilibili.com', 'SESSDATA', 'chrome-plain-sessdata', X'', '/', 15746918400000000, 1, 1, 13344000000000000),
           (13344000000000000, '.example.com', 'foreign_chrome', 'nope', X'', '/', 15746918400000000, 1, 0, 13344000000000000);",
    )
    .unwrap();
}

#[test]
fn browser_profile_sqlite_fixtures_support_firefox_containers_and_chromium() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let c2_p1 = read_fixture("bilibili/arc_search_creator2_p1.json");

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/arc/search" => MockHttpResponse::ok_json(&c2_p1),
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let ff_dir = TempDir::new().unwrap();
    create_firefox_profile_fixture(ff_dir.path());
    let chrome_dir = TempDir::new().unwrap();
    create_chromium_plaintext_profile_fixture(chrome_dir.path());

    let storage = TestStorage::new();
    let c = storage.follow_at(
        "888001",
        "Sanitized Creator Beta",
        Timestamp::from_unix_seconds(1_700_000_000),
    );
    let refresh = server.client(30);
    let downloader = RecordingDownloader::new();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh,
        downloader: &downloader,
    };

    // 1. Firefox default container (excludes Personal/Work/expired/foreign cookies).
    let ff_default_sel = format!("firefox:{}", ff_dir.path().display());
    run_cmd(
        &services,
        &[
            "siphon",
            "refresh",
            &c.creator.id.to_string(),
            "--cookies-from-browser",
            &ff_default_sel,
        ],
    )
    .unwrap();
    let sent_default = server
        .requests()
        .into_iter()
        .rfind(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap()
        .cookie_header
        .unwrap();
    assert!(sent_default.contains("SESSDATA=ff-default-sessdata"));
    assert!(sent_default.contains("bili_jct=ff-default-jct"));
    assert!(!sent_default.contains("ff-personal-sessdata"));
    assert!(!sent_default.contains("expired_ff"));
    assert!(!sent_default.contains("foreign_ff"));

    // 2. Firefox Personal container via `::Personal`.
    let ff_personal_sel = format!("firefox:{}::Personal", ff_dir.path().display());
    run_cmd(
        &services,
        &[
            "siphon",
            "refresh",
            &c.creator.id.to_string(),
            "--cookies-from-browser",
            &ff_personal_sel,
        ],
    )
    .unwrap();
    let sent_personal = server
        .requests()
        .into_iter()
        .rfind(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap()
        .cookie_header
        .unwrap();
    assert_eq!(sent_personal, "SESSDATA=ff-personal-sessdata");

    // 3. Firefox Work container via `::Work`.
    let ff_work_sel = format!("firefox:{}::Work", ff_dir.path().display());
    run_cmd(
        &services,
        &[
            "siphon",
            "refresh",
            &c.creator.id.to_string(),
            "--cookies-from-browser",
            &ff_work_sel,
        ],
    )
    .unwrap();
    let sent_work = server
        .requests()
        .into_iter()
        .rfind(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap()
        .cookie_header
        .unwrap();
    assert_eq!(sent_work, "SESSDATA=ff-work-sessdata");

    // 4. Firefox explicit default container via `::none`.
    let ff_none_sel = format!("firefox:{}::none", ff_dir.path().display());
    run_cmd(
        &services,
        &[
            "siphon",
            "refresh",
            &c.creator.id.to_string(),
            "--cookies-from-browser",
            &ff_none_sel,
        ],
    )
    .unwrap();
    let sent_none = server
        .requests()
        .into_iter()
        .rfind(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap()
        .cookie_header
        .unwrap();
    assert!(sent_none.contains("SESSDATA=ff-default-sessdata"));
    assert!(!sent_none.contains("ff-personal-sessdata"));

    // 5. Unknown Firefox container surfaces as CookieSourceInvalid.
    let ff_missing_sel = format!("firefox:{}::Missing", ff_dir.path().display());
    let missing_err = run_cmd(
        &services,
        &[
            "siphon",
            "refresh",
            &c.creator.id.to_string(),
            "--cookies-from-browser",
            &ff_missing_sel,
        ],
    )
    .unwrap_err();
    assert!(matches!(
        missing_err,
        AppError::RefreshPartialFailure { .. }
    ));

    // 6. Chromium plaintext `Cookies` SQLite fixture.
    let chrome_sel = format!("chrome:{}", chrome_dir.path().display());
    run_cmd(
        &services,
        &[
            "siphon",
            "refresh",
            &c.creator.id.to_string(),
            "--cookies-from-browser",
            &chrome_sel,
        ],
    )
    .unwrap();
    let sent_chrome = server
        .requests()
        .into_iter()
        .rfind(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap()
        .cookie_header
        .unwrap();
    assert_eq!(sent_chrome, "SESSDATA=chrome-plain-sessdata");
}

#[test]
fn unfollow_and_refollow_with_bilibili_fixture_preserves_uploads_and_resets_window() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let p1 = read_fixture("bilibili/arc_search_creator1_p1.json");
    let p2 = read_fixture("bilibili/arc_search_creator1_p2.json");

    let phase = Arc::new(Mutex::new(1u8));
    let phase_clone = Arc::clone(&phase);

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/arc/search" => {
            let current = *phase_clone.lock().unwrap();
            let pn = req.query.get("pn").map(String::as_str);
            match (current, pn) {
                (1, Some("1")) => MockHttpResponse::ok_json(&p2),
                (2, Some("1")) => MockHttpResponse::ok_json(&p1),
                (2, Some("2")) => MockHttpResponse::ok_json(&p2),
                _ => MockHttpResponse::status(404, "unexpected"),
            }
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let storage = TestStorage::new();
    let refresh = server.client(2);

    // Follow at 1_700_001_000 -> discovers BV1san000003 (1_700_001_200), excludes BV1san000002.
    let first = storage.follow_at(
        "946974",
        "Sanitized Creator Alpha",
        Timestamp::from_unix_seconds(1_700_001_000),
    );
    refresh_creators(
        &storage,
        &refresh,
        Some(first.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap()
    .check_failures()
    .unwrap();
    let upload3_id = storage.list_pending_uploads().unwrap()[0].id;
    storage.mark_upload_downloaded(upload3_id).unwrap();

    // Unfollow preserves uploads; re-following at 1_700_001_500 resets boundary so 1_700_001_500 is excluded
    // and only BV1san000005 (1_700_002_000) is discovered.
    storage.unfollow_creator(first.creator.id).unwrap();
    assert_eq!(
        storage.get_upload(upload3_id).unwrap().status,
        UploadStatus::Downloaded
    );

    *phase.lock().unwrap() = 2;
    let second = storage.follow_at(
        "946974",
        "Sanitized Creator Alpha",
        Timestamp::from_unix_seconds(1_700_001_500),
    );
    assert_eq!(second.creator.id, first.creator.id);

    refresh_creators(
        &storage,
        &refresh,
        Some(second.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap()
    .check_failures()
    .unwrap();

    let pending = storage.list_pending_uploads().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].platform_upload_id, "BV1san000005");
    assert_eq!(
        storage.get_upload(upload3_id).unwrap().status,
        UploadStatus::Downloaded
    );
}

#[test]
fn guest_buvid3_spi_and_wbi_cache_rotation_recovery() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_ok = read_fixture("bilibili/acc_info_946974.json");
    let c2_p1 = read_fixture("bilibili/arc_search_creator2_p1.json");
    let spi_json =
        r#"{"code":0,"message":"0","data":{"b_3":"GUEST-BUVID3-999","b_4":"GUEST-BUVID4"}}"#;

    let acc_calls = Arc::new(Mutex::new(0u32));
    let acc_calls_clone = Arc::clone(&acc_calls);

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/frontend/finger/spi" => MockHttpResponse::ok_json(spi_json),
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/acc/info" => {
            let mut count = acc_calls_clone.lock().unwrap();
            *count += 1;
            MockHttpResponse::ok_json(&acc_ok)
        }
        "/x/space/wbi/arc/search" => {
            let mut count = acc_calls_clone.lock().unwrap();
            *count += 1;
            // First arc/search call after WBI keys were cached triggers -352;
            // client should invalidate WBI cache, refetch /nav, and succeed on retry.
            if *count == 2 {
                MockHttpResponse::ok_json(r#"{"code":-352,"message":"stale wbi"}"#)
            } else {
                MockHttpResponse::ok_json(&c2_p1)
            }
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let client = BilibiliRefreshSource::with_config(
        BilibiliClientConfig::default()
            .with_api_base_url(server.base_url())
            .with_page_size(30)
            .with_guest_buvid(true),
    );
    let storage = TestStorage::new();
    let followed = follow_creator(
        &storage,
        &client,
        "https://space.bilibili.com/946974",
        Timestamp::from_unix_seconds(1_700_000_000),
    )
    .unwrap();
    assert_eq!(followed.creator.display_name, "Sanitized Creator Alpha");

    // Subsequent refresh uses cached WBI key, hits -352 once, refreshes nav, and succeeds.
    refresh_creators(
        &storage,
        &client,
        Some(followed.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap()
    .check_failures()
    .unwrap();
    assert_eq!(storage.list_pending_uploads().unwrap().len(), 1);

    let reqs = server.requests();
    assert!(reqs.iter().any(|r| r.path == "/x/frontend/finger/spi"));
    let nav_count = reqs
        .iter()
        .filter(|r| r.path == "/x/web-interface/nav")
        .count();
    assert_eq!(nav_count, 2);
    let last_arc = reqs
        .iter()
        .rfind(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap();
    assert_eq!(
        last_arc.cookie_header.as_deref(),
        Some("buvid3=GUEST-BUVID3-999")
    );
}

#[test]
fn pagination_cap_exhaustion_fails_without_committing_uploads_or_checkpoint() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let p1 = read_fixture("bilibili/arc_search_creator1_p1.json");
    let p2 = read_fixture("bilibili/arc_search_creator1_p2.json");
    let p3 = read_fixture("bilibili/arc_search_creator1_p3.json");

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/arc/search" => match req.query.get("pn").map(String::as_str) {
            Some("1") => MockHttpResponse::ok_json(&p1),
            Some("2") => MockHttpResponse::ok_json(&p2),
            Some("3") => MockHttpResponse::ok_json(&p3),
            _ => MockHttpResponse::status(404, "unexpected page"),
        },
        _ => MockHttpResponse::status(404, "unknown"),
    });

    // Creator followed at 1_700_000_000 needs all 3 pages (`count = 5`),
    // so max_pages = 2 cannot reach boundary or total and must fail atomically.
    let storage = TestStorage::new();
    let initial_cp = Timestamp::from_unix_seconds(1_700_000_000);
    let followed = storage.follow_at("946974", "Sanitized Creator Alpha", initial_cp);
    let client = BilibiliRefreshSource::with_config(
        BilibiliClientConfig::default()
            .with_api_base_url(server.base_url())
            .with_page_size(2)
            .with_max_pages(2)
            .with_guest_buvid(false),
    );

    let summary = refresh_creators(
        &storage,
        &client,
        Some(followed.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap();
    let err = summary.check_failures().unwrap_err();
    match err {
        AppError::RefreshPartialFailure {
            succeeded,
            failures,
        } => {
            assert_eq!(succeeded, 0);
            assert_eq!(failures.len(), 1);
            assert!(failures[0].error.contains("exceeded limit of 2 page(s)"));
        }
        other => panic!("expected RefreshPartialFailure, got {other:?}"),
    }

    assert!(storage.all_uploads().is_empty());
    assert_eq!(
        storage
            .get_followed_creator(followed.creator.id)
            .unwrap()
            .follow
            .refresh_checkpoint,
        initial_cp
    );
}

#[test]
fn duplicate_page_handling_recovers_single_shift_and_fails_on_stuck_pagination() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let p1 = read_fixture("bilibili/arc_search_creator1_p1.json");
    let p2_boundary = read_fixture("bilibili/arc_search_creator1_p2.json");

    // Case 1: Single duplicate page due to concurrent upload shift before pn=2 (`pn=2` repeats `p1`),
    // followed by `pn=3` which crosses the boundary at 1_700_001_000. Must still discover pn=3!
    let p1_shift = p1.clone();
    let p2_shift = p2_boundary.clone();
    let nav_shift = nav_json.clone();
    let shifted_server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_shift),
        "/x/space/wbi/arc/search" => match req.query.get("pn").map(String::as_str) {
            Some("1") | Some("2") => MockHttpResponse::ok_json(&p1_shift),
            Some("3") => MockHttpResponse::ok_json(&p2_shift),
            _ => MockHttpResponse::status(404, "unexpected page"),
        },
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let storage_ok = TestStorage::new();
    let c_ok = storage_ok.follow_at(
        "946974",
        "Sanitized Creator Alpha",
        Timestamp::from_unix_seconds(1_700_001_000),
    );
    let client_ok = shifted_server.client(2);
    refresh_creators(
        &storage_ok,
        &client_ok,
        Some(c_ok.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap()
    .check_failures()
    .unwrap();
    let ok_ids: Vec<String> = storage_ok
        .list_pending_uploads()
        .unwrap()
        .into_iter()
        .map(|u| u.platform_upload_id)
        .collect();
    assert_eq!(ok_ids, vec!["BV1san000005", "BV1san000004", "BV1san000003"]);

    // Case 2: Stuck server keeps repeating page 1 indefinitely (`count = 20`) without crossing boundary.
    // Must fail rather than returning Ok and losing remaining pages.
    let p1_stuck = p1.replace("\"count\": 5", "\"count\": 20");
    let stuck_server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/arc/search" => MockHttpResponse::ok_json(&p1_stuck),
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let storage_stuck = TestStorage::new();
    let initial_cp = Timestamp::from_unix_seconds(1_700_000_000);
    let c_stuck = storage_stuck.follow_at("946974", "Sanitized Creator Alpha", initial_cp);
    let client_stuck = BilibiliRefreshSource::with_config(
        BilibiliClientConfig::default()
            .with_api_base_url(stuck_server.base_url())
            .with_page_size(2)
            .with_max_pages(20)
            .with_guest_buvid(false),
    );

    let err = refresh_creators(
        &storage_stuck,
        &client_stuck,
        Some(c_stuck.creator.id),
        RefreshCookiePolicy::UseSavedDefault,
    )
    .unwrap()
    .check_failures()
    .unwrap_err();
    match err {
        AppError::RefreshPartialFailure {
            succeeded,
            failures,
        } => {
            assert_eq!(succeeded, 0);
            assert!(failures[0].error.contains("pagination stalled"));
        }
        other => panic!("expected stalled pagination error, got {other:?}"),
    }
    assert!(storage_stuck.all_uploads().is_empty());
    assert_eq!(
        storage_stuck
            .get_followed_creator(c_stuck.creator.id)
            .unwrap()
            .follow
            .refresh_checkpoint,
        initial_cp
    );
}

#[test]
fn code_zero_without_list_fails_as_malformed_response_and_preserves_checkpoint() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let mode = Arc::new(Mutex::new("missing_list"));
    let mode_clone = Arc::clone(&mode);

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/arc/search" => match *mode_clone.lock().unwrap() {
            "missing_list" => {
                MockHttpResponse::ok_json(r#"{"code":0,"message":"0","data":{"page":{"count":0}}}"#)
            }
            "null_list" => MockHttpResponse::ok_json(
                r#"{"code":0,"message":"0","data":{"list":null,"page":{"count":0}}}"#,
            ),
            _ => MockHttpResponse::ok_json(
                r#"{"code":0,"message":"0","data":{"list":{},"page":{"count":0}}}"#,
            ),
        },
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let storage = TestStorage::new();
    let initial_cp = Timestamp::from_unix_seconds(1_700_000_000);
    let followed = storage.follow_at("946974", "Sanitized Creator Alpha", initial_cp);
    let client = server.client(30);

    for case in ["missing_list", "null_list", "missing_vlist"] {
        *mode.lock().unwrap() = case;
        let err = refresh_creators(
            &storage,
            &client,
            Some(followed.creator.id),
            RefreshCookiePolicy::UseSavedDefault,
        )
        .unwrap()
        .check_failures()
        .unwrap_err();
        assert!(matches!(
            err,
            AppError::RefreshPartialFailure { succeeded: 0, .. }
        ));
        assert!(storage.all_uploads().is_empty());
        assert_eq!(
            storage
                .get_followed_creator(followed.creator.id)
                .unwrap()
                .follow
                .refresh_checkpoint,
            initial_cp
        );
    }
}

#[test]
fn cross_origin_redirects_are_rejected_without_leaking_cookies_or_headers() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_ok = read_fixture("bilibili/acc_info_946974.json");
    let cookie_file = fixture_path("cookies/bilibili_netscape.txt");
    let cookie_source = CookieSource::File(cookie_file);

    let sink_server = MockBilibiliServer::start(|_req| MockHttpResponse::ok_json("{}"));
    let redirect_target = format!(
        "{}/steal?token=location-header-sentinel",
        sink_server.base_url()
    );
    let redirect_target_clone = redirect_target.clone();

    let mode = Arc::new(Mutex::new("arc_302"));
    let mode_clone = Arc::clone(&mode);
    let api_server = MockBilibiliServer::start(move |req| {
        let current_mode = *mode_clone.lock().unwrap();
        match (current_mode, req.path.as_str()) {
            ("nav_302", "/x/web-interface/nav") => {
                MockHttpResponse::redirect(302, &redirect_target_clone)
            }
            ("acc_307", "/x/web-interface/nav") => MockHttpResponse::ok_json(&nav_json),
            ("acc_307", "/x/space/wbi/acc/info") => {
                MockHttpResponse::redirect(307, &redirect_target_clone)
            }
            ("arc_302", "/x/web-interface/nav") => MockHttpResponse::ok_json(&nav_json),
            ("arc_302", "/x/space/wbi/acc/info") => MockHttpResponse::ok_json(&acc_ok),
            ("arc_302", "/x/space/wbi/arc/search") => {
                MockHttpResponse::redirect(302, &redirect_target_clone)
            }
            ("spi_302", "/x/frontend/finger/spi") => {
                MockHttpResponse::redirect(302, &redirect_target_clone)
            }
            ("spi_302", "/x/web-interface/nav") => MockHttpResponse::ok_json(&nav_json),
            ("spi_302", "/x/space/wbi/acc/info") => MockHttpResponse::ok_json(&acc_ok),
            _ => MockHttpResponse::status(404, "unexpected"),
        }
    });

    let client = BilibiliRefreshSource::with_config(
        BilibiliClientConfig::default()
            .with_api_base_url(api_server.base_url())
            .with_page_size(30)
            .with_guest_buvid(true),
    );

    let assert_no_header_leaks = |msg: &str| {
        for secret in [
            "sanitized-sessdata-token",
            "sanitized-bili-jct",
            "SAN-BUVID3-0001",
            "SESSDATA",
            "location-header-sentinel",
            sink_server.base_url().as_str(),
        ] {
            assert!(
                !msg.contains(secret),
                "error message unexpectedly exposed '{secret}': {msg}"
            );
        }
    };

    // 1. `/x/web-interface/nav` 302 redirect with cookies fails without hitting cross-origin sink.
    *mode.lock().unwrap() = "nav_302";
    let nav_err = client
        .resolve_creator("https://space.bilibili.com/946974", Some(&cookie_source))
        .unwrap_err();
    assert!(matches!(nav_err, RefreshError::PlatformResponse(_)));
    assert!(nav_err.to_string().contains("redirect"));
    assert_no_header_leaks(&nav_err.to_string());

    // 2. `/x/space/wbi/acc/info` 307 redirect with cookies fails without hitting cross-origin sink.
    *mode.lock().unwrap() = "acc_307";
    let acc_err = client
        .resolve_creator("https://space.bilibili.com/946974", Some(&cookie_source))
        .unwrap_err();
    assert!(matches!(acc_err, RefreshError::PlatformResponse(_)));
    assert!(acc_err.to_string().contains("307"));
    assert_no_header_leaks(&acc_err.to_string());

    // 3. `/x/space/wbi/arc/search` 302 redirect with cookies fails cleanly and preserves checkpoint.
    *mode.lock().unwrap() = "arc_302";
    let storage = TestStorage::new();
    let initial_cp = Timestamp::from_unix_seconds(1_700_000_000);
    let followed = storage.follow_at("946974", "Sanitized Creator Alpha", initial_cp);
    let arc_err = refresh_creators(
        &storage,
        &client,
        Some(followed.creator.id),
        RefreshCookiePolicy::Explicit {
            source: cookie_source,
            save_as_default: false,
        },
    )
    .unwrap()
    .check_failures()
    .unwrap_err();
    assert!(matches!(
        arc_err,
        AppError::RefreshPartialFailure { succeeded: 0, .. }
    ));
    assert!(arc_err.to_string().contains("302"));
    assert_no_header_leaks(&arc_err.to_string());
    assert_eq!(
        storage
            .get_followed_creator(followed.creator.id)
            .unwrap()
            .follow
            .refresh_checkpoint,
        initial_cp
    );

    // 4. `/x/frontend/finger/spi` 302 redirect during guest lookup never follows cross-origin Location.
    *mode.lock().unwrap() = "spi_302";
    let resolved = client
        .resolve_creator("https://space.bilibili.com/946974", None)
        .unwrap();
    assert_eq!(resolved.platform_creator_id, "946974");

    // Verify cookies were sent to the configured API origin, and NEVER to the cross-origin sink.
    assert!(api_server.requests().iter().any(|r| {
        r.path == "/x/space/wbi/arc/search"
            && r.cookie_header
                .as_deref()
                .is_some_and(|c| c.contains("SESSDATA=sanitized-sessdata-token"))
    }));
    assert!(
        sink_server.requests().is_empty(),
        "cross-origin redirect target unexpectedly received requests: {:?}",
        sink_server.requests()
    );
}
