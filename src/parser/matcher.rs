use std::collections::HashSet;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::redirect::Policy;
use tracing::debug;
use url::Url;

use crate::types::LinkMatch;

struct Rule {
    media_type: &'static str,
    regex: Regex,
}

static RULES: Lazy<Vec<Rule>> = Lazy::new(|| {
    [
        (
            "bilibili",
            r"(?:https?://)?(?:(?:www\.)?bilibili\.com/(?:video/(BV\w+|av\d+)|bangumi/play/(ep\d+|ss\d+))|b23\.tv/([a-zA-Z0-9_-]+)|bili\d+\.cn/[a-zA-Z0-9_-]{4,})",
        ),
        (
            "douyin",
            r"(?:https?://)?(?:(?:www\.)?v\.douyin\.com/([a-zA-Z0-9_-]+)|(?:www\.)?iesdouyin\.com/share/video/(\d+)|(?:www\.)?douyin\.com/(?:video|note)/(\d+))",
        ),
        (
            "kuaishou",
            r"(?:https?://)?(?:(?:v|www|video)\.kuaishou\.com/(?:short-video/)?([a-zA-Z0-9_-]+)|(?:v|www|video)\.kuaishou\.com/[a-zA-Z0-9_-]+)",
        ),
        ("pipixia", r"(?:https?://)?(?:(?:h5\.)?pipix\.com/[^\s]+)"),
        (
            "pipigx",
            r"(?:https?://)?(?:h5\.pipigx\.com/[^\s]+|pipigx\.com/[^\s]+)",
        ),
        (
            "toutiao",
            r"(?:https?://)?(?:www\.)?(?:toutiao\.com/[^\s]+|m\.toutiao\.com/[^\s]+|toutiaoimg\.com/[^\s]+)",
        ),
        (
            "xigua",
            r"(?:https?://)?(?:www\.)?(?:ixigua\.com/(?:video/)?(\d+)|v\.ixigua\.com/([a-zA-Z0-9_-]+))",
        ),
        (
            "zuiyou",
            r"(?:https?://)?(?:www\.)?(?:izuiyou\.com/[^\s]+|xiaochuankeji\.cn/[^\s]+)",
        ),
        (
            "jimeng",
            r"(?:https?://)?(?:jimeng\.jianying\.com/[^\s]+|jimeng\.ai/[^\s]+|jianying\.com/[^\s]+)",
        ),
        (
            "xiaohongshu",
            r"(?:https?://)?(?:www\.)?(?:xhslink\.com/[a-zA-Z0-9_-]+|xiaohongshu\.com/discovery/item/[a-zA-Z0-9_-]+(?:\?[^\s]*)?|xiaohongshu\.com/explore/[a-zA-Z0-9_-]+(?:\?[^\s]*)?)",
        ),
        (
            "weibo",
            r"(?:https?://)?(?:(?:www\.)?weibo\.(?:com|cn)/(?:[^\s]+/)?(\w+)|(?:m\.)?weibo\.cn/(?:status/|detail/)?(\w+)|weibointl\.api\.weibo\.com/[^\s]+|video\.weibo\.com/[^\s]+)",
        ),
        (
            "acfun",
            r"(?:https?://)?(?:www\.)?acfun\.cn/(?:v/ac(\d+)|a/ac(\d+))",
        ),
        (
            "zhihu",
            r"(?:https?://)?(?:www\.)?zhihu\.com/(?:question/\d+/answer/\d+|zvideo/(\d+)|p/(\d+))",
        ),
        (
            "weishi",
            r"(?:https?://)?(?:www\.)?(?:weishi\.qq\.com/[^\s]+|isee\.weishi\.qq\.com/[^\s]+|h5\.weishi\.qq\.com/[^\s]+)",
        ),
        ("huya", r"(?:https?://)?(?:huya\.com/[^\s]+|v\.huya\.com/[^\s]+)"),
        (
            "haokan",
            r"(?:https?://)?(?:haokan\.baidu\.com/v\?vid=(\d+)|haokan\.baidu\.com/[^\s]+)",
        ),
        (
            "meipai",
            r"(?:https?://)?(?:www\.)?meipai\.com/media/(\d+)",
        ),
        (
            "youtube",
            r"(?:https?://)?(?:www\.)?(?:youtube\.com/(?:watch\?v=|shorts/)([a-zA-Z0-9_-]+)|youtu\.be/([a-zA-Z0-9_-]+))",
        ),
        (
            "tiktok",
            r"(?:https?://)?(?:www\.)?(?:tiktok\.com/(?:@[\w.-]+/video/(\d+)|v/(\d+))|vt\.tiktok\.com/([a-zA-Z0-9_-]+)|vm\.tiktok\.com/([a-zA-Z0-9_-]+))",
        ),
        (
            "twitter",
            r"(?:https?://)?(?:(?:www|mobile)\.)?(?:twitter\.com|x\.com)/[a-zA-Z0-9_]+/status/(\d+)",
        ),
        (
            "instagram",
            r"(?:https?://)?(?:www\.)?instagram\.com/(?:p|reel|tv)/([a-zA-Z0-9_-]+)",
        ),
        ("doubao", r"(?:https?://)?(?:www\.)?doubao\.com/[^\s]+"),
        (
            "channels",
            r"(?:https?://)?(?:weixin\.qq\.com/sph/[A-Za-z0-9_-]+|channels\.weixin\.qq\.com/finder-preview/pages/sph\?[^\s]+)",
        ),
    ]
    .into_iter()
    .map(|(media_type, pattern)| Rule {
        media_type,
        regex: Regex::new(&format!("(?i){pattern}")).unwrap(),
    })
    .collect()
});

pub fn link_type_parser(content: &str) -> Vec<LinkMatch> {
    if content.is_empty() {
        return Vec::new();
    }
    let content = content.replace(r"\/", "/");
    let mut matches = Vec::new();
    let mut seen = HashSet::new();

    for rule in RULES.iter() {
        for capture in rule.regex.captures_iter(&content) {
            let Some(found) = capture.get(0) else {
                continue;
            };
            let mut url = found.as_str().to_string();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                url = format!("https://{url}");
            }
            if !seen.insert(url.clone()) {
                continue;
            }
            let id = capture
                .get(1)
                .filter(|m| !m.as_str().is_empty())
                .map_or_else(|| url.clone(), |m| m.as_str().to_string());
            matches.push(LinkMatch {
                media_type: rule.media_type.into(),
                url,
                id,
            });
        }
    }
    matches
}

pub fn clean_url(raw: &str) -> String {
    let normalized = raw.replace("&amp;", "&");
    let Ok(mut url) = Url::parse(&normalized) else {
        return normalized.split('?').next().unwrap_or_default().to_string();
    };

    if url.scheme() == "http" {
        let _ = url.set_scheme("https");
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();

    if host == "bilibili.com" {
        let _ = url.set_host(Some("www.bilibili.com"));
    }
    if host.contains("douyin.com") {
        let remove = [
            "source",
            "share_type",
            "share_token",
            "timestamp",
            "from",
            "isappinstalled",
        ];
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| !remove.contains(&key.as_ref()))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(pairs);
        }
        return strip_query(&url);
    }
    if host.contains("bilibili.com") || host.contains("b23.tv") {
        return strip_query(&url);
    }
    if host == "x.com" || host == "www.x.com" {
        let _ = url.set_host(Some("twitter.com"));
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "s" && key != "t")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(pairs);
        }
        return format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or_default(),
            url.path()
        );
    }
    if host.contains("twitter.com") {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "s" && key != "t")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(pairs);
        }
        return format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or_default(),
            url.path()
        );
    }
    url.to_string()
}

fn strip_query(url: &Url) -> String {
    format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.path()
    )
}

pub async fn resolve_short_url(url: &str) -> String {
    static SHORT_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"b23\.tv|bili\d+\.cn|v\.douyin\.com|v\.kuaishou\.com|xhslink\.com|t\.co|youtu\.be|h5\.pipix\.com/s/",
        )
        .unwrap()
    });
    if !SHORT_RE.is_match(url) {
        return clean_url(url);
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        "User-Agent",
        HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"),
    );
    headers.insert(
        "Referer",
        HeaderValue::from_static("https://www.baidu.com/"),
    );
    let direct = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(8))
        .build();
    if let Ok(client) = direct {
        if let Ok(res) = client.get(url).headers(headers.clone()).send().await {
            if res.status().is_redirection() {
                if let Some(location) = res.headers().get(reqwest::header::LOCATION) {
                    if let Ok(location) = location.to_str() {
                        if let Ok(absolute) = Url::parse(url).and_then(|base| base.join(location)) {
                            return clean_url(absolute.as_str());
                        }
                    }
                }
            }
        }
    }

    let follow = reqwest::Client::builder()
        .redirect(Policy::limited(8))
        .timeout(Duration::from_secs(8))
        .build();
    match follow {
        Ok(client) => match client.get(url).headers(headers).send().await {
            Ok(res) => clean_url(res.url().as_str()),
            Err(err) => {
                debug!("短链解析失败: {err}");
                clean_url(url)
            }
        },
        Err(_) => clean_url(url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_supported_platforms() {
        let links = link_type_parser(
            r#"看 https://v.douyin.com/Pha48-74aOs/ 和 https://www.bilibili.com/video/BV1xx411c7mD 以及 {"jumpUrl":"https:\/\/v.kuaishou.com\/9xY2ab"}"#,
        );
        assert!(
            links
                .iter()
                .any(|m| m.media_type == "douyin" && m.url == "https://v.douyin.com/Pha48-74aOs")
        );
        assert!(links.iter().any(|m| m.media_type == "bilibili"));
        assert!(links.iter().any(|m| m.media_type == "kuaishou"));
    }

    #[test]
    fn preserves_xiaohongshu_token() {
        let links = link_type_parser(
            "https://www.xiaohongshu.com/explore/6acf1818000000000a0261e5?xsec_source=app_share&xsec_token=ABC123%3D&share_id=xyz",
        );
        let hit = links
            .iter()
            .find(|m| m.media_type == "xiaohongshu")
            .unwrap();
        assert!(hit.url.contains("xsec_token=ABC123%3D"));
    }

    #[test]
    fn normalizes_twitter_and_bilibili() {
        assert_eq!(
            clean_url("https://x.com/user/status/123?s=20"),
            "https://twitter.com/user/status/123"
        );
        assert_eq!(
            clean_url("https://www.bilibili.com/video/BV1?spm_id_from=x"),
            "https://www.bilibili.com/video/BV1"
        );
    }
}
