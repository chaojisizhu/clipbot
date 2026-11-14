use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;
use videorust::bot::message::{media_payload, text_payload};
use videorust::bot::qqbot::{INTENT_GROUP_AND_C2C, QQBotClient};
use videorust::types::{BotEvent, QQBotConfig};

#[derive(Clone, Debug)]
struct RequestLog {
    path: String,
    method: String,
    body: Value,
    authorization: Option<String>,
}

#[derive(Clone)]
struct MockState {
    calls: Arc<Mutex<Vec<RequestLog>>>,
    ops: Arc<Mutex<Vec<Value>>>,
    ws_tx: Arc<Mutex<Option<mpsc::Sender<Value>>>>,
    passive_limit: Arc<AtomicBool>,
    cos_parts: Arc<Mutex<Vec<Vec<u8>>>>,
    http_base: String,
    ws_url: String,
}

async fn record(state: &MockState, path: String, method: &str, headers: &HeaderMap, body: Value) {
    state.calls.lock().await.push(RequestLog {
        path,
        method: method.into(),
        body,
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string),
    });
}

async fn token(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(
        &state,
        "/app/getAppAccessToken".into(),
        "POST",
        &headers,
        body,
    )
    .await;
    Json(json!({ "access_token": "TEST_TOKEN_123", "expires_in": 7200 }))
}

async fn gateway(State(state): State<MockState>, headers: HeaderMap) -> Json<Value> {
    record(&state, "/gateway".into(), "GET", &headers, Value::Null).await;
    Json(json!({ "url": state.ws_url }))
}

async fn websocket(
    ws: WebSocketUpgrade,
    State(state): State<MockState>,
) -> axum::response::Response {
    ws.on_upgrade(move |socket| websocket_loop(socket, state))
}

async fn websocket_loop(socket: WebSocket, state: MockState) {
    let (mut sender, mut receiver) = socket.split();
    let _ = sender
        .send(Message::Text(
            json!({ "op": 10, "d": { "heartbeat_interval": 400 } })
                .to_string()
                .into(),
        ))
        .await;
    let (tx, mut rx) = mpsc::channel::<Value>(32);
    *state.ws_tx.lock().await = Some(tx);

    loop {
        tokio::select! {
            outbound = rx.recv() => {
                let Some(outbound) = outbound else { break };
                if sender.send(Message::Text(outbound.to_string().into())).await.is_err() {
                    break;
                }
            }
            inbound = receiver.next() => {
                let Some(Ok(message)) = inbound else { break };
                if let Message::Text(text) = message
                    && let Ok(payload) = serde_json::from_str::<Value>(&text) {
                        state.ops.lock().await.push(payload.clone());
                        if payload.get("op").and_then(Value::as_u64) == Some(1) {
                            let _ = sender.send(Message::Text(json!({ "op": 11 }).to_string().into())).await;
                        }
                    }
            }
        }
    }
}

async fn group_messages(
    State(state): State<MockState>,
    Path(_group): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(
        &state,
        "/v2/groups/GROUP001/messages".into(),
        "POST",
        &headers,
        body,
    )
    .await;
    if state.passive_limit.swap(false, Ordering::SeqCst) {
        Json(json!({
            "err_code": 40034128,
            "message": "被动回复时间或次数超限",
            "trace_id": "trace-1"
        }))
    } else {
        Json(json!({ "id": "ROBOT1.0_sent", "timestamp": "2026-09-23T00:00:00+08:00" }))
    }
}

async fn group_files(
    State(state): State<MockState>,
    Path(_group): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(
        &state,
        "/v2/groups/GROUP001/files".into(),
        "POST",
        &headers,
        body.clone(),
    )
    .await;
    if body.get("upload_id").is_some() {
        Json(json!({
            "file_uuid": "uuid-up",
            "file_info": "FILE_INFO_UPLOADED",
            "raw_url": "https://cos.example.com/raw_url.jpg?sig=x",
            "ttl": 300
        }))
    } else {
        Json(json!({ "file_uuid": "uuid-1", "file_info": "FILE_INFO_XYZ", "ttl": 300 }))
    }
}

async fn upload_prepare(
    State(state): State<MockState>,
    Path(_group): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(
        &state,
        "/v2/groups/GROUP001/upload_prepare".into(),
        "POST",
        &headers,
        body,
    )
    .await;
    Json(json!({
        "upload_id": "upload-test-1",
        "block_size": "4",
        "parts": (0..4).map(|index| json!({
            "index": index,
            "presigned_url": format!("{}/cos/{index}", state.http_base),
            "block_size": "4"
        })).collect::<Vec<_>>()
    }))
}

async fn upload_part_finish(
    State(state): State<MockState>,
    Path(_group): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(
        &state,
        "/v2/groups/GROUP001/upload_part_finish".into(),
        "POST",
        &headers,
        body,
    )
    .await;
    Json(json!({}))
}

async fn cos_part(
    State(state): State<MockState>,
    Path(_part): Path<String>,
    body: Bytes,
) -> &'static str {
    assert!(!body.is_empty());
    state.cos_parts.lock().await.push(body.to_vec());
    "{}"
}

async fn push_event(state: &MockState, value: Value) {
    let tx = state.ws_tx.lock().await.clone().expect("websocket sender");
    tx.send(value).await.expect("push websocket event");
}

async fn wait_for_identify(state: &MockState) {
    timeout(Duration::from_secs(3), async {
        loop {
            if state
                .ops
                .lock()
                .await
                .iter()
                .any(|op| op.get("op").and_then(Value::as_u64) == Some(2))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("identify timeout");
}

async fn start_mock() -> (MockState, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = MockState {
        calls: Arc::new(Mutex::new(Vec::new())),
        ops: Arc::new(Mutex::new(Vec::new())),
        ws_tx: Arc::new(Mutex::new(None)),
        passive_limit: Arc::new(AtomicBool::new(false)),
        cos_parts: Arc::new(Mutex::new(Vec::new())),
        http_base: format!("http://{address}"),
        ws_url: format!("ws://{address}/ws"),
    };
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/gateway", get(gateway))
        .route("/ws", get(websocket))
        .route("/v2/groups/{group}/messages", post(group_messages))
        .route("/v2/groups/{group}/files", post(group_files))
        .route("/v2/groups/{group}/upload_prepare", post(upload_prepare))
        .route(
            "/v2/groups/{group}/upload_part_finish",
            post(upload_part_finish),
        )
        .route("/cos/{part}", put(cos_part))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (state, server)
}

#[tokio::test]
async fn gateway_reply_and_chunked_upload_protocol() {
    let (state, server) = start_mock().await;
    let config = QQBotConfig {
        app_id: "test-app-id".into(),
        app_secret: "test-secret".into(),
        api_base: state.http_base.clone(),
        intents: INTENT_GROUP_AND_C2C,
        receive_all_messages: true,
        auto_parse: true,
        command_prefix: vec!["/parse".into()],
        show_waiting_tip: false,
        debug: false,
        ..QQBotConfig::default()
    };
    let bot = Arc::new(QQBotClient::new(config));
    let (events_tx, mut events_rx) = mpsc::channel(32);
    let bot_task = tokio::spawn(Arc::clone(&bot).run(events_tx));
    wait_for_identify(&state).await;

    push_event(
        &state,
        json!({
            "op": 0,
            "s": 1,
            "t": "READY",
            "d": { "session_id": "sess-abc-123", "user": { "username": "测试机器人" } }
        }),
    )
    .await;
    assert!(matches!(
        timeout(Duration::from_secs(2), events_rx.recv())
            .await
            .unwrap(),
        Some(BotEvent::Ready(_))
    ));

    push_event(
        &state,
        json!({
            "op": 0,
            "s": 2,
            "t": "GROUP_MESSAGE_CREATE",
            "d": {
                "id": "ROBOT1.0_msg001",
                "author": { "username": "小明" },
                "content": "看这个 https://v.douyin.com/abc12345/",
                "group_openid": "GROUP001"
            }
        }),
    )
    .await;
    let event = match timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .unwrap()
        .unwrap()
    {
        BotEvent::Message(event) => event,
        other => panic!("expected message event, got {other:?}"),
    };
    assert_eq!(event.chat_id, "GROUP001");
    assert!(event.text.contains("https://v.douyin.com/abc12345/"));

    state.passive_limit.store(true, Ordering::SeqCst);
    bot.reply(&event, &text_payload("被动超限兜底"))
        .await
        .unwrap();
    let calls = state.calls.lock().await.clone();
    let messages: Vec<_> = calls
        .iter()
        .filter(|call| call.path.ends_with("/messages"))
        .collect();
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(|call| call.method == "POST"));
    assert_eq!(messages[0].body["msg_id"], "ROBOT1.0_msg001");
    assert_eq!(messages[0].body["msg_seq"], 1);
    assert!(
        messages[0]
            .authorization
            .as_deref()
            .is_some_and(|value| value.starts_with("QQBot "))
    );
    assert!(messages[1].body.get("msg_id").is_none());
    assert!(messages[1].body.get("msg_seq").is_none());

    let upload = bot
        .upload_group_media_data("GROUP001", 2, "video.mp4", b"hello world abc")
        .await
        .unwrap();
    assert_eq!(upload["file_info"], "FILE_INFO_UPLOADED");
    assert!(
        upload["raw_url"]
            .as_str()
            .unwrap()
            .contains("cos.example.com")
    );
    let calls = state.calls.lock().await.clone();
    let prepare = calls
        .iter()
        .find(|call| call.path.ends_with("/upload_prepare"))
        .unwrap();
    assert_eq!(prepare.body["file_size"], "15");
    assert_eq!(
        prepare.body["md5"],
        format!("{:x}", md5::compute(b"hello world abc"))
    );
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.path.ends_with("/upload_part_finish"))
            .count(),
        4
    );
    let memory_parts = state.cos_parts.lock().await.clone();
    assert_eq!(memory_parts.concat(), b"hello world abc");

    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("streamed.mp4");
    tokio::fs::write(&path, b"hello world abc").await.unwrap();
    let streamed = bot
        .upload_group_media_file("GROUP001", 2, "streamed.mp4", &path)
        .await
        .unwrap();
    assert_eq!(streamed["file_info"], "FILE_INFO_UPLOADED");
    let calls = state.calls.lock().await.clone();
    let streamed_prepare = calls
        .iter()
        .rev()
        .find(|call| call.path.ends_with("/upload_prepare"))
        .unwrap();
    assert_eq!(streamed_prepare.body["file_size"], "15");
    assert_eq!(
        streamed_prepare.body["md5"],
        format!("{:x}", md5::compute(b"hello world abc"))
    );
    assert_eq!(
        streamed_prepare.body["md5_10m"],
        streamed_prepare.body["md5"]
    );
    assert_eq!(
        streamed_prepare.body["sha1"],
        hex::encode(Sha1::digest(b"hello world abc"))
    );
    assert_eq!(&state.cos_parts.lock().await[4..], memory_parts.as_slice());
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.path.ends_with("/upload_part_finish"))
            .count(),
        8
    );

    bot.reply(&event, &media_payload("FILE_INFO_UPLOADED"))
        .await
        .unwrap();
    let calls = state.calls.lock().await;
    let last_message = calls
        .iter()
        .rfind(|call| call.path.ends_with("/messages"))
        .unwrap();
    assert_eq!(last_message.body["msg_type"], 7);
    assert_eq!(
        last_message.body["media"]["file_info"],
        "FILE_INFO_UPLOADED"
    );

    bot.stop();
    let _ = timeout(Duration::from_secs(2), bot_task).await;
    server.abort();
}

#[tokio::test]
async fn full_event_queue_keeps_gateway_heartbeats_running() {
    let (state, server) = start_mock().await;
    let bot = Arc::new(QQBotClient::new(QQBotConfig {
        app_id: "test-app-id".into(),
        app_secret: "test-secret".into(),
        api_base: state.http_base.clone(),
        ..QQBotConfig::default()
    }));
    let (events_tx, events_rx) = mpsc::channel(1);
    let bot_task = tokio::spawn(Arc::clone(&bot).run(events_tx));
    wait_for_identify(&state).await;
    push_event(
        &state,
        json!({
            "op":0, "s":1, "t":"READY", "d":{"session_id":"full-queue", "user":{"username":"mock"}}
        }),
    )
    .await;
    timeout(Duration::from_secs(2), async {
        while events_rx.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let heartbeat_count = state
        .ops
        .lock()
        .await
        .iter()
        .filter(|op| op.get("op").and_then(Value::as_u64) == Some(1))
        .count();
    // All dispatch kinds encounter the full channel. None may suspend the
    // gateway reader, which must continue sending heartbeats and reading ACKs.
    push_event(
        &state,
        json!({
            "op":0, "s":2, "t":"GROUP_MESSAGE_CREATE", "d":{
                "id":"queue-msg-1", "group_openid":"GROUP001", "content":"queued message",
                "author":{"username":"mock"}
            }
        }),
    )
    .await;
    push_event(&state, json!({"op":0, "s":3, "t":"RESUMED", "d":{}})).await;
    push_event(
        &state,
        json!({
            "op":0, "s":4, "t":"READY", "d":{"session_id":"still-ready", "user":{"username":"mock"}}
        }),
    )
    .await;
    timeout(Duration::from_secs(3), async {
        loop {
            let count = state
                .ops
                .lock()
                .await
                .iter()
                .filter(|op| op.get("op").and_then(Value::as_u64) == Some(1))
                .count();
            if count >= heartbeat_count + 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("full dispatch channel stalled gateway heartbeats");
    assert_eq!(events_rx.len(), 1);
    bot.stop();
    timeout(Duration::from_secs(2), bot_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();
}
