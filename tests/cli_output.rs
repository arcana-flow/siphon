use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
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
        let dir = env::temp_dir().join(format!("siphon-cli-output-bin-{}", std::process::id()));
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
        let router: Arc<MockRouter> = Arc::new(handler);

        let stop_clone = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                if stop_clone.load(Ordering::SeqCst) {
                    break;
                }
                if let Some(req) = read_http_request(&mut stream) {
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
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
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
    let req_line = text.lines().next()?;
    let target = req_line.split_whitespace().nth(1)?;
    let full = format!("http://localhost{target}");
    let parsed = Url::parse(&full).ok()?;
    let query = parsed
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    Some(RecordedHttpRequest {
        path: parsed.path().to_owned(),
        query,
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
        let mut full_args = vec!["--db", self.db_path.to_str().expect("utf8 db path")];
        full_args.extend_from_slice(args);
        self.run_raw(&full_args, &[])
    }

    fn run_raw(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .current_dir(&self.work_dir)
            .env("PATH", path_with_prepended_dir(&self.stub_dir))
            .env("SIPHON_BILIBILI_API_BASE", &self.api_base)
            .env_remove("SIPHON_DB");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        cmd.output().expect("execute siphon binary")
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

fn read_stub_log(log_path: &Path) -> Vec<Vec<String>> {
    let Ok(contents) = fs::read_to_string(log_path) else {
        return Vec::new();
    };
    let mut invocations = Vec::new();
    let mut current = Vec::new();
    for line in contents.lines() {
        if line == "INVOCATION" {
            current = Vec::new();
        } else if let Some(rest) = line.strip_prefix("ARG[")
            && let Some((_, val)) = rest.split_once("]=")
        {
            current.push(val.to_owned());
        } else if line == "END" {
            invocations.push(std::mem::take(&mut current));
        }
    }
    invocations
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

#[test]
fn json_mode_covers_help_version_and_argument_config_bootstrap_failures() {
    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    fs::create_dir_all(&work_dir).unwrap();
    let db_path = temp.path().join("never-created.db");
    let runner = CliRunner::new(
        work_dir.clone(),
        db_path.clone(),
        "http://127.0.0.1:1".into(),
    );

    // 1. Help (root, subcommand, nested subcommand, and `help <subcommand>`).
    for help_args in [
        &["--json", "--help"][..],
        &["--help", "--json"],
        &["--json", "help"],
        &["creator", "--json", "--help"],
        &["--json", "creator", "add", "--help"],
        &["--json", "help", "download"],
    ] {
        let out = runner.run_raw(help_args, &[]);
        assert!(out.status.success(), "help failed for {help_args:?}");
        assert!(out.stderr.is_empty(), "help wrote stderr for {help_args:?}");
        let json = parse_single_json_stdout(&out);
        assert_eq!(json["ok"], true);
        assert!(json["error"].is_null());
        assert!(json["data"]["usage"].is_string());
        assert!(json["data"]["options"].is_array());
        assert!(json["data"]["subcommands"].is_array());
        assert!(json["data"]["arguments"].is_array());
    }
    let dl_help = parse_single_json_stdout(&runner.run_raw(&["--json", "download", "--help"], &[]));
    assert_eq!(dl_help["data"]["command"], "siphon download");
    assert!(
        dl_help["data"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["long"] == "--upload")
    );
    assert!(
        dl_help["data"]["options"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["long"] == "--json")
    );

    // 2. Version.
    for ver_args in [
        &["--json", "--version"][..],
        &["creator", "--version", "--json"],
    ] {
        let out = runner.run_raw(ver_args, &[]);
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        let json = parse_single_json_stdout(&out);
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["name"], "siphon");
        assert_eq!(json["data"]["version"], env!("CARGO_PKG_VERSION"));
    }

    assert!(!db_path.exists(), "help/version must not create SQLite DB");

    // 3. Argument and validation errors exit 2 with single JSON error on stdout.
    for bad_args in [
        &["--json"][..],
        &["--json", "creator"],
        &["--json", "--bogus-flag"],
        &["--json", "creator", "remove", "0"],
        &["--json", "download", "--upload", "0"],
        &["--json", "--db", "  ", "creator", "list"],
    ] {
        let out = runner.run_raw(bad_args, &[]);
        assert_eq!(out.status.code(), Some(2), "args={bad_args:?}");
        assert!(out.stderr.is_empty(), "stderr not empty for {bad_args:?}");
        let json = parse_single_json_stdout(&out);
        assert_eq!(json["ok"], false);
        assert!(json["data"].is_null());
        assert!(
            json["error"]["code"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
    }

    // 4. Config error (`SIPHON_BILIBILI_API_BASE` non-loopback) exits 1 with JSON error.
    let cfg_err_out = runner.run_raw(
        &[
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "creator",
            "list",
        ],
        &[("SIPHON_BILIBILI_API_BASE", "http://example.com")],
    );
    assert_eq!(cfg_err_out.status.code(), Some(1));
    assert!(cfg_err_out.stderr.is_empty());
    let cfg_json = parse_single_json_stdout(&cfg_err_out);
    assert_eq!(cfg_json["ok"], false);
    assert_eq!(cfg_json["error"]["code"], "invalid_config");

    // 5. Storage bootstrap error (`--db` pointing to an existing directory) exits 1 with JSON error.
    let dir_as_db = temp.path().join("db-is-a-dir");
    fs::create_dir_all(&dir_as_db).unwrap();
    let db_err_out = runner.run_raw(
        &[
            "--db",
            dir_as_db.to_str().unwrap(),
            "--json",
            "creator",
            "list",
        ],
        &[],
    );
    assert_eq!(db_err_out.status.code(), Some(1));
    assert!(db_err_out.stderr.is_empty());
    let db_json = parse_single_json_stdout(&db_err_out);
    assert_eq!(db_json["ok"], false);
    assert_eq!(db_json["error"]["code"], "storage_backend_error");
}

#[test]
fn json_and_human_workflows_handle_all_commands_partial_refresh_and_download_streams() {
    let nav_json = read_fixture("bilibili/nav_guest.json");
    let acc_alpha = read_fixture("bilibili/acc_info_946974.json");
    let acc_beta = read_fixture("bilibili/acc_info_888001.json");
    let arc_rate_limited = read_fixture("bilibili/arc_search_rate_limited.json");
    let spi_json = r#"{"code":0,"message":"0","data":{"b_3":"BUVID3","b_4":"BUVID4"}}"#;

    let fail_alpha = Arc::new(AtomicBool::new(false));
    let fail_alpha_clone = Arc::clone(&fail_alpha);
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
            Some("946974") => {
                if fail_alpha_clone.load(Ordering::SeqCst) {
                    MockHttpResponse::ok_json(&arc_rate_limited)
                } else {
                    MockHttpResponse::ok_json(arc_search_json(&[(
                        "BV1alpha001",
                        1001,
                        "Alpha One",
                        feed_ts,
                    )]))
                }
            }
            Some("888001") => MockHttpResponse::ok_json(arc_search_json(&[(
                "BV1beta001",
                2001,
                "Beta One",
                feed_ts,
            )])),
            _ => MockHttpResponse::status(404, "unknown"),
        },
        _ => MockHttpResponse::status(404, "unknown"),
    });

    let temp = TempDir::new().unwrap();
    let work_dir = temp.path().join("work");
    let db_path = temp.path().join("siphon.db");
    let log_path = temp.path().join("stub.log");
    fs::create_dir_all(&work_dir).unwrap();
    fs::write(
        work_dir.join(".siphon-stub.cfg"),
        format!("SIPHON_STUB_LOG={}\n", log_path.display()),
    )
    .unwrap();
    let runner = CliRunner::new(work_dir.clone(), db_path, server.base_url());

    // 1. Creator add / list / remove in JSON mode.
    let add_alpha = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "creator",
        "add",
        "https://space.bilibili.com/946974",
    ]));
    assert_eq!(add_alpha["ok"], true);
    assert_eq!(add_alpha["data"]["creator"]["id"], 1);
    assert_eq!(
        add_alpha["data"]["creator"]["platform_creator_id"],
        "946974"
    );

    let add_beta = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "creator",
        "add",
        "https://space.bilibili.com/888001",
    ]));
    assert_eq!(add_beta["ok"], true);
    assert_eq!(add_beta["data"]["creator"]["id"], 2);

    let list_creators =
        parse_single_json_stdout(&runner.run_with_db(&["--json", "creator", "list"]));
    assert_eq!(list_creators["ok"], true);
    assert_eq!(
        list_creators["data"]["creators"].as_array().unwrap().len(),
        2
    );

    // 2. Partial refresh failure returns nonzero, JSON error.details has ALL creator outcomes,
    //    and human mode preserves useful per-creator results.
    fail_alpha.store(true, Ordering::SeqCst);
    let partial_json_out = runner.run_with_db(&["--json", "refresh"]);
    assert_eq!(partial_json_out.status.code(), Some(1));
    assert!(partial_json_out.stderr.is_empty());
    let partial_json = parse_single_json_stdout(&partial_json_out);
    assert_eq!(partial_json["ok"], false);
    assert_eq!(partial_json["error"]["code"], "refresh_partial_failure");
    let details = &partial_json["error"]["details"];
    assert_eq!(details["succeeded"], 1);
    assert_eq!(details["failed"], 1);
    assert_eq!(details["total"], 2);
    let outcomes = details["outcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0]["creator_id"], 1);
    assert_eq!(outcomes[0]["status"], "failed");
    assert_eq!(outcomes[1]["creator_id"], 2);
    assert_eq!(outcomes[1]["status"], "succeeded");
    assert_eq!(outcomes[1]["new_uploads_inserted"], 1);

    // Human mode partial failure: useful per-creator outcomes on stdout + error on stderr.
    let partial_human = runner.run_with_db(&["refresh"]);
    assert_eq!(partial_human.status.code(), Some(1));
    let human_stdout = String::from_utf8_lossy(&partial_human.stdout);
    let human_stderr = String::from_utf8_lossy(&partial_human.stderr);
    assert!(human_stdout.contains("Refreshed creator 2 (Sanitized Creator Beta)"));
    assert!(human_stdout.contains("Failed creator 1 (Sanitized Creator Alpha)"));
    assert!(human_stderr.contains("1 succeeded and 1 failed"));

    // 3. Full refresh succeeds in JSON mode.
    fail_alpha.store(false, Ordering::SeqCst);
    let ok_refresh = parse_single_json_stdout(&runner.run_with_db(&["--json", "refresh"]));
    assert_eq!(ok_refresh["ok"], true);
    assert_eq!(ok_refresh["data"]["succeeded"], 2);
    assert_eq!(ok_refresh["data"]["failed"], 0);

    // 4. Upload list + ignore in JSON mode.
    let uploads = parse_single_json_stdout(&runner.run_with_db(&["--json", "upload", "list"]));
    assert_eq!(uploads["ok"], true);
    let upload_arr = uploads["data"]["uploads"].as_array().unwrap();
    assert_eq!(upload_arr.len(), 2);
    let beta_id = upload_arr
        .iter()
        .find(|u| u["platform_upload_id"] == "BV1beta001")
        .unwrap()["id"]
        .as_i64()
        .unwrap()
        .to_string();
    let alpha_id = upload_arr
        .iter()
        .find(|u| u["platform_upload_id"] == "BV1alpha001")
        .unwrap()["id"]
        .as_i64()
        .unwrap()
        .to_string();

    let ignored =
        parse_single_json_stdout(&runner.run_with_db(&["--json", "upload", "ignore", &beta_id]));
    assert_eq!(ignored["ok"], true);
    assert_eq!(ignored["data"]["upload"]["status"], "ignored");

    // Re-ignoring fails with stable `upload_not_pending` code.
    let reignore_out = runner.run_with_db(&["--json", "upload", "ignore", &beta_id]);
    assert_eq!(reignore_out.status.code(), Some(1));
    let reignore_json = parse_single_json_stdout(&reignore_out);
    assert_eq!(reignore_json["ok"], false);
    assert_eq!(reignore_json["error"]["code"], "upload_not_pending");

    // 5. Preset add / list / set-default / remove in JSON mode + `-- --json` preservation.
    let preset_add = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "preset",
        "add",
        "json_or_pipe",
        "--",
        "-o",
        "-",
    ]));
    assert_eq!(preset_add["ok"], true);
    assert_eq!(preset_add["data"]["preset"]["name"], "json_or_pipe");

    // Adding a preset containing `--json` after `--` does NOT activate Siphon JSON mode.
    let preset_with_json_arg = runner.run_with_db(&["preset", "add", "meta", "--", "--json"]);
    assert!(preset_with_json_arg.status.success());
    assert!(
        String::from_utf8_lossy(&preset_with_json_arg.stdout).starts_with("Saved preset 'meta'")
    );

    let preset_list = parse_single_json_stdout(&runner.run_with_db(&["--json", "preset", "list"]));
    assert_eq!(preset_list["ok"], true);
    assert!(preset_list["data"]["presets"].as_array().unwrap().len() >= 3);

    // 6. Media-to-stdout refusal in `--json` mode across CLI, preset, and yt-dlp.conf.
    let logs_before_refusal = read_stub_log(&log_path).len();
    let refused_cli = runner.run_with_db(&[
        "--json",
        "download",
        "https://www.bilibili.com/video/BV1pipe",
        "--",
        "-o",
        "-",
    ]);
    assert_eq!(refused_cli.status.code(), Some(1));
    let refused_cli_json = parse_single_json_stdout(&refused_cli);
    assert_eq!(refused_cli_json["ok"], false);
    assert_eq!(
        refused_cli_json["error"]["code"],
        "media_to_stdout_not_allowed"
    );

    let refused_preset = runner.run_with_db(&[
        "--json",
        "download",
        "https://www.bilibili.com/video/BV1pipepreset",
        "--preset",
        "json_or_pipe",
    ]);
    assert_eq!(refused_preset.status.code(), Some(1));
    assert_eq!(
        parse_single_json_stdout(&refused_preset)["error"]["code"],
        "media_to_stdout_not_allowed"
    );

    fs::write(work_dir.join("yt-dlp.conf"), "-o -\n").unwrap();
    let refused_conf = runner.run_with_db(&[
        "--json",
        "download",
        "https://www.bilibili.com/video/BV1pipeconf",
    ]);
    assert_eq!(refused_conf.status.code(), Some(1));
    assert_eq!(
        parse_single_json_stdout(&refused_conf)["error"]["code"],
        "media_to_stdout_not_allowed"
    );
    assert_eq!(
        read_stub_log(&log_path).len(),
        logs_before_refusal,
        "media-to-stdout must be rejected pre-spawn"
    );

    // Overriding `-o` on CLI or passing `--ignore-config` allows the download in JSON mode.
    let allowed_override = runner.run_with_db(&[
        "--json",
        "download",
        "https://www.bilibili.com/video/BV1override",
        "--preset",
        "json_or_pipe",
        "--",
        "-o",
        "safe_override.mp4",
    ]);
    assert!(allowed_override.status.success());
    let override_json = parse_single_json_stdout(&allowed_override);
    assert_eq!(override_json["ok"], true);
    fs::remove_file(work_dir.join("yt-dlp.conf")).unwrap();

    // 7. Text `yt-dlp` stdout (`-J`, `--print`, `--help`, normal download) goes to stderr in JSON mode,
    //    while stdout has ONLY the single Siphon JSON object.
    let dl_j = runner.run_with_db(&[
        "--json",
        "download",
        "https://www.bilibili.com/video/BV1jjson",
        "--no-preset",
        "--",
        "-J",
    ]);
    assert!(dl_j.status.success());
    let dl_j_json = parse_single_json_stdout(&dl_j);
    assert_eq!(dl_j_json["ok"], true);
    assert_eq!(dl_j_json["data"]["target"]["kind"], "direct_url");
    let dl_j_stderr = String::from_utf8_lossy(&dl_j.stderr);
    assert!(dl_j_stderr.contains("\"id\":\"BV1jjson\""));

    let dl_help = runner.run_with_db(&[
        "--json",
        "download",
        "https://www.bilibili.com/video/BV1help",
        "--no-preset",
        "--",
        "--help",
    ]);
    assert!(dl_help.status.success());
    assert_eq!(parse_single_json_stdout(&dl_help)["ok"], true);
    assert!(String::from_utf8_lossy(&dl_help.stderr).contains("yt-dlp stub"));

    // Pending upload download in JSON mode updates status and reports `marked_downloaded: true`.
    let dl_upload = runner.run_with_db(&["--json", "download", "--upload", &alpha_id]);
    assert!(dl_upload.status.success());
    let dl_upload_json = parse_single_json_stdout(&dl_upload);
    assert_eq!(dl_upload_json["ok"], true);
    assert_eq!(dl_upload_json["data"]["marked_downloaded"], true);
    assert_eq!(dl_upload_json["data"]["target"]["kind"], "pending_upload");
    assert_eq!(
        dl_upload_json["data"]["target"]["upload"]["status"],
        "downloaded"
    );
    assert!(
        !dl_upload_json["data"]["confirmed_artifacts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(String::from_utf8_lossy(&dl_upload.stderr).contains("[download] Destination:"));

    // Ordinary non-JSON passthrough keeps `-J` on stdout and Siphon note on stderr.
    let ordinary_j = runner.run_with_db(&[
        "download",
        "https://www.bilibili.com/video/BV1plainjson",
        "--no-preset",
        "--",
        "-J",
    ]);
    assert!(ordinary_j.status.success());
    let ordinary_parsed: Value =
        serde_json::from_str(String::from_utf8_lossy(&ordinary_j.stdout).trim()).unwrap();
    assert_eq!(ordinary_parsed["id"], "BV1plainjson");
    assert!(String::from_utf8_lossy(&ordinary_j.stderr).contains("Finished direct URL"));

    // Creator remove + preset remove in JSON mode.
    let rm_preset = parse_single_json_stdout(&runner.run_with_db(&[
        "--json",
        "preset",
        "remove",
        "json_or_pipe",
    ]));
    assert_eq!(rm_preset["ok"], true);
    assert_eq!(rm_preset["data"]["removed"], "json_or_pipe");

    let rm_creator =
        parse_single_json_stdout(&runner.run_with_db(&["--json", "creator", "remove", "1"]));
    assert_eq!(rm_creator["ok"], true);
    assert_eq!(rm_creator["data"]["creator"]["id"], 1);
    assert_eq!(rm_creator["data"]["uploads_preserved"], true);
}
