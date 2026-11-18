//! Isolated background job model, operation occupancy rules, and [`JobStore`] persistence contract.
//!
//! This module models durable job records, exclusive operation reservations shared across
//! foreground and background executions, display-safe target metadata, structured progress and
//! final outcomes, and fencing/reconciliation primitives without coupling to CLI or runner
//! execution.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use url::Url;

use crate::domain::{CreatorId, Timestamp, UploadId};
use crate::error::{StorageError, ValidationError};

macro_rules! define_positive_job_id {
    ($(#[$meta:meta])* $name:ident, $err_variant:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(i64);

        impl $name {
            /// Creates a validated positive identifier.
            pub fn new(id: i64) -> Result<Self, ValidationError> {
                if id <= 0 {
                    return Err(ValidationError::$err_variant {
                        input: id.to_string(),
                    });
                }
                Ok(Self(id))
            }

            /// Returns the underlying integer identifier.
            #[must_use]
            pub const fn as_i64(self) -> i64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = ValidationError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let parsed = s
                    .trim()
                    .parse::<i64>()
                    .map_err(|_| ValidationError::$err_variant {
                        input: s.to_owned(),
                    })?;
                Self::new(parsed).map_err(|_| ValidationError::$err_variant {
                    input: s.to_owned(),
                })
            }
        }
    };
}

define_positive_job_id!(
    /// Durable unique local identifier for a background job stored in Siphon.
    JobId,
    InvalidJobId
);

define_positive_job_id!(
    /// Monotonic fencing token assigned by storage when an ownership claim begins.
    ///
    /// Every mutation or release of a job or reservation must present the currently active token
    /// so stale writers cannot overwrite newer ownership after dead-owner reconciliation.
    OwnershipToken,
    InvalidOwnershipToken
);

/// Opaque OS process identity for the active or last owner of a reservation or job.
///
/// When [`process_started_at`](Self::process_started_at) is `None`, callers must treat process
/// identity across PID reuse as ambiguous unless corroborated by external platform checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOwner {
    /// Positive OS process identifier (`pid > 0`).
    pub pid: u32,
    /// Optional opaque process-start marker (for example, OS boot-relative ticks or start time)
    /// used by cross-platform liveness checks to guard against PID reuse.
    pub process_started_at: Option<String>,
}

impl ProcessOwner {
    /// Validates and constructs a [`ProcessOwner`].
    pub fn new(pid: u32, process_started_at: Option<String>) -> Result<Self, ValidationError> {
        if pid == 0 {
            return Err(ValidationError::InvalidProcessOwner {
                reason: "process ID must be greater than zero".to_owned(),
            });
        }
        let process_started_at = match process_started_at {
            Some(marker) => {
                let trimmed = marker.trim();
                if trimmed.is_empty() {
                    return Err(ValidationError::InvalidProcessOwner {
                        reason: "process start marker cannot be blank when provided".to_owned(),
                    });
                }
                Some(trimmed.to_owned())
            }
            None => None,
        };
        Ok(Self {
            pid,
            process_started_at,
        })
    }

    /// Captures the current OS process ID with an unspecified (`None`) start marker.
    #[must_use]
    pub fn current_pid() -> Self {
        Self {
            pid: std::process::id(),
            process_started_at: None,
        }
    }
}

/// Display-safe URL with embedded credentials, query parameters, and fragments redacted.
///
/// Stored job history accepts only [`DisplayUrl`] values so raw operational URLs containing query
/// tokens or userinfo credentials are never persisted in SQLite.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DisplayUrl(String);

impl DisplayUrl {
    /// Parses `raw` as a hierarchical URL and strips username, password, query, and fragment.
    ///
    /// Callers may pass either a raw operational URL (which is sanitized before construction) or
    /// an already-sanitized URL. Non-URL strings or URLs without a host are rejected.
    pub fn sanitize(raw: &str) -> Result<Self, ValidationError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ValidationError::InvalidDisplayUrl {
                reason: "URL cannot be empty".to_owned(),
            });
        }

        let mut parsed = Url::parse(trimmed).map_err(|err| ValidationError::InvalidDisplayUrl {
            reason: format!("invalid URL: {err}"),
        })?;
        if parsed.cannot_be_a_base() || !parsed.has_host() {
            return Err(ValidationError::InvalidDisplayUrl {
                reason: "URL must include a host".to_owned(),
            });
        }
        if parsed.set_username("").is_err() || parsed.set_password(None).is_err() {
            return Err(ValidationError::InvalidDisplayUrl {
                reason: "failed to redact URL credentials".to_owned(),
            });
        }
        parsed.set_query(None);
        parsed.set_fragment(None);

        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(ValidationError::InvalidDisplayUrl {
                reason: "sanitized URL still contained restricted components".to_owned(),
            });
        }

        Ok(Self(parsed.into()))
    }

    /// Returns the sanitized URL string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Exclusive occupancy slot enforced per database.
///
/// Refresh and tracked upload downloads share these slots across foreground and background
/// executions. Direct URL downloads require no exclusive slot and run independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OperationSlot {
    /// Database-wide exclusive slot allowing at most one active refresh at a time.
    Refresh,
    /// Per-upload exclusive slot allowing at most one active download per tracked upload.
    UploadDownload(UploadId),
}

impl OperationSlot {
    /// Returns the canonical SQLite `slot_key` string for this slot.
    #[must_use]
    pub fn slot_key(self) -> String {
        match self {
            Self::Refresh => "refresh".to_owned(),
            Self::UploadDownload(upload_id) => format!("upload_download:{}", upload_id.as_i64()),
        }
    }
}

impl fmt::Display for OperationSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refresh => f.write_str("refresh"),
            Self::UploadDownload(upload_id) => write!(f, "download for upload {upload_id}"),
        }
    }
}

/// Execution context holding an exclusive [`OperationSlot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionMode {
    /// Foreground CLI command holding occupancy without creating public job history.
    Foreground,
    /// Detached background worker backed by a persistent [`JobRecord`].
    Background,
}

impl ExecutionMode {
    /// Returns the canonical lowercase storage identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Background => "background",
        }
    }
}

impl fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ExecutionMode {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "foreground" => Ok(Self::Foreground),
            "background" => Ok(Self::Background),
            other => Err(ValidationError::InvalidExecutionMode {
                mode: other.to_owned(),
            }),
        }
    }
}

/// Snapshot of the current active owner holding an exclusive [`OperationSlot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveOperation {
    /// Occupied slot.
    pub slot: OperationSlot,
    /// Whether the holder is a foreground command or a background job.
    pub execution_mode: ExecutionMode,
    /// Associated job ID when `execution_mode` is [`ExecutionMode::Background`], or `None` for
    /// foreground reservations.
    pub job_id: Option<JobId>,
    /// Monotonic fencing token for this ownership claim.
    pub owner_token: OwnershipToken,
    /// OS process identity of the current owner.
    pub owner: ProcessOwner,
    /// UTC timestamp when the slot was acquired.
    pub acquired_at: Timestamp,
}

impl fmt::Display for ActiveOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.job_id {
            Some(job_id) => write!(
                f,
                "{} is already running in {} (job {}, pid {})",
                self.slot, self.execution_mode, job_id, self.owner.pid
            ),
            None => write!(
                f,
                "{} is already running in {} (pid {})",
                self.slot, self.execution_mode, self.owner.pid
            ),
        }
    }
}

/// Lease token for an active foreground operation reservation (no public job history row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationReservation {
    /// Reserved exclusive slot.
    pub slot: OperationSlot,
    /// Fencing token required to release the reservation.
    pub owner_token: OwnershipToken,
    /// Process identity recorded on acquisition.
    pub owner: ProcessOwner,
    /// UTC timestamp when the reservation was acquired.
    pub acquired_at: Timestamp,
}

/// High-level operation kind of a persisted background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// Refresh all followed creators or a single followed creator.
    Refresh,
    /// Download a tracked pending upload.
    UploadDownload,
    /// Download directly from a user-supplied URL.
    DirectDownload,
}

impl JobKind {
    /// Returns the canonical lowercase storage identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::UploadDownload => "upload_download",
            Self::DirectDownload => "direct_download",
        }
    }
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobKind {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "refresh" => Ok(Self::Refresh),
            "upload_download" => Ok(Self::UploadDownload),
            "direct_download" => Ok(Self::DirectDownload),
            other => Err(ValidationError::InvalidJobKind {
                kind: other.to_owned(),
            }),
        }
    }
}

/// Display-safe target metadata persisted for a background job.
///
/// Sensitive runtime inputs (resolved cookie values, cookie sources, working directories, or raw
/// command-line credentials) are intentionally excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobTarget {
    /// Refresh all followed creators (`creator_id == None`) or a specific creator.
    Refresh {
        /// Optional local creator ID targeted by the refresh.
        creator_id: Option<CreatorId>,
    },
    /// Download a tracked pending upload.
    UploadDownload {
        /// Local upload ID.
        upload_id: UploadId,
        /// Optional sanitized display URL snapshot.
        url: Option<DisplayUrl>,
    },
    /// Download directly from a user-supplied URL.
    DirectDownload {
        /// Display-safe target URL (credentials, query, and fragment redacted).
        url: DisplayUrl,
    },
}

impl JobTarget {
    /// Returns the [`JobKind`] corresponding to this target.
    #[must_use]
    pub const fn kind(&self) -> JobKind {
        match self {
            Self::Refresh { .. } => JobKind::Refresh,
            Self::UploadDownload { .. } => JobKind::UploadDownload,
            Self::DirectDownload { .. } => JobKind::DirectDownload,
        }
    }

    /// Returns the exclusive [`OperationSlot`] required by this target, or `None` for direct
    /// downloads.
    #[must_use]
    pub const fn occupancy_slot(&self) -> Option<OperationSlot> {
        match self {
            Self::Refresh { .. } => Some(OperationSlot::Refresh),
            Self::UploadDownload { upload_id, .. } => {
                Some(OperationSlot::UploadDownload(*upload_id))
            }
            Self::DirectDownload { .. } => None,
        }
    }
}

/// Lifecycle state of a persisted background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Worker acknowledged ownership and execution is in progress.
    Running,
    /// Execution completed successfully.
    Succeeded,
    /// Execution failed (including partial creator failures during refresh or startup errors).
    Failed,
    /// Previous owner process was confirmed gone and the job was reconciled.
    Interrupted,
}

impl JobState {
    /// Returns the canonical lowercase storage identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }

    /// Returns `true` when no further progress updates or state transitions are permitted.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for JobState {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(ValidationError::InvalidJobState {
                state: other.to_owned(),
            }),
        }
    }
}

/// Structured progress reported for a running refresh job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshProgress {
    /// Total number of followed creators targeted by this refresh, when known.
    pub total_creators: Option<u64>,
    /// Number of creators processed so far (`succeeded_creators + failed_creators`).
    pub processed_creators: u64,
    /// Number of creators refreshed successfully so far.
    pub succeeded_creators: u64,
    /// Number of creators whose refresh failed so far.
    pub failed_creators: u64,
    /// Creator currently being refreshed, if any.
    pub current_creator_id: Option<CreatorId>,
    /// Display name of the creator currently being refreshed, if any.
    pub current_creator_name: Option<String>,
}

/// Execution stage of a running download job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DownloadStage {
    /// Preparing downloader arguments and runtime inputs.
    Preparing,
    /// Resolving remote stream metadata.
    Resolving,
    /// Transferring media or subtitle payloads.
    Downloading,
    /// Post-processing or muxing downloaded streams.
    PostProcessing,
    /// Verifying output artifacts and updating tracked upload state.
    Verifying,
}

impl DownloadStage {
    /// Returns the canonical lowercase storage identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Resolving => "resolving",
            Self::Downloading => "downloading",
            Self::PostProcessing => "post_processing",
            Self::Verifying => "verifying",
        }
    }
}

impl fmt::Display for DownloadStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DownloadStage {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "preparing" => Ok(Self::Preparing),
            "resolving" => Ok(Self::Resolving),
            "downloading" => Ok(Self::Downloading),
            "post_processing" => Ok(Self::PostProcessing),
            "verifying" => Ok(Self::Verifying),
            other => Err(ValidationError::InvalidDownloadStage {
                stage: other.to_owned(),
            }),
        }
    }
}

/// Structured progress reported for a running download job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadProgress {
    /// Current download stage.
    pub stage: DownloadStage,
    /// Bytes transferred so far when reliably reported.
    pub downloaded_bytes: Option<u64>,
    /// Expected total transfer bytes when reliably known.
    pub total_bytes: Option<u64>,
}

/// Structured progress snapshot persisted on a job record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobProgress {
    /// Progress for a refresh job.
    Refresh(RefreshProgress),
    /// Progress for an upload or direct download job.
    Download(DownloadProgress),
}

impl JobProgress {
    /// Validates that `self` matches `kind` and satisfies structural invariants.
    pub fn validate_for_kind(&self, kind: JobKind) -> Result<(), StorageError> {
        match (self, kind) {
            (Self::Refresh(progress), JobKind::Refresh) => {
                let expected = progress
                    .succeeded_creators
                    .checked_add(progress.failed_creators)
                    .ok_or_else(|| {
                        StorageError::Backend(
                            "refresh progress creator count overflowed".to_owned(),
                        )
                    })?;
                if progress.processed_creators != expected {
                    return Err(StorageError::Backend(format!(
                        "invalid refresh progress: processed_creators ({}) != succeeded_creators ({}) + failed_creators ({})",
                        progress.processed_creators,
                        progress.succeeded_creators,
                        progress.failed_creators
                    )));
                }
                Ok(())
            }
            (Self::Download(_), JobKind::UploadDownload | JobKind::DirectDownload) => Ok(()),
            (Self::Refresh(_), JobKind::UploadDownload | JobKind::DirectDownload)
            | (Self::Download(_), JobKind::Refresh) => Err(StorageError::Backend(format!(
                "job progress kind does not match job kind '{kind}'"
            ))),
        }
    }
}

/// Per-creator result retained inside a finished refresh job outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreatorJobOutcome {
    /// Creator feed was fetched and its checkpoint committed atomically.
    Succeeded {
        /// Local creator ID.
        creator_id: CreatorId,
        /// Creator display name.
        display_name: String,
        /// Count of newly inserted pending uploads.
        new_uploads_inserted: u64,
        /// Count of already-known uploads encountered whose existing statuses were preserved.
        existing_uploads_seen: u64,
        /// Refresh checkpoint before this refresh.
        previous_checkpoint: Timestamp,
        /// Refresh checkpoint stored after this refresh.
        new_checkpoint: Timestamp,
    },
    /// Creator refresh failed without advancing its checkpoint.
    Failed {
        /// Local creator ID.
        creator_id: CreatorId,
        /// Creator display name.
        display_name: String,
        /// Failure summary.
        error: String,
    },
}

/// Final outcome persisted for a `succeeded` or `failed` refresh job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshJobOutcome {
    /// Per-creator outcomes in execution order (retained even when some or all creators failed).
    pub creator_outcomes: Vec<CreatorJobOutcome>,
    /// Optional top-level or startup/operation failure summary.
    pub error: Option<String>,
}

/// Final outcome persisted for a `succeeded` or `failed` download job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadJobOutcome {
    /// Confirmed output artifact paths saved on disk.
    pub confirmed_artifacts: Vec<PathBuf>,
    /// Whether a tracked pending upload was transitioned to `downloaded`.
    pub marked_downloaded: bool,
    /// Optional failure summary when the download job failed.
    pub error: Option<String>,
}

/// Final outcome persisted when an abandoned job is reconciled as `interrupted`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedJobOutcome {
    /// Reason explaining why the job was marked interrupted.
    pub reason: String,
}

/// Terminal outcome persisted on a finished job record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// Outcome of a `succeeded` or `failed` refresh job.
    Refresh(RefreshJobOutcome),
    /// Outcome of a `succeeded` or `failed` download job.
    Download(DownloadJobOutcome),
    /// Outcome of an `interrupted` job reconciled after its owner process was confirmed gone.
    Interrupted(InterruptedJobOutcome),
}

/// Terminal completion payload submitted by the active job owner via [`JobStore::finish_job`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobCompletion {
    /// Transition the job from `running` to `succeeded`.
    Succeeded {
        /// Optional final progress snapshot recorded atomically on completion.
        progress: Option<JobProgress>,
        /// Final success outcome matching the job's kind.
        outcome: JobOutcome,
        /// UTC completion timestamp.
        finished_at: Timestamp,
    },
    /// Transition the job from `running` to `failed`.
    Failed {
        /// Optional final progress snapshot recorded atomically on failure.
        progress: Option<JobProgress>,
        /// Final failure outcome matching the job's kind.
        outcome: JobOutcome,
        /// UTC failure timestamp.
        finished_at: Timestamp,
    },
}

impl JobCompletion {
    /// Returns the target terminal [`JobState`].
    #[must_use]
    pub const fn target_state(&self) -> JobState {
        match self {
            Self::Succeeded { .. } => JobState::Succeeded,
            Self::Failed { .. } => JobState::Failed,
        }
    }

    /// Returns the optional final progress snapshot.
    #[must_use]
    pub const fn progress(&self) -> Option<&JobProgress> {
        match self {
            Self::Succeeded { progress, .. } | Self::Failed { progress, .. } => progress.as_ref(),
        }
    }

    /// Returns the terminal [`JobOutcome`].
    #[must_use]
    pub const fn outcome(&self) -> &JobOutcome {
        match self {
            Self::Succeeded { outcome, .. } | Self::Failed { outcome, .. } => outcome,
        }
    }

    /// Returns the UTC completion timestamp.
    #[must_use]
    pub const fn finished_at(&self) -> Timestamp {
        match self {
            Self::Succeeded { finished_at, .. } | Self::Failed { finished_at, .. } => *finished_at,
        }
    }

    /// Validates that the completion state and outcome satisfy the invariants for `kind`.
    pub fn validate_for_kind(&self, kind: JobKind) -> Result<(), StorageError> {
        if let Some(progress) = self.progress() {
            progress.validate_for_kind(kind)?;
        }
        validate_outcome_for_state_and_kind(self.target_state(), kind, self.outcome())
            .map_err(StorageError::Backend)
    }
}

/// Validates that `outcome` is structurally legal for `(state, kind)`.
pub(crate) fn validate_outcome_for_state_and_kind(
    state: JobState,
    kind: JobKind,
    outcome: &JobOutcome,
) -> Result<(), String> {
    match (state, kind, outcome) {
        (JobState::Succeeded, JobKind::Refresh, JobOutcome::Refresh(refresh)) => {
            if refresh.error.is_some()
                || refresh
                    .creator_outcomes
                    .iter()
                    .any(|item| matches!(item, CreatorJobOutcome::Failed { .. }))
            {
                return Err(
                    "refresh job with creator or operation errors cannot finish as succeeded"
                        .to_owned(),
                );
            }
            Ok(())
        }
        (JobState::Failed, JobKind::Refresh, JobOutcome::Refresh(refresh)) => {
            let has_creator_failure = refresh
                .creator_outcomes
                .iter()
                .any(|item| matches!(item, CreatorJobOutcome::Failed { .. }));
            let has_error = refresh
                .error
                .as_deref()
                .is_some_and(|msg| !msg.trim().is_empty());
            if !has_creator_failure && !has_error {
                return Err(
                    "failed refresh job must include a failure reason or at least one failed creator outcome"
                        .to_owned(),
                );
            }
            Ok(())
        }
        (JobState::Succeeded, JobKind::UploadDownload, JobOutcome::Download(download)) => {
            if download.error.is_some() {
                return Err("succeeded upload download cannot carry an error message".to_owned());
            }
            if !download.marked_downloaded || download.confirmed_artifacts.is_empty() {
                return Err(
                    "succeeded upload download requires confirmed artifacts and marked_downloaded = true"
                        .to_owned(),
                );
            }
            Ok(())
        }
        (JobState::Succeeded, JobKind::DirectDownload, JobOutcome::Download(download)) => {
            if download.error.is_some() {
                return Err("succeeded direct download cannot carry an error message".to_owned());
            }
            if download.marked_downloaded {
                return Err("direct download cannot set marked_downloaded = true".to_owned());
            }
            Ok(())
        }
        (
            JobState::Failed,
            JobKind::UploadDownload | JobKind::DirectDownload,
            JobOutcome::Download(download),
        ) => {
            if download.marked_downloaded {
                return Err("failed download cannot set marked_downloaded = true".to_owned());
            }
            if !download
                .error
                .as_deref()
                .is_some_and(|msg| !msg.trim().is_empty())
            {
                return Err("failed download job must include a non-empty error message".to_owned());
            }
            Ok(())
        }
        (JobState::Interrupted, _, JobOutcome::Interrupted(interrupted)) => {
            if interrupted.reason.trim().is_empty() {
                return Err("interrupted job outcome must include a non-empty reason".to_owned());
            }
            Ok(())
        }
        (JobState::Running, _, _) => Err("running job cannot have a terminal outcome".to_owned()),
        _ => Err(format!(
            "outcome does not match job state '{state}' and kind '{kind}'"
        )),
    }
}

/// Full persisted background job record returned by queries and lifecycle transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// Durable unique local job ID.
    pub id: JobId,
    /// Display-safe target metadata.
    pub target: JobTarget,
    /// Current job lifecycle state.
    pub state: JobState,
    /// UTC timestamp when execution began.
    pub started_at: Timestamp,
    /// UTC timestamp of the latest recorded update.
    pub updated_at: Timestamp,
    /// UTC timestamp when the job entered a terminal state, if finished.
    pub finished_at: Option<Timestamp>,
    /// Latest structured progress snapshot, if any.
    pub progress: Option<JobProgress>,
    /// Final structured outcome once terminal, or `None` while `running`.
    pub outcome: Option<JobOutcome>,
    /// Fencing token for the active or last recorded owner.
    pub owner_token: OwnershipToken,
    /// OS process metadata of the active or last recorded owner.
    pub owner: ProcessOwner,
}

/// Fencing capability returned to the owning worker when starting a background job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLease {
    /// Owned job ID.
    pub job_id: JobId,
    /// Fencing token minted for this execution.
    pub owner_token: OwnershipToken,
    /// Exclusive occupancy slot held by this job, or `None` for direct downloads.
    pub slot: Option<OperationSlot>,
}

/// Request to atomically acquire any required occupancy slot and start a background job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartJobRequest {
    /// Display-safe operation target.
    pub target: JobTarget,
    /// OS process identity of the worker acknowledging ownership.
    pub owner: ProcessOwner,
    /// UTC timestamp when the job enters `running`.
    pub started_at: Timestamp,
    /// Optional initial structured progress snapshot.
    pub initial_progress: Option<JobProgress>,
}

/// Combined record and fencing capability produced when a background job starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedJob {
    /// Persisted and immediately queryable `running` job record.
    pub job: JobRecord,
    /// Fencing capability required for subsequent progress and completion updates.
    pub lease: JobLease,
}

/// Result of conditional reconciliation against an [`OperationSlot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileReservationOutcome {
    /// The slot was still held by `expected_owner_token` and was atomically released (along with
    /// transitioning any linked `running` job to `interrupted`).
    Reconciled {
        /// Slot released by reconciliation.
        released_slot: OperationSlot,
        /// Linked job marked `interrupted`, or `None` if the reservation belonged to a foreground
        /// command.
        interrupted_job: Option<JobRecord>,
    },
    /// The slot was already free or is now owned by a different fencing token; nothing was
    /// modified.
    StaleOrReleased {
        /// Current reservation holder, if the slot is now owned by someone else.
        current_active: Option<ActiveOperation>,
    },
}

/// Result of conditional reconciliation against a specific [`JobId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileJobOutcome {
    /// The job was still `running` with `expected_owner_token`, was marked `interrupted`, and any
    /// linked occupancy slot was released in the same transaction.
    Reconciled(JobRecord),
    /// The job had already finished or its `owner_token` no longer matched; nothing was modified.
    NotApplied {
        /// Current persisted state of the job.
        current: JobRecord,
    },
}

/// Persistence and operation occupancy contract for background jobs and foreground locks.
///
/// Implementations must:
/// - Enforce one active `refresh` per database and one active `upload_download` per upload across
///   both foreground reservations and background jobs, while keeping `direct_download` independent.
/// - Allow foreground reservations without inserting rows into public job history.
/// - Fence every mutation with [`OwnershipToken`] so stale writers cannot alter newer ownership or
///   reconciled jobs.
/// - Support conditional reconciliation when the caller confirms the previous owner process is
///   gone (without using progress inactivity as a death heuristic).
/// - Keep terminal job records (`succeeded`, `failed`, `interrupted`) queryable by [`JobId`].
pub trait JobStore: Send + Sync {
    /// Atomically acquires an exclusive foreground reservation for `slot` without creating a
    /// public job history row.
    ///
    /// Fails with [`StorageError::OperationBusy`] carrying the current [`ActiveOperation`] if
    /// `slot` is already occupied.
    fn acquire_reservation(
        &self,
        slot: OperationSlot,
        owner: ProcessOwner,
        acquired_at: Timestamp,
    ) -> Result<OperationReservation, StorageError>;

    /// Releases a foreground reservation previously acquired via [`Self::acquire_reservation`].
    ///
    /// Rejects stale tokens, missing reservations, or background-linked reservations with
    /// [`StorageError::StaleJobOwnership`].
    fn release_reservation(&self, reservation: &OperationReservation) -> Result<(), StorageError>;

    /// Atomically acquires any required occupancy slot and inserts a `running` job record.
    ///
    /// Refresh and tracked upload downloads acquire their [`OperationSlot`] in the same
    /// transaction; direct URL downloads take no slot and can run concurrently. Fails with
    /// [`StorageError::OperationBusy`] if the required slot is already occupied.
    fn start_background_job(&self, request: StartJobRequest) -> Result<StartedJob, StorageError>;

    /// Updates structured progress on a `running` job owned by `lease`.
    ///
    /// Rejects stale tokens or already-terminal jobs with [`StorageError::StaleJobOwnership`].
    fn update_job_progress(
        &self,
        lease: &JobLease,
        progress: JobProgress,
        updated_at: Timestamp,
    ) -> Result<JobRecord, StorageError>;

    /// Atomically transitions a `running` job owned by `lease` to `succeeded` or `failed` and
    /// releases any linked occupancy slot in the same transaction.
    ///
    /// Rejects stale owners and already-terminal jobs with [`StorageError::StaleJobOwnership`].
    fn finish_job(
        &self,
        lease: &JobLease,
        completion: JobCompletion,
    ) -> Result<JobRecord, StorageError>;

    /// Conditionally reconciles a dead owner's exclusive reservation after confirming the holding
    /// process is gone.
    ///
    /// Mutates state only when `slot` is still held by `expected_owner_token`: releases the
    /// reservation and, if linked to a running background job, marks that job `interrupted` in the
    /// same transaction. Returns [`ReconcileReservationOutcome::StaleOrReleased`] without mutating
    /// newer ownership if the token no longer matches.
    fn reconcile_dead_reservation(
        &self,
        slot: OperationSlot,
        expected_owner_token: OwnershipToken,
        reason: &str,
        finished_at: Timestamp,
    ) -> Result<ReconcileReservationOutcome, StorageError>;

    /// Conditionally marks a `running` job `interrupted` and releases any linked occupancy slot
    /// after confirming the owning process is gone (including direct downloads with no slot).
    ///
    /// Mutates state only when the job is still `running` and held by `expected_owner_token`.
    /// Otherwise returns [`ReconcileJobOutcome::NotApplied`] with the current record.
    fn reconcile_interrupted_job(
        &self,
        job_id: JobId,
        expected_owner_token: OwnershipToken,
        reason: &str,
        finished_at: Timestamp,
    ) -> Result<ReconcileJobOutcome, StorageError>;

    /// Fetches a persisted job record (running or terminal) by [`JobId`].
    fn get_job(&self, job_id: JobId) -> Result<JobRecord, StorageError>;

    /// Returns the active operation holding `slot`, or `None` if unoccupied.
    fn get_active_reservation(
        &self,
        slot: OperationSlot,
    ) -> Result<Option<ActiveOperation>, StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_url_redacts_credentials_query_and_fragment_idempotently() {
        let sanitized = DisplayUrl::sanitize(
            "https://alice:s3cr3t@www.bilibili.com:8443/video/BV1xx?p=2&sess=abc#t=12",
        )
        .unwrap();
        assert_eq!(
            sanitized.as_str(),
            "https://www.bilibili.com:8443/video/BV1xx"
        );
        assert_eq!(DisplayUrl::sanitize(sanitized.as_str()).unwrap(), sanitized);

        let user_only = DisplayUrl::sanitize("https://token@example.com/media").unwrap();
        assert_eq!(user_only.as_str(), "https://example.com/media");

        let pass_only = DisplayUrl::sanitize("https://:secret@example.com/media?k=v").unwrap();
        assert_eq!(pass_only.as_str(), "https://example.com/media");

        assert!(DisplayUrl::sanitize("").is_err());
        assert!(DisplayUrl::sanitize("not a url").is_err());
        assert!(DisplayUrl::sanitize("mailto:user@example.com").is_err());
        assert!(DisplayUrl::sanitize("file:///tmp/video.mp4").is_err());
    }

    #[test]
    fn job_ids_tokens_enums_and_slots_validate_correctly() {
        assert_eq!("7".parse::<JobId>().unwrap().as_i64(), 7);
        assert!("0".parse::<JobId>().is_err());
        assert!("-1".parse::<JobId>().is_err());

        assert_eq!("3".parse::<OwnershipToken>().unwrap().as_i64(), 3);
        assert!(OwnershipToken::new(0).is_err());

        assert!(ProcessOwner::new(0, None).is_err());
        assert!(ProcessOwner::new(123, Some("   ".to_owned())).is_err());
        assert_eq!(
            ProcessOwner::new(123, Some(" 456 ".to_owned())).unwrap(),
            ProcessOwner {
                pid: 123,
                process_started_at: Some("456".to_owned()),
            }
        );

        assert_eq!(OperationSlot::Refresh.slot_key(), "refresh");
        assert_eq!(
            OperationSlot::UploadDownload(UploadId::new(42).unwrap()).slot_key(),
            "upload_download:42"
        );

        assert!(!JobState::Running.is_terminal());
        assert!(JobState::Succeeded.is_terminal());
        assert!(JobState::Failed.is_terminal());
        assert!(JobState::Interrupted.is_terminal());
    }

    #[test]
    fn job_completion_and_progress_enforce_domain_invariants() {
        let now = Timestamp::from_unix_seconds(1_760_000_000);
        let cid = CreatorId::new(1).unwrap();

        let bad_refresh_progress = JobProgress::Refresh(RefreshProgress {
            total_creators: Some(2),
            processed_creators: 1,
            succeeded_creators: 1,
            failed_creators: 1,
            current_creator_id: None,
            current_creator_name: None,
        });
        assert!(
            bad_refresh_progress
                .validate_for_kind(JobKind::Refresh)
                .is_err()
        );
        assert!(
            bad_refresh_progress
                .validate_for_kind(JobKind::DirectDownload)
                .is_err()
        );

        let mixed_refresh_outcome = JobOutcome::Refresh(RefreshJobOutcome {
            creator_outcomes: vec![
                CreatorJobOutcome::Succeeded {
                    creator_id: cid,
                    display_name: "Alice".to_owned(),
                    new_uploads_inserted: 1,
                    existing_uploads_seen: 0,
                    previous_checkpoint: now,
                    new_checkpoint: now,
                },
                CreatorJobOutcome::Failed {
                    creator_id: CreatorId::new(2).unwrap(),
                    display_name: "Bob".to_owned(),
                    error: "network error".to_owned(),
                },
            ],
            error: None,
        });
        assert!(
            JobCompletion::Succeeded {
                progress: None,
                outcome: mixed_refresh_outcome.clone(),
                finished_at: now,
            }
            .validate_for_kind(JobKind::Refresh)
            .is_err()
        );
        assert!(
            JobCompletion::Failed {
                progress: None,
                outcome: mixed_refresh_outcome,
                finished_at: now,
            }
            .validate_for_kind(JobKind::Refresh)
            .is_ok()
        );

        let missing_artifact_upload = JobOutcome::Download(DownloadJobOutcome {
            confirmed_artifacts: Vec::new(),
            marked_downloaded: true,
            error: None,
        });
        assert!(
            JobCompletion::Succeeded {
                progress: None,
                outcome: missing_artifact_upload,
                finished_at: now,
            }
            .validate_for_kind(JobKind::UploadDownload)
            .is_err()
        );

        let direct_marked_upload = JobOutcome::Download(DownloadJobOutcome {
            confirmed_artifacts: vec![PathBuf::from("video.mp4")],
            marked_downloaded: true,
            error: None,
        });
        assert!(
            JobCompletion::Succeeded {
                progress: None,
                outcome: direct_marked_upload,
                finished_at: now,
            }
            .validate_for_kind(JobKind::DirectDownload)
            .is_err()
        );
    }
}
