use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::types::{AppConfig, QQChatEvent, QQMention};

pub const DEFAULT_BLACKLIST_REASON: &str = "你已被权限";
pub const BLACKLIST_FILE_NAME: &str = ".clipbot-blacklist.json";
const MAX_BLACKLIST_BYTES: u64 = 1024 * 1024;
const MAX_OPENID_LEN: usize = 128;
const MAX_REASON_CHARS: usize = 200;
const MAX_NAME_CHARS: usize = 80;

static MENTION_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<@!?([^>\s]+)>").unwrap());

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlacklistEntry {
    pub openid: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub added_at_unix: u64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BlacklistFile {
    version: u32,
    entries: Vec<BlacklistEntry>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BanOutcome {
    Added,
    Updated,
}

#[derive(Debug)]
pub struct BlacklistStore {
    path: PathBuf,
    entries: Vec<BlacklistEntry>,
}

impl BlacklistStore {
    pub fn resolve_path(config: &AppConfig) -> PathBuf {
        let configured = config.qqbot.blacklist_file.trim();
        if !configured.is_empty() {
            PathBuf::from(configured)
        } else {
            Path::new(&config.output.temp_dir).join(BLACKLIST_FILE_NAME)
        }
    }

    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let entries = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    bail!("黑名单文件不是普通文件，拒绝加载");
                }
                if metadata.len() > MAX_BLACKLIST_BYTES {
                    bail!("黑名单文件过大，拒绝加载");
                }
                let mut bytes = Vec::new();
                File::open(&path)
                    .context("读取黑名单文件失败")?
                    .take(MAX_BLACKLIST_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .context("读取黑名单文件失败")?;
                if bytes.len() as u64 > MAX_BLACKLIST_BYTES {
                    bail!("黑名单文件过大，拒绝加载");
                }
                let mut parsed: BlacklistFile =
                    serde_json::from_slice(&bytes).context("黑名单文件损坏，原文件已保留")?;
                if parsed.version != 1 {
                    bail!("黑名单文件版本不支持，原文件已保留");
                }
                for entry in &mut parsed.entries {
                    entry.openid = entry.openid.trim().to_string();
                    entry.name = normalize_single_line(&entry.name, MAX_NAME_CHARS);
                    entry.reason = normalize_reason(&entry.reason);
                }
                validate_entries(&parsed.entries)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                        .context("设置黑名单文件权限失败")?;
                }
                parsed.entries
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error).context("检查黑名单文件失败"),
        };
        Ok(Self { path, entries })
    }

    pub fn entries(&self) -> &[BlacklistEntry] {
        &self.entries
    }

    pub fn reason(&self, openid: &str) -> Option<&str> {
        let openid = openid.trim();
        if openid.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .find(|entry| entry.openid == openid)
            .map(|entry| entry.reason.as_str())
    }

    pub fn add(&mut self, openid: &str, name: &str, reason: &str) -> Result<BanOutcome> {
        let openid = openid.trim();
        validate_openid(openid)?;
        let name = normalize_single_line(name, MAX_NAME_CHARS);
        let reason = normalize_reason(reason);
        let now = unix_now()?;

        let mut next = self.entries.clone();
        let outcome = if let Some(existing) = next.iter_mut().find(|entry| entry.openid == openid) {
            existing.reason = reason;
            if !name.is_empty() {
                existing.name = name;
            }
            existing.added_at_unix = now;
            BanOutcome::Updated
        } else {
            next.push(BlacklistEntry {
                openid: openid.to_string(),
                reason,
                name,
                added_at_unix: now,
            });
            BanOutcome::Added
        };
        save_entries(&self.path, &next)?;
        self.entries = next;
        Ok(outcome)
    }

    pub fn remove(&mut self, openid: &str) -> Result<Option<BlacklistEntry>> {
        let openid = openid.trim();
        if openid.is_empty() {
            return Ok(None);
        }
        let mut next = self.entries.clone();
        let Some(index) = next.iter().position(|entry| entry.openid == openid) else {
            return Ok(None);
        };
        let removed = next.remove(index);
        save_entries(&self.path, &next)?;
        self.entries = next;
        Ok(Some(removed))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlacklistTarget {
    pub openid: String,
    pub name: String,
}

pub fn format_user_label(name: &str, openid: &str) -> String {
    let name = name.trim();
    if name.is_empty() {
        openid.to_string()
    } else {
        format!("{name} ({openid})")
    }
}

impl BlacklistTarget {
    pub fn display_label(&self) -> String {
        format_user_label(&self.name, &self.openid)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlacklistCommand {
    Add {
        targets: Vec<BlacklistTarget>,
        reason: String,
    },
    Remove {
        targets: Vec<BlacklistTarget>,
    },
    MissingTarget {
        is_add: bool,
    },
}

pub fn normalize_reason(raw: &str) -> String {
    let cleaned = normalize_single_line(raw, MAX_REASON_CHARS);
    if cleaned.is_empty() {
        DEFAULT_BLACKLIST_REASON.to_string()
    } else {
        cleaned
    }
}

pub fn parse_blacklist_command(event: &QQChatEvent, self_ids: &[&str]) -> Option<BlacklistCommand> {
    let raw = if event.current_text.is_empty() {
        event.text.trim()
    } else {
        event.current_text.trim()
    };
    let (is_add, after_keyword) = if let Some(rest) = raw.strip_prefix("拉黑") {
        (true, rest)
    } else {
        let rest = raw.strip_prefix("滚出去")?;
        (false, rest)
    };

    let non_self_mentions: Vec<&QQMention> = event
        .mentions
        .iter()
        .filter(|mention| !is_ignored_mention(mention, self_ids))
        .collect();

    let looks_like_command = after_keyword.starts_with(char::is_whitespace)
        || after_keyword.starts_with('@')
        || after_keyword.starts_with("<@")
        || after_keyword.starts_with(':')
        || after_keyword.starts_with('：')
        || !non_self_mentions.is_empty();
    if !looks_like_command {
        return None;
    }

    let rest =
        after_keyword.trim_start_matches(|ch: char| ch.is_whitespace() || ch == ':' || ch == '：');

    let mut tag_ids = Vec::new();
    for captures in MENTION_TAG_RE.captures_iter(rest) {
        let Some(matched) = captures.get(1) else {
            continue;
        };
        let id = matched.as_str().trim();
        if id.is_empty()
            || id.eq_ignore_ascii_case("all")
            || id.eq_ignore_ascii_case("everyone")
            || is_self_id(id, self_ids)
            || event
                .mentions
                .iter()
                .any(|mention| mention.is_you && mention_matches_id(mention, id))
        {
            continue;
        }
        push_unique_string(&mut tag_ids, id);
    }

    let mut remaining = MENTION_TAG_RE.replace_all(rest, " ").into_owned();
    let mut targets = Vec::new();

    if !tag_ids.is_empty() {
        for id in &tag_ids {
            if let Some(mention) = non_self_mentions
                .iter()
                .find(|mention| mention_matches_id(mention, id))
            {
                let openid = mention.target_openid();
                let target_id = if openid.is_empty() {
                    id.as_str()
                } else {
                    openid
                };
                push_unique_target(&mut targets, target_id, mention.display_name());
            } else {
                push_unique_target(&mut targets, id, "");
            }
        }
    } else if !non_self_mentions.is_empty() {
        for mention in &non_self_mentions {
            push_unique_target(
                &mut targets,
                mention.target_openid(),
                mention.display_name(),
            );
        }
        remaining = strip_leading_plain_mentions(&remaining, &non_self_mentions);
    } else if let Some((openid, tail)) = take_leading_hex_openid(&remaining) {
        push_unique_target(&mut targets, openid, "");
        remaining = tail.to_string();
    }

    if targets.is_empty() {
        return Some(BlacklistCommand::MissingTarget { is_add });
    }

    if is_add {
        Some(BlacklistCommand::Add {
            targets,
            reason: normalize_reason(&remaining),
        })
    } else {
        Some(BlacklistCommand::Remove { targets })
    }
}

fn is_ignored_mention(mention: &QQMention, self_ids: &[&str]) -> bool {
    let target = mention.target_openid();
    mention.is_you
        || mention.bot
        || mention.scope.eq_ignore_ascii_case("all")
        || target.is_empty()
        || is_self_id(target, self_ids)
}

fn is_self_id(id: &str, self_ids: &[&str]) -> bool {
    !id.is_empty()
        && self_ids
            .iter()
            .any(|expected| !expected.is_empty() && id == *expected)
}

fn mention_matches_id(mention: &QQMention, id: &str) -> bool {
    (!mention.id.is_empty() && mention.id == id)
        || (!mention.member_openid.is_empty() && mention.member_openid == id)
        || (!mention.user_openid.is_empty() && mention.user_openid == id)
}

fn push_unique_string(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|existing| existing == value) {
        list.push(value.to_string());
    }
}

fn push_unique_target(targets: &mut Vec<BlacklistTarget>, openid: &str, name: &str) {
    let openid = openid.trim();
    if openid.is_empty() {
        return;
    }
    let name = normalize_single_line(name, MAX_NAME_CHARS);
    if let Some(existing) = targets.iter_mut().find(|item| item.openid == openid) {
        if existing.name.is_empty() && !name.is_empty() {
            existing.name = name;
        }
        return;
    }
    targets.push(BlacklistTarget {
        openid: openid.to_string(),
        name,
    });
}

fn strip_leading_plain_mentions(input: &str, mentions: &[&QQMention]) -> String {
    let mut remaining = input.trim_start();
    while let Some(after_at) = remaining.strip_prefix('@') {
        let mut matched_len = None;
        for mention in mentions {
            for candidate in [&mention.member_nickname, &mention.username] {
                let candidate = candidate.trim();
                if !candidate.is_empty() && after_at.starts_with(candidate) {
                    matched_len = Some(matched_len.unwrap_or(0).max(candidate.len()));
                }
            }
        }
        let Some(len) = matched_len else {
            break;
        };
        remaining = after_at[len..].trim_start();
    }
    remaining.to_string()
}

fn take_leading_hex_openid(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim_start();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let token = &trimmed[..end];
    if token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some((token, &trimmed[end..]))
    } else {
        None
    }
}

fn normalize_single_line(raw: &str, max_chars: usize) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn validate_openid(openid: &str) -> Result<()> {
    if openid.is_empty()
        || openid.len() > MAX_OPENID_LEN
        || openid
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        bail!("OpenID 为空、过长或包含非法字符");
    }
    Ok(())
}

fn validate_entries(entries: &[BlacklistEntry]) -> Result<()> {
    let mut seen = HashSet::with_capacity(entries.len());
    for entry in entries {
        validate_openid(&entry.openid)?;
        if !seen.insert(entry.openid.as_str()) {
            bail!("黑名单存在重复 OpenID，原文件已保留");
        }
    }
    Ok(())
}

fn unix_now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 Unix 纪元")?
        .as_secs())
}

fn save_entries(path: &Path, entries: &[BlacklistEntry]) -> Result<()> {
    validate_entries(entries)?;
    let bytes = serde_json::to_vec_pretty(&BlacklistFile {
        version: 1,
        entries: entries.to_vec(),
    })?;
    if bytes.len() as u64 > MAX_BLACKLIST_BYTES {
        bail!("黑名单记录过大，拒绝写入");
    }
    atomic_write(path, &bytes)
}

struct PendingFile(PathBuf);

impl Drop for PendingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("黑名单路径缺少父目录")?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent).context("创建黑名单目录失败")?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            bail!("黑名单路径不是普通文件，拒绝覆盖");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("检查黑名单文件失败"),
    }
    let temporary = parent.join(format!(
        ".clipbot-blacklist-{:032x}.tmp",
        rand::random::<u128>()
    ));
    let _pending = PendingFile(temporary.clone());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).context("创建黑名单临时文件失败")?;
    file.write_all(bytes).context("写入黑名单临时文件失败")?;
    file.sync_all().context("同步黑名单临时文件失败")?;
    drop(file);
    fs::rename(&temporary, path).context("原子更新黑名单文件失败")?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .context("同步黑名单目录失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mention(id: &str, name: &str) -> QQMention {
        QQMention {
            id: id.into(),
            member_openid: id.into(),
            username: name.into(),
            ..QQMention::default()
        }
    }

    #[test]
    fn parses_ban_and_unban_commands_with_mentions_and_default_reason() {
        let with_reason = QQChatEvent {
            current_text: "拉黑<@7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D>   刷屏  发广告 ".into(),
            mentions: vec![mention("7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D", "张三")],
            ..QQChatEvent::default()
        };
        assert_eq!(
            parse_blacklist_command(&with_reason, &["BOT"]),
            Some(BlacklistCommand::Add {
                targets: vec![BlacklistTarget {
                    openid: "7A3B9C1D5E2F4A6B8C0D1E3F5A7B9C2D".into(),
                    name: "张三".into(),
                }],
                reason: "刷屏 发广告".into(),
            })
        );

        let empty_reason = QQChatEvent {
            current_text: "拉黑 <@!USER_001>   ".into(),
            mentions: vec![mention("USER_001", "李四")],
            ..QQChatEvent::default()
        };
        assert_eq!(
            parse_blacklist_command(&empty_reason, &["BOT"]),
            Some(BlacklistCommand::Add {
                targets: vec![BlacklistTarget {
                    openid: "USER_001".into(),
                    name: "李四".into(),
                }],
                reason: DEFAULT_BLACKLIST_REASON.into(),
            })
        );

        let plain_at_with_mention = QQChatEvent {
            current_text: "拉黑@小明 乱发链接".into(),
            mentions: vec![QQMention {
                id: "USER_002".into(),
                member_openid: "USER_002".into(),
                member_nickname: "小明".into(),
                ..QQMention::default()
            }],
            ..QQChatEvent::default()
        };
        assert_eq!(
            parse_blacklist_command(&plain_at_with_mention, &["BOT"]),
            Some(BlacklistCommand::Add {
                targets: vec![BlacklistTarget {
                    openid: "USER_002".into(),
                    name: "小明".into(),
                }],
                reason: "乱发链接".into(),
            })
        );

        let unban = QQChatEvent {
            current_text: "滚出去<@USER_001>".into(),
            mentions: vec![mention("USER_001", "李四")],
            ..QQChatEvent::default()
        };
        assert_eq!(
            parse_blacklist_command(&unban, &["BOT"]),
            Some(BlacklistCommand::Remove {
                targets: vec![BlacklistTarget {
                    openid: "USER_001".into(),
                    name: "李四".into(),
                }],
            })
        );
    }

    #[test]
    fn parses_hex_openid_and_rejects_noise_or_bot_self_mentions() {
        let hex = "A1B2C3D4E5F6A1B2C3D4E5F6A1B2C3D4";
        let hex_ban = QQChatEvent {
            current_text: format!("拉黑 {hex}"),
            ..QQChatEvent::default()
        };
        assert_eq!(
            parse_blacklist_command(&hex_ban, &["BOT"]),
            Some(BlacklistCommand::Add {
                targets: vec![BlacklistTarget {
                    openid: hex.into(),
                    name: String::new(),
                }],
                reason: DEFAULT_BLACKLIST_REASON.into(),
            })
        );

        let hex_unban = QQChatEvent {
            current_text: format!("滚出去 {hex}"),
            ..QQChatEvent::default()
        };
        assert_eq!(
            parse_blacklist_command(&hex_unban, &["BOT"]),
            Some(BlacklistCommand::Remove {
                targets: vec![BlacklistTarget {
                    openid: hex.into(),
                    name: String::new(),
                }],
            })
        );

        let self_or_all = QQChatEvent {
            current_text: "拉黑<@BOT_SELF> <@all> 理由".into(),
            mentions: vec![QQMention {
                id: "BOT_SELF".into(),
                member_openid: "BOT_SELF".into(),
                is_you: true,
                bot: true,
                ..QQMention::default()
            }],
            ..QQChatEvent::default()
        };
        assert_eq!(
            parse_blacklist_command(&self_or_all, &["BOT_SELF"]),
            Some(BlacklistCommand::MissingTarget { is_add: true })
        );

        for chatter in ["拉黑了吗", "滚出去玩吧", "我被拉黑了"] {
            let ev = QQChatEvent {
                current_text: chatter.into(),
                ..QQChatEvent::default()
            };
            assert_eq!(parse_blacklist_command(&ev, &[]), None);
        }
    }

    #[test]
    fn persists_updates_and_preserves_corrupt_blacklist_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLACKLIST_FILE_NAME);

        let mut store = BlacklistStore::open(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(store.add("USER_1", "张三", "").unwrap(), BanOutcome::Added);
        assert!(path.exists());
        assert_eq!(store.reason("USER_1"), Some(DEFAULT_BLACKLIST_REASON));
        assert_eq!(
            store.add("USER_1", "", "第二次违规").unwrap(),
            BanOutcome::Updated
        );
        drop(store);

        let mut reloaded = BlacklistStore::open(&path).unwrap();
        assert_eq!(reloaded.reason("USER_1"), Some("第二次违规"));
        assert_eq!(reloaded.entries()[0].name, "张三");
        assert!(reloaded.remove("USER_1").unwrap().is_some());
        assert!(reloaded.reason("USER_1").is_none());
        drop(reloaded);

        assert!(BlacklistStore::open(&path).unwrap().entries().is_empty());

        fs::write(&path, b"{broken").unwrap();
        assert!(BlacklistStore::open(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
    }
}
