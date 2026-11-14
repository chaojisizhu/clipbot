//! Native adapters checked against parse-video-py and media-parser site protocols.

use anyhow::{Context, Result, bail, ensure};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, REFERER, USER_AGENT};
use serde_json::{Value, json};
use url::Url;

use crate::parser::http::{MOBILE_USER_AGENT, SiteHttp};
use crate::types::{LivePhoto, ParsedData, VideoQuality};

static SCRIPTS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<script\b[^>]*>(.*?)</script\s*>").unwrap());

pub async fn parse(http: &SiteHttp, url: &str, platform: &str) -> Result<ParsedData> {
    let (query_keys, referer): (&[&str], &str) = match platform {
        "pipixia" => (&["item_id", "cell_id"], "https://h5.pipix.com/"),
        "zuiyou" => (&["pid"], "https://share.xiaochuankeji.cn/"),
        "weishi" => (&["id", "feedid"], "https://isee.weishi.qq.com/"),
        "haokan" => (&["vid"], "https://haokan.baidu.com/v"),
        _ => bail!("平台 {platform} 没有附加原生解析器"),
    };
    let resolved = if extract_id(url, query_keys, platform).is_none() {
        http.resolve_url(url, platform).await?
    } else {
        url.to_owned()
    };
    let id = extract_id(&resolved, query_keys, platform)
        .with_context(|| format!("{platform} 分享链接缺少作品ID"))?;
    let mut parsed = match platform {
        "pipixia" => parse_pipixia(http, &resolved, &id).await?,
        "zuiyou" => parse_zuiyou(http, &resolved, &id).await?,
        "weishi" => parse_weishi(http, &resolved, &id).await?,
        "haokan" => parse_haokan(http, &resolved, &id).await?,
        _ => unreachable!(),
    };
    ensure!(
        parsed.has_media(),
        "{platform} 原站未提供目标作品的可访问媒体，请检查登录权限或链接有效性"
    );
    parsed
        .media_headers
        .insert("Referer".into(), referer.into());
    if parsed.video.is_empty() {
        parsed.media_type = if parsed.live_photo.is_empty() {
            "image"
        } else {
            "live_photo"
        }
        .into();
    }
    Ok(parsed)
}

fn extract_id(raw: &str, keys: &[&str], platform: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    if let Some(id) = url
        .query_pairs()
        .find(|(key, value)| keys.contains(&key.as_ref()) && !value.is_empty())
        .map(|(_, value)| value.into_owned())
    {
        let valid = id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        return valid.then_some(id);
    }
    if !matches!(platform, "pipixia" | "zuiyou") {
        return None;
    }
    let segments: Vec<_> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    let id = *segments.last()?;
    let numeric = id.bytes().all(|byte| byte.is_ascii_digit());
    let is_detail = segments
        .iter()
        .any(|segment| matches!(*segment, "item" | "detail" | "post"));
    (numeric && !id.is_empty() && is_detail).then(|| id.to_owned())
}

fn headers(referer: &str, mobile: bool) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(REFERER, HeaderValue::from_str(referer)?);
    if mobile {
        headers.insert(USER_AGENT, HeaderValue::from_static(MOBILE_USER_AGENT));
    }
    Ok(headers)
}

fn text(value: &Value, pointers: &[&str]) -> String {
    pointers
        .iter()
        .find_map(|pointer| match value.pointer(pointer) {
            Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
            Some(Value::Number(number)) => Some(number.to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

fn number(value: &Value, pointers: &[&str]) -> i64 {
    pointers
        .iter()
        .find_map(|pointer| {
            let value = value.pointer(pointer)?;
            value.as_i64().or_else(|| value.as_str()?.parse().ok())
        })
        .unwrap_or(0)
}

fn seconds(value: &Value, pointer: &str) -> u64 {
    value
        .pointer(pointer)
        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        .unwrap_or(0.0)
        .round()
        .max(0.0) as u64
}

fn media_url(raw: &str) -> Option<String> {
    let raw = html_escape::decode_html_entities(raw).replace(r"\/", "/");
    let raw = if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw
    };
    let url = Url::parse(&raw).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then_some(raw)
}

fn url_at(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .find_map(|pointer| media_url(&text(value, &[*pointer])))
}

fn verify_id(value: &Value, id: &str, pointers: &[&str], platform: &str) -> Result<()> {
    let actual = text(value, pointers);
    ensure!(
        !actual.is_empty() && actual == id,
        "{platform} 原站返回的作品ID与分享链接不一致"
    );
    Ok(())
}

fn first_json(source: &str) -> Option<Value> {
    let value = serde_json::Deserializer::from_str(source.trim())
        .into_iter::<Value>()
        .next()?
        .ok()?;
    match value {
        Value::String(inner) => serde_json::from_str(&inner).ok(),
        other => Some(other),
    }
}

fn page_states(html: &str, markers: &[&str]) -> Vec<Value> {
    let mut states = Vec::new();
    for script in SCRIPTS
        .captures_iter(html)
        .filter_map(|capture| capture.get(1))
    {
        let source = script.as_str().trim();
        for marker in markers {
            if let Some(start) = source.find(marker) {
                let tail = source[start + marker.len()..].trim_start();
                if let Some(value) = tail.strip_prefix('=').and_then(first_json) {
                    states.push(value);
                }
            }
        }
        let decoded = urlencoding::decode(source).unwrap_or_else(|_| source.into());
        if let Some(value) = first_json(&decoded) {
            states.push(value);
        }
    }
    states
}

fn set_video(parsed: &mut ParsedData, url: String, label: &str) {
    parsed.video = url.clone();
    parsed.videos.push(VideoQuality {
        quality: label.into(),
        url,
        bit_rate: None,
    });
}

async fn parse_pipixia(http: &SiteHttp, url: &str, id: &str) -> Result<ParsedData> {
    let html = http
        .get_text_headers(url, "pipixia", headers("https://h5.pipix.com/", false)?)
        .await?;
    for state in page_states(&html, &["window.__INITIAL_STATE__"]) {
        if let Some(item) = state.pointer("/ppxItemDetail/item") {
            return map_pipixia(item, id);
        }
    }
    let api = format!(
        "https://api.pipix.com/bds/cell/cell_comment/?offset=0&cell_type=1&api_version=1&cell_id={id}&ac=wifi&channel=huawei_1319_64&aid=1319&app_name=super"
    );
    let response = http
        .get_json_headers(&api, "pipixia", headers("https://h5.pipix.com/", false)?)
        .await?;
    ensure!(
        number(&response, &["/status_code"]) == 0,
        "皮皮虾原站拒绝访问，{}",
        text(&response, &["/prompt", "/message"])
    );
    let item = response
        .pointer("/data/cell_comments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|comment| comment.pointer("/comment_info/item"))
        .find(|item| text(item, &["/item_id_str", "/item_id"]) == id)
        .context("皮皮虾原站未返回目标作品，可能需要登录权限或有效分享链接")?;
    map_pipixia(item, id)
}

fn map_pipixia(item: &Value, id: &str) -> Result<ParsedData> {
    verify_id(item, id, &["/item_id_str", "/item_id"], "皮皮虾")?;
    let mut parsed = ParsedData::empty("video");
    parsed.title = text(item, &["/content", "/video/title"]);
    parsed.desc = parsed.title.clone();
    parsed.author = text(item, &["/author/name"]);
    parsed.uid = text(item, &["/author/id_str", "/author/id"]);
    parsed.avatar = url_at(
        item,
        &[
            "/author/avatar/download_list/0/url",
            "/author/avatar/url_list/0/url",
        ],
    )
    .unwrap_or_default();
    parsed.cover = url_at(
        item,
        &["/cover/url_list/0/url", "/video/cover_image/url_list/0/url"],
    )
    .unwrap_or_default();
    parsed.duration = seconds(item, "/video/duration");
    parsed.publish_time = number(item, &["/create_time"]).saturating_mul(1000);
    parsed.like = number(item, &["/stats/like_count"]);
    parsed.comment = number(item, &["/stats/comment_count"]);
    parsed.share = number(item, &["/stats/share_count"]);
    parsed.play = number(item, &["/stats/play_count", "/stats/view_count"]);
    if let Some(url) = url_at(
        item,
        &[
            "/video/video_download/url_list/0/url",
            "/video/video_high/url_list/0/url",
            "/video/video_fallback/url_list/0/url",
            "/video/video_mid/url_list/0/url",
            "/video/video_low/url_list/0/url",
        ],
    ) {
        set_video(&mut parsed, url, "原站");
    }
    for image in item
        .pointer("/note/multi_image")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(url) = url_at(image, &["/url_list/0/url"]) {
            parsed.images.push(url);
        }
    }
    ensure!(parsed.has_media(), "皮皮虾目标作品没有可访问的视频或图集");
    Ok(parsed)
}

async fn parse_zuiyou(http: &SiteHttp, input: &str, id: &str) -> Result<ParsedData> {
    let pid: u64 = id.parse().context("最右作品ID必须为数字")?;
    let response = http
        .post_json(
            "https://share.xiaochuankeji.cn/planck/share/post/detail_h5",
            "zuiyou",
            &json!({"h_av":"5.2.13.011", "pid":pid}),
        )
        .await?;
    ensure!(
        number(&response, &["/ret"]) == 1,
        "最右原站拒绝访问，{}",
        text(&response, &["/msg", "/message"])
    );
    let post = response
        .pointer("/data/post")
        .context("最右原站响应缺少目标帖子")?;
    let requested_video = Url::parse(input)?
        .query_pairs()
        .find(|(key, _)| key == "vid")
        .map(|(_, value)| value.into_owned());
    map_zuiyou(post, id, requested_video.as_deref())
}

fn zuiyou_image(image: &Value) -> Option<String> {
    url_at(
        image,
        &[
            "/urls/origin/urls/0",
            "/urls/origin_webp/urls/0",
            "/urls/540/urls/0",
            "/url",
        ],
    )
}

fn map_zuiyou(post: &Value, id: &str, requested_video: Option<&str>) -> Result<ParsedData> {
    verify_id(post, id, &["/id", "/pid"], "最右")?;
    let mut parsed = ParsedData::empty("video");
    parsed.title = text(post, &["/content"]);
    parsed.desc = parsed.title.clone();
    parsed.author = text(post, &["/member/name"]);
    parsed.uid = text(post, &["/member/id"]);
    parsed.avatar = url_at(post, &["/member/avatar_urls/origin/urls/0"]).unwrap_or_default();
    parsed.publish_time = number(post, &["/ct"]).saturating_mul(1000);
    parsed.like = number(post, &["/likes", "/up"]);
    parsed.comment = number(post, &["/reviews"]);
    parsed.share = number(post, &["/share"]);
    let images = post
        .get("imgs")
        .and_then(Value::as_array)
        .context("最右帖子缺少媒体列表")?;
    let videos = post.get("videos").and_then(Value::as_object);
    let explicit = requested_video
        .map(|requested| {
            videos
                .and_then(|videos| videos.get(requested))
                .context("最右分享链接指定的视频不属于当前帖子")
        })
        .transpose()?;
    if let Some(video) = explicit {
        let url = url_at(video, &["/url"]).context("最右指定视频缺少播放地址")?;
        set_video(&mut parsed, url, "原站");
        parsed.duration = seconds(video, "/dur");
    }
    for image in images {
        let cover = zuiyou_image(image);
        if parsed.cover.is_empty() {
            parsed.cover = cover.clone().unwrap_or_default();
        }
        let media_id = text(image, &["/id"]);
        let motion = videos.and_then(|videos| videos.get(&media_id));
        if let Some(video) = motion {
            if explicit.is_some() {
                continue;
            }
            let url = url_at(video, &["/url"]);
            if images.len() == 1 {
                if let Some(url) = url {
                    set_video(&mut parsed, url, "原站");
                    parsed.duration = seconds(video, "/dur");
                }
            } else if let (Some(image), Some(video)) = (cover, url) {
                parsed.images.push(image.clone());
                parsed.live_photo.push(LivePhoto { image, video });
            }
        } else if number(image, &["/video"]) == 0
            && let Some(url) = cover
        {
            parsed.images.push(url);
        }
    }
    ensure!(
        parsed.has_media(),
        "最右目标帖子没有可访问的媒体，视频封面无法替代播放内容"
    );
    Ok(parsed)
}

async fn parse_weishi(http: &SiteHttp, url: &str, id: &str) -> Result<ParsedData> {
    let html = http
        .get_text_headers(
            url,
            "weishi",
            headers("https://isee.weishi.qq.com/", false)?,
        )
        .await?;
    for state in page_states(&html, &["window.Vise.initState"]) {
        if let Some(feed) = state
            .get("feedsList")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|feed| text(feed, &["/id", "/feedid", "/feedId"]) == id)
        {
            return map_weishi(feed, id);
        }
    }
    // The current official share-page client uses the TRPC envelope for this method.
    // H5GetPlayPageNew returns recommendations and cannot substitute for this lookup.
    let api =
        "https://api.weishi.qq.com/trpc.weishi.weishi_h5_proxy.weishi_h5_proxy/WSH5GetPlayPage";
    let response = http
        .post_json(
            api,
            "weishi",
            &json!({
                "req_body": {
                    "feedid": id,
                    "recommendtype": 0,
                    "datalvl": "all",
                    "_weishi_mapExt": {}
                },
                "req_header": {"channelId": 4, "authInfo": {}}
            }),
        )
        .await
        .context("微视分享页仅提供客户端渲染，原站详情查询失败，请补充登录Cookie或完整分享链接")?;
    map_weishi_response(&response, id)
}

fn map_weishi_response(response: &Value, id: &str) -> Result<ParsedData> {
    ensure!(
        response.pointer("/rsp_header/ret").is_some()
            && number(response, &["/rsp_header/ret"]) == 0
            && text(response, &["/rsp_body/errmsg"]).is_empty(),
        "微视原站需要访问验证或登录权限，{}",
        text(response, &["/rsp_header/errMsg", "/rsp_body/errmsg"])
    );
    let feed = response
        .pointer("/rsp_body/feeds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|feed| text(feed, &["/id", "/feedid", "/feedId"]) == id)
        .context("微视原站未返回目标视频，请补充登录Cookie或完整分享链接")?;
    map_weishi(feed, id)
}

fn map_weishi(feed: &Value, id: &str) -> Result<ParsedData> {
    verify_id(feed, id, &["/id", "/feedid", "/feedId"], "微视")?;
    let mut parsed = ParsedData::empty("video");
    let url = url_at(feed, &["/videoUrl", "/video_url"]).context("微视目标作品缺少可访问视频流")?;
    set_video(&mut parsed, url, "原站");
    parsed.title = text(
        feed,
        &[
            "/feedDescWithat",
            "/feed_desc_withat",
            "/feedDesc",
            "/feed_desc",
        ],
    );
    parsed.desc = parsed.title.clone();
    parsed.cover =
        url_at(feed, &["/videoCover", "/video_cover", "/images/0/url"]).unwrap_or_default();
    parsed.author = text(feed, &["/poster/nick"]);
    parsed.uid = text(feed, &["/poster/id", "/posterId", "/poster_id"]);
    parsed.avatar = url_at(feed, &["/poster/avatar"]).unwrap_or_default();
    parsed.duration = (number(feed, &["/video/duration"]) as f64 / 1000.0)
        .round()
        .max(0.0) as u64;
    parsed.like = number(feed, &["/dingCount", "/ding_count"]);
    parsed.comment = number(feed, &["/totalCommentNum", "/total_comment_num"]);
    parsed.collect = number(feed, &["/totalFavorNum", "/total_favor_num"]);
    parsed.share = number(feed, &["/totalShareNum", "/total_share_num"]);
    parsed.play = number(feed, &["/playNum", "/play_num"]);
    parsed.publish_time =
        number(feed, &["/createTime", "/create_time", "/createtime"]).saturating_mul(1000);
    Ok(parsed)
}

async fn parse_haokan(http: &SiteHttp, url: &str, id: &str) -> Result<ParsedData> {
    let api = format!("https://haokan.baidu.com/v?_format=json&vid={id}");
    let source = http
        .get_text_headers(&api, "haokan", headers("https://haokan.baidu.com/v", true)?)
        .await?;
    if let Ok(response) = serde_json::from_str::<Value>(&source) {
        ensure!(
            number(&response, &["/errno"]) == 0,
            "好看视频原站拒绝访问，{}",
            text(&response, &["/error"])
        );
        if let Some(meta) = response.pointer("/data/apiData/curVideoMeta") {
            return map_haokan(meta, id);
        }
    }
    for state in page_states(&source, &["window.jsonData", "window.__PRELOADED_STATE__"]) {
        if let Some(meta) = haokan_meta(&state, id) {
            return map_haokan(meta, id);
        }
    }
    let html = http
        .get_text_headers(url, "haokan", headers("https://haokan.baidu.com/v", true)?)
        .await?;
    for state in page_states(&html, &["window.jsonData", "window.__PRELOADED_STATE__"]) {
        if let Some(meta) = haokan_meta(&state, id) {
            return map_haokan(meta, id);
        }
    }
    bail!("好看视频页面未返回目标作品媒体，请检查分享链接及登录权限")
}

fn haokan_meta<'a>(state: &'a Value, id: &str) -> Option<&'a Value> {
    [
        "/data/apiData/curVideoMeta",
        "/curVideoMeta",
        "/data/curVideoMeta",
        "/data/videoInfo",
        "/videoInfo",
        "/data/response",
    ]
    .into_iter()
    .filter_map(|pointer| state.pointer(pointer))
    .find(|meta| text(meta, &["/id", "/vid", "/video_id"]) == id)
}

fn map_haokan(meta: &Value, id: &str) -> Result<ParsedData> {
    verify_id(meta, id, &["/id", "/vid", "/video_id"], "好看视频")?;
    let mut parsed = ParsedData::empty("video");
    parsed.title = text(meta, &["/title", "/seo_title"]);
    parsed.desc = text(meta, &["/description"]);
    parsed.cover = url_at(meta, &["/poster", "/posterImage", "/cover"]).unwrap_or_default();
    parsed.author = text(meta, &["/mth/author_name", "/author_name"]);
    parsed.uid = text(meta, &["/mth/mthid", "/author_id"]);
    parsed.avatar = url_at(meta, &["/mth/author_photo", "/avatar"]).unwrap_or_default();
    parsed.duration = seconds(meta, "/duration");
    parsed.publish_time = number(meta, &["/publish_time"]).saturating_mul(1000);
    parsed.play = number(meta, &["/playcnt"]);
    parsed.like = number(meta, &["/like_num"]);
    let mut qualities: Vec<(i64, VideoQuality)> = Vec::new();
    for entry in meta
        .get("clarityUrl")
        .or_else(|| meta.get("clarityArr"))
        .or_else(|| meta.get("clarity_arr"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(url) = url_at(entry, &["/url", "/play_url"]) {
            qualities.push((
                number(entry, &["/rank"]),
                VideoQuality {
                    quality: text(entry, &["/key", "/title", "/name"]),
                    url,
                    bit_rate: None,
                },
            ));
        }
    }
    qualities.sort_by_key(|(rank, _)| std::cmp::Reverse(*rank));
    parsed.videos = qualities.into_iter().map(|(_, video)| video).collect();
    parsed.video = parsed
        .videos
        .first()
        .map(|video| video.url.clone())
        .or_else(|| url_at(meta, &["/playurl", "/play_url", "/html5_playurl", "/url"]))
        .context("好看视频目标作品没有可访问视频地址")?;
    if parsed.videos.is_empty() {
        let url = parsed.video.clone();
        set_video(&mut parsed, url, "原站");
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoded_pipixia_state_maps_only_requested_item() {
        let item = json!({"item_id_str":"7641086036809881866", "content":"作品", "video":{"duration":12.4,"video_high":{"url_list":[{"url":"https://media.example/video.mp4"}]}}, "cover":{"url_list":[{"url":"https://image.example/cover.jpg"}]}});
        let encoded =
            urlencoding::encode(&json!({"ppxItemDetail":{"item":item}}).to_string()).into_owned();
        let html = format!("<script type=\"application/json\">{encoded}</script>");
        let states = page_states(&html, &[]);
        let item = &states[0]["ppxItemDetail"]["item"];
        let parsed = map_pipixia(item, "7641086036809881866").unwrap();
        assert_eq!(parsed.video, "https://media.example/video.mp4");
        assert_eq!(parsed.duration, 12);
        assert!(map_pipixia(item, "1").is_err());
        assert!(map_pipixia(&json!({"item_id":"1","cover":{"url_list":[{"url":"https://image.example/cover.jpg"}]}}), "1").is_err());
    }

    #[test]
    fn zuiyou_uses_requested_video_and_preserves_motion_gallery() {
        let post = json!({"id":423835942,"content":"帖子","imgs":[{"id":2542343457u64,"video":1,"urls":{"origin":{"urls":["https://image.example/cover.jpg"]}}}],"videos":{"2542343457":{"dur":15,"url":"https://media.example/video.mp4"}}});
        let parsed = map_zuiyou(&post, "423835942", Some("2542343457")).unwrap();
        assert_eq!(parsed.video, "https://media.example/video.mp4");
        assert!(parsed.images.is_empty());
        assert!(map_zuiyou(&post, "423835942", Some("5")).is_err());
        assert!(map_zuiyou(&post, "1", None).is_err());
        let gallery = json!({"id":1,"imgs":[{"id":10,"video":1,"urls":{"origin":{"urls":["https://image.example/a.jpg"]}}},{"id":11,"video":0,"url":"https://image.example/b.jpg"}],"videos":{"10":{"dur":2,"url":"https://media.example/live.mp4"}}});
        let parsed = map_zuiyou(&gallery, "1", None).unwrap();
        assert_eq!(parsed.images.len(), 2);
        assert_eq!(parsed.live_photo.len(), 1);
        assert!(parsed.video.is_empty());
    }

    #[test]
    fn weishi_uses_exact_feed_and_never_cover_as_media() {
        let html = r#"<script>window.Vise.initState = {"feedsList":[{"id":"target","videoUrl":"https://media.example/video.mp4","video":{"duration":12000},"videoCover":"https://image.example/cover.jpg","feedDesc":"正文","poster":{"id":"author","nick":"作者"}}]};other()</script>"#;
        let states = page_states(html, &["window.Vise.initState"]);
        let feed = &states[0]["feedsList"][0];
        let parsed = map_weishi(feed, "target").unwrap();
        assert_eq!(parsed.uid, "author");
        assert_eq!(parsed.title, "正文");
        assert_eq!(parsed.duration, 12);
        assert!(map_weishi(feed, "other").is_err());
        assert!(
            map_weishi(
                &json!({"id":"target","videoCover":"https://image.example/cover.jpg"}),
                "target"
            )
            .is_err()
        );
    }

    #[test]
    fn weishi_trpc_maps_target_and_ignores_recommendations() {
        let response = json!({
            "rsp_header": {"ret": 0, "errMsg": ""},
            "rsp_body": {
                "errmsg": "",
                "feeds": [{
                    "id": "target",
                    "video_url": "https://media.example/target.mp4",
                    "video_cover": "https://image.example/target.jpg",
                    "feed_desc_withat": "目标作品",
                    "video": {"duration": 10751},
                    "createtime": 1773103561
                }],
                "recommendfeeds": [{"id": "other", "video_url": "https://media.example/other.mp4"}]
            }
        });
        let parsed = map_weishi_response(&response, "target").unwrap();
        assert_eq!(parsed.video, "https://media.example/target.mp4");
        assert_eq!(parsed.duration, 11);
        assert_eq!(parsed.publish_time, 1773103561000);
        assert_eq!(parsed.cover, "https://image.example/target.jpg");
        assert!(map_weishi_response(&response, "other").is_err());
        let mut recommendations_only = response;
        recommendations_only["rsp_body"]["feeds"] = json!([]);
        assert!(map_weishi_response(&recommendations_only, "other").is_err());
        assert!(map_weishi_response(&json!({"rsp_body":{"feeds":[]}}), "target").is_err());
    }

    #[test]
    fn haokan_matches_id_and_selects_best_rank() {
        let meta = json!({"id":"17831460188721240800","duration":245,"title":"视频","clarityUrl":[{"key":"sd","rank":0,"url":"https://media.example/sd.mp4"},{"key":"hd","rank":1,"url":"https://media.example/hd.mp4"}],"mth":{"mthid":"123","author_name":"作者"}});
        let parsed = map_haokan(&meta, "17831460188721240800").unwrap();
        assert_eq!(parsed.video, "https://media.example/hd.mp4");
        assert_eq!(parsed.duration, 245);
        assert_eq!(parsed.author, "作者");
        assert!(map_haokan(&meta, "1").is_err());
        assert!(
            map_haokan(
                &json!({"id":"1","poster":"https://image.example/p.jpg"}),
                "1"
            )
            .is_err()
        );
    }

    #[test]
    fn extract_ids_without_truncating_tokens() {
        assert_eq!(
            extract_id(
                "https://h5.pipix.com/ppx/item/7641086036809881866?app_id=1319",
                &[],
                "pipixia"
            )
            .as_deref(),
            Some("7641086036809881866")
        );
        assert_eq!(
            extract_id(
                "https://isee.weishi.qq.com/ws/app-pages/share/index.html?id=7BHJE5R6Z1VZLexao",
                &["id"],
                "weishi"
            )
            .as_deref(),
            Some("7BHJE5R6Z1VZLexao")
        );
        assert!(extract_id("https://h5.pipix.com/s/lggjR-ynbpI/", &[], "pipixia").is_none());
    }
}
