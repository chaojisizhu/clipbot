use std::path::Path;
use std::time::Duration;

use axum::extract::{Path as RoutePath, State};
use axum::http::{StatusCode, header};
use axum::routing::get;
use clipbot::parser::process::spawn_capture;
use clipbot::parser::remux::download_live_album_temp;
use clipbot::types::{AppConfig, LivePhoto, ParsedData};

async fn create_clip(path: &Path, color: &str, dimensions: &str) {
    spawn_capture(
        "ffmpeg",
        &[
            "-v".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("color=c={color}:s={dimensions}:r=15"),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=440:sample_rate=44100".into(),
            "-t".into(),
            "1".into(),
            "-c:v".into(),
            "libx264".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-c:a".into(),
            "aac".into(),
            path.to_string_lossy().into_owned(),
        ],
        Duration::from_secs(15),
    )
    .await
    .unwrap();
}

async fn frame_digest(path: &Path, seconds: &str) -> String {
    let output = spawn_capture(
        "ffmpeg",
        &[
            "-v".into(),
            "error".into(),
            "-ss".into(),
            seconds.into(),
            "-i".into(),
            path.to_string_lossy().into_owned(),
            "-map".into(),
            "0:v:0".into(),
            "-frames:v".into(),
            "1".into(),
            "-f".into(),
            "framemd5".into(),
            "-".into(),
        ],
        Duration::from_secs(15),
    )
    .await
    .unwrap();
    output
        .lines()
        .find(|line| !line.starts_with('#'))
        .and_then(|line| line.rsplit(',').next())
        .unwrap()
        .trim()
        .into()
}

async fn frame_color(path: &Path, seconds: &str) -> [u8; 3] {
    let bytes = tokio::process::Command::new("ffmpeg")
        .args(["-v", "error", "-ss", seconds, "-i"])
        .arg(path)
        .args([
            "-vf",
            "crop=iw/2:ih/2:iw/4:ih/4,scale=1:1",
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .output()
        .await
        .unwrap();
    assert!(bytes.status.success());
    bytes.stdout.try_into().unwrap()
}

#[tokio::test]
#[ignore = "requires local ffmpeg and ffprobe with libx264, AAC and framemd5"]
async fn concatenates_live_clips_in_order_and_normalizes_mixed_dimensions() {
    let sources = tempfile::tempdir().unwrap();
    let red = sources.path().join("red.mp4");
    let blue = sources.path().join("blue.mp4");
    create_clip(&red, "red", "160x90").await;
    create_clip(&blue, "blue", "160x90").await;
    create_clip(&sources.path().join("wide.mp4"), "blue", "240x90").await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route(
            "/{name}",
            get(
                |State(root): State<std::path::PathBuf>, RoutePath(name): RoutePath<String>| async move {
                    let bytes = tokio::fs::read(root.join(name))
                        .await
                        .map_err(|_| StatusCode::NOT_FOUND)?;
                    Ok::<_, StatusCode>(([(header::CONTENT_TYPE, "video/mp4")], bytes))
                },
            ),
        )
        .with_state(sources.path().to_path_buf());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let destination = tempfile::tempdir().unwrap();
    let mut config = AppConfig::default();
    config.output.temp_dir = destination.path().to_string_lossy().into_owned();
    config.output.max_video_size = 1;
    config.resources.max_temp_bytes = 2 * 1024 * 1024;
    let mut data = ParsedData::empty("live_photo");
    data.live_photo = ["red.mp4", "blue.mp4"]
        .into_iter()
        .map(|name| LivePhoto {
            image: String::new(),
            video: format!("{base}/{name}"),
        })
        .collect();

    let merged = download_live_album_temp(&data, &config).await.unwrap();
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 1);
    assert_eq!(
        frame_digest(merged.path(), "0.5").await,
        frame_digest(&red, "0.5").await
    );
    assert_eq!(
        frame_digest(merged.path(), "1.5").await,
        frame_digest(&blue, "0.5").await
    );
    drop(merged);
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);

    data.live_photo[1].video = format!("{base}/wide.mp4");
    let normalized = download_live_album_temp(&data, &config).await.unwrap();
    let red_color = frame_color(normalized.path(), "0.5").await;
    let blue_color = frame_color(normalized.path(), "1.5").await;
    assert!(red_color[0] > 200 && red_color[1] < 40 && red_color[2] < 40);
    assert!(blue_color[2] > 200 && blue_color[0] < 40 && blue_color[1] < 40);
    drop(normalized);
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);

    config.output.video_download_timeout = 1;
    assert!(download_live_album_temp(&data, &config).await.is_err());
    assert_eq!(std::fs::read_dir(destination.path()).unwrap().count(), 0);
    server.abort();
}
