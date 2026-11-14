use std::collections::HashMap;

use chrono::{DateTime, Local};
use once_cell::sync::Lazy;
use regex::Regex;

use crate::types::ParsedData;

static VAR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\$\{([^}]+)\}").unwrap());

pub fn format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return String::new();
    }
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

pub fn format_publish_time(ms: i64) -> String {
    if ms == 0 {
        return String::new();
    }
    DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y年%m月%d日 %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

pub fn generate_formatted_text(data: &ParsedData, format: &str) -> String {
    let image_count = if !data.images.is_empty() {
        data.images.len()
    } else {
        data.live_photo.len()
    };
    let vars: HashMap<&str, String> = HashMap::from([
        ("标题", data.title.clone()),
        ("作者", data.author.clone()),
        ("简介", data.desc.clone()),
        ("视频时长", format_duration(data.duration)),
        ("点赞数", data.like.to_string()),
        ("收藏数", data.collect.to_string()),
        ("转发数", data.share.to_string()),
        ("播放数", data.play.to_string()),
        ("评论数", data.comment.to_string()),
        (
            "发布时间",
            if data.publish_time != 0 {
                format_publish_time(data.publish_time)
            } else {
                String::new()
            },
        ),
        ("图片数量", image_count.to_string()),
        ("作者ID", data.uid.clone()),
        ("封面", data.cover.clone()),
        ("视频链接", data.video.clone()),
    ]);

    let mut result = Vec::new();
    for line in format.lines() {
        let captures: Vec<_> = VAR_RE.captures_iter(line).collect();
        if !captures.is_empty() {
            let all_empty = captures.iter().all(|cap| {
                vars.get(&cap[1])
                    .is_none_or(|value| value.is_empty() || value == "0")
            });
            if all_empty {
                continue;
            }
        }
        let mut rendered = line.to_string();
        for (key, value) in &vars {
            rendered = rendered.replace(&format!("${{{key}}}"), value);
        }
        result.push(rendered);
    }
    result.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_values_and_hides_empty_lines() {
        let data = ParsedData {
            title: "标题X".into(),
            like: 42,
            duration: 3661,
            ..ParsedData::default()
        };
        let text = generate_formatted_text(
            &data,
            "标题：${标题}\n点赞：${点赞数}\n收藏：${收藏数}\n时长：${视频时长}",
        );
        assert_eq!(text, "标题：标题X\n点赞：42\n时长：1:01:01");
    }
}
