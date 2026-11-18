use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::Value;
use siphon::domain::Timestamp;
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

fn stub_bin_dir() -> &'static PathBuf {
    static STUB_DIR: OnceLock<PathBuf> = OnceLock::new();
    STUB_DIR.get_or_init(|| {
        let source = fixture_path("yt_dlp_stub.rs");
        let dir = env::temp_dir().join(format!("siphon-cli-e2e-bin-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create stub bin dir");
        let out_bin = dir.join(format!("yt-dlp{}", env::consts::EXE_SUFFIX));

        let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = Command::new(rustc)
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&out_bin)
            .output()
            .expect("compile offline yt-dlp stub binary");
        assert!(
            output.status.success(),
            "failed to compile yt-dlp stub:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        dir
    })
}

fn path_with_prepended_dir(dir: &Path) -> OsString {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = env::var_os("PATH") {
        paths.extend(env::split_paths(&existing));
    }
    env::join_paths(paths).expect("join PATH entries")
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
}

impl MockHttpResponse {
    fn ok_json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
        }
    }

    fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
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
                        404 => "Not Found",
                        500 => "Internal Server Error",
                        502 => "Bad Gateway",
                        _ => "Status",
                    };
                    let raw = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        resp.status,
                        reason,
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

struct CliRunner {
    bin: PathBuf,
    work_dir: PathBuf,
    db_path: PathBuf,
    api_base: String,
    stub_dir: PathBuf,
}

impl CliRunner {
    fn new(work_dir: PathBuf, db_path: PathBuf, api_base: String) -> Self {
        Self {
            bin: PathBuf::from(env!("CARGO_BIN_EXE_siphon")),
            work_dir,
            db_path,
            api_base,
            stub_dir: stub_bin_dir().clone(),
        }
    }

    fn run_with_db_flag(&self, args: &[&str]) -> Output {
        let mut full_args = vec!["--db", self.db_path.to_str().expect("utf8 db path")];
        full_args.extend_from_slice(args);
        self.run_raw(&full_args, &[])
    }

    fn run_with_db_env(&self, args: &[&str]) -> Output {
        let db_str = self.db_path.to_str().expect("utf8 db path");
        self.run_raw(args, &[("SIPHON_DB", db_str)])
    }

    fn run_raw(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .current_dir(&self.work_dir)
            .env("PATH", path_with_prepended_dir(&self.stub_dir))
            .env("SIPHON_BILIBILI_API_BASE", &self.api_base)
            .env_remove("SIPHON_DB");
        for key in [
            "SIPHON_STUB_LOG",
            "SIPHON_STUB_MODE",
            "SIPHON_STUB_EXIT_CODE",
            "SIPHON_STUB_STDERR",
            "SIPHON_STUB_UPLOAD_ID",
            "SIPHON_STUB_TITLE",
            "SIPHON_STUB_CREATE_PART",
            "SIPHON_STUB_STDERR_PROGRESS",
        ] {
            cmd.env_remove(key);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd.output().expect("execute siphon binary")
    }
}

fn stdout_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr_str(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
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

#[derive(Debug, PartialEq, Eq)]
struct LoggedInvocation {
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
    let mut current_args = Vec::new();

    for line in contents.lines() {
        if line == "INVOCATION" {
            current_args = Vec::new();
        } else if let Some(rest) = line.strip_prefix("ARG[")
            && let Some((_, val)) = rest.split_once("]=")
        {
            current_args.push(unescape_field(val));
        } else if line == "END" {
            invocations.push(LoggedInvocation {
                args: std::mem::take(&mut current_args),
            });
        }
    }
    invocations
}

fn find_upload_id(upload_list_stdout: &str, bvid: &str) -> String {
    upload_list_stdout
        .lines()
        .find(|line| line.contains(bvid))
        .and_then(|line| line.split('\t').next())
        .unwrap_or_else(|| panic!("expected pending upload {bvid} in:\n{upload_list_stdout}"))
        .to_owned()
}

fn arc_search_json(items: &[(&str, i64, &str, i64)], pn: u32, ps: u32, count: u64) -> String {
    let vlist: Vec<Value> = items
        .iter()
        .map(|&(bvid, aid, title, created)| {
            serde_json::json!({
                "aid": aid,
                "bvid": bvid,
                "title": title,
                "created": created,
            })
        })
        .collect();
    serde_json::json!({
        "code": 0,
        "message": "0",
        "ttl": 1,
        "data": {
            "list": {
                "vlist": vlist
            },
            "page": {
                "pn": pn,
                "ps": ps,
                "count": count
            }
        }
    })
    .to_string()
}

#[test]
fn help_and_cli_errors_agree_with_readme_through_binary() {
    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("help-check.db");
    let runner = CliRunner::new(work_dir, db_path.clone(), "https://api.bilibili.com".into());

    // Top-level help & version match README commands without creating SQLite DB.
    let help = runner.run_raw(&["--help"], &[]);
    assert!(help.status.success());
    let help_out = stdout_str(&help);
    for expected in [
        "creator",
        "refresh",
        "upload",
        "download",
        "preset",
        "--db <PATH>",
        "SIPHON_DB",
    ] {
        assert!(
            help_out.contains(expected),
            "top-level --help missing '{expected}':\n{help_out}"
        );
    }

    let version = runner.run_raw(&["--version"], &[]);
    assert!(version.status.success());
    assert!(stdout_str(&version).contains("siphon"));

    // Subcommand help pages document flags and semantics accurately.
    let refresh_help = stdout_str(&runner.run_raw(&["refresh", "--help"], &[]));
    for flag in [
        "[CREATOR_ID]",
        "--cookies <FILE>",
        "--cookies-from-browser <BROWSER>",
        "--save-cookies",
        "--clear-cookies",
        "--no-cookies",
    ] {
        assert!(
            refresh_help.contains(flag),
            "refresh --help missing '{flag}'"
        );
    }

    let download_help = stdout_str(&runner.run_raw(&["download", "--help"], &[]));
    for expected in [
        "<URL|--upload <ID>>",
        "--upload <ID>",
        "--preset <NAME>",
        "--no-preset",
        "--cookies <FILE>",
        "--cookies-from-browser <BROWSER>",
        "[YT_DLP_ARGS]...",
    ] {
        assert!(
            download_help.contains(expected),
            "download --help missing '{expected}'"
        );
    }

    let preset_add_help = stdout_str(&runner.run_raw(&["preset", "add", "--help"], &[]));
    assert!(preset_add_help.contains("<NAME> -- <ARGS>..."));
    assert!(preset_add_help.contains("video"));

    assert!(
        !db_path.exists(),
        "help/version invocations should not create the SQLite database"
    );

    // Usage & validation errors exit 2.
    for bad_args in [
        &[][..],
        &["creator"],
        &["upload"],
        &["preset"],
        &["download"],
        &[
            "download",
            "https://www.bilibili.com/video/BV1xx",
            "--upload",
            "1",
        ],
        &[
            "download",
            "https://www.bilibili.com/video/BV1xx",
            "-f",
            "best",
        ],
        &[
            "download",
            "https://www.bilibili.com/video/BV1xx",
            "--preset",
            "audio",
            "--no-preset",
        ],
        &["preset", "add", "audio"],
        &["preset", "add", "audio", "--"],
        &["refresh", "--save-cookies"],
        &["refresh", "--clear-cookies", "--cookies", "cookies.txt"],
        &["creator", "remove", "0"],
        &["upload", "ignore", "0"],
        &["download", "--upload", "0"],
    ] {
        let out = runner.run_raw(bad_args, &[]);
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected exit code 2 for {bad_args:?}, got {:?}, stderr={}",
            out.status.code(),
            stderr_str(&out)
        );
    }

    // Unsafe non-loopback http:// override is rejected cleanly before network I/O.
    let bad_api = runner.run_raw(
        &["--db", db_path.to_str().unwrap(), "creator", "list"],
        &[("SIPHON_BILIBILI_API_BASE", "http://example.com")],
    );
    assert_eq!(bad_api.status.code(), Some(1));
    assert!(stderr_str(&bad_api).contains("loopback"));
}

#[derive(Default)]
struct MockFeedState {
    alpha_p1_items: Vec<(String, i64, String, i64)>,
    alpha_p2_items: Vec<(String, i64, String, i64)>,
    alpha_p3_items: Vec<(String, i64, String, i64)>,
    alpha_total_count: u64,
    alpha_fail_page2: bool,
    beta_items: Vec<(String, i64, String, i64)>,
}

#[test]
fn full_readme_workflows_run_end_to_end_through_siphon_binary() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_alpha = read_fixture("bilibili/acc_info_946974.json");
    let acc_beta = read_fixture("bilibili/acc_info_888001.json");
    let acc_404 = read_fixture("bilibili/acc_info_not_found.json");
    let acc_352 = read_fixture("bilibili/acc_info_risk_control.json");
    let arc_rate_limited = read_fixture("bilibili/arc_search_rate_limited.json");
    let spi_json =
        r#"{"code":0,"message":"0","data":{"b_3":"E2E-GUEST-BUVID3","b_4":"E2E-BUVID4"}}"#;

    let state = Arc::new(Mutex::new(MockFeedState::default()));
    let state_clone = Arc::clone(&state);

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/frontend/finger/spi" => MockHttpResponse::ok_json(spi_json),
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/acc/info" => {
            assert!(req.query.contains_key("w_rid"));
            assert!(req.query.contains_key("wts"));
            match req.query.get("mid").map(String::as_str) {
                Some("946974") => MockHttpResponse::ok_json(&acc_alpha),
                Some("888001") => MockHttpResponse::ok_json(&acc_beta),
                Some("404040") => MockHttpResponse::ok_json(&acc_404),
                Some("352352") => MockHttpResponse::ok_json(&acc_352),
                _ => MockHttpResponse::status(404, "not found"),
            }
        }
        "/x/space/wbi/arc/search" => {
            assert!(req.query.contains_key("w_rid"));
            let guard = state_clone.lock().unwrap();
            let mid = req.query.get("mid").map(String::as_str);
            let pn = req.query.get("pn").map(String::as_str);
            match (mid, pn) {
                (Some("946974"), Some("1")) => {
                    let items: Vec<(&str, i64, &str, i64)> = guard
                        .alpha_p1_items
                        .iter()
                        .map(|(b, a, t, c)| (b.as_str(), *a, t.as_str(), *c))
                        .collect();
                    MockHttpResponse::ok_json(arc_search_json(
                        &items,
                        1,
                        2,
                        guard.alpha_total_count,
                    ))
                }
                (Some("946974"), Some("2")) => {
                    if guard.alpha_fail_page2 {
                        MockHttpResponse::ok_json(&arc_rate_limited)
                    } else {
                        let items: Vec<(&str, i64, &str, i64)> = guard
                            .alpha_p2_items
                            .iter()
                            .map(|(b, a, t, c)| (b.as_str(), *a, t.as_str(), *c))
                            .collect();
                        MockHttpResponse::ok_json(arc_search_json(
                            &items,
                            2,
                            2,
                            guard.alpha_total_count,
                        ))
                    }
                }
                (Some("946974"), Some("3")) => {
                    let items: Vec<(&str, i64, &str, i64)> = guard
                        .alpha_p3_items
                        .iter()
                        .map(|(b, a, t, c)| (b.as_str(), *a, t.as_str(), *c))
                        .collect();
                    MockHttpResponse::ok_json(arc_search_json(
                        &items,
                        3,
                        2,
                        guard.alpha_total_count,
                    ))
                }
                (Some("888001"), Some("1")) => {
                    let items: Vec<(&str, i64, &str, i64)> = guard
                        .beta_items
                        .iter()
                        .map(|(b, a, t, c)| (b.as_str(), *a, t.as_str(), *c))
                        .collect();
                    MockHttpResponse::ok_json(arc_search_json(&items, 1, 30, items.len() as u64))
                }
                _ => MockHttpResponse::status(404, "unexpected page"),
            }
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    let db_path = temp.path().join("db").join("siphon.db");
    let log_path = temp.path().join("stub.log");
    fs::create_dir_all(&work_dir).unwrap();
    write_stub_config(
        &work_dir,
        &[("SIPHON_STUB_LOG", &log_path.to_string_lossy())],
    );

    let runner = CliRunner::new(work_dir.clone(), db_path.clone(), server.base_url());

    // 1. Fresh database state matches docs.
    let init_creators = runner.run_with_db_flag(&["creator", "list"]);
    assert!(init_creators.status.success());
    assert_eq!(stdout_str(&init_creators), "No followed creators.\n");

    let init_uploads = runner.run_with_db_flag(&["upload", "list"]);
    assert!(init_uploads.status.success());
    assert_eq!(stdout_str(&init_uploads), "No pending uploads.\n");

    let init_refresh = runner.run_with_db_flag(&["refresh"]);
    assert!(init_refresh.status.success());
    assert_eq!(
        stdout_str(&init_refresh),
        "No followed creators to refresh.\n"
    );

    let init_presets = runner.run_with_db_flag(&["preset", "list"]);
    assert!(init_presets.status.success());
    assert!(stdout_str(&init_presets).contains("* video\t[built-in]\t-f bv*+ba/b"));

    // 2. Follow creator, verify duplicate/not-found/risk/invalid-URL errors.
    let add_alpha =
        runner.run_with_db_flag(&["creator", "add", "https://space.bilibili.com/946974"]);
    assert!(add_alpha.status.success(), "{}", stderr_str(&add_alpha));
    let t_after_follow_alpha = Timestamp::now().as_unix_seconds();
    assert!(
        stdout_str(&add_alpha).contains("Followed creator 1 (Sanitized Creator Alpha) [946974]")
    );

    let list_creators = runner.run_with_db_flag(&["creator", "list"]);
    assert!(list_creators.status.success());
    let creators_out = stdout_str(&list_creators);
    assert!(creators_out.contains("1\tbilibili\t946974\tSanitized Creator Alpha\tfollowed="));
    assert!(creators_out.contains("\tcheckpoint="));

    let dup_alpha =
        runner.run_with_db_flag(&["creator", "add", "https://space.bilibili.com/946974"]);
    assert_eq!(dup_alpha.status.code(), Some(1));
    assert!(stderr_str(&dup_alpha).contains("already followed"));

    let err_404 = runner.run_with_db_flag(&["creator", "add", "https://space.bilibili.com/404040"]);
    assert_eq!(err_404.status.code(), Some(1));
    assert!(stderr_str(&err_404).contains("creator not found for URL"));

    let err_352 = runner.run_with_db_flag(&["creator", "add", "https://space.bilibili.com/352352"]);
    assert_eq!(err_352.status.code(), Some(1));
    assert!(stderr_str(&err_352).contains("-352"));
    assert!(stderr_str(&err_352).contains("cookies"));

    let err_bad_url =
        runner.run_with_db_flag(&["creator", "add", "https://www.bilibili.com/video/BV1xx"]);
    assert_eq!(err_bad_url.status.code(), Some(1));
    assert!(stderr_str(&err_bad_url).contains("invalid creator URL"));

    // 3. Paginated refresh (`ps=30`) discovers only uploads strictly after follow time, stops before page 3.
    {
        let mut guard = state.lock().unwrap();
        let mut p1 = Vec::with_capacity(30);
        for offset in (3..=32).rev() {
            p1.push((
                format!("BV1e2e{offset:04}"),
                10000 + offset as i64,
                format!("Alpha Upload {offset}"),
                t_after_follow_alpha + (offset as i64 * 10),
            ));
        }
        guard.alpha_p1_items = p1;
        guard.alpha_p2_items = vec![
            (
                "BV1e2e0002".into(),
                10002,
                "Alpha Upload Two".into(),
                t_after_follow_alpha + 15,
            ),
            (
                "BV1e2e0001".into(),
                10001,
                "Pre-follow Upload One".into(),
                t_after_follow_alpha - 600,
            ),
        ];
        guard.alpha_p3_items = vec![(
            "BV1e2e0000".into(),
            10000,
            "Ancient Upload Zero".into(),
            t_after_follow_alpha - 900,
        )];
        guard.alpha_total_count = 33;
    }

    let refresh_1 = runner.run_with_db_flag(&["refresh", "1"]);
    assert!(refresh_1.status.success(), "{}", stderr_str(&refresh_1));
    let r1_out = stdout_str(&refresh_1);
    assert!(
        r1_out.contains("Refreshed creator 1 (Sanitized Creator Alpha): 31 new upload(s), 0 known"),
        "unexpected refresh output: {r1_out}"
    );

    let arc_pages: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|r| r.path == "/x/space/wbi/arc/search")
        .filter_map(|r| r.query.get("pn").cloned())
        .collect();
    assert_eq!(arc_pages, vec!["1", "2"]);

    let list_1 = stdout_str(&runner.run_with_db_flag(&["upload", "list"]));
    assert!(list_1.contains("BV1e2e0032"));
    assert!(list_1.contains("BV1e2e0003"));
    assert!(list_1.contains("BV1e2e0002"));
    assert!(!list_1.contains("BV1e2e0001"));

    let id_32 = find_upload_id(&list_1, "BV1e2e0032");
    let id_31 = find_upload_id(&list_1, "BV1e2e0031");
    let id_02 = find_upload_id(&list_1, "BV1e2e0002");

    // 4. Upload ignore workflow & direct download leaves pending upload unchanged.
    let ignore_02 = runner.run_with_db_flag(&["upload", "ignore", &id_02]);
    assert!(ignore_02.status.success(), "{}", stderr_str(&ignore_02));
    assert!(stdout_str(&ignore_02).contains(&format!("Ignored upload {id_02} (Alpha Upload Two)")));

    let re_ignore_02 = runner.run_with_db_flag(&["upload", "ignore", &id_02]);
    assert_eq!(re_ignore_02.status.code(), Some(1));
    assert!(stderr_str(&re_ignore_02).contains("is ignored, expected pending"));

    let dl_ignored = runner.run_with_db_flag(&["download", "--upload", &id_02]);
    assert_eq!(dl_ignored.status.code(), Some(1));
    assert!(stderr_str(&dl_ignored).contains("is ignored, expected pending"));

    // Direct URL matching pending upload leaves it pending; stdout stays pure yt-dlp output.
    let direct_matching =
        runner.run_with_db_flag(&["download", "https://www.bilibili.com/video/BV1e2e0032"]);
    assert!(
        direct_matching.status.success(),
        "{}",
        stderr_str(&direct_matching)
    );
    assert!(stdout_str(&direct_matching).contains("[download] Destination:"));
    assert!(!stdout_str(&direct_matching).contains("Downloaded direct URL"));
    assert!(stderr_str(&direct_matching).contains("Downloaded direct URL"));
    assert!(stdout_str(&runner.run_with_db_flag(&["upload", "list"])).contains("BV1e2e0032"));

    // 5. Simulation on pending upload leaves it pending; real download marks it downloaded.
    let sim_upload_32 =
        runner.run_with_db_flag(&["download", "--upload", &id_32, "--", "--simulate"]);
    assert!(
        sim_upload_32.status.success(),
        "{}",
        stderr_str(&sim_upload_32)
    );
    assert!(stderr_str(&sim_upload_32).contains("remains pending"));
    assert!(stdout_str(&runner.run_with_db_flag(&["upload", "list"])).contains("BV1e2e0032"));

    let dl_upload_32 = runner.run_with_db_flag(&["download", "--upload", &id_32]);
    assert!(
        dl_upload_32.status.success(),
        "{}",
        stderr_str(&dl_upload_32)
    );
    assert!(stderr_str(&dl_upload_32).contains("marked as downloaded"));
    assert!(work_dir.join("Stub Video [BV1e2e0032].mp4").is_file());
    assert!(!stdout_str(&runner.run_with_db_flag(&["upload", "list"])).contains("BV1e2e0032"));

    // Re-downloading downloaded upload fails cleanly without spawning yt-dlp.
    let logs_before_redl = read_stub_log(&log_path).len();
    let redl_32 = runner.run_with_db_flag(&["download", "--upload", &id_32]);
    assert_eq!(redl_32.status.code(), Some(1));
    assert!(stderr_str(&redl_32).contains("is downloaded, expected pending"));
    assert_eq!(read_stub_log(&log_path).len(), logs_before_redl);

    // 6. Subtitle-only downloads on pending upload: missing subs stays pending, saved sub marks downloaded.
    let sub_missing = runner.run_raw(
        &[
            "--db",
            db_path.to_str().unwrap(),
            "download",
            "--upload",
            &id_31,
            "--no-preset",
            "--",
            "--skip-download",
            "--write-subs",
        ],
        &[("SIPHON_STUB_MODE", "subtitle_missing")],
    );
    assert!(sub_missing.status.success(), "{}", stderr_str(&sub_missing));
    assert!(stderr_str(&sub_missing).contains("remains pending"));
    assert!(stdout_str(&runner.run_with_db_flag(&["upload", "list"])).contains("BV1e2e0031"));

    let sub_ok = runner.run_with_db_flag(&[
        "download",
        "--upload",
        &id_31,
        "--no-preset",
        "--",
        "-q",
        "--skip-download",
        "--write-subs",
        "--convert-subs",
        "srt",
    ]);
    assert!(sub_ok.status.success(), "{}", stderr_str(&sub_ok));
    assert!(stderr_str(&sub_ok).contains("marked as downloaded"));
    assert!(work_dir.join("Stub Video [BV1e2e0031].en.srt").is_file());
    assert!(!stdout_str(&runner.run_with_db_flag(&["upload", "list"])).contains("BV1e2e0031"));

    // 7. Replay at same-second checkpoint preserves statuses (Downloaded & Ignored) while discovering ties & URL changes.
    {
        let mut guard = state.lock().unwrap();
        let mut p1 = vec![
            (
                "BV1e2e0033".into(),
                10033,
                "Alpha Tie Upload 33".into(),
                t_after_follow_alpha + 320,
            ),
            (
                "BV1e2e0032".into(),
                10032,
                "Alpha Upload 32 Renamed".into(),
                t_after_follow_alpha + 320,
            ),
        ];
        for offset in (4..=31).rev() {
            p1.push((
                format!("BV1e2e{offset:04}"),
                10000 + offset as i64,
                format!("Alpha Upload {offset}"),
                t_after_follow_alpha + (offset as i64 * 10),
            ));
        }
        guard.alpha_p1_items = p1;
        guard.alpha_p2_items = vec![
            (
                "BV1e2e0003".into(),
                10003,
                "Alpha Upload 3".into(),
                t_after_follow_alpha + 30,
            ),
            (
                "BV1e2e0002".into(),
                10002,
                "Alpha Upload Two".into(),
                t_after_follow_alpha + 15,
            ),
        ];
        guard.alpha_total_count = 32;
    }

    let refresh_replay = runner.run_with_db_flag(&["refresh", "1"]);
    assert!(
        refresh_replay.status.success(),
        "{}",
        stderr_str(&refresh_replay)
    );
    assert!(
        stdout_str(&refresh_replay).contains("1 new upload(s), 1 known"),
        "unexpected replay output: {}",
        stdout_str(&refresh_replay)
    );
    let pending_after_replay = stdout_str(&runner.run_with_db_flag(&["upload", "list"]));
    assert!(pending_after_replay.contains("BV1e2e0033"));
    assert!(!pending_after_replay.contains("BV1e2e0032"));
    assert!(!pending_after_replay.contains("BV1e2e0002"));

    // 8. Independent refresh failure isolation + SIPHON_DB env support.
    let add_beta = runner.run_with_db_env(&["creator", "add", "https://space.bilibili.com/888001"]);
    assert!(add_beta.status.success(), "{}", stderr_str(&add_beta));
    let t_after_follow_beta = Timestamp::now().as_unix_seconds();

    {
        let mut guard = state.lock().unwrap();
        let mut p1 = Vec::with_capacity(30);
        p1.push((
            "BV1e2e0034".into(),
            10034,
            "Should Rollback On Page 2 Failure".into(),
            t_after_follow_alpha + 350,
        ));
        for offset in 1..=29 {
            p1.push((
                format!("BV1pad{offset:04}"),
                11000 + offset as i64,
                format!("Pad {offset}"),
                t_after_follow_alpha + 340,
            ));
        }
        guard.alpha_p1_items = p1;
        guard.alpha_total_count = 60;
        guard.alpha_fail_page2 = true;
        guard.beta_items = vec![(
            "BV1beta0001".into(),
            20001,
            "Beta Upload One".into(),
            t_after_follow_beta + 10,
        )];
    }

    let partial = runner.run_with_db_env(&["refresh"]);
    assert_eq!(partial.status.code(), Some(1));
    let partial_err = stderr_str(&partial);
    assert!(partial_err.contains("1 succeeded and 1 failed"));
    assert!(partial_err.contains("creator 1 (Sanitized Creator Alpha)"));

    let pending_after_partial = stdout_str(&runner.run_with_db_env(&["upload", "list"]));
    assert!(pending_after_partial.contains("BV1beta0001"));
    assert!(!pending_after_partial.contains("BV1e2e0034"));

    // 9. Cookies on refresh (file + --save-cookies + --no-cookies + --clear-cookies) and isolation from download.
    let bili_cookies = fixture_path("cookies/bilibili_netscape.txt");
    {
        let mut guard = state.lock().unwrap();
        guard.alpha_fail_page2 = false;
        guard.alpha_p1_items = vec![
            (
                "BV1e2e0035".into(),
                10035,
                "Alpha Upload 35".into(),
                t_after_follow_alpha + 400,
            ),
            (
                "BV1e2e0033".into(),
                10033,
                "Alpha Tie Upload 33".into(),
                t_after_follow_alpha + 320,
            ),
            (
                "BV1e2e0031".into(),
                10031,
                "Alpha Upload 31".into(),
                t_after_follow_alpha + 310,
            ),
        ];
        guard.alpha_p2_items = Vec::new();
        guard.alpha_total_count = 3;
    }

    let save_cookies = runner.run_with_db_flag(&[
        "refresh",
        "1",
        "--cookies",
        bili_cookies.to_str().unwrap(),
        "--save-cookies",
    ]);
    assert!(
        save_cookies.status.success(),
        "{}",
        stderr_str(&save_cookies)
    );
    assert!(stdout_str(&save_cookies).contains("Saved default refresh cookie source:"));

    // Invalid CREATOR_ID must not clear or overwrite saved cookies.
    let bad_refresh_id = runner.run_with_db_flag(&["refresh", "999", "--clear-cookies"]);
    assert_eq!(bad_refresh_id.status.code(), Some(1));
    assert!(stderr_str(&bad_refresh_id).contains("creator 999 not found"));

    // Subsequent refresh uses saved cookie default.
    let reqs_before_saved = server.requests().len();
    let refresh_saved = runner.run_with_db_flag(&["refresh", "1"]);
    assert!(
        refresh_saved.status.success(),
        "{}",
        stderr_str(&refresh_saved)
    );
    let saved_arc = server.requests()[reqs_before_saved..]
        .iter()
        .rfind(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap()
        .cookie_header
        .clone()
        .unwrap();
    assert!(saved_arc.contains("SESSDATA=sanitized-sessdata-token"));
    assert!(!saved_arc.contains("foreign_cookie"));

    // `--no-cookies` skips saved default without clearing it (uses unauthenticated guest buvid3 only).
    let reqs_before_nocookie = server.requests().len();
    let refresh_nocookie = runner.run_with_db_flag(&["refresh", "1", "--no-cookies"]);
    assert!(refresh_nocookie.status.success());
    let nocookie_arc = server.requests()[reqs_before_nocookie..]
        .iter()
        .rfind(|r| r.path == "/x/space/wbi/arc/search")
        .unwrap()
        .cookie_header
        .clone();
    assert_eq!(nocookie_arc.as_deref(), Some("buvid3=E2E-GUEST-BUVID3"));

    // Download does NOT inherit refresh default cookies.
    let dl_no_inherit =
        runner.run_with_db_flag(&["download", "https://www.bilibili.com/video/BV1noinherit"]);
    assert!(dl_no_inherit.status.success());
    let last_stub = read_stub_log(&log_path).pop().unwrap();
    assert!(!last_stub.args.contains(&"--cookies".to_owned()));

    // Missing cookie file fails cleanly on both refresh and download.
    let missing_cookie_path = temp.path().join("missing-cookies.txt");
    let missing_refresh_cookie = runner.run_with_db_flag(&[
        "refresh",
        "1",
        "--cookies",
        missing_cookie_path.to_str().unwrap(),
    ]);
    assert_eq!(missing_refresh_cookie.status.code(), Some(1));
    assert!(stderr_str(&missing_refresh_cookie).contains("cookie error"));

    let missing_dl_cookie = runner.run_with_db_flag(&[
        "download",
        "https://www.bilibili.com/video/BV1missingcookie",
        "--cookies",
        missing_cookie_path.to_str().unwrap(),
    ]);
    assert_eq!(missing_dl_cookie.status.code(), Some(1));
    assert!(stderr_str(&missing_dl_cookie).contains("cookie error"));

    // Explicit --cookies-from-browser forwards to yt-dlp for download.
    let dl_browser_cookie = runner.run_with_db_flag(&[
        "download",
        "https://www.bilibili.com/video/BV1browsercookie",
        "--cookies-from-browser",
        "firefox:default",
    ]);
    assert!(dl_browser_cookie.status.success());
    let browser_stub = read_stub_log(&log_path).pop().unwrap();
    assert!(
        browser_stub
            .args
            .windows(2)
            .any(|w| w == ["--cookies-from-browser", "firefox:default"])
    );

    // Clear refresh default cookies.
    let clear_cookies = runner.run_with_db_flag(&["refresh", "--clear-cookies"]);
    assert!(clear_cookies.status.success());
    assert!(
        stdout_str(&clear_cookies).contains("Cleared remembered default refresh cookie source.")
    );

    // 10. Preset lifecycle, forwarding order, `--preset`, `--no-preset`, `-J` stdout transparency, and playlist isolation.
    let add_builtin = runner.run_with_db_flag(&["preset", "add", "video", "--", "-f", "best"]);
    assert_eq!(add_builtin.status.code(), Some(1));
    assert!(stderr_str(&add_builtin).contains("reserved"));

    let rm_builtin = runner.run_with_db_flag(&["preset", "remove", "video"]);
    assert_eq!(rm_builtin.status.code(), Some(1));
    assert!(stderr_str(&rm_builtin).contains("reserved"));

    let add_audio = runner.run_with_db_flag(&[
        "preset",
        "add",
        "audio",
        "--",
        "-x",
        "--audio-format",
        "mp3",
    ]);
    assert!(add_audio.status.success(), "{}", stderr_str(&add_audio));
    assert!(
        runner
            .run_with_db_flag(&["preset", "set-default", "audio"])
            .status
            .success()
    );

    let dl_cookies = fixture_path("cookies.txt");
    let dl_with_preset = runner.run_with_db_flag(&[
        "download",
        "https://www.bilibili.com/video/BV1presetorder",
        "--cookies",
        dl_cookies.to_str().unwrap(),
        "--",
        "--no-overwrites",
    ]);
    assert!(
        dl_with_preset.status.success(),
        "{}",
        stderr_str(&dl_with_preset)
    );
    let preset_stub = read_stub_log(&log_path).pop().unwrap();
    let tail = &preset_stub.args[preset_stub.args.len() - 8..];
    assert_eq!(
        tail,
        &[
            "-x",
            "--audio-format",
            "mp3",
            "--no-overwrites",
            "--cookies",
            dl_cookies.to_str().unwrap(),
            "--",
            "https://www.bilibili.com/video/BV1presetorder",
        ]
    );

    // Explicit `--preset video` overrides the `audio` default for this invocation only.
    let dl_explicit_video = runner.run_with_db_flag(&[
        "download",
        "https://www.bilibili.com/video/BV1explicitvideo",
        "--preset",
        "video",
        "--",
        "-q",
    ]);
    assert!(dl_explicit_video.status.success());
    let video_stub = read_stub_log(&log_path).pop().unwrap();
    let video_tail = &video_stub.args[video_stub.args.len() - 5..];
    assert_eq!(
        video_tail,
        &[
            "-f",
            "bv*+ba/b",
            "-q",
            "--",
            "https://www.bilibili.com/video/BV1explicitvideo",
        ]
    );

    // Direct JSON passthrough `-J` leaves stdout pure JSON and routes Siphon summary to stderr.
    let dl_json = runner.run_with_db_flag(&[
        "download",
        "https://www.bilibili.com/video/BV1jsonout",
        "--no-preset",
        "--",
        "-J",
    ]);
    assert!(dl_json.status.success());
    let json_stdout = stdout_str(&dl_json);
    let parsed_json: Value =
        serde_json::from_str(json_stdout.trim()).expect("direct -J stdout must be valid JSON");
    assert_eq!(parsed_json["id"], "BV1jsonout");
    assert!(!json_stdout.contains("Finished direct URL"));
    assert!(stderr_str(&dl_json).contains("Finished direct URL"));

    // Direct playlist download expands playlist; tracked `--upload` isolates to single upload.
    assert!(
        runner
            .run_with_db_flag(&["preset", "add", "playlisty", "--", "--yes-playlist"])
            .status
            .success()
    );
    let dl_direct_pl = runner.run_with_db_flag(&[
        "download",
        "https://www.bilibili.com/video/BV1directpl?list=PL1",
        "--preset",
        "playlisty",
    ]);
    assert!(dl_direct_pl.status.success());
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

    let pending_lines = stdout_str(&runner.run_with_db_flag(&["upload", "list"]));
    let beta_upload_id = find_upload_id(&pending_lines, "BV1beta0001");
    let dl_tracked_pl = runner.run_with_db_flag(&[
        "download",
        "--upload",
        &beta_upload_id,
        "--preset",
        "playlisty",
    ]);
    assert!(dl_tracked_pl.status.success());
    assert!(stderr_str(&dl_tracked_pl).contains("marked as downloaded"));
    let tracked_stub = read_stub_log(&log_path).pop().unwrap();
    assert!(tracked_stub.args.contains(&"--ignore-config".to_owned()));
    assert!(tracked_stub.args.contains(&"--no-playlist".to_owned()));

    // Disallowed multi-target options on `--upload <ID>` fail pre-spawn.
    let id_33 = find_upload_id(&pending_lines, "BV1e2e0033");
    let bad_upload_flag = runner.run_with_db_flag(&[
        "download",
        "--upload",
        &id_33,
        "--no-preset",
        "--",
        "--batch-file",
        "urls.txt",
    ]);
    assert_eq!(bad_upload_flag.status.code(), Some(1));
    assert!(stderr_str(&bad_upload_flag).contains("invalid download arguments"));

    // Removing active default preset resets default back to `video`.
    let rm_audio = runner.run_with_db_flag(&["preset", "remove", "audio"]);
    assert!(rm_audio.status.success());
    assert!(stdout_str(&rm_audio).contains("default preset reset to 'video'"));
    assert!(
        stdout_str(&runner.run_with_db_flag(&["preset", "list"])).contains("* video\t[built-in]")
    );

    // 11. Unfollow creator 1 keeps pending uploads, stops refreshes, and re-follow starts fresh window.
    let unfollow_1 = runner.run_with_db_flag(&["creator", "remove", "1"]);
    assert!(unfollow_1.status.success());
    assert!(stdout_str(&unfollow_1).contains("existing uploads preserved"));
    assert!(stdout_str(&runner.run_with_db_flag(&["upload", "list"])).contains("BV1e2e0035"));

    let refresh_unfollowed = runner.run_with_db_flag(&["refresh", "1"]);
    assert_eq!(refresh_unfollowed.status.code(), Some(1));
    assert!(stderr_str(&refresh_unfollowed).contains("creator 1 is not currently followed"));

    let refollow_1 =
        runner.run_with_db_flag(&["creator", "add", "https://space.bilibili.com/946974"]);
    assert!(refollow_1.status.success(), "{}", stderr_str(&refollow_1));
    let t_refollow = Timestamp::now().as_unix_seconds();
    assert!(stdout_str(&refollow_1).contains("Followed creator 1 (Sanitized Creator Alpha)"));

    {
        let mut guard = state.lock().unwrap();
        guard.alpha_p1_items = vec![
            (
                "BV1e2e0036".into(),
                10036,
                "Post Refollow Upload".into(),
                t_refollow + 500,
            ),
            (
                "BV1e2e0035".into(),
                10035,
                "Alpha Upload 35".into(),
                t_after_follow_alpha + 400,
            ),
            (
                "BV1preref".into(),
                10099,
                "Before Refollow Upload".into(),
                t_refollow - 10,
            ),
        ];
        guard.alpha_total_count = 3;
    }

    let refresh_refollow = runner.run_with_db_flag(&["refresh", "1"]);
    assert!(
        refresh_refollow.status.success(),
        "{}",
        stderr_str(&refresh_refollow)
    );
    assert!(
        stdout_str(&refresh_refollow).contains("1 new upload(s), 1 known"),
        "unexpected refollow refresh output: {}",
        stdout_str(&refresh_refollow)
    );
    let list_after_refollow = stdout_str(&runner.run_with_db_flag(&["upload", "list"]));
    assert!(list_after_refollow.contains("BV1e2e0036"));
    assert!(!list_after_refollow.contains("BV1preref"));

    // 12. Downloader failure and missing yt-dlp leave upload pending and surface errors on stderr.
    let fail_dl = runner.run_raw(
        &[
            "--db",
            db_path.to_str().unwrap(),
            "download",
            "--upload",
            &id_33,
        ],
        &[
            ("SIPHON_STUB_MODE", "fail"),
            ("SIPHON_STUB_EXIT_CODE", "23"),
            ("SIPHON_STUB_STDERR", "ERROR: simulated network error"),
        ],
    );
    assert_eq!(fail_dl.status.code(), Some(1));
    assert!(stderr_str(&fail_dl).contains("yt-dlp failed with exit code 23"));
    assert!(stdout_str(&runner.run_with_db_flag(&["upload", "list"])).contains("BV1e2e0033"));

    let empty_path_dir = temp.path().join("empty-path");
    fs::create_dir_all(&empty_path_dir).unwrap();
    let missing_yt_dlp = Command::new(&runner.bin)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "download",
            "https://www.bilibili.com/video/BV1missingbin",
        ])
        .current_dir(&work_dir)
        .env("PATH", &empty_path_dir)
        .output()
        .expect("run siphon without yt-dlp on PATH");
    assert_eq!(missing_yt_dlp.status.code(), Some(1));
    assert!(stderr_str(&missing_yt_dlp).contains("ensure yt-dlp is installed and on PATH"));
}
