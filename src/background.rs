//! Detached background job execution, worker IPC handshake, and dead-owner reconciliation.

mod process;

use std::env;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cli::{DownloadArgs, RefreshArgs};
use crate::config::resolve_bilibili_api_base;
use crate::domain::{
    CookieSource, CreatorId, FollowedCreator, Platform, PresetName, Timestamp, Upload, UploadId,
    UploadStatus,
};
use crate::download::{
    DownloadExecutionOptions, DownloadRequest, DownloadTarget, Downloader, YtDlpDownloader,
    ensure_no_media_to_stdout, resolve_download_request,
};
use crate::error::{AppError, BackgroundError, DownloadError, RefreshError, StorageError};
use crate::jobs::{
    ActiveOperation, CreatorJobOutcome, DisplayUrl, DownloadJobOutcome, DownloadProgress,
    DownloadStage, ExecutionMode, JobCompletion, JobId, JobKind, JobLease, JobOutcome, JobProgress,
    JobRecord, JobState, JobStore, JobTarget, OperationReservation, OperationSlot, OwnershipToken,
    ProcessOwner, ReconcileJobOutcome, RefreshJobOutcome, RefreshProgress, StartJobRequest,
    StartedJob,
};
use crate::platform::bilibili::{BilibiliClientConfig, BilibiliRefreshSource};
use crate::refresh::{
    CreatorRefreshOutcome, RefreshCookiePolicy, RefreshExecutionEvent,
    refresh_targets_with_observer, resolve_refresh_targets,
};
use crate::storage::{SqliteStorage, Storage};

pub(crate) use process::spawn_tracked_command;
pub use process::{
    ProcessLiveness, ProcessTrackerConfig, check_process_liveness, current_process_owner,
};

const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_secs(15);
const PROGRESS_PERSIST_INTERVAL: Duration = Duration::from_millis(200);
const MAX_LAUNCH_SPEC_BYTES: usize = 4 * 1024 * 1024;
const MAX_ACK_LINE_BYTES: usize = 64 * 1024;
const MAX_RECONCILE_ATTEMPTS: usize = 4;

/// Runtime configuration controlling how detached background workers are spawned and awaited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundRuntime {
    /// Optional Bilibili API base URL override propagated to background refresh workers.
    pub api_base: Option<String>,
    /// Optional override for the worker executable path (defaults to `std::env::current_exe()`).
    pub worker_exe: Option<PathBuf>,
    /// Arguments passed to the worker executable (defaults to `["__worker"]`).
    pub worker_args: Vec<String>,
    /// Maximum duration to wait for worker startup (payload write + ownership ack).
    pub ack_timeout: Duration,
}

impl Default for BackgroundRuntime {
    fn default() -> Self {
        Self {
            api_base: None,
            worker_exe: None,
            worker_args: vec!["__worker".to_owned()],
            ack_timeout: DEFAULT_ACK_TIMEOUT,
        }
    }
}

impl BackgroundRuntime {
    /// Creates a [`BackgroundRuntime`] carrying an optional validated Bilibili API base URL.
    #[must_use]
    pub fn with_api_base(api_base: Option<String>) -> Self {
        Self {
            api_base,
            ..Self::default()
        }
    }

    /// Overrides the executable spawned for background workers.
    #[must_use]
    pub fn with_worker_exe(mut self, exe: impl Into<PathBuf>) -> Self {
        self.worker_exe = Some(exe.into());
        self
    }

    /// Overrides the CLI arguments passed to the worker executable.
    #[must_use]
    pub fn with_worker_args(mut self, args: Vec<String>) -> Self {
        self.worker_args = args;
        self
    }

    /// Overrides the startup acknowledgment timeout.
    #[must_use]
    pub fn with_ack_timeout(mut self, timeout: Duration) -> Self {
        self.ack_timeout = timeout;
        self
    }
}

/// RAII lease guard ensuring foreground [`OperationReservation`] slots are released on every exit.
pub struct ReservationGuard<'a> {
    job_store: &'a dyn JobStore,
    reservation: Option<OperationReservation>,
}

impl<'a> ReservationGuard<'a> {
    /// Wraps an active foreground [`OperationReservation`] for automatic release on drop.
    #[must_use]
    pub fn new(job_store: &'a dyn JobStore, reservation: OperationReservation) -> Self {
        Self {
            job_store,
            reservation: Some(reservation),
        }
    }

    /// Returns the active fencing token for the reserved slot.
    #[must_use]
    pub fn owner_token(&self) -> OwnershipToken {
        self.reservation
            .as_ref()
            .expect("reservation is live until Drop")
            .owner_token
    }

    /// Returns the recorded OS owner identity for the reserved slot.
    #[must_use]
    pub fn owner(&self) -> &ProcessOwner {
        &self
            .reservation
            .as_ref()
            .expect("reservation is live until Drop")
            .owner
    }
}

impl Drop for ReservationGuard<'_> {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            let _ = self.job_store.release_reservation(&reservation);
        }
    }
}

/// Acquires an exclusive foreground reservation, reconciling confirmed-dead slot owners and any
/// tracked child process trees.
pub fn acquire_reservation_with_reconcile<'a>(
    job_store: &'a dyn JobStore,
    db_path: Option<&Path>,
    slot: OperationSlot,
) -> Result<ReservationGuard<'a>, AppError> {
    let owner = current_process_owner();
    for _ in 0..MAX_RECONCILE_ATTEMPTS {
        match job_store.acquire_reservation(slot, owner.clone(), Timestamp::now()) {
            Ok(reservation) => return Ok(ReservationGuard::new(job_store, reservation)),
            Err(StorageError::OperationBusy { active }) => {
                match process::check_active_owner_liveness(
                    &active.owner,
                    db_path,
                    Some(active.owner_token),
                    active.execution_mode,
                ) {
                    ProcessLiveness::ConfirmedDead { reason } => {
                        let _ = job_store.reconcile_dead_reservation(
                            slot,
                            active.owner_token,
                            &reason,
                            Timestamp::now(),
                        )?;
                    }
                    ProcessLiveness::Alive | ProcessLiveness::Ambiguous => {
                        return Err(StorageError::OperationBusy { active }.into());
                    }
                }
            }
            Err(err) => return Err(err.into()),
        }
    }

    // Final attempt after bounded reconciliations; returns OperationBusy if still contested.
    let reservation = job_store.acquire_reservation(slot, owner, Timestamp::now())?;
    Ok(ReservationGuard::new(job_store, reservation))
}

/// Checks whether `slot` is available, reconciling any confirmed-dead owner before returning.
pub fn ensure_slot_available_with_reconcile(
    job_store: &dyn JobStore,
    db_path: Option<&Path>,
    slot: OperationSlot,
) -> Result<(), AppError> {
    for _ in 0..MAX_RECONCILE_ATTEMPTS {
        let Some(active) = job_store.get_active_reservation(slot)? else {
            return Ok(());
        };
        match process::check_active_owner_liveness(
            &active.owner,
            db_path,
            Some(active.owner_token),
            active.execution_mode,
        ) {
            ProcessLiveness::ConfirmedDead { reason } => {
                let _ = job_store.reconcile_dead_reservation(
                    slot,
                    active.owner_token,
                    &reason,
                    Timestamp::now(),
                )?;
            }
            ProcessLiveness::Alive | ProcessLiveness::Ambiguous => {
                return Err(StorageError::OperationBusy {
                    active: Box::new(active),
                }
                .into());
            }
        }
    }

    if let Some(active) = job_store.get_active_reservation(slot)? {
        return Err(StorageError::OperationBusy {
            active: Box::new(active),
        }
        .into());
    }
    Ok(())
}

/// Starts a background job and acquires any required occupancy slot, reconciling dead owners.
pub fn start_background_job_with_reconcile(
    job_store: &dyn JobStore,
    db_path: Option<&Path>,
    request: StartJobRequest,
) -> Result<StartedJob, AppError> {
    for _ in 0..MAX_RECONCILE_ATTEMPTS {
        match job_store.start_background_job(request.clone()) {
            Ok(started) => return Ok(started),
            Err(StorageError::OperationBusy { active }) => {
                match process::check_active_owner_liveness(
                    &active.owner,
                    db_path,
                    Some(active.owner_token),
                    active.execution_mode,
                ) {
                    ProcessLiveness::ConfirmedDead { reason } => {
                        let _ = job_store.reconcile_dead_reservation(
                            active.slot,
                            active.owner_token,
                            &reason,
                            Timestamp::now(),
                        )?;
                    }
                    ProcessLiveness::Alive | ProcessLiveness::Ambiguous => {
                        return Err(StorageError::OperationBusy { active }.into());
                    }
                }
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(job_store.start_background_job(request)?)
}

/// Fetches a job record by ID, marking it `interrupted` if `running` and both its owner process
/// and any active downloader process tree are confirmed gone.
pub fn query_job_status_with_reconcile(
    job_store: &dyn JobStore,
    db_path: Option<&Path>,
    job_id: JobId,
) -> Result<JobRecord, AppError> {
    let job = job_store.get_job(job_id)?;
    if job.state != JobState::Running {
        return Ok(job);
    }

    match process::check_active_owner_liveness(
        &job.owner,
        db_path,
        Some(job.owner_token),
        ExecutionMode::Background,
    ) {
        ProcessLiveness::ConfirmedDead { reason } => {
            match job_store.reconcile_interrupted_job(
                job.id,
                job.owner_token,
                &reason,
                Timestamp::now(),
            )? {
                ReconcileJobOutcome::Reconciled(updated)
                | ReconcileJobOutcome::NotApplied { current: updated } => Ok(updated),
            }
        }
        ProcessLiveness::Alive | ProcessLiveness::Ambiguous => Ok(job),
    }
}

/// Submits a detached background refresh job and returns the queryable [`JobRecord`] once the
/// worker acknowledges ownership.
pub fn submit_background_refresh(
    storage: &dyn Storage,
    args: RefreshArgs,
    runtime: &BackgroundRuntime,
) -> Result<JobRecord, AppError> {
    let job_store = storage
        .job_store()
        .ok_or(BackgroundError::UnsupportedStorage)?;
    let db_path = storage
        .database_path()
        .ok_or(BackgroundError::UnsupportedStorage)?
        .to_path_buf();

    let cookie_policy = args.cookie_policy()?;
    // Fast pre-check so invalid creator arguments fail before spawning a worker.
    let _ = resolve_refresh_targets(storage, args.creator_id)?;

    let submission_cwd = env::current_dir()
        .map_err(|err| BackgroundError::Io(format!("failed to determine working dir: {err}")))?;
    let (cookies, preference_update) =
        resolve_submission_refresh_cookies(storage, cookie_policy, &submission_cwd)?;

    ensure_slot_available_with_reconcile(job_store, Some(&db_path), OperationSlot::Refresh)?;

    let spec = WorkerLaunchSpec {
        db_path: EncodedPath::from_path(&db_path),
        working_dir: EncodedPath::from_path(&submission_cwd),
        api_base: runtime.api_base.clone(),
        operation: WorkerOperation::Refresh {
            creator_id: args.creator_id.map(CreatorId::as_i64),
            cookies: cookies.as_ref().map(EncodedCookieSource::from_domain),
            preference_update,
        },
    };

    let job_id = spawn_and_await_worker(runtime, &submission_cwd, &spec)?;
    Ok(job_store.get_job(job_id)?)
}

/// Submits a detached background download job and returns the queryable [`JobRecord`] once the
/// worker acknowledges ownership.
pub fn submit_background_download(
    storage: &dyn Storage,
    args: DownloadArgs,
    runtime: &BackgroundRuntime,
) -> Result<JobRecord, AppError> {
    let job_store = storage
        .job_store()
        .ok_or(BackgroundError::UnsupportedStorage)?;
    let db_path = storage
        .database_path()
        .ok_or(BackgroundError::UnsupportedStorage)?
        .to_path_buf();

    let mut plan = args.to_plan_input()?;
    let submission_cwd = env::current_dir()
        .map_err(|err| BackgroundError::Io(format!("failed to determine working dir: {err}")))?;
    plan.cookies = plan
        .cookies
        .map(|source| absolutize_cookie_source(source, &submission_cwd))
        .transpose()?;

    let request = resolve_download_request(storage, plan)?;
    request.validate()?;
    ensure_no_media_to_stdout(&request, &submission_cwd)?;

    match &request.target {
        DownloadTarget::DirectUrl { url } => {
            // Ensure the display-safe target URL can be redacted and persisted safely before spawning.
            let _ = DisplayUrl::sanitize(url)?;
        }
        DownloadTarget::PendingUpload { upload } => {
            ensure_slot_available_with_reconcile(
                job_store,
                Some(&db_path),
                OperationSlot::UploadDownload(upload.id),
            )?;
        }
    }

    let spec = WorkerLaunchSpec {
        db_path: EncodedPath::from_path(&db_path),
        working_dir: EncodedPath::from_path(&submission_cwd),
        api_base: runtime.api_base.clone(),
        operation: WorkerOperation::Download {
            request: EncodedDownloadRequest::from_domain(&request),
        },
    };

    let job_id = spawn_and_await_worker(runtime, &submission_cwd, &spec)?;
    Ok(job_store.get_job(job_id)?)
}

fn resolve_submission_refresh_cookies(
    storage: &dyn Storage,
    policy: RefreshCookiePolicy,
    submission_cwd: &Path,
) -> Result<(Option<CookieSource>, Option<RefreshPreferenceUpdate>), AppError> {
    match policy {
        RefreshCookiePolicy::UseSavedDefault => Ok((storage.get_default_refresh_cookies()?, None)),
        RefreshCookiePolicy::Explicit {
            source,
            save_as_default,
        } => {
            let resolved = absolutize_cookie_source(source, submission_cwd)?;
            let pref = save_as_default.then(|| RefreshPreferenceUpdate::Save {
                source: EncodedCookieSource::from_domain(&resolved),
            });
            Ok((Some(resolved), pref))
        }
        RefreshCookiePolicy::ClearSavedDefault => Ok((None, Some(RefreshPreferenceUpdate::Clear))),
        RefreshCookiePolicy::IgnoreSavedDefault => Ok((None, None)),
    }
}

fn absolutize_cookie_source(
    source: CookieSource,
    submission_cwd: &Path,
) -> Result<CookieSource, AppError> {
    match source {
        CookieSource::File(path) => {
            let resolved = if path.is_relative() {
                submission_cwd.join(path)
            } else {
                path
            };
            let abs = std::path::absolute(&resolved).map_err(|err| {
                BackgroundError::Io(format!(
                    "failed to resolve cookie file path '{}': {err}",
                    resolved.display()
                ))
            })?;
            Ok(CookieSource::File(abs))
        }
        CookieSource::BrowserProfile(spec) => Ok(CookieSource::BrowserProfile(spec)),
    }
}

enum StartupPipeEvent {
    StdinDone(io::Result<()>),
    AckRead(io::Result<Option<String>>),
}

fn spawn_and_await_worker(
    runtime: &BackgroundRuntime,
    working_dir: &Path,
    spec: &WorkerLaunchSpec,
) -> Result<JobId, AppError> {
    let exe = match &runtime.worker_exe {
        Some(path) => path.clone(),
        None => env::current_exe().map_err(|err| {
            BackgroundError::Io(format!("failed to locate siphon executable: {err}"))
        })?,
    };

    let payload = serde_json::to_vec(spec).map_err(|err| {
        BackgroundError::Io(format!("failed to serialize worker launch payload: {err}"))
    })?;

    let mut cmd = Command::new(&exe);
    cmd.args(&runtime.worker_args);
    cmd.current_dir(working_dir);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());
    process::configure_detached_worker_command(&mut cmd);

    let deadline = Instant::now() + runtime.ack_timeout;

    let mut child = cmd
        .spawn()
        .map_err(|err| BackgroundError::WorkerSpawnFailed {
            binary: exe.to_string_lossy().into_owned(),
            reason: err.to_string(),
        })?;

    let mut stdin = child.stdin.take().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        BackgroundError::Io("worker stdin pipe was unavailable".to_owned())
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        BackgroundError::Io("worker stdout pipe was unavailable".to_owned())
    })?;

    let (event_tx, event_rx) = mpsc::channel::<StartupPipeEvent>();
    let writer_tx = event_tx.clone();
    thread::spawn(move || {
        let res = stdin.write_all(&payload);
        drop(stdin);
        let _ = writer_tx.send(StartupPipeEvent::StdinDone(res));
    });

    let reader_tx = event_tx;
    let reader_handle = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let res = read_bounded_line(&mut reader, MAX_ACK_LINE_BYTES);
        let _ = reader_tx.send(StartupPipeEvent::AckRead(res));
    });

    let ack_line_res = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match event_rx.recv_timeout(remaining) {
            Ok(StartupPipeEvent::StdinDone(Ok(()))) => {}
            Ok(StartupPipeEvent::StdinDone(Err(err))) => {
                // BrokenPipe is expected if the worker returned `WorkerAck::Err` and closed stdin
                // before draining the payload; wait for `AckRead` to surface that structured error.
                if err.kind() != io::ErrorKind::BrokenPipe {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BackgroundError::Io(format!(
                        "failed to deliver launch configuration to worker: {err}"
                    ))
                    .into());
                }
            }
            Ok(StartupPipeEvent::AckRead(res)) => break res,
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BackgroundError::WorkerAckTimeout.into());
            }
        }
    };

    let _ = reader_handle.join();
    match ack_line_res {
        Ok(Some(line)) => {
            let ack: WorkerAck = serde_json::from_str(line.trim()).map_err(|err| {
                let _ = child.kill();
                let _ = child.wait();
                BackgroundError::WorkerStartupFailed {
                    reason: format!("invalid worker acknowledgment: {err}"),
                }
            })?;
            match ack {
                WorkerAck::Ok { job_id } => {
                    let parsed_id =
                        JobId::new(job_id).map_err(|err| BackgroundError::WorkerStartupFailed {
                            reason: format!("invalid job ID in worker acknowledgment: {err}"),
                        })?;
                    Ok(parsed_id)
                }
                WorkerAck::Err { failure } => {
                    let _ = child.wait();
                    Err(failure.into_app_error())
                }
            }
        }
        Ok(None) => {
            let status_desc = match child.wait() {
                Ok(status) => match status.code() {
                    Some(code) => format!("exit code {code}"),
                    None => "terminated by signal".to_owned(),
                },
                Err(err) => format!("wait error: {err}"),
            };
            Err(BackgroundError::WorkerStartupFailed {
                reason: format!("worker exited ({status_desc}) before acknowledging job ownership"),
            }
            .into())
        }
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(BackgroundError::Io(format!("failed reading worker acknowledgment: {err}")).into())
        }
    }
}

fn read_bounded_line<R: BufRead>(reader: &mut R, max_bytes: usize) -> io::Result<Option<String>> {
    let mut raw = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if raw.is_empty() {
                return Ok(None);
            }
            break;
        }
        let (consume, done) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (available.len(), false),
        };
        if raw.len() + consume > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker acknowledgment exceeded maximum size",
            ));
        }
        raw.extend_from_slice(&available[..consume]);
        reader.consume(consume);
        if done {
            break;
        }
    }

    let text = String::from_utf8(raw).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("non-UTF-8 ack: {}", err.utf8_error()),
        )
    })?;
    Ok(Some(text))
}

/// Entrypoint invoked inside the detached `siphon __worker` child process.
pub fn run_worker_entrypoint() -> ExitCode {
    let startup_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(prepare_worker));
    let prepared = match startup_res {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(err)) => {
            let _ = write_worker_ack(&WorkerAck::Err {
                failure: WorkerStartupFailure::from_app_error(&err),
            });
            return ExitCode::FAILURE;
        }
        Err(_) => {
            let _ = write_worker_ack(&WorkerAck::Err {
                failure: WorkerStartupFailure::Other {
                    message: "worker panicked before acknowledging job ownership".to_owned(),
                },
            });
            return ExitCode::FAILURE;
        }
    };

    let job_id = prepared.job_id();
    if write_worker_ack(&WorkerAck::Ok {
        job_id: job_id.as_i64(),
    })
    .is_err()
    {
        // Parent vanished before receiving ack; fail the job cleanly and release its slot.
        prepared.abort_unacknowledged("submitting process disconnected before receiving ack");
        return ExitCode::FAILURE;
    }

    process::redirect_worker_stdio_to_null();

    let exec_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepared.execute()));
    match exec_res {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) | Err(_) => ExitCode::FAILURE,
    }
}

fn write_worker_ack(ack: &WorkerAck) -> io::Result<()> {
    let mut line = serde_json::to_string(ack)
        .map_err(|err| io::Error::other(format!("serialize worker ack: {err}")))?;
    line.push('\n');
    let mut stdout = io::stdout().lock();
    stdout.write_all(line.as_bytes())?;
    stdout.flush()
}

/// Ensures any job lease acquired during worker startup is failed cleanly (releasing its slot)
/// whenever a post-claim startup check errors or panics before acknowledgment is sent.
struct StartupLeaseGuard<'a> {
    storage: &'a SqliteStorage,
    lease: Option<JobLease>,
    kind: JobKind,
}

impl<'a> StartupLeaseGuard<'a> {
    fn new(storage: &'a SqliteStorage, lease: JobLease, kind: JobKind) -> Self {
        Self {
            storage,
            lease: Some(lease),
            kind,
        }
    }

    fn lease(&self) -> &JobLease {
        self.lease.as_ref().expect("lease is live until disarmed")
    }

    fn fail_and_release(mut self, err: &AppError) {
        self.finish_failed_inner(&persist_safe_app_error(err));
    }

    fn disarm(mut self) -> JobLease {
        self.lease.take().expect("lease is live until disarmed")
    }

    fn finish_failed_inner(&mut self, safe_reason: &str) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        let outcome = match self.kind {
            JobKind::Refresh => JobOutcome::Refresh(RefreshJobOutcome {
                creator_outcomes: Vec::new(),
                error: Some(safe_reason.to_owned()),
            }),
            JobKind::UploadDownload | JobKind::DirectDownload => {
                JobOutcome::Download(DownloadJobOutcome {
                    confirmed_artifacts: Vec::new(),
                    marked_downloaded: false,
                    error: Some(safe_reason.to_owned()),
                })
            }
        };
        let _ = self.storage.finish_job(
            &lease,
            JobCompletion::Failed {
                progress: None,
                outcome,
                finished_at: Timestamp::now(),
            },
        );
    }
}

impl Drop for StartupLeaseGuard<'_> {
    fn drop(&mut self) {
        self.finish_failed_inner("background worker aborted before startup completed");
    }
}

enum PreparedWorker {
    Refresh {
        storage: SqliteStorage,
        lease: JobLease,
        api_base: Option<String>,
        targets: Vec<FollowedCreator>,
        cookies: Option<CookieSource>,
    },
    Download {
        storage: SqliteStorage,
        db_path: PathBuf,
        lease: JobLease,
        owner: ProcessOwner,
        working_dir: PathBuf,
        request: Box<DownloadRequest>,
        display_url: Option<DisplayUrl>,
    },
}

impl PreparedWorker {
    fn job_id(&self) -> JobId {
        match self {
            Self::Refresh { lease, .. } | Self::Download { lease, .. } => lease.job_id,
        }
    }

    fn abort_unacknowledged(self, reason: &str) {
        let now = Timestamp::now();
        match self {
            Self::Refresh { storage, lease, .. } => {
                let _ = storage.finish_job(
                    &lease,
                    JobCompletion::Failed {
                        progress: None,
                        outcome: JobOutcome::Refresh(RefreshJobOutcome {
                            creator_outcomes: Vec::new(),
                            error: Some(reason.to_owned()),
                        }),
                        finished_at: now,
                    },
                );
            }
            Self::Download { storage, lease, .. } => {
                let _ = storage.finish_job(
                    &lease,
                    JobCompletion::Failed {
                        progress: None,
                        outcome: JobOutcome::Download(DownloadJobOutcome {
                            confirmed_artifacts: Vec::new(),
                            marked_downloaded: false,
                            error: Some(reason.to_owned()),
                        }),
                        finished_at: now,
                    },
                );
            }
        }
    }

    fn execute(self) -> bool {
        match self {
            Self::Refresh {
                storage,
                lease,
                api_base,
                targets,
                cookies,
            } => execute_refresh_worker(&storage, &lease, api_base, targets, cookies),
            Self::Download {
                storage,
                db_path,
                lease,
                owner,
                working_dir,
                request,
                display_url,
            } => execute_download_worker(
                &storage,
                &db_path,
                &lease,
                &owner,
                &working_dir,
                &request,
                display_url,
            ),
        }
    }
}

fn prepare_worker() -> Result<PreparedWorker, AppError> {
    let mut raw = Vec::new();
    io::stdin()
        .take((MAX_LAUNCH_SPEC_BYTES + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(|err| BackgroundError::Io(format!("failed reading worker stdin: {err}")))?;
    if raw.len() > MAX_LAUNCH_SPEC_BYTES {
        return Err(BackgroundError::WorkerStartupFailed {
            reason: "worker launch specification exceeded size limit".to_owned(),
        }
        .into());
    }

    let spec: WorkerLaunchSpec =
        serde_json::from_slice(&raw).map_err(|err| BackgroundError::WorkerStartupFailed {
            reason: format!("invalid worker launch specification: {err}"),
        })?;

    let db_path = spec.db_path.into_path_buf();
    let working_dir = spec.working_dir.into_path_buf();
    let api_base = match spec.api_base {
        Some(base) => Some(base),
        None => resolve_bilibili_api_base()
            .map_err(|reason| BackgroundError::WorkerStartupFailed { reason })?,
    };

    let storage = SqliteStorage::open(&db_path)?;
    let owner = current_process_owner();

    match spec.operation {
        WorkerOperation::Refresh {
            creator_id,
            cookies,
            preference_update,
        } => {
            let creator_id = creator_id
                .map(CreatorId::new)
                .transpose()
                .map_err(AppError::Validation)?;
            let cookies = cookies.map(EncodedCookieSource::into_domain);

            // Claim the refresh slot BEFORE resolving targets/checkpoints so no concurrent refresh
            // can commit newer checkpoints between target resolution and slot ownership.
            let started = start_background_job_with_reconcile(
                &storage,
                Some(&db_path),
                StartJobRequest {
                    target: JobTarget::Refresh { creator_id },
                    owner,
                    started_at: Timestamp::now(),
                    initial_progress: None,
                },
            )?;
            let lease_guard = StartupLeaseGuard::new(&storage, started.lease, JobKind::Refresh);

            let targets = match resolve_refresh_targets(&storage, creator_id) {
                Ok(targets) => targets,
                Err(err) => {
                    lease_guard.fail_and_release(&err);
                    return Err(err);
                }
            };

            if let Some(update) = preference_update {
                let res = match update {
                    RefreshPreferenceUpdate::Save { source } => {
                        storage.set_default_refresh_cookies(Some(&source.into_domain()))
                    }
                    RefreshPreferenceUpdate::Clear => storage.set_default_refresh_cookies(None),
                };
                if let Err(err) = res {
                    let app_err: AppError = err.into();
                    lease_guard.fail_and_release(&app_err);
                    return Err(app_err);
                }
            }

            let total_creators = targets.len() as u64;
            let first = targets.first();
            let initial_progress = RefreshProgress {
                total_creators: Some(total_creators),
                processed_creators: 0,
                succeeded_creators: 0,
                failed_creators: 0,
                current_creator_id: first.map(|c| c.creator.id),
                current_creator_name: first.map(|c| c.creator.display_name.clone()),
            };
            if let Err(err) = storage.update_job_progress(
                lease_guard.lease(),
                JobProgress::Refresh(initial_progress),
                Timestamp::now(),
            ) {
                let app_err: AppError = err.into();
                lease_guard.fail_and_release(&app_err);
                return Err(app_err);
            }

            let lease = lease_guard.disarm();
            Ok(PreparedWorker::Refresh {
                storage,
                lease,
                api_base,
                targets,
                cookies,
            })
        }
        WorkerOperation::Download { request } => {
            let mut request = request.into_domain()?;
            let (initial_target, mut display_url, kind) = match &request.target {
                DownloadTarget::DirectUrl { url } => {
                    let display = DisplayUrl::sanitize(url)?;
                    (
                        JobTarget::DirectDownload {
                            url: display.clone(),
                        },
                        Some(display),
                        JobKind::DirectDownload,
                    )
                }
                DownloadTarget::PendingUpload { upload } => {
                    let display = DisplayUrl::sanitize(&upload.url).ok();
                    (
                        JobTarget::UploadDownload {
                            upload_id: upload.id,
                            url: display.clone(),
                        },
                        display,
                        JobKind::UploadDownload,
                    )
                }
            };

            // Acquire job ownership (and UploadDownload slot when targeted) BEFORE checking current
            // upload status so another downloader cannot complete the upload between check & claim.
            let started = start_background_job_with_reconcile(
                &storage,
                Some(&db_path),
                StartJobRequest {
                    target: initial_target,
                    owner: owner.clone(),
                    started_at: Timestamp::now(),
                    initial_progress: Some(JobProgress::Download(DownloadProgress {
                        stage: DownloadStage::Preparing,
                        downloaded_bytes: None,
                        total_bytes: None,
                    })),
                },
            )?;
            let lease_guard = StartupLeaseGuard::new(&storage, started.lease, kind);

            if let DownloadTarget::PendingUpload { upload } = &request.target {
                let current_upload = match storage.get_upload(upload.id) {
                    Ok(found) => found,
                    Err(err) => {
                        let app_err: AppError = err.into();
                        lease_guard.fail_and_release(&app_err);
                        return Err(app_err);
                    }
                };
                if current_upload.status != UploadStatus::Pending {
                    let app_err: AppError = StorageError::UploadNotPending {
                        upload_id: current_upload.id,
                        status: current_upload.status,
                    }
                    .into();
                    lease_guard.fail_and_release(&app_err);
                    return Err(app_err);
                }
                display_url = DisplayUrl::sanitize(&current_upload.url).ok();
                request.target = DownloadTarget::PendingUpload {
                    upload: current_upload,
                };
                if let Err(err) = request.validate() {
                    let app_err: AppError = err.into();
                    lease_guard.fail_and_release(&app_err);
                    return Err(app_err);
                }
            }

            let lease = lease_guard.disarm();
            Ok(PreparedWorker::Download {
                storage,
                db_path,
                lease,
                owner,
                working_dir,
                request: Box::new(request),
                display_url,
            })
        }
    }
}

fn execute_refresh_worker(
    storage: &SqliteStorage,
    lease: &JobLease,
    api_base: Option<String>,
    targets: Vec<FollowedCreator>,
    cookies: Option<CookieSource>,
) -> bool {
    let mut bili_config = BilibiliClientConfig::default();
    if let Some(base_url) = api_base {
        bili_config = bili_config.with_api_base_url(base_url);
    }
    let refresh_source = BilibiliRefreshSource::with_config(bili_config);

    let total_creators = targets.len() as u64;
    let mut succeeded_creators = 0u64;
    let mut failed_creators = 0u64;

    let raw_outcomes = refresh_targets_with_observer(
        storage,
        &refresh_source,
        targets.clone(),
        cookies.as_ref(),
        &mut |event| match event {
            RefreshExecutionEvent::StartingCreator { index, creator, .. } => {
                // Index 0 was already recorded in `initial_progress` before ack.
                if index > 0 {
                    let progress = RefreshProgress {
                        total_creators: Some(total_creators),
                        processed_creators: succeeded_creators + failed_creators,
                        succeeded_creators,
                        failed_creators,
                        current_creator_id: Some(creator.creator.id),
                        current_creator_name: Some(creator.creator.display_name.clone()),
                    };
                    let _ = storage.update_job_progress(
                        lease,
                        JobProgress::Refresh(progress),
                        Timestamp::now(),
                    );
                }
            }
            RefreshExecutionEvent::FinishedCreator {
                index,
                total,
                outcome,
            } => {
                match outcome {
                    CreatorRefreshOutcome::Succeeded(_) => succeeded_creators += 1,
                    CreatorRefreshOutcome::Failed { .. } => failed_creators += 1,
                }
                let next_creator = targets.get(index + 1);
                let progress = RefreshProgress {
                    total_creators: Some(total as u64),
                    processed_creators: succeeded_creators + failed_creators,
                    succeeded_creators,
                    failed_creators,
                    current_creator_id: next_creator.map(|c| c.creator.id),
                    current_creator_name: next_creator.map(|c| c.creator.display_name.clone()),
                };
                let _ = storage.update_job_progress(
                    lease,
                    JobProgress::Refresh(progress),
                    Timestamp::now(),
                );
            }
        },
    );

    let creator_outcomes: Vec<CreatorJobOutcome> = raw_outcomes
        .into_iter()
        .map(|item| match item {
            CreatorRefreshOutcome::Succeeded(committed) => CreatorJobOutcome::Succeeded {
                creator_id: committed.creator_id,
                display_name: committed.display_name,
                new_uploads_inserted: committed.new_uploads_inserted as u64,
                existing_uploads_seen: committed.existing_uploads_seen as u64,
                previous_checkpoint: committed.previous_checkpoint,
                new_checkpoint: committed.new_checkpoint,
            },
            CreatorRefreshOutcome::Failed { creator, error } => CreatorJobOutcome::Failed {
                creator_id: creator.creator.id,
                display_name: creator.creator.display_name,
                error: persist_safe_refresh_error(&error),
            },
        })
        .collect();

    let final_progress = Some(JobProgress::Refresh(RefreshProgress {
        total_creators: Some(total_creators),
        processed_creators: succeeded_creators + failed_creators,
        succeeded_creators,
        failed_creators,
        current_creator_id: None,
        current_creator_name: None,
    }));

    let finished_at = Timestamp::now();
    if failed_creators == 0 {
        let completion = JobCompletion::Succeeded {
            progress: final_progress,
            outcome: JobOutcome::Refresh(RefreshJobOutcome {
                creator_outcomes,
                error: None,
            }),
            finished_at,
        };
        storage.finish_job(lease, completion).is_ok()
    } else {
        let error_summary = format!(
            "{failed_creators} of {total_creators} creator(s) failed during background refresh"
        );
        let completion = JobCompletion::Failed {
            progress: final_progress,
            outcome: JobOutcome::Refresh(RefreshJobOutcome {
                creator_outcomes,
                error: Some(error_summary),
            }),
            finished_at,
        };
        let _ = storage.finish_job(lease, completion);
        false
    }
}

fn execute_download_worker(
    storage: &SqliteStorage,
    db_path: &Path,
    lease: &JobLease,
    owner: &ProcessOwner,
    working_dir: &Path,
    request: &DownloadRequest,
    display_url: Option<DisplayUrl>,
) -> bool {
    let downloader = YtDlpDownloader::new().with_working_dir(working_dir);
    let mut last_persisted: Option<DownloadProgress> = None;
    let mut last_persist_at = Instant::now() - PROGRESS_PERSIST_INTERVAL;
    let mut latest_progress: Option<DownloadProgress> = None;
    let tracking = ProcessTrackerConfig {
        db_path: Some(db_path.to_path_buf()),
        owner_token: lease.owner_token,
        owner: owner.clone(),
        execution_mode: ExecutionMode::Background,
        isolate_process_group: false,
    };

    let download_res = downloader.download_with_tracking(
        request,
        DownloadExecutionOptions::background_capture(),
        Some(&tracking),
        &mut |progress| {
            latest_progress = Some(progress.clone());
            let should_persist = match &last_persisted {
                None => true,
                Some(prev) => {
                    progress.stage != prev.stage
                        || progress.total_bytes != prev.total_bytes
                        || (progress.downloaded_bytes.is_some()
                            && progress.downloaded_bytes == progress.total_bytes
                            && progress != *prev)
                        || (last_persist_at.elapsed() >= PROGRESS_PERSIST_INTERVAL
                            && progress != *prev)
                }
            };
            if should_persist
                && storage
                    .update_job_progress(
                        lease,
                        JobProgress::Download(progress.clone()),
                        Timestamp::now(),
                    )
                    .is_ok()
            {
                last_persisted = Some(progress);
                last_persist_at = Instant::now();
            }
        },
    );

    let final_progress = latest_progress.map(JobProgress::Download);
    let finished_at = Timestamp::now();

    match download_res {
        Ok(outcome) => {
            if !outcome.succeeded {
                let err_msg = match &request.target {
                    DownloadTarget::DirectUrl { .. } => {
                        let shown = display_url
                            .as_ref()
                            .map_or("target URL", DisplayUrl::as_str);
                        format!("[download_unsuccessful] downloader did not succeed for {shown}")
                    }
                    DownloadTarget::PendingUpload { upload } => format!(
                        "[download_unsuccessful] downloader did not succeed for upload {} (status remains {})",
                        upload.id, upload.status
                    ),
                };
                let _ = storage.finish_job(
                    lease,
                    JobCompletion::Failed {
                        progress: final_progress,
                        outcome: JobOutcome::Download(DownloadJobOutcome {
                            confirmed_artifacts: outcome.confirmed_artifacts,
                            marked_downloaded: false,
                            error: Some(err_msg),
                        }),
                        finished_at,
                    },
                );
                return false;
            }

            let marked_downloaded = match &request.target {
                DownloadTarget::PendingUpload { upload } if outcome.should_mark_downloaded() => {
                    match storage.mark_upload_downloaded(upload.id) {
                        Ok(_) => true,
                        Err(err) => {
                            let _ = storage.finish_job(
                                lease,
                                JobCompletion::Failed {
                                    progress: final_progress,
                                    outcome: JobOutcome::Download(DownloadJobOutcome {
                                        confirmed_artifacts: outcome.confirmed_artifacts,
                                        marked_downloaded: false,
                                        error: Some(persist_safe_storage_error(&err)),
                                    }),
                                    finished_at,
                                },
                            );
                            return false;
                        }
                    }
                }
                _ => false,
            };

            storage
                .finish_job(
                    lease,
                    JobCompletion::Succeeded {
                        progress: final_progress,
                        outcome: JobOutcome::Download(DownloadJobOutcome {
                            confirmed_artifacts: outcome.confirmed_artifacts,
                            marked_downloaded,
                            error: None,
                        }),
                        finished_at,
                    },
                )
                .is_ok()
        }
        Err(err) => {
            let msg = persist_safe_download_error(&err);
            let _ = storage.finish_job(
                lease,
                JobCompletion::Failed {
                    progress: final_progress,
                    outcome: JobOutcome::Download(DownloadJobOutcome {
                        confirmed_artifacts: Vec::new(),
                        marked_downloaded: false,
                        error: Some(msg),
                    }),
                    finished_at,
                },
            );
            false
        }
    }
}

fn persist_safe_app_error(err: &AppError) -> String {
    match err {
        AppError::Storage(storage_err) => persist_safe_storage_error(storage_err),
        AppError::Refresh(refresh_err) => persist_safe_refresh_error(refresh_err),
        AppError::Download(download_err) => persist_safe_download_error(download_err),
        AppError::Validation(val_err) => {
            format!("[{}] input validation failed", val_err.error_code())
        }
        AppError::Background(bg) => format!("[{}] background operation failed", bg.error_code()),
        AppError::RefreshPartialFailure {
            succeeded,
            failures,
        } => {
            let total = succeeded + failures.len();
            format!(
                "[refresh_partial_failure] {} of {total} creator(s) failed during refresh",
                failures.len()
            )
        }
    }
}

fn persist_safe_storage_error(err: &StorageError) -> String {
    let code = err.error_code();
    match err {
        StorageError::NotImplemented => {
            format!("[{code}] storage operation is not implemented")
        }
        StorageError::CreatorNotFound { creator_id } => {
            format!("[{code}] creator {creator_id} not found")
        }
        StorageError::CreatorAlreadyFollowed { .. } => {
            format!("[{code}] creator is already followed")
        }
        StorageError::CreatorNotFollowed { creator_id } => {
            format!("[{code}] creator {creator_id} is not followed")
        }
        StorageError::UploadNotFound { upload_id } => {
            format!("[{code}] upload {upload_id} not found")
        }
        StorageError::UploadNotPending { upload_id, status } => {
            format!("[{code}] upload {upload_id} is {status}, expected pending")
        }
        StorageError::PresetNotFound { .. } => format!("[{code}] download preset not found"),
        StorageError::PresetAlreadyExists { .. } => {
            format!("[{code}] download preset already exists")
        }
        StorageError::CannotModifyBuiltinPreset { .. } => {
            format!("[{code}] built-in preset cannot be modified")
        }
        StorageError::StaleRefreshCheckpoint { creator_id } => {
            format!("[{code}] creator {creator_id} refresh checkpoint changed concurrently")
        }
        StorageError::JobNotFound { job_id } => format!("[{code}] job {job_id} not found"),
        StorageError::OperationBusy { active } => format!(
            "[{code}] slot '{}' held by {} pid {}",
            active.slot, active.execution_mode, active.owner.pid
        ),
        StorageError::StaleJobOwnership { job_id, slot } => match (job_id, slot) {
            (Some(id), _) => format!("[{code}] ownership for job {id} is no longer active"),
            (None, Some(slot)) => {
                format!("[{code}] ownership for slot '{slot}' is no longer active")
            }
            (None, None) => format!("[{code}] job ownership is no longer active"),
        },
        StorageError::CorruptedData(_) => {
            format!("[{code}] stored data violated expected schema invariants")
        }
        StorageError::Backend(_) => format!("[{code}] SQLite storage backend error"),
    }
}

fn persist_safe_refresh_error(err: &RefreshError) -> String {
    let code = err.error_code();
    match err {
        RefreshError::NotImplemented => {
            format!("[{code}] refresh backend is not implemented")
        }
        RefreshError::InvalidCreatorUrl { .. } => format!("[{code}] creator URL is invalid"),
        RefreshError::CreatorNotFoundRemote { .. } => {
            format!("[{code}] creator was not found on remote platform")
        }
        RefreshError::CookieSourceInvalid { .. } => {
            format!("[{code}] refresh cookie source is invalid or unreadable")
        }
        RefreshError::Network(_) => {
            format!("[{code}] network request failed while refreshing creator")
        }
        RefreshError::PlatformResponse(_) => {
            format!("[{code}] remote platform returned an unexpected response")
        }
        RefreshError::Storage(storage_err) => persist_safe_storage_error(storage_err),
    }
}

fn persist_safe_download_error(err: &DownloadError) -> String {
    let code = err.error_code();
    match err {
        DownloadError::NotImplemented => {
            format!("[{code}] download backend is not implemented")
        }
        DownloadError::DownloaderUnavailable { .. } => {
            format!("[{code}] downloader executable could not be started")
        }
        DownloadError::ProcessFailed { exit_code, .. } => match exit_code {
            Some(exit) => format!("[{code}] yt-dlp exited with exit code {exit}"),
            None => format!("[{code}] yt-dlp was terminated by signal"),
        },
        DownloadError::CookieSourceInvalid { .. } => {
            format!("[{code}] download cookie source is invalid or unreadable")
        }
        DownloadError::InvalidArguments { .. } => {
            format!("[{code}] download arguments violated validation rules")
        }
        DownloadError::MediaToStdoutDisallowed { .. } => {
            format!("[{code}] streaming media to stdout is disallowed in background mode")
        }
        DownloadError::Io(_) => format!("[{code}] download I/O operation failed"),
        DownloadError::Storage(storage_err) => persist_safe_storage_error(storage_err),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum EncodedPath {
    Utf8(String),
    Native {
        display: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unix_bytes: Option<Vec<u8>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        windows_wide: Option<Vec<u16>>,
    },
}

impl EncodedPath {
    fn from_path(path: &Path) -> Self {
        if let Some(utf8) = path.to_str() {
            return Self::Utf8(utf8.to_owned());
        }
        let display = path.to_string_lossy().into_owned();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::Native {
                display,
                unix_bytes: Some(path.as_os_str().as_bytes().to_vec()),
                windows_wide: None,
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Self::Native {
                display,
                unix_bytes: None,
                windows_wide: Some(path.as_os_str().encode_wide().collect()),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self::Native {
                display,
                unix_bytes: None,
                windows_wide: None,
            }
        }
    }

    fn into_path_buf(self) -> PathBuf {
        match self {
            Self::Utf8(utf8) => PathBuf::from(utf8),
            Self::Native {
                display,
                unix_bytes,
                windows_wide,
            } => {
                let _ = (&unix_bytes, &windows_wide);
                #[cfg(unix)]
                if let Some(bytes) = unix_bytes {
                    use std::os::unix::ffi::OsStringExt;
                    return PathBuf::from(OsString::from_vec(bytes));
                }
                #[cfg(windows)]
                if let Some(wide) = windows_wide {
                    use std::os::windows::ffi::OsStringExt;
                    return PathBuf::from(OsString::from_wide(&wide));
                }
                PathBuf::from(display)
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EncodedCookieSource {
    File { path: EncodedPath },
    BrowserProfile { spec: String },
}

impl EncodedCookieSource {
    fn from_domain(source: &CookieSource) -> Self {
        match source {
            CookieSource::File(path) => Self::File {
                path: EncodedPath::from_path(path),
            },
            CookieSource::BrowserProfile(spec) => Self::BrowserProfile { spec: spec.clone() },
        }
    }

    fn into_domain(self) -> CookieSource {
        match self {
            Self::File { path } => CookieSource::File(path.into_path_buf()),
            Self::BrowserProfile { spec } => CookieSource::BrowserProfile(spec),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RefreshPreferenceUpdate {
    Save { source: EncodedCookieSource },
    Clear,
}

#[derive(Serialize, Deserialize)]
struct EncodedUpload {
    id: i64,
    creator_id: i64,
    platform: String,
    platform_upload_id: String,
    url: String,
    title: String,
    published_at: i64,
    status: String,
}

impl EncodedUpload {
    fn from_domain(upload: &Upload) -> Self {
        Self {
            id: upload.id.as_i64(),
            creator_id: upload.creator_id.as_i64(),
            platform: upload.platform.as_str().to_owned(),
            platform_upload_id: upload.platform_upload_id.clone(),
            url: upload.url.clone(),
            title: upload.title.clone(),
            published_at: upload.published_at.as_unix_seconds(),
            status: upload.status.as_str().to_owned(),
        }
    }

    fn into_domain(self) -> Result<Upload, AppError> {
        Ok(Upload {
            id: UploadId::new(self.id)?,
            creator_id: CreatorId::new(self.creator_id)?,
            platform: self.platform.parse::<Platform>()?,
            platform_upload_id: self.platform_upload_id,
            url: self.url,
            title: self.title,
            published_at: Timestamp::from_unix_seconds(self.published_at),
            status: self.status.parse::<UploadStatus>()?,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum EncodedDownloadTarget {
    DirectUrl { url: String },
    PendingUpload { upload: EncodedUpload },
}

#[derive(Serialize, Deserialize)]
struct EncodedDownloadRequest {
    target: EncodedDownloadTarget,
    preset_name: Option<String>,
    preset_args: Vec<String>,
    extra_args: Vec<String>,
    cookies: Option<EncodedCookieSource>,
}

impl EncodedDownloadRequest {
    fn from_domain(request: &DownloadRequest) -> Self {
        let target = match &request.target {
            DownloadTarget::DirectUrl { url } => {
                EncodedDownloadTarget::DirectUrl { url: url.clone() }
            }
            DownloadTarget::PendingUpload { upload } => EncodedDownloadTarget::PendingUpload {
                upload: EncodedUpload::from_domain(upload),
            },
        };
        Self {
            target,
            preset_name: request.preset_name.as_ref().map(|p| p.as_str().to_owned()),
            preset_args: request.preset_args.clone(),
            extra_args: request.extra_args.clone(),
            cookies: request
                .cookies
                .as_ref()
                .map(EncodedCookieSource::from_domain),
        }
    }

    fn into_domain(self) -> Result<DownloadRequest, AppError> {
        let target = match self.target {
            EncodedDownloadTarget::DirectUrl { url } => DownloadTarget::DirectUrl { url },
            EncodedDownloadTarget::PendingUpload { upload } => DownloadTarget::PendingUpload {
                upload: upload.into_domain()?,
            },
        };
        let preset_name = self.preset_name.map(PresetName::new).transpose()?;
        Ok(DownloadRequest {
            target,
            preset_name,
            preset_args: self.preset_args,
            extra_args: self.extra_args,
            cookies: self.cookies.map(EncodedCookieSource::into_domain),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct WorkerLaunchSpec {
    db_path: EncodedPath,
    working_dir: EncodedPath,
    api_base: Option<String>,
    operation: WorkerOperation,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkerOperation {
    Refresh {
        creator_id: Option<i64>,
        cookies: Option<EncodedCookieSource>,
        preference_update: Option<RefreshPreferenceUpdate>,
    },
    Download {
        request: EncodedDownloadRequest,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WorkerAck {
    Ok { job_id: i64 },
    Err { failure: WorkerStartupFailure },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkerStartupFailure {
    OperationBusy {
        slot_kind: String,
        upload_id: Option<i64>,
        execution_mode: String,
        job_id: Option<i64>,
        owner_token: i64,
        owner_pid: u32,
        owner_process_started_at: Option<String>,
        acquired_at: i64,
    },
    UploadNotPending {
        upload_id: i64,
        status: String,
    },
    UploadNotFound {
        upload_id: i64,
    },
    CreatorNotFound {
        creator_id: i64,
    },
    CreatorNotFollowed {
        creator_id: i64,
    },
    StorageBackend {
        message: String,
    },
    Other {
        message: String,
    },
}

impl WorkerStartupFailure {
    fn from_app_error(err: &AppError) -> Self {
        match err {
            AppError::Storage(StorageError::OperationBusy { active }) => {
                let (slot_kind, upload_id) = match active.slot {
                    OperationSlot::Refresh => ("refresh".to_owned(), None),
                    OperationSlot::UploadDownload(id) => {
                        ("upload_download".to_owned(), Some(id.as_i64()))
                    }
                };
                Self::OperationBusy {
                    slot_kind,
                    upload_id,
                    execution_mode: active.execution_mode.as_str().to_owned(),
                    job_id: active.job_id.map(JobId::as_i64),
                    owner_token: active.owner_token.as_i64(),
                    owner_pid: active.owner.pid,
                    owner_process_started_at: active.owner.process_started_at.clone(),
                    acquired_at: active.acquired_at.as_unix_seconds(),
                }
            }
            AppError::Storage(StorageError::UploadNotPending { upload_id, status }) => {
                Self::UploadNotPending {
                    upload_id: upload_id.as_i64(),
                    status: status.as_str().to_owned(),
                }
            }
            AppError::Storage(StorageError::UploadNotFound { upload_id }) => Self::UploadNotFound {
                upload_id: upload_id.as_i64(),
            },
            AppError::Storage(StorageError::CreatorNotFound { creator_id }) => {
                Self::CreatorNotFound {
                    creator_id: creator_id.as_i64(),
                }
            }
            AppError::Storage(StorageError::CreatorNotFollowed { creator_id }) => {
                Self::CreatorNotFollowed {
                    creator_id: creator_id.as_i64(),
                }
            }
            AppError::Storage(StorageError::Backend(message)) => Self::StorageBackend {
                message: message.clone(),
            },
            AppError::Background(BackgroundError::WorkerStartupFailed { reason }) => Self::Other {
                message: reason.clone(),
            },
            other => Self::Other {
                message: other.to_string(),
            },
        }
    }

    fn into_app_error(self) -> AppError {
        match self {
            Self::OperationBusy {
                slot_kind,
                upload_id,
                execution_mode,
                job_id,
                owner_token,
                owner_pid,
                owner_process_started_at,
                acquired_at,
            } => {
                let slot = match (
                    slot_kind.as_str(),
                    upload_id.and_then(|id| UploadId::new(id).ok()),
                ) {
                    ("refresh", _) => OperationSlot::Refresh,
                    ("upload_download", Some(id)) => OperationSlot::UploadDownload(id),
                    _ => {
                        return BackgroundError::WorkerStartupFailed {
                            reason: "corrupted busy slot in worker acknowledgment".to_owned(),
                        }
                        .into();
                    }
                };
                let Ok(mode) = execution_mode.parse::<ExecutionMode>() else {
                    return BackgroundError::WorkerStartupFailed {
                        reason: "corrupted execution mode in worker acknowledgment".to_owned(),
                    }
                    .into();
                };
                let Ok(token) = OwnershipToken::new(owner_token) else {
                    return BackgroundError::WorkerStartupFailed {
                        reason: "corrupted ownership token in worker acknowledgment".to_owned(),
                    }
                    .into();
                };
                let Ok(owner) = ProcessOwner::new(owner_pid, owner_process_started_at) else {
                    return BackgroundError::WorkerStartupFailed {
                        reason: "corrupted process owner in worker acknowledgment".to_owned(),
                    }
                    .into();
                };
                let job_id = job_id.and_then(|id| JobId::new(id).ok());
                StorageError::OperationBusy {
                    active: Box::new(ActiveOperation {
                        slot,
                        execution_mode: mode,
                        job_id,
                        owner_token: token,
                        owner,
                        acquired_at: Timestamp::from_unix_seconds(acquired_at),
                    }),
                }
                .into()
            }
            Self::UploadNotPending { upload_id, status } => {
                match (UploadId::new(upload_id), status.parse::<UploadStatus>()) {
                    (Ok(upload_id), Ok(status)) => {
                        StorageError::UploadNotPending { upload_id, status }.into()
                    }
                    _ => BackgroundError::WorkerStartupFailed {
                        reason: format!("upload {upload_id} is {status}"),
                    }
                    .into(),
                }
            }
            Self::UploadNotFound { upload_id } => match UploadId::new(upload_id) {
                Ok(upload_id) => StorageError::UploadNotFound { upload_id }.into(),
                Err(err) => err.into(),
            },
            Self::CreatorNotFound { creator_id } => match CreatorId::new(creator_id) {
                Ok(creator_id) => StorageError::CreatorNotFound { creator_id }.into(),
                Err(err) => err.into(),
            },
            Self::CreatorNotFollowed { creator_id } => match CreatorId::new(creator_id) {
                Ok(creator_id) => StorageError::CreatorNotFollowed { creator_id }.into(),
                Err(err) => err.into(),
            },
            Self::StorageBackend { message } => StorageError::Backend(message).into(),
            Self::Other { message } => {
                BackgroundError::WorkerStartupFailed { reason: message }.into()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SplitChunkReader {
        chunks: Vec<Vec<u8>>,
        index: usize,
    }

    impl SplitChunkReader {
        fn new(chunks: Vec<&[u8]>) -> Self {
            Self {
                chunks: chunks.into_iter().map(|c| c.to_vec()).collect(),
                index: 0,
            }
        }
    }

    impl io::Read for SplitChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            if available.is_empty() {
                return Ok(0);
            }
            let n = available.len().min(buf.len());
            buf[..n].copy_from_slice(&available[..n]);
            self.consume(n);
            Ok(n)
        }
    }

    impl BufRead for SplitChunkReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            while self.index < self.chunks.len() && self.chunks[self.index].is_empty() {
                self.index += 1;
            }
            Ok(self
                .chunks
                .get(self.index)
                .map(Vec::as_slice)
                .unwrap_or(&[]))
        }

        fn consume(&mut self, amt: usize) {
            if let Some(chunk) = self.chunks.get_mut(self.index) {
                chunk.drain(..amt.min(chunk.len()));
            }
        }
    }

    #[test]
    fn persisted_errors_emit_safe_classifications_without_arbitrary_diagnostics() {
        let dl_err = DownloadError::ProcessFailed {
            exit_code: Some(1),
            stderr: "ERROR: https://other.secret/stream?sig=leak99 Cookie: SESSDATA=secret_hdr --cookies /tmp/secret/cookies.txt --proxy http://u:p99@proxy:8080".to_owned(),
        };
        let dl_safe = persist_safe_download_error(&dl_err);
        assert_eq!(
            dl_safe,
            "[download_process_failed] yt-dlp exited with exit code 1"
        );
        for leak in ["other.secret", "sig=leak99", "SESSDATA", "mycookies", "p99"] {
            assert!(!dl_safe.contains(leak));
        }

        let cookie_dl = DownloadError::CookieSourceInvalid {
            cookie_source: "cookie file '/tmp/secret/mycookies.txt'".to_owned(),
            reason: "permission denied for secret".to_owned(),
        };
        assert_eq!(
            persist_safe_download_error(&cookie_dl),
            "[cookie_source_invalid] download cookie source is invalid or unreadable"
        );

        let refresh_net = RefreshError::Network(
            "request https://api.secret/x?tok=refresh_secret failed with Cookie: SESSDATA=foo"
                .to_owned(),
        );
        assert_eq!(
            persist_safe_refresh_error(&refresh_net),
            "[refresh_network_error] network request failed while refreshing creator"
        );
        let refresh_resp = RefreshError::PlatformResponse(
            "unexpected payload containing Cookie: bar=baz".to_owned(),
        );
        assert_eq!(
            persist_safe_refresh_error(&refresh_resp),
            "[refresh_platform_error] remote platform returned an unexpected response"
        );

        let upload_not_pending = StorageError::UploadNotPending {
            upload_id: UploadId::new(7).unwrap(),
            status: UploadStatus::Downloaded,
        };
        assert_eq!(
            persist_safe_storage_error(&upload_not_pending),
            "[upload_not_pending] upload 7 is downloaded, expected pending"
        );
    }

    #[test]
    fn read_bounded_line_decodes_multibyte_utf8_split_across_chunks() {
        // 4-byte emoji '🦀' (`\xF0\x9F\xA6\x80`) + 3-byte '中' (`\xE4\xB8\xAD`) split mid-codepoint.
        let full = b"{\"status\":\"ok\",\"note\":\"\xF0\x9F\xA6\x80\xE4\xB8\xAD\"}\n";
        let split_at = full.iter().position(|&b| b == 0xF0).unwrap() + 2;
        let split_at_2 = full.iter().position(|&b| b == 0xE4).unwrap() + 1;
        let mut reader = SplitChunkReader::new(vec![
            &full[..split_at],
            &full[split_at..split_at_2],
            &full[split_at_2..],
        ]);
        let decoded = read_bounded_line(&mut reader, 1024).unwrap().unwrap();
        assert_eq!(decoded, std::str::from_utf8(full).unwrap());

        // Invalid complete frame is still rejected.
        let mut bad_reader = SplitChunkReader::new(vec![b"{\"bad\":", b"\xFF\n"]);
        assert!(read_bounded_line(&mut bad_reader, 1024).is_err());

        // Oversized line is rejected.
        let mut big_reader = SplitChunkReader::new(vec![b"0123456789\n"]);
        assert!(read_bounded_line(&mut big_reader, 5).is_err());
    }

    #[test]
    fn encoded_path_roundtrips_non_utf8_on_unix_and_windows() {
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(OsString::from_vec(
                b"/tmp/siphon_\xff_\xfe/db.sqlite".to_vec(),
            ))
        };
        #[cfg(windows)]
        let path = {
            use std::os::windows::ffi::OsStringExt;
            PathBuf::from(OsString::from_wide(&[
                b'C' as u16,
                b':' as u16,
                b'\\' as u16,
                0xD800,
                b'.' as u16,
                b'd' as u16,
                b'b' as u16,
            ]))
        };
        #[cfg(not(any(unix, windows)))]
        let path = PathBuf::from("/tmp/siphon/db.sqlite");

        let json = serde_json::to_string(&EncodedPath::from_path(&path)).unwrap();
        let decoded: EncodedPath = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.into_path_buf(), path);
    }
}
