//! Stable storage integration contract for Siphon's local SQLite persistence.

pub mod migrations;
pub mod sqlite;

pub use sqlite::SqliteStorage;

use crate::domain::{
    CookieSource, Creator, CreatorId, DiscoveredUpload, DownloadPreset, FollowId, FollowedCreator,
    Platform, PresetName, PresetSummary, Timestamp, Upload, UploadId,
};
use crate::error::StorageError;

/// Parameters for following a creator at a specific follow timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowCreatorRequest {
    /// Origin media platform.
    pub platform: Platform,
    /// Platform-assigned unique creator ID.
    pub platform_creator_id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Timestamp when the follow starts; also initializes `refresh_checkpoint`.
    pub followed_at: Timestamp,
}

/// Transactional request to record discovered uploads and advance a creator's checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRefreshRequest {
    /// Followed creator being refreshed.
    pub creator_id: CreatorId,
    /// Durable follow ID observed before fetching; must still match on commit.
    pub expected_follow_id: FollowId,
    /// Active follow start time observed before fetching; must still match on commit.
    pub expected_follow_time: Timestamp,
    /// Refresh checkpoint observed before fetching; must still match on commit.
    pub expected_checkpoint: Timestamp,
    /// New refresh checkpoint to persist in the same transaction.
    pub new_checkpoint: Timestamp,
    /// Uploads discovered since `expected_checkpoint`.
    pub discovered_uploads: Vec<DiscoveredUpload>,
}

/// Summary returned after committing a single creator's refresh transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRefreshOutcome {
    /// Local ID of the refreshed creator.
    pub creator_id: CreatorId,
    /// Creator display name.
    pub display_name: String,
    /// Count of newly inserted pending uploads.
    pub new_uploads_inserted: usize,
    /// Count of already-known uploads encountered whose existing status was preserved.
    pub existing_uploads_seen: usize,
    /// Refresh checkpoint before this refresh.
    pub previous_checkpoint: Timestamp,
    /// Refresh checkpoint stored after this refresh.
    pub new_checkpoint: Timestamp,
}

/// Outcome of removing a named download preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovePresetOutcome {
    /// Name of the preset that was removed.
    pub removed: PresetName,
    /// `true` if the removed preset was the active default and default was reset to `video`.
    pub reset_default_to_builtin: bool,
}

/// Persistence contract implemented by downstream storage backends (SQLite in v1).
///
/// Implementations must preserve the transactional and status invariants described in
/// `docs/database.md` and `docs/adr/0001-store-download-presets-in-sqlite.md`.
pub trait Storage: Send + Sync {
    /// Follows a creator, reusing its existing [`CreatorId`] if it was previously unfollowed.
    ///
    /// Returns [`StorageError::CreatorAlreadyFollowed`] if the creator already has an active
    /// follow. Otherwise assigns a fresh durable [`FollowId`] (never reusing prior IDs across
    /// unfollow/re-follow) and initializes both `followed_at` and `refresh_checkpoint` to
    /// `request.followed_at`.
    fn follow_creator(
        &self,
        request: FollowCreatorRequest,
    ) -> Result<FollowedCreator, StorageError>;

    /// Lists all creators that currently have an active follow, ordered deterministically by ID.
    fn list_followed_creators(&self) -> Result<Vec<FollowedCreator>, StorageError>;

    /// Fetches a single actively followed creator by local ID.
    ///
    /// Returns [`StorageError::CreatorNotFound`] if the creator does not exist, or
    /// [`StorageError::CreatorNotFollowed`] if it exists without an active follow.
    fn get_followed_creator(&self, creator_id: CreatorId) -> Result<FollowedCreator, StorageError>;

    /// Removes the active follow for `creator_id` while retaining the creator and its uploads.
    ///
    /// Returns [`StorageError::CreatorNotFound`] if the creator does not exist, or
    /// [`StorageError::CreatorNotFollowed`] if it is not currently followed.
    fn unfollow_creator(&self, creator_id: CreatorId) -> Result<Creator, StorageError>;

    /// Atomically persists discovered uploads and advances the creator's refresh checkpoint.
    ///
    /// Must verify in the same transaction that an active follow exists for `request.creator_id`
    /// with `id == request.expected_follow_id`, `followed_at == request.expected_follow_time`, and
    /// `refresh_checkpoint == request.expected_checkpoint`; otherwise fails with
    /// [`StorageError::StaleRefreshCheckpoint`]. Comparing by [`FollowId`] ensures a same-second
    /// unfollow and re-follow is rejected even when timestamps are equal. Existing uploads keyed by
    /// `(platform, platform_upload_id)` must retain their current status.
    fn commit_refresh(
        &self,
        request: CommitRefreshRequest,
    ) -> Result<CommitRefreshOutcome, StorageError>;

    /// Lists all uploads currently in [`UploadStatus::Pending`](crate::domain::UploadStatus::Pending).
    fn list_pending_uploads(&self) -> Result<Vec<Upload>, StorageError>;

    /// Fetches a single upload by local ID.
    fn get_upload(&self, upload_id: UploadId) -> Result<Upload, StorageError>;

    /// Transitions a pending upload to [`UploadStatus::Ignored`](crate::domain::UploadStatus::Ignored).
    ///
    /// Fails with [`StorageError::UploadNotPending`] if the upload is not currently `pending`.
    fn mark_upload_ignored(&self, upload_id: UploadId) -> Result<Upload, StorageError>;

    /// Transitions a pending upload to [`UploadStatus::Downloaded`](crate::domain::UploadStatus::Downloaded).
    ///
    /// Fails with [`StorageError::UploadNotPending`] if the upload is not currently `pending`.
    fn mark_upload_downloaded(&self, upload_id: UploadId) -> Result<Upload, StorageError>;

    /// Saves a new custom download preset with ordered `yt-dlp` arguments.
    ///
    /// Rejects the reserved built-in name `video` with [`StorageError::CannotModifyBuiltinPreset`]
    /// and duplicate names with [`StorageError::PresetAlreadyExists`].
    fn add_preset(
        &self,
        name: PresetName,
        args: Vec<String>,
    ) -> Result<DownloadPreset, StorageError>;

    /// Lists all available presets including the built-in `video` preset and indicates the default.
    fn list_presets(&self) -> Result<Vec<PresetSummary>, StorageError>;

    /// Resolves a preset by name, returning the built-in `video` preset when requested.
    fn get_preset(&self, name: &PresetName) -> Result<DownloadPreset, StorageError>;

    /// Removes a user-defined preset.
    ///
    /// Rejects removing the built-in `video` preset with
    /// [`StorageError::CannotModifyBuiltinPreset`]. If the removed preset was the selected
    /// default, resets the active default to `video` in the same transaction.
    fn remove_preset(&self, name: &PresetName) -> Result<RemovePresetOutcome, StorageError>;

    /// Sets the active default preset (either a stored preset or the built-in `video` preset).
    fn set_default_preset(&self, name: &PresetName) -> Result<DownloadPreset, StorageError>;

    /// Returns the active default download preset (initially the built-in `video` preset).
    fn get_default_preset(&self) -> Result<DownloadPreset, StorageError>;

    /// Returns the remembered default cookie source for `refresh`, if any.
    fn get_default_refresh_cookies(&self) -> Result<Option<CookieSource>, StorageError>;

    /// Saves or clears the remembered default cookie source for `refresh`.
    fn set_default_refresh_cookies(
        &self,
        source: Option<&CookieSource>,
    ) -> Result<(), StorageError>;
}

/// Placeholder [`Storage`] implementation used until the SQLite backend is wired.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnimplementedStorage;

impl Storage for UnimplementedStorage {
    fn follow_creator(
        &self,
        _request: FollowCreatorRequest,
    ) -> Result<FollowedCreator, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn list_followed_creators(&self) -> Result<Vec<FollowedCreator>, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn get_followed_creator(
        &self,
        _creator_id: CreatorId,
    ) -> Result<FollowedCreator, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn unfollow_creator(&self, _creator_id: CreatorId) -> Result<Creator, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn commit_refresh(
        &self,
        _request: CommitRefreshRequest,
    ) -> Result<CommitRefreshOutcome, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn list_pending_uploads(&self) -> Result<Vec<Upload>, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn get_upload(&self, _upload_id: UploadId) -> Result<Upload, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn mark_upload_ignored(&self, _upload_id: UploadId) -> Result<Upload, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn mark_upload_downloaded(&self, _upload_id: UploadId) -> Result<Upload, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn add_preset(
        &self,
        _name: PresetName,
        _args: Vec<String>,
    ) -> Result<DownloadPreset, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn list_presets(&self) -> Result<Vec<PresetSummary>, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn get_preset(&self, _name: &PresetName) -> Result<DownloadPreset, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn remove_preset(&self, _name: &PresetName) -> Result<RemovePresetOutcome, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn set_default_preset(&self, _name: &PresetName) -> Result<DownloadPreset, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn get_default_preset(&self) -> Result<DownloadPreset, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn get_default_refresh_cookies(&self) -> Result<Option<CookieSource>, StorageError> {
        Err(StorageError::NotImplemented)
    }

    fn set_default_refresh_cookies(
        &self,
        _source: Option<&CookieSource>,
    ) -> Result<(), StorageError> {
        Err(StorageError::NotImplemented)
    }
}
