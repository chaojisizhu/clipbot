use std::path::Path;
use std::time::Duration;

use anyhow::{Result, bail, ensure};
use url::Url;

use crate::parser::media::{TempMedia, download_media_temp, is_foreign_url};
use crate::parser::process::spawn_capture;
use crate::types::{AppConfig, ParsedData};

pub async fn download_parsed_temp(data: &ParsedData, config: &AppConfig) -> Result<TempMedia> {
    let max_bytes = if config.output.max_video_size == 0 {
        190 * 1024 * 1024
    } else {
        config.output.max_video_size.saturating_mul(1024 * 1024)
    };
    let proxies = [config.api.proxy.clone(), config.ytdlp.proxy.clone()];
    let timeout = Duration::from_millis(config.output.video_download_timeout.max(1));
    let is_manifest = Url::parse(&data.video).ok().is_some_and(|url| {
        let path = url.path().to_ascii_lowercase();
        path.ends_with(".m3u8") || path.ends_with(".mpd")
    });
    if !is_manifest && data.audio_url.is_none() {
        return download_media_temp(
            &data.video,
            &proxies,
            &data.media_headers,
            Path::new(&config.output.temp_dir),
            max_bytes,
            config.resources.max_temp_bytes,
            timeout,
            data.video_decode_key.as_deref(),
        )
        .await;
    }
    ensure!(data.video_decode_key.is_none(), "视频号加密流需要直接下载");
    ensure!(
        !config.output.ffmpeg_command.trim().is_empty(),
        "请配置 output.ffmpegCommand 以合流此媒体"
    );
    let mut temporary = TempMedia::reserve(
        Path::new(&config.output.temp_dir),
        max_bytes.saturating_add(1),
        config.resources.max_temp_bytes,
    )?;
    let mut args = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-max_alloc".into(),
        "16777216".into(),
    ];
    for input in std::iter::once(data.video.as_str()).chain(data.audio_url.as_deref()) {
        let url = Url::parse(input)?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none(),
            "合流输入必须为 HTTP(S) 媒体"
        );
        args.extend([
            "-protocol_whitelist".into(),
            "http,https,tcp,tls,crypto".into(),
            "-rw_timeout".into(),
            "15000000".into(),
            "-threads".into(),
            "1".into(),
        ]);
        if is_foreign_url(input)
            && let Some(proxy) = proxies.iter().find(|proxy| !proxy.trim().is_empty())
        {
            args.extend(["-http_proxy".into(), proxy.clone()]);
        }
        let mut headers = String::new();
        for (name, value) in &data.media_headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "user-agent" | "referer" | "origin"
            ) {
                ensure!(!value.contains(['\r', '\n']), "播放请求头包含非法字符");
                headers.push_str(&format!("{name}: {value}\r\n"));
            }
        }
        if !headers.is_empty() {
            args.extend(["-headers".into(), headers]);
        }
        args.extend(["-i".into(), input.into()]);
    }
    args.extend(["-map".into(), "0:v:0".into()]);
    args.extend([
        "-map".into(),
        if data.audio_url.is_some() {
            "1:a:0"
        } else {
            "0:a:0?"
        }
        .into(),
    ]);
    args.extend([
        "-c".into(),
        "copy".into(),
        "-threads".into(),
        "1".into(),
        "-fs".into(),
        max_bytes.saturating_add(1).to_string(),
        "-movflags".into(),
        "+faststart".into(),
        "-f".into(),
        "mp4".into(),
        temporary.path().to_string_lossy().into_owned(),
    ]);
    spawn_capture(&config.output.ffmpeg_command, &args, timeout)
        .await
        .map_err(|error| {
            anyhow::anyhow!("本地媒体合流失败，请检查 FFmpeg、播放地址和网络。{error}")
        })?;
    let size = tokio::fs::metadata(temporary.path()).await?.len();
    if size > max_bytes {
        bail!("合流媒体超过大小上限，已清理输出");
    }
    temporary.finish()?;
    Ok(temporary)
}
