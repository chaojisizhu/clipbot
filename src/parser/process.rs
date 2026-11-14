use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
}

pub struct ProcessDiskBudget {
    pub path: PathBuf,
    pub max_bytes: u64,
}

pub(crate) fn disk_usage(path: &Path, limit: u64) -> std::io::Result<u64> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if metadata.is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return std::fs::File::open(path)?
            .metadata()
            .map(|metadata| metadata.len());
    }
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        total = total.saturating_add(disk_usage(&entry.path(), limit.saturating_sub(total))?);
        if total > limit {
            break;
        }
    }
    Ok(total)
}

async fn monitor_disk(budget: Option<ProcessDiskBudget>) -> std::io::Result<()> {
    let Some(budget) = budget else {
        return futures_util::future::pending().await;
    };
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let path = budget.path.clone();
        let maximum = budget.max_bytes;
        let size = tokio::task::spawn_blocking(move || disk_usage(&path, maximum))
            .await
            .map_err(std::io::Error::other)??;
        if size > maximum {
            return Err(std::io::Error::other("子进程临时文件超过磁盘大小上限"));
        }
    }
}

pub async fn spawn_capture(
    command: &str,
    args: &[String],
    timeout: Duration,
) -> Result<String, ProcessError> {
    spawn_capture_in(command, args, timeout, None).await
}

pub async fn spawn_capture_in(
    command: &str,
    args: &[String],
    timeout: Duration,
    cwd: Option<&Path>,
) -> Result<String, ProcessError> {
    spawn_capture_in_monitored(command, args, timeout, cwd, None).await
}

pub async fn spawn_capture_in_monitored(
    command: &str,
    args: &[String],
    timeout: Duration,
    cwd: Option<&Path>,
    disk_budget: Option<ProcessDiskBudget>,
) -> Result<String, ProcessError> {
    let command = command.trim();
    if command.is_empty() {
        return Err(ProcessError::NotFound("命令为空".into()));
    }
    // A configured executable path may contain spaces. Treat the whole string as
    // one program when it resolves to an existing file; otherwise support forms
    // such as "python -m yt_dlp".
    let direct_path = Path::new(command);
    let (executable, base_args): (&str, Vec<&str>) = if direct_path.exists() {
        (command, Vec::new())
    } else {
        let mut parts = command.split_whitespace();
        let executable = parts
            .next()
            .ok_or_else(|| ProcessError::NotFound("命令为空".into()))?;
        (executable, parts.collect())
    };
    let mut process = Command::new(executable);
    process
        .args(base_args)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        process.creation_flags(0x0800_0000);
    }

    let mut child = process.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ProcessError::NotFound(error.to_string())
        } else {
            ProcessError::Failed(error.to_string())
        }
    })?;
    let mut stdout_pipe = child.stdout.take().expect("stdout configured");
    let mut stderr_pipe = child.stderr.take().expect("stderr configured");
    let capture = async {
        let stdout = async {
            let mut bytes = Vec::new();
            (&mut stdout_pipe)
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > 8 * 1024 * 1024 {
                return Err(std::io::Error::other("子进程输出超过8MiB限制"));
            }
            Ok(bytes)
        };
        let stderr = async {
            let mut bytes = Vec::new();
            (&mut stderr_pipe)
                .take(128 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await?;
            if bytes.len() > 128 * 1024 {
                return Err(std::io::Error::other("子进程错误输出超过128KiB限制"));
            }
            Ok(bytes)
        };
        let capture = async { tokio::try_join!(child.wait(), stdout, stderr) };
        tokio::select! {
            result = capture => result,
            result = monitor_disk(disk_budget) => {
                Err(result.err().unwrap_or_else(|| std::io::Error::other("子进程磁盘监控意外退出")))
            }
        }
    };
    let (status, stdout_bytes, stderr_bytes) = match tokio::time::timeout(timeout, capture).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(ProcessError::Failed(error.to_string()));
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(ProcessError::Failed(format!(
                "进程执行超时 ({}s)",
                timeout.as_secs()
            )));
        }
    };
    let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
    if status.success() {
        Ok(stdout)
    } else {
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        let tail = detail
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" ");
        Err(ProcessError::Failed(if tail.is_empty() {
            format!("退出码 {:?}", status.code())
        } else {
            tail
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_monitor_counts_nested_files_and_stops_after_limit() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("chapter")).unwrap();
        std::fs::write(directory.path().join("001.jpg"), [0u8; 8]).unwrap();
        std::fs::write(directory.path().join("chapter/001.jpg"), [0u8; 16]).unwrap();
        assert_eq!(disk_usage(directory.path(), 32).unwrap(), 24);
        assert!(disk_usage(directory.path(), 4).unwrap() > 4);
    }

    #[tokio::test]
    #[ignore = "requires Python for subprocess disk-supervision verification"]
    async fn disk_monitor_terminates_an_oversized_writer() {
        let directory = tempfile::tempdir().unwrap();
        let args = vec!["-c".into(),
            "import pathlib,sys,time;pathlib.Path(sys.argv[1]).write_bytes(bytes(8192));time.sleep(20)".into(),
            directory.path().join("oversized.bin").to_string_lossy().into_owned()];
        let result = spawn_capture_in_monitored(
            "python",
            &args,
            Duration::from_secs(10),
            None,
            Some(ProcessDiskBudget {
                path: directory.path().to_owned(),
                max_bytes: 1024,
            }),
        )
        .await;
        assert!(
            matches!(result, Err(ProcessError::Failed(message)) if message.contains("磁盘大小上限"))
        );
        std::fs::remove_file(directory.path().join("oversized.bin")).unwrap();
    }
}
