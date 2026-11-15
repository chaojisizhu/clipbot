//! Douyin Web detail signing, based on the platform protocol documented by
//! Evil0ctal/Douyin_TikTok_Download_API's native/websign.py (Apache-2.0).
//! Only the visitor ID already present in the caller's own Cookie is used.

use anyhow::{Result, ensure};
use reqwest::header::{HeaderMap, HeaderValue, REFERER, USER_AGENT};
use url::Url;

pub(super) const DETAIL_ENDPOINT: &str = "https://www.douyin.com/aweme/v1/web/aweme/detail/";
pub(super) const UIFID_COOKIES: &[&str] = &[
    "uifid",
    "uifid_temp",
    "uifidtemp",
    "UIFID",
    "UIFID_TEMP",
    "UIFIDTEMP",
];
const WEB_SIGN_SALT: &str = "A96D855A08C0A9707F8BEF0D9A527E4E";
pub(super) const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36";

fn signature(query: &str, uifid: &str, timestamp: &str) -> String {
    let covered = format!("{uifid}_{timestamp}_{WEB_SIGN_SALT}_{query}");
    format!("{:x}", md5::compute(covered))
}

pub(super) fn detail_request(
    id: &str,
    uifid: &str,
    verify_fp: Option<&str>,
    timestamp: u64,
) -> Result<(Url, HeaderMap)> {
    ensure!(
        !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()),
        "抖音作品 ID 无效"
    );
    ensure!(!uifid.is_empty(), "抖音 Web 详情需要已有 UIFID Cookie");
    let stamp = timestamp.to_string();
    let mut url = Url::parse(DETAIL_ENDPOINT)?;
    {
        let mut query = url.query_pairs_mut();
        query.extend_pairs([
            ("device_platform", "webapp"),
            ("aid", "6383"),
            ("channel", "channel_pc_web"),
            ("aweme_id", id),
            ("pc_client_type", "1"),
            ("cookie_enabled", "true"),
            ("browser_name", "Chrome"),
            ("browser_version", "142.0.0.0"),
            ("browser_platform", "Win32"),
            ("browser_online", "true"),
        ]);
        if let Some(verify_fp) = verify_fp.filter(|value| !value.is_empty()) {
            query.extend_pairs([("verifyFp", verify_fp), ("fp", verify_fp)]);
        }
        query.extend_pairs([("uifid", uifid), ("timestamp", stamp.as_str())]);
    }
    // Hash the exact serialized query that will be sent. Re-encoding it after
    // signing changes the covered bytes, particularly for '+' and '%'.
    let signature = signature(url.query().unwrap_or_default(), uifid, &stamp);
    url.query_pairs_mut()
        .append_pair("x-secsdk-web-signature", &signature);
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(WEB_USER_AGENT));
    headers.insert(
        REFERER,
        HeaderValue::from_str(&format!("https://www.douyin.com/note/{id}"))?,
    );
    headers.insert("uifid", HeaderValue::from_str(uifid)?);
    headers.insert("x-secsdk-web-signature", HeaderValue::from_str(&signature)?);
    headers.insert("x-secsdk-web-expire", HeaderValue::from_str(&stamp)?);
    Ok((url, headers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_signature_matches_the_reference_vector() {
        let query = "device_platform=webapp&aid=6383&sec_user_id=MS4wLjABAAAAtest&whale_cut_token=&uifid=UIFID-ABC&timestamp=1720000000";
        assert_eq!(
            signature(query, "UIFID-ABC", "1720000000"),
            "45dd7924a045b5507675713c4620fba2"
        );
    }

    #[test]
    fn detail_signature_covers_exact_encoding_and_matches_headers() {
        let (url, headers) =
            detail_request("123", "fixture+id", Some("verify_+/%"), 1720000000).unwrap();
        let (covered, signed) = url
            .query()
            .unwrap()
            .rsplit_once("&x-secsdk-web-signature=")
            .unwrap();
        assert!(covered.contains("verifyFp=verify_%2B%2F%25&fp=verify_%2B%2F%25"));
        assert!(covered.ends_with("uifid=fixture%2Bid&timestamp=1720000000"));
        assert_eq!(signed, signature(covered, "fixture+id", "1720000000"));
        assert_eq!(headers["x-secsdk-web-signature"], signed);
        assert_eq!(headers["uifid"], "fixture+id");
        assert_eq!(headers["x-secsdk-web-expire"], "1720000000");
        assert!(detail_request("123/456", "fixture", None, 1).is_err());
        assert!(detail_request("123", "", None, 1).is_err());
        assert!(detail_request("123", "fixture\r\ninvalid", None, 1).is_err());
    }
}
