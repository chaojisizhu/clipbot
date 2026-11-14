use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use videorust::bot::qqbot::QQBotClient;
use videorust::parser::api::VideoParserService;
use videorust::service::AppState;
use videorust::types::{AppConfig, ChatType, QQChatEvent};

struct ImageFixture {
    bytes: Vec<u8>,
    md5: String,
}

#[derive(Clone, Copy)]
enum MarkdownFailure {
    Always(i64),
    RawUrlsOnly(i64),
}

#[derive(Clone)]
struct GalleryMock {
    base: String,
    images: Arc<Vec<ImageFixture>>,
    source_reads: Arc<Mutex<Vec<usize>>>,
    messages: Arc<Mutex<Vec<Value>>>,
    prepares: Arc<Mutex<Vec<Value>>>,
    missing_raw_url: Option<usize>,
    markdown_error: Option<MarkdownFailure>,
}

fn png_image(width: u32, height: u32, value: u8) -> ImageFixture {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&vec![value; width as usize * height as usize])
            .unwrap();
    }
    ImageFixture {
        md5: format!("{:x}", md5::compute(&bytes)),
        bytes,
    }
}

async fn source_image(
    State(state): State<GalleryMock>,
    Path(index): Path<usize>,
) -> Result<([(header::HeaderName, &'static str); 1], Bytes), StatusCode> {
    let image = state.images.get(index).ok_or(StatusCode::NOT_FOUND)?;
    state.source_reads.lock().await.push(index);
    Ok((
        [(header::CONTENT_TYPE, "image/png")],
        Bytes::copy_from_slice(&image.bytes),
    ))
}

async fn transferred_image(
    State(state): State<GalleryMock>,
    Path(index): Path<usize>,
) -> Result<([(header::HeaderName, &'static str); 1], Bytes), StatusCode> {
    let image = state.images.get(index).ok_or(StatusCode::NOT_FOUND)?;
    Ok((
        [(header::CONTENT_TYPE, "image/png")],
        Bytes::copy_from_slice(&image.bytes),
    ))
}

async fn upload_prepare(
    State(state): State<GalleryMock>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let digest = body.get("md5").and_then(Value::as_str).unwrap_or_default();
    let index = state
        .images
        .iter()
        .position(|image| image.md5 == digest)
        .ok_or(StatusCode::BAD_REQUEST)?;
    state.prepares.lock().await.push(body);
    Ok(Json(
        json!({"upload_id":format!("gallery-{index}"),"parts":[]}),
    ))
}

async fn uploaded_file(
    State(state): State<GalleryMock>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let index = body
        .get("upload_id")
        .and_then(Value::as_str)
        .and_then(|id| id.strip_prefix("gallery-"))
        .and_then(|index| index.parse::<usize>().ok())
        .ok_or(StatusCode::BAD_REQUEST)?;
    if state.images.get(index).is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut uploaded = json!({
        "file_info":format!("gallery-file-{index}"),
        "file_uuid":format!("gallery-uuid-{index}"),
        "ttl":86400,
    });
    if state.missing_raw_url != Some(index) {
        uploaded["raw_url"] = Value::String(format!("{}/qq-image/{index}", state.base));
    }
    Ok(Json(uploaded))
}

async fn send_message(State(state): State<GalleryMock>, Json(body): Json<Value>) -> Json<Value> {
    let markdown = body
        .pointer("/markdown/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_image_markdown = markdown.contains("![");
    let error_code = match state.markdown_error.filter(|_| is_image_markdown) {
        Some(MarkdownFailure::Always(code)) => Some(code),
        Some(MarkdownFailure::RawUrlsOnly(code)) if markdown.contains("/qq-image/") => Some(code),
        _ => None,
    };
    state.messages.lock().await.push(body);
    if let Some(code) = error_code {
        return Json(json!({"err_code":code,"message":"测试消息错误"}));
    }
    Json(json!({"id":"gallery-sent"}))
}

async fn start_gallery_mock(
    missing_raw_url: Option<usize>,
    markdown_error: Option<MarkdownFailure>,
    max_images: usize,
) -> (GalleryMock, Arc<AppState>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let dimensions = [
        (1200, 600),
        (600, 1200),
        (100, 5000),
        (300, 300),
        (1920, 1080),
        (640, 360),
        (480, 640),
        (200, 80),
        (80, 200),
    ];
    let state = GalleryMock {
        base: base.clone(),
        images: Arc::new(
            dimensions
                .into_iter()
                .enumerate()
                .map(|(index, (width, height))| png_image(width, height, index as u8))
                .collect(),
        ),
        source_reads: Arc::new(Mutex::new(Vec::new())),
        messages: Arc::new(Mutex::new(Vec::new())),
        prepares: Arc::new(Mutex::new(Vec::new())),
        missing_raw_url,
        markdown_error,
    };
    let app = Router::new()
        .route(
            "/app/getAppAccessToken",
            post(|| async { Json(json!({"access_token":"GALLERY_TEST_TOKEN","expires_in":7200})) }),
        )
        .route(
            "/api/weixin/get_parse_result",
            post(|| async {
                Json(json!({"code":0,"data":{"playable_url":"https://channels.weixin.qq.com/feed?token=fixture&eid=fixture"}}))
            }),
        )
        .route(
            "/finder-preview/api/feed/get_feed_info",
            post(|State(state): State<GalleryMock>| async move {
                let images = (0..state.images.len())
                    .map(|index| json!({"url":format!("{}/source/{index}",state.base)}))
                    .collect::<Vec<_>>();
                Json(json!({"data":{"feedInfo":{
                    "description":"九张混合比例图片",
                    "coverUrl":format!("{}/source/cover-must-not-be-downloaded",state.base),
                    "picInfo":images,
                }}}))
            }),
        )
        .route("/source/{index}", get(source_image))
        .route("/qq-image/{index}", get(transferred_image))
        .route("/v2/groups/{group}/upload_prepare", post(upload_prepare))
        .route("/v2/groups/{group}/files", post(uploaded_file))
        .route("/v2/groups/{group}/messages", post(send_message))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut config = AppConfig::default();
    config.qqbot.app_id = "gallery-app".into();
    config.qqbot.app_secret = "gallery-secret".into();
    config.qqbot.api_base = base.clone();
    config.qqbot.show_waiting_tip = true;
    config.qqbot.parse_miniapp_cards = false;
    config.channels.yuanbao_cookie = "fixture-only=1".into();
    config.channels.api_base = Some(base);
    config.output.max_markdown_images = max_images;
    config.output.show_image_text = true;
    config.output.show_cover = true;
    config.output.deduplication_interval = 180;
    config.api.proxy.clear();
    config.ytdlp.enabled = false;
    config.gpt_detect.enabled = false;
    config.jmcomic.enabled = false;
    let config = Arc::new(config);
    let app_state = Arc::new(AppState::new(
        Arc::clone(&config),
        Arc::new(VideoParserService::new((*config).clone()).unwrap()),
        Arc::new(QQBotClient::new(config.qqbot.clone())),
    ));
    (state, app_state, server)
}

fn gallery_event(message_id: &str) -> QQChatEvent {
    QQChatEvent {
        chat_type: ChatType::Group,
        chat_id: "GALLERY_GROUP".into(),
        message_id: message_id.into(),
        text: "https://weixin.qq.com/sph/gallery-fixture".into(),
        ..QQChatEvent::default()
    }
}

async fn deliver(app: Arc<AppState>, message_id: &str) {
    tokio::time::timeout(
        Duration::from_secs(15),
        app.handle_message(gallery_event(message_id)),
    )
    .await
    .expect("gallery delivery timed out");
}

#[tokio::test]
async fn nine_images_share_one_markdown_with_real_sizes_and_no_native_image_messages() {
    let (mock, app, server) = start_gallery_mock(None, None, 9).await;
    deliver(Arc::clone(&app), "gallery-nine").await;
    let messages = mock.messages.lock().await.clone();
    assert_eq!(
        messages.len(),
        2,
        "one waiting message and one complete gallery reply"
    );
    assert_eq!(messages[0]["msg_type"], 0);
    assert_eq!(messages[1]["msg_type"], 2);
    assert_eq!(messages[1]["markdown"]["force_verify_image_resource"], true);
    let markdown = messages[1]["markdown"]["content"].as_str().unwrap();
    assert_eq!(markdown.matches("![").count(), 9);
    assert!(markdown.contains("#750px #375px"));
    assert!(markdown.contains("#600px #1200px"));
    assert!(markdown.contains("#100px #5000px"));
    for index in 0..9 {
        assert!(markdown.contains(&format!("{}/qq-image/{index}", mock.base)));
    }
    assert!(!markdown.contains("/source/"));
    assert_eq!(messages[1]["msg_seq"], 2);
    assert_eq!(messages[1]["msg_id"], "gallery-nine");
    let reads = mock.source_reads.lock().await;
    assert_eq!(reads.len(), 9);
    let mut counts = HashMap::new();
    for index in reads.iter() {
        *counts.entry(*index).or_insert(0usize) += 1;
    }
    assert_eq!(counts.len(), 9);
    assert!(counts.values().all(|count| *count == 1));
    assert_eq!(mock.prepares.lock().await.len(), 9);
    drop(reads);
    deliver(app, "gallery-duplicate").await;
    assert_eq!(
        mock.messages.lock().await.len(),
        2,
        "successful gallery commits deduplication"
    );
    server.abort();
}

#[tokio::test]
async fn missing_raw_url_is_visible_and_does_not_hide_the_other_gallery_images() {
    let (mock, app, server) = start_gallery_mock(Some(4), None, 9).await;
    deliver(Arc::clone(&app), "gallery-partial").await;
    let messages = mock.messages.lock().await.clone();
    assert_eq!(messages.len(), 2);
    let markdown = messages[1]["markdown"]["content"].as_str().unwrap();
    assert_eq!(markdown.matches("![").count(), 8);
    assert!(!markdown.contains(&format!("{}/qq-image/4", mock.base)));
    let compact = markdown.split_whitespace().collect::<String>();
    assert!(compact.contains("1张"));
    assert!(compact.contains("失败") || compact.contains("未发送"));
    assert!(messages.iter().all(|message| message["msg_type"] != 7));
    deliver(app, "gallery-partial-retry").await;
    assert_eq!(
        mock.messages.lock().await.len(),
        4,
        "partial delivery releases its deduplication claim"
    );
    server.abort();
}

#[tokio::test]
async fn failed_markdown_image_transfer_retries_once_then_reports_undelivered_images() {
    for error_code in [304003, 304010, 40034004, 40034141] {
        let (mock, app, server) =
            start_gallery_mock(None, Some(MarkdownFailure::Always(error_code)), 9).await;
        deliver(app, &format!("gallery-error-{error_code}")).await;
        let messages = mock.messages.lock().await.clone();
        let galleries = messages
            .iter()
            .filter(|body| {
                body.pointer("/markdown/content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.contains("!["))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            galleries.len(),
            2,
            "only one gallery retry is permitted for {error_code}"
        );
        assert_eq!(
            messages.len(),
            4,
            "waiting, two gallery attempts and one visible failure"
        );
        assert!(messages.iter().all(|message| message["msg_type"] != 7));
        let notice = messages.last().unwrap()["content"].as_str().unwrap();
        assert!(notice.contains('9'));
        assert!(notice.contains("图片") || notice.contains("图集"));
        assert!(notice.contains("失败") || notice.contains("未发送"));
        assert_eq!(
            mock.prepares.lock().await.len(),
            9,
            "retry reuses prepared gallery resources"
        );
        server.abort();
    }
}

#[tokio::test]
async fn configured_gallery_limit_reports_the_remaining_images() {
    let (mock, app, server) = start_gallery_mock(None, None, 3).await;
    deliver(app, "gallery-configured-limit").await;
    let messages = mock.messages.lock().await.clone();
    assert_eq!(messages.len(), 2);
    let markdown = messages[1]["markdown"]["content"].as_str().unwrap();
    assert_eq!(markdown.matches("![").count(), 3);
    assert!(markdown.contains('6'));
    assert!(markdown.contains("其余") || markdown.contains("未显示") || markdown.contains("上限"));
    assert_eq!(mock.source_reads.lock().await.len(), 3);
    assert_eq!(mock.prepares.lock().await.len(), 3);
    server.abort();
}

#[tokio::test]
async fn rejected_relay_urls_retry_once_with_full_source_gallery_and_commit_deduplication() {
    let (mock, app, server) =
        start_gallery_mock(None, Some(MarkdownFailure::RawUrlsOnly(40034141)), 9).await;
    deliver(Arc::clone(&app), "gallery-source-retry").await;
    let messages = mock.messages.lock().await.clone();
    assert_eq!(
        messages.len(),
        3,
        "waiting, relay gallery and successful source gallery"
    );
    assert_eq!(messages[0]["msg_type"], 0);
    for body in &messages[1..] {
        assert_eq!(body["msg_type"], 2);
        assert_eq!(body["markdown"]["force_verify_image_resource"], true);
        let markdown = body["markdown"]["content"].as_str().unwrap();
        assert_eq!(markdown.matches("![").count(), 9);
        assert!(markdown.contains("#750px #375px"));
        assert!(markdown.contains("#600px #1200px"));
        assert!(markdown.contains("#100px #5000px"));
    }
    let first = messages[1]["markdown"]["content"].as_str().unwrap();
    let retried = messages[2]["markdown"]["content"].as_str().unwrap();
    for index in 0..9 {
        assert!(first.contains(&format!("{}/qq-image/{index}", mock.base)));
        assert!(retried.contains(&format!("{}/source/{index}", mock.base)));
    }
    assert!(!first.contains("/source/"));
    assert!(!retried.contains("/qq-image/"));
    assert_eq!(mock.prepares.lock().await.len(), 9);
    assert_eq!(mock.source_reads.lock().await.len(), 9);
    deliver(app, "gallery-successful-source-dedup").await;
    assert_eq!(
        mock.messages.lock().await.len(),
        3,
        "successful source gallery commits deduplication"
    );
    server.abort();
}

#[tokio::test]
async fn non_image_api_error_does_not_retry_the_entire_gallery() {
    let (mock, app, server) = start_gallery_mock(None, Some(MarkdownFailure::Always(500)), 9).await;
    deliver(app, "gallery-other-api-error").await;
    let messages = mock.messages.lock().await.clone();
    assert_eq!(
        messages.len(),
        3,
        "waiting, one gallery attempt and visible failure"
    );
    let galleries = messages
        .iter()
        .filter(|body| {
            body.pointer("/markdown/content")
                .and_then(Value::as_str)
                .is_some_and(|content| content.contains("!["))
        })
        .collect::<Vec<_>>();
    assert_eq!(galleries.len(), 1);
    assert_eq!(
        galleries[0]["markdown"]["force_verify_image_resource"],
        true
    );
    assert!(messages.iter().all(|body| body["msg_type"] != 7));
    assert_eq!(messages.last().unwrap()["msg_type"], 0);
    let notice = messages.last().unwrap()["content"].as_str().unwrap();
    assert!(notice.contains('9'));
    assert!(notice.contains("失败") || notice.contains("未发送"));
    assert!(notice.contains("500"));
    assert_eq!(mock.prepares.lock().await.len(), 9);
    server.abort();
}
