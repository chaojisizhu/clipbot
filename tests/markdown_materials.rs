use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post, put};
use axum::{Json, Router};
use clipbot::bot::qqbot::{QQBotClient, QQBotError};
use clipbot::types::QQBotConfig;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const IMAGE: &[u8] = b"test image bytes with a stable digest";
const COS_ETAG: &str = "0123456789abcdef0123456789abcdef";

#[derive(Clone, Debug)]
struct Call {
    method: &'static str,
    path: String,
    headers: HeaderMap,
    json: Value,
    bytes: Vec<u8>,
}

#[derive(Clone)]
struct MaterialMock {
    base: String,
    calls: Arc<Mutex<Vec<Call>>>,
    etag: Option<&'static str>,
    presigned_url: Option<&'static str>,
    material_url: &'static str,
    error_stage: Option<&'static str>,
}

async fn record(
    state: &MaterialMock,
    method: &'static str,
    path: String,
    headers: HeaderMap,
    json: Value,
    bytes: Vec<u8>,
) {
    state.calls.lock().await.push(Call {
        method,
        path,
        headers,
        json,
        bytes,
    });
}

async fn token(
    State(state): State<MaterialMock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(
        &state,
        "POST",
        "/app/getAppAccessToken".into(),
        headers,
        body,
        Vec::new(),
    )
    .await;
    Json(json!({"access_token":"MATERIAL_TEST_TOKEN","expires_in":7200}))
}

async fn prepare(
    State(state): State<MaterialMock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(
        &state,
        "POST",
        "/v2/media-images/prepare".into(),
        headers,
        body,
        Vec::new(),
    )
    .await;
    if state.error_stage == Some("prepare") {
        return Json(json!({"err_code":40035007,"message":"素材数量超限"}));
    }
    Json(json!({
        "upload_id":"material-upload-1",
        "presigned_url":state.presigned_url.map(str::to_owned).unwrap_or_else(||format!("{}/cos",state.base)),
        "expires_in":3600
    }))
}

async fn cos(State(state): State<MaterialMock>, headers: HeaderMap, bytes: Bytes) -> Response {
    record(
        &state,
        "PUT",
        "/cos".into(),
        headers,
        Value::Null,
        bytes.to_vec(),
    )
    .await;
    let mut response = StatusCode::OK.into_response();
    if let Some(etag) = state.etag {
        response
            .headers_mut()
            .insert(header::ETAG, HeaderValue::from_static(etag));
    }
    response
}

async fn complete(
    State(state): State<MaterialMock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(
        &state,
        "POST",
        "/v2/media-images/complete".into(),
        headers,
        body,
        Vec::new(),
    )
    .await;
    if state.error_stage == Some("complete") {
        return Json(json!({"err_code":40035003,"message":"素材上传任务无效"}));
    }
    Json(json!({
        "image_id":"102083127_material-1",
        "url":state.material_url,
        "ttl":7776000,
        "preview_url":"https://preview.example/unused"
    }))
}

async fn remove(
    State(state): State<MaterialMock>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Json<Value> {
    record(
        &state,
        "DELETE",
        uri.to_string(),
        headers,
        Value::Null,
        Vec::new(),
    )
    .await;
    if state.error_stage == Some("delete") {
        return Json(json!({"err_code":40035008,"message":"素材图片不存在"}));
    }
    if state.error_stage == Some("delete-denied") {
        return Json(json!({"err_code":40034105,"message":"无权限"}));
    }
    Json(json!({}))
}

struct Server(tokio::task::JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn mock(
    etag: Option<&'static str>,
    presigned_url: Option<&'static str>,
    material_url: &'static str,
    error_stage: Option<&'static str>,
) -> (MaterialMock, QQBotClient, Server) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let state = MaterialMock {
        base: format!("http://{}", listener.local_addr().unwrap()),
        calls: Arc::new(Mutex::new(Vec::new())),
        etag,
        presigned_url,
        material_url,
        error_stage,
    };
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/v2/media-images/prepare", post(prepare))
        .route("/cos", put(cos))
        .route("/v2/media-images/complete", post(complete))
        .route("/v2/media-images/{image_id}/delete", delete(remove))
        .with_state(state.clone());
    let server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let client = QQBotClient::new(QQBotConfig {
        app_id: "test-app".into(),
        app_secret: "test-secret".into(),
        api_base: state.base.clone(),
        ..Default::default()
    });
    (state, client, server)
}

#[tokio::test]
async fn uploads_material_with_etag_and_deletes_only_the_returned_id() {
    let (state, client, _server) = mock(
        Some("\"0123456789abcdef0123456789abcdef\""),
        None,
        "qqbot-media://image/v1/opaque-material_token",
        None,
    )
    .await;
    let image = client
        .upload_markdown_image("gallery.jpg", "image/jpeg", IMAGE)
        .await
        .unwrap();
    assert_eq!(image.image_id, "102083127_material-1");
    assert_eq!(image.url, "qqbot-media://image/v1/opaque-material_token");
    client.delete_markdown_image(&image.image_id).await.unwrap();

    let calls = state.calls.lock().await;
    assert_eq!(calls.len(), 5);
    let prepared = &calls[1];
    assert_eq!(prepared.method, "POST");
    assert_eq!(prepared.path, "/v2/media-images/prepare");
    assert_eq!(
        prepared.json["file_size"].as_u64(),
        Some(IMAGE.len() as u64)
    );
    assert_eq!(prepared.json["file_name"], "gallery.jpg");
    assert_eq!(prepared.json["content_type"], "image/jpeg");
    assert_eq!(prepared.json["md5"], format!("{:x}", md5::compute(IMAGE)));
    assert_eq!(prepared.json["sha1"], hex::encode(Sha1::digest(IMAGE)));
    let uploaded = &calls[2];
    assert_eq!(uploaded.method, "PUT");
    assert_eq!(uploaded.headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(
        uploaded.headers[header::CONTENT_LENGTH],
        IMAGE.len().to_string()
    );
    assert!(!uploaded.headers.contains_key(header::AUTHORIZATION));
    assert_eq!(uploaded.bytes, IMAGE);
    let completed = &calls[3];
    assert_eq!(completed.path, "/v2/media-images/complete");
    assert_eq!(
        completed.json,
        json!({"upload_id":"material-upload-1","md5":COS_ETAG})
    );
    assert_ne!(COS_ETAG, format!("{:x}", md5::compute(IMAGE)));
    assert_eq!(calls[4].method, "DELETE");
    assert_eq!(
        calls[4].path,
        "/v2/media-images/102083127_material-1/delete"
    );
    for call in [&calls[1], &calls[3], &calls[4]] {
        assert_eq!(
            call.headers[header::AUTHORIZATION],
            "QQBot MATERIAL_TEST_TOKEN"
        );
    }
}

#[tokio::test]
async fn missing_or_invalid_etag_never_completes_with_a_local_digest() {
    for etag in [
        None,
        Some("not-a-file-md5"),
        Some("W/\"0123456789abcdef0123456789abcdef\""),
    ] {
        let (state, client, _server) = mock(etag, None, "qqbot-media://image/v1/test", None).await;
        let error = client
            .upload_markdown_image("gallery.png", "image/png", IMAGE)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ETag"));
        assert_eq!(state.calls.lock().await.len(), 3);
    }
}

#[tokio::test]
async fn rejects_local_validation_errors_before_requesting_a_token() {
    let (state, client, _server) = mock(None, None, "", None).await;
    let too_large = vec![0; 10_000_001];
    for (name, mime, bytes) in [
        ("empty.png", "image/png", &[][..]),
        ("large.png", "image/png", too_large.as_slice()),
        ("wrong.jpg", "image/png", IMAGE),
        ("image.svg", "image/svg+xml", IMAGE),
        ("missing-extension", "image/png", IMAGE),
        ("bad\r\n.png", "image/png", IMAGE),
    ] {
        assert!(
            client
                .upload_markdown_image(name, mime, bytes)
                .await
                .is_err()
        );
    }
    for id in ["", ".", "..", "bad\nidentifier", "bad identifier"] {
        assert!(client.delete_markdown_image(id).await.is_err());
    }
    assert!(state.calls.lock().await.is_empty());
}

#[tokio::test]
async fn rejects_unsafe_presigned_urls_without_put_or_complete() {
    for presigned in [
        "ftp://example.com/image",
        "http://username:password@example.com/image",
        "http://example.com/image\r\nInjected:value",
    ] {
        let (state, client, _server) = mock(None, Some(presigned), "", None).await;
        assert!(
            client
                .upload_markdown_image("gallery.webp", "image/webp", IMAGE)
                .await
                .is_err()
        );
        assert_eq!(state.calls.lock().await.len(), 2);
    }
}

#[tokio::test]
async fn rejects_non_material_or_empty_references() {
    for reference in [
        "https://example.com/image.jpg",
        "qqbot-media://image/v1/",
        "qqbot-media://other/v1/token",
        "qqbot-media://image/v1/token\r\n",
        "qqbot-media://image/v1/token)injected",
    ] {
        let (state, client, _server) = mock(Some(COS_ETAG), None, reference, None).await;
        assert!(
            client
                .upload_markdown_image("gallery.gif", "image/gif", IMAGE)
                .await
                .is_err()
        );
        let calls = state.calls.lock().await;
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[4].method, "DELETE");
        assert_eq!(
            calls[4].path,
            "/v2/media-images/102083127_material-1/delete"
        );
    }
}

#[tokio::test]
async fn invalid_reference_keeps_the_original_error_when_cleanup_fails() {
    let (state, client, _server) = mock(Some(COS_ETAG), None, "", Some("delete-denied")).await;
    let error = client
        .upload_markdown_image("gallery.png", "image/png", IMAGE)
        .await
        .unwrap_err();
    assert!(matches!(error, QQBotError::Other(message) if message.contains("qqbot-media")));
    let calls = state.calls.lock().await;
    assert_eq!(calls.len(), 5);
    assert_eq!(calls[4].method, "DELETE");
    assert_eq!(
        calls[4].path,
        "/v2/media-images/102083127_material-1/delete"
    );
}

#[tokio::test]
async fn tracks_created_material_before_invalid_reference_cleanup() {
    let (state, client, _server) = mock(Some(COS_ETAG), None, "", Some("delete-denied")).await;
    let tracked = std::sync::Mutex::new(Vec::new());
    let result = client
        .upload_markdown_image_tracked("gallery.png", "image/png", IMAGE, |id| {
            tracked.lock().unwrap().push(id.to_owned());
            Ok(())
        })
        .await;
    assert!(result.is_err());
    assert_eq!(*tracked.lock().unwrap(), ["102083127_material-1"]);
    let calls = state.calls.lock().await;
    assert_eq!(calls.last().unwrap().method, "DELETE");
}

#[tokio::test]
async fn preserves_official_material_error_codes() {
    for (stage, expected) in [("prepare", 40035007), ("complete", 40035003)] {
        let (_state, client, _server) = mock(
            Some(COS_ETAG),
            None,
            "qqbot-media://image/v1/test",
            Some(stage),
        )
        .await;
        let error = client
            .upload_markdown_image("gallery.png", "image/png", IMAGE)
            .await
            .unwrap_err();
        assert!(matches!(error, QQBotError::Api(error) if error.err_code == expected));
    }
}

#[tokio::test]
async fn deletion_encodes_an_opaque_id_as_a_single_path_segment() {
    let (state, client, _server) = mock(None, None, "", None).await;
    client
        .delete_markdown_image("opaque/id?scope=own")
        .await
        .unwrap();
    let calls = state.calls.lock().await;
    assert_eq!(
        calls[1].path,
        "/v2/media-images/opaque%2Fid%3Fscope%3Down/delete"
    );
}

#[tokio::test]
async fn deleting_an_already_removed_material_is_idempotent() {
    let (_state, client, _server) = mock(None, None, "", Some("delete")).await;
    client
        .delete_markdown_image("known-ledger-id")
        .await
        .unwrap();
}
