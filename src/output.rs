//! Typed command outputs, JSON response envelopes, and TTY/plain human rendering.
//!
//! ## Extending for background jobs / new storage errors
//!
//! - New [`StorageError`] variants (for example, `JobNotFound`, `OperationBusy`, or
//!   `StaleJobOwnership`) should add their stable snake_case code in
//!   [`StorageError::error_code`] and any structured context in `storage_error_details`
//!   inside this module.
//! - New CLI commands (such as `job status` or `job list`) can add variants to
//!   [`CommandResult`] or serialize any `Serialize` payload directly via [`JsonResponse::ok`],
//!   and reuse [`crate::download::DownloadExecutionOptions::background_capture`] when running
//!   `yt-dlp` from background workers.

use std::env;
use std::error::Error as _;
use std::fmt::Write as _;
use std::io::{self, IsTerminal};

use clap::error::ErrorKind as ClapErrorKind;
use serde::Serialize;

use crate::domain::{
    CookieSource, Creator, DownloadPreset, FollowedCreator, PresetSummary, Timestamp, Upload,
    UploadStatus,
};
use crate::download::{DownloadExecutionReport, DownloadTarget};
use crate::error::{
    AppError, BackgroundError, DownloadError, RefreshError, StorageError, ValidationError,
};
use crate::jobs::{CreatorJobOutcome, JobOutcome, JobProgress, JobRecord, JobState, JobTarget};
use crate::refresh::{CreatorRefreshOutcome, RefreshSummary};

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_CYAN: &str = "\x1b[36m";

/// Formatted user-facing output produced for standard output and standard error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Text printed to standard output.
    pub stdout: String,
    /// Text printed to standard error (for example, Siphon download summaries).
    pub stderr: String,
}

impl CommandOutput {
    /// Creates a [`CommandOutput`] carrying only standard output text.
    #[must_use]
    pub fn from_stdout(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// Creates a [`CommandOutput`] carrying only standard error text.
    #[must_use]
    pub fn from_stderr(stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// Top-level JSON response envelope emitted on stdout when `--json` is supplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JsonResponse<T: Serialize> {
    /// Whether the command succeeded (`true`) or failed (`false`).
    pub ok: bool,
    /// Structured payload present on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    /// Structured error present on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonError>,
}

impl<T: Serialize> JsonResponse<T> {
    /// Builds a successful JSON response wrapping `data`.
    #[must_use]
    pub fn ok(data: T) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// Serializes the response as a single compact JSON line (terminated by `\n`).
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

impl JsonResponse<serde_json::Value> {
    /// Builds a failed JSON response wrapping `error`.
    #[must_use]
    pub fn err(error: JsonError) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error),
        }
    }
}

/// Structured error payload inside [`JsonResponse::error`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JsonError {
    /// Stable machine-readable snake_case error code.
    pub code: &'static str,
    /// Human-readable diagnostic summary.
    pub message: String,
    /// Optional structured error context (for example, per-creator refresh outcomes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl JsonError {
    /// Creates a [`JsonError`] with the given stable code and message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Attaches structured JSON details to this error.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    /// Creates a configuration/bootstrap error payload.
    #[must_use]
    pub fn invalid_config(message: impl Into<String>) -> Self {
        Self::new("invalid_config", message)
    }

    /// Converts a [`clap::Error`] into a structured [`JsonError`].
    #[must_use]
    pub fn from_clap_error(err: &clap::Error) -> Self {
        if err.kind() == ClapErrorKind::ValueValidation
            && let Some(val_err) = find_validation_error_source(err)
        {
            return Self::from_validation_error(val_err);
        }

        let raw = strip_ansi(&err.to_string());
        let usage = extract_usage_line(&raw);
        let message = if err.kind() == ClapErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
            "missing required subcommand or argument".to_owned()
        } else {
            extract_clap_error_message(&raw)
        };

        let mut details = serde_json::Map::new();
        details.insert(
            "kind".to_owned(),
            serde_json::Value::String(clap_kind_slug(err.kind()).to_owned()),
        );
        details.insert(
            "exit_code".to_owned(),
            serde_json::Value::Number(serde_json::Number::from(err.exit_code())),
        );
        if let Some(usage_str) = usage {
            details.insert("usage".to_owned(), serde_json::Value::String(usage_str));
        }

        Self::new("invalid_arguments", message).with_details(serde_json::Value::Object(details))
    }

    /// Converts an [`AppError`] (and optional full [`RefreshSummary`] on partial failure) into a [`JsonError`].
    #[must_use]
    pub fn from_app_error(err: &AppError, partial_refresh: Option<&RefreshSummary>) -> Self {
        let code = err.error_code();
        let message = err.to_string();
        let details = match err {
            AppError::Validation(val) => return Self::from_validation_error(val),
            AppError::Storage(storage) => storage_error_details(storage),
            AppError::Refresh(refresh) => refresh_error_details(refresh),
            AppError::Download(download) => download_error_details(download),
            AppError::Background(background) => background_error_details(background),
            AppError::RefreshPartialFailure {
                succeeded,
                failures,
            } => {
                if let Some(summary) = partial_refresh {
                    serde_json::to_value(RefreshReportDto::from_summary(summary)).ok()
                } else {
                    let failure_dtos: Vec<serde_json::Value> = failures
                        .iter()
                        .map(|f| {
                            serde_json::json!({
                                "status": "failed",
                                "creator_id": f.creator_id.as_i64(),
                                "display_name": f.display_name,
                                "error": {
                                    "message": f.error,
                                }
                            })
                        })
                        .collect();
                    Some(serde_json::json!({
                        "succeeded": succeeded,
                        "failed": failures.len(),
                        "total": succeeded + failures.len(),
                        "outcomes": failure_dtos,
                    }))
                }
            }
        };

        let mut out = Self::new(code, message);
        if let Some(details) = details {
            out = out.with_details(details);
        }
        out
    }

    fn from_validation_error(err: &ValidationError) -> Self {
        let details = match err {
            ValidationError::InvalidCreatorId { input }
            | ValidationError::InvalidUploadId { input }
            | ValidationError::InvalidFollowId { input }
            | ValidationError::InvalidJobId { input }
            | ValidationError::InvalidOwnershipToken { input } => Some(serde_json::json!({
                "input": input,
            })),
            ValidationError::InvalidPresetName { input, reason } => Some(serde_json::json!({
                "input": input,
                "reason": reason,
            })),
            ValidationError::ReservedPresetName { name } => Some(serde_json::json!({
                "preset": name,
            })),
            ValidationError::EmptyValue { field } => Some(serde_json::json!({
                "field": field,
            })),
            ValidationError::InvalidCookiePath { reason } => Some(serde_json::json!({
                "reason": reason,
            })),
            ValidationError::InvalidDisplayUrl { reason }
            | ValidationError::InvalidProcessOwner { reason } => Some(serde_json::json!({
                "reason": reason,
            })),
            ValidationError::UnsupportedPlatform { platform } => Some(serde_json::json!({
                "platform": platform,
            })),
            ValidationError::InvalidUploadStatus { status } => Some(serde_json::json!({
                "status": status,
            })),
            ValidationError::InvalidJobKind { kind } => Some(serde_json::json!({
                "kind": kind,
            })),
            ValidationError::InvalidJobState { state } => Some(serde_json::json!({
                "state": state,
            })),
            ValidationError::InvalidDownloadStage { stage } => Some(serde_json::json!({
                "stage": stage,
            })),
            ValidationError::InvalidExecutionMode { mode } => Some(serde_json::json!({
                "mode": mode,
            })),
            ValidationError::EmptyDatabasePath => None,
        };
        let mut out = Self::new(err.error_code(), err.to_string());
        if let Some(details) = details {
            out = out.with_details(details);
        }
        out
    }
}

fn find_validation_error_source(err: &clap::Error) -> Option<&ValidationError> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = err.source();
    while let Some(source) = current {
        if let Some(val) = source.downcast_ref::<ValidationError>() {
            return Some(val);
        }
        current = source.source();
    }
    None
}

fn storage_error_details(err: &StorageError) -> Option<serde_json::Value> {
    match err {
        StorageError::CreatorNotFound { creator_id }
        | StorageError::CreatorNotFollowed { creator_id }
        | StorageError::StaleRefreshCheckpoint { creator_id } => Some(serde_json::json!({
            "creator_id": creator_id.as_i64(),
        })),
        StorageError::CreatorAlreadyFollowed {
            creator_id,
            display_name,
        } => Some(serde_json::json!({
            "creator_id": creator_id.as_i64(),
            "display_name": display_name,
        })),
        StorageError::UploadNotFound { upload_id } => Some(serde_json::json!({
            "upload_id": upload_id.as_i64(),
        })),
        StorageError::UploadNotPending { upload_id, status } => Some(serde_json::json!({
            "upload_id": upload_id.as_i64(),
            "status": status.as_str(),
        })),
        StorageError::PresetNotFound { name }
        | StorageError::PresetAlreadyExists { name }
        | StorageError::CannotModifyBuiltinPreset { name } => Some(serde_json::json!({
            "preset": name.as_str(),
        })),
        StorageError::JobNotFound { job_id } => Some(serde_json::json!({
            "job_id": job_id.as_i64(),
        })),
        StorageError::OperationBusy { active } => Some(serde_json::json!({
            "slot": active.slot.slot_key(),
            "execution_mode": active.execution_mode.as_str(),
            "job_id": active.job_id.map(|id| id.as_i64()),
            "pid": active.owner.pid,
            "acquired_at": active.acquired_at.format_utc_iso(),
        })),
        StorageError::StaleJobOwnership { job_id, slot } => Some(serde_json::json!({
            "job_id": job_id.map(|id| id.as_i64()),
            "slot": slot.map(|s| s.slot_key()),
        })),
        StorageError::NotImplemented
        | StorageError::CorruptedData(_)
        | StorageError::Backend(_) => None,
    }
}

fn refresh_error_details(err: &RefreshError) -> Option<serde_json::Value> {
    match err {
        RefreshError::InvalidCreatorUrl { url, reason } => Some(serde_json::json!({
            "url": url,
            "reason": reason,
        })),
        RefreshError::CreatorNotFoundRemote { url } => Some(serde_json::json!({
            "url": url,
        })),
        RefreshError::CookieSourceInvalid {
            cookie_source,
            reason,
        } => Some(serde_json::json!({
            "cookie_source": cookie_source,
            "reason": reason,
        })),
        RefreshError::Storage(storage) => storage_error_details(storage),
        RefreshError::NotImplemented
        | RefreshError::Network(_)
        | RefreshError::PlatformResponse(_) => None,
    }
}

fn download_error_details(err: &DownloadError) -> Option<serde_json::Value> {
    match err {
        DownloadError::DownloaderUnavailable { binary, reason } => Some(serde_json::json!({
            "binary": binary,
            "reason": reason,
        })),
        DownloadError::ProcessFailed { exit_code, stderr } => Some(serde_json::json!({
            "exit_code": exit_code,
            "stderr": stderr,
        })),
        DownloadError::CookieSourceInvalid {
            cookie_source,
            reason,
        } => Some(serde_json::json!({
            "cookie_source": cookie_source,
            "reason": reason,
        })),
        DownloadError::InvalidArguments { reason } => Some(serde_json::json!({
            "reason": reason,
        })),
        DownloadError::MediaToStdoutDisallowed { source_detail } => Some(serde_json::json!({
            "source": source_detail,
        })),
        DownloadError::Storage(storage) => storage_error_details(storage),
        DownloadError::NotImplemented | DownloadError::Io(_) => None,
    }
}

fn background_error_details(err: &BackgroundError) -> Option<serde_json::Value> {
    match err {
        BackgroundError::WorkerSpawnFailed { binary, reason } => Some(serde_json::json!({
            "binary": binary,
            "reason": reason,
        })),
        BackgroundError::WorkerStartupFailed { reason } => Some(serde_json::json!({
            "reason": reason,
        })),
        BackgroundError::UnsupportedStorage
        | BackgroundError::WorkerAckTimeout
        | BackgroundError::Io(_) => None,
    }
}

/// Typed outcome of any Siphon CLI command prior to rendering as JSON or human text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult {
    /// Result of `siphon creator add <URL>`.
    CreatorAdded(FollowedCreator),
    /// Result of `siphon creator list`.
    CreatorsListed(Vec<FollowedCreator>),
    /// Result of `siphon creator remove <ID>`.
    CreatorRemoved(Creator),
    /// Result of `siphon refresh [CREATOR_ID]`.
    Refreshed(RefreshSummary),
    /// Result of `siphon upload list`.
    UploadsListed(Vec<Upload>),
    /// Result of `siphon upload ignore <ID>`.
    UploadIgnored(Upload),
    /// Result of `siphon download ...`.
    Downloaded(DownloadExecutionReport),
    /// Result of `siphon preset add <NAME> -- <ARGS>`.
    PresetAdded(DownloadPreset),
    /// Result of `siphon preset list`.
    PresetsListed(Vec<PresetSummary>),
    /// Result of `siphon preset remove <NAME>`.
    PresetRemoved(crate::storage::RemovePresetOutcome),
    /// Result of `siphon preset set-default <NAME>`.
    PresetDefaultSet(DownloadPreset),
    /// Result of `siphon download --background` or `siphon refresh --background`.
    JobStarted(JobRecord),
    /// Result of `siphon job status <ID>`.
    JobStatus(JobRecord),
}

impl CommandResult {
    /// Serializes this typed command result into its structured JSON `data` value.
    pub fn to_json_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        match self {
            Self::CreatorAdded(followed) => {
                serde_json::to_value(FollowedCreatorDto::from_domain(followed))
            }
            Self::CreatorsListed(creators) => serde_json::to_value(CreatorListData {
                creators: creators
                    .iter()
                    .map(FollowedCreatorDto::from_domain)
                    .collect(),
            }),
            Self::CreatorRemoved(creator) => serde_json::to_value(CreatorRemovedData {
                creator: CreatorDto::from_domain(creator),
                uploads_preserved: true,
            }),
            Self::Refreshed(summary) => {
                serde_json::to_value(RefreshReportDto::from_summary(summary))
            }
            Self::UploadsListed(uploads) => serde_json::to_value(UploadListData {
                uploads: uploads.iter().map(UploadDto::from_domain).collect(),
            }),
            Self::UploadIgnored(upload) => serde_json::to_value(UploadIgnoredData {
                upload: UploadDto::from_domain(upload),
            }),
            Self::Downloaded(report) => serde_json::to_value(DownloadData::from_report(report)),
            Self::PresetAdded(preset) => serde_json::to_value(PresetMutationData {
                preset: PresetDto::from_domain(preset),
            }),
            Self::PresetsListed(presets) => serde_json::to_value(PresetListData {
                presets: presets.iter().map(PresetSummaryDto::from_domain).collect(),
            }),
            Self::PresetRemoved(outcome) => serde_json::to_value(PresetRemovedData {
                removed: outcome.removed.as_str().to_owned(),
                reset_default_to_builtin: outcome.reset_default_to_builtin,
            }),
            Self::PresetDefaultSet(preset) => serde_json::to_value(PresetMutationData {
                preset: PresetDto::from_domain(preset),
            }),
            Self::JobStarted(job) | Self::JobStatus(job) => {
                serde_json::to_value(JobRecordDto::from_domain(job))
            }
        }
    }

    /// Serializes this typed command result as a complete successful [`JsonResponse`] line.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        let data = self.to_json_value()?;
        JsonResponse::ok(data).to_json_line()
    }

    /// Renders this typed command result for human consumption according to `options`.
    #[must_use]
    pub fn render_human(&self, options: HumanRenderOptions) -> CommandOutput {
        match self {
            Self::CreatorAdded(followed) => CommandOutput::from_stdout(render_creator_added(
                followed,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::CreatorsListed(creators) => CommandOutput::from_stdout(render_creators_list(
                creators,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::CreatorRemoved(creator) => CommandOutput::from_stdout(render_creator_removed(
                creator,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::Refreshed(summary) => CommandOutput::from_stdout(render_refresh_summary(
                summary,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::UploadsListed(uploads) => CommandOutput::from_stdout(render_uploads_list(
                uploads,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::UploadIgnored(upload) => CommandOutput::from_stdout(render_upload_ignored(
                upload,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::Downloaded(report) => CommandOutput::from_stderr(render_download_summary(
                report,
                options.stderr_tty,
                options.stderr_color,
            )),
            Self::PresetAdded(preset) => CommandOutput::from_stdout(render_preset_added(
                preset,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::PresetsListed(presets) => CommandOutput::from_stdout(render_presets_list(
                presets,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::PresetRemoved(outcome) => CommandOutput::from_stdout(render_preset_removed(
                outcome,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::PresetDefaultSet(preset) => CommandOutput::from_stdout(render_preset_default(
                preset,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::JobStarted(job) => CommandOutput::from_stdout(render_job_started(
                job,
                options.stdout_tty,
                options.stdout_color,
            )),
            Self::JobStatus(job) => CommandOutput::from_stdout(render_job_status(
                job,
                options.stdout_tty,
                options.stdout_color,
            )),
        }
    }
}

/// Controls TTY alignment and ANSI color usage for human-mode rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HumanRenderOptions {
    /// Whether standard output is an interactive terminal.
    pub stdout_tty: bool,
    /// Whether ANSI color should be emitted on standard output.
    pub stdout_color: bool,
    /// Whether standard error is an interactive terminal.
    pub stderr_tty: bool,
    /// Whether ANSI color should be emitted on standard error.
    pub stderr_color: bool,
}

impl HumanRenderOptions {
    /// Plain non-TTY output (TSV/plain text, no ANSI escapes).
    #[must_use]
    pub const fn plain() -> Self {
        Self {
            stdout_tty: false,
            stdout_color: false,
            stderr_tty: false,
            stderr_color: false,
        }
    }

    /// Interactive TTY output with explicit color control.
    #[must_use]
    pub const fn tty(color: bool) -> Self {
        Self {
            stdout_tty: true,
            stdout_color: color,
            stderr_tty: true,
            stderr_color: color,
        }
    }

    /// Detects terminal attachment and `NO_COLOR` / `TERM` environment settings.
    #[must_use]
    pub fn detect() -> Self {
        let stdout_tty = io::stdout().is_terminal();
        let stderr_tty = io::stderr().is_terminal();
        let get_env = |key: &str| env::var(key).ok();
        Self {
            stdout_tty,
            stdout_color: should_use_color_from_env(stdout_tty, get_env),
            stderr_tty,
            stderr_color: should_use_color_from_env(stderr_tty, get_env),
        }
    }
}

/// Determines whether ANSI color should be enabled given TTY status and environment lookup.
///
/// Respects `NO_COLOR` (when present and non-empty per `no-color.org`) and `TERM=dumb`.
#[must_use]
pub fn should_use_color_from_env<F>(is_tty: bool, get_env: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    if !is_tty {
        return false;
    }
    if get_env("NO_COLOR").is_some_and(|val| !val.is_empty()) {
        return false;
    }
    if get_env("TERM").is_some_and(|val| val == "dumb") {
        return false;
    }
    true
}

/// Formats a human-readable error line for stderr, respecting color settings.
#[must_use]
pub fn format_human_error(message: &str, use_color: bool) -> String {
    if use_color {
        format!("{ANSI_BOLD}{ANSI_RED}error:{ANSI_RESET} {message}\n")
    } else {
        format!("error: {message}\n")
    }
}

/// Structured version metadata returned for `siphon --json --version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionData {
    /// Executable/package name.
    pub name: String,
    /// Package version string.
    pub version: String,
}

impl VersionData {
    /// Returns version metadata for the compiled `siphon` binary.
    #[must_use]
    pub fn current() -> Self {
        Self {
            name: env!("CARGO_PKG_NAME").to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Structured command help metadata returned for `siphon --json --help`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HelpData {
    /// Full command invocation name (for example, `"siphon creator"`).
    pub command: String,
    /// Command path segments (for example, `["siphon", "creator"]`).
    pub path: Vec<String>,
    /// Package version, if configured on this command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Short command description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// Extended command description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_about: Option<String>,
    /// Usage synopsis for this command.
    pub usage: String,
    /// Rendered plain-text help output produced by `clap`.
    pub help_text: String,
    /// Visible child subcommands.
    pub subcommands: Vec<HelpSubcommandDto>,
    /// Visible positional arguments.
    pub arguments: Vec<HelpArgumentDto>,
    /// Visible flags and options.
    pub options: Vec<HelpOptionDto>,
}

/// Subcommand entry inside [`HelpData`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HelpSubcommandDto {
    /// Subcommand name.
    pub name: String,
    /// Short description of the subcommand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
}

/// Positional argument entry inside [`HelpData`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HelpArgumentDto {
    /// Argument identifier.
    pub id: String,
    /// Value placeholders (for example, `["URL"]`).
    pub value_names: Vec<String>,
    /// Whether the argument is required.
    pub required: bool,
    /// Whether multiple values are accepted.
    pub multiple: bool,
    /// Whether this argument is captured after `--`.
    pub last: bool,
    /// Short help text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

/// Option/flag entry inside [`HelpData`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HelpOptionDto {
    /// Option identifier.
    pub id: String,
    /// Short flag (if any, e.g. `"-h"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short: Option<String>,
    /// Long flag (if any, e.g. `"--json"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long: Option<String>,
    /// Whether the option applies globally to subcommands.
    pub global: bool,
    /// Whether the option is required.
    pub required: bool,
    /// Whether the option takes one or more values.
    pub takes_value: bool,
    /// Value placeholder names when `takes_value` is `true`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub value_names: Vec<String>,
    /// Environment variable fallback, if configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Short help text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl HelpData {
    /// Extracts structured command metadata and rendered help text directly from a `clap`
    /// `DisplayHelp` error, guaranteeing parity with non-JSON `--help` / `-h` resolution.
    #[must_use]
    pub fn from_clap_help_error(err: &clap::Error) -> Self {
        let help_text = strip_ansi(&err.to_string());
        let usage_line = extract_usage_line(&help_text);
        let (path, cmd) =
            crate::cli::resolve_command_from_usage(usage_line.as_deref().unwrap_or("siphon"));
        Self::from_clap_command_with_text(&path, &cmd, Some(help_text), usage_line)
    }

    /// Extracts structured command metadata from a [`clap::Command`] and its resolved path.
    #[must_use]
    pub fn from_clap_command(path: &[String], cmd: &clap::Command) -> Self {
        Self::from_clap_command_with_text(path, cmd, None, None)
    }

    fn from_clap_command_with_text(
        path: &[String],
        cmd: &clap::Command,
        rendered_help: Option<String>,
        rendered_usage: Option<String>,
    ) -> Self {
        let mut build_cmd = cmd.clone();
        if path.len() > 1 {
            build_cmd = build_cmd.bin_name(path.join(" "));
        }
        build_cmd.build();

        let help_text =
            rendered_help.unwrap_or_else(|| strip_ansi(&build_cmd.render_long_help().to_string()));
        let usage = rendered_usage.unwrap_or_else(|| {
            let raw_usage = strip_ansi(&build_cmd.render_usage().to_string());
            raw_usage
                .trim()
                .strip_prefix("Usage:")
                .map_or_else(|| raw_usage.trim().to_owned(), |s| s.trim().to_owned())
        });

        let subcommands = build_cmd
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
            .map(|sub| HelpSubcommandDto {
                name: sub.get_name().to_owned(),
                about: sub.get_about().map(ToString::to_string),
            })
            .collect();

        let arguments = build_cmd
            .get_positionals()
            .filter(|arg| !arg.is_hide_set())
            .map(|arg| {
                let num_args = arg.get_num_args();
                HelpArgumentDto {
                    id: arg.get_id().as_str().to_owned(),
                    value_names: arg
                        .get_value_names()
                        .unwrap_or_default()
                        .iter()
                        .map(|s| s.as_str().to_owned())
                        .collect(),
                    required: arg.is_required_set(),
                    multiple: num_args.is_some_and(|r| r.max_values() > 1),
                    last: arg.is_last_set(),
                    help: arg.get_help().map(ToString::to_string),
                }
            })
            .collect();

        let options = build_cmd
            .get_arguments()
            .filter(|arg| !arg.is_positional() && !arg.is_hide_set())
            .map(|arg| {
                let takes_value = arg.get_action().takes_values();
                HelpOptionDto {
                    id: arg.get_id().as_str().to_owned(),
                    short: arg.get_short().map(|c| format!("-{c}")),
                    long: arg.get_long().map(|l| format!("--{l}")),
                    global: arg.is_global_set(),
                    required: arg.is_required_set(),
                    takes_value,
                    value_names: if takes_value {
                        arg.get_value_names()
                            .unwrap_or_default()
                            .iter()
                            .map(|s| s.as_str().to_owned())
                            .collect()
                    } else {
                        Vec::new()
                    },
                    env: arg
                        .get_env()
                        .and_then(|s| s.to_str())
                        .map(ToString::to_string),
                    help: arg.get_help().map(ToString::to_string),
                }
            })
            .collect();

        Self {
            command: path.join(" "),
            path: path.to_vec(),
            version: build_cmd.get_version().map(str::to_owned),
            about: build_cmd.get_about().map(ToString::to_string),
            long_about: build_cmd.get_long_about().map(ToString::to_string),
            usage,
            help_text,
            subcommands,
            arguments,
            options,
        }
    }
}

#[derive(Debug, Serialize)]
struct CreatorDto {
    id: i64,
    platform: String,
    platform_creator_id: String,
    display_name: String,
}

impl CreatorDto {
    fn from_domain(creator: &Creator) -> Self {
        Self {
            id: creator.id.as_i64(),
            platform: creator.platform.as_str().to_owned(),
            platform_creator_id: creator.platform_creator_id.clone(),
            display_name: creator.display_name.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct FollowDto {
    id: i64,
    creator_id: i64,
    followed_at: String,
    refresh_checkpoint: String,
}

impl FollowDto {
    fn from_domain(follow: &crate::domain::Follow) -> Self {
        Self {
            id: follow.id.as_i64(),
            creator_id: follow.creator_id.as_i64(),
            followed_at: follow.followed_at.format_utc_iso(),
            refresh_checkpoint: follow.refresh_checkpoint.format_utc_iso(),
        }
    }
}

#[derive(Debug, Serialize)]
struct FollowedCreatorDto {
    creator: CreatorDto,
    follow: FollowDto,
}

impl FollowedCreatorDto {
    fn from_domain(item: &FollowedCreator) -> Self {
        Self {
            creator: CreatorDto::from_domain(&item.creator),
            follow: FollowDto::from_domain(&item.follow),
        }
    }
}

#[derive(Debug, Serialize)]
struct CreatorListData {
    creators: Vec<FollowedCreatorDto>,
}

#[derive(Debug, Serialize)]
struct CreatorRemovedData {
    creator: CreatorDto,
    uploads_preserved: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CookieSourceDto {
    File { path: String },
    BrowserProfile { spec: String },
}

impl CookieSourceDto {
    fn from_domain(source: &CookieSource) -> Self {
        match source {
            CookieSource::File(path) => Self::File {
                path: path.to_string_lossy().into_owned(),
            },
            CookieSource::BrowserProfile(spec) => Self::BrowserProfile { spec: spec.clone() },
        }
    }
}

#[derive(Debug, Serialize)]
struct RefreshOutcomeErrorDto {
    code: &'static str,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum RefreshOutcomeDto {
    Succeeded {
        creator_id: i64,
        display_name: String,
        new_uploads_inserted: usize,
        existing_uploads_seen: usize,
        previous_checkpoint: String,
        new_checkpoint: String,
    },
    Failed {
        creator_id: i64,
        display_name: String,
        checkpoint: String,
        error: RefreshOutcomeErrorDto,
    },
}

#[derive(Debug, Serialize)]
struct RefreshReportDto {
    succeeded: usize,
    failed: usize,
    total: usize,
    used_cookies: Option<CookieSourceDto>,
    cleared_default_cookies: bool,
    saved_default_cookies: bool,
    outcomes: Vec<RefreshOutcomeDto>,
}

impl RefreshReportDto {
    fn from_summary(summary: &RefreshSummary) -> Self {
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let mut outcomes = Vec::with_capacity(summary.outcomes.len());
        for outcome in &summary.outcomes {
            match outcome {
                CreatorRefreshOutcome::Succeeded(committed) => {
                    succeeded += 1;
                    outcomes.push(RefreshOutcomeDto::Succeeded {
                        creator_id: committed.creator_id.as_i64(),
                        display_name: committed.display_name.clone(),
                        new_uploads_inserted: committed.new_uploads_inserted,
                        existing_uploads_seen: committed.existing_uploads_seen,
                        previous_checkpoint: committed.previous_checkpoint.format_utc_iso(),
                        new_checkpoint: committed.new_checkpoint.format_utc_iso(),
                    });
                }
                CreatorRefreshOutcome::Failed { creator, error } => {
                    failed += 1;
                    outcomes.push(RefreshOutcomeDto::Failed {
                        creator_id: creator.creator.id.as_i64(),
                        display_name: creator.creator.display_name.clone(),
                        checkpoint: creator.follow.refresh_checkpoint.format_utc_iso(),
                        error: RefreshOutcomeErrorDto {
                            code: error.error_code(),
                            message: error.to_string(),
                        },
                    });
                }
            }
        }
        Self {
            succeeded,
            failed,
            total: summary.outcomes.len(),
            used_cookies: summary
                .used_cookies
                .as_ref()
                .map(CookieSourceDto::from_domain),
            cleared_default_cookies: summary.cleared_default_cookies,
            saved_default_cookies: summary.saved_default_cookies,
            outcomes,
        }
    }
}

#[derive(Debug, Serialize)]
struct UploadDto {
    id: i64,
    creator_id: i64,
    platform: String,
    platform_upload_id: String,
    url: String,
    title: String,
    published_at: String,
    status: String,
}

impl UploadDto {
    fn from_domain(upload: &Upload) -> Self {
        Self::from_domain_with_status(upload, upload.status)
    }

    fn from_domain_with_status(upload: &Upload, status: UploadStatus) -> Self {
        Self {
            id: upload.id.as_i64(),
            creator_id: upload.creator_id.as_i64(),
            platform: upload.platform.as_str().to_owned(),
            platform_upload_id: upload.platform_upload_id.clone(),
            url: upload.url.clone(),
            title: upload.title.clone(),
            published_at: upload.published_at.format_utc_iso(),
            status: status.as_str().to_owned(),
        }
    }
}

#[derive(Debug, Serialize)]
struct UploadListData {
    uploads: Vec<UploadDto>,
}

#[derive(Debug, Serialize)]
struct UploadIgnoredData {
    upload: UploadDto,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DownloadTargetDto {
    DirectUrl { url: String },
    PendingUpload { upload: UploadDto },
}

#[derive(Debug, Serialize)]
struct DownloadData {
    target: DownloadTargetDto,
    preset: Option<String>,
    extra_args: Vec<String>,
    cookies: Option<CookieSourceDto>,
    succeeded: bool,
    confirmed_artifacts: Vec<String>,
    marked_downloaded: bool,
}

impl DownloadData {
    fn from_report(report: &DownloadExecutionReport) -> Self {
        let target = match &report.request.target {
            DownloadTarget::DirectUrl { url } => DownloadTargetDto::DirectUrl { url: url.clone() },
            DownloadTarget::PendingUpload { upload } => {
                let effective_status = if report.marked_downloaded {
                    UploadStatus::Downloaded
                } else {
                    upload.status
                };
                DownloadTargetDto::PendingUpload {
                    upload: UploadDto::from_domain_with_status(upload, effective_status),
                }
            }
        };
        Self {
            target,
            preset: report
                .request
                .preset_name
                .as_ref()
                .map(|p| p.as_str().to_owned()),
            extra_args: report.request.extra_args.clone(),
            cookies: report
                .request
                .cookies
                .as_ref()
                .map(CookieSourceDto::from_domain),
            succeeded: report.outcome.succeeded,
            confirmed_artifacts: report
                .outcome
                .confirmed_artifacts
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            marked_downloaded: report.marked_downloaded,
        }
    }
}

#[derive(Debug, Serialize)]
struct PresetDto {
    name: String,
    args: Vec<String>,
    is_builtin: bool,
}

impl PresetDto {
    fn from_domain(preset: &DownloadPreset) -> Self {
        Self {
            name: preset.name.as_str().to_owned(),
            args: preset.args.clone(),
            is_builtin: preset.is_builtin,
        }
    }
}

#[derive(Debug, Serialize)]
struct PresetMutationData {
    preset: PresetDto,
}

#[derive(Debug, Serialize)]
struct PresetSummaryDto {
    name: String,
    args: Vec<String>,
    is_builtin: bool,
    is_default: bool,
}

impl PresetSummaryDto {
    fn from_domain(entry: &PresetSummary) -> Self {
        Self {
            name: entry.preset.name.as_str().to_owned(),
            args: entry.preset.args.clone(),
            is_builtin: entry.preset.is_builtin,
            is_default: entry.is_default,
        }
    }
}

#[derive(Debug, Serialize)]
struct PresetListData {
    presets: Vec<PresetSummaryDto>,
}

#[derive(Debug, Serialize)]
struct PresetRemovedData {
    removed: String,
    reset_default_to_builtin: bool,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JobTargetDto {
    Refresh { creator_id: Option<i64> },
    UploadDownload { upload_id: i64, url: Option<String> },
    DirectDownload { url: String },
}

impl JobTargetDto {
    fn from_domain(target: &JobTarget) -> Self {
        match target {
            JobTarget::Refresh { creator_id } => Self::Refresh {
                creator_id: creator_id.map(|id| id.as_i64()),
            },
            JobTarget::UploadDownload { upload_id, url } => Self::UploadDownload {
                upload_id: upload_id.as_i64(),
                url: url.as_ref().map(|u| u.as_str().to_owned()),
            },
            JobTarget::DirectDownload { url } => Self::DirectDownload {
                url: url.as_str().to_owned(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JobProgressDto {
    Refresh {
        total_creators: Option<u64>,
        processed_creators: u64,
        succeeded_creators: u64,
        failed_creators: u64,
        current_creator_id: Option<i64>,
        current_creator_name: Option<String>,
    },
    Download {
        stage: &'static str,
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
    },
}

impl JobProgressDto {
    fn from_domain(progress: &JobProgress) -> Self {
        match progress {
            JobProgress::Refresh(refresh) => Self::Refresh {
                total_creators: refresh.total_creators,
                processed_creators: refresh.processed_creators,
                succeeded_creators: refresh.succeeded_creators,
                failed_creators: refresh.failed_creators,
                current_creator_id: refresh.current_creator_id.map(|id| id.as_i64()),
                current_creator_name: refresh.current_creator_name.clone(),
            },
            JobProgress::Download(download) => Self::Download {
                stage: download.stage.as_str(),
                downloaded_bytes: download.downloaded_bytes,
                total_bytes: download.total_bytes,
            },
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum JobCreatorOutcomeDto {
    Succeeded {
        creator_id: i64,
        display_name: String,
        new_uploads_inserted: u64,
        existing_uploads_seen: u64,
        previous_checkpoint: String,
        new_checkpoint: String,
    },
    Failed {
        creator_id: i64,
        display_name: String,
        error: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JobOutcomeDto {
    Refresh {
        succeeded: usize,
        failed: usize,
        total: usize,
        creator_outcomes: Vec<JobCreatorOutcomeDto>,
        error: Option<String>,
    },
    Download {
        confirmed_artifacts: Vec<String>,
        marked_downloaded: bool,
        error: Option<String>,
    },
    Interrupted {
        reason: String,
    },
}

impl JobOutcomeDto {
    fn from_domain(outcome: &JobOutcome) -> Self {
        match outcome {
            JobOutcome::Refresh(refresh) => {
                let mut succeeded = 0usize;
                let mut failed = 0usize;
                let creator_outcomes = refresh
                    .creator_outcomes
                    .iter()
                    .map(|item| match item {
                        CreatorJobOutcome::Succeeded {
                            creator_id,
                            display_name,
                            new_uploads_inserted,
                            existing_uploads_seen,
                            previous_checkpoint,
                            new_checkpoint,
                        } => {
                            succeeded += 1;
                            JobCreatorOutcomeDto::Succeeded {
                                creator_id: creator_id.as_i64(),
                                display_name: display_name.clone(),
                                new_uploads_inserted: *new_uploads_inserted,
                                existing_uploads_seen: *existing_uploads_seen,
                                previous_checkpoint: previous_checkpoint.format_utc_iso(),
                                new_checkpoint: new_checkpoint.format_utc_iso(),
                            }
                        }
                        CreatorJobOutcome::Failed {
                            creator_id,
                            display_name,
                            error,
                        } => {
                            failed += 1;
                            JobCreatorOutcomeDto::Failed {
                                creator_id: creator_id.as_i64(),
                                display_name: display_name.clone(),
                                error: error.clone(),
                            }
                        }
                    })
                    .collect();
                Self::Refresh {
                    succeeded,
                    failed,
                    total: succeeded + failed,
                    creator_outcomes,
                    error: refresh.error.clone(),
                }
            }
            JobOutcome::Download(download) => Self::Download {
                confirmed_artifacts: download
                    .confirmed_artifacts
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
                marked_downloaded: download.marked_downloaded,
                error: download.error.clone(),
            },
            JobOutcome::Interrupted(interrupted) => Self::Interrupted {
                reason: interrupted.reason.clone(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct JobRecordDto {
    id: i64,
    kind: &'static str,
    target: JobTargetDto,
    state: &'static str,
    pid: u32,
    started_at: String,
    updated_at: String,
    finished_at: Option<String>,
    progress: Option<JobProgressDto>,
    outcome: Option<JobOutcomeDto>,
}

impl JobRecordDto {
    fn from_domain(job: &JobRecord) -> Self {
        Self {
            id: job.id.as_i64(),
            kind: job.target.kind().as_str(),
            target: JobTargetDto::from_domain(&job.target),
            state: job.state.as_str(),
            pid: job.owner.pid,
            started_at: job.started_at.format_utc_iso(),
            updated_at: job.updated_at.format_utc_iso(),
            finished_at: job.finished_at.map(Timestamp::format_utc_iso),
            progress: job.progress.as_ref().map(JobProgressDto::from_domain),
            outcome: job.outcome.as_ref().map(JobOutcomeDto::from_domain),
        }
    }
}

fn render_creator_added(followed: &FollowedCreator, is_tty: bool, color: bool) -> String {
    if !is_tty {
        return format!(
            "Followed creator {} ({}) [{}] starting at {}\n",
            followed.creator.id,
            followed.creator.display_name,
            followed.creator.platform_creator_id,
            followed.follow.refresh_checkpoint
        );
    }
    let badge = style_badge("Followed", ANSI_GREEN, color);
    let cp = style_dim(&followed.follow.refresh_checkpoint.format_utc_iso(), color);
    format!(
        "{badge} creator {} ({}) [{}] starting at {cp}\n",
        followed.creator.id,
        style_bold(&followed.creator.display_name, color),
        followed.creator.platform_creator_id,
    )
}

fn render_creator_removed(creator: &Creator, is_tty: bool, color: bool) -> String {
    if !is_tty {
        return format!(
            "Unfollowed creator {} ({}); existing uploads preserved\n",
            creator.id, creator.display_name
        );
    }
    let badge = style_badge("Unfollowed", ANSI_YELLOW, color);
    format!(
        "{badge} creator {} ({}); existing uploads preserved\n",
        creator.id,
        style_bold(&creator.display_name, color)
    )
}

fn render_creators_list(creators: &[FollowedCreator], is_tty: bool, color: bool) -> String {
    if creators.is_empty() {
        return if is_tty {
            format!("{}\n", style_dim("No followed creators.", color))
        } else {
            "No followed creators.\n".to_owned()
        };
    }

    if !is_tty {
        let mut out = String::new();
        for item in creators {
            let _ = writeln!(
                out,
                "{}\t{}\t{}\t{}\tfollowed={}\tcheckpoint={}",
                item.creator.id,
                item.creator.platform,
                item.creator.platform_creator_id,
                item.creator.display_name,
                item.follow.followed_at,
                item.follow.refresh_checkpoint,
            );
        }
        return out;
    }

    let headers = [
        "ID",
        "PLATFORM",
        "CREATOR ID",
        "NAME",
        "FOLLOWED",
        "CHECKPOINT",
    ];
    let rows: Vec<[String; 6]> = creators
        .iter()
        .map(|item| {
            [
                item.creator.id.to_string(),
                item.creator.platform.to_string(),
                item.creator.platform_creator_id.clone(),
                item.creator.display_name.clone(),
                item.follow.followed_at.format_utc_iso(),
                item.follow.refresh_checkpoint.format_utc_iso(),
            ]
        })
        .collect();
    render_aligned_table(
        &headers,
        &rows,
        color,
        |col_idx, cell, padded| match col_idx {
            0 => style_colored(padded, ANSI_CYAN, color),
            3 => style_bold(padded, color),
            4 => style_dim(padded, color),
            5 => style_dim(cell, color),
            _ => padded.to_owned(),
        },
    )
}

fn render_refresh_summary(summary: &RefreshSummary, is_tty: bool, color: bool) -> String {
    let mut out = String::new();
    if summary.cleared_default_cookies {
        let line = "Cleared remembered default refresh cookie source.";
        if is_tty {
            let _ = writeln!(out, "{}", style_dim(line, color));
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
    if summary.saved_default_cookies
        && let Some(cookies) = &summary.used_cookies
    {
        let line = format!("Saved default refresh cookie source: {cookies}.");
        if is_tty {
            let _ = writeln!(out, "{}", style_dim(&line, color));
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
    if summary.outcomes.is_empty() {
        let line = "No followed creators to refresh.";
        if is_tty {
            let _ = writeln!(out, "{}", style_dim(line, color));
        } else {
            let _ = writeln!(out, "{line}");
        }
        return out;
    }

    if !is_tty {
        for outcome in &summary.outcomes {
            match outcome {
                CreatorRefreshOutcome::Succeeded(committed) => {
                    let _ = writeln!(
                        out,
                        "Refreshed creator {} ({}): {} new upload(s), {} known, checkpoint {} -> {}",
                        committed.creator_id,
                        committed.display_name,
                        committed.new_uploads_inserted,
                        committed.existing_uploads_seen,
                        committed.previous_checkpoint,
                        committed.new_checkpoint,
                    );
                }
                CreatorRefreshOutcome::Failed { creator, error } => {
                    let _ = writeln!(
                        out,
                        "Failed creator {} ({}): {}",
                        creator.creator.id, creator.creator.display_name, error
                    );
                }
            }
        }
        return out;
    }

    let id_width = summary
        .outcomes
        .iter()
        .map(|o| match o {
            CreatorRefreshOutcome::Succeeded(c) => display_width(&c.creator_id.to_string()),
            CreatorRefreshOutcome::Failed { creator, .. } => {
                display_width(&creator.creator.id.to_string())
            }
        })
        .max()
        .unwrap_or(1);
    let name_width = summary
        .outcomes
        .iter()
        .map(|o| match o {
            CreatorRefreshOutcome::Succeeded(c) => display_width(&c.display_name),
            CreatorRefreshOutcome::Failed { creator, .. } => {
                display_width(&creator.creator.display_name)
            }
        })
        .max()
        .unwrap_or(1);

    for outcome in &summary.outcomes {
        match outcome {
            CreatorRefreshOutcome::Succeeded(committed) => {
                let status = style_badge("OK  ", ANSI_GREEN, color);
                let id_pad = pad_right(&committed.creator_id.to_string(), id_width);
                let name_pad = pad_right(&committed.display_name, name_width);
                let cp = style_dim(
                    &format!(
                        "{} -> {}",
                        committed.previous_checkpoint, committed.new_checkpoint
                    ),
                    color,
                );
                let _ = writeln!(
                    out,
                    "{status}  #{id_pad}  {}  {} new, {} known  {cp}",
                    style_bold(&name_pad, color),
                    committed.new_uploads_inserted,
                    committed.existing_uploads_seen,
                );
            }
            CreatorRefreshOutcome::Failed { creator, error } => {
                let status = style_badge("FAIL", ANSI_RED, color);
                let id_pad = pad_right(&creator.creator.id.to_string(), id_width);
                let name_pad = pad_right(&creator.creator.display_name, name_width);
                let _ = writeln!(
                    out,
                    "{status}  #{id_pad}  {}  {error}",
                    style_bold(&name_pad, color),
                );
            }
        }
    }
    out
}

fn render_uploads_list(uploads: &[Upload], is_tty: bool, color: bool) -> String {
    if uploads.is_empty() {
        return if is_tty {
            format!("{}\n", style_dim("No pending uploads.", color))
        } else {
            "No pending uploads.\n".to_owned()
        };
    }

    if !is_tty {
        let mut out = String::new();
        for upload in uploads {
            let _ = writeln!(
                out,
                "{}\tcreator={}\t{}\t{}\t{}",
                upload.id, upload.creator_id, upload.published_at, upload.title, upload.url
            );
        }
        return out;
    }

    let headers = ["ID", "CREATOR", "PUBLISHED", "TITLE", "URL"];
    let rows: Vec<[String; 5]> = uploads
        .iter()
        .map(|upload| {
            [
                upload.id.to_string(),
                upload.creator_id.to_string(),
                upload.published_at.format_utc_iso(),
                upload.title.clone(),
                upload.url.clone(),
            ]
        })
        .collect();
    render_aligned_table(
        &headers,
        &rows,
        color,
        |col_idx, cell, padded| match col_idx {
            0 => style_colored(padded, ANSI_CYAN, color),
            2 => style_dim(padded, color),
            3 => style_bold(padded, color),
            4 => style_dim(cell, color),
            _ => padded.to_owned(),
        },
    )
}

fn render_upload_ignored(upload: &Upload, is_tty: bool, color: bool) -> String {
    if !is_tty {
        return format!("Ignored upload {} ({})\n", upload.id, upload.title);
    }
    let badge = style_badge("Ignored", ANSI_YELLOW, color);
    format!(
        "{badge} upload {} ({})\n",
        upload.id,
        style_bold(&upload.title, color)
    )
}

fn render_download_summary(report: &DownloadExecutionReport, is_tty: bool, color: bool) -> String {
    let (plain, accent) = match &report.request.target {
        DownloadTarget::DirectUrl { url } => {
            if report.outcome.confirmed_artifacts.is_empty() {
                (
                    format!("Finished direct URL {url} without confirmed output artifact"),
                    ANSI_YELLOW,
                )
            } else {
                (
                    format!(
                        "Downloaded direct URL {url} ({} artifact(s) confirmed)",
                        report.outcome.confirmed_artifacts.len()
                    ),
                    ANSI_GREEN,
                )
            }
        }
        DownloadTarget::PendingUpload { upload } => {
            if report.marked_downloaded {
                (
                    format!(
                        "Downloaded upload {} ({}) and marked as downloaded ({} artifact(s) confirmed)",
                        upload.id,
                        upload.title,
                        report.outcome.confirmed_artifacts.len()
                    ),
                    ANSI_GREEN,
                )
            } else {
                (
                    format!(
                        "Finished upload {} ({}) without confirmed output artifact; status remains pending",
                        upload.id, upload.title
                    ),
                    ANSI_YELLOW,
                )
            }
        }
    };

    if is_tty && color {
        format!("{accent}{plain}{ANSI_RESET}\n")
    } else {
        format!("{plain}\n")
    }
}

fn render_preset_added(preset: &DownloadPreset, is_tty: bool, color: bool) -> String {
    if !is_tty {
        return format!(
            "Saved preset '{}' with {} argument(s)\n",
            preset.name,
            preset.args.len()
        );
    }
    let badge = style_badge("Saved", ANSI_GREEN, color);
    format!(
        "{badge} preset '{}' with {} argument(s)\n",
        style_bold(preset.name.as_str(), color),
        preset.args.len()
    )
}

fn render_presets_list(presets: &[PresetSummary], is_tty: bool, color: bool) -> String {
    if !is_tty {
        let mut out = String::new();
        for entry in presets {
            let marker = if entry.is_default { "*" } else { " " };
            let kind = if entry.preset.is_builtin {
                "built-in"
            } else {
                "custom"
            };
            let _ = writeln!(
                out,
                "{marker} {}\t[{kind}]\t{}",
                entry.preset.name,
                entry.preset.args.join(" ")
            );
        }
        return out;
    }

    let headers = [" ", "NAME", "KIND", "ARGS"];
    let rows: Vec<[String; 4]> = presets
        .iter()
        .map(|entry| {
            let marker = if entry.is_default { "*" } else { " " };
            let kind = if entry.preset.is_builtin {
                "built-in"
            } else {
                "custom"
            };
            [
                marker.to_owned(),
                entry.preset.name.as_str().to_owned(),
                format!("[{kind}]"),
                entry.preset.args.join(" "),
            ]
        })
        .collect();

    let mut out = String::new();
    let widths = column_widths(&headers, &rows);
    // Header line
    let mut header_line = String::new();
    for (idx, header) in headers.iter().enumerate() {
        if idx > 0 {
            header_line.push_str("  ");
        }
        if idx + 1 == headers.len() {
            header_line.push_str(header);
        } else {
            header_line.push_str(&pad_right(header, widths[idx]));
        }
    }
    let _ = writeln!(out, "{}", style_bold(&header_line, color));

    for (row_idx, row) in rows.iter().enumerate() {
        let is_default = presets[row_idx].is_default;
        let marker_padded = pad_right(&row[0], widths[0]);
        let name_padded = pad_right(&row[1], widths[1]);
        let kind_padded = pad_right(&row[2], widths[2]);
        let marker_styled = if is_default {
            style_colored(&marker_padded, ANSI_GREEN, color)
        } else {
            marker_padded
        };
        let name_styled = if is_default {
            style_colored(&style_bold(&name_padded, color), ANSI_GREEN, color)
        } else {
            style_bold(&name_padded, color)
        };
        let kind_styled = style_dim(&kind_padded, color);
        let _ = writeln!(
            out,
            "{marker_styled}  {name_styled}  {kind_styled}  {}",
            row[3]
        );
    }
    out
}

fn render_preset_removed(
    outcome: &crate::storage::RemovePresetOutcome,
    is_tty: bool,
    color: bool,
) -> String {
    let mut msg = format!("Removed preset '{}'", outcome.removed);
    if outcome.reset_default_to_builtin {
        msg.push_str("; default preset reset to 'video'");
    }
    if !is_tty {
        msg.push('\n');
        return msg;
    }
    let badge = style_badge("Removed", ANSI_YELLOW, color);
    let mut out = format!(
        "{badge} preset '{}'",
        style_bold(outcome.removed.as_str(), color)
    );
    if outcome.reset_default_to_builtin {
        out.push_str("; default preset reset to 'video'");
    }
    out.push('\n');
    out
}

fn render_preset_default(preset: &DownloadPreset, is_tty: bool, color: bool) -> String {
    if !is_tty {
        return format!("Default preset set to '{}'\n", preset.name);
    }
    let badge = style_badge("Default", ANSI_GREEN, color);
    format!(
        "{badge} preset set to '{}'\n",
        style_bold(preset.name.as_str(), color)
    )
}

fn describe_job_target(target: &JobTarget) -> String {
    match target {
        JobTarget::Refresh { creator_id: None } => "all followed creators".to_owned(),
        JobTarget::Refresh {
            creator_id: Some(id),
        } => format!("creator {id}"),
        JobTarget::UploadDownload { upload_id, url } => match url {
            Some(display_url) => format!("upload {upload_id} ({display_url})"),
            None => format!("upload {upload_id}"),
        },
        JobTarget::DirectDownload { url } => url.as_str().to_owned(),
    }
}

fn render_job_started(job: &JobRecord, is_tty: bool, color: bool) -> String {
    let target_desc = describe_job_target(&job.target);
    let kind = job.target.kind();
    if !is_tty {
        return format!("Started background {kind} job {} ({target_desc})\n", job.id);
    }
    let badge = style_badge("Started", ANSI_GREEN, color);
    format!(
        "{badge} background {kind} job {} ({target_desc})\n",
        style_bold(&job.id.to_string(), color)
    )
}

fn format_job_progress_line(progress: &JobProgress) -> String {
    match progress {
        JobProgress::Refresh(refresh) => {
            let total_str = refresh
                .total_creators
                .map_or_else(|| "?".to_owned(), |n| n.to_string());
            let mut summary = format!(
                "{}/{} creators processed ({} succeeded, {} failed)",
                refresh.processed_creators,
                total_str,
                refresh.succeeded_creators,
                refresh.failed_creators
            );
            if let Some(cid) = refresh.current_creator_id {
                match &refresh.current_creator_name {
                    Some(name) => {
                        let _ = write!(summary, "; current: creator {cid} ({name})");
                    }
                    None => {
                        let _ = write!(summary, "; current: creator {cid}");
                    }
                }
            }
            summary
        }
        JobProgress::Download(download) => {
            let mut summary = format!("stage={}", download.stage);
            match (download.downloaded_bytes, download.total_bytes) {
                (Some(dl), Some(total)) => {
                    let _ = write!(summary, ", downloaded={dl} B, total={total} B");
                }
                (Some(dl), None) => {
                    let _ = write!(summary, ", downloaded={dl} B, total=unknown");
                }
                (None, Some(total)) => {
                    let _ = write!(summary, ", total={total} B");
                }
                (None, None) => {}
            }
            summary
        }
    }
}

fn render_job_status(job: &JobRecord, is_tty: bool, color: bool) -> String {
    let mut out = String::new();
    let kind = job.target.kind();
    let target_desc = describe_job_target(&job.target);

    if is_tty {
        let (state_label, state_color) = match job.state {
            JobState::Running => ("RUNNING", ANSI_CYAN),
            JobState::Succeeded => ("SUCCEEDED", ANSI_GREEN),
            JobState::Failed => ("FAILED", ANSI_RED),
            JobState::Interrupted => ("INTERRUPTED", ANSI_YELLOW),
        };
        let badge = style_badge(state_label, state_color, color);
        let _ = writeln!(
            out,
            "{} {badge}",
            style_bold(&format!("Job {}", job.id), color)
        );
    } else {
        let _ = writeln!(out, "Job {}", job.id);
    }

    let _ = writeln!(out, "kind: {kind}");
    let _ = writeln!(out, "target: {target_desc}");
    let _ = writeln!(out, "state: {}", job.state);
    let _ = writeln!(out, "pid: {}", job.owner.pid);
    if is_tty {
        let _ = writeln!(
            out,
            "started_at: {}",
            style_dim(&job.started_at.format_utc_iso(), color)
        );
        let _ = writeln!(
            out,
            "updated_at: {}",
            style_dim(&job.updated_at.format_utc_iso(), color)
        );
        if let Some(finished_at) = job.finished_at {
            let _ = writeln!(
                out,
                "finished_at: {}",
                style_dim(&finished_at.format_utc_iso(), color)
            );
        }
    } else {
        let _ = writeln!(out, "started_at: {}", job.started_at.format_utc_iso());
        let _ = writeln!(out, "updated_at: {}", job.updated_at.format_utc_iso());
        if let Some(finished_at) = job.finished_at {
            let _ = writeln!(out, "finished_at: {}", finished_at.format_utc_iso());
        }
    }

    if let Some(progress) = &job.progress {
        let _ = writeln!(out, "progress: {}", format_job_progress_line(progress));
    }

    if let Some(outcome) = &job.outcome {
        match outcome {
            JobOutcome::Refresh(refresh) => {
                let succeeded = refresh
                    .creator_outcomes
                    .iter()
                    .filter(|item| matches!(item, CreatorJobOutcome::Succeeded { .. }))
                    .count();
                let failed = refresh.creator_outcomes.len() - succeeded;
                let _ = write!(
                    out,
                    "outcome: {succeeded} succeeded, {failed} failed ({} total)",
                    refresh.creator_outcomes.len()
                );
                if let Some(err) = &refresh.error {
                    let _ = write!(out, "; error: {err}");
                }
                out.push('\n');
                for item in &refresh.creator_outcomes {
                    match item {
                        CreatorJobOutcome::Succeeded {
                            creator_id,
                            display_name,
                            new_uploads_inserted,
                            existing_uploads_seen,
                            previous_checkpoint,
                            new_checkpoint,
                        } => {
                            let badge = if is_tty {
                                format!("{} ", style_badge("OK  ", ANSI_GREEN, color))
                            } else {
                                String::new()
                            };
                            let _ = writeln!(
                                out,
                                "  {badge}creator {creator_id} ({display_name}): {new_uploads_inserted} new, {existing_uploads_seen} known, checkpoint {previous_checkpoint} -> {new_checkpoint}"
                            );
                        }
                        CreatorJobOutcome::Failed {
                            creator_id,
                            display_name,
                            error,
                        } => {
                            let badge = if is_tty {
                                format!("{} ", style_badge("FAIL", ANSI_RED, color))
                            } else {
                                String::new()
                            };
                            let _ = writeln!(
                                out,
                                "  {badge}creator {creator_id} ({display_name}): failed: {error}"
                            );
                        }
                    }
                }
            }
            JobOutcome::Download(download) => {
                if let Some(err) = &download.error {
                    let _ = writeln!(out, "outcome: failed: {err}");
                } else {
                    let _ = writeln!(
                        out,
                        "outcome: succeeded ({} artifact(s), marked_downloaded={})",
                        download.confirmed_artifacts.len(),
                        download.marked_downloaded
                    );
                }
                for artifact in &download.confirmed_artifacts {
                    let _ = writeln!(out, "  artifact: {}", artifact.display());
                }
            }
            JobOutcome::Interrupted(interrupted) => {
                let _ = writeln!(out, "outcome: interrupted ({})", interrupted.reason);
            }
        }
    }

    out
}

fn render_aligned_table<const N: usize, F>(
    headers: &[&str; N],
    rows: &[[String; N]],
    color: bool,
    style_cell: F,
) -> String
where
    F: Fn(usize, &str, &str) -> String,
{
    let widths = column_widths(headers, rows);
    let mut out = String::new();

    let mut header_line = String::new();
    for (idx, header) in headers.iter().enumerate() {
        if idx > 0 {
            header_line.push_str("  ");
        }
        if idx + 1 == N {
            header_line.push_str(header);
        } else {
            header_line.push_str(&pad_right(header, widths[idx]));
        }
    }
    let _ = writeln!(out, "{}", style_bold(&header_line, color));

    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            if idx > 0 {
                out.push_str("  ");
            }
            if idx + 1 == N {
                out.push_str(&style_cell(idx, cell, cell));
            } else {
                let padded = pad_right(cell, widths[idx]);
                out.push_str(&style_cell(idx, cell, &padded));
            }
        }
        out.push('\n');
    }
    out
}

fn column_widths<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) -> [usize; N] {
    let mut widths = [0usize; N];
    for (idx, header) in headers.iter().enumerate() {
        widths[idx] = display_width(header);
    }
    for row in rows {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(display_width(cell));
        }
    }
    widths
}

fn pad_right(text: &str, target_width: usize) -> String {
    let width = display_width(text);
    if width >= target_width {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + (target_width - width));
    out.push_str(text);
    for _ in 0..(target_width - width) {
        out.push(' ');
    }
    out
}

fn display_width(text: &str) -> usize {
    text.chars().map(char_display_width).sum()
}

fn char_display_width(ch: char) -> usize {
    if ch.is_control() {
        return 0;
    }
    let cp = ch as u32;
    // Zero-width combining diacritical marks and variation selectors.
    if matches!(
        cp,
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
    ) {
        return 0;
    }
    // Common East Asian Wide / Fullwidth blocks (CJK, Hangul, Hiragana/Katakana, Fullwidth).
    if matches!(
        cp,
        0x1100..=0x115F
            | 0x2329..=0x232A
            | 0x2E80..=0x303E
            | 0x3040..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
            | 0x20000..=0x2FFFD
            | 0x30000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

fn style_badge(text: &str, ansi_fg: &str, color: bool) -> String {
    if color {
        format!("{ANSI_BOLD}{ansi_fg}{text}{ANSI_RESET}")
    } else {
        text.to_owned()
    }
}

fn style_colored(text: &str, ansi_fg: &str, color: bool) -> String {
    if color {
        format!("{ansi_fg}{text}{ANSI_RESET}")
    } else {
        text.to_owned()
    }
}

fn style_bold(text: &str, color: bool) -> String {
    if color {
        format!("{ANSI_BOLD}{text}{ANSI_RESET}")
    } else {
        text.to_owned()
    }
}

fn style_dim(text: &str, color: bool) -> String {
    if color {
        format!("{ANSI_DIM}{text}{ANSI_RESET}")
    } else {
        text.to_owned()
    }
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn extract_clap_error_message(raw: &str) -> String {
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("error:") {
            let msg = rest.trim();
            if !msg.is_empty() {
                return msg.to_owned();
            }
        }
    }
    raw.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("invalid command-line arguments")
        .to_owned()
}

fn extract_usage_line(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Usage:") {
            let usage = rest.trim();
            if !usage.is_empty() {
                return Some(usage.to_owned());
            }
        }
    }
    None
}

fn clap_kind_slug(kind: ClapErrorKind) -> &'static str {
    match kind {
        ClapErrorKind::InvalidValue => "invalid_value",
        ClapErrorKind::UnknownArgument => "unknown_argument",
        ClapErrorKind::InvalidSubcommand => "invalid_subcommand",
        ClapErrorKind::NoEquals => "no_equals",
        ClapErrorKind::ValueValidation => "value_validation",
        ClapErrorKind::TooManyValues => "too_many_values",
        ClapErrorKind::TooFewValues => "too_few_values",
        ClapErrorKind::WrongNumberOfValues => "wrong_number_of_values",
        ClapErrorKind::ArgumentConflict => "argument_conflict",
        ClapErrorKind::MissingRequiredArgument => "missing_required_argument",
        ClapErrorKind::MissingSubcommand => "missing_subcommand",
        ClapErrorKind::InvalidUtf8 => "invalid_utf8",
        ClapErrorKind::DisplayHelp => "display_help",
        ClapErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => "missing_argument_or_subcommand",
        ClapErrorKind::DisplayVersion => "display_version",
        ClapErrorKind::Io => "io",
        ClapErrorKind::Format => "format",
        _ => "invalid_arguments",
    }
}

/// Formats a [`Timestamp`] using UTC ISO 8601 when serializing domain outputs.
#[must_use]
pub fn format_timestamp_iso(ts: Timestamp) -> String {
    ts.format_utc_iso()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::domain::{CreatorId, Follow, FollowId, Platform, PresetName, UploadId};
    use crate::error::CreatorRefreshFailure;
    use crate::storage::CommitRefreshOutcome;
    use clap::{CommandFactory, Parser};

    #[test]
    fn color_detection_respects_tty_no_color_and_dumb_term() {
        assert!(!should_use_color_from_env(false, |_| None));
        assert!(should_use_color_from_env(true, |_| None));
        assert!(!should_use_color_from_env(true, |k| {
            (k == "NO_COLOR").then(|| "1".to_owned())
        }));
        assert!(!should_use_color_from_env(true, |k| {
            (k == "NO_COLOR").then(|| "0".to_owned())
        }));
        assert!(should_use_color_from_env(true, |k| {
            (k == "NO_COLOR").then(String::new)
        }));
        assert!(!should_use_color_from_env(true, |k| {
            (k == "TERM").then(|| "dumb".to_owned())
        }));
    }

    #[test]
    fn human_tty_lists_align_columns_with_modest_color_and_preserve_plain_tsv() {
        let creators = vec![
            FollowedCreator {
                creator: Creator {
                    id: CreatorId::new(1).unwrap(),
                    platform: Platform::Bilibili,
                    platform_creator_id: "946974".to_owned(),
                    display_name: "影视飓风".to_owned(),
                },
                follow: Follow {
                    id: FollowId::new(1).unwrap(),
                    creator_id: CreatorId::new(1).unwrap(),
                    followed_at: Timestamp::from_unix_seconds(1_700_000_000),
                    refresh_checkpoint: Timestamp::from_unix_seconds(1_700_000_100),
                },
            },
            FollowedCreator {
                creator: Creator {
                    id: CreatorId::new(12).unwrap(),
                    platform: Platform::Bilibili,
                    platform_creator_id: "888001".to_owned(),
                    display_name: "Alpha".to_owned(),
                },
                follow: Follow {
                    id: FollowId::new(2).unwrap(),
                    creator_id: CreatorId::new(12).unwrap(),
                    followed_at: Timestamp::from_unix_seconds(1_700_000_000),
                    refresh_checkpoint: Timestamp::from_unix_seconds(1_700_000_200),
                },
            },
        ];

        let result = CommandResult::CreatorsListed(creators);

        let plain = result.render_human(HumanRenderOptions::plain());
        assert!(
            plain
                .stdout
                .contains("1\tbilibili\t946974\t影视飓风\tfollowed=")
        );
        assert!(!plain.stdout.contains("\x1b["));

        let tty_no_color = result.render_human(HumanRenderOptions::tty(false));
        assert!(!tty_no_color.stdout.contains("\x1b["));
        assert!(!tty_no_color.stdout.contains('\t'));
        let lines: Vec<&str> = tty_no_color.stdout.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("ID  PLATFORM"));
        // Verify CJK display-width alignment: FOLLOWED column starts at the exact same column on both rows.
        let col_row1 = display_width(lines[1].split("2023-").next().unwrap());
        let col_row2 = display_width(lines[2].split("2023-").next().unwrap());
        assert_eq!(col_row1, col_row2);

        let tty_color = result.render_human(HumanRenderOptions::tty(true));
        assert!(tty_color.stdout.contains("\x1b["));
    }

    #[test]
    fn partial_refresh_populates_all_creator_outcomes_in_json_and_human_output() {
        let summary = RefreshSummary {
            outcomes: vec![
                CreatorRefreshOutcome::Succeeded(CommitRefreshOutcome {
                    creator_id: CreatorId::new(2).unwrap(),
                    display_name: "Creator Beta".to_owned(),
                    new_uploads_inserted: 3,
                    existing_uploads_seen: 1,
                    previous_checkpoint: Timestamp::from_unix_seconds(1_700_000_000),
                    new_checkpoint: Timestamp::from_unix_seconds(1_700_000_500),
                }),
                CreatorRefreshOutcome::Failed {
                    creator: FollowedCreator {
                        creator: Creator {
                            id: CreatorId::new(1).unwrap(),
                            platform: Platform::Bilibili,
                            platform_creator_id: "946974".to_owned(),
                            display_name: "Creator Alpha".to_owned(),
                        },
                        follow: Follow {
                            id: FollowId::new(1).unwrap(),
                            creator_id: CreatorId::new(1).unwrap(),
                            followed_at: Timestamp::from_unix_seconds(1_700_000_000),
                            refresh_checkpoint: Timestamp::from_unix_seconds(1_700_000_000),
                        },
                    },
                    error: RefreshError::Network("simulated 502".to_owned()),
                },
            ],
            used_cookies: None,
            cleared_default_cookies: false,
            saved_default_cookies: false,
        };

        let err = AppError::RefreshPartialFailure {
            succeeded: 1,
            failures: vec![CreatorRefreshFailure {
                creator_id: CreatorId::new(1).unwrap(),
                display_name: "Creator Alpha".to_owned(),
                error: "network error while refreshing: simulated 502".to_owned(),
            }],
        };

        let json_err = JsonError::from_app_error(&err, Some(&summary));
        assert_eq!(json_err.code, "refresh_partial_failure");
        let details = json_err.details.expect("partial failure details");
        assert_eq!(details["succeeded"], 1);
        assert_eq!(details["failed"], 1);
        assert_eq!(details["total"], 2);
        let outcomes = details["outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0]["status"], "succeeded");
        assert_eq!(outcomes[0]["creator_id"], 2);
        assert_eq!(outcomes[0]["new_uploads_inserted"], 3);
        assert_eq!(outcomes[1]["status"], "failed");
        assert_eq!(outcomes[1]["creator_id"], 1);
        assert_eq!(outcomes[1]["error"]["code"], "refresh_network_error");

        let human = CommandResult::Refreshed(summary).render_human(HumanRenderOptions::plain());
        assert!(
            human
                .stdout
                .contains("Refreshed creator 2 (Creator Beta): 3 new upload(s), 1 known")
        );
        assert!(human.stdout.contains("Failed creator 1 (Creator Alpha)"));
    }

    #[test]
    fn clap_help_and_errors_serialize_as_structured_json_without_ansi() {
        let help = HelpData::from_clap_command(&["siphon".to_owned()], &Cli::command());
        assert_eq!(help.command, "siphon");
        assert!(help.help_text.contains("Usage: siphon"));
        assert!(help.subcommands.iter().any(|s| s.name == "download"));
        assert!(
            help.options
                .iter()
                .any(|o| o.long.as_deref() == Some("--json"))
        );

        // `siphon --json --help creator list` must match `siphon --help creator list` (root help).
        let json_root_help_err =
            Cli::try_parse_from(["siphon", "--json", "--help", "creator", "list"]).unwrap_err();
        let plain_root_help_err =
            Cli::try_parse_from(["siphon", "--help", "creator", "list"]).unwrap_err();
        let json_root_help = HelpData::from_clap_help_error(&json_root_help_err);
        assert_eq!(json_root_help.command, "siphon");
        assert_eq!(
            json_root_help.help_text,
            strip_ansi(&plain_root_help_err.to_string())
        );

        // Nested help with `--json` before or after the subcommand resolves `siphon creator list`.
        let nested_before =
            Cli::try_parse_from(["siphon", "--json", "creator", "list", "--help"]).unwrap_err();
        let nested_after =
            Cli::try_parse_from(["siphon", "creator", "list", "--json", "--help"]).unwrap_err();
        let nested_plain =
            Cli::try_parse_from(["siphon", "creator", "list", "--help"]).unwrap_err();
        let nested_before_data = HelpData::from_clap_help_error(&nested_before);
        let nested_after_data = HelpData::from_clap_help_error(&nested_after);
        assert_eq!(nested_before_data.command, "siphon creator list");
        assert_eq!(nested_after_data.command, "siphon creator list");
        assert_eq!(
            nested_before_data.help_text,
            strip_ansi(&nested_plain.to_string())
        );
        assert_eq!(
            nested_after_data.help_text,
            strip_ansi(&nested_plain.to_string())
        );

        let missing_sub = Cli::try_parse_from(["siphon", "--json"]).unwrap_err();
        let json_missing = JsonError::from_clap_error(&missing_sub);
        assert_eq!(json_missing.code, "invalid_arguments");
        assert!(!json_missing.message.contains("\x1b["));

        let bad_id =
            Cli::try_parse_from(["siphon", "--json", "creator", "remove", "0"]).unwrap_err();
        let json_bad_id = JsonError::from_clap_error(&bad_id);
        assert!(!json_bad_id.message.contains("\x1b["));
        assert!(
            json_bad_id.code == "invalid_creator_id" || json_bad_id.code == "invalid_arguments"
        );

        let upload = Upload {
            id: UploadId::new(3).unwrap(),
            creator_id: CreatorId::new(1).unwrap(),
            platform: Platform::Bilibili,
            platform_upload_id: "BV1x".to_owned(),
            url: "https://www.bilibili.com/video/BV1x".to_owned(),
            title: "Test".to_owned(),
            published_at: Timestamp::from_unix_seconds(1_700_000_000),
            status: UploadStatus::Pending,
        };
        let preset = PresetSummary {
            preset: DownloadPreset {
                name: PresetName::builtin_video(),
                args: vec!["-f".to_owned(), "bv*+ba/b".to_owned()],
                is_builtin: true,
            },
            is_default: true,
        };
        let presets_tty =
            CommandResult::PresetsListed(vec![preset]).render_human(HumanRenderOptions::tty(false));
        assert!(presets_tty.stdout.contains("*  video"));
        assert!(!presets_tty.stdout.contains('\t'));

        let uploads_tty =
            CommandResult::UploadsListed(vec![upload]).render_human(HumanRenderOptions::tty(false));
        assert!(uploads_tty.stdout.contains("ID  CREATOR  PUBLISHED"));
        assert!(!uploads_tty.stdout.contains('\t'));

        // Job storage and validation errors map to stable snake_case codes with structured details.
        let job_not_found = AppError::Storage(StorageError::JobNotFound {
            job_id: crate::jobs::JobId::new(42).unwrap(),
        });
        let job_err_json = JsonError::from_app_error(&job_not_found, None);
        assert_eq!(job_err_json.code, "job_not_found");
        assert_eq!(job_err_json.details.unwrap()["job_id"], 42);

        let busy = AppError::Storage(StorageError::OperationBusy {
            active: Box::new(crate::jobs::ActiveOperation {
                slot: crate::jobs::OperationSlot::Refresh,
                execution_mode: crate::jobs::ExecutionMode::Background,
                job_id: Some(crate::jobs::JobId::new(9).unwrap()),
                owner_token: crate::jobs::OwnershipToken::new(3).unwrap(),
                owner: crate::jobs::ProcessOwner::new(1234, None).unwrap(),
                acquired_at: Timestamp::from_unix_seconds(1_700_000_000),
            }),
        });
        let busy_json = JsonError::from_app_error(&busy, None);
        assert_eq!(busy_json.code, "operation_busy");
        let busy_details = busy_json.details.unwrap();
        assert_eq!(busy_details["slot"], "refresh");
        assert_eq!(busy_details["job_id"], 9);
        assert_eq!(busy_details["pid"], 1234);

        let spawn_err = AppError::Background(BackgroundError::WorkerSpawnFailed {
            binary: "/bin/missing".to_owned(),
            reason: "not found".to_owned(),
        });
        let spawn_json = JsonError::from_app_error(&spawn_err, None);
        assert_eq!(spawn_json.code, "background_worker_spawn_failed");
        assert_eq!(spawn_json.details.unwrap()["binary"], "/bin/missing");

        let help = HelpData::from_clap_command(&["siphon".to_owned()], &Cli::command());
        assert!(help.subcommands.iter().any(|s| s.name == "job"));
        assert!(!help.subcommands.iter().any(|s| s.name == "__worker"));
    }

    #[test]
    fn job_started_and_job_status_render_json_and_human_outputs() {
        use crate::jobs::{
            DisplayUrl, DownloadJobOutcome, DownloadProgress, DownloadStage, InterruptedJobOutcome,
            JobId, OwnershipToken, ProcessOwner, RefreshJobOutcome,
        };

        let t0 = Timestamp::from_unix_seconds(1_760_000_000);
        let t1 = Timestamp::from_unix_seconds(1_760_000_015);
        let cid1 = CreatorId::new(1).unwrap();
        let cid2 = CreatorId::new(2).unwrap();

        let running_dl = JobRecord {
            id: JobId::new(7).unwrap(),
            target: JobTarget::DirectDownload {
                url: DisplayUrl::sanitize("https://u:p@example.com/watch?token=abc#frag").unwrap(),
            },
            state: JobState::Running,
            started_at: t0,
            updated_at: t1,
            finished_at: None,
            progress: Some(JobProgress::Download(DownloadProgress {
                stage: DownloadStage::Downloading,
                downloaded_bytes: Some(512),
                total_bytes: None,
            })),
            outcome: None,
            owner_token: OwnershipToken::new(3).unwrap(),
            owner: ProcessOwner::new(4321, Some("marker-1".to_owned())).unwrap(),
        };

        let started_json: serde_json::Value = serde_json::from_str(
            &CommandResult::JobStarted(running_dl.clone())
                .to_json_line()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(started_json["ok"], true);
        assert_eq!(started_json["data"]["id"], 7);
        assert_eq!(started_json["data"]["state"], "running");
        assert_eq!(
            started_json["data"]["target"]["url"],
            "https://example.com/watch"
        );
        assert_eq!(started_json["data"]["progress"]["stage"], "downloading");
        assert_eq!(started_json["data"]["progress"]["downloaded_bytes"], 512);
        assert!(started_json["data"]["progress"]["total_bytes"].is_null());

        let started_human =
            CommandResult::JobStarted(running_dl.clone()).render_human(HumanRenderOptions::plain());
        assert!(
            started_human
                .stdout
                .contains("Started background direct_download job 7")
        );
        assert!(started_human.stdout.contains("https://example.com/watch"));

        let status_running_human =
            CommandResult::JobStatus(running_dl).render_human(HumanRenderOptions::plain());
        assert!(status_running_human.stdout.contains("Job 7"));
        assert!(status_running_human.stdout.contains("state: running"));
        assert!(
            status_running_human
                .stdout
                .contains("downloaded=512 B, total=unknown")
        );

        let mixed_refresh = JobRecord {
            id: JobId::new(8).unwrap(),
            target: JobTarget::Refresh { creator_id: None },
            state: JobState::Failed,
            started_at: t0,
            updated_at: t1,
            finished_at: Some(t1),
            progress: Some(JobProgress::Refresh(crate::jobs::RefreshProgress {
                total_creators: Some(2),
                processed_creators: 2,
                succeeded_creators: 1,
                failed_creators: 1,
                current_creator_id: None,
                current_creator_name: None,
            })),
            outcome: Some(JobOutcome::Refresh(RefreshJobOutcome {
                creator_outcomes: vec![
                    CreatorJobOutcome::Succeeded {
                        creator_id: cid1,
                        display_name: "Alice".to_owned(),
                        new_uploads_inserted: 2,
                        existing_uploads_seen: 1,
                        previous_checkpoint: t0,
                        new_checkpoint: t1,
                    },
                    CreatorJobOutcome::Failed {
                        creator_id: cid2,
                        display_name: "Bob".to_owned(),
                        error: "network timeout".to_owned(),
                    },
                ],
                error: Some("1 of 2 creator(s) failed".to_owned()),
            })),
            owner_token: OwnershipToken::new(4).unwrap(),
            owner: ProcessOwner::new(4322, None).unwrap(),
        };

        let refresh_json: serde_json::Value = serde_json::from_str(
            &CommandResult::JobStatus(mixed_refresh.clone())
                .to_json_line()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(refresh_json["ok"], true);
        assert_eq!(refresh_json["data"]["state"], "failed");
        assert_eq!(refresh_json["data"]["outcome"]["kind"], "refresh");
        assert_eq!(refresh_json["data"]["outcome"]["succeeded"], 1);
        assert_eq!(refresh_json["data"]["outcome"]["failed"], 1);
        assert_eq!(
            refresh_json["data"]["outcome"]["creator_outcomes"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let refresh_tty =
            CommandResult::JobStatus(mixed_refresh).render_human(HumanRenderOptions::tty(true));
        assert!(refresh_tty.stdout.contains("FAILED"));
        assert!(refresh_tty.stdout.contains("Alice"));
        assert!(refresh_tty.stdout.contains("Bob"));
        assert!(refresh_tty.stdout.contains("network timeout"));

        let interrupted_job = JobRecord {
            id: JobId::new(9).unwrap(),
            target: JobTarget::UploadDownload {
                upload_id: UploadId::new(5).unwrap(),
                url: Some(DisplayUrl::sanitize("https://example.com/video/BV5").unwrap()),
            },
            state: JobState::Interrupted,
            started_at: t0,
            updated_at: t1,
            finished_at: Some(t1),
            progress: None,
            outcome: Some(JobOutcome::Interrupted(InterruptedJobOutcome {
                reason: "process 999 is no longer running".to_owned(),
            })),
            owner_token: OwnershipToken::new(5).unwrap(),
            owner: ProcessOwner::new(999, None).unwrap(),
        };
        let interrupted_human = CommandResult::JobStatus(interrupted_job.clone())
            .render_human(HumanRenderOptions::plain());
        assert!(interrupted_human.stdout.contains("state: interrupted"));
        assert!(
            interrupted_human
                .stdout
                .contains("process 999 is no longer running")
        );

        let failed_dl = JobRecord {
            id: JobId::new(10).unwrap(),
            target: JobTarget::UploadDownload {
                upload_id: UploadId::new(6).unwrap(),
                url: None,
            },
            state: JobState::Succeeded,
            started_at: t0,
            updated_at: t1,
            finished_at: Some(t1),
            progress: Some(JobProgress::Download(DownloadProgress {
                stage: DownloadStage::Verifying,
                downloaded_bytes: Some(1024),
                total_bytes: Some(1024),
            })),
            outcome: Some(JobOutcome::Download(DownloadJobOutcome {
                confirmed_artifacts: vec![std::path::PathBuf::from("video.mp4")],
                marked_downloaded: true,
                error: None,
            })),
            owner_token: OwnershipToken::new(6).unwrap(),
            owner: ProcessOwner::new(1000, None).unwrap(),
        };
        let dl_human =
            CommandResult::JobStatus(failed_dl).render_human(HumanRenderOptions::plain());
        assert!(dl_human.stdout.contains("downloaded=1024 B, total=1024 B"));
        assert!(dl_human.stdout.contains("artifact: video.mp4"));
    }
}
