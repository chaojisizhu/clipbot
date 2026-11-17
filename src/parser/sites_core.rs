//! First-party metadata and media-stream extraction.
//!
//! Platform protocol details were checked against the upstream yt-dlp extractors.
//! DASH audio/video pairs and HLS streams are assembled by the media relay.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, REFERER};
use serde_json::Value;
use url::Url;

use crate::parser::http::SiteHttp;
use crate::types::{ImageSize, ParsedData, VideoQuality};

static BILI_ID: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/(BV[0-9A-Za-z]+|av[0-9]+|ep[0-9]+|ss[0-9]+)(?:/|$)").unwrap());
static HTML_TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static WBI_KEY: Lazy<tokio::sync::Mutex<Option<(Instant, String)>>> =
    Lazy::new(|| tokio::sync::Mutex::new(None));

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BilibiliMediaUnavailable(pub String);

impl std::fmt::Display for BilibiliMediaUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BilibiliMediaUnavailable {}

pub async fn parse(http: &SiteHttp, url: &str, platform: &str) -> Result<ParsedData> {
    let mut parsed = match platform {
        "bilibili" => parse_bilibili(http, url).await?,
        "weibo" => parse_weibo(http, url).await?,
        "acfun" => parse_acfun(http, url).await?,
        _ => bail!("该平台没有原生核心解析器"),
    };
    let referer = match platform {
        "bilibili" => "https://www.bilibili.com/",
        "weibo" => "https://weibo.com/",
        _ => "https://www.acfun.cn/",
    };
    parsed
        .media_headers
        .insert("Referer".into(), referer.into());
    Ok(parsed)
}

fn headers(referer: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(REFERER, HeaderValue::from_str(referer)?);
    Ok(headers)
}

fn endpoint(base: &str, pairs: &[(&str, String)]) -> Result<String> {
    let mut url = Url::parse(base)?;
    url.query_pairs_mut()
        .extend_pairs(pairs.iter().map(|(key, value)| (*key, value)));
    Ok(url.into())
}

fn text(value: &Value, pointer: &str) -> String {
    match value.pointer(pointer) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

fn number(value: &Value, pointer: &str) -> i64 {
    let Some(value) = value.pointer(pointer) else {
        return 0;
    };
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or(0)
}

fn duration(value: &Value, pointer: &str) -> u64 {
    let Some(value) = value.pointer(pointer) else {
        return 0;
    };
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        .unwrap_or(0.0)
        .round()
        .max(0.0) as u64
}

fn media_url(raw: &str) -> Option<String> {
    let raw = raw.replace("&amp;", "&");
    let normalized = if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw
    };
    let url = Url::parse(&normalized).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(normalized)
}

fn progressive_url(raw: &str) -> Option<String> {
    let url = media_url(raw)?;
    let path = Url::parse(&url).ok()?.path().to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".mpd") || path.ends_with(".m4s") {
        return None;
    }
    Some(url)
}

fn checked_bili_payload(response: &Value) -> Result<&Value> {
    ensure!(
        response.pointer("/data/v_voucher").is_none()
            && response.pointer("/result/v_voucher").is_none(),
        "B站原站要求风控验证，请在浏览器完成验证并补充 bilibili Cookie"
    );
    let code = number(response, "/code");
    if code == -404 {
        return Err(
            BilibiliMediaUnavailable("B站原站返回 -404，视频不存在或已被删除".into()).into(),
        );
    }
    if code == -10403 {
        return Err(BilibiliMediaUnavailable("B站原站返回 -10403，受地区或版权限制".into()).into());
    }
    ensure!(
        code == 0,
        "B站原站返回错误 {code}，{}。需要权限时请补充 bilibili Cookie",
        text(response, "/message")
    );
    let data = response
        .get("data")
        .or_else(|| response.get("result"))
        .context("B站原站响应缺少数据")?;
    let playback = data.get("video_info").unwrap_or(data);
    ensure!(
        !bili_is_preview(data) && !bili_is_preview(playback),
        "B站当前只返回试看内容，请补充有观看权限的Cookie"
    );
    Ok(playback)
}

fn bili_is_preview(playback: &Value) -> bool {
    playback.get("is_preview").and_then(Value::as_bool) == Some(true)
        || number(playback, "/is_preview") == 1
        || text(playback, "/play_video_type") == "preview"
        || text(playback, "/play_check/play_detail") == "PLAY_PREVIEW"
}

fn bili_page(url: &Url) -> Result<usize> {
    let part = url
        .query_pairs()
        .find(|(key, _)| key == "p")
        .map(|(_, value)| value.into_owned());
    match part {
        Some(part) => {
            let index: usize = part.parse().context("B站分P参数无效")?;
            ensure!(index > 0, "B站分P参数必须大于0");
            Ok(index)
        }
        None => Ok(1),
    }
}

async fn parse_bilibili(http: &SiteHttp, input: &str) -> Result<ParsedData> {
    let url = Url::parse(input)?;
    let id = BILI_ID
        .captures(url.path())
        .and_then(|capture| capture.get(1))
        .context("B站链接缺少BV、av、ep或ss标识")?
        .as_str();
    if id.starts_with("ep") || id.starts_with("ss") {
        return parse_bangumi(http, id).await;
    }
    let part = bili_page(&url)?;
    let identity = if let Some(aid) = id.strip_prefix("av") {
        ("aid", aid.to_owned())
    } else {
        ("bvid", id.to_owned())
    };
    let view_url = endpoint("https://api.bilibili.com/x/web-interface/view", &[identity])?;
    let response = http
        .get_json_headers(&view_url, "bilibili", headers("https://www.bilibili.com/")?)
        .await?;
    let data = checked_bili_payload(&response)?;
    let redirect = text(data, "/redirect_url");
    if !redirect.is_empty()
        && let Ok(redirect) = Url::parse(&redirect)
        && let Some(capture) = BILI_ID.captures(redirect.path())
        && let Some(episode) = capture.get(1).filter(|id| id.as_str().starts_with("ep"))
    {
        return parse_bangumi(http, episode.as_str()).await;
    }
    let pages = data
        .get("pages")
        .and_then(Value::as_array)
        .context("B站视频缺少分P列表")?;
    let page = pages.get(part - 1).context("请求的B站分P不存在")?;
    let cid = text(page, "/cid");
    ensure!(!cid.is_empty(), "B站分P缺少cid");
    let mut params = [
        ("avid", text(data, "/aid")),
        ("bvid", text(data, "/bvid")),
        ("cid", cid),
        ("qn", "64".into()),
        ("fnval", "1".into()),
        ("fnver", "0".into()),
        ("fourk", "0".into()),
        ("platform", "html5".into()),
        ("high_quality", "1".into()),
    ];
    let play_url = endpoint("https://api.bilibili.com/x/player/playurl", &params)?;
    let playback = http
        .get_json_headers(&play_url, "bilibili", headers("https://www.bilibili.com/")?)
        .await?;
    let mut playback = if matches!(number(&playback, "/code"), -352 | -403) {
        signed_bili_play(http, &params).await?
    } else {
        playback
    };
    let payload = checked_bili_payload(&playback)?;
    if payload.get("dash").is_none()
        && !payload
            .get("durl")
            .and_then(Value::as_array)
            .is_some_and(|segments| !segments.is_empty())
    {
        for (key, value) in &mut params {
            if *key == "fnval" {
                *value = "16".into();
            }
        }
        let dash_url = endpoint("https://api.bilibili.com/x/player/playurl", &params)?;
        playback = http
            .get_json_headers(&dash_url, "bilibili", headers("https://www.bilibili.com/")?)
            .await?;
        if matches!(number(&playback, "/code"), -352 | -403) {
            playback = signed_bili_play(http, &params).await?;
        }
    }
    map_bilibili(
        data,
        page,
        checked_bili_payload(&playback)?,
        part,
        pages.len(),
    )
}

async fn signed_bili_play(http: &SiteHttp, params: &[(&str, String)]) -> Result<Value> {
    let key = bili_wbi_key(http).await?;
    let mut values: BTreeMap<String, String> = params
        .iter()
        .map(|(key, value)| {
            (
                key.to_string(),
                value.replace(['!', '\'', '(', ')', '*'], ""),
            )
        })
        .collect();
    values.insert("wts".into(), chrono::Utc::now().timestamp().to_string());
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(values)
        .finish()
        .replace('+', "%20");
    let signature = format!("{:x}", md5::compute(format!("{query}{key}")));
    http.get_json_headers(
        &format!("https://api.bilibili.com/x/player/wbi/playurl?{query}&w_rid={signature}"),
        "bilibili",
        headers("https://www.bilibili.com/")?,
    )
    .await
}

async fn bili_wbi_key(http: &SiteHttp) -> Result<String> {
    let mut cache = WBI_KEY.lock().await;
    if let Some((created, key)) = &*cache
        && created.elapsed() < Duration::from_secs(30)
    {
        return Ok(key.clone());
    }
    let nav = http
        .get_json_headers(
            "https://api.bilibili.com/x/web-interface/nav",
            "bilibili",
            headers("https://www.bilibili.com/")?,
        )
        .await?;
    let image = text(&nav, "/data/wbi_img/img_url");
    let sub = text(&nav, "/data/wbi_img/sub_url");
    let source: String = [&image, &sub]
        .into_iter()
        .filter_map(|url| url.rsplit('/').next())
        .filter_map(|name| name.split('.').next())
        .collect();
    ensure!(
        source.is_ascii() && source.len() >= 64,
        "B站WBI签名材料不可用，请补充登录Cookie后重试"
    );
    // This permutation is part of Bilibili's public web-client WBI protocol.
    const MIXIN: [usize; 32] = [
        46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19,
        29, 28, 14, 39, 12, 38, 41, 13,
    ];
    let key: String = MIXIN
        .into_iter()
        .map(|index| source.as_bytes()[index] as char)
        .collect();
    *cache = Some((Instant::now(), key.clone()));
    Ok(key)
}

fn bili_progressive(playback: &Value) -> Result<Vec<VideoQuality>> {
    ensure!(
        !bili_is_preview(playback),
        "B站当前只返回试看内容，请补充有观看权限的Cookie"
    );
    let segments = playback
        .get("durl")
        .and_then(Value::as_array)
        .context("B站当前只提供DASH分离音画流，需要合流后才能发送")?;
    ensure!(
        segments.len() == 1,
        "B站当前返回{}段视频，需要拼接后才能完整发送",
        segments.len()
    );
    let segment = &segments[0];
    let url = progressive_url(&text(segment, "/url")).context("B站未提供可直接发送的完整媒体流")?;
    let format = text(playback, "/format").to_ascii_lowercase();
    let path = Url::parse(&url)?.path().to_ascii_lowercase();
    ensure!(
        format.contains("mp4") || path.ends_with(".mp4"),
        "B站当前返回{format}封装，需要转换为MP4后才能发送"
    );
    Ok(vec![VideoQuality {
        quality: text(playback, "/quality"),
        url,
        bit_rate: None,
    }])
}

fn dash_url(stream: &Value) -> Option<String> {
    ["/baseUrl", "/base_url", "/url"]
        .into_iter()
        .find_map(|pointer| media_url(&text(stream, pointer)))
}

fn bili_streams(playback: &Value) -> Result<(Vec<VideoQuality>, Option<String>)> {
    ensure!(
        !bili_is_preview(playback),
        "B站当前只返回试看内容，请补充有观看权限的Cookie"
    );
    if playback
        .get("durl")
        .and_then(Value::as_array)
        .is_some_and(|segments| !segments.is_empty())
    {
        return Ok((bili_progressive(playback)?, None));
    }
    let dash = playback.get("dash").context("B站未返回可播放的媒体流")?;
    let h264: Vec<&Value> = dash
        .get("video")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|stream| {
            let codec = text(stream, "/codecs").to_ascii_lowercase();
            (number(stream, "/codecid") == 7 || codec.starts_with("avc1") || codec.contains("h264"))
                && dash_url(stream).is_some()
        })
        .collect();
    // Keep the normal 720p target while allowing streams that only offer a larger size.
    let preferred = h264
        .iter()
        .copied()
        .filter(|stream| number(stream, "/height") <= 720)
        .max_by_key(|stream| (number(stream, "/height"), number(stream, "/bandwidth")));
    let video = preferred
        .or_else(|| {
            h264.iter()
                .copied()
                .min_by_key(|stream| number(stream, "/height"))
        })
        .context("B站DASH未提供可用H.264视频流")?;
    let audio = dash
        .get("audio")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|stream| {
            let codec = text(stream, "/codecs").to_ascii_lowercase();
            let known_aac =
                codec.is_empty() && matches!(number(stream, "/id"), 30216 | 30232 | 30280);
            (codec.starts_with("mp4a") || codec.contains("aac") || known_aac)
                && dash_url(stream).is_some()
        })
        .max_by_key(|stream| number(stream, "/bandwidth"))
        .context("B站DASH缺少可用AAC音轨，无法生成完整音画文件")?;
    let video_url = dash_url(video).context("B站DASH视频地址无效")?;
    let audio_url = dash_url(audio).context("B站DASH音频地址无效")?;
    Ok((
        vec![VideoQuality {
            quality: text(video, "/id"),
            url: video_url,
            bit_rate: Some(number(video, "/bandwidth").max(0) as u64),
        }],
        Some(audio_url),
    ))
}

fn map_bilibili(
    data: &Value,
    page: &Value,
    playback: &Value,
    part: usize,
    part_count: usize,
) -> Result<ParsedData> {
    let (videos, audio_url) = bili_streams(playback)?;
    let mut parsed = ParsedData::empty("video");
    parsed.title = text(data, "/title");
    if part_count > 1 {
        parsed.title = format!("{} P{part} {}", parsed.title, text(page, "/part"));
    }
    parsed.desc = text(data, "/desc");
    parsed.author = text(data, "/owner/name");
    parsed.uid = text(data, "/owner/mid");
    parsed.avatar = text(data, "/owner/face");
    parsed.cover = text(data, "/pic");
    parsed.duration = duration(page, "/duration");
    parsed.publish_time = number(data, "/pubdate").saturating_mul(1000);
    parsed.like = number(data, "/stat/like");
    parsed.comment = number(data, "/stat/reply");
    parsed.collect = number(data, "/stat/favorite");
    parsed.share = number(data, "/stat/share");
    parsed.play = number(data, "/stat/view");
    let width = number(page, "/dimension/width");
    let height = number(page, "/dimension/height");
    if width > 0 && height > 0 {
        parsed.cover_size = Some(ImageSize {
            width: width as u32,
            height: height as u32,
        });
    }
    parsed.video = videos[0].url.clone();
    parsed.audio_url = audio_url;
    parsed.videos = videos;
    Ok(parsed)
}

fn bangumi_episode<'a>(season: &'a Value, id: &str) -> Result<&'a Value> {
    let episodes = season
        .get("episodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let sections = season
        .get("section")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|section| {
            section
                .get("episodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        });
    if let Some(episode_id) = id.strip_prefix("ep") {
        return episodes
            .chain(sections)
            .find(|episode| text(episode, "/id") == episode_id)
            .context("B站番剧中未找到目标集数");
    }
    episodes
        .chain(sections)
        .next()
        .context("B站番剧暂无可播放集数")
}

async fn parse_bangumi(http: &SiteHttp, id: &str) -> Result<ParsedData> {
    let param = if let Some(episode) = id.strip_prefix("ep") {
        ("ep_id", episode.to_owned())
    } else {
        ("season_id", id.trim_start_matches("ss").to_owned())
    };
    let url = endpoint("https://api.bilibili.com/pgc/view/web/season", &[param])?;
    let response = http
        .get_json_headers(&url, "bilibili", headers("https://www.bilibili.com/")?)
        .await?;
    let season = checked_bili_payload(&response)?;
    let episode = bangumi_episode(season, id)?;
    let episode_id = text(episode, "/id");
    let mut params = [
        ("ep_id", episode_id),
        ("cid", text(episode, "/cid")),
        ("qn", "64".into()),
        ("fnval", "1".into()),
        ("platform", "html5".into()),
    ];
    let play_url = endpoint("https://api.bilibili.com/pgc/player/web/playurl", &params)?;
    let mut playback_response = http
        .get_json_headers(&play_url, "bilibili", headers("https://www.bilibili.com/")?)
        .await?;
    let payload = checked_bili_payload(&playback_response)?;
    if payload.get("dash").is_none()
        && !payload
            .get("durl")
            .and_then(Value::as_array)
            .is_some_and(|segments| !segments.is_empty())
    {
        for (key, value) in &mut params {
            if *key == "fnval" {
                *value = "16".into();
            }
        }
        let dash_url = endpoint("https://api.bilibili.com/pgc/player/web/playurl", &params)?;
        playback_response = http
            .get_json_headers(&dash_url, "bilibili", headers("https://www.bilibili.com/")?)
            .await?;
    }
    let playback = checked_bili_payload(&playback_response)?;
    ensure!(
        text(playback, "/play_video_type") != "preview",
        "B站番剧只提供试看内容，请补充有观看权限的Cookie"
    );
    let (videos, audio_url) = bili_streams(playback)?;
    let mut parsed = ParsedData::empty("video");
    parsed.title = format!(
        "{} {} {}",
        text(season, "/title"),
        text(episode, "/title"),
        text(episode, "/long_title")
    )
    .trim()
    .to_owned();
    parsed.desc = text(season, "/evaluate");
    parsed.cover = media_url(&text(episode, "/cover"))
        .or_else(|| media_url(&text(season, "/cover")))
        .unwrap_or_default();
    parsed.author = text(season, "/up_info/uname");
    parsed.uid = text(season, "/up_info/mid");
    parsed.duration = duration(episode, "/duration") / 1000;
    parsed.play = number(season, "/stat/views");
    parsed.like = number(season, "/stat/likes");
    parsed.collect = number(season, "/stat/favorites");
    parsed.video = videos[0].url.clone();
    parsed.audio_url = audio_url;
    parsed.videos = videos;
    Ok(parsed)
}

async fn parse_weibo(http: &SiteHttp, input: &str) -> Result<ParsedData> {
    let url = Url::parse(input)?;
    let mut id = url
        .query_pairs()
        .find(|(key, _)| key == "fid" || key == "id" || key == "layerid")
        .map(|(_, value)| value.into_owned())
        .or_else(|| {
            url.path_segments()
                .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
                .map(str::to_owned)
        })
        .context("微博链接缺少动态标识")?;
    if id.contains(':') {
        ensure!(
            id.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b':'),
            "微博视频标识无效"
        );
        let component = endpoint(
            "https://weibo.com/tv/api/component",
            &[("page", format!("/tv/show/{id}"))],
        )?;
        let data = serde_json::json!({"Component_Play_Playinfo":{"oid":id}}).to_string();
        let response = http
            .post_form(&component, "weibo", &[("data".to_owned(), data)])
            .await?;
        id = text(&response, "/data/Component_Play_Playinfo/mid");
    }
    ensure!(
        !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric()),
        "请提供微博正文分享链接（status、detail或作者ID/动态ID），当前视频卡片链接需要先还原正文"
    );
    let desktop = endpoint(
        "https://weibo.com/ajax/statuses/show",
        &[("id", id.clone())],
    )?;
    let response = http
        .get_json_headers(&desktop, "weibo", headers("https://weibo.com/")?)
        .await;
    if let Ok(response) = response
        && response.get("user").is_some()
    {
        return map_weibo(&response);
    }
    let mobile = endpoint("https://m.weibo.cn/statuses/show", &[("id", id)])?;
    let response = http
        .get_json_headers(&mobile, "weibo", headers("https://m.weibo.cn/")?)
        .await
        .context("微博原站请求失败，请补充 weibo Cookie 后重试")?;
    ensure!(
        number(&response, "/ok") == 1,
        "微博原站未返回正文，{}，请补充 weibo Cookie",
        text(&response, "/msg")
    );
    map_weibo(response.get("data").context("微博响应缺少正文")?)
}

fn clean_html(raw: &str) -> String {
    HTML_TAG
        .replace_all(raw, "")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_owned()
}

fn weibo_media(media: &Value) -> Vec<VideoQuality> {
    let mut videos: Vec<VideoQuality> = media
        .get("playback_list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let play = entry.get("play_info")?;
            let url = progressive_url(&text(play, "/url"))?;
            Some(VideoQuality {
                quality: text(play, "/quality_desc"),
                url,
                bit_rate: Some(number(play, "/bitrate").max(0) as u64),
            })
        })
        .collect();
    for (key, label) in [
        ("mp4_1080p_mp4", "1080p"),
        ("mp4_720p_mp4", "720p"),
        ("stream_url_hd", "HD"),
        ("stream_url", "SD"),
    ] {
        if let Some(url) = progressive_url(&text(media, &format!("/{key}")))
            && !videos.iter().any(|video| video.url == url)
        {
            videos.push(VideoQuality {
                quality: label.into(),
                url,
                bit_rate: None,
            });
        }
    }
    videos.sort_by_key(|video| std::cmp::Reverse(video.bit_rate.unwrap_or(0)));
    videos
}

fn map_weibo(data: &Value) -> Result<ParsedData> {
    let mut parsed = ParsedData::empty("video");
    parsed.desc = text(data, "/text_raw");
    if parsed.desc.is_empty() {
        parsed.desc = clean_html(&text(data, "/text"));
    }
    parsed.title = text(data, "/page_info/media_info/video_title");
    if parsed.title.is_empty() {
        parsed.title = parsed.desc.chars().take(100).collect();
    }
    parsed.author = text(data, "/user/screen_name");
    parsed.uid = text(data, "/user/id");
    parsed.avatar = text(data, "/user/avatar_hd");
    parsed.cover = media_url(&text(data, "/page_info/page_pic/url"))
        .or_else(|| media_url(&text(data, "/page_info/page_pic")))
        .unwrap_or_default();
    parsed.like = number(data, "/attitudes_count");
    parsed.comment = number(data, "/comments_count");
    parsed.share = number(data, "/reposts_count");
    parsed.duration = duration(data, "/page_info/media_info/duration");
    parsed.publish_time =
        number(data, "/page_info/media_info/video_publish_time").saturating_mul(1000);
    parsed.play = number(data, "/page_info/media_info/online_users_number");
    let media = data
        .pointer("/page_info/media_info")
        .unwrap_or(&Value::Null);
    parsed.videos = weibo_media(media);
    if parsed.videos.is_empty() {
        let urls = data.pointer("/page_info/urls").unwrap_or(&Value::Null);
        for key in ["mp4_1080p_mp4", "mp4_720p_mp4", "mp4_hd_mp4", "mp4_ld_mp4"] {
            if let Some(url) = progressive_url(&text(urls, &format!("/{key}"))) {
                parsed.videos.push(VideoQuality {
                    quality: key.into(),
                    url,
                    bit_rate: None,
                });
            }
        }
    }
    for picture in data
        .get("pics")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(url) =
            media_url(&text(picture, "/large/url")).or_else(|| media_url(&text(picture, "/url")))
            && !parsed.images.contains(&url)
        {
            parsed.images.push(url);
        }
    }
    // Newer posts can mix still images and videos in a single attachment list.
    for item in data
        .pointer("/mix_media_info/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let content = item.get("data").unwrap_or(&Value::Null);
        if text(item, "/type") == "pic" {
            if let Some(url) = media_url(&text(content, "/largest/url"))
                .or_else(|| media_url(&text(content, "/large/url")))
                .or_else(|| media_url(&text(content, "/url")))
                && !parsed.images.contains(&url)
            {
                parsed.images.push(url);
            }
        } else if parsed.videos.is_empty() {
            parsed.videos = weibo_media(content.get("media_info").unwrap_or(content));
        }
    }
    parsed.video = parsed
        .videos
        .first()
        .map(|video| video.url.clone())
        .unwrap_or_default();
    if parsed.video.is_empty() {
        parsed.media_type = "image".into();
    }
    if parsed.cover.is_empty() {
        parsed.cover = parsed.images.first().cloned().unwrap_or_default();
    }
    ensure!(
        parsed.has_media(),
        "微博正文没有可直接发送的媒体，可能需要登录权限或原站仅提供分段流"
    );
    Ok(parsed)
}

fn assigned_json(html: &str, marker: &str) -> Result<Value> {
    let start = html
        .find(marker)
        .context("AcFun页面缺少视频元数据，可能需要登录或分享链接已经失效")?
        + marker.len();
    let source = html[start..]
        .trim_start()
        .strip_prefix('=')
        .context("AcFun视频元数据格式发生变化")?
        .trim_start();
    serde_json::Deserializer::from_str(source)
        .into_iter::<Value>()
        .next()
        .context("AcFun视频数据为空")?
        .context("AcFun视频数据不是有效JSON")
}

async fn parse_acfun(http: &SiteHttp, url: &str) -> Result<ParsedData> {
    let html = http
        .get_text_headers(url, "acfun", headers("https://www.acfun.cn/")?)
        .await?;
    let data = assigned_json(&html, "window.videoInfo")?;
    map_acfun(&data)
}

fn map_acfun(data: &Value) -> Result<ParsedData> {
    let current = data
        .get("currentVideoInfo")
        .context("AcFun页面未返回当前分P")?;
    let playback: Value =
        serde_json::from_str(&text(current, "/ksPlayJson")).context("AcFun播放参数缺失或无效")?;
    let mut videos = Vec::new();
    for representation in playback
        .get("adaptationSet")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|set| {
            set.get("representation")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
    {
        if let Some(url) = acfun_stream_url(&text(representation, "/url")) {
            videos.push(VideoQuality {
                quality: text(representation, "/qualityLabel"),
                url,
                bit_rate: Some(number(representation, "/avgBitrate").max(0) as u64),
            });
        }
        for backup in representation
            .get("backupUrl")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if let Some(url) = acfun_stream_url(backup) {
                videos.push(VideoQuality {
                    quality: text(representation, "/qualityLabel"),
                    url,
                    bit_rate: Some(number(representation, "/avgBitrate").max(0) as u64),
                });
            }
        }
    }
    ensure!(!videos.is_empty(), "AcFun未返回可用的MP4或HLS播放地址");
    videos.sort_by(|left, right| {
        let left_is_mp4 = !left
            .url
            .split('?')
            .next()
            .unwrap_or_default()
            .ends_with(".m3u8");
        let right_is_mp4 = !right
            .url
            .split('?')
            .next()
            .unwrap_or_default()
            .ends_with(".m3u8");
        right_is_mp4
            .cmp(&left_is_mp4)
            .then_with(|| right.bit_rate.unwrap_or(0).cmp(&left.bit_rate.unwrap_or(0)))
    });
    let mut parsed = ParsedData::empty("video");
    parsed.title = text(data, "/title");
    let list = data.get("videoList").and_then(Value::as_array);
    if let Some(list) = list.filter(|list| list.len() > 1)
        && let Some((index, part)) = list
            .iter()
            .enumerate()
            .find(|(_, part)| text(part, "/id") == text(current, "/id"))
    {
        parsed.title = format!("{} P{} {}", parsed.title, index + 1, text(part, "/title"));
    }
    parsed.desc = text(data, "/description");
    parsed.cover = text(data, "/coverUrl");
    parsed.author = text(data, "/user/name");
    parsed.uid = text(data, "/user/id");
    parsed.avatar = text(data, "/user/headUrl");
    parsed.duration = duration(current, "/durationMillis") / 1000;
    parsed.publish_time = number(current, "/uploadTime");
    parsed.like = number(data, "/likeCount");
    parsed.comment = number(data, "/commentCount");
    parsed.play = number(data, "/viewCount");
    parsed.video = videos[0].url.clone();
    parsed.videos = videos;
    Ok(parsed)
}

fn acfun_stream_url(raw: &str) -> Option<String> {
    let url = media_url(raw)?;
    let path = Url::parse(&url).ok()?.path().to_ascii_lowercase();
    (path.ends_with(".mp4") || path.ends_with(".m3u8")).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bili_keeps_part_and_refuses_invalid_part() {
        assert_eq!(
            bili_page(&Url::parse("https://www.bilibili.com/video/BV1xx?p=2").unwrap()).unwrap(),
            2
        );
        assert!(
            bili_page(&Url::parse("https://www.bilibili.com/video/BV1xx?p=0").unwrap()).is_err()
        );
        let parsed = map_bilibili(
            &json!({"title":"全集","owner":{"name":"作者"}}),
            &json!({"part":"第二集","duration":31}),
            &json!({"format":"mp4","quality":32,"durl":[{"url":"https://media.example/full.mp4"}]}),
            2,
            3,
        )
        .unwrap();
        assert_eq!(parsed.title, "全集 P2 第二集");
        assert_eq!(parsed.duration, 31);
    }

    #[test]
    fn bili_rejects_partial_or_missing_audio_or_preview_stream() {
        assert!(
            checked_bili_payload(&json!({"code":0,"data":{"v_voucher":"voucher_test"}})).is_err()
        );
        assert!(
            bili_streams(
                &json!({"dash":{"video":[{"baseUrl":"https://media.example/v.m4s"}],"audio":[{}]}})
            )
            .is_err()
        );
        assert!(bili_streams(&json!({"durl":[{"url":"https://media.example/a.mp4"},{"url":"https://media.example/b.mp4"}]})).is_err());
        assert!(
            bili_streams(
                &json!({"is_preview":true,"durl":[{"url":"https://media.example/a.mp4"}]})
            )
            .is_err()
        );
        let not_found =
            checked_bili_payload(&json!({"code":-404,"message":"啥都木有"})).unwrap_err();
        assert!(not_found.is::<BilibiliMediaUnavailable>());
        assert!(not_found.to_string().contains("-404"));
        let area = checked_bili_payload(&json!({"code":-10403,"message":"版权限制"})).unwrap_err();
        assert!(area.is::<BilibiliMediaUnavailable>());
    }

    #[test]
    fn bili_dash_selects_h264_and_highest_aac_with_both_urls() {
        let playback = json!({"dash": {
            "video": [
                {"id":80,"codecs":"avc1.640028","height":1080,"bandwidth":3000,"baseUrl":"https://media.example/1080.m4s"},
                {"id":64,"codecs":"avc1.640028","height":720,"bandwidth":1500,"base_url":"https://media.example/720.m4s"},
                {"id":64,"codecs":"hev1.1.6.L120","height":720,"bandwidth":1000,"baseUrl":"https://media.example/hevc.m4s"}
            ],
            "audio": [
                {"id":30216,"codecs":"mp4a.40.2","bandwidth":64000,"baseUrl":"https://media.example/audio-low.m4s"},
                {"id":30280,"codecs":"mp4a.40.2","bandwidth":192000,"baseUrl":"https://media.example/audio-high.m4s"},
                {"id":30251,"codecs":"fLaC","bandwidth":512000,"baseUrl":"https://media.example/lossless.m4s"}
            ]
        }});
        let parsed = map_bilibili(&json!({"title":"DASH"}), &json!({}), &playback, 1, 1).unwrap();
        assert_eq!(parsed.video, "https://media.example/720.m4s");
        assert_eq!(
            parsed.audio_url.as_deref(),
            Some("https://media.example/audio-high.m4s")
        );
        let mut without_audio = playback;
        without_audio["dash"]["audio"] = json!([]);
        assert!(bili_streams(&without_audio).is_err());
        let mut preview = without_audio;
        preview["is_preview"] = 1.into();
        assert!(
            bili_streams(&preview)
                .unwrap_err()
                .to_string()
                .contains("试看")
        );
    }

    #[test]
    fn bangumi_finds_episode_inside_bonus_section() {
        let season = json!({"episodes":[{"id":10}],"section":[{"episodes":[{"id":11}]}]});
        assert_eq!(text(bangumi_episode(&season, "ep11").unwrap(), "/id"), "11");
        assert!(bangumi_episode(&season, "ep12").is_err());
    }

    #[test]
    fn weibo_selects_high_bitrate_and_preserves_gallery() {
        let parsed = map_weibo(&json!({"text":"正文<b>文字</b>","pics":[{"large":{"url":"//wx1.sinaimg.cn/large/a.jpg"}}],"page_info":{"media_info":{"duration":10.1,"playback_list":[{"play_info":{"url":"https://media.example/low.mp4","bitrate":100}},{"play_info":{"url":"https://media.example/high.mp4","bitrate":500}},{"play_info":{"url":"https://media.example/stream.m3u8","bitrate":900}}]}}})).unwrap();
        assert_eq!(parsed.video, "https://media.example/high.mp4");
        assert_eq!(parsed.images, vec!["https://wx1.sinaimg.cn/large/a.jpg"]);
        assert_eq!(parsed.desc, "正文文字");
        assert_eq!(parsed.duration, 10);
    }

    #[test]
    fn weibo_image_only_and_mixed_attachments() {
        let parsed = map_weibo(&json!({"text_raw":"图集","mix_media_info":{"items":[{"type":"pic","data":{"largest":{"url":"https://image.example/p.jpg"}}}]}})).unwrap();
        assert_eq!(parsed.media_type, "image");
        assert_eq!(parsed.images, vec!["https://image.example/p.jpg"]);
    }

    #[test]
    fn acfun_reads_balanced_json_with_braces_in_strings() {
        let value = assigned_json(
            "<script>window.videoInfo = {\"title\":\"a}b\",\"x\":{\"v\":1}};other()</script>",
            "window.videoInfo",
        )
        .unwrap();
        assert_eq!(value["title"], "a}b");
    }

    #[test]
    fn acfun_maps_part_and_passes_hls_to_media_relay() {
        let mut data = json!({"title":"系列","videoList":[{"id":"a","title":"第一集"},{"id":"b","title":"第二集"}],"currentVideoInfo":{"id":"b","durationMillis":12340,"ksPlayJson":json!({"adaptationSet":[{"representation":[{"url":"https://media.example/b.mp4","avgBitrate":500}]}]}).to_string()}});
        let parsed = map_acfun(&data).unwrap();
        assert_eq!(parsed.title, "系列 P2 第二集");
        assert_eq!(parsed.duration, 12);
        data["currentVideoInfo"]["ksPlayJson"] =
            json!({"adaptationSet":[{"representation":[{"url":"https://media.example/b.m3u8"}]}]})
                .to_string()
                .into();
        assert_eq!(
            map_acfun(&data).unwrap().video,
            "https://media.example/b.m3u8"
        );
        data["currentVideoInfo"]["ksPlayJson"] = json!({"adaptationSet":[{"representation":[
            {"url":"https://media.example/b.m3u8","avgBitrate":9000},
            {"url":"https://media.example/b.mp4","avgBitrate":1000}
        ]}]})
        .to_string()
        .into();
        assert_eq!(
            map_acfun(&data).unwrap().video,
            "https://media.example/b.mp4"
        );
    }
}
