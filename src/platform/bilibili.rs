//! Bilibili creator lookup, WBI request signing, and paginated upload refresh.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::cookies::{CookieJar, load_cookies};
use crate::domain::{
    CookieSource, Creator, DiscoveredUpload, FollowedCreator, Platform, Timestamp,
};
use crate::error::RefreshError;
use crate::refresh::{RefreshFeedResult, RefreshSource, ResolvedCreator};

const DEFAULT_API_BASE_URL: &str = "https://api.bilibili.com";
const DEFAULT_COOKIE_TARGET_URL: &str = "https://api.bilibili.com/x/space/wbi/arc/search";
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const DEFAULT_PAGE_SIZE: u32 = 30;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_WBI_CACHE_TTL: Duration = Duration::from_secs(600);
const MAX_PAGINATION_PAGES: u32 = 10_000;

// Default WebGL fingerprint values expected by Bilibili space WBI endpoints.
const DEFAULT_DM_IMG_LIST: &str = "[]";
const DEFAULT_DM_IMG_STR: &str = "V2ViR0wgMS4wIChPcGVuR0wgRVMgMi4wIENocm9taXVtKQ";
const DEFAULT_DM_COVER_IMG_STR: &str = "QU5HTEUgKEdvb2dsZSwgVnVsa2FuIDEuMy4wIChTd2lmdFNoYWRlciBEZXZpY2UgKFN1Ynplcm8pICgweDAwMDBDMERFKSksIFN3aWZ0U2hhZGVyIGRyaXZlcilHb29nbGUgSW5jLiAoR29vZ2xlKQ";
const DEFAULT_DM_IMG_INTER: &str = r#"{"ds":[],"wh":[0,0,0],"of":[0,0,0]}"#;

const MIXIN_KEY_ENC_TAB: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

/// Configuration for [`BilibiliRefreshSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BilibiliClientConfig {
    /// Base URL for Bilibili API requests (overrideable for offline mock servers).
    pub api_base_url: String,
    /// Canonical HTTPS URL used when matching cookies from a [`CookieJar`].
    pub cookie_target_url: String,
    /// HTTP `User-Agent` header sent on Bilibili API requests.
    pub user_agent: String,
    /// Number of uploads requested per page (`ps` parameter).
    pub page_size: u32,
    /// Global timeout for each HTTP request.
    pub request_timeout: Duration,
    /// How long fetched WBI signing keys remain cached in memory.
    pub wbi_cache_ttl: Duration,
    /// Whether to best-effort fetch a guest `buvid3` via `/x/frontend/finger/spi`
    /// when no user cookie source is provided.
    pub fetch_guest_buvid: bool,
}

impl Default for BilibiliClientConfig {
    fn default() -> Self {
        Self {
            api_base_url: DEFAULT_API_BASE_URL.to_owned(),
            cookie_target_url: DEFAULT_COOKIE_TARGET_URL.to_owned(),
            user_agent: DEFAULT_USER_AGENT.to_owned(),
            page_size: DEFAULT_PAGE_SIZE,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            wbi_cache_ttl: DEFAULT_WBI_CACHE_TTL,
            fetch_guest_buvid: true,
        }
    }
}

impl BilibiliClientConfig {
    /// Overrides the API base URL (trims trailing `/`).
    #[must_use]
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        let raw = url.into();
        self.api_base_url = raw.trim_end_matches('/').to_owned();
        self
    }

    /// Overrides the pagination page size (`ps`, clamped to `1..=50`).
    #[must_use]
    pub fn with_page_size(mut self, page_size: u32) -> Self {
        self.page_size = page_size.clamp(1, 50);
        self
    }

    /// Overrides the per-request HTTP timeout.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Enables or disables automatic guest `buvid3` lookup when unauthenticated.
    #[must_use]
    pub fn with_guest_buvid(mut self, enabled: bool) -> Self {
        self.fetch_guest_buvid = enabled;
        self
    }
}

#[derive(Debug, Clone)]
struct CachedWbiKeys {
    mixin_key: String,
    fetched_at: Instant,
}

/// Live Bilibili [`RefreshSource`] implementation backed by `ureq` and WBI signing.
#[derive(Debug)]
pub struct BilibiliRefreshSource {
    config: BilibiliClientConfig,
    agent: ureq::Agent,
    wbi_cache: Mutex<Option<CachedWbiKeys>>,
    guest_buvid3: Mutex<Option<String>>,
}

impl Default for BilibiliRefreshSource {
    fn default() -> Self {
        Self::new()
    }
}

impl BilibiliRefreshSource {
    /// Creates a [`BilibiliRefreshSource`] with default production settings.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(BilibiliClientConfig::default())
    }

    /// Creates a [`BilibiliRefreshSource`] with custom configuration (for example, an offline mock base URL).
    #[must_use]
    pub fn with_config(mut config: BilibiliClientConfig) -> Self {
        config.api_base_url = config.api_base_url.trim_end_matches('/').to_owned();
        if config.page_size == 0 {
            config.page_size = DEFAULT_PAGE_SIZE;
        }
        let agent_config = ureq::Agent::config_builder()
            .timeout_global(Some(config.request_timeout))
            .http_status_as_error(false)
            .build();
        let agent = ureq::Agent::new_with_config(agent_config);
        Self {
            config,
            agent,
            wbi_cache: Mutex::new(None),
            guest_buvid3: Mutex::new(None),
        }
    }

    fn prepare_cookie_header(
        &self,
        cookies: Option<&CookieSource>,
    ) -> Result<Option<String>, RefreshError> {
        if let Some(source) = cookies {
            let jar: CookieJar = load_cookies(source)?;
            return Ok(jar.cookie_header_for_url(&self.config.cookie_target_url, Timestamp::now()));
        }
        if !self.config.fetch_guest_buvid {
            return Ok(None);
        }
        if let Some(cached) = self
            .guest_buvid3
            .lock()
            .expect("guest buvid3 lock poisoned")
            .clone()
        {
            return Ok(Some(format!("buvid3={cached}")));
        }

        // Best-effort guest buvid3 initialization; silently skipped if unavailable (e.g. minimal mocks).
        let spi_url = format!("{}/x/frontend/finger/spi", self.config.api_base_url);
        if let Ok((200, body)) = self.raw_get(&spi_url, "https://www.bilibili.com/", None)
            && let Ok(parsed) = serde_json::from_str::<BiliEnvelope<SpiData>>(&body)
            && parsed.code == 0
            && let Some(data) = parsed.data
            && !data.b_3.trim().is_empty()
        {
            let b3 = data.b_3.trim().to_owned();
            *self
                .guest_buvid3
                .lock()
                .expect("guest buvid3 lock poisoned") = Some(b3.clone());
            return Ok(Some(format!("buvid3={b3}")));
        }
        Ok(None)
    }

    fn get_mixin_key(&self, cookie_header: Option<&str>) -> Result<(String, bool), RefreshError> {
        {
            let guard = self.wbi_cache.lock().expect("wbi cache lock poisoned");
            if let Some(cached) = guard.as_ref()
                && cached.fetched_at.elapsed() < self.config.wbi_cache_ttl
            {
                return Ok((cached.mixin_key.clone(), true));
            }
        }
        let fresh = self.fetch_fresh_mixin_key(cookie_header)?;
        Ok((fresh, false))
    }

    fn invalidate_wbi_cache(&self) {
        *self.wbi_cache.lock().expect("wbi cache lock poisoned") = None;
    }

    fn fetch_fresh_mixin_key(&self, cookie_header: Option<&str>) -> Result<String, RefreshError> {
        let nav_url = format!("{}/x/web-interface/nav", self.config.api_base_url);
        let (status, body) = self.raw_get(&nav_url, "https://www.bilibili.com/", cookie_header)?;
        if status >= 400 {
            return Err(RefreshError::PlatformResponse(format!(
                "HTTP {status} from Bilibili nav endpoint"
            )));
        }
        let env: BiliEnvelope<NavData> = serde_json::from_str(&body).map_err(|err| {
            RefreshError::PlatformResponse(format!(
                "invalid JSON from Bilibili nav endpoint: {err}"
            ))
        })?;
        // Note: `/x/web-interface/nav` returns `code = -101` when unauthenticated while still
        // providing `data.wbi_img`, so inspect `wbi_img` regardless of `-101`.
        let wbi_img = env.data.and_then(|d| d.wbi_img).ok_or_else(|| {
            RefreshError::PlatformResponse(format!(
                "missing wbi_img keys in Bilibili nav response (code {}, message: {})",
                env.code, env.message
            ))
        })?;

        let img_key = extract_wbi_key(&wbi_img.img_url)?;
        let sub_key = extract_wbi_key(&wbi_img.sub_url)?;
        let mixin_key = compute_mixin_key(&img_key, &sub_key)?;

        *self.wbi_cache.lock().expect("wbi cache lock poisoned") = Some(CachedWbiKeys {
            mixin_key: mixin_key.clone(),
            fetched_at: Instant::now(),
        });
        Ok(mixin_key)
    }

    fn raw_get(
        &self,
        url: &str,
        referer: &str,
        cookie_header: Option<&str>,
    ) -> Result<(u16, String), RefreshError> {
        let mut req = self
            .agent
            .get(url)
            .header("User-Agent", &self.config.user_agent)
            .header("Referer", referer)
            .header("Origin", "https://www.bilibili.com")
            .header("Accept", "application/json, text/plain, */*");

        if let Some(cookies) = cookie_header {
            req = req.header("Cookie", cookies);
        }

        let mut response = req
            .call()
            .map_err(|err| RefreshError::Network(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|err| RefreshError::Network(format!("failed reading response body: {err}")))?;
        Ok((status, body))
    }

    fn signed_get<T: DeserializeOwned>(
        &self,
        endpoint_path: &str,
        params: &[(&str, &str)],
        referer: &str,
        cookie_header: Option<&str>,
    ) -> Result<BiliEnvelope<T>, RefreshError> {
        let (mut mixin_key, mut used_cache) = self.get_mixin_key(cookie_header)?;
        for attempt in 0..2 {
            let wts = Timestamp::now().as_unix_seconds();
            let query = sign_wbi_params(params, &mixin_key, wts);
            let url = format!("{}{endpoint_path}?{query}", self.config.api_base_url);
            let (status, body) = self.raw_get(&url, referer, cookie_header)?;
            if status == 404 {
                return Ok(BiliEnvelope {
                    code: -404,
                    message: "HTTP 404 Not Found".to_owned(),
                    data: None,
                });
            }
            if status >= 400 {
                return Err(RefreshError::PlatformResponse(format!(
                    "HTTP {status} from Bilibili endpoint {endpoint_path}"
                )));
            }
            let envelope: BiliEnvelope<T> = serde_json::from_str(&body).map_err(|err| {
                RefreshError::PlatformResponse(format!(
                    "invalid JSON from Bilibili endpoint {endpoint_path}: {err}"
                ))
            })?;

            // If cached WBI keys became stale and triggered a risk/signature rejection,
            // refresh keys once before surfacing the error.
            if attempt == 0 && used_cache && matches!(envelope.code, -403 | -352) {
                self.invalidate_wbi_cache();
                mixin_key = self.fetch_fresh_mixin_key(cookie_header)?;
                used_cache = false;
                continue;
            }
            return Ok(envelope);
        }

        unreachable!("WBI signed request retry loop always returns in <= 2 iterations")
    }

    fn fetch_uploads_with_boundary(
        &self,
        creator: &Creator,
        followed_at: Timestamp,
        checkpoint: Timestamp,
        replay_checkpoint: bool,
        cookies: Option<&CookieSource>,
    ) -> Result<RefreshFeedResult, RefreshError> {
        if creator.platform != Platform::Bilibili {
            return Err(RefreshError::PlatformResponse(format!(
                "unsupported platform '{}' for BilibiliRefreshSource",
                creator.platform
            )));
        }
        let mid: u64 = creator
            .platform_creator_id
            .trim()
            .parse()
            .ok()
            .filter(|&id| id > 0)
            .ok_or_else(|| {
                RefreshError::PlatformResponse(format!(
                    "invalid stored Bilibili mid '{}'",
                    creator.platform_creator_id
                ))
            })?;

        let cookie_header = self.prepare_cookie_header(cookies)?;
        let referer = format!("https://space.bilibili.com/{mid}/video");
        let mid_str = mid.to_string();
        let ps = self.config.page_size.max(1);
        let ps_str = ps.to_string();

        let mut uploads = Vec::new();
        let mut seen_ids = HashSet::new();

        for pn in 1..=MAX_PAGINATION_PAGES {
            let pn_str = pn.to_string();
            let params = [
                ("mid", mid_str.as_str()),
                ("ps", ps_str.as_str()),
                ("tid", "0"),
                ("pn", pn_str.as_str()),
                ("order", "pubdate"),
                ("dm_img_list", DEFAULT_DM_IMG_LIST),
                ("dm_img_str", DEFAULT_DM_IMG_STR),
                ("dm_cover_img_str", DEFAULT_DM_COVER_IMG_STR),
                ("dm_img_inter", DEFAULT_DM_IMG_INTER),
            ];
            let envelope: BiliEnvelope<ArcSearchData> = self.signed_get(
                "/x/space/wbi/arc/search",
                &params,
                &referer,
                cookie_header.as_deref(),
            )?;

            if envelope.code != 0 {
                return Err(map_bilibili_api_error(
                    envelope.code,
                    &envelope.message,
                    cookies.is_some(),
                ));
            }

            let data = envelope.data.ok_or_else(|| {
                RefreshError::PlatformResponse(
                    "Bilibili arc/search response returned code 0 without data".to_owned(),
                )
            })?;
            let vlist = data.list.map(|l| l.vlist).unwrap_or_default();
            if vlist.is_empty() {
                break;
            }

            let mut page_had_unseen = false;
            let mut last_crossed_boundary = false;

            for item in &vlist {
                let published_at = Timestamp::from_unix_seconds(item.created);
                let upload_id = resolve_upload_id(&item.bvid, item.aid)?;
                if seen_ids.insert(upload_id.clone()) {
                    page_had_unseen = true;
                    if should_include_upload(
                        published_at,
                        followed_at,
                        checkpoint,
                        replay_checkpoint,
                    ) {
                        uploads.push(DiscoveredUpload {
                            platform: Platform::Bilibili,
                            url: format!("https://www.bilibili.com/video/{upload_id}"),
                            platform_upload_id: upload_id,
                            title: item.title.clone(),
                            published_at,
                        });
                    }
                }
            }

            if let Some(last) = vlist.last() {
                let last_ts = Timestamp::from_unix_seconds(last.created);
                last_crossed_boundary =
                    past_pagination_boundary(last_ts, followed_at, checkpoint, replay_checkpoint);
            }

            if last_crossed_boundary || !page_had_unseen {
                break;
            }

            let reached_total = match data.page {
                Some(page) if page.count > 0 => {
                    u64::from(pn).saturating_mul(u64::from(ps)) >= page.count
                }
                _ => vlist.len() < ps as usize,
            };
            if reached_total {
                break;
            }
        }

        let max_discovered = uploads
            .iter()
            .map(|upload| upload.published_at)
            .max()
            .unwrap_or(checkpoint);
        let new_checkpoint = max_discovered.max(checkpoint);

        Ok(RefreshFeedResult {
            uploads,
            new_checkpoint,
        })
    }
}

impl RefreshSource for BilibiliRefreshSource {
    fn resolve_creator(
        &self,
        url: &str,
        cookies: Option<&CookieSource>,
    ) -> Result<ResolvedCreator, RefreshError> {
        let mid = parse_creator_mid(url)?;
        let cookie_header = self.prepare_cookie_header(cookies)?;
        let mid_str = mid.to_string();
        let referer = format!("https://space.bilibili.com/{mid}");
        let params = [
            ("mid", mid_str.as_str()),
            ("dm_img_list", DEFAULT_DM_IMG_LIST),
            ("dm_img_str", DEFAULT_DM_IMG_STR),
            ("dm_cover_img_str", DEFAULT_DM_COVER_IMG_STR),
            ("dm_img_inter", DEFAULT_DM_IMG_INTER),
        ];

        let envelope: BiliEnvelope<AccInfoData> = self.signed_get(
            "/x/space/wbi/acc/info",
            &params,
            &referer,
            cookie_header.as_deref(),
        )?;

        if envelope.code == -404 {
            return Err(RefreshError::CreatorNotFoundRemote {
                url: url.to_owned(),
            });
        }
        if envelope.code != 0 {
            return Err(map_bilibili_api_error(
                envelope.code,
                &envelope.message,
                cookies.is_some(),
            ));
        }

        let info = envelope.data.ok_or_else(|| {
            RefreshError::PlatformResponse(
                "Bilibili acc/info response returned code 0 without data".to_owned(),
            )
        })?;
        let display_name = info.name.trim();
        if display_name.is_empty() {
            return Err(RefreshError::PlatformResponse(
                "Bilibili creator profile returned an empty display name".to_owned(),
            ));
        }
        let resolved_mid = info.mid.filter(|&m| m > 0).unwrap_or(mid);

        Ok(ResolvedCreator {
            platform: Platform::Bilibili,
            platform_creator_id: resolved_mid.to_string(),
            display_name: display_name.to_owned(),
        })
    }

    fn fetch_uploads_since(
        &self,
        creator: &Creator,
        checkpoint: Timestamp,
        cookies: Option<&CookieSource>,
    ) -> Result<RefreshFeedResult, RefreshError> {
        self.fetch_uploads_with_boundary(creator, checkpoint, checkpoint, false, cookies)
    }

    fn fetch_uploads_for_follow(
        &self,
        followed: &FollowedCreator,
        cookies: Option<&CookieSource>,
    ) -> Result<RefreshFeedResult, RefreshError> {
        let followed_at = followed.follow.followed_at;
        let checkpoint = followed.follow.refresh_checkpoint.max(followed_at);
        let replay_checkpoint = checkpoint > followed_at;
        self.fetch_uploads_with_boundary(
            &followed.creator,
            followed_at,
            checkpoint,
            replay_checkpoint,
            cookies,
        )
    }
}

/// Parses a Bilibili creator profile URL and returns the numeric creator UID (`mid`).
///
/// Accepts `https://space.bilibili.com/<mid>` (with optional trailing slash, sub-paths such as
/// `/video` or `/upload/video`, and query/fragment) as well as `www.bilibili.com/space/<mid>` and
/// `m.bilibili.com/space/<mid>`.
pub fn parse_creator_mid(raw_url: &str) -> Result<u64, RefreshError> {
    let trimmed = raw_url.trim();
    let parsed = Url::parse(trimmed).map_err(|err| RefreshError::InvalidCreatorUrl {
        url: raw_url.to_owned(),
        reason: format!("invalid URL syntax: {err}"),
    })?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(RefreshError::InvalidCreatorUrl {
            url: raw_url.to_owned(),
            reason: "expected http or https URL".to_owned(),
        });
    }

    let Some(host) = parsed.host_str() else {
        return Err(RefreshError::InvalidCreatorUrl {
            url: raw_url.to_owned(),
            reason: "missing URL host".to_owned(),
        });
    };
    let host_lower = host.to_ascii_lowercase();
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|segs| segs.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();

    let mid_segment = match host_lower.as_str() {
        "space.bilibili.com" => segments.first().copied(),
        "www.bilibili.com" | "m.bilibili.com" | "bilibili.com" => {
            if segments.first().copied() == Some("space") {
                segments.get(1).copied()
            } else {
                None
            }
        }
        _ => {
            return Err(RefreshError::InvalidCreatorUrl {
                url: raw_url.to_owned(),
                reason: format!("unsupported Bilibili creator host '{host}'"),
            });
        }
    };

    let Some(raw_mid) = mid_segment else {
        return Err(RefreshError::InvalidCreatorUrl {
            url: raw_url.to_owned(),
            reason: "expected creator space URL with numeric UID (for example, https://space.bilibili.com/<uid>)".to_owned(),
        });
    };

    if !raw_mid.bytes().all(|b| b.is_ascii_digit()) {
        return Err(RefreshError::InvalidCreatorUrl {
            url: raw_url.to_owned(),
            reason: format!("creator UID '{raw_mid}' is not a positive integer"),
        });
    }

    let mid = raw_mid
        .parse::<u64>()
        .map_err(|_| RefreshError::InvalidCreatorUrl {
            url: raw_url.to_owned(),
            reason: format!("creator UID '{raw_mid}' is out of range"),
        })?;

    if mid == 0 {
        return Err(RefreshError::InvalidCreatorUrl {
            url: raw_url.to_owned(),
            reason: "creator UID must be greater than 0".to_owned(),
        });
    }

    Ok(mid)
}

fn should_include_upload(
    published_at: Timestamp,
    followed_at: Timestamp,
    checkpoint: Timestamp,
    replay_checkpoint: bool,
) -> bool {
    if published_at <= followed_at {
        return false;
    }
    if replay_checkpoint {
        published_at >= checkpoint
    } else {
        published_at > checkpoint
    }
}

fn past_pagination_boundary(
    published_at: Timestamp,
    followed_at: Timestamp,
    checkpoint: Timestamp,
    replay_checkpoint: bool,
) -> bool {
    if published_at <= followed_at {
        return true;
    }
    if replay_checkpoint {
        published_at < checkpoint
    } else {
        published_at <= checkpoint
    }
}

fn resolve_upload_id(bvid: &str, aid: i64) -> Result<String, RefreshError> {
    let trimmed_bvid = bvid.trim();
    if !trimmed_bvid.is_empty() {
        return Ok(trimmed_bvid.to_owned());
    }
    if aid > 0 {
        return Ok(format!("av{aid}"));
    }
    Err(RefreshError::PlatformResponse(
        "Bilibili upload entry is missing both bvid and aid".to_owned(),
    ))
}

fn map_bilibili_api_error(code: i64, message: &str, has_cookies: bool) -> RefreshError {
    let msg = if message.trim().is_empty() {
        "unknown error"
    } else {
        message.trim()
    };
    let hint = if !has_cookies && matches!(code, -352 | -101 | -401 | -403 | -412) {
        " (try supplying Bilibili cookies via --cookies or --cookies-from-browser)"
    } else {
        ""
    };
    RefreshError::PlatformResponse(format!("Bilibili API error {code}: {msg}{hint}"))
}

fn extract_wbi_key(raw_url: &str) -> Result<String, RefreshError> {
    let filename = raw_url
        .rsplit('/')
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("");
    let stem = filename
        .rsplit_once('.')
        .map_or(filename, |(stem, _ext)| stem);
    if stem.is_empty() {
        return Err(RefreshError::PlatformResponse(format!(
            "cannot extract WBI key from '{raw_url}'"
        )));
    }
    Ok(stem.to_owned())
}

/// Derives the 32-character WBI mixin key from `img_key` and `sub_key`.
pub(crate) fn compute_mixin_key(img_key: &str, sub_key: &str) -> Result<String, RefreshError> {
    let combined = format!("{img_key}{sub_key}");
    let bytes = combined.as_bytes();
    let mixin: String = MIXIN_KEY_ENC_TAB
        .iter()
        .filter_map(|&idx| bytes.get(idx).map(|&b| b as char))
        .take(32)
        .collect();
    if mixin.len() < 32 {
        return Err(RefreshError::PlatformResponse(
            "Bilibili WBI keys were too short to derive mixin key".to_owned(),
        ));
    }
    Ok(mixin)
}

/// Signs sorted query parameters with `wts` and MD5 `w_rid`.
pub(crate) fn sign_wbi_params(params: &[(&str, &str)], mixin_key: &str, wts: i64) -> String {
    let mut sorted: BTreeMap<&str, String> = BTreeMap::new();
    for &(k, v) in params {
        sorted.insert(k, filter_wbi_value(v));
    }
    sorted.insert("wts", wts.to_string());

    let encoded_query = sorted
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                percent_encode_rfc3986(k),
                percent_encode_rfc3986(v)
            )
        })
        .collect::<Vec<_>>()
        .join("&");

    let mut hasher = Md5::new();
    hasher.update(encoded_query.as_bytes());
    hasher.update(mixin_key.as_bytes());
    let digest = hasher.finalize();
    let w_rid = format!("{digest:x}");

    format!("{encoded_query}&w_rid={w_rid}")
}

fn filter_wbi_value(raw: &str) -> String {
    raw.chars()
        .filter(|c| !matches!(c, '!' | '\'' | '(' | ')' | '*'))
        .collect()
}

fn percent_encode_rfc3986(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            other => {
                out.push('%');
                out.push(HEX[(other >> 4) as usize] as char);
                out.push(HEX[(other & 0x0F) as usize] as char);
            }
        }
    }
    out
}

#[derive(Debug, Deserialize)]
struct BiliEnvelope<T> {
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default = "none_option")]
    data: Option<T>,
}

fn none_option<T>() -> Option<T> {
    None
}

#[derive(Debug, Deserialize)]
struct SpiData {
    #[serde(default)]
    b_3: String,
}

#[derive(Debug, Deserialize)]
struct NavData {
    #[serde(default)]
    wbi_img: Option<WbiImg>,
}

#[derive(Debug, Deserialize)]
struct WbiImg {
    img_url: String,
    sub_url: String,
}

#[derive(Debug, Deserialize)]
struct AccInfoData {
    #[serde(default)]
    mid: Option<u64>,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct ArcSearchData {
    #[serde(default)]
    list: Option<ArcSearchList>,
    #[serde(default)]
    page: Option<ArcSearchPage>,
}

#[derive(Debug, Deserialize)]
struct ArcSearchList {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    vlist: Vec<ArcVideoItem>,
}

#[derive(Debug, Deserialize)]
struct ArcSearchPage {
    #[serde(default)]
    count: u64,
}

#[derive(Debug, Deserialize)]
struct ArcVideoItem {
    #[serde(default)]
    aid: i64,
    #[serde(default)]
    bvid: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    created: i64,
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let opt = Option::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_and_invalid_bilibili_creator_urls() {
        assert_eq!(
            parse_creator_mid("https://space.bilibili.com/946974").unwrap(),
            946974
        );
        assert_eq!(
            parse_creator_mid("http://space.bilibili.com/12345/video?spm_id_from=333").unwrap(),
            12345
        );
        assert_eq!(
            parse_creator_mid("https://space.bilibili.com/42/upload/video").unwrap(),
            42
        );
        assert_eq!(
            parse_creator_mid("https://m.bilibili.com/space/777").unwrap(),
            777
        );
        assert_eq!(
            parse_creator_mid("https://www.bilibili.com/space/888/").unwrap(),
            888
        );

        assert!(parse_creator_mid("https://www.bilibili.com/video/BV1xx411c7mD").is_err());
        assert!(parse_creator_mid("https://space.bilibili.com/").is_err());
        assert!(parse_creator_mid("https://space.bilibili.com/0").is_err());
        assert!(parse_creator_mid("https://space.bilibili.com/abc").is_err());
        assert!(parse_creator_mid("https://space.bilibili.com.evil.com/123").is_err());
        assert!(parse_creator_mid("ftp://space.bilibili.com/123").is_err());
        assert!(parse_creator_mid("not a url").is_err());
    }

    #[test]
    fn computes_official_wbi_mixin_key_and_signature() {
        let img_key =
            extract_wbi_key("https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png")
                .unwrap();
        let sub_key =
            extract_wbi_key("https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png")
                .unwrap();
        let mixin_key = compute_mixin_key(&img_key, &sub_key).unwrap();
        assert_eq!(mixin_key, "ea1db124af3c7062474693fa704f4ff8");

        let signed = sign_wbi_params(
            &[("foo", "114"), ("bar", "514"), ("baz", "1919810!'()*")],
            &mixin_key,
            1_702_204_169,
        );
        assert_eq!(
            signed,
            "bar=514&baz=1919810&foo=114&wts=1702204169&w_rid=6149fdadf571698ca7e6a567265cd0ee"
        );

        let encoded_space = sign_wbi_params(&[("q", "hello world!")], &mixin_key, 1_702_204_169);
        assert!(encoded_space.starts_with("q=hello%20world&wts=1702204169&w_rid="));
    }
}
