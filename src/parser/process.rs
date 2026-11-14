use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
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

    let output = match tokio::time::timeout(timeout, process.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProcessError::NotFound(err.to_string()));
        }
        Ok(Err(err)) => return Err(ProcessError::Failed(err.to_string())),
        Err(_) => {
            return Err(ProcessError::Failed(format!(
                "进程执行超时 ({}s)",
                timeout.as_secs()
            )));
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
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
            format!("退出码 {:?}", output.status.code())
        } else {
            tail
        }))
    }
}
