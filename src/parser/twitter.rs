use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use chrono::DateTime;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use tracing::{debug, info};

use crate::parser::process::{ProcessError, spawn_capture};
use crate::types::{ImageSize, ParsedData, VideoQuality, YtdlpConfig};

static STATUS_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)(?:twitter\.com|x\.com)/[^/?]+/status(?:es)?/(\d+)").unwrap());

pub fn extract_twitter_status_id(url: &str) -> Option<String> {
    STATUS_RE
        .captures(url)
        .and_then(|capture| capture.get(1))
        .map(|found| found.as_str().to_string())
}

fn bundled_ytdlp() -> Option<PathBuf> {
    let bin = std::env::current_dir().ok()?.join("bin");
    let candidates: &[&str] = if cfg!(windows) {
        &["yt-dlp.exe", "yt-dlp"]
    } else {
        &["yt-dlp", "yt-dlp.exe"]
    };
    candidates
        .iter()
        .map(|name| bin.join(name))
        .find(|path| path.exists())
}

async fn run_ytdlp_json(url: &str, config: &YtdlpConfig) -> anyhow::Result<Value> {
    let mut args = vec!["-J".to_string(), "--no-warnings".to_string()];
    if !config.proxy.is_empty() {
        args.push("--proxy".into());
        args.push(config.proxy.clone());
    }
    args.push(url.to_string());

    let mut candidates = vec![config.command.clone()];
    if let Some(bundled) = bundled_ytdlp() {
        let bundled = bundled.to_string_lossy().to_string();
        if !config.command.contains(&bundled) {
            candidates.push(bundled);
        }
    }
    candidates.push("python -m yt_dlp".into());

    let mut last_error = None;
    for command in candidates {
        match spawn_capture(
            &command,
            &args,
            Duration::from_millis(config.timeout.max(1)),
        )
        .await
        {
            Ok(output) => {
                return serde_json::from_str(&output)
                    .map_err(|err| anyhow::anyhow!("yt-dlp JSON 解析失败: {err}"));
            }
            Err(ProcessError::NotFound(err)) => {
                debug!("yt-dlp 命令不可用，尝试下一个候选: {command} ({err})");
                last_error = Some(err);
            }
            Err(err) => return Err(err.into()),
        }
    }
    anyhow::bail!("{}", last_error.unwrap_or_else(|| "yt-dlp 不可用".into()))
}

fn pick_direct_mp4s(formats: Option<&Vec<Value>>) -> Vec<VideoQuality> {
    let mut videos: Vec<VideoQuality> = formats
        .into_iter()
        .flatten()
        .filter(|format| {
            let url = format
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let protocol = format
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or_default();
            !url.is_empty() && protocol.starts_with("http") && !url.contains(".m3u8")
        })
        .map(|format| VideoQuality {
            quality: format
                .get("format_id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| {
                    format
                        .get("height")
                        .and_then(Value::as_u64)
                        .map(|height| height.to_string())
                })
                .unwrap_or_else(|| "unknown".into()),
            url: format
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            bit_rate: format.get("tbr").and_then(Value::as_u64),
        })
        .collect();
    videos.sort_by(|a, b| b.bit_rate.unwrap_or(0).cmp(&a.bit_rate.unwrap_or(0)));
    videos
}

pub fn map_ytdlp_json(info: &Value) -> ParsedData {
    if let Some(entries) = info.get("entries").and_then(Value::as_array) {
        if let Some(entry) = entries.iter().find(|entry| {
            entry
                .get("formats")
                .and_then(Value::as_array)
                .is_some_and(|formats| !formats.is_empty())
        }) {
            return map_ytdlp_json(entry);
        }
    }

    let videos = pick_direct_mp4s(info.get("formats").and_then(Value::as_array));
    let mut parsed = ParsedData::empty("video");
    parsed.desc = info
        .get("description")
        .or_else(|| info.get("title"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    parsed.title = info
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    parsed.author = info
        .get("uploader")
        .or_else(|| info.get("uploader_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    parsed.uid = info
        .get("uploader_id")
        .or_else(|| info.get("channel_id"))
        .or_else(|| info.get("id"))
        .map(value_to_string)
        .unwrap_or_default();
    let last_thumbnail = info
        .get("thumbnails")
        .and_then(Value::as_array)
        .and_then(|thumbnails| thumbnails.last());
    parsed.cover = last_thumbnail
        .and_then(|thumbnail| thumbnail.get("url"))
        .and_then(Value::as_str)
        .or_else(|| info.get("thumbnail").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    if let Some(thumbnail) = last_thumbnail {
        let width = thumbnail.get("width").and_then(Value::as_u64);
        let height = thumbnail.get("height").and_then(Value::as_u64);
        if let (Some(width), Some(height)) = (width, height) {
            parsed.cover_size = Some(ImageSize {
                width: width as u32,
                height: height as u32,
            });
        }
    }
    parsed.video = videos
        .first()
        .map(|video| video.url.clone())
        .unwrap_or_default();
    parsed.videos = videos;
    parsed.like = info.get("like_count").and_then(Value::as_i64).unwrap_or(0);
    parsed.share = info
        .get("repost_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    parsed.comment = info
        .get("comment_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    parsed.play = info.get("view_count").and_then(Value::as_i64).unwrap_or(0);
    parsed.duration = info
        .get("duration")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .round()
        .max(0.0) as u64;
    parsed.publish_time = info
        .get("timestamp")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .saturating_mul(1000);
    parsed
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn media_size(media: &Value) -> Option<ImageSize> {
    let large = media.get("sizes")?.get("large")?;
    let width = large.get("w")?.as_u64()?;
    let height = large.get("h")?.as_u64()?;
    Some(ImageSize {
        width: width as u32,
        height: height as u32,
    })
}

fn parse_created_at(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_str)
        .and_then(|raw| {
            DateTime::parse_from_rfc2822(raw)
                .ok()
                .or_else(|| DateTime::parse_from_str(raw, "%a %b %d %H:%M:%S %z %Y").ok())
                .or_else(|| {
                    let without_weekday = raw.split_once(' ').map(|(_, rest)| rest).unwrap_or(raw);
                    DateTime::parse_from_str(without_weekday, "%b %d %H:%M:%S %z %Y").ok()
                })
        })
        .map(|date| date.timestamp_millis())
        .unwrap_or(0)
}

pub fn map_syndication(data: &Value) -> ParsedData {
    let media = data
        .get("mediaDetails")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let photos: Vec<&Value> = media
        .iter()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("photo")
                && item
                    .get("media_url_https")
                    .and_then(Value::as_str)
                    .is_some()
        })
        .collect();
    let videos: Vec<&Value> = media
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("video") | Some("animated_gif")
            ) && item.get("video_info").is_some()
        })
        .collect();

    if !videos.is_empty() {
        let mut variants: Vec<&Value> = videos
            .iter()
            .filter_map(|media| {
                media
                    .pointer("/video_info/variants")
                    .and_then(Value::as_array)
            })
            .flatten()
            .filter(|variant| {
                variant.get("content_type").and_then(Value::as_str) == Some("video/mp4")
                    && variant.get("url").and_then(Value::as_str).is_some()
            })
            .collect();
        variants.sort_by_key(|variant| {
            std::cmp::Reverse(variant.get("bitrate").and_then(Value::as_u64).unwrap_or(0))
        });
        if !variants.is_empty() {
            let mut parsed = ParsedData::empty("video");
            parsed.desc = data
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            parsed.author = data
                .pointer("/user/name")
                .or_else(|| data.pointer("/user/screen_name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            parsed.uid = data
                .pointer("/user/screen_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            parsed.avatar = data
                .pointer("/user/profile_image_url_https")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            parsed.cover = videos[0]
                .get("media_url_https")
                .and_then(Value::as_str)
                .map(|url| format!("{url}?name=large"))
                .unwrap_or_default();
            parsed.cover_size = media_size(videos[0]);
            parsed.video = variants[0]
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            parsed.videos = variants
                .into_iter()
                .map(|variant| VideoQuality {
                    quality: variant
                        .get("bitrate")
                        .and_then(Value::as_u64)
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    url: variant
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    bit_rate: variant.get("bitrate").and_then(Value::as_u64),
                })
                .collect();
            parsed.like = data
                .get("favorite_count")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            parsed.publish_time = parse_created_at(data.get("created_at"));
            parsed.duration = videos[0]
                .pointer("/video_info/duration_millis")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(500)
                / 1000;
            return parsed;
        }
    }

    if !photos.is_empty() {
        let mut parsed = ParsedData::empty("image");
        parsed.desc = data
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        parsed.author = data
            .pointer("/user/name")
            .or_else(|| data.pointer("/user/screen_name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        parsed.uid = data
            .pointer("/user/screen_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        parsed.avatar = data
            .pointer("/user/profile_image_url_https")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut seen = HashSet::new();
        let mut size_map = std::collections::HashMap::new();
        for photo in photos {
            let Some(base) = photo.get("media_url_https").and_then(Value::as_str) else {
                continue;
            };
            let url = format!("{base}?name=large");
            if seen.insert(base.to_string()) {
                parsed.images.push(url.clone());
            }
            if let Some(size) = media_size(photo) {
                size_map.insert(url, size);
            }
        }
        parsed.cover = parsed.images.first().cloned().unwrap_or_default();
        parsed.cover_size = size_map.get(&parsed.cover).cloned();
        parsed.image_size_map = Some(size_map);
        parsed.like = data
            .get("favorite_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        parsed.publish_time = parse_created_at(data.get("created_at"));
        return parsed;
    }

    let mut parsed = ParsedData::empty("image");
    parsed.desc = data
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    parsed.author = data
        .pointer("/user/name")
        .or_else(|| data.pointer("/user/screen_name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    parsed.uid = data
        .pointer("/user/screen_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    parsed.avatar = data
        .pointer("/user/profile_image_url_https")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    parsed.like = data
        .get("favorite_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    parsed.publish_time = parse_created_at(data.get("created_at"));
    parsed
}

async fn fetch_syndication(status_id: &str, config: &YtdlpConfig) -> anyhow::Result<ParsedData> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(20));
    if !config.proxy.is_empty() {
        builder = builder.proxy(reqwest::Proxy::all(&config.proxy)?);
    }
    let response = builder
        .build()?
        .get(format!(
            "https://cdn.syndication.twimg.com/tweet-result?id={status_id}&token=x"
        ))
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
        )
        .send()
        .await?;
    let status = response.status();
    if status.as_u16() != 200 {
        anyhow::bail!("syndication 接口返回异常: HTTP {}", status.as_u16());
    }
    let data: Value = response.json().await?;
    if !data.is_object() {
        anyhow::bail!("syndication 接口返回异常: 响应不是对象");
    }
    Ok(map_syndication(&data))
}

pub async fn parse_twitter_local(url: &str, config: &YtdlpConfig) -> anyhow::Result<ParsedData> {
    let status_id = extract_twitter_status_id(url);
    let mut last_error: Option<String>;

    match run_ytdlp_json(url, config).await {
        Ok(info) => {
            let parsed = map_ytdlp_json(&info);
            if parsed.has_media() {
                info!("yt-dlp 本地解析成功: {url}");
                return Ok(parsed);
            }
            last_error = Some("yt-dlp 未解析出媒体内容".to_string());
        }
        Err(err) => {
            last_error = Some(err.to_string());
            debug!("yt-dlp 解析失败: {err}");
        }
    }

    if let Some(status_id) = status_id {
        match fetch_syndication(&status_id, config).await {
            Ok(parsed) => {
                info!("syndication 兜底解析成功: {url}");
                return Ok(parsed);
            }
            Err(err) => {
                last_error = Some(err.to_string());
                debug!("syndication 兜底失败: {err}");
            }
        }
    }
    anyhow::bail!("{}", last_error.unwrap_or_else(|| "本地解析失败".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_status_ids() {
        assert_eq!(
            extract_twitter_status_id("https://x.com/Tz_2022/status/2102501726431687055?s=20")
                .as_deref(),
            Some("2102501726431687055")
        );
        assert_eq!(
            extract_twitter_status_id("https://twitter.com/a/statuses/123456").as_deref(),
            Some("123456")
        );
        assert!(extract_twitter_status_id("https://x.com/nostatushere").is_none());
    }

    #[test]
    fn maps_ytdlp_video_and_skips_hls() {
        let fixture = serde_json::json!({
            "id": "2102457077390299136",
            "title": "Tz - 测试推文",
            "description": "测试推文 https://t.co/abc",
            "uploader": "Tz",
            "uploader_id": "Tz_2022",
            "timestamp": 1790110448,
            "duration": 13.541,
            "view_count": 57825, "like_count": 372, "repost_count": 27, "comment_count": 34,
            "thumbnail": "https://pbs.twimg.com/amplify_video_thumb/2102457077390299136/img/x.jpg?name=orig",
            "formats": [
                { "format_id": "hls-1414", "protocol": "m3u8_native", "url": "https://video.twimg.com/x.m3u8", "tbr": 1414, "height": 720 },
                { "format_id": "http-288", "protocol": "https", "url": "https://video.twimg.com/low.mp4", "tbr": 288, "height": 270 },
                { "format_id": "http-2176", "protocol": "https", "url": "https://video.twimg.com/high.mp4", "tbr": 2176, "height": 720 }
            ]
        });
        let parsed = map_ytdlp_json(&fixture);
        assert_eq!(parsed.media_type, "video");
        assert_eq!(parsed.video, "https://video.twimg.com/high.mp4");
        assert_eq!(parsed.videos.len(), 2);
        assert_eq!(parsed.duration, 14);
        assert_eq!(parsed.publish_time, 1_790_110_448_000);
    }

    #[test]
    fn maps_syndication_images() {
        let fixture = serde_json::json!({
            "text": "cos 照片 ( 2/2 )",
            "created_at": "Wed Sep 24 02:06:01 +0000 2026",
            "favorite_count": 81,
            "user": { "name": "몰루", "screen_name": "mollupan", "profile_image_url_https": "https://pbs.twimg.com/profile_images/x.jpg" },
            "mediaDetails": [
                { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/HS8nIhRa4AAyHOv.jpg" },
                { "type": "photo", "media_url_https": "https://pbs.twimg.com/media/HS8nIhNaQAALuhJ.jpg" }
            ]
        });
        let parsed = map_syndication(&fixture);
        assert_eq!(parsed.media_type, "image");
        assert_eq!(parsed.images.len(), 2);
        assert!(parsed.images[0].contains("name=large"));
        assert_eq!(parsed.author, "몰루");
        assert_eq!(parsed.uid, "mollupan");
        assert_eq!(parsed.like, 81);
        assert!(parsed.publish_time > 0);
    }

    #[test]
    fn maps_syndication_video_variants() {
        let fixture = serde_json::json!({
            "text": "video tweet",
            "created_at": "Mon Sep 22 20:54:08 +0000 2026",
            "favorite_count": 10,
            "user": { "name": "Tz", "screen_name": "Tz_2022" },
            "mediaDetails": [{
                "type": "video",
                "media_url_https": "https://pbs.twimg.com/ext_tw_video_thumb/x/pu/img/y.jpg",
                "video_info": {
                    "duration_millis": 13541,
                    "variants": [
                        { "content_type": "video/mp4", "url": "https://video.twimg.com/low.mp4", "bitrate": 288000 },
                        { "content_type": "video/mp4", "url": "https://video.twimg.com/high.mp4", "bitrate": 2176000 },
                        { "content_type": "application/x-mpegURL", "url": "https://video.twimg.com/x.m3u8" }
                    ]
                }
            }]
        });
        let parsed = map_syndication(&fixture);
        assert_eq!(parsed.media_type, "video");
        assert_eq!(parsed.video, "https://video.twimg.com/high.mp4");
        assert_eq!(parsed.duration, 14);
    }
}
