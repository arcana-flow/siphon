use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clap::Parser;
use rusqlite::Connection;
use serde_json::Value;
use siphon::app::{AppExecuteOptions, AppServices, execute_with_runtime};
use siphon::background::BackgroundRuntime;
use siphon::cli::Cli;
use siphon::domain::{Timestamp, UploadStatus};
use siphon::download::YtDlpDownloader;
use siphon::error::{AppError, BackgroundError, StorageError};
use siphon::output::CommandResult;
use siphon::platform::bilibili::BilibiliRefreshSource;
use siphon::storage::SqliteStorage;
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
        let dir = env::temp_dir().join(format!("siphon-bg-jobs-bin-{}", std::process::id()));
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
                let router = Arc::clone(&router);
                let recorded_clone = Arc::clone(&recorded_clone);
                thread::spawn(move || {
                    if let Some(req) = read_http_request(&mut stream) {
                        recorded_clone.lock().unwrap().push(req.clone());
                        let resp = router(&req);
                        let reason = match resp.status {
                            200 => "OK",
                            404 => "Not Found",
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
                });
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

    fn run_with_db(&self, args: &[&str]) -> Output {
        self.run_with_db_in_dir(&self.work_dir, args, &[])
    }

    fn run_with_db_in_dir(&self, dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut full_args = vec!["--db", self.db_path.to_str().expect("utf8 db path")];
        full_args.extend_from_slice(args);
        self.command_in_dir(dir, &full_args, extra_env)
            .output()
            .expect("execute siphon binary")
    }

    fn command_with_db(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Command {
        let mut full_args = vec!["--db", self.db_path.to_str().expect("utf8 db path")];
        full_args.extend_from_slice(args);
        self.command_in_dir(&self.work_dir, &full_args, extra_env)
    }

    fn command_in_dir(&self, dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .current_dir(dir)
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
            "SIPHON_STUB_STDOUT_PROGRESS",
            "SIPHON_STUB_PROGRESS_BYTES",
            "SIPHON_STUB_READY_FILE",
            "SIPHON_STUB_PID_FILE",
            "SIPHON_STUB_READ_STDIN_EXPECT",
            "SIPHON_STUB_WAIT_FILE",
            "SIPHON_STUB_POST_READY_FILE",
            "SIPHON_STUB_POST_WAIT_FILE",
        ] {
            cmd.env_remove(key);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd
    }
}

fn parse_single_json_stdout(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    assert!(
        !trimmed.is_empty(),
        "expected JSON object on stdout, got empty stdout (stderr={})",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut stream = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
    let first = stream
        .next()
        .expect("must contain one JSON value")
        .unwrap_or_else(|err| panic!("invalid JSON on stdout: {err}\nstdout={stdout}"));
    assert!(
        stream.next().is_none(),
        "expected exactly ONE JSON object on stdout, got multiple: {stdout}"
    );
    assert!(
        first.is_object(),
        "expected top-level JSON object on stdout, got: {first}"
    );
    first
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let start = Instant::now();
    while !path.is_file() {
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for marker file '{}'",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn poll_job_until<F>(runner: &CliRunner, job_id: i64, timeout: Duration, mut predicate: F) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let id_str = job_id.to_string();
    let start = Instant::now();
    let mut last = Value::Null;
    while start.elapsed() < timeout {
        let out = runner.run_with_db(&["--json", "job", "status", &id_str]);
        assert!(
            out.status.success(),
            "job status {id_str} failed: stdout={}, stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let json = parse_single_json_stdout(&out);
        if predicate(&json["data"]) {
            return json["data"].clone();
        }
        last = json;
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for job {job_id} condition; last status: {last}");
}

fn kill_process_by_pid(pid: u32) {
    #[cfg(unix)]
    {
        const SIGKILL: i32 = 9;
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let raw_pid = i32::try_from(pid).expect("pid fits in i32");
        let rc = unsafe { kill(raw_pid, SIGKILL) };
        assert_eq!(rc, 0, "kill(SIGKILL) failed for pid {pid}");
    }

    #[cfg(windows)]
    {
        const PROCESS_TERMINATE: u32 = 0x0001;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        unsafe extern "system" {
            fn OpenProcess(
                dw_desired_access: u32,
                b_inherit_handle: i32,
                dw_process_id: u32,
            ) -> *mut core::ffi::c_void;
            fn TerminateProcess(h_process: *mut core::ffi::c_void, u_exit_code: u32) -> i32;
            fn WaitForSingleObject(h_handle: *mut core::ffi::c_void, dw_milliseconds: u32) -> u32;
            fn CloseHandle(h_object: *mut core::ffi::c_void) -> i32;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
            assert!(!handle.is_null(), "OpenProcess failed for pid {pid}");
            let terminated = TerminateProcess(handle, 1);
            assert_ne!(terminated, 0, "TerminateProcess failed for pid {pid}");
            let _ = WaitForSingleObject(handle, 5_000);
            let _ = CloseHandle(handle);
        }
    }
}

fn arc_search_json(items: &[(&str, i64, &str, i64)]) -> String {
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
            "list": { "vlist": vlist },
            "page": { "pn": 1, "ps": 30, "count": vlist.len() }
        }
    })
    .to_string()
}

struct Gate {
    pair: (Mutex<bool>, Condvar),
    entered: AtomicBool,
}

impl Gate {
    fn new(open: bool) -> Self {
        Self {
            pair: (Mutex::new(open), Condvar::new()),
            entered: AtomicBool::new(false),
        }
    }

    fn wait(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let (lock, cvar) = &self.pair;
        let mut open = lock.lock().unwrap();
        while !*open {
            let (guard, timeout) = cvar.wait_timeout(open, Duration::from_secs(15)).unwrap();
            open = guard;
            if timeout.timed_out() {
                break;
            }
        }
    }

    fn open(&self) {
        let (lock, cvar) = &self.pair;
        let mut open = lock.lock().unwrap();
        *open = true;
        cvar.notify_all();
    }

    fn wait_until_entered(&self, timeout: Duration) {
        let start = Instant::now();
        while !self.entered.load(Ordering::SeqCst) {
            assert!(
                start.elapsed() < timeout,
                "timed out waiting for gate entry"
            );
            thread::sleep(Duration::from_millis(15));
        }
    }
}

#[test]
fn background_refresh_reports_live_progress_mixed_outcomes_and_submission_cookies() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_alpha = read_fixture("bilibili/acc_info_946974.json");
    let acc_beta = read_fixture("bilibili/acc_info_888001.json");
    let arc_rate_limited = read_fixture("bilibili/arc_search_rate_limited.json");
    let spi_json = r#"{"code":0,"message":"0","data":{"b_3":"BUVID3","b_4":"BUVID4"}}"#;

    let gate_beta = Arc::new(Gate::new(false));
    let gate_beta_clone = Arc::clone(&gate_beta);
    let fail_beta = Arc::new(AtomicBool::new(true));
    let fail_beta_clone = Arc::clone(&fail_beta);
    let feed_ts = Timestamp::now().as_unix_seconds() + 100;

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/frontend/finger/spi" => MockHttpResponse::ok_json(spi_json),
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/acc/info" => match req.query.get("mid").map(String::as_str) {
            Some("946974") => MockHttpResponse::ok_json(&acc_alpha),
            Some("888001") => MockHttpResponse::ok_json(&acc_beta),
            _ => MockHttpResponse::status(404, "not found"),
        },
        "/x/space/wbi/arc/search" => match req.query.get("mid").map(String::as_str) {
            Some("946974") => MockHttpResponse::ok_json(arc_search_json(&[(
                "BV1alpha001",
                1001,
                "Alpha One",
                feed_ts,
            )])),
            Some("888001") => {
                gate_beta_clone.wait();
                if fail_beta_clone.load(Ordering::SeqCst) {
                    MockHttpResponse::ok_json(&arc_rate_limited)
                } else {
                    MockHttpResponse::ok_json(arc_search_json(&[(
                        "BV1beta001",
                        2001,
                        "Beta One",
                        feed_ts,
                    )]))
                }
            }
            _ => MockHttpResponse::status(404, "unknown"),
        },
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    let other_dir = temp.path().join("other");
    fs::create_dir_all(&work_dir).unwrap();
    fs::create_dir_all(&other_dir).unwrap();
    let db_path = temp.path().join("siphon.db");
    let runner = CliRunner::new(work_dir.clone(), db_path.clone(), server.base_url());

    // Copy cookie fixture into work_dir so a relative `--cookies ./bili.txt` tests submission cwd resolution.
    let rel_cookie = work_dir.join("bili.txt");
    fs::copy(fixture_path("cookies/bilibili_netscape.txt"), &rel_cookie).unwrap();

    assert!(
        runner
            .run_with_db(&[
                "--json",
                "creator",
                "add",
                "https://space.bilibili.com/946974"
            ])
            .status
            .success()
    );
    assert!(
        runner
            .run_with_db(&[
                "--json",
                "creator",
                "add",
                "https://space.bilibili.com/888001"
            ])
            .status
            .success()
    );

    // Submit background refresh with relative cookies + `--save-cookies`.
    let submitted = runner.run_with_db(&[
        "--json",
        "refresh",
        "--background",
        "--cookies",
        "./bili.txt",
        "--save-cookies",
    ]);
    assert!(
        submitted.status.success(),
        "stdout={}, stderr={}",
        String::from_utf8_lossy(&submitted.stdout),
        String::from_utf8_lossy(&submitted.stderr)
    );
    let sub_json = parse_single_json_stdout(&submitted);
    assert_eq!(sub_json["ok"], true);
    let job_id = sub_json["data"]["id"].as_i64().unwrap();
    assert_eq!(sub_json["data"]["kind"], "refresh");

    // Wait until creator 1 finished and creator 2 entered the mock gate.
    gate_beta.wait_until_entered(Duration::from_secs(10));
    let mid_status = poll_job_until(&runner, job_id, Duration::from_secs(10), |data| {
        data["state"] == "running" && data["progress"]["processed_creators"] == 1
    });
    assert_eq!(mid_status["progress"]["total_creators"], 2);
    assert_eq!(mid_status["progress"]["succeeded_creators"], 1);
    assert_eq!(mid_status["progress"]["failed_creators"], 0);
    assert_eq!(mid_status["progress"]["current_creator_id"], 2);

    // Release beta to fail; verify mixed outcome marks the job failed while retaining all creator outcomes.
    gate_beta.open();
    let final_status = poll_job_until(&runner, job_id, Duration::from_secs(10), |data| {
        data["state"] != "running"
    });
    assert_eq!(final_status["state"], "failed");
    assert_eq!(final_status["progress"]["processed_creators"], 2);
    assert_eq!(final_status["progress"]["succeeded_creators"], 1);
    assert_eq!(final_status["progress"]["failed_creators"], 1);
    assert_eq!(final_status["outcome"]["kind"], "refresh");
    assert_eq!(final_status["outcome"]["succeeded"], 1);
    assert_eq!(final_status["outcome"]["failed"], 1);
    let outcomes = final_status["outcome"]["creator_outcomes"]
        .as_array()
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0]["status"], "succeeded");
    assert_eq!(outcomes[0]["new_uploads_inserted"], 1);
    assert_eq!(outcomes[1]["status"], "failed");

    // Human `job status <ID>` reports target, state, timing, progress, and per-creator outcomes.
    let human_status =
        runner.run_with_db_in_dir(&other_dir, &["job", "status", &job_id.to_string()], &[]);
    assert!(human_status.status.success());
    let human_out = String::from_utf8_lossy(&human_status.stdout);
    assert!(human_out.contains(&format!("Job {job_id}")));
    assert!(human_out.contains("state: failed"));
    assert!(human_out.contains("2/2 creators processed"));
    assert!(human_out.contains("creator 1"));
    assert!(human_out.contains("creator 2"));

    // Verify saved cookies were used for the request and persisted for subsequent refresh.
    assert!(server.requests().iter().any(|r| {
        r.path == "/x/space/wbi/arc/search"
            && r.cookie_header
                .as_deref()
                .is_some_and(|c| c.contains("SESSDATA=sanitized-sessdata-token"))
    }));

    // Alpha's discovered upload is persisted even though Beta failed.
    let uploads = parse_single_json_stdout(&runner.run_with_db(&["--json", "upload", "list"]));
    assert_eq!(uploads["data"]["uploads"].as_array().unwrap().len(), 1);

    // Run a single-creator background refresh for Beta (now succeeding) in human mode.
    fail_beta.store(false, Ordering::SeqCst);
    let single_sub = runner.run_with_db(&["refresh", "--background", "2"]);
    assert!(single_sub.status.success());
    let single_stdout = String::from_utf8_lossy(&single_sub.stdout);
    assert!(single_stdout.contains("Started background refresh job"));
    let job_id_2: i64 = single_stdout
        .split("job ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let done_2 = poll_job_until(&runner, job_id_2, Duration::from_secs(10), |data| {
        data["state"] != "running"
    });
    assert_eq!(done_2["state"], "succeeded");
    assert_eq!(done_2["target"]["creator_id"], 2);
}

#[test]
fn background_download_reports_stages_reliable_totals_and_updates_upload() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_alpha = read_fixture("bilibili/acc_info_946974.json");
    let feed_ts = Timestamp::now().as_unix_seconds() + 100;
    let spi_json = r#"{"code":0,"message":"0","data":{"b_3":"BUVID3","b_4":"BUVID4"}}"#;

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/frontend/finger/spi" => MockHttpResponse::ok_json(spi_json),
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/acc/info" => MockHttpResponse::ok_json(&acc_alpha),
        "/x/space/wbi/arc/search" => MockHttpResponse::ok_json(arc_search_json(&[(
            "BV1stage01",
            101,
            "Stage Test",
            feed_ts,
        )])),
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("siphon.db");
    let runner = CliRunner::new(work_dir.clone(), db_path, server.base_url());

    assert!(
        runner
            .run_with_db(&[
                "--json",
                "creator",
                "add",
                "https://space.bilibili.com/946974"
            ])
            .status
            .success()
    );
    assert!(runner.run_with_db(&["refresh"]).status.success());
    let uploads = parse_single_json_stdout(&runner.run_with_db(&["--json", "upload", "list"]));
    let upload_id = uploads["data"]["uploads"][0]["id"]
        .as_i64()
        .unwrap()
        .to_string();

    // 0. Simulated background download succeeds without confirmed artifacts or marking downloaded.
    let sim_sub = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "download",
        "--background",
        "--upload",
        &upload_id,
        "--",
        "--simulate",
    ]));
    let sim_done = poll_job_until(
        &runner,
        sim_sub["data"]["id"].as_i64().unwrap(),
        Duration::from_secs(10),
        |data| data["state"] != "running",
    );
    assert_eq!(sim_done["state"], "succeeded");
    assert_eq!(sim_done["outcome"]["kind"], "download");
    assert_eq!(sim_done["outcome"]["marked_downloaded"], false);
    assert!(
        sim_done["outcome"]["confirmed_artifacts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let uploads_still_pending =
        parse_single_json_stdout(&runner.run_with_db(&["--json", "upload", "list"]));
    assert_eq!(
        uploads_still_pending["data"]["uploads"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // 1. Tracked upload download with reliable bytes + gated Downloading and PostProcessing stages.
    let ready_dl = temp.path().join("dl.ready");
    let wait_dl = temp.path().join("dl.wait");
    let ready_post = temp.path().join("post.ready");
    let wait_post = temp.path().join("post.wait");
    fs::write(&wait_dl, b"1").unwrap();
    fs::write(&wait_post, b"1").unwrap();
    fs::write(
        work_dir.join(".siphon-stub.cfg"),
        format!(
            "SIPHON_STUB_PROGRESS_BYTES=512:1024\nSIPHON_STUB_READY_FILE={}\nSIPHON_STUB_WAIT_FILE={}\nSIPHON_STUB_POST_READY_FILE={}\nSIPHON_STUB_POST_WAIT_FILE={}\n",
            ready_dl.display(),
            wait_dl.display(),
            ready_post.display(),
            wait_post.display()
        ),
    )
    .unwrap();

    let sub = runner.run_with_db(&["--json", "download", "--background", "--upload", &upload_id]);
    assert!(
        sub.status.success(),
        "{}",
        String::from_utf8_lossy(&sub.stderr)
    );
    let sub_json = parse_single_json_stdout(&sub);
    let job_id = sub_json["data"]["id"].as_i64().unwrap();

    wait_for_file(&ready_dl, Duration::from_secs(10));
    let dl_stage = poll_job_until(&runner, job_id, Duration::from_secs(10), |data| {
        data["progress"]["stage"] == "downloading"
    });
    assert_eq!(dl_stage["state"], "running");
    assert_eq!(dl_stage["progress"]["downloaded_bytes"], 512);
    assert_eq!(dl_stage["progress"]["total_bytes"], 1024);

    fs::remove_file(&wait_dl).unwrap();
    wait_for_file(&ready_post, Duration::from_secs(10));
    let post_stage = poll_job_until(&runner, job_id, Duration::from_secs(10), |data| {
        data["progress"]["stage"] == "post_processing"
    });
    assert_eq!(post_stage["state"], "running");

    fs::remove_file(&wait_post).unwrap();
    let done = poll_job_until(&runner, job_id, Duration::from_secs(10), |data| {
        data["state"] != "running"
    });
    assert_eq!(done["state"], "succeeded");
    assert_eq!(done["progress"]["stage"], "verifying");
    assert_eq!(done["outcome"]["kind"], "download");
    assert_eq!(done["outcome"]["marked_downloaded"], true);
    assert!(
        !done["outcome"]["confirmed_artifacts"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Upload is no longer pending.
    let uploads_after =
        parse_single_json_stdout(&runner.run_with_db(&["--json", "upload", "list"]));
    assert!(
        uploads_after["data"]["uploads"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // 2. Direct URL download with estimated `~` progress leaves `total_bytes` as null (unreliable omitted).
    let ready_est = temp.path().join("est.ready");
    let wait_est = temp.path().join("est.wait");
    fs::write(&wait_est, b"1").unwrap();
    fs::write(
        work_dir.join(".siphon-stub.cfg"),
        format!(
            "SIPHON_STUB_STDOUT_PROGRESS=[download]  42.5% of ~ 10.00MiB at 1.00MiB/s ETA 00:02\nSIPHON_STUB_READY_FILE={}\nSIPHON_STUB_WAIT_FILE={}\n",
            ready_est.display(),
            wait_est.display()
        ),
    )
    .unwrap();
    let est_sub = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "download",
        "--background",
        "https://www.bilibili.com/video/BV1est",
    ]));
    let est_job = est_sub["data"]["id"].as_i64().unwrap();
    wait_for_file(&ready_est, Duration::from_secs(10));
    let est_status = poll_job_until(&runner, est_job, Duration::from_secs(10), |data| {
        data["progress"]["stage"] == "downloading"
    });
    assert!(est_status["progress"]["total_bytes"].is_null());
    assert!(est_status["progress"]["downloaded_bytes"].is_null());
    fs::remove_file(&wait_est).unwrap();
    let est_done = poll_job_until(&runner, est_job, Duration::from_secs(10), |data| {
        data["state"] != "running"
    });
    assert_eq!(est_done["state"], "succeeded");
    assert_eq!(est_done["outcome"]["marked_downloaded"], false);

    // 3. Direct URL download with known downloaded bytes and null total (`700:null`).
    fs::write(
        work_dir.join(".siphon-stub.cfg"),
        "SIPHON_STUB_PROGRESS_BYTES=700:null\n",
    )
    .unwrap();
    let null_total_sub = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "download",
        "--background",
        "https://www.bilibili.com/video/BV1nulltotal",
    ]));
    let null_total_done = poll_job_until(
        &runner,
        null_total_sub["data"]["id"].as_i64().unwrap(),
        Duration::from_secs(10),
        |data| data["state"] != "running",
    );
    assert_eq!(null_total_done["state"], "succeeded");
    assert_eq!(null_total_done["progress"]["downloaded_bytes"], 700);
    assert!(null_total_done["progress"]["total_bytes"].is_null());
}

#[test]
fn background_uses_submission_cwd_and_resolved_inputs_and_sanitizes_history() {
    let temp = TempDir::new().unwrap();
    let dir_a = temp.path().join("submit_dir");
    let dir_b = temp.path().join("query_dir");
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();
    let db_path = temp.path().join("siphon.db");
    let runner = CliRunner::new(dir_a.clone(), db_path.clone(), "http://127.0.0.1:1".into());

    // Create preset `custom_tpl` and set it as default.
    assert!(
        runner
            .run_with_db(&[
                "preset",
                "add",
                "custom_tpl",
                "--",
                "-o",
                "custom_out/%(id)s.%(ext)s"
            ])
            .status
            .success()
    );
    assert!(
        runner
            .run_with_db(&["preset", "set-default", "custom_tpl"])
            .status
            .success()
    );

    let log_path = temp.path().join("stub.log");
    fs::write(
        dir_a.join(".siphon-stub.cfg"),
        format!("SIPHON_STUB_LOG={}\n", log_path.display()),
    )
    .unwrap();
    fs::write(dir_a.join("cookies.txt"), "# Netscape HTTP Cookie File\n.bilibili.com\tTRUE\t/\tFALSE\t0\tSESSDATA\tSECRET_COOKIE_TOKEN\n").unwrap();

    // Gate stub so we can mutate preset/default right after submission ack!
    let ready_file = temp.path().join("sub_inputs.ready");
    let wait_file = temp.path().join("sub_inputs.wait");
    fs::write(&wait_file, b"1").unwrap();

    let raw_url =
        "https://alice:s3cr3tPass@www.bilibili.com/video/BV1subinputs?token=qsecret123#frag";
    let sub_out = runner.run_with_db_in_dir(
        &dir_a,
        &[
            "--json",
            "download",
            "--background",
            raw_url,
            "--cookies",
            "./cookies.txt",
            "--",
            "-P",
            "./rel_dl",
        ],
        &[
            ("SIPHON_STUB_READY_FILE", ready_file.to_str().unwrap()),
            ("SIPHON_STUB_WAIT_FILE", wait_file.to_str().unwrap()),
        ],
    );
    assert!(
        sub_out.status.success(),
        "{}",
        String::from_utf8_lossy(&sub_out.stderr)
    );
    let sub_json = parse_single_json_stdout(&sub_out);
    let job_id = sub_json["data"]["id"].as_i64().unwrap();
    assert_eq!(
        sub_json["data"]["target"]["url"],
        "https://www.bilibili.com/video/BV1subinputs"
    );

    // From dir_b, immediately mutate default preset back to `video` and delete `custom_tpl`.
    assert!(
        runner
            .run_with_db_in_dir(&dir_b, &["preset", "set-default", "video"], &[])
            .status
            .success()
    );
    assert!(
        runner
            .run_with_db_in_dir(&dir_b, &["preset", "remove", "custom_tpl"], &[])
            .status
            .success()
    );

    fs::remove_file(&wait_file).unwrap();
    let done = poll_job_until(&runner, job_id, Duration::from_secs(10), |data| {
        data["state"] != "running"
    });
    assert_eq!(done["state"], "succeeded");

    // Output artifact resolved relative to submission cwd (`dir_a`) using submission-time preset!
    let expected_artifact = dir_a
        .join("rel_dl")
        .join("custom_out")
        .join("BV1subinputs.mp4");
    assert!(
        expected_artifact.is_file(),
        "expected artifact at '{}'",
        expected_artifact.display()
    );

    // Also submit a failing download echoing raw URL + non-target secrets (another URL, Cookie
    // header, cookie path, proxy credentials) in stderr; verify safe error classification only!
    let fail_url =
        "https://alice:s3cr3tPass@www.bilibili.com/video/BV1failurl?token=qsecret123#frag";
    let fail_sub = parse_single_json_stdout(&runner.run_with_db_in_dir(
        &dir_a,
        &["--json", "download", "--background", fail_url],
        &[
            ("SIPHON_STUB_MODE", "fail"),
            (
                "SIPHON_STUB_STDERR",
                &format!(
                    "ERROR: download failed for {fail_url} other=https://cdn.evil/stream?sig=OTHER_URL_SECRET Cookie: SESSDATA=HEADER_SECRET_99 --cookies /tmp/secret/OTHER_COOKIES.txt --proxy http://pxu:PROXY_PASS_77@proxy:8080"
                ),
            ),
        ],
    ));
    let fail_job_id = fail_sub["data"]["id"].as_i64().unwrap();
    let fail_done = poll_job_until(&runner, fail_job_id, Duration::from_secs(10), |data| {
        data["state"] != "running"
    });
    assert_eq!(fail_done["state"], "failed");
    assert_eq!(
        fail_done["outcome"]["error"],
        "[download_process_failed] yt-dlp exited with exit code 1"
    );

    // Inspect SQLite `jobs` rows directly: zero sensitive inputs or raw credentials/tokens retained!
    let conn = Connection::open(&db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT target_url, progress_json, outcome_json FROM jobs")
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .unwrap();
    let mut target_and_progress = String::new();
    let mut all_job_text = String::new();
    for row in rows {
        let (url, prog, out) = row.unwrap();
        if let Some(u) = url {
            target_and_progress.push_str(&u);
            all_job_text.push_str(&u);
        }
        if let Some(p) = prog {
            target_and_progress.push_str(&p);
            all_job_text.push_str(&p);
        }
        if let Some(o) = out {
            all_job_text.push_str(&o);
        }
    }
    let fail_done_text = fail_done.to_string();
    for secret in [
        "alice",
        "s3cr3tPass",
        "qsecret123",
        "#frag",
        "cookies.txt",
        "SECRET_COOKIE_TOKEN",
        "custom_tpl",
        "OTHER_URL_SECRET",
        "cdn.evil",
        "HEADER_SECRET_99",
        "SESSDATA",
        "OTHER_COOKIES.txt",
        "pxu",
        "PROXY_PASS_77",
    ] {
        assert!(
            !all_job_text.contains(secret),
            "persisted jobs table retained sensitive input '{secret}': {all_job_text}"
        );
        assert!(
            !fail_done_text.contains(secret),
            "job status output leaked sensitive input '{secret}': {fail_done_text}"
        );
    }
    for runtime_input in ["rel_dl", dir_a.to_str().unwrap()] {
        assert!(
            !target_and_progress.contains(runtime_input),
            "target/progress retained runtime input '{runtime_input}': {target_and_progress}"
        );
    }
    assert!(all_job_text.contains("https://www.bilibili.com/video/BV1failurl"));

    // Verify worker executed inside submission cwd (`dir_a`) with exact resolved runtime inputs.
    fn unescape_stub_field(raw: &str) -> String {
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

    let stub_log = fs::read_to_string(&log_path).expect("read stub log");
    let first_inv_cwd = stub_log
        .lines()
        .find_map(|line| line.strip_prefix("CWD="))
        .map(unescape_stub_field)
        .expect("stub log should record CWD");
    assert_eq!(
        fs::canonicalize(&first_inv_cwd).unwrap(),
        fs::canonicalize(&dir_a).unwrap()
    );
    let logged_args: Vec<String> = stub_log
        .lines()
        .filter_map(|line| {
            line.strip_prefix("ARG[")
                .and_then(|rest| rest.split_once("]="))
                .map(|(_, val)| unescape_stub_field(val))
        })
        .collect();
    let cookies_pos = logged_args
        .iter()
        .position(|arg| arg == "--cookies")
        .expect("logged --cookies flag");
    assert_eq!(
        fs::canonicalize(&logged_args[cookies_pos + 1]).unwrap(),
        fs::canonicalize(dir_a.join("cookies.txt")).unwrap()
    );
    assert!(
        logged_args
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "custom_out/%(id)s.%(ext)s")
    );
    assert!(
        logged_args
            .windows(2)
            .any(|w| w[0] == "-P" && w[1] == "./rel_dl")
    );
    assert!(logged_args.iter().any(|arg| arg == raw_url));
}

#[test]
fn shared_concurrency_conflicts_and_dead_owner_reconciliation_work_end_to_end() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_alpha = read_fixture("bilibili/acc_info_946974.json");
    let nav_json_fg = nav_json.clone();
    let acc_alpha_fg = acc_alpha.clone();
    let spi_json = r#"{"code":0,"message":"0","data":{"b_3":"BUVID3","b_4":"BUVID4"}}"#;
    let feed_ts = Timestamp::now().as_unix_seconds() + 100;

    let refresh_gate = Arc::new(Gate::new(false));
    let refresh_gate_clone = Arc::clone(&refresh_gate);
    let search_hits = Arc::new(AtomicUsize::new(0));
    let search_hits_clone = Arc::clone(&search_hits);

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/frontend/finger/spi" => MockHttpResponse::ok_json(spi_json),
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/acc/info" => MockHttpResponse::ok_json(&acc_alpha),
        "/x/space/wbi/arc/search" => {
            search_hits_clone.fetch_add(1, Ordering::SeqCst);
            refresh_gate_clone.wait();
            MockHttpResponse::ok_json(arc_search_json(&[
                ("BV1conc01", 101, "Upload 1", feed_ts),
                ("BV1conc02", 102, "Upload 2", feed_ts + 1),
                ("BV1conc03", 103, "Upload 3", feed_ts + 2),
            ]))
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("siphon.db");
    let runner = CliRunner::new(work_dir.clone(), db_path.clone(), server.base_url());

    assert!(
        runner
            .run_with_db(&[
                "--json",
                "creator",
                "add",
                "https://space.bilibili.com/946974"
            ])
            .status
            .success()
    );

    // 1. Background refresh running -> foreground & second background refresh fail with OperationBusy.
    let bg_ref =
        parse_single_json_stdout(&runner.run_with_db(&["--json", "refresh", "--background"]));
    let bg_ref_id = bg_ref["data"]["id"].as_i64().unwrap();
    let bg_ref_pid = bg_ref["data"]["pid"].as_u64().unwrap() as u32;
    refresh_gate.wait_until_entered(Duration::from_secs(10));

    let fg_conflict = runner.run_with_db(&["--json", "refresh"]);
    assert_eq!(fg_conflict.status.code(), Some(1));
    let fg_err = parse_single_json_stdout(&fg_conflict);
    assert_eq!(fg_err["error"]["code"], "operation_busy");
    assert_eq!(fg_err["error"]["details"]["slot"], "refresh");
    assert_eq!(fg_err["error"]["details"]["execution_mode"], "background");
    assert_eq!(fg_err["error"]["details"]["job_id"], bg_ref_id);

    let bg_conflict = runner.run_with_db(&["--json", "refresh", "--background"]);
    assert_eq!(bg_conflict.status.code(), Some(1));
    assert_eq!(
        parse_single_json_stdout(&bg_conflict)["error"]["code"],
        "operation_busy"
    );

    // Direct URL downloads still run concurrently even while refresh is active!
    let direct_1 = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "download",
        "--background",
        "https://www.bilibili.com/video/BV1dir1",
    ]));
    let direct_2 = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "download",
        "--background",
        "https://www.bilibili.com/video/BV1dir2",
    ]));
    assert_eq!(direct_1["ok"], true);
    assert_eq!(direct_2["ok"], true);
    poll_job_until(
        &runner,
        direct_1["data"]["id"].as_i64().unwrap(),
        Duration::from_secs(10),
        |d| d["state"] == "succeeded",
    );
    poll_job_until(
        &runner,
        direct_2["data"]["id"].as_i64().unwrap(),
        Duration::from_secs(10),
        |d| d["state"] == "succeeded",
    );

    // Silent live job with old `updated_at` must stay `running` (never interrupted by inactivity).
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE jobs SET updated_at = updated_at - 86400 WHERE id = ?1",
            [bg_ref_id],
        )
        .unwrap();
    }
    let still_running = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "job",
        "status",
        &bg_ref_id.to_string(),
    ]));
    assert_eq!(still_running["data"]["state"], "running");

    // 2. Kill background refresh worker -> conflicting `refresh --background` reconciles & proceeds.
    kill_process_by_pid(bg_ref_pid);
    refresh_gate.open();
    let recovered_ref =
        parse_single_json_stdout(&runner.run_with_db(&["--json", "refresh", "--background"]));
    assert_eq!(recovered_ref["ok"], true);
    let recovered_id = recovered_ref["data"]["id"].as_i64().unwrap();
    assert!(recovered_id > bg_ref_id);

    let interrupted_ref = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "job",
        "status",
        &bg_ref_id.to_string(),
    ]));
    assert_eq!(interrupted_ref["data"]["state"], "interrupted");
    assert_eq!(interrupted_ref["data"]["outcome"]["kind"], "interrupted");

    let recovered_done = poll_job_until(&runner, recovered_id, Duration::from_secs(10), |d| {
        d["state"] != "running"
    });
    assert_eq!(recovered_done["state"], "succeeded");

    // 3. Shared foreground/background upload download conflict & dead-owner reconciliation via `job status`.
    let uploads = parse_single_json_stdout(&runner.run_with_db(&["--json", "upload", "list"]));
    let arr = uploads["data"]["uploads"].as_array().unwrap();
    assert_eq!(arr.len(), 3);
    let u1 = arr[0]["id"].as_i64().unwrap().to_string();
    let u2 = arr[1]["id"].as_i64().unwrap().to_string();
    let u3 = arr[2]["id"].as_i64().unwrap().to_string();

    let ready_u1 = temp.path().join("u1.ready");
    let wait_u1 = temp.path().join("u1.wait");
    fs::write(&wait_u1, b"1").unwrap();
    let bg_u1 = parse_single_json_stdout(&runner.run_with_db_in_dir(
        &work_dir,
        &["--json", "download", "--background", "--upload", &u1],
        &[
            ("SIPHON_STUB_READY_FILE", ready_u1.to_str().unwrap()),
            ("SIPHON_STUB_WAIT_FILE", wait_u1.to_str().unwrap()),
        ],
    ));
    let bg_u1_id = bg_u1["data"]["id"].as_i64().unwrap();
    let bg_u1_pid = bg_u1["data"]["pid"].as_u64().unwrap() as u32;
    wait_for_file(&ready_u1, Duration::from_secs(10));

    // Same upload in foreground & background both hit OperationBusy.
    let fg_u1_busy = runner.run_with_db(&["--json", "download", "--upload", &u1]);
    assert_eq!(fg_u1_busy.status.code(), Some(1));
    let fg_u1_err = parse_single_json_stdout(&fg_u1_busy);
    assert_eq!(fg_u1_err["error"]["code"], "operation_busy");
    assert_eq!(
        fg_u1_err["error"]["details"]["slot"],
        format!("upload_download:{u1}")
    );

    let bg_u1_busy = runner.run_with_db(&["--json", "download", "--background", "--upload", &u1]);
    assert_eq!(bg_u1_busy.status.code(), Some(1));

    // Distinct upload `u2` succeeds concurrently!
    let bg_u2 = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "download",
        "--background",
        "--upload",
        &u2,
    ]));
    let u2_done = poll_job_until(
        &runner,
        bg_u2["data"]["id"].as_i64().unwrap(),
        Duration::from_secs(10),
        |d| d["state"] != "running",
    );
    assert_eq!(u2_done["state"], "succeeded");

    // Kill ONLY `u1` Siphon worker while real `yt-dlp-stub` child is still live on `wait_u1`.
    kill_process_by_pid(bg_u1_pid);
    #[cfg(unix)]
    {
        // Under Unix detached session, active `yt-dlp-stub` child still holds ownership so
        // concurrent writes to `u1` remain blocked and `job status` stays `running`.
        let conflict_while_child_alive =
            runner.run_with_db(&["--json", "download", "--upload", &u1]);
        assert_eq!(conflict_while_child_alive.status.code(), Some(1));
        assert_eq!(
            parse_single_json_stdout(&conflict_while_child_alive)["error"]["code"],
            "operation_busy"
        );
        let status_while_child_alive = parse_single_json_stdout(&runner.run_with_db(&[
            "--json",
            "job",
            "status",
            &bg_u1_id.to_string(),
        ]));
        assert_eq!(status_while_child_alive["data"]["state"], "running");
        fs::remove_file(&wait_u1).unwrap();
    }
    #[cfg(windows)]
    {
        let _ = fs::remove_file(&wait_u1);
    }

    // Once active downloader process tree is gone, `job status <ID>` marks `interrupted` and
    // frees the `upload_download:u1` slot so retry succeeds cleanly.
    let u1_interrupted = poll_job_until(&runner, bg_u1_id, Duration::from_secs(10), |d| {
        d["state"] != "running"
    });
    assert_eq!(u1_interrupted["state"], "interrupted");

    // Resubmission succeeds now that slot is released.
    let u1_retry = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "download",
        "--background",
        "--upload",
        &u1,
    ]));
    let u1_done = poll_job_until(
        &runner,
        u1_retry["data"]["id"].as_i64().unwrap(),
        Duration::from_secs(10),
        |d| d["state"] != "running",
    );
    assert_eq!(u1_done["state"], "succeeded");

    // 4. Foreground refresh holds slot -> background sees `execution_mode == "foreground"`,
    //    and killing foreground process lets background reconcile & run.
    let fg_gate = Arc::new(Gate::new(false));
    let fg_gate_clone = Arc::clone(&fg_gate);
    let fg_server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/frontend/finger/spi" => MockHttpResponse::ok_json(spi_json),
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json_fg),
        "/x/space/wbi/acc/info" => MockHttpResponse::ok_json(&acc_alpha_fg),
        "/x/space/wbi/arc/search" => {
            fg_gate_clone.wait();
            MockHttpResponse::ok_json(arc_search_json(&[]))
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });
    let fg_runner = CliRunner::new(work_dir, db_path, fg_server.base_url());
    let mut fg_child = fg_runner
        .command_with_db(&["refresh"], &[])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn foreground refresh");
    let fg_pid = fg_child.id();
    fg_gate.wait_until_entered(Duration::from_secs(10));

    let bg_during_fg = fg_runner.run_with_db(&["--json", "refresh", "--background"]);
    assert_eq!(bg_during_fg.status.code(), Some(1));
    let bg_during_fg_json = parse_single_json_stdout(&bg_during_fg);
    assert_eq!(bg_during_fg_json["error"]["code"], "operation_busy");
    assert_eq!(
        bg_during_fg_json["error"]["details"]["execution_mode"],
        "foreground"
    );
    assert!(bg_during_fg_json["error"]["details"]["job_id"].is_null());
    assert_eq!(bg_during_fg_json["error"]["details"]["pid"], fg_pid);

    // Kill foreground process (and reap it) -> background reconciles the dead foreground slot!
    let _ = fg_child.kill();
    let _ = fg_child.wait();
    fg_gate.open();

    let bg_after_dead_fg =
        parse_single_json_stdout(&fg_runner.run_with_db(&["--json", "refresh", "--background"]));
    assert_eq!(bg_after_dead_fg["ok"], true);
    let after_done = poll_job_until(
        &fg_runner,
        bg_after_dead_fg["data"]["id"].as_i64().unwrap(),
        Duration::from_secs(10),
        |d| d["state"] != "running",
    );
    assert_eq!(after_done["state"], "succeeded");

    // 5. Foreground managed upload (`--upload <u3>`) tracks its `yt-dlp-stub` child process tree:
    //    killing ONLY the foreground `siphon` keeps ownership while `yt-dlp-stub` is still alive
    //    on Unix (and tears down the child Job Object on Windows), then cleanly allows retry.
    let ready_fg_u3 = temp.path().join("fg_u3.ready");
    let wait_fg_u3 = temp.path().join("fg_u3.wait");
    fs::write(&wait_fg_u3, b"1").unwrap();
    let mut fg_dl_child = fg_runner
        .command_with_db(
            &["--json", "download", "--upload", &u3],
            &[
                ("SIPHON_STUB_READY_FILE", ready_fg_u3.to_str().unwrap()),
                ("SIPHON_STUB_WAIT_FILE", wait_fg_u3.to_str().unwrap()),
            ],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn foreground managed download");
    let fg_dl_pid = fg_dl_child.id();
    wait_for_file(&ready_fg_u3, Duration::from_secs(10));

    let bg_u3_busy =
        fg_runner.run_with_db(&["--json", "download", "--background", "--upload", &u3]);
    assert_eq!(bg_u3_busy.status.code(), Some(1));
    let bg_u3_err = parse_single_json_stdout(&bg_u3_busy);
    assert_eq!(bg_u3_err["error"]["code"], "operation_busy");
    assert_eq!(
        bg_u3_err["error"]["details"]["execution_mode"],
        "foreground"
    );
    assert_eq!(bg_u3_err["error"]["details"]["pid"], fg_dl_pid);

    let _ = fg_dl_child.kill();
    let _ = fg_dl_child.wait();
    #[cfg(unix)]
    {
        let busy_while_child_alive =
            fg_runner.run_with_db(&["--json", "download", "--upload", &u3]);
        assert_eq!(busy_while_child_alive.status.code(), Some(1));
        assert_eq!(
            parse_single_json_stdout(&busy_while_child_alive)["error"]["code"],
            "operation_busy"
        );
        fs::remove_file(&wait_fg_u3).unwrap();
    }
    #[cfg(windows)]
    {
        let _ = fs::remove_file(&wait_fg_u3);
    }

    let retry_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let retry_out = fg_runner.run_with_db(&["--json", "download", "--upload", &u3]);
        if retry_out.status.success() {
            let json = parse_single_json_stdout(&retry_out);
            assert_eq!(json["data"]["marked_downloaded"], true);
            break;
        }
        let err_json = parse_single_json_stdout(&retry_out);
        assert_eq!(
            err_json["error"]["code"], "operation_busy",
            "unexpected retry error: {err_json}"
        );
        assert!(
            Instant::now() < retry_deadline,
            "timed out waiting for dead foreground child process cleanup"
        );
        thread::sleep(Duration::from_millis(50));
    }

    // 6. Direct URL background job killed mid-flight preserves ownership until `yt-dlp-stub` exits,
    //    then reconciles cleanly to `interrupted`.
    let ready_direct = temp.path().join("direct_kill.ready");
    let wait_direct = temp.path().join("direct_kill.wait");
    fs::write(&wait_direct, b"1").unwrap();
    let direct_sub = parse_single_json_stdout(&fg_runner.run_with_db_in_dir(
        &fg_runner.work_dir,
        &[
            "--json",
            "download",
            "--background",
            "https://www.bilibili.com/video/BV1directkill",
        ],
        &[
            ("SIPHON_STUB_READY_FILE", ready_direct.to_str().unwrap()),
            ("SIPHON_STUB_WAIT_FILE", wait_direct.to_str().unwrap()),
        ],
    ));
    let direct_id = direct_sub["data"]["id"].as_i64().unwrap();
    let direct_pid = direct_sub["data"]["pid"].as_u64().unwrap() as u32;
    wait_for_file(&ready_direct, Duration::from_secs(10));
    kill_process_by_pid(direct_pid);
    #[cfg(unix)]
    {
        let busy_status = parse_single_json_stdout(&fg_runner.run_with_db(&[
            "--json",
            "job",
            "status",
            &direct_id.to_string(),
        ]));
        assert_eq!(busy_status["data"]["state"], "running");
        fs::remove_file(&wait_direct).unwrap();
    }
    #[cfg(windows)]
    {
        let _ = fs::remove_file(&wait_direct);
    }
    let direct_interrupted = poll_job_until(&fg_runner, direct_id, Duration::from_secs(10), |d| {
        d["state"] != "running"
    });
    assert_eq!(direct_interrupted["state"], "interrupted");
}

#[test]
fn background_rejects_media_to_stdout_and_preserves_foreground_passthrough() {
    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("siphon.db");
    let log_path = temp.path().join("stub.log");
    fs::write(
        work_dir.join(".siphon-stub.cfg"),
        format!("SIPHON_STUB_LOG={}\n", log_path.display()),
    )
    .unwrap();
    let runner = CliRunner::new(
        work_dir.clone(),
        db_path.clone(),
        "http://127.0.0.1:1".into(),
    );

    assert!(
        runner
            .run_with_db(&["preset", "add", "pipe_out", "--", "-o", "-"])
            .status
            .success()
    );

    // Background rejects CLI `-o -`, preset `-o -`, `--config-locations -`, and `yt-dlp.conf -o -`.
    for bad_args in [
        &[
            "--json",
            "download",
            "--background",
            "https://www.bilibili.com/video/BV1p1",
            "--",
            "-o",
            "-",
        ][..],
        &[
            "--json",
            "download",
            "--background",
            "https://www.bilibili.com/video/BV1p2",
            "--",
            "-o",
            "chapter:-",
            "-o",
            "ok.mp4",
        ],
        &[
            "--json",
            "download",
            "--background",
            "https://www.bilibili.com/video/BV1p3",
            "--preset",
            "pipe_out",
        ],
        &[
            "--json",
            "download",
            "--background",
            "https://www.bilibili.com/video/BV1p4",
            "--",
            "--config-locations",
            "-",
        ],
    ] {
        let out = runner.run_with_db(bad_args);
        assert_eq!(out.status.code(), Some(1), "args={bad_args:?}");
        let json = parse_single_json_stdout(&out);
        assert_eq!(json["ok"], false);
        assert!(
            json["error"]["code"] == "media_to_stdout_not_allowed"
                || json["error"]["code"] == "invalid_download_arguments"
        );
    }

    fs::write(work_dir.join("yt-dlp.conf"), "-o -\n").unwrap();
    let conf_out = runner.run_with_db(&[
        "download",
        "--background",
        "https://www.bilibili.com/video/BV1p5",
    ]);
    assert_eq!(conf_out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&conf_out.stderr).contains("streaming media to stdout"),
        "{}",
        String::from_utf8_lossy(&conf_out.stderr)
    );
    fs::remove_file(work_dir.join("yt-dlp.conf")).unwrap();

    // Zero stub invocations & zero jobs created by rejected background submissions.
    assert!(!log_path.exists());
    let conn = Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);

    // Ordinary foreground passthrough (`-J` and `--config-locations -`) remains completely intact.
    let fg_j = runner.run_with_db(&[
        "download",
        "https://www.bilibili.com/video/BV1fgjson",
        "--no-preset",
        "--",
        "-J",
    ]);
    assert!(fg_j.status.success());
    let parsed: Value = serde_json::from_str(String::from_utf8_lossy(&fg_j.stdout).trim()).unwrap();
    assert_eq!(parsed["id"], "BV1fgjson");
}

#[test]
fn startup_failures_return_command_errors_without_orphaned_jobs() {
    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("siphon.db");
    let runner = CliRunner::new(
        work_dir.clone(),
        db_path.clone(),
        "http://127.0.0.1:1".into(),
    );

    // 1. CLI preflight startup failures (missing upload, missing preset, missing creator, invalid URL).
    for (args, expected_code) in [
        (
            &["--json", "download", "--background", "--upload", "999"][..],
            "upload_not_found",
        ),
        (
            &[
                "--json",
                "download",
                "--background",
                "https://example.com/v",
                "--preset",
                "nope",
            ],
            "preset_not_found",
        ),
        (
            &["--json", "refresh", "--background", "999"],
            "creator_not_found",
        ),
        (
            &["--json", "download", "--background", "not-a-valid-url"],
            "invalid_display_url",
        ),
        (&["--json", "job", "status", "999"], "job_not_found"),
    ] {
        let out = runner.run_with_db(args);
        assert_eq!(out.status.code(), Some(1), "args={args:?}");
        let json = parse_single_json_stdout(&out);
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], expected_code);
    }

    // 2. Unwritable DB path (parent path is a regular file) returns storage error cleanly.
    let file_parent = temp.path().join("not_a_dir");
    fs::write(&file_parent, b"blocker").unwrap();
    let bad_db = file_parent.join("siphon.db");
    let bad_db_out = runner
        .command_in_dir(
            &work_dir,
            &[
                "--db",
                bad_db.to_str().unwrap(),
                "--json",
                "download",
                "--background",
                "https://example.com/v",
            ],
            &[],
        )
        .output()
        .unwrap();
    assert_eq!(bad_db_out.status.code(), Some(1));
    assert_eq!(
        parse_single_json_stdout(&bad_db_out)["error"]["code"],
        "storage_backend_error"
    );

    // 3. Worker binary / handshake startup failures via BackgroundRuntime + compiled helper binaries.
    let storage = SqliteStorage::open(&db_path).unwrap();
    let refresh_src = BilibiliRefreshSource::new();
    let downloader = YtDlpDownloader::new();
    let services = AppServices {
        storage: &storage,
        refresh_source: &refresh_src,
        downloader: &downloader,
    };

    // 3a. Missing worker executable -> WorkerSpawnFailed.
    let missing_rt =
        BackgroundRuntime::default().with_worker_exe(temp.path().join("no-such-worker"));
    let cli = Cli::try_parse_from([
        "siphon",
        "download",
        "--background",
        "https://example.com/vid",
    ])
    .unwrap();
    let spawn_fail = execute_with_runtime(
        cli.clone(),
        &services,
        AppExecuteOptions::default(),
        &missing_rt,
    );
    assert!(matches!(
        spawn_fail.error,
        Some(siphon::error::AppError::Background(
            BackgroundError::WorkerSpawnFailed { .. }
        ))
    ));

    // Compile tiny helper binary for crash / garbage ack / ack-timeout modes.
    let helper_src = temp.path().join("bad_worker.rs");
    let helper_bin = temp
        .path()
        .join(format!("bad_worker{}", env::consts::EXE_SUFFIX));
    fs::write(
        &helper_src,
        r#"
use std::env;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        Some("crash") => {
            let mut buf = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut buf);
            ExitCode::from(42)
        }
        Some("bad_ack") => {
            let mut buf = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut buf);
            println!("not-json-ack");
            ExitCode::SUCCESS
        }
        Some("hang") => {
            let mut buf = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut buf);
            thread::sleep(Duration::from_secs(10));
            ExitCode::SUCCESS
        }
        Some("hang_no_read") => {
            thread::sleep(Duration::from_secs(10));
            ExitCode::SUCCESS
        }
        Some("gate") => {
            let args: Vec<String> = env::args().collect();
            let ready = &args[2];
            let wait = &args[3];
            let target = &args[4];
            let target_args = &args[5..];
            fs::write(ready, b"ready").unwrap();
            let start = Instant::now();
            while Path::new(wait).exists() && start.elapsed() < Duration::from_secs(30) {
                thread::sleep(Duration::from_millis(20));
            }
            let status = Command::new(target).args(target_args).status().unwrap();
            ExitCode::from(status.code().unwrap_or(1) as u8)
        }
        _ => ExitCode::FAILURE,
    }
}
"#,
    )
    .unwrap();
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let compile = Command::new(rustc)
        .arg("--edition=2024")
        .arg(&helper_src)
        .arg("-o")
        .arg(&helper_bin)
        .output()
        .unwrap();
    assert!(compile.status.success());

    // 3b. Worker exits before ack -> WorkerStartupFailed.
    let crash_rt = BackgroundRuntime::default()
        .with_worker_exe(&helper_bin)
        .with_worker_args(vec!["crash".to_owned()]);
    let crash_exec = execute_with_runtime(
        cli.clone(),
        &services,
        AppExecuteOptions::default(),
        &crash_rt,
    );
    assert!(matches!(
        crash_exec.error,
        Some(siphon::error::AppError::Background(
            BackgroundError::WorkerStartupFailed { .. }
        ))
    ));

    // 3c. Worker emits invalid ack -> WorkerStartupFailed.
    let bad_ack_rt = BackgroundRuntime::default()
        .with_worker_exe(&helper_bin)
        .with_worker_args(vec!["bad_ack".to_owned()]);
    let bad_ack_exec = execute_with_runtime(
        cli.clone(),
        &services,
        AppExecuteOptions::default(),
        &bad_ack_rt,
    );
    assert!(matches!(
        bad_ack_exec.error,
        Some(siphon::error::AppError::Background(
            BackgroundError::WorkerStartupFailed { .. }
        ))
    ));

    // 3d. Worker hangs past ack timeout -> WorkerAckTimeout.
    let timeout_rt = BackgroundRuntime::default()
        .with_worker_exe(&helper_bin)
        .with_worker_args(vec!["hang".to_owned()])
        .with_ack_timeout(Duration::from_millis(150));
    let timeout_exec = execute_with_runtime(
        cli.clone(),
        &services,
        AppExecuteOptions::default(),
        &timeout_rt,
    );
    assert!(matches!(
        timeout_exec.error,
        Some(siphon::error::AppError::Background(
            BackgroundError::WorkerAckTimeout
        ))
    ));

    // 3e. Worker that never drains stdin hits WorkerAckTimeout even when launch payload exceeds
    //     OS pipe buffer (> 256 KiB), proving timeout covers writing stdin as well as reading ack.
    let big_arg = "x".repeat(300 * 1024);
    let big_cli = Cli::try_parse_from([
        "siphon",
        "download",
        "--background",
        "https://example.com/big",
        "--",
        &big_arg,
    ])
    .unwrap();
    let no_read_rt = BackgroundRuntime::default()
        .with_worker_exe(&helper_bin)
        .with_worker_args(vec!["hang_no_read".to_owned()])
        .with_ack_timeout(Duration::from_millis(200));
    let started_write = Instant::now();
    let no_read_exec = execute_with_runtime(
        big_cli,
        &services,
        AppExecuteOptions::default(),
        &no_read_rt,
    );
    assert!(
        started_write.elapsed() < Duration::from_secs(4),
        "stdin write must respect startup timeout"
    );
    assert!(matches!(
        no_read_exec.error,
        Some(siphon::error::AppError::Background(
            BackgroundError::WorkerAckTimeout
        ))
    ));

    // 3f. In-memory storage cannot run background workers -> UnsupportedStorage.
    let mem_storage = SqliteStorage::open_in_memory().unwrap();
    let mem_services = AppServices {
        storage: &mem_storage,
        refresh_source: &refresh_src,
        downloader: &downloader,
    };
    let mem_exec = execute_with_runtime(
        cli,
        &mem_services,
        AppExecuteOptions::default(),
        &BackgroundRuntime::default(),
    );
    assert!(matches!(
        mem_exec.error,
        Some(siphon::error::AppError::Background(
            BackgroundError::UnsupportedStorage
        ))
    ));

    // Confirm zero orphaned job rows exist in `db_path`.
    drop(storage);
    let conn = Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn worker_revalidates_state_after_claiming_slot_and_releases_on_startup_failure() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_alpha = read_fixture("bilibili/acc_info_946974.json");
    let spi_json = r#"{"code":0,"message":"0","data":{"b_3":"BUVID3","b_4":"BUVID4"}}"#;
    let base_ts = Timestamp::now().as_unix_seconds() + 100;
    let phase = Arc::new(AtomicUsize::new(0));
    let phase_clone = Arc::clone(&phase);

    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/frontend/finger/spi" => MockHttpResponse::ok_json(spi_json),
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/acc/info" => MockHttpResponse::ok_json(&acc_alpha),
        "/x/space/wbi/arc/search" => {
            let body = if phase_clone.load(Ordering::SeqCst) == 0 {
                arc_search_json(&[("BV1race01", 101, "Race 1", base_ts + 10)])
            } else {
                arc_search_json(&[
                    ("BV1race02", 102, "Race 2", base_ts + 20),
                    ("BV1race01", 101, "Race 1", base_ts + 10),
                ])
            };
            MockHttpResponse::ok_json(body)
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("siphon.db");
    let runner = CliRunner::new(work_dir.clone(), db_path.clone(), server.base_url());
    let siphon_bin = PathBuf::from(env!("CARGO_BIN_EXE_siphon"));

    let gate_src = temp.path().join("gate_worker.rs");
    let gate_bin = temp
        .path()
        .join(format!("gate_worker{}", env::consts::EXE_SUFFIX));
    fs::write(
        &gate_src,
        r#"
use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let ready = &args[1];
    let wait = &args[2];
    let target = &args[3];
    let target_args = &args[4..];
    fs::write(ready, b"ready").unwrap();
    let start = Instant::now();
    while Path::new(wait).exists() && start.elapsed() < Duration::from_secs(30) {
        thread::sleep(Duration::from_millis(20));
    }
    let status = Command::new(target).args(target_args).status().unwrap();
    ExitCode::from(status.code().unwrap_or(1) as u8)
}
"#,
    )
    .unwrap();
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let compile = Command::new(rustc)
        .arg("--edition=2024")
        .arg(&gate_src)
        .arg("-o")
        .arg(&gate_bin)
        .output()
        .unwrap();
    assert!(compile.status.success());

    assert!(
        runner
            .run_with_db(&[
                "--json",
                "creator",
                "add",
                "https://space.bilibili.com/946974"
            ])
            .status
            .success()
    );
    assert!(runner.run_with_db(&["--json", "refresh"]).status.success());
    let uploads = parse_single_json_stdout(&runner.run_with_db(&["--json", "upload", "list"]));
    let upload_id = uploads["data"]["uploads"][0]["id"].as_i64().unwrap();

    // 1. Background download spawned while upload is `pending`, paused BEFORE claiming slot;
    //    meanwhile foreground downloads and completes the upload -> worker claims slot, sees
    //    UploadNotPending, releases slot and fails startup cleanly without double-downloading.
    let dl_ready = temp.path().join("dl_gate.ready");
    let dl_wait = temp.path().join("dl_gate.wait");
    fs::write(&dl_wait, b"1").unwrap();
    let dl_db = db_path.clone();
    let dl_gate = gate_bin.clone();
    let dl_siphon = siphon_bin.clone();
    let dl_ready_clone = dl_ready.clone();
    let dl_wait_clone = dl_wait.clone();
    let dl_handle = thread::spawn(move || {
        let storage = SqliteStorage::open(&dl_db).unwrap();
        let refresh_src = BilibiliRefreshSource::new();
        let downloader = YtDlpDownloader::new();
        let services = AppServices {
            storage: &storage,
            refresh_source: &refresh_src,
            downloader: &downloader,
        };
        let cli = Cli::try_parse_from([
            "siphon",
            "download",
            "--background",
            "--upload",
            &upload_id.to_string(),
        ])
        .unwrap();
        let rt = BackgroundRuntime::default()
            .with_worker_exe(&dl_gate)
            .with_worker_args(vec![
                dl_ready_clone.to_string_lossy().into_owned(),
                dl_wait_clone.to_string_lossy().into_owned(),
                dl_siphon.to_string_lossy().into_owned(),
                "__worker".to_owned(),
            ]);
        execute_with_runtime(cli, &services, AppExecuteOptions::default(), &rt)
    });

    wait_for_file(&dl_ready, Duration::from_secs(10));
    let fg_dl = runner.run_with_db(&["--json", "download", "--upload", &upload_id.to_string()]);
    assert!(
        fg_dl.status.success(),
        "{}",
        String::from_utf8_lossy(&fg_dl.stderr)
    );
    fs::remove_file(&dl_wait).unwrap();

    let bg_dl_exec = dl_handle.join().unwrap();
    assert!(matches!(
        bg_dl_exec.error,
        Some(AppError::Storage(StorageError::UploadNotPending {
            status: UploadStatus::Downloaded,
            ..
        }))
    ));
    let conn = Connection::open(&db_path).unwrap();
    let active_res: i64 = conn
        .query_row("SELECT COUNT(*) FROM operation_reservations", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(active_res, 0);
    let running_jobs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM jobs WHERE state = 'running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(running_jobs, 0);
    drop(conn);

    // 2. Background refresh spawned at checkpoint T1, paused BEFORE claiming Refresh slot;
    //    meanwhile foreground refresh advances checkpoint to T2 -> worker resolves targets &
    //    checkpoints AFTER taking Refresh slot and observes `previous_checkpoint == T2`.
    phase.store(1, Ordering::SeqCst);
    let ref_ready = temp.path().join("ref_gate.ready");
    let ref_wait = temp.path().join("ref_gate.wait");
    fs::write(&ref_wait, b"1").unwrap();
    let ref_db = db_path.clone();
    let ref_api = server.base_url();
    let ref_ready_clone = ref_ready.clone();
    let ref_wait_clone = ref_wait.clone();
    let ref_handle = thread::spawn(move || {
        let storage = SqliteStorage::open(&ref_db).unwrap();
        let refresh_src = BilibiliRefreshSource::new();
        let downloader = YtDlpDownloader::new();
        let services = AppServices {
            storage: &storage,
            refresh_source: &refresh_src,
            downloader: &downloader,
        };
        let cli = Cli::try_parse_from(["siphon", "refresh", "--background"]).unwrap();
        let rt = BackgroundRuntime::with_api_base(Some(ref_api))
            .with_worker_exe(&gate_bin)
            .with_worker_args(vec![
                ref_ready_clone.to_string_lossy().into_owned(),
                ref_wait_clone.to_string_lossy().into_owned(),
                siphon_bin.to_string_lossy().into_owned(),
                "__worker".to_owned(),
            ]);
        execute_with_runtime(cli, &services, AppExecuteOptions::default(), &rt)
    });

    wait_for_file(&ref_ready, Duration::from_secs(10));
    // Foreground refresh commits BV1race02 at `base_ts + 20`.
    assert!(runner.run_with_db(&["--json", "refresh"]).status.success());
    fs::remove_file(&ref_wait).unwrap();

    let bg_ref_exec = ref_handle.join().unwrap();
    assert_eq!(bg_ref_exec.error, None);
    let bg_ref_job = match bg_ref_exec.result {
        Some(CommandResult::JobStarted(job)) => job,
        other => panic!("expected JobStarted, got {other:?}"),
    };
    let bg_ref_done = poll_job_until(
        &runner,
        bg_ref_job.id.as_i64(),
        Duration::from_secs(10),
        |d| d["state"] != "running",
    );
    assert_eq!(bg_ref_done["state"], "succeeded");
    let creator_outcome = &bg_ref_done["outcome"]["creator_outcomes"][0];
    assert_eq!(
        creator_outcome["previous_checkpoint"],
        Timestamp::from_unix_seconds(base_ts + 20).format_utc_iso()
    );
    assert_eq!(creator_outcome["new_uploads_inserted"], 0);
}

#[cfg(unix)]
mod unix_pty_support {
    use core::ffi::{c_char, c_int, c_ulong, c_void};
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const F_SETFD: c_int = 2;
    const FD_CLOEXEC: c_int = 1;
    const ESRCH: i32 = 3;

    #[cfg(target_os = "linux")]
    const TIOCSCTTY: c_ulong = 0x540E;
    #[cfg(target_os = "macos")]
    const TIOCSCTTY: c_ulong = 0x2000_7461;

    unsafe extern "C" {
        fn openpty(
            amaster: *mut c_int,
            aslave: *mut c_int,
            name: *mut c_char,
            termp: *const c_void,
            winp: *const c_void,
        ) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        fn setsid() -> c_int;
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn tcsetpgrp(fd: c_int, pgrp: c_int) -> c_int;
        fn getpgrp() -> c_int;
        fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
        fn kill(pid: c_int, sig: c_int) -> c_int;
    }

    pub fn spawn_under_pty(mut cmd: Command) -> (Child, File, JoinHandle<Vec<u8>>) {
        let mut master_raw: c_int = -1;
        let mut slave_raw: c_int = -1;
        // SAFETY: `openpty` initializes two valid PTY file descriptors when returning `0`.
        assert_eq!(
            unsafe {
                openpty(
                    &mut master_raw,
                    &mut slave_raw,
                    core::ptr::null_mut(),
                    core::ptr::null(),
                    core::ptr::null(),
                )
            },
            0,
            "openpty failed: {}",
            io::Error::last_os_error()
        );
        // SAFETY: newly allocated descriptors from `openpty`.
        let master_fd = unsafe { OwnedFd::from_raw_fd(master_raw) };
        let slave_fd = unsafe { OwnedFd::from_raw_fd(slave_raw) };

        for fd in [master_fd.as_raw_fd(), slave_fd.as_raw_fd()] {
            // SAFETY: set close-on-exec so only `dup2`'d `0/1/2` cross `exec` into child.
            assert_eq!(
                unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) },
                0,
                "fcntl FD_CLOEXEC failed: {}",
                io::Error::last_os_error()
            );
        }

        let slave_raw = slave_fd.as_raw_fd();
        // SAFETY: `pre_exec` runs between `fork` and `exec` inside the child process.
        unsafe {
            cmd.pre_exec(move || {
                if setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if ioctl(slave_raw, TIOCSCTTY, 0 as c_int) == -1 {
                    return Err(io::Error::last_os_error());
                }
                let pgrp = getpgrp();
                if tcsetpgrp(slave_raw, pgrp) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if dup2(slave_raw, 0) == -1 || dup2(slave_raw, 1) == -1 || dup2(slave_raw, 2) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn().expect("spawn CLI process attached to PTY");
        drop(slave_fd);

        let master_writer = File::from(master_fd);
        let mut master_reader = master_writer.try_clone().expect("clone PTY master");
        let drain_handle = thread::spawn(move || {
            let mut out = Vec::new();
            let mut buf = [0u8; 1024];
            while let Ok(n) = master_reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out
        });

        (child, master_writer, drain_handle)
    }

    pub fn wait_child_bounded(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().expect("poll child status") {
                return status;
            }
            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!("timed out after {timeout:?} waiting for PTY child exit");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn wait_pid_gone(pid: u32, timeout: Duration) {
        let raw = i32::try_from(pid).expect("positive pid");
        let start = Instant::now();
        while start.elapsed() < timeout {
            // SAFETY: `kill(raw, 0)` checks existence without sending a signal.
            let rc = unsafe { kill(raw, 0) };
            if rc == -1 && io::Error::last_os_error().raw_os_error() == Some(ESRCH) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("process {pid} stayed alive longer than {timeout:?}");
    }
}

fn seed_single_upload_runner(
    work_dir: PathBuf,
    db_path: PathBuf,
    bvid: &'static str,
    title: &'static str,
) -> (CliRunner, MockBilibiliServer) {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_json = read_fixture("bilibili/acc_info_946974.json");
    let spi_json = r#"{"code":0,"message":"0","data":{"b_3":"BUVID3","b_4":"BUVID4"}}"#;
    let feed_ts = Timestamp::now().as_unix_seconds() + 100;
    let server = MockBilibiliServer::start(move |req| match req.path.as_str() {
        "/x/frontend/finger/spi" => MockHttpResponse::ok_json(spi_json),
        "/x/web-interface/nav" => MockHttpResponse::ok_json(&nav_json),
        "/x/space/wbi/acc/info" => MockHttpResponse::ok_json(&acc_json),
        "/x/space/wbi/arc/search" => {
            MockHttpResponse::ok_json(arc_search_json(&[(bvid, 1001, title, feed_ts)]))
        }
        _ => MockHttpResponse::status(404, "unknown"),
    });
    let runner = CliRunner::new(work_dir, db_path, server.base_url());
    assert!(
        runner
            .run_with_db(&[
                "--json",
                "creator",
                "add",
                "https://space.bilibili.com/946974",
            ])
            .status
            .success()
    );
    assert!(runner.run_with_db(&["--json", "refresh"]).status.success());
    (runner, server)
}

#[cfg(unix)]
#[test]
fn foreground_managed_upload_preserves_tty_read_without_sigttin() {
    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("siphon.db");
    let (runner, _server) =
        seed_single_upload_runner(work_dir, db_path, "BV1ptyread", "PTY Read Video");

    let ready_file = temp.path().join("pty_read.ready");
    let cmd = runner.command_with_db(
        &["download", "--upload", "1", "--no-preset"],
        &[
            ("SIPHON_STUB_READY_FILE", ready_file.to_str().unwrap()),
            ("SIPHON_STUB_READ_STDIN_EXPECT", "confirm"),
        ],
    );
    let (mut child, mut master, drain) = unix_pty_support::spawn_under_pty(cmd);
    wait_for_file(&ready_file, Duration::from_secs(10));
    master.write_all(b"confirm\n").unwrap();
    master.flush().unwrap();

    let status = unix_pty_support::wait_child_bounded(&mut child, Duration::from_secs(10));
    drop(master);
    let pty_output = drain.join().unwrap();
    assert!(
        status.success(),
        "foreground download under PTY failed (status={status:?}): {}",
        String::from_utf8_lossy(&pty_output)
    );

    let pending = parse_single_json_stdout(&runner.run_with_db(&["--json", "upload", "list"]));
    assert_eq!(pending["data"]["uploads"].as_array().unwrap().len(), 0);
}

#[cfg(unix)]
#[test]
fn foreground_managed_upload_receives_pty_ctrl_c() {
    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("siphon.db");
    let (runner, _server) =
        seed_single_upload_runner(work_dir, db_path, "BV1ptyctrlc", "PTY CtrlC Video");

    let ready_file = temp.path().join("pty_ctrlc.ready");
    let pid_file = temp.path().join("pty_ctrlc.pid");
    let wait_file = temp.path().join("pty_ctrlc.wait");
    fs::write(&wait_file, b"1").unwrap();

    let cmd = runner.command_with_db(
        &["download", "--upload", "1", "--no-preset"],
        &[
            ("SIPHON_STUB_PID_FILE", pid_file.to_str().unwrap()),
            ("SIPHON_STUB_READY_FILE", ready_file.to_str().unwrap()),
            ("SIPHON_STUB_WAIT_FILE", wait_file.to_str().unwrap()),
        ],
    );
    let (mut child, mut master, drain) = unix_pty_support::spawn_under_pty(cmd);
    wait_for_file(&ready_file, Duration::from_secs(10));
    let stub_pid: u32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .expect("stub pid");

    // Send Ctrl-C (`VINTR`) on the controlling PTY; both `siphon` and `yt-dlp-stub` must
    // belong to the foreground process group and terminate immediately.
    master.write_all(&[0x03]).unwrap();
    master.flush().unwrap();

    let status = unix_pty_support::wait_child_bounded(&mut child, Duration::from_secs(5));
    assert!(!status.success(), "Ctrl-C should interrupt download");
    unix_pty_support::wait_pid_gone(stub_pid, Duration::from_secs(3));
    drop(master);
    let _ = drain.join();
    let _ = fs::remove_file(&wait_file);

    // Slot was cleanly released and a subsequent download succeeds.
    let retry = runner.run_with_db(&["--json", "download", "--upload", "1", "--no-preset"]);
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let retry_json = parse_single_json_stdout(&retry);
    assert_eq!(retry_json["data"]["marked_downloaded"], true);
}

#[test]
fn reconciliation_tracks_child_across_database_path_aliases() {
    let temp = TempDir::new().unwrap();
    let real_dir = temp.path().join("real");
    let nested_dir = real_dir.join("nested");
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&nested_dir).unwrap();
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = real_dir.join("siphon.db");

    let (runner_real, server) = seed_single_upload_runner(
        work_dir.clone(),
        db_path,
        "BV1alias1",
        "Alias Tracked Video",
    );

    #[cfg(unix)]
    let alias_db = {
        let symlink_dir = temp.path().join("symlink_dir");
        std::os::unix::fs::symlink(&real_dir, &symlink_dir).unwrap();
        symlink_dir.join("siphon.db")
    };
    #[cfg(windows)]
    let alias_db = nested_dir.join("..").join("SIPHON.DB");
    #[cfg(not(any(unix, windows)))]
    let alias_db = nested_dir.join("..").join("siphon.db");

    let runner_alias = CliRunner::new(work_dir, alias_db, server.base_url());
    let ready = temp.path().join("alias.ready");
    let wait = temp.path().join("alias.wait");
    fs::write(&wait, b"1").unwrap();

    let started = parse_single_json_stdout(&runner_alias.run_with_db_in_dir(
        &runner_alias.work_dir,
        &[
            "--json",
            "download",
            "--background",
            "--upload",
            "1",
            "--no-preset",
        ],
        &[
            ("SIPHON_STUB_READY_FILE", ready.to_str().unwrap()),
            ("SIPHON_STUB_WAIT_FILE", wait.to_str().unwrap()),
        ],
    ));
    let job_id = started["data"]["id"].as_i64().unwrap();
    let worker_pid = started["data"]["pid"].as_u64().unwrap() as u32;
    wait_for_file(&ready, Duration::from_secs(10));

    kill_process_by_pid(worker_pid);

    // Status and contention lookups via the primary DB path see the canonical child marker written
    // through the alias DB path.
    #[cfg(unix)]
    {
        let status = parse_single_json_stdout(&runner_real.run_with_db(&[
            "--json",
            "job",
            "status",
            &job_id.to_string(),
        ]));
        assert_eq!(status["data"]["state"], "running");

        let busy = runner_real.run_with_db(&["--json", "download", "--upload", "1", "--no-preset"]);
        assert_eq!(busy.status.code(), Some(1));
        assert_eq!(
            parse_single_json_stdout(&busy)["error"]["code"],
            "operation_busy"
        );
        fs::remove_file(&wait).unwrap();
    }
    #[cfg(windows)]
    {
        let _ = fs::remove_file(&wait);
    }

    let final_status = poll_job_until(&runner_real, job_id, Duration::from_secs(10), |d| {
        d["state"] != "running"
    });
    assert_eq!(final_status["state"], "interrupted");
}

#[test]
fn managed_downloads_fail_closed_without_executing_child_when_tracking_fails() {
    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("siphon.db");
    let (runner, _server) = seed_single_upload_runner(
        work_dir,
        db_path.clone(),
        "BV1failclosed",
        "Fail Closed Video",
    );

    let canonical_db = fs::canonicalize(&db_path).unwrap();
    let mut owners_os = canonical_db.as_os_str().to_os_string();
    owners_os.push(".owners");
    let blocked_owners = PathBuf::from(owners_os);
    fs::write(&blocked_owners, b"block-owners-directory").unwrap();

    // Foreground managed upload fails closed before `yt-dlp-stub` runs and releases the slot.
    let fg_ready = temp.path().join("fg_fail_closed.ready");
    let fg_out = runner.run_with_db_in_dir(
        &runner.work_dir,
        &["--json", "download", "--upload", "1", "--no-preset"],
        &[("SIPHON_STUB_READY_FILE", fg_ready.to_str().unwrap())],
    );
    assert_eq!(fg_out.status.code(), Some(1));
    assert!(
        !fg_ready.exists(),
        "foreground child should never execute when tracking setup fails"
    );

    // Background managed download also fails closed, finalizes job `failed`, and releases slot.
    let bg_ready = temp.path().join("bg_fail_closed.ready");
    let bg_sub = parse_single_json_stdout(&runner.run_with_db_in_dir(
        &runner.work_dir,
        &[
            "--json",
            "download",
            "--background",
            "--upload",
            "1",
            "--no-preset",
        ],
        &[("SIPHON_STUB_READY_FILE", bg_ready.to_str().unwrap())],
    ));
    let bg_id = bg_sub["data"]["id"].as_i64().unwrap();
    let bg_done = poll_job_until(&runner, bg_id, Duration::from_secs(10), |d| {
        d["state"] != "running"
    });
    assert_eq!(bg_done["state"], "failed");
    assert!(
        !bg_ready.exists(),
        "background child should never execute when tracking setup fails"
    );

    // Removing the obstruction allows subsequent downloads on upload 1 without a stuck slot.
    fs::remove_file(&blocked_owners).unwrap();
    let retry = runner.run_with_db(&["--json", "download", "--upload", "1", "--no-preset"]);
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let retry_json = parse_single_json_stdout(&retry);
    assert_eq!(retry_json["data"]["marked_downloaded"], true);
}
