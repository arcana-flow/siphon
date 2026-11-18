use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

const KNOWN_LONG_OPTIONS: &[(&str, &str, usize)] = &[
    ("alias", "alias", 2),
    ("all-subs", "all-subs", 0),
    ("audio-format", "audio-format", 1),
    ("batch-file", "batch-file", 1),
    ("config-location", "config-locations", 1),
    ("config-locations", "config-locations", 1),
    ("convert-subtitles", "convert-subs", 1),
    ("convert-subs", "convert-subs", 1),
    ("cookies", "cookies", 1),
    ("cookies-from-browser", "cookies-from-browser", 1),
    ("dump-json", "dump-json", 0),
    ("dump-single-json", "dump-single-json", 0),
    ("embed-thumbnail", "embed-thumbnail", 0),
    ("exec", "exec", 1),
    ("extract-audio", "extract-audio", 0),
    ("format", "format", 1),
    ("get-filename", "get-filename", 0),
    ("get-title", "get-title", 0),
    ("get-url", "get-url", 0),
    ("help", "help", 0),
    ("ignore-config", "ignore-config", 0),
    ("list-formats", "list-formats", 0),
    ("list-subs", "list-subs", 0),
    ("list-thumbnails", "list-thumbnails", 0),
    ("load-info-json", "load-info-json", 1),
    ("newline", "newline", 0),
    ("no-overwrites", "no-overwrites", 0),
    ("no-playlist", "no-playlist", 0),
    ("no-progress", "no-progress", 0),
    ("no-simulate", "no-simulate", 0),
    ("output", "output", 1),
    ("parse-metadata", "parse-metadata", 2),
    ("paths", "paths", 1),
    ("playlist-items", "playlist-items", 1),
    ("ppa", "ppa", 1),
    ("print", "print", 1),
    ("print-to-file", "print-to-file", 2),
    ("progress", "progress", 0),
    ("progress-template", "progress-template", 1),
    ("proxy", "proxy", 1),
    ("quiet", "quiet", 0),
    ("referer", "referer", 1),
    ("replace-in-metadata", "replace-in-metadata", 3),
    ("simulate", "simulate", 0),
    ("skip-download", "skip-download", 0),
    ("sub-lang", "sub-langs", 1),
    ("sub-langs", "sub-langs", 1),
    ("version", "version", 0),
    ("write-auto-sub", "write-auto-subs", 0),
    ("write-auto-subs", "write-auto-subs", 0),
    ("write-sub", "write-subs", 0),
    ("write-subs", "write-subs", 0),
    ("write-thumbnail", "write-thumbnail", 0),
    ("yes-playlist", "yes-playlist", 0),
];

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

fn json_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
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
        "SIPHON_STUB_STDERR_PROGRESS",
        "SIPHON_STUB_STDOUT_PROGRESS",
        "SIPHON_STUB_PROGRESS_BYTES",
        "SIPHON_STUB_READY_FILE",
        "SIPHON_STUB_WAIT_FILE",
        "SIPHON_STUB_POST_READY_FILE",
        "SIPHON_STUB_POST_WAIT_FILE",
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

#[derive(Clone)]
struct PrintToFileRule {
    when: String,
    template: String,
    file: PathBuf,
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
    batch_files: Vec<PathBuf>,
    config_locations: Vec<PathBuf>,
    load_info_json: Option<PathBuf>,
    print_to_files: Vec<PrintToFileRule>,
    print_exprs: Vec<(String, String)>,
    dump_json: bool,
    list_formats: bool,
    aliases: HashMap<String, Vec<String>>,
    help_or_version: bool,
    progress_template: Option<String>,
}

fn resolve_long_option(prefix: &str) -> Result<(&'static str, usize), String> {
    if let Some(&(_, canonical, arity)) = KNOWN_LONG_OPTIONS.iter().find(|&&(n, _, _)| n == prefix)
    {
        return Ok((canonical, arity));
    }
    let matches: Vec<(&'static str, &'static str, usize)> = KNOWN_LONG_OPTIONS
        .iter()
        .copied()
        .filter(|(n, _, _)| n.starts_with(prefix))
        .collect();
    if matches.is_empty() {
        return Err(format!("yt-dlp: error: no such option: --{prefix}"));
    }
    let first_canonical = matches[0].1;
    if matches.iter().all(|(_, c, _)| *c == first_canonical) {
        return Ok((first_canonical, matches[0].2));
    }
    let names: Vec<String> = matches.iter().map(|(n, _, _)| format!("--{n}")).collect();
    Err(format!(
        "yt-dlp: error: ambiguous option: --{prefix} ({})",
        names.join(", ")
    ))
}

fn short_option_takes_arg(ch: char) -> bool {
    matches!(
        ch,
        'a' | 'f' | 'I' | 'N' | 'o' | 'O' | 'p' | 'P' | 'r' | 'R' | 'S' | 't' | 'u' | '2'
    )
}

fn parse_token_stream(tokens: &[String], parsed: &mut ParsedArgs) -> Result<(), String> {
    let mut queue: Vec<String> = tokens.to_vec();
    let mut i = 0usize;
    let mut after_double_dash = false;
    while i < queue.len() {
        let arg = queue[i].clone();
        if after_double_dash {
            parsed.urls.push(arg);
            i += 1;
            continue;
        }
        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if arg == "-" || !arg.starts_with('-') {
            parsed.urls.push(arg);
            i += 1;
            continue;
        }
        if let Some(expansion) = parsed.aliases.get(&arg).cloned() {
            queue.splice(i..=i, expansion);
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            let (raw_name, inline) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v.to_owned())),
                None => (rest, None),
            };
            let (name, arity) = resolve_long_option(raw_name)?;
            if inline.is_some() && arity != 1 {
                return Err(format!(
                    "yt-dlp: error: --{name} option does not take a value"
                ));
            }
            let get_arg = |offset: usize| -> Result<String, String> {
                if offset == 0
                    && let Some(val) = &inline
                {
                    return Ok(val.clone());
                }
                queue
                    .get(i + 1 + offset)
                    .cloned()
                    .ok_or_else(|| format!("yt-dlp: error: --{name} option requires an argument"))
            };
            match name {
                "help" | "version" => parsed.help_or_version = true,
                "ignore-config" => parsed.ignore_config = true,
                "no-playlist" => parsed.no_playlist = true,
                "yes-playlist" => parsed.yes_playlist = true,
                "simulate" => parsed.simulate = Some(true),
                "no-simulate" => parsed.simulate = Some(false),
                "dump-json" | "dump-single-json" => {
                    parsed.implicit_simulate = true;
                    parsed.dump_json = true;
                }
                "get-url" | "get-title" | "get-filename" => {
                    parsed.implicit_simulate = true;
                }
                "list-formats" => {
                    parsed.implicit_simulate = true;
                    parsed.list_formats = true;
                }
                "list-subs" | "list-thumbnails" => {
                    parsed.implicit_simulate = true;
                }
                "skip-download" => parsed.skip_download = true,
                "write-subs" | "write-sub" | "write-auto-subs" | "write-auto-sub" | "all-subs" => {
                    parsed.write_subs = true;
                }
                "write-thumbnail" => parsed.write_thumbnail = true,
                "quiet" => parsed.quiet = true,
                "no-overwrites" => parsed.no_overwrites = true,
                "extract-audio" => parsed.extract_audio = true,
                "audio-format" => parsed.audio_format = Some(get_arg(0)?),
                "convert-subs" | "convert-subtitles" => parsed.sub_format = Some(get_arg(0)?),
                "paths" => {
                    let raw = get_arg(0)?;
                    let clean = raw.split_once(':').map(|(_, p)| p).unwrap_or(&raw);
                    parsed.paths_dir = Some(PathBuf::from(clean));
                }
                "output" => parsed.output_template = Some(get_arg(0)?),
                "cookies" => parsed.cookie_file = Some(PathBuf::from(get_arg(0)?)),
                "batch-file" => parsed.batch_files.push(PathBuf::from(get_arg(0)?)),
                "config-locations" | "config-location" => {
                    parsed.config_locations.push(PathBuf::from(get_arg(0)?));
                }
                "load-info-json" => parsed.load_info_json = Some(PathBuf::from(get_arg(0)?)),
                "alias" => {
                    let alias_flag = get_arg(0)?;
                    let alias_body = get_arg(1)?;
                    let expanded: Vec<String> =
                        alias_body.split_whitespace().map(str::to_owned).collect();
                    parsed.aliases.insert(alias_flag, expanded);
                }
                "print" => {
                    let v = get_arg(0)?;
                    let (when, expr) = match v.split_once(':') {
                        Some((w, t))
                            if matches!(
                                w,
                                "pre_process"
                                    | "after_filter"
                                    | "video"
                                    | "before_dl"
                                    | "post_dl"
                                    | "post_process"
                                    | "after_move"
                                    | "after_video"
                                    | "playlist"
                            ) =>
                        {
                            (w.to_owned(), t.to_owned())
                        }
                        _ => ("video".to_owned(), v),
                    };
                    if !matches!(
                        when.as_str(),
                        "after_move" | "post_process" | "after_video" | "post_dl"
                    ) {
                        parsed.implicit_simulate = true;
                    }
                    parsed.quiet = true;
                    parsed.print_exprs.push((when, expr));
                }
                "print-to-file" => {
                    let raw_tpl = get_arg(0)?;
                    let raw_file = get_arg(1)?;
                    let (when, template) = match raw_tpl.split_once(':') {
                        Some((w, t))
                            if matches!(
                                w,
                                "pre_process"
                                    | "after_filter"
                                    | "video"
                                    | "before_dl"
                                    | "post_dl"
                                    | "post_process"
                                    | "after_move"
                                    | "after_video"
                                    | "playlist"
                            ) =>
                        {
                            (w.to_owned(), t.to_owned())
                        }
                        _ => ("video".to_owned(), raw_tpl),
                    };
                    let unescaped_file = raw_file.replace("%%", "%");
                    parsed.print_to_files.push(PrintToFileRule {
                        when,
                        template,
                        file: PathBuf::from(unescaped_file),
                    });
                }
                "progress-template" => {
                    parsed.progress_template = Some(get_arg(0)?);
                }
                _ => {}
            }
            if inline.is_some() {
                i += 1;
            } else {
                i += 1 + arity;
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
                    queue.get(i + 1).cloned()
                }
            };
            match ch {
                'h' => parsed.help_or_version = true,
                's' => parsed.simulate = Some(true),
                'j' | 'J' => {
                    parsed.implicit_simulate = true;
                    parsed.dump_json = true;
                }
                'F' => {
                    parsed.implicit_simulate = true;
                    parsed.list_formats = true;
                }
                'g' | 'e' => parsed.implicit_simulate = true,
                'q' => parsed.quiet = true,
                'w' => parsed.no_overwrites = true,
                'x' => parsed.extract_audio = true,
                'a' => {
                    if let Some(raw) = next_val() {
                        parsed.batch_files.push(PathBuf::from(raw));
                    }
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
                c if short_option_takes_arg(c) => {
                    let _ = next_val();
                    break;
                }
                _ => {}
            }
        }
        i += 1 + consumed_next;
    }
    Ok(())
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

fn render_print_expr(expr: &str, title: &str, id: &str, filepath: Option<&Path>) -> String {
    match expr {
        "title" | "%(title)s" => title.to_owned(),
        "id" | "%(id)s" => id.to_owned(),
        "filepath" | "%(filepath)s" => filepath
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "NA".to_owned()),
        other => other
            .replace(
                "%(filepath)s",
                &filepath
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "NA".to_owned()),
            )
            .replace("%(title)s", title)
            .replace("%(id)s", id),
    }
}

fn emit_print_to_file(
    parsed: &ParsedArgs,
    when: &str,
    id: &str,
    filepath: Option<&Path>,
    subtitles: &[(&str, &Path)],
) {
    for rule in &parsed.print_to_files {
        if rule.when != when {
            continue;
        }
        let filepath_json = match filepath {
            Some(p) => json_escape(&p.to_string_lossy()),
            None => "null".to_owned(),
        };
        let subs_json = if subtitles.is_empty() {
            "null".to_owned()
        } else {
            let entries: Vec<String> = subtitles
                .iter()
                .map(|(lang, path)| {
                    format!(
                        "{}:{}",
                        json_escape(lang),
                        json_escape(&path.to_string_lossy())
                    )
                })
                .collect();
            format!("{{{}}}", entries.join(","))
        };
        let line = rule
            .template
            .replace("%(id)j", &json_escape(id))
            .replace("%(id)s", id)
            .replace("%(filepath)j", &filepath_json)
            .replace("%(requested_subtitles.:.filepath)j", &subs_json);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&rule.file)
            .expect("open print-to-file target");
        writeln!(f, "{line}").unwrap();
    }
}

fn main() -> ExitCode {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let settings = load_stub_settings();
    log_invocation(&settings, &raw_args);

    let mut parsed = ParsedArgs::default();
    if let Err(err) = parse_token_stream(&raw_args, &mut parsed) {
        eprintln!("{err}");
        return ExitCode::from(2);
    }

    if parsed.help_or_version {
        println!("yt-dlp stub 2026.01.01");
        return ExitCode::SUCCESS;
    }

    if !parsed.ignore_config {
        let local_conf = Path::new("yt-dlp.conf");
        if local_conf.is_file() {
            let extra = load_config_tokens(local_conf);
            if let Err(err) = parse_token_stream(&extra, &mut parsed) {
                eprintln!("{err}");
                return ExitCode::from(2);
            }
        }
    }
    let config_paths = parsed.config_locations.clone();
    for conf in config_paths {
        let extra = load_config_tokens(&conf);
        if let Err(err) = parse_token_stream(&extra, &mut parsed) {
            eprintln!("{err}");
            return ExitCode::from(2);
        }
    }

    for batch in &parsed.batch_files {
        if let Ok(contents) = fs::read_to_string(batch) {
            for line in contents.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                    parsed.urls.push(trimmed.to_owned());
                }
            }
        }
    }

    if let Some(info_path) = &parsed.load_info_json
        && let Ok(contents) = fs::read_to_string(info_path)
    {
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            parsed.urls.push(trimmed.to_owned());
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

    if let Some(stderr_progress) = settings
        .get("SIPHON_STUB_STDERR_PROGRESS")
        .filter(|s| !s.is_empty())
    {
        eprintln!("{stderr_progress}");
        let _ = io::stderr().flush();
    }

    if let Some(stdout_progress) = settings
        .get("SIPHON_STUB_STDOUT_PROGRESS")
        .filter(|s| !s.is_empty())
    {
        for line in stdout_progress.split("||") {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                println!("{trimmed}");
            }
        }
        let _ = io::stdout().flush();
    }

    if let Some(bytes_spec) = settings
        .get("SIPHON_STUB_PROGRESS_BYTES")
        .filter(|s| !s.is_empty())
        && let Some((dl_raw, total_raw)) = bytes_spec.split_once(':')
        && let Some(tpl) = &parsed.progress_template
    {
        let body = tpl.strip_prefix("download:").unwrap_or(tpl.as_str());
        let rendered = body
            .replace("%(progress.status)j", "\"downloading\"")
            .replace("%(progress.downloaded_bytes)j", dl_raw.trim())
            .replace("%(progress.total_bytes)j", total_raw.trim());
        println!("{rendered}");
        let _ = io::stdout().flush();
    }

    if let Some(ready_path) = settings
        .get("SIPHON_STUB_READY_FILE")
        .filter(|s| !s.is_empty())
    {
        write_artifact(Path::new(ready_path), b"ready");
    }

    if let Some(wait_path) = settings
        .get("SIPHON_STUB_WAIT_FILE")
        .filter(|s| !s.is_empty())
    {
        let start = Instant::now();
        while Path::new(wait_path).exists() && start.elapsed() < Duration::from_secs(30) {
            thread::sleep(Duration::from_millis(20));
        }
    }

    let simulated = parsed.simulate == Some(true)
        || (parsed.implicit_simulate && parsed.simulate != Some(false));
    if simulated {
        for url in &parsed.urls {
            let id = resolve_id_from_url(&settings, url);
            if parsed.dump_json {
                println!(
                    "{{\"id\":{},\"title\":{},\"webpage_url\":{}}}",
                    json_escape(&id),
                    json_escape(&title),
                    json_escape(url)
                );
            } else if parsed.list_formats {
                println!("[info] Available formats for {id}:");
                println!("ID  EXT RESOLUTION | PROTO");
                println!("18  mp4 640x360    | https");
            } else if !parsed.print_exprs.is_empty() {
                for (_, expr) in &parsed.print_exprs {
                    let rendered = render_print_expr(expr, &title, &id, None);
                    println!("{rendered}");
                }
            } else if !parsed.quiet {
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
        emit_print_to_file(&parsed, "after_move", "BV999wrong", Some(&wrong_path), &[]);
        emit_print_to_file(&parsed, "after_video", "BV999wrong", Some(&wrong_path), &[]);
        if !parsed.quiet {
            println!("[download] Destination: {}", wrong_path.display());
        }
        return ExitCode::SUCCESS;
    }

    if parsed.skip_download {
        if parsed.write_subs {
            if mode == "subtitle_missing" {
                for url in &parsed.urls {
                    let id = resolve_id_from_url(&settings, url);
                    emit_print_to_file(&parsed, "after_video", &id, None, &[]);
                }
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
                emit_print_to_file(&parsed, "after_video", &id, None, &[("en", &sub_path)]);
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
                emit_print_to_file(&parsed, "after_video", &id, None, &[]);
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
                    let _ = io::stdout().flush();
                }
                if let Some(post_ready) = settings
                    .get("SIPHON_STUB_POST_READY_FILE")
                    .filter(|s| !s.is_empty())
                {
                    write_artifact(Path::new(post_ready), b"post");
                }
                if let Some(post_wait) = settings
                    .get("SIPHON_STUB_POST_WAIT_FILE")
                    .filter(|s| !s.is_empty())
                {
                    let start = Instant::now();
                    while Path::new(post_wait).exists() && start.elapsed() < Duration::from_secs(30) {
                        thread::sleep(Duration::from_millis(20));
                    }
                }
            }
            emit_print_to_file(&parsed, "after_move", &item_id, Some(&out_path), &[]);
            emit_print_to_file(&parsed, "after_video", &item_id, Some(&out_path), &[]);
            for (_, expr) in &parsed.print_exprs {
                let rendered = render_print_expr(expr, &item_title, &item_id, Some(&out_path));
                println!("{rendered}");
            }
            if parsed.dump_json {
                println!(
                    "{{\"id\":{},\"title\":{},\"filepath\":{}}}",
                    json_escape(&item_id),
                    json_escape(&item_title),
                    json_escape(&out_path.to_string_lossy())
                );
            }
        }
    }

    ExitCode::SUCCESS
}
