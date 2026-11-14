//! Additional first-party extractors. These protocols were checked against
//! upstream yt-dlp and independently inspected public sharing responses.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{Value, json};
use url::Url;

use crate::parser::http::{MOBILE_USER_AGENT, SiteHttp, USER_AGENT};
use crate::types::{ImageSize, ParsedData, VideoQuality};

static HUYA_ID: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/(?:video/)?play/(\d+)(?:\.html)?(?:/|$)").unwrap());
static MEIPAI_ID: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/(?:media/|video/\d+/)(\d+)(?:/|$)").unwrap());
static ATTR: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?is)([\w:-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());
static META: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<meta\b([^>]+)>").unwrap());
static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static DATA_VIDEO: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?is)\bdata-video\s*=\s*["']([^"']+)["']"#).unwrap());
static MEIPAI_HLS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)\bfile\s*:\s*encodeURIComponent\(["']([^"']+)["']\)"#).unwrap()
});
static BYTE_ID: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"/(?:video/|w/|group/|item/|article/|xg/video/|i)?(\d{8,})(?:/|$)").unwrap()
});
static RENDER_DATA: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?is)<script\b[^>]*\bid\s*=\s*["']RENDER_DATA["'][^>]*>(.*?)</script\s*>"#)
        .unwrap()
});

pub async fn parse(http: &SiteHttp, url: &str, platform: &str) -> Result<ParsedData> {
    let mut result = match platform {
        "huya" => parse_huya(http, url).await?,
        "jimeng" => parse_jimeng(http, url).await?,
        "doubao" => parse_doubao(http, url).await?,
        "meipai" => parse_meipai(http, url).await?,
        "pipigx" => parse_pipigx(http, url).await?,
        "toutiao" | "xigua" => parse_byte_article(http, url, platform).await?,
        _ => bail!("该平台尚无专用原站扩展解析器"),
    };
    ensure!(result.has_media(), "{platform} 原站未提供可访问媒体");
    result.media_type = if result.video.is_empty() {
        "image"
    } else {
        "video"
    }
    .into();
    let (referer, user_agent) = match platform {
        "huya" => ("https://www.huya.com/", USER_AGENT),
        "jimeng" => ("https://jimeng.jianying.com/", USER_AGENT),
        "doubao" => ("https://www.doubao.com/", USER_AGENT),
        "meipai" => ("https://www.meipai.com/", MOBILE_USER_AGENT),
        "toutiao" | "xigua" => ("https://m.toutiao.com/", MOBILE_USER_AGENT),
        _ => ("https://h5.pipigx.com/", USER_AGENT),
    };
    result
        .media_headers
        .insert("Referer".into(), referer.into());
    result
        .media_headers
        .insert("User-Agent".into(), user_agent.into());
    Ok(result)
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn text(data: &Value, pointers: &[&str]) -> String {
    pointers
        .iter()
        .find_map(|pointer| data.pointer(pointer).and_then(scalar))
        .unwrap_or_default()
}

fn number(data: &Value, pointers: &[&str]) -> i64 {
    pointers
        .iter()
        .find_map(|pointer| {
            let value = data.pointer(pointer)?;
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|number| number as i64))
                .or_else(|| {
                    value
                        .as_str()
                        .and_then(|text| text.parse::<f64>().ok())
                        .map(|number| number as i64)
                })
        })
        .unwrap_or(0)
}

fn media_url(raw: &str) -> String {
    let raw = html_escape::decode_html_entities(raw);
    let normalized = if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw.into_owned()
    };
    Url::parse(&normalized)
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        .map(|url| url.to_string())
        .unwrap_or_default()
}

fn url_at(data: &Value, pointers: &[&str]) -> String {
    pointers
        .iter()
        .map(|pointer| media_url(&text(data, &[*pointer])))
        .find(|url| !url.is_empty())
        .unwrap_or_default()
}

fn plain_text(raw: &str) -> String {
    html_escape::decode_html_entities(&TAG.replace_all(raw, ""))
        .trim()
        .to_owned()
}

fn duration(raw: &str) -> u64 {
    if raw.contains(':') {
        raw.split(':')
            .try_fold(0u64, |total, part| {
                total.checked_mul(60)?.checked_add(part.parse().ok()?)
            })
            .unwrap_or(0)
    } else {
        raw.parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(0.0) as u64
    }
}

fn requested_id(raw: &str, keys: &[&str]) -> Result<String> {
    let url = Url::parse(raw)?;
    let query = url
        .query_pairs()
        .find(|(key, _)| keys.contains(&key.as_ref()))
        .map(|(_, value)| value.into_owned());
    let id = query
        .or_else(|| {
            url.path_segments()
                .and_then(|mut parts| parts.rfind(|part| !part.is_empty()))
                .map(str::to_owned)
        })
        .context("分享链接缺少作品 ID")?;
    ensure!(
        !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()),
        "作品 ID 格式错误"
    );
    Ok(id)
}

fn json_assignment(html: &str, marker: &str) -> Option<Value> {
    html.match_indices(marker)
        .take(16)
        .find_map(|(position, _)| {
            let rest = html[position + marker.len()..]
                .trim_start()
                .strip_prefix('=')?
                .trim_start();
            serde_json::Deserializer::from_str(rest)
                .into_iter::<Value>()
                .next()?
                .ok()
        })
}

fn metadata(html: &str) -> HashMap<String, String> {
    META.captures_iter(html)
        .filter_map(|capture| {
            let attrs: HashMap<String, String> = ATTR
                .captures_iter(&capture[1])
                .map(|attribute| {
                    (
                        attribute[1].to_ascii_lowercase(),
                        html_escape::decode_html_entities(
                            attribute
                                .get(2)
                                .or_else(|| attribute.get(3))
                                .unwrap()
                                .as_str(),
                        )
                        .into_owned(),
                    )
                })
                .collect();
            let name = attrs
                .get("property")
                .or_else(|| attrs.get("name"))
                .or_else(|| attrs.get("itemprop"))?;
            Some((name.to_ascii_lowercase(), attrs.get("content")?.clone()))
        })
        .collect()
}

async fn parse_huya(http: &SiteHttp, raw: &str) -> Result<ParsedData> {
    let url = Url::parse(raw)?;
    let id = HUYA_ID
        .captures(url.path())
        .context("虎牙仅支持视频录播链接，请提供 /video/play/数字.html")?[1]
        .to_owned();
    let response = http
        .get_json(
            &format!("https://liveapi.huya.com/moment/getMomentContent?videoId={id}"),
            "huya",
        )
        .await?;
    map_huya(&response, &id)
}

fn map_huya(response: &Value, id: &str) -> Result<ParsedData> {
    let moment = response
        .pointer("/data/moment")
        .context("虎牙视频不存在或权限受限")?;
    ensure!(
        text(moment, &["/videoInfo/vid"]) == id,
        "虎牙返回的作品 ID 与请求不一致"
    );
    let mut result = ParsedData {
        title: text(moment, &["/videoInfo/videoTitle", "/title"]),
        desc: plain_text(&text(moment, &["/content"])),
        author: text(moment, &["/videoInfo/nickName", "/nickName"]),
        uid: text(moment, &["/videoInfo/uid", "/uid"]),
        avatar: url_at(moment, &["/videoInfo/avatarUrl", "/iconUrl"]),
        cover: url_at(
            moment,
            &["/videoInfo/videoBigCover", "/videoInfo/videoCover"],
        ),
        duration: duration(&text(moment, &["/videoInfo/videoDuration"])),
        publish_time: number(moment, &["/cTime"]).saturating_mul(1000),
        like: number(moment, &["/favorCount"]),
        comment: number(moment, &["/commentCount"]),
        share: number(moment, &["/shareCount"]),
        play: number(moment, &["/videoInfo/videoPlayNum"]),
        ..ParsedData::default()
    };
    let definitions = moment
        .pointer("/videoInfo/definitions")
        .and_then(Value::as_array)
        .context("虎牙未返回播放清晰度")?;
    let mut choices = definitions
        .iter()
        .filter_map(|definition| {
            let direct = url_at(definition, &["/url"]);
            let (address, is_direct) = if !direct.is_empty() {
                (direct, true)
            } else {
                (url_at(definition, &["/m3u8"]), false)
            };
            if address.is_empty() {
                return None;
            }
            let mut url = Url::parse(&address).ok()?;
            if url.scheme() == "http"
                && url
                    .host_str()
                    .is_some_and(|host| host.ends_with(".huya.com"))
            {
                let _ = url.set_scheme("https");
            }
            let bitrate = url
                .query_pairs()
                .find(|(key, _)| key == "bitrate")
                .and_then(|(_, value)| value.parse::<u64>().ok())
                .map(|rate| rate.saturating_mul(1000));
            let quality = VideoQuality {
                quality: text(definition, &["/defName", "/definition"]),
                url: url.to_string(),
                bit_rate: bitrate,
            };
            Some((
                (
                    is_direct,
                    number(definition, &["/height"]),
                    bitrate.unwrap_or(0),
                ),
                quality,
            ))
        })
        .collect::<Vec<_>>();
    choices.sort_by_key(|(rank, _)| std::cmp::Reverse(*rank));
    result.videos = choices.into_iter().map(|(_, quality)| quality).collect();
    result.video = result
        .videos
        .first()
        .context("虎牙没有可访问的播放流")?
        .url
        .clone();
    Ok(result)
}

async fn parse_jimeng(http: &SiteHttp, raw: &str) -> Result<ParsedData> {
    let mut landing = raw.to_owned();
    if Url::parse(raw)?.path().starts_with("/s/") {
        landing = http.resolve_url(raw, "jimeng").await?;
    }
    let url = Url::parse(&landing)?;
    ensure!(
        !url.query_pairs()
            .any(|(key, value)| (key == "share_campaign_key"
                && value == "dreamina_invite_fission")
                || (key == "item_type" && value == "local")),
        "即梦邀请或本地草稿链接未包含已发布作品"
    );
    let id = requested_id(&landing, &["id", "item_id", "published_item_id"])?;
    let response = http.post_json("https://jimeng.jianying.com/mweb/v1/get_item_info?aid=513695&device_platform=web&region=CN", "jimeng",
        &json!({"published_item_id":id,"pack_item_opt":{},"item_not_find_detail":true})).await?;
    map_jimeng(&response, &id)
}

fn map_jimeng(response: &Value, id: &str) -> Result<ParsedData> {
    ensure!(
        text(response, &["/ret"]) == "0",
        "即梦作品接口返回失败，作品可能已删除或需要登录"
    );
    let data = response.get("data").context("即梦返回缺少作品详情")?;
    ensure!(
        ["/common_attr/id", "/common_attr/published_item_id"]
            .iter()
            .any(|pointer| text(data, &[*pointer]) == id),
        "即梦返回的作品 ID 与请求不一致"
    );
    let mut result = ParsedData {
        title: text(data, &["/common_attr/title", "/aigc_image_params/prompt"]),
        desc: text(data, &["/common_attr/description"]),
        author: text(data, &["/author/nickname", "/author/name"]),
        uid: text(data, &["/author/uid", "/author/user_id"]),
        avatar: url_at(data, &["/author/avatar_url", "/author/avatar"]),
        cover: url_at(data, &["/common_attr/cover_url"]),
        duration: number(data, &["/video/duration"]).max(0) as u64,
        publish_time: number(data, &["/common_attr/create_time"]).saturating_mul(1000),
        like: number(data, &["/statistic/digg_count", "/statistic/like_count"]),
        share: number(data, &["/statistic/share_count"]),
        collect: number(data, &["/statistic/collect_count"]),
        ..ParsedData::default()
    };
    for quality in ["origin", "1080p", "720p", "480p", "360p"] {
        let address = url_at(
            data,
            &[&format!("/video/transcoded_video/{quality}/video_url")],
        );
        if !address.is_empty() {
            result.videos.push(VideoQuality {
                quality: quality.into(),
                url: address,
                bit_rate: None,
            });
        }
    }
    result.video = result
        .videos
        .first()
        .map(|quality| quality.url.clone())
        .unwrap_or_else(|| url_at(data, &["/video/origin_video/video_url"]));
    if let Some(images) = data
        .pointer("/image/large_images")
        .and_then(Value::as_array)
    {
        let mut sizes = HashMap::new();
        for image in images {
            let address = url_at(image, &["/image_url"]);
            if !address.is_empty() {
                let width = number(image, &["/width"]).max(0) as u32;
                let height = number(image, &["/height"]).max(0) as u32;
                if width > 0 && height > 0 {
                    sizes.insert(address.clone(), ImageSize { width, height });
                }
                result.images.push(address);
            }
        }
        if !sizes.is_empty() {
            result.image_size_map = Some(sizes);
        }
    }
    ensure!(result.has_media(), "即梦详情未包含视频或原图");
    if result.title.is_empty() {
        result.title = "即梦作品".into();
    }
    Ok(result)
}

async fn parse_doubao(http: &SiteHttp, raw: &str) -> Result<ParsedData> {
    ensure!(
        Url::parse(raw)?.path().trim_end_matches('/') == "/video-sharing",
        "豆包专用接口当前支持 /video-sharing 分享链接"
    );
    let query: HashMap<String, String> = Url::parse(raw)?
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let share_id = query
        .get("share_id")
        .filter(|id| !id.is_empty())
        .context("豆包分享链接缺少 share_id")?;
    let video_id = query
        .get("video_id")
        .filter(|id| !id.is_empty())
        .context("豆包分享链接缺少 video_id")?;
    let response = http.post_json("https://www.doubao.com/creativity/share/get_video_share_info?version_code=20800&language=zh-CN&device_platform=web&aid=497858&real_aid=497858&pkg_type=release_version&samantha_web=1&use-olympus-account=1", "doubao",
        &json!({"share_id":share_id,"vid":video_id,"creation_id":""})).await?;
    map_doubao(&response, video_id)
}

fn map_doubao(response: &Value, video_id: &str) -> Result<ParsedData> {
    ensure!(
        text(response, &["/code"]) == "0",
        "豆包分享接口返回失败，链接可能已失效或需要登录"
    );
    let data = response.get("data").context("豆包返回缺少分享详情")?;
    let actual_id = text(data, &["/vid", "/video_id", "/play_info/vid"]);
    ensure!(
        actual_id.is_empty() || actual_id == video_id,
        "豆包返回的作品 ID 与请求不一致"
    );
    let video = url_at(data, &["/play_info/main", "/play_info/backup"]);
    ensure!(!video.is_empty(), "豆包未提供可访问的视频地址");
    Ok(ParsedData {
        title: text(data, &["/prompt"]),
        video,
        cover: url_at(data, &["/play_info/poster_url", "/play_info/cover_url"]),
        author: text(data, &["/user_info/nickname", "/user_info/user_name"]),
        uid: text(data, &["/user_info/user_id"]),
        avatar: url_at(data, &["/user_info/avatar_url", "/user_info/avatar"]),
        duration: number(data, &["/play_info/duration"]).max(0) as u64,
        ..ParsedData::default()
    })
}

async fn parse_meipai(http: &SiteHttp, raw: &str) -> Result<ParsedData> {
    let url = Url::parse(raw)?;
    let id = MEIPAI_ID
        .captures(url.path())
        .context("美拍链接缺少作品 ID")?[1]
        .to_owned();
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(MOBILE_USER_AGENT),
    );
    let html = http
        .get_text_headers(
            &format!("https://www.meipai.com/media/{id}"),
            "meipai",
            headers,
        )
        .await?;
    map_meipai_page(&html, &id)
}

fn map_meipai_page(html: &str, id: &str) -> Result<ParsedData> {
    let mut result = ParsedData::default();
    if let Some(data) = json_assignment(html, "window.PHPDATA") {
        let media = data.get("mediaInfo").context("美拍页面没有作品详情")?;
        ensure!(
            text(media, &["/id"]) == id,
            "美拍返回的作品 ID 与请求不一致"
        );
        result.title = plain_text(&text(media, &["/caption_origin", "/caption"]));
        result.cover = url_at(media, &["/cover_pic"]);
        result.author = text(media, &["/user/screen_name"]);
        result.uid = text(media, &["/user/id"]);
        result.avatar = url_at(media, &["/user/avatar"]);
        result.duration = number(media, &["/duration"]).max(0) as u64;
        result.like = number(media, &["/likes_count"]);
        result.comment = number(media, &["/comments_count"]);
        result.play = number(media, &["/plays_count"]);
        result.video = decode_meipai(&text(media, &["/video"])).unwrap_or_default();
    }
    if result.video.is_empty()
        && let Some(capture) = DATA_VIDEO.captures(html)
    {
        result.video = decode_meipai(&capture[1]).unwrap_or_default();
    }
    if result.video.is_empty()
        && let Some(capture) = MEIPAI_HLS.captures(html)
    {
        result.video = media_url(&capture[1]);
    }
    ensure!(!result.video.is_empty(), "美拍页面未包含可访问的视频");
    let tags = metadata(html);
    if result.title.is_empty() {
        result.title = tags.get("og:title").cloned().unwrap_or_default();
    }
    if result.cover.is_empty() {
        result.cover = tags
            .get("og:image")
            .map(|raw| media_url(raw))
            .unwrap_or_default();
    }
    result.desc = tags.get("og:description").cloned().unwrap_or_default();
    if result.duration == 0 {
        result.duration = tags.get("duration").map(|raw| duration(raw)).unwrap_or(0);
    }
    Ok(result)
}

fn decode_meipai(encoded: &str) -> Option<String> {
    let direct = media_url(encoded);
    if !direct.is_empty() {
        return Some(direct);
    }
    if !encoded.is_ascii() || encoded.len() < 8 {
        return None;
    }
    let prefix = encoded[..4].chars().rev().collect::<String>();
    let positions = u16::from_str_radix(&prefix, 16).ok()?.to_string();
    if positions.len() != 4 {
        return None;
    }
    let digits = positions
        .bytes()
        .map(|byte| usize::from(byte - b'0'))
        .collect::<Vec<_>>();
    let mut body = encoded[4..].to_owned();
    let first_end = digits[0].checked_add(digits[1])?;
    if first_end > body.len() {
        return None;
    }
    body.replace_range(digits[0]..first_end, "");
    let tail_end = body.len().checked_sub(digits[2])?;
    let tail_start = tail_end.checked_sub(digits[3])?;
    body.replace_range(tail_start..tail_end, "");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(body)
        .ok()?;
    let address = media_url(std::str::from_utf8(&decoded).ok()?);
    (!address.is_empty()).then_some(address)
}

async fn parse_pipigx(http: &SiteHttp, raw: &str) -> Result<ParsedData> {
    let id = requested_id(raw, &["pid"])?;
    let pid = id.parse::<u64>().context("皮皮搞笑作品 ID 超出范围")?;
    let mid = Url::parse(raw)?
        .query_pairs()
        .find(|(key, _)| key == "mid")
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .unwrap_or(0);
    let response = http
        .post_json(
            "https://h5.pipigx.com/ppapi/share/fetch_content",
            "pipigx",
            &json!({"pid":pid,"mid":mid,"type":"post"}),
        )
        .await?;
    map_pipigx(&response, &id)
}

fn map_pipigx(response: &Value, id: &str) -> Result<ParsedData> {
    let post = response
        .pointer("/data/post")
        .context("皮皮搞笑帖子不存在或需要登录")?;
    ensure!(
        text(post, &["/id"]) == id,
        "皮皮搞笑返回的帖子 ID 与请求不一致"
    );
    let mut result = ParsedData {
        title: text(post, &["/content"]),
        author: text(post, &["/space_info/name", "/space_info/nickname"]),
        uid: text(post, &["/mid"]),
        like: number(post, &["/up", "/likes"]),
        comment: number(post, &["/reviews"]),
        share: number(post, &["/share"]),
        publish_time: number(post, &["/ct"]).saturating_mul(1000),
        ..ParsedData::default()
    };
    if let Some(videos) = post.get("videos").and_then(Value::as_object) {
        for (index, video) in videos.values().enumerate() {
            let address = url_at(video, &["/url", "/urlext", "/urlsrc"]);
            if !address.is_empty() {
                if result.cover.is_empty() {
                    result.cover = url_at(video, &["/cover_urls/0"]);
                }
                if result.duration == 0 {
                    result.duration = number(video, &["/dur"]).max(0) as u64;
                }
                result.videos.push(VideoQuality {
                    quality: format!("视频{}", index + 1),
                    url: address,
                    bit_rate: None,
                });
            }
        }
    }
    result.video = result
        .videos
        .first()
        .map(|quality| quality.url.clone())
        .unwrap_or_default();
    if result.video.is_empty()
        && let Some(images) = post.get("imgs").and_then(Value::as_array)
    {
        result.images = images
            .iter()
            .map(|image| {
                url_at(
                    image,
                    &[
                        "/urls/origin/urls/0",
                        "/urls/origin_webp/urls/0",
                        "/urls/large/urls/0",
                        "/urls/540/urls/0",
                        "/url",
                    ],
                )
            })
            .filter(|url| !url.is_empty())
            .collect();
    }
    ensure!(result.has_media(), "皮皮搞笑帖子没有可访问的视频或图片");
    Ok(result)
}

async fn parse_byte_article(http: &SiteHttp, raw: &str, platform: &str) -> Result<ParsedData> {
    let url = Url::parse(raw)?;
    let id = BYTE_ID
        .captures(url.path())
        .context("头条或西瓜链接缺少作品 ID")?[1]
        .to_owned();
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(MOBILE_USER_AGENT),
    );
    let html = http
        .get_text_headers(
            &format!("https://m.toutiao.com/video/{id}/"),
            platform,
            headers.clone(),
        )
        .await?;
    let article = byte_article_from_page(&html, &id)?;
    let token = text(&article, &["/playAuthTokenV2"]);
    if token.is_empty() {
        return map_byte_article(&article, &Value::Null);
    }
    let video_id = text(&article, &["/videoId"]);
    let endpoint = byte_vod_endpoint(&token, &video_id)?;
    let vod = http.get_json_headers(&endpoint, platform, headers).await?;
    map_byte_article(&article, &vod)
}

fn byte_article_from_page(html: &str, id: &str) -> Result<Value> {
    let raw = RENDER_DATA
        .captures(html)
        .context("头条移动页面返回访问验证或空页面，未提供作品渲染数据")?[1]
        .trim()
        .to_owned();
    let decoded = if raw.starts_with('%') {
        urlencoding::decode(&raw)?.into_owned()
    } else {
        raw
    };
    let state: Value = serde_json::from_str(&decoded).context("头条移动页面作品数据格式错误")?;
    let article = state
        .get("articleInfo")
        .context("头条移动页面未包含作品详情")?;
    ensure!(
        text(article, &["/gid", "/groupId", "/itemId"]) == id,
        "头条返回的作品 ID 与请求不一致"
    );
    Ok(article.clone())
}

fn byte_vod_endpoint(token: &str, video_id: &str) -> Result<String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(token)
        .context("头条播放授权数据格式错误")?;
    let data: Value = serde_json::from_slice(&bytes)?;
    let query = text(&data, &["/GetPlayInfoToken"]);
    ensure!(!query.is_empty(), "头条播放授权缺少签名查询参数");
    let mut endpoint = Url::parse("https://vod.bytedanceapi.com/")?;
    // Reordering or re-encoding these query pairs invalidates the upstream signature.
    endpoint.set_query(Some(&query));
    ensure!(
        endpoint
            .query_pairs()
            .any(|(key, value)| key == "Action" && value == "GetPlayInfo"),
        "头条播放授权操作不受支持"
    );
    ensure!(
        !video_id.is_empty()
            && endpoint
                .query_pairs()
                .any(|(key, value)| key == "video_id" && value == video_id),
        "头条播放授权与作品视频 ID 不一致"
    );
    Ok(endpoint.to_string())
}

fn map_byte_article(article: &Value, response: &Value) -> Result<ParsedData> {
    let vod = response.pointer("/Result/Data").unwrap_or(&Value::Null);
    let expected_video = text(article, &["/videoId"]);
    let actual_video = text(vod, &["/VideoID"]);
    ensure!(
        actual_video.is_empty() || actual_video == expected_video,
        "头条播放接口返回的作品视频 ID 不一致"
    );
    let mut result = ParsedData {
        title: text(article, &["/title"]),
        desc: plain_text(&text(article, &["/content"])),
        author: text(article, &["/mediaUser/screenName", "/userName", "/source"]),
        uid: text(article, &["/creatorUid", "/mediaId"]),
        avatar: url_at(article, &["/mediaUser/avatarUrl"]),
        cover: url_at(article, &["/posterUrl"]),
        duration: number(vod, &["/Duration"]).max(0) as u64,
        publish_time: number(article, &["/publishTime"]).saturating_mul(1000),
        like: number(article, &["/diggCount"]),
        comment: number(article, &["/commentCount"]),
        share: number(article, &["/repostCount"]),
        collect: number(article, &["/repinCount"]),
        play: number(article, &["/videoPlayCount"]),
        ..ParsedData::default()
    };
    if result.duration == 0 {
        result.duration = number(article, &["/videoDuration"]).max(0) as u64;
    }
    if result.cover.is_empty() {
        result.cover = url_at(vod, &["/CoverUrl"]);
    }
    if let Some(streams) = vod.get("PlayInfoList").and_then(Value::as_array) {
        let compatible = streams
            .iter()
            .filter(|stream| {
                let codec = text(stream, &["/Codec"]);
                codec.is_empty() || codec.eq_ignore_ascii_case("h264")
            })
            .collect::<Vec<_>>();
        let candidates = if compatible.is_empty() {
            streams.iter().collect()
        } else {
            compatible
        };
        for stream in candidates {
            let address = url_at(stream, &["/MainPlayUrl", "/BackupPlayUrl"]);
            if !address.is_empty() {
                result.videos.push(VideoQuality {
                    quality: text(stream, &["/Definition"]),
                    url: address,
                    bit_rate: Some(
                        (number(stream, &["/Bitrate"]).max(0) as u64).saturating_mul(1000),
                    ),
                });
            }
        }
    }
    result
        .videos
        .sort_by_key(|stream| std::cmp::Reverse(stream.bit_rate.unwrap_or(0)));
    result.video = result
        .videos
        .first()
        .map(|stream| stream.url.clone())
        .unwrap_or_else(|| {
            url_at(
                article,
                &["/playUrlList/0/mainUrl", "/playUrlList/0/backupUrl"],
            )
        });
    ensure!(!result.video.is_empty(), "头条或西瓜原站没有可访问的视频流");
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huya_prefers_direct_mp4_and_preserves_video_identity() {
        let data = json!({"data":{"moment":{"videoInfo":{"vid":123,"videoDuration":"1:02:03","definitions":[
            {"height":1080,"m3u8":"https://cdn.huya.com/high.m3u8"},{"height":720,"url":"http://cdn.huya.com/clip.mp4?bitrate=100"}]}}}});
        let parsed = map_huya(&data, "123").unwrap();
        assert_eq!(parsed.video, "https://cdn.huya.com/clip.mp4?bitrate=100");
        assert_eq!(parsed.duration, 3723);
        assert!(map_huya(&data, "999").is_err());
    }

    #[test]
    fn jimeng_selects_original_and_accepts_image_posts() {
        let response = json!({"ret":"0","data":{"common_attr":{"id":"123"},"video":{"transcoded_video":{
            "720p":{"video_url":"https://cdn.example.org/720.mp4"},"origin":{"video_url":"https://cdn.example.org/original.mp4?a=%2B"}}}}});
        assert_eq!(
            map_jimeng(&response, "123").unwrap().video,
            "https://cdn.example.org/original.mp4?a=%2B"
        );
        assert!(map_jimeng(&response, "456").is_err());
        let images = json!({"ret":0,"data":{"common_attr":{"id":"123"},"image":{"large_images":[{"image_url":"https://cdn.example.org/image.png","width":1440,"height":2560}]}}});
        let parsed = map_jimeng(&images, "123").unwrap();
        assert_eq!(parsed.images.len(), 1);
        assert_eq!(
            parsed
                .image_size_map
                .unwrap()
                .values()
                .next()
                .unwrap()
                .width,
            1440
        );
    }

    #[test]
    fn doubao_reads_only_share_play_info_and_validates_echoed_id() {
        let response = json!({"code":0,"data":{"vid":"video1","prompt":"title","play_info":{"main":"https://cdn.doubao.com/video?a=1%2B2"}}});
        assert_eq!(
            map_doubao(&response, "video1").unwrap().video,
            "https://cdn.doubao.com/video?a=1%2B2"
        );
        assert!(map_doubao(&response, "video2").is_err());
        assert!(map_doubao(&json!({"code":1001}), "video1").is_err());
    }

    #[test]
    fn meipai_decodes_url_and_rejects_malformed_positions() {
        let raw = "https://cdn.example.org/video.mp4";
        let mut encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        encoded.insert_str(encoded.len() - 1, "zz");
        encoded.insert_str(1, "xy");
        assert_eq!(decode_meipai(&format!("cb40{encoded}")).unwrap(), raw);
        assert!(decode_meipai("zzzzbad-token").is_none());
        assert!(decode_meipai("你好吗").is_none());
    }

    #[test]
    fn meipai_phpdata_rejects_unrelated_video() {
        let html = format!(
            "<script>window.PHPDATA={};</script>",
            json!({"mediaInfo":{"id":"123","video":"https://cdn.example.org/video.mp4","caption":"a <b>title</b>"}})
        );
        assert_eq!(map_meipai_page(&html, "123").unwrap().title, "a title");
        assert!(map_meipai_page(&html, "456").is_err());
    }

    #[test]
    fn pipigx_uses_returned_cdn_cover_and_post_id() {
        let response = json!({"ret":1,"data":{"post":{"id":123,"content":"title","videos":{"456":{"url":"https://cdn.example.org/video","dur":39,"cover_urls":["https://cdn.example.org/cover.jpg"]}},"imgs":[{"url":"https://cdn.example.org/cover.jpg"}]}}});
        let parsed = map_pipigx(&response, "123").unwrap();
        assert_eq!(parsed.duration, 39);
        assert!(parsed.images.is_empty());
        assert!(map_pipigx(&response, "999").is_err());
    }

    #[test]
    fn toutiao_preserves_authorization_query_bytes() {
        let query = "Action=GetPlayInfo&video_id=video123&X-Amz-Signature=x%2By%2Fz&keep=1%20a";
        let token = base64::engine::general_purpose::STANDARD
            .encode(json!({"GetPlayInfoToken":query}).to_string());
        assert_eq!(
            byte_vod_endpoint(&token, "video123").unwrap(),
            format!("https://vod.bytedanceapi.com/?{query}")
        );
        assert!(byte_vod_endpoint(&token, "other").is_err());
    }

    #[test]
    fn toutiao_uses_exact_article_and_video_ids() {
        let state = json!({"articleInfo":{"gid":"12345678","videoId":"video1","title":"title","videoDuration":188}});
        let html = format!(
            "<script type='application/json' id='RENDER_DATA'>{}</script>",
            urlencoding::encode(&state.to_string())
        );
        let article = byte_article_from_page(&html, "12345678").unwrap();
        assert!(byte_article_from_page(&html, "99999999").is_err());
        let response = json!({"Result":{"Data":{"VideoID":"video1","PlayInfoList":[{"Definition":"720p","Bitrate":800,"MainPlayUrl":"https://cdn.example.org/video.mp4"}]}}});
        assert_eq!(map_byte_article(&article, &response).unwrap().duration, 188);
        assert!(
            map_byte_article(&article, &json!({"Result":{"Data":{"VideoID":"other"}}})).is_err()
        );
    }
}
