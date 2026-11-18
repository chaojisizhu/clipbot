use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::header::{COOKIE, HeaderMap, HeaderValue, LOCATION, REFERER, RETRY_AFTER};
use reqwest::{Client, Method, Response};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

use crate::types::AppConfig;

#[derive(Debug)]
pub(super) struct SiteAccessRestricted(String);

impl std::fmt::Display for SiteAccessRestricted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SiteAccessRestricted {}

#[derive(Debug)]
pub(super) struct SiteJsonUnavailable(String);

impl std::fmt::Display for SiteJsonUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SiteJsonUnavailable {}

#[derive(Default)]
struct RequestOptions<'a> {
    headers: HeaderMap,
    json: Option<&'a Value>,
    form: Option<&'a [(String, String)]>,
    stop_at: Option<fn(&Url) -> bool>,
}

enum SiteResponse {
    Final(Response),
    RedirectTarget(Url),
}

impl SiteResponse {
    fn into_final(self) -> Result<Response> {
        match self {
            Self::Final(response) => Ok(response),
            Self::RedirectTarget(_) => bail!("平台请求未到达最终响应"),
        }
    }
}

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
pub const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 Version/17.5 Mobile/15E148 Safari/604.1";

struct RequestState {
    next_request: Instant,
    blocked_until: Instant,
}

struct Cookie {
    domain: String,
    subdomains: bool,
    path: String,
    secure: bool,
    expires: u64,
    pair: String,
}

pub struct SiteHttp {
    direct: Client,
    proxy: Option<Client>,
    raw_cookies: HashMap<String, String>,
    file_cookies: HashMap<String, Vec<Cookie>>,
    max_bytes: usize,
    retries: u32,
    retry_interval: Duration,
    gates: HashMap<&'static str, Mutex<RequestState>>,
    min_interval: Duration,
    jitter_ms: u64,
    cooldown: Duration,
}

fn domains(platform: &str) -> &'static [&'static str] {
    match platform {
        "bilibili" => &["bilibili.com", "b23.tv", "bili2233.cn"],
        "douyin" => &["douyin.com", "iesdouyin.com", "amemv.com", "snssdk.com"],
        "xiaohongshu" => &[
            "xiaohongshu.com",
            "rednote.com",
            "xhslink.com",
            "xhslink.cn",
        ],
        "kuaishou" => &["kuaishou.com", "gifshow.com", "chenzhongtech.com"],
        "weibo" => &["weibo.com", "weibo.cn"],
        "twitter" => &["twitter.com", "x.com", "twimg.com", "t.co"],
        "youtube" => &["youtube.com", "youtu.be"],
        "tiktok" => &["tiktok.com"],
        "instagram" => &["instagram.com"],
        "acfun" => &["acfun.cn"],
        "toutiao" => &["toutiao.com", "toutiaoimg.com", "vod.bytedanceapi.com"],
        "xigua" => &[
            "ixigua.com",
            "iesdouyin.com",
            "toutiao.com",
            "vod.bytedanceapi.com",
        ],
        "pipixia" => &["pipix.com", "pipixia.com"],
        "pipigx" => &["pipigx.com", "ippzone.com"],
        "zuiyou" => &["izuiyou.com", "xiaochuankeji.cn"],
        "jimeng" => &["jianying.com", "jimeng.ai"],
        "zhihu" => &["zhihu.com", "zhihuishu.com"],
        "weishi" => &["weishi.qq.com"],
        "huya" => &["huya.com", "hy.fan"],
        "haokan" => &["haokan.baidu.com"],
        "meipai" => &["meipai.com"],
        "doubao" => &["doubao.com"],
        "channels" => &["weixin.qq.com"],
        _ => &[],
    }
}

pub fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn is_xiaohongshu_restricted_redirect(platform: &str, next: &Url) -> bool {
    let Some(host) = next.host_str() else {
        return false;
    };
    platform == "xiaohongshu"
        && (domain_matches(host, "xiaohongshu.com") || domain_matches(host, "rednote.com"))
        && (next.path().starts_with("/login")
            || next.path().starts_with("/web-login")
            || next.path().starts_with("/website-login"))
}

pub fn validate_site_url(raw: &str, platform: &str) -> Result<Url> {
    let mut url = Url::parse(raw).context("链接格式错误")?;
    let host = url.host_str().unwrap_or_default();
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.port_or_known_default(), Some(80 | 443))
        || !domains(platform)
            .iter()
            .any(|domain| domain_matches(host, domain))
    {
        bail!("链接域名与平台 {platform} 不匹配");
    }
    if url.scheme() == "http" {
        url.set_scheme("https")
            .map_err(|_| anyhow::anyhow!("链接协议升级失败"))?;
        if url.port() == Some(80) {
            url.set_port(None)
                .map_err(|_| anyhow::anyhow!("链接端口无效"))?;
        }
    }
    Ok(url)
}

fn raw_cookie_allowed(url: &Url, platform: &str) -> bool {
    let host = url.host_str().unwrap_or_default();
    match platform {
        "twitter" => domain_matches(host, "twitter.com") || domain_matches(host, "x.com"),
        "bilibili" => domain_matches(host, "bilibili.com"),
        "xiaohongshu" => domain_matches(host, "xiaohongshu.com"),
        "youtube" => domain_matches(host, "youtube.com"),
        _ => domains(platform)
            .first()
            .is_some_and(|domain| domain_matches(host, domain)),
    }
}

impl SiteHttp {
    pub fn new(config: &AppConfig) -> Result<Self> {
        let cookies = Arc::new(reqwest::cookie::Jar::default());
        let build = || {
            Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_millis(
                    config.api.timeout.clamp(1000, 120_000),
                ))
                .pool_max_idle_per_host(2)
                .pool_idle_timeout(Duration::from_secs(30))
                .cookie_provider(Arc::clone(&cookies))
        };
        let direct = build().build()?;
        let proxy = if config.api.proxy.trim().is_empty() {
            None
        } else {
            Some(
                build()
                    .proxy(reqwest::Proxy::all(&config.api.proxy)?)
                    .build()?,
            )
        };
        let mut file_cookies = HashMap::new();
        for (platform, path) in &config.auth.cookie_files {
            if !path.trim().is_empty() {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("读取 {platform} 的 Cookie 文件失败"))?;
                file_cookies.insert(platform.clone(), parse_netscape_cookies(&content)?);
            }
        }
        Ok(Self {
            direct,
            proxy,
            raw_cookies: config.auth.cookies.clone(),
            file_cookies,
            max_bytes: config
                .resources
                .max_response_bytes
                .clamp(1024, 16 * 1024 * 1024),
            retries: config.api.retry_times.clamp(1, 3),
            retry_interval: Duration::from_millis(config.api.retry_interval.min(3000)),
            gates: [
                "bilibili",
                "douyin",
                "xiaohongshu",
                "kuaishou",
                "weibo",
                "twitter",
                "youtube",
                "tiktok",
                "instagram",
                "acfun",
                "toutiao",
                "xigua",
                "pipixia",
                "pipigx",
                "zuiyou",
                "jimeng",
                "zhihu",
                "weishi",
                "huya",
                "haokan",
                "meipai",
                "doubao",
                "channels",
            ]
            .into_iter()
            .map(|platform| {
                (
                    platform,
                    Mutex::new(RequestState {
                        next_request: Instant::now(),
                        blocked_until: Instant::now(),
                    }),
                )
            })
            .collect(),
            min_interval: Duration::from_millis(config.api.min_request_interval.min(30_000)),
            jitter_ms: config.api.request_jitter.min(5000),
            cooldown: Duration::from_secs(config.api.rate_limit_cooldown.clamp(1, 3600)),
        })
    }

    pub async fn cool_down(&self, platform: &str) {
        if let Some(gate) = self.gates.get(platform) {
            gate.lock().await.blocked_until = Instant::now() + self.cooldown;
        }
    }

    pub async fn is_cooling_down(&self, platform: &str) -> bool {
        if let Some(gate) = self.gates.get(platform) {
            return gate.lock().await.blocked_until > Instant::now();
        }
        false
    }

    fn cookie_header(&self, url: &Url, platform: &str) -> Result<Option<HeaderValue>> {
        if url.scheme() != "https" {
            return Ok(None);
        }
        let mut pairs = Vec::new();
        if raw_cookie_allowed(url, platform)
            && let Some(raw) = self
                .raw_cookies
                .get(platform)
                .filter(|raw| !raw.trim().is_empty())
        {
            pairs.push(raw.clone());
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        if let Some(cookies) = self.file_cookies.get(platform) {
            let host = url.host_str().unwrap_or_default();
            for cookie in cookies {
                let same_domain = host == cookie.domain
                    || cookie.subdomains && domain_matches(host, &cookie.domain);
                let path_match = url.path() == cookie.path
                    || url
                        .path()
                        .strip_prefix(&cookie.path)
                        .is_some_and(|tail| cookie.path.ends_with('/') || tail.starts_with('/'));
                if same_domain
                    && path_match
                    && (!cookie.secure || url.scheme() == "https")
                    && (cookie.expires == 0 || cookie.expires > now)
                {
                    pairs.push(cookie.pair.clone());
                }
            }
        }
        if pairs.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            HeaderValue::from_str(&pairs.join("; ")).context("Cookie 包含非法请求头字符")?,
        ))
    }

    /// Reads a selected session value from the same domain/path/expiry filtered
    /// Cookie header that will accompany the original-site request.
    pub(super) fn session_cookie(
        &self,
        raw_url: &str,
        platform: &str,
        names: &[&str],
    ) -> Result<Option<String>> {
        let url = validate_site_url(raw_url, platform)?;
        let Some(header) = self.cookie_header(&url, platform)? else {
            return Ok(None);
        };
        let header = header.to_str().context("Cookie 包含非法字符")?;
        Ok(names.iter().find_map(|name| {
            header.split(';').find_map(|pair| {
                let (key, value) = pair.trim().split_once('=')?;
                (*name == key && !value.is_empty()).then(|| value.to_owned())
            })
        }))
    }

    async fn request(
        &self,
        method: Method,
        raw: &str,
        platform: &str,
        options: RequestOptions<'_>,
    ) -> Result<SiteResponse> {
        let mut url = validate_site_url(raw, platform)?;
        let mut state = self.gates.get(platform).context("未知平台")?.lock().await;
        let now = Instant::now();
        if state.blocked_until > now {
            return Err(SiteAccessRestricted(format!(
                "{platform} 处于访问限制冷却期，请在 {} 秒后重试",
                state
                    .blocked_until
                    .duration_since(now)
                    .as_secs()
                    .saturating_add(1)
            ))
            .into());
        }
        let mut request_method = method;
        for _ in 0..=8 {
            let send = |client: &Client| {
                let mut request = client
                    .request(request_method.clone(), url.clone())
                    .headers(options.headers.clone());
                if !options.headers.contains_key(reqwest::header::USER_AGENT)
                    && matches!(platform, "douyin" | "kuaishou" | "xiaohongshu")
                {
                    request = request.header(reqwest::header::USER_AGENT, MOBILE_USER_AGENT);
                }
                if platform == "xiaohongshu"
                    && !options
                        .headers
                        .contains_key(reqwest::header::ACCEPT_LANGUAGE)
                {
                    request =
                        request.header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8");
                }
                if !options.headers.contains_key(REFERER) {
                    request =
                        request.header(REFERER, format!("{}/", url.origin().ascii_serialization()));
                }
                if let Some(cookie) = self.cookie_header(&url, platform)? {
                    request = request.header(COOKIE, cookie);
                }
                if request_method == Method::POST {
                    if let Some(body) = options.json {
                        request = request.json(body);
                    }
                    if let Some(form) = options.form {
                        request = request.form(form);
                    }
                }
                Ok::<_, anyhow::Error>(request)
            };
            let mut response = None;
            for attempt in 0..self.retries {
                tokio::time::sleep_until(state.next_request).await;
                let result = match send(&self.direct)?.send().await {
                    Err(_) if self.proxy.is_some() => {
                        send(self.proxy.as_ref().unwrap())?.send().await
                    }
                    result => result,
                };
                let jitter = if self.jitter_ms == 0 {
                    0
                } else {
                    rand::random::<u64>() % (self.jitter_ms + 1)
                };
                state.next_request =
                    Instant::now() + self.min_interval + Duration::from_millis(jitter);
                match result {
                    Ok(value) if value.status().is_server_error() && attempt + 1 < self.retries => {
                    }
                    Ok(value) => {
                        response = Some(value);
                        break;
                    }
                    Err(error) if attempt + 1 == self.retries => {
                        return Err(error.without_url().into());
                    }
                    Err(_) => {}
                }
                tokio::time::sleep(self.retry_interval.saturating_mul(1 << attempt)).await;
            }
            let response = response.context("平台请求失败")?;
            if response.status().is_redirection() {
                if options.headers.contains_key("x-secsdk-web-signature") {
                    bail!("抖音签名接口返回重定向，已停止请求");
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .context("重定向缺少 Location")?
                    .to_str()?;
                let next = url.join(location)?;
                if is_xiaohongshu_restricted_redirect(platform, &next) {
                    return Err(SiteAccessRestricted(
                        "小红书需要登录态或触发海外跳转验证，请更新 auth.cookies.xiaohongshu"
                            .into(),
                    )
                    .into());
                }
                url = validate_site_url(next.as_str(), platform)?;
                if options.stop_at.is_some_and(|predicate| predicate(&url)) {
                    return Ok(SiteResponse::RedirectTarget(url));
                }
                if matches!(response.status().as_u16(), 301..=303) {
                    request_method = Method::GET;
                }
                continue;
            }
            match response.status().as_u16() {
                401 | 403 | 412 => {
                    state.blocked_until = Instant::now() + self.cooldown;
                    return Err(SiteAccessRestricted(format!(
                        "{platform} 要求登录或触发访问验证（HTTP {}），已进入冷却，请配置 auth.cookies.{platform} 或 auth.cookieFiles.{platform}",
                        response.status().as_u16()
                    )).into());
                }
                429 => {
                    let retry_after = retry_after(response.headers().get(RETRY_AFTER));
                    let delay = retry_after.unwrap_or(self.cooldown);
                    state.blocked_until = Instant::now() + delay;
                    let hint = if retry_after.is_some() {
                        "按 Retry-After "
                    } else {
                        ""
                    };
                    return Err(SiteAccessRestricted(format!(
                        "{platform} 请求受限（HTTP 429），已{hint}冷却 {} 秒",
                        delay.as_secs()
                    ))
                    .into());
                }
                _ => {}
            }
            if !response.status().is_success() {
                bail!("{platform} 返回 HTTP {}", response.status().as_u16());
            }
            return Ok(SiteResponse::Final(response));
        }
        bail!("平台重定向次数过多")
    }

    async fn text_response(&self, response: Response) -> Result<String> {
        if response
            .content_length()
            .is_some_and(|size| size > self.max_bytes as u64)
        {
            bail!("平台响应超过大小上限");
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(reqwest::Error::without_url)?;
            if chunk.len() > self.max_bytes.saturating_sub(bytes.len()) {
                bail!("平台响应超过大小上限");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub async fn resolve_url(&self, url: &str, platform: &str) -> Result<String> {
        let response = self
            .request(Method::GET, url, platform, RequestOptions::default())
            .await?
            .into_final()?;
        Ok(response.url().to_string())
    }

    pub(super) async fn resolve_douyin_short(&self, url: &str) -> Result<String> {
        let result = self
            .request(
                Method::GET,
                url,
                "douyin",
                RequestOptions {
                    stop_at: Some(|url| super::sites_page::douyin_path_id(url).is_some()),
                    ..RequestOptions::default()
                },
            )
            .await?;
        Ok(match result {
            SiteResponse::Final(response) => response.url().to_string(),
            SiteResponse::RedirectTarget(target) => target.into(),
        })
    }

    pub(super) async fn resolve_xhs_short(&self, url: &str) -> Result<String> {
        let result = self
            .request(
                Method::GET,
                url,
                "xiaohongshu",
                RequestOptions {
                    stop_at: Some(|url| {
                        url.host_str().is_some_and(|host| {
                            domain_matches(host, "xiaohongshu.com")
                                || domain_matches(host, "rednote.com")
                        })
                    }),
                    ..RequestOptions::default()
                },
            )
            .await?;
        Ok(match result {
            SiteResponse::Final(response) => response.url().to_string(),
            SiteResponse::RedirectTarget(target) => target.into(),
        })
    }

    pub async fn get_text(&self, url: &str, platform: &str) -> Result<String> {
        self.get_text_headers(url, platform, HeaderMap::new()).await
    }

    pub async fn get_text_headers(
        &self,
        url: &str,
        platform: &str,
        headers: HeaderMap,
    ) -> Result<String> {
        let response = self
            .request(
                Method::GET,
                url,
                platform,
                RequestOptions {
                    headers,
                    ..RequestOptions::default()
                },
            )
            .await?
            .into_final()?;
        self.text_response(response).await
    }

    pub async fn get_json(&self, url: &str, platform: &str) -> Result<Value> {
        self.get_json_headers(url, platform, HeaderMap::new()).await
    }

    pub async fn get_json_headers(
        &self,
        url: &str,
        platform: &str,
        headers: HeaderMap,
    ) -> Result<Value> {
        let text = self.get_text_headers(url, platform, headers).await?;
        serde_json::from_str(&text).with_context(|| {
            SiteJsonUnavailable(format!(
                "{platform} 返回页面验证或无效 JSON，请检查登录状态"
            ))
        })
    }

    pub async fn post_json(&self, url: &str, platform: &str, body: &Value) -> Result<Value> {
        let response = self
            .request(
                Method::POST,
                url,
                platform,
                RequestOptions {
                    json: Some(body),
                    ..RequestOptions::default()
                },
            )
            .await?
            .into_final()?;
        Ok(serde_json::from_str(&self.text_response(response).await?)?)
    }

    pub async fn post_form(
        &self,
        url: &str,
        platform: &str,
        form: &[(String, String)],
    ) -> Result<Value> {
        let response = self
            .request(
                Method::POST,
                url,
                platform,
                RequestOptions {
                    form: Some(form),
                    ..RequestOptions::default()
                },
            )
            .await?
            .into_final()?;
        Ok(serde_json::from_str(&self.text_response(response).await?)?)
    }
}

fn retry_after(header: Option<&HeaderValue>) -> Option<Duration> {
    let text = header?.to_str().ok()?;
    if let Ok(seconds) = text.parse::<u64>() {
        return Some(Duration::from_secs(seconds.clamp(1, 86_400)));
    }
    let date = chrono::DateTime::parse_from_rfc2822(text).ok()?;
    Some(Duration::from_secs(
        (date.timestamp() - chrono::Utc::now().timestamp()).clamp(1, 86_400) as u64,
    ))
}

fn parse_netscape_cookies(content: &str) -> Result<Vec<Cookie>> {
    let mut cookies = Vec::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 7 {
            bail!("Cookie 文件应使用 Netscape 七列格式");
        }
        cookies.push(Cookie {
            domain: fields[0].trim_start_matches('.').to_ascii_lowercase(),
            subdomains: fields[1].eq_ignore_ascii_case("true"),
            path: fields[2].into(),
            secure: fields[3].eq_ignore_ascii_case("true"),
            expires: fields[4].parse().context("Cookie 过期时间无效")?,
            pair: format!("{}={}", fields[5], fields[6]),
        });
    }
    Ok(cookies)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_spoofed_hosts_and_credentials() {
        for url in [
            "https://bilibili.com.evil.test/video/BV1",
            "http://127.0.0.1/",
            "https://user@bilibili.com/",
            "https://bilibili.com:8443/",
        ] {
            assert!(validate_site_url(url, "bilibili").is_err());
        }
        assert!(
            validate_site_url("https://api.bilibili.com/x/web-interface/view", "bilibili").is_ok()
        );
    }

    #[test]
    fn cookies_respect_domain_path_and_expiry() {
        let mut http = SiteHttp::new(&AppConfig::default()).unwrap();
        http.raw_cookies
            .insert("twitter".into(), "auth_token=secret".into());
        assert!(
            http.cookie_header(
                &Url::parse("https://cdn.syndication.twimg.com/tweet-result").unwrap(),
                "twitter"
            )
            .unwrap()
            .is_none()
        );
        http.file_cookies.insert("bilibili".into(), parse_netscape_cookies("# Netscape HTTP Cookie File\n#HttpOnly_.bilibili.com\tTRUE\t/x\tTRUE\t0\ta\tb\n.bilibili.com\tTRUE\t/\tTRUE\t1\texpired\tx").unwrap());
        assert_eq!(
            http.cookie_header(
                &Url::parse("https://api.bilibili.com/x/test").unwrap(),
                "bilibili"
            )
            .unwrap()
            .unwrap(),
            "a=b"
        );
        assert!(
            http.cookie_header(
                &Url::parse("https://api.bilibili.com/xyz").unwrap(),
                "bilibili"
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn signing_session_values_use_the_outgoing_cookie_scope() {
        let mut http = SiteHttp::new(&AppConfig::default()).unwrap();
        http.raw_cookies.insert(
            "douyin".into(),
            "UIFID_TEMP=temporary; UIFID=primary; s_v_web_id=verify_example".into(),
        );
        let names = crate::parser::douyin_sign::UIFID_COOKIES;
        let endpoint = crate::parser::douyin_sign::DETAIL_ENDPOINT;
        assert_eq!(
            http.session_cookie(endpoint, "douyin", names).unwrap(),
            Some("primary".into())
        );
        assert_eq!(
            http.session_cookie(
                "https://www.iesdouyin.com/web/api/v2/aweme/slidesinfo/",
                "douyin",
                names,
            )
            .unwrap(),
            None
        );
        http.raw_cookies.clear();
        http.file_cookies.insert("douyin".into(), parse_netscape_cookies(
            "# Netscape HTTP Cookie File\nwww.douyin.com\tFALSE\t/aweme/\tTRUE\t0\tUIFID\tscoped\nwww.douyin.com\tFALSE\t/\tTRUE\t1\tUIFID_TEMP\texpired\n",
        ).unwrap());
        assert_eq!(
            http.session_cookie(endpoint, "douyin", names).unwrap(),
            Some("scoped".into())
        );
        assert_eq!(
            http.session_cookie("https://www.douyin.com/note/123", "douyin", names)
                .unwrap(),
            None
        );
    }

    #[test]
    fn upgrades_share_links_before_attaching_authentication() {
        let mut http = SiteHttp::new(&AppConfig::default()).unwrap();
        http.raw_cookies
            .insert("xiaohongshu".into(), "web_session=secret".into());
        let raw = Url::parse("http://www.xiaohongshu.com/explore/abc").unwrap();
        assert!(http.cookie_header(&raw, "xiaohongshu").unwrap().is_none());
        let secured = validate_site_url(raw.as_str(), "xiaohongshu").unwrap();
        assert_eq!(secured.scheme(), "https");
        assert!(
            http.cookie_header(&secured, "xiaohongshu")
                .unwrap()
                .is_some()
        );
        let rednote =
            validate_site_url("https://www.rednote.com/discovery/item/abc", "xiaohongshu").unwrap();
        assert!(
            http.cookie_header(&rednote, "xiaohongshu")
                .unwrap()
                .is_none()
        );
        assert!(validate_site_url("https://bili2233.cn/abc", "bilibili").is_ok());
    }

    #[test]
    fn honors_retry_after_seconds_and_dates() {
        assert_eq!(
            retry_after(Some(&HeaderValue::from_static("120"))),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            retry_after(Some(&HeaderValue::from_static("999999999"))),
            Some(Duration::from_secs(86_400))
        );
        assert!(
            retry_after(Some(&HeaderValue::from_static(
                "Wed, 21 Oct 2015 07:28:00 GMT"
            )))
            .is_some()
        );
        assert!(retry_after(Some(&HeaderValue::from_static("invalid"))).is_none());
    }

    #[tokio::test]
    async fn cooldown_stops_requests_before_network_io() {
        let http = SiteHttp::new(&AppConfig::default()).unwrap();
        http.cool_down("bilibili").await;
        assert!(http.is_cooling_down("bilibili").await);
        let result = http
            .get_json("https://api.bilibili.com/x/web-interface/view", "bilibili")
            .await;
        let error = result.unwrap_err();
        assert!(error.is::<SiteAccessRestricted>());
        assert!(error.to_string().contains("冷却期"));
        assert!(!http.is_cooling_down("douyin").await);
    }

    #[test]
    fn recognizes_xiaohongshu_rednote_and_login_redirects() {
        assert!(!is_xiaohongshu_restricted_redirect(
            "xiaohongshu",
            &Url::parse("https://www.rednote.com/discovery/item/abc").unwrap()
        ));
        assert!(is_xiaohongshu_restricted_redirect(
            "xiaohongshu",
            &Url::parse("https://www.rednote.com/login?redirectPath=/").unwrap()
        ));
        assert!(is_xiaohongshu_restricted_redirect(
            "xiaohongshu",
            &Url::parse("https://www.xiaohongshu.com/login?redirectPath=/").unwrap()
        ));
        assert!(is_xiaohongshu_restricted_redirect(
            "xiaohongshu",
            &Url::parse("https://www.xiaohongshu.com/website-login/captcha").unwrap()
        ));
        assert!(!is_xiaohongshu_restricted_redirect(
            "xiaohongshu",
            &Url::parse("https://www.xiaohongshu.com/discovery/item/abc").unwrap()
        ));
        assert!(!is_xiaohongshu_restricted_redirect(
            "douyin",
            &Url::parse("https://www.rednote.com/login").unwrap()
        ));
    }
}
