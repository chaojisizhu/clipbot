use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use once_cell::sync::Lazy;
use rand::RngCore;
use regex::Regex;
use tracing::{info, warn};

use crate::parser::process::{
    ProcessDiskBudget, ProcessError, disk_usage, spawn_capture_in_monitored,
};
use crate::parser::zipcrypto::{ZipFileEntry, random_password, write_encrypted_zip_file};
use crate::types::{Attachment, JmConfig, ParsedData};

static JM_ALBUM_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^jm[ ]+(\d{3,})").unwrap());
static JM_PHOTO_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^jm[ ]+photo[ ]+(\d{3,})").unwrap());
static IMAGE_EXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\.(jpe?g|png|webp)$").unwrap());

struct WorkDirectory {
    root: PathBuf,
    path: PathBuf,
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        // Resolve the recursive deletion target against the configured temp root.
        let Ok(resolved) = std::fs::canonicalize(&self.path) else {
            return;
        };
        if resolved.parent() != Some(self.root.as_path()) {
            return;
        }
        if let Err(error) = std::fs::remove_dir_all(&resolved) {
            warn!("清理 JM 临时目录失败: {error}");
        }
    }
}

struct ArchiveOutput {
    path: PathBuf,
    persisted: bool,
}

impl Drop for ArchiveOutput {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

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
    [exe_dir.join(relative), exe_dir.join("..").join(relative)]
        .into_iter()
        .find(|candidate| candidate.exists())
}

fn command_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .map(|directory| directory.join(name))
                .any(|candidate| {
                    candidate.is_file()
                        || (cfg!(windows) && candidate.with_extension("exe").is_file())
                })
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
    args.push(format!("--max-pages={}", config.max_pages));
    let timeout = Duration::from_millis(config.timeout.max(1));
    let budget = || {
        Some(ProcessDiskBudget {
            path: out_dir.to_owned(),
            max_bytes: config.max_bytes,
        })
    };
    let output = match spawn_capture_in_monitored(python, &args, timeout, None, budget()).await {
        Ok(output) => output,
        Err(ProcessError::NotFound(err)) if python == "python" => {
            warn!("python 不可用，回退 python3: {err}");
            spawn_capture_in_monitored("python3", &args, timeout, None, budget()).await?
        }
        Err(err) => return Err(err.into()),
    };
    if !output.lines().any(|line| line.starts_with("JM_OK")) {
        anyhow::bail!(
            "下载脚本未返回 JM_OK: {}",
            output.trim().chars().take(120).collect::<String>()
        );
    }
    let path = out_dir.to_owned();
    let maximum = config.max_bytes;
    let size = tokio::task::spawn_blocking(move || disk_usage(&path, maximum)).await??;
    anyhow::ensure!(size <= maximum, "JM 下载图片超过磁盘大小上限");
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
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&path, output);
            } else if kind.is_file()
                && path
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
            &left.strip_prefix(root).unwrap_or(left).to_string_lossy(),
            &right.strip_prefix(root).unwrap_or(right).to_string_lossy(),
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
    create_selected_7z_archive(
        work_dir,
        output_path,
        password,
        timeout,
        &collect_images(work_dir),
        u32::MAX as u64,
    )
    .await
}

fn selected_file_list(work_dir: &Path, images: &[PathBuf]) -> anyhow::Result<String> {
    images
        .iter()
        .map(|image| {
            let relative = image.strip_prefix(work_dir)?;
            let name = relative.to_string_lossy().replace('\\', "/");
            anyhow::ensure!(
                !name.contains(['\r', '\n']) && !name.starts_with('-'),
                "压缩文件名无效"
            );
            Ok(format!("{name}\n"))
        })
        .collect()
}

async fn create_selected_7z_archive(
    work_dir: &Path,
    output_path: &Path,
    password: &str,
    timeout: Duration,
    images: &[PathBuf],
    max_bytes: u64,
) -> anyhow::Result<()> {
    let seven_zip =
        seven_zip_command().ok_or_else(|| anyhow::anyhow!("找不到 7zr/7zz/7z 压缩工具"))?;
    let list_path = work_dir.join(".clipbot-files.txt");
    std::fs::write(&list_path, selected_file_list(work_dir, images)?)?;
    let args = vec![
        "a".into(),
        "-y".into(),
        format!("-p{password}"),
        "-mhe=on".into(),
        "-mx=0".into(),
        "-mmt=1".into(),
        "-ms=off".into(),
        "-scsUTF-8".into(),
        output_path.to_string_lossy().to_string(),
        format!("@{}", list_path.to_string_lossy()),
    ];
    spawn_capture_in_monitored(
        &seven_zip,
        &args,
        timeout,
        Some(work_dir),
        Some(ProcessDiskBudget {
            path: output_path.to_owned(),
            max_bytes,
        }),
    )
    .await?;
    anyhow::ensure!(
        std::fs::metadata(output_path)?.len() <= max_bytes,
        "JM 压缩包超过大小上限"
    );
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
    anyhow::ensure!(
        !id.is_empty() && id.len() <= 20 && id.bytes().all(|byte| byte.is_ascii_digit()),
        "JM 编号必须为1至20位数字"
    );
    anyhow::ensure!(config.max_bytes > 0, "JM maxBytes 必须大于零");
    let temp_dir = PathBuf::from(if config.temp_dir.is_empty() {
        "./temp_jm"
    } else {
        &config.temp_dir
    });
    std::fs::create_dir_all(&temp_dir)?;
    let temp_dir = std::fs::canonicalize(&temp_dir)?;
    let mut random = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut random);
    let work_dir = temp_dir.join(format!(
        "jm_{id}_{}_{}",
        chrono::Utc::now().timestamp_millis(),
        hex::encode(random)
    ));
    std::fs::create_dir_all(&work_dir)?;
    let work = Arc::new(WorkDirectory {
        root: temp_dir.clone(),
        path: work_dir.clone(),
    });

    async {
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
        let mut archive = if seven_zip_command().is_some() {
            let path = temp_dir.join(format!("{unique}.7z"));
            let archive = ArchiveOutput {
                path,
                persisted: false,
            };
            create_selected_7z_archive(
                &work_dir,
                &archive.path,
                &password,
                Duration::from_secs(120),
                &images,
                config.max_bytes,
            )
            .await?;
            info!(
                "JM{id} 已打包 7z(AES-256): {} 页, {}MB",
                images.len(),
                std::fs::metadata(&archive.path)
                    .map(|metadata| metadata.len() / 1024 / 1024)
                    .unwrap_or(0)
            );
            archive
        } else {
            let entries: Vec<ZipFileEntry> = images
                .iter()
                .enumerate()
                .map(|(index, image)| {
                    let extension = image
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .map(|extension| format!(".{}", extension.to_ascii_lowercase()))
                        .unwrap_or_else(|| ".jpg".into());
                    ZipFileEntry {
                        name: format!("{:03}{extension}", index + 1),
                        path: image.to_owned(),
                    }
                })
                .collect();
            let path = temp_dir.join(format!("{unique}.zip"));
            let blocking_work = Arc::clone(&work);
            let password = password.clone();
            let album_id = id.to_owned();
            let maximum = config.max_bytes;
            tokio::task::spawn_blocking(move || {
                let _work = blocking_work;
                let size = write_encrypted_zip_file(&entries, &password, &path, maximum)?;
                info!(
                    "JM{album_id} 已流式打包加密 ZIP: {} 页, {}MB",
                    entries.len(),
                    size / 1024 / 1024
                );
                Ok::<_, anyhow::Error>(ArchiveOutput {
                    path,
                    persisted: false,
                })
            })
            .await??
        };

        let file_name = archive
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let mut parsed = ParsedData::empty("jmcomic");
        parsed.title = format!("JM{id}");
        parsed.desc = format!("JMComic 本子 {id} ({}页)", images.len());
        parsed.uid = id.to_string();
        parsed.attachment = Some(Attachment {
            path: archive.path.to_string_lossy().to_string(),
            password,
            file_name: Some(file_name),
        });
        archive.persisted = true;
        Ok(parsed)
    }
    .await
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

    #[test]
    fn keeps_chapter_order_and_archives_only_selected_images() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["2/001.jpg", "1/010.jpg", "1/002.jpg", "1/001.jpg"] {
            let path = directory.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, [0]).unwrap();
        }
        std::fs::write(directory.path().join("debug.log"), [0]).unwrap();
        let images = collect_images(directory.path());
        assert_eq!(images.len(), 4);
        let selected = selected_file_list(directory.path(), &images[..2]).unwrap();
        assert_eq!(selected, "1/001.jpg\n1/002.jpg\n");
    }

    #[test]
    fn cleanup_stays_inside_the_configured_temporary_root() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let path = root.join("jm-test-owned");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("001.jpg"), [0]).unwrap();
        drop(WorkDirectory {
            root: root.clone(),
            path: path.clone(),
        });
        assert!(!path.exists());
        drop(WorkDirectory {
            root,
            path: outside.path().to_owned(),
        });
        assert!(outside.path().exists());
    }
}
