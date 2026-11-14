use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha1::{Digest as Sha1Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::{MissedTickBehavior, interval_at};
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{debug, error, info, warn};

use crate::bot::message::{
    extract_card_fields, extract_current_text_from_qq_event, extract_quoted_content_from_qq_event,
    extract_text_from_qq_event, strip_leading_self_mentions,
};
use crate::types::{BotEvent, ChatType, QQApiError, QQBotConfig, QQChatEvent};

pub const INTENT_GROUP_AND_C2C: u64 = 1 << 25;
const PASSIVE_FALLBACK_CODES: &[i64] = &[40_034_128, 40_034_005, 40_034_026, 304_027];
const DUPLICATE_SEQ_CODE: i64 = 40_054_005;
const UPLOAD_BUFFER_SIZE: usize = 64 * 1024;
const UPLOAD_HEAD_SIZE: u64 = 10_002_432;
pub const MARKDOWN_IMAGE_MAX_BYTES: usize = 10_000_000;

#[derive(Debug, Clone)]
pub struct MarkdownImage {
    pub image_id: String,
    pub url: String,
}

fn valid_markdown_image_id(image_id: &str) -> bool {
    !image_id.is_empty()
        && image_id != "."
        && image_id != ".."
        && !image_id.chars().any(char::is_whitespace)
        && !image_id.chars().any(char::is_control)
}

fn valid_markdown_image_url(image_url: &str) -> bool {
    image_url
        .strip_prefix("qqbot-media://image/v1/")
        .is_some_and(|token| {
            !token.is_empty()
                && token.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'=' | b'~')
                })
        })
}

struct UploadPart {
    index: u64,
    url: String,
    offset: u64,
    length: u64,
}

fn upload_parts(prepare: &Value, file_size: u64) -> anyhow::Result<Vec<UploadPart>> {
    let parts = prepare
        .get("parts")
        .and_then(Value::as_array)
        .filter(|parts| parts.len() <= 4096)
        .ok_or_else(|| anyhow::anyhow!("upload_prepare 分片列表缺失或过长"))?;
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    let block_size = prepare
        .get("block_size")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .filter(|size| *size > 0)
        .ok_or_else(|| anyhow::anyhow!("upload_prepare 分片大小必须为正整数"))?;
    let mut indices = std::collections::HashSet::new();
    let mut result = Vec::with_capacity(parts.len());
    for part in parts {
        let index = part
            .get("index")
            .and_then(Value::as_u64)
            .filter(|index| *index > 0)
            .ok_or_else(|| anyhow::anyhow!("分片 index 必须为从 1 开始的正整数"))?;
        if !indices.insert(index) {
            anyhow::bail!("分片编号重复");
        }
        let offset = (index - 1)
            .checked_mul(block_size)
            .ok_or_else(|| anyhow::anyhow!("分片范围溢出"))?;
        let remaining = file_size
            .checked_sub(offset)
            .filter(|remaining| *remaining > 0)
            .ok_or_else(|| anyhow::anyhow!("分片超过文件范围"))?;
        let url = part
            .get("presigned_url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("分片缺少 presigned_url"))?;
        let parsed = url::Url::parse(url)?;
        if !matches!(parsed.scheme(), "http" | "https") {
            anyhow::bail!("分片上传 URL 必须为 HTTP(S)");
        }
        result.push(UploadPart {
            index,
            url: url.into(),
            offset,
            length: block_size.min(remaining),
        });
    }
    Ok(result)
}

#[derive(Debug, thiserror::Error)]
pub enum QQBotError {
    #[error("{0}")]
    Api(#[from] QQApiError),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for QQBotError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error.to_string())
    }
}

impl From<reqwest::Error> for QQBotError {
    fn from(error: reqwest::Error) -> Self {
        Self::Other(error.to_string())
    }
}

#[derive(Debug, Default)]
struct TokenState {
    token: String,
    expire_at: Option<Instant>,
}

#[derive(Debug)]
struct ConnectionOutcome {
    stop: bool,
    resumable: bool,
    close_code: Option<u16>,
}

pub struct QQBotClient {
    config: QQBotConfig,
    http: reqwest::Client,
    upload_http: reqwest::Client,
    token: Mutex<TokenState>,
    token_refresh_lock: Mutex<()>,
    session_id: Mutex<Option<String>>,
    bot_user_id: Mutex<Option<String>>,
    last_seq: Mutex<Option<i64>>,
    reply_seqs: Mutex<HashMap<String, (u64, Instant)>>,
    seen_message_ids: Mutex<HashMap<String, Instant>>,
    last_queue_warning: std::sync::Mutex<Option<Instant>>,
    reconnect_attempts: AtomicU32,
    stopped: AtomicBool,
    shutdown: Notify,
}

impl QQBotClient {
    pub fn new(config: QQBotConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            upload_http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(2)
                .pool_idle_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(120))
                .build()
                .expect("QQ 媒体客户端初始化失败"),
            token: Mutex::new(TokenState::default()),
            token_refresh_lock: Mutex::new(()),
            session_id: Mutex::new(None),
            bot_user_id: Mutex::new(None),
            last_seq: Mutex::new(None),
            reply_seqs: Mutex::new(HashMap::new()),
            seen_message_ids: Mutex::new(HashMap::new()),
            last_queue_warning: std::sync::Mutex::new(None),
            reconnect_attempts: AtomicU32::new(0),
            stopped: AtomicBool::new(false),
            shutdown: Notify::new(),
        }
    }

    pub fn config(&self) -> &QQBotConfig {
        &self.config
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.shutdown.notify_waiters();
    }

    fn api_base(&self) -> &str {
        if self.config.api_base.is_empty() {
            "https://api.bot.qq.com"
        } else {
            self.config.api_base.trim_end_matches('/')
        }
    }

    async fn ensure_token(&self, force: bool) -> anyhow::Result<String> {
        {
            let state = self.token.lock().await;
            if !force
                && !state.token.is_empty()
                && state
                    .expire_at
                    .is_some_and(|expire| Instant::now() + Duration::from_secs(60) < expire)
            {
                return Ok(state.token.clone());
            }
        }
        let _refresh_guard = self.token_refresh_lock.lock().await;
        {
            let state = self.token.lock().await;
            if !force
                && !state.token.is_empty()
                && state
                    .expire_at
                    .is_some_and(|expire| Instant::now() + Duration::from_secs(60) < expire)
            {
                return Ok(state.token.clone());
            }
        }

        let response = self
            .http
            .post(format!("{}/app/getAppAccessToken", self.api_base()))
            .timeout(Duration::from_secs(15))
            .json(&json!({
                "appId": self.config.app_id,
                "clientSecret": self.config.app_secret
            }))
            .send()
            .await?
            .error_for_status()?;
        let data: Value = response.json().await?;
        if data.get("code").is_some_and(json_truthy) {
            anyhow::bail!(
                "获取 QQ 机器人 access_token 失败: [{}] {}",
                data.get("code")
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                data.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            );
        }
        let token = data
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("获取 QQ 机器人 access_token 失败: 响应缺少 access_token")
            })?
            .to_string();
        let expires_in = data
            .get("expires_in")
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            })
            .filter(|value| *value > 0)
            .unwrap_or(7200);
        {
            let mut state = self.token.lock().await;
            state.token = token.clone();
            state.expire_at = Some(Instant::now() + Duration::from_secs(expires_in));
        }
        info!("access_token 已更新，有效期 {expires_in}s");
        Ok(token)
    }

    async fn fetch_gateway(&self) -> anyhow::Result<String> {
        let token = self.ensure_token(false).await?;
        let data: Value = self
            .http
            .get(format!("{}/gateway", self.api_base()))
            .timeout(Duration::from_secs(15))
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let url = data
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| anyhow::anyhow!("获取 QQ 网关地址失败: 响应缺少 url"))?;
        info!("QQ 网关地址: {}", redact_url(url));
        Ok(url.to_string())
    }

    pub async fn run(self: Arc<Self>, events: mpsc::Sender<BotEvent>) -> anyhow::Result<()> {
        self.stopped.store(false, Ordering::SeqCst);
        self.reconnect_attempts.store(0, Ordering::SeqCst);
        let token = self.ensure_token(false).await?;
        drop(token);
        let gateway_url = self.fetch_gateway().await?;

        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return Ok(());
            }
            let outcome = self.connect_once(&gateway_url, &events).await;
            if outcome.stop || self.stopped.load(Ordering::SeqCst) {
                return Ok(());
            }
            if !outcome.resumable {
                *self.session_id.lock().await = None;
            }
            if let Some(code) = outcome.close_code {
                warn!("QQ 网关连接断开 (code: {code})");
                if code == 4914 || code == 4915 {
                    error!(
                        "机器人已下架或封禁 (code: {code})，停止重连，请前往 QQ 开放平台检查机器人状态"
                    );
                    return Ok(());
                }
                if code == 4013 {
                    error!("无效的 intent ({})，请检查配置", self.config.intents);
                }
                if code == 4014 {
                    error!("intent 无权限，请前往 QQ 开放平台为机器人开通相应能力");
                }
            } else {
                warn!("QQ 网关连接中断，将尝试恢复会话");
            }

            let attempts = self.reconnect_attempts.fetch_add(1, Ordering::SeqCst) + 1;
            let delay =
                Duration::from_millis(5_000u64.saturating_mul(u64::from(attempts)).min(60_000));
            info!("{} 秒后尝试重新连接 (第 {attempts} 次)...", delay.as_secs());
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.shutdown.notified() => return Ok(()),
            }
        }
    }

    async fn connect_once(
        &self,
        gateway_url: &str,
        events: &mpsc::Sender<BotEvent>,
    ) -> ConnectionOutcome {
        info!("正在连接 QQ 官方网关...");
        let websocket_config = WebSocketConfig {
            max_message_size: Some(1024 * 1024),
            max_frame_size: Some(1024 * 1024),
            max_write_buffer_size: 1024 * 1024,
            ..WebSocketConfig::default()
        };
        let (socket, _) =
            match connect_async_with_config(gateway_url, Some(websocket_config), false).await {
                Ok(connection) => connection,
                Err(err) => {
                    error!("连接 QQ 网关失败: {err}");
                    return ConnectionOutcome {
                        stop: false,
                        resumable: true,
                        close_code: None,
                    };
                }
            };
        info!("✔ 已连接 QQ 官方网关，等待握手...");
        let (mut sink, mut stream) = socket.split();
        let mut heartbeat: Option<tokio::time::Interval> = None;
        let mut missed_acks = 0u8;

        loop {
            let frame = tokio::select! {
                _ = self.shutdown.notified() => {
                    return ConnectionOutcome { stop: true, resumable: true, close_code: Some(1000) };
                }
                frame = stream.next() => frame,
                _ = async {
                    if let Some(interval) = heartbeat.as_mut() {
                        interval.tick().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    missed_acks += 1;
                    if missed_acks >= 3 {
                        warn!("心跳连续 {missed_acks} 次未收到 ACK，主动断开重连");
                        return ConnectionOutcome { stop: false, resumable: true, close_code: None };
                    }
                    let seq = *self.last_seq.lock().await;
                    if sink.send(Message::Text(json!({ "op": 1, "d": seq }).to_string())).await.is_err() {
                        return ConnectionOutcome { stop: false, resumable: true, close_code: None };
                    }
                    continue;
                }
            };

            let Some(frame) = frame else {
                return ConnectionOutcome {
                    stop: false,
                    resumable: true,
                    close_code: None,
                };
            };
            let message = match frame {
                Ok(message) => message,
                Err(err) => {
                    error!("QQ 网关通信异常: {err}");
                    return ConnectionOutcome {
                        stop: false,
                        resumable: true,
                        close_code: None,
                    };
                }
            };
            match message {
                Message::Text(text) => {
                    let payload: Value = match serde_json::from_str(&text) {
                        Ok(payload) => payload,
                        Err(err) => {
                            error!("解析网关消息失败: {err}");
                            continue;
                        }
                    };
                    let op = payload
                        .get("op")
                        .and_then(Value::as_u64)
                        .unwrap_or(u64::MAX);
                    match op {
                        10 => {
                            let interval_ms = payload
                                .pointer("/d/heartbeat_interval")
                                .and_then(|value| {
                                    value.as_u64().or_else(|| {
                                        value.as_str().and_then(|value| value.parse().ok())
                                    })
                                })
                                .filter(|value| *value > 0)
                                .unwrap_or(45_000);
                            debug!(heartbeat_interval_ms = interval_ms, "QQ 网关 Hello");
                            missed_acks = 0;
                            if let Err(err) = self.ensure_token(true).await {
                                error!("刷新 access_token 失败: {err}，稍后重试");
                                return ConnectionOutcome {
                                    stop: false,
                                    resumable: true,
                                    close_code: Some(4009),
                                };
                            }
                            let token = self.token.lock().await.token.clone();
                            let session_id = self.session_id.lock().await.clone();
                            let identify = if let Some(session_id) = session_id {
                                info!(
                                    "正在恢复会话 (session: {session_id}, seq: {:?})...",
                                    *self.last_seq.lock().await
                                );
                                json!({
                                    "op": 6,
                                    "d": {
                                        "token": format!("QQBot {token}"),
                                        "session_id": session_id,
                                        "seq": self.last_seq.lock().await.unwrap_or(0)
                                    }
                                })
                            } else {
                                info!("正在登录鉴权 (intents: {})...", self.config.intents);
                                json!({
                                    "op": 2,
                                    "d": {
                                        "token": format!("QQBot {token}"),
                                        "intents": self.config.intents,
                                        "shard": [0, 1],
                                        "properties": {
                                            "$os": platform_name(),
                                            "$browser": "qq-official-video-parser",
                                            "$device": "qq-official-video-parser"
                                        }
                                    }
                                })
                            };
                            if sink
                                .send(Message::Text(identify.to_string()))
                                .await
                                .is_err()
                            {
                                return ConnectionOutcome {
                                    stop: false,
                                    resumable: true,
                                    close_code: None,
                                };
                            }
                            let mut timer = interval_at(
                                tokio::time::Instant::now() + Duration::from_millis(interval_ms),
                                Duration::from_millis(interval_ms),
                            );
                            timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
                            heartbeat = Some(timer);
                        }
                        11 => {
                            missed_acks = 0;
                            debug!("QQ 网关心跳 ACK 已收到");
                        }
                        7 => {
                            warn!("服务端要求重新连接，将走 Resume 流程");
                            return ConnectionOutcome {
                                stop: false,
                                resumable: true,
                                close_code: Some(4009),
                            };
                        }
                        9 => {
                            warn!("会话无效 (INVALID_SESSION)，刷新 token 后重新 Identify");
                            *self.session_id.lock().await = None;
                            *self.last_seq.lock().await = None;
                            let _ = self.ensure_token(true).await;
                            return ConnectionOutcome {
                                stop: false,
                                resumable: false,
                                close_code: Some(4009),
                            };
                        }
                        0 => {
                            if let Some(sequence) = payload.get("s").and_then(value_to_i64) {
                                *self.last_seq.lock().await = Some(sequence);
                            }
                            let dispatch_type =
                                payload.get("t").and_then(Value::as_str).unwrap_or_default();
                            let dispatch_data = payload.get("d").unwrap_or(&Value::Null);
                            self.handle_dispatch(dispatch_type, dispatch_data, events)
                                .await;
                        }
                        _ => debug!("未处理的网关 opcode: {op}"),
                    }
                }
                Message::Ping(data) => {
                    let _ = sink.send(Message::Pong(data)).await;
                }
                Message::Close(frame) => {
                    let code = frame.map(|frame| u16::from(frame.code));
                    return ConnectionOutcome {
                        stop: false,
                        resumable: code.is_some_and(|code| {
                            code == 4009 || code == 4008 || (4900..=4913).contains(&code)
                        }),
                        close_code: code,
                    };
                }
                _ => {}
            }
        }
    }

    async fn handle_dispatch(
        &self,
        dispatch_type: &str,
        data: &Value,
        events: &mpsc::Sender<BotEvent>,
    ) {
        debug!(dispatch_type, "QQ 网关事件已收到");
        if dispatch_type == "READY" {
            let session_id = data
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            *self.session_id.lock().await = (!session_id.is_empty()).then_some(session_id.clone());
            *self.bot_user_id.lock().await = data.pointer("/user/id").and_then(|id| match id {
                Value::String(text) if !text.is_empty() => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            });
            self.reconnect_attempts.store(0, Ordering::SeqCst);
            info!(
                "✔ 登录鉴权成功！机器人: {} (session: {session_id})",
                data.pointer("/user/username")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
            );
            self.publish_event(events, BotEvent::Ready(data.clone()));
            return;
        }
        if dispatch_type == "RESUMED" {
            info!("✔ 会话已恢复，遗漏事件补发完成");
            self.reconnect_attempts.store(0, Ordering::SeqCst);
            self.publish_event(events, BotEvent::Resumed(data.clone()));
            return;
        }

        let chat_type = match dispatch_type {
            "GROUP_MESSAGE_CREATE" => {
                if !self.config.receive_all_messages {
                    return;
                }
                ChatType::Group
            }
            "GROUP_AT_MESSAGE_CREATE" => ChatType::Group,
            "C2C_MESSAGE_CREATE" => ChatType::Private,
            _ => return,
        };
        let Some(event) = self
            .normalize_message_event(chat_type, dispatch_type, data)
            .await
        else {
            return;
        };
        {
            let mut seen = self.seen_message_ids.lock().await;
            let now = Instant::now();
            if seen.contains_key(&event.message_id) {
                return;
            }
            seen.insert(event.message_id.clone(), now);
            if seen.len() > 2000 {
                seen.retain(|_, timestamp| {
                    now.duration_since(*timestamp) <= Duration::from_secs(600)
                });
                if seen.len() > 2000
                    && let Some(oldest) = seen
                        .iter()
                        .min_by_key(|(_, timestamp)| **timestamp)
                        .map(|(key, _)| key.clone())
                {
                    seen.remove(&oldest);
                }
            }
        }
        self.publish_event(events, BotEvent::Message(event));
    }

    fn publish_event(&self, events: &mpsc::Sender<BotEvent>, event: BotEvent) {
        if let Err(mpsc::error::TrySendError::Full(_)) = events.try_send(event) {
            let now = Instant::now();
            if let Ok(mut last_warning) = self.last_queue_warning.lock()
                && last_warning
                    .is_none_or(|previous| now.duration_since(previous) >= Duration::from_secs(30))
            {
                warn!("消息处理队列已满，已跳过新事件");
                *last_warning = Some(now);
            }
        }
    }

    async fn normalize_message_event(
        &self,
        chat_type: ChatType,
        dispatch_type: &str,
        data: &Value,
    ) -> Option<QQChatEvent> {
        let message_id = data.get("id").filter(|id| json_truthy(id));
        let chat_id = match chat_type {
            ChatType::Group => data.get("group_openid"),
            ChatType::Private => data.pointer("/author/user_openid"),
        }
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
        let (Some(message_id), Some(chat_id)) = (message_id, chat_id) else {
            debug!(
                dispatch_type,
                missing_message_id = message_id.is_none(),
                missing_chat_id = chat_id.is_none(),
                "QQ 消息缺少必需字段"
            );
            return None;
        };
        let message_id = match message_id {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let chat_id = chat_id.to_string();
        let raw_current_text = extract_current_text_from_qq_event(data);
        let raw_text = extract_text_from_qq_event(data);
        let bot_user_id = self.bot_user_id.lock().await.clone();
        let self_ids = [
            bot_user_id.as_deref().unwrap_or_default(),
            &self.config.app_id,
        ];
        let (current_text, current_mention_removed) =
            strip_leading_self_mentions(&raw_current_text, &self_ids);
        let (text, aggregate_mention_removed) = strip_leading_self_mentions(&raw_text, &self_ids);
        let received_chars = data
            .get("content")
            .and_then(Value::as_str)
            .map_or(0, |text| text.chars().count());
        debug!(
            dispatch_type,
            received_chars,
            self_mention_removed = current_mention_removed || aggregate_mention_removed,
            "QQ 消息正文已规范化"
        );
        let quoted = extract_quoted_content_from_qq_event(data);
        if let Some(ark) = data.get("ark_data") {
            let fields = ark
                .get("fields")
                .and_then(Value::as_object)
                .map(|fields| {
                    fields
                        .iter()
                        .map(|(key, value)| {
                            format!(
                                "{key}={}",
                                value
                                    .as_str()
                                    .map(ToString::to_string)
                                    .unwrap_or_else(|| value.to_string())
                                    .chars()
                                    .take(200)
                                    .collect::<String>()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                })
                .unwrap_or_default();
            info!(
                "收到 ark 卡片 (type={}): {fields}",
                ark.get("ark_type")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
            );
        }
        let (card_title, card_source) = extract_card_fields(data);
        Some(QQChatEvent {
            chat_type,
            chat_id,
            message_id,
            sender_name: data
                .pointer("/author/username")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            current_text: current_text.to_owned(),
            text: text.to_owned(),
            card_title,
            card_source,
            quoted_message_id: quoted.message_id,
            quoted_text: (!quoted.text.is_empty()).then_some(quoted.text),
            quoted_images: quoted.images,
        })
    }

    async fn api_call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        retried: bool,
        timeout: Duration,
    ) -> Result<Value, QQBotError> {
        let token = self.ensure_token(false).await?;
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.api_base()))
            .timeout(timeout)
            .header("Authorization", format!("QQBot {token}"))
            .header("Content-Type", "application/json; charset=utf-8");
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED && !retried {
            warn!("access_token 失效，刷新后重试: {method} {path}");
            self.ensure_token(true).await?;
            return Box::pin(self.api_call(method, path, body, true, timeout)).await;
        }
        if (status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            && !retried
        {
            warn!("HTTP {}，1s 后重试: {method} {path}", status.as_u16());
            tokio::time::sleep(Duration::from_secs(1)).await;
            return Box::pin(self.api_call(method, path, body, true, timeout)).await;
        }
        let data: Value = response.json().await.unwrap_or(Value::Null);
        if let Some(err_code) = data.get("err_code").filter(|value| json_truthy(value)) {
            let code = err_code.as_i64().unwrap_or_else(|| {
                err_code
                    .as_str()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0)
            });
            return Err(QQApiError::new(
                code,
                data.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                data.get("trace_id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            )
            .into());
        }
        if !status.is_success() {
            return Err(QQApiError::new(
                i64::from(status.as_u16()),
                format!(
                    "HTTP {}: {}",
                    status.as_u16(),
                    data.to_string().chars().take(200).collect::<String>()
                ),
                None,
            )
            .into());
        }
        Ok(data)
    }

    pub async fn send_group_message(
        &self,
        group_openid: &str,
        body: &Value,
    ) -> Result<Value, QQBotError> {
        self.api_call(
            reqwest::Method::POST,
            &format!("/v2/groups/{group_openid}/messages"),
            Some(body),
            false,
            Duration::from_secs(30),
        )
        .await
    }

    pub async fn send_user_message(
        &self,
        user_openid: &str,
        body: &Value,
    ) -> Result<Value, QQBotError> {
        self.api_call(
            reqwest::Method::POST,
            &format!("/v2/users/{user_openid}/messages"),
            Some(body),
            false,
            Duration::from_secs(30),
        )
        .await
    }

    pub async fn upload_group_media(
        &self,
        group_openid: &str,
        file_type: u32,
        url: &str,
    ) -> Result<Value, QQBotError> {
        self.api_call(
            reqwest::Method::POST,
            &format!("/v2/groups/{group_openid}/files"),
            Some(&json!({
                "file_type": file_type,
                "url": url,
                "srv_send_msg": false
            })),
            false,
            Duration::from_secs(120),
        )
        .await
    }

    /// Upload a Markdown material, rather than a native media message attachment.
    pub async fn upload_markdown_image(
        &self,
        file_name: &str,
        content_type: &str,
        data: &[u8],
    ) -> Result<MarkdownImage, QQBotError> {
        self.upload_markdown_image_tracked(file_name, content_type, data, |_| Ok(()))
            .await
    }

    /// The callback records the created ID before response validation or any cleanup await.
    pub async fn upload_markdown_image_tracked(
        &self,
        file_name: &str,
        content_type: &str,
        data: &[u8],
        on_created: impl FnOnce(&str) -> Result<(), QQBotError>,
    ) -> Result<MarkdownImage, QQBotError> {
        if data.is_empty() || data.len() > MARKDOWN_IMAGE_MAX_BYTES {
            return Err(QQBotError::Other(
                "Markdown 图片不能为空或超过 10 MB".into(),
            ));
        }
        if file_name.is_empty() || file_name.chars().any(char::is_control) {
            return Err(QQBotError::Other("Markdown 图片文件名无效".into()));
        }
        let extension = Path::new(file_name)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let expected_type = match extension.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => return Err(QQBotError::Other("Markdown 图片格式不支持".into())),
        };
        if content_type != expected_type {
            return Err(QQBotError::Other(
                "Markdown 图片扩展名与 Content-Type 不一致".into(),
            ));
        }
        let prepared = self
            .api_call(
                reqwest::Method::POST,
                "/v2/media-images/prepare",
                Some(&json!({
                    "file_name": file_name,
                    "file_size": data.len(),
                    "content_type": content_type,
                    "md5": format!("{:x}", md5::compute(data)),
                    "sha1": hex::encode(Sha1::digest(data)),
                })),
                false,
                Duration::from_secs(120),
            )
            .await?;
        let upload_id = prepared
            .get("upload_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && !id.chars().any(char::is_control))
            .ok_or_else(|| QQBotError::Other("素材上传响应缺少有效 upload_id".into()))?;
        let presigned_url = prepared
            .get("presigned_url")
            .and_then(Value::as_str)
            .filter(|url| !url.chars().any(char::is_control))
            .ok_or_else(|| QQBotError::Other("素材上传响应缺少有效预签名 URL".into()))?;
        let parsed = url::Url::parse(presigned_url)
            .map_err(|_| QQBotError::Other("素材预签名 URL 无效".into()))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(QQBotError::Other(
                "素材预签名 URL 必须为无认证信息的 HTTP(S) 地址".into(),
            ));
        }
        let uploaded = self
            .upload_http
            .put(parsed)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::CONTENT_LENGTH, data.len())
            .body(data.to_vec())
            .send()
            .await?
            .error_for_status()?;
        let etag = uploaded
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
                    .unwrap_or(value)
            })
            .filter(|value| value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| QQBotError::Other("素材上传响应缺少有效 MD5 ETag".into()))?
            .to_ascii_lowercase();
        drop(uploaded);
        let completed = self
            .api_call(
                reqwest::Method::POST,
                "/v2/media-images/complete",
                Some(&json!({"upload_id": upload_id, "md5": etag})),
                false,
                Duration::from_secs(120),
            )
            .await?;
        let image_id = completed
            .get("image_id")
            .and_then(Value::as_str)
            .filter(|id| valid_markdown_image_id(id))
            .ok_or_else(|| QQBotError::Other("素材上传响应缺少有效 image_id".into()))?;
        if let Err(error) = on_created(image_id) {
            if let Err(cleanup_error) = self.delete_markdown_image(image_id).await {
                warn!("素材所有权记录失败后，清理本次素材暂未完成: {cleanup_error}");
            }
            return Err(error);
        }
        let image_url = completed
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| valid_markdown_image_url(url));
        let Some(image_url) = image_url else {
            // The material already exists even when its reference is unusable.
            if let Err(error) = self.delete_markdown_image(image_id).await {
                warn!("素材图片引用无效，清理本次已创建素材失败: {error}");
            }
            return Err(QQBotError::Other(
                "素材上传响应缺少有效 qqbot-media 图片引用".into(),
            ));
        };
        Ok(MarkdownImage {
            image_id: image_id.to_owned(),
            url: image_url.to_owned(),
        })
    }

    /// Delete only a material ID tracked by the caller's own upload ledger.
    pub async fn delete_markdown_image(&self, image_id: &str) -> Result<(), QQBotError> {
        if !valid_markdown_image_id(image_id) {
            return Err(QQBotError::Other("待清理的 Markdown 素材 ID 无效".into()));
        }
        match self
            .api_call(
                reqwest::Method::DELETE,
                &format!("/v2/media-images/{}/delete", urlencoding::encode(image_id)),
                None,
                false,
                Duration::from_secs(30),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(QQBotError::Api(error)) if error.err_code == 40_035_008 => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub async fn upload_user_media(
        &self,
        user_openid: &str,
        file_type: u32,
        url: &str,
    ) -> Result<Value, QQBotError> {
        self.api_call(
            reqwest::Method::POST,
            &format!("/v2/users/{user_openid}/files"),
            Some(&json!({
                "file_type": file_type,
                "url": url,
                "srv_send_msg": false
            })),
            false,
            Duration::from_secs(120),
        )
        .await
    }

    pub async fn upload_group_media_data(
        &self,
        group_openid: &str,
        file_type: u32,
        file_name: &str,
        data: &[u8],
    ) -> Result<Value, QQBotError> {
        self.upload_media_data(
            &format!("/v2/groups/{group_openid}"),
            file_type,
            file_name,
            data,
        )
        .await
    }

    pub async fn upload_user_media_data(
        &self,
        user_openid: &str,
        file_type: u32,
        file_name: &str,
        data: &[u8],
    ) -> Result<Value, QQBotError> {
        self.upload_media_data(
            &format!("/v2/users/{user_openid}"),
            file_type,
            file_name,
            data,
        )
        .await
    }

    pub async fn upload_group_media_file(
        &self,
        group_openid: &str,
        file_type: u32,
        file_name: &str,
        path: &Path,
    ) -> Result<Value, QQBotError> {
        self.upload_media_file(
            &format!("/v2/groups/{group_openid}"),
            file_type,
            file_name,
            path,
        )
        .await
    }

    pub async fn upload_user_media_file(
        &self,
        user_openid: &str,
        file_type: u32,
        file_name: &str,
        path: &Path,
    ) -> Result<Value, QQBotError> {
        self.upload_media_file(
            &format!("/v2/users/{user_openid}"),
            file_type,
            file_name,
            path,
        )
        .await
    }

    async fn upload_media_file(
        &self,
        base_path: &str,
        file_type: u32,
        file_name: &str,
        path: &Path,
    ) -> Result<Value, QQBotError> {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(anyhow::Error::from)?;
        let size = file.metadata().await.map_err(anyhow::Error::from)?.len();
        if size == 0 {
            return Err(QQBotError::Other("上传文件为空".into()));
        }
        let mut md5_whole = md5::Context::new();
        let mut md5_head = md5::Context::new();
        let mut sha1 = Sha1::new();
        let mut buffer = vec![0u8; UPLOAD_BUFFER_SIZE];
        let mut read = 0u64;
        loop {
            let count = file.read(&mut buffer).await.map_err(anyhow::Error::from)?;
            if count == 0 {
                break;
            }
            md5_whole.consume(&buffer[..count]);
            sha1.update(&buffer[..count]);
            let head_bytes = UPLOAD_HEAD_SIZE.saturating_sub(read).min(count as u64) as usize;
            md5_head.consume(&buffer[..head_bytes]);
            read += count as u64;
        }
        if read != size {
            return Err(QQBotError::Other("上传文件在计算摘要时发生变化".into()));
        }
        let prepare = self
            .api_call(
                reqwest::Method::POST,
                &format!("{base_path}/upload_prepare"),
                Some(&json!({
                    "file_type": file_type,
                    "file_size": size,
                    "file_name": file_name,
                    "md5": format!("{:x}", md5_whole.compute()),
                    "sha1": hex::encode(sha1.finalize()),
                    "md5_10m": format!("{:x}", md5_head.compute())
                })),
                false,
                Duration::from_secs(30),
            )
            .await?;
        let upload_id = prepare
            .get("upload_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| QQBotError::Other("upload_prepare 缺少 upload_id".into()))?;
        let parts = upload_parts(&prepare, size)?;
        // A fresh file handle is used for each request body. The stream owns its
        // reader and yields at most 64 KiB even for very large server blocks.
        for part in parts {
            file.seek(std::io::SeekFrom::Start(part.offset))
                .await
                .map_err(anyhow::Error::from)?;
            let mut remaining = part.length;
            let mut block_md5 = md5::Context::new();
            while remaining > 0 {
                let count = remaining.min(buffer.len() as u64) as usize;
                file.read_exact(&mut buffer[..count])
                    .await
                    .map_err(anyhow::Error::from)?;
                block_md5.consume(&buffer[..count]);
                remaining -= count as u64;
            }
            let mut body_file = tokio::fs::File::open(path)
                .await
                .map_err(anyhow::Error::from)?;
            body_file
                .seek(std::io::SeekFrom::Start(part.offset))
                .await
                .map_err(anyhow::Error::from)?;
            let stream = futures_util::stream::try_unfold(
                (body_file, part.length),
                |(mut file, remaining)| async move {
                    if remaining == 0 {
                        return Ok::<_, std::io::Error>(None);
                    }
                    let mut chunk = vec![0u8; remaining.min(UPLOAD_BUFFER_SIZE as u64) as usize];
                    file.read_exact(&mut chunk).await?;
                    let remaining = remaining - chunk.len() as u64;
                    Ok(Some((chunk, (file, remaining))))
                },
            );
            self.upload_http
                .put(&part.url)
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", part.length)
                .body(reqwest::Body::wrap_stream(stream))
                .send()
                .await?
                .error_for_status()?;
            self.api_call(
                reqwest::Method::POST,
                &format!("{base_path}/upload_part_finish"),
                Some(&json!({
                    "upload_id": upload_id,
                    "part_index": part.index,
                    "block_size": part.length,
                    "md5": format!("{:x}", block_md5.compute())
                })),
                false,
                Duration::from_secs(30),
            )
            .await?;
        }
        self.api_call(
            reqwest::Method::POST,
            &format!("{base_path}/files"),
            Some(&json!({ "upload_id": upload_id })),
            false,
            Duration::from_secs(30),
        )
        .await
    }

    async fn upload_media_data(
        &self,
        base_path: &str,
        file_type: u32,
        file_name: &str,
        data: &[u8],
    ) -> Result<Value, QQBotError> {
        let md5_whole = format!("{:x}", md5::compute(data));
        let sha1 = hex::encode(Sha1::digest(data));
        let head = &data[..data.len().min(10_002_432)];
        let md5_head = format!("{:x}", md5::compute(head));
        let prepare = self
            .api_call(
                reqwest::Method::POST,
                &format!("{base_path}/upload_prepare"),
                Some(&json!({
                    "file_type": file_type,
                    "file_size": data.len(),
                    "file_name": file_name,
                    "md5": md5_whole,
                    "sha1": sha1,
                    "md5_10m": md5_head
                })),
                false,
                Duration::from_secs(30),
            )
            .await?;
        let upload_id = prepare
            .get("upload_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| QQBotError::Other("upload_prepare 响应异常: 缺少 upload_id".into()))?;
        let parts = upload_parts(&prepare, data.len() as u64)?;
        for part in parts {
            let index = part.index;
            let offset = part.offset as usize;
            let chunk = &data[offset..offset + part.length as usize];
            let response = self
                .upload_http
                .put(&part.url)
                .timeout(Duration::from_secs(120))
                .header("Content-Type", "application/octet-stream")
                .body(chunk.to_vec())
                .send()
                .await?;
            if !response.status().is_success() {
                return Err(QQBotError::Other(format!(
                    "分片 {index} 上传失败: HTTP {}",
                    response.status().as_u16()
                )));
            }
            let chunk_md5 = format!("{:x}", md5::compute(chunk));
            self.api_call(
                reqwest::Method::POST,
                &format!("{base_path}/upload_part_finish"),
                Some(&json!({
                    "upload_id": upload_id,
                    "part_index": index,
                    "block_size": chunk.len(),
                    "md5": chunk_md5
                })),
                false,
                Duration::from_secs(30),
            )
            .await?;
        }
        self.api_call(
            reqwest::Method::POST,
            &format!("{base_path}/files"),
            Some(&json!({ "upload_id": upload_id })),
            false,
            Duration::from_secs(30),
        )
        .await
    }

    async fn next_seq(&self, message_id: &str) -> u64 {
        let mut guard = self.reply_seqs.lock().await;
        let now = Instant::now();
        if guard.len() >= 4096 && !guard.contains_key(message_id) {
            guard.retain(|_, (_, last_used)| {
                now.duration_since(*last_used) <= Duration::from_secs(600)
            });
            if guard.len() >= 4096
                && let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, (_, last_used))| *last_used)
                    .map(|(key, _)| key.clone())
            {
                guard.remove(&oldest);
            }
        }
        let (sequence, last_used) = guard.entry(message_id.to_string()).or_insert((0, now));
        *sequence += 1;
        *last_used = now;
        *sequence
    }

    pub async fn reply(&self, event: &QQChatEvent, body: &Value) -> Result<Value, QQBotError> {
        let mut passive = body
            .as_object()
            .cloned()
            .ok_or_else(|| QQBotError::Other("消息 payload 必须是对象".into()))?;
        passive.insert("msg_id".into(), Value::String(event.message_id.clone()));
        passive.insert(
            "msg_seq".into(),
            json!(self.next_seq(&event.message_id).await),
        );
        let passive = Value::Object(passive);
        let result = match event.chat_type {
            ChatType::Group => self.send_group_message(&event.chat_id, &passive).await,
            ChatType::Private => self.send_user_message(&event.chat_id, &passive).await,
        };
        match result {
            Ok(result) => Ok(result),
            Err(QQBotError::Api(error)) if PASSIVE_FALLBACK_CODES.contains(&error.err_code) => {
                warn!(
                    "被动回复受限 (err_code: {})，转主动消息发送",
                    error.err_code
                );
                let mut active = body.as_object().cloned().unwrap_or_default();
                active.remove("msg_id");
                active.remove("msg_seq");
                let active = Value::Object(active);
                match event.chat_type {
                    ChatType::Group => self.send_group_message(&event.chat_id, &active).await,
                    ChatType::Private => self.send_user_message(&event.chat_id, &active).await,
                }
            }
            Err(QQBotError::Api(error)) if error.err_code == DUPLICATE_SEQ_CODE => {
                warn!("消息被去重 (err_code: 40054005)，递增 msg_seq 重试");
                let mut retried = body.as_object().cloned().unwrap_or_default();
                retried.insert("msg_id".into(), Value::String(event.message_id.clone()));
                retried.insert(
                    "msg_seq".into(),
                    json!(self.next_seq(&event.message_id).await),
                );
                let retried = Value::Object(retried);
                match event.chat_type {
                    ChatType::Group => self.send_group_message(&event.chat_id, &retried).await,
                    ChatType::Private => self.send_user_message(&event.chat_id, &retried).await,
                }
            }
            Err(error) => Err(error),
        }
    }
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn value_to_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn redact_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return "<invalid gateway url>".into();
    };
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_upload_part_ranges() {
        let part = |index| {
            json!({
                "index": index, "presigned_url": "https://cos.example/part"
            })
        };
        assert!(upload_parts(&json!({"block_size":8,"parts":[part(0)]}), 15).is_err());
        assert!(upload_parts(&json!({"block_size":0,"parts":[part(1)]}), 15).is_err());
        assert!(upload_parts(&json!({"block_size":-1,"parts":[part(1)]}), 15).is_err());
        assert!(upload_parts(&json!({"parts":[part(1)]}), 15).is_err());
        assert!(upload_parts(&json!({"block_size":8,"parts":[part(3)]}), 15).is_err());
        assert!(upload_parts(&json!({"block_size":8,"parts":[part(1),part(1)]}), 15).is_err());
        assert!(upload_parts(&json!({"block_size":8,"parts":[part(u64::MAX)]}), 15).is_err());
        let too_many: Vec<_> = (1..=4097).map(part).collect();
        assert!(upload_parts(&json!({"block_size":1,"parts":too_many}), 4097).is_err());
        assert!(upload_parts(&json!({}), 15).is_err());
    }

    #[test]
    fn upload_parts_use_indices_for_partial_and_unordered_requests() {
        let prepare = json!({"block_size":"4","parts":[
            {"index":4,"presigned_url":"https://cos.example/last"},
            {"index":2,"presigned_url":"https://cos.example/second"}
        ]});
        let parts = upload_parts(&prepare, 15).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            (parts[0].index, parts[0].offset, parts[0].length),
            (4, 12, 3)
        );
        assert_eq!(
            (parts[1].index, parts[1].offset, parts[1].length),
            (2, 4, 4)
        );
        assert!(upload_parts(&json!({"parts":[]}), 15).unwrap().is_empty());
        assert!(
            upload_parts(&json!({"block_size":0,"parts":[]}), 15)
                .unwrap()
                .is_empty()
        );
        let whole = upload_parts(
            &json!({"block_size":u64::MAX,"parts":[
                {"index":1,"presigned_url":"https://cos.example/full"}
            ]}),
            15,
        )
        .unwrap();
        assert_eq!((whole[0].offset, whole[0].length), (0, 15));
    }
    use crate::bot::message::{media_payload, quoted_text_payload, text_payload};

    #[test]
    fn payloads_match_reference_shape() {
        assert_eq!(
            text_payload("hi"),
            json!({ "msg_type": 0, "content": "hi" })
        );
        assert_eq!(
            media_payload("FILE_INFO"),
            json!({ "msg_type": 7, "media": { "file_info": "FILE_INFO" } })
        );
        assert_eq!(
            quoted_text_payload("reply", "REFIDX_target"),
            json!({
                "msg_type": 0,
                "content": "reply",
                "message_reference": {
                    "message_id": "REFIDX_target",
                    "ignore_get_message_error": true
                }
            })
        );
    }

    #[test]
    fn json_truthy_matches_javascript() {
        assert!(!json_truthy(&json!(0)));
        assert!(!json_truthy(&json!("")));
        assert!(json_truthy(&json!("0")));
        assert!(json_truthy(&json!([])));
    }
}
