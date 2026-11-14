use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use qrcode::{Color, QrCode};
use reqwest::cookie::Jar;
use reqwest::header::{LOCATION, RETRY_AFTER, SET_COOKIE};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use tokio::time::Instant;
use url::Url;

use crate::parser::http::USER_AGENT;

const PASSPORT: &str = "https://passport.bilibili.com";
const NAV: &str = "https://api.bilibili.com/x/web-interface/nav";
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RESPONSE: usize = 512 * 1024;
const LEGACY_COOKIE_NAMES: &[&str] = &[
    "SESSDATA",
    "bili_jct",
    "DedeUserID",
    "DedeUserID__ckMd5",
    "sid",
];

pub struct LoginOptions {
    pub cookie_file: PathBuf,
    pub qr_file: PathBuf,
    pub timeout: Duration,
    pub proxy: String,
}

pub struct LoginOutcome {
    pub cookie_file: PathBuf,
    pub qr_file: PathBuf,
    pub verified: bool,
    pub verification_message: String,
}

#[derive(Clone)]
struct StoredCookie {
    domain: String,
    subdomains: bool,
    path: String,
    secure: bool,
    http_only: bool,
    expires: u64,
    name: String,
    value: String,
}

#[derive(Default)]
struct LoginCookies(BTreeMap<(String, String, String), StoredCookie>);

impl LoginCookies {
    fn collect(&mut self, response: &Response) {
        let origin = response.url().host_str().unwrap_or_default();
        let now = unix_seconds();
        for raw in response.headers().get_all(SET_COOKIE) {
            let Ok(raw) = raw.to_str() else {
                continue;
            };
            let Ok(cookie) = cookie::Cookie::parse(raw) else {
                continue;
            };
            let domain = cookie.domain().unwrap_or(origin).to_ascii_lowercase();
            let bare_domain = domain.trim_start_matches('.');
            if bare_domain != "bilibili.com" && !bare_domain.ends_with(".bilibili.com") {
                continue;
            }
            let expires = if let Some(age) = cookie.max_age() {
                let seconds = age.whole_seconds();
                if seconds <= 0 {
                    now
                } else {
                    now.saturating_add(seconds as u64)
                }
            } else {
                cookie
                    .expires_datetime()
                    .map(|time| time.unix_timestamp().max(1) as u64)
                    .unwrap_or(0)
            };
            let stored = StoredCookie {
                domain: bare_domain.into(),
                subdomains: cookie.domain().is_some(),
                path: cookie.path().unwrap_or("/").into(),
                secure: cookie.secure().unwrap_or(false),
                http_only: cookie.http_only().unwrap_or(false),
                expires,
                name: cookie.name().into(),
                value: cookie.value().into(),
            };
            let key = (
                stored.domain.clone(),
                stored.path.clone(),
                stored.name.clone(),
            );
            if stored.value.is_empty() || stored.expires > 0 && stored.expires <= now {
                self.0.remove(&key);
            } else {
                self.0.insert(key, stored);
            }
        }
    }

    fn add_legacy_query(&mut self, url: &Url) {
        let fields: BTreeMap<&str, &str> = url
            .query()
            .unwrap_or_default()
            .split('&')
            .filter_map(|field| field.split_once('='))
            .collect();
        let expires = fields
            .get("Expires")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        for name in LEGACY_COOKIE_NAMES {
            if let Some(value) = fields.get(name).filter(|value| !value.is_empty()) {
                // Legacy crossDomain values are already Cookie-header encoded.
                // Keeping the raw query value preserves SESSDATA percent escapes.
                let key = ("bilibili.com".into(), "/".into(), (*name).into());
                self.0.entry(key).or_insert_with(|| StoredCookie {
                    domain: "bilibili.com".into(),
                    subdomains: true,
                    path: "/".into(),
                    secure: true,
                    http_only: *name == "SESSDATA",
                    expires,
                    name: (*name).into(),
                    value: (*value).into(),
                });
            }
        }
    }

    fn has_required(&self) -> bool {
        let now = unix_seconds();
        ["SESSDATA", "bili_jct"].iter().all(|name| {
            self.0.values().any(|cookie| {
                cookie.name == *name
                    && !cookie.value.is_empty()
                    && (cookie.expires == 0 || cookie.expires > now)
                    && cookie.path == "/"
                    && (cookie.domain == "api.bilibili.com"
                        || cookie.subdomains && cookie.domain == "bilibili.com")
            })
        })
    }

    fn seed_jar(&self, jar: &Jar) -> Result<()> {
        let api = Url::parse(NAV)?;
        let now = unix_seconds();
        for cookie in self
            .0
            .values()
            .filter(|cookie| cookie.expires == 0 || cookie.expires > now)
        {
            let value = format!(
                "{}={}; Domain={}; Path={}; {}",
                cookie.name,
                cookie.value,
                cookie.domain,
                cookie.path,
                if cookie.secure { "Secure" } else { "" }
            );
            jar.add_cookie_str(&value, &api);
        }
        Ok(())
    }

    fn netscape(&self) -> Result<String> {
        let mut output =
            String::from("# Netscape HTTP Cookie File\n# AllVideo Bilibili QR login\n");
        let now = unix_seconds();
        for cookie in self
            .0
            .values()
            .filter(|cookie| cookie.expires == 0 || cookie.expires > now)
        {
            for field in [&cookie.domain, &cookie.path, &cookie.name, &cookie.value] {
                ensure!(
                    !field.contains(['\t', '\r', '\n']),
                    "登录 Cookie 含非法文件分隔符"
                );
            }
            let prefix = if cookie.http_only { "#HttpOnly_" } else { "" };
            let domain_prefix = if cookie.subdomains { "." } else { "" };
            output.push_str(&format!(
                "{prefix}{domain_prefix}{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                cookie.domain,
                if cookie.subdomains { "TRUE" } else { "FALSE" },
                cookie.path,
                if cookie.secure { "TRUE" } else { "FALSE" },
                cookie.expires,
                cookie.name,
                cookie.value
            ));
        }
        Ok(output)
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Runs the original site's web QR flow and saves only a completed login.
pub async fn login(options: LoginOptions) -> Result<LoginOutcome> {
    ensure!(!options.timeout.is_zero(), "扫码等待时间必须大于零");
    ensure!(
        output_path_identity(&options.cookie_file)? != output_path_identity(&options.qr_file)?,
        "Cookie 文件与二维码图片需使用不同路径"
    );
    let jar = Arc::new(Jar::default());
    let mut builder = Client::builder()
        .no_proxy()
        .user_agent(USER_AGENT)
        .cookie_provider(Arc::clone(&jar))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(2);
    if !options.proxy.trim().is_empty() {
        builder =
            builder.proxy(reqwest::Proxy::all(&options.proxy).context("B站登录代理格式错误")?);
    }
    let client = builder.build().context("建立 B站登录 HTTP 客户端失败")?;
    login_with_client(options, &client, &jar, PASSPORT, NAV, POLL_INTERVAL).await
}

async fn login_with_client(
    options: LoginOptions,
    client: &Client,
    jar: &Jar,
    passport: &str,
    nav: &str,
    poll_interval: Duration,
) -> Result<LoginOutcome> {
    let mut cookies = LoginCookies::default();
    let generate = client
        .get(format!("{passport}/x/passport-login/web/qrcode/generate"))
        .header("Referer", "https://www.bilibili.com/")
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .context("申请 B站登录二维码失败")?;
    check_status(generate.status(), "申请二维码")?;
    cookies.collect(&generate);
    let generated = bounded_json(generate).await?;
    check_business_code(&generated)?;
    let qr_url = generated
        .pointer("/data/url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty() && url.len() <= 8192)
        .context("B站未返回二维码地址")?;
    let qr_url = Url::parse(qr_url).context("B站二维码地址无效")?;
    validate_login_url(&qr_url)?;
    let key = generated
        .pointer("/data/qrcode_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty() && key.len() <= 256)
        .context("B站未返回二维码密钥")?;
    let code = QrCode::new(qr_url.as_str().as_bytes()).context("生成 B站二维码失败")?;
    save_qr_png(&code, &options.qr_file)?;
    eprintln!("B站登录二维码已保存 {}", options.qr_file.display());
    eprintln!("请使用哔哩哔哩手机客户端扫码并确认登录");

    let deadline = Instant::now() + options.timeout;
    let poll_url = format!("{passport}/x/passport-login/web/qrcode/poll");
    let mut last_state = None;
    let mut network_errors = 0u8;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "B站扫码登录等待超时，请重新申请二维码"
        );
        tokio::time::sleep(poll_interval.min(remaining)).await;
        ensure!(
            Instant::now() < deadline,
            "B站扫码登录等待超时，请重新申请二维码"
        );
        let response = match client
            .get(&poll_url)
            .query(&[("qrcode_key", key)])
            .header("Referer", "https://passport.bilibili.com/")
            .timeout(
                Duration::from_secs(15).min(deadline.saturating_duration_since(Instant::now())),
            )
            .send()
            .await
        {
            Ok(response) => {
                network_errors = 0;
                response
            }
            Err(_) => {
                network_errors += 1;
                ensure!(
                    network_errors < 3,
                    "B站扫码状态请求连续失败，请稍后重新登录"
                );
                eprintln!("B站扫码状态请求暂时失败，稍后重试");
                continue;
            }
        };
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            let delay = retry_after(&response).unwrap_or(Duration::from_secs(30));
            ensure!(
                Instant::now() + delay < deadline,
                "B站扫码状态查询受到限流，请稍后重新申请二维码"
            );
            eprintln!("B站扫码状态查询受到限流，暂停 {} 秒", delay.as_secs());
            tokio::time::sleep(delay).await;
            continue;
        }
        check_status(response.status(), "查询扫码状态")?;
        cookies.collect(&response);
        let polled = bounded_json(response).await?;
        check_business_code(&polled)?;
        let state = polled
            .pointer("/data/code")
            .and_then(Value::as_i64)
            .context("B站扫码状态响应缺少状态码")?;
        match state {
            86101 | 86090 => {
                if last_state != Some(state) {
                    eprintln!(
                        "{}",
                        if state == 86101 {
                            "等待扫码"
                        } else {
                            "二维码已扫码，请在手机上确认登录"
                        }
                    );
                    last_state = Some(state);
                }
            }
            86038 => bail!("B站登录二维码已失效，请重新申请"),
            0 => {
                if let Some(raw) = polled
                    .pointer("/data/url")
                    .and_then(Value::as_str)
                    .filter(|url| !url.is_empty())
                {
                    let url = Url::parse(raw).context("B站登录确认地址无效")?;
                    validate_login_url(&url)?;
                    cookies.add_legacy_query(&url);
                    if !cookies.has_required() {
                        follow_cross_domain(client, url, &mut cookies).await?;
                    }
                }
                ensure!(
                    cookies.has_required(),
                    "扫码已确认，但未获得完整登录 Cookie，请重新登录"
                );
                cookies.seed_jar(jar)?;
                let verification = verify_login(client, nav, &mut cookies).await?;
                ensure!(
                    cookies.has_required(),
                    "原站校验清除了登录 Cookie，请重新扫码"
                );
                atomic_write(&options.cookie_file, cookies.netscape()?.as_bytes())?;
                return Ok(LoginOutcome {
                    cookie_file: options.cookie_file,
                    qr_file: options.qr_file,
                    verified: verification.0,
                    verification_message: verification.1,
                });
            }
            _ => bail!("B站返回未知扫码状态码 {state}，登录 Cookie 未保存"),
        }
    }
}

fn validate_login_url(url: &Url) -> Result<()> {
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && matches!(
                url.host_str(),
                Some(
                    "passport.bilibili.com"
                        | "account.bilibili.com"
                        | "passport.biligame.com"
                        | "www.bilibili.com"
                )
            ),
        "B站返回了非可信 HTTPS 登录地址"
    );
    Ok(())
}

async fn follow_cross_domain(
    client: &Client,
    mut url: Url,
    cookies: &mut LoginCookies,
) -> Result<()> {
    for _ in 0..5 {
        validate_login_url(&url)?;
        let response = client
            .get(url.clone())
            .header("Referer", "https://passport.bilibili.com/")
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("获取 B站跨域登录 Cookie 失败")?;
        if !response.status().is_redirection() {
            check_status(response.status(), "获取跨域登录 Cookie")?;
        }
        cookies.collect(&response);
        if cookies.has_required() {
            return Ok(());
        }
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .context("B站登录跳转缺少目标")?
                .to_str()
                .context("B站登录跳转目标无效")?;
            url = url.join(location).context("B站登录跳转地址无效")?;
            continue;
        }
        return Ok(());
    }
    bail!("B站跨域登录跳转次数过多")
}

fn check_status(status: StatusCode, action: &str) -> Result<()> {
    match status.as_u16() {
        200..=299 => Ok(()),
        401 | 403 | 412 => bail!(
            "B站{action}遇到访问验证（HTTP {}），请完成原站验证后重试",
            status.as_u16()
        ),
        429 => bail!("B站{action}受到限流，请稍后重试"),
        code => bail!("B站{action}返回 HTTP {code}"),
    }
}

fn check_business_code(data: &Value) -> Result<()> {
    let code = data
        .get("code")
        .and_then(Value::as_i64)
        .context("B站登录响应缺少返回码")?;
    ensure!(code == 0, "B站登录接口返回错误码 {code}");
    Ok(())
}

async fn bounded_json(response: Response) -> Result<Value> {
    ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= MAX_RESPONSE as u64),
        "B站登录响应超过大小上限"
    );
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(reqwest::Error::without_url)?;
        ensure!(
            chunk.len() <= MAX_RESPONSE.saturating_sub(bytes.len()),
            "B站登录响应超过大小上限"
        );
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("B站登录接口返回无效 JSON 或访问验证页面")
}

fn retry_after(response: &Response) -> Option<Duration> {
    let raw = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds.clamp(2, 3600)));
    }
    let timestamp = chrono::DateTime::parse_from_rfc2822(raw).ok()?.timestamp();
    Some(Duration::from_secs(
        (timestamp - unix_seconds() as i64).clamp(2, 3600) as u64,
    ))
}

async fn verify_login(
    client: &Client,
    nav: &str,
    cookies: &mut LoginCookies,
) -> Result<(bool, String)> {
    let response = match client
        .get(nav)
        .header("Referer", "https://www.bilibili.com/")
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            return Ok((
                false,
                "扫码已确认，登录 Cookie 已保存；网络暂时阻止登录态校验".into(),
            ));
        }
    };
    cookies.collect(&response);
    if !response.status().is_success() {
        return Ok((
            false,
            format!(
                "扫码已确认，登录 Cookie 已保存；原站 HTTP {} 阻止登录态校验",
                response.status().as_u16()
            ),
        ));
    }
    let data = match bounded_json(response).await {
        Ok(data) => data,
        Err(_) => {
            return Ok((
                false,
                "扫码已确认，登录 Cookie 已保存；原站验证页面阻止登录态校验".into(),
            ));
        }
    };
    let code = data.get("code").and_then(Value::as_i64);
    if code == Some(-101)
        || code == Some(0) && data.pointer("/data/isLogin").and_then(Value::as_bool) == Some(false)
    {
        bail!("扫码已确认，但原站明确拒绝该登录态；原有 Cookie 文件已保留，请重新扫码");
    }
    if code == Some(0) && data.pointer("/data/isLogin").and_then(Value::as_bool) == Some(true) {
        return Ok((true, "B站登录态校验通过，Cookie 已保存".into()));
    }
    Ok((
        false,
        "扫码已确认，登录 Cookie 已保存；原站未提供可确认的登录态校验结果".into(),
    ))
}

fn save_qr_png(code: &QrCode, path: &Path) -> Result<()> {
    let modules = code.width();
    let pixels = (modules + 8) * 8;
    let mut data = vec![255u8; pixels * pixels];
    for row in 0..modules {
        for column in 0..modules {
            if code[(column, row)] == Color::Dark {
                for y in (row + 4) * 8..(row + 5) * 8 {
                    data[y * pixels + (column + 4) * 8..y * pixels + (column + 5) * 8].fill(0);
                }
            }
        }
    }
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, pixels as u32, pixels as u32);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&data)?;
    }
    atomic_write(path, &encoded)
}

struct PendingFile(PathBuf);
impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn output_path_identity(path: &Path) -> Result<PathBuf> {
    let absolute = std::env::current_dir()
        .context("解析登录文件路径失败")?
        .join(path);
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => {
                normalized.push(component.as_os_str());
                if let Ok(canonical) = std::fs::canonicalize(&normalized) {
                    normalized = canonical;
                }
            }
        }
    }
    #[cfg(windows)]
    {
        normalized = PathBuf::from(normalized.to_string_lossy().to_lowercase());
    }
    Ok(normalized)
}

fn atomic_write(target: &Path, bytes: &[u8]) -> Result<()> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent).context("创建登录文件目录失败")?;
    }
    let temporary = parent.join(format!(
        ".allvideo-login-{:032x}.tmp",
        rand::random::<u128>()
    ));
    let pending = PendingFile(temporary.clone());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).context("创建私有登录文件失败")?;
    file.write_all(bytes).context("写入登录文件失败")?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, target).context("原子更新登录文件失败")?;
    drop(pending);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn mock_login(
        states: Vec<Value>,
        cookie_headers: Vec<&'static str>,
        nav_status: StatusCode,
        nav_body: Value,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let polls = Arc::new(AtomicUsize::new(0));
        let state_counter = Arc::clone(&polls);
        let app = Router::new()
            .route("/x/passport-login/web/qrcode/generate", get(|| async {
                Json(json!({"code":0,"data":{"url":"https://passport.bilibili.com/h5-app/passport/login/scan?qrcode_key=test-key","qrcode_key":"test-key"}}))
            }))
            .route("/x/passport-login/web/qrcode/poll", get(move || {
                let index = state_counter.fetch_add(1, Ordering::Relaxed);
                let body = states.get(index).unwrap_or_else(|| states.last().unwrap()).clone();
                let mut headers = HeaderMap::new();
                if body.pointer("/data/code").and_then(Value::as_i64) == Some(0) {
                    for value in &cookie_headers { headers.append(SET_COOKIE, (*value).parse().unwrap()); }
                }
                async move { (headers, Json(body)) }
            }))
            .route("/nav", get(move || {
                let body = nav_body.clone();
                async move { (nav_status, Json(body)) }
            }));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, polls, server)
    }

    fn test_options(root: &Path) -> LoginOptions {
        LoginOptions {
            cookie_file: root.join("bilibili.txt"),
            qr_file: root.join("login.png"),
            timeout: Duration::from_secs(2),
            proxy: String::new(),
        }
    }

    fn completed_state() -> Value {
        json!({"code":0,"data":{"code":0,"url":"","refresh_token":"test-refresh"}})
    }

    const AUTH_HEADERS: [&str; 2] = [
        "SESSDATA=session%2Ctoken; Domain=.bilibili.com; Path=/; Secure; HttpOnly; Max-Age=3600",
        "bili_jct=csrf-value; Domain=.bilibili.com; Path=/; Secure; Max-Age=3600",
    ];

    #[tokio::test]
    async fn completed_scan_saves_cookies_even_when_nav_is_restricted() {
        let (base, polls, server) = mock_login(
            vec![
                json!({"code":0,"data":{"code":86101}}),
                json!({"code":0,"data":{"code":86090}}),
                completed_state(),
            ],
            AUTH_HEADERS.to_vec(),
            StatusCode::PRECONDITION_FAILED,
            json!({}),
        )
        .await;
        let root = tempfile::tempdir().unwrap();
        let options = test_options(root.path());
        let jar = Arc::new(Jar::default());
        let client = Client::builder()
            .no_proxy()
            .cookie_provider(Arc::clone(&jar))
            .build()
            .unwrap();
        let outcome = login_with_client(
            options,
            &client,
            &jar,
            &base,
            &format!("{base}/nav"),
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        assert!(!outcome.verified);
        assert!(outcome.verification_message.contains("412"));
        assert_eq!(polls.load(Ordering::Relaxed), 3);
        let saved = std::fs::read_to_string(outcome.cookie_file).unwrap();
        assert!(saved.contains("#HttpOnly_.bilibili.com\tTRUE\t/\tTRUE\t"));
        assert!(saved.contains("SESSDATA\tsession%2Ctoken"));
        assert!(saved.contains("bili_jct\tcsrf-value"));
        server.abort();
    }

    #[tokio::test]
    async fn rejects_incomplete_expired_unknown_and_invalidated_logins_without_replacing_old_cookie()
     {
        for (states, headers, nav) in [
            (
                vec![completed_state()],
                vec![AUTH_HEADERS[0]],
                json!({"code":0,"data":{"isLogin":true}}),
            ),
            (
                vec![json!({"code":0,"data":{"code":86038}})],
                AUTH_HEADERS.to_vec(),
                json!({"code":0,"data":{"isLogin":true}}),
            ),
            (
                vec![json!({"code":0,"data":{"code":99999}})],
                AUTH_HEADERS.to_vec(),
                json!({"code":0,"data":{"isLogin":true}}),
            ),
            (
                vec![completed_state()],
                AUTH_HEADERS.to_vec(),
                json!({"code":-101}),
            ),
        ] {
            let (base, _, server) = mock_login(states, headers, StatusCode::OK, nav).await;
            let root = tempfile::tempdir().unwrap();
            let options = test_options(root.path());
            std::fs::write(&options.cookie_file, "previous-valid-cookie").unwrap();
            let jar = Arc::new(Jar::default());
            let client = Client::builder()
                .no_proxy()
                .cookie_provider(Arc::clone(&jar))
                .build()
                .unwrap();
            assert!(
                login_with_client(
                    options,
                    &client,
                    &jar,
                    &base,
                    &format!("{base}/nav"),
                    Duration::from_millis(1)
                )
                .await
                .is_err()
            );
            assert_eq!(
                std::fs::read_to_string(root.path().join("bilibili.txt")).unwrap(),
                "previous-valid-cookie"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn modern_cookie_headers_respect_max_age_deletion_and_scope() {
        let (base, _, server) = mock_login(vec![completed_state()], vec![
            AUTH_HEADERS[0], AUTH_HEADERS[1],
            "SESSDATA=deleted; Domain=.bilibili.com; Path=/; Max-Age=-1; Expires=Wed, 01 Jan 2098 00:00:00 GMT",
            "private=other-site; Domain=example.com; Path=/",
            "sid=short-lived; Domain=.bilibili.com; Path=/; Max-Age=120",
        ], StatusCode::OK, json!({})).await;
        let response = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{base}/x/passport-login/web/qrcode/poll"))
            .send()
            .await
            .unwrap();
        let mut cookies = LoginCookies::default();
        cookies.collect(&response);
        assert!(!cookies.has_required());
        let saved = cookies.netscape().unwrap();
        assert!(!saved.contains("SESSDATA"));
        assert!(!saved.contains("other-site"));
        assert!(saved.contains("bili_jct\tcsrf-value"));
        let sid = cookies
            .0
            .values()
            .find(|cookie| cookie.name == "sid")
            .unwrap();
        assert!(sid.expires >= unix_seconds() + 110);
        assert!(sid.expires <= unix_seconds() + 120);
        server.abort();
    }

    #[test]
    fn legacy_query_keeps_cookie_encoding_and_rejects_empty_credentials() {
        let mut cookies = LoginCookies::default();
        let url = Url::parse("https://passport.biligame.com/crossDomain?SESSDATA=sess%2Cdata%3D&bili_jct=jct&DedeUserID=123&gourl=https%3A%2F%2Fwww.bilibili.com").unwrap();
        cookies.add_legacy_query(&url);
        assert!(cookies.has_required());
        let output = cookies.netscape().unwrap();
        assert!(output.contains("sess%2Cdata%3D"));
        assert!(!output.contains("gourl"));
        let mut empty = LoginCookies::default();
        empty.add_legacy_query(
            &Url::parse("https://passport.bilibili.com/?SESSDATA=&bili_jct=j").unwrap(),
        );
        assert!(!empty.has_required());
    }

    #[test]
    fn login_redirect_whitelist_rejects_credentials_and_other_hosts() {
        assert!(
            validate_login_url(
                &Url::parse(
                    "https://account.bilibili.com/h5/account-h5/auth/scan-web?code=fixture"
                )
                .unwrap()
            )
            .is_ok()
        );
        for raw in [
            "http://passport.bilibili.com/",
            "https://passport.bilibili.com.evil.test/",
            "https://user@passport.bilibili.com/",
            "https://passport.bilibili.com:8443/",
        ] {
            assert!(validate_login_url(&Url::parse(raw).unwrap()).is_err());
        }
        assert!(
            validate_login_url(
                &Url::parse(
                    "https://passport.biligame.com/x/passport-login/web/crossDomain?ticket=example"
                )
                .unwrap()
            )
            .is_ok()
        );
    }

    #[test]
    fn output_paths_identify_aliases_before_replacing_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let cookie = directory.path().join("bilibili.txt");
        std::fs::write(&cookie, b"previous-cookie").unwrap();
        let aliases = [
            directory.path().join(".").join("bilibili.txt"),
            directory
                .path()
                .join("not-created")
                .join("..")
                .join("bilibili.txt"),
        ];
        for alias in aliases {
            assert_eq!(
                output_path_identity(&cookie).unwrap(),
                output_path_identity(&alias).unwrap()
            );
        }
        assert_ne!(
            output_path_identity(&cookie).unwrap(),
            output_path_identity(&directory.path().join("login.png")).unwrap()
        );
        #[cfg(windows)]
        assert_eq!(
            output_path_identity(&cookie).unwrap(),
            output_path_identity(&directory.path().join("BILIBILI.TXT")).unwrap()
        );
        assert_eq!(std::fs::read(&cookie).unwrap(), b"previous-cookie");
    }

    #[cfg(unix)]
    #[test]
    fn output_paths_resolve_existing_parent_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let actual = directory.path().join("actual");
        std::fs::create_dir(&actual).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        assert_eq!(
            output_path_identity(&actual.join("bilibili.txt")).unwrap(),
            output_path_identity(&alias.join("bilibili.txt")).unwrap()
        );
    }

    #[tokio::test]
    async fn login_rejects_cookie_qr_alias_without_changing_existing_cookie() {
        let directory = tempfile::tempdir().unwrap();
        let cookie = directory.path().join("bilibili.txt");
        std::fs::write(&cookie, b"previous-cookie").unwrap();
        let result = login(LoginOptions {
            cookie_file: cookie.clone(),
            qr_file: directory.path().join(".").join("bilibili.txt"),
            timeout: Duration::from_secs(1),
            proxy: String::new(),
        })
        .await;
        assert!(result.err().unwrap().to_string().contains("不同路径"));
        assert_eq!(std::fs::read(cookie).unwrap(), b"previous-cookie");
    }

    #[test]
    fn atomic_cookie_replace_is_complete_and_qr_is_png() {
        let directory = tempfile::tempdir().unwrap();
        let cookie = directory.path().join("bilibili.txt");
        atomic_write(&cookie, b"old").unwrap();
        atomic_write(&cookie, b"new-complete").unwrap();
        assert_eq!(std::fs::read(&cookie).unwrap(), b"new-complete");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&cookie).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let qr = directory.path().join("login.png");
        save_qr_png(
            &QrCode::new(b"https://passport.bilibili.com/example").unwrap(),
            &qr,
        )
        .unwrap();
        assert!(std::fs::read(qr).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn requires_explicit_business_success() {
        assert!(check_business_code(&json!({"code":0})).is_ok());
        assert!(check_business_code(&json!({"message":"ok"})).is_err());
        assert!(check_business_code(&json!({"code":-1})).is_err());
    }
}
