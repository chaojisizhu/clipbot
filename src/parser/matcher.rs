use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;
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
            r#"(?:https?://)?(?:(?:www\.|m\.)?bilibili\.com/(?:video/(BV\w+|av\d+)|bangumi/play/(ep\d+|ss\d+)|opus/(\d+)|dynamic/(\d+)|read/(?:cv(\d+)|mobile/?\?[^\s"<>]*\bid=(\d+)[^\s"<>]*))|t\.bilibili\.com/(?:h5/dynamic/detail/)?(\d+)|b23\.tv/([a-zA-Z0-9_-]+)|bili2233\.cn/([a-zA-Z0-9_-]{4,}))/?(?:\?[^\s"<>]+)?"#,
        ),
        (
            "douyin",
            r"(?:https?://)?(?:(?:www\.)?v\.douyin\.com/([a-zA-Z0-9_-]+)|(?:www\.)?iesdouyin\.com/share/(?:video|note|slides)/(\d+)|(?:www\.)?douyin\.com/(?:video|note|slides)/(\d+))",
        ),
        (
            "kuaishou",
            r"(?:https?://)?(?:(?:v|www|video)\.kuaishou\.com/(?:short-video/)?([a-zA-Z0-9_-]+)|(?:v|www|video)\.kuaishou\.com/[a-zA-Z0-9_-]+)",
        ),
        ("pipixia", r"(?:https?://)?(?:(?:h5\.)?pipix\.com/[^\s]+)"),
        (
            "pipigx",
            r"(?:https?://)?(?:(?:h5\.)?(?:pipigx|ippzone)\.com/[^\s]+)",
        ),
        (
            "toutiao",
            r"(?:https?://)?(?:www\.)?(?:toutiao\.com/[^\s]+|m\.toutiao\.com/[^\s]+|toutiaoimg\.com/[^\s]+)",
        ),
        (
            "xigua",
            r"(?:https?://)?(?:www\.)?(?:ixigua\.com/(?:video/)?(\d+)|v\.ixigua\.com/([a-zA-Z0-9_-]+)|iesdouyin\.com/xg/video/(\d+))",
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
            r"(?:https?://)?(?:www\.)?(?:xhslink\.(?:com|cn)/(?:[a-zA-Z0-9_-]+/)*[a-zA-Z0-9_-]+|(?:xiaohongshu|rednote)\.com/(?:discovery/item|explore|user/profile/[a-zA-Z0-9_-]+)/[a-zA-Z0-9_-]+(?:\?[^\s]*)?)",
        ),
        (
            "weibo",
            r"(?:https?://)?(?:(?:www\.)?weibo\.(?:com|cn)/(?:[^\s]+/)?(\w+)|(?:m\.)?weibo\.cn/(?:status/|detail/)?(\w+)|weibointl\.api\.weibo\.com/[^\s]+|video\.weibo\.com/[^\s]+)",
        ),
        (
            "acfun",
            r#"(?:https?://)?(?:www\.)?acfun\.cn/(?:v/ac(\d+)|a/ac(\d+))(?:\?[^\s"<>]+)?"#,
        ),
        (
            "zhihu",
            r"(?:https?://)?(?:www\.)?zhihu\.com/(?:question/\d+/answer/\d+|zvideo/(\d+)|p/(\d+))",
        ),
        (
            "weishi",
            r"(?:https?://)?(?:www\.)?(?:weishi\.qq\.com/[^\s]+|video\.weishi\.qq\.com/[^\s]+|isee\.weishi\.qq\.com/[^\s]+|h5\.weishi\.qq\.com/[^\s]+)",
        ),
        ("huya", r"(?:https?://)?(?:(?:www\.|v\.)?huya\.com/[^\s]+|hy\.fan/[^\s]+)"),
        (
            "haokan",
            r"(?:https?://)?(?:haokan\.baidu\.com/v\?vid=(\d+)|haokan\.baidu\.com/[^\s]+)",
        ),
        (
            "meipai",
            r"(?:https?://)?(?:www\.)?meipai\.com/(?:media/(\d+)|video/\d+/(\d+))",
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
            let mut url = found
                .as_str()
                .trim_end_matches(['，', '。', '！', '？', ')', '）', '】', '》', '"', '\''])
                .to_string();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                url = format!("https://{url}");
            }
            if !seen.insert(url.clone()) {
                continue;
            }
            let id = capture
                .iter()
                .skip(1)
                .flatten()
                .find(|m| !m.as_str().is_empty())
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
    if host == "bilibili.com" || host.ends_with(".bilibili.com") {
        let trimmed_path = url.path().trim_end_matches('/').to_owned();
        if trimmed_path == "/read/mobile"
            && let Some(article_id) = url
                .query_pairs()
                .find(|(key, value)| {
                    key == "id" && !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
                })
                .map(|(_, value)| value.into_owned())
        {
            url.set_path(&format!("/read/cv{article_id}"));
            url.set_query(None);
            url.set_fragment(None);
            return url.to_string();
        }
        if !trimmed_path.is_empty() && trimmed_path != url.path() {
            url.set_path(&trimmed_path);
        }
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key == "p")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        url.set_fragment(None);
        if !pairs.is_empty() {
            url.query_pairs_mut().extend_pairs(pairs);
        }
        return url.to_string();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_douyin_slides_share_links() {
        let links =
            link_type_parser("实况 https://www.iesdouyin.com/share/slides/7707979871255619449/");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].media_type, "douyin");
        assert_eq!(
            links[0].url,
            "https://www.iesdouyin.com/share/slides/7707979871255619449"
        );
    }

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
    fn keeps_bilibili_and_acfun_parts_and_nested_short_paths() {
        let hits = link_type_parser(
            "https://www.bilibili.com/video/BV17x411w7KC?p=2&spm_id_from=test https://www.acfun.cn/v/ac123?p=2 https://xhslink.com/a/ABC123，",
        );
        assert_eq!(
            clean_url(
                &hits
                    .iter()
                    .find(|hit| hit.media_type == "bilibili")
                    .unwrap()
                    .url
            ),
            "https://www.bilibili.com/video/BV17x411w7KC?p=2"
        );
        assert!(
            hits.iter()
                .find(|hit| hit.media_type == "acfun")
                .unwrap()
                .url
                .ends_with("?p=2")
        );
        assert_eq!(
            hits.iter()
                .find(|hit| hit.media_type == "xiaohongshu")
                .unwrap()
                .url,
            "https://xhslink.com/a/ABC123"
        );
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

        let rednote_links = link_type_parser(
            "看 https://www.rednote.com/discovery/item/6a325cd7000000002103d8e2?xsec_token=TOK123 与 https://xhslink.cn/m/abc",
        );
        assert_eq!(rednote_links.len(), 2);
        assert!(rednote_links.iter().all(|m| m.media_type == "xiaohongshu"));
        assert!(rednote_links[0].url.contains("xsec_token=TOK123"));
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
        assert_eq!(
            clean_url("https://www.bilibili.com/opus/1271940823012868096/?spm_id_from=333"),
            "https://www.bilibili.com/opus/1271940823012868096"
        );
        assert_eq!(
            clean_url("https://www.bilibili.com/read/mobile?spm=1&id=53711426"),
            "https://www.bilibili.com/read/cv53711426"
        );
    }

    #[test]
    fn recognizes_bilibili_dynamics_opus_and_read_cv_links() {
        let text = concat!(
            "动态1 https://t.bilibili.com/1271940823012868096?tab=2 ",
            "动态2 https://www.bilibili.com/opus/1265772221318561798/ ",
            "移动 https://m.bilibili.com/dynamic/1270724202670850065，",
            "H5 https://t.bilibili.com/h5/dynamic/detail/1270796658725093378 ",
            "专栏 https://www.bilibili.com/read/cv53711426/ ",
            "移动专栏 https://m.bilibili.com/read/mobile?spm=1&id=12345 ",
            "视频 https://m.bilibili.com/video/BV1mpy6ziE1P"
        );
        let hits = link_type_parser(text);
        assert_eq!(hits.len(), 7);
        assert!(hits.iter().all(|hit| hit.media_type == "bilibili"));
        assert!(hits.iter().any(|hit| hit.id == "1271940823012868096"));
        assert!(hits.iter().any(|hit| hit.id == "1265772221318561798"));
        assert!(hits.iter().any(|hit| hit.id == "1270724202670850065"));
        assert!(hits.iter().any(|hit| hit.id == "1270796658725093378"));
        assert!(hits.iter().any(|hit| hit.id == "53711426"));
        assert!(hits.iter().any(|hit| hit.id == "12345"));
        assert!(hits.iter().any(|hit| hit.id == "BV1mpy6ziE1P"));
        assert!(
            link_type_parser("https://www.bilibili.com/read/mobile").is_empty(),
            "bare mobile read endpoint without id must not match"
        );
    }
}
