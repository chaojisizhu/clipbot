use std::time::Duration;

use rand::RngCore;
use serde_json::{Value, json};
use tracing::debug;
use url::Url;

use crate::types::{ChannelsConfig, ParsedData, VideoQuality};

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const ENCRYPT_HEAD_LEN: usize = 131_072;

struct IsaacCtx {
    seed: [u64; 256],
    mm: [u64; 256],
    aa: u64,
    bb: u64,
    cc: u64,
    rand_cnt: usize,
}

impl IsaacCtx {
    fn new(enc_key: u64) -> Self {
        let golden = 0x9e37_79b9_7f4a_7c13u64;
        let mut seed = [0u64; 256];
        seed[0] = enc_key;
        let mut mm = [0u64; 256];
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            golden, golden, golden, golden, golden, golden, golden, golden,
        );

        macro_rules! mix {
            () => {{
                a = a.wrapping_sub(e);
                f ^= h >> 9;
                h = h.wrapping_add(a);
                b = b.wrapping_sub(f);
                g ^= a << 9;
                a = a.wrapping_add(b);
                c = c.wrapping_sub(g);
                h ^= b >> 23;
                b = b.wrapping_add(c);
                d = d.wrapping_sub(h);
                a ^= c << 15;
                c = c.wrapping_add(d);
                e = e.wrapping_sub(a);
                b ^= d >> 14;
                d = d.wrapping_add(e);
                f = f.wrapping_sub(b);
                c ^= e << 20;
                e = e.wrapping_add(f);
                g = g.wrapping_sub(c);
                d ^= f >> 17;
                f = f.wrapping_add(g);
                h = h.wrapping_sub(d);
                e ^= g << 14;
                g = g.wrapping_add(h);
            }};
        }

        for _ in 0..4 {
            mix!();
        }
        for i in (0..256).step_by(8) {
            a = a.wrapping_add(seed[i]);
            b = b.wrapping_add(seed[i + 1]);
            c = c.wrapping_add(seed[i + 2]);
            d = d.wrapping_add(seed[i + 3]);
            e = e.wrapping_add(seed[i + 4]);
            f = f.wrapping_add(seed[i + 5]);
            g = g.wrapping_add(seed[i + 6]);
            h = h.wrapping_add(seed[i + 7]);
            mix!();
            mm[i] = a;
            mm[i + 1] = b;
            mm[i + 2] = c;
            mm[i + 3] = d;
            mm[i + 4] = e;
            mm[i + 5] = f;
            mm[i + 6] = g;
            mm[i + 7] = h;
        }
        for i in (0..256).step_by(8) {
            a = a.wrapping_add(mm[i]);
            b = b.wrapping_add(mm[i + 1]);
            c = c.wrapping_add(mm[i + 2]);
            d = d.wrapping_add(mm[i + 3]);
            e = e.wrapping_add(mm[i + 4]);
            f = f.wrapping_add(mm[i + 5]);
            g = g.wrapping_add(mm[i + 6]);
            h = h.wrapping_add(mm[i + 7]);
            mix!();
            mm[i] = a;
            mm[i + 1] = b;
            mm[i + 2] = c;
            mm[i + 3] = d;
            mm[i + 4] = e;
            mm[i + 5] = f;
            mm[i + 6] = g;
            mm[i + 7] = h;
        }

        let mut ctx = Self {
            seed,
            mm,
            aa: 0,
            bb: 0,
            cc: 0,
            rand_cnt: 255,
        };
        ctx.isaac64();
        ctx
    }

    fn isaac64(&mut self) {
        self.cc = self.cc.wrapping_add(1);
        self.bb = self.bb.wrapping_add(self.cc);
        for i in 0..256 {
            match i % 4 {
                0 => self.aa = !(self.aa ^ (self.aa << 21)),
                1 => self.aa ^= self.aa >> 5,
                2 => self.aa ^= self.aa << 12,
                _ => self.aa ^= self.aa >> 33,
            }
            self.aa = self.aa.wrapping_add(self.mm[(i + 128) % 256]);
            let x = self.mm[i];
            let y = self.mm[((x >> 3) % 256) as usize]
                .wrapping_add(self.aa)
                .wrapping_add(self.bb);
            self.mm[i] = y;
            self.bb = self.mm[((y >> 11) % 256) as usize].wrapping_add(x);
            self.seed[i] = self.bb;
        }
    }

    fn random(&mut self) -> u64 {
        let result = self.seed[self.rand_cnt];
        if self.rand_cnt == 0 {
            self.isaac64();
            self.rand_cnt = 255;
        } else {
            self.rand_cnt -= 1;
        }
        result
    }
}

pub fn isaac64_keystream(key: u64, length: usize) -> Vec<u8> {
    let mut ctx = IsaacCtx::new(key);
    let mut output = vec![0u8; length];
    for (index, chunk) in output.chunks_mut(8).enumerate() {
        let value = ctx.random().to_be_bytes();
        let offset = index * 8;
        let count = chunk.len().min(8);
        chunk.copy_from_slice(&value[..count]);
        let _ = offset;
    }
    output
}

pub fn decrypt_channels_video(data: &[u8], decode_key: u64) -> Vec<u8> {
    let mut output = data.to_vec();
    let mut ctx = IsaacCtx::new(decode_key);
    let encrypt_len = ENCRYPT_HEAD_LEN.min(output.len());
    let mut index = 0usize;
    while index < encrypt_len {
        let key = ctx.random().to_be_bytes();
        for byte in key {
            if index >= encrypt_len {
                break;
            }
            output[index] ^= byte;
            index += 1;
        }
    }
    output
}

pub fn extract_sph_id(raw_url: &str) -> Option<String> {
    let url = Url::parse(raw_url).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "weixin.qq.com" && url.path().starts_with("/sph/") {
        let id = url.path().trim_start_matches("/sph/").trim_end_matches('/');
        return (!id.is_empty()).then(|| id.to_string());
    }
    if host == "channels.weixin.qq.com" && url.path() == "/finder-preview/pages/sph" {
        return url
            .query_pairs()
            .find(|(key, _)| key == "id")
            .map(|(_, value)| value.into_owned());
    }
    None
}

async fn yuanbao_parse(share_url: &str, cookie: &str, api_base: &str) -> anyhow::Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let response = client
        .post(format!("{api_base}/api/weixin/get_parse_result"))
        .header("accept", "application/json, text/plain, */*")
        .header("content-type", "application/json")
        .header("origin", "https://yuanbao.tencent.com")
        .header("referer", "https://yuanbao.tencent.com/chat/")
        .header("user-agent", UA)
        .header("x-source", "web")
        .header("x-webversion", "2.69.0")
        .header("cookie", cookie)
        .json(&json!({
            "type": "video_channel_url",
            "url": share_url,
            "scene": 1
        }))
        .send()
        .await?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        anyhow::bail!("元宝登录态已过期，请重新登录元宝并更新 channels.yuanbaoCookie（HTTP 401）");
    }
    if !status.is_success() {
        anyhow::bail!("元宝解析接口异常: HTTP {}", status.as_u16());
    }
    let data: Value = response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("元宝解析接口返回无效 JSON 响应"))?;
    let error_code = data.pointer("/error/code").and_then(numeric_code);
    if error_code == Some(20002) {
        anyhow::bail!(
            "元宝登录态已过期，请重新登录元宝并更新 channels.yuanbaoCookie（元宝错误码 20002）"
        );
    }
    let code = data.get("code").and_then(numeric_code).unwrap_or(0);
    if data.get("error").is_some_and(|value| !value.is_null()) || code != 0 {
        if let Some(error_code) = error_code.or((code != 0).then_some(code)) {
            anyhow::bail!("元宝解析失败: 错误码 {error_code}");
        }
        anyhow::bail!("元宝解析失败: 接口返回错误");
    }
    Ok(data.get("data").cloned().unwrap_or(Value::Null))
}

fn numeric_code(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

async fn fetch_feed_info(
    export_id: &str,
    general_token: &str,
    api_base: &str,
) -> anyhow::Result<Value> {
    let mut random = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut random);
    let rid = format!(
        "{:x}-{}",
        chrono::Utc::now().timestamp(),
        hex::encode(random)
    );
    let page_url = "https://channels.weixin.qq.com/finder-preview/pages/feed";
    let referer = format!(
        "{page_url}?token={}&eid={}",
        urlencoding::encode(general_token),
        urlencoding::encode(export_id)
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let response = client
        .post(format!(
            "{api_base}/finder-preview/api/feed/get_feed_info?_rid={rid}&_pageUrl={}",
            urlencoding::encode(page_url)
        ))
        .header("accept", "application/json, text/plain, */*")
        .header("content-type", "application/json")
        .header("origin", "https://channels.weixin.qq.com")
        .header("referer", referer)
        .header("user-agent", UA)
        .json(&json!({
            "baseReq": { "generalToken": general_token },
            "exportId": export_id
        }))
        .send()
        .await?;
    let status = response.status();
    let data: Value = response.json().await?;
    if !status.is_success() {
        anyhow::bail!("视频号详情接口异常: HTTP {}", status.as_u16());
    }
    if let Some(code) = data.get("errCode").and_then(Value::as_i64)
        && code != 0
    {
        anyhow::bail!(
            "视频号详情失败: errCode {code} {}",
            data.get("errMsg")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(150)
                .collect::<String>()
        );
    }
    Ok(data)
}

fn parse_formatted_count(raw: &str) -> Option<i64> {
    let cleaned = raw.trim().replace([',', '，', '+', '＋'], "");
    if cleaned.is_empty() {
        return None;
    }
    let trimmed = cleaned.trim();
    let (number_part, multiplier) = if let Some(prefix) = trimmed.strip_suffix('亿') {
        (prefix.trim(), 100_000_000.0)
    } else if let Some(prefix) = trimmed
        .strip_suffix('万')
        .or_else(|| trimmed.strip_suffix('w'))
        .or_else(|| trimmed.strip_suffix('W'))
    {
        (prefix.trim(), 10_000.0)
    } else if let Some(prefix) = trimmed
        .strip_suffix('千')
        .or_else(|| trimmed.strip_suffix('k'))
        .or_else(|| trimmed.strip_suffix('K'))
    {
        (prefix.trim(), 1_000.0)
    } else if let Some(prefix) = trimmed
        .strip_suffix('m')
        .or_else(|| trimmed.strip_suffix('M'))
    {
        (prefix.trim(), 1_000_000.0)
    } else {
        (trimmed, 1.0)
    };
    if number_part.is_empty() {
        return None;
    }
    if multiplier == 1.0
        && number_part.bytes().all(|byte| byte.is_ascii_digit())
        && let Ok(parsed) = number_part.parse::<i64>()
    {
        return Some(parsed);
    }
    let value: f64 = number_part.parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let scaled = (value * multiplier).round();
    if scaled > i64::MAX as f64 {
        return None;
    }
    Some(scaled as i64)
}

fn stat_value(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number
            .as_i64()
            .filter(|count| *count >= 0)
            .or_else(|| number.as_u64().and_then(|count| i64::try_from(count).ok()))
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|count| count.is_finite() && *count >= 0.0)
                    .map(|count| count.round() as i64)
            }),
        Value::String(text) => parse_formatted_count(text),
        _ => None,
    }
}

fn stat_number(raw: Option<&Value>, fmt: Option<&Value>) -> i64 {
    let raw_count = stat_value(raw);
    let fmt_count = stat_value(fmt);
    match (raw_count, fmt_count) {
        (Some(count), _) if count > 0 => count,
        (Some(0), Some(fmt)) if fmt > 0 => fmt,
        (Some(count), _) => count,
        (None, Some(fmt)) => fmt,
        (None, None) => 0,
    }
}

pub fn map_feed_to_parsed(feed: &Value) -> ParsedData {
    let data = feed.get("data").unwrap_or(&Value::Null);
    let info = data
        .get("feedInfo")
        .or_else(|| feed.get("feedInfo"))
        .unwrap_or(&Value::Null);
    let author_info = data.get("authorInfo").unwrap_or(&Value::Null);
    let mut parsed = ParsedData::empty("video");
    parsed.desc = info
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    parsed.title = parsed.desc.chars().take(60).collect();
    parsed.author = author_info
        .get("nickname")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    parsed.uid = author_info
        .get("exportId")
        .or_else(|| author_info.get("username"))
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    parsed.avatar = author_info
        .get("headImgUrl")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    parsed.cover = info
        .get("coverUrl")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    parsed.like = stat_number(info.get("likeCount"), info.get("likeCountFmt"));
    parsed.comment = stat_number(info.get("commentCount"), info.get("commentCountFmt"));
    parsed.share = stat_number(info.get("forwardCount"), info.get("forwardCountFmt"));
    parsed.collect = stat_number(info.get("favCount"), info.get("favCountFmt"));
    parsed.play = stat_number(info.get("readCount"), info.get("readCountFmt"));
    let create_time = info
        .get("createtime")
        .or_else(|| info.get("createTime"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    parsed.publish_time = if create_time > 0 {
        create_time * 1000
    } else {
        0
    };

    let pictures = info
        .get("picInfo")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let video_url = info
        .get("videoUrl")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !pictures.is_empty() && video_url.is_empty() {
        parsed.media_type = "image".into();
        parsed.images = pictures
            .into_iter()
            .filter_map(|picture| {
                if let Some(url) = picture.as_str() {
                    return Some(url.to_string());
                }
                picture
                    .get("url")
                    .or_else(|| picture.get("picUrl"))
                    .or_else(|| picture.get("mediaUrl"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .filter(|url| !url.is_empty())
            .collect();
        if parsed.cover.is_empty() {
            parsed.cover = parsed.images.first().cloned().unwrap_or_default();
        }
        return parsed;
    }

    parsed.media_type = "video".into();
    parsed.video = video_url.to_string();
    if parsed.video.is_empty() {
        parsed.video = info
            .pointer("/h264VideoInfo/videoUrl")
            .or_else(|| info.pointer("/h265VideoInfo/videoUrl"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    if !parsed.video.is_empty() {
        parsed.videos = vec![VideoQuality {
            quality: "default".into(),
            url: parsed.video.clone(),
            bit_rate: None,
        }];
        if let Some(h264) = info
            .pointer("/h264VideoInfo/videoUrl")
            .and_then(Value::as_str)
            && h264 != parsed.video
        {
            parsed.videos.insert(
                0,
                VideoQuality {
                    quality: "h264".into(),
                    url: h264.to_string(),
                    bit_rate: None,
                },
            );
        }
    }
    parsed
}

pub async fn parse_channels_share(
    share_url: &str,
    config: &ChannelsConfig,
) -> anyhow::Result<ParsedData> {
    if config.yuanbao_cookie.is_empty() {
        anyhow::bail!("未配置 channels.yuanbaoCookie (腾讯元宝网页 Cookie)");
    }
    if extract_sph_id(share_url).is_none() {
        anyhow::bail!("非视频号分享链接: {share_url}");
    }
    let channels_base = config
        .api_base
        .as_deref()
        .unwrap_or("https://channels.weixin.qq.com");
    let yuanbao_base = config
        .api_base
        .as_deref()
        .unwrap_or("https://yuanbao.tencent.com");
    let parsed = yuanbao_parse(share_url, &config.yuanbao_cookie, yuanbao_base).await?;
    let playable = parsed
        .get("playable_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if playable.is_empty() {
        anyhow::bail!("元宝解析未返回 playable_url");
    }
    let playable_url = Url::parse(playable)?;
    let mut general_token = String::new();
    let mut export_id = String::new();
    for (key, value) in playable_url.query_pairs() {
        match key.as_ref() {
            "token" => general_token = value.into_owned(),
            "eid" => export_id = value.into_owned(),
            _ => {}
        }
    }
    if general_token.is_empty() || export_id.is_empty() {
        anyhow::bail!("playable_url 缺少 token/eid");
    }

    let feed = fetch_feed_info(&export_id, &general_token, channels_base).await?;
    debug!("视频号 feed 详情已获取");
    let mut result = map_feed_to_parsed(&feed);
    if result.cover.is_empty() {
        result.cover = parsed
            .get("cover_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    if result.author.is_empty() {
        result.author = parsed
            .get("author")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
    }
    if result.desc.is_empty() {
        result.desc = parsed
            .get("desc")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
    }
    if result.title.is_empty() {
        result.title = result.desc.chars().take(60).collect();
    }
    if result.video.is_empty() && result.images.is_empty() {
        anyhow::bail!("视频号解析成功但无媒体内容");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn yuanbao_error_fixture(status: u16, response_body: &'static str) -> String {
        use axum::http::{HeaderMap, StatusCode};
        use axum::{Json, Router, routing::post};

        const PRIVATE_COOKIE: &str =
            "hy_user=fixture-private-user; hy_token=fixture-private-cookie";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/api/weixin/get_parse_result",
            post(
                move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers["cookie"], PRIVATE_COOKIE);
                    assert_eq!(headers["x-webversion"], "2.69.0");
                    assert_eq!(body["type"], "video_channel_url");
                    assert_eq!(body["scene"], 1);
                    (StatusCode::from_u16(status).unwrap(), response_body)
                },
            ),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let error = yuanbao_parse("https://weixin.qq.com/sph/fixture", PRIVATE_COOKIE, &base)
            .await
            .unwrap_err()
            .to_string();
        server.abort();
        assert!(
            !error.contains("fixture-private"),
            "error leaked fixture secret"
        );
        assert!(!error.contains(PRIVATE_COOKIE));
        error
    }

    #[tokio::test]
    async fn yuanbao_401_json_requests_cookie_renewal_without_echoing_credentials() {
        let error = yuanbao_error_fixture(
            401,
            r#"{"error":{"code":20002,"message":"fixture-private-body","hy_token":"fixture-private-token"}}"#,
        )
        .await;
        assert_eq!(
            error,
            "元宝登录态已过期，请重新登录元宝并更新 channels.yuanbaoCookie（HTTP 401）"
        );
    }

    #[tokio::test]
    async fn yuanbao_401_html_requests_cookie_renewal_before_json_decode() {
        let error = yuanbao_error_fixture(
            401,
            "<html>fixture-private-body and fixture-private-cookie</html>",
        )
        .await;
        assert!(error.contains("重新登录元宝并更新 channels.yuanbaoCookie"));
        assert!(error.contains("HTTP 401"));
    }

    #[tokio::test]
    async fn yuanbao_200_business_auth_error_requests_cookie_renewal() {
        for body in [
            r#"{"error":{"code":20002,"message":"fixture-private-body"}}"#,
            r#"{"error":{"code":"20002","message":"fixture-private-body"}}"#,
        ] {
            let error = yuanbao_error_fixture(200, body).await;
            assert!(error.contains("重新登录元宝并更新 channels.yuanbaoCookie"));
            assert!(error.contains("20002"));
        }
    }

    #[tokio::test]
    async fn yuanbao_other_errors_report_only_safe_error_code() {
        let error = yuanbao_error_fixture(
            200,
            r#"{"error":{"code":20003,"message":"fixture-private-body","url":"https://example.com/?token=fixture-private-token"}}"#,
        )
        .await;
        assert_eq!(error, "元宝解析失败: 错误码 20003");
    }

    #[test]
    fn isaac64_matches_go_reference_vector() {
        let stream = isaac64_keystream(123_456_789, 64);
        assert_eq!(
            hex::encode(stream),
            "9c87e6946a410182aee08a5a116f7cb986f10740dd490b9a330cd31db53ddc4ab7c372731d687043c11035c5e3f7141001fd43bf6035029496f725dae4bbcbc9"
        );
    }

    #[test]
    fn extracts_sph_ids() {
        assert_eq!(
            extract_sph_id("https://weixin.qq.com/sph/Abc123XY").as_deref(),
            Some("Abc123XY")
        );
        assert_eq!(
            extract_sph_id("https://channels.weixin.qq.com/finder-preview/pages/sph?id=Zz9_-8a")
                .as_deref(),
            Some("Zz9_-8a")
        );
        assert!(extract_sph_id("https://x.com/a/status/123").is_none());
    }

    #[test]
    fn maps_feed_video() {
        let feed = serde_json::json!({
            "errCode": 0,
            "data": {
                "authorInfo": { "nickname": "测试作者", "headImgUrl": "https://x/head.jpg" },
                "feedInfo": {
                    "description": "视频号测试描述",
                    "createtime": 1727000000,
                    "likeCountFmt": "100",
                    "commentCountFmt": "5",
                    "forwardCountFmt": "3",
                    "favCountFmt": "50",
                    "coverUrl": "https://x/cover.jpg",
                    "videoUrl": "https://x/video.mp4",
                    "h264VideoInfo": { "videoUrl": "https://x/video.mp4" },
                    "picInfo": []
                }
            }
        });
        let parsed = map_feed_to_parsed(&feed);
        assert_eq!(parsed.media_type, "video");
        assert_eq!(parsed.video, "https://x/video.mp4");
        assert_eq!(parsed.like, 100);
        assert_eq!(parsed.collect, 50);
    }

    #[test]
    fn prefers_raw_counts_and_parses_formatted_units() {
        let both = serde_json::json!({
            "errCode": 0,
            "data": {
                "feedInfo": {
                    "description": "同时包含原始数值与展示文本",
                    "likeCount": 12345,
                    "likeCountFmt": "1.2万",
                    "commentCount": 560,
                    "commentCountFmt": "560",
                    "forwardCount": 12000,
                    "forwardCountFmt": "1.2万",
                    "favCount": 3456,
                    "favCountFmt": "3.5千",
                    "readCount": 250000,
                    "readCountFmt": "25万",
                    "videoUrl": "https://x/video.mp4"
                }
            }
        });
        let parsed_both = map_feed_to_parsed(&both);
        assert_eq!(parsed_both.like, 12_345);
        assert_eq!(parsed_both.comment, 560);
        assert_eq!(parsed_both.share, 12_000);
        assert_eq!(parsed_both.collect, 3_456);
        assert_eq!(parsed_both.play, 250_000);

        let fmt_only = serde_json::json!({
            "errCode": 0,
            "data": {
                "feedInfo": {
                    "description": "仅有展示文本",
                    "likeCountFmt": "1.2万",
                    "commentCountFmt": "3.5千",
                    "forwardCountFmt": "10万+",
                    "favCountFmt": "1,234",
                    "readCountFmt": "1.2亿",
                    "videoUrl": "https://x/video.mp4"
                }
            }
        });
        let parsed_fmt = map_feed_to_parsed(&fmt_only);
        assert_eq!(parsed_fmt.like, 12_000);
        assert_eq!(parsed_fmt.comment, 3_500);
        assert_eq!(parsed_fmt.share, 100_000);
        assert_eq!(parsed_fmt.collect, 1_234);
        assert_eq!(parsed_fmt.play, 120_000_000);
    }
}
