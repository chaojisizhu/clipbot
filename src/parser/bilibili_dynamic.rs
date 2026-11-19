use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::COOKIE;
use serde_json::Value;
use url::Url;

use crate::parser::http::{SiteHttp, domain_matches};
use crate::parser::sites_core::{
    BilibiliMediaUnavailable, clean_html, endpoint, headers, https_media_url, number,
    parse_bangumi, parse_bilibili_video_by_id, text,
};
use crate::types::{ImageSize, LivePhoto, ParsedData};

const DYNAMIC_FEATURES: &str = "itemOpusStyle,opusBigCover,onlyfansVote,endFooterHidden,decorationCard,onlyfansAssetsV2,ugcDelete,onlyfansQaCard,commentsNewVersion";
const VISITOR_COOKIE_TTL: Duration = Duration::from_secs(3600);

static DYN_PATH_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^/(?:opus|dynamic)/(\d+)/?$").unwrap());
static T_DYN_PATH_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^/(?:h5/dynamic/detail/)?(\d+)/?$").unwrap());
static ARTICLE_PATH_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^/read/cv(\d+)/?$").unwrap());
static BANGUMI_PATH_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/(ep\d+|ss\d+)(?:/|\?|$)").unwrap());
static HTML_IMG_SRC_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?i)<img\b[^>]*?\b(?:data-src|src)=["']([^"']+)["']"#).unwrap());
static VISITOR_COOKIES: Lazy<tokio::sync::Mutex<Option<(Instant, String)>>> =
    Lazy::new(|| tokio::sync::Mutex::new(None));

#[derive(Default)]
struct ExtractedBody {
    paragraphs: Vec<String>,
    images: Vec<String>,
    live_photo: Vec<LivePhoto>,
    image_size_map: HashMap<String, ImageSize>,
}

pub(crate) fn extract_bilibili_cache_identity(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    if let Some(dyn_id) = extract_dynamic_id(&url) {
        return Some(format!("bilibili:dynamic:{dyn_id}"));
    }
    if let Some(cv_id) = extract_article_id(&url) {
        return Some(format!("bilibili:article:{cv_id}"));
    }
    None
}

pub(super) fn extract_dynamic_id(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "t.bilibili.com" {
        return T_DYN_PATH_RE
            .captures(url.path())
            .and_then(|capture| capture.get(1))
            .map(|found| found.as_str().to_owned());
    }
    if domain_matches(&host, "bilibili.com") {
        return DYN_PATH_RE
            .captures(url.path())
            .and_then(|capture| capture.get(1))
            .map(|found| found.as_str().to_owned());
    }
    None
}

pub(super) fn extract_article_id(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    if !domain_matches(&host, "bilibili.com") {
        return None;
    }
    if let Some(cv_id) = ARTICLE_PATH_RE
        .captures(url.path())
        .and_then(|capture| capture.get(1))
        .map(|found| found.as_str().to_owned())
    {
        return Some(cv_id);
    }
    if url.path().trim_end_matches('/') == "/read/mobile" {
        return url
            .query_pairs()
            .find(|(key, value)| {
                key == "id" && !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
            })
            .map(|(_, value)| value.into_owned());
    }
    None
}

pub(super) async fn parse_bilibili_post(http: &SiteHttp, url: &Url) -> Result<Option<ParsedData>> {
    if let Some(cv_id) = extract_article_id(url) {
        return parse_bilibili_article(http, &cv_id).await.map(Some);
    }
    if let Some(dyn_id) = extract_dynamic_id(url) {
        return parse_bilibili_dynamic(http, &dyn_id, url.as_str())
            .await
            .map(Some);
    }
    Ok(None)
}

async fn visitor_cookie(http: &SiteHttp) -> Result<Option<String>> {
    let mut cache = VISITOR_COOKIES.lock().await;
    if let Some((created, cookie)) = &*cache
        && created.elapsed() < VISITOR_COOKIE_TTL
    {
        return Ok(Some(cookie.clone()));
    }
    let response = http
        .get_json_headers(
            "https://api.bilibili.com/x/frontend/finger/spi",
            "bilibili",
            headers("https://www.bilibili.com/")?,
        )
        .await?;
    let b3 = text(&response, "/data/b_3");
    let b4 = text(&response, "/data/b_4");
    if b3.is_empty() {
        return Ok(None);
    }
    let cookie = if b4.is_empty() {
        format!("buvid3={b3}")
    } else {
        format!("buvid3={b3}; buvid4={b4}")
    };
    *cache = Some((Instant::now(), cookie.clone()));
    Ok(Some(cookie))
}

async fn fetch_bili_json(http: &SiteHttp, url: &str, referer: &str) -> Result<Value> {
    let response = http
        .get_json_headers(url, "bilibili", headers(referer)?)
        .await?;
    if number(&response, "/code") == -352
        && !http.has_platform_cookie(url, "bilibili")
        && let Ok(Some(cookie)) = visitor_cookie(http).await
        && let Ok(value) = reqwest::header::HeaderValue::from_str(&cookie)
    {
        let mut retry_headers = headers(referer)?;
        retry_headers.insert(COOKIE, value);
        return http.get_json_headers(url, "bilibili", retry_headers).await;
    }
    Ok(response)
}

fn check_bili_response(response: &Value, resource: &str) -> Result<()> {
    if response.pointer("/data/v_voucher").is_some() {
        bail!("B站原站要求风控验证，请在浏览器完成验证并补充 bilibili Cookie");
    }
    let code = number(response, "/code");
    match code {
        0 => Ok(()),
        -404 | 4101131 | 4101148 => Err(BilibiliMediaUnavailable(format!(
            "B站原站返回 {code}，{resource}不存在、仅自己可见或已被删除"
        ))
        .into()),
        -400 if response.pointer("/data/item").is_none_or(Value::is_null)
            && response.get("data").is_none_or(Value::is_null) =>
        {
            Err(BilibiliMediaUnavailable(format!(
                "B站原站未返回该{resource}，链接可能无效或已被删除"
            ))
            .into())
        }
        -509 => bail!("B站{resource}接口请求过于频繁，请稍后重试"),
        -352 | -412 => bail!("B站原站要求风控验证，请在浏览器完成验证并补充 bilibili Cookie"),
        -10403 => {
            Err(BilibiliMediaUnavailable("B站原站返回 -10403，受地区或版权限制".into()).into())
        }
        _ => bail!(
            "B站原站返回错误 {code}，{}。需要权限时请补充 bilibili Cookie",
            text(response, "/message")
        ),
    }
}

fn push_pic_entry(pic: &Value, body: &mut ExtractedBody, exclude_url: Option<&str>) {
    let raw = text(pic, "/url");
    let raw = if raw.is_empty() {
        text(pic, "/src")
    } else {
        raw
    };
    let Some(url) = https_media_url(&raw) else {
        return;
    };
    if exclude_url.is_some_and(|excluded| excluded == url) {
        return;
    }
    if !body.images.contains(&url) {
        let width = number(pic, "/width");
        let height = number(pic, "/height");
        if width > 0 && height > 0 {
            body.image_size_map.insert(
                url.clone(),
                ImageSize {
                    width: width as u32,
                    height: height as u32,
                },
            );
        }
        if let Some(live_url) = https_media_url(&text(pic, "/live_url")) {
            body.live_photo.push(LivePhoto {
                image: url.clone(),
                video: live_url,
            });
        }
        body.images.push(url);
    }
}

fn push_image_url_only(raw: &str, body: &mut ExtractedBody, exclude_url: Option<&str>) {
    let Some(url) = https_media_url(raw) else {
        return;
    };
    if exclude_url.is_some_and(|excluded| excluded == url) || body.images.contains(&url) {
        return;
    }
    body.images.push(url);
}

fn extract_node_text(node: &Value) -> String {
    for ptr in [
        "/word/words",
        "/rich/text",
        "/rich/orig_text",
        "/text",
        "/orig_text",
    ] {
        let value = text(node, ptr);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

fn extract_opus_paragraphs(paragraphs: Option<&Value>, body: &mut ExtractedBody) {
    let Some(list) = paragraphs.and_then(Value::as_array) else {
        return;
    };
    for paragraph in list {
        if let Some(nodes) = paragraph.pointer("/text/nodes").and_then(Value::as_array) {
            let line: String = nodes.iter().map(extract_node_text).collect();
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                body.paragraphs.push(trimmed.to_owned());
            }
        }
        if let Some(pics) = paragraph.pointer("/pic/pics").and_then(Value::as_array) {
            for pic in pics {
                push_pic_entry(pic, body, None);
            }
        }
    }
}

fn finalize_media_type(parsed: &mut ParsedData) {
    if let Some(first) = parsed.images.first()
        && parsed.cover.is_empty()
    {
        parsed.cover = first.clone();
        parsed.cover_size = parsed
            .image_size_map
            .as_ref()
            .and_then(|map| map.get(first).cloned());
    }
    if !parsed
        .image_size_map
        .as_ref()
        .is_some_and(|map| !map.is_empty())
    {
        parsed.image_size_map = None;
    }
    if !parsed.live_photo.is_empty() {
        parsed.media_type = "live_photo".into();
    } else if !parsed.images.is_empty() {
        parsed.media_type = "image".into();
    } else if !parsed.title.trim().is_empty() || !parsed.desc.trim().is_empty() {
        parsed.media_type = "text".into();
    }
}

pub(super) fn map_article_payload(data: &Value) -> Result<(ParsedData, Option<String>)> {
    let top_bvid = text(data, "/top_video_info/bvid");
    let top_pic = https_media_url(&text(data, "/top_video_info/pic"));
    let exclude_top_pic = (!top_bvid.is_empty())
        .then_some(top_pic.as_deref())
        .flatten();

    let mut body = ExtractedBody::default();
    extract_opus_paragraphs(data.pointer("/opus/content/paragraphs"), &mut body);

    let html_content = {
        let h5 = text(data, "/opus/h5_content");
        if !h5.is_empty() {
            h5
        } else {
            text(data, "/content")
        }
    };
    if body.images.is_empty() && !html_content.is_empty() {
        for capture in HTML_IMG_SRC_RE.captures_iter(&html_content) {
            if let Some(src) = capture.get(1) {
                push_image_url_only(src.as_str(), &mut body, exclude_top_pic);
            }
        }
    }
    if body.images.is_empty() && top_bvid.is_empty() {
        for pointer in ["/origin_image_urls", "/image_urls"] {
            if let Some(urls) = data.pointer(pointer).and_then(Value::as_array) {
                for url in urls.iter().filter_map(Value::as_str) {
                    push_image_url_only(url, &mut body, exclude_top_pic);
                }
            }
        }
        let banner = text(data, "/banner_url");
        if !banner.is_empty() && body.images.is_empty() {
            push_image_url_only(&banner, &mut body, exclude_top_pic);
        }
    }

    let desc = if !body.paragraphs.is_empty() {
        body.paragraphs.join("\n")
    } else {
        let cleaned = clean_html(&html_content);
        if !cleaned.is_empty() {
            cleaned
        } else {
            text(data, "/summary")
        }
    };

    let mut parsed = ParsedData::empty("image");
    parsed.title = {
        let opus_title = text(data, "/opus/title");
        if !opus_title.is_empty() {
            opus_title
        } else {
            text(data, "/title")
        }
    };
    parsed.desc = desc;
    parsed.author = text(data, "/author/name");
    parsed.uid = text(data, "/author/mid");
    parsed.avatar = https_media_url(&text(data, "/author/face")).unwrap_or_default();
    parsed.publish_time = number(data, "/publish_time").saturating_mul(1000);
    parsed.like = number(data, "/stats/like");
    parsed.comment = number(data, "/stats/reply");
    parsed.collect = number(data, "/stats/favorite");
    parsed.share = number(data, "/stats/share");
    parsed.play = number(data, "/stats/view");
    parsed.images = body.images;
    parsed.live_photo = body.live_photo;
    parsed.image_size_map = Some(body.image_size_map);
    finalize_media_type(&mut parsed);

    let fallback_video = if parsed.images.is_empty() && !top_bvid.is_empty() {
        Some(top_bvid)
    } else {
        None
    };
    Ok((parsed, fallback_video))
}

async fn finish_opus_or_article(
    http: &SiteHttp,
    parsed: ParsedData,
    top_bvid: Option<String>,
) -> Result<Option<ParsedData>> {
    if !parsed.images.is_empty() {
        return Ok(Some(parsed));
    }
    if let Some(bvid) = top_bvid {
        return parse_bilibili_video_by_id(http, &bvid, 1).await.map(Some);
    }
    if parsed.has_deliverable() {
        return Ok(Some(parsed));
    }
    Ok(None)
}

async fn parse_bilibili_article(http: &SiteHttp, cv_id: &str) -> Result<ParsedData> {
    let url = endpoint(
        "https://api.bilibili.com/x/article/view",
        &[("id", cv_id.to_owned())],
    )?;
    let referer = format!("https://www.bilibili.com/read/cv{cv_id}");
    let response = fetch_bili_json(http, &url, &referer).await?;
    check_bili_response(&response, "专栏")?;
    let data = response
        .get("data")
        .filter(|value| value.is_object())
        .context("B站专栏响应缺少数据")?;
    let (parsed, top_bvid) = map_article_payload(data)?;
    finish_opus_or_article(http, parsed, top_bvid)
        .await?
        .context("B站专栏未返回可显示的正文、图片或视频")
}

async fn parse_bilibili_dynamic(
    http: &SiteHttp,
    dyn_id: &str,
    referer_url: &str,
) -> Result<ParsedData> {
    let url = endpoint(
        "https://api.bilibili.com/x/polymer/web-dynamic/v1/detail",
        &[
            ("id", dyn_id.to_owned()),
            ("features", DYNAMIC_FEATURES.to_owned()),
        ],
    )?;
    let response = fetch_bili_json(http, &url, referer_url).await?;
    check_bili_response(&response, "动态")?;
    let item = response
        .pointer("/data/item")
        .filter(|value| value.is_object())
        .ok_or_else(|| {
            BilibiliMediaUnavailable("B站原站未返回该动态，动态可能不存在或已被删除".into())
        })?;
    parse_dynamic_item(http, item, true, 0).await
}

fn parse_dynamic_item<'a>(
    http: &'a SiteHttp,
    item: &'a Value,
    allow_opus_detail_fetch: bool,
    depth: usize,
) -> Pin<Box<dyn Future<Output = Result<ParsedData>> + Send + 'a>> {
    Box::pin(async move {
        if depth > 2 {
            bail!("B站转发动态嵌套层级过深");
        }
        if item.pointer("/visible").and_then(Value::as_bool) == Some(false) {
            return Err(BilibiliMediaUnavailable("该动态已删除或设为不可见".into()).into());
        }
        let is_only_fans =
            item.pointer("/basic/is_only_fans").and_then(Value::as_bool) == Some(true);
        let major = item
            .pointer("/modules/module_dynamic/major")
            .unwrap_or(&Value::Null);
        let major_type = text(major, "/type");
        if major_type == "MAJOR_TYPE_BLOCKED"
            || major.get("blocked").is_some_and(|value| !value.is_null())
            || (is_only_fans && major_type == "MAJOR_TYPE_UPOWER_COMMON")
        {
            return Err(BilibiliMediaUnavailable("此动态为充电会员专属，不可访问".into()).into());
        }

        let item_type = text(item, "/type");
        if item_type == "DYNAMIC_TYPE_FORWARD" {
            return parse_forwarded_dynamic(http, item, depth).await;
        }
        if item_type == "DYNAMIC_TYPE_AV" || major_type == "MAJOR_TYPE_ARCHIVE" {
            let bvid = text(major, "/archive/bvid");
            let id = if !bvid.is_empty() {
                bvid
            } else {
                text(major, "/archive/aid")
            };
            if !id.is_empty() {
                return parse_bilibili_video_by_id(http, &id, 1).await;
            }
        }
        if (item_type == "DYNAMIC_TYPE_PGC" || major_type == "MAJOR_TYPE_PGC")
            && let Some(pgc_id) = extract_pgc_id(major)
        {
            return parse_bangumi(http, &pgc_id).await;
        }

        let dyn_id = text(item, "/id_str");
        let has_more = major
            .pointer("/opus/summary/has_more")
            .and_then(Value::as_bool)
            == Some(true);
        if allow_opus_detail_fetch
            && !dyn_id.is_empty()
            && (item_type == "DYNAMIC_TYPE_ARTICLE" || has_more)
        {
            let opus_url = endpoint(
                "https://api.bilibili.com/x/polymer/web-dynamic/v1/opus/detail",
                &[("id", dyn_id.clone())],
            )?;
            if let Ok(opus_resp) = fetch_bili_json(
                http,
                &opus_url,
                &format!("https://www.bilibili.com/opus/{dyn_id}"),
            )
            .await
                && number(&opus_resp, "/code") == 0
            {
                if let Some(opus_item) = opus_resp
                    .pointer("/data/item")
                    .filter(|value| value.is_object())
                {
                    let (parsed, top_bvid) = map_opus_detail_item(opus_item);
                    if let Some(resolved) = finish_opus_or_article(http, parsed, top_bvid).await? {
                        return Ok(resolved);
                    }
                }
                let fallback_type = number(&opus_resp, "/data/fallback/type");
                let fallback_id = text(&opus_resp, "/data/fallback/id");
                if fallback_type == 2 && !fallback_id.is_empty() {
                    return parse_bilibili_article(http, &fallback_id).await;
                }
            }
            if item_type == "DYNAMIC_TYPE_ARTICLE" {
                let rid = text(item, "/basic/rid_str");
                if !rid.is_empty() && rid.bytes().all(|byte| byte.is_ascii_digit()) {
                    return parse_bilibili_article(http, &rid).await;
                }
            }
        }

        let parsed = map_detail_item(item);
        if parsed.has_deliverable() {
            return Ok(parsed);
        }
        if is_only_fans {
            return Err(BilibiliMediaUnavailable("此动态为充电会员专属，不可访问".into()).into());
        }
        if major_type == "MAJOR_TYPE_NONE" {
            return Err(BilibiliMediaUnavailable("该动态已删除或不可见".into()).into());
        }
        bail!("B站动态未包含可显示的正文、图片或视频")
    })
}

async fn parse_forwarded_dynamic(
    http: &SiteHttp,
    item: &Value,
    depth: usize,
) -> Result<ParsedData> {
    let orig = item
        .get("orig")
        .filter(|value| value.is_object())
        .ok_or_else(|| BilibiliMediaUnavailable("源动态已被删除或不可见".into()))?;
    if orig.pointer("/visible").and_then(Value::as_bool) == Some(false) {
        return Err(BilibiliMediaUnavailable("源动态已被删除或不可见".into()).into());
    }
    let orig_major_type = text(orig, "/modules/module_dynamic/major/type");
    let orig_none_tips = text(orig, "/modules/module_dynamic/major/none/tips");
    if !orig_none_tips.is_empty()
        || (orig_major_type == "MAJOR_TYPE_NONE" && extract_detail_text(orig).trim().is_empty())
    {
        return Err(BilibiliMediaUnavailable("源动态已被删除或不可见".into()).into());
    }
    let orig_parsed = parse_dynamic_item(http, orig, true, depth + 1).await?;
    Ok(merge_forwarded_dynamic(item, orig_parsed))
}

pub(super) fn merge_forwarded_dynamic(item: &Value, orig_parsed: ParsedData) -> ParsedData {
    let outer_desc = extract_detail_text(item).trim().to_owned();
    let orig_author = orig_parsed.author.clone();
    let orig_body = if orig_parsed.desc.trim().is_empty() {
        orig_parsed.title.trim().to_owned()
    } else {
        orig_parsed.desc.trim().to_owned()
    };

    let combined_desc = match (
        !outer_desc.is_empty() && outer_desc != "转发动态",
        !orig_body.is_empty(),
        !orig_author.is_empty(),
    ) {
        (true, true, true) => format!("{outer_desc}\n\n转发 @{orig_author}：\n{orig_body}"),
        (true, true, false) => format!("{outer_desc}\n\n{orig_body}"),
        (true, false, true) => format!("{outer_desc}\n\n转发 @{orig_author}"),
        (true, false, false) => outer_desc,
        (false, true, true) => format!("转发 @{orig_author}：\n{orig_body}"),
        (false, true, false) => orig_body,
        (false, false, true) => format!("转发 @{orig_author}"),
        (false, false, false) => String::new(),
    };

    let mut merged = orig_parsed;
    merged.desc = combined_desc;
    let outer_author = text(item, "/modules/module_author/name");
    if !outer_author.is_empty() {
        merged.author = outer_author;
        merged.uid = text(item, "/modules/module_author/mid");
        let avatar =
            https_media_url(&text(item, "/modules/module_author/face")).unwrap_or_default();
        if !avatar.is_empty() {
            merged.avatar = avatar;
        }
    }
    let outer_pub_ts = number(item, "/modules/module_author/pub_ts");
    if outer_pub_ts > 0 {
        merged.publish_time = outer_pub_ts.saturating_mul(1000);
    }
    let outer_like = number(item, "/modules/module_stat/like/count");
    let outer_comment = number(item, "/modules/module_stat/comment/count");
    let outer_share = number(item, "/modules/module_stat/forward/count");
    let outer_collect = number(item, "/modules/module_stat/favorite/count");
    if outer_like > 0 || outer_comment > 0 || outer_share > 0 {
        merged.like = outer_like;
        merged.comment = outer_comment;
        merged.share = outer_share;
    }
    if outer_collect > 0 {
        merged.collect = outer_collect;
    }
    if !merged.has_deliverable()
        && (!merged.desc.trim().is_empty() || !merged.title.trim().is_empty())
    {
        merged.media_type = "text".into();
    }
    merged
}

fn extract_pgc_id(major: &Value) -> Option<String> {
    let epid = number(major, "/pgc/epid");
    if epid > 0 {
        return Some(format!("ep{epid}"));
    }
    let season_id = number(major, "/pgc/season_id");
    if season_id > 0 {
        return Some(format!("ss{season_id}"));
    }
    let jump_url = text(major, "/pgc/jump_url");
    BANGUMI_PATH_RE
        .captures(&jump_url)
        .and_then(|capture| capture.get(1))
        .map(|found| found.as_str().to_owned())
}

fn extract_detail_text(item: &Value) -> String {
    let opus_summary = text(item, "/modules/module_dynamic/major/opus/summary/text");
    if !opus_summary.trim().is_empty() {
        return opus_summary;
    }
    let desc_text = text(item, "/modules/module_dynamic/desc/text");
    if !desc_text.trim().is_empty() {
        return desc_text;
    }
    let major = item
        .pointer("/modules/module_dynamic/major")
        .unwrap_or(&Value::Null);
    match text(major, "/type").as_str() {
        "MAJOR_TYPE_LIVE" => {
            let live_title = text(major, "/live/title");
            let area = text(major, "/live/desc_first");
            if live_title.is_empty() {
                String::new()
            } else if area.is_empty() {
                format!("直播间：{live_title}")
            } else {
                format!("直播间：{live_title}（{area}）")
            }
        }
        "MAJOR_TYPE_LIVE_RCMD" => {
            let raw_content = text(major, "/live_rcmd/content");
            serde_json::from_str::<Value>(&raw_content)
                .ok()
                .map(|json| {
                    let live_title = text(&json, "/live_play_info/title");
                    let area = text(&json, "/live_play_info/area_name");
                    if live_title.is_empty() {
                        String::new()
                    } else if area.is_empty() {
                        format!("直播：{live_title}")
                    } else {
                        format!("直播：{live_title}（{area}）")
                    }
                })
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

pub(super) fn map_detail_item(item: &Value) -> ParsedData {
    let mut body = ExtractedBody::default();
    if let Some(pics) = item
        .pointer("/modules/module_dynamic/major/opus/pics")
        .and_then(Value::as_array)
    {
        for pic in pics {
            push_pic_entry(pic, &mut body, None);
        }
    }
    if body.images.is_empty()
        && let Some(items) = item
            .pointer("/modules/module_dynamic/major/draw/items")
            .and_then(Value::as_array)
    {
        for pic in items {
            push_pic_entry(pic, &mut body, None);
        }
    }

    let mut parsed = ParsedData::empty("image");
    parsed.title = text(item, "/modules/module_dynamic/major/opus/title");
    parsed.desc = extract_detail_text(item);
    parsed.author = text(item, "/modules/module_author/name");
    parsed.uid = text(item, "/modules/module_author/mid");
    parsed.avatar = https_media_url(&text(item, "/modules/module_author/face")).unwrap_or_default();
    parsed.publish_time = number(item, "/modules/module_author/pub_ts").saturating_mul(1000);
    parsed.like = number(item, "/modules/module_stat/like/count");
    parsed.comment = number(item, "/modules/module_stat/comment/count");
    parsed.share = number(item, "/modules/module_stat/forward/count");
    parsed.collect = number(item, "/modules/module_stat/favorite/count");
    parsed.images = body.images;
    parsed.live_photo = body.live_photo;
    parsed.image_size_map = Some(body.image_size_map);
    finalize_media_type(&mut parsed);
    parsed
}

pub(super) fn map_opus_detail_item(item: &Value) -> (ParsedData, Option<String>) {
    let mut parsed = ParsedData::empty("image");
    let mut body = ExtractedBody::default();
    let mut top_bvid = String::new();

    for module in item
        .get("modules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match text(module, "/module_type").as_str() {
            "MODULE_TYPE_TITLE" => {
                parsed.title = text(module, "/module_title/text");
            }
            "MODULE_TYPE_AUTHOR" => {
                parsed.author = text(module, "/module_author/name");
                parsed.uid = text(module, "/module_author/mid");
                parsed.avatar =
                    https_media_url(&text(module, "/module_author/face")).unwrap_or_default();
                parsed.publish_time = number(module, "/module_author/pub_ts").saturating_mul(1000);
            }
            "MODULE_TYPE_CONTENT" => {
                extract_opus_paragraphs(module.pointer("/module_content/paragraphs"), &mut body);
            }
            "MODULE_TYPE_TOP" => {
                top_bvid = text(module, "/module_top/display/video/bvid");
                if let Some(pics) = module
                    .pointer("/module_top/display/album/pics")
                    .and_then(Value::as_array)
                {
                    for pic in pics {
                        push_pic_entry(pic, &mut body, None);
                    }
                }
            }
            "MODULE_TYPE_STAT" => {
                parsed.like = number(module, "/module_stat/like/count");
                parsed.comment = number(module, "/module_stat/comment/count");
                parsed.share = number(module, "/module_stat/forward/count");
                parsed.collect = number(module, "/module_stat/favorite/count");
            }
            _ => {}
        }
    }

    parsed.desc = body.paragraphs.join("\n");
    parsed.images = body.images;
    parsed.live_photo = body.live_photo;
    parsed.image_size_map = Some(body.image_size_map);
    finalize_media_type(&mut parsed);

    let fallback_video = if parsed.images.is_empty() && !top_bvid.is_empty() {
        Some(top_bvid)
    } else {
        None
    };
    (parsed, fallback_video)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn extracts_dynamic_and_article_ids_and_cache_keys() {
        for url in [
            "https://t.bilibili.com/1271940823012868096",
            "https://t.bilibili.com/h5/dynamic/detail/1271940823012868096",
            "https://www.bilibili.com/opus/1271940823012868096/",
            "https://m.bilibili.com/dynamic/1271940823012868096",
        ] {
            let parsed = Url::parse(url).unwrap();
            assert_eq!(
                extract_dynamic_id(&parsed).as_deref(),
                Some("1271940823012868096")
            );
            assert_eq!(
                extract_bilibili_cache_identity(url).as_deref(),
                Some("bilibili:dynamic:1271940823012868096")
            );
        }

        for url in [
            "https://www.bilibili.com/read/cv53711426",
            "https://www.bilibili.com/read/cv53711426/",
            "https://www.bilibili.com/read/mobile?spm_id_from=333&id=53711426",
        ] {
            let parsed = Url::parse(url).unwrap();
            assert_eq!(extract_article_id(&parsed).as_deref(), Some("53711426"));
            assert_eq!(
                extract_bilibili_cache_identity(url).as_deref(),
                Some("bilibili:article:53711426")
            );
        }
    }

    #[test]
    fn maps_opus_pics_gif_and_live_photo_and_upgrades_http() {
        let item = json!({
            "id_str": "1271940823012868096",
            "type": "DYNAMIC_TYPE_DRAW",
            "modules": {
                "module_author": {"name": "明日方舟", "mid": 161775300, "pub_ts": "1794985200", "face": "http://i0.hdslb.com/bfs/face/a.jpg"},
                "module_stat": {"like": {"count": 9697}, "comment": {"count": 1353}, "forward": {"count": 82}},
                "module_dynamic": {
                    "major": {
                        "type": "MAJOR_TYPE_OPUS",
                        "opus": {
                            "title": "",
                            "summary": {"text": "【新增服饰】\n星空之下"},
                            "pics": [
                                {"url": "http://i0.hdslb.com/bfs/new_dyn/1.jpg", "width": 1920, "height": 1080, "live_url": ""},
                                {"url": "http://i0.hdslb.com/bfs/new_dyn/2.gif", "width": 500, "height": 725, "live_url": ""},
                                {"url": "//i0.hdslb.com/bfs/new_dyn/3.jpg", "width": 1000, "height": 1500, "live_url": "http://video.hdslb.com/bfs/live/3.mp4"}
                            ]
                        }
                    }
                }
            }
        });
        let parsed = map_detail_item(&item);
        assert_eq!(parsed.media_type, "live_photo");
        assert_eq!(
            parsed.images,
            vec![
                "https://i0.hdslb.com/bfs/new_dyn/1.jpg",
                "https://i0.hdslb.com/bfs/new_dyn/2.gif",
                "https://i0.hdslb.com/bfs/new_dyn/3.jpg",
            ]
        );
        assert_eq!(parsed.live_photo.len(), 1);
        assert_eq!(
            parsed.live_photo[0].image,
            "https://i0.hdslb.com/bfs/new_dyn/3.jpg"
        );
        assert_eq!(
            parsed.live_photo[0].video,
            "https://video.hdslb.com/bfs/live/3.mp4"
        );
        assert_eq!(parsed.like, 9697);
        assert_eq!(parsed.comment, 1353);
        assert_eq!(parsed.share, 82);
        assert_eq!(
            parsed.image_size_map.as_ref().unwrap()["https://i0.hdslb.com/bfs/new_dyn/1.jpg"],
            ImageSize {
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn maps_text_only_dynamic_to_markdown_text_post() {
        let item = json!({
            "id_str": "1270724202670850065",
            "type": "DYNAMIC_TYPE_WORD",
            "modules": {
                "module_author": {"name": "陈睿", "mid": 208259, "pub_ts": "1794701934"},
                "module_stat": {"like": {"count": 38000}, "comment": {"count": 4447}, "forward": {"count": 473}},
                "module_dynamic": {
                    "desc": {"text": "就在今天！\n我们是冠军！！！"},
                    "major": null
                }
            }
        });
        let parsed = map_detail_item(&item);
        assert_eq!(parsed.media_type, "text");
        assert!(parsed.has_deliverable());
        assert!(!parsed.has_media());
        assert_eq!(parsed.desc, "就在今天！\n我们是冠军！！！");
        assert_eq!(parsed.author, "陈睿");
    }

    #[test]
    fn maps_opus_detail_paragraphs_and_article_top_video() {
        let opus = json!({
            "modules": [
                {"module_type": "MODULE_TYPE_TITLE", "module_title": {"text": "长文标题"}},
                {"module_type": "MODULE_TYPE_AUTHOR", "module_author": {"name": "碧诗", "mid": 2, "pub_ts": "1793548961"}},
                {"module_type": "MODULE_TYPE_TOP", "module_top": {"display": {"type": 2, "video": {"bvid": "BV1JPx16tEWG"}}}},
                {"module_type": "MODULE_TYPE_CONTENT", "module_content": {"paragraphs": [
                    {"para_type": 2, "pic": {"pics": [{"url": "http://i0.hdslb.com/bfs/note/1.png", "width": 1282, "height": 1858, "live_url": ""}]}},
                    {"para_type": 1, "text": {"nodes": [{"word": {"words": "第一段正文"}}]}},
                    {"para_type": 2, "pic": {"pics": [{"url": "https://i0.hdslb.com/bfs/note/2.png", "width": 1253, "height": 456, "live_url": ""}]}}
                ]}},
                {"module_type": "MODULE_TYPE_STAT", "module_stat": {"like": {"count": 3160}, "comment": {"count": 246}, "forward": {"count": 17}, "favorite": {"count": 290}}}
            ]
        });
        let (parsed, fallback_bvid) = map_opus_detail_item(&opus);
        assert_eq!(parsed.media_type, "image");
        assert!(fallback_bvid.is_none());
        assert_eq!(parsed.title, "长文标题");
        assert_eq!(parsed.desc, "第一段正文");
        assert_eq!(parsed.images.len(), 2);
        assert_eq!(parsed.collect, 290);

        let video_only_article = json!({
            "title": "纯视频顶部卡片专栏",
            "summary": "摘要",
            "author": {"name": "作者", "mid": 1},
            "banner_url": "http://i1.hdslb.com/bfs/archive/video_cover.jpg",
            "origin_image_urls": ["https://i1.hdslb.com/bfs/archive/video_cover.jpg"],
            "top_video_info": {"bvid": "BV1JPx16tEWG", "pic": "http://i1.hdslb.com/bfs/archive/video_cover.jpg"},
            "opus": {"content": {"paragraphs": [{"para_type": 1, "text": {"nodes": [{"word": {"words": "只有一段话"}}]}}]}}
        });
        let (parsed_art, video_bvid) = map_article_payload(&video_only_article).unwrap();
        assert!(parsed_art.images.is_empty());
        assert_eq!(video_bvid.as_deref(), Some("BV1JPx16tEWG"));
    }

    #[test]
    fn merges_forwarded_dynamic_preserving_media_and_combining_text() {
        let outer = json!({
            "id_str": "1270796658725093378",
            "type": "DYNAMIC_TYPE_FORWARD",
            "modules": {
                "module_author": {"name": "转发者", "mid": 100, "pub_ts": "1795000000"},
                "module_stat": {"like": {"count": 18}, "comment": {"count": 5}, "forward": {"count": 2}},
                "module_dynamic": {"desc": {"text": "恭喜中奖"}}
            }
        });
        let mut orig = ParsedData::empty("image");
        orig.author = "原作者".into();
        orig.desc = "原动态正文".into();
        orig.images = vec!["https://i0.hdslb.com/bfs/new_dyn/orig.jpg".into()];

        let merged = merge_forwarded_dynamic(&outer, orig);
        assert_eq!(merged.author, "转发者");
        assert_eq!(merged.desc, "恭喜中奖\n\n转发 @原作者：\n原动态正文");
        assert_eq!(
            merged.images,
            vec!["https://i0.hdslb.com/bfs/new_dyn/orig.jpg"]
        );
        assert_eq!(merged.like, 18);
    }

    #[test]
    fn detects_pgc_jump_id() {
        assert_eq!(
            extract_pgc_id(&json!({"pgc": {"epid": 812345, "season_id": 456}})).as_deref(),
            Some("ep812345")
        );
        assert_eq!(
            extract_pgc_id(
                &json!({"pgc": {"jump_url": "https://www.bilibili.com/bangumi/play/ss44123"}})
            )
            .as_deref(),
            Some("ss44123")
        );
    }

    #[test]
    fn classifies_unavailable_blocked_and_deleted_responses() {
        let not_found = check_bili_response(&json!({"code": 4101131, "message": "denied"}), "动态")
            .unwrap_err();
        assert!(not_found.is::<BilibiliMediaUnavailable>());

        let bad_req =
            check_bili_response(&json!({"code": -400, "message": "请求错误"}), "动态").unwrap_err();
        assert!(bad_req.is::<BilibiliMediaUnavailable>());

        let voucher =
            check_bili_response(&json!({"code": 0, "data": {"v_voucher": "abc"}}), "动态")
                .unwrap_err();
        assert!(voucher.to_string().contains("风控验证"));

        let legacy_html = json!({
            "title": "旧版图文专栏",
            "summary": "摘要内容",
            "author": {"name": "作者甲", "mid": 99},
            "content": "<p>正文段落</p><img data-src=\"http://i0.hdslb.com/bfs/article/a.jpg\" />",
            "stats": {"like": 5, "reply": 2, "favorite": 1, "share": 0, "view": 100}
        });
        let (parsed, top_bvid) = map_article_payload(&legacy_html).unwrap();
        assert!(top_bvid.is_none());
        assert_eq!(parsed.media_type, "image");
        assert_eq!(
            parsed.images,
            vec!["https://i0.hdslb.com/bfs/article/a.jpg"]
        );
        assert_eq!(parsed.desc, "正文段落");
    }

    #[tokio::test]
    async fn dynamic_errors_classify_blocked_charging_and_deleted_orig() {
        let http = SiteHttp::new(&crate::types::AppConfig::default()).unwrap();

        let charging = json!({
            "id_str": "123",
            "type": "DYNAMIC_TYPE_DRAW",
            "basic": {"is_only_fans": true},
            "modules": {"module_dynamic": {"major": {"type": "MAJOR_TYPE_BLOCKED", "blocked": {"hint_message": "充电专属"}}}}
        });
        let err = parse_dynamic_item(&http, &charging, false, 0)
            .await
            .unwrap_err();
        assert!(err.is::<BilibiliMediaUnavailable>());
        assert_eq!(err.to_string(), "此动态为充电会员专属，不可访问");

        let deleted_orig = json!({
            "id_str": "456",
            "type": "DYNAMIC_TYPE_FORWARD",
            "modules": {"module_dynamic": {"desc": {"text": "转发"}}},
            "orig": {
                "type": "DYNAMIC_TYPE_NONE",
                "modules": {"module_dynamic": {"major": {"type": "MAJOR_TYPE_NONE", "none": {"tips": "源动态已被删除"}}}}
            }
        });
        let err = parse_dynamic_item(&http, &deleted_orig, false, 0)
            .await
            .unwrap_err();
        assert!(err.is::<BilibiliMediaUnavailable>());
        assert_eq!(err.to_string(), "源动态已被删除或不可见");
    }
}
