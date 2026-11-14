use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use once_cell::sync::Lazy;
use rand::RngCore;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Client, Proxy, StatusCode};
use serde_json::Value;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};
use url::Url;

use crate::parser::channels::{decrypt_channels_video, parse_channels_share};
use crate::parser::jmcomic::download_jm_album;
use crate::parser::matcher::resolve_short_url;
use crate::parser::signature::generate_istero_signature;
use crate::parser::twitter::parse_twitter_local;
use crate::types::{
    ApiTarget, AppConfig, ImageSize, LivePhoto, MusicInfo, ParsedData, VideoQuality,
};

const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(30);
const BACKUP_PLATFORMS: &[&str] = &["douyin", "xiaohongshu", "instagram", "jimeng"];
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";

static FOREIGN_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"twimg\.com|twitter\.com|x\.com|instagram\.com|tiktokcdn\.com|ytimg\.com|youtube\.com|googleusercontent\.com|snapcdn\.app|dl\.snapcdn",
    )
    .unwrap()
});
static RATE_LIMIT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"频繁|限频|限流|rate.?limit|too.?many|429").unwrap());
static TRANSIENT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"status code (403|429|5\d\d)|HTTP status code (403|429|5\d\d)|ECONN|ETIMEDOUT|timeout|网络")
        .unwrap()
});

#[derive(Debug)]
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

#[derive(Clone)]
struct CacheEntry {
    data: ParsedData,
    expire: Instant,
    last_used: u64,
}

pub struct VideoParserService {
    config: AppConfig,
    semaphore: Semaphore,
    cache: Mutex<HashMap<String, CacheEntry>>,
    cooldown: Mutex<HashMap<String, Instant>>,
    cache_clock: AtomicU64,
}

impl VideoParserService {
    pub fn new(config: AppConfig) -> Self {
        let permits = if config.api.max_concurrency == 0 {
            3
        } else {
            config.api.max_concurrency as usize
        };
        Self {
            config,
            semaphore: Semaphore::new(permits),
            cache: Mutex::new(HashMap::new()),
            cooldown: Mutex::new(HashMap::new()),
            cache_clock: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    fn default_dedicated_url(platform: &str) -> Option<&'static str> {
        Some(match platform {
            "bilibili" => "https://api.bugpk.com/api/bilibili",
            "douyin" => "https://api.bugpk.com/api/douyin",
            "doubao" => "https://api.bugpk.com/api/dbvideos",
            "kuaishou" => "https://api.bugpk.com/api/kuaishou",
            "xiaohongshu" => "https://api.bugpk.com/api/xhs",
            "jimeng" => "https://api.bugpk.com/api/jimengai",
            "toutiao" => "https://api.bugpk.com/api/toutiao",
            "weibo" => "https://api.bugpk.com/api/weibo",
            "huya" => "https://api.bugpk.com/api/huya",
            "pipigx" => "https://api.bugpk.com/api/pipigx",
            "pipixia" => "https://api.bugpk.com/api/pipixia",
            "zuiyou" => "https://api.bugpk.com/api/zuiyou",
            _ => return None,
        })
    }

    fn cache_get(&self, key: &str) -> Option<ParsedData> {
        let now = Instant::now();
        let sequence = self.cache_clock.fetch_add(1, Ordering::Relaxed) + 1;
        let mut cache = self.cache.lock().ok()?;
        let entry = cache.get_mut(key)?;
        if entry.expire <= now {
            cache.remove(key);
            return None;
        }
        entry.last_used = sequence;
        Some(entry.data.clone())
    }

    fn cache_set(&self, key: String, data: ParsedData) {
        let sequence = self.cache_clock.fetch_add(1, Ordering::Relaxed) + 1;
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        cache.insert(
            key,
            CacheEntry {
                data,
                expire: Instant::now() + Duration::from_secs(10 * 60),
                last_used: sequence,
            },
        );
        if cache.len() > 500 {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
    }

    fn host_in_cooldown(&self, host: &str) -> Option<Duration> {
        let cooldown = self.cooldown.lock().ok()?;
        let until = *cooldown.get(host)?;
        let now = Instant::now();
        (until > now).then(|| until - now)
    }

    fn set_cooldown(&self, host: String, duration: Duration) {
        if let Ok(mut cooldown) = self.cooldown.lock() {
            cooldown.insert(host, Instant::now() + duration);
        }
    }

    pub async fn parse_url(&self, url: &str, media_type: &str) -> ParseResult {
        let _permit = self.semaphore.acquire().await;
        match self.do_parse_url(url, media_type).await {
            Ok(ParseResult::Success(data)) => ParseResult::Success(data),
            Ok(ParseResult::Failure(message)) => ParseResult::Failure(message),
            Err(err) => ParseResult::Failure(err.to_string()),
        }
    }

    async fn do_parse_url(&self, url: &str, media_type: &str) -> anyhow::Result<ParseResult> {
        let real_url = resolve_short_url(url).await;

        if media_type == "twitter" && self.config.ytdlp.enabled {
            match parse_twitter_local(&real_url, &self.config.ytdlp).await {
                Ok(parsed) if parsed.has_media() => return Ok(ParseResult::Success(parsed)),
                Ok(_) => {}
                Err(err) => warn!("推特本地解析失败，降级远程 API: {err}"),
            }
        }

        if media_type == "channels" {
            if !self.config.channels.enabled {
                return Ok(ParseResult::Failure("视频号解析未启用".into()));
            }
            return match parse_channels_share(&real_url, &self.config.channels).await {
                Ok(parsed) if parsed.has_media() => Ok(ParseResult::Success(parsed)),
                Ok(_) => Ok(ParseResult::Failure("视频号解析成功但无媒体内容".into())),
                Err(err) => Ok(ParseResult::Failure(if err.to_string().is_empty() {
                    "视频号解析失败".into()
                } else {
                    err.to_string()
                })),
            };
        }

        if media_type == "jmcomic" {
            if !self.config.jmcomic.enabled {
                return Ok(ParseResult::Failure("JM 下载未启用".into()));
            }
            let Some(capture) = Regex::new(r"^jm://(album|photo)/(\d+)$")?.captures(&real_url)
            else {
                return Ok(ParseResult::Failure(format!("无效的 JM 目标: {real_url}")));
            };
            return match download_jm_album(
                &capture[2],
                &capture[1] == "photo",
                &self.config.jmcomic,
            )
            .await
            {
                Ok(parsed) => Ok(ParseResult::Success(parsed)),
                Err(err) => Ok(ParseResult::Failure(if err.to_string().is_empty() {
                    "JM 下载失败".into()
                } else {
                    err.to_string()
                })),
            };
        }

        let first_error = match self.fetch_with_chain_retry(&real_url, media_type).await {
            Ok(info) if info.has_media() => return Ok(ParseResult::Success(info)),
            Ok(_) => "解析成功但无有效内容".to_string(),
            Err(err) => err.to_string(),
        };

        if real_url != url {
            if let Ok(info) = self.fetch_with_chain_retry(url, media_type).await {
                if info.has_media() {
                    return Ok(ParseResult::Success(info));
                }
            }
        }
        Ok(ParseResult::Failure(first_error))
    }

    async fn fetch_with_chain_retry(
        &self,
        url: &str,
        media_type: &str,
    ) -> anyhow::Result<ParsedData> {
        match self.fetch_api(url, media_type).await {
            Ok(parsed) => Ok(parsed),
            Err(err) => {
                let message = err.to_string();
                if TRANSIENT_RE.is_match(&message) {
                    warn!("解析出现瞬时错误 ({message})，2s 后整链重试一次");
                    tokio::time::sleep(Duration::from_millis(
                        self.config.api.retry_interval.max(2000),
                    ))
                    .await;
                    self.fetch_api(url, media_type).await
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn fetch_api(&self, url: &str, media_type: &str) -> anyhow::Result<ParsedData> {
        if let Some(cached) = self.cache_get(url) {
            debug!("使用缓存: {url}");
            return Ok(cached);
        }

        let custom = self
            .config
            .api
            .custom_apis
            .iter()
            .find(|item| item.platform == media_type && !item.api_url.is_empty());
        let custom_target = custom.map(|custom| {
            let provider = custom
                .provider
                .clone()
                .filter(|provider| !provider.is_empty())
                .unwrap_or_else(|| {
                    if custom.api_url.contains("istero.com") {
                        "istero".into()
                    } else {
                        "bugpk".into()
                    }
                });
            let token = custom
                .token
                .clone()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| self.config.istero.token.clone());
            let app_secret = custom
                .app_secret
                .clone()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| self.config.istero.app_secret.clone());
            ApiTarget {
                url: custom.api_url.clone(),
                label: format!("专属API({media_type})[{provider}]"),
                provider: provider.clone(),
                enable_sign: custom.enable_sign.or_else(|| {
                    Some(
                        (!app_secret.is_empty() && !token.is_empty())
                            || (provider == "istero" && self.config.istero.enable_sign),
                    )
                }),
                token: Some(token),
                app_secret: Some(app_secret),
            }
        });
        let dedicated_url = Self::default_dedicated_url(media_type);
        let dedicated_first = self
            .config
            .api
            .platform_dedicated_first
            .get(media_type)
            .copied()
            .unwrap_or(false);
        let primary_url = if self.config.api.primary_url.is_empty() {
            "https://api.bugpk.com/api/short_videos"
        } else {
            &self.config.api.primary_url
        };
        let backup_url = if self.config.api.backup_url.is_empty() {
            ""
        } else {
            &self.config.api.backup_url
        };
        let backup_allowed = BACKUP_PLATFORMS.contains(&media_type);
        let provider_for = |url: &str| {
            if url.contains("istero.com") {
                "istero".to_string()
            } else {
                "bugpk".to_string()
            }
        };
        let primary = ApiTarget {
            url: primary_url.to_string(),
            label: "默认主API".into(),
            provider: provider_for(primary_url),
            token: Some(self.config.istero.token.clone()),
            app_secret: Some(self.config.istero.app_secret.clone()),
            enable_sign: Some(self.config.istero.enable_sign),
        };
        let backup = (!backup_url.is_empty()).then(|| ApiTarget {
            url: backup_url.to_string(),
            label: "备用主API".into(),
            provider: provider_for(backup_url),
            token: Some(self.config.istero.token.clone()),
            app_secret: Some(self.config.istero.app_secret.clone()),
            enable_sign: Some(self.config.istero.enable_sign),
        });
        let dedicated = custom_target.or_else(|| {
            dedicated_url.map(|url| ApiTarget {
                url: url.to_string(),
                label: format!("内置专属API({media_type})"),
                provider: "bugpk".into(),
                ..ApiTarget::default()
            })
        });

        let mut targets = Vec::new();
        if dedicated_first {
            if let Some(target) = &dedicated {
                targets.push(target.clone());
            }
            targets.push(primary.clone());
            if backup_allowed {
                if let Some(target) = &backup {
                    targets.push(target.clone());
                }
            }
        } else {
            targets.push(primary);
            if backup_allowed {
                if let Some(target) = backup {
                    targets.push(target);
                }
            }
            if let Some(target) = dedicated {
                targets.push(target);
            }
        }

        let max_attempts = self.config.api.retry_times.clamp(1, 3);
        let mut last_error = None;
        for target in targets {
            let host = Url::parse(&target.url)
                .ok()
                .and_then(|url| url.host_str().map(ToString::to_string))
                .unwrap_or_else(|| target.url.clone());
            if let Some(remaining) = self.host_in_cooldown(&host) {
                warn!(
                    "{} 处于限频冷却期 (剩余 {}s)，跳过",
                    target.label,
                    remaining.as_secs()
                );
                continue;
            }

            for attempt in 1..=max_attempts {
                let params = {
                    let mut params = vec![("url".to_string(), url.to_string())];
                    if target.provider == "bugpk" && !target.url.contains(&format!("/{media_type}"))
                    {
                        params.push(("platform".to_string(), media_type.to_string()));
                    }
                    params
                };
                let mut headers = HeaderMap::new();
                headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
                if target.provider == "istero" {
                    let token = target
                        .token
                        .clone()
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| self.config.istero.token.clone());
                    let app_secret = target
                        .app_secret
                        .clone()
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| self.config.istero.app_secret.clone());
                    let enable_sign = target.enable_sign.unwrap_or(
                        (!app_secret.is_empty() && !token.is_empty())
                            || self.config.istero.enable_sign,
                    );
                    if !token.is_empty() {
                        headers.insert(
                            "Authorization",
                            HeaderValue::from_str(&format!("Bearer {token}"))?,
                        );
                    }
                    if enable_sign && !token.is_empty() && !app_secret.is_empty() {
                        let timestamp = chrono::Utc::now().timestamp();
                        let mut random = [0u8; 8];
                        rand::thread_rng().fill_bytes(&mut random);
                        let nonce = hex::encode(random);
                        let signature = generate_istero_signature(
                            &token,
                            &app_secret,
                            timestamp,
                            &nonce,
                            &params,
                        );
                        headers.insert("X-Signature", HeaderValue::from_str(&signature)?);
                        headers.insert(
                            "X-Timestamp",
                            HeaderValue::from_str(&timestamp.to_string())?,
                        );
                        headers.insert("X-Nonce", HeaderValue::from_str(&nonce)?);
                    }
                } else {
                    headers.insert(
                        "Referer",
                        HeaderValue::from_static("https://www.baidu.com/"),
                    );
                }
                debug!(
                    "发送请求 [{}]: {} 参数: {:?}",
                    target.label, target.url, params
                );

                let client = Client::builder()
                    .timeout(Duration::from_millis(self.config.api.timeout.max(1)))
                    .build()?;
                let response = client
                    .get(&target.url)
                    .headers(headers)
                    .query(&params)
                    .send()
                    .await;
                let response = match response {
                    Ok(response) => response,
                    Err(err) => {
                        last_error = Some(anyhow::anyhow!(err.to_string()));
                        warn!("{} 第 {attempt} 次请求失败: {err}", target.label);
                        if attempt < max_attempts {
                            tokio::time::sleep(Duration::from_millis(
                                self.config.api.retry_interval,
                            ))
                            .await;
                        }
                        continue;
                    }
                };
                let status = response.status();
                let retry_after = response.headers().get(RETRY_AFTER).cloned();
                let body: Value = response.json().await.unwrap_or(Value::Null);
                if status == StatusCode::TOO_MANY_REQUESTS {
                    let retry_after_secs = retry_after
                        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok())
                        .unwrap_or(0);
                    let cooldown = RATE_LIMIT_COOLDOWN.max(Duration::from_secs(retry_after_secs));
                    self.set_cooldown(host.clone(), cooldown);
                    warn!(
                        "{} 触发限频 (HTTP 429)，域名 {} 冷却 {}s",
                        target.label,
                        host,
                        cooldown.as_secs()
                    );
                    last_error = Some(anyhow::anyhow!("HTTP status code 429"));
                    break;
                }
                if matches!(
                    status,
                    StatusCode::BAD_REQUEST
                        | StatusCode::UNAUTHORIZED
                        | StatusCode::FORBIDDEN
                        | StatusCode::NOT_FOUND
                ) {
                    last_error = Some(anyhow::anyhow!(
                        "HTTP status code {}: {}",
                        status.as_u16(),
                        body.to_string().chars().take(200).collect::<String>()
                    ));
                    break;
                }
                if !status.is_success() {
                    last_error = Some(anyhow::anyhow!(
                        "HTTP status code {}: {}",
                        status.as_u16(),
                        body.to_string().chars().take(200).collect::<String>()
                    ));
                    if attempt < max_attempts {
                        tokio::time::sleep(Duration::from_millis(self.config.api.retry_interval))
                            .await;
                    }
                    continue;
                }

                let code = &body["code"];
                let data_object = body.get("data").filter(|data| data.is_object());
                let has_usable_data = data_object
                    .is_some_and(|data| data.as_object().is_some_and(|object| !object.is_empty()));
                let success =
                    code == 200 || code == "200" || ((code == 0 || code == "0") && has_usable_data);
                if success {
                    let parsed = parse_api_response(&body, self.config.output.max_desc_length);
                    if !parsed.video.is_empty()
                        || !parsed.images.is_empty()
                        || !parsed.live_photo.is_empty()
                        || !parsed.cover.is_empty()
                    {
                        self.cache_set(url.to_string(), parsed.clone());
                        return Ok(parsed);
                    }
                    last_error = Some(anyhow::anyhow!(
                        "{}",
                        body.get("msg")
                            .or_else(|| body.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("API返回成功但无有效内容")
                    ));
                    break;
                }
                let error_message = body
                    .get("message")
                    .or_else(|| body.get("msg"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("API返回错误码: {code}"));
                last_error = Some(anyhow::anyhow!("{error_message}"));
                if error_message.contains("无法识别")
                    || error_message.contains("不是B站")
                    || error_message.contains("链接好像不太对")
                    || error_message.contains("不支持")
                    || error_message.contains("未支持")
                    || error_message.contains("解析失败")
                {
                    break;
                }
                if RATE_LIMIT_RE.is_match(&error_message) {
                    self.set_cooldown(host.clone(), RATE_LIMIT_COOLDOWN);
                    warn!(
                        "{} 触发业务限频提示，域名 {} 冷却 {}s",
                        target.label,
                        host,
                        RATE_LIMIT_COOLDOWN.as_secs()
                    );
                    break;
                }
                if attempt < max_attempts {
                    tokio::time::sleep(Duration::from_millis(self.config.api.retry_interval)).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("所有API请求全部失败")))
    }

    pub async fn download_media_file(
        &self,
        media_url: &str,
        extension: &str,
    ) -> anyhow::Result<String> {
        if media_url.is_empty() {
            anyhow::bail!("媒体链接为空");
        }
        let temp_dir = PathBuf::from(&self.config.output.temp_dir);
        tokio::fs::create_dir_all(&temp_dir).await?;
        let file_name = format!(
            "media_{}_{}.{}",
            chrono::Utc::now().timestamp_millis(),
            random_suffix(6),
            extension
        );
        let file_path = temp_dir.join(file_name);
        let client = media_client(if is_foreign_url(media_url) {
            Some(&self.config.api.proxy)
        } else {
            None
        })?;
        let response = client
            .get(media_url)
            .timeout(Duration::from_secs(30))
            .header("User-Agent", USER_AGENT)
            .header("Referer", "https://www.baidu.com/")
            .send()
            .await?
            .error_for_status()?;
        let bytes = response.bytes().await?;
        tokio::fs::write(&file_path, bytes).await?;
        Ok(file_path.to_string_lossy().to_string())
    }

    pub async fn download_video_file(&self, video_url: &str) -> anyhow::Result<String> {
        if video_url.is_empty() {
            anyhow::bail!("视频链接为空");
        }
        let temp_dir = PathBuf::from(&self.config.output.temp_dir);
        tokio::fs::create_dir_all(&temp_dir).await?;
        let file_path = temp_dir.join(format!(
            "video_{}_{}.mp4",
            chrono::Utc::now().timestamp_millis(),
            random_suffix(6)
        ));
        let max_size = self
            .config
            .output
            .max_video_size
            .saturating_mul(1024 * 1024);
        let mut last_error = None;
        for attempt in 1..=2 {
            if attempt > 1 {
                info!("第 {attempt} 次尝试下载视频...");
                tokio::time::sleep(Duration::from_millis(self.config.api.retry_interval)).await;
            }
            let client = media_client(if is_foreign_url(video_url) {
                Some(&self.config.api.proxy)
            } else {
                None
            })?;
            let response = match client
                .get(video_url)
                .timeout(Duration::from_millis(
                    self.config.output.video_download_timeout.max(1),
                ))
                .header("User-Agent", USER_AGENT)
                .header("Referer", "https://www.bilibili.com/")
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(err.to_string());
                    continue;
                }
            };
            if !response.status().is_success() {
                last_error = Some(format!("HTTP status code {}", response.status().as_u16()));
                continue;
            }
            let content_length = response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            if max_size > 0 && content_length > max_size {
                anyhow::bail!(
                    "视频文件过大({}MB)，超过限制({}MB)",
                    content_length.div_ceil(1024 * 1024),
                    self.config.output.max_video_size
                );
            }
            let mut file = tokio::fs::File::create(&file_path).await?;
            let mut stream = response.bytes_stream();
            let mut downloaded = 0u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                downloaded += chunk.len() as u64;
                if max_size > 0 && downloaded > max_size {
                    let _ = tokio::fs::remove_file(&file_path).await;
                    anyhow::bail!(
                        "视频文件过大({}MB)，超过限制({}MB)",
                        downloaded.div_ceil(1024 * 1024),
                        self.config.output.max_video_size
                    );
                }
                use tokio::io::AsyncWriteExt;
                file.write_all(&chunk).await?;
            }
            return Ok(file_path.to_string_lossy().to_string());
        }
        let _ = tokio::fs::remove_file(&file_path).await;
        anyhow::bail!(
            "下载视频失败: {}",
            last_error.unwrap_or_else(|| "未知错误".into())
        )
    }
}

fn random_suffix(length: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut random = vec![0u8; length];
    rand::thread_rng().fill_bytes(&mut random);
    random
        .into_iter()
        .map(|byte| CHARS[usize::from(byte) % CHARS.len()] as char)
        .collect()
}

pub fn is_foreign_url(url: &str) -> bool {
    FOREIGN_RE.is_match(url)
}

fn normalized_proxy(proxy: &str) -> Option<String> {
    let url = Url::parse(proxy).ok()?;
    let scheme = url.scheme();
    let host = url.host_str()?;
    let port = url
        .port()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });
    Some(format!("{scheme}://{host}:{port}"))
}

fn media_client(proxy: Option<&str>) -> anyhow::Result<Client> {
    let mut builder = Client::builder().timeout(Duration::from_secs(120));
    builder = match proxy.filter(|proxy| !proxy.is_empty()) {
        Some(proxy) => match normalized_proxy(proxy) {
            Some(proxy) => builder.proxy(Proxy::all(proxy)?),
            None => builder.no_proxy(),
        },
        None => builder.no_proxy(),
    };
    Ok(builder.build()?)
}

fn candidate_clients(url: &str, proxies: &[String]) -> Vec<(Client, bool)> {
    let proxy_clients: Vec<(Client, bool)> = proxies
        .iter()
        .filter(|proxy| !proxy.is_empty())
        .filter_map(|proxy| media_client(Some(proxy)).ok().map(|client| (client, true)))
        .collect();
    let direct = media_client(None)
        .ok()
        .map(|client| vec![(client, false)])
        .unwrap_or_default();
    if is_foreign_url(url) {
        proxy_clients.into_iter().chain(direct).collect()
    } else {
        direct.into_iter().chain(proxy_clients).collect()
    }
}

pub async fn download_media_buffer(
    url: &str,
    proxies: &[String],
    max_bytes: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut first_error = None;
    for (client, proxy_enabled) in candidate_clients(url, proxies) {
        match download_with_client(&client, url, max_bytes).await {
            Ok(data) => return Ok(data),
            Err(err) => {
                debug!(
                    "媒体下载失败 (proxy={}): {err}",
                    if proxy_enabled { "on" } else { "off" }
                );
                first_error.get_or_insert(err.to_string());
            }
        }
    }
    anyhow::bail!(
        "媒体下载失败: {}",
        first_error.unwrap_or_else(|| "未知错误".into())
    )
}

async fn download_with_client(
    client: &Client,
    url: &str,
    max_bytes: u64,
) -> anyhow::Result<Vec<u8>> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(120))
        .header("User-Agent", USER_AGENT)
        .header("Accept", "*/*")
        .send()
        .await?
        .error_for_status()?;
    let mut stream = response.bytes_stream();
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if data.len() as u64 + chunk.len() as u64 > max_bytes {
            anyhow::bail!(
                "媒体过大({}MB)，超过限制",
                (data.len() as u64 + chunk.len() as u64).div_ceil(1024 * 1024)
            );
        }
        data.extend_from_slice(&chunk);
    }
    if data.is_empty() {
        anyhow::bail!("下载内容为空");
    }
    Ok(data)
}

pub async fn head_content_length(url: &str, proxies: &[String]) -> u64 {
    for (client, _) in candidate_clients(url, proxies) {
        let Ok(response) = client
            .head(url)
            .timeout(Duration::from_secs(15))
            .header("User-Agent", USER_AGENT)
            .send()
            .await
        else {
            continue;
        };
        if response.status().as_u16() < 400 {
            if let Some(length) = response
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
            {
                if length > 0 {
                    return length;
                }
            }
        }
    }
    0
}

fn string_at<'a>(value: &'a Value, keys: &[&str]) -> &'a str {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(Value::as_str) {
            if !text.is_empty() {
                return text;
            }
        }
    }
    ""
}

fn number_at(value: &Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(value) = value.get(*key) {
            if value.is_null() {
                continue;
            }
            if let Some(number) = value.as_f64() {
                return Some(number);
            }
            if let Some(text) = value.as_str() {
                if let Ok(number) = text.parse::<f64>() {
                    return Some(number);
                }
            }
        }
    }
    None
}

fn leading_i64(text: &str) -> i64 {
    let trimmed = text.trim_start();
    let mut digits = String::new();
    for (index, character) in trimmed.char_indices() {
        if index == 0 && (character == '-' || character == '+') {
            digits.push(character);
        } else if character.is_ascii_digit() {
            digits.push(character);
        } else {
            break;
        }
    }
    digits.parse().unwrap_or(0)
}

fn prefix_https(url: &str) -> String {
    if url.starts_with("http") {
        url.to_string()
    } else {
        format!("https:{url}")
    }
}

fn quality_from(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(Value::as_str) {
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    "unknown".into()
}

fn video_entries(value: Option<&Value>, mode: &str) -> Vec<(String, VideoQuality)> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let (quality, url, bit_rate) = if let Some(url) = item.as_str() {
                ("unknown".to_string(), url.to_string(), 0u64)
            } else if let Some(url) = item.get("url").and_then(Value::as_str) {
                let quality = match mode {
                    "video_backup" => {
                        quality_from(item, &["quality", "label", "definition", "resolution"])
                    }
                    "download_links" => {
                        quality_from(item, &["definition", "quality", "resolution"])
                    }
                    _ => item
                        .pointer("/accept/0")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| quality_from(item, &["quality", "label"])),
                };
                (
                    quality,
                    url.to_string(),
                    item.get("bit_rate").and_then(Value::as_u64).unwrap_or(0),
                )
            } else {
                return None;
            };
            Some((
                url.clone(),
                VideoQuality {
                    quality,
                    url,
                    bit_rate: (bit_rate > 0).then_some(bit_rate),
                },
            ))
        })
        .collect()
}

pub fn parse_api_response(raw: &Value, max_desc_len: usize) -> ParsedData {
    debug!("原始API返回数据: {raw}");
    let data = raw.get("data").unwrap_or(&Value::Null);
    let extra = data.get("extra").unwrap_or(&Value::Null);

    let mut media_type = data
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let images_array = data.get("images").and_then(Value::as_array);
    let video_backup = data.get("video_backup").and_then(Value::as_array);
    let download_links = data.get("download_links").and_then(Value::as_array);
    let data_video_url = data.get("video_url").and_then(Value::as_str);
    if media_type.is_empty() {
        if images_array.is_some_and(|items| !items.is_empty())
            && data.get("url").is_none()
            && data.get("video").is_none()
            && data_video_url.is_none()
            && download_links.is_none_or(|items| items.is_empty())
        {
            media_type = "image".into();
        } else if data
            .get("live_photo")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
        {
            media_type = "live_photo".into();
        } else if raw.get("msg").and_then(Value::as_str) == Some("live")
            || data.get("live").and_then(Value::as_bool) == Some(true)
        {
            media_type = "live".into();
        } else {
            media_type = "video".into();
        }
    }

    let author_value = data.get("author").or_else(|| data.get("user"));
    let (author, uid, avatar) = if let Some(author) = author_value.filter(|value| value.is_object())
    {
        (
            string_at(author, &["name", "author", "nickname", "username"]).to_string(),
            string_at(author, &["id", "uid"]).to_string(),
            string_at(author, &["avatar", "avatar_url", "profile_image_url"]).to_string(),
        )
    } else {
        (
            string_at(
                data,
                &["author", "auther", "author_name", "nickname", "uploader"],
            )
            .to_string(),
            string_at(data, &["uid", "twitter_id", "video_id", "id", "aweme_id"]).to_string(),
            string_at(data, &["avatar", "avatar_url"]).to_string(),
        )
    };
    let uid = if uid.is_empty() {
        string_at(data, &["uid", "twitter_id", "video_id"]).to_string()
    } else {
        uid
    };
    let avatar = if avatar.is_empty() {
        string_at(data, &["avatar"]).to_string()
    } else {
        avatar
    };

    let mut title = string_at(data, &["title", "caption"]).to_string();
    let mut desc = string_at(
        data,
        &["desc", "description", "content", "content_text", "text"],
    )
    .to_string();
    desc = desc
        .chars()
        .take(max_desc_len)
        .collect::<String>()
        .trim()
        .to_string();
    if title.is_empty() && !desc.is_empty() {
        title = desc.chars().take(60).collect();
    }
    let cover = string_at(
        data,
        &[
            "cover",
            "cover_url",
            "image_url",
            "thumbnail",
            "picture",
            "thumb",
        ],
    )
    .to_string();

    let mut videos = Vec::new();
    let mut video = String::new();
    if let Some(backup) = video_backup.filter(|items| !items.is_empty()) {
        let mut entries = video_entries(Some(&Value::Array(backup.clone())), "video_backup");
        entries.sort_by_key(|(_, quality)| std::cmp::Reverse(quality.bit_rate.unwrap_or(0)));
        videos = entries.into_iter().map(|(_, quality)| quality).collect();
        video = videos
            .first()
            .map(|quality| quality.url.clone())
            .unwrap_or_default();
    }
    if video.is_empty() {
        if download_links.is_some_and(|items| !items.is_empty()) {
            videos = video_entries(
                Some(&Value::Array(download_links.cloned().unwrap_or_default())),
                "download_links",
            )
            .into_iter()
            .map(|(_, quality)| quality)
            .collect();
            video = videos
                .first()
                .map(|quality| quality.url.clone())
                .unwrap_or_default();
        }
    }
    if video.is_empty() {
        if let Some(items) = data
            .get("videos")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
        {
            videos = video_entries(Some(&Value::Array(items.clone())), "videos")
                .into_iter()
                .map(|(_, quality)| quality)
                .collect();
            video = videos
                .first()
                .map(|quality| quality.url.clone())
                .unwrap_or_default();
        }
    }
    if video.is_empty() {
        video = string_at(
            data,
            &[
                "url",
                "video",
                "video_url",
                "download_url",
                "download_link",
                "play_url",
                "play_addr",
            ],
        )
        .to_string();
    }
    if !video.is_empty() {
        video = prefix_https(&video);
    }

    let images = data
        .get("images")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(prefix_https)
                .collect()
        })
        .unwrap_or_default();
    let live_photo = data
        .get("live_photo")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let image = item.get("image").and_then(Value::as_str)?;
                    let video = item
                        .get("video")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    Some(LivePhoto {
                        image: prefix_https(image),
                        video: if video.is_empty() {
                            String::new()
                        } else {
                            prefix_https(video)
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let music = MusicInfo {
        title: first_nonempty_pointer(data, &["/music/title", "/music/name"]),
        author: first_nonempty_pointer(data, &["/music/author", "/music/artist"]),
        cover: first_nonempty_pointer(data, &["/music/cover"]),
        url: first_nonempty_pointer(data, &["/music/url"]),
    };
    let stats = extra.get("statistics").unwrap_or(&Value::Null);
    let like = number_at(data, &["like", "likes"])
        .or_else(|| number_at(stats, &["digg_count"]))
        .unwrap_or(0.0) as i64;
    let comment = number_at(stats, &["comment_count"])
        .or_else(|| number_at(data, &["comments"]))
        .unwrap_or(0.0) as i64;
    let collect = number_at(stats, &["collect_count"])
        .or_else(|| number_at(data, &["favorites"]))
        .unwrap_or(0.0) as i64;
    let share = number_at(stats, &["share_count"])
        .or_else(|| number_at(data, &["shares"]))
        .unwrap_or(0.0) as i64;
    let play = number_at(stats, &["play_count"])
        .or_else(|| number_at(data, &["views"]))
        .unwrap_or(0.0) as i64;

    let mut duration = if let Some(value) = data.get("duration") {
        match value {
            Value::String(text) => leading_i64(text).max(0) as u64,
            other => other
                .as_u64()
                .or_else(|| other.as_f64().map(|value| value as u64))
                .unwrap_or(0),
        }
    } else {
        extra
            .get("duration_ms")
            .and_then(Value::as_f64)
            .map(|value| (value / 1000.0) as u64)
            .unwrap_or(0)
    };
    if duration > 100_000 {
        duration /= 1000;
    }

    let mut publish_time = if let Some(raw_time) = data
        .get("time")
        .or_else(|| data.get("timestamp"))
        .or_else(|| data.get("created_at"))
    {
        match raw_time {
            Value::Number(number) => number.as_i64().unwrap_or(0),
            Value::String(text) => {
                if let Ok(number) = text.parse::<i64>() {
                    number
                } else {
                    chrono::DateTime::parse_from_rfc3339(text)
                        .or_else(|_| chrono::DateTime::parse_from_rfc2822(text))
                        .map(|date| date.timestamp_millis())
                        .unwrap_or(0)
                }
            }
            _ => 0,
        }
    } else {
        extra
            .get("create_time")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .saturating_mul(1000)
    };
    if publish_time != 0 && publish_time < 10_000_000_000 {
        publish_time = publish_time.saturating_mul(1000);
    }

    ParsedData {
        media_type,
        title,
        desc,
        author,
        uid,
        avatar,
        cover,
        cover_size: None::<ImageSize>,
        image_size_map: None,
        video,
        video_decode_key: None,
        attachment: None,
        videos,
        images,
        live_photo,
        music,
        like,
        comment,
        collect,
        share,
        play,
        duration,
        publish_time,
    }
}

fn first_nonempty_pointer(value: &Value, pointers: &[&str]) -> String {
    pointers
        .iter()
        .find_map(|pointer| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or_default()
        .to_string()
}

pub fn decrypt_video_if_needed(data: Vec<u8>, key: Option<&str>) -> anyhow::Result<Vec<u8>> {
    let Some(key) = key else {
        return Ok(data);
    };
    let key = key
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("无效的视频解密密钥: {key}"))?;
    Ok(decrypt_channels_video(&data, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duration_and_video_fields() {
        let parsed = parse_api_response(
            &serde_json::json!({
                "code": 200,
                "data": {
                    "title": "t",
                    "url": "https://a.com/v.mp4",
                    "duration": 180000
                }
            }),
            200,
        );
        assert_eq!(parsed.duration, 180);
        assert_eq!(parsed.video, "https://a.com/v.mp4");
    }

    #[test]
    fn picks_highest_bitrate_backup() {
        let parsed = parse_api_response(
            &serde_json::json!({
                "data": {
                    "title": "t",
                    "video_backup": [
                        { "url": "https://x/low.mp4", "quality": "low", "bit_rate": 100 },
                        { "url": "https://x/high.mp4", "quality": "high", "bit_rate": 900 },
                        "https://x/str.mp4"
                    ]
                }
            }),
            200,
        );
        assert_eq!(parsed.video, "https://x/high.mp4");
        assert_eq!(parsed.videos.len(), 3);
    }

    #[test]
    fn detects_foreign_cdns() {
        assert!(is_foreign_url("https://pbs.twimg.com/a.jpg"));
        assert!(is_foreign_url("https://i.ytimg.com/a.jpg"));
        assert!(!is_foreign_url("https://cdn.example.com/a.jpg"));
    }
}
