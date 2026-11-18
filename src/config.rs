//! Runtime configuration and cross-platform SQLite database path resolution.

use std::env;
use std::net::IpAddr;
use std::path::PathBuf;

use url::{Host, Url};

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

/// Resolves an optional Bilibili API base URL override from `SIPHON_BILIBILI_API_BASE`.
///
/// Permits only loopback `http://` or `https://` origins (`localhost`, `127.0.0.0/8`, `::1`)
/// with no path, query, fragment, or credentials so offline CLI fixture runs cannot
/// route Bilibili requests or saved cookies to non-loopback hosts.
pub fn resolve_bilibili_api_base() -> Result<Option<String>, String> {
    resolve_bilibili_api_base_from_env(|key| env::var(key).ok())
}

/// Resolves an optional Bilibili API base URL override using an explicit environment lookup.
pub fn resolve_bilibili_api_base_from_env<F>(get_env: F) -> Result<Option<String>, String>
where
    F: Fn(&str) -> Option<String>,
{
    let Some(raw) = get_env("SIPHON_BILIBILI_API_BASE") else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let parsed = Url::parse(trimmed)
        .map_err(|err| format!("invalid SIPHON_BILIBILI_API_BASE '{trimmed}': {err}"))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "invalid SIPHON_BILIBILI_API_BASE: embedded credentials are not allowed".to_owned(),
        );
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(
            "invalid SIPHON_BILIBILI_API_BASE: query parameters and fragments are not allowed"
                .to_owned(),
        );
    }
    if !parsed.path().is_empty() && parsed.path() != "/" {
        return Err(
            "invalid SIPHON_BILIBILI_API_BASE: path components are not allowed (expected an origin)"
                .to_owned(),
        );
    }

    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "invalid SIPHON_BILIBILI_API_BASE '{trimmed}': unsupported scheme '{other}'"
            ));
        }
    }

    let Some(host) = parsed.host() else {
        return Err("invalid SIPHON_BILIBILI_API_BASE: missing host".to_owned());
    };
    if !is_loopback_host(&host) {
        return Err(format!(
            "invalid SIPHON_BILIBILI_API_BASE '{trimmed}': only loopback origins (localhost, 127.0.0.1, ::1) are allowed"
        ));
    }

    Ok(Some(parsed.origin().ascii_serialization()))
}

fn is_loopback_host(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(ip) => IpAddr::V4(*ip).is_loopback(),
        Host::Ipv6(ip) => IpAddr::V6(*ip).is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_and_validates_bilibili_api_base_override() {
        assert_eq!(resolve_bilibili_api_base_from_env(|_| None).unwrap(), None);
        assert_eq!(
            resolve_bilibili_api_base_from_env(|_| Some("   ".to_owned())).unwrap(),
            None
        );
        assert_eq!(
            resolve_bilibili_api_base_from_env(|_| Some("http://127.0.0.1:8080/".to_owned()))
                .unwrap(),
            Some("http://127.0.0.1:8080".to_owned())
        );
        assert_eq!(
            resolve_bilibili_api_base_from_env(|_| Some("http://localhost:9000".to_owned()))
                .unwrap(),
            Some("http://localhost:9000".to_owned())
        );
        assert_eq!(
            resolve_bilibili_api_base_from_env(|_| Some("http://[::1]:8123/".to_owned())).unwrap(),
            Some("http://[::1]:8123".to_owned())
        );
        assert_eq!(
            resolve_bilibili_api_base_from_env(|_| Some("https://127.0.0.1:8443/".to_owned()))
                .unwrap(),
            Some("https://127.0.0.1:8443".to_owned())
        );
        assert_eq!(
            resolve_bilibili_api_base_from_env(|_| Some("https://localhost:9443/".to_owned()))
                .unwrap(),
            Some("https://localhost:9443".to_owned())
        );
        assert_eq!(
            resolve_bilibili_api_base_from_env(|_| Some("https://[::1]:8443/".to_owned())).unwrap(),
            Some("https://[::1]:8443".to_owned())
        );

        // Non-loopback HTTP and non-loopback HTTPS must both be rejected.
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some("http://example.com".to_owned())).is_err()
        );
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some("https://api.example.test/".to_owned()))
                .is_err()
        );
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some("https://api.bilibili.com".to_owned()))
                .is_err()
        );
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some(
                "http://127.0.0.1.evil.com:8080".to_owned()
            ))
            .is_err()
        );
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some(
                "https://localhost.evil.com:8443".to_owned()
            ))
            .is_err()
        );
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some("http://127.0.0.1:8080/api".to_owned()))
                .is_err()
        );
        assert!(resolve_bilibili_api_base_from_env(|_| Some("not-a-url".to_owned())).is_err());
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some("ftp://127.0.0.1:8080".to_owned()))
                .is_err()
        );
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some("http://user:pass@127.0.0.1".to_owned()))
                .is_err()
        );
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some("http://127.0.0.1:8080?x=1".to_owned()))
                .is_err()
        );
        assert!(
            resolve_bilibili_api_base_from_env(|_| Some("http://127.0.0.1:8080#frag".to_owned()))
                .is_err()
        );
    }
}
