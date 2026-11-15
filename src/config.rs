use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::types::AppConfig;

pub fn load_config(config_path: Option<&Path>) -> Result<AppConfig> {
    if let Some(path) = config_path {
        return read_config(path);
    }
    for name in ["config.yaml", "config.yml", "config.json"] {
        let path = Path::new(name);
        if path.exists() {
            return read_config(path);
        }
    }
    Ok(normalize_config(AppConfig::default()))
}

fn read_config(path: &Path) -> Result<AppConfig> {
    let content =
        fs::read_to_string(path).with_context(|| format!("读取配置失败: {}", path.display()))?;
    let config: AppConfig = if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        serde_json::from_str(&content)?
    } else {
        serde_yaml::from_str(&content)?
    };
    Ok(normalize_config(config))
}

fn normalize_config(mut config: AppConfig) -> AppConfig {
    apply_environment(&mut config);
    let mut api_keys = Vec::new();
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        push_api_key(&mut api_keys, &api_key);
    }
    if let Ok(api_keys_env) = std::env::var("OPENAI_API_KEYS") {
        for api_key in api_keys_env.split([',', ';', '\n']) {
            push_api_key(&mut api_keys, api_key);
        }
    }
    push_api_key(&mut api_keys, &config.gpt_detect.api_key);
    for api_key in &config.gpt_detect.api_keys {
        push_api_key(&mut api_keys, api_key);
    }
    config.gpt_detect.api_keys = api_keys;
    let bilibili_cookie = Path::new("cookies/bilibili.txt");
    if !config
        .auth
        .cookies
        .get("bilibili")
        .is_some_and(|cookie| !cookie.trim().is_empty())
        && !config
            .auth
            .cookie_files
            .get("bilibili")
            .is_some_and(|path| !path.trim().is_empty())
        && bilibili_cookie.is_file()
    {
        config.auth.cookie_files.insert(
            "bilibili".into(),
            bilibili_cookie.to_string_lossy().into_owned(),
        );
    }
    if config.gpt_detect.keywords.is_empty() {
        config.gpt_detect.keywords = vec!["ai吗".into()];
    }
    config
}

fn apply_environment(config: &mut AppConfig) {
    for (name, target) in [
        ("QQBOT_APP_ID", &mut config.qqbot.app_id),
        ("QQBOT_APP_SECRET", &mut config.qqbot.app_secret),
        ("YUANBAO_COOKIE", &mut config.channels.yuanbao_cookie),
        ("ALLVIDEO_PROXY", &mut config.api.proxy),
    ] {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            *target = value;
        }
    }
    if let Ok(value) = std::env::var("CLIPBOT_PROXY")
        && !value.trim().is_empty()
    {
        config.api.proxy = value;
    }
    if config.gpt_detect.api_key.is_empty() {
        config.gpt_detect.api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    }
}

fn push_api_key(keys: &mut Vec<String>, api_key: &str) {
    let api_key = api_key.trim();
    if !api_key.is_empty() && !keys.iter().any(|existing| existing == api_key) {
        keys.push(api_key.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_yaml_uses_defaults() {
        let cfg: AppConfig = serde_yaml::from_str(
            r#"
qqbot:
  appId: test
api:
  timeout: 42
"#,
        )
        .unwrap();
        assert_eq!(cfg.qqbot.app_id, "test");
        assert_eq!(cfg.qqbot.api_base, "https://api.bot.qq.com");
        assert_eq!(cfg.api.timeout, 42);
        assert_eq!(cfg.resources.cache_entries, 128);
        assert!(!cfg.platform_enabled("weibo"));
        assert!(!cfg.platform_enabled("youtube"));
        assert!(!cfg.platform_enabled("zhihu"));
        assert!(!cfg.platform_enabled("instagram"));
        assert!(!cfg.platform_enabled("tiktok"));
        let explicitly_enabled: AppConfig =
            serde_yaml::from_str("platformEnable: {zhihu: true}").unwrap();
        assert!(explicitly_enabled.platform_enabled("zhihu"));
        assert!(!explicitly_enabled.platform_enabled("youtube"));
    }

    #[test]
    fn merges_and_deduplicates_api_keys() {
        let mut keys = Vec::new();
        push_api_key(&mut keys, "key-a");
        push_api_key(&mut keys, " key-a ");
        push_api_key(&mut keys, "key-b");
        push_api_key(&mut keys, "");
        assert_eq!(keys, vec!["key-a", "key-b"]);
    }

    #[test]
    fn parses_legacy_and_multiple_openai_keys() {
        let cfg: AppConfig = serde_yaml::from_str(
            r#"
gptDetect:
  apiKey: legacy-key
  apiKeys:
    - key-2
    - key-3
"#,
        )
        .unwrap();
        assert_eq!(cfg.gpt_detect.api_key, "legacy-key");
        assert_eq!(cfg.gpt_detect.api_keys, vec!["key-2", "key-3"]);
    }
}
