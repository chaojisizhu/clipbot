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

static ARCHIVE_EXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.(7z|zip)$").unwrap());
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
        Self {
            config,
            parser,
            bot,
            dedup_cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
            gpt_detect_busy: AtomicBool::new(false),
            local_proxies,
            gallery_material_gate: tokio::sync::Mutex::new(()),
            owned_materials: std::sync::Mutex::new(owned_materials),
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
        for (item, parsed, text, mut claim) in parsed_results {
            if let Some(attachment) = parsed.attachment.clone() {
                if self.send_attachment(&event, &attachment).await {
                    claim.commit();
                    delivered_count += 1;
                }
                continue;
            }
            let delivered = if matches!(item.media_type.as_str(), "douyin" | "xiaohongshu")
                && parsed.media_type == "live_photo"
                && self.config.output.show_video_file
            {
                self.send_live_album_result(&event, &parsed, &text).await
            } else {
                self.send_media_result(&event, parsed, text).await
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
            warn!("QQ 消息发送失败: {err}");
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
            return self.send_gallery_result(event, parsed, text).await;
        }
        let started = Instant::now();
        let Ok(permit) = LIVE_ALBUM_GATE.try_acquire() else {
            self.send_message(
                event,
                text_payload("另一个实况图集正在合并，先发原图。想看视频可以稍后重试。"),
            )
            .await;
            self.send_gallery_result(event, parsed, text).await;
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
                return self.send_gallery_result(event, parsed, text).await;
            }
        };
        match self.bot.reply(event, &media_payload(file_info)).await {
            Ok(_) => {}
            Err(QQBotError::Api(error)) => {
                warn!("QQ 拒绝发送合并后的实况视频: {error}");
                self.send_message(event, text_payload("QQ 拒绝发送实况视频，下面是原图集。"))
                    .await;
                return self.send_gallery_result(event, parsed, text).await;
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
    ) -> bool {
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
            return self.send_gallery_result(event, &parsed, &text).await;
        }

        let mut video_file_info = String::new();
        let mut video_failed_note = String::new();
        if is_video && self.config.output.show_video_file {
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
        let mut delivered = true;
        let mut markdown = build_result_markdown(
            &ParsedData::empty("video"),
            &text,
            &MarkdownOptions {
                show_image_text: self.config.output.show_image_text,
                show_cover: false,
                max_images: 0,
            },
        );
        if !video_failed_note.is_empty() {
            markdown.push('\n');
            markdown.push_str(&video_failed_note);
        }
        if !markdown.trim().is_empty() {
            delivered &= self
                .send_message(event, markdown_payload(markdown, false))
                .await;
        }
        if !video_file_info.is_empty() {
            delivered &= self
                .send_message(event, media_payload(video_file_info))
                .await;
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
    ) -> bool {
        // The account may deduplicate material IDs. Keep ownership, publication and
        // deletion in one bounded gallery turn so another reply cannot lose its material.
        let _gate = self.gallery_material_gate.lock().await;
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
        let markdown = self.gallery_markdown(text, &images, failed_count, omitted_count);
        match self
            .bot
            .reply(event, &markdown_payload(markdown, !images.is_empty()))
            .await
        {
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
                failed_count == 0 && !images.is_empty()
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
        let mut gallery = ParsedData::empty("image");
        gallery.images = images.iter().map(|image| image.url.clone()).collect();
        gallery.image_size_map = Some(
            images
                .iter()
                .map(|image| (image.url.clone(), image.size.clone()))
                .collect(),
        );
        let mut markdown = build_result_markdown(
            &gallery,
            text,
            &MarkdownOptions {
                show_image_text: self.config.output.show_image_text,
                show_cover: false,
                max_images: images.len(),
            },
        );
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
    if base.contains("缺少BV、av、ep或ss标识") {
        return "该链接不是可解析的 B站视频地址".into();
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
}
