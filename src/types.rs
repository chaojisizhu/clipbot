use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct QQBotConfig {
    pub app_id: String,
    pub app_secret: String,
    pub api_base: String,
    pub intents: u64,
    pub receive_all_messages: bool,
    pub parse_miniapp_cards: bool,
    pub auto_parse: bool,
    pub command_prefix: Vec<String>,
    pub show_waiting_tip: bool,
    pub show_parse_error: bool,
    pub debug: bool,
}

impl Default for QQBotConfig {
    fn default() -> Self {
        Self {
            app_id: String::new(),
            app_secret: String::new(),
            api_base: "https://api.bot.qq.com".into(),
            intents: 1 << 25,
            receive_all_messages: true,
            parse_miniapp_cards: true,
            auto_parse: true,
            command_prefix: vec!["/parse".into(), "解析".into(), "video".into()],
            show_waiting_tip: true,
            show_parse_error: true,
            debug: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiTextConfig {
    pub waiting_tip_text: String,
    pub unsupported_platform_text: String,
    pub invalid_link_text: String,
    pub parse_error_prefix: String,
    pub parse_error_item_format: String,
}

impl Default for UiTextConfig {
    fn default() -> Self {
        Self {
            waiting_tip_text: "正在解析视频，请稍候...".into(),
            unsupported_platform_text: "不支持该平台链接".into(),
            invalid_link_text: "无效的视频链接".into(),
            parse_error_prefix: "❌ 解析失败：".into(),
            parse_error_item_format: "【${url}】: ${msg}".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct IsteroConfig {
    pub token: String,
    pub app_secret: String,
    pub enable_sign: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CustomApiConfig {
    pub platform: String,
    pub provider: Option<String>,
    pub api_url: String,
    pub token: Option<String>,
    pub app_secret: Option<String>,
    pub enable_sign: Option<bool>,
}

impl Default for CustomApiConfig {
    fn default() -> Self {
        Self {
            platform: String::new(),
            provider: None,
            api_url: String::new(),
            token: None,
            app_secret: None,
            enable_sign: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ApiConfig {
    pub primary_url: String,
    pub backup_url: String,
    pub timeout: u64,
    pub retry_times: u32,
    pub retry_interval: u64,
    pub proxy: String,
    pub max_concurrency: u32,
    pub custom_apis: Vec<CustomApiConfig>,
    pub platform_dedicated_first: HashMap<String, bool>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            primary_url: "https://api.bugpk.com/api/short_videos".into(),
            backup_url: "https://api.bugpk.com/api/svparse".into(),
            timeout: 30_000,
            retry_times: 3,
            retry_interval: 1_000,
            proxy: std::env::var("http_proxy")
                .or_else(|_| std::env::var("HTTP_PROXY"))
                .unwrap_or_default(),
            max_concurrency: 3,
            custom_apis: Vec::new(),
            platform_dedicated_first: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OutputConfig {
    pub show_image_text: bool,
    pub show_cover: bool,
    pub show_video_file: bool,
    pub max_desc_length: usize,
    pub video_download_timeout: u64,
    pub video_send_timeout: u64,
    pub temp_dir: String,
    pub max_video_size: u64,
    pub deduplication_interval: u64,
    pub max_markdown_images: usize,
    pub format: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            show_image_text: true,
            show_cover: true,
            show_video_file: true,
            max_desc_length: 200,
            video_download_timeout: 120_000,
            video_send_timeout: 60_000,
            temp_dir: "./temp_videos".into(),
            max_video_size: 0,
            deduplication_interval: 180,
            max_markdown_images: 9,
            format: concat!(
                "标题：${标题}\n作者：${作者}\n简介：${简介}\n时长：${视频时长}\n",
                "点赞：${点赞数}\n收藏：${收藏数}\n转发：${转发数}\n播放：${播放数}\n",
                "评论：${评论数}\n图片数量：${图片数量}"
            )
            .into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct YtdlpConfig {
    pub enabled: bool,
    pub command: String,
    pub proxy: String,
    pub timeout: u64,
}

impl Default for YtdlpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            command: "yt-dlp".into(),
            proxy: std::env::var("http_proxy")
                .or_else(|_| std::env::var("HTTP_PROXY"))
                .unwrap_or_default(),
            timeout: 90_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ChannelsConfig {
    pub enabled: bool,
    pub yuanbao_cookie: String,
    #[serde(skip)]
    pub api_base: Option<String>,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            yuanbao_cookie: String::new(),
            api_base: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct JmConfig {
    pub enabled: bool,
    pub python: String,
    pub password: String,
    pub timeout: u64,
    pub temp_dir: String,
    pub max_pages: usize,
    pub proxy: String,
    pub threads: u32,
}

impl Default for JmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            python: "python".into(),
            password: String::new(),
            timeout: 300_000,
            temp_dir: "./temp_jm".into(),
            max_pages: 0,
            proxy: String::new(),
            threads: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GptDetectConfig {
    pub enabled: bool,
    pub keywords: Vec<String>,
    pub api_key: String,
    pub api_keys: Vec<String>,
    pub base_url: String,
    pub proxy: String,
    pub timeout: u64,
    pub max_image_size_mb: u64,
}

impl Default for GptDetectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keywords: vec!["ai吗".into()],
            api_key: std::env::var("OPENAI_API_KEY").unwrap_or_default(),
            api_keys: Vec::new(),
            base_url: "https://api.openai.com/v1".into(),
            proxy: String::new(),
            timeout: 60_000,
            max_image_size_mb: 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AppConfig {
    pub qqbot: QQBotConfig,
    pub ui_text: UiTextConfig,
    pub istero: IsteroConfig,
    pub platform_enable: HashMap<String, bool>,
    pub api: ApiConfig,
    pub output: OutputConfig,
    pub ytdlp: YtdlpConfig,
    pub channels: ChannelsConfig,
    pub jmcomic: JmConfig,
    pub gpt_detect: GptDetectConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        let mut platform_enable = HashMap::new();
        for name in [
            "bilibili",
            "douyin",
            "kuaishou",
            "xiaohongshu",
            "weibo",
            "toutiao",
            "xigua",
            "pipixia",
            "pipigx",
            "zuiyou",
            "jimeng",
            "acfun",
            "zhihu",
            "weishi",
            "huya",
            "haokan",
            "meipai",
            "youtube",
            "tiktok",
            "twitter",
            "instagram",
            "doubao",
            "channels",
            "jmcomic",
        ] {
            platform_enable.insert(name.into(), true);
        }
        Self {
            qqbot: QQBotConfig::default(),
            ui_text: UiTextConfig::default(),
            istero: IsteroConfig::default(),
            platform_enable,
            api: ApiConfig::default(),
            output: OutputConfig::default(),
            ytdlp: YtdlpConfig::default(),
            channels: ChannelsConfig::default(),
            jmcomic: JmConfig::default(),
            gpt_detect: GptDetectConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn platform_enabled(&self, platform: &str) -> bool {
        self.platform_enable.get(platform).copied().unwrap_or(true)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VideoQuality {
    pub quality: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LivePhoto {
    pub image: String,
    pub video: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MusicInfo {
    pub title: String,
    pub author: String,
    pub cover: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attachment {
    pub path: String,
    pub password: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParsedData {
    #[serde(rename = "type")]
    pub media_type: String,
    pub title: String,
    pub desc: String,
    pub author: String,
    pub uid: String,
    pub avatar: String,
    pub cover: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_size: Option<ImageSize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_size_map: Option<HashMap<String, ImageSize>>,
    pub video: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_decode_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<Attachment>,
    pub videos: Vec<VideoQuality>,
    pub images: Vec<String>,
    pub live_photo: Vec<LivePhoto>,
    pub music: MusicInfo,
    pub like: i64,
    pub comment: i64,
    pub collect: i64,
    pub share: i64,
    pub play: i64,
    pub duration: u64,
    pub publish_time: i64,
}

impl ParsedData {
    pub fn empty(media_type: impl Into<String>) -> Self {
        Self {
            media_type: media_type.into(),
            ..Self::default()
        }
    }

    pub fn has_media(&self) -> bool {
        !self.video.is_empty() || !self.images.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LinkMatch {
    #[serde(rename = "type")]
    pub media_type: String,
    pub url: String,
    pub id: String,
}

#[derive(Debug, Clone, Default)]
pub struct ApiTarget {
    pub url: String,
    pub label: String,
    pub provider: String,
    pub token: Option<String>,
    pub app_secret: Option<String>,
    pub enable_sign: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QQImageAttachment {
    pub url: String,
    #[serde(rename = "contentType")]
    pub content_type: String,
    pub filename: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatType {
    Group,
    Private,
}

#[derive(Debug, Clone, Default)]
pub struct QQChatEvent {
    pub chat_type: ChatType,
    pub chat_id: String,
    pub message_id: String,
    pub sender_name: String,
    pub current_text: String,
    pub text: String,
    pub card_title: Option<String>,
    pub card_source: Option<String>,
    pub quoted_message_id: Option<String>,
    pub quoted_text: Option<String>,
    pub quoted_images: Vec<QQImageAttachment>,
}

impl Default for ChatType {
    fn default() -> Self {
        Self::Group
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct QQApiError {
    pub err_code: i64,
    pub message: String,
    pub trace_id: Option<String>,
}

impl QQApiError {
    pub fn new(err_code: i64, message: impl Into<String>, trace_id: Option<String>) -> Self {
        let message = message.into();
        Self {
            err_code,
            message: if message.is_empty() {
                format!("QQ API 错误码 {err_code}")
            } else {
                message
            },
            trace_id,
        }
    }
}

#[derive(Debug, Clone)]
pub enum BotEvent {
    Ready(serde_json::Value),
    Resumed(serde_json::Value),
    Message(QQChatEvent),
}
