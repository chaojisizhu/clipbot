use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use tokio::time::Instant;
use url::Url;

use crate::parser::media::{TempMedia, download_media_temp, is_foreign_url};
use crate::parser::process::spawn_capture;
use crate::types::{AppConfig, ParsedData};

const MAX_LIVE_ALBUM_CLIPS: usize = 12;
const MAX_LIVE_ALBUM_BYTES: u64 = 64 * 1024 * 1024;
const MAX_NORMALIZE_SECONDS: f64 = 60.0;
const MAX_NORMALIZE_PIXELS: u64 = 1920 * 1080;
const CONCAT_MANIFEST_BYTES: u64 = 4096;

fn max_video_bytes(config: &AppConfig) -> u64 {
    if config.output.max_video_size == 0 {
        190 * 1024 * 1024
    } else {
        config.output.max_video_size.saturating_mul(1024 * 1024)
    }
}

pub async fn download_parsed_temp(data: &ParsedData, config: &AppConfig) -> Result<TempMedia> {
    let max_bytes = max_video_bytes(config);
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

struct ClipInfo {
    streams: Vec<Value>,
    duration: f64,
}

fn normalization_canvas(info: &[ClipInfo]) -> Result<(u64, u64)> {
    let first = info.first().context("实况图集没有媒体信息")?;
    let audio = |clip: &ClipInfo| {
        clip.streams
            .iter()
            .find(|stream| stream["codec_type"] == "audio")
            .cloned()
    };
    let expected_audio = audio(first);
    ensure!(
        info.iter().map(|clip| clip.duration).sum::<f64>() <= MAX_NORMALIZE_SECONDS,
        "异规格实况图集超过 60 秒转换上限"
    );
    let mut first_dimensions = None;
    for clip in info {
        ensure!(
            audio(clip) == expected_audio,
            "实况短片音频参数不同，无法直接合并音轨"
        );
        let video = clip
            .streams
            .iter()
            .find(|stream| stream["codec_type"] == "video")
            .context("实况短片缺少视频轨")?;
        let width = video["width"].as_u64().context("实况短片宽度无效")?;
        let height = video["height"].as_u64().context("实况短片高度无效")?;
        ensure!(
            width <= 1920 && height <= 1920 && width.saturating_mul(height) <= MAX_NORMALIZE_PIXELS,
            "异规格实况短片超过 1080p 转换上限"
        );
        ensure!(
            video["sample_aspect_ratio"]
                .as_str()
                .is_none_or(|ratio| matches!(ratio, "1:1" | "0:1")),
            "异规格实况短片包含不支持的像素比例"
        );
        ensure!(
            video
                .get("side_data_list")
                .and_then(Value::as_array)
                .is_none_or(|entries| {
                    entries
                        .iter()
                        .all(|entry| entry["rotation"].as_i64().unwrap_or(0) == 0)
                })
                && video
                    .pointer("/tags/rotate")
                    .and_then(Value::as_str)
                    .is_none_or(|rotation| rotation == "0"),
            "异规格实况短片包含旋转参数，无法统一画布"
        );
        first_dimensions.get_or_insert((width, height));
    }
    let (width, height) = first_dimensions.context("实况短片缺少尺寸")?;
    let long_edge = width.max(height).max(1280);
    let width = (width * 1280 / long_edge) / 2 * 2;
    let height = (height * 1280 / long_edge) / 2 * 2;
    ensure!(width >= 2 && height >= 2, "实况短片比例无法生成有效画布");
    Ok((width, height))
}

#[allow(clippy::too_many_arguments)]
async fn normalize_live_clips(
    inputs: Vec<TempMedia>,
    info: &[ClipInfo],
    directory: &Path,
    ffmpeg: &str,
    ffprobe: &str,
    max_bytes: u64,
    max_temp_bytes: u64,
    deadline: Instant,
) -> Result<(Vec<TempMedia>, Vec<ClipInfo>)> {
    let (width, height) = normalization_canvas(info)?;
    let filter = format!(
        "scale=w='min(iw,{width})':h='min(ih,{height})':force_original_aspect_ratio=decrease:force_divisible_by=2,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30"
    );
    let mut converted = Vec::with_capacity(inputs.len());
    let mut converted_info = Vec::with_capacity(inputs.len());
    let mut converted_bytes = 0;
    for (input, original) in inputs.into_iter().zip(info) {
        let remaining = max_bytes.saturating_sub(converted_bytes);
        ensure!(remaining > 0, "实况转换文件总量超过大小上限");
        let mut output = TempMedia::reserve(directory, remaining, max_temp_bytes)?;
        let args = vec![
            "-nostdin".into(),
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-y".into(),
            "-max_alloc".into(),
            "16777216".into(),
            "-threads".into(),
            "1".into(),
            "-filter_threads".into(),
            "1".into(),
            "-protocol_whitelist".into(),
            "file".into(),
            "-f".into(),
            "mov".into(),
            "-i".into(),
            input.path().to_string_lossy().into_owned(),
            "-map".into(),
            "0:v:0".into(),
            "-map".into(),
            "0:a:0?".into(),
            "-map_metadata".into(),
            "-1".into(),
            "-vf".into(),
            filter.clone(),
            "-c:v".into(),
            "libx264".into(),
            "-threads".into(),
            "1".into(),
            "-preset".into(),
            "ultrafast".into(),
            "-crf".into(),
            "23".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-c:a".into(),
            "copy".into(),
            "-video_track_timescale".into(),
            "15360".into(),
            "-fs".into(),
            remaining.to_string(),
            "-movflags".into(),
            "+faststart".into(),
            "-f".into(),
            "mp4".into(),
            output.path().to_string_lossy().into_owned(),
        ];
        spawn_capture(ffmpeg, &args, remaining_time(deadline)?)
            .await
            .context("FFmpeg 实况短片规格统一失败")?;
        output.finish()?;
        let normalized = inspect_clip(ffprobe, output.path(), deadline).await?;
        ensure!(
            (normalized.duration - original.duration).abs() <= 0.15,
            "实况短片转换后时长异常"
        );
        if let Some(first) = converted_info.first() {
            let first: &ClipInfo = first;
            ensure!(
                first.streams == normalized.streams,
                "实况短片转换后的流参数不一致"
            );
        }
        converted_bytes += output.size();
        converted.push(output);
        converted_info.push(normalized);
        drop(input);
    }
    Ok((converted, converted_info))
}

fn remaining_time(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "实况图集处理超时");
    Ok(remaining)
}

fn ffprobe_for(ffmpeg: &str) -> String {
    let filename = if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    };
    let candidate = Path::new(ffmpeg.trim()).with_file_name(filename);
    if candidate.is_file() {
        return candidate.to_string_lossy().into_owned();
    }
    filename.into()
}

async fn inspect_clip(ffprobe: &str, file: &Path, deadline: Instant) -> Result<ClipInfo> {
    let args = vec![
        "-v".into(),
        "error".into(),
        "-max_alloc".into(),
        "16777216".into(),
        "-protocol_whitelist".into(),
        "file".into(),
        "-f".into(),
        "mov".into(),
        "-show_data_hash".into(),
        "sha256".into(),
        "-show_entries".into(),
        concat!(
            "stream=codec_type,codec_name,codec_tag_string,profile,level,width,height,pix_fmt,",
            "time_base,r_frame_rate,sample_aspect_ratio,sample_fmt,sample_rate,channels,",
            "channel_layout,extradata_hash:stream_disposition=attached_pic:stream_side_data=rotation:",
            "stream_tags=rotate:format=duration"
        ).into(),
        "-of".into(),
        "json".into(),
        file.to_string_lossy().into_owned(),
    ];
    let output = spawn_capture(ffprobe, &args, remaining_time(deadline)?)
        .await
        .context("无法用 ffprobe 检查实况短片")?;
    let data: Value = serde_json::from_str(&output).context("无法识别实况短片的媒体信息")?;
    let streams = data["streams"].as_array().context("实况短片缺少媒体流")?;
    let duration = data
        .pointer("/format/duration")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .context("实况短片时长无效")?;
    let mut video_tracks = 0;
    let mut audio_tracks = 0;
    for stream in streams {
        match (stream["codec_type"].as_str(), stream["codec_name"].as_str()) {
            (Some("video"), Some("h264"))
                if stream
                    .pointer("/disposition/attached_pic")
                    .and_then(Value::as_u64)
                    == Some(0)
                    && matches!(stream["pix_fmt"].as_str(), Some("yuv420p" | "yuvj420p"))
                    && stream["width"].as_u64().is_some_and(|width| width > 0)
                    && stream["height"].as_u64().is_some_and(|height| height > 0) =>
            {
                video_tracks += 1;
            }
            (Some("audio"), Some("aac")) => audio_tracks += 1,
            _ => bail!("实况短片包含无法直接合并的媒体流"),
        }
        ensure!(
            stream["extradata_hash"]
                .as_str()
                .is_some_and(|value| value.starts_with("SHA256:")),
            "无法确认实况短片的编码参数"
        );
    }
    ensure!(
        video_tracks == 1 && audio_tracks <= 1,
        "实况短片音视频轨道不兼容"
    );
    Ok(ClipInfo {
        streams: streams.clone(),
        duration,
    })
}

/// Download each live slide in source order, then join compatible MP4 streams.
/// Both input and output file sizes count towards the shared temporary disk budget.
pub async fn download_live_album_temp(data: &ParsedData, config: &AppConfig) -> Result<TempMedia> {
    let deadline = Instant::now()
        + Duration::from_millis(config.output.video_download_timeout.clamp(1, 300_000));
    let clips = &data.live_photo;
    ensure!(!clips.is_empty(), "实况图集没有可用短片");
    ensure!(
        clips.len() <= MAX_LIVE_ALBUM_CLIPS,
        "实况图集超过 {MAX_LIVE_ALBUM_CLIPS} 段合并上限"
    );
    let ffmpeg = &config.output.ffmpeg_command;
    ensure!(!ffmpeg.trim().is_empty(), "实况图集需要 FFmpeg 和 ffprobe");
    let ffprobe = ffprobe_for(ffmpeg);
    let max_temp_bytes = config.resources.max_temp_bytes;
    let max_bytes = max_video_bytes(config)
        .min(MAX_LIVE_ALBUM_BYTES)
        .min(max_temp_bytes);
    let directory = std::path::absolute(&config.output.temp_dir)?;
    std::fs::create_dir_all(&directory)?;
    let workspace = if clips.len() > 1 {
        Some(
            tempfile::Builder::new()
                .prefix("live-album-")
                .tempdir_in(&directory)?,
        )
    } else {
        None
    };
    let input_directory = workspace
        .as_ref()
        .map_or(directory.as_path(), |dir| dir.path());
    let proxies = [config.api.proxy.clone(), config.ytdlp.proxy.clone()];
    let mut inputs = Vec::with_capacity(clips.len());
    let mut input_bytes = 0;
    let mut clip_info = Vec::with_capacity(clips.len());
    for (index, clip) in clips.iter().enumerate() {
        let url = Url::parse(&clip.video).context("实况短片链接格式错误")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none(),
            "实况短片需要 HTTP(S) 播放地址"
        );
        let remaining_bytes = max_bytes - input_bytes;
        ensure!(remaining_bytes > 0, "实况短片下载总量超过上限");
        let input = tokio::time::timeout_at(
            deadline,
            download_media_temp(
                url.as_str(),
                &proxies,
                &data.media_headers,
                input_directory,
                remaining_bytes,
                max_temp_bytes,
                remaining_time(deadline)?,
                None,
            ),
        )
        .await
        .context("实况短片下载超过总时限")?
        .with_context(|| format!("第 {} 段实况下载失败", index + 1))?;
        input_bytes += input.size();
        let info = inspect_clip(&ffprobe, input.path(), deadline)
            .await
            .with_context(|| format!("第 {} 段实况校验失败", index + 1))?;
        if clips.len() == 1 {
            return Ok(input);
        }
        clip_info.push(info);
        inputs.push(input);
    }

    if clip_info
        .iter()
        .any(|info| info.streams != clip_info[0].streams)
    {
        (inputs, clip_info) = normalize_live_clips(
            inputs,
            &clip_info,
            input_directory,
            ffmpeg,
            &ffprobe,
            max_bytes,
            max_temp_bytes,
            deadline,
        )
        .await?;
    }
    let total_duration = clip_info.iter().map(|info| info.duration).sum::<f64>();
    let input_bytes = inputs.iter().map(TempMedia::size).sum::<u64>();

    let mut manifest = TempMedia::reserve(input_directory, CONCAT_MANIFEST_BYTES, max_temp_bytes)?;
    let mut entries = String::from("ffconcat version 1.0\n");
    for input in &inputs {
        let filename = input
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .context("临时片段文件名无效")?;
        entries.push_str(&format!("file '{filename}'\n"));
    }
    std::fs::write(manifest.path(), entries)?;
    manifest.finish()?;
    let output_limit = max_bytes.min(max_temp_bytes.saturating_sub(input_bytes + manifest.size()));
    let mut output = TempMedia::reserve(&directory, output_limit, max_temp_bytes)?;
    let args = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-max_alloc".into(),
        "16777216".into(),
        "-protocol_whitelist".into(),
        "file".into(),
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "1".into(),
        "-i".into(),
        manifest.path().to_string_lossy().into_owned(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
        "-fs".into(),
        output_limit.to_string(),
        "-movflags".into(),
        "+faststart".into(),
        "-f".into(),
        "mp4".into(),
        output.path().to_string_lossy().into_owned(),
    ];
    spawn_capture(ffmpeg, &args, remaining_time(deadline)?)
        .await
        .context("FFmpeg 实况图集无损合并失败")?;
    output.finish()?;
    let merged = inspect_clip(&ffprobe, output.path(), deadline)
        .await
        .context("合并输出校验失败")?;
    ensure!(
        merged.streams.len() == clip_info[0].streams.len()
            && (merged.duration - total_duration).abs() <= (clips.len() as f64 * 0.12).max(0.5),
        "实况合并结果音轨或时长异常"
    );
    Ok(output)
}
