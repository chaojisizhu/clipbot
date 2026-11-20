use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::parser::http::SiteHttp;
use chrono::DateTime;
use serde_json::Value;
use tracing::debug;
use url::Url;

use crate::parser::process::{ProcessError, spawn_capture};
use crate::types::{ImageSize, ParsedData, VideoQuality, YtdlpConfig};

#[path = "twitter_detail.rs"]
mod detail;
pub(super) use detail::{TwitterDetailsError, parse_authenticated_details};

const GENERIC_FORMAT_SELECTOR: &str = "best*[ext=mp4][vcodec!=?none][acodec!=?none]/best*[protocol=https][vcodec!=?none][acodec!=?none]";
const TWITTER_FORMAT_SELECTOR: &str =
    "best*[ext=mp4][protocol=https][vcodec!=?none]/best*[ext=mp4][protocol=http][vcodec!=?none]";
// Keep a fixed byte budget in the shared parse cache even for very large notes.
const MAX_POST_BODY_BYTES: usize = 64 * 1024;

pub fn extract_twitter_status_id(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(
            url.host_str(),
            Some(
                "x.com"
                    | "www.x.com"
                    | "mobile.x.com"
                    | "twitter.com"
                    | "www.twitter.com"
                    | "mobile.twitter.com"
            )
        )
    {
        return None;
    }
    let mut segments = url.path_segments()?;
    let account = segments.next()?;
    if account.is_empty() {
        return None;
    }
    let mut status = segments.next()?;
    if account == "i" && status == "web" {
        status = segments.next()?;
    }
    if !matches!(status, "status" | "statuses") {
        return None;
    }
    let id = segments.next()?;
    (!id.is_empty() && id.len() <= 20 && id.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| id.to_owned())
}

fn twitter_post_url(id: &str) -> String {
    format!("https://x.com/i/status/{id}")
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

async fn run_ytdlp_json(
    url: &str,
    config: &YtdlpConfig,
    cookie_file: Option<&str>,
) -> anyhow::Result<Value> {
    let mut args = vec![
        "--dump-single-json".into(),
        "--skip-download".into(),
        "--no-playlist".into(),
        "--ignore-config".into(),
        "--no-warnings".into(),
        "--sleep-requests".into(),
        "0.8".into(),
        "--socket-timeout".into(),
        "15".into(),
        "--retries".into(),
        "1".into(),
        "--format".into(),
        ytdlp_format_selector(url).into(),
    ];
    let cookie_file = cookie_file
        .filter(|path| !path.trim().is_empty())
        .unwrap_or(&config.cookie_file);
    if !cookie_file.is_empty() {
        args.extend(["--cookies".into(), cookie_file.into()]);
    }
    if !config.proxy.is_empty() {
        args.push("--proxy".into());
        args.push(config.proxy.clone());
    }
    args.extend(["--".into(), url.to_string()]);

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
            Err(err) => return Err(safe_ytdlp_error(err)),
        }
    }
    anyhow::bail!("{}", last_error.unwrap_or_else(|| "yt-dlp 不可用".into()))
}

fn ytdlp_format_selector(raw_url: &str) -> &'static str {
    if extract_twitter_status_id(raw_url).is_some() {
        TWITTER_FORMAT_SELECTOR
    } else {
        GENERIC_FORMAT_SELECTOR
    }
}

fn safe_ytdlp_error(error: ProcessError) -> anyhow::Error {
    let message = error.to_string().to_ascii_lowercase();
    if ["cookie file entry", "auth_token", "ct0"]
        .iter()
        .any(|marker| message.contains(marker))
    {
        anyhow::anyhow!("本地 yt-dlp 无法读取登录 Cookie，请检查 Netscape Cookie 文件格式")
    } else {
        error.into()
    }
}

fn is_direct_mp4(url: &str, protocol: &str) -> bool {
    matches!(protocol, "http" | "https")
        && url::Url::parse(url).ok().is_some_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.path().to_ascii_lowercase().ends_with(".mp4")
        })
}

fn pick_direct_mp4s(formats: Option<&Vec<Value>>, allow_silent: bool) -> Vec<VideoQuality> {
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
            !url.is_empty()
                && matches!(protocol, "http" | "https")
                && !url.contains(".m3u8")
                && format.get("vcodec").and_then(Value::as_str) != Some("none")
                && (format.get("acodec").and_then(Value::as_str) != Some("none")
                    || allow_silent && is_direct_mp4(url, protocol))
                && format
                    .get("ext")
                    .and_then(Value::as_str)
                    .is_none_or(|ext| ext == "mp4")
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
    videos.sort_by_key(|video| std::cmp::Reverse(video.bit_rate.unwrap_or(0)));
    videos
}

pub fn map_ytdlp_json(info: &Value) -> ParsedData {
    if let Some(entries) = info.get("entries").and_then(Value::as_array)
        && let Some(entry) = entries.iter().find(|entry| {
            entry
                .get("formats")
                .and_then(Value::as_array)
                .is_some_and(|formats| !formats.is_empty())
        })
    {
        let mut parsed = map_ytdlp_json(entry);
        parsed.post_body_is_preview |= ytdlp_twitter_is_preview(info, &parsed.desc);
        return parsed;
    }

    let source = info
        .get("extractor_key")
        .or_else(|| info.get("extractor"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let allow_silent = matches!(
        source.as_str(),
        "twitter" | "twitteramplify" | "twitter:amplify"
    );
    let videos = pick_direct_mp4s(info.get("formats").and_then(Value::as_array), allow_silent);
    let mut parsed = ParsedData::empty("video");
    parsed.desc = info
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| info.get("title").and_then(Value::as_str))
        .unwrap_or_default()
        .trim()
        .to_string();
    parsed.post_body_is_preview = ytdlp_twitter_is_preview(info, &parsed.desc);
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
    let selected_url = info.get("url").and_then(Value::as_str).unwrap_or_default();
    let selected_protocol = info
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if parsed.video.is_empty()
        && (info.get("acodec").and_then(Value::as_str) != Some("none")
            || allow_silent && is_direct_mp4(selected_url, selected_protocol))
        && info.get("vcodec").and_then(Value::as_str) != Some("none")
        && info.get("ext").and_then(Value::as_str) == Some("mp4")
    {
        parsed.video = selected_url.to_string();
    }
    if let Some(headers) = info.get("http_headers").and_then(Value::as_object) {
        for name in ["Referer", "User-Agent", "Origin"] {
            if let Some(value) = headers.get(name).and_then(Value::as_str) {
                parsed.media_headers.insert(name.into(), value.into());
            }
        }
    }
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

fn ytdlp_twitter_is_preview(info: &Value, desc: &str) -> bool {
    let extractor = info
        .get("extractor_key")
        .or_else(|| info.get("extractor"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !extractor.eq_ignore_ascii_case("twitter")
        && !extractor.eq_ignore_ascii_case("twitteramplify")
        && !extractor.eq_ignore_ascii_case("twitter:amplify")
    {
        return false;
    }
    let media_suffix = desc
        .rsplit_once(char::is_whitespace)
        .filter(|(_, tail)| tail.starts_with("https://t.co/"))
        .map(|(body, _)| body.trim_end());
    // yt-dlp does not expose whether it read a Note Tweet. X ends most
    // truncated descriptions with an ellipsis immediately before the media URL.
    info.get("truncated").and_then(Value::as_bool) == Some(true)
        || info.get("note_tweet").is_some_and(|note| !note.is_null())
        || media_suffix
            .is_some_and(|body| body.len() >= 140 && (body.ends_with('…') || body.ends_with("...")))
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn media_size(media: &Value) -> Option<ImageSize> {
    let size = media
        .pointer("/sizes/large")
        .or_else(|| media.get("original_info"))?;
    let width = size.get("w").or_else(|| size.get("width"))?.as_u64()?;
    let height = size.get("h").or_else(|| size.get("height"))?.as_u64()?;
    let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    (width > 0 && height > 0).then_some(ImageSize { width, height })
}

fn parse_created_at(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_str)
        .and_then(|raw| {
            DateTime::parse_from_rfc3339(raw)
                .ok()
                .or_else(|| DateTime::parse_from_rfc2822(raw).ok())
                .or_else(|| DateTime::parse_from_str(raw, "%a %b %d %H:%M:%S %z %Y").ok())
                .or_else(|| {
                    let without_weekday = raw.split_once(' ').map(|(_, rest)| rest).unwrap_or(raw);
                    DateTime::parse_from_str(without_weekday, "%b %d %H:%M:%S %z %Y").ok()
                })
        })
        .map(|date| date.timestamp_millis())
        .unwrap_or(0)
}

fn twitter_note_text(data: &Value) -> Option<&str> {
    [
        "/note_tweet/note_tweet_results/result/text",
        "/note_tweet/text",
    ]
    .into_iter()
    .filter_map(|path| data.pointer(path).and_then(Value::as_str))
    .map(str::trim)
    .find(|body| !body.is_empty())
}

fn twitter_short_text(data: &Value) -> &str {
    ["/full_text", "/text"]
        .into_iter()
        .filter_map(|path| data.pointer(path).and_then(Value::as_str))
        .map(str::trim)
        .find(|body| !body.is_empty())
        .unwrap_or_default()
}

fn store_twitter_body(parsed: &mut ParsedData, data: &Value) {
    parsed.desc = twitter_short_text(data).to_owned();
    if let Some(text) = twitter_note_text(data) {
        let end = text.floor_char_boundary(MAX_POST_BODY_BYTES);
        parsed.post_body = text[..end].to_string();
        parsed.post_body_is_preview = end < text.len();
    } else {
        parsed.post_body_is_preview = data.get("note_tweet").is_some_and(|note| !note.is_null())
            || data.get("truncated").and_then(Value::as_bool) == Some(true);
    }
}

fn public_media_hint(data: &Value) -> bool {
    data.get("possibly_sensitive").and_then(Value::as_bool) == Some(true)
        || ["/mediaDetails", "/photos", "/entities/media"]
            .iter()
            .any(|path| {
                data.pointer(path)
                    .and_then(Value::as_array)
                    .is_some_and(|items| !items.is_empty())
            })
}

fn extract_quoted_tweet_text(data: &Value) -> Option<String> {
    let quoted = data.get("quoted_tweet")?;
    let q_text = twitter_note_text(quoted).unwrap_or_else(|| twitter_short_text(quoted));
    if q_text.is_empty() {
        return None;
    }
    let q_text = &q_text[..q_text.floor_char_boundary(MAX_POST_BODY_BYTES)];
    let author = quoted
        .pointer("/user/screen_name")
        .or_else(|| quoted.pointer("/user/name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if author.is_empty() {
        Some(format!("引用推文：\n{q_text}"))
    } else {
        Some(format!("引用 @{author}：\n{q_text}"))
    }
}

fn populate_syndication_metadata(parsed: &mut ParsedData, data: &Value) {
    store_twitter_body(parsed, data);
    if let Some(id) = data.get("id_str").and_then(Value::as_str).filter(|id| {
        !id.is_empty() && id.len() <= 20 && id.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        parsed.source_post_url = twitter_post_url(id);
    }
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
    parsed.comment = data
        .get("conversation_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    parsed.publish_time = parse_created_at(data.get("created_at"));
}

pub fn map_syndication(data: &Value) -> ParsedData {
    let media = data
        .get("mediaDetails")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
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
            populate_syndication_metadata(&mut parsed, data);
            parsed.cover = videos[0]
                .get("media_url_https")
                .and_then(Value::as_str)
                .map(twitter_image_large)
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
        populate_syndication_metadata(&mut parsed, data);
        let mut seen = HashSet::new();
        let mut size_map = std::collections::HashMap::new();
        for photo in photos {
            let Some(base) = photo.get("media_url_https").and_then(Value::as_str) else {
                continue;
            };
            let url = twitter_image_large(base);
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
        return parsed;
    }

    let mut parsed = ParsedData::empty("text");
    populate_syndication_metadata(&mut parsed, data);
    if let Some(quoted) = extract_quoted_tweet_text(data) {
        if parsed.desc.is_empty() {
            parsed.desc = quoted;
        } else {
            parsed.desc = format!("{}\n\n{quoted}", parsed.desc);
        }
    }
    parsed
}

fn twitter_image_large(raw: &str) -> String {
    if !raw.contains('?') {
        return format!("{raw}?name=large");
    }
    let Ok(mut url) = Url::parse(raw) else {
        return raw.to_owned();
    };
    let other_params: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "name")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.query_pairs_mut()
        .clear()
        .extend_pairs(other_params)
        .append_pair("name", "large");
    url.into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TwitterMediaUnavailable;

impl std::fmt::Display for TwitterMediaUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("X 未返回公开媒体（原帖可能已删除、设私或受年龄/登录限制）")
    }
}

impl std::error::Error for TwitterMediaUnavailable {}

pub async fn parse_native(http: &SiteHttp, url: &str) -> anyhow::Result<ParsedData> {
    let (parsed, _) = parse_public_with_media_hint(http, url).await?;
    Ok(parsed)
}

pub(super) async fn parse_public_with_media_hint(
    http: &SiteHttp,
    url: &str,
) -> anyhow::Result<(ParsedData, bool)> {
    let status_id =
        extract_twitter_status_id(url).ok_or_else(|| anyhow::anyhow!("X帖子链接格式错误"))?;
    let endpoint =
        format!("https://cdn.syndication.twimg.com/tweet-result?id={status_id}&token=x&lang=en");
    let data = http.get_json(&endpoint, "twitter").await?;
    let mut parsed = map_syndication(&data);
    parsed.source_post_url = twitter_post_url(&status_id);
    let is_tombstone = data.get("__typename").and_then(Value::as_str) == Some("TweetTombstone")
        || data.get("tombstone").is_some();
    if is_tombstone || !parsed.has_deliverable() {
        return Err(TwitterMediaUnavailable.into());
    }
    let should_check_media = !parsed.has_media() && public_media_hint(&data);
    Ok((parsed, should_check_media))
}

pub async fn parse_ytdlp(
    url: &str,
    config: &YtdlpConfig,
    cookie_file: Option<&str>,
) -> anyhow::Result<ParsedData> {
    let info = run_ytdlp_json(url, config, cookie_file).await?;
    let mut parsed = map_ytdlp_json(&info);
    if let Some(id) = extract_twitter_status_id(url) {
        parsed.source_post_url = twitter_post_url(&id);
    }
    if !parsed.has_media() {
        anyhow::bail!("本地 yt-dlp 未获得完整的直链 MP4");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_parse_errors_do_not_expose_cookie_values() {
        let error = safe_ytdlp_error(ProcessError::Failed(
            "WARNING: skipping cookie file entry '.x.com\\tTRUE\\t/\\tTRUE\\t0\\tauth_token\\tfixture-secret-value'".into(),
        ));
        assert!(error.to_string().contains("Netscape"));
        assert!(!error.to_string().contains("fixture-secret-value"));
        assert_eq!(
            safe_ytdlp_error(ProcessError::Failed(
                "HTTP Error 429: Too Many Requests".into()
            ))
            .to_string(),
            "HTTP Error 429: Too Many Requests"
        );
    }

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
        assert_eq!(
            extract_twitter_status_id("https://x.com/i/web/status/123456").as_deref(),
            Some("123456")
        );
        assert!(
            extract_twitter_status_id(
                "https://example.com/?next=https://x.com/example/status/123456"
            )
            .is_none()
        );
        assert!(extract_twitter_status_id("https://x.com/example/status/123abc").is_none());
    }

    #[test]
    fn accepts_unknown_codec_metadata_and_silent_twitter_mp4_only() {
        let unknown = serde_json::json!({
            "extractor_key":"Twitter",
            "formats":[{"format_id":"http-2176", "protocol":"https", "ext":"mp4",
                "url":"https://video.twimg.com/clip.mp4", "vcodec":null, "acodec":null}]
        });
        assert_eq!(
            map_ytdlp_json(&unknown).video,
            "https://video.twimg.com/clip.mp4"
        );
        let mut silent = unknown;
        silent["formats"][0]["acodec"] = Value::String("none".into());
        assert!(map_ytdlp_json(&silent).has_media());
        silent["extractor_key"] = Value::String("Generic".into());
        assert!(!map_ytdlp_json(&silent).has_media());
        silent["extractor_key"] = Value::String("Twitter".into());
        silent["formats"][0]["vcodec"] = Value::String("none".into());
        assert!(!map_ytdlp_json(&silent).has_media());
        assert_eq!(
            ytdlp_format_selector("https://x.com/example/status/123"),
            TWITTER_FORMAT_SELECTOR
        );
        assert_eq!(
            ytdlp_format_selector("https://example.com/?url=x.com/example/status/123"),
            GENERIC_FORMAT_SELECTOR
        );
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
    fn identifies_twitter_video_preview_without_querying_for_short_captions() {
        let preview = serde_json::json!({
            "extractor_key":"Twitter",
            "description":format!("{}… https://t.co/example", "文".repeat(160)),
        });
        let long = map_ytdlp_json(&preview);
        assert!(long.post_body_is_preview);
        let ordinary = map_ytdlp_json(&serde_json::json!({
            "extractor_key":"Twitter", "description":"好看… https://t.co/example",
        }));
        assert!(!ordinary.post_body_is_preview);
    }

    #[test]
    fn distinguishes_missing_note_body_and_preserves_a_complete_note() {
        let preview = serde_json::json!({
            "id_str":"123",
            "note_tweet":{"id":"456"},
            "text":"预览…",
            "mediaDetails":[{
                "type":"photo",
                "media_url_https":"https://pbs.twimg.com/media/x?format=jpg&name=small",
            }]
        });
        let parsed = map_syndication(&preview);
        assert!(parsed.post_body_is_preview);
        assert!(parsed.post_body.is_empty());
        assert_eq!(
            parsed.images[0],
            "https://pbs.twimg.com/media/x?format=jpg&name=large"
        );
        let mut complete = preview;
        complete["note_tweet"]["text"] = Value::String("第一段\n\n第二段".into());
        let parsed = map_syndication(&complete);
        assert!(!parsed.post_body_is_preview);
        assert_eq!(parsed.post_body, "第一段\n\n第二段");
        assert_eq!(parsed.images.len(), 1);
        complete["note_tweet"]["text"] = Value::String("🩷文".repeat(12_000));
        let bounded = map_syndication(&complete);
        assert!(bounded.post_body_is_preview);
        assert!(bounded.post_body.len() <= MAX_POST_BODY_BYTES);
        assert!(MAX_POST_BODY_BYTES - bounded.post_body.len() < 4);
    }

    #[test]
    fn maps_syndication_images() {
        let fixture = serde_json::json!({
            "text": "cos 照片 ( 2/2 )",
            "possibly_sensitive": true,
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
    fn detects_public_media_hints_for_text_only_responses() {
        assert!(public_media_hint(&serde_json::json!({
            "text":"preview", "possibly_sensitive":true
        })));
        assert!(public_media_hint(&serde_json::json!({
            "text":"preview", "entities":{"media":[{}]}
        })));
        assert!(!public_media_hint(&serde_json::json!({
            "text":"ordinary post", "possibly_sensitive":false, "photos":[]
        })));
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

    #[test]
    fn maps_text_only_and_rejects_tombstone_tweets() {
        let text_only = serde_json::json!({
            "__typename": "Tweet",
            "id_str": "2122240158502437045",
            "text": "纯文字长推文",
            "created_at": "2006-03-21T20:50:14.000Z",
            "conversation_count": 12,
            "favorite_count": 88,
            "user": { "name": "Dzy", "screen_name": "dzy_test" },
            "quoted_tweet": {
                "text": "被引用的推文正文",
                "user": { "screen_name": "quoted_user" }
            }
        });
        let parsed = map_syndication(&text_only);
        assert_eq!(parsed.media_type, "text");
        assert!(parsed.has_deliverable());
        assert!(!parsed.has_media());
        assert_eq!(parsed.publish_time, 1_142_974_214_000);
        assert_eq!(parsed.comment, 12);
        assert_eq!(parsed.like, 88);
        assert_eq!(
            parsed.desc,
            "纯文字长推文\n\n引用 @quoted_user：\n被引用的推文正文"
        );

        let tombstone = serde_json::json!({
            "__typename": "TweetTombstone",
            "tombstone": { "text": { "text": "Age-restricted" } }
        });
        assert!(!map_syndication(&tombstone).has_deliverable());
        assert!(!map_syndication(&serde_json::json!({})).has_deliverable());
    }
}
