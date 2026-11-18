//! Runtime configuration and cross-platform SQLite database path resolution.

use std::env;
use std::path::PathBuf;

/// Application runtime configuration resolved from CLI arguments and environment variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// Path to the local SQLite database file.
    pub db_path: PathBuf,
}

impl RuntimeConfig {
    /// Resolves runtime configuration from an optional `--db` / `SIPHON_DB` override.
    #[must_use]
    pub fn resolve(db_override: Option<PathBuf>) -> Self {
        let db_path = db_override.unwrap_or_else(default_db_path);
        Self { db_path }
    }
}

/// Returns the default per-user SQLite database path for the current platform.
#[must_use]
pub fn default_db_path() -> PathBuf {
    default_db_path_from_env(|key| env::var_os(key).map(PathBuf::from))
}

/// Resolves the default database path using an explicit environment lookup for testability.
#[must_use]
pub fn default_db_path_from_env<F>(get_env: F) -> PathBuf
where
    F: Fn(&str) -> Option<PathBuf>,
{
    let non_empty = |key: &str| get_env(key).filter(|path| !path.as_os_str().is_empty());

    if cfg!(target_os = "windows") {
        if let Some(dir) = non_empty("LOCALAPPDATA").or_else(|| non_empty("APPDATA")) {
            return dir.join("siphon").join("siphon.db");
        }
    } else if cfg!(target_os = "macos") {
        if let Some(home) = non_empty("HOME") {
            return home
                .join("Library")
                .join("Application Support")
                .join("siphon")
                .join("siphon.db");
        }
    } else {
        if let Some(xdg) = non_empty("XDG_DATA_HOME") {
            return xdg.join("siphon").join("siphon.db");
        }
        if let Some(home) = non_empty("HOME") {
            return home
                .join(".local")
                .join("share")
                .join("siphon")
                .join("siphon.db");
        }
    }

    PathBuf::from("siphon.db")
}
