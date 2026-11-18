//! Structured error types across Siphon validation, storage, refresh, and download layers.

use thiserror::Error;

use crate::domain::{CreatorId, PresetName, UploadId, UploadStatus};

/// Input and domain value validation failures.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// Creator ID could not be parsed as a positive integer.
    #[error("invalid creator ID '{input}': expected a positive integer")]
    InvalidCreatorId {
        /// Raw user-provided value.
        input: String,
    },

    /// Upload ID could not be parsed as a positive integer.
    #[error("invalid upload ID '{input}': expected a positive integer")]
    InvalidUploadId {
        /// Raw user-provided value.
        input: String,
    },

    /// Follow ID could not be parsed as a positive integer.
    #[error("invalid follow ID '{input}': expected a positive integer")]
    InvalidFollowId {
        /// Raw user-provided value.
        input: String,
    },

    /// Preset name failed syntax rules.
    #[error("invalid preset name '{input}': {reason}")]
    InvalidPresetName {
        /// Raw user-provided value.
        input: String,
        /// Explanation of the validation failure.
        reason: &'static str,
    },

    /// User attempted to create or mutate a preset using the reserved built-in name.
    #[error("preset name '{name}' is reserved for the built-in preset")]
    ReservedPresetName {
        /// Reserved preset name.
        name: String,
    },

    /// Required string input was empty or whitespace-only.
    #[error("{field} cannot be empty")]
    EmptyValue {
        /// Name of the empty field.
        field: &'static str,
    },

    /// Cookie file path was invalid.
    #[error("invalid cookie file path: {reason}")]
    InvalidCookiePath {
        /// Explanation of the invalid path.
        reason: &'static str,
    },

    /// Database path was empty.
    #[error("database path cannot be empty")]
    EmptyDatabasePath,

    /// Unknown platform identifier.
    #[error("unsupported platform '{platform}'")]
    UnsupportedPlatform {
        /// Raw platform name.
        platform: String,
    },

    /// Unknown upload status identifier.
    #[error("invalid upload status '{status}'")]
    InvalidUploadStatus {
        /// Raw status string.
        status: String,
    },
}

/// Errors produced by the local persistence layer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StorageError {
    /// Storage backend has not been wired/implemented yet.
    #[error("storage backend is not implemented yet (SQLite storage task pending)")]
    NotImplemented,

    /// Referenced creator does not exist in storage.
    #[error("creator {creator_id} not found")]
    CreatorNotFound {
        /// Missing creator ID.
        creator_id: CreatorId,
    },

    /// Creator is already actively followed.
    #[error("creator {creator_id} ({display_name}) is already followed")]
    CreatorAlreadyFollowed {
        /// Existing local creator ID.
        creator_id: CreatorId,
        /// Creator display name.
        display_name: String,
    },

    /// Creator exists in storage history but does not have an active follow.
    #[error("creator {creator_id} is not currently followed")]
    CreatorNotFollowed {
        /// Unfollowed creator ID.
        creator_id: CreatorId,
    },

    /// Referenced upload does not exist in storage.
    #[error("upload {upload_id} not found")]
    UploadNotFound {
        /// Missing upload ID.
        upload_id: UploadId,
    },

    /// Operation requires an upload in `pending` status, but its current status differs.
    #[error("upload {upload_id} is {status}, expected pending")]
    UploadNotPending {
        /// Target upload ID.
        upload_id: UploadId,
        /// Actual current status.
        status: UploadStatus,
    },

    /// Referenced download preset does not exist.
    #[error("download preset '{name}' not found")]
    PresetNotFound {
        /// Missing preset name.
        name: PresetName,
    },

    /// Preset name already exists in storage.
    #[error("download preset '{name}' already exists")]
    PresetAlreadyExists {
        /// Duplicate preset name.
        name: PresetName,
    },

    /// Built-in preset cannot be overwritten or removed.
    #[error("built-in preset '{name}' cannot be added, modified, or removed")]
    CannotModifyBuiltinPreset {
        /// Built-in preset name.
        name: PresetName,
    },

    /// Refresh commit rejected because the active follow or checkpoint changed during fetch.
    #[error(
        "refresh aborted for creator {creator_id}: active follow or checkpoint changed while fetching"
    )]
    StaleRefreshCheckpoint {
        /// Creator whose checkpoint changed concurrently.
        creator_id: CreatorId,
    },

    /// Stored data violated expected schema invariants.
    #[error("corrupted storage state: {0}")]
    CorruptedData(String),

    /// Underlying database or I/O error.
    #[error("storage backend error: {0}")]
    Backend(String),
}

/// Errors produced when resolving creators or fetching uploads from a platform.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RefreshError {
    /// Platform refresh source has not been wired/implemented yet.
    #[error("refresh backend is not implemented yet (Bilibili refresh task pending)")]
    NotImplemented,

    /// Creator URL could not be parsed as a valid creator profile URL.
    #[error("invalid creator URL '{url}': {reason}")]
    InvalidCreatorUrl {
        /// User-provided URL.
        url: String,
        /// Specific parsing failure.
        reason: String,
    },

    /// Creator could not be located on the remote platform.
    #[error("creator not found for URL '{url}'")]
    CreatorNotFoundRemote {
        /// Requested creator URL.
        url: String,
    },

    /// Selected cookie file or browser profile could not be read or parsed.
    #[error("cookie error for {cookie_source}: {reason}")]
    CookieSourceInvalid {
        /// Non-secret description of the cookie source.
        cookie_source: String,
        /// Cause of failure.
        reason: String,
    },

    /// HTTP or transport failure while communicating with the platform.
    #[error("network error while refreshing: {0}")]
    Network(String),

    /// Platform returned an unexpected status or malformed payload.
    #[error("platform error while refreshing: {0}")]
    PlatformResponse(String),

    /// Storage error encountered during refresh orchestration.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Errors produced when invoking `yt-dlp` or verifying download artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DownloadError {
    /// Downloader backend has not been wired/implemented yet.
    #[error("download backend is not implemented yet (yt-dlp download task pending)")]
    NotImplemented,

    /// `yt-dlp` executable could not be found or spawned.
    #[error(
        "failed to start downloader '{binary}': {reason}; ensure yt-dlp is installed and on PATH"
    )]
    DownloaderUnavailable {
        /// Executable name or path attempted.
        binary: String,
        /// Underlying OS error description.
        reason: String,
    },

    /// `yt-dlp` exited with a non-zero status or signal termination.
    #[error("{}", format_process_failure(*exit_code, stderr))]
    ProcessFailed {
        /// Process exit code if terminated normally.
        exit_code: Option<i32>,
        /// Captured stderr output (or concise error context).
        stderr: String,
    },

    /// Selected cookie source could not be prepared for `yt-dlp`.
    #[error("cookie error for {cookie_source}: {reason}")]
    CookieSourceInvalid {
        /// Non-secret description of the cookie source.
        cookie_source: String,
        /// Cause of failure.
        reason: String,
    },

    /// Supplied download arguments violate single-target or safety constraints.
    #[error("invalid download arguments: {reason}")]
    InvalidArguments {
        /// Explanation of why the arguments were rejected.
        reason: String,
    },

    /// Unexpected I/O error while preparing or inspecting download artifacts.
    #[error("download I/O error: {0}")]
    Io(String),

    /// Storage error encountered during download orchestration.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

fn format_process_failure(exit_code: Option<i32>, stderr: &str) -> String {
    let trimmed = stderr.trim();
    match (exit_code, trimmed.is_empty()) {
        (Some(code), true) => format!("yt-dlp failed with exit code {code}"),
        (Some(code), false) => format!("yt-dlp failed with exit code {code}: {trimmed}"),
        (None, true) => "yt-dlp terminated by signal".to_owned(),
        (None, false) => format!("yt-dlp terminated by signal: {trimmed}"),
    }
}

/// Summary of an individual creator failure during `siphon refresh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatorRefreshFailure {
    /// Local creator ID.
    pub creator_id: CreatorId,
    /// Creator display name.
    pub display_name: String,
    /// Failure description.
    pub error: String,
}

/// Top-level application error surfaced by CLI command execution.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AppError {
    /// Validation failure on input or domain values.
    #[error(transparent)]
    Validation(#[from] ValidationError),

    /// Persistence failure.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// Refresh source or orchestration failure.
    #[error(transparent)]
    Refresh(#[from] RefreshError),

    /// Downloader or artifact verification failure.
    #[error(transparent)]
    Download(#[from] DownloadError),

    /// One or more creators failed to refresh while others may have succeeded.
    #[error("{}", format_partial_refresh(*succeeded, failures))]
    RefreshPartialFailure {
        /// Number of creators refreshed successfully.
        succeeded: usize,
        /// Per-creator failure details.
        failures: Vec<CreatorRefreshFailure>,
    },
}

fn format_partial_refresh(succeeded: usize, failures: &[CreatorRefreshFailure]) -> String {
    let failed = failures.len();
    let details = failures
        .iter()
        .map(|item| {
            format!(
                "creator {} ({}): {}",
                item.creator_id, item.display_name, item.error
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("refresh completed with {succeeded} succeeded and {failed} failed: {details}")
}
