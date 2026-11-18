//! Media download integration contract, preset resolution, and upload status rules.

use std::path::PathBuf;

use crate::domain::{CookieSource, PresetName, Upload, UploadId, UploadStatus};
use crate::error::{AppError, DownloadError, StorageError};
use crate::storage::Storage;

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
    /// 4. `--no-playlist` when downloading `--upload <ID>`, keeping it scoped to one upload.
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
            // Ensure `--upload <ID>` remains single-upload even if a preset enables playlists.
            args.push("--no-playlist".to_owned());
        }
        args.push("--".to_owned());
        args.push(self.target.url().to_owned());
        args
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
