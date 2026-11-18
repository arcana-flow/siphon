use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn escape_field(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(ch),
        }
    }
    out
}

fn load_stub_settings() -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(text) = fs::read_to_string(".siphon-stub.cfg") {
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_owned(), v.trim().to_owned());
            }
        }
    }
    for key in [
        "SIPHON_STUB_LOG",
        "SIPHON_STUB_MODE",
        "SIPHON_STUB_EXIT_CODE",
        "SIPHON_STUB_STDERR",
        "SIPHON_STUB_UPLOAD_ID",
        "SIPHON_STUB_TITLE",
        "SIPHON_STUB_CREATE_PART",
    ] {
        if let Ok(val) = env::var(key) {
            map.insert(key.to_owned(), val);
        }
    }
    map
}

fn log_invocation(settings: &HashMap<String, String>, args: &[String]) {
    let Some(log_path) = settings.get("SIPHON_STUB_LOG").filter(|s| !s.is_empty()) else {
        return;
    };
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .expect("open stub log");
    writeln!(file, "INVOCATION").unwrap();
    writeln!(file, "CWD={}", escape_field(&cwd)).unwrap();
    for (idx, arg) in args.iter().enumerate() {
        writeln!(file, "ARG[{idx}]={}", escape_field(arg)).unwrap();
    }
    writeln!(file, "END").unwrap();
}

#[derive(Default)]
struct ParsedArgs {
    urls: Vec<String>,
    ignore_config: bool,
    no_playlist: bool,
    yes_playlist: bool,
    simulate: Option<bool>,
    implicit_simulate: bool,
    skip_download: bool,
    write_subs: bool,
    write_thumbnail: bool,
    quiet: bool,
    no_overwrites: bool,
    extract_audio: bool,
    audio_format: Option<String>,
    sub_format: Option<String>,
    paths_dir: Option<PathBuf>,
    output_template: Option<String>,
    cookie_file: Option<PathBuf>,
    cookies_from_browser: Option<String>,
    batch_file: Option<PathBuf>,
    config_locations: Vec<PathBuf>,
}

fn option_arity(flag: &str) -> usize {
    match flag {
        "--print-to-file" | "--parse-metadata" | "--alias" => 2,
        "--replace-in-metadata" => 3,
        "--config-locations"
        | "--config-location"
        | "--batch-file"
        | "--paths"
        | "--output"
        | "--cookies"
        | "--cookies-from-browser"
        | "--format"
        | "--audio-format"
        | "--convert-subs"
        | "--convert-subtitles"
        | "--sub-langs"
        | "--sub-lang"
        | "--referer"
        | "--proxy"
        | "--print"
        | "--exec"
        | "--playlist-items" => 1,
        _ => 0,
    }
}

fn parse_token_stream(tokens: &[String], parsed: &mut ParsedArgs) {
    let mut i = 0usize;
    let mut after_double_dash = false;
    while i < tokens.len() {
        let arg = &tokens[i];
        if after_double_dash {
            parsed.urls.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if arg == "-" || !arg.starts_with('-') {
            parsed.urls.push(arg.clone());
            i += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            let (name, inline) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (rest, None),
            };
            let next_val = || inline.clone().or_else(|| tokens.get(i + 1).cloned());
            match name {
                "ignore-config" => parsed.ignore_config = true,
                "no-playlist" => parsed.no_playlist = true,
                "yes-playlist" => parsed.yes_playlist = true,
                "simulate" => parsed.simulate = Some(true),
                "no-simulate" => parsed.simulate = Some(false),
                "dump-json" | "dump-single-json" | "get-url" | "get-filename" => {
                    parsed.implicit_simulate = true;
                }
                "skip-download" | "no-download" => parsed.skip_download = true,
                "write-subs" | "write-sub" | "write-auto-subs" | "write-auto-sub" | "all-subs" => {
                    parsed.write_subs = true;
                }
                "write-thumbnail" => parsed.write_thumbnail = true,
                "quiet" => parsed.quiet = true,
                "no-overwrites" => parsed.no_overwrites = true,
                "extract-audio" => parsed.extract_audio = true,
                "audio-format" => parsed.audio_format = next_val(),
                "convert-subs" | "convert-subtitles" => parsed.sub_format = next_val(),
                "paths" => {
                    if let Some(raw) = next_val() {
                        let clean = raw.split_once(':').map(|(_, p)| p).unwrap_or(&raw);
                        parsed.paths_dir = Some(PathBuf::from(clean));
                    }
                }
                "output" => parsed.output_template = next_val(),
                "cookies" => parsed.cookie_file = next_val().map(PathBuf::from),
                "cookies-from-browser" => parsed.cookies_from_browser = next_val(),
                "batch-file" => parsed.batch_file = next_val().map(PathBuf::from),
                "config-locations" | "config-location" => {
                    if let Some(p) = next_val() {
                        parsed.config_locations.push(PathBuf::from(p));
                    }
                }
                "print" => {
                    if let Some(v) = next_val()
                        && !v.starts_with("after_move:")
                        && !v.starts_with("post_process:")
                    {
                        parsed.implicit_simulate = true;
                    }
                }
                _ => {}
            }
            if inline.is_some() {
                i += 1;
            } else {
                let full = format!("--{name}");
                i += 1 + option_arity(&full);
            }
            continue;
        }

        let shorts = &arg[1..];
        let mut consumed_next = 0usize;
        for (idx, ch) in shorts.char_indices() {
            let inline = &shorts[idx + ch.len_utf8()..];
            let mut next_val = || {
                if !inline.is_empty() {
                    Some(inline.to_owned())
                } else {
                    consumed_next = 1;
                    tokens.get(i + 1).cloned()
                }
            };
            match ch {
                's' => parsed.simulate = Some(true),
                'j' | 'J' | 'g' => parsed.implicit_simulate = true,
                'q' => parsed.quiet = true,
                'w' => parsed.no_overwrites = true,
                'x' => parsed.extract_audio = true,
                'a' => {
                    parsed.batch_file = next_val().map(PathBuf::from);
                    break;
                }
                'P' => {
                    if let Some(raw) = next_val() {
                        parsed.paths_dir = Some(PathBuf::from(raw));
                    }
                    break;
                }
                'o' => {
                    parsed.output_template = next_val();
                    break;
                }
                'f' | 'I' | 'N' | 'O' | 'p' | 'r' | 'R' | 'S' | 't' | 'u' | '2' => {
                    let _ = next_val();
                    break;
                }
                _ => {}
            }
        }
        i += 1 + consumed_next;
    }
}

fn load_config_tokens(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        for tok in trimmed.split_whitespace() {
            out.push(tok.to_owned());
        }
    }
    out
}

fn resolve_id_from_url(settings: &HashMap<String, String>, url: &str) -> String {
    if let Some(override_id) = settings
        .get("SIPHON_STUB_UPLOAD_ID")
        .filter(|s| !s.is_empty())
    {
        return override_id.clone();
    }
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    without_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("video")
        .to_owned()
}

fn render_output_path(parsed: &ParsedArgs, title: &str, id: &str, ext: &str) -> PathBuf {
    let base_dir = parsed
        .paths_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let file_name = if let Some(tpl) = &parsed.output_template {
        tpl.replace("%(title)s", title)
            .replace("%(id)s", id)
            .replace("%(ext)s", ext)
    } else {
        format!("{title} [{id}].{ext}")
    };
    let candidate = PathBuf::from(file_name);
    if candidate.is_absolute() {
        candidate
    } else {
        base_dir.join(candidate)
    }
}

fn write_artifact(path: &Path, content: &[u8]) {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, content).expect("write stub artifact");
}

fn main() -> ExitCode {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let settings = load_stub_settings();
    log_invocation(&settings, &raw_args);

    let mut parsed = ParsedArgs::default();
    parse_token_stream(&raw_args, &mut parsed);

    if !parsed.ignore_config {
        let local_conf = Path::new("yt-dlp.conf");
        if local_conf.is_file() {
            let extra = load_config_tokens(local_conf);
            parse_token_stream(&extra, &mut parsed);
        }
    }
    let config_paths = parsed.config_locations.clone();
    for conf in config_paths {
        let extra = load_config_tokens(&conf);
        parse_token_stream(&extra, &mut parsed);
    }

    if let Some(batch) = &parsed.batch_file
        && let Ok(contents) = fs::read_to_string(batch)
    {
        for line in contents.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                parsed.urls.push(trimmed.to_owned());
            }
        }
    }

    if let Some(cookie_path) = &parsed.cookie_file
        && let Err(err) = fs::read_to_string(cookie_path)
    {
        eprintln!(
            "ERROR: unable to read cookie file {}: {err}",
            cookie_path.display()
        );
        return ExitCode::from(1);
    }

    let mode = settings
        .get("SIPHON_STUB_MODE")
        .map(String::as_str)
        .unwrap_or("normal");
    let title = settings
        .get("SIPHON_STUB_TITLE")
        .cloned()
        .unwrap_or_else(|| "Stub Video".to_owned());

    if mode == "fail" {
        if settings.get("SIPHON_STUB_CREATE_PART").map(String::as_str) == Some("1") {
            let id = parsed
                .urls
                .first()
                .map(|u| resolve_id_from_url(&settings, u))
                .unwrap_or_else(|| "unknown".to_owned());
            let part_path = render_output_path(&parsed, &title, &id, "mp4.part");
            write_artifact(&part_path, b"partial");
        }
        let stderr_msg = settings
            .get("SIPHON_STUB_STDERR")
            .cloned()
            .unwrap_or_else(|| "ERROR: simulated yt-dlp download failure".to_owned());
        eprintln!("{stderr_msg}");
        let code: u8 = settings
            .get("SIPHON_STUB_EXIT_CODE")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        return ExitCode::from(code);
    }

    if parsed.urls.is_empty() {
        eprintln!("Usage: yt-dlp [OPTIONS] URL [URL...]");
        eprintln!("yt-dlp: error: You must provide at least one URL.");
        return ExitCode::from(2);
    }

    let simulated = parsed.simulate == Some(true)
        || (parsed.implicit_simulate && parsed.simulate != Some(false));
    if simulated {
        if !parsed.quiet {
            for url in &parsed.urls {
                let id = resolve_id_from_url(&settings, url);
                println!("[info] {id}: Simulating download for {url}");
            }
        }
        return ExitCode::SUCCESS;
    }

    if mode == "silent_no_artifact" {
        return ExitCode::SUCCESS;
    }

    if mode == "wrong_id" {
        let wrong_path = render_output_path(&parsed, "Unrelated", "BV999wrong", "mp4");
        write_artifact(&wrong_path, b"wrong-upload-media");
        if !parsed.quiet {
            println!("[download] Destination: {}", wrong_path.display());
        }
        return ExitCode::SUCCESS;
    }

    if parsed.skip_download {
        if parsed.write_subs {
            if mode == "subtitle_missing" {
                if !parsed.quiet {
                    println!("[info] There are no subtitles for the requested languages");
                }
                return ExitCode::SUCCESS;
            }
            let sub_ext = parsed.sub_format.as_deref().unwrap_or("vtt");
            let ext = format!("en.{sub_ext}");
            for url in &parsed.urls {
                let id = resolve_id_from_url(&settings, url);
                let sub_path = render_output_path(&parsed, &title, &id, &ext);
                write_artifact(
                    &sub_path,
                    b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nOffline stub subtitle\n",
                );
                if !parsed.quiet {
                    println!("[info] Writing video subtitles to: {}", sub_path.display());
                }
            }
            return ExitCode::SUCCESS;
        }
        if parsed.write_thumbnail {
            for url in &parsed.urls {
                let id = resolve_id_from_url(&settings, url);
                let thumb = render_output_path(&parsed, &title, &id, "jpg");
                write_artifact(&thumb, b"fake-jpeg");
                if !parsed.quiet {
                    println!("[info] Writing video thumbnail to: {}", thumb.display());
                }
            }
        }
        return ExitCode::SUCCESS;
    }

    let media_ext = if parsed.extract_audio {
        parsed.audio_format.as_deref().unwrap_or("mp3")
    } else {
        "mp4"
    };

    for url in &parsed.urls {
        let id = resolve_id_from_url(&settings, url);
        let is_playlist_url = url.contains("playlist") || url.contains("list=");
        let expand_playlist = is_playlist_url && !parsed.no_playlist && parsed.yes_playlist;

        let targets = if expand_playlist {
            vec![
                (format!("{title} Part 1"), format!("{id}_1")),
                (format!("{title} Part 2"), format!("{id}_2")),
            ]
        } else {
            vec![(title.clone(), id)]
        };

        for (item_title, item_id) in targets {
            let out_path = render_output_path(&parsed, &item_title, &item_id, media_ext);
            if parsed.no_overwrites && out_path.exists() {
                if !parsed.quiet {
                    println!(
                        "[download] {} has already been downloaded",
                        out_path.display()
                    );
                }
            } else {
                write_artifact(&out_path, b"offline-stub-media-payload");
                if !parsed.quiet {
                    println!("[download] Destination: {}", out_path.display());
                    if parsed.extract_audio {
                        println!("[ExtractAudio] Destination: {}", out_path.display());
                    } else {
                        println!("[Merger] Merging formats into \"{}\"", out_path.display());
                    }
                }
            }
        }
    }

    ExitCode::SUCCESS
}
