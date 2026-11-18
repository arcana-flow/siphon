//! Command-line argument definitions (`clap`) matching the Siphon CLI specification.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::domain::{CookieSource, CreatorId, NonEmptyString, PresetName, UploadId};
use crate::download::{DownloadPlanInput, DownloadTargetInput, PresetSelection};
use crate::error::ValidationError;
use crate::refresh::RefreshCookiePolicy;

/// Top-level CLI parser for `siphon`.
#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(
    name = "siphon",
    version,
    about = "Follow Bilibili creators, track new uploads, and download media via yt-dlp",
    long_about = "Siphon tracks followed Bilibili creators, records newly discovered uploads in a local SQLite database, and downloads media or pending uploads using yt-dlp and reusable presets.",
    arg_required_else_help = true,
    propagate_version = true
)]
pub struct Cli {
    /// Path to the local SQLite database file.
    #[arg(
        long,
        global = true,
        env = "SIPHON_DB",
        value_name = "PATH",
        value_parser = parse_non_empty_path
    )]
    pub db: Option<PathBuf>,

    /// Primary Siphon subcommand.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level Siphon subcommands.
#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Follow, list, and unfollow Bilibili creators.
    Creator(CreatorCommandArgs),

    /// Refresh one creator or all followed creators for new uploads.
    Refresh(RefreshArgs),

    /// Inspect or ignore tracked creator uploads.
    Upload(UploadCommandArgs),

    /// Download media from a URL or a pending upload via yt-dlp.
    Download(DownloadArgs),

    /// Manage reusable yt-dlp download presets and the active default preset.
    Preset(PresetCommandArgs),
}

/// Container for `siphon creator` subcommands.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub struct CreatorCommandArgs {
    /// Creator management action.
    #[command(subcommand)]
    pub command: CreatorCommand,
}

/// Subcommands under `siphon creator`.
#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum CreatorCommand {
    /// Follow a Bilibili creator starting from the current time.
    Add {
        /// Creator profile URL.
        #[arg(
            value_name = "URL",
            value_parser = parse_non_empty_url,
            help = "Creator profile URL (for example, https://space.bilibili.com/<uid>)"
        )]
        url: String,
    },

    /// List currently followed creators and their refresh checkpoints.
    List,

    /// Unfollow a creator while preserving previously discovered uploads.
    Remove {
        /// Local creator ID to unfollow.
        #[arg(value_name = "ID")]
        id: CreatorId,
    },
}

/// Arguments for `siphon refresh [CREATOR_ID]`.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(
    long_about = "Refresh one followed creator or all followed creators independently.\n\nEach creator fetches pages back to its previous refresh checkpoint and commits discovered uploads plus the advanced checkpoint in one transaction. A failure on one creator leaves its checkpoint unchanged and allows remaining creators to finish."
)]
pub struct RefreshArgs {
    /// Optional local creator ID to refresh; refreshes all followed creators when omitted.
    #[arg(value_name = "CREATOR_ID")]
    pub creator_id: Option<CreatorId>,

    /// Cookie source override for this refresh.
    #[command(flatten)]
    pub cookies: CookieArgs,

    /// Save the supplied cookie source as the default for future refreshes.
    #[arg(
        long,
        requires = "cookie_source",
        conflicts_with_all = ["clear_cookies", "no_cookies"]
    )]
    pub save_cookies: bool,

    /// Clear any remembered default cookie source before refreshing.
    #[arg(
        long,
        conflicts_with_all = ["cookie_source", "save_cookies", "no_cookies"]
    )]
    pub clear_cookies: bool,

    /// Ignore any remembered default cookie source for this invocation only.
    #[arg(
        long,
        conflicts_with_all = ["cookie_source", "save_cookies", "clear_cookies"]
    )]
    pub no_cookies: bool,
}

impl RefreshArgs {
    /// Converts parsed CLI cookie flags into a [`RefreshCookiePolicy`].
    pub fn cookie_policy(&self) -> Result<RefreshCookiePolicy, ValidationError> {
        if self.clear_cookies {
            return Ok(RefreshCookiePolicy::ClearSavedDefault);
        }
        if self.no_cookies {
            return Ok(RefreshCookiePolicy::IgnoreSavedDefault);
        }
        match self.cookies.to_cookie_source()? {
            Some(source) => Ok(RefreshCookiePolicy::Explicit {
                source,
                save_as_default: self.save_cookies,
            }),
            None => Ok(RefreshCookiePolicy::UseSavedDefault),
        }
    }
}

/// Container for `siphon upload` subcommands.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub struct UploadCommandArgs {
    /// Upload management action.
    #[command(subcommand)]
    pub command: UploadCommand,
}

/// Subcommands under `siphon upload`.
#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum UploadCommand {
    /// List pending uploads awaiting download or ignore.
    List,

    /// Mark a pending upload as ignored.
    Ignore {
        /// Local upload ID to ignore.
        #[arg(value_name = "ID")]
        id: UploadId,
    },
}

/// Arguments for `siphon download <URL>` / `siphon download --upload <ID>`.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(
    long_about = "Download media directly from a URL or from a pending tracked upload via yt-dlp.\n\nPreset arguments are passed to yt-dlp first, followed by any one-off arguments supplied after `--` in exact order. Use `--no-preset` before `--` to omit preset arguments.\n\nDirect URL downloads follow yt-dlp playlist behavior and leave tracked upload statuses unchanged. `--upload <ID>` restricts the download to that single upload and marks it as downloaded only if yt-dlp succeeds and an output artifact is confirmed."
)]
pub struct DownloadArgs {
    /// Target URL or pending upload ID (mutually exclusive, one required).
    #[command(flatten)]
    pub target: DownloadTargetArgs,

    /// Select a named download preset for this invocation instead of the default preset.
    #[arg(long, value_name = "NAME", conflicts_with = "no_preset")]
    pub preset: Option<PresetName>,

    /// Omit all preset arguments for this invocation.
    #[arg(long, conflicts_with = "preset")]
    pub no_preset: bool,

    /// Cookie source passed to yt-dlp for this download invocation only.
    #[command(flatten)]
    pub cookies: CookieArgs,

    /// One-off arguments passed to yt-dlp after `--` (appended after preset arguments).
    #[arg(last = true, allow_hyphen_values = true, value_name = "YT_DLP_ARGS")]
    pub extra_args: Vec<String>,
}

impl DownloadArgs {
    /// Converts parsed CLI arguments into an unresolved [`DownloadPlanInput`].
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] if cookie flags contain invalid values.
    pub fn to_plan_input(&self) -> Result<DownloadPlanInput, ValidationError> {
        let target = match (&self.target.url, self.target.upload) {
            (Some(url), None) => DownloadTargetInput::Url(url.clone()),
            (None, Some(upload_id)) => DownloadTargetInput::Upload(upload_id),
            // Enforced by `DownloadTargetArgs`'s required mutually-exclusive group.
            _ => {
                return Err(ValidationError::EmptyValue {
                    field: "download target",
                });
            }
        };

        let preset = if self.no_preset {
            PresetSelection::None
        } else if let Some(name) = &self.preset {
            PresetSelection::Named(name.clone())
        } else {
            PresetSelection::Default
        };

        Ok(DownloadPlanInput {
            target,
            preset,
            cookies: self.cookies.to_cookie_source()?,
            extra_args: self.extra_args.clone(),
        })
    }
}

/// Mutually exclusive target selection for `siphon download`.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[group(id = "download_target", required = true, multiple = false)]
pub struct DownloadTargetArgs {
    /// Direct media URL to download without following its creator.
    #[arg(value_name = "URL", value_parser = parse_non_empty_url)]
    pub url: Option<String>,

    /// Local ID of a pending upload to download and mark as downloaded on success.
    #[arg(long, value_name = "ID")]
    pub upload: Option<UploadId>,
}

/// Mutually exclusive cookie selection options shared by `refresh` and `download`.
#[derive(Debug, Clone, Args, PartialEq, Eq, Default)]
#[group(id = "cookie_source", multiple = false)]
pub struct CookieArgs {
    /// Path to a Netscape-format cookie file.
    #[arg(long, value_name = "FILE", value_parser = parse_non_empty_path)]
    pub cookies: Option<PathBuf>,

    /// Browser profile selector compatible with `yt-dlp --cookies-from-browser`.
    #[arg(long, value_name = "BROWSER", value_parser = parse_non_empty_browser)]
    pub cookies_from_browser: Option<String>,
}

impl CookieArgs {
    /// Converts the parsed cookie flags into an optional validated [`CookieSource`].
    pub fn to_cookie_source(&self) -> Result<Option<CookieSource>, ValidationError> {
        match (&self.cookies, &self.cookies_from_browser) {
            (Some(path), None) => Ok(Some(CookieSource::from_file(path.clone())?)),
            (None, Some(spec)) => Ok(Some(CookieSource::from_browser_profile(spec)?)),
            (None, None) => Ok(None),
            (Some(_), Some(_)) => Err(ValidationError::InvalidCookiePath {
                reason: "--cookies and --cookies-from-browser cannot be used together",
            }),
        }
    }
}

/// Container for `siphon preset` subcommands.
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(subcommand_required = true, arg_required_else_help = true)]
pub struct PresetCommandArgs {
    /// Preset management action.
    #[command(subcommand)]
    pub command: PresetCommand,
}

/// Subcommands under `siphon preset`.
#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum PresetCommand {
    /// Save a named download preset (`video` is reserved for the built-in preset).
    Add {
        /// Unique preset name.
        #[arg(value_name = "NAME")]
        name: PresetName,

        /// Ordered yt-dlp arguments saved in this preset (supply after `--`).
        #[arg(
            last = true,
            required = true,
            num_args = 1..,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },

    /// List available download presets and indicate the active default.
    List,

    /// Remove a custom download preset.
    Remove {
        /// Preset name to remove.
        #[arg(value_name = "NAME")]
        name: PresetName,
    },

    /// Set the default download preset used when `--preset` and `--no-preset` are omitted.
    SetDefault {
        /// Preset name to select as the default (may be `video`).
        #[arg(value_name = "NAME")]
        name: PresetName,
    },
}

fn parse_non_empty_url(raw: &str) -> Result<String, ValidationError> {
    NonEmptyString::new("URL", raw).map(NonEmptyString::into_string)
}

fn parse_non_empty_browser(raw: &str) -> Result<String, ValidationError> {
    NonEmptyString::new("browser profile", raw).map(NonEmptyString::into_string)
}

fn parse_non_empty_path(raw: &str) -> Result<PathBuf, ValidationError> {
    if raw.trim().is_empty() {
        return Err(ValidationError::EmptyValue { field: "path" });
    }
    Ok(PathBuf::from(raw))
}
