//! Command dispatch wiring connecting the CLI to storage, refresh, and download services.

use crate::cli::{Cli, Command, CreatorCommand, PresetCommand, UploadCommand};
use crate::domain::Timestamp;
use crate::download::{DownloadExecutionOptions, execute_download_with_options};
use crate::error::{AppError, DownloadError};
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
    match cli.command {
        Command::Creator(args) => match execute_creator(args.command, services) {
            Ok(res) => CommandExecution::ok(res),
            Err(err) => CommandExecution::err(err),
        },
        Command::Refresh(args) => execute_refresh(args, services),
        Command::Upload(args) => match execute_upload(args.command, services.storage) {
            Ok(res) => CommandExecution::ok(res),
            Err(err) => CommandExecution::err(err),
        },
        Command::Download(args) => {
            match execute_download_command(args, services, options.download) {
                Ok(res) => CommandExecution::ok(res),
                Err(err) => CommandExecution::err(err),
            }
        }
        Command::Preset(args) => match execute_preset(args.command, services.storage) {
            Ok(res) => CommandExecution::ok(res),
            Err(err) => CommandExecution::err(err),
        },
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

fn execute_refresh(args: crate::cli::RefreshArgs, services: &AppServices<'_>) -> CommandExecution {
    let policy = match args.cookie_policy() {
        Ok(policy) => policy,
        Err(err) => return CommandExecution::err(err.into()),
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
) -> Result<CommandResult, AppError> {
    let plan = args.to_plan_input()?;
    let report = execute_download_with_options(
        services.storage,
        services.downloader,
        plan,
        download_options,
    )?;
    if !report.outcome.succeeded {
        let message = match &report.request.target {
            crate::download::DownloadTarget::DirectUrl { url } => {
                format!("downloader reported an unsuccessful outcome for {url}")
            }
            crate::download::DownloadTarget::PendingUpload { upload } => {
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
