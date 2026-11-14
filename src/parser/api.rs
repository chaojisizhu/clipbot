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
use crate::parser::http::{SiteHttp, validate_site_url};
use crate::parser::jmcomic::download_jm_album;
use crate::parser::matcher::clean_url;
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
    data: ParsedData,
    expires: Instant,
    last_used: Instant,
    weight: usize,
}

pub struct VideoParserService {
    config: AppConfig,
    http: SiteHttp,
    semaphore: Semaphore,
    cache: Mutex<HashMap<String, CacheEntry>>,
    // Fixed lock stripes coalesce concurrent requests without growing a pending-key map.
    key_locks: [AsyncMutex<()>; 64],
}

impl VideoParserService {
    pub fn new(config: AppConfig) -> Result<Self> {
        Ok(Self {
            http: SiteHttp::new(&config)?,
            semaphore: Semaphore::new(config.api.max_concurrency.clamp(1, 16) as usize),
            config,
            cache: Mutex::new(HashMap::new()),
            key_locks: std::array::from_fn(|_| AsyncMutex::new(())),
        })
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    fn cached(&self, key: &str) -> Option<ParsedData> {
        let mut cache = self.cache.lock().ok()?;
        let now = Instant::now();
        cache.retain(|_, entry| entry.expires > now);
        let entry = cache.get_mut(key)?;
        entry.last_used = now;
        Some(entry.data.clone())
    }

    fn cache_result(&self, key: String, data: &ParsedData) {
        if self.config.resources.cache_entries == 0 || data.attachment.is_some() {
            return;
        }
        let weight = serde_json::to_vec(data)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
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
                    data: data.clone(),
                    expires: now
                        + Duration::from_secs(self.config.resources.cache_ttl_seconds.min(300)),
                    last_used: now,
                    weight,
                },
            );
        }
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
        let key = format!("{platform}:{}", clean_url(url));
        if let Some(data) = self.cached(&key) {
            return Ok(data);
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let _key_guard = self.key_locks[hasher.finish() as usize % 64].lock().await;
        if let Some(data) = self.cached(&key) {
            return Ok(data);
        }
        let _permit = self.semaphore.acquire().await?;
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
        let mut data = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            self.parse_uncached(url, platform),
        )
        .await
        .map_err(|_| anyhow::anyhow!("{platform} 解析超时"))??;
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
        let resolved = if is_short_link(url) {
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
        if let Err(error) = &native {
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
                    &resolved,
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
                | "v.kuaishou.com"
                | "xhslink.com"
                | "t.co"
                | "youtu.be"
                | "vt.tiktok.com"
                | "vm.tiktok.com"
                | "v.ixigua.com"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
