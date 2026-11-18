//! Shared cookie-source parsing, validation, and cookie loading for Siphon.
//!
//! Supports Netscape-format `cookies.txt` files and `yt-dlp`-compatible
//! browser profile selectors (`BROWSER[+KEYRING][:PROFILE][::CONTAINER]`).
//! Cookie values are redacted from `Debug` output and never placed in error messages.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use rookie_cookies::direct_path::{DirectPathError, InvalidDirectPathOptionsReason};
use rookie_cookies::{FromPathRequest, ReadRequest, RequestError};
use serde::Deserialize;
use url::Url;

use crate::domain::{CookieSource, Timestamp};
use crate::error::{DownloadError, RefreshError};

const KNOWN_KEYRINGS: &[&str] = &[
    "basictext",
    "gnomekeyring",
    "kwallet",
    "kwallet5",
    "kwallet6",
];

const GECKO_BROWSERS: &[&str] = &["firefox", "librewolf", "zen", "cachy"];

/// Structured failure while validating, reading, or parsing a [`CookieSource`].
///
/// Never includes raw cookie secret values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieError {
    /// Non-secret description of the cookie source.
    pub cookie_source: String,
    /// Human-readable cause of the failure.
    pub reason: String,
}

impl CookieError {
    /// Creates a new [`CookieError`] for a [`CookieSource`].
    #[must_use]
    pub fn for_source(source: &CookieSource, reason: impl Into<String>) -> Self {
        Self {
            cookie_source: source.describe(),
            reason: reason.into(),
        }
    }

    /// Creates a new [`CookieError`] with a pre-formatted source label.
    #[must_use]
    pub fn new(cookie_source: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            cookie_source: cookie_source.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for CookieError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cookie error for {}: {}",
            self.cookie_source, self.reason
        )
    }
}

impl std::error::Error for CookieError {}

impl From<CookieError> for RefreshError {
    fn from(err: CookieError) -> Self {
        Self::CookieSourceInvalid {
            cookie_source: err.cookie_source,
            reason: err.reason,
        }
    }
}

impl From<CookieError> for DownloadError {
    fn from(err: CookieError) -> Self {
        Self::CookieSourceInvalid {
            cookie_source: err.cookie_source,
            reason: err.reason,
        }
    }
}

/// An HTTP cookie loaded from a Netscape file or browser profile.
#[derive(Clone, PartialEq, Eq)]
pub struct Cookie {
    /// Cookie domain (may begin with `.` for domain-wide cookies).
    pub domain: String,
    /// Whether subdomains of `domain` should also receive this cookie.
    pub include_subdomains: bool,
    /// URL path scope for the cookie.
    pub path: String,
    /// Whether the cookie requires an HTTPS transport.
    pub secure: bool,
    /// Optional expiration timestamp (`None` for session cookies).
    pub expires: Option<Timestamp>,
    /// Whether the cookie is marked `HttpOnly`.
    pub http_only: bool,
    /// Cookie name.
    pub name: String,
    /// Cookie secret value (redacted in `Debug` formatting).
    pub value: String,
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("domain", &self.domain)
            .field("include_subdomains", &self.include_subdomains)
            .field("path", &self.path)
            .field("secure", &self.secure)
            .field("expires", &self.expires)
            .field("http_only", &self.http_only)
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// Collection of cookies with RFC 6265 URL matching and header formatting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    /// Wraps a list of cookies into a [`CookieJar`].
    #[must_use]
    pub fn from_cookies(cookies: Vec<Cookie>) -> Self {
        Self { cookies }
    }

    /// Borrows all cookies in the jar.
    #[must_use]
    pub fn cookies(&self) -> &[Cookie] {
        &self.cookies
    }

    /// Consumes the jar and returns the inner cookie vector.
    #[must_use]
    pub fn into_cookies(self) -> Vec<Cookie> {
        self.cookies
    }

    /// Returns the number of cookies in the jar.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// Returns `true` if the jar contains no cookies.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Returns cookies matching `target_url` at `now`, ordered by specificity.
    #[must_use]
    pub fn matching_url(&self, target_url: &str, now: Timestamp) -> Vec<&Cookie> {
        let Ok(parsed) = Url::parse(target_url) else {
            return Vec::new();
        };
        let scheme = parsed.scheme();
        if scheme != "http" && scheme != "https" {
            return Vec::new();
        }
        let Some(host) = parsed.host_str() else {
            return Vec::new();
        };
        let req_host = host.to_ascii_lowercase();
        let req_path = if parsed.path().is_empty() {
            "/"
        } else {
            parsed.path()
        };
        let is_https = scheme == "https";

        let mut matched: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|cookie| {
                cookie.expires.is_none_or(|exp| exp > now)
                    && (!cookie.secure || is_https)
                    && domain_matches(&cookie.domain, cookie.include_subdomains, &req_host)
                    && path_matches(&cookie.path, req_path)
            })
            .collect();

        // Longer paths must precede shorter paths per RFC 6265 §5.4.
        matched.sort_by_key(|a| std::cmp::Reverse(a.path.len()));
        matched
    }

    /// Formats an HTTP `Cookie` request header for `target_url` at `now`, if any cookie matches.
    #[must_use]
    pub fn cookie_header_for_url(&self, target_url: &str, now: Timestamp) -> Option<String> {
        let matched = self.matching_url(target_url, now);
        if matched.is_empty() {
            return None;
        }
        // Preserve the most specific entry per cookie name when duplicates exist across paths.
        let mut seen = BTreeMap::new();
        let mut pairs = Vec::with_capacity(matched.len());
        for cookie in matched {
            let key = (
                cookie.name.as_str(),
                cookie.path.as_str(),
                cookie.domain.as_str(),
            );
            if seen.insert(key, ()).is_none() {
                pairs.push(format!("{}={}", cookie.name, cookie.value));
            }
        }
        if pairs.is_empty() {
            None
        } else {
            Some(pairs.join("; "))
        }
    }
}

/// Parsed representation of `BROWSER[+KEYRING][:PROFILE][::CONTAINER]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProfileSpec {
    /// Browser identifier (lowercased).
    pub browser: String,
    /// Optional Linux keyring selector (lowercased).
    pub keyring: Option<String>,
    /// Optional profile name or filesystem path.
    pub profile: Option<String>,
    /// Optional Firefox Multi-Account Container selector.
    pub container: Option<String>,
}

impl BrowserProfileSpec {
    /// Parses a `yt-dlp --cookies-from-browser` compatible selector string.
    pub fn parse(raw: &str) -> Result<Self, CookieError> {
        let trimmed = raw.trim();
        let label = format!("browser profile {trimmed}");
        if trimmed.is_empty() {
            return Err(CookieError::new(
                "browser profile",
                "browser profile selector cannot be empty",
            ));
        }

        let (head, tail) = match trimmed.split_once(':') {
            Some((head, tail)) => (head, Some(tail)),
            None => (trimmed, None),
        };

        let (browser_raw, keyring) = match head.split_once('+') {
            Some((b, k)) => {
                let b = b.trim();
                let k = k.trim();
                if b.is_empty() {
                    return Err(CookieError::new(&label, "missing browser name before '+'"));
                }
                if k.is_empty() {
                    return Err(CookieError::new(&label, "empty keyring name after '+'"));
                }
                let k_lower = k.to_ascii_lowercase();
                if !KNOWN_KEYRINGS.contains(&k_lower.as_str()) {
                    return Err(CookieError::new(
                        &label,
                        format!(
                            "unsupported keyring '{k}'; expected one of: {}",
                            KNOWN_KEYRINGS.join(", ")
                        ),
                    ));
                }
                (b, Some(k_lower))
            }
            None => (head.trim(), None),
        };

        if browser_raw.is_empty() {
            return Err(CookieError::new(&label, "missing browser name"));
        }
        let browser = browser_raw.to_ascii_lowercase();

        let (profile, container) = match tail {
            None => (None, None),
            Some(rest) => {
                if let Some(after_double) = rest.strip_prefix(':') {
                    if after_double.is_empty() {
                        return Err(CookieError::new(&label, "empty container name after '::'"));
                    }
                    if after_double.contains("::") {
                        return Err(CookieError::new(
                            &label,
                            "unexpected extra '::' in container selector",
                        ));
                    }
                    (None, Some(after_double.trim().to_owned()))
                } else if let Some((prof, cont)) = rest.rsplit_once("::") {
                    let prof = prof.trim();
                    let cont = cont.trim();
                    if cont.is_empty() {
                        return Err(CookieError::new(&label, "empty container name after '::'"));
                    }
                    if prof.contains("::") {
                        return Err(CookieError::new(
                            &label,
                            "unexpected extra '::' in profile selector",
                        ));
                    }
                    let profile = if prof.is_empty() {
                        None
                    } else {
                        Some(prof.to_owned())
                    };
                    (profile, Some(cont.to_owned()))
                } else {
                    let prof = rest.trim();
                    if prof.is_empty() {
                        return Err(CookieError::new(&label, "empty profile selector after ':'"));
                    }
                    (Some(prof.to_owned()), None)
                }
            }
        };

        let spec = Self {
            browser,
            keyring,
            profile,
            container,
        };

        if spec.container.is_some() && !spec.is_gecko_family() {
            return Err(CookieError::new(
                &label,
                format!(
                    "container selector '::{}' is only supported for Firefox-family browsers, not '{}'",
                    spec.container.as_deref().unwrap_or_default(),
                    spec.browser
                ),
            ));
        }

        Ok(spec)
    }

    /// Maps common `yt-dlp` browser aliases to `rookie-cookies` canonical browser IDs.
    #[must_use]
    pub fn canonical_browser_id(&self) -> String {
        match self.browser.as_str() {
            "google-chrome" | "google_chrome" => "chrome".to_owned(),
            "microsoft-edge" | "msedge" => "edge".to_owned(),
            "opera-gx" | "opera gx" => "opera_gx".to_owned(),
            other => other.to_owned(),
        }
    }

    /// Returns `true` if the selected browser belongs to the Firefox/Gecko family.
    #[must_use]
    pub fn is_gecko_family(&self) -> bool {
        let id = self.canonical_browser_id();
        GECKO_BROWSERS.contains(&id.as_str())
    }
}

/// Parser and loader for Netscape-format `cookies.txt` files.
pub struct NetscapeCookieFile;

impl NetscapeCookieFile {
    /// Parses Netscape `cookies.txt` content into a [`CookieJar`].
    pub fn parse_str(contents: &str, source_label: &str) -> Result<CookieJar, CookieError> {
        let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
        let mut cookies = Vec::new();
        for (idx, raw_line) in contents.lines().enumerate() {
            let line_num = idx + 1;
            let trimmed_start = raw_line.trim_start();
            if trimmed_start.is_empty() {
                continue;
            }

            let (http_only, entry_line) =
                if let Some(rest) = strip_prefix_ignore_ascii_case(trimmed_start, "#HttpOnly_") {
                    (true, rest)
                } else if trimmed_start.starts_with('#') {
                    continue;
                } else {
                    (false, raw_line)
                };

            let fields: Vec<&str> = entry_line.split('\t').collect();
            if fields.len() != 7 {
                return Err(CookieError::new(
                    source_label,
                    format!(
                        "line {line_num}: expected 7 tab-separated Netscape cookie fields, found {}",
                        fields.len()
                    ),
                ));
            }

            let domain = fields[0].trim();
            if domain.is_empty() || domain.chars().any(char::is_whitespace) {
                return Err(CookieError::new(
                    source_label,
                    format!("line {line_num}: invalid cookie domain"),
                ));
            }

            let include_subdomains =
                parse_netscape_bool(fields[1], source_label, line_num, "flag")?;
            let path = fields[2].trim();
            if path.is_empty() || !path.starts_with('/') {
                return Err(CookieError::new(
                    source_label,
                    format!("line {line_num}: invalid cookie path"),
                ));
            }
            let secure = parse_netscape_bool(fields[3], source_label, line_num, "secure")?;

            let expires_raw = fields[4].trim();
            let expires_secs = expires_raw.parse::<u64>().map_err(|_| {
                CookieError::new(
                    source_label,
                    format!("line {line_num}: invalid expiration timestamp"),
                )
            })?;
            let expires = if expires_secs == 0 {
                None
            } else {
                Some(Timestamp::from_unix_seconds(
                    i64::try_from(expires_secs).unwrap_or(i64::MAX),
                ))
            };

            let name = fields[5].trim();
            if name.is_empty() || name.chars().any(char::is_whitespace) {
                return Err(CookieError::new(
                    source_label,
                    format!("line {line_num}: invalid cookie name"),
                ));
            }
            // Strip only trailing carriage return (`.lines()` handles `\r\n`), preserve value verbatim.
            let value = fields[6].to_owned();

            cookies.push(Cookie {
                domain: domain.to_owned(),
                include_subdomains,
                path: path.to_owned(),
                secure,
                expires,
                http_only,
                name: name.to_owned(),
                value,
            });
        }

        Ok(CookieJar::from_cookies(cookies))
    }

    /// Reads and parses a Netscape-format cookie file from disk.
    pub fn load_file(path: &Path) -> Result<CookieJar, CookieError> {
        let label = format!("cookie file {}", path.display());
        let metadata = fs::metadata(path)
            .map_err(|err| CookieError::new(&label, format!("cannot access cookie file: {err}")))?;
        if !metadata.is_file() {
            return Err(CookieError::new(
                &label,
                "cookie path is not a regular file",
            ));
        }
        let contents = fs::read_to_string(path)
            .map_err(|err| CookieError::new(&label, format!("cannot read cookie file: {err}")))?;
        Self::parse_str(&contents, &label)
    }
}

/// Validates a [`CookieSource`] without necessarily performing full browser decryption.
///
/// For file sources, verifies the file exists, is readable, and conforms to Netscape format.
/// For browser profiles, validates selector syntax (`BROWSER[+KEYRING][:PROFILE][::CONTAINER]`).
pub fn validate_cookie_source(source: &CookieSource) -> Result<(), CookieError> {
    match source {
        CookieSource::File(path) => {
            NetscapeCookieFile::load_file(path)?;
            Ok(())
        }
        CookieSource::BrowserProfile(spec) => {
            BrowserProfileSpec::parse(spec)?;
            Ok(())
        }
    }
}

/// Loads all non-expired cookies from `source` at the current system time.
pub fn load_cookies(source: &CookieSource) -> Result<CookieJar, CookieError> {
    load_cookies_at(source, Timestamp::now())
}

/// Loads all cookies from `source` that are valid (unexpired) as of `now`.
pub fn load_cookies_at(source: &CookieSource, now: Timestamp) -> Result<CookieJar, CookieError> {
    let jar = match source {
        CookieSource::File(path) => NetscapeCookieFile::load_file(path)?,
        CookieSource::BrowserProfile(raw) => load_browser_cookies(source, raw)?,
    };

    let valid = jar
        .into_cookies()
        .into_iter()
        .filter(|cookie| cookie.expires.is_none_or(|exp| exp > now))
        .collect();
    Ok(CookieJar::from_cookies(valid))
}

fn load_browser_cookies(source: &CookieSource, raw: &str) -> Result<CookieJar, CookieError> {
    let spec = BrowserProfileSpec::parse(raw)?;
    let browser_id = spec.canonical_browser_id();

    let mut request = ReadRequest::browser(&browser_id)
        .include_session()
        .include_expired(true);
    if let Some(profile) = &spec.profile {
        request = request.profile(profile);
    }

    match rookie_cookies::read(request) {
        Ok(snapshot) => {
            let profile_dir = if spec.container.is_some() {
                resolve_discovered_profile_dir(
                    &browser_id,
                    snapshot.profile_id(),
                    spec.profile.as_deref(),
                )
            } else {
                None
            };
            filter_detailed_cookies(
                source,
                &spec,
                snapshot.into_detailed_cookies(),
                profile_dir.as_deref(),
            )
        }
        Err(rookie_cookies::Error::Request(RequestError::UnknownProfile { .. }))
            if spec
                .profile
                .as_deref()
                .is_some_and(|p| Path::new(p).exists()) =>
        {
            let profile_path = Path::new(spec.profile.as_deref().unwrap_or_default());
            load_browser_cookies_from_path(source, &spec, &browser_id, profile_path)
        }
        Err(err) => Err(CookieError::for_source(source, err.to_string())),
    }
}

fn load_browser_cookies_from_path(
    source: &CookieSource,
    spec: &BrowserProfileSpec,
    browser_id: &str,
    path: &Path,
) -> Result<CookieJar, CookieError> {
    let (db_path, profile_dir) = resolve_explicit_db_path(source, spec, browser_id, path)?;
    let snapshot = read_explicit_db_snapshot(source, browser_id, &db_path)?;
    filter_detailed_cookies(
        source,
        spec,
        snapshot.into_detailed_cookies(),
        profile_dir.as_deref(),
    )
}

fn resolve_explicit_db_path(
    source: &CookieSource,
    spec: &BrowserProfileSpec,
    browser_id: &str,
    path: &Path,
) -> Result<(PathBuf, Option<PathBuf>), CookieError> {
    if path.is_file() {
        return Ok((path.to_path_buf(), path.parent().map(Path::to_path_buf)));
    }
    if !path.is_dir() {
        return Err(CookieError::for_source(
            source,
            format!(
                "profile path '{}' is neither a file nor a directory",
                path.display()
            ),
        ));
    }

    let candidates: &[&str] = if spec.is_gecko_family() {
        &["cookies.sqlite"]
    } else if browser_id == "safari" {
        &["Cookies.binarycookies", "Cookies"]
    } else {
        &["Network/Cookies", "Cookies"]
    };

    for rel in candidates {
        let candidate = path.join(rel);
        if candidate.is_file() {
            return Ok((candidate, Some(path.to_path_buf())));
        }
    }

    Err(CookieError::for_source(
        source,
        format!(
            "no cookie database found inside profile directory '{}'",
            path.display()
        ),
    ))
}

fn read_explicit_db_snapshot(
    source: &CookieSource,
    browser_id: &str,
    db_path: &Path,
) -> Result<rookie_cookies::ReadResult, CookieError> {
    let base_req = FromPathRequest::new(db_path).include_expired(true);
    match rookie_cookies::from_path(base_req) {
        Ok(snapshot) => Ok(snapshot),
        Err(rookie_cookies::Error::Source(DirectPathError::InvalidOptions {
            reason:
                InvalidDirectPathOptionsReason::MissingChromiumCredentials
                | InvalidDirectPathOptionsReason::MissingLocalStateFile,
            ..
        })) => read_explicit_chromium_with_fallback(source, browser_id, db_path),
        Err(err) => Err(CookieError::for_source(source, err.to_string())),
    }
}

#[cfg(unix)]
fn read_explicit_chromium_with_fallback(
    source: &CookieSource,
    browser_id: &str,
    db_path: &Path,
) -> Result<rookie_cookies::ReadResult, CookieError> {
    let browser_req = FromPathRequest::new(db_path)
        .include_expired(true)
        .chromium_browser_id(browser_id);
    match rookie_cookies::from_path(browser_req) {
        Ok(snapshot) => Ok(snapshot),
        Err(_) => rookie_cookies::from_path(
            FromPathRequest::new(db_path)
                .include_expired(true)
                .chromium_plaintext(),
        )
        .map_err(|err| CookieError::for_source(source, err.to_string())),
    }
}

#[cfg(windows)]
fn read_explicit_chromium_with_fallback(
    source: &CookieSource,
    _browser_id: &str,
    db_path: &Path,
) -> Result<rookie_cookies::ReadResult, CookieError> {
    if let Some(local_state) = find_windows_local_state(db_path) {
        let req = FromPathRequest::new(db_path)
            .include_expired(true)
            .chromium_local_state(local_state);
        if let Ok(snapshot) = rookie_cookies::from_path(req) {
            return Ok(snapshot);
        }
    }
    rookie_cookies::from_path(
        FromPathRequest::new(db_path)
            .include_expired(true)
            .chromium_plaintext(),
    )
    .map_err(|err| CookieError::for_source(source, err.to_string()))
}

#[cfg(not(any(unix, windows)))]
fn read_explicit_chromium_with_fallback(
    source: &CookieSource,
    _browser_id: &str,
    db_path: &Path,
) -> Result<rookie_cookies::ReadResult, CookieError> {
    rookie_cookies::from_path(
        FromPathRequest::new(db_path)
            .include_expired(true)
            .chromium_plaintext(),
    )
    .map_err(|err| CookieError::for_source(source, err.to_string()))
}

#[cfg(windows)]
fn find_windows_local_state(db_path: &Path) -> Option<PathBuf> {
    // Handles both `User Data/Default/Cookies` and `User Data/Default/Network/Cookies`.
    let mut dir = db_path.parent();
    for _ in 0..3 {
        let current = dir?;
        let candidate = current.join("Local State");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}

fn resolve_discovered_profile_dir(
    browser_id: &str,
    profile_id: Option<&str>,
    profile_query: Option<&str>,
) -> Option<PathBuf> {
    let profiles = rookie_cookies::profiles(browser_id).ok()?;
    if let Some(pid) = profile_id
        && let Some(found) = profiles
            .iter()
            .find(|p| p.profile.profile_id.as_str() == pid)
    {
        return Some(PathBuf::from(&found.profile.path));
    }
    if let Some(query) = profile_query {
        let as_path = Path::new(query);
        if as_path.is_dir() {
            return Some(as_path.to_path_buf());
        }
    }
    profiles
        .into_iter()
        .find(|p| p.is_default)
        .map(|p| PathBuf::from(p.profile.path))
}

fn filter_detailed_cookies(
    source: &CookieSource,
    spec: &BrowserProfileSpec,
    detailed: Vec<rookie_cookies::common::enums::DetailedCookie>,
    profile_dir: Option<&Path>,
) -> Result<CookieJar, CookieError> {
    let target_container_id: Option<u32> = match &spec.container {
        None => None,
        Some(container_name) => {
            if container_name.eq_ignore_ascii_case("none") {
                Some(0)
            } else {
                Some(resolve_firefox_container_id(
                    source,
                    profile_dir,
                    container_name,
                )?)
            }
        }
    };

    let mut out = Vec::with_capacity(detailed.len());
    for entry in detailed {
        if spec.is_gecko_family() {
            let cookie_ctx_id = entry.context.user_context_id.unwrap_or(0);
            match target_container_id {
                None | Some(0) => {
                    if cookie_ctx_id != 0 {
                        continue;
                    }
                }
                Some(expected_id) => {
                    if cookie_ctx_id != expected_id {
                        continue;
                    }
                }
            }
        }

        // Skip private-browsing isolated cookies if any leaked into session stores.
        if entry.context.private_browsing_id.is_some_and(|id| id > 0) {
            continue;
        }

        let c = entry.cookie;
        let domain = c.domain;
        let include_subdomains = domain.starts_with('.');
        let expires = c
            .expires
            .map(|secs| Timestamp::from_unix_seconds(i64::try_from(secs).unwrap_or(i64::MAX)));

        out.push(Cookie {
            domain,
            include_subdomains,
            path: if c.path.is_empty() {
                "/".to_owned()
            } else {
                c.path
            },
            secure: c.secure,
            expires,
            http_only: c.http_only,
            name: c.name,
            value: c.value,
        });
    }

    Ok(CookieJar::from_cookies(out))
}

#[derive(Debug, Deserialize)]
struct FirefoxContainersFile {
    #[serde(default)]
    identities: Vec<FirefoxContainerIdentity>,
}

#[derive(Debug, Deserialize)]
struct FirefoxContainerIdentity {
    #[serde(rename = "userContextId")]
    user_context_id: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "l10nId", alias = "l10nID")]
    l10n_id: Option<String>,
}

fn resolve_firefox_container_id(
    source: &CookieSource,
    profile_dir: Option<&Path>,
    container_name: &str,
) -> Result<u32, CookieError> {
    let dir = profile_dir.ok_or_else(|| {
        CookieError::for_source(
            source,
            format!(
                "cannot resolve Firefox container '{container_name}' without a profile directory"
            ),
        )
    })?;
    let containers_path = dir.join("containers.json");
    let contents = fs::read_to_string(&containers_path).map_err(|err| {
        CookieError::for_source(
            source,
            format!(
                "cannot read Firefox containers.json at '{}': {err}",
                containers_path.display()
            ),
        )
    })?;
    let parsed: FirefoxContainersFile = serde_json::from_str(&contents).map_err(|err| {
        CookieError::for_source(
            source,
            format!("invalid Firefox containers.json format: {err}"),
        )
    })?;

    if let Some(exact) = parsed
        .identities
        .iter()
        .find(|id| resolved_container_name(id) == Some(container_name))
    {
        return Ok(exact.user_context_id);
    }

    let ci_matches: Vec<&FirefoxContainerIdentity> = parsed
        .identities
        .iter()
        .filter(|id| {
            resolved_container_name(id).is_some_and(|n| n.eq_ignore_ascii_case(container_name))
        })
        .collect();
    if ci_matches.len() == 1 {
        return Ok(ci_matches[0].user_context_id);
    }

    if let Ok(numeric_id) = container_name.parse::<u32>()
        && numeric_id > 0
        && parsed
            .identities
            .iter()
            .any(|id| id.user_context_id == numeric_id)
    {
        return Ok(numeric_id);
    }

    Err(CookieError::for_source(
        source,
        format!("Firefox container '{container_name}' not found"),
    ))
}

fn resolved_container_name(identity: &FirefoxContainerIdentity) -> Option<&str> {
    if let Some(name) = identity.name.as_deref()
        && !name.is_empty()
    {
        return Some(name);
    }
    match identity.l10n_id.as_deref() {
        Some("user-context-personal") => Some("Personal"),
        Some("user-context-work") => Some("Work"),
        Some("user-context-banking") => Some("Banking"),
        Some("user-context-shopping") => Some("Shopping"),
        _ => None,
    }
}

fn parse_netscape_bool(
    raw: &str,
    source_label: &str,
    line_num: usize,
    field: &str,
) -> Result<bool, CookieError> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("TRUE") {
        Ok(true)
    } else if trimmed.eq_ignore_ascii_case("FALSE") {
        Ok(false)
    } else {
        Err(CookieError::new(
            source_label,
            format!("line {line_num}: invalid Netscape boolean for {field}"),
        ))
    }
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}

fn domain_matches(cookie_domain: &str, include_subdomains: bool, req_host: &str) -> bool {
    let norm = cookie_domain.trim_start_matches('.').to_ascii_lowercase();
    if norm.is_empty() {
        return false;
    }
    if req_host == norm {
        return true;
    }
    if !(include_subdomains || cookie_domain.starts_with('.')) {
        return false;
    }
    req_host.len() > norm.len()
        && req_host.ends_with(&norm)
        && req_host.as_bytes()[req_host.len() - norm.len() - 1] == b'.'
}

fn path_matches(cookie_path: &str, req_path: &str) -> bool {
    if req_path == cookie_path {
        return true;
    }
    if let Some(rest) = req_path.strip_prefix(cookie_path) {
        return cookie_path.ends_with('/') || rest.starts_with('/');
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_netscape_cookies_including_httponly_and_session() {
        let data = "\
# Netscape HTTP Cookie File
# Comment line

.bilibili.com\tTRUE\t/\tTRUE\t4102444800\tbuvid3\tBUVID3-VAL
#HttpOnly_.bilibili.com\tTRUE\t/\tTRUE\t4102444800\tSESSDATA\tSECRET-TOKEN
api.bilibili.com\tFALSE\t/x/space\tFALSE\t0\tbili_jct\tCSRF123
.example.com\tTRUE\t/\tTRUE\t4102444800\tother\tOTHER
.bilibili.com\tTRUE\t/\tTRUE\t1000\texpired\tOLD
";

        let jar = NetscapeCookieFile::parse_str(data, "test cookies.txt").unwrap();
        assert_eq!(jar.len(), 5);
        assert!(jar.cookies()[1].http_only);
        assert_eq!(jar.cookies()[2].expires, None);

        let now = Timestamp::from_unix_seconds(1_700_000_000);
        let header = jar
            .cookie_header_for_url("https://api.bilibili.com/x/space/wbi/acc/info", now)
            .unwrap();
        assert!(header.contains("bili_jct=CSRF123"));
        assert!(header.contains("SESSDATA=SECRET-TOKEN"));
        assert!(header.contains("buvid3=BUVID3-VAL"));
        assert!(!header.contains("other=OTHER"));
        assert!(!header.contains("expired=OLD"));

        // Non-HTTPS skips `secure = TRUE` cookies.
        let http_header = jar
            .cookie_header_for_url("http://api.bilibili.com/x/space/wbi/acc/info", now)
            .unwrap();
        assert_eq!(http_header, "bili_jct=CSRF123");

        // Subdomain matching must not match lookalike suffix domains.
        assert!(
            jar.cookie_header_for_url("https://evilbilibili.com/x/space", now)
                .is_none()
        );
    }

    #[test]
    fn cookie_debug_redacts_secret_values() {
        let cookie = Cookie {
            domain: ".bilibili.com".to_owned(),
            include_subdomains: true,
            path: "/".to_owned(),
            secure: true,
            expires: None,
            http_only: true,
            name: "SESSDATA".to_owned(),
            value: "SUPER_SECRET_VALUE".to_owned(),
        };
        let rendered = format!("{cookie:?}");
        assert!(!rendered.contains("SUPER_SECRET_VALUE"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("SESSDATA"));
    }

    #[test]
    fn rejects_malformed_netscape_without_leaking_values() {
        let bad = ".bilibili.com\tTRUE\t/\tTRUE\t4102444800\tSESSDATA\n";
        let err = NetscapeCookieFile::parse_str(bad, "bad.txt").unwrap_err();
        assert!(err.reason.contains("expected 7 tab-separated"));
        assert!(!err.reason.contains("SESSDATA"));
    }

    #[test]
    fn parses_browser_profile_selectors() {
        let simple = BrowserProfileSpec::parse("firefox").unwrap();
        assert_eq!(simple.browser, "firefox");
        assert_eq!(simple.keyring, None);
        assert_eq!(simple.profile, None);
        assert_eq!(simple.container, None);

        let full = BrowserProfileSpec::parse("firefox:default-release::Work").unwrap();
        assert_eq!(full.browser, "firefox");
        assert_eq!(full.profile.as_deref(), Some("default-release"));
        assert_eq!(full.container.as_deref(), Some("Work"));

        let container_only = BrowserProfileSpec::parse("firefox::Personal").unwrap();
        assert_eq!(container_only.browser, "firefox");
        assert_eq!(container_only.profile, None);
        assert_eq!(container_only.container.as_deref(), Some("Personal"));

        let chrome = BrowserProfileSpec::parse("chrome+gnomekeyring:Profile 1").unwrap();
        assert_eq!(chrome.browser, "chrome");
        assert_eq!(chrome.keyring.as_deref(), Some("gnomekeyring"));
        assert_eq!(chrome.profile.as_deref(), Some("Profile 1"));

        let win_path = BrowserProfileSpec::parse(r"firefox:C:\Users\alice\profile::Work").unwrap();
        assert_eq!(win_path.profile.as_deref(), Some(r"C:\Users\alice\profile"));
        assert_eq!(win_path.container.as_deref(), Some("Work"));

        assert!(BrowserProfileSpec::parse("").is_err());
        assert!(BrowserProfileSpec::parse("firefox:").is_err());
        assert!(BrowserProfileSpec::parse("firefox::").is_err());
        assert!(BrowserProfileSpec::parse("chrome+").is_err());
        assert!(BrowserProfileSpec::parse("chrome+badkeyring").is_err());
        assert!(BrowserProfileSpec::parse("chrome::Work").is_err());
    }

    #[test]
    fn handles_utf8_bom_and_cookie_error_conversions() {
        let bom_data = "\u{feff}# Netscape HTTP Cookie File\r\n.bilibili.com\tTRUE\t/\tTRUE\t0\tbuvid3\tABC\r\n";
        let jar = NetscapeCookieFile::parse_str(bom_data, "bom.txt").unwrap();
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.cookies()[0].name, "buvid3");

        let source = CookieSource::from_browser_profile("firefox::Personal").unwrap();
        validate_cookie_source(&source).unwrap();

        let bad = CookieSource::from_browser_profile("chrome::Personal").unwrap();
        let err = validate_cookie_source(&bad).unwrap_err();
        let refresh_err: RefreshError = err.clone().into();
        let download_err: DownloadError = err.into();
        assert!(matches!(
            refresh_err,
            RefreshError::CookieSourceInvalid { .. }
        ));
        assert!(matches!(
            download_err,
            DownloadError::CookieSourceInvalid { .. }
        ));
    }
}
