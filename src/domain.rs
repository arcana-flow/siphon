//! Core domain types, identifiers, and invariants for Siphon.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::ValidationError;

/// Reserved name for the built-in download preset.
pub const BUILTIN_VIDEO_PRESET_NAME: &str = "video";

/// Default `yt-dlp` arguments for the built-in `video` preset.
///
/// Selects the best video and audio streams (falling back to a combined format)
/// so media is downloaded with both video and audio.
pub const BUILTIN_VIDEO_PRESET_ARGS: &[&str] = &["-f", "bv*+ba/b"];

/// Returns the built-in `video` preset definition.
#[must_use]
pub fn builtin_video_preset() -> DownloadPreset {
    DownloadPreset {
        name: PresetName::builtin_video(),
        args: BUILTIN_VIDEO_PRESET_ARGS
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect(),
        is_builtin: true,
    }
}

/// Stable local identifier for a creator stored in Siphon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CreatorId(i64);

impl CreatorId {
    /// Creates a validated positive [`CreatorId`].
    pub fn new(id: i64) -> Result<Self, ValidationError> {
        if id <= 0 {
            return Err(ValidationError::InvalidCreatorId {
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

impl fmt::Display for CreatorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for CreatorId {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let parsed = trimmed
            .parse::<i64>()
            .map_err(|_| ValidationError::InvalidCreatorId {
                input: s.to_owned(),
            })?;
        Self::new(parsed).map_err(|_| ValidationError::InvalidCreatorId {
            input: s.to_owned(),
        })
    }
}

/// Stable local identifier for a tracked upload stored in Siphon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UploadId(i64);

impl UploadId {
    /// Creates a validated positive [`UploadId`].
    pub fn new(id: i64) -> Result<Self, ValidationError> {
        if id <= 0 {
            return Err(ValidationError::InvalidUploadId {
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

impl fmt::Display for UploadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for UploadId {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let parsed = trimmed
            .parse::<i64>()
            .map_err(|_| ValidationError::InvalidUploadId {
                input: s.to_owned(),
            })?;
        Self::new(parsed).map_err(|_| ValidationError::InvalidUploadId {
            input: s.to_owned(),
        })
    }
}

/// Validated download preset name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PresetName(String);

impl PresetName {
    /// Parses and validates a preset name.
    ///
    /// Preset names must be non-empty, cannot contain whitespace or control
    /// characters, and cannot start with `'-'` so they are never confused with CLI flags.
    pub fn new(raw: impl Into<String>) -> Result<Self, ValidationError> {
        let input = raw.into();
        if input.is_empty() {
            return Err(ValidationError::InvalidPresetName {
                input,
                reason: "preset name cannot be empty",
            });
        }
        if input.chars().any(char::is_whitespace) {
            return Err(ValidationError::InvalidPresetName {
                input,
                reason: "preset name cannot contain whitespace",
            });
        }
        if input.chars().any(char::is_control) {
            return Err(ValidationError::InvalidPresetName {
                input,
                reason: "preset name cannot contain control characters",
            });
        }
        if input.starts_with('-') {
            return Err(ValidationError::InvalidPresetName {
                input,
                reason: "preset name cannot start with '-'",
            });
        }
        Ok(Self(input))
    }

    /// Returns the built-in `video` preset name.
    #[must_use]
    pub fn builtin_video() -> Self {
        Self(BUILTIN_VIDEO_PRESET_NAME.to_owned())
    }

    /// Returns the preset name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns `true` if this name refers to the built-in `video` preset.
    #[must_use]
    pub fn is_builtin_video(&self) -> bool {
        self.0 == BUILTIN_VIDEO_PRESET_NAME
    }

    /// Ensures this preset name is not reserved for the built-in preset.
    pub fn ensure_custom(&self) -> Result<(), ValidationError> {
        if self.is_builtin_video() {
            return Err(ValidationError::ReservedPresetName {
                name: self.0.clone(),
            });
        }
        Ok(())
    }
}

impl fmt::Display for PresetName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PresetName {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Non-empty trimmed string value used for CLI inputs such as URLs and browser selectors.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonEmptyString(String);

impl NonEmptyString {
    /// Trims and validates a non-empty string for the given field label.
    pub fn new(field: &'static str, raw: &str) -> Result<Self, ValidationError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ValidationError::EmptyValue { field });
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Returns the validated string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the wrapper and returns the inner [`String`].
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for NonEmptyString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for NonEmptyString {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new("value", s)
    }
}

/// Supported media platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Platform {
    /// Bilibili (`bilibili.com`).
    Bilibili,
}

impl Platform {
    /// Returns the canonical lowercase storage identifier for the platform.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bilibili => "bilibili",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bilibili" => Ok(Self::Bilibili),
            other => Err(ValidationError::UnsupportedPlatform {
                platform: other.to_owned(),
            }),
        }
    }
}

/// UTC timestamp stored with 1-second precision as Unix epoch seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Returns the current UTC timestamp truncated to whole seconds.
    #[must_use]
    pub fn now() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        Self(secs)
    }

    /// Constructs a [`Timestamp`] from Unix epoch seconds.
    #[must_use]
    pub const fn from_unix_seconds(seconds: i64) -> Self {
        Self(seconds)
    }

    /// Returns the UTC Unix epoch seconds.
    #[must_use]
    pub const fn as_unix_seconds(self) -> i64 {
        self.0
    }

    /// Formats the timestamp as an RFC 3339 / ISO 8601 UTC string (`YYYY-MM-DDTHH:MM:SSZ`).
    #[must_use]
    pub fn format_utc_iso(self) -> String {
        let days = self.0.div_euclid(86_400);
        let secs_of_day = self.0.rem_euclid(86_400);
        let hour = secs_of_day / 3_600;
        let minute = (secs_of_day % 3_600) / 60;
        let second = secs_of_day % 60;

        let (year, month, day) = civil_from_days(days);
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_utc_iso())
    }
}

/// Converts days since 1970-01-01 to proleptic Gregorian `(year, month, day)`.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    (year, m as u32, d as u32)
}

/// A creator known to Siphon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Creator {
    /// Stable local creator ID.
    pub id: CreatorId,
    /// Origin media platform.
    pub platform: Platform,
    /// Platform-assigned unique creator identifier (for Bilibili, the numeric `mid`).
    pub platform_creator_id: String,
    /// Human-readable creator display name.
    pub display_name: String,
}

/// Active follow metadata and refresh boundary for a creator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Follow {
    /// Local creator ID being followed.
    pub creator_id: CreatorId,
    /// Timestamp when the active follow began.
    pub followed_at: Timestamp,
    /// High-water publication timestamp already checked for this active follow.
    pub refresh_checkpoint: Timestamp,
}

/// Combined creator and active follow record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowedCreator {
    /// Creator identity and display metadata.
    pub creator: Creator,
    /// Active follow timing and checkpoint state.
    pub follow: Follow,
}

/// Lifecycle state of a tracked upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UploadStatus {
    /// Discovered during refresh and awaiting download or ignore.
    Pending,
    /// Downloaded via `siphon download --upload <ID>` with confirmed output artifact.
    Downloaded,
    /// Explicitly ignored via `siphon upload ignore <ID>`.
    Ignored,
}

impl UploadStatus {
    /// Returns the canonical lowercase storage string for the status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Downloaded => "downloaded",
            Self::Ignored => "ignored",
        }
    }
}

impl fmt::Display for UploadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UploadStatus {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "downloaded" => Ok(Self::Downloaded),
            "ignored" => Ok(Self::Ignored),
            other => Err(ValidationError::InvalidUploadStatus {
                status: other.to_owned(),
            }),
        }
    }
}

/// A tracked creator upload persisted in Siphon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    /// Stable local upload ID.
    pub id: UploadId,
    /// Creator that published this upload.
    pub creator_id: CreatorId,
    /// Origin media platform.
    pub platform: Platform,
    /// Platform-assigned upload identifier (unique per platform).
    pub platform_upload_id: String,
    /// Canonical or latest known media URL.
    pub url: String,
    /// Upload title.
    pub title: String,
    /// Publication timestamp on the platform.
    pub published_at: Timestamp,
    /// Current lifecycle status.
    pub status: UploadStatus,
}

/// An upload discovered from a platform feed prior to local ID assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredUpload {
    /// Origin media platform.
    pub platform: Platform,
    /// Platform-assigned upload identifier.
    pub platform_upload_id: String,
    /// Media URL for downloading.
    pub url: String,
    /// Upload title.
    pub title: String,
    /// Publication timestamp on the platform.
    pub published_at: Timestamp,
}

/// Reference to an external cookie source; cookie secrets are never stored in SQLite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CookieSource {
    /// Path to a Netscape-format `cookies.txt` file.
    File(PathBuf),
    /// Browser profile selector compatible with `yt-dlp --cookies-from-browser`.
    BrowserProfile(String),
}

impl CookieSource {
    /// Validates and constructs a file-backed [`CookieSource`].
    pub fn from_file(path: PathBuf) -> Result<Self, ValidationError> {
        if path.as_os_str().is_empty() {
            return Err(ValidationError::InvalidCookiePath {
                reason: "cookie file path cannot be empty",
            });
        }
        Ok(Self::File(path))
    }

    /// Validates and constructs a browser-profile-backed [`CookieSource`].
    pub fn from_browser_profile(spec: &str) -> Result<Self, ValidationError> {
        let validated = NonEmptyString::new("browser profile", spec)?;
        Ok(Self::BrowserProfile(validated.into_string()))
    }

    /// Returns a human-readable summary of the cookie source without exposing secrets.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::File(path) => format!("cookie file {}", path.display()),
            Self::BrowserProfile(spec) => format!("browser profile {spec}"),
        }
    }
}

impl fmt::Display for CookieSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Named download preset defining ordered `yt-dlp` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadPreset {
    /// Unique preset name.
    pub name: PresetName,
    /// Arguments passed to `yt-dlp` in exact order.
    pub args: Vec<String>,
    /// `true` for the built-in `video` preset; `false` for user-created presets.
    pub is_builtin: bool,
}

/// Download preset paired with whether it is currently selected as the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetSummary {
    /// Preset definition.
    pub preset: DownloadPreset,
    /// Whether this preset is the active default.
    pub is_default: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_formats_utc_iso_accurately() {
        assert_eq!(
            Timestamp::from_unix_seconds(0).format_utc_iso(),
            "1970-01-01T00:00:00Z"
        );
        // 2024-02-29T23:59:59Z (leap day)
        assert_eq!(
            Timestamp::from_unix_seconds(1_709_251_199).format_utc_iso(),
            "2024-02-29T23:59:59Z"
        );
        // 1969-12-31T23:59:59Z (pre-epoch)
        assert_eq!(
            Timestamp::from_unix_seconds(-1).format_utc_iso(),
            "1969-12-31T23:59:59Z"
        );
    }

    #[test]
    fn ids_and_enums_validate_inputs() {
        assert_eq!("42".parse::<CreatorId>().unwrap().as_i64(), 42);
        assert!("0".parse::<CreatorId>().is_err());
        assert!("-5".parse::<CreatorId>().is_err());

        assert_eq!("9".parse::<UploadId>().unwrap().as_i64(), 9);
        assert!("0".parse::<UploadId>().is_err());

        assert_eq!("bilibili".parse::<Platform>().unwrap(), Platform::Bilibili);
        assert!("youtube".parse::<Platform>().is_err());

        assert_eq!(
            "pending".parse::<UploadStatus>().unwrap(),
            UploadStatus::Pending
        );
        assert_eq!(
            "downloaded".parse::<UploadStatus>().unwrap(),
            UploadStatus::Downloaded
        );
        assert_eq!(
            "ignored".parse::<UploadStatus>().unwrap(),
            UploadStatus::Ignored
        );
        assert!("unknown".parse::<UploadStatus>().is_err());
    }

    #[test]
    fn preset_name_and_cookie_source_validation() {
        let builtin = builtin_video_preset();
        assert!(builtin.is_builtin);
        assert!(builtin.name.is_builtin_video());
        assert!(builtin.name.ensure_custom().is_err());

        let custom = PresetName::new("audio_mp3").unwrap();
        assert!(custom.ensure_custom().is_ok());

        assert!(CookieSource::from_file(PathBuf::new()).is_err());
        assert!(CookieSource::from_browser_profile("   ").is_err());
        assert_eq!(
            CookieSource::from_browser_profile("firefox")
                .unwrap()
                .describe(),
            "browser profile firefox"
        );
    }
}
