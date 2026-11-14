use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::types::AppConfig;

pub fn load_config(config_path: Option<&Path>) -> AppConfig {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut target = config_path.map(Path::to_path_buf);

    if target.is_none() {
        for name in ["config.yaml", "config.yml", "config.json"] {
            let candidate = root.join(name);
            if candidate.exists() {
                target = Some(candidate);
                break;
            }
        }
    }

    if target.is_none() {
        let example = root.join("config.example.yaml");
        let yaml = root.join("config.yaml");
        if example.exists() {
            match fs::copy(&example, &yaml) {
                Ok(_) => {
                    info!("已自动从 config.example.yaml 创建 config.yaml 配置文件");
                    target = Some(yaml);
                }
                Err(err) => warn!("创建 config.yaml 失败，将使用默认配置: {err}"),
            }
        }
    }

    let Some(target) = target else {
        warn!("未找到配置文件，将使用默认内置配置");
        return AppConfig::default();
    };

    match read_config(&target) {
        Ok(config) => {
            info!("已成功加载配置文件: {}", target.display());
            config
        }
        Err(err) => {
            warn!("加载配置文件失败，回退到默认配置: {err:#}");
            AppConfig::default()
        }
    }
}

fn read_config(path: &Path) -> Result<AppConfig> {
    let content =
        fs::read_to_string(path).with_context(|| format!("读取配置失败: {}", path.display()))?;
    let mut config: AppConfig = if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        serde_json::from_str(&content)?
    } else {
        serde_yaml::from_str(&content)?
    };
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
    if config.gpt_detect.keywords.is_empty() {
        config.gpt_detect.keywords = vec!["ai吗".into()];
    }
    Ok(config)
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
        assert_eq!(
            cfg.api.primary_url,
            "https://api.bugpk.com/api/short_videos"
        );
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
