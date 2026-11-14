use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{delete, get, post, put};
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

#[derive(Clone)]
struct GalleryMock {
    base: String,
    images: Arc<Vec<ImageFixture>>,
    source_reads: Arc<Mutex<Vec<usize>>>,
    messages: Arc<Mutex<Vec<Value>>>,
    prepares: Arc<Mutex<Vec<Value>>>,
    puts: Arc<Mutex<Vec<usize>>>,
    completions: Arc<Mutex<Vec<usize>>>,
    deletions: Arc<Mutex<Vec<String>>>,
    lifecycle: Arc<Mutex<Vec<String>>>,
    directory: Arc<tempfile::TempDir>,
    failed_complete: Option<usize>,
    markdown_error: Option<i64>,
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
    if body["file_size"].as_u64() != Some(state.images[index].bytes.len() as u64)
        || body["content_type"] != "image/png"
        || body["file_name"].as_str().is_none_or(str::is_empty)
        || !body["sha1"].as_str().is_some_and(|digest| {
            digest.len() == 40 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    state.prepares.lock().await.push(body);
    state
        .lifecycle
        .lock()
        .await
        .push(format!("prepare-{index}"));
    Ok(Json(json!({
        "upload_id":format!("gallery-{index}"),
        "presigned_url":format!("{}/material-put/{index}", state.base),
        "expires_in":3600,
    })))
}

async fn material_put(
    State(state): State<GalleryMock>,
    Path(index): Path<usize>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<[(header::HeaderName, String); 1], StatusCode> {
    let image = state.images.get(index).ok_or(StatusCode::NOT_FOUND)?;
    if headers.contains_key(header::AUTHORIZATION)
        || headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            != Some("image/png")
        || headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            != Some(image.bytes.len())
        || bytes.as_ref() != image.bytes.as_slice()
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    state.puts.lock().await.push(index);
    state.lifecycle.lock().await.push(format!("put-{index}"));
    Ok([(header::ETAG, format!("\"{}\"", image.md5))])
}

async fn upload_complete(
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
    if body["md5"] != state.images[index].md5 {
        return Err(StatusCode::BAD_REQUEST);
    }
    state.completions.lock().await.push(index);
    state
        .lifecycle
        .lock()
        .await
        .push(format!("complete-{index}"));
    if state.failed_complete == Some(index) {
        return Ok(Json(
            json!({"err_code":40035004,"message":"测试素材完成失败"}),
        ));
    }
    Ok(Json(json!({
        "image_id":format!("gallery-{index}"),
        "url":format!("qqbot-media://image/v1/mock-{index}"),
        "ttl":7776000,
    })))
}

async fn remove_material(
    State(state): State<GalleryMock>,
    Path(id): Path<String>,
) -> Result<Json<Value>, StatusCode> {
    let index = id
        .strip_prefix("gallery-")
        .and_then(|index| index.parse::<usize>().ok())
        .filter(|index| *index < state.images.len())
        .ok_or(StatusCode::BAD_REQUEST)?;
    if state.failed_complete == Some(index) {
        return Err(StatusCode::BAD_REQUEST);
    }
    state.deletions.lock().await.push(id.clone());
    state.lifecycle.lock().await.push(format!("delete-{index}"));
    Ok(Json(json!({})))
}

async fn send_message(State(state): State<GalleryMock>, Json(body): Json<Value>) -> Json<Value> {
    let markdown = body
        .pointer("/markdown/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_image_markdown = markdown.contains("![");
    let error_code = state.markdown_error.filter(|_| is_image_markdown);
    state.lifecycle.lock().await.push(if is_image_markdown {
        "markdown".into()
    } else {
        "text".into()
    });
    state.messages.lock().await.push(body);
    if let Some(code) = error_code {
        return Json(json!({"err_code":code,"message":"测试消息错误"}));
    }
    Json(json!({"id":"gallery-sent"}))
}

async fn start_gallery_mock(
    failed_complete: Option<usize>,
    markdown_error: Option<i64>,
    max_images: usize,
) -> (GalleryMock, Arc<AppState>, tokio::task::JoinHandle<()>) {
    start_gallery_mock_with_count(failed_complete, markdown_error, max_images, 9).await
}

async fn start_gallery_mock_with_count(
    failed_complete: Option<usize>,
    markdown_error: Option<i64>,
    max_images: usize,
    image_count: usize,
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
                .cycle()
                .take(image_count)
                .enumerate()
                .map(|(index, (width, height))| png_image(width, height, index as u8))
                .collect(),
        ),
        source_reads: Arc::new(Mutex::new(Vec::new())),
        messages: Arc::new(Mutex::new(Vec::new())),
        prepares: Arc::new(Mutex::new(Vec::new())),
        puts: Arc::new(Mutex::new(Vec::new())),
        completions: Arc::new(Mutex::new(Vec::new())),
        deletions: Arc::new(Mutex::new(Vec::new())),
        lifecycle: Arc::new(Mutex::new(Vec::new())),
        directory: Arc::new(tempfile::tempdir().unwrap()),
        failed_complete,
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
                    "description":format!("{} 张混合比例图片",state.images.len()),
                    "coverUrl":format!("{}/source/cover-must-not-be-downloaded",state.base),
                    "picInfo":images,
                }}}))
            }),
        )
        .route("/source/{index}", get(source_image))
        .route("/v2/media-images/prepare", post(upload_prepare))
        .route("/material-put/{index}", put(material_put))
        .route("/v2/media-images/complete", post(upload_complete))
        .route("/v2/media-images/{id}/delete", delete(remove_material))
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
    config.output.temp_dir = state.directory.path().to_str().unwrap().into();
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

async fn assert_materials_removed_after_reply(mock: &GalleryMock, expected: &[usize]) {
    let mut deleted = mock.deletions.lock().await.clone();
    deleted.sort();
    let mut expected_ids = expected
        .iter()
        .map(|index| format!("gallery-{index}"))
        .collect::<Vec<_>>();
    expected_ids.sort();
    assert_eq!(
        deleted, expected_ids,
        "only this batch's owned IDs are deleted"
    );
    let lifecycle = mock.lifecycle.lock().await;
    let reply = lifecycle
        .iter()
        .position(|event| event == "markdown")
        .unwrap();
    for index in expected {
        let complete = lifecycle
            .iter()
            .position(|event| event == &format!("complete-{index}"))
            .unwrap();
        let delete = lifecycle
            .iter()
            .position(|event| event == &format!("delete-{index}"))
            .unwrap();
        assert!(
            complete < reply && reply < delete,
            "material {index} must survive until the reply attempt"
        );
    }
    let ledger: Value = serde_json::from_slice(
        &std::fs::read(
            mock.directory
                .path()
                .join(".allvideo-markdown-materials.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        ledger["entries"],
        json!([]),
        "no completed batch materials remain owned"
    );
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
        assert!(markdown.contains(&format!("qqbot-media://image/v1/mock-{index}")));
    }
    assert!(!markdown.contains("/source/"));
    assert!(messages.iter().all(|message| message["msg_type"] != 7));
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
    assert_eq!(mock.puts.lock().await.len(), 9);
    assert_eq!(mock.completions.lock().await.len(), 9);
    drop(reads);
    assert_materials_removed_after_reply(&mock, &(0..9).collect::<Vec<_>>()).await;
    deliver(app, "gallery-duplicate").await;
    assert_eq!(
        mock.messages.lock().await.len(),
        2,
        "successful gallery commits deduplication"
    );
    server.abort();
}

#[tokio::test]
async fn thirteen_images_fit_the_larger_limit_in_one_complete_markdown() {
    let (mock, app, server) = start_gallery_mock_with_count(None, None, 30, 13).await;
    deliver(app, "gallery-thirteen").await;

    let messages = mock.messages.lock().await.clone();
    assert_eq!(
        messages.len(),
        2,
        "one waiting message and one complete gallery reply"
    );
    assert_eq!(messages[0]["msg_type"], 0);
    assert_eq!(messages[1]["msg_type"], 2);
    assert_eq!(messages[1]["markdown"]["force_verify_image_resource"], true);
    assert_eq!(messages[1]["msg_id"], "gallery-thirteen");
    assert_eq!(messages[1]["msg_seq"], 2);
    let markdown = messages[1]["markdown"]["content"].as_str().unwrap();
    assert_eq!(markdown.matches("![").count(), 13);
    assert_eq!(markdown.matches("qqbot-media://image/v1/").count(), 13);
    for index in 0..13 {
        assert_eq!(
            markdown
                .matches(&format!("](qqbot-media://image/v1/mock-{index})"))
                .count(),
            1,
            "image {index} must appear exactly once"
        );
    }
    assert!(!markdown.contains("受配置上限"));
    assert!(!markdown.contains("未显示"));
    assert!(!markdown.contains("失败"));
    assert!(!markdown.contains("/source/"));
    assert!(messages.iter().all(|message| message["msg_type"] != 7));

    let expected = (0..13).collect::<Vec<_>>();
    assert_eq!(*mock.source_reads.lock().await, expected);
    assert_eq!(mock.prepares.lock().await.len(), 13);
    assert_eq!(*mock.puts.lock().await, expected);
    assert_eq!(*mock.completions.lock().await, expected);
    assert_materials_removed_after_reply(&mock, &expected).await;
    server.abort();
}

#[tokio::test]
async fn failed_material_completion_is_visible_and_preserves_other_gallery_images() {
    let (mock, app, server) = start_gallery_mock(Some(4), None, 9).await;
    deliver(Arc::clone(&app), "gallery-partial").await;
    let messages = mock.messages.lock().await.clone();
    assert_eq!(messages.len(), 2);
    let markdown = messages[1]["markdown"]["content"].as_str().unwrap();
    assert_eq!(markdown.matches("![").count(), 8);
    assert!(!markdown.contains("qqbot-media://image/v1/mock-4"));
    let compact = markdown.split_whitespace().collect::<String>();
    assert!(compact.contains("1张"));
    assert!(compact.contains("失败") || compact.contains("未发送"));
    assert!(messages.iter().all(|message| message["msg_type"] != 7));
    let completed_indices = (0..9).filter(|index| *index != 4).collect::<Vec<_>>();
    assert_materials_removed_after_reply(&mock, &completed_indices).await;
    assert_eq!(mock.prepares.lock().await.len(), 9);
    assert_eq!(mock.puts.lock().await.len(), 9);
    assert_eq!(mock.completions.lock().await.len(), 9);
    deliver(app, "gallery-partial-retry").await;
    assert_eq!(
        mock.messages.lock().await.len(),
        4,
        "partial delivery releases its deduplication claim"
    );
    assert_eq!(mock.prepares.lock().await.len(), 18);
    assert_eq!(mock.deletions.lock().await.len(), 16);
    server.abort();
}

#[tokio::test]
async fn failed_markdown_material_reply_reports_undelivered_images_without_url_retry() {
    for error_code in [304003, 304010, 40034004, 40034141] {
        let (mock, app, server) = start_gallery_mock(None, Some(error_code), 9).await;
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
            1,
            "dedicated material references must not retry failed HTTP URLs for {error_code}"
        );
        assert_eq!(
            messages.len(),
            3,
            "waiting, one material gallery attempt and one visible failure"
        );
        assert!(messages.iter().all(|message| message["msg_type"] != 7));
        let notice = messages.last().unwrap()["content"].as_str().unwrap();
        assert!(notice.contains('9'));
        assert!(notice.contains("图片") || notice.contains("图集"));
        assert!(notice.contains("失败") || notice.contains("未发送"));
        assert!(notice.contains(&error_code.to_string()));
        assert_eq!(
            mock.prepares.lock().await.len(),
            9,
            "each image is prepared once"
        );
        assert_eq!(mock.source_reads.lock().await.len(), 9);
        assert_materials_removed_after_reply(&mock, &(0..9).collect::<Vec<_>>()).await;
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
    assert_eq!(mock.puts.lock().await.len(), 3);
    assert_eq!(mock.completions.lock().await.len(), 3);
    assert_materials_removed_after_reply(&mock, &[0, 1, 2]).await;
    server.abort();
}

#[tokio::test]
async fn rejected_material_gallery_releases_deduplication_for_later_retry() {
    let (mock, app, server) = start_gallery_mock(None, Some(40034141), 9).await;
    deliver(Arc::clone(&app), "gallery-material-error").await;
    let messages = mock.messages.lock().await.clone();
    assert_eq!(
        messages.len(),
        3,
        "waiting, material gallery and visible failure"
    );
    assert_eq!(messages[0]["msg_type"], 0);
    assert_eq!(messages[1]["msg_type"], 2);
    assert_eq!(messages[1]["markdown"]["force_verify_image_resource"], true);
    let markdown = messages[1]["markdown"]["content"].as_str().unwrap();
    assert_eq!(markdown.matches("![").count(), 9);
    for index in 0..9 {
        assert!(markdown.contains(&format!("qqbot-media://image/v1/mock-{index}")));
    }
    assert!(!markdown.contains("/source/"));
    assert_eq!(mock.prepares.lock().await.len(), 9);
    assert_eq!(mock.source_reads.lock().await.len(), 9);
    assert_materials_removed_after_reply(&mock, &(0..9).collect::<Vec<_>>()).await;
    deliver(app, "gallery-material-later-retry").await;
    assert_eq!(
        mock.messages.lock().await.len(),
        6,
        "explicit rejection releases deduplication so the same gallery can be retried"
    );
    assert_eq!(mock.prepares.lock().await.len(), 18);
    assert_eq!(mock.deletions.lock().await.len(), 18);
    server.abort();
}

#[tokio::test]
async fn non_image_api_error_does_not_retry_the_entire_gallery() {
    let (mock, app, server) = start_gallery_mock(None, Some(500), 9).await;
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
    assert_materials_removed_after_reply(&mock, &(0..9).collect::<Vec<_>>()).await;
    server.abort();
}
