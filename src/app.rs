//! Command dispatch wiring connecting the CLI to storage, refresh, and download services.

use crate::background::{
    BackgroundRuntime, ProcessTrackerConfig, acquire_reservation_with_reconcile,
    query_job_status_with_reconcile, submit_background_download, submit_background_refresh,
};
use crate::cli::{Cli, Command, CreatorCommand, JobCommand, PresetCommand, UploadCommand};
use crate::domain::{Timestamp, UploadStatus};
use crate::download::{
    DownloadExecutionOptions, DownloadTarget, execute_resolved_download_with_tracking,
    resolve_download_request,
};
use crate::error::{AppError, BackgroundError, DownloadError, StorageError};
use crate::jobs::{ExecutionMode, OperationSlot};
use crate::output::{CommandResult, HumanRenderOptions};
use crate::refresh::{follow_creator, refresh_creators};
use crate::storage::Storage;

pub use crate::output::CommandOutput;

/// Borrowed service dependencies supplied to execute a parsed Siphon CLI command.
pub struct AppServices<'a> {
    /// Local persistence backend.
    pub storage: &'a dyn Storage,
    /// Platform feed/creator resolver backend.
    pub refresh_source: &'a dyn crate::refresh::RefreshSource,
    /// Media downloader backend.
    pub downloader: &'a dyn crate::download::Downloader,
}

/// Execution options controlling downloader stream behavior and output assumptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AppExecuteOptions {
    /// Stream-routing and media-to-stdout options passed to [`execute_download_with_options`].
    pub download: DownloadExecutionOptions,
}

/// Typed result of executing a CLI command, preserving partial-failure context such as full
/// per-creator refresh outcomes alongside any nonzero [`AppError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandExecution {
    /// Structured command outcome, present on success and on partial refresh failure.
    pub result: Option<CommandResult>,
    /// Failure to report (and exit nonzero for), if any.
    pub error: Option<AppError>,
}

impl CommandExecution {
    fn ok(result: CommandResult) -> Self {
        Self {
            result: Some(result),
            error: None,
        }
    }

    fn err(error: AppError) -> Self {
        Self {
            result: None,
            error: Some(error),
        }
    }

    fn partial(result: CommandResult, error: AppError) -> Self {
        Self {
            result: Some(result),
            error: Some(error),
        }
    }
}

/// Dispatches a parsed [`Cli`] command against the provided [`AppServices`] and renders plain
/// human-readable output (TSV/plain text on stdout/stderr).
pub fn execute(cli: Cli, services: &AppServices<'_>) -> Result<CommandOutput, AppError> {
    let result = execute_typed(cli, services)?;
    Ok(result.render_human(HumanRenderOptions::plain()))
}

/// Dispatches a parsed [`Cli`] command and returns its typed [`CommandResult`] or [`AppError`].
pub fn execute_typed(cli: Cli, services: &AppServices<'_>) -> Result<CommandResult, AppError> {
    let execution = execute_with_options(cli, services, AppExecuteOptions::default());
    match execution.error {
        Some(err) => Err(err),
        None => Ok(execution
            .result
            .expect("successful execution always produces a CommandResult")),
    }
}

/// Dispatches a parsed [`Cli`] command with explicit [`AppExecuteOptions`], preserving partial
/// results (such as all per-creator refresh outcomes) even when returning a nonzero error.
pub fn execute_with_options(
    cli: Cli,
    services: &AppServices<'_>,
    options: AppExecuteOptions,
) -> CommandExecution {
    execute_with_runtime(cli, services, options, &BackgroundRuntime::default())
}

/// Dispatches a parsed [`Cli`] command with explicit [`AppExecuteOptions`] and [`BackgroundRuntime`].
pub fn execute_with_runtime(
    cli: Cli,
    services: &AppServices<'_>,
    options: AppExecuteOptions,
    background: &BackgroundRuntime,
) -> CommandExecution {
    match cli.command {
        Command::Creator(args) => match execute_creator(args.command, services) {
            Ok(res) => CommandExecution::ok(res),
            Err(err) => CommandExecution::err(err),
        },
        Command::Refresh(args) => execute_refresh(args, services, background),
        Command::Upload(args) => match execute_upload(args.command, services.storage) {
            Ok(res) => CommandExecution::ok(res),
            Err(err) => CommandExecution::err(err),
        },
        Command::Download(args) => {
            match execute_download_command(args, services, options.download, background) {
                Ok(res) => CommandExecution::ok(res),
                Err(err) => CommandExecution::err(err),
            }
        }
        Command::Preset(args) => match execute_preset(args.command, services.storage) {
            Ok(res) => CommandExecution::ok(res),
            Err(err) => CommandExecution::err(err),
        },
        Command::Job(args) => match execute_job(args.command, services.storage) {
            Ok(res) => CommandExecution::ok(res),
            Err(err) => CommandExecution::err(err),
        },
        Command::InternalWorker => CommandExecution::err(
            BackgroundError::WorkerStartupFailed {
                reason: "internal worker command cannot be dispatched through app::execute"
                    .to_owned(),
            }
            .into(),
        ),
    }
}

fn execute_creator(
    command: CreatorCommand,
    services: &AppServices<'_>,
) -> Result<CommandResult, AppError> {
    match command {
        CreatorCommand::Add { url } => {
            let followed = follow_creator(
                services.storage,
                services.refresh_source,
                &url,
                Timestamp::now(),
            )?;
            Ok(CommandResult::CreatorAdded(followed))
        }
        CreatorCommand::List => {
            let creators = services.storage.list_followed_creators()?;
            Ok(CommandResult::CreatorsListed(creators))
        }
        CreatorCommand::Remove { id } => {
            let creator = services.storage.unfollow_creator(id)?;
            Ok(CommandResult::CreatorRemoved(creator))
        }
    }
}

fn execute_refresh(
    args: crate::cli::RefreshArgs,
    services: &AppServices<'_>,
    background: &BackgroundRuntime,
) -> CommandExecution {
    if args.background {
        return match submit_background_refresh(services.storage, args, background) {
            Ok(job) => CommandExecution::ok(CommandResult::JobStarted(job)),
            Err(err) => CommandExecution::err(err),
        };
    }

    let policy = match args.cookie_policy() {
        Ok(policy) => policy,
        Err(err) => return CommandExecution::err(err.into()),
    };
    let _reservation_guard = match services.storage.job_store() {
        Some(job_store) => match acquire_reservation_with_reconcile(
            job_store,
            services.storage.database_path(),
            OperationSlot::Refresh,
        ) {
            Ok(guard) => Some(guard),
            Err(err) => return CommandExecution::err(err),
        },
        None => None,
    };

    let summary = match refresh_creators(
        services.storage,
        services.refresh_source,
        args.creator_id,
        policy,
    ) {
        Ok(summary) => summary,
        Err(err) => return CommandExecution::err(err),
    };
    match summary.check_failures() {
        Ok(()) => CommandExecution::ok(CommandResult::Refreshed(summary)),
        Err(err) => CommandExecution::partial(CommandResult::Refreshed(summary), err),
    }
}

fn execute_upload(
    command: UploadCommand,
    storage: &dyn Storage,
) -> Result<CommandResult, AppError> {
    match command {
        UploadCommand::List => {
            let uploads = storage.list_pending_uploads()?;
            Ok(CommandResult::UploadsListed(uploads))
        }
        UploadCommand::Ignore { id } => {
            let upload = storage.mark_upload_ignored(id)?;
            Ok(CommandResult::UploadIgnored(upload))
        }
    }
}

fn execute_download_command(
    args: crate::cli::DownloadArgs,
    services: &AppServices<'_>,
    download_options: DownloadExecutionOptions,
    background: &BackgroundRuntime,
) -> Result<CommandResult, AppError> {
    if args.background {
        let job = submit_background_download(services.storage, args, background)?;
        return Ok(CommandResult::JobStarted(job));
    }

    let plan = args.to_plan_input()?;
    let mut request = resolve_download_request(services.storage, plan)?;
    request.validate()?;

    let db_path = services.storage.database_path();
    let (_reservation_guard, tracking) = match (&request.target, services.storage.job_store()) {
        (DownloadTarget::PendingUpload { upload }, Some(job_store)) => {
            let guard = acquire_reservation_with_reconcile(
                job_store,
                db_path,
                OperationSlot::UploadDownload(upload.id),
            )?;
            let current = services.storage.get_upload(upload.id)?;
            if current.status != UploadStatus::Pending {
                return Err(StorageError::UploadNotPending {
                    upload_id: upload.id,
                    status: current.status,
                }
                .into());
            }
            let tracker = ProcessTrackerConfig {
                db_path: db_path.map(std::path::Path::to_path_buf),
                owner_token: guard.owner_token(),
                owner: guard.owner().clone(),
                execution_mode: ExecutionMode::Foreground,
                isolate_process_group: true,
            };
            request.target = DownloadTarget::PendingUpload { upload: current };
            request.validate()?;
            (Some(guard), Some(tracker))
        }
        _ => (None, None),
    };

    let report = execute_resolved_download_with_tracking(
        services.storage,
        services.downloader,
        request,
        download_options,
        tracking.as_ref(),
    )?;
    if !report.outcome.succeeded {
        let message = match &report.request.target {
            DownloadTarget::DirectUrl { url } => {
                format!("downloader reported an unsuccessful outcome for {url}")
            }
            DownloadTarget::PendingUpload { upload } => {
                format!(
                    "downloader reported an unsuccessful outcome for upload {} ({}); status remains {}",
                    upload.id, upload.title, upload.status
                )
            }
        };
        return Err(AppError::Download(DownloadError::ProcessFailed {
            exit_code: Some(1),
            stderr: message,
        }));
    }

    Ok(CommandResult::Downloaded(report))
}

fn execute_job(command: JobCommand, storage: &dyn Storage) -> Result<CommandResult, AppError> {
    match command {
        JobCommand::Status { id } => {
            let job_store = storage.job_store().ok_or(StorageError::NotImplemented)?;
            let job = query_job_status_with_reconcile(job_store, storage.database_path(), id)?;
            Ok(CommandResult::JobStatus(job))
        }
    }
}

fn execute_preset(
    command: PresetCommand,
    storage: &dyn Storage,
) -> Result<CommandResult, AppError> {
    match command {
        PresetCommand::Add { name, args } => {
            name.ensure_custom()?;
            let preset = storage.add_preset(name, args)?;
            Ok(CommandResult::PresetAdded(preset))
        }
        PresetCommand::List => {
            let presets = storage.list_presets()?;
            Ok(CommandResult::PresetsListed(presets))
        }
        PresetCommand::Remove { name } => {
            name.ensure_custom()?;
            let outcome = storage.remove_preset(&name)?;
            Ok(CommandResult::PresetRemoved(outcome))
        }
        PresetCommand::SetDefault { name } => {
            let preset = storage.set_default_preset(&name)?;
            Ok(CommandResult::PresetDefaultSet(preset))
        }
    }
}
