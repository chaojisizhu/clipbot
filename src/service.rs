use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::bot::message::{
    MarkdownOptions, build_result_markdown, markdown_payload, media_payload, quoted_text_payload,
    text_payload,
};
use crate::bot::qqbot::QQBotClient;
use crate::parser::api::{
    VideoParserService, decrypt_video_if_needed, download_media_buffer, head_content_length,
    is_foreign_url,
};
use crate::parser::cardsearch::resolve_card_by_title;
use crate::parser::formatter::generate_formatted_text;
use crate::parser::gpt_detect::{
    GPT_DETECT_BUSY_TEXT, GPT_DETECT_WAITING_TEXT, detect_openai_image, format_gpt_detect_error,
    format_gpt_detect_reply, matches_gpt_detect_keyword,
};
use crate::parser::jmcomic::{cleanup_jm_zip, extract_jm_id};
use crate::parser::matcher::link_type_parser;
use crate::types::{AppConfig, ChatType, LinkMatch, ParsedData, QQChatEvent};

static ARCHIVE_EXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.(7z|zip)$").unwrap());

pub struct AppState {
    config: Arc<AppConfig>,
    parser: Arc<VideoParserService>,
    bot: Arc<QQBotClient>,
    dedup_cache: Mutex<HashMap<String, Instant>>,
    gpt_detect_busy: AtomicBool,
    local_proxies: Vec<String>,
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
            dedup_cache: Mutex::new(HashMap::new()),
            gpt_detect_busy: AtomicBool::new(false),
            local_proxies,
        }
    }

    pub async fn handle_message(self: Arc<Self>, event: QQChatEvent) {
        if self.handle_gpt_detect(&event).await {
            return;
        }

        let raw_text = event.text.clone();
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
        for item in matches {
            if self.is_deduplicated(&item.url).await {
                info!("链接处于去重冷却期内，跳过: {}", item.url);
            } else {
                pending.push(item);
            }
        }
        if pending.is_empty() {
            return;
        }

        let chat_label = match event.chat_type {
            ChatType::Group => format!("群 {}", event.chat_id),
            ChatType::Private => format!("私聊 {}", event.chat_id),
        };
        info!(
            "[{chat_label}] 检测到 {} 个待解析链接: {}",
            pending.len(),
            pending
                .iter()
                .map(|item| item.url.as_str())
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

        let mut parsed_results: Vec<(LinkMatch, ParsedData, String)> = Vec::new();
        let mut failed_errors = Vec::new();
        for item in pending {
            match self.parser.parse_url(&item.url, &item.media_type).await {
                crate::parser::api::ParseResult::Success(data) => {
                    let text = generate_formatted_text(&data, &self.config.output.format);
                    parsed_results.push((item, data, text));
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

        for (_, parsed, text) in parsed_results {
            if let Some(attachment) = parsed.attachment.clone() {
                self.send_attachment(&event, &attachment).await;
                continue;
            }
            self.send_media_result(&event, parsed, text).await;
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
        self.gpt_detect_busy.store(false, Ordering::SeqCst);
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

    async fn send_message(&self, event: &QQChatEvent, payload: Value) {
        if let Err(err) = self.bot.reply(event, &payload).await {
            warn!("发送消息失败 (已忽略): {err}");
        }
    }

    async fn is_deduplicated(&self, url: &str) -> bool {
        let interval = self.config.output.deduplication_interval;
        if interval == 0 {
            return false;
        }
        let now = Instant::now();
        let mut cache = self.dedup_cache.lock().await;
        if cache.get(url).is_some_and(|expire| *expire > now) {
            return true;
        }
        cache.insert(url.to_string(), now + Duration::from_secs(interval));
        false
    }

    async fn localize_foreign_image(
        &self,
        event: &QQChatEvent,
        url: &str,
    ) -> Option<(String, String)> {
        if !is_foreign_url(url) {
            return None;
        }
        match download_media_buffer(url, &self.local_proxies, 20 * 1024 * 1024).await {
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

    async fn send_attachment(&self, event: &QQChatEvent, attachment: &crate::types::Attachment) {
        let result = async {
            let extension = ARCHIVE_EXT_RE
                .find(attachment.file_name.as_deref().unwrap_or_default())
                .map(|found| found.as_str().to_ascii_lowercase())
                .unwrap_or_else(|| ".7z".into());
            let named_file = format!("密码{}{}", attachment.password, extension);
            let buffer = tokio::fs::read(&attachment.path).await?;
            let upload = self
                .upload_media_data(event, 4, &named_file, &buffer)
                .await?;
            let file_info = upload
                .get("file_info")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("文件上传响应缺少 file_info"))?;
            self.send_message(event, media_payload(file_info)).await;
            info!("加密压缩包已发送: {named_file}");
            Ok::<(), anyhow::Error>(())
        }
        .await;
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
        cleanup_jm_zip(&attachment.path);
    }

    async fn send_media_result(&self, event: &QQChatEvent, parsed: ParsedData, text: String) {
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
                let max_bytes = if self.config.output.max_video_size > 0 {
                    self.config.output.max_video_size * 1024 * 1024
                } else {
                    190 * 1024 * 1024
                };
                let mut buffer =
                    download_media_buffer(&parsed.video, &self.local_proxies, max_bytes).await?;
                if parsed.video_decode_key.is_some() {
                    buffer = decrypt_video_if_needed(buffer, parsed.video_decode_key.as_deref())?;
                    info!("视频号加密流已解密 ({}MB)", buffer.len() / 1024 / 1024);
                }
                let file_name = format!("video_{}.mp4", chrono::Utc::now().timestamp_millis());
                let upload = self
                    .upload_media_data(event, 2, &file_name, &buffer)
                    .await?;
                let file_info = upload
                    .get("file_info")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("分片上传响应缺少 file_info"))?
                    .to_string();
                info!("视频已本地上传中转 ({}MB)", buffer.len() / 1024 / 1024);
                Ok::<String, anyhow::Error>(file_info)
            };

            if parsed.video_decode_key.is_some() || is_foreign_url(&parsed.video) {
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
            match self.resolve_image_file_info(event, image).await {
                Ok(file_info) => image_infos.push(file_info),
                Err(err) => {
                    warn!("图片发送失败: {err}");
                    image_fail_count += 1;
                }
            }
        }

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
            self.send_message(event, markdown_payload(markdown, false))
                .await;
        }

        for file_info in image_infos {
            self.send_message(event, media_payload(file_info)).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        if !video_file_info.is_empty() {
            self.send_message(event, media_payload(video_file_info))
                .await;
        }
    }

    async fn resolve_image_file_info(
        &self,
        event: &QQChatEvent,
        image: &str,
    ) -> anyhow::Result<String> {
        if is_foreign_url(image) {
            let (_, file_info) =
                self.localize_foreign_image(event, image)
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
                let buffer =
                    download_media_buffer(image, &self.local_proxies, 20 * 1024 * 1024).await?;
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
