use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use once_cell::sync::Lazy;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Proxy};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter};
use url::Url;

use crate::parser::channels::decrypt_channels_video;

const BUFFER_LIMIT: u64 = 50 * 1024 * 1024;
const DOWNLOAD_WRITE_BUFFER: usize = 64 * 1024;
const ENCRYPTED_PREFIX: usize = 128 * 1024;
const STREAM_CHUNK_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STREAM_RETRIES: usize = 3;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";
static TEMP_BYTES: AtomicU64 = AtomicU64::new(0);
static DIRECT: Lazy<Client> = Lazy::new(|| build_client(None).expect("媒体 HTTP 客户端初始化失败"));
static PROXY_CLIENTS: Lazy<std::sync::Mutex<HashMap<String, Client>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

#[derive(Debug)]
struct PermanentMediaError(String);

impl std::fmt::Display for PermanentMediaError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PermanentMediaError {}

#[derive(Debug)]
struct DownloadTimeoutError(String);

impl std::fmt::Display for DownloadTimeoutError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DownloadTimeoutError {}

struct TempFileTransfer {
    // Field order matters on Windows: close the file handle before TempMedia deletes the path.
    file: Option<tokio::fs::File>,
    media: TempMedia,
    can_range_resume: bool,
}

/// Owns only files created by this module. Dropping a cancelled transfer also
/// releases its reserved disk budget and removes its partial file.
#[derive(Debug)]
pub struct TempMedia {
    path: PathBuf,
    size: u64,
    reserved: u64,
}

impl TempMedia {
    /// Reserves the maximum output size before an external remuxer starts.
    /// The created pathname is owned by this guard and is removed on failure.
    pub fn reserve(directory: &Path, max_bytes: u64, max_temp_bytes: u64) -> Result<Self> {
        if max_bytes == 0 {
            bail!("临时媒体大小上限必须大于零");
        }
        std::fs::create_dir_all(directory)?;
        let path = directory.join(format!("clipbot-{:032x}.mp4", rand::random::<u128>()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        drop(file);
        let mut media = Self {
            path,
            size: 0,
            reserved: 0,
        };
        reserve_temp(max_bytes, max_temp_bytes)?;
        media.reserved = max_bytes;
        Ok(media)
    }

    /// Verifies output size and releases the unused portion of its reservation.
    pub fn finish(&mut self) -> Result<()> {
        let size = std::fs::metadata(&self.path)?.len();
        if size == 0 || size > self.reserved {
            bail!("媒体输出为空或超过预留的文件大小上限");
        }
        TEMP_BYTES.fetch_sub(self.reserved - size, Ordering::Relaxed);
        self.reserved = size;
        self.size = size;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

impl Drop for TempMedia {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        TEMP_BYTES.fetch_sub(self.reserved, Ordering::Relaxed);
    }
}

fn build_client(proxy: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(2)
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(USER_AGENT)
        .no_proxy();
    if let Some(proxy) = proxy {
        builder = builder.proxy(Proxy::all(proxy)?);
    }
    Ok(builder.build()?)
}

fn clients(url: &str, proxies: &[String]) -> Vec<Client> {
    let mut candidates = Vec::with_capacity(proxies.len().min(8) + 1);
    if let Ok(mut cached) = PROXY_CLIENTS.lock() {
        for proxy in proxies.iter().filter(|proxy| !proxy.is_empty()).take(8) {
            if let Some(client) = cached.get(proxy) {
                candidates.push(client.clone());
            } else if let Ok(client) = build_client(Some(proxy)) {
                if cached.len() < 16 {
                    cached.insert(proxy.clone(), client.clone());
                }
                candidates.push(client);
            }
        }
    }
    if is_foreign_url(url) {
        candidates.push(DIRECT.clone());
    } else {
        candidates.insert(0, DIRECT.clone());
    }
    candidates
}

pub fn is_foreign_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    [
        "twimg.com",
        "twitter.com",
        "x.com",
        "instagram.com",
        "cdninstagram.com",
        "tiktokcdn.com",
        "tiktokv.com",
        "ytimg.com",
        "youtube.com",
        "googlevideo.com",
        "googleusercontent.com",
    ]
    .iter()
    .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

fn safe_headers(headers: &HashMap<String, String>) -> HeaderMap {
    let mut result = HeaderMap::new();
    // Media URLs may redirect to another CDN. Credentials belong to SiteHttp,
    // while media requests carry only playback headers that can follow a CDN.
    for (name, value) in headers {
        if !matches!(
            name.to_ascii_lowercase().as_str(),
            "referer" | "user-agent" | "accept" | "origin"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            result.insert(name, value);
        }
    }
    result
}

fn check_url(raw: &str) -> Result<()> {
    let url = Url::parse(raw)?;
    if !matches!(url.scheme(), "https" | "http") || url.host_str().is_none() {
        bail!("媒体 URL 必须为 HTTP(S)");
    }
    Ok(())
}

fn can_try_next_media_client(error: &anyhow::Error) -> bool {
    if error.is::<PermanentMediaError>() || error.is::<DownloadTimeoutError>() {
        return false;
    }
    !error
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        .is_some_and(|status| matches!(status.as_u16(), 401 | 403 | 404 | 410 | 412 | 429))
}

fn format_error_chain(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

fn has_content_encoding(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|encoding| {
            !encoding.trim().is_empty() && !encoding.eq_ignore_ascii_case("identity")
        })
}

enum ParsedContentRange {
    Complete(u64),
    Slice {
        start: u64,
        end: u64,
        total: Option<u64>,
    },
}

struct TempDownloadParams<'a> {
    url: &'a str,
    headers: &'a HeaderMap,
    directory: &'a Path,
    max_bytes: u64,
    max_temp_bytes: u64,
    timeout: Duration,
    decode_key: Option<&'a str>,
}

fn parse_content_range(header: &str) -> Option<ParsedContentRange> {
    let (unit, spec) = header.trim().split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, total) = spec.trim().split_once('/')?;
    let total = if total.trim() == "*" {
        None
    } else {
        Some(total.trim().parse::<u64>().ok()?)
    };
    if range.trim() == "*" {
        return total.map(ParsedContentRange::Complete);
    }
    let (start, end) = range.trim().split_once('-')?;
    let start = start.trim().parse::<u64>().ok()?;
    let end = end.trim().parse::<u64>().ok()?;
    (start <= end).then_some(ParsedContentRange::Slice { start, end, total })
}

fn is_retryable_reqwest(error: &reqwest::Error) -> bool {
    if let Some(status) = error.status() {
        return status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT;
    }
    !error.is_builder()
}

fn check_video_response(response: &reqwest::Response) -> Result<()> {
    let path = response.url().path().to_ascii_lowercase();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if path.ends_with(".m3u8")
        || path.ends_with(".mpd")
        || content_type.contains("mpegurl")
        || content_type.contains("dash+xml")
    {
        return Err(PermanentMediaError("视频返回分段播放清单，需要合流后才能发送".into()).into());
    }
    if content_type.contains("text/html") || content_type.contains("application/json") {
        return Err(PermanentMediaError(
            "视频下载返回网页或错误信息，请检查登录状态和播放链接".into(),
        )
        .into());
    }
    Ok(())
}

fn check_video_prefix(prefix: &[u8]) -> Result<()> {
    let prefix = String::from_utf8_lossy(prefix);
    let prefix = prefix
        .trim_start_matches(['\u{feff}', ' ', '\r', '\n', '\t'])
        .to_ascii_lowercase();
    if prefix.starts_with("#extm3u") || prefix.starts_with("<?xml") || prefix.starts_with("<mpd") {
        return Err(PermanentMediaError("视频返回分段播放清单，需要合流后才能发送".into()).into());
    }
    if prefix.starts_with('<') || prefix.starts_with('{') || prefix.starts_with('[') {
        return Err(PermanentMediaError(
            "视频内容为网页或错误信息，请检查登录状态和播放链接".into(),
        )
        .into());
    }
    Ok(())
}

fn reserve_temp(bytes: u64, limit: u64) -> Result<()> {
    let mut used = TEMP_BYTES.load(Ordering::Relaxed);
    loop {
        let next = used
            .checked_add(bytes)
            .filter(|next| *next <= limit)
            .ok_or_else(|| anyhow::anyhow!("临时媒体总量已达到磁盘预算"))?;
        match TEMP_BYTES.compare_exchange_weak(used, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(()),
            Err(actual) => used = actual,
        }
    }
}

pub async fn download_media_buffer(
    url: &str,
    proxies: &[String],
    max_bytes: u64,
) -> Result<Vec<u8>> {
    download_media_buffer_with_headers(url, proxies, max_bytes, &HashMap::new()).await
}

pub async fn download_media_buffer_with_headers(
    url: &str,
    proxies: &[String],
    max_bytes: u64,
    headers: &HashMap<String, String>,
) -> Result<Vec<u8>> {
    check_url(url)?;
    let limit = max_bytes.min(BUFFER_LIMIT);
    let headers = safe_headers(headers);
    let mut last_error = String::new();
    for client in clients(url, proxies) {
        let attempt = async {
            let response = client
                .get(url)
                .headers(headers.clone())
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .map_err(reqwest::Error::without_url)?;
            if response
                .content_length()
                .is_some_and(|length| length > limit)
            {
                return Err(PermanentMediaError(format!(
                    "媒体超过内存中转上限 {} MiB",
                    limit / 1024 / 1024
                ))
                .into());
            }
            let mut data = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(reqwest::Error::without_url)?;
                if data.len() as u64 + chunk.len() as u64 > limit {
                    return Err(PermanentMediaError(format!(
                        "媒体超过内存中转上限 {} MiB",
                        limit / 1024 / 1024
                    ))
                    .into());
                }
                data.extend_from_slice(&chunk);
            }
            if data.is_empty() {
                return Err(PermanentMediaError("下载内容为空".into()).into());
            }
            Ok::<_, anyhow::Error>(data)
        }
        .await;
        match attempt {
            Ok(data) => return Ok(data),
            Err(error) if can_try_next_media_client(&error) => {
                last_error = format_error_chain(&error);
            }
            Err(error) => bail!("媒体下载失败: {}", format_error_chain(&error)),
        }
    }
    bail!("媒体下载失败 {last_error}")
}

pub async fn head_content_length(url: &str, proxies: &[String]) -> u64 {
    if check_url(url).is_err() {
        return 0;
    }
    for client in clients(url, proxies) {
        if let Ok(response) = client
            .head(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            && response.status().is_success()
            && let Some(size) = response.content_length().filter(|size| *size > 0)
        {
            return size;
        }
    }
    0
}

fn timeout_error(timeout: Duration) -> anyhow::Error {
    DownloadTimeoutError(format!(
        "视频下载超时（超过 {} 秒）",
        timeout.as_secs().max(1)
    ))
    .into()
}

fn open_temp_transfer(directory: &Path, can_range_resume: bool) -> Result<TempFileTransfer> {
    let path = directory.join(format!("clipbot-{:032x}.mp4", rand::random::<u128>()));
    // Claim the pathname synchronously so a cancelled open cannot leave
    // a file created by an unfinished Tokio filesystem worker.
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(&path)?;
    Ok(TempFileTransfer {
        file: Some(tokio::fs::File::from_std(opened)),
        media: TempMedia {
            path,
            size: 0,
            reserved: 0,
        },
        can_range_resume,
    })
}

async fn download_media_temp_with_client(
    client: &Client,
    params: &TempDownloadParams<'_>,
) -> Result<TempMedia> {
    let deadline = tokio::time::Instant::now() + params.timeout;
    let mut transfer: Option<TempFileTransfer> = None;
    let mut retries_used = 0usize;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(timeout_error(params.timeout));
        }

        let mut request = client
            .get(params.url)
            .headers(params.headers.clone())
            .timeout(remaining);
        if let Some(state) = transfer
            .as_ref()
            .filter(|state| state.media.size > 0 && state.can_range_resume)
        {
            request = request.header(
                reqwest::header::RANGE,
                format!("bytes={}-", state.media.size),
            );
        }

        let response = match tokio::time::timeout(remaining, request.send()).await {
            Err(_) => return Err(timeout_error(params.timeout)),
            Ok(Err(error)) => {
                let error = error.without_url();
                let now = tokio::time::Instant::now();
                if now >= deadline || (error.is_timeout() && remaining <= STREAM_CHUNK_IDLE_TIMEOUT)
                {
                    return Err(timeout_error(params.timeout));
                }
                if is_retryable_reqwest(&error) && retries_used < MAX_STREAM_RETRIES {
                    retries_used += 1;
                    tokio::time::sleep(Duration::from_millis(250).min(remaining)).await;
                    continue;
                }
                return Err(error.into());
            }
            Ok(Ok(response)) => response,
        };

        let status = response.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            let completed = transfer.as_ref().is_some_and(|state| {
                response
                    .headers()
                    .get(reqwest::header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_content_range)
                    .is_some_and(|range| {
                        matches!(range, ParsedContentRange::Complete(total) if total == state.media.size)
                    })
            });
            if completed {
                break;
            }
            return Err(PermanentMediaError("媒体断点续传范围无效（HTTP 416）".into()).into());
        }
        if !status.is_success() {
            let error = response.error_for_status().unwrap_err().without_url();
            if (status.is_server_error() || status == reqwest::StatusCode::REQUEST_TIMEOUT)
                && retries_used < MAX_STREAM_RETRIES
            {
                retries_used += 1;
                tokio::time::sleep(Duration::from_millis(250).min(remaining)).await;
                continue;
            }
            return Err(error.into());
        }

        check_video_response(&response)?;
        let encoded = has_content_encoding(&response);
        let (expected_end, known_total) = if status == reqwest::StatusCode::PARTIAL_CONTENT {
            let offset = transfer.as_ref().map(|state| state.media.size).unwrap_or(0);
            if offset == 0 || encoded {
                return Err(
                    PermanentMediaError("远端返回了意外的分片响应（HTTP 206）".into()).into(),
                );
            }
            let content_range = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_content_range)
                .and_then(|range| match range {
                    ParsedContentRange::Slice { start, end, total } => Some((start, end, total)),
                    ParsedContentRange::Complete(_) => None,
                })
                .ok_or_else(|| {
                    PermanentMediaError("远端分片响应缺少有效的 Content-Range".into())
                })?;
            let (start, end, total) = content_range;
            if start != offset {
                return Err(PermanentMediaError(format!(
                    "远端分片起始偏移不匹配（期望 {offset}，实际 {start}）"
                ))
                .into());
            }
            if total.is_some_and(|size| size > params.max_bytes) {
                return Err(PermanentMediaError(format!(
                    "视频超过大小上限 {} MiB",
                    params.max_bytes / 1024 / 1024
                ))
                .into());
            }
            (Some(end.saturating_add(1)), total)
        } else {
            if response
                .content_length()
                .is_some_and(|size| size > params.max_bytes)
            {
                return Err(PermanentMediaError(format!(
                    "视频超过大小上限 {} MiB",
                    params.max_bytes / 1024 / 1024
                ))
                .into());
            }
            if let Some(state) = transfer.as_mut()
                && state.media.size > 0
            {
                let mut file = state.file.take().expect("transfer owns file handle");
                file.set_len(0).await?;
                file.seek(std::io::SeekFrom::Start(0)).await?;
                state.file = Some(file);
                TEMP_BYTES.fetch_sub(state.media.reserved, Ordering::Relaxed);
                state.media.reserved = 0;
                state.media.size = 0;
            }
            if let Some(state) = transfer.as_mut() {
                state.can_range_resume = !encoded;
            }
            let len = (!encoded).then(|| response.content_length()).flatten();
            (len, len)
        };

        if transfer.is_none() {
            transfer = Some(open_temp_transfer(params.directory, !encoded)?);
        }
        let state = transfer.as_mut().expect("transfer initialized");
        let before_attempt = state.media.size;
        let mut file = state.file.take().expect("transfer owns file handle");
        file.seek(std::io::SeekFrom::Start(state.media.size))
            .await?;
        let mut writer = BufWriter::with_capacity(DOWNLOAD_WRITE_BUFFER, file);
        let mut stream = response.bytes_stream();
        let mut stream_error: Option<anyhow::Error> = None;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                stream_error = Some(timeout_error(params.timeout));
                break;
            }
            let chunk_wait = STREAM_CHUNK_IDLE_TIMEOUT.min(remaining);
            match tokio::time::timeout(chunk_wait, stream.next()).await {
                Err(_) => {
                    stream_error = Some(if tokio::time::Instant::now() >= deadline {
                        timeout_error(params.timeout)
                    } else {
                        anyhow::anyhow!("媒体流读取停滞超时")
                    });
                    break;
                }
                Ok(None) => break,
                Ok(Some(Err(error))) => {
                    let error = error.without_url();
                    stream_error = Some(
                        if tokio::time::Instant::now() >= deadline
                            || (error.is_timeout() && remaining <= STREAM_CHUNK_IDLE_TIMEOUT)
                        {
                            timeout_error(params.timeout)
                        } else {
                            error.into()
                        },
                    );
                    break;
                }
                Ok(Some(Ok(chunk))) => {
                    let chunk_len = chunk.len() as u64;
                    let next_size = state.media.size.saturating_add(chunk_len);
                    if next_size > params.max_bytes {
                        stream_error = Some(
                            PermanentMediaError(format!(
                                "视频超过大小上限 {} MiB",
                                params.max_bytes / 1024 / 1024
                            ))
                            .into(),
                        );
                        break;
                    }
                    if let Err(budget_error) = reserve_temp(chunk_len, params.max_temp_bytes) {
                        stream_error = Some(PermanentMediaError(budget_error.to_string()).into());
                        break;
                    }
                    state.media.size = next_size;
                    state.media.reserved += chunk_len;
                    writer.write_all(&chunk).await?;
                }
            }
        }

        writer.flush().await?;
        state.file = Some(writer.into_inner());

        if let Some(error) = stream_error {
            if error.is::<PermanentMediaError>() || error.is::<DownloadTimeoutError>() {
                return Err(error);
            }
            if retries_used < MAX_STREAM_RETRIES {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining > Duration::from_millis(500) {
                    retries_used += 1;
                    tracing::debug!(
                        retries_used,
                        offset = state.media.size,
                        "媒体流中断，尝试断点续传: {}",
                        format_error_chain(&error)
                    );
                    tokio::time::sleep(Duration::from_millis(250).min(remaining)).await;
                    continue;
                }
                return Err(timeout_error(params.timeout));
            }
            return Err(error);
        }

        if let Some(end) = expected_end
            && state.media.size < end
        {
            if retries_used < MAX_STREAM_RETRIES {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining > Duration::from_millis(500) {
                    retries_used += 1;
                    tracing::debug!(
                        retries_used,
                        offset = state.media.size,
                        expected_end = end,
                        "媒体响应提前结束，尝试断点续传"
                    );
                    tokio::time::sleep(Duration::from_millis(250).min(remaining)).await;
                    continue;
                }
                return Err(timeout_error(params.timeout));
            }
            bail!(
                "媒体下载不完整（预期 {end} 字节，实际 {} 字节）",
                state.media.size
            );
        }

        if let Some(total) = known_total
            && state.media.size < total
        {
            if state.media.size > before_attempt {
                continue;
            }
            bail!(
                "媒体分片未继续推进（已完成 {} / {total} 字节）",
                state.media.size
            );
        }
        break;
    }

    let mut state = transfer.ok_or_else(|| PermanentMediaError("下载内容为空".into()))?;
    if state.media.size == 0 {
        return Err(PermanentMediaError("下载内容为空".into()).into());
    }
    let mut file = state.file.take().expect("transfer owns file handle");
    if let Some(key) = params.decode_key {
        let key: u64 = key
            .parse()
            .map_err(|_| PermanentMediaError("无效的视频号解密密钥".into()))?;
        file.seek(std::io::SeekFrom::Start(0)).await?;
        let mut prefix = vec![0; ENCRYPTED_PREFIX.min(state.media.size as usize)];
        file.read_exact(&mut prefix).await?;
        let decrypted = decrypt_channels_video(&prefix, key);
        file.seek(std::io::SeekFrom::Start(0)).await?;
        file.write_all(&decrypted).await?;
    }
    file.seek(std::io::SeekFrom::Start(0)).await?;
    let mut prefix = vec![0u8; 512.min(state.media.size as usize)];
    file.read_exact(&mut prefix).await?;
    check_video_prefix(&prefix)?;
    file.flush().await?;
    drop(file);
    Ok(state.media)
}

#[allow(clippy::too_many_arguments)]
pub async fn download_media_temp(
    url: &str,
    proxies: &[String],
    headers: &HashMap<String, String>,
    directory: &Path,
    max_bytes: u64,
    max_temp_bytes: u64,
    timeout: Duration,
    decode_key: Option<&str>,
) -> Result<TempMedia> {
    check_url(url)?;
    tokio::fs::create_dir_all(directory).await?;
    let headers = safe_headers(headers);
    let params = TempDownloadParams {
        url,
        headers: &headers,
        directory,
        max_bytes,
        max_temp_bytes,
        timeout,
        decode_key,
    };
    let mut last_error = String::new();
    for client in clients(url, proxies) {
        match download_media_temp_with_client(&client, &params).await {
            Ok(temporary) => return Ok(temporary),
            Err(error) if can_try_next_media_client(&error) => {
                last_error = format_error_chain(&error);
            }
            Err(error) => bail!("媒体下载失败: {}", format_error_chain(&error)),
        }
    }
    bail!("媒体下载失败 {last_error}")
}

pub fn decrypt_video_if_needed(mut data: Vec<u8>, key: Option<&str>) -> Result<Vec<u8>> {
    if let Some(key) = key {
        let key = key.parse::<u64>().context("无效的视频号解密密钥")?;
        let length = data.len().min(ENCRYPTED_PREFIX);
        let prefix = decrypt_channels_video(&data[..length], key);
        data[..length].copy_from_slice(&prefix);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_cdn_hosts_without_matching_url_parameters() {
        assert!(is_foreign_url("https://video.twimg.com/file.mp4"));
        assert!(!is_foreign_url("https://example.com/?target=x.com"));
        assert!(!is_foreign_url("https://notx.com/file.mp4"));
    }

    #[test]
    fn decrypts_only_encrypted_prefix() {
        let bytes = vec![17; ENCRYPTED_PREFIX + 128];
        let decrypted = decrypt_video_if_needed(bytes.clone(), Some("123456")).unwrap();
        assert_ne!(decrypted[..ENCRYPTED_PREFIX], bytes[..ENCRYPTED_PREFIX]);
        assert_eq!(decrypted[ENCRYPTED_PREFIX..], bytes[ENCRYPTED_PREFIX..]);
        assert_eq!(
            decrypt_video_if_needed(decrypted, Some("123456")).unwrap(),
            bytes
        );
    }

    #[test]
    fn credential_headers_do_not_reach_media_cdns() {
        let headers = safe_headers(&HashMap::from([
            ("Referer".into(), "https://www.bilibili.com/".into()),
            ("Cookie".into(), "SESSDATA=secret".into()),
            ("Authorization".into(), "Bearer secret".into()),
        ]));
        assert!(headers.contains_key("referer"));
        assert!(!headers.contains_key("cookie"));
        assert!(!headers.contains_key("authorization"));
    }

    #[test]
    fn rejects_manifests_and_html_disguised_as_video() {
        assert!(check_video_prefix(b"#EXTM3U\n#EXT-X-VERSION:3").is_err());
        assert!(check_video_prefix(b" \r\n<!doctype html><html>login</html>").is_err());
        assert!(check_video_prefix(br#"{"message":"login required"}"#).is_err());
        assert!(check_video_prefix(b"\0\0\0\x18ftypisom").is_ok());
    }

    #[test]
    fn external_output_reservation_checks_size_and_releases_unused_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let mut media = TempMedia::reserve(directory.path(), 16384, u64::MAX).unwrap();
        std::fs::write(media.path(), vec![0u8; 8192]).unwrap();
        media.finish().unwrap();
        assert_eq!(media.size(), 8192);
        assert_eq!(media.reserved, 8192);
        let path = media.path().to_owned();
        drop(media);
        assert!(!path.exists());
        let mut oversized = TempMedia::reserve(directory.path(), 1, u64::MAX).unwrap();
        std::fs::write(oversized.path(), [0, 1]).unwrap();
        assert!(oversized.finish().is_err());
        drop(oversized);
        assert!(TempMedia::reserve(directory.path(), 1, 0).is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn bounds_disk_transfers_and_cleans_files_on_failure_and_drop() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/video", listener.local_addr().unwrap());
        let app =
            axum::Router::new().route("/video", axum::routing::get(|| async { vec![42u8; 8192] }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let headers = HashMap::new();
        let failed = download_media_temp(
            &url,
            &[],
            &headers,
            directory.path(),
            4096,
            65536,
            Duration::from_secs(3),
            None,
        )
        .await;
        assert!(failed.is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let failed = download_media_temp(
            &url,
            &[],
            &headers,
            directory.path(),
            16384,
            4096,
            Duration::from_secs(3),
            None,
        )
        .await;
        assert!(failed.is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let temporary = download_media_temp(
            &url,
            &[],
            &headers,
            directory.path(),
            16384,
            65536,
            Duration::from_secs(3),
            None,
        )
        .await
        .unwrap();
        assert_eq!(temporary.size(), 8192);
        assert_eq!(
            tokio::fs::read(temporary.path()).await.unwrap(),
            vec![42u8; 8192]
        );
        let path = temporary.path().to_owned();
        drop(temporary);
        assert!(!path.exists());
        server.abort();
    }

    #[tokio::test]
    async fn rate_limited_media_does_not_try_a_second_route() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/video", listener.local_addr().unwrap());
        let denied = axum::Router::new().fallback(|| async {
            (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                [(reqwest::header::RETRY_AFTER, "60")],
            )
        });
        let origin = tokio::spawn(async move { axum::serve(listener, denied).await.unwrap() });
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxies = [format!("http://{}", proxy.local_addr().unwrap())];
        let directory = tempfile::tempdir().unwrap();

        let error = download_media_temp(
            &url,
            &proxies,
            &HashMap::new(),
            directory.path(),
            16384,
            65536,
            Duration::from_secs(1),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("429"));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), proxy.accept())
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        origin.abort();
    }

    #[tokio::test]
    async fn cancellation_removes_partial_download() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/slow", listener.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/slow",
            axum::routing::get(|| async {
                let first = futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(bytes::Bytes::from(vec![0; 8192]))
                });
                axum::body::Body::from_stream(first.chain(futures_util::stream::pending()))
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_owned();
        let task = tokio::spawn(async move {
            download_media_temp(
                &url,
                &[],
                &HashMap::new(),
                &path,
                16384,
                65536,
                Duration::from_secs(10),
                None,
            )
            .await
        });
        let started = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                // A partial chunk can still be buffered when the transfer is cancelled.
                let opened = std::fs::read_dir(directory.path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| entry.path().is_file());
                if opened {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if started.is_err() {
            let entries: Vec<_> = std::fs::read_dir(directory.path())
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| {
                    (
                        entry.path(),
                        entry.metadata().map(|metadata| metadata.len()),
                    )
                })
                .collect();
            let finished = task.is_finished();
            task.abort();
            let result = task.await;
            panic!(
                "download did not start, finished={finished}, files={entries:?}, outcome={result:?}"
            );
        }
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while std::fs::read_dir(directory.path())
                .unwrap()
                .next()
                .is_some()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn resumes_truncated_stream_with_range_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/video.mp4", listener.local_addr().unwrap());
        let expected: Vec<u8> = (0..8192).map(|index| (index % 251) as u8).collect();
        let full = expected.clone();
        let app = axum::Router::new().route(
            "/video.mp4",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let full = full.clone();
                async move {
                    if let Some(range) = headers.get(axum::http::header::RANGE)
                        && range.to_str().ok() == Some("bytes=4096-")
                    {
                        let part = full[4096..].to_vec();
                        return axum::response::Response::builder()
                            .status(axum::http::StatusCode::PARTIAL_CONTENT)
                            .header(axum::http::header::CONTENT_RANGE, "bytes 4096-8191/8192")
                            .header(axum::http::header::CONTENT_LENGTH, part.len().to_string())
                            .body(axum::body::Body::from(part))
                            .unwrap();
                    }
                    let total_len = full.len();
                    let first = futures_util::stream::once(async move {
                        Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&full[..4096]))
                    })
                    .chain(futures_util::stream::once(async {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "simulated disconnect",
                        ))
                    }));
                    axum::response::Response::builder()
                        .status(axum::http::StatusCode::OK)
                        .header(axum::http::header::CONTENT_LENGTH, total_len.to_string())
                        .body(axum::body::Body::from_stream(first))
                        .unwrap()
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let temporary = download_media_temp(
            &url,
            &[],
            &HashMap::new(),
            directory.path(),
            16384,
            65536,
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
        assert_eq!(temporary.size(), 8192);
        assert_eq!(tokio::fs::read(temporary.path()).await.unwrap(), expected);
        let path = temporary.path().to_owned();
        drop(temporary);
        assert!(!path.exists());
        server.abort();
    }

    #[tokio::test]
    async fn timeout_reports_clear_reason_and_cleans_partial_file() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/slow.mp4", listener.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/slow.mp4",
            axum::routing::get(|| async {
                let first = futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(bytes::Bytes::from(vec![7; 1024]))
                });
                axum::body::Body::from_stream(first.chain(futures_util::stream::pending()))
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let error = download_media_temp(
            &url,
            &[],
            &HashMap::new(),
            directory.path(),
            16384,
            65536,
            Duration::from_millis(250),
            None,
        )
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("超时"),
            "expected clear timeout error, got: {message}"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        server.abort();
    }
}
