//! Platform refresh integration contract and independent per-creator orchestration.

use std::collections::HashSet;

use crate::domain::{
    CookieSource, Creator, CreatorId, DiscoveredUpload, FollowedCreator, Platform, Timestamp,
};
use crate::error::{AppError, CreatorRefreshFailure, RefreshError};
use crate::storage::{CommitRefreshOutcome, CommitRefreshRequest, FollowCreatorRequest, Storage};

/// Creator metadata resolved from a user-provided creator URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCreator {
    /// Resolved origin platform.
    pub platform: Platform,
    /// Platform-assigned unique creator ID.
    pub platform_creator_id: String,
    /// Human-readable display name.
    pub display_name: String,
}

/// Uploads discovered and the resulting checkpoint computed by a [`RefreshSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshFeedResult {
    /// Discovered uploads published after the creator's previous checkpoint.
    pub uploads: Vec<DiscoveredUpload>,
    /// Proposed new checkpoint after fetching all required pages.
    pub new_checkpoint: Timestamp,
}

/// Platform client contract for resolving Bilibili creators and fetching upload feeds.
pub trait RefreshSource: Send + Sync {
    /// Parses `url` and resolves the creator's platform ID and display name.
    fn resolve_creator(
        &self,
        url: &str,
        cookies: Option<&CookieSource>,
    ) -> Result<ResolvedCreator, RefreshError>;

    /// Fetches all pages up to `checkpoint` (or end of feed) for `creator`.
    ///
    /// Returning `Err` leaves the creator's checkpoint unchanged in storage.
    fn fetch_uploads_since(
        &self,
        creator: &Creator,
        checkpoint: Timestamp,
        cookies: Option<&CookieSource>,
    ) -> Result<RefreshFeedResult, RefreshError>;

    /// Fetches all pages for an active follow, distinguishing the initial `followed_at`
    /// boundary (exclusive) from a subsequent `refresh_checkpoint` (inclusive replay).
    ///
    /// Returning `Err` leaves the creator's checkpoint unchanged in storage.
    fn fetch_uploads_for_follow(
        &self,
        followed: &FollowedCreator,
        cookies: Option<&CookieSource>,
    ) -> Result<RefreshFeedResult, RefreshError> {
        self.fetch_uploads_since(
            &followed.creator,
            followed.follow.refresh_checkpoint,
            cookies,
        )
    }
}

/// Placeholder [`RefreshSource`] implementation used until the Bilibili backend is wired.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnimplementedRefreshSource;

impl RefreshSource for UnimplementedRefreshSource {
    fn resolve_creator(
        &self,
        _url: &str,
        _cookies: Option<&CookieSource>,
    ) -> Result<ResolvedCreator, RefreshError> {
        Err(RefreshError::NotImplemented)
    }

    fn fetch_uploads_since(
        &self,
        _creator: &Creator,
        _checkpoint: Timestamp,
        _cookies: Option<&CookieSource>,
    ) -> Result<RefreshFeedResult, RefreshError> {
        Err(RefreshError::NotImplemented)
    }
}

/// Cookie selection policy for a `siphon refresh` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshCookiePolicy {
    /// Use the saved default cookie source from `refresh_preferences`, if any.
    UseSavedDefault,
    /// Use the explicitly provided cookie source, optionally persisting it as the new default.
    Explicit {
        /// Selected cookie source for this invocation.
        source: CookieSource,
        /// Whether to persist `source` in `refresh_preferences`.
        save_as_default: bool,
    },
    /// Clear the saved default cookie source and refresh without cookies.
    ClearSavedDefault,
    /// Ignore any saved default cookie source for this invocation without modifying storage.
    IgnoreSavedDefault,
}

/// Per-creator result within a `siphon refresh` execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreatorRefreshOutcome {
    /// Creator feed was fetched and committed atomically.
    Succeeded(CommitRefreshOutcome),
    /// Creator refresh failed without modifying its checkpoint.
    Failed {
        /// Creator whose refresh failed.
        creator: FollowedCreator,
        /// Underlying failure.
        error: RefreshError,
    },
}

/// Aggregated result of running `siphon refresh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshSummary {
    /// Individual creator refresh outcomes in execution order.
    pub outcomes: Vec<CreatorRefreshOutcome>,
    /// Effective cookie source used during refresh, if any.
    pub used_cookies: Option<CookieSource>,
    /// Whether the saved default cookie source was cleared.
    pub cleared_default_cookies: bool,
    /// Whether an explicit cookie source was persisted as the new default.
    pub saved_default_cookies: bool,
}

impl RefreshSummary {
    /// Converts any per-creator failures into [`AppError::RefreshPartialFailure`], or returns `Ok(())`.
    pub fn check_failures(&self) -> Result<(), AppError> {
        let mut succeeded = 0usize;
        let mut failures = Vec::new();
        for outcome in &self.outcomes {
            match outcome {
                CreatorRefreshOutcome::Succeeded(_) => succeeded += 1,
                CreatorRefreshOutcome::Failed { creator, error } => {
                    failures.push(CreatorRefreshFailure {
                        creator_id: creator.creator.id,
                        display_name: creator.creator.display_name.clone(),
                        error: error.to_string(),
                    });
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(AppError::RefreshPartialFailure {
                succeeded,
                failures,
            })
        }
    }
}

/// Resolves a creator URL and creates an active follow starting at `followed_at`.
pub fn follow_creator(
    storage: &dyn Storage,
    source: &dyn RefreshSource,
    url: &str,
    followed_at: Timestamp,
) -> Result<FollowedCreator, AppError> {
    let cookies = storage.get_default_refresh_cookies()?;
    let resolved = source.resolve_creator(url, cookies.as_ref())?;
    let followed = storage.follow_creator(FollowCreatorRequest {
        platform: resolved.platform,
        platform_creator_id: resolved.platform_creator_id,
        display_name: resolved.display_name,
        followed_at,
    })?;
    Ok(followed)
}

/// Refreshes either a single followed creator or all followed creators independently.
///
/// Each creator's fetch and transactional checkpoint commit runs independently so a failure on
/// one creator leaves its checkpoint unchanged and does not block remaining creators.
pub fn refresh_creators(
    storage: &dyn Storage,
    source: &dyn RefreshSource,
    creator_id: Option<CreatorId>,
    cookie_policy: RefreshCookiePolicy,
) -> Result<RefreshSummary, AppError> {
    let (used_cookies, cleared_default_cookies, saved_default_cookies) =
        resolve_refresh_cookies(storage, cookie_policy)?;

    let targets = match creator_id {
        Some(id) => vec![storage.get_followed_creator(id)?],
        None => storage.list_followed_creators()?,
    };

    let mut outcomes = Vec::with_capacity(targets.len());
    for target in targets {
        match refresh_single_creator(storage, source, &target, used_cookies.as_ref()) {
            Ok(committed) => outcomes.push(CreatorRefreshOutcome::Succeeded(committed)),
            Err(error) => outcomes.push(CreatorRefreshOutcome::Failed {
                creator: target,
                error,
            }),
        }
    }

    Ok(RefreshSummary {
        outcomes,
        used_cookies,
        cleared_default_cookies,
        saved_default_cookies,
    })
}

fn resolve_refresh_cookies(
    storage: &dyn Storage,
    policy: RefreshCookiePolicy,
) -> Result<(Option<CookieSource>, bool, bool), AppError> {
    match policy {
        RefreshCookiePolicy::UseSavedDefault => {
            Ok((storage.get_default_refresh_cookies()?, false, false))
        }
        RefreshCookiePolicy::Explicit {
            source,
            save_as_default,
        } => {
            if save_as_default {
                storage.set_default_refresh_cookies(Some(&source))?;
            }
            Ok((Some(source), false, save_as_default))
        }
        RefreshCookiePolicy::ClearSavedDefault => {
            storage.set_default_refresh_cookies(None)?;
            Ok((None, true, false))
        }
        RefreshCookiePolicy::IgnoreSavedDefault => Ok((None, false, false)),
    }
}

fn refresh_single_creator(
    storage: &dyn Storage,
    source: &dyn RefreshSource,
    target: &FollowedCreator,
    cookies: Option<&CookieSource>,
) -> Result<CommitRefreshOutcome, RefreshError> {
    let mut feed = source.fetch_uploads_for_follow(target, cookies)?;

    // Never admit uploads at or before the initial follow boundary, and deduplicate
    // repeated `(platform, platform_upload_id)` entries before handing off to storage.
    let had_raw_uploads = !feed.uploads.is_empty();
    let mut seen = HashSet::new();
    feed.uploads.retain(|upload| {
        upload.published_at > target.follow.followed_at
            && seen.insert((upload.platform, upload.platform_upload_id.clone()))
    });

    // Ensure a creator's refresh checkpoint never regresses even if an API feed is unordered.
    let max_discovered = feed
        .uploads
        .iter()
        .map(|upload| upload.published_at)
        .max()
        .unwrap_or(target.follow.refresh_checkpoint);
    let new_checkpoint = if had_raw_uploads && feed.uploads.is_empty() {
        target.follow.refresh_checkpoint
    } else {
        feed.new_checkpoint
            .max(max_discovered)
            .max(target.follow.refresh_checkpoint)
    };

    let outcome = storage.commit_refresh(CommitRefreshRequest {
        creator_id: target.creator.id,
        expected_follow_id: target.follow.id,
        expected_follow_time: target.follow.followed_at,
        expected_checkpoint: target.follow.refresh_checkpoint,
        new_checkpoint,
        discovered_uploads: feed.uploads,
    })?;
    Ok(outcome)
}
