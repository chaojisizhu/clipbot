use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use once_cell::sync::Lazy;
use rand::RngCore;
use regex::Regex;
use tracing::{info, warn};

use crate::parser::process::{ProcessError, spawn_capture, spawn_capture_in};
use crate::parser::zipcrypto::{ZipEntry, create_encrypted_zip, random_password};
use crate::types::{Attachment, JmConfig, ParsedData};

static JM_ALBUM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^jm[ ]+(\d{3,})").unwrap());
static JM_PHOTO_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^jm[ ]+photo[ ]+(\d{3,})").unwrap());
static IMAGE_EXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.(jpe?g|png|webp)$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JmTrigger {
    pub id: String,
    pub is_photo: bool,
}

pub fn extract_jm_id(text: &str) -> Option<JmTrigger> {
    let trimmed = text.trim();
    if let Some(capture) = JM_ALBUM_RE.captures(trimmed) {
        return Some(JmTrigger {
            id: capture[1].to_string(),
            is_photo: false,
        });
    }
    JM_PHOTO_RE.captures(trimmed).map(|capture| JmTrigger {
        id: capture[1].to_string(),
        is_photo: true,
    })
}

fn resource_path(relative: &str) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?.join(relative);
    if cwd.exists() {
        return Some(cwd);
    }
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    for candidate in [exe_dir.join(relative), exe_dir.join("..").join(relative)] {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn command_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|directory| directory.join(name))
                .any(|candidate| candidate.is_file())
        })
        .unwrap_or(false)
}

fn seven_zip_command() -> Option<String> {
    for relative in ["bin/7zr.exe", "bin/7zzs", "bin/7zz", "bin/7z"] {
        if let Some(path) = resource_path(relative) {
            return Some(path.to_string_lossy().to_string());
        }
    }
    ["7zz", "7z"]
        .into_iter()
        .find(|command| command_on_path(command))
        .map(ToString::to_string)
}

async fn run_download(
    python: &str,
    id: &str,
    is_photo: bool,
    out_dir: &Path,
    config: &JmConfig,
) -> anyhow::Result<()> {
    let script = resource_path("scripts/jm_download.py")
        .ok_or_else(|| anyhow::anyhow!("找不到 scripts/jm_download.py"))?;
    let mut args = vec![
        script.to_string_lossy().to_string(),
        id.to_string(),
        out_dir.to_string_lossy().to_string(),
    ];
    if is_photo {
        args.push("--photo".into());
    }
    if !config.proxy.is_empty() {
        args.push(format!("--proxy={}", config.proxy));
    }
    if config.threads > 0 {
        args.push(format!("--threads={}", config.threads.clamp(1, 32)));
    }
    let timeout = Duration::from_millis(config.timeout.max(1));
    let output = match spawn_capture(python, &args, timeout).await {
        Ok(output) => output,
        Err(ProcessError::NotFound(err)) if python == "python" => {
            warn!("python 不可用，回退 python3: {err}");
            spawn_capture("python3", &args, timeout).await?
        }
        Err(err) => return Err(err.into()),
    };
    if !output.lines().any(|line| line.starts_with("JM_OK")) {
        anyhow::bail!(
            "下载脚本未返回 JM_OK: {}",
            output.trim().chars().take(120).collect::<String>()
        );
    }
    Ok(())
}

fn natural_cmp(left: &str, right: &str) -> Ordering {
    let left = left.to_ascii_lowercase();
    let right = right.to_ascii_lowercase();
    let left_bytes = left.as_bytes();
    let right_bytes = right.as_bytes();
    let mut li = 0usize;
    let mut ri = 0usize;
    while li < left_bytes.len() && ri < right_bytes.len() {
        if left_bytes[li].is_ascii_digit() && right_bytes[ri].is_ascii_digit() {
            let l_start = li;
            let r_start = ri;
            while li < left_bytes.len() && left_bytes[li].is_ascii_digit() {
                li += 1;
            }
            while ri < right_bytes.len() && right_bytes[ri].is_ascii_digit() {
                ri += 1;
            }
            let l_num = &left[l_start..li];
            let r_num = &right[r_start..ri];
            let l_trim = l_num.trim_start_matches('0');
            let r_trim = r_num.trim_start_matches('0');
            let order = l_trim
                .len()
                .cmp(&r_trim.len())
                .then_with(|| l_trim.cmp(r_trim))
                .then_with(|| l_num.len().cmp(&r_num.len()));
            if order != Ordering::Equal {
                return order;
            }
        } else {
            let order = left_bytes[li].cmp(&right_bytes[ri]);
            if order != Ordering::Equal {
                return order;
            }
            li += 1;
            ri += 1;
        }
    }
    left_bytes.len().cmp(&right_bytes.len())
}

fn collect_images(root: &Path) -> Vec<PathBuf> {
    fn walk(path: &Path, output: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, output);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| IMAGE_EXT_RE.is_match(name))
            {
                output.push(path);
            }
        }
    }
    let mut output = Vec::new();
    walk(root, &mut output);
    output.sort_by(|left, right| {
        natural_cmp(
            left.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
            right
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        )
    });
    output
}

pub async fn create_7z_archive(
    work_dir: &Path,
    output_path: &Path,
    password: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let seven_zip =
        seven_zip_command().ok_or_else(|| anyhow::anyhow!("找不到 7zr/7zz/7z 压缩工具"))?;
    let args = vec![
        "a".into(),
        "-y".into(),
        format!("-p{password}"),
        "-mhe=on".into(),
        "-mx=5".into(),
        "-ms=off".into(),
        output_path.to_string_lossy().to_string(),
        ".".into(),
    ];
    spawn_capture_in(&seven_zip, &args, timeout, Some(work_dir)).await?;
    Ok(())
}

pub async fn download_jm_album(
    id: &str,
    is_photo: bool,
    config: &JmConfig,
) -> anyhow::Result<ParsedData> {
    if !config.enabled {
        anyhow::bail!("JM 下载未启用");
    }
    let temp_dir = PathBuf::from(if config.temp_dir.is_empty() {
        "./temp_jm"
    } else {
        &config.temp_dir
    });
    std::fs::create_dir_all(&temp_dir)?;
    let temp_dir = std::fs::canonicalize(&temp_dir)?;
    let mut random = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut random);
    let work_dir = temp_dir.join(format!(
        "jm_{id}_{}_{}",
        chrono::Utc::now().timestamp_millis(),
        hex::encode(random)
    ));
    std::fs::create_dir_all(&work_dir)?;

    let result = async {
        run_download(
            if config.python.is_empty() {
                "python"
            } else {
                &config.python
            },
            id,
            is_photo,
            &work_dir,
            config,
        )
        .await?;
        let mut images = collect_images(&work_dir);
        if config.max_pages > 0 {
            images.truncate(config.max_pages);
        }
        if images.is_empty() {
            anyhow::bail!("本子 {id} 未下载到任何图片");
        }

        let password = if config.password.trim().is_empty() {
            random_password(8)
        } else {
            config.password.trim().to_string()
        };
        let unique = format!(
            "JM{id}_{}_{}",
            chrono::Utc::now().timestamp_millis(),
            hex::encode(random)
        );
        let archive_path = if seven_zip_command().is_some() {
            let path = temp_dir.join(format!("{unique}.7z"));
            if let Err(err) =
                create_7z_archive(&work_dir, &path, &password, Duration::from_secs(120)).await
            {
                let _ = std::fs::remove_file(&path);
                return Err(err);
            }
            info!(
                "JM{id} 已打包 7z(AES-256): {} 页, {}MB",
                images.len(),
                std::fs::metadata(&path)
                    .map(|metadata| metadata.len() / 1024 / 1024)
                    .unwrap_or(0)
            );
            path
        } else {
            let entries: Vec<ZipEntry> = images
                .iter()
                .enumerate()
                .map(|(index, image)| {
                    let extension = image
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .map(|extension| format!(".{}", extension.to_ascii_lowercase()))
                        .unwrap_or_else(|| ".jpg".into());
                    Ok(ZipEntry {
                        name: format!("{:03}{extension}", index + 1),
                        data: std::fs::read(image)?,
                    })
                })
                .collect::<std::io::Result<Vec<_>>>()?;
            let zip = create_encrypted_zip(&entries, &password);
            let path = temp_dir.join(format!("{unique}.zip"));
            std::fs::write(&path, &zip)?;
            info!(
                "JM{id} 已打包加密 ZIP(回退模式): {} 页, {}MB",
                entries.len(),
                zip.len() / 1024 / 1024
            );
            path
        };

        let file_name = archive_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let mut parsed = ParsedData::empty("jmcomic");
        parsed.title = format!("JM{id}");
        parsed.desc = format!("JMComic 本子 {id} ({}页)", images.len());
        parsed.uid = id.to_string();
        parsed.attachment = Some(Attachment {
            path: archive_path.to_string_lossy().to_string(),
            password,
            file_name: Some(file_name),
        });
        Ok(parsed)
    }
    .await;

    if let Err(err) = std::fs::remove_dir_all(&work_dir) {
        if work_dir.exists() {
            warn!("清理 JM 临时目录失败: {err}");
        }
    }
    result
}

pub fn cleanup_jm_zip(path: &str) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_leading_jm_commands() {
        assert_eq!(
            extract_jm_id("jm 350234"),
            Some(JmTrigger {
                id: "350234".into(),
                is_photo: false
            })
        );
        assert_eq!(
            extract_jm_id("JM  123456 "),
            Some(JmTrigger {
                id: "123456".into(),
                is_photo: false
            })
        );
        assert_eq!(
            extract_jm_id("jm photo 987654"),
            Some(JmTrigger {
                id: "987654".into(),
                is_photo: true
            })
        );
        assert_eq!(extract_jm_id("解析jm的来了"), None);
        assert_eq!(extract_jm_id("看看这个 jm 350234"), None);
        assert_eq!(extract_jm_id("jm abc"), None);
    }

    #[test]
    fn natural_sort_orders_pages() {
        let mut names = vec!["10.jpg", "2.jpg", "1.jpg", "001.jpg"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["1.jpg", "001.jpg", "2.jpg", "10.jpg"]);
    }
}
