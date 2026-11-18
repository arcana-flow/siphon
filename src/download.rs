//! Media download integration contract, preset resolution, `yt-dlp` execution, and artifact rules.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::domain::{CookieSource, PresetName, Upload, UploadId, UploadStatus};
use crate::error::{AppError, DownloadError, StorageError};
use crate::storage::Storage;

/// Default executable name used to invoke `yt-dlp`.
pub const DEFAULT_YT_DLP_BINARY: &str = "yt-dlp";

static NEXT_MANIFEST_SEQ: AtomicU64 = AtomicU64::new(1);

/// Known `yt-dlp` output/path type prefixes used in `-P TYPE:PATH` and `-o TYPE:TEMPLATE`.
const YT_DLP_PATH_TYPES: &[&str] = &[
    "home",
    "temp",
    "default",
    "chapter",
    "subtitle",
    "thumbnail",
    "description",
    "infojson",
    "pl_thumbnail",
    "pl_description",
    "pl_infojson",
    "pl_video",
    "link",
];

const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "webm", "flv", "mov", "avi", "wmv", "m4v", "ts", "mts", "m2ts", "3gp", "ogv",
    "mpg", "mpeg", "vob", "mp3", "m4a", "aac", "opus", "ogg", "oga", "flac", "wav", "wma", "alac",
    "aiff", "mka", "weba",
];

const SUBTITLE_EXTENSIONS: &[&str] = &[
    "srt", "vtt", "ass", "ssa", "lrc", "ttml", "srv1", "srv2", "srv3", "json3", "sub", "idx",
    "smi", "dfxp",
];

const IGNORED_EXTENSIONS: &[&str] = &[
    "part",
    "ytdl",
    "tmp",
    "temp",
    "frag",
    "aria2",
    "db",
    "db-wal",
    "db-shm",
    "sqlite",
    "sqlite3",
    "txt",
    "description",
    "xml",
    "jpg",
    "jpeg",
    "png",
    "webp",
    "gif",
    "conf",
    "cfg",
    "log",
];

/// Resolved target for a `yt-dlp` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadTarget {
    /// Direct URL invocation (`siphon download <URL>`); never updates upload status.
    DirectUrl {
        /// User-supplied media or playlist URL.
        url: String,
    },
    /// Pending upload invocation (`siphon download --upload <ID>`).
    PendingUpload {
        /// Pending upload record to download and potentially mark as `downloaded`.
        upload: Upload,
    },
}

impl DownloadTarget {
    /// Returns the URL passed to `yt-dlp`.
    #[must_use]
    pub fn url(&self) -> &str {
        match self {
            Self::DirectUrl { url } => url,
            Self::PendingUpload { upload } => &upload.url,
        }
    }

    /// Returns `true` when this download is tied to a pending upload.
    #[must_use]
    pub const fn is_pending_upload(&self) -> bool {
        matches!(self, Self::PendingUpload { .. })
    }
}

/// User target input prior to resolving pending upload records from storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadTargetInput {
    /// Direct URL supplied on the CLI.
    Url(String),
    /// Local upload ID supplied via `--upload <ID>`.
    Upload(UploadId),
}

/// How preset arguments are chosen for a `siphon download` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetSelection {
    /// Use the active default preset stored in `download_preferences`.
    Default,
    /// Use the explicitly requested named preset (`--preset <NAME>`).
    Named(PresetName),
    /// Omit all preset arguments (`--no-preset`).
    None,
}

/// Unresolved download invocation produced by the CLI before touching storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadPlanInput {
    /// Direct URL or `--upload <ID>`.
    pub target: DownloadTargetInput,
    /// Preset selection mode.
    pub preset: PresetSelection,
    /// Cookie source explicitly supplied on this download invocation (never inherited from refresh).
    pub cookies: Option<CookieSource>,
    /// One-off `yt-dlp` arguments passed after `--` in exact supplied order.
    pub extra_args: Vec<String>,
}

/// Fully resolved request passed to a [`Downloader`] implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRequest {
    /// Target URL or pending upload.
    pub target: DownloadTarget,
    /// Resolved preset name, or `None` when `--no-preset` was specified.
    pub preset_name: Option<PresetName>,
    /// Ordered arguments from the selected preset.
    pub preset_args: Vec<String>,
    /// One-off arguments from after `--` in the exact order supplied.
    pub extra_args: Vec<String>,
    /// Explicit per-invocation cookie source, if supplied.
    pub cookies: Option<CookieSource>,
}

impl DownloadRequest {
    /// Returns `[preset_args..., extra_args...]` in the exact forwarding order required by README.
    #[must_use]
    pub fn forward_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity(self.preset_args.len() + self.extra_args.len());
        args.extend(self.preset_args.iter().cloned());
        args.extend(self.extra_args.iter().cloned());
        args
    }

    /// Assembles the full `yt-dlp` argument vector for this request.
    ///
    /// Order:
    /// 1. Preset arguments (omitted if `--no-preset`).
    /// 2. One-off extra arguments from after `--` in exact order.
    /// 3. Explicit cookie flags (`--cookies` or `--cookies-from-browser`), if any.
    /// 4. `--ignore-config` and `--no-playlist` when downloading `--upload <ID>`, keeping it
    ///    scoped to that single upload without reading external config files.
    /// 5. `"--"` followed by the target URL.
    #[must_use]
    pub fn build_yt_dlp_args(&self) -> Vec<String> {
        let mut args = self.forward_args();
        if let Some(cookies) = &self.cookies {
            match cookies {
                CookieSource::File(path) => {
                    args.push("--cookies".to_owned());
                    args.push(path.to_string_lossy().into_owned());
                }
                CookieSource::BrowserProfile(spec) => {
                    args.push("--cookies-from-browser".to_owned());
                    args.push(spec.clone());
                }
            }
        }
        if self.target.is_pending_upload() {
            // Ensure `--upload <ID>` remains single-upload even if presets or ambient configs
            // specify playlists or external targets.
            args.push("--ignore-config".to_owned());
            args.push("--no-playlist".to_owned());
        }
        args.push("--".to_owned());
        args.push(self.target.url().to_owned());
        args
    }

    /// Validates that a tracked upload request cannot expand or displace its single target.
    pub fn validate(&self) -> Result<(), DownloadError> {
        ensure_no_nul("target URL", self.target.url())?;
        if self.target.url().trim().is_empty() {
            return Err(DownloadError::InvalidArguments {
                reason: "target URL cannot be empty".to_owned(),
            });
        }
        for arg in &self.preset_args {
            ensure_no_nul("preset argument", arg)?;
        }
        for arg in &self.extra_args {
            ensure_no_nul("extra argument", arg)?;
        }
        if self.target.is_pending_upload() {
            validate_single_target_slice(&self.preset_args, "preset")?;
            validate_single_target_slice(&self.extra_args, "command-line")?;
        }
        Ok(())
    }
}

/// Outcome returned by a [`Downloader`] after running `yt-dlp` and checking artifacts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    /// Whether `yt-dlp` exited successfully.
    pub succeeded: bool,
    /// Confirmed output artifact paths saved on disk (media or subtitles).
    ///
    /// Simulations (`--simulate`) or runs with no confirmed saved file leave this empty.
    pub confirmed_artifacts: Vec<PathBuf>,
}

impl DownloadOutcome {
    /// Returns `true` only when `yt-dlp` succeeded and at least one output artifact was confirmed.
    #[must_use]
    pub fn should_mark_downloaded(&self) -> bool {
        self.succeeded && !self.confirmed_artifacts.is_empty()
    }
}

/// Downloader backend contract (invokes `yt-dlp` and verifies output artifacts in v1).
pub trait Downloader: Send + Sync {
    /// Executes the download described by `request` and reports confirmed output artifacts.
    fn download(&self, request: &DownloadRequest) -> Result<DownloadOutcome, DownloadError>;
}

/// Placeholder [`Downloader`] implementation used until the `yt-dlp` backend is wired.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnimplementedDownloader;

impl Downloader for UnimplementedDownloader {
    fn download(&self, _request: &DownloadRequest) -> Result<DownloadOutcome, DownloadError> {
        Err(DownloadError::NotImplemented)
    }
}

/// Production [`Downloader`] implementation that invokes `yt-dlp` as a child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YtDlpDownloader {
    binary: OsString,
    working_dir: Option<PathBuf>,
}

impl Default for YtDlpDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl YtDlpDownloader {
    /// Creates a downloader invoking `yt-dlp` from `PATH` in the current working directory.
    #[must_use]
    pub fn new() -> Self {
        Self {
            binary: OsString::from(DEFAULT_YT_DLP_BINARY),
            working_dir: None,
        }
    }

    /// Creates a downloader invoking the specified `yt-dlp` executable path or command name.
    #[must_use]
    pub fn with_binary(binary: impl Into<OsString>) -> Self {
        Self {
            binary: binary.into(),
            working_dir: None,
        }
    }

    /// Sets the working directory used when spawning the child `yt-dlp` process.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Returns the configured executable path or command name.
    #[must_use]
    pub fn binary(&self) -> &OsStr {
        &self.binary
    }

    /// Returns the configured working directory override, if any.
    #[must_use]
    pub fn working_dir(&self) -> Option<&Path> {
        self.working_dir.as_deref()
    }

    fn resolve_effective_working_dir(&self) -> Result<PathBuf, DownloadError> {
        match &self.working_dir {
            Some(dir) => Ok(dir.clone()),
            None => std::env::current_dir().map_err(|err| {
                DownloadError::Io(format!("failed to determine working dir: {err}"))
            }),
        }
    }

    fn prepare_cookie_args(
        &self,
        cookies: Option<&CookieSource>,
    ) -> Result<Vec<OsString>, DownloadError> {
        let Some(source) = cookies else {
            return Ok(Vec::new());
        };
        match source {
            CookieSource::File(path) => {
                if path.as_os_str().is_empty() {
                    return Err(DownloadError::CookieSourceInvalid {
                        cookie_source: source.describe(),
                        reason: "cookie file path cannot be empty".to_owned(),
                    });
                }
                if path.to_string_lossy().contains('\0') {
                    return Err(DownloadError::CookieSourceInvalid {
                        cookie_source: source.describe(),
                        reason: "cookie file path contains null byte".to_owned(),
                    });
                }

                // If `working_dir` is overridden and the caller passed a relative path resolved
                // against the parent process's cwd, absolutize it so the child finds the file.
                let resolved_for_check = if path.is_relative() {
                    if let Some(dir) = &self.working_dir {
                        let in_working_dir = dir.join(path);
                        if in_working_dir.exists() {
                            in_working_dir
                        } else {
                            std::env::current_dir()
                                .map(|cwd| cwd.join(path))
                                .unwrap_or_else(|_| path.clone())
                        }
                    } else {
                        path.clone()
                    }
                } else {
                    path.clone()
                };

                let meta = fs::metadata(&resolved_for_check).map_err(|err| {
                    DownloadError::CookieSourceInvalid {
                        cookie_source: source.describe(),
                        reason: err.to_string(),
                    }
                })?;
                if !meta.is_file() {
                    return Err(DownloadError::CookieSourceInvalid {
                        cookie_source: source.describe(),
                        reason: "cookie path is not a regular file".to_owned(),
                    });
                }
                fs::File::open(&resolved_for_check).map_err(|err| {
                    DownloadError::CookieSourceInvalid {
                        cookie_source: source.describe(),
                        reason: format!("cookie file is not readable: {err}"),
                    }
                })?;

                let passed_path = if self.working_dir.is_some() && path.is_relative() {
                    resolved_for_check
                } else {
                    path.clone()
                };
                Ok(vec![
                    OsString::from("--cookies"),
                    passed_path.into_os_string(),
                ])
            }
            CookieSource::BrowserProfile(spec) => {
                if spec.trim().is_empty() || spec.contains('\0') {
                    return Err(DownloadError::CookieSourceInvalid {
                        cookie_source: source.describe(),
                        reason: "browser profile selector is invalid".to_owned(),
                    });
                }
                Ok(vec![
                    OsString::from("--cookies-from-browser"),
                    OsString::from(spec),
                ])
            }
        }
    }

    fn build_child_argv(
        &self,
        request: &DownloadRequest,
        manifest_path: Option<&Path>,
    ) -> Result<Vec<OsString>, DownloadError> {
        request.validate()?;
        let cookie_args = self.prepare_cookie_args(request.cookies.as_ref())?;

        let mut argv = Vec::with_capacity(
            request.preset_args.len() + request.extra_args.len() + cookie_args.len() + 10,
        );
        if let Some(manifest) = manifest_path {
            let escaped_manifest = escape_yt_dlp_outtmpl_literal(manifest);
            argv.push(OsString::from("--print-to-file"));
            argv.push(OsString::from(
                "after_move:{\"kind\":\"media\",\"id\":%(id)j,\"path\":%(filepath)j}",
            ));
            argv.push(escaped_manifest.clone());

            argv.push(OsString::from("--print-to-file"));
            argv.push(OsString::from(
                "after_video:{\"kind\":\"subtitles\",\"id\":%(id)j,\"subtitles\":%(requested_subtitles.:.filepath)j}",
            ));
            argv.push(escaped_manifest);
        }

        for arg in &request.preset_args {
            argv.push(OsString::from(arg));
        }
        for arg in &request.extra_args {
            argv.push(OsString::from(arg));
        }
        argv.extend(cookie_args);
        if request.target.is_pending_upload() {
            argv.push(OsString::from("--ignore-config"));
            argv.push(OsString::from("--no-playlist"));
        }
        argv.push(OsString::from("--"));
        argv.push(OsString::from(request.target.url()));
        Ok(argv)
    }
}

impl Downloader for YtDlpDownloader {
    fn download(&self, request: &DownloadRequest) -> Result<DownloadOutcome, DownloadError> {
        request.validate()?;
        let effective_working_dir = self.resolve_effective_working_dir()?;
        let forwarded = request.forward_args();
        let mode = InvocationMode::from_args(&forwarded);
        let attach_manifest = !has_help_or_version(&forwarded);

        let manifest_guard = if attach_manifest {
            Some(TempManifestFile::create()?)
        } else {
            None
        };

        let argv =
            self.build_child_argv(request, manifest_guard.as_ref().map(TempManifestFile::path))?;

        // Pass each argument directly without invoking a shell.
        let mut command = Command::new(&self.binary);
        command.args(&argv);
        if let Some(dir) = &self.working_dir {
            command.current_dir(dir);
        }

        let output = command.output().map_err(|err| {
            if is_binary_unavailable_error(&err) {
                DownloadError::DownloaderUnavailable {
                    binary: self.binary.to_string_lossy().into_owned(),
                    reason: err.to_string(),
                }
            } else {
                DownloadError::Io(format!("failed to execute downloader process: {err}"))
            }
        })?;

        if !output.status.success() {
            let stderr_text = String::from_utf8_lossy(&output.stderr);
            let stdout_text = String::from_utf8_lossy(&output.stdout);
            let detail = if stderr_text.trim().is_empty() {
                stdout_text.trim().to_owned()
            } else {
                stderr_text.trim().to_owned()
            };
            return Err(DownloadError::ProcessFailed {
                exit_code: output.status.code(),
                stderr: detail,
            });
        }

        if !mode.expects_artifacts() {
            return Ok(DownloadOutcome {
                succeeded: true,
                confirmed_artifacts: Vec::new(),
            });
        }

        let manifest_contents = match &manifest_guard {
            Some(guard) => fs::read_to_string(guard.path()).map_err(|err| {
                DownloadError::Io(format!(
                    "failed to read downloader artifact manifest '{}': {err}",
                    guard.path().display()
                ))
            })?,
            None => String::new(),
        };

        let stdout_text = String::from_utf8_lossy(&output.stdout);
        let path_hints = OutputPathHints::from_args(&forwarded, &effective_working_dir);
        let confirmed_artifacts = confirm_reported_artifacts(
            &request.target,
            mode,
            &path_hints,
            &effective_working_dir,
            &manifest_contents,
            &stdout_text,
        );

        Ok(DownloadOutcome {
            succeeded: true,
            confirmed_artifacts,
        })
    }
}

/// Complete report returned by [`execute_download`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadExecutionReport {
    /// Resolved request sent to the downloader.
    pub request: DownloadRequest,
    /// Downloader outcome and confirmed artifact paths.
    pub outcome: DownloadOutcome,
    /// Whether a pending upload was transitioned to `downloaded`.
    pub marked_downloaded: bool,
}

/// Resolves preset/target inputs, executes the download, and updates pending upload status.
///
/// Direct URL downloads never modify upload status even when the URL matches a pending upload.
/// Pending upload downloads update status only when `yt-dlp` succeeds and at least one output
/// artifact is confirmed.
pub fn execute_download(
    storage: &dyn Storage,
    downloader: &dyn Downloader,
    input: DownloadPlanInput,
) -> Result<DownloadExecutionReport, AppError> {
    let target = match input.target {
        DownloadTargetInput::Url(url) => DownloadTarget::DirectUrl { url },
        DownloadTargetInput::Upload(upload_id) => {
            let upload = storage.get_upload(upload_id)?;
            if upload.status != UploadStatus::Pending {
                return Err(StorageError::UploadNotPending {
                    upload_id,
                    status: upload.status,
                }
                .into());
            }
            DownloadTarget::PendingUpload { upload }
        }
    };

    let (preset_name, preset_args) = match input.preset {
        PresetSelection::None => (None, Vec::new()),
        PresetSelection::Named(name) => {
            let preset = storage.get_preset(&name)?;
            (Some(preset.name), preset.args)
        }
        PresetSelection::Default => {
            let preset = storage.get_default_preset()?;
            (Some(preset.name), preset.args)
        }
    };

    let request = DownloadRequest {
        target,
        preset_name,
        preset_args,
        extra_args: input.extra_args,
        cookies: input.cookies,
    };

    let outcome = downloader.download(&request)?;
    let marked_downloaded = match &request.target {
        DownloadTarget::PendingUpload { upload } if outcome.should_mark_downloaded() => {
            storage.mark_upload_downloaded(upload.id)?;
            true
        }
        _ => false,
    };

    Ok(DownloadExecutionReport {
        request,
        outcome,
        marked_downloaded,
    })
}

struct TempManifestFile {
    path: PathBuf,
}

impl TempManifestFile {
    fn create() -> Result<Self, DownloadError> {
        let dir = std::env::temp_dir();
        for _ in 0..32 {
            let seq = NEXT_MANIFEST_SEQ.fetch_add(1, Ordering::Relaxed);
            let candidate = dir.join(format!("siphon-yt-dlp-{}-{seq}.jsonl", std::process::id()));
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&candidate)
            {
                Ok(_) => return Ok(Self { path: candidate }),
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(DownloadError::Io(format!(
                        "failed to create downloader manifest file '{}': {err}",
                        candidate.display()
                    )));
                }
            }
        }
        Err(DownloadError::Io(
            "failed to allocate unique downloader manifest file".to_owned(),
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempManifestFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn escape_yt_dlp_outtmpl_literal(path: &Path) -> OsString {
    match path.to_str() {
        Some(utf8) => OsString::from(utf8.replace('%', "%%")),
        None => path.as_os_str().to_os_string(),
    }
}

fn ensure_no_nul(label: &str, value: &str) -> Result<(), DownloadError> {
    if value.contains('\0') {
        return Err(DownloadError::InvalidArguments {
            reason: format!("{label} cannot contain null bytes"),
        });
    }
    Ok(())
}

fn is_binary_unavailable_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::NotFound
            | ErrorKind::PermissionDenied
            | ErrorKind::InvalidInput
            | ErrorKind::NotADirectory
    )
}

fn validate_single_target_slice(args: &[String], source: &str) -> Result<(), DownloadError> {
    let mut expect_values = 0usize;
    let mut active_flag = String::new();

    for arg in args {
        if expect_values > 0 {
            expect_values -= 1;
            continue;
        }

        if arg == "--" {
            return Err(DownloadError::InvalidArguments {
                reason: format!(
                    "{source} arguments cannot include '--' when downloading a tracked upload"
                ),
            });
        }

        if arg == "-" || !arg.starts_with('-') {
            return Err(DownloadError::InvalidArguments {
                reason: format!(
                    "{source} positional argument '{arg}' is not allowed when downloading a tracked upload"
                ),
            });
        }

        if let Some(rest) = arg.strip_prefix("--") {
            let (flag_name, has_eq) = match rest.split_once('=') {
                Some((name, _)) => (name, true),
                None => (rest, false),
            };
            let full_flag = format!("--{flag_name}");
            let Some(arity) = allowed_long_option_arity(flag_name) else {
                return Err(DownloadError::InvalidArguments {
                    reason: format!(
                        "{source} option '{full_flag}' is not permitted when downloading a tracked upload"
                    ),
                });
            };
            if has_eq && arity != 1 {
                return Err(DownloadError::InvalidArguments {
                    reason: format!(
                        "{source} option '{full_flag}' does not accept '=' inline syntax"
                    ),
                });
            }
            if !has_eq && arity > 0 {
                expect_values = arity;
                active_flag = full_flag;
            }
            continue;
        }

        // Short option (possibly bundled, e.g. `-qf best` or `-fbest`).
        let shorts = &arg[1..];
        for (idx, ch) in shorts.char_indices() {
            let Some(arity) = allowed_short_option_arity(ch) else {
                return Err(DownloadError::InvalidArguments {
                    reason: format!(
                        "{source} short option '-{ch}' is not permitted when downloading a tracked upload"
                    ),
                });
            };
            if arity > 0 {
                let has_inline_value = idx + ch.len_utf8() < shorts.len();
                if !has_inline_value {
                    expect_values = arity;
                    active_flag = format!("-{ch}");
                }
                break;
            }
        }
    }

    if expect_values > 0 {
        return Err(DownloadError::InvalidArguments {
            reason: format!(
                "{source} option '{active_flag}' is missing its argument value when downloading a tracked upload"
            ),
        });
    }

    Ok(())
}

/// Strict allowlist of safe canonical `yt-dlp` short options for `--upload <ID>`.
///
/// Explicitly rejects `-a` (`--batch-file`) and any unknown short option letters.
fn allowed_short_option_arity(ch: char) -> Option<usize> {
    match ch {
        'h' | 'U' | 'i' | 'q' | 'v' | 's' | 'j' | 'J' | 'g' | 'e' | 'F' | 'x' | 'k' | 'w' | 'c'
        | '4' | '6' | 'K' | 'X' => Some(0),
        'f' | 'I' | 'N' | 'o' | 'O' | 'p' | 'P' | 'r' | 'R' | 'S' | 't' | 'u' | '2' => Some(1),
        _ => None,
    }
}

/// Strict allowlist of canonical `yt-dlp` long options for `--upload <ID>`.
///
/// Excludes `--alias`, `--batch-file`, `--config-locations`, `--config-location`,
/// `--load-info-json`, all `optparse` prefix abbreviations, and all unknown options.
fn allowed_long_option_arity(flag_name: &str) -> Option<usize> {
    match flag_name {
        "help"
        | "version"
        | "update"
        | "no-update"
        | "ignore-errors"
        | "no-ignore-errors"
        | "abort-on-error"
        | "no-abort-on-error"
        | "dump-user-agent"
        | "list-extractors"
        | "extractor-descriptions"
        | "ignore-config"
        | "no-config"
        | "flat-playlist"
        | "no-flat-playlist"
        | "live-from-start"
        | "no-live-from-start"
        | "mark-watched"
        | "no-mark-watched"
        | "no-colors"
        | "quiet"
        | "no-quiet"
        | "no-warnings"
        | "simulate"
        | "no-simulate"
        | "ignore-no-formats-error"
        | "no-ignore-no-formats-error"
        | "skip-download"
        | "no-download"
        | "no-skip-download"
        | "dump-json"
        | "dump-single-json"
        | "force-write-archive"
        | "newline"
        | "no-progress"
        | "progress"
        | "console-title"
        | "verbose"
        | "dump-pages"
        | "write-pages"
        | "print-traffic"
        | "call-home"
        | "no-call-home"
        | "get-url"
        | "get-title"
        | "get-id"
        | "get-thumbnail"
        | "get-description"
        | "get-duration"
        | "get-filename"
        | "get-format"
        | "list-formats"
        | "list-subs"
        | "list-thumbnails"
        | "force-ipv4"
        | "force-ipv6"
        | "geo-bypass"
        | "no-geo-bypass"
        | "no-check-certificates"
        | "no-check-certificate"
        | "prefer-insecure"
        | "legacy-server-connect"
        | "bidi-workaround"
        | "yes-playlist"
        | "no-playlist"
        | "no-match-filters"
        | "no-break-match-filters"
        | "no-download-archive"
        | "include-ads"
        | "no-include-ads"
        | "break-on-existing"
        | "no-break-on-existing"
        | "break-on-reject"
        | "no-break-on-reject"
        | "break-per-input"
        | "no-break-per-input"
        | "lazy-playlist"
        | "no-lazy-playlist"
        | "playlist-random"
        | "playlist-reverse"
        | "no-playlist-reverse"
        | "skip-unavailable-fragments"
        | "abort-on-unavailable-fragments"
        | "no-abort-on-unavailable-fragments"
        | "keep-fragments"
        | "no-keep-fragments"
        | "no-resize-buffer"
        | "hls-use-mpegts"
        | "no-hls-use-mpegts"
        | "hls-prefer-native"
        | "hls-prefer-ffmpeg"
        | "restrict-filenames"
        | "no-restrict-filenames"
        | "windows-filenames"
        | "no-windows-filenames"
        | "no-overwrites"
        | "force-overwrites"
        | "no-force-overwrites"
        | "continue"
        | "no-continue"
        | "part"
        | "no-part"
        | "mtime"
        | "no-mtime"
        | "write-description"
        | "no-write-description"
        | "write-info-json"
        | "no-write-info-json"
        | "write-playlist-metafiles"
        | "no-write-playlist-metafiles"
        | "clean-info-json"
        | "no-clean-info-json"
        | "write-comments"
        | "no-write-comments"
        | "no-cookies"
        | "no-cookies-from-browser"
        | "no-cache-dir"
        | "rm-cache-dir"
        | "write-thumbnail"
        | "no-write-thumbnail"
        | "write-all-thumbnails"
        | "write-link"
        | "write-url-shortcut"
        | "write-webloc-shortcut"
        | "write-desktop-link"
        | "xattr-set-filesize"
        | "format-sort-force"
        | "no-format-sort-force"
        | "video-multistreams"
        | "no-video-multistreams"
        | "audio-multistreams"
        | "no-audio-multistreams"
        | "all-formats"
        | "no-all-formats"
        | "prefer-free-formats"
        | "no-prefer-free-formats"
        | "check-formats"
        | "check-all-formats"
        | "no-check-formats"
        | "allow-unplayable-formats"
        | "no-allow-unplayable-formats"
        | "write-subs"
        | "no-write-subs"
        | "write-sub"
        | "no-write-sub"
        | "write-auto-subs"
        | "no-write-auto-subs"
        | "write-auto-sub"
        | "no-write-auto-sub"
        | "all-subs"
        | "netrc"
        | "no-netrc"
        | "ap-list-mso"
        | "extract-audio"
        | "keep-video"
        | "no-keep-video"
        | "post-overwrites"
        | "no-post-overwrites"
        | "embed-subs"
        | "no-embed-subs"
        | "embed-thumbnail"
        | "no-embed-thumbnail"
        | "embed-metadata"
        | "no-embed-metadata"
        | "add-metadata"
        | "embed-chapters"
        | "no-embed-chapters"
        | "embed-info-json"
        | "no-embed-info-json"
        | "xattrs"
        | "no-exec"
        | "split-chapters"
        | "no-split-chapters"
        | "no-remove-chapters"
        | "force-keyframes-at-cuts"
        | "no-force-keyframes-at-cuts"
        | "no-sponsorblock"
        | "chapters-from-comments"
        | "no-chapters-from-comments" => Some(0),
        "update-to"
        | "use-extractors"
        | "plugin-dirs"
        | "js-runtimes"
        | "remote-components"
        | "wait-for-video"
        | "color"
        | "compat-options"
        | "preset-alias"
        | "print"
        | "progress-template"
        | "progress-delta"
        | "encoding"
        | "proxy"
        | "socket-timeout"
        | "source-address"
        | "impersonate"
        | "geo-verification-proxy"
        | "xff"
        | "user-agent"
        | "referer"
        | "add-headers"
        | "add-header"
        | "sleep-requests"
        | "sleep-interval"
        | "max-sleep-interval"
        | "sleep-subtitles"
        | "extractor-args"
        | "playlist-items"
        | "playlist-start"
        | "playlist-end"
        | "min-filesize"
        | "max-filesize"
        | "date"
        | "datebefore"
        | "dateafter"
        | "match-filters"
        | "match-filter"
        | "break-match-filters"
        | "age-limit"
        | "download-archive"
        | "max-downloads"
        | "skip-playlist-after-errors"
        | "match-title"
        | "reject-title"
        | "min-views"
        | "max-views"
        | "concurrent-fragments"
        | "limit-rate"
        | "throttled-rate"
        | "retries"
        | "file-access-retries"
        | "fragment-retries"
        | "extractor-retries"
        | "retry-sleep"
        | "buffer-size"
        | "http-chunk-size"
        | "external-downloader"
        | "downloader"
        | "external-downloader-args"
        | "downloader-args"
        | "download-sections"
        | "paths"
        | "output"
        | "output-na-placeholder"
        | "trim-filenames"
        | "cookies"
        | "cookies-from-browser"
        | "cache-dir"
        | "format"
        | "format-sort"
        | "merge-output-format"
        | "sub-format"
        | "sub-langs"
        | "sub-lang"
        | "username"
        | "password"
        | "twofactor"
        | "netrc-location"
        | "netrc-cmd"
        | "video-password"
        | "ap-mso"
        | "ap-username"
        | "ap-password"
        | "client-certificate"
        | "client-certificate-key"
        | "client-certificate-password"
        | "audio-format"
        | "audio-quality"
        | "remux-video"
        | "recode-video"
        | "postprocessor-args"
        | "ppa"
        | "concat-playlist"
        | "fixup"
        | "ffmpeg-location"
        | "exec"
        | "convert-subs"
        | "convert-subtitles"
        | "convert-thumbnails"
        | "remove-chapters"
        | "use-postprocessor"
        | "sponsorblock-mark"
        | "sponsorblock-remove"
        | "sponsorblock-chapter-title"
        | "sponsorblock-api" => Some(1),
        "print-to-file" | "parse-metadata" => Some(2),
        "replace-in-metadata" => Some(3),
        _ => None,
    }
}

fn known_arity_for_passthrough_scan(flag_name: &str) -> usize {
    match flag_name {
        "batch-file" | "config-locations" | "config-location" | "load-info-json" => 1,
        "alias" => 2,
        other => allowed_long_option_arity(other).unwrap_or(0),
    }
}

fn has_help_or_version(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            break;
        }
        if arg == "--help" || arg == "--version" || arg == "-h" {
            return true;
        }
        if !arg.starts_with("--")
            && arg.starts_with('-')
            && arg != "-"
            && let Some(ch) = arg[1..].chars().next()
            && ch == 'h'
        {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvocationMode {
    DownloadMediaOrSubs,
    SubtitleOnly,
    SimulatedOrNoDownload,
}

impl InvocationMode {
    fn expects_artifacts(self) -> bool {
        !matches!(self, Self::SimulatedOrNoDownload)
    }

    fn from_args(args: &[String]) -> Self {
        let mut explicit_simulate: Option<bool> = None;
        let mut implicit_simulate = false;
        let mut hard_no_download = false;
        let mut skip_download = false;
        let mut writes_subs = false;

        let mut i = 0usize;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--" {
                break;
            }
            if !arg.starts_with('-') || arg == "-" {
                i += 1;
                continue;
            }

            if let Some(rest) = arg.strip_prefix("--") {
                let (flag_name, inline_value) = match rest.split_once('=') {
                    Some((name, val)) => (name, Some(val)),
                    None => (rest, None),
                };
                match flag_name {
                    "simulate" => explicit_simulate = Some(true),
                    "no-simulate" => explicit_simulate = Some(false),
                    "dump-json" | "dump-single-json" | "get-url" | "get-title" | "get-id"
                    | "get-thumbnail" | "get-description" | "get-duration" | "get-filename"
                    | "get-format" => implicit_simulate = true,
                    "list-formats" | "list-subs" | "list-thumbnails" | "help" | "version" => {
                        hard_no_download = true;
                    }
                    "skip-download" | "no-download" => skip_download = true,
                    "no-skip-download" => skip_download = false,
                    "write-subs" | "write-sub" | "write-auto-subs" | "write-auto-sub"
                    | "all-subs" => {
                        writes_subs = true;
                    }
                    "no-write-subs" | "no-write-sub" | "no-write-auto-subs"
                    | "no-write-auto-sub" => {
                        writes_subs = false;
                    }
                    "print" => {
                        let value = inline_value.or_else(|| args.get(i + 1).map(String::as_str));
                        if value.is_none_or(print_template_implies_simulate) {
                            implicit_simulate = true;
                        }
                    }
                    _ => {}
                }
                if inline_value.is_none() {
                    i += 1 + known_arity_for_passthrough_scan(flag_name);
                } else {
                    i += 1;
                }
                continue;
            }

            // Short flags
            let shorts = &arg[1..];
            let mut extra_consumed = 0usize;
            for (idx, ch) in shorts.char_indices() {
                match ch {
                    's' => explicit_simulate = Some(true),
                    'j' | 'J' | 'g' | 'e' => implicit_simulate = true,
                    'F' | 'h' => hard_no_download = true,
                    'O' => {
                        let inline = &shorts[idx + ch.len_utf8()..];
                        let val = if inline.is_empty() {
                            extra_consumed = 1;
                            args.get(i + 1).map(String::as_str)
                        } else {
                            Some(inline)
                        };
                        if val.is_none_or(print_template_implies_simulate) {
                            implicit_simulate = true;
                        }
                        break;
                    }
                    ch if allowed_short_option_arity(ch) == Some(1) || ch == 'a' => {
                        let has_inline = idx + ch.len_utf8() < shorts.len();
                        if !has_inline {
                            extra_consumed = 1;
                        }
                        break;
                    }
                    _ => {}
                }
            }
            i += 1 + extra_consumed;
        }

        let simulated = hard_no_download
            || explicit_simulate == Some(true)
            || (implicit_simulate && explicit_simulate != Some(false));

        if simulated || (skip_download && !writes_subs) {
            Self::SimulatedOrNoDownload
        } else if skip_download && writes_subs {
            Self::SubtitleOnly
        } else {
            Self::DownloadMediaOrSubs
        }
    }
}

fn print_template_implies_simulate(template: &str) -> bool {
    let Some((when, _)) = template.split_once(':') else {
        return true;
    };
    !matches!(
        when,
        "post_process" | "after_move" | "after_video" | "post_dl"
    )
}

#[derive(Debug, Clone)]
struct OutputPathHints {
    explicit_dirs: Vec<PathBuf>,
    output_templates: Vec<OutputTemplateSpec>,
}

#[derive(Debug, Clone)]
struct OutputTemplateSpec {
    uses_id: bool,
    uses_title: bool,
    literal_prefix: Option<String>,
}

impl OutputPathHints {
    fn from_args(args: &[String], working_dir: &Path) -> Self {
        let mut path_entries: Vec<(Option<String>, PathBuf)> = Vec::new();
        let mut raw_outputs: Vec<String> = Vec::new();

        let mut i = 0usize;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--" {
                break;
            }
            if !arg.starts_with('-') || arg == "-" {
                i += 1;
                continue;
            }

            if let Some(rest) = arg.strip_prefix("--") {
                let (flag_name, inline_value) = match rest.split_once('=') {
                    Some((name, val)) => (name, Some(val)),
                    None => (rest, None),
                };
                match flag_name {
                    "paths" => {
                        let val = inline_value
                            .map(str::to_owned)
                            .or_else(|| args.get(i + 1).cloned());
                        if let Some(raw) = val {
                            path_entries.push(parse_typed_path_entry(&raw, working_dir));
                        }
                    }
                    "output" => {
                        let val = inline_value
                            .map(str::to_owned)
                            .or_else(|| args.get(i + 1).cloned());
                        if let Some(raw) = val {
                            raw_outputs.push(raw);
                        }
                    }
                    _ => {}
                }
                if inline_value.is_none() {
                    i += 1 + known_arity_for_passthrough_scan(flag_name);
                } else {
                    i += 1;
                }
                continue;
            }

            let shorts = &arg[1..];
            let mut extra_consumed = 0usize;
            for (idx, ch) in shorts.char_indices() {
                if ch == 'P' || ch == 'o' {
                    let inline = &shorts[idx + ch.len_utf8()..];
                    let val = if inline.is_empty() {
                        extra_consumed = 1;
                        args.get(i + 1).cloned()
                    } else {
                        Some(inline.to_owned())
                    };
                    if let Some(raw) = val {
                        if ch == 'P' {
                            path_entries.push(parse_typed_path_entry(&raw, working_dir));
                        } else {
                            raw_outputs.push(raw);
                        }
                    }
                    break;
                }
                if allowed_short_option_arity(ch) == Some(1) || ch == 'a' {
                    let has_inline = idx + ch.len_utf8() < shorts.len();
                    if !has_inline {
                        extra_consumed = 1;
                    }
                    break;
                }
            }
            i += 1 + extra_consumed;
        }

        let mut home_dir: Option<PathBuf> = None;
        let mut explicit_dirs = Vec::new();
        for (kind, path) in &path_entries {
            if kind.as_deref() == Some("temp") {
                continue;
            }
            if kind.is_none() || kind.as_deref() == Some("home") {
                home_dir = Some(path.clone());
            }
            if !explicit_dirs.contains(path) {
                explicit_dirs.push(path.clone());
            }
        }

        let mut output_templates = Vec::new();
        for raw in raw_outputs {
            if raw == "-" || raw.trim().is_empty() {
                continue;
            }
            let (kind, template) = split_yt_dlp_type_prefix(&raw);
            if matches!(
                kind.as_deref(),
                Some("thumbnail" | "description" | "infojson")
            ) {
                continue;
            }
            if let Some(dir) =
                extract_static_template_dir(template, home_dir.as_deref(), working_dir)
                && !explicit_dirs.contains(&dir)
            {
                explicit_dirs.push(dir);
            }
            if let Some(spec) = OutputTemplateSpec::parse(template) {
                output_templates.push(spec);
            }
        }

        Self {
            explicit_dirs,
            output_templates,
        }
    }
}

impl OutputTemplateSpec {
    fn parse(template: &str) -> Option<Self> {
        let normalized = template.replace('\\', "/");
        let file_pattern = normalized
            .rsplit('/')
            .next()
            .unwrap_or(&normalized)
            .trim()
            .to_owned();
        if file_pattern.is_empty() {
            return None;
        }

        let uses_id = file_pattern.contains("%(id)");
        let uses_title = file_pattern.contains("%(title)");
        let before_placeholder = file_pattern
            .split("%(")
            .next()
            .unwrap_or("")
            .trim_end_matches('.');
        let literal_prefix = if before_placeholder.is_empty() {
            None
        } else {
            Some(before_placeholder.to_owned())
        };

        Some(Self {
            uses_id,
            uses_title,
            literal_prefix,
        })
    }
}

fn split_yt_dlp_type_prefix(raw: &str) -> (Option<String>, &str) {
    if let Some((prefix, rest)) = raw.split_once(':')
        && YT_DLP_PATH_TYPES.contains(&prefix)
    {
        return (Some(prefix.to_owned()), rest);
    }
    (None, raw)
}

fn parse_typed_path_entry(raw: &str, working_dir: &Path) -> (Option<String>, PathBuf) {
    let (kind, raw_path) = split_yt_dlp_type_prefix(raw);
    let path = PathBuf::from(raw_path);
    let resolved = if path.is_relative() {
        working_dir.join(path)
    } else {
        path
    };
    (kind, resolved)
}

fn extract_static_template_dir(
    template: &str,
    home_dir: Option<&Path>,
    working_dir: &Path,
) -> Option<PathBuf> {
    let prefix = template.split("%(").next().unwrap_or(template);
    let parent = if prefix.ends_with('/') || prefix.ends_with('\\') {
        Path::new(prefix)
    } else {
        Path::new(prefix).parent()?
    };
    if parent.as_os_str().is_empty() {
        return None;
    }
    let resolved = if parent.is_relative() {
        home_dir.unwrap_or(working_dir).join(parent)
    } else {
        parent.to_path_buf()
    };
    Some(resolved)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReportedCandidate {
    path: PathBuf,
    reported_id: Option<String>,
}

fn confirm_reported_artifacts(
    target: &DownloadTarget,
    mode: InvocationMode,
    path_hints: &OutputPathHints,
    working_dir: &Path,
    manifest_contents: &str,
    stdout: &str,
) -> Vec<PathBuf> {
    let mut candidates: Vec<ReportedCandidate> = Vec::new();

    for record in parse_manifest_records(manifest_contents) {
        if let Some(resolved) =
            resolve_reported_path(&record.path, working_dir, &path_hints.explicit_dirs)
        {
            merge_candidate(
                &mut candidates,
                ReportedCandidate {
                    path: resolved,
                    reported_id: record.id,
                },
            );
        }
    }

    for reported in parse_stdout_artifact_paths(stdout) {
        if let Some(resolved) =
            resolve_reported_path(&reported, working_dir, &path_hints.explicit_dirs)
        {
            merge_candidate(
                &mut candidates,
                ReportedCandidate {
                    path: resolved,
                    reported_id: None,
                },
            );
        }
    }

    let mut confirmed: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        if !is_valid_artifact_file(&candidate.path, mode) {
            continue;
        }
        if let DownloadTarget::PendingUpload { upload } = target
            && !candidate_matches_upload(&candidate, upload, &path_hints.output_templates)
        {
            continue;
        }
        if !confirmed
            .iter()
            .any(|seen| paths_equivalent(seen.as_path(), &candidate.path))
        {
            confirmed.push(candidate.path);
        }
    }
    confirmed.sort();
    confirmed
}

fn merge_candidate(candidates: &mut Vec<ReportedCandidate>, incoming: ReportedCandidate) {
    if let Some(existing) = candidates
        .iter_mut()
        .find(|item| paths_equivalent(&item.path, &incoming.path))
    {
        if existing.reported_id.is_none() {
            existing.reported_id = incoming.reported_id;
        }
    } else {
        candidates.push(incoming);
    }
}

fn resolve_reported_path(
    raw: &Path,
    working_dir: &Path,
    explicit_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if raw.as_os_str().is_empty() || raw == Path::new("-") {
        return None;
    }
    if raw.is_absolute() {
        return raw.is_file().then(|| raw.to_path_buf());
    }
    let from_working = working_dir.join(raw);
    if from_working.is_file() {
        return Some(from_working);
    }
    for dir in explicit_dirs {
        let candidate = dir.join(raw);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ManifestRecord {
    id: Option<String>,
    path: PathBuf,
}

fn parse_manifest_records(contents: &str) -> Vec<ManifestRecord> {
    let mut records = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(JsonValue::Object(obj)) = parse_json(trimmed) else {
            continue;
        };
        let id = match obj.get("id") {
            Some(JsonValue::String(s)) if !s.is_empty() && s != "NA" => Some(s.clone()),
            _ => None,
        };
        match obj.get("kind") {
            Some(JsonValue::String(kind)) if kind == "media" => {
                if let Some(JsonValue::String(path)) = obj.get("path")
                    && !path.is_empty()
                    && path != "NA"
                    && path != "-"
                {
                    records.push(ManifestRecord {
                        id,
                        path: PathBuf::from(path),
                    });
                }
            }
            Some(JsonValue::String(kind)) if kind == "subtitles" => {
                if let Some(JsonValue::Object(subs)) = obj.get("subtitles") {
                    for val in subs.values() {
                        if let JsonValue::String(path) = val
                            && !path.is_empty()
                            && path != "NA"
                            && path != "-"
                        {
                            records.push(ManifestRecord {
                                id: id.clone(),
                                path: PathBuf::from(path),
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }
    records
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonValue {
    Null,
    Bool(bool),
    Number,
    String(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

fn parse_json(input: &str) -> Result<JsonValue, ()> {
    let mut parser = JsonParser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let val = parser.parse_value()?;
    parser.skip_ws();
    if parser.pos == parser.bytes.len() {
        Ok(val)
    } else {
        Err(())
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_value(&mut self) -> Result<JsonValue, ()> {
        self.skip_ws();
        match self.peek().ok_or(())? {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(JsonValue::String),
            b'n' => {
                self.expect_literal(b"null")?;
                Ok(JsonValue::Null)
            }
            b't' => {
                self.expect_literal(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            b'f' => {
                self.expect_literal(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            b'-' | b'0'..=b'9' => {
                self.consume_number();
                Ok(JsonValue::Number)
            }
            _ => Err(()),
        }
    }

    fn expect_literal(&mut self, lit: &[u8]) -> Result<(), ()> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(())
        }
    }

    fn consume_number(&mut self) {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_array(&mut self) -> Result<JsonValue, ()> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek().ok_or(())? {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    return Ok(JsonValue::Array(items));
                }
                _ => return Err(()),
            }
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, ()> {
        self.pos += 1; // '{'
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(());
            }
            self.pos += 1;
            let value = self.parse_value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek().ok_or(())? {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    return Ok(JsonValue::Object(map));
                }
                _ => return Err(()),
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, ()> {
        if self.peek() != Some(b'"') {
            return Err(());
        }
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                let esc = *self.bytes.get(self.pos).ok_or(())?;
                self.pos += 1;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            // UTF-16 surrogate pair
                            if self.bytes[self.pos..].starts_with(b"\\u") {
                                self.pos += 2;
                                let low = self.parse_hex4()?;
                                if (0xDC00..=0xDFFF).contains(&low) {
                                    let combined = 0x10000
                                        + (((code as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                                    let ch = char::from_u32(combined).ok_or(())?;
                                    out.push(ch);
                                } else {
                                    return Err(());
                                }
                            } else {
                                return Err(());
                            }
                        } else {
                            let ch = char::from_u32(code as u32).ok_or(())?;
                            out.push(ch);
                        }
                    }
                    _ => return Err(()),
                }
                continue;
            }
            let s = std::str::from_utf8(&self.bytes[self.pos..]).map_err(|_| ())?;
            let ch = s.chars().next().ok_or(())?;
            out.push(ch);
            self.pos += ch.len_utf8();
        }
        Err(())
    }

    fn parse_hex4(&mut self) -> Result<u16, ()> {
        if self.pos + 4 > self.bytes.len() {
            return Err(());
        }
        let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4]).map_err(|_| ())?;
        self.pos += 4;
        u16::from_str_radix(hex, 16).map_err(|_| ())
    }
}

fn parse_stdout_artifact_paths(stdout: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for raw_line in stdout.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("[download] Destination: ") {
            push_non_empty_path(&mut paths, rest);
            continue;
        }
        if let Some(rest) = line.strip_prefix("[download] ")
            && let Some(path_str) = rest.strip_suffix(" has already been downloaded")
        {
            push_non_empty_path(&mut paths, path_str);
            continue;
        }
        if let Some(rest) = line.strip_prefix("[Merger] Merging formats into ") {
            push_non_empty_path(&mut paths, unquote_yt_dlp_path(rest));
            continue;
        }
        if let Some(rest) = line.strip_prefix("[ExtractAudio] Destination: ") {
            push_non_empty_path(&mut paths, rest);
            continue;
        }
        if let Some(rest) = line.strip_prefix("[info] Writing video subtitles to: ") {
            push_non_empty_path(&mut paths, rest);
            continue;
        }
        if let Some(rest) = line.strip_prefix("[info] Writing subtitles to: ") {
            push_non_empty_path(&mut paths, rest);
            continue;
        }
        if let Some(rest) = line.strip_prefix("[MoveFiles] Moving file ")
            && let Some((_, dest)) = rest.split_once(" to ")
        {
            push_non_empty_path(&mut paths, unquote_yt_dlp_path(dest));
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("[VideoRemuxer] ")
            .or_else(|| line.strip_prefix("[VideoConvertor] "))
        {
            if let Some(inner) = rest.strip_prefix("Not converting video file ") {
                push_non_empty_path(&mut paths, unquote_yt_dlp_path(inner));
            } else if let Some((_, dest)) = rest.split_once(" to ") {
                push_non_empty_path(&mut paths, unquote_yt_dlp_path(dest));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("[EmbedSubtitle] Embedding subtitles in ") {
            push_non_empty_path(&mut paths, unquote_yt_dlp_path(rest));
            continue;
        }
    }
    paths
}

fn push_non_empty_path(out: &mut Vec<PathBuf>, raw: &str) {
    let trimmed = raw.trim();
    if !trimmed.is_empty() && trimmed != "-" {
        out.push(PathBuf::from(trimmed));
    }
}

fn unquote_yt_dlp_path(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

fn is_valid_artifact_file(path: &Path, mode: InvocationMode) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }

    let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    if file_name.contains(".part-Frag") || file_name.ends_with(".info.json") {
        return false;
    }
    if file_name.ends_with(".live_chat.json") || file_name.ends_with(".annotations.xml") {
        return false;
    }

    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase);
    let Some(ext) = ext else {
        return false;
    };

    if IGNORED_EXTENSIONS.contains(&ext.as_str()) {
        return false;
    }

    match mode {
        InvocationMode::SimulatedOrNoDownload => false,
        InvocationMode::SubtitleOnly => SUBTITLE_EXTENSIONS.contains(&ext.as_str()),
        InvocationMode::DownloadMediaOrSubs => {
            MEDIA_EXTENSIONS.contains(&ext.as_str()) || SUBTITLE_EXTENSIONS.contains(&ext.as_str())
        }
    }
}

fn candidate_matches_upload(
    candidate: &ReportedCandidate,
    upload: &Upload,
    output_templates: &[OutputTemplateSpec],
) -> bool {
    if let Some(reported_id) = &candidate.reported_id {
        return id_matches_upload(reported_id, &upload.platform_upload_id);
    }

    // Fallback for stdout-only reports when `--print-to-file` manifest records are absent.
    let Some(file_name) = candidate.path.file_name().and_then(OsStr::to_str) else {
        return false;
    };

    let sanitized_id = sanitize_yt_dlp_component(&upload.platform_upload_id);
    if file_name.contains(&upload.platform_upload_id)
        || (!sanitized_id.is_empty() && file_name.contains(&sanitized_id))
    {
        return true;
    }

    let custom_without_id = output_templates.iter().any(|tpl| !tpl.uses_id);
    if !custom_without_id {
        return false;
    }

    if contains_conflicting_bracket_id(file_name, &upload.platform_upload_id) {
        return false;
    }

    for tpl in output_templates {
        if tpl.uses_id {
            continue;
        }
        if let Some(prefix) = &tpl.literal_prefix
            && file_name.starts_with(prefix)
        {
            return true;
        }
        if tpl.uses_title && !upload.title.trim().is_empty() {
            let sanitized_title = sanitize_yt_dlp_component(&upload.title);
            if file_name.contains(&upload.title)
                || (!sanitized_title.is_empty() && file_name.contains(&sanitized_title))
            {
                return true;
            }
        }
        if tpl.literal_prefix.is_none() && !tpl.uses_title {
            return true;
        }
    }

    false
}

fn id_matches_upload(reported_id: &str, expected_id: &str) -> bool {
    if reported_id == expected_id {
        return true;
    }
    let sanitized = sanitize_yt_dlp_component(expected_id);
    if reported_id == sanitized {
        return true;
    }
    // Single-part Bilibili uploads may carry `_p1` suffix in yt-dlp metadata.
    if let Some(rest) = reported_id
        .strip_prefix(expected_id)
        .or_else(|| reported_id.strip_prefix(&sanitized))
    {
        return rest.starts_with("_p") && rest[2..].bytes().all(|b| b.is_ascii_digit());
    }
    false
}

fn contains_conflicting_bracket_id(file_name: &str, expected_id: &str) -> bool {
    let Some(start) = file_name.rfind('[') else {
        return false;
    };
    let Some(rel_end) = file_name[start + 1..].find(']') else {
        return false;
    };
    let inside = &file_name[start + 1..start + 1 + rel_end];
    !inside.is_empty() && inside != expected_id && !inside.contains(expected_id)
}

fn sanitize_yt_dlp_component(raw: &str) -> String {
    raw.chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CreatorId, Platform, Timestamp};

    fn sample_upload() -> Upload {
        Upload {
            id: UploadId::new(7).unwrap(),
            creator_id: CreatorId::new(1).unwrap(),
            platform: Platform::Bilibili,
            platform_upload_id: "BV1xx411c7mD".to_owned(),
            url: "https://www.bilibili.com/video/BV1xx411c7mD".to_owned(),
            title: "Sample Upload".to_owned(),
            published_at: Timestamp::from_unix_seconds(1_700_000_000),
            status: UploadStatus::Pending,
        }
    }

    fn pending_request(preset_args: Vec<&str>, extra_args: Vec<&str>) -> DownloadRequest {
        DownloadRequest {
            target: DownloadTarget::PendingUpload {
                upload: sample_upload(),
            },
            preset_name: None,
            preset_args: preset_args.into_iter().map(str::to_owned).collect(),
            extra_args: extra_args.into_iter().map(str::to_owned).collect(),
            cookies: None,
        }
    }

    #[test]
    fn build_yt_dlp_args_keeps_direct_vs_pending_order() {
        let direct = DownloadRequest {
            target: DownloadTarget::DirectUrl {
                url: "https://www.bilibili.com/video/BV1direct".to_owned(),
            },
            preset_name: Some(PresetName::builtin_video()),
            preset_args: vec!["-f".to_owned(), "bv*+ba/b".to_owned()],
            extra_args: vec!["--yes-playlist".to_owned()],
            cookies: Some(CookieSource::BrowserProfile("firefox".to_owned())),
        };
        assert_eq!(
            direct.build_yt_dlp_args(),
            vec![
                "-f",
                "bv*+ba/b",
                "--yes-playlist",
                "--cookies-from-browser",
                "firefox",
                "--",
                "https://www.bilibili.com/video/BV1direct",
            ]
        );

        let pending = pending_request(vec!["--yes-playlist"], vec!["--write-subs"]);
        assert_eq!(
            pending.build_yt_dlp_args(),
            vec![
                "--yes-playlist",
                "--write-subs",
                "--ignore-config",
                "--no-playlist",
                "--",
                "https://www.bilibili.com/video/BV1xx411c7mD",
            ]
        );
    }

    #[test]
    fn pending_upload_validation_rejects_aliases_abbreviations_unknowns_and_multi_target() {
        let cases = [
            pending_request(vec![], vec!["--", "https://other.example/video"]),
            pending_request(vec!["--"], vec![]),
            pending_request(vec![], vec!["https://other.example/video"]),
            pending_request(vec![], vec!["-f", "best", "https://other.example/video"]),
            pending_request(vec![], vec!["-a", "urls.txt"]),
            pending_request(vec![], vec!["-aurls.txt"]),
            pending_request(vec![], vec!["-qa", "urls.txt"]),
            pending_request(vec![], vec!["--batch-file", "urls.txt"]),
            pending_request(vec![], vec!["--batch-file=urls.txt"]),
            pending_request(vec![], vec!["--batch", "urls.txt"]),
            pending_request(vec![], vec!["--batch=urls.txt"]),
            pending_request(vec!["--config-locations", "custom.conf"], vec![]),
            pending_request(vec![], vec!["--config-location=custom.conf"]),
            pending_request(vec![], vec!["--config=custom.conf"]),
            pending_request(vec![], vec!["--config-loc", "custom.conf"]),
            pending_request(vec![], vec!["--load-info-json", "other.info.json"]),
            pending_request(vec![], vec!["--load-info=other.info.json"]),
            pending_request(vec![], vec!["--alias", "--x", "--batch-file urls.txt"]),
            pending_request(vec![], vec!["--ali", "--x", "https://other.example"]),
            pending_request(vec![], vec!["--unknown-flag"]),
            pending_request(vec![], vec!["--unknown-flag=value"]),
            pending_request(vec![], vec!["--simulate=yes"]),
            pending_request(vec![], vec!["-z"]),
            pending_request(vec!["-f"], vec!["best"]),
            pending_request(vec![], vec!["--output"]),
        ];

        for req in cases {
            assert!(
                matches!(req.validate(), Err(DownloadError::InvalidArguments { .. })),
                "expected InvalidArguments for {req:?}"
            );
        }
    }

    #[test]
    fn pending_upload_validation_allows_legitimate_yt_dlp_option_values() {
        let valid = pending_request(
            vec!["-f", "bv*+ba/b", "--yes-playlist"],
            vec![
                "--referer",
                "https://www.bilibili.com/",
                "--proxy",
                "http://127.0.0.1:7890",
                "--print-to-file",
                "after_move:filepath",
                "out.txt",
                "--replace-in-metadata",
                "title",
                "foo",
                "bar",
                "-o",
                "%(title)s [%(id)s].%(ext)s",
            ],
        );
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn invocation_mode_detects_simulation_and_subtitle_only() {
        let to_vec = |slice: &[&str]| slice.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();

        assert_eq!(
            InvocationMode::from_args(&to_vec(&["-f", "bv*+ba/b"])),
            InvocationMode::DownloadMediaOrSubs
        );
        assert_eq!(
            InvocationMode::from_args(&to_vec(&["--simulate"])),
            InvocationMode::SimulatedOrNoDownload
        );
        assert_eq!(
            InvocationMode::from_args(&to_vec(&["-qs"])),
            InvocationMode::SimulatedOrNoDownload
        );
        assert_eq!(
            InvocationMode::from_args(&to_vec(&["--simulate", "--no-simulate"])),
            InvocationMode::DownloadMediaOrSubs
        );
        assert_eq!(
            InvocationMode::from_args(&to_vec(&["--dump-json"])),
            InvocationMode::SimulatedOrNoDownload
        );
        assert_eq!(
            InvocationMode::from_args(&to_vec(&["--print", "title"])),
            InvocationMode::SimulatedOrNoDownload
        );
        assert_eq!(
            InvocationMode::from_args(&to_vec(&["--print", "after_move:filepath"])),
            InvocationMode::DownloadMediaOrSubs
        );
        assert_eq!(
            InvocationMode::from_args(&to_vec(&["--skip-download"])),
            InvocationMode::SimulatedOrNoDownload
        );
        assert_eq!(
            InvocationMode::from_args(&to_vec(&["--skip-download", "--write-subs"])),
            InvocationMode::SubtitleOnly
        );
        assert_eq!(
            InvocationMode::from_args(&to_vec(&["--skip-download", "--write-subs", "--simulate"])),
            InvocationMode::SimulatedOrNoDownload
        );
    }

    #[test]
    fn parses_manifest_records_with_escaped_json_and_subtitles() {
        let manifest = r#"
{"kind":"media","id":"BV1xx411c7mD","path":"/tmp/My \"Video\" [BV1xx411c7mD].mp4"}
{"kind":"subtitles","id":"BV1xx411c7mD","subtitles":{"en":"/tmp/sub.en.vtt","zh-Hans":"/tmp/sub.zh.srt"}}
{"kind":"media","id":"BV1xx411c7mD","path":null}
{"kind":"subtitles","id":"BV1xx411c7mD","subtitles":null}
"#;
        let records = parse_manifest_records(manifest);
        assert_eq!(
            records,
            vec![
                ManifestRecord {
                    id: Some("BV1xx411c7mD".to_owned()),
                    path: PathBuf::from(r#"/tmp/My "Video" [BV1xx411c7mD].mp4"#),
                },
                ManifestRecord {
                    id: Some("BV1xx411c7mD".to_owned()),
                    path: PathBuf::from("/tmp/sub.en.vtt"),
                },
                ManifestRecord {
                    id: Some("BV1xx411c7mD".to_owned()),
                    path: PathBuf::from("/tmp/sub.zh.srt"),
                },
            ]
        );
    }

    #[test]
    fn parses_yt_dlp_stdout_artifact_lines() {
        let stdout = "\
[download] Destination: Video [BV1xx].f137.mp4
[Merger] Merging formats into \"Video [BV1xx].mp4\"
[ExtractAudio] Destination: Audio [BV1xx].mp3
[info] Writing video subtitles to: Video [BV1xx].en.vtt
[download] Existing [BV1xx].mp4 has already been downloaded
[MoveFiles] Moving file \"tmp.srt\" to \"Final [BV1xx].srt\"
";
        let paths = parse_stdout_artifact_paths(stdout);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("Video [BV1xx].f137.mp4"),
                PathBuf::from("Video [BV1xx].mp4"),
                PathBuf::from("Audio [BV1xx].mp3"),
                PathBuf::from("Video [BV1xx].en.vtt"),
                PathBuf::from("Existing [BV1xx].mp4"),
                PathBuf::from("Final [BV1xx].srt"),
            ]
        );
    }

    #[test]
    fn candidate_matching_checks_manifest_id_and_stdout_fallback() {
        let upload = sample_upload();

        assert!(candidate_matches_upload(
            &ReportedCandidate {
                path: PathBuf::from("arbitrary_custom_name.mp4"),
                reported_id: Some("BV1xx411c7mD".to_owned()),
            },
            &upload,
            &[],
        ));
        assert!(candidate_matches_upload(
            &ReportedCandidate {
                path: PathBuf::from("arbitrary_custom_name.mp4"),
                reported_id: Some("BV1xx411c7mD_p1".to_owned()),
            },
            &upload,
            &[],
        ));
        assert!(!candidate_matches_upload(
            &ReportedCandidate {
                path: PathBuf::from("Sample Upload [BV1xx411c7mD].mp4"),
                reported_id: Some("BV999other".to_owned()),
            },
            &upload,
            &[],
        ));
        assert!(candidate_matches_upload(
            &ReportedCandidate {
                path: PathBuf::from("Sample Upload [BV1xx411c7mD].en.srt"),
                reported_id: None,
            },
            &upload,
            &[],
        ));
        assert!(!candidate_matches_upload(
            &ReportedCandidate {
                path: PathBuf::from("unrelated.mp4"),
                reported_id: None,
            },
            &upload,
            &[],
        ));
    }
}
