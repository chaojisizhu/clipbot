use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::multipart::{Form, Part};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::parser::api::download_media_buffer;
use crate::types::{GptDetectConfig, QQImageAttachment};

pub const GPT_DETECT_WAITING_TEXT: &str = "正在检测OpenAI水印";
pub const GPT_DETECT_BUSY_TEXT: &str = "有任务在检测，请稍后";

static EXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.(png|jpe?g|webp)$").unwrap());
static KEY_CURSOR: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct GptDetectError {
    pub message: String,
    pub status: Option<u16>,
    pub code: Option<String>,
}

impl GptDetectError {
    pub fn new(message: impl Into<String>, status: Option<u16>, code: Option<&str>) -> Self {
        Self {
            message: message.into(),
            status,
            code: code.map(ToString::to_string),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct GptDetectResult {
    pub detected: bool,
    pub evidence: Vec<String>,
    pub model: Option<String>,
    pub generated_at: Option<String>,
}

pub fn matches_gpt_detect_keyword(text: &str, keywords: &[String]) -> bool {
    let normalized = text.trim();
    keywords.iter().any(|keyword| keyword.trim() == normalized)
}

fn normalize_content_type(content_type: &str, url: &str) -> Result<String, GptDetectError> {
    let normalized = content_type
        .to_ascii_lowercase()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if matches!(
        normalized.as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/webp"
    ) {
        return Ok(if normalized == "image/jpg" {
            "image/jpeg".into()
        } else {
            normalized
        });
    }
    if Regex::new(r"(?i)\.png(?:[?#]|$)").unwrap().is_match(url) {
        return Ok("image/png".into());
    }
    if Regex::new(r"(?i)\.webp(?:[?#]|$)").unwrap().is_match(url) {
        return Ok("image/webp".into());
    }
    if Regex::new(r"(?i)\.jpe?g(?:[?#]|$)").unwrap().is_match(url) {
        return Ok("image/jpeg".into());
    }
    Err(GptDetectError::new(
        "仅支持 PNG、JPEG、WebP 图片",
        None,
        Some("INVALID_IMAGE"),
    ))
}

fn file_name_for(image: &QQImageAttachment, content_type: &str) -> String {
    let original = image.filename.as_deref().unwrap_or_default();
    if EXT_RE.is_match(original) {
        return original.to_string();
    }
    let extension = match content_type {
        "image/png" => "png",
        "image/webp" => "webp",
        _ => "jpg",
    };
    let hash = hex::encode(Sha256::digest(image.url.as_bytes()));
    format!("quoted-{}.{}", &hash[..12], extension)
}

fn friendly_api_error(status: u16, data: &Value) -> GptDetectError {
    let upstream_code = data
        .pointer("/error/code")
        .or_else(|| data.pointer("/error/type"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let code = upstream_code.as_deref();
    match status {
        400 => GptDetectError::new(
            "图片格式不受支持或文件无效（仅支持 PNG、JPEG、WebP）",
            Some(status),
            Some(code.unwrap_or("INVALID_REQUEST")),
        ),
        401 | 403 => GptDetectError::new(
            "OpenAI API Key 无效或无权调用溯源接口",
            Some(status),
            Some(code.unwrap_or("AUTH_ERROR")),
        ),
        404 => GptDetectError::new(
            "当前 OpenAI 组织未开通 Content Provenance API",
            Some(status),
            Some(code.unwrap_or("ACCESS_NOT_FOUND")),
        ),
        429 => GptDetectError::new(
            "OpenAI 溯源接口请求频率超限，请稍后重试",
            Some(status),
            Some(code.unwrap_or("RATE_LIMIT")),
        ),
        500..=599 => GptDetectError::new(
            "OpenAI 溯源接口暂时不可用，请稍后重试",
            Some(status),
            Some(code.unwrap_or("UPSTREAM_ERROR")),
        ),
        _ => GptDetectError::new(
            "OpenAI 溯源接口请求失败，请稍后重试",
            Some(status),
            Some(code.unwrap_or("UNKNOWN_ERROR")),
        ),
    }
}

fn to_result(data: &Value) -> GptDetectResult {
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let c2pa = results
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("c2pa"));
    let synthid = results
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("synthid"));
    let c2pa_detected = c2pa
        .and_then(|item| item.get("outcome"))
        .and_then(Value::as_str)
        == Some("detected");
    let synthid_detected = synthid
        .and_then(|item| item.get("outcome"))
        .and_then(Value::as_str)
        == Some("detected");
    let mut evidence = Vec::new();
    if c2pa_detected {
        let issuer = c2pa
            .and_then(|item| item.get("issuer"))
            .and_then(Value::as_str)
            .map(|issuer| format!("，签发方 {issuer}"))
            .unwrap_or_default();
        evidence.push(format!("C2PA Content Credentials{issuer}"));
    }
    if synthid_detected {
        evidence.push("SynthID 水印".into());
    }
    let model = c2pa
        .and_then(|item| item.get("model"))
        .or_else(|| synthid.and_then(|item| item.get("model")))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let generated_at = c2pa
        .and_then(|item| item.get("generated_at"))
        .or_else(|| synthid.and_then(|item| item.get("generated_at")))
        .and_then(Value::as_str)
        .map(ToString::to_string);

    GptDetectResult {
        detected: c2pa_detected || synthid_detected,
        evidence,
        model,
        generated_at,
    }
}

fn build_client(config: &GptDetectConfig) -> Result<reqwest::Client, GptDetectError> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_millis(config.timeout.max(1)))
        .danger_accept_invalid_certs(false);
    if !config.proxy.is_empty() {
        let proxy = reqwest::Proxy::all(&config.proxy).map_err(|_| {
            GptDetectError::new(
                format!("无效的 gptDetect.proxy: {}", config.proxy),
                None,
                Some("CONFIG_ERROR"),
            )
        })?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|err| {
        GptDetectError::new(
            format!("创建 OpenAI HTTP 客户端失败: {err}"),
            None,
            Some("NETWORK_ERROR"),
        )
    })
}

fn push_api_key(keys: &mut Vec<String>, api_key: &str) {
    let api_key = api_key.trim();
    if !api_key.is_empty() && !keys.iter().any(|existing| existing == api_key) {
        keys.push(api_key.to_string());
    }
}

fn effective_api_keys(config: &GptDetectConfig) -> Vec<String> {
    let mut keys = Vec::new();
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        push_api_key(&mut keys, &api_key);
    }
    if let Ok(api_keys) = std::env::var("OPENAI_API_KEYS") {
        for api_key in api_keys.split([',', ';', '\n']) {
            push_api_key(&mut keys, api_key);
        }
    }
    push_api_key(&mut keys, &config.api_key);
    for api_key in &config.api_keys {
        push_api_key(&mut keys, api_key);
    }
    keys
}

fn is_rate_limit_error(error: &GptDetectError) -> bool {
    error.status == Some(429) || error.code.as_deref() == Some("RATE_LIMIT")
}

async fn request_with_key(
    client: &reqwest::Client,
    endpoint: &str,
    file_name: &str,
    content_type: &str,
    buffer: &[u8],
    api_key: &str,
    proxy_hint: &str,
) -> Result<GptDetectResult, GptDetectError> {
    let mut last_error = None;
    for attempt in 1..=3 {
        let part = Part::bytes(buffer.to_vec())
            .file_name(file_name.to_string())
            .mime_str(content_type)
            .map_err(|err| {
                GptDetectError::new(
                    format!("构造图片上传请求失败: {err}"),
                    None,
                    Some("INVALID_REQUEST"),
                )
            })?;
        let form = Form::new().part("file", part);
        match client
            .post(endpoint)
            .bearer_auth(api_key)
            .multipart(form)
            .send()
            .await
        {
            Ok(response) => {
                let status = response.status();
                let data: Value = response.json().await.unwrap_or(Value::Null);
                if !status.is_success() {
                    return Err(friendly_api_error(status.as_u16(), &data));
                }
                return Ok(to_result(&data));
            }
            Err(err) => {
                last_error = Some(err.to_string());
                let retryable = err.is_timeout() || err.is_connect() || err.is_request();
                if !retryable || attempt == 3 {
                    break;
                }
                let wait_ms = attempt * 1500;
                warn!(
                    "OpenAI 溯源请求网络异常，{wait_ms}ms 后重试 ({attempt}/{})",
                    3 - 1
                );
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
        }
    }

    Err(GptDetectError::new(
        format!(
            "无法连接 OpenAI 溯源接口{proxy_hint}: {}（已尝试 3 次）",
            last_error.unwrap_or_else(|| "未知网络错误".into())
        ),
        None,
        Some("NETWORK_ERROR"),
    ))
}

pub async fn detect_openai_image(
    image: &QQImageAttachment,
    config: &GptDetectConfig,
) -> Result<GptDetectResult, GptDetectError> {
    let api_keys = effective_api_keys(config);
    if api_keys.is_empty() {
        return Err(GptDetectError::new(
            "未配置 gptDetect.apiKey / apiKeys 或 OPENAI_API_KEY / OPENAI_API_KEYS",
            None,
            Some("CONFIG_ERROR"),
        ));
    }

    let content_type = normalize_content_type(&image.content_type, &image.url)?;
    let max_bytes = config.max_image_size_mb.max(1) * 1024 * 1024;
    if image.size.is_some_and(|size| size > max_bytes) {
        return Err(GptDetectError::new(
            format!("图片超过 {}MB 限制", config.max_image_size_mb.max(1)),
            None,
            Some("FILE_TOO_LARGE"),
        ));
    }
    let proxies = if config.proxy.is_empty() {
        Vec::new()
    } else {
        vec![config.proxy.clone()]
    };
    let buffer = download_media_buffer(&image.url, &proxies, max_bytes)
        .await
        .map_err(|err| {
            GptDetectError::new(
                format!("引用图片下载失败: {err}"),
                None,
                Some("DOWNLOAD_ERROR"),
            )
        })?;
    if buffer.len() as u64 > max_bytes {
        return Err(GptDetectError::new(
            format!("图片超过 {}MB 限制", config.max_image_size_mb.max(1)),
            None,
            Some("FILE_TOO_LARGE"),
        ));
    }

    let client = build_client(config)?;
    let endpoint = format!(
        "{}/content_provenance_checks",
        config.base_url.trim_end_matches('/')
    );
    let file_name = file_name_for(image, &content_type);
    let proxy_hint = if config.proxy.is_empty() {
        String::new()
    } else {
        format!("，请检查代理 {} 是否可用", config.proxy)
    };
    let start = KEY_CURSOR.fetch_add(1, Ordering::Relaxed) % api_keys.len();
    for offset in 0..api_keys.len() {
        let index = (start + offset) % api_keys.len();
        match request_with_key(
            &client,
            &endpoint,
            &file_name,
            &content_type,
            &buffer,
            &api_keys[index],
            &proxy_hint,
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error) if is_rate_limit_error(&error) => {
                warn!("OpenAI Key #{} 触发限流，切换到下一个 Key", index + 1);
                if offset + 1 == api_keys.len() {
                    return Err(GptDetectError::new(
                        format!(
                            "所有 {} 个 OpenAI Key 均已触发限流，请稍后重试",
                            api_keys.len()
                        ),
                        Some(429),
                        Some("RATE_LIMIT_ALL_KEYS"),
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(GptDetectError::new(
        "OpenAI Key 均不可用",
        None,
        Some("CONFIG_ERROR"),
    ))
}

pub fn format_gpt_detect_reply(result: &GptDetectResult) -> String {
    if result.detected {
        let mut lines = vec!["图片检测到 OpenAI 生成标识。".to_string()];
        if !result.evidence.is_empty() {
            lines.push(format!("检测依据：{}", result.evidence.join("、")));
        }
        if let Some(model) = &result.model {
            lines.push(format!("模型：{model}"));
        }
        if let Some(generated_at) = &result.generated_at {
            lines.push(format!("生成时间：{generated_at}"));
        }
        return lines.join("\n");
    }
    "未检测到OpenAI生成痕迹".into()
}

pub fn format_gpt_detect_error(error: &GptDetectError) -> String {
    let mut code_parts = Vec::new();
    if let Some(status) = error.status {
        code_parts.push(format!("HTTP {status}"));
    }
    if let Some(code) = &error.code {
        code_parts.push(code.clone());
    }
    [
        "检测失败".to_string(),
        format!(
            "错误码：{}",
            if code_parts.is_empty() {
                "UNKNOWN_ERROR".into()
            } else {
                code_parts.join(" / ")
            }
        ),
        format!("提示：{}", error.message),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_requires_exact_trimmed_match() {
        let keywords = vec!["ai吗".to_string()];
        assert!(matches_gpt_detect_keyword("ai吗", &keywords));
        assert!(matches_gpt_detect_keyword("  ai吗  ", &keywords));
        assert!(!matches_gpt_detect_keyword("AI吗", &keywords));
        assert!(!matches_gpt_detect_keyword("ai吗？", &keywords));
        assert!(!matches_gpt_detect_keyword("请 ai吗", &keywords));
    }

    #[test]
    fn formats_detection_messages() {
        let result = GptDetectResult {
            detected: true,
            evidence: vec![
                "C2PA Content Credentials，签发方 OpenAI OpCo, LLC".into(),
                "SynthID 水印".into(),
            ],
            model: Some("gpt-image".into()),
            generated_at: Some("2026-10-22T00:00:00Z".into()),
        };
        assert_eq!(
            format_gpt_detect_reply(&result),
            "图片检测到 OpenAI 生成标识。\n检测依据：C2PA Content Credentials，签发方 OpenAI OpCo, LLC、SynthID 水印\n模型：gpt-image\n生成时间：2026-10-22T00:00:00Z"
        );
    }

    #[test]
    fn formats_api_error() {
        let error = GptDetectError::new(
            "OpenAI 溯源接口请求频率超限，请稍后重试",
            Some(429),
            Some("rate_limit_exceeded"),
        );
        assert_eq!(
            format_gpt_detect_error(&error),
            "检测失败\n错误码：HTTP 429 / rate_limit_exceeded\n提示：OpenAI 溯源接口请求频率超限，请稍后重试"
        );
    }

    #[test]
    fn deduplicates_api_key_pool() {
        let mut keys = Vec::new();
        push_api_key(&mut keys, "key-a");
        push_api_key(&mut keys, " key-a ");
        push_api_key(&mut keys, "key-b");
        assert_eq!(keys, vec!["key-a", "key-b"]);
    }

    #[test]
    fn detects_rate_limit_errors() {
        assert!(is_rate_limit_error(&GptDetectError::new(
            "rate limited",
            Some(429),
            Some("rate_limit_exceeded")
        )));
        assert!(!is_rate_limit_error(&GptDetectError::new(
            "auth failed",
            Some(401),
            Some("auth_error")
        )));
    }
}
