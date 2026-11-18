//! Command dispatch wiring connecting the CLI to storage, refresh, and download services.

use std::fmt::Write as _;

use crate::cli::{Cli, Command, CreatorCommand, PresetCommand, UploadCommand};
use crate::domain::Timestamp;
use crate::download::execute_download;
use crate::error::AppError;
use crate::refresh::{CreatorRefreshOutcome, RefreshSummary, follow_creator, refresh_creators};
use crate::storage::Storage;

/// Borrowed service dependencies supplied to execute a parsed Siphon CLI command.
pub struct AppServices<'a> {
    /// Local persistence backend.
    pub storage: &'a dyn Storage,
    /// Platform feed/creator resolver backend.
    pub refresh_source: &'a dyn crate::refresh::RefreshSource,
    /// Media downloader backend.
    pub downloader: &'a dyn crate::download::Downloader,
}

/// Formatted user-facing output produced by a completed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Text printed to standard output.
    pub stdout: String,
}

/// Dispatches a parsed [`Cli`] command against the provided [`AppServices`].
pub fn execute(cli: Cli, services: &AppServices<'_>) -> Result<CommandOutput, AppError> {
    match cli.command {
        Command::Creator(args) => execute_creator(args.command, services),
        Command::Refresh(args) => execute_refresh(args, services),
        Command::Upload(args) => execute_upload(args.command, services.storage),
        Command::Download(args) => execute_download_command(args, services),
        Command::Preset(args) => execute_preset(args.command, services.storage),
    }
}

fn execute_creator(
    command: CreatorCommand,
    services: &AppServices<'_>,
) -> Result<CommandOutput, AppError> {
    match command {
        CreatorCommand::Add { url } => {
            let followed = follow_creator(
                services.storage,
                services.refresh_source,
                &url,
                Timestamp::now(),
            )?;
            Ok(CommandOutput {
                stdout: format!(
                    "Followed creator {} ({}) [{}] starting at {}\n",
                    followed.creator.id,
                    followed.creator.display_name,
                    followed.creator.platform_creator_id,
                    followed.follow.refresh_checkpoint
                ),
            })
        }
        CreatorCommand::List => {
            let creators = services.storage.list_followed_creators()?;
            if creators.is_empty() {
                return Ok(CommandOutput {
                    stdout: "No followed creators.\n".to_owned(),
                });
            }
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
            Ok(CommandOutput { stdout: out })
        }
        CreatorCommand::Remove { id } => {
            let creator = services.storage.unfollow_creator(id)?;
            Ok(CommandOutput {
                stdout: format!(
                    "Unfollowed creator {} ({}); existing uploads preserved\n",
                    creator.id, creator.display_name
                ),
            })
        }
    }
}

fn execute_refresh(
    args: crate::cli::RefreshArgs,
    services: &AppServices<'_>,
) -> Result<CommandOutput, AppError> {
    let policy = args.cookie_policy()?;
    let summary = refresh_creators(
        services.storage,
        services.refresh_source,
        args.creator_id,
        policy,
    )?;
    summary.check_failures()?;
    Ok(CommandOutput {
        stdout: format_refresh_summary(&summary),
    })
}

fn format_refresh_summary(summary: &RefreshSummary) -> String {
    let mut out = String::new();
    if summary.cleared_default_cookies {
        out.push_str("Cleared remembered default refresh cookie source.\n");
    }
    if summary.saved_default_cookies
        && let Some(cookies) = &summary.used_cookies
    {
        let _ = writeln!(out, "Saved default refresh cookie source: {cookies}.");
    }
    if summary.outcomes.is_empty() {
        out.push_str("No followed creators to refresh.\n");
        return out;
    }
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
    out
}

fn execute_upload(
    command: UploadCommand,
    storage: &dyn Storage,
) -> Result<CommandOutput, AppError> {
    match command {
        UploadCommand::List => {
            let uploads = storage.list_pending_uploads()?;
            if uploads.is_empty() {
                return Ok(CommandOutput {
                    stdout: "No pending uploads.\n".to_owned(),
                });
            }
            let mut out = String::new();
            for upload in uploads {
                let _ = writeln!(
                    out,
                    "{}\tcreator={}\t{}\t{}\t{}",
                    upload.id, upload.creator_id, upload.published_at, upload.title, upload.url
                );
            }
            Ok(CommandOutput { stdout: out })
        }
        UploadCommand::Ignore { id } => {
            let upload = storage.mark_upload_ignored(id)?;
            Ok(CommandOutput {
                stdout: format!("Ignored upload {} ({})\n", upload.id, upload.title),
            })
        }
    }
}

fn execute_download_command(
    args: crate::cli::DownloadArgs,
    services: &AppServices<'_>,
) -> Result<CommandOutput, AppError> {
    let plan = args.to_plan_input()?;
    let report = execute_download(services.storage, services.downloader, plan)?;
    let status_note = match &report.request.target {
        crate::download::DownloadTarget::DirectUrl { url } => {
            format!("Downloaded direct URL {url}")
        }
        crate::download::DownloadTarget::PendingUpload { upload } => {
            if report.marked_downloaded {
                format!(
                    "Downloaded upload {} ({}) and marked as downloaded ({} artifact(s) confirmed)",
                    upload.id,
                    upload.title,
                    report.outcome.confirmed_artifacts.len()
                )
            } else {
                format!(
                    "Finished upload {} ({}) without confirmed output artifact; status remains pending",
                    upload.id, upload.title
                )
            }
        }
    };
    Ok(CommandOutput {
        stdout: format!("{status_note}\n"),
    })
}

fn execute_preset(
    command: PresetCommand,
    storage: &dyn Storage,
) -> Result<CommandOutput, AppError> {
    match command {
        PresetCommand::Add { name, args } => {
            name.ensure_custom()?;
            let preset = storage.add_preset(name, args)?;
            Ok(CommandOutput {
                stdout: format!(
                    "Saved preset '{}' with {} argument(s)\n",
                    preset.name,
                    preset.args.len()
                ),
            })
        }
        PresetCommand::List => {
            let presets = storage.list_presets()?;
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
            Ok(CommandOutput { stdout: out })
        }
        PresetCommand::Remove { name } => {
            name.ensure_custom()?;
            let outcome = storage.remove_preset(&name)?;
            let mut msg = format!("Removed preset '{}'", outcome.removed);
            if outcome.reset_default_to_builtin {
                msg.push_str("; default preset reset to 'video'");
            }
            msg.push('\n');
            Ok(CommandOutput { stdout: msg })
        }
        PresetCommand::SetDefault { name } => {
            let preset = storage.set_default_preset(&name)?;
            Ok(CommandOutput {
                stdout: format!("Default preset set to '{}'\n", preset.name),
            })
        }
    }
}
