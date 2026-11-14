use std::path::Path;
use std::time::Duration;

use axum::extract::{Path as RoutePath, State};
use axum::http::{StatusCode, header};
use axum::routing::get;
use serde_json::Value;
use videorust::parser::process::spawn_capture;
use videorust::parser::remux::download_parsed_temp;
use videorust::types::{AppConfig, ParsedData};

async fn ffmpeg(args: Vec<String>) {
    spawn_capture("ffmpeg", &args, Duration::from_secs(30))
        .await
        .unwrap();
}

async fn streams(path: &Path) -> Vec<String> {
    let output = spawn_capture(
        "ffprobe",
        &[
            "-v".into(),
            "error".into(),
            "-show_entries".into(),
            "stream=codec_type".into(),
            "-of".into(),
            "json".into(),
            path.to_string_lossy().into_owned(),
        ],
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    let data: Value = serde_json::from_str(&output).unwrap();
    data["streams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|stream| stream["codec_type"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
#[ignore = "requires local ffmpeg and ffprobe"]
async fn remuxes_http_dash_and_hls_with_audio_and_cleans_outputs() {
    let source = tempfile::tempdir().unwrap();
    let full = source.path().join("full.mp4");
    ffmpeg(vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "color=c=blue:s=160x90:r=15".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "sine=frequency=440:sample_rate=44100".into(),
        "-t".into(),
        "1.5".into(),
        "-c:v".into(),
        "libx264".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
        "-movflags".into(),
        "+faststart".into(),
        full.to_string_lossy().into_owned(),
    ])
    .await;
    for (name, mapping) in [("video.mp4", "0:v"), ("audio.m4a", "0:a")] {
        ffmpeg(vec![
            "-v".into(),
            "error".into(),
            "-i".into(),
            full.to_string_lossy().into_owned(),
            "-map".into(),
            mapping.into(),
            "-c".into(),
            "copy".into(),
            source.path().join(name).to_string_lossy().into_owned(),
        ])
        .await;
    }
    ffmpeg(vec![
        "-v".into(),
        "error".into(),
        "-i".into(),
        full.to_string_lossy().into_owned(),
        "-c".into(),
        "copy".into(),
        "-hls_time".into(),
        "1".into(),
        "-hls_list_size".into(),
        "0".into(),
        source
            .path()
            .join("playlist.m3u8")
            .to_string_lossy()
            .into_owned(),
    ])
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route("/{name}", get(|State(root): State<std::path::PathBuf>, RoutePath(name): RoutePath<String>| async move {
        let content_type = if name.ends_with(".m3u8") { "application/vnd.apple.mpegurl" } else { "application/octet-stream" };
        match tokio::fs::read(root.join(name)).await {
            Ok(bytes) => Ok(([(header::CONTENT_TYPE, content_type)], bytes)),
            Err(_) => Err(StatusCode::NOT_FOUND),
        }
    })).with_state(source.path().to_path_buf());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let target = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.output.temp_dir = target.path().to_string_lossy().into_owned();
    config.output.max_video_size = 1;
    config.resources.max_temp_bytes = 2 * 1024 * 1024;
    let dash = ParsedData {
        media_type: "video".into(),
        video: format!("{base}/video.mp4"),
        audio_url: Some(format!("{base}/audio.m4a")),
        ..ParsedData::default()
    };
    let file = download_parsed_temp(&dash, &config).await.unwrap();
    assert_eq!(streams(file.path()).await, ["video", "audio"]);
    drop(file);
    let hls = ParsedData {
        media_type: "video".into(),
        video: format!("{base}/playlist.m3u8"),
        ..ParsedData::default()
    };
    let file = download_parsed_temp(&hls, &config).await.unwrap();
    assert_eq!(streams(file.path()).await, ["video", "audio"]);
    drop(file);
    config.output.ffmpeg_command = "allvideo-test-missing-ffmpeg".into();
    assert!(download_parsed_temp(&hls, &config).await.is_err());
    assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
    server.abort();
}
