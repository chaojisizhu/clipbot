use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde_json::Value;
use tracing::{info, warn};

use crate::types::LinkMatch;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const CARD_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const WBI_MIXIN: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

static WBI_CACHE: Lazy<Mutex<Option<(String, String, Instant)>>> = Lazy::new(|| Mutex::new(None));

async fn get_wbi_keys(api_base: &str) -> anyhow::Result<(String, String)> {
    if let Ok(cache) = WBI_CACHE.lock() {
        if let Some((img, sub, expire)) = cache.as_ref() {
            if Instant::now() < *expire {
                return Ok((img.clone(), sub.clone()));
            }
        }
    }
    let client = reqwest::Client::builder()
        .timeout(CARD_REQUEST_TIMEOUT)
        .build()?;
    let data: Value = client
        .get(format!("{api_base}/x/web-interface/nav"))
        .header("User-Agent", UA)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let wbi = &data["data"]["wbi_img"];
    let img = wbi["img_url"]
        .as_str()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_string();
    let sub = wbi["sub_url"]
        .as_str()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_string();
    if img.is_empty() || sub.is_empty() {
        anyhow::bail!("获取 bilibili wbi 密钥失败");
    }
    if let Ok(mut cache) = WBI_CACHE.lock() {
        *cache = Some((
            img.clone(),
            sub.clone(),
            Instant::now() + Duration::from_secs(12 * 3600),
        ));
    }
    Ok((img, sub))
}

fn wbi_sign(mut params: BTreeMap<String, String>, img: &str, sub: &str) -> (String, String) {
    let mixin = format!("{img}{sub}");
    let key: String = WBI_MIXIN
        .iter()
        .filter_map(|index| mixin.as_bytes().get(*index).copied())
        .map(char::from)
        .take(32)
        .collect();
    params.insert(
        "wts".into(),
        (chrono::Utc::now().timestamp_millis() / 1000).to_string(),
    );
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let digest = md5::compute(format!("{query}{key}").as_bytes());
    (query, format!("{digest:x}"))
}

fn strip_em_tags(value: &str) -> String {
    let regex = regex::Regex::new(r"<[^>]+>").unwrap();
    regex.replace_all(value, "").trim().to_string()
}

fn title_likely_match(card_title: &str, hit_title: &str) -> bool {
    let a: String = card_title.chars().filter(|c| !c.is_whitespace()).collect();
    let b: String = hit_title.chars().filter(|c| !c.is_whitespace()).collect();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a.contains(&b) || b.contains(&a) {
        return true;
    }
    a.chars().take(10).eq(b.chars().take(10))
}

async fn search_bilibili(title: &str, api_base: &str) -> anyhow::Result<Option<(String, String)>> {
    let (img, sub) = get_wbi_keys(api_base).await?;
    let params = BTreeMap::from([
        ("keyword".to_string(), title.to_string()),
        ("search_type".to_string(), "video".to_string()),
    ]);
    let (query, w_rid) = wbi_sign(params, &img, &sub);
    let client = reqwest::Client::builder()
        .timeout(CARD_REQUEST_TIMEOUT)
        .build()?;
    let data: Value = client
        .get(format!(
            "{api_base}/x/web-interface/wbi/search/type?{query}&w_rid={w_rid}"
        ))
        .header("User-Agent", UA)
        .header("Referer", "https://search.bilibili.com/")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let Some(results) = data["data"]["result"].as_array() else {
        return Ok(None);
    };
    for result in results.iter().take(3) {
        let hit_title = strip_em_tags(result["title"].as_str().unwrap_or_default());
        if let Some(bvid) = result["bvid"].as_str() {
            if title_likely_match(title, &hit_title) {
                return Ok(Some((bvid.to_string(), hit_title)));
            }
        }
    }
    Ok(None)
}

pub async fn resolve_card_by_title(
    card_title: &str,
    card_source: &str,
    api_base: &str,
) -> Option<LinkMatch> {
    let title = card_title.trim();
    if title.is_empty() {
        return None;
    }
    if card_source.to_ascii_lowercase().contains("bilibili") || card_source.contains("哔哩") {
        match search_bilibili(title, api_base).await {
            Ok(Some((bvid, hit_title))) => {
                info!(
                    "小程序卡片标题搜索命中: {} ({})",
                    bvid,
                    hit_title.chars().take(30).collect::<String>()
                );
                let url = format!("https://www.bilibili.com/video/{bvid}");
                return Some(LinkMatch {
                    media_type: "bilibili".into(),
                    id: url.clone(),
                    url,
                });
            }
            Ok(None) => info!(
                "小程序卡片标题搜索无匹配: {}",
                title.chars().take(40).collect::<String>()
            ),
            Err(err) => warn!("小程序卡片标题搜索失败: {err}"),
        }
        return None;
    }
    info!("小程序卡片来源暂不支持搜索还原: {card_source}");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_matching_supports_contains_and_prefix() {
        assert!(title_likely_match("哀鸿日配现已实装", "哀鸿日配现已实装！"));
        assert!(!title_likely_match("完全不同的标题", "另一个视频"));
    }
}
