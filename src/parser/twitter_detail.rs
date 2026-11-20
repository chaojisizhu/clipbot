use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

use crate::parser::process::{ProcessError, spawn_capture};
use crate::types::{ParsedData, YtdlpConfig};

use super::{extract_twitter_status_id, is_direct_mp4, map_syndication};

// The helper examines one post. Large responses should not enter the parse cache.
const MAX_METADATA_BYTES: usize = 512 * 1024;
const DETAILS_TIMEOUT_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(in crate::parser) enum TwitterDetailsError {
    #[error("未安装本地 gallery-dl 或未启用 X 详情查询")]
    Unavailable,
    #[error("X 详情查询暂时限流，请稍后再试")]
    RateLimited,
    #[error("X 登录态不可用，请检查配置的 Netscape Cookie 文件")]
    LoginRequired,
    #[error("当前 X 账号没有查看此帖的权限，或原帖已不可访问")]
    AccessDenied,
    #[error("X 未返回受支持的图片或直链 MP4")]
    UnsupportedMedia,
    #[error("X 详情查询失败，请稍后重试")]
    Failed,
}

pub(in crate::parser) async fn parse_authenticated_details(
    url: &str,
    config: &YtdlpConfig,
    cookie_file: &str,
) -> Result<ParsedData, TwitterDetailsError> {
    let id = extract_twitter_status_id(url).ok_or(TwitterDetailsError::Failed)?;
    let command = config.x_details_command.trim();
    if command.is_empty() {
        return Err(TwitterDetailsError::Unavailable);
    }
    if cookie_file.trim().is_empty() {
        return Err(TwitterDetailsError::LoginRequired);
    }
    let mut args: Vec<String> = [
        "--config-ignore",
        "--dump-json",
        "--filter",
        "False", // Only the post metadata record is needed.
        "--cache-file",
        ":memory:",
        "--no-colors",
        "--retries",
        "0",
        "--http-timeout",
        "6",
        "-o",
        "extractor.cookies-update=false",
        "-o",
        "extractor.twitter.transform=false",
        "-o",
        "extractor.twitter.text-tweets=true",
        "-o",
        "extractor.twitter.tweet-endpoint=result",
        "-o",
        "extractor.twitter.articles=false",
        "-o",
        "extractor.twitter.ratelimit=abort",
        "--cookies",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(cookie_file.to_owned());
    if !config.proxy.trim().is_empty() {
        args.extend(["--proxy".into(), config.proxy.clone()]);
    }
    args.extend(["--".into(), format!("https://x.com/i/web/status/{id}")]);
    let timeout = Duration::from_millis(config.timeout.clamp(1, DETAILS_TIMEOUT_MS));
    let output = match spawn_capture(command, &args, timeout).await {
        Err(ProcessError::NotFound(_))
            if command == "gallery-dl" && Path::new(".venv/bin/gallery-dl").is_file() =>
        {
            spawn_capture(".venv/bin/gallery-dl", &args, timeout).await
        }
        result => result,
    }
    .map_err(|error| match error {
        ProcessError::NotFound(_) => TwitterDetailsError::Unavailable,
        ProcessError::Failed(message) => classify_failure(&message),
    })?;
    if output.len() > MAX_METADATA_BYTES {
        return Err(TwitterDetailsError::Failed);
    }
    let rows = serde_json::from_str(&output).map_err(|_| TwitterDetailsError::Failed)?;
    map_gallery_details(&id, rows)
}

fn classify_failure(detail: &str) -> TwitterDetailsError {
    let detail = detail.to_ascii_lowercase();
    if detail.contains("rate limit exceeded")
        || detail.contains("too many requests")
        || detail.contains("http 429")
    {
        TwitterDetailsError::RateLimited
    } else if [
        "nsfw tweet",
        "protected tweet",
        "age-restricted",
        "tweet unavailable",
        "403 forbidden",
        "'unavailable'",
        "not authorized to see",
    ]
    .iter()
    .any(|marker| detail.contains(marker))
    {
        TwitterDetailsError::AccessDenied
    } else if [
        "authrequired",
        "authorizationerror",
        "login required",
        "authenticated cookies",
        "could not authenticate",
        "401 unauthorized",
        "cookies:",
        "auth_token",
    ]
    .iter()
    .any(|marker| detail.contains(marker))
    {
        TwitterDetailsError::LoginRequired
    } else {
        TwitterDetailsError::Failed
    }
}

fn is_twitter_cdn_url(value: &Value, host: &str) -> bool {
    value
        .as_str()
        .and_then(|raw| Url::parse(raw).ok())
        .is_some_and(|url| {
            url.scheme() == "https"
                && url.host_str() == Some(host)
                && url.port().is_none()
                && url.username().is_empty()
                && url.password().is_none()
        })
}

fn normalized_media(media: &Value) -> Option<Value> {
    if media
        .pointer("/ext_media_availability/status")
        .and_then(Value::as_str)
        == Some("Unavailable")
    {
        return None;
    }
    match media.get("type").and_then(Value::as_str)? {
        "photo" if is_twitter_cdn_url(&media["media_url_https"], "pbs.twimg.com") => {
            Some(media.clone())
        }
        "video" | "animated_gif" => {
            let variants: Vec<_> = media
                .pointer("/video_info/variants")?
                .as_array()?
                .iter()
                .filter(|variant| {
                    variant["content_type"] == "video/mp4"
                        && is_twitter_cdn_url(&variant["url"], "video.twimg.com")
                        && variant["url"]
                            .as_str()
                            .is_some_and(|url| is_direct_mp4(url, "https"))
                })
                .collect();
            let cover = &media["media_url_https"];
            Some(json!({
                "type": media["type"],
                "media_url_https": is_twitter_cdn_url(cover, "pbs.twimg.com").then_some(cover),
                "sizes": media["sizes"],
                "original_info": media["original_info"],
                "video_info": {
                    "variants": variants,
                    "duration_millis": media["video_info"]["duration_millis"],
                },
            }))
        }
        _ => None,
    }
}

fn map_gallery_details(id: &str, rows: Value) -> Result<ParsedData, TwitterDetailsError> {
    let rows = rows.as_array().ok_or(TwitterDetailsError::Failed)?;
    if let Some(error) = rows.iter().find(|row| row[0].as_i64() == Some(-1)) {
        let kind = error[1]["error"].as_str().unwrap_or_default();
        let message = error[1]["message"].as_str().unwrap_or_default();
        return Err(classify_failure(&format!("{kind} {message}")));
    }
    // gallery-dl -j emits [2, metadata] for a post directory. Ignore unrelated
    // URL/queue records and confirm the numeric ID before accepting the metadata.
    let tweet = rows
        .iter()
        .filter(|record| record[0].as_u64() == Some(2))
        .filter_map(|record| record.as_array()?.last())
        .find(|tweet| {
            tweet
                .get("rest_id")
                .or_else(|| tweet.get("id_str"))
                .and_then(Value::as_str)
                == Some(id)
        })
        .ok_or(TwitterDetailsError::Failed)?;
    let legacy = tweet.get("legacy").unwrap_or(tweet);
    let user = &tweet["core"]["user_results"]["result"];
    if user.pointer("/legacy/protected").and_then(Value::as_bool) == Some(true)
        || user.pointer("/privacy/protected").and_then(Value::as_bool) == Some(true)
        || legacy.get("withheld_scope").is_some()
        || tweet.get("withheld_scope").is_some()
    {
        return Err(TwitterDetailsError::AccessDenied);
    }
    let source_media = legacy
        .pointer("/extended_entities/media")
        .and_then(Value::as_array)
        .or_else(|| tweet.get("media_entities2").and_then(Value::as_array))
        .map_or(&[][..], Vec::as_slice);
    let media: Vec<_> = source_media.iter().filter_map(normalized_media).collect();
    let mut parsed = map_syndication(&json!({
        "id_str": id,
        "full_text": legacy["full_text"],
        "text": legacy["text"],
        "truncated": legacy["truncated"],
        "note_tweet": tweet["note_tweet"],
        "mediaDetails": media,
        "created_at": legacy["created_at"],
        "favorite_count": legacy["favorite_count"],
        "conversation_count": legacy["reply_count"],
        "user": {
            "name": user.pointer("/core/name").or_else(|| user.pointer("/legacy/name")),
            "screen_name": user.pointer("/core/screen_name").or_else(|| user.pointer("/legacy/screen_name")),
            "profile_image_url_https": user.pointer("/avatar/image_url").or_else(|| user.pointer("/legacy/profile_image_url_https")),
        },
    }));
    parsed.share = legacy["retweet_count"].as_i64().unwrap_or(0);
    parsed.play = tweet["views"]["count"]
        .as_str()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    if !parsed.has_media() && !source_media.is_empty() {
        return Err(TwitterDetailsError::UnsupportedMedia);
    }
    if !parsed.has_deliverable() {
        return Err(TwitterDetailsError::Failed);
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_raw_gallery_rows_for_only_the_focal_post() {
        let parsed = map_gallery_details(
            "123",
            json!([
                [2, {"rest_id":"124","full_text":"other post"}],
                [2, {
                    "rest_id":"123",
                    "full_text":"preview",
                    "note_tweet":{"note_tweet_results":{"result":{"text":"全文一\n\n第二段"}}},
                    "core":{"user_results":{"result":{"core":{"name":"评测","screen_name":"example"}}}},
                    "extended_entities":{"media":[
                        {"type":"photo","media_url_https":"https://pbs.twimg.com/media/first.jpg",
                         "original_info":{"width":100,"height":200}},
                        {"type":"photo","media_url_https":"https://pbs.twimg.com/media/second.jpg"},
                        {"type":"photo","media_url_https":"https://other.example/media/other.jpg"}
                    ]}
                }],
                [3, "https://other.example/media/unrelated.jpg", {}]
            ]),
        )
        .unwrap();
        assert_eq!(parsed.author, "评测");
        assert_eq!(parsed.post_body, "全文一\n\n第二段");
        assert!(!parsed.post_body_is_preview);
        assert_eq!(parsed.source_post_url, "https://x.com/i/status/123");
        assert_eq!(parsed.images.len(), 2);
        assert_eq!(parsed.cover_size.unwrap().height, 200);
    }

    #[test]
    fn maps_marked_media_returned_to_the_authenticated_account() {
        let parsed = map_gallery_details(
            "123",
            json!([[2, {
                "rest_id":"123",
                "full_text":"fixture",
                "possibly_sensitive":true,
                "extended_entities":{"media":[
                    {"type":"photo","media_url_https":"https://pbs.twimg.com/media/example.jpg",
                     "sensitive_media_warning":{"adult_content":true}},
                    {"type":"photo","media_url_https":"https://pbs.twimg.com/media/unavailable.jpg",
                     "ext_media_availability":{"status":"Unavailable"}}
                ]}
            }]]),
        )
        .unwrap();
        assert_eq!(
            parsed.images,
            ["https://pbs.twimg.com/media/example.jpg?name=large"]
        );
        assert_eq!(parsed.desc, "fixture");

        let video = map_gallery_details(
            "124",
            json!([[2, {
                "rest_id":"124",
                "possibly_sensitive":true,
                "extended_entities":{"media":[{
                    "type":"video",
                    "media_url_https":"https://pbs.twimg.com/media/cover.jpg",
                    "sensitive_media_warning":{"adult_content":true},
                    "video_info":{"variants":[{
                        "content_type":"video/mp4",
                        "url":"https://video.twimg.com/fixture.mp4"
                    }]}
                }]}
            }]]),
        )
        .unwrap();
        assert_eq!(video.video, "https://video.twimg.com/fixture.mp4");
    }

    #[test]
    fn classifies_access_denied_and_rate_limit() {
        assert_eq!(
            map_gallery_details(
                "123",
                json!([[-1, {"error":"AuthRequired","message":"NSFW Tweet: sample private detail"}]])
            ).unwrap_err(),
            TwitterDetailsError::AccessDenied
        );
        let error = classify_failure("Rate limit exceeded: sample private detail");
        assert_eq!(error, TwitterDetailsError::RateLimited);
        assert!(!error.to_string().contains("sample private detail"));
    }
}
