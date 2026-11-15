use std::collections::HashMap;

use anyhow::{Result, bail};
use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use url::Url;

use crate::parser::douyin_sign;
use crate::parser::http::{
    MOBILE_USER_AGENT, SiteAccessRestricted, SiteHttp, SiteJsonUnavailable, USER_AGENT,
    domain_matches,
};
use crate::types::{ImageSize, LivePhoto, ParsedData, VideoQuality};

static SCRIPTS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<script\b([^>]*)>(.*?)</script\s*>").unwrap());
static META: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<meta\b([^>]+)>").unwrap());
static ATTRS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?is)([\w:-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());
const DOUYIN_APP_UA: &str = "com.ss.android.ugc.aweme/290101 (Linux; U; Android 10; zh_CN; Pixel 4; Build/QQ3A.200805.001; Cronet/TTNetVersion:5f9037be 2023-01-13 QuicVersion:4668bb42 2022-11-21)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DouyinMediaUnavailable {
    Work,
    Slides,
    SlidesFiltered(u64),
}

impl std::fmt::Display for DouyinMediaUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Work => formatter.write_str(
                "抖音分享页未提供可下载的作品媒体，请检查原帖是否可访问，需登录时请配置对应域的 auth.cookieFiles.douyin",
            ),
            Self::Slides => formatter.write_str(
                "抖音未返回此图集的媒体详情，请在抖音确认原帖是否可播放",
            ),
            Self::SlidesFiltered(reason) => write!(
                formatter,
                "抖音未返回此图集的媒体详情（原站过滤码 {reason}），请在抖音确认原帖是否可播放",
            ),
        }
    }
}

impl std::error::Error for DouyinMediaUnavailable {}

/// Read only data delivered by the original site's public page.
pub async fn parse(http: &SiteHttp, url: &str, platform: &str) -> Result<ParsedData> {
    if platform == "douyin"
        && let Some(id) = douyin_gallery_id(url)
    {
        return parse_douyin_gallery(http, url, &id).await;
    }
    if platform == "douyin"
        && let Some(id) = douyin_id(url)
        && let Some(mut parsed) = douyin_feed(http, &id).await?
    {
        set_media_headers(&mut parsed, "https://www.douyin.com/", platform);
        return Ok(parsed);
    }
    if platform == "zhihu" && url.contains("/zvideo/") {
        let id = item_id(url, platform);
        if !id.is_empty() && id.chars().all(|character| character.is_ascii_digit()) {
            let data = http
                .get_json(
                    &format!("https://www.zhihu.com/api/v4/zvideos/{id}"),
                    platform,
                )
                .await?;
            let mut parsed = map_zhihu(&data);
            if parsed.has_media() {
                set_media_headers(&mut parsed, url, platform);
                return Ok(parsed);
            }
            bail!("知乎视频详情未提供可访问的播放地址，可能受登录权限限制");
        }
    }
    let target = if platform == "douyin" {
        douyin_id(url)
            .map(|id| format!("https://www.iesdouyin.com/share/video/{id}/"))
            .unwrap_or_else(|| url.to_owned())
    } else {
        url.to_owned()
    };
    let html = http.get_text(&target, platform).await?;
    let mut parsed = parse_page(&html, url, platform)?;
    set_media_headers(&mut parsed, &target, platform);
    Ok(parsed)
}

async fn parse_douyin_gallery(http: &SiteHttp, url: &str, id: &str) -> Result<ParsedData> {
    let kind = if douyin_slides_id(url).is_some() {
        "slides"
    } else {
        "note"
    };
    let share_url = format!("https://www.iesdouyin.com/share/{kind}/{id}/");
    let page_url = if Url::parse(url).ok().is_some_and(|url| {
        url.host_str()
            .is_some_and(|host| domain_matches(host, "iesdouyin.com"))
    }) {
        url
    } else {
        &share_url
    };
    let html = http.get_text(page_url, "douyin").await?;
    let page_media = embedded_states(&html)
        .iter()
        .filter_map(|state| {
            find_item(state, id, &["aweme_id", "awemeId"], |item| {
                aweme_images(item).is_some()
            })
            .map(map_aweme)
        })
        .filter(|data| !data.images.is_empty())
        .max_by_key(|data| (data.live_photo.len(), data.images.len()));
    let page_media = match page_media {
        Some(mut data) if data.live_photo.len() == data.images.len() => {
            set_media_headers(&mut data, page_url, "douyin");
            return Ok(data);
        }
        other => other,
    };
    let signed_detail = match douyin_web_detail(http, id).await {
        Ok(Some(mut data)) if douyin_detail_complete(&data) => {
            set_media_headers(&mut data, "https://www.douyin.com/", "douyin");
            data.media_headers
                .insert("User-Agent".into(), douyin_sign::WEB_USER_AGENT.into());
            return Ok(data);
        }
        Ok(data) => data,
        Err(error) if error.is::<SiteAccessRestricted>() => {
            if let Some(mut page) = page_media {
                set_media_headers(&mut page, page_url, "douyin");
                return Ok(page);
            }
            return Err(error);
        }
        Err(_) => None,
    };
    let mut endpoint = Url::parse("https://www.iesdouyin.com/web/api/v2/aweme/slidesinfo/")?;
    endpoint
        .query_pairs_mut()
        .append_pair("aweme_ids", &format!("[{id}]"))
        .append_pair("reflow_source", "reflow_page")
        .append_pair("request_source", "200");
    let mut detail = if let Some(data) = signed_detail {
        Ok(data)
    } else {
        http.get_json(endpoint.as_str(), "douyin")
            .await
            .and_then(|value| map_douyin_slides_detail(&value, id).map_err(Into::into))
    };
    let need_pc_page = match &detail {
        Ok(data) => data.live_photo.len() < data.images.len(),
        Err(error) => !error.is::<SiteAccessRestricted>(),
    };
    if need_pc_page {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(USER_AGENT),
        );
        let pc_page = http
            .get_text_headers(
                &format!("https://www.douyin.com/note/{id}"),
                "douyin",
                headers,
            )
            .await;
        match pc_page {
            Ok(html) => {
                if let Some(pc_media) = embedded_states(&html)
                    .iter()
                    .filter_map(|state| {
                        find_item(state, id, &["aweme_id", "awemeId"], |item| {
                            aweme_images(item).is_some()
                        })
                        .map(map_aweme)
                    })
                    .filter(|data| !data.images.is_empty())
                    .max_by_key(|data| (data.live_photo.len(), data.images.len()))
                    && detail.as_ref().map_or(true, |data| {
                        (pc_media.live_photo.len(), pc_media.images.len())
                            > (data.live_photo.len(), data.images.len())
                    })
                {
                    detail = Ok(pc_media);
                }
            }
            Err(error) if error.is::<SiteAccessRestricted>() => {
                if let Some(mut page) = page_media {
                    set_media_headers(&mut page, page_url, "douyin");
                    return Ok(page);
                }
                return Err(error);
            }
            Err(_) => {}
        }
    }
    let mut parsed = match (detail, page_media) {
        (Ok(mut detail), Some(page)) => {
            if page.live_photo.len() > detail.live_photo.len() {
                detail.live_photo = page.live_photo;
            }
            if page.images.len() > detail.images.len() {
                detail.images = page.images;
                detail.image_size_map = page.image_size_map;
            }
            if page.media_type == "live_photo" {
                detail.media_type = page.media_type;
            }
            detail
        }
        (Ok(detail), _) => detail,
        (Err(_), Some(page)) => page,
        (Err(error), None) => return Err(error),
    };
    set_media_headers(&mut parsed, page_url, "douyin");
    Ok(parsed)
}

fn douyin_detail_complete(data: &ParsedData) -> bool {
    !data.images.is_empty()
        && (data.media_type != "live_photo" || data.live_photo.len() == data.images.len())
}

async fn douyin_web_detail(http: &SiteHttp, id: &str) -> Result<Option<ParsedData>> {
    let Some(uifid) = http.session_cookie(
        douyin_sign::DETAIL_ENDPOINT,
        "douyin",
        douyin_sign::UIFID_COOKIES,
    )?
    else {
        return Ok(None);
    };
    let verify_fp = http.session_cookie(douyin_sign::DETAIL_ENDPOINT, "douyin", &["s_v_web_id"])?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let (url, headers) = douyin_sign::detail_request(id, &uifid, verify_fp.as_deref(), timestamp)?;
    let value = http
        .get_json_headers(url.as_str(), "douyin", headers)
        .await?;
    Ok(map_douyin_web_detail(&value, id))
}

fn map_douyin_web_detail(data: &Value, id: &str) -> Option<ParsedData> {
    let item = data.get("aweme_detail")?;
    if text(item, &["/aweme_id", "/awemeId"]) != id || aweme_images(item).is_none() {
        return None;
    }
    let parsed = map_aweme(item);
    (!parsed.images.is_empty()).then_some(parsed)
}

fn map_douyin_slides_detail(data: &Value, id: &str) -> Result<ParsedData, DouyinMediaUnavailable> {
    if let Some(work) = data
        .get("aweme_details")
        .and_then(Value::as_array)
        .and_then(|details| {
            details
                .iter()
                .find(|item| text(item, &["/aweme_id", "/awemeId"]) == id)
        })
        .filter(|item| aweme_images(item).is_some())
    {
        let parsed = map_aweme(work);
        if !parsed.images.is_empty() {
            return Ok(parsed);
        }
    }
    if let Some(reason) = data
        .get("filter_list")
        .and_then(Value::as_array)
        .and_then(|filters| {
            filters
                .iter()
                .find(|item| text(item, &["/aweme_id", "/awemeId"]) == id)
        })
        .and_then(|filter| filter.get("reason").and_then(Value::as_u64))
    {
        return Err(DouyinMediaUnavailable::SlidesFiltered(reason));
    }
    Err(DouyinMediaUnavailable::Slides)
}

async fn douyin_feed(http: &SiteHttp, id: &str) -> Result<Option<ParsedData>> {
    for endpoint in [
        "https://api5-normal-c-hl.amemv.com/aweme/v1/feed/",
        "https://aweme.snssdk.com/aweme/v1/feed/",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(DOUYIN_APP_UA),
        );
        let response = http
            .get_json_headers(
                &format!("{endpoint}?aweme_id={id}&aid=1128"),
                "douyin",
                headers,
            )
            .await;
        match response {
            Ok(data) => {
                if let Some(parsed) = map_douyin_feed(&data, id) {
                    return Ok(Some(parsed));
                }
            }
            Err(error)
                if error.is::<SiteAccessRestricted>() || error.is::<SiteJsonUnavailable>() =>
            {
                return Err(error);
            }
            Err(_) => {}
        }
    }
    Ok(None)
}

fn map_douyin_feed(data: &Value, id: &str) -> Option<ParsedData> {
    // Feed responses include recommendations. Accept only the requested ID.
    let item = data
        .get("aweme_list")?
        .as_array()?
        .iter()
        .find(|item| text(item, &["/aweme_id", "/id"]) == id)?;
    let parsed = map_aweme(item);
    parsed.has_media().then_some(parsed)
}

fn set_media_headers(parsed: &mut ParsedData, url: &str, platform: &str) {
    let user_agent = if matches!(platform, "douyin" | "kuaishou") {
        MOBILE_USER_AGENT
    } else {
        USER_AGENT
    };
    parsed
        .media_headers
        .insert("User-Agent".into(), user_agent.into());
    if let Ok(url) = Url::parse(url) {
        parsed.media_headers.insert(
            "Referer".into(),
            format!("{}/", url.origin().ascii_serialization()),
        );
    }
}

pub fn parse_page(html: &str, url: &str, platform: &str) -> Result<ParsedData> {
    let states = embedded_states(html);
    let id = item_id(url, platform);
    for state in &states {
        let parsed = match platform {
            "douyin" => find_item(state, &id, &["aweme_id", "awemeId"], is_aweme).map(map_aweme),
            "xiaohongshu" => xhs_note(state, &id).map(map_xhs),
            "kuaishou" => find_item(state, "", &[], |photo| kuaishou_matches(photo, &id))
                .map(|photo| map_kuaishou(photo, state)),
            "tiktok" => find_item(state, &id, &["id", "aweme_id"], is_tiktok).map(map_tiktok),
            "instagram" => {
                find_item(state, &id, &["shortcode", "code"], is_instagram).map(map_instagram)
            }
            "youtube" => youtube_player(state, &id).map(map_youtube),
            "toutiao" | "xigua" => current_byte_video(state, &id)
                .or_else(|| find_item(state, &id, &["group_id", "item_id", "videoId"], is_toutiao))
                .map(map_toutiao),
            _ => None,
        };
        if let Some(mut parsed) = parsed.filter(ParsedData::has_media) {
            apply_page_metadata(&mut parsed, html);
            return Ok(parsed);
        }
    }
    // A cover on a login page is insufficient evidence of accessible media.
    let mut parsed = parse_structured_media(html, url);
    apply_page_metadata(&mut parsed, html);
    if parsed.has_media() {
        return Ok(parsed);
    }
    if platform == "douyin" {
        return Err(DouyinMediaUnavailable::Work.into());
    }
    let message = match platform {
        "xiaohongshu" => {
            "小红书未提供可访问的笔记媒体，请保留分享链接的 xsec_token，并补充 auth.cookies.xiaohongshu 登录 Cookie"
        }
        "kuaishou" => {
            "快手页面未提供作品媒体，可能需要登录或网页验证，请补充 auth.cookies.kuaishou 登录 Cookie"
        }
        "youtube" => {
            "YouTube 页面未提供可直接下载的音视频合一流，可能需要播放器签名、登录或外部提取器"
        }
        _ => "原站页面未提供可访问的媒体，可能需要登录、网页验证或该站点的专用提取规则",
    };
    bail!("{platform}: {message}")
}

pub(super) fn douyin_path_id(url: &Url) -> Option<&str> {
    douyin_work_path(url).map(|(_, id)| id)
}

pub(super) fn douyin_slides_id(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let (kind, id) = douyin_work_path(&url)?;
    (kind == "slides").then(|| id.to_owned())
}

fn douyin_gallery_id(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let (kind, id) = douyin_work_path(&url)?;
    matches!(kind, "note" | "slides").then(|| id.to_owned())
}

fn douyin_work_path(url: &Url) -> Option<(&str, &str)> {
    let host = url.host_str()?;
    if !domain_matches(host, "douyin.com") && !domain_matches(host, "iesdouyin.com") {
        return None;
    }
    let raw_path = url.path().trim_end_matches('/');
    let mut segments = raw_path.strip_prefix('/')?.split('/');
    let first = segments.next()?;
    let kind = if first == "share" {
        segments.next()?
    } else {
        first
    };
    let id = segments.next()?;
    (matches!(kind, "video" | "note" | "slides") && segments.next().is_none() && is_douyin_id(id))
        .then_some((kind, id))
}

pub(super) fn douyin_id(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    douyin_path_id(&url).map(str::to_owned).or_else(|| {
        url.query_pairs()
            .find(|(key, value)| {
                matches!(key.as_ref(), "modal_id" | "aweme_id") && is_douyin_id(value)
            })
            .map(|(_, value)| value.into_owned())
    })
}

fn is_douyin_id(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn item_id(url: &str, platform: &str) -> String {
    if platform == "douyin" {
        return douyin_id(url).unwrap_or_default();
    }
    let Ok(parsed) = Url::parse(url) else {
        return String::new();
    };
    if platform == "youtube"
        && let Some((_, value)) = parsed.query_pairs().find(|(key, _)| key == "v")
    {
        return value.into_owned();
    }
    parsed
        .path_segments()
        .and_then(|mut segments| segments.rfind(|part| !part.is_empty()))
        .unwrap_or_default()
        .to_owned()
}

fn attributes(text: &str) -> HashMap<String, String> {
    ATTRS
        .captures_iter(text)
        .map(|capture| {
            let value = capture.get(2).or_else(|| capture.get(3)).unwrap().as_str();
            (
                capture[1].to_ascii_lowercase(),
                html_escape::decode_html_entities(value).into_owned(),
            )
        })
        .collect()
}

fn embedded_states(html: &str) -> Vec<Value> {
    let mut states = Vec::new();
    let mut pace_chunks = String::new();
    let mut pace_overflow = false;
    for capture in SCRIPTS.captures_iter(html) {
        let attrs = attributes(&capture[1]);
        let id = attrs.get("id").map(String::as_str).unwrap_or_default();
        let content_type = attrs.get("type").map(String::as_str).unwrap_or_default();
        if (content_type.eq_ignore_ascii_case("application/json")
            || matches!(
                id,
                "RENDER_DATA"
                    | "SIGI_STATE"
                    | "sigi-persisted-data"
                    | "__UNIVERSAL_DATA_FOR_REHYDRATION__"
                    | "__NEXT_DATA__"
                    | "SSR_HYDRATED_DATA"
            ))
            && let Some(value) = read_js_value(&capture[2])
        {
            states.push(value);
        }
        for (position, _) in capture[2].match_indices("self.__pace_f.push(").take(1024) {
            let raw = &capture[2][position + "self.__pace_f.push(".len()..];
            let Some(Value::Array(chunk)) = read_js_value(raw) else {
                continue;
            };
            if chunk.first().and_then(Value::as_u64) != Some(1) {
                continue;
            }
            let Some(payload) = chunk.get(1).and_then(Value::as_str) else {
                continue;
            };
            if payload.len() > (16 * 1024 * 1024usize).saturating_sub(pace_chunks.len()) {
                pace_overflow = true;
                break;
            }
            pace_chunks.push_str(payload);
        }
    }
    if !pace_overflow {
        for record in pace_chunks.lines() {
            let Some((id, payload)) = record.split_once(':') else {
                continue;
            };
            if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            if let Some(value) = read_js_value(payload) {
                states.push(value);
            }
        }
    }
    for marker in [
        "window._ROUTER_DATA",
        "window.__INITIAL_STATE__",
        "window.__APOLLO_STATE__",
        "window.INIT_STATE",
        "window._SSR_DATA",
        "window.__INIT_PROPS__",
        "window.__INITIAL_DATA__",
        "window._sharedData",
        "window._SSR_HYDRATED_DATA",
        "ytInitialPlayerResponse",
    ] {
        for (position, _) in html.match_indices(marker).take(16) {
            let rest = html[position + marker.len()..].trim_start();
            if let Some(value) = rest.strip_prefix('=').and_then(read_js_value) {
                states.push(value);
                break;
            }
        }
    }
    states
}

fn read_js_value(raw: &str) -> Option<Value> {
    let raw = raw.trim();
    let decoded = if raw.starts_with('%') {
        urlencoding::decode(raw).ok()?
    } else {
        std::borrow::Cow::Borrowed(raw)
    };
    let text = decoded
        .trim()
        .strip_prefix("JSON.parse(")
        .unwrap_or(decoded.trim());
    // Undefined appears in otherwise JSON-compatible SSR state. Replace only
    // tokens outside strings so captions and signed URLs keep their original bytes.
    let first_json = |text: &str| {
        serde_json::Deserializer::from_str(text)
            .into_iter::<Value>()
            .next()?
            .ok()
    };
    let value = first_json(text).or_else(|| first_json(&normalize_undefined(text)))?;
    match value {
        Value::String(inner) => serde_json::from_str(&inner).ok(),
        other => Some(other),
    }
}

fn normalize_undefined(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut quoted = false;
    let mut escaped = false;
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        let character = rest.chars().next().unwrap();
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
        } else if character == '"' {
            quoted = true;
        } else if rest.starts_with("undefined") {
            result.push_str("null");
            index += "undefined".len();
            continue;
        }
        result.push(character);
        index += character.len_utf8();
    }
    result
}

fn find_item<'a>(
    root: &'a Value,
    id: &str,
    id_keys: &[&str],
    shape: impl Fn(&Value) -> bool,
) -> Option<&'a Value> {
    let mut pending = vec![(root, 0usize)];
    let mut visited = 0;
    while let Some((value, depth)) = pending.pop() {
        visited += 1;
        if visited > 50_000 {
            break;
        }
        if depth > 48 {
            continue;
        }
        if shape(value) {
            let value_id = id_keys
                .iter()
                .find_map(|key| value.get(*key).and_then(scalar));
            if id.is_empty() || value_id.as_deref() == Some(id) {
                return Some(value);
            }
        }
        match value {
            Value::Object(map) => {
                pending.extend(map.values().rev().map(|value| (value, depth + 1)))
            }
            Value::Array(array) => {
                pending.extend(array.iter().rev().map(|value| (value, depth + 1)))
            }
            _ => {}
        }
    }
    None
}

fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.to_owned()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn text(value: &Value, pointers: &[&str]) -> String {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(scalar))
        .unwrap_or_default()
}

fn number(value: &Value, pointers: &[&str]) -> i64 {
    pointers
        .iter()
        .find_map(|pointer| {
            let value = value.pointer(pointer)?;
            value
                .as_i64()
                .or_else(|| value.as_f64().map(|number| number as i64))
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(0)
}

fn media_url(raw: &str) -> String {
    let decoded = html_escape::decode_html_entities(raw);
    let absolute = if decoded.starts_with("//") {
        format!("https:{decoded}")
    } else {
        decoded.into_owned()
    };
    Url::parse(&absolute)
        .ok()
        .filter(|url| matches!(url.scheme(), "https" | "http") && url.host_str().is_some())
        .map(|url| url.to_string())
        .unwrap_or_default()
}

fn url_at(value: &Value, pointers: &[&str]) -> String {
    pointers
        .iter()
        .map(|pointer| text(value, &[*pointer]))
        .map(|raw| media_url(&raw))
        .find(|url| !url.is_empty())
        .unwrap_or_default()
}

fn image_size(value: &Value) -> Option<ImageSize> {
    let width = number(value, &["/width"]).max(0) as u32;
    let height = number(value, &["/height"]).max(0) as u32;
    (width > 0 && height > 0).then_some(ImageSize { width, height })
}

fn set_type(parsed: &mut ParsedData) {
    parsed.media_type = if parsed.video.is_empty() {
        "image"
    } else {
        "video"
    }
    .to_owned();
}

fn is_aweme(value: &Value) -> bool {
    (value.get("aweme_id").is_some() || value.get("awemeId").is_some())
        && (value.get("video").is_some() || aweme_images(value).is_some())
}

fn aweme_images(item: &Value) -> Option<&[Value]> {
    [
        "/image_post_info/images",
        "/image_post_info/image_list",
        "/imagePostInfo/images",
        "/imagePostInfo/imageList",
        "/images",
        "/image_list",
    ]
    .into_iter()
    .filter_map(|pointer| item.pointer(pointer)?.as_array())
    .find(|images| !images.is_empty())
    .map(Vec::as_slice)
}

fn map_aweme(item: &Value) -> ParsedData {
    let mut result = ParsedData {
        title: text(item, &["/desc", "/title"]),
        desc: text(item, &["/desc"]),
        author: text(
            item,
            &[
                "/author/nickname",
                "/author/unique_id",
                "/authorInfo/nickname",
            ],
        ),
        uid: text(
            item,
            &[
                "/author/uid",
                "/author/sec_uid",
                "/authorInfo/uid",
                "/authorInfo/secUid",
            ],
        ),
        avatar: url_at(
            item,
            &[
                "/author/avatar_thumb/url_list/0",
                "/author/avatarThumb/urlList/0",
                "/authorInfo/avatarThumb/urlList/0",
                "/authorInfo/avatarThumb/0/src",
                "/authorInfo/avatarThumb",
            ],
        ),
        cover: url_at(
            item,
            &[
                "/video/cover/url_list/0",
                "/video/origin_cover/url_list/0",
                "/video/cover/urlList/0",
                "/video/coverUrlList/0/src",
                "/video/coverUrlList/0",
                "/video/cover",
            ],
        ),
        duration: number(item, &["/video/duration"]).max(0) as u64 / 1000,
        like: number(
            item,
            &[
                "/statistics/digg_count",
                "/statistics/diggCount",
                "/stats/diggCount",
            ],
        ),
        comment: number(
            item,
            &[
                "/statistics/comment_count",
                "/statistics/commentCount",
                "/stats/commentCount",
            ],
        ),
        collect: number(
            item,
            &[
                "/statistics/collect_count",
                "/statistics/collectCount",
                "/stats/collectCount",
            ],
        ),
        share: number(
            item,
            &[
                "/statistics/share_count",
                "/statistics/shareCount",
                "/stats/shareCount",
            ],
        ),
        play: number(
            item,
            &[
                "/statistics/play_count",
                "/statistics/playCount",
                "/stats/playCount",
            ],
        ),
        publish_time: number(item, &["/create_time", "/createTime"]).saturating_mul(1000),
        ..ParsedData::default()
    };
    let mut sizes = HashMap::new();
    let mut has_live_slide = false;
    let aweme_images = aweme_images(item);
    if let Some(images) = aweme_images {
        for image in images {
            has_live_slide |= number(image, &["/live_photo_type", "/livePhotoType"]) == 1
                || number(image, &["/clip_type", "/clipType"]) == 5;
            let url = url_at(
                image,
                &[
                    "/url_list/0",
                    "/urlList/0/src",
                    "/urlList/0/url",
                    "/urlList/0",
                    "/display_image/url_list/0",
                    "/download_url_list/0",
                ],
            );
            if !url.is_empty() {
                if let Some(size) = image_size(image) {
                    sizes.insert(url.clone(), size);
                }
                let live_video = url_at(
                    image,
                    &[
                        "/video/play_addr_h264/url_list/0",
                        "/video/play_addr/url_list/0",
                        "/video/playAddr/0/src",
                        "/video/playAddr/0/url",
                        "/video/playAddr/0",
                        "/video_play_addr/url_list/0",
                        "/video_download_addr/url_list/0",
                    ],
                );
                if !live_video.is_empty() {
                    result.live_photo.push(LivePhoto {
                        image: url.clone(),
                        video: live_video,
                    });
                }
                result.images.push(url);
            }
        }
    }
    if !sizes.is_empty() {
        result.image_size_map = Some(sizes);
    }
    // Image posts can expose their background MP3 as video.play_addr.
    if aweme_images.is_none() {
        let video = item.get("video").unwrap_or(&Value::Null);
        if let Some(rates) = video
            .get("bit_rate")
            .or_else(|| video.get("bitRateList"))
            .and_then(Value::as_array)
        {
            for rate in rates {
                let codec = text(rate, &["/gear_name", "/gearName", "/codec_type"]);
                if number(rate, &["/is_h265", "/isH265"]) > 0
                    || codec.contains("bytevc2")
                    || codec.contains("h265")
                {
                    continue;
                }
                let url = url_at(rate, &["/play_addr/url_list/0", "/playAddr/0/src"]);
                if !url.is_empty() {
                    result.videos.push(VideoQuality {
                        quality: text(rate, &["/gear_name", "/gearName"]),
                        url,
                        bit_rate: Some(number(rate, &["/bit_rate", "/bitRate"]).max(0) as u64),
                    });
                }
            }
        }
        result
            .videos
            .sort_by_key(|quality| std::cmp::Reverse(quality.bit_rate.unwrap_or(0)));
        result.video = result
            .videos
            .first()
            .map(|quality| quality.url.clone())
            .unwrap_or_else(|| {
                url_at(
                    video,
                    &[
                        "/play_addr_h264/url_list/0",
                        "/play_addr/url_list/0",
                        "/playAddr/0/src",
                        "/download_addr/url_list/0",
                    ],
                )
            });
        if let Ok(mut url) = Url::parse(&result.video)
            && url.path().contains("/playwm/")
        {
            url.set_path(&url.path().replace("/playwm/", "/play/"));
            result.video = url.to_string();
        }
    }
    if result.cover.is_empty() {
        result.cover = result.images.first().cloned().unwrap_or_default();
    }
    set_type(&mut result);
    if has_live_slide || !result.live_photo.is_empty() {
        result.media_type = "live_photo".into();
    }
    result
}

fn xhs_note<'a>(root: &'a Value, id: &str) -> Option<&'a Value> {
    let map = root.pointer("/note/noteDetailMap")?.as_object()?;
    let note = map.get(id)?.get("note")?;
    let actual = text(note, &["/noteId", "/id"]);
    (actual.is_empty() || actual == id).then_some(note)
}

fn map_xhs(note: &Value) -> ParsedData {
    let mut result = ParsedData {
        title: text(note, &["/title"]),
        desc: text(note, &["/desc"]),
        author: text(note, &["/user/nickname", "/user/nickName"]),
        uid: text(note, &["/user/userId"]),
        avatar: url_at(note, &["/user/avatar", "/user/image"]),
        like: number(note, &["/interactInfo/likedCount"]),
        comment: number(note, &["/interactInfo/commentCount"]),
        collect: number(note, &["/interactInfo/collectedCount"]),
        share: number(note, &["/interactInfo/shareCount"]),
        publish_time: number(note, &["/time"]),
        ..ParsedData::default()
    };
    if let Some(streams) = note
        .pointer("/video/media/stream")
        .and_then(Value::as_object)
    {
        let preferred = streams
            .get("h264")
            .filter(|value| value.as_array().is_some_and(|items| !items.is_empty()));
        let lists: Vec<&Value> = preferred
            .map(|value| vec![value])
            .unwrap_or_else(|| streams.values().collect());
        for stream in lists.iter().filter_map(|value| value.as_array()).flatten() {
            let url = url_at(stream, &["/masterUrl", "/backupUrls/0"]);
            if !url.is_empty() {
                result.duration = number(stream, &["/duration"]).max(0) as u64 / 1000;
                result.videos.push(VideoQuality {
                    quality: text(stream, &["/qualityType", "/qualityDesc"]),
                    url,
                    bit_rate: Some(number(stream, &["/avgBitrate", "/videoBitrate"]).max(0) as u64),
                });
            }
        }
    }
    result
        .videos
        .sort_by_key(|quality| std::cmp::Reverse(quality.bit_rate.unwrap_or(0)));
    result.video = result
        .videos
        .first()
        .map(|quality| quality.url.clone())
        .unwrap_or_default();
    if result.video.is_empty() {
        let origin_key = text(note, &["/video/consumer/originVideoKey"]);
        if !origin_key.is_empty() {
            result.video = media_url(&format!("https://sns-video-bd.xhscdn.com/{origin_key}"));
        }
    }
    let mut sizes = HashMap::new();
    if let Some(images) = note.get("imageList").and_then(Value::as_array) {
        for image in images {
            let url = url_at(image, &["/urlDefault", "/urlPre", "/infoList/0/url"]);
            if !url.is_empty() {
                if result.cover.is_empty() {
                    result.cover = url.clone();
                    result.cover_size = image_size(image);
                }
                // A video note's imageList contains its cover variants.
                if result.video.is_empty() && text(note, &["/type"]) != "video" {
                    if let Some(size) = image_size(image) {
                        sizes.insert(url.clone(), size);
                    }
                    result.images.push(url);
                }
            }
        }
    }
    if !sizes.is_empty() {
        result.image_size_map = Some(sizes);
    }
    set_type(&mut result);
    result
}

fn is_kuaishou(value: &Value) -> bool {
    value.get("__typename").and_then(Value::as_str) == Some("VisionVideoDetailPhoto")
        || (value.get("photoId").is_some()
            && (value.get("mainMvUrls").is_some() || value.get("photoUrl").is_some()))
        || (value.get("id").is_some() && value.get("videoResource").is_some())
}

fn kuaishou_matches(photo: &Value, id: &str) -> bool {
    if !is_kuaishou(photo) {
        return false;
    }
    if id.is_empty() || text(photo, &["/photoId", "/id"]) == id {
        return true;
    }
    // Mobile SSR uses an internal numeric ID. Its CDN cache key retains the
    // encoded public route ID and lets us reject unrelated recommendations.
    ["/mainMvUrls/0/url", "/coverUrls/0/url", "/photoUrl"]
        .iter()
        .any(|pointer| {
            Url::parse(&text(photo, &[*pointer]))
                .ok()
                .is_some_and(|url| {
                    url.query_pairs().any(|(key, value)| {
                        key == "clientCacheKey"
                            && (value == id
                                || value == format!("{id}.mp4")
                                || value == format!("{id}.jpg"))
                    })
                })
        })
}

fn map_kuaishou(photo: &Value, state: &Value) -> ParsedData {
    let user_ref = text(photo, &["/user/__ref", "/author/__ref"]);
    let user = state
        .pointer("/defaultClient")
        .and_then(|client| client.get(&user_ref))
        .or_else(|| photo.get("user"))
        .or_else(|| photo.get("author"))
        .unwrap_or(photo);
    let mut result = ParsedData {
        title: text(photo, &["/caption", "/title"]),
        desc: text(photo, &["/caption"]),
        author: text(user, &["/name", "/userName"]),
        uid: text(user, &["/id", "/userId"]),
        avatar: url_at(user, &["/headerUrl", "/headUrl"]),
        cover: url_at(photo, &["/coverUrl", "/poster", "/coverUrls/0/url"]),
        duration: number(photo, &["/duration"]).max(0) as u64 / 1000,
        like: number(photo, &["/realLikeCount", "/likeCount"]),
        comment: number(photo, &["/commentCount"]),
        play: number(photo, &["/viewCount"]),
        publish_time: number(photo, &["/timestamp"]),
        video: url_at(
            photo,
            &[
                "/photoUrl",
                "/mainMvUrls/0/url",
                "/mainMvUrls/0",
                "/playUrl",
            ],
        ),
        ..ParsedData::default()
    };
    let resource = photo.pointer("/videoResource/json");
    let parsed_resource = resource
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let resource = parsed_resource.as_ref().or(resource);
    if let Some(sets) = resource
        .and_then(|value| value.pointer("/h264/adaptationSet"))
        .and_then(Value::as_array)
    {
        for set in sets {
            for representation in set
                .get("representation")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let url = url_at(representation, &["/url", "/backupUrl/0"]);
                if !url.is_empty() {
                    result.videos.push(VideoQuality {
                        quality: text(representation, &["/qualityLabel"]),
                        url,
                        bit_rate: Some(
                            number(representation, &["/maxBitrate", "/avgBitrate"]).max(0) as u64,
                        ),
                    });
                }
            }
        }
    }
    result
        .videos
        .sort_by_key(|quality| std::cmp::Reverse(quality.bit_rate.unwrap_or(0)));
    if let Some(quality) = result.videos.first() {
        result.video = quality.url.clone();
    }
    set_type(&mut result);
    result
}

fn is_tiktok(value: &Value) -> bool {
    value.get("id").is_some() && value.get("video").is_some() && value.get("author").is_some()
}

fn map_tiktok(item: &Value) -> ParsedData {
    let mut result = ParsedData {
        title: text(item, &["/desc"]),
        desc: text(item, &["/desc"]),
        author: text(item, &["/author/nickname", "/author/uniqueId"]),
        uid: text(item, &["/author/id"]),
        avatar: url_at(item, &["/author/avatarThumb"]),
        video: url_at(item, &["/video/playAddr", "/video/downloadAddr"]),
        cover: url_at(item, &["/video/cover", "/video/originCover"]),
        duration: number(item, &["/video/duration"]).max(0) as u64,
        like: number(item, &["/stats/diggCount"]),
        comment: number(item, &["/stats/commentCount"]),
        share: number(item, &["/stats/shareCount"]),
        play: number(item, &["/stats/playCount"]),
        collect: number(item, &["/stats/collectCount"]),
        publish_time: number(item, &["/createTime"]).saturating_mul(1000),
        ..ParsedData::default()
    };
    if let Some(images) = item.pointer("/imagePost/images").and_then(Value::as_array) {
        result.images = images
            .iter()
            .map(|image| url_at(image, &["/imageURL/urlList/0"]))
            .filter(|url| !url.is_empty())
            .collect();
    }
    set_type(&mut result);
    result
}

fn is_instagram(value: &Value) -> bool {
    (value.get("shortcode").is_some() || value.get("code").is_some())
        && (value.get("video_url").is_some()
            || value.get("edge_sidecar_to_children").is_some()
            || value.get("is_video").is_some())
}

fn map_instagram(item: &Value) -> ParsedData {
    let mut result = ParsedData {
        title: text(
            item,
            &[
                "/edge_media_to_caption/edges/0/node/text",
                "/caption/text",
                "/accessibility_caption",
            ],
        ),
        author: text(item, &["/owner/full_name", "/owner/username"]),
        uid: text(item, &["/owner/id"]),
        video: url_at(item, &["/video_url"]),
        cover: url_at(item, &["/display_url", "/thumbnail_src"]),
        duration: number(item, &["/video_duration"]).max(0) as u64,
        like: number(
            item,
            &["/edge_media_preview_like/count", "/edge_liked_by/count"],
        ),
        comment: number(item, &["/edge_media_to_parent_comment/count"]),
        play: number(item, &["/video_view_count"]),
        publish_time: number(item, &["/taken_at_timestamp"]).saturating_mul(1000),
        ..ParsedData::default()
    };
    result.desc = result.title.clone();
    if let Some(edges) = item
        .pointer("/edge_sidecar_to_children/edges")
        .and_then(Value::as_array)
    {
        for edge in edges {
            let node = edge.get("node").unwrap_or(&Value::Null);
            let video = url_at(node, &["/video_url"]);
            if result.video.is_empty() && !video.is_empty() {
                result.video = video;
            }
            if node.get("is_video").and_then(Value::as_bool) == Some(false) {
                let image = url_at(node, &["/display_url"]);
                if !image.is_empty() {
                    result.images.push(image);
                }
            }
        }
    } else if item.get("is_video").and_then(Value::as_bool) == Some(false)
        && !result.cover.is_empty()
    {
        result.images.push(result.cover.clone());
    }
    set_type(&mut result);
    result
}

fn youtube_player<'a>(root: &'a Value, id: &str) -> Option<&'a Value> {
    (root.get("streamingData").is_some() && text(root, &["/videoDetails/videoId"]) == id)
        .then_some(root)
}

fn map_youtube(player: &Value) -> ParsedData {
    let mut result = ParsedData {
        title: text(player, &["/videoDetails/title"]),
        desc: text(player, &["/videoDetails/shortDescription"]),
        author: text(player, &["/videoDetails/author"]),
        uid: text(player, &["/videoDetails/channelId"]),
        duration: number(player, &["/videoDetails/lengthSeconds"]).max(0) as u64,
        play: number(player, &["/videoDetails/viewCount"]),
        ..ParsedData::default()
    };
    if let Some(thumbnails) = player
        .pointer("/videoDetails/thumbnail/thumbnails")
        .and_then(Value::as_array)
    {
        result.cover = thumbnails
            .iter()
            .rev()
            .map(|image| url_at(image, &["/url"]))
            .find(|url| !url.is_empty())
            .unwrap_or_default();
    }
    // adaptiveFormats are separate audio/video tracks and need a muxer.
    if let Some(formats) = player
        .pointer("/streamingData/formats")
        .and_then(Value::as_array)
    {
        for format in formats {
            let mime = text(format, &["/mimeType"]);
            let url = url_at(format, &["/url"]);
            if mime.starts_with("video/mp4")
                && format.get("audioQuality").is_some()
                && !url.is_empty()
            {
                result.videos.push(VideoQuality {
                    quality: text(format, &["/qualityLabel"]),
                    url,
                    bit_rate: Some(number(format, &["/bitrate"]).max(0) as u64),
                });
            }
        }
    }
    result
        .videos
        .sort_by_key(|quality| std::cmp::Reverse(quality.bit_rate.unwrap_or(0)));
    result.video = result
        .videos
        .first()
        .map(|quality| quality.url.clone())
        .unwrap_or_default();
    set_type(&mut result);
    result
}

fn is_toutiao(value: &Value) -> bool {
    value.get("video_list").is_some()
        || value.pointer("/videoResource/normal/video_list").is_some()
        || value.pointer("/videoPlayInfo/video_list").is_some()
}

fn current_byte_video<'a>(state: &'a Value, id: &str) -> Option<&'a Value> {
    [
        "/data/initialVideo",
        "/anyVideo/gidInformation/packerData/video",
    ]
    .iter()
    .find_map(|pointer| {
        let video = state.pointer(pointer)?;
        let actual_id = text(video, &["/group_id", "/item_id", "/groupId"]);
        (is_toutiao(video) && (actual_id.is_empty() || actual_id == id)).then_some(video)
    })
}

fn map_toutiao(item: &Value) -> ParsedData {
    let mut result = ParsedData {
        title: text(item, &["/title"]),
        author: text(item, &["/user_info/name", "/userInfo/name"]),
        uid: text(item, &["/user_info/user_id", "/userInfo/userId"]),
        cover: url_at(item, &["/poster_url", "/posterUrl", "/poster", "/coverUrl"]),
        desc: text(item, &["/video_abstract", "/description"]),
        like: number(item, &["/video_like_count", "/repinCount"]),
        comment: number(item, &["/commentCount"]),
        play: number(item, &["/video_watch_count", "/playCount"]),
        publish_time: number(item, &["/video_publish_time", "/publishTime"]).saturating_mul(1000),
        duration: number(
            item,
            &[
                "/video_duration",
                "/duration",
                "/videoPlayInfo/video_duration",
            ],
        )
        .max(0) as u64,
        ..ParsedData::default()
    };
    let list = item
        .get("video_list")
        .or_else(|| item.pointer("/videoResource/normal/video_list"))
        .or_else(|| item.pointer("/videoPlayInfo/video_list"));
    if let Some(videos) = list.and_then(Value::as_object) {
        for (name, video) in videos {
            let raw = text(video, &["/main_url", "/backup_url_1"]);
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&raw)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok());
            let url = media_url(decoded.as_deref().unwrap_or(&raw));
            if !url.is_empty() {
                result.videos.push(VideoQuality {
                    quality: name.clone(),
                    url,
                    bit_rate: Some(
                        number(video, &["/bitrate", "/video_meta/real_bitrate"]).max(0) as u64,
                    ),
                });
            }
        }
    }
    result
        .videos
        .sort_by_key(|quality| std::cmp::Reverse(quality.bit_rate.unwrap_or(0)));
    result.video = result
        .videos
        .first()
        .map(|quality| quality.url.clone())
        .unwrap_or_default();
    set_type(&mut result);
    result
}

fn map_zhihu(data: &Value) -> ParsedData {
    let mut result = ParsedData {
        title: text(data, &["/title"]),
        author: text(data, &["/author/name"]),
        uid: text(data, &["/author/id"]),
        cover: url_at(data, &["/video/thumbnail", "/image_url"]),
        avatar: url_at(data, &["/author/avatar_url"]),
        duration: number(data, &["/video/duration"]).max(0) as u64,
        publish_time: number(data, &["/published_at"]).saturating_mul(1000),
        play: number(data, &["/play_count"]),
        like: number(data, &["/liked_count"]),
        comment: number(data, &["/comment_count"]),
        ..ParsedData::default()
    };
    if let Some(playlist) = data.pointer("/video/playlist").and_then(Value::as_object) {
        for (name, stream) in playlist {
            let url = url_at(stream, &["/url", "/play_url"]);
            if !url.is_empty() {
                result.videos.push(VideoQuality {
                    quality: name.clone(),
                    url,
                    bit_rate: Some(number(stream, &["/bitrate"]).max(0) as u64),
                });
            }
        }
    }
    result
        .videos
        .sort_by_key(|quality| std::cmp::Reverse(quality.bit_rate.unwrap_or(0)));
    result.video = result
        .videos
        .first()
        .map(|quality| quality.url.clone())
        .unwrap_or_default();
    set_type(&mut result);
    result
}

fn page_metadata(html: &str) -> HashMap<String, String> {
    META.captures_iter(html)
        .filter_map(|capture| {
            let attrs = attributes(&capture[1]);
            let key = attrs
                .get("property")
                .or_else(|| attrs.get("name"))?
                .to_ascii_lowercase();
            Some((key, attrs.get("content")?.clone()))
        })
        .collect()
}

fn apply_page_metadata(parsed: &mut ParsedData, html: &str) {
    let metadata = page_metadata(html);
    if parsed.title.is_empty() {
        parsed.title = metadata.get("og:title").cloned().unwrap_or_default();
    }
    if parsed.desc.is_empty() {
        parsed.desc = metadata
            .get("og:description")
            .or_else(|| metadata.get("description"))
            .cloned()
            .unwrap_or_default();
    }
    if parsed.cover.is_empty() {
        parsed.cover = metadata
            .get("og:image")
            .map(|raw| media_url(raw))
            .unwrap_or_default();
    }
}

fn parse_structured_media(html: &str, _url: &str) -> ParsedData {
    for capture in SCRIPTS.captures_iter(html) {
        if attributes(&capture[1])
            .get("type")
            .is_some_and(|value| value.eq_ignore_ascii_case("application/ld+json"))
            && let Some(state) = read_js_value(&capture[2])
            && let Some(video) = find_item(&state, "", &[], |value| {
                value.get("@type").and_then(Value::as_str) == Some("VideoObject")
                    && value.get("contentUrl").is_some()
            })
        {
            let video_url = url_at(video, &["/contentUrl"]);
            if !video_url.is_empty() {
                return ParsedData {
                    media_type: "video".into(),
                    video: video_url,
                    title: text(video, &["/name", "/headline"]),
                    desc: text(video, &["/description"]),
                    author: text(video, &["/author/name"]),
                    cover: url_at(video, &["/thumbnailUrl", "/thumbnailUrl/0"]),
                    ..ParsedData::default()
                };
            }
        }
    }
    let metadata = page_metadata(html);
    let raw = [
        "og:video:secure_url",
        "og:video:url",
        "og:video",
        "twitter:player:stream",
    ]
    .iter()
    .filter_map(|key| metadata.get(*key))
    .map(|url| media_url(url))
    .find(|url| !url.is_empty())
    .unwrap_or_default();
    let mime = metadata
        .get("og:video:type")
        .map(String::as_str)
        .unwrap_or_default();
    let is_file = Url::parse(&raw).ok().is_some_and(|url| {
        let path = url.path().to_ascii_lowercase();
        path.ends_with(".mp4") || path.ends_with(".webm") || path.ends_with(".mov")
    });
    if !raw.is_empty()
        && (is_file || matches!(mime, "video/mp4" | "video/webm" | "video/quicktime"))
    {
        ParsedData {
            media_type: "video".into(),
            video: raw,
            ..ParsedData::default()
        }
    } else {
        ParsedData::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn script(state: Value, marker: &str) -> String {
        format!("<script>{marker}={state};window.end=true;</script>")
    }

    #[test]
    fn reads_nested_js_without_modifying_undefined_in_strings() {
        let value = read_js_value(r#"{"a":undefined,"caption":"undefined %26 { }","b":{"url":"https://example.org/a?x=1%2B2"}};more()"#).unwrap();
        assert!(value["a"].is_null());
        assert_eq!(value["caption"], "undefined %26 { }");
        assert_eq!(value["b"]["url"], "https://example.org/a?x=1%2B2");
    }

    #[test]
    fn streamed_douyin_slides_join_chunks_and_match_the_requested_work() {
        let detail = json!({"awemeId":"123", "authorInfo":{"uid":"author-123", "nickname":"sample author", "avatarThumb":"https://cdn.example.org/avatar.jpg"},
            "stats":{"diggCount":7,"commentCount":3,"collectCount":2,"shareCount":4,"playCount":11}, "images":[
            {"urlList":["https://cdn.example.org/first.jpg"], "video":{"playAddr":[{"src":"https://cdn.example.org/first.mp4"}]}},
            {"urlList":["https://cdn.example.org/second.jpg"], "video":{"playAddr":[{"src":"https://cdn.example.org/second.mp4"}]}}
        ]});
        let record = format!("1:{}\n", json!(["$", null, {"detail":detail}]));
        let split = record.find("first.mp4").unwrap() + 4;
        let mut html = String::new();
        for chunk in [&record[..split], &record[split..]] {
            html.push_str(&format!(
                "<script>self.__pace_f.push({});</script>",
                json!([1, chunk])
            ));
        }
        let parsed = parse_page(&html, "https://www.douyin.com/slides/123", "douyin").unwrap();
        assert_eq!(parsed.media_type, "live_photo");
        assert_eq!(parsed.images.len(), 2);
        assert_eq!(parsed.live_photo.len(), 2);
        assert_eq!(parsed.author, "sample author");
        assert_eq!(parsed.uid, "author-123");
        assert_eq!(parsed.avatar, "https://cdn.example.org/avatar.jpg");
        assert_eq!(
            (
                parsed.like,
                parsed.comment,
                parsed.collect,
                parsed.share,
                parsed.play
            ),
            (7, 3, 2, 4, 11)
        );
        assert_eq!(
            parsed.live_photo[0].image,
            "https://cdn.example.org/first.jpg"
        );
        assert_eq!(
            parsed.live_photo[0].video,
            "https://cdn.example.org/first.mp4"
        );
        assert_eq!(
            parsed.live_photo[1].video,
            "https://cdn.example.org/second.mp4"
        );
        assert!(parse_page(&html, "https://www.douyin.com/slides/999", "douyin").is_err());
    }

    #[test]
    fn streamed_douyin_data_ignores_bootstrap_and_non_json_records() {
        let html = format!(
            "<script>self.__pace_f.push({}); self.__pace_f.push({});</script>",
            json!([0, "1:{\"unexpected\":true}\n"]),
            json!([1, "not-json\n2:I[\"module\"]\n3:{\"wanted\":true}\n"]),
        );
        let states = embedded_states(&html);
        assert_eq!(states, vec![json!({"wanted":true})]);
    }

    #[test]
    fn selects_requested_aweme_and_h264_stream() {
        let html = script(
            json!({"loaderData":{"video_(id)/page":{"videoInfoRes":{"item_list":[
                {"aweme_id":"999","video":{"play_addr":{"url_list":["https://cdn.example.org/wrong.mp4"]}}},
                {"aweme_id":"123","desc":"wanted","video":{"duration":30500,"bit_rate":[
                    {"is_h265":1,"bit_rate":999,"play_addr":{"url_list":["https://cdn.example.org/hevc.mp4"]}},
                    {"is_h265":0,"bit_rate":100,"play_addr":{"url_list":["https://cdn.example.org/h264.mp4"]}}]}}
            ]}}}}),
            "window._ROUTER_DATA",
        );
        let parsed = parse_page(&html, "https://www.douyin.com/video/123", "douyin").unwrap();
        assert_eq!(parsed.title, "wanted");
        assert_eq!(parsed.video, "https://cdn.example.org/h264.mp4");
        assert_eq!(parsed.duration, 30);
    }

    #[test]
    fn image_aweme_ignores_fake_video_music() {
        let parsed = map_aweme(
            &json!({"aweme_id":"123","images":[{"url_list":["https://cdn.example.org/image.webp"]}],
            "video":{"play_addr":{"url_list":["https://cdn.example.org/audio.mp3"]}}}),
        );
        assert_eq!(parsed.media_type, "image");
        assert!(parsed.video.is_empty());
        assert_eq!(parsed.images.len(), 1);
    }

    #[test]
    fn xhs_requires_exact_note_and_keeps_cover_separate() {
        let html = script(
            json!({"note":{"noteDetailMap":{"123":{"note":{"noteId":"123","type":"video","title":"note",
                "imageList":[{"urlDefault":"https://cdn.example.org/cover.jpg"}],
                "video":{"media":{"stream":{"h264":[{"masterUrl":"https://cdn.example.org/clip.mp4","duration":5000,"avgBitrate":5}]}}}
            }}}}}),
            "window.__INITIAL_STATE__",
        );
        let parsed = parse_page(
            &html,
            "https://www.xiaohongshu.com/explore/123?xsec_token=token",
            "xiaohongshu",
        )
        .unwrap();
        assert_eq!(parsed.video, "https://cdn.example.org/clip.mp4");
        assert!(parsed.images.is_empty());
        assert!(
            parse_page(
                &html,
                "https://www.xiaohongshu.com/explore/456",
                "xiaohongshu"
            )
            .is_err()
        );
    }

    #[test]
    fn kuaishou_resolves_apollo_user_reference() {
        let state = json!({"defaultClient":{"photo":{"__typename":"VisionVideoDetailPhoto","id":"3x123","caption":"clip",
            "photoUrl":"https://cdn.example.org/clip.mp4","user":{"__ref":"User:123"}},"User:123":{"name":"author"}}});
        let parsed = parse_page(
            &script(state, "window.__APOLLO_STATE__"),
            "https://www.kuaishou.com/short-video/3x123",
            "kuaishou",
        )
        .unwrap();
        assert_eq!(parsed.author, "author");
        assert_eq!(parsed.video, "https://cdn.example.org/clip.mp4");
    }

    #[test]
    fn accepts_tiktok_universal_data() {
        let html = format!(
            "<script id='__UNIVERSAL_DATA_FOR_REHYDRATION__' type='application/json'>{}</script>",
            json!({"__DEFAULT_SCOPE__":{"webapp.video-detail":{"itemInfo":{"itemStruct":{"id":"123","author":{"nickname":"person"},"video":{"playAddr":"https://cdn.example.org/tiktok.mp4"}}}}}})
        );
        let parsed =
            parse_page(&html, "https://www.tiktok.com/@person/video/123", "tiktok").unwrap();
        assert_eq!(parsed.author, "person");
    }

    #[test]
    fn youtube_excludes_separate_tracks() {
        let state = json!({"videoDetails":{"videoId":"123"},"streamingData":{"adaptiveFormats":[{"url":"https://cdn.example.org/video.mp4"}],
            "formats":[{"mimeType":"video/mp4","audioQuality":"AUDIO_QUALITY_MEDIUM","url":"https://cdn.example.org/av.mp4"}]}});
        assert_eq!(map_youtube(&state).video, "https://cdn.example.org/av.mp4");
    }

    #[test]
    fn metadata_does_not_treat_cover_or_embed_as_download() {
        assert!(
            parse_page(
                r#"<meta property="og:image" content="https://cdn.example.org/cover.jpg">"#,
                "https://example.org/1",
                "zhihu"
            )
            .is_err()
        );
        assert!(
            parse_page(
                r#"<meta property="og:video" content="https://example.org/embed/1">"#,
                "https://example.org/1",
                "zhihu"
            )
            .is_err()
        );
        let parsed = parse_page(r#"<meta content='https://cdn.example.org/clip.mp4?a=1&amp;b=2' property='og:video'><meta content='title &amp; more' property='og:title'>"#,
            "https://example.org/1", "zhihu").unwrap();
        assert_eq!(parsed.title, "title & more");
        assert_eq!(parsed.video, "https://cdn.example.org/clip.mp4?a=1&b=2");
    }

    #[test]
    fn nested_jsonld_requires_video_object_content() {
        let html = format!(
            "<script type='application/ld+json'>{}</script>",
            json!({"@graph":[{"@type":"VideoObject","name":"title","contentUrl":"https://cdn.example.org/video.mp4"}]})
        );
        assert_eq!(
            parse_page(&html, "https://example.org/1", "huya")
                .unwrap()
                .title,
            "title"
        );
    }

    #[test]
    fn mobile_kuaishou_checks_encoded_public_id_against_cache_key() {
        let state = json!({"opaque-key":{"photo":{"photoId":"5229805182084287773", "userName":"author",
            "mainMvUrls":[{"url":"https://cdn.example.org/clip.mp4?clientCacheKey=3x123.mp4"}]}}});
        let html = script(state, "window.INIT_STATE");
        let result = parse_page(
            &html,
            "https://www.kuaishou.com/short-video/3x123",
            "kuaishou",
        )
        .unwrap();
        assert_eq!(result.author, "author");
        assert!(
            parse_page(
                &html,
                "https://www.kuaishou.com/short-video/3x456",
                "kuaishou"
            )
            .is_err()
        );
    }

    #[test]
    fn douyin_feed_matches_requested_id_even_after_recommendations() {
        let data = json!({"aweme_list":[
            {"aweme_id":"999","video":{"play_addr":{"url_list":["https://cdn.example.org/wrong.mp4"]}}},
            {"aweme_id":"123","video":{"play_addr":{"url_list":["https://cdn.example.org/right.mp4"]}}}
        ]});
        assert_eq!(
            map_douyin_feed(&data, "123").unwrap().video,
            "https://cdn.example.org/right.mp4"
        );
        assert!(map_douyin_feed(&data, "456").is_none());
    }

    #[test]
    fn signed_douyin_detail_keeps_only_the_requested_slide_pairs() {
        let data = json!({"aweme_detail": {
            "aweme_id":"123", "images":[
                {"url_list":["https://cdn.example.org/1.jpg"], "video":{"play_addr":{"url_list":["https://cdn.example.org/1.mp4"]}}},
                {"url_list":["https://cdn.example.org/2.jpg"], "video":{"play_addr":{"url_list":["https://cdn.example.org/2.mp4"]}}}
            ]
        }});
        let parsed = map_douyin_web_detail(&data, "123").unwrap();
        assert_eq!(parsed.media_type, "live_photo");
        assert_eq!(parsed.images.len(), 2);
        assert_eq!(parsed.live_photo[0].video, "https://cdn.example.org/1.mp4");
        assert_eq!(parsed.live_photo[1].video, "https://cdn.example.org/2.mp4");
        assert!(map_douyin_web_detail(&data, "999").is_none());
        assert!(map_douyin_web_detail(&json!({"aweme_detail":null}), "123").is_none());
    }

    #[test]
    fn douyin_short_link_stop_requires_an_exact_work_path() {
        for url in [
            "https://www.douyin.com/video/123?tracking=example",
            "https://www.douyin.com/note/123",
            "https://www.iesdouyin.com/share/video/123/",
            "https://www.iesdouyin.com/share/slides/123/",
        ] {
            assert_eq!(douyin_path_id(&Url::parse(url).unwrap()), Some("123"));
        }
        for url in [
            "https://v.douyin.com/one-hop",
            "https://www.douyin.com/redirect?modal_id=123",
            "https://www.douyin.com/video/123more",
            "https://www.douyin.com/video/123/verify",
            "https://aweme.snssdk.com/video/123",
        ] {
            assert_eq!(douyin_path_id(&Url::parse(url).unwrap()), None);
        }
        assert_eq!(
            douyin_id("https://www.douyin.com/user/current?modal_id=123").as_deref(),
            Some("123")
        );
    }

    #[test]
    fn douyin_notes_and_slides_use_the_gallery_route() {
        for url in [
            "https://www.iesdouyin.com/share/note/123/",
            "https://www.iesdouyin.com/share/slides/123/",
            "https://www.douyin.com/note/123",
            "https://www.douyin.com/slides/123",
        ] {
            assert_eq!(douyin_gallery_id(url).as_deref(), Some("123"));
        }
        assert!(douyin_gallery_id("https://www.douyin.com/video/123").is_none());
        assert!(douyin_gallery_id("https://www.douyin.com/note/123/more").is_none());
    }

    #[test]
    fn signed_static_gallery_is_complete_without_a_pc_page_retry() {
        let mut work = json!({"aweme_detail": {
            "aweme_id":"123", "images":[{"url_list":["https://example.com/1.webp"]}]
        }});
        let static_gallery = map_douyin_web_detail(&work, "123").unwrap();
        assert_eq!(static_gallery.media_type, "image");
        assert!(static_gallery.video.is_empty() && static_gallery.live_photo.is_empty());
        assert!(douyin_detail_complete(&static_gallery));
        work["aweme_detail"]["images"][0]["live_photo_type"] = json!(1);
        let missing_clip = map_douyin_web_detail(&work, "123").unwrap();
        assert_eq!(missing_clip.media_type, "live_photo");
        assert!(!douyin_detail_complete(&missing_clip));
        work["aweme_detail"]["images"][0]["video"] =
            json!({"play_addr":{"url_list":["https://example.com/1.mp4"]}});
        let live_gallery = map_douyin_web_detail(&work, "123").unwrap();
        assert!(douyin_detail_complete(&live_gallery));
    }

    #[test]
    fn douyin_slides_keep_their_image_order_and_clip_pairs() {
        let details = serde_json::json!({
            "aweme_details": [
                {
                    "aweme_id": "999",
                    "images": [{"url_list": ["https://example.com/other.webp"]}]
                },
                {
                    "aweme_id": "123",
                    "desc": "实况图集",
                    "image_post_info": {"images": [
                        {
                            "url_list": ["https://example.com/1.webp"],
                            "video": {"play_addr": {"url_list": ["https://example.com/1.mp4"]}}
                        },
                        {"urlList": [{"src": "https://example.com/2.webp"}]},
                        {
                            "urlList": [{"src": "https://example.com/3.webp"}],
                            "video": {"playAddr": [{"src": "https://example.com/3.mp4"}]}
                        }
                    ]},
                    "video": {"play_addr": {"url_list": ["https://example.com/music.mp3"]}}
                }
            ]
        });
        let parsed = map_douyin_slides_detail(&details, "123").unwrap();
        assert_eq!(parsed.media_type, "live_photo");
        assert!(parsed.video.is_empty(), "background music is not the video");
        assert_eq!(
            parsed.images,
            [
                "https://example.com/1.webp",
                "https://example.com/2.webp",
                "https://example.com/3.webp"
            ]
        );
        let clips: Vec<_> = parsed
            .live_photo
            .iter()
            .map(|slide| (slide.image.as_str(), slide.video.as_str()))
            .collect();
        assert_eq!(
            clips,
            [
                ("https://example.com/1.webp", "https://example.com/1.mp4"),
                ("https://example.com/3.webp", "https://example.com/3.mp4")
            ]
        );
    }

    #[test]
    fn douyin_slides_without_mp4_remain_a_static_gallery() {
        let details = serde_json::json!({"aweme_details": [{
            "aweme_id": "123",
            "images": [{"url_list": ["https://example.com/1.webp"]}],
            "video": {"play_addr": {"url_list": ["https://example.com/music.mp3"]}}
        }]});
        let parsed = map_douyin_slides_detail(&details, "123").unwrap();
        assert_eq!(parsed.media_type, "image");
        assert!(parsed.live_photo.is_empty() && parsed.video.is_empty());
        assert_eq!(parsed.images, ["https://example.com/1.webp"]);
    }

    #[test]
    fn known_live_slide_without_media_keeps_its_static_fallback() {
        let details = serde_json::json!({"aweme_details": [{
            "aweme_id": "123",
            "images": [{"live_photo_type": 1, "url_list": ["https://example.com/1.webp"]}]
        }]});
        let parsed = map_douyin_slides_detail(&details, "123").unwrap();
        assert_eq!(parsed.media_type, "live_photo");
        assert!(parsed.live_photo.is_empty());
        assert_eq!(parsed.images, ["https://example.com/1.webp"]);
    }

    #[test]
    fn douyin_filter_reason_applies_only_to_the_requested_slides() {
        let details = serde_json::json!({
            "aweme_details": [],
            "filter_list": [{"aweme_id": "123", "reason": 8}]
        });
        assert_eq!(
            map_douyin_slides_detail(&details, "123").unwrap_err(),
            DouyinMediaUnavailable::SlidesFiltered(8)
        );
        assert_eq!(
            map_douyin_slides_detail(&details, "999").unwrap_err(),
            DouyinMediaUnavailable::Slides
        );
    }

    #[test]
    fn byte_video_uses_current_player_path_and_decodes_base64() {
        let address =
            base64::engine::general_purpose::STANDARD.encode("https://cdn.example.org/video.mp4");
        let state = json!({"anyVideo":{"gidInformation":{"packerData":{"video":{"title":"wanted","videoResource":{"normal":{"video_list":{"video_1":{"main_url":address}}}}}}}}});
        let parsed = parse_page(
            &script(state, "window._SSR_HYDRATED_DATA"),
            "https://www.ixigua.com/123",
            "xigua",
        )
        .unwrap();
        assert_eq!(parsed.title, "wanted");
        assert_eq!(parsed.video, "https://cdn.example.org/video.mp4");
    }

    #[test]
    fn skips_script_references_before_the_actual_state_assignment() {
        let state = json!({"videoDetails":{"videoId":"123"},"streamingData":{"formats":[
            {"mimeType":"video/mp4","audioQuality":"AUDIO_QUALITY_MEDIUM","url":"https://cdn.example.org/av.mp4"}]}});
        let html = format!(
            "<script>if (window.ytInitialPlayerResponse) window.saved = true;</script>{}",
            script(state, "ytInitialPlayerResponse")
        );
        assert_eq!(
            parse_page(&html, "https://www.youtube.com/watch?v=123", "youtube")
                .unwrap()
                .video,
            "https://cdn.example.org/av.mp4"
        );
    }
}
