use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::bot::blacklist::{
    BanOutcome, BlacklistCommand, BlacklistStore, format_user_label, parse_blacklist_command,
};
use crate::bot::message::{
    MarkdownOptions, build_result_markdown, markdown_image_tag, markdown_payload, media_payload,
    quoted_text_payload, text_payload,
};
use crate::bot::qqbot::{MARKDOWN_IMAGE_MAX_BYTES, QQBotClient, QQBotError};
use crate::parser::api::VideoParserService;
use crate::parser::cardsearch::resolve_card_by_title;
use crate::parser::formatter::generate_formatted_text;
use crate::parser::gpt_detect::{
    GPT_DETECT_BUSY_TEXT, GPT_DETECT_WAITING_TEXT, detect_openai_image, format_gpt_detect_error,
    format_gpt_detect_reply, matches_gpt_detect_keyword,
};
use crate::parser::image_dimensions::{markdown_size, probe_image};
use crate::parser::jmcomic::{cleanup_jm_zip, extract_jm_id};
use crate::parser::matcher::link_type_parser;
use crate::parser::materials::{OwnedMaterials, unix_now};
use crate::parser::media::{
    download_media_buffer_with_headers, head_content_length, is_foreign_url,
};
use crate::parser::remux::{download_live_album_temp, download_parsed_temp};
use crate::types::{
    AppConfig, ChatType, DEFAULT_MAX_MARKDOWN_IMAGES, ImageSize, LinkMatch, ParsedData, QQApiError,
    QQChatEvent,
};

const QQ_ERR_CONTENT_VIOLATION: i64 = 40_034_006;
// Give Markdown syntax and media tags room in each X post reply.
const X_MARKDOWN_REPLY_BYTES: usize = 3500;
const X_MAX_POST_REPLIES: usize = 3;

static ARCHIVE_EXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.(7z|zip)$").unwrap());
static X_CANONICAL_URL_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^https://x\.com/(?:i/web|[A-Za-z0-9_]{1,15})/status/[0-9]{1,20}$").unwrap()
});
static JM_TASK_GATE: Semaphore = Semaphore::const_new(1);
static LIVE_ALBUM_GATE: Semaphore = Semaphore::const_new(1);

pub struct AppState {
    config: Arc<AppConfig>,
    parser: Arc<VideoParserService>,
    bot: Arc<QQBotClient>,
    dedup_cache: Arc<std::sync::Mutex<HashMap<String, DedupEntry>>>,
    gpt_detect_busy: AtomicBool,
    local_proxies: Vec<String>,
    gallery_material_gate: tokio::sync::Mutex<()>,
    owned_materials: std::sync::Mutex<Result<OwnedMaterials, String>>,
    blacklist: std::sync::Mutex<Result<BlacklistStore, String>>,
}

struct GalleryImage {
    image_id: String,
    url: String,
    size: ImageSize,
}

struct DedupEntry {
    token: u128,
    expires: Instant,
    pending: bool,
}

struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

struct AttachmentCleanup<'a>(&'a str);

impl Drop for AttachmentCleanup<'_> {
    fn drop(&mut self) {
        cleanup_jm_zip(self.0);
    }
}

struct DedupClaim {
    cache: Arc<std::sync::Mutex<HashMap<String, DedupEntry>>>,
    key: String,
    token: u128,
    interval: Duration,
    committed: bool,
}

impl DedupClaim {
    fn commit(&mut self) {
        if let Ok(mut cache) = self.cache.lock()
            && let Some(entry) = cache
                .get_mut(&self.key)
                .filter(|entry| entry.token == self.token)
        {
            entry.pending = false;
            entry.expires = Instant::now() + self.interval;
        }
        self.committed = true;
    }
}

impl Drop for DedupClaim {
    fn drop(&mut self) {
        if !self.committed
            && let Ok(mut cache) = self.cache.lock()
            && cache
                .get(&self.key)
                .is_some_and(|entry| entry.token == self.token)
        {
            cache.remove(&self.key);
        }
    }
}

impl AppState {
    pub fn new(
        config: Arc<AppConfig>,
        parser: Arc<VideoParserService>,
        bot: Arc<QQBotClient>,
    ) -> Self {
        let mut local_proxies = Vec::new();
        if !config.ytdlp.proxy.is_empty() {
            local_proxies.push(config.ytdlp.proxy.clone());
        }
        if !config.api.proxy.is_empty() && !local_proxies.contains(&config.api.proxy) {
            local_proxies.push(config.api.proxy.clone());
        }
        let owned_materials = OwnedMaterials::new(Path::new(&config.output.temp_dir))
            .map_err(|error| format!("{error:#}"));
        let blacklist = BlacklistStore::open(BlacklistStore::resolve_path(&config))
            .map_err(|error| format!("{error:#}"));
        if let Err(ref error) = blacklist {
            warn!("初始化全局黑名单失败: {error}");
        }
        Self {
            config,
            parser,
            bot,
            dedup_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            gpt_detect_busy: AtomicBool::new(false),
            local_proxies,
            gallery_material_gate: tokio::sync::Mutex::new(()),
            owned_materials: std::sync::Mutex::new(owned_materials),
            blacklist: std::sync::Mutex::new(blacklist),
        }
    }

    pub async fn handle_message(self: Arc<Self>, event: QQChatEvent) {
        if self.handle_openid_command(&event).await {
            return;
        }
        if self.handle_blacklist_command(&event).await {
            return;
        }
        if self.handle_gpt_detect(&event).await {
            return;
        }

        let raw_text = event.text.trim_start().to_string();
        if raw_text.trim().is_empty() {
            return;
        }

        let mut is_command = false;
        let mut target_text = raw_text.clone();
        for prefix in &self.config.qqbot.command_prefix {
            if raw_text.starts_with(prefix) {
                is_command = true;
                target_text = raw_text[prefix.len()..].trim().to_string();
                break;
            }
        }

        let jm_trigger = extract_jm_id(&raw_text);
        if !is_command && !self.config.qqbot.auto_parse && jm_trigger.is_none() {
            return;
        }

        let mut matches = if let Some(trigger) = &jm_trigger {
            vec![LinkMatch {
                media_type: "jmcomic".into(),
                url: format!(
                    "jm://{}/{}",
                    if trigger.is_photo { "photo" } else { "album" },
                    trigger.id
                ),
                id: trigger.id.clone(),
            }]
        } else {
            link_type_parser(&target_text)
        };
        let will_resolve_card = matches.is_empty()
            && event.card_title.is_some()
            && self.config.qqbot.parse_miniapp_cards;
        let has_enabled_match = matches
            .iter()
            .any(|item| self.config.platform_enabled(&item.media_type));
        if (is_command || will_resolve_card || has_enabled_match)
            && self.reply_if_blacklisted(&event).await
        {
            return;
        }
        let mut card_resolved = false;

        if will_resolve_card {
            if self.config.qqbot.show_waiting_tip
                && !self.config.ui_text.waiting_tip_text.is_empty()
            {
                self.send_message(&event, text_payload(&self.config.ui_text.waiting_tip_text))
                    .await;
            }
            let title = event.card_title.clone().unwrap_or_default();
            let source = event.card_source.clone().unwrap_or_default();
            match tokio::time::timeout(
                Duration::from_secs(8),
                resolve_card_by_title(&title, &source, "https://api.bilibili.com"),
            )
            .await
            {
                Ok(Some(resolved)) => {
                    info!("卡片无链接，已按标题还原: {}", resolved.url);
                    matches = vec![resolved];
                    card_resolved = true;
                }
                Ok(None) => info!(
                    "卡片标题搜索超时或无匹配，按无链接处理: {}",
                    title.chars().take(40).collect::<String>()
                ),
                Err(_) => info!(
                    "卡片标题搜索超时或无匹配，按无链接处理: {}",
                    title.chars().take(40).collect::<String>()
                ),
            }
        }

        if matches.is_empty() {
            if is_command {
                self.send_message(&event, text_payload(&self.config.ui_text.invalid_link_text))
                    .await;
            }
            return;
        }

        matches.retain(|item| {
            let enabled = self.config.platform_enabled(&item.media_type);
            if !enabled {
                info!(
                    "平台 {} 已在配置中禁用，跳过: {}",
                    item.media_type, item.url
                );
            }
            enabled
        });
        if matches.is_empty() {
            if is_command {
                self.send_message(
                    &event,
                    text_payload(&self.config.ui_text.unsupported_platform_text),
                )
                .await;
            }
            return;
        }

        let mut pending = Vec::new();
        for item in matches
            .into_iter()
            .take(self.config.resources.max_links_per_message.max(1))
        {
            if let Some(claim) = self.claim_link(&event, &item.url) {
                pending.push((item, claim));
            } else {
                info!("链接处于去重冷却期内，跳过: {}", item.url);
            }
        }
        if pending.is_empty() {
            return;
        }
        // Keep the JM gate until the archive is uploaded and cleaned.
        let _jm_permit = if jm_trigger.is_some() {
            Some(JM_TASK_GATE.acquire().await.expect("JM 并发闸门保持打开"))
        } else {
            None
        };

        let chat_label = match event.chat_type {
            ChatType::Group => format!("群 {}", event.chat_id),
            ChatType::Private => format!("私聊 {}", event.chat_id),
        };
        info!(
            "[{chat_label}] 检测到 {} 个待解析链接: {}",
            pending.len(),
            pending
                .iter()
                .map(|(item, _)| item.url.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );

        if !card_resolved
            && self.config.qqbot.show_waiting_tip
            && !self.config.ui_text.waiting_tip_text.is_empty()
        {
            self.send_message(&event, text_payload(&self.config.ui_text.waiting_tip_text))
                .await;
        }

        let mut parsed_results: Vec<(LinkMatch, ParsedData, String, DedupClaim)> = Vec::new();
        let mut failed_errors = Vec::new();
        for (item, claim) in pending {
            let started = Instant::now();
            match self.parser.parse_url(&item.url, &item.media_type).await {
                crate::parser::api::ParseResult::Success(data) => {
                    info!(platform = %item.media_type, elapsed_ms = started.elapsed().as_millis(), "作品解析完成");
                    let text = generate_formatted_text(&data, &self.config.output.format);
                    parsed_results.push((item, data, text, claim));
                }
                crate::parser::api::ParseResult::Failure(message) => {
                    warn!(
                        elapsed_ms = started.elapsed().as_millis(),
                        "解析失败 [{}]: {message}", item.url
                    );
                    let user_message = concise_user_parse_error(&message);
                    failed_errors.push(render_parse_error(&self.config, &item.url, &user_message));
                }
            }
        }

        if !failed_errors.is_empty() && self.config.qqbot.show_parse_error {
            let message = format!(
                "{}\n{}",
                self.config.ui_text.parse_error_prefix,
                failed_errors.join("\n")
            );
            self.send_message(&event, text_payload(message)).await;
        }
        if parsed_results.is_empty() {
            info!("没有成功解析的内容");
            return;
        }

        let result_count = parsed_results.len();
        let mut delivered_count = 0usize;
        for (index, (item, parsed, text, mut claim)) in parsed_results.into_iter().enumerate() {
            if let Some(attachment) = parsed.attachment.clone() {
                if self.send_attachment(&event, &attachment).await {
                    claim.commit();
                    delivered_count += 1;
                }
                continue;
            }
            let delivered = if matches!(
                item.media_type.as_str(),
                "douyin" | "xiaohongshu" | "bilibili"
            ) && parsed.media_type == "live_photo"
                && self.config.output.show_video_file
            {
                self.send_live_album_result(&event, &parsed, &text).await
            } else {
                self.send_media_result(&event, parsed, text, result_count - index - 1)
                    .await
            };
            if delivered {
                claim.commit();
                delivered_count += 1;
            }
        }
        if delivered_count == result_count {
            info!("所有内容发送完成");
        } else {
            warn!(
                "内容发送未完成，成功 {delivered_count} 项，失败 {} 项",
                result_count - delivered_count
            );
        }
    }

    fn is_super_admin(&self, event: &QQChatEvent) -> bool {
        let admin_openid = self.config.qqbot.super_admin_openid.trim();
        let sender_id = event.sender_id.trim();
        !admin_openid.is_empty() && !sender_id.is_empty() && sender_id == admin_openid
    }

    fn with_blacklist<T>(
        &self,
        operation: impl FnOnce(&mut BlacklistStore) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut state = self
            .blacklist
            .lock()
            .map_err(|_| anyhow::anyhow!("黑名单状态锁不可用"))?;
        let store = state
            .as_mut()
            .map_err(|error| anyhow::anyhow!("黑名单状态不可用: {error}"))?;
        operation(store)
    }

    async fn handle_openid_command(&self, event: &QQChatEvent) -> bool {
        let text = if event.current_text.is_empty() {
            event.text.trim()
        } else {
            event.current_text.trim()
        };
        if !text.eq_ignore_ascii_case("/openid") {
            return false;
        }
        if event.chat_type != ChatType::Private {
            return true;
        }
        let sender_id = event.sender_id.trim();
        let message = if sender_id.is_empty() {
            "暂未获取到你的 OpenID".to_string()
        } else {
            format!("你的 OpenID: {sender_id}")
        };
        self.send_message(event, text_payload(message)).await;
        true
    }

    async fn handle_blacklist_command(&self, event: &QQChatEvent) -> bool {
        let bot_user_id = self.bot.bot_user_id().await;
        let self_ids = [
            bot_user_id.as_deref().unwrap_or_default(),
            self.bot.config().app_id.as_str(),
        ];
        let Some(command) = parse_blacklist_command(event, &self_ids) else {
            return false;
        };
        if !self.is_super_admin(event) {
            return true;
        }
        match command {
            BlacklistCommand::MissingTarget { is_add } => {
                let action = if is_add { "拉黑" } else { "移出黑名单" };
                self.send_message(
                    event,
                    text_payload(format!(
                        "未识别到要{action}的目标用户，请在群内使用 QQ 的 @成员 功能点选目标，或直接附带目标 32 位 OpenID。"
                    )),
                )
                .await;
            }
            BlacklistCommand::Add { targets, reason } => {
                let admin_openid = self.config.qqbot.super_admin_openid.trim();
                let mut lines = Vec::new();
                for target in targets {
                    if !admin_openid.is_empty() && target.openid == admin_openid {
                        lines.push("不能将唯一超级管理员加入黑名单".to_string());
                        continue;
                    }
                    let label = target.display_label();
                    match self
                        .with_blacklist(|store| store.add(&target.openid, &target.name, &reason))
                    {
                        Ok(BanOutcome::Added) => {
                            info!(openid = %target.openid, reason = %reason, "已加入全局黑名单");
                            lines.push(format!("已将 {label} 加入全局黑名单，理由: {reason}"));
                        }
                        Ok(BanOutcome::Updated) => {
                            info!(openid = %target.openid, reason = %reason, "已更新全局黑名单理由");
                            lines.push(format!("已更新 {label} 的黑名单理由: {reason}"));
                        }
                        Err(error) => {
                            warn!("写入全局黑名单失败: {error}");
                            lines.push(format!("❌ 加入黑名单失败: {error}"));
                        }
                    }
                }
                self.send_message(event, text_payload(lines.join("\n")))
                    .await;
            }
            BlacklistCommand::Remove { targets } => {
                let mut lines = Vec::new();
                for target in targets {
                    let label = target.display_label();
                    match self.with_blacklist(|store| store.remove(&target.openid)) {
                        Ok(Some(removed)) => {
                            let display = if target.name.is_empty() && !removed.name.is_empty() {
                                format_user_label(&removed.name, &removed.openid)
                            } else {
                                label
                            };
                            info!(openid = %target.openid, "已移出全局黑名单");
                            lines.push(format!("已将 {display} 移出全局黑名单"));
                        }
                        Ok(None) => {
                            lines.push(format!("{label} 不在全局黑名单中"));
                        }
                        Err(error) => {
                            warn!("移出全局黑名单失败: {error}");
                            lines.push(format!("❌ 移出黑名单失败: {error}"));
                        }
                    }
                }
                self.send_message(event, text_payload(lines.join("\n")))
                    .await;
            }
        }
        true
    }

    async fn reply_if_blacklisted(&self, event: &QQChatEvent) -> bool {
        let sender_id = event.sender_id.trim();
        if sender_id.is_empty() || self.is_super_admin(event) {
            return false;
        }
        let reason = match self
            .with_blacklist(|store| Ok(store.reason(sender_id).map(ToString::to_string)))
        {
            Ok(reason) => reason,
            Err(error) => {
                warn!("读取全局黑名单失败: {error}");
                None
            }
        };
        let Some(reason) = reason else {
            return false;
        };
        info!(openid = %sender_id, "命中全局黑名单，已拦截请求");
        let payload = text_payload(format!("您已被加入全局黑名单，理由: {reason}"));
        match self.bot.reply(event, &payload).await {
            Ok(_) => {}
            Err(QQBotError::Api(error)) if error.err_code == QQ_ERR_CONTENT_VIOLATION => {
                warn!(
                    err_code = error.err_code,
                    trace_id = error.trace_id.as_deref().unwrap_or(""),
                    "黑名单提示中的理由触发腾讯审核拦截，降级发送无理由提示: {error}"
                );
                self.send_message(event, text_payload("您已被加入全局黑名单"))
                    .await;
            }
            Err(error) => {
                warn!("QQ 黑名单提示发送失败: {error}");
            }
        }
        true
    }

    async fn handle_gpt_detect(&self, event: &QQChatEvent) -> bool {
        if !self.config.gpt_detect.enabled {
            return false;
        }
        let current_text = if event.current_text.is_empty() {
            &event.text
        } else {
            &event.current_text
        };
        if !matches_gpt_detect_keyword(current_text, &self.config.gpt_detect.keywords) {
            return false;
        }
        if self.reply_if_blacklisted(event).await {
            return true;
        }
        if event.quoted_message_id.is_none() || event.quoted_images.is_empty() {
            self.send_message(event, text_payload("请引用一张图片，然后精确回复“ai吗”。"))
                .await;
            return true;
        }
        if event.quoted_images.len() > 1 {
            self.send_message(
                event,
                text_payload("一次只能检测一张引用图片，请只引用一张。"),
            )
            .await;
            return true;
        }
        if self.config.gpt_detect.api_key.is_empty()
            && self
                .config
                .gpt_detect
                .api_keys
                .iter()
                .all(|key| key.trim().is_empty())
            && std::env::var("OPENAI_API_KEY")
                .unwrap_or_default()
                .is_empty()
        {
            let error = crate::parser::gpt_detect::GptDetectError::new(
                "未配置 OpenAI API Key，请设置 gptDetect.apiKey 或环境变量 OPENAI_API_KEY",
                None,
                Some("CONFIG_ERROR"),
            );
            self.send_message(event, text_payload(format_gpt_detect_error(&error)))
                .await;
            return true;
        }
        if self
            .gpt_detect_busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            self.send_message(event, text_payload(GPT_DETECT_BUSY_TEXT))
                .await;
            return true;
        }
        let _busy = BusyGuard(&self.gpt_detect_busy);

        let image = event.quoted_images[0].clone();
        info!(
            "[ai吗] 开始检测引用图片 ({}, {} bytes)",
            if image.content_type.is_empty() {
                "image"
            } else {
                &image.content_type
            },
            image
                .size
                .map(|size| size.to_string())
                .unwrap_or_else(|| "未知大小".into())
        );
        self.send_message(event, text_payload(GPT_DETECT_WAITING_TEXT))
            .await;
        match detect_openai_image(&image, &self.config.gpt_detect).await {
            Ok(result) => {
                info!(
                    "[ai吗] 检测完成: {}",
                    if result.detected {
                        "detected"
                    } else {
                        "not_detected"
                    }
                );
                self.send_quoted_message(event, &format_gpt_detect_reply(&result))
                    .await;
            }
            Err(error) => {
                warn!("[ai吗] 检测失败: {error}");
                self.send_message(event, text_payload(format_gpt_detect_error(&error)))
                    .await;
            }
        }
        true
    }

    async fn send_quoted_message(&self, event: &QQChatEvent, content: &str) {
        let Some(quoted_message_id) = &event.quoted_message_id else {
            self.send_message(event, text_payload(content)).await;
            return;
        };
        match self
            .bot
            .reply(event, &quoted_text_payload(content, quoted_message_id))
            .await
        {
            Ok(_) => {}
            Err(err) => {
                warn!("引用回复失败，降级为普通回复: {err}");
                self.send_message(event, text_payload(content)).await;
            }
        }
    }

    async fn send_message(&self, event: &QQChatEvent, payload: Value) -> bool {
        if let Err(err) = self.bot.reply(event, &payload).await {
            warn!("QQ 消息发送失败: {err}");
            return false;
        }
        true
    }

    async fn send_x_post_reply(&self, event: &QQChatEvent, payload: Value) -> bool {
        match self.bot.reply_passive(event, &payload).await {
            Ok(_) => true,
            Err(error) => {
                warn!("X 帖子回复发送失败: {error}");
                if let QQBotError::Api(error) = &error
                    && error.err_code == QQ_ERR_CONTENT_VIOLATION
                    && self.bot.remaining_passive_replies(event).await > 0
                {
                    self.bot
                        .reply_passive(
                            event,
                            &text_payload("⚠️ QQ 内容审核拦截，未发送此部分 X 帖子。"),
                        )
                        .await
                        .ok();
                }
                false
            }
        }
    }

    async fn send_x_post_replies(
        &self,
        event: &QQChatEvent,
        replies: impl IntoIterator<Item = String>,
    ) -> bool {
        for markdown in replies {
            if !self
                .send_x_post_reply(event, markdown_payload(markdown, false))
                .await
            {
                return false;
            }
        }
        true
    }

    fn claim_link(&self, event: &QQChatEvent, url: &str) -> Option<DedupClaim> {
        let interval = self.config.output.deduplication_interval;
        let key = format!("{:?}:{}:{url}", event.chat_type, event.chat_id);
        let now = Instant::now();
        let mut cache = self.dedup_cache.lock().ok()?;
        cache.retain(|_, entry| entry.pending || entry.expires > now);
        if cache.contains_key(&key) {
            return None;
        }
        if cache.len() >= self.config.resources.cache_entries.max(1) {
            let oldest = cache
                .iter()
                .filter(|(_, entry)| !entry.pending)
                .min_by_key(|(_, entry)| entry.expires)
                .map(|(key, _)| key.clone())?;
            cache.remove(&oldest);
        }
        let token = rand::random();
        cache.insert(
            key.clone(),
            DedupEntry {
                token,
                expires: now,
                pending: true,
            },
        );
        Some(DedupClaim {
            cache: Arc::clone(&self.dedup_cache),
            key,
            token,
            interval: Duration::from_secs(interval),
            committed: false,
        })
    }

    async fn upload_media_url(
        &self,
        event: &QQChatEvent,
        file_type: u32,
        url: &str,
    ) -> anyhow::Result<Value> {
        Ok(match event.chat_type {
            ChatType::Group => {
                self.bot
                    .upload_group_media(&event.chat_id, file_type, url)
                    .await?
            }
            ChatType::Private => {
                self.bot
                    .upload_user_media(&event.chat_id, file_type, url)
                    .await?
            }
        })
    }

    async fn upload_media_file(
        &self,
        event: &QQChatEvent,
        file_type: u32,
        file_name: &str,
        path: &Path,
    ) -> anyhow::Result<Value> {
        Ok(match event.chat_type {
            ChatType::Group => {
                self.bot
                    .upload_group_media_file(&event.chat_id, file_type, file_name, path)
                    .await?
            }
            ChatType::Private => {
                self.bot
                    .upload_user_media_file(&event.chat_id, file_type, file_name, path)
                    .await?
            }
        })
    }

    async fn send_attachment(
        &self,
        event: &QQChatEvent,
        attachment: &crate::types::Attachment,
    ) -> bool {
        let _cleanup = AttachmentCleanup(&attachment.path);
        let result = async {
            let extension = ARCHIVE_EXT_RE
                .find(attachment.file_name.as_deref().unwrap_or_default())
                .map(|found| found.as_str().to_ascii_lowercase())
                .unwrap_or_else(|| ".7z".into());
            let named_file = format!("密码{}{}", attachment.password, extension);
            let upload = self
                .upload_media_file(event, 4, &named_file, Path::new(&attachment.path))
                .await?;
            let file_info = upload
                .get("file_info")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("文件上传响应缺少 file_info"))?;
            if !self.send_message(event, media_payload(file_info)).await {
                anyhow::bail!("附件消息发送失败");
            }
            info!("加密压缩包已发送: {named_file}");
            Ok::<(), anyhow::Error>(())
        }
        .await;
        let succeeded = result.is_ok();
        if let Err(err) = result {
            warn!("附件发送失败: {err}");
            self.send_message(
                event,
                text_payload(format!(
                    "⚠️ 压缩包发送失败: {err}\n密码: {}",
                    attachment.password
                )),
            )
            .await;
        }
        succeeded
    }

    async fn send_live_album_result(
        &self,
        event: &QQChatEvent,
        parsed: &ParsedData,
        text: &str,
    ) -> bool {
        if parsed.live_photo.is_empty() {
            self.send_message(event, text_payload("原帖只返回了静态图片，下面是原图集。"))
                .await;
            return self.send_gallery_result(event, parsed, text, None).await;
        }
        let started = Instant::now();
        let Ok(permit) = LIVE_ALBUM_GATE.try_acquire() else {
            self.send_message(
                event,
                text_payload("另一个实况图集正在合并，先发原图。想看视频可以稍后重试。"),
            )
            .await;
            self.send_gallery_result(event, parsed, text, None).await;
            return false;
        };
        let file_info = async {
            let video = download_live_album_temp(parsed, &self.config).await?;
            let file_name = format!("live_album_{}.mp4", random_suffix(12));
            let upload = self
                .upload_media_file(event, 2, &file_name, video.path())
                .await?;
            let file_info = file_info_from_upload(upload)?;
            info!(
                clips = parsed.live_photo.len(),
                bytes = video.size(),
                elapsed_ms = started.elapsed().as_millis(),
                "实况图集已合并并上传"
            );
            Ok::<_, anyhow::Error>(file_info)
        }
        .await;
        drop(permit);

        let file_info = match file_info {
            Ok(file_info) => file_info,
            Err(error) => {
                warn!("实况图集无法合并或上传: {error}");
                self.send_message(
                    event,
                    text_payload("实况短片暂时无法合并发送，下面是原图集。"),
                )
                .await;
                return self.send_gallery_result(event, parsed, text, None).await;
            }
        };
        match self.bot.reply(event, &media_payload(file_info)).await {
            Ok(_) => {}
            Err(QQBotError::Api(error)) => {
                warn!("QQ 拒绝发送合并后的实况视频: {error}");
                self.send_message(event, text_payload("QQ 拒绝发送实况视频，下面是原图集。"))
                    .await;
                return self.send_gallery_result(event, parsed, text, None).await;
            }
            Err(error) => {
                warn!("实况视频发送结果未确认: {error}");
                self.send_message(
                    event,
                    text_payload("实况视频发送结果未确认，请查看是否已收到。"),
                )
                .await;
                return false;
            }
        }
        let static_pages = parsed.images.len().saturating_sub(parsed.live_photo.len());
        let mut caption = if self.config.output.show_image_text {
            text.to_owned()
        } else {
            String::new()
        };
        if static_pages > 0 {
            caption.push_str(&format!(
                "\n已按顺序合并 {} 段实况短片。另有 {static_pages} 页只取得静态图，未纳入视频。",
                parsed.live_photo.len()
            ));
        }
        if !caption.trim().is_empty() {
            self.send_message(event, markdown_payload(caption, false))
                .await;
        }
        true
    }

    async fn send_media_result(
        &self,
        event: &QQChatEvent,
        parsed: ParsedData,
        text: String,
        remaining_results: usize,
    ) -> bool {
        let x_reply_budget = if (parsed.is_text_post() || self.config.output.show_image_text)
            && (!parsed.post_body.is_empty() || parsed.post_body_is_preview)
        {
            let budget = self
                .bot
                .remaining_passive_replies(event)
                .await
                .saturating_sub(remaining_results)
                .min(X_MAX_POST_REPLIES);
            if budget == 0 {
                warn!("当前消息的剩余 QQ 回复额度不足，未发送 X 帖子");
                return false;
            }
            Some(budget)
        } else {
            None
        };
        let is_video = !parsed.video.is_empty()
            && (parsed.media_type == "video"
                || (parsed.media_type == "live"
                    && parsed.live_photo.is_empty()
                    && parsed.images.is_empty()));
        let is_gallery = parsed.media_type == "image"
            || parsed.media_type == "live_photo"
            || (parsed.media_type == "live"
                && (!parsed.images.is_empty() || !parsed.live_photo.is_empty()));
        if is_gallery {
            return self
                .send_gallery_result(event, &parsed, &text, x_reply_budget)
                .await;
        }

        let mut video_file_info = String::new();
        let mut video_failed_note = String::new();
        let video_reply = is_video && self.config.output.show_video_file;
        let one_reply_for_video_post = video_reply && x_reply_budget == Some(1);
        if video_reply && !one_reply_for_video_post {
            let relay_video = || async {
                let download_started = Instant::now();
                let temporary =
                    download_parsed_temp(&parsed, &self.config)
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!(
                                "下载或合流失败（{} ms）: {error}",
                                download_started.elapsed().as_millis()
                            )
                        })?;
                let download_ms = download_started.elapsed().as_millis();
                let upload_started = Instant::now();
                let file_name = format!("video_{}.mp4", chrono::Utc::now().timestamp_millis());
                let upload = self
                    .upload_media_file(event, 2, &file_name, temporary.path())
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "QQ 上传失败（{} ms，下载 {} ms）: {error}",
                            upload_started.elapsed().as_millis(),
                            download_ms
                        )
                    })?;
                let file_info = upload
                    .get("file_info")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("分片上传响应缺少 file_info"))?
                    .to_string();
                info!(
                    bytes = temporary.size(),
                    download_ms,
                    qq_upload_ms = upload_started.elapsed().as_millis(),
                    "视频已本地上传中转"
                );
                Ok::<String, anyhow::Error>(file_info)
            };

            if parsed.video_decode_key.is_some()
                || parsed.audio_url.is_some()
                || is_foreign_url(&parsed.video)
                || !parsed.media_headers.is_empty()
            {
                match relay_video().await {
                    Ok(file_info) => video_file_info = file_info,
                    Err(err) => {
                        warn!("视频中转失败: {err}");
                        video_failed_note = concise_video_transfer_note(&err);
                    }
                }
            } else {
                let platform_upload_max = 30 * 1024 * 1024;
                let size_hint = head_content_length(&parsed.video, &self.local_proxies).await;
                if size_hint > platform_upload_max {
                    info!(
                        "视频较大({:.1}MB)，跳过平台转存直走本地上传",
                        size_hint as f64 / 1024.0 / 1024.0
                    );
                    match relay_video().await {
                        Ok(file_info) => video_file_info = file_info,
                        Err(err) => {
                            warn!("视频中转失败: {err}");
                            video_failed_note = concise_video_transfer_note(&err);
                        }
                    }
                } else {
                    match self
                        .upload_media_url(event, 2, &parsed.video)
                        .await
                        .and_then(file_info_from_upload)
                    {
                        Ok(file_info) => {
                            video_file_info = file_info;
                            info!(
                                "视频已转存: {}",
                                parsed.video.chars().take(80).collect::<String>()
                            );
                        }
                        Err(first_error) => {
                            warn!("平台转存视频失败，1.5s 后重试: {first_error}");
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                            match self
                                .upload_media_url(event, 2, &parsed.video)
                                .await
                                .and_then(file_info_from_upload)
                            {
                                Ok(file_info) => {
                                    video_file_info = file_info;
                                    info!(
                                        "平台转存重试成功: {}",
                                        parsed.video.chars().take(80).collect::<String>()
                                    );
                                }
                                Err(second_error) => {
                                    warn!("平台转存重试失败，尝试本地上传中转: {second_error}");
                                    match relay_video().await {
                                        Ok(file_info) => video_file_info = file_info,
                                        Err(err) => {
                                            warn!("视频中转失败: {err}");
                                            video_failed_note = concise_video_transfer_note(&err);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let transfer_succeeded = video_failed_note.is_empty();
        let is_text_post = parsed.is_text_post();
        let mut delivered = true;
        let mut markdown = build_result_markdown(
            &ParsedData::empty("video"),
            &text,
            &MarkdownOptions {
                show_image_text: self.config.output.show_image_text || is_text_post,
                show_cover: false,
                max_images: 0,
            },
        );
        if is_text_post && markdown.trim().is_empty() {
            let description = if parsed.post_body.is_empty() {
                &parsed.desc
            } else {
                &parsed.post_body
            };
            markdown = if !parsed.title.is_empty() && !description.is_empty() {
                format!("{}\n{}", parsed.title, description)
            } else if !description.is_empty() {
                description.clone()
            } else {
                parsed.title.clone()
            };
        }
        if !video_failed_note.is_empty() {
            markdown.push('\n');
            markdown.push_str(&video_failed_note);
        }
        if let Some(budget) = x_reply_budget {
            if one_reply_for_video_post {
                append_markdown_part(
                    &mut markdown,
                    &x_original_post_notice(
                        "回复额度不足，视频请查看原帖",
                        &parsed.source_post_url,
                    ),
                );
            }
            let text_budget = budget - usize::from(!video_file_info.is_empty());
            let replies = x_post_markdown_replies(markdown, String::new(), &parsed, text_budget);
            delivered &= self.send_x_post_replies(event, replies).await;
        } else if !markdown.trim().is_empty() {
            delivered &= self
                .send_message(event, markdown_payload(markdown, false))
                .await;
        } else if is_text_post {
            delivered = false;
        }
        if !video_file_info.is_empty() {
            let payload = media_payload(video_file_info);
            delivered &= if x_reply_budget.is_some() {
                self.send_x_post_reply(event, payload).await
            } else {
                self.send_message(event, payload).await
            };
        }
        transfer_succeeded && delivered
    }

    pub async fn cleanup_markdown_materials(&self) {
        let _gate = self.gallery_material_gate.lock().await;
        self.cleanup_due_materials().await;
    }

    fn with_materials<T>(
        &self,
        operation: impl FnOnce(&mut OwnedMaterials) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut state = self
            .owned_materials
            .lock()
            .map_err(|_| anyhow::anyhow!("素材记录锁不可用"))?;
        let owned = state
            .as_mut()
            .map_err(|error| anyhow::anyhow!("素材记录不可用: {error}"))?;
        operation(owned)
    }

    async fn cleanup_due_materials(&self) {
        let ids = match self.with_materials(|owned| {
            if owned.is_dirty() {
                owned.flush()?;
            }
            Ok(owned.due(unix_now()?))
        }) {
            Ok(ids) => ids,
            Err(error) => {
                warn!("读取待清理素材失败: {error}");
                return;
            }
        };
        if ids.is_empty() {
            return;
        }
        let cleanup = async {
            for id in ids {
                match self.bot.delete_markdown_image(&id).await {
                    Ok(()) => {
                        if let Err(error) = self
                            .with_materials(|owned| owned.mark_removed(std::slice::from_ref(&id)))
                        {
                            warn!("更新素材清理记录失败: {error}");
                            break;
                        }
                    }
                    Err(error) => {
                        warn!("本项目 Markdown 素材清理暂未完成: {error}");
                        break;
                    }
                }
            }
        };
        if tokio::time::timeout(Duration::from_secs(10), cleanup)
            .await
            .is_err()
        {
            warn!("本项目素材清理达到时间上限，保留记录供后续重试");
        }
    }

    async fn release_gallery_materials(&self, ids: &[String]) {
        if let Err(error) = self.with_materials(|owned| owned.mark_due(ids, 0)) {
            warn!("记录本批素材清理期限失败: {error}");
        }
        self.cleanup_due_materials().await;
    }

    async fn send_gallery_result(
        &self,
        event: &QQChatEvent,
        parsed: &ParsedData,
        text: &str,
        x_reply_budget: Option<usize>,
    ) -> bool {
        // The account may deduplicate material IDs. Keep ownership, publication and
        // deletion in one bounded gallery turn so another reply cannot lose its material.
        let gate = self.gallery_material_gate.lock().await;
        self.cleanup_due_materials().await;
        if let Err(error) = self.with_materials(|owned| owned.ensure_capacity(1)) {
            warn!("图集暂不可创建素材: {error}");
            self.send_message(event, text_payload("⚠️ 图集素材状态暂不可用，请稍后重试。"))
                .await;
            return false;
        }
        let max_images = if self.config.output.max_markdown_images == 0 {
            DEFAULT_MAX_MARKDOWN_IMAGES
        } else {
            self.config.output.max_markdown_images
        };
        let sources: Vec<&str> = if parsed.images.is_empty() {
            parsed
                .live_photo
                .iter()
                .map(|photo| photo.image.as_str())
                .collect()
        } else {
            parsed.images.iter().map(String::as_str).collect()
        };
        let selected_count = sources.len().min(max_images);
        let omitted_count = sources.len() - selected_count;
        info!(
            total_images = sources.len(),
            selected_images = selected_count,
            omitted_images = omitted_count,
            configured_limit = max_images,
            "图集图片数量已核对"
        );
        let prior_ids = self
            .with_materials(|owned| {
                Ok(owned
                    .entries()
                    .iter()
                    .map(|entry| entry.id.clone())
                    .collect::<Vec<_>>())
            })
            .unwrap_or_default();
        let mut images = Vec::with_capacity(selected_count);
        for (index, source) in sources.into_iter().take(selected_count).enumerate() {
            match self
                .prepare_gallery_image(source, &parsed.media_headers)
                .await
            {
                Ok(image) => images.push(image),
                Err(error) => warn!(image_index = index + 1, "图集图片素材准备失败: {error}"),
            }
        }
        let releasable: Vec<String> = images
            .iter()
            .filter(|image| !prior_ids.contains(&image.image_id))
            .map(|image| image.image_id.clone())
            .collect();
        let failed_count = selected_count - images.len();
        let (markdown, followups) = if let Some(budget) = x_reply_budget {
            let image_markup = self.gallery_markdown("", &images, failed_count, omitted_count);
            let caption = if self.config.output.show_image_text {
                text
            } else {
                ""
            };
            let mut replies =
                x_post_markdown_replies(caption.to_owned(), image_markup, parsed, budget)
                    .into_iter();
            (replies.next().unwrap_or_default(), replies)
        } else {
            (
                self.gallery_markdown(text, &images, failed_count, omitted_count),
                Vec::new().into_iter(),
            )
        };
        let payload = markdown_payload(markdown, !images.is_empty());
        let sent = if x_reply_budget.is_some() {
            self.bot.reply_passive(event, &payload).await
        } else {
            self.bot.reply(event, &payload).await
        };
        match sent {
            Ok(_) => {
                info!(
                    "图集素材 Markdown 已发送，{} 张图片，{} 张准备失败，{} 张受配置上限限制",
                    images.len(),
                    failed_count,
                    omitted_count
                );
                // QQ stores a sent-message copy, so owned materials can be deleted
                // after its successful acknowledgement without breaking that message.
                self.release_gallery_materials(&releasable).await;
                drop(gate);
                let caption_delivered = self.send_x_post_replies(event, followups).await;
                caption_delivered && failed_count == 0 && !images.is_empty()
            }
            Err(error) => {
                warn!("图集素材 Markdown 发送未完成: {error}");
                let notice = match error {
                    QQBotError::Api(error) => {
                        self.release_gallery_materials(&releasable).await;
                        concise_gallery_api_error_note(selected_count, &error)
                    }
                    QQBotError::Other(_) => {
                        let ids: Vec<String> =
                            images.iter().map(|image| image.image_id.clone()).collect();
                        let deadline = unix_now().unwrap_or(0).saturating_add(300);
                        if let Err(error) =
                            self.with_materials(|owned| owned.mark_due(&ids, deadline))
                        {
                            warn!("延后清理未确认素材的记录更新失败: {error}");
                        }
                        format!(
                            "⚠️ 图集发送结果未确认，本次共 {selected_count} 张图片，请检查群内显示，缺少时再重试。"
                        )
                    }
                };
                self.send_message(event, text_payload(notice)).await;
                false
            }
        }
    }

    fn gallery_markdown(
        &self,
        text: &str,
        images: &[GalleryImage],
        failed_count: usize,
        omitted_count: usize,
    ) -> String {
        let mut markdown = if self.config.output.show_image_text {
            text.to_owned()
        } else {
            String::new()
        };
        for image in images {
            if !markdown.is_empty() {
                markdown.push('\n');
            }
            markdown.push_str(&markdown_image_tag(&image.url, Some(&image.size)));
        }
        if failed_count > 0 {
            markdown.push_str(&format!("\n⚠️ {failed_count} 张图片准备失败，未发送。支持 PNG/JPEG/GIF/WebP，单张上限 10 MB。"));
        }
        if omitted_count > 0 {
            markdown.push_str(&format!(
                "\n（其余 {omitted_count} 张图片受配置上限限制，未显示）"
            ));
        }
        if images.is_empty() && failed_count == 0 {
            markdown.push_str("\n⚠️ 图集中没有可显示的图片。");
        }
        markdown
    }

    async fn prepare_gallery_image(
        &self,
        image: &str,
        headers: &HashMap<String, String>,
    ) -> anyhow::Result<GalleryImage> {
        self.with_materials(|owned| owned.ensure_capacity(1))?;
        let buffer = download_media_buffer_with_headers(
            image,
            &self.local_proxies,
            MARKDOWN_IMAGE_MAX_BYTES as u64,
            headers,
        )
        .await?;
        let metadata = probe_image(&buffer)?;
        let name = format!("img_{}.{}", random_suffix(12), metadata.extension);
        let uploaded = self
            .bot
            .upload_markdown_image_tracked(&name, metadata.mime_type, &buffer, |id| {
                self.with_materials(|owned| owned.track(id))
                    .map_err(QQBotError::from)
            })
            .await?;
        info!(
            "图集图片素材已创建，{} KiB，显示尺寸 {}×{}",
            buffer.len() / 1024,
            metadata.size.width,
            metadata.size.height
        );
        Ok(GalleryImage {
            image_id: uploaded.image_id,
            url: uploaded.url,
            size: markdown_size(&metadata.size),
        })
    }
}

fn append_markdown_part(markdown: &mut String, part: &str) {
    if !part.is_empty() {
        if !markdown.is_empty() && !markdown.ends_with('\n') && !part.starts_with('\n') {
            markdown.push('\n');
        }
        markdown.push_str(part);
    }
}

fn x_original_post_notice(notice: &str, source_post_url: &str) -> String {
    if X_CANONICAL_URL_RE.is_match(source_post_url) {
        format!("{notice}：{source_post_url}")
    } else {
        format!("{notice}。")
    }
}

fn x_post_markdown_replies(
    mut caption: String,
    image_markup: String,
    post: &ParsedData,
    budget: usize,
) -> Vec<String> {
    if budget == 0 {
        return Vec::new();
    }
    if post.post_body_is_preview {
        append_markdown_part(
            &mut caption,
            &x_original_post_notice("正文预览，完整内容请查看原帖", &post.source_post_url),
        );
    }
    if caption.len() + image_markup.len() < X_MARKDOWN_REPLY_BYTES || caption.is_empty() {
        append_markdown_part(&mut caption, &image_markup);
        return if caption.is_empty() {
            Vec::new()
        } else {
            vec![caption]
        };
    }

    let link_note = x_original_post_notice(
        "此处仅展示可发送的正文，完整内容请查看原帖",
        &post.source_post_url,
    );
    let mut replies = Vec::with_capacity(budget);
    let mut rest = caption.as_str();
    for index in 0..budget {
        let images = if index == 0 {
            image_markup.as_str()
        } else {
            ""
        };
        let capacity =
            X_MARKDOWN_REPLY_BYTES.saturating_sub(images.len() + usize::from(!images.is_empty()));
        let needs_note = index + 1 == budget && rest.len() > capacity;
        let body_room = if needs_note {
            capacity.saturating_sub(link_note.len() + 1)
        } else {
            capacity
        };
        let cut = x_caption_split_at(rest, body_room);
        let mut message = rest[..cut].to_owned();
        rest = &rest[cut..];

        if needs_note {
            // A gallery is sent atomically. Even with no room for the body, the
            // notice may use the remaining caption capacity before the images.
            append_markdown_part(&mut message, &link_note);
        }
        append_markdown_part(&mut message, images);
        if !message.is_empty() {
            replies.push(message);
        }
        if rest.is_empty() {
            break;
        }
    }
    replies
}

fn x_caption_split_at(text: &str, capacity: usize) -> usize {
    if text.len() <= capacity {
        return text.len();
    }
    let end = text.floor_char_boundary(capacity);
    if end == 0 {
        return 0;
    }
    let minimum = text.floor_char_boundary(end / 2);
    let suffix = &text[minimum..end];
    suffix
        .rfind("\n\n")
        .map(|index| index + 2)
        .or_else(|| suffix.rfind('\n').map(|index| index + 1))
        .or_else(|| {
            suffix
                .char_indices()
                .rev()
                .find(|(_, ch)| ch.is_whitespace())
                .map(|(index, ch)| index + ch.len_utf8())
        })
        .map_or(end, |index| minimum + index)
}

fn file_info_from_upload(upload: Value) -> anyhow::Result<String> {
    upload
        .get("file_info")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("上传响应缺少 file_info"))
}

fn concise_user_parse_error(message: &str) -> String {
    let base = message
        .split("；本地 yt-dlp 解析失败")
        .next()
        .unwrap_or(message)
        .trim();
    if let Some(module) = base.strip_prefix("JM 缺少 Python 依赖 ")
        && !module.is_empty()
        && module.len() <= 64
        && module
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_'))
    {
        return base.to_owned();
    }
    if base.starts_with("X 登录态无效或已过期") || base.starts_with("X 登录态不可用")
    {
        return "X 登录态不可用，请联系管理员检查 Cookie".into();
    }
    if base.starts_with("X 原站拒绝访问") || base.starts_with("当前 X 账号没有查看此帖的权限")
    {
        return "当前账号在 X 上无法查看原帖，请检查作品和账号权限".into();
    }
    if base.starts_with("X 未返回可用的直链 MP4") || base.starts_with("X 未返回受支持的图片")
    {
        return "X 未返回支持的媒体格式，请前往原帖查看".into();
    }
    if base.starts_with("X 暂时限流") || base.starts_with("X 详情查询暂时限流") {
        return "X 暂时限流，请稍后再试".into();
    }
    if base.contains("此动态为充电会员专属") {
        return "此动态为充电会员专属，不可访问".into();
    }
    if base.contains("源动态已被删除或不可见") {
        return "源动态已被删除或不可见".into();
    }
    if base.contains("该动态已删除或设为不可见") || base.contains("该动态已删除或不可见")
    {
        return "该动态已被删除或不可见".into();
    }
    if base.contains("仅自己可见或已被删除")
        || base.contains("动态可能不存在")
        || base.contains("专栏不存在或已被删除")
    {
        return "该动态或专栏不存在、仅自己可见或已被删除".into();
    }
    if base.contains("纯文字") {
        return "该帖子为纯文字内容，不包含视频或图片".into();
    }
    if base.contains("B站原站返回 -404")
        || base.contains("视频不存在或已被删除")
        || message.contains("This video may be deleted")
    {
        return "该视频不存在或已被删除".into();
    }
    if base.contains("试看") || base.contains("-10403") || base.contains("地区或版权限制")
    {
        return "该作品为试看或受地区/权限限制，暂无法提取".into();
    }
    if base.contains("冷却")
        || base.contains("风控")
        || base.contains("HTTP 429")
        || base.contains("HTTP 412")
        || base.contains("-352")
    {
        return "平台暂时限流或触发访问验证，请稍后再试".into();
    }
    if base.contains("X 未返回公开媒体") || base.contains("X 公开图片提取失败") {
        return "该 X 帖子已删除、设私或受年龄/登录限制".into();
    }
    if base.contains("抖音分享页未提供可下载的作品媒体")
        || base.contains("抖音未返回此图集的媒体详情")
        || base.contains("原站过滤码")
    {
        return "该抖音作品暂不可访问（可能已删除、设为私密或需登录）".into();
    }
    if base.contains("小红书该笔记不存在") || base.contains("已删除或暂不可访问")
    {
        return "该小红书笔记已删除、失效或暂不可访问".into();
    }
    if base.contains("小红书未提供可访问的笔记媒体") || base.contains("小红书需要登录态")
    {
        return "该小红书笔记暂无法访问（可能已删除、链接失效或需要登录）".into();
    }
    if base.contains("元宝")
        && (base.contains("401") || base.contains("20002") || base.contains("登录"))
    {
        return "微信视频号解析登录态已失效，请联系管理员更新".into();
    }
    if base.contains("缺少BV、av、ep") {
        return "该链接不是可解析的 B站作品、动态或专栏地址".into();
    }
    if base.contains("Cookie") || base.contains("登录") {
        return "该作品需要登录态或访问权限，暂无法提取".into();
    }
    if base.is_empty() {
        "未知错误".into()
    } else {
        base.chars().take(80).collect()
    }
}

fn concise_video_transfer_note(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    if text.contains("群文件超过上限") {
        return "⚠️ 视频文件过大，超出当前 QQ 群文件上限，无法发送。".into();
    }
    if text.contains("超过大小上限")
        || text.contains("超过预留的文件大小上限")
        || text.contains("临时媒体总量已达到磁盘预算")
    {
        return "⚠️ 视频超过大小上限，无法发送。".into();
    }
    if text.contains("富媒体文件上传超时")
        || (text.contains("QQ 上传失败") && (text.contains("超时") || text.contains("timed out")))
    {
        return "⚠️ 视频上传 QQ 超时，请稍后重试。".into();
    }
    if text.contains("40034006") || text.contains("消息内容违规") {
        return "⚠️ 消息内容违规，触发腾讯审核拦截，无法发送。（QQ 错误码 40034006）".into();
    }
    if text.contains("QQ 上传失败") {
        return "⚠️ QQ 暂无法接收该视频，请稍后再试。".into();
    }
    if text.contains("视频下载超时")
        || text.contains("执行超时")
        || text.contains("timed out")
        || text.contains("停滞超时")
    {
        return "⚠️ 视频下载超时（文件较大或原站拉取较慢），请稍后再试。".into();
    }
    "⚠️ 视频下载或处理失败，请稍后再试。".into()
}

fn concise_gallery_api_error_note(selected_count: usize, error: &QQApiError) -> String {
    if error.err_code == QQ_ERR_CONTENT_VIOLATION {
        return format!(
            "⚠️ 图集发送失败，本次 {selected_count} 张图片未发送：消息内容违规，触发腾讯审核拦截。（QQ 错误码 {}）",
            error.err_code
        );
    }
    format!(
        "⚠️ 图集发送失败，本次 {selected_count} 张图片未发送，请稍后重试。（QQ 错误码 {}）",
        error.err_code
    )
}

fn render_parse_error(config: &AppConfig, url: &str, message: &str) -> String {
    config
        .ui_text
        .parse_error_item_format
        .replace("${url}", url)
        .replace(
            "${msg}",
            if message.is_empty() {
                "未知错误"
            } else {
                message
            },
        )
}

fn random_suffix(length: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = vec![0u8; length];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes
        .into_iter()
        .map(|byte| CHARS[usize::from(byte) % CHARS.len()] as char)
        .collect()
}

pub fn attachment_extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!(".{}", extension.to_ascii_lowercase()))
        .unwrap_or_else(|| ".7z".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        let mut config = AppConfig::default();
        config.resources.cache_entries = 2;
        let config = Arc::new(config);
        AppState::new(
            Arc::clone(&config),
            Arc::new(VideoParserService::new((*config).clone()).unwrap()),
            Arc::new(QQBotClient::new(config.qqbot.clone())),
        )
    }

    #[test]
    fn scopes_link_dedup_to_chat_and_releases_failed_claims() {
        let state = state();
        let first = QQChatEvent {
            chat_id: "first".into(),
            ..QQChatEvent::default()
        };
        let second = QQChatEvent {
            chat_id: "second".into(),
            ..QQChatEvent::default()
        };
        let mut claim = state
            .claim_link(&first, "https://example.com/video")
            .unwrap();
        assert!(
            state
                .claim_link(&first, "https://example.com/video")
                .is_none()
        );
        let second_claim = state
            .claim_link(&second, "https://example.com/video")
            .unwrap();
        drop(second_claim);
        assert!(
            state
                .claim_link(&second, "https://example.com/video")
                .is_some()
        );
        claim.commit();
        drop(claim);
        assert!(
            state
                .claim_link(&first, "https://example.com/video")
                .is_none()
        );
        assert!(state.dedup_cache.lock().unwrap().len() <= 2);
    }

    #[test]
    fn maps_non_bug_failures_to_concise_group_notices() {
        assert_eq!(
            concise_user_parse_error("JM 缺少 Python 依赖 requests"),
            "JM 缺少 Python 依赖 requests"
        );
        assert_eq!(
            concise_user_parse_error("此动态为充电会员专属，不可访问"),
            "此动态为充电会员专属，不可访问"
        );
        assert_eq!(
            concise_user_parse_error("源动态已被删除或不可见"),
            "源动态已被删除或不可见"
        );
        assert_eq!(
            concise_user_parse_error("B站原站返回 4101131，动态不存在、仅自己可见或已被删除"),
            "该动态或专栏不存在、仅自己可见或已被删除"
        );
        assert_eq!(
            concise_user_parse_error("该 X 帖子为纯文字内容，不包含视频或图片"),
            "该帖子为纯文字内容，不包含视频或图片"
        );
        assert_eq!(
            concise_user_parse_error(
                "B站原站返回 -404，视频不存在或已被删除；本地 yt-dlp 解析失败 ERROR: deleted"
            ),
            "该视频不存在或已被删除"
        );
        assert_eq!(
            concise_user_parse_error("小红书该笔记不存在、已删除或暂不可访问"),
            "该小红书笔记已删除、失效或暂不可访问"
        );
        assert_eq!(
            concise_user_parse_error(
                "X 公开图片提取失败。X 未返回公开媒体（原帖可能已删除、设私或受年龄/登录限制）"
            ),
            "该 X 帖子已删除、设私或受年龄/登录限制"
        );
        assert_eq!(
            concise_video_transfer_note(&anyhow::anyhow!(
                "QQ 上传失败（478 ms，下载 212532 ms）: 群文件超过上限"
            )),
            "⚠️ 视频文件过大，超出当前 QQ 群文件上限，无法发送。"
        );
        assert_eq!(
            concise_video_transfer_note(&anyhow::anyhow!(
                "QQ 上传失败（11197 ms，下载 76583 ms）: 富媒体文件上传超时"
            )),
            "⚠️ 视频上传 QQ 超时，请稍后重试。"
        );
        assert_eq!(
            concise_video_transfer_note(&anyhow::anyhow!(
                "下载或合流失败（300014 ms）: 媒体下载失败: 视频下载超时（超过 300 秒）"
            )),
            "⚠️ 视频下载超时（文件较大或原站拉取较慢），请稍后再试。"
        );
        assert_eq!(
            concise_video_transfer_note(&anyhow::anyhow!(
                "QQ 上传失败（800 ms，下载 1200 ms）: {}",
                QQApiError::new(
                    QQ_ERR_CONTENT_VIOLATION,
                    "消息内容违规",
                    Some("trace-v1".into())
                )
            )),
            "⚠️ 消息内容违规，触发腾讯审核拦截，无法发送。（QQ 错误码 40034006）"
        );
        assert_eq!(
            concise_gallery_api_error_note(
                2,
                &QQApiError::new(
                    QQ_ERR_CONTENT_VIOLATION,
                    "消息内容违规",
                    Some("trace-g1".into())
                )
            ),
            "⚠️ 图集发送失败，本次 2 张图片未发送：消息内容违规，触发腾讯审核拦截。（QQ 错误码 40034006）"
        );
        assert_eq!(
            concise_gallery_api_error_note(3, &QQApiError::new(40034141, "转存失败", None)),
            "⚠️ 图集发送失败，本次 3 张图片未发送，请稍后重试。（QQ 错误码 40034141）"
        );
    }

    #[test]
    fn x_post_markdown_keeps_the_gallery_and_splits_on_paragraphs() {
        let post = ParsedData::empty("image");
        let first = format!("{}\n\n", "正文🙂".repeat(240));
        let second = "第二段。".repeat(240);
        let caption = format!("{first}{second}");
        let gallery = "![#100px #100px](qqbot-media://image/v1/one)\n![#100px #100px](qqbot-media://image/v1/two)";
        let replies = x_post_markdown_replies(caption, gallery.into(), &post, 3);
        assert_eq!(replies, [format!("{first}{gallery}"), second]);
        assert!(
            replies
                .iter()
                .all(|message| message.len() <= X_MARKDOWN_REPLY_BYTES)
        );
    }

    #[test]
    fn x_post_markdown_labels_preview_and_respects_the_reply_budget() {
        let mut post = ParsedData::empty("text");
        post.source_post_url = "https://x.com/i/web/status/1234".into();
        post.post_body_is_preview = true;
        let replies = x_post_markdown_replies("可见正文".into(), String::new(), &post, 1);
        assert_eq!(
            replies,
            ["可见正文\n正文预览，完整内容请查看原帖：https://x.com/i/web/status/1234"]
        );

        post.post_body_is_preview = false;
        let body = "🙂".repeat(2500);
        let replies = x_post_markdown_replies(body, String::new(), &post, 2);
        assert_eq!(replies.len(), 2);
        assert!(
            replies
                .iter()
                .all(|message| message.len() <= X_MARKDOWN_REPLY_BYTES)
        );
        assert!(replies[1].ends_with(&post.source_post_url));
        assert!(replies[1].contains("此处仅展示可发送的正文"));
    }

    #[tokio::test]
    async fn send_media_result_delivers_text_only_posts_as_markdown_even_when_show_image_text_disabled()
     {
        use axum::{Json, Router, routing::post};
        use serde_json::json;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        let sent = Arc::new(Mutex::new(Vec::<Value>::new()));
        let captured = Arc::clone(&sent);
        let app = Router::new()
            .route(
                "/app/getAppAccessToken",
                post(|| async { Json(json!({"access_token":"TEXT_TOKEN","expires_in":7200})) }),
            )
            .route(
                "/v2/groups/{group}/messages",
                post(move |Json(body): Json<Value>| {
                    let captured = Arc::clone(&captured);
                    async move {
                        captured.lock().await.push(body);
                        Json(json!({"id":"sent-text-1"}))
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut config = AppConfig::default();
        config.qqbot.app_id = "app".into();
        config.qqbot.app_secret = "secret".into();
        config.qqbot.api_base = base;
        config.output.show_image_text = false;
        let config = Arc::new(config);
        let app_state = AppState::new(
            Arc::clone(&config),
            Arc::new(VideoParserService::new((*config).clone()).unwrap()),
            Arc::new(QQBotClient::new(config.qqbot.clone())),
        );

        let mut parsed = ParsedData::empty("text");
        parsed.author = "陈睿".into();
        parsed.desc = "就在今天！\n我们是冠军！！！".into();
        parsed.like = 38000;
        let formatted = generate_formatted_text(&parsed, &config.output.format);
        let event = QQChatEvent {
            chat_type: ChatType::Group,
            chat_id: "GROUP_TEXT".into(),
            message_id: "MSG_TEXT_1".into(),
            ..QQChatEvent::default()
        };

        assert!(
            app_state
                .send_media_result(&event, parsed, formatted, 0)
                .await
        );
        let messages = sent.lock().await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["msg_type"], 2);
        assert!(
            messages[0]["markdown"]
                .get("force_verify_image_resource")
                .is_none()
        );
        let md = messages[0]["markdown"]["content"].as_str().unwrap();
        assert!(md.contains("作者：陈睿"));
        assert!(md.contains("就在今天！\n我们是冠军！！！"));
        assert!(md.contains("点赞：38000"));
        drop(messages);

        let mut x_post = ParsedData::empty("text");
        x_post.desc = "旧的预览".into();
        x_post.post_body = format!("{}\n\n第二段的尾巴", "长文🙂".repeat(360));
        x_post.source_post_url = "https://x.com/i/status/123".into();
        let tail = "第二段的尾巴";
        let formatted = generate_formatted_text(&x_post, &config.output.format);
        assert!(formatted.contains(tail));
        assert!(!formatted.contains("旧的预览"));
        assert!(
            app_state
                .send_media_result(&event, x_post, formatted, 1)
                .await
        );

        let messages = sent.lock().await;
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["msg_type"], 2);
        assert_eq!(messages[2]["msg_type"], 2);
        assert_eq!(messages[1]["msg_seq"], 2);
        assert_eq!(messages[2]["msg_seq"], 3);
        let continuation = messages[2]["markdown"]["content"].as_str().unwrap();
        assert!(continuation.ends_with(tail));
        server.abort();
    }

    #[tokio::test]
    async fn marked_x_video_uses_regular_qq_media_and_honors_moderation() {
        use axum::{
            Json, Router,
            http::StatusCode,
            routing::{head, post},
        };
        use serde_json::json;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let source = format!("{base}/fixture.mp4");
        let uploads = Arc::new(Mutex::new(Vec::<Value>::new()));
        let messages = Arc::new(Mutex::new(Vec::<Value>::new()));
        let received_uploads = Arc::clone(&uploads);
        let received_messages = Arc::clone(&messages);
        let app = Router::new()
            .route(
                "/app/getAppAccessToken",
                post(|| async { Json(json!({"access_token":"VIDEO_TOKEN","expires_in":7200})) }),
            )
            .route("/fixture.mp4", head(|| async { StatusCode::OK }))
            .route(
                "/v2/groups/{group}/files",
                post(move |Json(body): Json<Value>| {
                    let uploads = Arc::clone(&received_uploads);
                    async move {
                        uploads.lock().await.push(body);
                        Json(json!({"file_info":"synthetic-video"}))
                    }
                }),
            )
            .route(
                "/v2/groups/{group}/messages",
                post(move |Json(body): Json<Value>| {
                    let messages = Arc::clone(&received_messages);
                    async move {
                        let blocked = body["msg_id"] == "blocked";
                        messages.lock().await.push(body);
                        if blocked {
                            Json(json!({"err_code":QQ_ERR_CONTENT_VIOLATION,"message":"消息内容违规"}))
                        } else {
                            Json(json!({"id":"sent-video"}))
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.qqbot.app_id = "app".into();
        config.qqbot.app_secret = "secret".into();
        config.qqbot.api_base = base;
        config.output.show_image_text = false;
        config.output.temp_dir = directory.path().to_str().unwrap().into();
        let config = Arc::new(config);
        let app = AppState::new(
            Arc::clone(&config),
            Arc::new(VideoParserService::new((*config).clone()).unwrap()),
            Arc::new(QQBotClient::new(config.qqbot.clone())),
        );

        let video = crate::parser::twitter::map_ytdlp_json(&json!({
            "extractor_key": "Twitter",
            "age_limit": 18,
            "description": "测试短正文",
            "formats": [{
                "url": source,
                "ext": "mp4",
                "protocol": "http",
                "vcodec": "h264",
                "acodec": "aac"
            }]
        }));
        for (message_id, accepted) in [("allowed", true), ("blocked", false)] {
            let event = QQChatEvent {
                chat_type: ChatType::Group,
                chat_id: "GROUP_VIDEO".into(),
                message_id: message_id.into(),
                ..QQChatEvent::default()
            };
            let formatted = generate_formatted_text(&video, &config.output.format);
            assert_eq!(
                app.send_media_result(&event, video.clone(), formatted, 0)
                    .await,
                accepted
            );
        }

        let uploads = uploads.lock().await;
        assert_eq!(uploads.len(), 2);
        assert!(uploads.iter().all(|body| {
            body["file_type"] == 2 && body["url"] == source && body["srv_send_msg"] == false
        }));
        let messages = messages.lock().await;
        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|body| {
            body["msg_type"] == 7 && body["media"]["file_info"] == "synthetic-video"
        }));
        assert_eq!(messages[1]["msg_id"], "blocked");
        server.abort();
    }

    #[tokio::test]
    async fn openid_command_and_blacklist_workflow_work_end_to_end() {
        use axum::{Json, Router, extract::Path, routing::post};
        use serde_json::json;
        use tokio::net::TcpListener;
        use tokio::sync::Mutex;

        let sent = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let captured_group = Arc::clone(&sent);
        let captured_user = Arc::clone(&sent);
        let app = Router::new()
            .route(
                "/app/getAppAccessToken",
                post(|| async { Json(json!({"access_token":"BL_TOKEN","expires_in":7200})) }),
            )
            .route(
                "/v2/groups/{group}/messages",
                post(move |Path(group): Path<String>, Json(body): Json<Value>| {
                    let captured_group = Arc::clone(&captured_group);
                    async move {
                        captured_group
                            .lock()
                            .await
                            .push((format!("group:{group}"), body));
                        Json(json!({"id":"sent-group-1"}))
                    }
                }),
            )
            .route(
                "/v2/users/{user}/messages",
                post(move |Path(user): Path<String>, Json(body): Json<Value>| {
                    let captured_user = Arc::clone(&captured_user);
                    async move {
                        captured_user
                            .lock()
                            .await
                            .push((format!("user:{user}"), body));
                        Json(json!({"id":"sent-user-1"}))
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let temp_dir = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.qqbot.app_id = "bot-app".into();
        config.qqbot.app_secret = "bot-secret".into();
        config.qqbot.api_base = base.clone();
        config.qqbot.super_admin_openid = "ADMIN_OPENID".into();
        config.qqbot.show_waiting_tip = false;
        config.gpt_detect.enabled = true;
        config.output.temp_dir = temp_dir.path().to_string_lossy().into_owned();
        let config = Arc::new(config);
        let state = Arc::new(AppState::new(
            Arc::clone(&config),
            Arc::new(VideoParserService::new((*config).clone()).unwrap()),
            Arc::new(QQBotClient::new(config.qqbot.clone())),
        ));

        // 1. /openid only replies in Private chat, silent in Group
        Arc::clone(&state)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Group,
                chat_id: "GROUP_1".into(),
                message_id: "m1".into(),
                sender_id: "USER_A".into(),
                current_text: "/openid".into(),
                text: "/openid".into(),
                ..QQChatEvent::default()
            })
            .await;
        assert!(sent.lock().await.is_empty());

        Arc::clone(&state)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Private,
                chat_id: "USER_A".into(),
                message_id: "m2".into(),
                sender_id: "USER_A".into(),
                current_text: "/openid".into(),
                text: "/openid".into(),
                ..QQChatEvent::default()
            })
            .await;
        {
            let guard = sent.lock().await;
            assert_eq!(guard.len(), 1);
            assert_eq!(guard[0].0, "user:USER_A");
            assert_eq!(guard[0].1["content"], "你的 OpenID: USER_A");
        }

        // 2. Non-super-admin invoking 拉黑/滚出去 is ignored silently
        for text in ["拉黑<@USER_B> 刷屏", "滚出去<@USER_B>", "拉黑@不存在的人"] {
            Arc::clone(&state)
                .handle_message(QQChatEvent {
                    chat_type: ChatType::Group,
                    chat_id: "GROUP_1".into(),
                    message_id: format!("nonadmin-{text}"),
                    sender_id: "USER_A".into(),
                    current_text: text.into(),
                    text: text.into(),
                    ..QQChatEvent::default()
                })
                .await;
        }
        assert_eq!(sent.lock().await.len(), 1);

        // 3. Super admin cannot ban self
        Arc::clone(&state)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Group,
                chat_id: "GROUP_1".into(),
                message_id: "m3".into(),
                sender_id: "ADMIN_OPENID".into(),
                current_text: "拉黑<@ADMIN_OPENID> 测试".into(),
                text: "拉黑<@ADMIN_OPENID> 测试".into(),
                ..QQChatEvent::default()
            })
            .await;
        assert_eq!(
            sent.lock().await.last().unwrap().1["content"],
            "不能将唯一超级管理员加入黑名单"
        );

        // 4. Super admin bans USER_A without reason -> default reason "你已被权限"
        Arc::clone(&state)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Group,
                chat_id: "GROUP_1".into(),
                message_id: "m4".into(),
                sender_id: "ADMIN_OPENID".into(),
                current_text: "拉黑<@USER_A>".into(),
                text: "拉黑<@USER_A>".into(),
                mentions: vec![crate::types::QQMention {
                    id: "USER_A".into(),
                    member_openid: "USER_A".into(),
                    username: "测试号A".into(),
                    ..crate::types::QQMention::default()
                }],
                ..QQChatEvent::default()
            })
            .await;
        assert_eq!(
            sent.lock().await.last().unwrap().1["content"],
            "已将 测试号A (USER_A) 加入全局黑名单，理由: 你已被权限"
        );

        // 5. Blocked USER_A normal group chatter stays silent (no spam!)
        let count_before_chatter = sent.lock().await.len();
        Arc::clone(&state)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Group,
                chat_id: "GROUP_1".into(),
                message_id: "m5".into(),
                sender_id: "USER_A".into(),
                current_text: "大家好呀今天吃什么".into(),
                text: "大家好呀今天吃什么".into(),
                ..QQChatEvent::default()
            })
            .await;
        assert_eq!(sent.lock().await.len(), count_before_chatter);

        // 6. Blocked USER_A triggers "ai吗" and parsing in another group & private chat -> blocked reply!
        Arc::clone(&state)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Group,
                chat_id: "GROUP_2".into(),
                message_id: "m6".into(),
                sender_id: "USER_A".into(),
                current_text: "ai吗".into(),
                text: "ai吗".into(),
                ..QQChatEvent::default()
            })
            .await;
        assert_eq!(
            sent.lock().await.last().unwrap().1["content"],
            "您已被加入全局黑名单，理由: 你已被权限"
        );

        // Update reason via super admin
        Arc::clone(&state)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Group,
                chat_id: "GROUP_1".into(),
                message_id: "m7".into(),
                sender_id: "ADMIN_OPENID".into(),
                current_text: "拉黑 <@USER_A> 恶意刷链接".into(),
                text: "拉黑 <@USER_A> 恶意刷链接".into(),
                ..QQChatEvent::default()
            })
            .await;

        // Reconstruct AppState from disk to verify persistence across restarts
        let reloaded = Arc::new(AppState::new(
            Arc::clone(&config),
            Arc::new(VideoParserService::new((*config).clone()).unwrap()),
            Arc::new(QQBotClient::new(config.qqbot.clone())),
        ));
        Arc::clone(&reloaded)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Private,
                chat_id: "USER_A".into(),
                message_id: "m8".into(),
                sender_id: "USER_A".into(),
                current_text: "https://www.bilibili.com/video/BV1GJ411x7h7".into(),
                text: "https://www.bilibili.com/video/BV1GJ411x7h7".into(),
                ..QQChatEvent::default()
            })
            .await;
        {
            let guard = sent.lock().await;
            assert_eq!(guard.last().unwrap().0, "user:USER_A");
            assert_eq!(
                guard.last().unwrap().1["content"],
                "您已被加入全局黑名单，理由: 恶意刷链接"
            );
        }

        // 7. Super admin unbans USER_A via 滚出去@xxx -> normal usage restored
        Arc::clone(&reloaded)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Group,
                chat_id: "GROUP_1".into(),
                message_id: "m9".into(),
                sender_id: "ADMIN_OPENID".into(),
                current_text: "滚出去<@USER_A>".into(),
                text: "滚出去<@USER_A>".into(),
                ..QQChatEvent::default()
            })
            .await;
        assert_eq!(
            sent.lock().await.last().unwrap().1["content"],
            "已将 测试号A (USER_A) 移出全局黑名单"
        );

        Arc::clone(&reloaded)
            .handle_message(QQChatEvent {
                chat_type: ChatType::Group,
                chat_id: "GROUP_2".into(),
                message_id: "m10".into(),
                sender_id: "USER_A".into(),
                current_text: "ai吗".into(),
                text: "ai吗".into(),
                ..QQChatEvent::default()
            })
            .await;
        assert_eq!(
            sent.lock().await.last().unwrap().1["content"],
            "请引用一张图片，然后精确回复“ai吗”。"
        );

        server.abort();
    }
}
