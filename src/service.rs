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

use crate::bot::message::{
    MarkdownOptions, build_result_markdown, markdown_payload, media_payload, quoted_text_payload,
    text_payload,
};
use crate::bot::qqbot::QQBotClient;
use crate::parser::api::VideoParserService;
use crate::parser::cardsearch::resolve_card_by_title;
use crate::parser::formatter::generate_formatted_text;
use crate::parser::gpt_detect::{
    GPT_DETECT_BUSY_TEXT, GPT_DETECT_WAITING_TEXT, detect_openai_image, format_gpt_detect_error,
    format_gpt_detect_reply, matches_gpt_detect_keyword,
};
use crate::parser::jmcomic::{cleanup_jm_zip, extract_jm_id};
use crate::parser::matcher::link_type_parser;
use crate::parser::media::{
    download_media_buffer_with_headers, head_content_length, is_foreign_url,
};
use crate::parser::remux::download_parsed_temp;
use crate::types::{AppConfig, ChatType, LinkMatch, ParsedData, QQChatEvent};

static ARCHIVE_EXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.(7z|zip)$").unwrap());
static JM_TASK_GATE: Semaphore = Semaphore::const_new(1);

pub struct AppState {
    config: Arc<AppConfig>,
    parser: Arc<VideoParserService>,
    bot: Arc<QQBotClient>,
    dedup_cache: Arc<std::sync::Mutex<HashMap<String, DedupEntry>>>,
    gpt_detect_busy: AtomicBool,
    local_proxies: Vec<String>,
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
        Self {
            config,
            parser,
            bot,
            dedup_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            gpt_detect_busy: AtomicBool::new(false),
            local_proxies,
        }
    }

    pub async fn handle_message(self: Arc<Self>, event: QQChatEvent) {
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
        let mut card_resolved = false;

        if matches.is_empty() && event.card_title.is_some() && self.config.qqbot.parse_miniapp_cards
        {
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
            match self.parser.parse_url(&item.url, &item.media_type).await {
                crate::parser::api::ParseResult::Success(data) => {
                    let text = generate_formatted_text(&data, &self.config.output.format);
                    parsed_results.push((item, data, text, claim));
                }
                crate::parser::api::ParseResult::Failure(message) => {
                    warn!("解析失败 [{}]: {message}", item.url);
                    failed_errors.push(render_parse_error(&self.config, &item.url, &message));
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

        for (_, parsed, text, mut claim) in parsed_results {
            if let Some(attachment) = parsed.attachment.clone() {
                if self.send_attachment(&event, &attachment).await {
                    claim.commit();
                }
                continue;
            }
            if self.send_media_result(&event, parsed, text).await {
                claim.commit();
            }
        }
        info!("所有内容处理完成");
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
            warn!("发送消息失败 (已忽略): {err}");
            return false;
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

    async fn localize_foreign_image(
        &self,
        event: &QQChatEvent,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Option<(String, String)> {
        if !is_foreign_url(url) {
            return None;
        }
        match download_media_buffer_with_headers(
            url,
            &self.local_proxies,
            20 * 1024 * 1024,
            headers,
        )
        .await
        {
            Ok(buffer) => {
                let file_name = format!(
                    "img_{}_{}.jpg",
                    chrono::Utc::now().timestamp_millis(),
                    random_suffix(4)
                );
                match self.upload_media_data(event, 1, &file_name, &buffer).await {
                    Ok(data) => {
                        let raw_url = data
                            .get("raw_url")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let file_info = data
                            .get("file_info")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if !raw_url.is_empty() && !file_info.is_empty() {
                            info!(
                                "图片已本地上传中转 ({}KB): {}",
                                buffer.len() / 1024,
                                url.chars().take(70).collect::<String>()
                            );
                            return Some((raw_url, file_info));
                        }
                        warn!("分片上传响应缺少 raw_url 或 file_info");
                    }
                    Err(err) => warn!("图片中转失败: {err}"),
                }
            }
            Err(err) => warn!(
                "图片中转失败: {err} [{}]",
                url.chars().take(70).collect::<String>()
            ),
        }
        None
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

    async fn upload_media_data(
        &self,
        event: &QQChatEvent,
        file_type: u32,
        file_name: &str,
        data: &[u8],
    ) -> anyhow::Result<Value> {
        Ok(match event.chat_type {
            ChatType::Group => {
                self.bot
                    .upload_group_media_data(&event.chat_id, file_type, file_name, data)
                    .await?
            }
            ChatType::Private => {
                self.bot
                    .upload_user_media_data(&event.chat_id, file_type, file_name, data)
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

    async fn send_media_result(
        &self,
        event: &QQChatEvent,
        parsed: ParsedData,
        text: String,
    ) -> bool {
        let is_video = !parsed.video.is_empty()
            && (parsed.media_type == "video"
                || (parsed.media_type == "live"
                    && parsed.live_photo.is_empty()
                    && parsed.images.is_empty()));
        let max_images = if self.config.output.max_markdown_images == 0 {
            9
        } else {
            self.config.output.max_markdown_images
        };

        let mut video_file_info = String::new();
        let mut video_failed_note = String::new();
        if is_video && self.config.output.show_video_file {
            let relay_video = || async {
                let temporary = download_parsed_temp(&parsed, &self.config).await?;
                let file_name = format!("video_{}.mp4", chrono::Utc::now().timestamp_millis());
                let upload = self
                    .upload_media_file(event, 2, &file_name, temporary.path())
                    .await?;
                let file_info = upload
                    .get("file_info")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("分片上传响应缺少 file_info"))?
                    .to_string();
                info!("视频已本地上传中转 ({}MB)", temporary.size() / 1024 / 1024);
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
                        video_failed_note =
                            "⚠️ 视频发送失败（下载/上传中转失败，请稍后再试）".into();
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
                            video_failed_note =
                                "⚠️ 视频发送失败（本地上传中转失败，请稍后再试）".into();
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
                                            video_failed_note =
                                                "⚠️ 视频发送失败（平台转存与本地上传均失败，请稍后再试）"
                                                    .into();
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let is_gallery = parsed.media_type == "image"
            || parsed.media_type == "live_photo"
            || (parsed.media_type == "live"
                && (!parsed.live_photo.is_empty() || !parsed.images.is_empty()));
        let image_pool: Vec<String> = if is_gallery {
            if !parsed.images.is_empty() {
                parsed.images.iter().take(max_images).cloned().collect()
            } else {
                parsed
                    .live_photo
                    .iter()
                    .take(max_images)
                    .map(|photo| photo.image.clone())
                    .collect()
            }
        } else {
            Vec::new()
        };

        let mut image_infos = Vec::new();
        let mut image_fail_count = 0usize;
        for image in &image_pool {
            match self
                .resolve_image_file_info(event, image, &parsed.media_headers)
                .await
            {
                Ok(file_info) => image_infos.push(file_info),
                Err(err) => {
                    warn!("图片发送失败: {err}");
                    image_fail_count += 1;
                }
            }
        }

        let transfer_succeeded = video_failed_note.is_empty() && image_fail_count == 0;
        let mut delivered = true;
        let mut notes = Vec::new();
        if !video_failed_note.is_empty() {
            notes.push(video_failed_note);
        }
        if image_fail_count > 0 {
            notes.push(format!("⚠️ {image_fail_count} 张图片获取失败"));
        }
        let mut markdown_data = parsed.clone();
        markdown_data.cover.clear();
        markdown_data.images.clear();
        markdown_data.live_photo.clear();
        let mut markdown = build_result_markdown(
            &markdown_data,
            &text,
            &MarkdownOptions {
                show_image_text: self.config.output.show_image_text,
                show_cover: false,
                max_images: 0,
            },
        );
        if !notes.is_empty() {
            markdown.push('\n');
            markdown.push_str(&notes.join("\n"));
        }
        if !markdown.trim().is_empty() {
            delivered &= self
                .send_message(event, markdown_payload(markdown, false))
                .await;
        }

        for file_info in image_infos {
            delivered &= self.send_message(event, media_payload(file_info)).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        if !video_file_info.is_empty() {
            delivered &= self
                .send_message(event, media_payload(video_file_info))
                .await;
        }
        transfer_succeeded && delivered
    }

    async fn resolve_image_file_info(
        &self,
        event: &QQChatEvent,
        image: &str,
        headers: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        if is_foreign_url(image) {
            let (_, file_info) = self
                .localize_foreign_image(event, image, headers)
                .await
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "境外图片中转失败: {}",
                        image.chars().take(60).collect::<String>()
                    )
                })?;
            return Ok(file_info);
        }
        match self
            .upload_media_url(event, 1, image)
            .await
            .and_then(file_info_from_upload)
        {
            Ok(file_info) => Ok(file_info),
            Err(err) => {
                warn!("图片平台转存失败，尝试本地上传中转: {err}");
                let buffer = download_media_buffer_with_headers(
                    image,
                    &self.local_proxies,
                    20 * 1024 * 1024,
                    headers,
                )
                .await?;
                let file_name = format!(
                    "img_{}_{}.jpg",
                    chrono::Utc::now().timestamp_millis(),
                    random_suffix(4)
                );
                let upload = self
                    .upload_media_data(event, 1, &file_name, &buffer)
                    .await?;
                let file_info = file_info_from_upload(upload)?;
                info!("图片已本地上传中转 ({}KB)", buffer.len() / 1024);
                Ok(file_info)
            }
        }
    }
}

fn file_info_from_upload(upload: Value) -> anyhow::Result<String> {
    upload
        .get("file_info")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("上传响应缺少 file_info"))
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
}
