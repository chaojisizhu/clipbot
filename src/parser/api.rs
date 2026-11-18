use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use url::Url;

use crate::parser::channels::parse_channels_share;
use crate::parser::http::{SiteAccessRestricted, SiteHttp, SiteJsonUnavailable, validate_site_url};
use crate::parser::jmcomic::download_jm_album;
use crate::parser::matcher::clean_url;
use crate::parser::sites_core::BilibiliMediaUnavailable;
use crate::parser::sites_page::{DouyinMediaUnavailable, XhsMediaUnavailable};
use crate::parser::twitter::TwitterMediaUnavailable;
use crate::parser::{sites_core, sites_extra, sites_other, sites_page, twitter};
use crate::types::{AppConfig, ParsedData};

#[derive(Debug, Serialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "status", content = "data", rename_all = "lowercase")]
pub enum ParseResult {
    Success(ParsedData),
    Failure(String),
}

impl ParseResult {
    pub fn success(self) -> Option<ParsedData> {
        match self {
            Self::Success(data) => Some(data),
            Self::Failure(_) => None,
        }
    }

    pub fn failure_message(&self) -> Option<&str> {
        match self {
            Self::Success(_) => None,
            Self::Failure(message) => Some(message),
        }
    }
}

struct CacheEntry {
    value: CacheValue,
    expires: Instant,
    last_used: Instant,
    weight: usize,
}

#[derive(Clone)]
enum CacheValue {
    Media(Box<ParsedData>),
    DouyinRedirect(String),
    DouyinUnavailable(DouyinMediaUnavailable),
}

pub struct VideoParserService {
    config: AppConfig,
    http: SiteHttp,
    semaphore: Semaphore,
    cache: Mutex<HashMap<String, CacheEntry>>,
    // Fixed lock stripes coalesce concurrent requests without growing a pending-key map.
    key_locks: [AsyncMutex<()>; 64],
    // The mutex serializes authenticated X extraction; its value is a rate-limit cooldown.
    twitter_auth_gate: AsyncMutex<Instant>,
}

impl VideoParserService {
    pub fn new(config: AppConfig) -> Result<Self> {
        Ok(Self {
            http: SiteHttp::new(&config)?,
            semaphore: Semaphore::new(config.api.max_concurrency.clamp(1, 16) as usize),
            config,
            cache: Mutex::new(HashMap::new()),
            key_locks: std::array::from_fn(|_| AsyncMutex::new(())),
            twitter_auth_gate: AsyncMutex::new(Instant::now()),
        })
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    fn cached(&self, key: &str) -> Option<CacheValue> {
        let mut cache = self.cache.lock().ok()?;
        let now = Instant::now();
        cache.retain(|_, entry| entry.expires > now);
        let entry = cache.get_mut(key)?;
        entry.last_used = now;
        Some(entry.value.clone())
    }

    fn cache_result(&self, key: String, data: &ParsedData) {
        if data.attachment.is_none() {
            self.cache_value(key, CacheValue::Media(Box::new(data.clone())), 300);
        }
    }

    fn cache_value(&self, key: String, value: CacheValue, max_ttl_seconds: u64) {
        let ttl_seconds = self.config.resources.cache_ttl_seconds.min(max_ttl_seconds);
        if self.config.resources.cache_entries == 0 || ttl_seconds == 0 {
            return;
        }
        let value_weight = match &value {
            CacheValue::Media(data) => serde_json::to_vec(data)
                .map(|bytes| bytes.len())
                .unwrap_or(usize::MAX),
            CacheValue::DouyinRedirect(url) => url.len(),
            CacheValue::DouyinUnavailable(_) => 0,
        };
        let weight = value_weight.saturating_add(key.len());
        let budget = self.config.resources.max_cache_bytes.min(64 * 1024 * 1024);
        if weight > budget {
            return;
        }
        if let Ok(mut cache) = self.cache.lock() {
            let now = Instant::now();
            cache.retain(|_, entry| entry.expires > now);
            let mut total = cache.values().map(|entry| entry.weight).sum::<usize>();
            while !cache.is_empty()
                && (cache.len() >= self.config.resources.cache_entries.min(4096)
                    || total.saturating_add(weight) > budget)
            {
                let oldest = cache
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(key, _)| key.clone())
                    .unwrap();
                if let Some(removed) = cache.remove(&oldest) {
                    total = total.saturating_sub(removed.weight);
                }
            }
            cache.insert(
                key,
                CacheEntry {
                    value,
                    expires: now + Duration::from_secs(ttl_seconds),
                    last_used: now,
                    weight,
                },
            );
        }
    }

    fn cached_result(&self, key: &str) -> Option<Result<ParsedData>> {
        match self.cached(key)? {
            CacheValue::Media(data) => Some(Ok(*data)),
            CacheValue::DouyinUnavailable(error) => Some(Err(error.into())),
            CacheValue::DouyinRedirect(_) => None,
        }
    }

    fn key_lock(&self, key: &str) -> &AsyncMutex<()> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        &self.key_locks[hasher.finish() as usize % self.key_locks.len()]
    }

    pub async fn parse_url(&self, url: &str, platform: &str) -> ParseResult {
        match self.parse(url, platform).await {
            Ok(data) => ParseResult::Success(data),
            Err(error) => ParseResult::Failure(error.to_string()),
        }
    }

    async fn parse(&self, url: &str, platform: &str) -> Result<ParsedData> {
        if !self.config.platform_enabled(platform) {
            bail!("平台 {platform} 已禁用");
        }
        if url.len() > 8192 {
            bail!("链接长度超过上限");
        }
        if platform != "jmcomic" {
            validate_site_url(url, platform)?;
        }
        let timeout_ms = if platform == "jmcomic" {
            self.config.jmcomic.timeout.clamp(1000, 600_000)
        } else {
            (self.config.api.timeout.saturating_mul(3)
                + if self.config.ytdlp.enabled {
                    self.config.ytdlp.timeout
                } else {
                    0
                })
            .clamp(1000, 300_000)
        };
        let timeout = Duration::from_millis(timeout_ms);
        if platform == "douyin" && is_short_link(url) {
            return tokio::time::timeout(timeout, async {
                let resolved = self.resolve_douyin_short(url).await?;
                self.parse_resolved(&resolved, platform, None).await
            })
            .await
            .map_err(|_| anyhow::anyhow!("{platform} 解析超时"))?;
        }
        self.parse_resolved(url, platform, Some(timeout)).await
    }

    async fn parse_resolved(
        &self,
        url: &str,
        platform: &str,
        timeout: Option<Duration>,
    ) -> Result<ParsedData> {
        let key = parse_cache_key(url, platform);
        if let Some(result) = self.cached_result(&key) {
            return result;
        }
        let _key_guard = self.key_lock(&key).lock().await;
        if let Some(result) = self.cached_result(&key) {
            return result;
        }
        let _permit = self.semaphore.acquire().await?;
        let request = self.parse_uncached(url, platform);
        let result = if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, request)
                .await
                .map_err(|_| anyhow::anyhow!("{platform} 解析超时"))?
        } else {
            request.await
        };
        let mut data = match result {
            Ok(data) => data,
            Err(error) => {
                if let Some(reason) = error.downcast_ref::<DouyinMediaUnavailable>() {
                    self.cache_value(key, CacheValue::DouyinUnavailable(*reason), 30);
                }
                return Err(error);
            }
        };
        if !data.has_media() && data.attachment.is_none() {
            bail!("{platform} 未找到可用媒体，请补充登录 Cookie 或检查链接访问权限");
        }
        data.desc = data
            .desc
            .chars()
            .take(self.config.output.max_desc_length.min(16_384))
            .collect();
        if data.media_headers.is_empty()
            && let Ok(url) = Url::parse(url)
        {
            data.media_headers.insert(
                "Referer".into(),
                format!("{}/", url.origin().ascii_serialization()),
            );
        }
        self.cache_result(key, &data);
        Ok(data)
    }

    async fn resolve_douyin_short(&self, url: &str) -> Result<String> {
        let key = format!("douyin-short:{}", clean_url(url));
        if let Some(CacheValue::DouyinRedirect(resolved)) = self.cached(&key) {
            return Ok(resolved);
        }
        let _short_guard = self.key_lock(&key).lock().await;
        if let Some(CacheValue::DouyinRedirect(resolved)) = self.cached(&key) {
            return Ok(resolved);
        }
        let _permit = self.semaphore.acquire().await?;
        let resolved = self.http.resolve_douyin_short(url).await?;
        self.cache_value(key, CacheValue::DouyinRedirect(resolved.clone()), 300);
        Ok(resolved)
    }

    async fn parse_uncached(&self, url: &str, platform: &str) -> Result<ParsedData> {
        if platform == "channels" {
            if !self.config.channels.enabled {
                bail!("视频号解析未启用");
            }
            return parse_channels_share(url, &self.config.channels).await;
        }
        if platform == "jmcomic" {
            if !self.config.jmcomic.enabled {
                bail!("JM 下载未启用");
            }
            static JM: Lazy<Regex> =
                Lazy::new(|| Regex::new(r"^jm://(album|photo)/(\d+)$").unwrap());
            let capture = JM
                .captures(url)
                .ok_or_else(|| anyhow::anyhow!("JM 链接格式无效"))?;
            return download_jm_album(&capture[2], &capture[1] == "photo", &self.config.jmcomic)
                .await;
        }
        if platform == "twitter"
            && let Some(cookie_file) = authenticated_twitter_cookie_file(&self.config)
        {
            return self
                .parse_twitter_authenticated(&clean_url(url), cookie_file)
                .await;
        }
        let resolved = if platform == "xiaohongshu" && is_short_link(url) {
            self.http.resolve_xhs_short(url).await?
        } else if platform != "douyin" && is_short_link(url) {
            self.http.resolve_url(url, platform).await?
        } else {
            url.to_string()
        };
        let resolved = clean_url(&resolved);
        let native = match platform {
            "bilibili" | "weibo" | "acfun" => {
                sites_core::parse(&self.http, &resolved, platform).await
            }
            "pipixia" | "zuiyou" | "weishi" | "haokan" => {
                sites_extra::parse(&self.http, &resolved, platform).await
            }
            "huya" | "jimeng" | "doubao" | "meipai" | "pipigx" | "toutiao" | "xigua" => {
                sites_other::parse(&self.http, &resolved, platform).await
            }
            "twitter" => twitter::parse_native(&self.http, &resolved).await,
            _ => sites_page::parse(&self.http, &resolved, platform).await,
        };
        self.complete_native(native, &resolved, platform).await
    }

    async fn complete_native(
        &self,
        native: Result<ParsedData>,
        resolved: &str,
        platform: &str,
    ) -> Result<ParsedData> {
        if let Err(error) = &native {
            if error.is::<SiteAccessRestricted>()
                || error.is::<DouyinMediaUnavailable>()
                || error.is::<XhsMediaUnavailable>()
                || error.is::<BilibiliMediaUnavailable>()
                || matches!(
                    error.downcast_ref::<TwitterMediaUnavailable>(),
                    Some(TwitterMediaUnavailable::TextOnly)
                )
                || platform == "douyin"
                    && (error.is::<SiteJsonUnavailable>()
                        || sites_page::douyin_slides_id(resolved).is_some())
            {
                return native;
            }
            let message = error.to_string();
            if [
                "访问验证",
                "请求受限",
                "冷却",
                "-352",
                "-412",
                "风控",
                "验证码",
            ]
            .iter()
            .any(|part| message.contains(part))
            {
                if !self.http.is_cooling_down(platform).await {
                    self.http.cool_down(platform).await;
                }
                return native;
            }
        }
        match native {
            Ok(data) if data.has_media() => Ok(data),
            native_result if self.config.ytdlp.enabled => {
                let native_error = native_result
                    .err()
                    .map(|error| error.to_string())
                    .unwrap_or_else(|| "原生解析未返回媒体".into());
                twitter::parse_ytdlp(
                    resolved,
                    &self.config.ytdlp,
                    self.config
                        .auth
                        .cookie_files
                        .get(platform)
                        .map(String::as_str),
                )
                .await
                .map_err(|error| anyhow::anyhow!("{native_error}；本地 yt-dlp 解析失败 {error}"))
            }
            Ok(_) => bail!("{platform} 页面未提供可用媒体，请配置登录 Cookie"),
            Err(error) => Err(error),
        }
    }

    async fn parse_twitter_authenticated(
        &self,
        url: &str,
        cookie_file: &str,
    ) -> Result<ParsedData> {
        let mut blocked_until = self.twitter_auth_gate.lock().await;
        let now = Instant::now();
        if *blocked_until > now {
            bail!(
                "X 登录态请求处于限流冷却期，请在 {} 秒后重试",
                blocked_until
                    .duration_since(now)
                    .as_secs()
                    .saturating_add(1)
            );
        }
        let result = twitter::parse_ytdlp(url, &self.config.ytdlp, Some(cookie_file)).await;
        if let Err(error) = &result {
            let message = error.to_string().to_ascii_lowercase();
            if message
                .split(|character: char| !character.is_ascii_alphanumeric())
                .any(|part| part == "429")
                || ["rate-limit", "rate limit", "too many requests"]
                    .iter()
                    .any(|marker| message.contains(marker))
            {
                *blocked_until = Instant::now()
                    + Duration::from_secs(self.config.api.rate_limit_cooldown.clamp(1, 3600));
            }
            if is_twitter_photo_fallback(&message) {
                return twitter::parse_native(&self.http, url)
                    .await
                    .map_err(|error| {
                        if error.is::<TwitterMediaUnavailable>() {
                            error
                        } else {
                            anyhow::anyhow!("X 公开图片提取失败。{error}")
                        }
                    });
            }
        }
        result.map_err(|error| {
            anyhow::anyhow!("X 登录态解析失败，请检查 Cookie 和作品访问权限。{error}")
        })
    }
}

fn is_twitter_photo_fallback(error: &str) -> bool {
    let message = error.to_ascii_lowercase();
    let denied_http_status = message
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| ["401", "403", "429"].contains(&part));
    message.contains("no video could be found in this tweet")
        && !denied_http_status
        && !["not authorized", "login", "log in", "expired", "cookie"]
            .iter()
            .any(|marker| message.contains(marker))
}

fn authenticated_twitter_cookie_file(config: &AppConfig) -> Option<&str> {
    if !config.ytdlp.enabled {
        return None;
    }
    config
        .auth
        .cookie_files
        .get("twitter")
        .map(String::as_str)
        .filter(|path| !path.trim().is_empty())
        .or_else(|| {
            (!config.ytdlp.cookie_file.trim().is_empty())
                .then_some(config.ytdlp.cookie_file.as_str())
        })
}

fn is_short_link(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    matches!(
        url.host_str(),
        Some(
            "b23.tv"
                | "bili2233.cn"
                | "hy.fan"
                | "v.douyin.com"
                | "www.v.douyin.com"
                | "v.kuaishou.com"
                | "xhslink.com"
                | "xhslink.cn"
                | "t.co"
                | "youtu.be"
                | "vt.tiktok.com"
                | "vm.tiktok.com"
                | "v.ixigua.com"
        )
    )
}

fn parse_cache_key(url: &str, platform: &str) -> String {
    if platform == "douyin"
        && let Some(id) = sites_page::douyin_slides_id(url)
    {
        return format!("douyin:slides:{id}");
    }
    if platform == "douyin"
        && let Some(id) = sites_page::douyin_id(url)
    {
        return format!("douyin:work:{id}");
    }
    format!("{platform}:{}", clean_url(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unavailable_douyin_item_does_not_activate_platform_cooldown_or_fallback() {
        let mut config = AppConfig::default();
        config.ytdlp.enabled = true;
        let service = VideoParserService::new(config).unwrap();
        let url = "https://www.iesdouyin.com/share/video/123/";
        let unavailable = sites_page::parse_page("<html></html>", url, "douyin");

        let error = service
            .complete_native(unavailable, url, "douyin")
            .await
            .unwrap_err();
        assert!(error.is::<DouyinMediaUnavailable>());
        assert!(!service.http.is_cooling_down("douyin").await);
    }

    #[tokio::test]
    async fn douyin_long_and_resolved_short_links_share_the_cached_item_result() {
        let mut config = AppConfig::default();
        config.resources.cache_ttl_seconds = 120;
        let service = VideoParserService::new(config).unwrap();
        let short = "https://v.douyin.com/example";
        let share = "https://www.iesdouyin.com/share/video/123?utm_source=share";
        let long = "https://www.douyin.com/video/123";
        let key = parse_cache_key(share, "douyin");
        assert_eq!(key, parse_cache_key(long, "douyin"));
        service.cache_value(
            format!("douyin-short:{short}"),
            CacheValue::DouyinRedirect(share.into()),
            300,
        );
        service.cache_value(
            key.clone(),
            CacheValue::DouyinUnavailable(DouyinMediaUnavailable::Work),
            30,
        );
        let expiry = service.cache.lock().unwrap().get(&key).unwrap().expires;
        assert!(expiry.saturating_duration_since(Instant::now()) <= Duration::from_secs(30));

        service.http.cool_down("douyin").await;
        for url in [short, share, long] {
            assert!(
                service
                    .parse(url, "douyin")
                    .await
                    .unwrap_err()
                    .is::<DouyinMediaUnavailable>()
            );
        }
        assert!(
            service
                .parse("https://www.douyin.com/video/456", "douyin")
                .await
                .unwrap_err()
                .is::<SiteAccessRestricted>()
        );
    }

    #[test]
    fn authenticated_twitter_selects_platform_cookie_file_and_honors_disable() {
        let mut config = AppConfig::default();
        config.ytdlp.enabled = true;
        config.ytdlp.cookie_file = "global.txt".into();
        config
            .auth
            .cookie_files
            .insert("twitter".into(), "platform.txt".into());
        assert_eq!(
            authenticated_twitter_cookie_file(&config),
            Some("platform.txt")
        );
        config
            .auth
            .cookie_files
            .insert("twitter".into(), "  ".into());
        assert_eq!(
            authenticated_twitter_cookie_file(&config),
            Some("global.txt")
        );
        config.ytdlp.enabled = false;
        assert!(authenticated_twitter_cookie_file(&config).is_none());
    }

    #[test]
    fn public_photo_fallback_excludes_authentication_and_rate_limit_failures() {
        let no_video = "ERROR: [twitter] 123: No video could be found in this tweet";
        assert!(is_twitter_photo_fallback(no_video));
        assert!(is_twitter_photo_fallback(
            "ERROR: [twitter] 123429403401789: No video could be found in this tweet"
        ));
        for suffix in [
            "HTTP 401",
            "HTTP 403",
            "HTTP 429",
            "Not authorized",
            "Login required",
            "Cookie expired",
        ] {
            assert!(!is_twitter_photo_fallback(&format!("{no_video}; {suffix}")));
        }
        assert!(!is_twitter_photo_fallback(
            "Requested format is not available"
        ));
    }

    #[cfg(unix)]
    fn authenticated_twitter_fixture(directory: &std::path::Path, body: &str) -> AppConfig {
        use std::os::unix::fs::PermissionsExt;

        let cookies = directory.join("twitter fixture.txt");
        std::fs::write(
            &cookies,
            "# Netscape HTTP Cookie File\n.x.com\tTRUE\t/\tTRUE\t0\tauth_token\tfixture-value\n",
        )
        .unwrap();
        let executable = directory.join("local-extractor.sh");
        std::fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = AppConfig::default();
        config.ytdlp.enabled = true;
        config.ytdlp.command = executable.to_string_lossy().into_owned();
        config.ytdlp.timeout = 2000;
        config
            .auth
            .cookie_files
            .insert("twitter".into(), cookies.to_string_lossy().into_owned());
        config
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn authenticated_twitter_uses_local_cookie_route_despite_native_cooldown() {
        let directory = tempfile::tempdir().unwrap();
        let config = authenticated_twitter_fixture(
            directory.path(),
            r#"cookie_file=
previous=
for arg; do
  if [ "$previous" = --cookies ]; then cookie_file=$arg; fi
  previous=$arg
done
test -r "$cookie_file" || exit 31
printf '%s\n' '{"id":"123","title":"fixture","ext":"mp4","vcodec":"h264","acodec":"aac","url":"https://video.twimg.com/fixture.mp4"}'"#,
        );
        let parser = VideoParserService::new(config).unwrap();
        parser.http.cool_down("twitter").await;
        let data = parser
            .parse_url("https://x.com/example/status/123", "twitter")
            .await
            .success()
            .expect("authenticated extraction must bypass the blocked anonymous endpoint");
        assert_eq!(data.title, "fixture");
        assert_eq!(data.video, "https://video.twimg.com/fixture.mp4");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn authenticated_twitter_rate_limit_stops_subsequent_local_requests() {
        let directory = tempfile::tempdir().unwrap();
        let config = authenticated_twitter_fixture(
            directory.path(),
            r#"printf 'run' >> "$(dirname "$0")/calls"
printf '%s\n' 'HTTP Error 429: Too Many Requests' >&2
exit 1"#,
        );
        let parser = VideoParserService::new(config).unwrap();
        parser.http.cool_down("twitter").await;
        let first = parser
            .parse_url("https://x.com/example/status/123", "twitter")
            .await;
        assert!(first.failure_message().unwrap().contains("429"));
        let second = parser
            .parse_url("https://x.com/example/status/456", "twitter")
            .await;
        assert!(second.failure_message().unwrap().contains("限流冷却"));
        assert_eq!(
            std::fs::read_to_string(directory.path().join("calls")).unwrap(),
            "run"
        );
    }

    #[tokio::test]
    async fn cache_is_bounded_and_expires() {
        let mut config = AppConfig::default();
        config.resources.cache_entries = 1;
        let parser = VideoParserService::new(config).unwrap();
        parser.cache_result("a".into(), &ParsedData::empty("video"));
        parser.cache_result("b".into(), &ParsedData::empty("video"));
        assert!(parser.cached("a").is_none());
        assert!(parser.cached("b").is_some());
        parser.cache.lock().unwrap().get_mut("b").unwrap().expires = Instant::now();
        assert!(parser.cached("b").is_none());
        assert!(matches!(
            parser.parse_url("http://127.0.0.1/", "bilibili").await,
            ParseResult::Failure(_)
        ));
    }
}
