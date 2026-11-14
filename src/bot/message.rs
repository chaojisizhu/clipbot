use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Value, json};

use crate::types::{ImageSize, ParsedData, QQImageAttachment};

static URL_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"https?://[^\s"'<>\\]+"#).unwrap());
static IMAGE_EXT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\.(png|jpe?g|webp|gif)(?:[?#]|$)").unwrap());

pub fn text_payload(content: impl Into<String>) -> Value {
    json!({ "msg_type": 0, "content": content.into() })
}

pub fn quoted_text_payload(content: impl Into<String>, message_id: &str) -> Value {
    json!({
        "msg_type": 0,
        "content": content.into(),
        "message_reference": {
            "message_id": message_id,
            "ignore_get_message_error": true
        }
    })
}

pub fn markdown_payload(content: impl Into<String>, verify_images: bool) -> Value {
    let mut markdown = json!({ "content": content.into() });
    if verify_images {
        markdown["force_verify_image_resource"] = Value::Bool(true);
    }
    json!({ "msg_type": 2, "markdown": markdown })
}

pub fn media_payload(file_info: impl Into<String>) -> Value {
    json!({ "msg_type": 7, "media": { "file_info": file_info.into() } })
}

pub fn markdown_image_tag(url: &str, size: Option<&ImageSize>) -> String {
    let width = size.map_or(750, |size| size.width.max(1));
    let height = size.map_or(1000, |size| size.height.max(1));
    format!("![img#{width}px #{height}px]({url})")
}

pub struct MarkdownOptions {
    pub show_image_text: bool,
    pub show_cover: bool,
    pub max_images: usize,
}

pub fn build_result_markdown(data: &ParsedData, text: &str, options: &MarkdownOptions) -> String {
    let max_images = if options.max_images == 0 {
        9
    } else {
        options.max_images
    };
    let is_gallery = data.media_type == "image"
        || data.media_type == "live_photo"
        || (data.media_type == "live" && (!data.live_photo.is_empty() || !data.images.is_empty()));
    let mut parts = Vec::new();
    let mut image_count = 0usize;

    if !text.is_empty() && options.show_image_text {
        parts.push(text.to_string());
    }

    if options.show_cover && !data.cover.is_empty() && !is_gallery && image_count < max_images {
        parts.push(markdown_image_tag(&data.cover, data.cover_size.as_ref()));
        image_count += 1;
    }

    if is_gallery {
        let urls: Vec<&String> = if !data.images.is_empty() {
            data.images.iter().collect()
        } else {
            data.live_photo.iter().map(|photo| &photo.image).collect()
        };
        for url in urls {
            if image_count >= max_images {
                parts.push(format!(
                    "(其余 {} 张图片未显示)",
                    data.images.len().max(data.live_photo.len()) - image_count
                ));
                break;
            }
            let size = data
                .image_size_map
                .as_ref()
                .and_then(|sizes| sizes.get(url));
            parts.push(markdown_image_tag(url, size));
            image_count += 1;
        }
    }

    parts.join("\n")
}

fn normalize_slashes(value: &str) -> String {
    value.replace(r"\/", "/")
}

fn parse_message_scene_ext(data: &Value) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let Some(items) = data
        .get("message_scene")
        .and_then(|scene| scene.get("ext"))
        .and_then(Value::as_array)
    else {
        return result;
    };
    for item in items {
        let Some(item) = item.as_str() else {
            continue;
        };
        if let Some((key, value)) = item.split_once('=')
            && !key.is_empty()
        {
            result.insert(key.to_string(), value.to_string());
        }
    }
    result
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn is_image_attachment(attachment: &Value) -> bool {
    let Some(url) = string_field(attachment, "url") else {
        return false;
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return false;
    }
    let content_type = string_field(attachment, "content_type")
        .or_else(|| string_field(attachment, "mime_type"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    content_type.starts_with("image/") || IMAGE_EXT_RE.is_match(url)
}

fn collect_quoted_images(elements: &[Value], images: &mut Vec<QQImageAttachment>) {
    for element in elements {
        if let Some(attachments) = element.get("attachments").and_then(Value::as_array) {
            for attachment in attachments {
                if !is_image_attachment(attachment) {
                    continue;
                }
                images.push(QQImageAttachment {
                    url: normalize_slashes(string_field(attachment, "url").unwrap_or_default()),
                    content_type: string_field(attachment, "content_type")
                        .or_else(|| string_field(attachment, "mime_type"))
                        .unwrap_or_default()
                        .to_string(),
                    filename: string_field(attachment, "filename").map(ToString::to_string),
                    width: attachment
                        .get("width")
                        .and_then(Value::as_u64)
                        .map(|value| value as u32),
                    height: attachment
                        .get("height")
                        .and_then(Value::as_u64)
                        .map(|value| value as u32),
                    size: attachment.get("size").and_then(Value::as_u64),
                });
            }
        }
        if let Some(children) = element.get("msg_elements").and_then(Value::as_array) {
            collect_quoted_images(children, images);
        }
    }
}

#[derive(Debug, Default)]
pub struct QuotedMessageContent {
    pub message_id: Option<String>,
    pub text: String,
    pub images: Vec<QQImageAttachment>,
}

pub fn extract_quoted_content_from_qq_event(data: &Value) -> QuotedMessageContent {
    let scene = parse_message_scene_ext(data);
    let empty = Vec::new();
    let elements = data
        .get("msg_elements")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let mut text_parts = Vec::new();
    let mut images = Vec::new();

    fn walk(items: &[Value], text_parts: &mut Vec<String>) {
        for item in items {
            if let Some(content) = string_field(item, "content") {
                text_parts.push(normalize_slashes(content));
            }
            if let Some(children) = item.get("msg_elements").and_then(Value::as_array) {
                walk(children, text_parts);
            }
        }
    }
    walk(elements, &mut text_parts);
    collect_quoted_images(elements, &mut images);

    let mut latest: Vec<QQImageAttachment> = Vec::new();
    for image in images {
        if let Some(existing) = latest.iter_mut().find(|existing| existing.url == image.url) {
            *existing = image;
        } else {
            latest.push(image);
        }
    }
    images = latest;
    let target_id = scene
        .get("ref_msg_idx")
        .cloned()
        .or_else(|| {
            elements
                .iter()
                .find_map(|element| string_field(element, "msg_idx").map(ToString::to_string))
        })
        .filter(|id| !id.is_empty());

    QuotedMessageContent {
        message_id: target_id,
        text: text_parts.join(" ").trim().to_string(),
        images,
    }
}

pub fn extract_current_text_from_qq_event(data: &Value) -> String {
    string_field(data, "content")
        .map(normalize_slashes)
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub fn strip_leading_self_mentions<'a>(text: &'a str, self_ids: &[&str]) -> (&'a str, bool) {
    let mut remaining = text;
    let mut removed = false;
    while let Some(mention) = remaining.trim_start().strip_prefix("<@") {
        let mention = mention.strip_prefix('!').unwrap_or(mention);
        let Some((id, following)) = mention.split_once('>') else {
            break;
        };
        if !self_ids
            .iter()
            .any(|expected| !expected.is_empty() && id == *expected)
        {
            break;
        }
        remaining = following.trim_start();
        removed = true;
    }
    (remaining, removed)
}

fn collect_urls_from_fields(fields: &Value) -> Vec<String> {
    let Some(object) = fields.as_object() else {
        return Vec::new();
    };
    object
        .values()
        .filter_map(Value::as_str)
        .flat_map(|value| {
            URL_RE
                .find_iter(&normalize_slashes(value))
                .map(|found| found.as_str().to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn extract_text_from_qq_event(data: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(content) = string_field(data, "content") {
        parts.push(normalize_slashes(content));
    }
    if let Some(prompt) = data
        .get("ark_data")
        .and_then(|ark| ark.get("prompt"))
        .and_then(Value::as_str)
    {
        parts.push(normalize_slashes(prompt));
    }
    if let Some(fields) = data.get("ark_data").and_then(|ark| ark.get("fields")) {
        parts.extend(collect_urls_from_fields(fields));
    }

    fn walk(items: &[Value], parts: &mut Vec<String>) {
        for item in items {
            if let Some(content) = string_field(item, "content") {
                parts.push(normalize_slashes(content));
            }
            if let Some(fields) = item.get("ark_data").and_then(|ark| ark.get("fields")) {
                parts.extend(collect_urls_from_fields(fields));
            }
            if let Some(children) = item.get("msg_elements").and_then(Value::as_array) {
                walk(children, parts);
            }
        }
    }
    if let Some(elements) = data.get("msg_elements").and_then(Value::as_array) {
        walk(elements, &mut parts);
    }
    parts.join(" ")
}

pub fn extract_card_fields(data: &Value) -> (Option<String>, Option<String>) {
    let fields = data.get("ark_data").and_then(|ark| ark.get("fields"));
    let title = fields
        .and_then(|fields| fields.get("title"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let source = fields
        .and_then(|fields| fields.get("source"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    (title, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_leading_mentions_of_this_robot() {
        let own_ids = ["654321", "123456"];
        for text in [
            "<@654321> 解析 链接",
            "  <@!654321>\n\t解析 链接",
            "<@123456>解析 链接",
            "<@!123456><@654321> 解析 链接",
        ] {
            assert_eq!(
                strip_leading_self_mentions(text, &own_ids),
                ("解析 链接", true)
            );
        }
        assert_eq!(
            strip_leading_self_mentions("<@!654321> <@other> 解析 <@123456>", &own_ids),
            ("<@other> 解析 <@123456>", true)
        );
        for text in [
            "  <@other> <@654321> 解析 链接",
            "解析 <@654321> 链接",
            "<@!> 解析 链接",
            "<@654321 解析 链接",
        ] {
            assert_eq!(strip_leading_self_mentions(text, &own_ids), (text, false));
        }
        assert_eq!(
            strip_leading_self_mentions("<@> 解析", &[""]),
            ("<@> 解析", false)
        );
    }

    #[test]
    fn extracts_nested_and_escaped_links() {
        let data = json!({
            "content": "a",
            "msg_elements": [{
                "content": "b",
                "msg_elements": [{ "content": "https:\\/\\/v.douyin.com\\/nested1" }]
            }]
        });
        assert!(extract_text_from_qq_event(&data).contains("https://v.douyin.com/nested1"));
    }

    #[test]
    fn extracts_miniapp_url_from_any_field() {
        let data = json!({
            "content": "[分享] 哔哩哔哩",
            "ark_data": {
                "ark_type": "miniapp",
                "fields": {
                    "title": "哀鸿日配现已实装",
                    "source": "哔哩哔哩",
                    "appLink": "https://m.bilibili.com/video/BV1mpy6ziE1P"
                }
            }
        });
        assert!(
            extract_text_from_qq_event(&data).contains("https://m.bilibili.com/video/BV1mpy6ziE1P")
        );
    }

    #[test]
    fn extracts_quoted_image_and_ref_id() {
        let data = json!({
            "id": "ROBOT1.0_trigger",
            "content": "ai吗",
            "message_scene": { "ext": ["msg_idx=REFIDX_self", "ref_msg_idx=REFIDX_target_image"] },
            "msg_elements": [{
                "msg_idx": "REFIDX_target_image",
                "content": "引用图片",
                "attachments": [{
                    "url": "http://127.0.0.1/media/test.bin",
                    "filename": "quoted.png",
                    "content_type": "image/png",
                    "width": 500,
                    "height": 700,
                    "size": 15
                }]
            }]
        });
        let quoted = extract_quoted_content_from_qq_event(&data);
        assert_eq!(quoted.message_id.as_deref(), Some("REFIDX_target_image"));
        assert_eq!(quoted.images.len(), 1);
        assert_eq!(quoted.images[0].content_type, "image/png");
    }

    #[test]
    fn markdown_truncates_gallery_images() {
        let data = ParsedData {
            media_type: "image".into(),
            images: vec![
                "https://a/1.jpg".into(),
                "https://a/2.jpg".into(),
                "https://a/3.jpg".into(),
            ],
            image_size_map: Some(HashMap::from([(
                "https://a/1.jpg".into(),
                ImageSize {
                    width: 1366,
                    height: 2048,
                },
            )])),
            ..ParsedData::default()
        };
        let markdown = build_result_markdown(
            &data,
            "图集",
            &MarkdownOptions {
                show_image_text: true,
                show_cover: true,
                max_images: 2,
            },
        );
        assert!(markdown.contains("![img#1366px #2048px](https://a/1.jpg)"));
        assert!(markdown.contains("![img#750px #1000px](https://a/2.jpg)"));
        assert!(markdown.contains("其余 1 张"));
    }
}
