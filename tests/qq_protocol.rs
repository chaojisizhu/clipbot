use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use clipbot::bot::message::{media_payload, text_payload};
use clipbot::bot::qqbot::{INTENT_GROUP_AND_C2C, QQBotClient};
use clipbot::parser::api::VideoParserService;
use clipbot::service::AppState;
use clipbot::types::{AppConfig, BotEvent, QQBotConfig};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::net::TcpListener;
use tokio::sync::{Barrier, Mutex, mpsc};
use tokio::time::timeout;

const PARALLEL_BLOCK_SIZE: usize = 64 * 1024 + 4;

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
    parallel_puts: Option<Arc<ParallelPuts>>,
    http_base: String,
    ws_url: String,
}

struct ParallelPuts {
    barrier: Barrier,
    active: AtomicUsize,
    peak: AtomicUsize,
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

async fn user_messages(
    State(state): State<MockState>,
    Path(user): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    record(
        &state,
        format!("/v2/users/{user}/messages"),
        "POST",
        &headers,
        body,
    )
    .await;
    Json(json!({ "id": "ROBOT1.0_sent_dm", "timestamp": "2026-09-23T00:00:00+08:00" }))
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
    let already_uploaded = body.get("file_name").and_then(Value::as_str) == Some("cached.mp4");
    record(
        &state,
        "/v2/groups/GROUP001/upload_prepare".into(),
        "POST",
        &headers,
        body,
    )
    .await;
    if already_uploaded {
        return Json(json!({"upload_id":"upload-test-cached","parts":[]}));
    }
    let parts = [4, 1, 3, 2]
        .into_iter()
        .map(|index| {
            json!({
                "index": index,
                "presigned_url": format!("{}/cos/{index}", state.http_base)
            })
        })
        .collect::<Vec<_>>();
    let mut response = json!({
        "upload_id": "upload-test-1",
        "block_size": 4,
        "parts": parts
    });
    if state.parallel_puts.is_some() {
        response["concurrency"] = json!(2);
        response["block_size"] = json!(PARALLEL_BLOCK_SIZE);
    }
    Json(response)
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
    Path(part): Path<usize>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<&'static str, StatusCode> {
    assert!(!body.is_empty());
    if let Some(puts) = &state.parallel_puts {
        let active = puts.active.fetch_add(1, Ordering::SeqCst) + 1;
        puts.peak.fetch_max(active, Ordering::SeqCst);
        let reached = timeout(Duration::from_secs(2), puts.barrier.wait()).await;
        // Keep both requests in flight so additional PUTs would raise the peak.
        tokio::time::sleep(Duration::from_millis(30)).await;
        puts.active.fetch_sub(1, Ordering::SeqCst);
        reached.map_err(|_| StatusCode::GATEWAY_TIMEOUT)?;
        record(
            &state,
            format!("/cos/{part}"),
            "PUT",
            &headers,
            json!({
                "bytes": body.len(),
                "md5": format!("{:x}", md5::compute(&body)),
            }),
        )
        .await;
    }
    state.cos_parts.lock().await.push(body.to_vec());
    Ok("{}")
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
    start_mock_with_parallel_puts(false).await
}

async fn start_mock_with_parallel_puts(
    parallel_puts: bool,
) -> (MockState, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = MockState {
        calls: Arc::new(Mutex::new(Vec::new())),
        ops: Arc::new(Mutex::new(Vec::new())),
        ws_tx: Arc::new(Mutex::new(None)),
        passive_limit: Arc::new(AtomicBool::new(false)),
        cos_parts: Arc::new(Mutex::new(Vec::new())),
        parallel_puts: parallel_puts.then(|| {
            Arc::new(ParallelPuts {
                barrier: Barrier::new(2),
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            })
        }),
        http_base: format!("http://{address}"),
        ws_url: format!("ws://{address}/ws"),
    };
    let app = Router::new()
        .route("/app/getAppAccessToken", post(token))
        .route("/gateway", get(gateway))
        .route("/ws", get(websocket))
        .route("/v2/groups/{group}/messages", post(group_messages))
        .route("/v2/users/{user}/messages", post(user_messages))
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
        BotEvent::Message(event) => *event,
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
    assert_eq!(prepare.body["file_size"], 15);
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
    assert_eq!(
        memory_parts,
        [
            b"abc".to_vec(),
            b"hell".to_vec(),
            b"rld ".to_vec(),
            b"o wo".to_vec()
        ]
    );
    let finishes: Vec<_> = calls
        .iter()
        .filter(|call| call.path.ends_with("/upload_part_finish"))
        .collect();
    for (call, (index, length)) in finishes.iter().zip([(4, 3), (1, 4), (3, 4), (2, 4)]) {
        assert_eq!(call.body["part_index"], index);
        assert_eq!(call.body["block_size"], length);
    }
    let complete = calls
        .iter()
        .rfind(|call| call.path.ends_with("/files"))
        .unwrap();
    assert_eq!(complete.body, json!({"upload_id":"upload-test-1"}));

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
    assert_eq!(streamed_prepare.body["file_size"], 15);
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

    let cached = bot
        .upload_group_media_data("GROUP001", 2, "cached.mp4", b"hello world abc")
        .await
        .unwrap();
    assert_eq!(cached["file_info"], "FILE_INFO_UPLOADED");
    assert_eq!(state.cos_parts.lock().await.len(), 8);
    let calls = state.calls.lock().await.clone();
    let complete = calls
        .iter()
        .rfind(|call| call.path.ends_with("/files"))
        .unwrap();
    assert_eq!(complete.body, json!({"upload_id":"upload-test-cached"}));

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
async fn file_upload_overlaps_two_puts_and_finishes_before_assembly() {
    let (state, server) = start_mock_with_parallel_puts(true).await;
    let bot = QQBotClient::new(QQBotConfig {
        app_id: "test-app-id".into(),
        app_secret: "test-secret".into(),
        api_base: state.http_base.clone(),
        ..QQBotConfig::default()
    });
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("parallel.mp4");
    // Cross the streaming reader's 64 KiB boundary within each full part.
    let data: Vec<u8> = (0..3 * PARALLEL_BLOCK_SIZE + 19)
        .map(|offset| (offset % 251) as u8)
        .collect();
    tokio::fs::write(&path, &data).await.unwrap();

    let uploaded = timeout(
        Duration::from_secs(5),
        bot.upload_group_media_file("GROUP001", 2, "parallel.mp4", &path),
    )
    .await
    .expect("upload timed out")
    .expect("the server waits for two simultaneous PUTs");
    assert_eq!(uploaded["file_info"], "FILE_INFO_UPLOADED");
    assert_eq!(
        state
            .parallel_puts
            .as_ref()
            .unwrap()
            .peak
            .load(Ordering::SeqCst),
        2
    );

    let calls = state.calls.lock().await;
    let complete = calls.last().unwrap();
    assert!(complete.path.ends_with("/files"));
    assert_eq!(complete.body, json!({"upload_id":"upload-test-1"}));
    assert_eq!(calls.iter().filter(|call| call.method == "PUT").count(), 4);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.path.ends_with("/upload_part_finish"))
            .count(),
        4
    );
    for (offset, bytes) in data.chunks(PARALLEL_BLOCK_SIZE).enumerate() {
        let index = offset + 1;
        let expected_md5 = format!("{:x}", md5::compute(bytes));
        let put_path = format!("/cos/{index}");
        let put = calls.iter().position(|call| call.path == put_path).unwrap();
        let finish = calls
            .iter()
            .position(|call| {
                call.path.ends_with("/upload_part_finish") && call.body["part_index"] == index
            })
            .unwrap();
        assert!(put < finish);
        assert_eq!(calls[put].body["bytes"], bytes.len());
        assert_eq!(calls[put].body["md5"], expected_md5);
        assert!(calls[put].authorization.is_none());
        assert_eq!(calls[finish].body["block_size"], bytes.len());
        assert_eq!(calls[finish].body["md5"], expected_md5);
        assert_eq!(calls[finish].body["upload_id"], "upload-test-1");
    }
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

#[tokio::test]
async fn mentioned_manual_commands_reach_message_handler_with_auto_parse_disabled() {
    let (state, server) = start_mock().await;
    let config = AppConfig {
        qqbot: QQBotConfig {
            app_id: "123456".into(),
            app_secret: "test-secret".into(),
            api_base: state.http_base.clone(),
            receive_all_messages: false,
            auto_parse: false,
            command_prefix: vec!["解析".into()],
            show_waiting_tip: false,
            ..QQBotConfig::default()
        },
        ..AppConfig::default()
    };
    let expected_reply = config.ui_text.invalid_link_text.clone();
    let config = Arc::new(config);
    let bot = Arc::new(QQBotClient::new(config.qqbot.clone()));
    let parser = Arc::new(VideoParserService::new((*config).clone()).unwrap());
    let app = Arc::new(AppState::new(config, parser, Arc::clone(&bot)));
    let (events_tx, mut events_rx) = mpsc::channel(32);
    let bot_task = tokio::spawn(Arc::clone(&bot).run(events_tx));
    wait_for_identify(&state).await;

    let mut sequence = 0;
    let mut expected_messages = 0;
    for ready_id in [json!("654321"), json!(987654321u64)] {
        sequence += 1;
        push_event(
            &state,
            json!({
                "op":0, "s":sequence, "t":"READY",
                "d":{"session_id":"mention-session", "user":{"id":ready_id,"username":"mock"}}
            }),
        )
        .await;
        assert!(matches!(
            timeout(Duration::from_secs(2), events_rx.recv())
                .await
                .unwrap(),
            Some(BotEvent::Ready(_))
        ));
        let own_id = ready_id
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| ready_id.to_string());
        for mention in [
            format!("<@{own_id}>"),
            format!("<@!{own_id}>"),
            "<@123456>".into(),
            "<@!123456>".into(),
        ] {
            sequence += 1;
            push_event(
                &state,
                json!({
                    "op":0, "s":sequence, "t":"GROUP_AT_MESSAGE_CREATE", "d":{
                        "id":format!("mention-{sequence}"), "group_openid":"GROUP001",
                        "content":format!(" \n{mention}\t 解析 没有链接"),
                        "author":{"username":"mock"}
                    }
                }),
            )
            .await;
            let Some(BotEvent::Message(event)) = timeout(Duration::from_secs(2), events_rx.recv())
                .await
                .unwrap()
            else {
                panic!("expected normalized mentioned command");
            };
            assert_eq!(event.current_text, "解析 没有链接");
            assert_eq!(event.text, "解析 没有链接");
            Arc::clone(&app).handle_message(*event).await;
            expected_messages += 1;
            let calls = state.calls.lock().await;
            let messages = calls
                .iter()
                .filter(|call| call.path.ends_with("/messages"))
                .collect::<Vec<_>>();
            assert_eq!(messages.len(), expected_messages);
            assert_eq!(messages.last().unwrap().body["content"], expected_reply);
        }

        sequence += 1;
        let foreign_content = format!("<@other> 解析 没有链接 <@!{own_id}>");
        push_event(
            &state,
            json!({
                "op":0, "s":sequence, "t":"GROUP_AT_MESSAGE_CREATE", "d":{
                    "id":format!("foreign-{sequence}"), "group_openid":"GROUP001",
                    "content":foreign_content, "author":{"username":"mock"}
                }
            }),
        )
        .await;
        let Some(BotEvent::Message(event)) = timeout(Duration::from_secs(2), events_rx.recv())
            .await
            .unwrap()
        else {
            panic!("expected message preserving other mentions");
        };
        assert_eq!(event.current_text, foreign_content);
        assert_eq!(event.text, foreign_content);
        Arc::clone(&app).handle_message(*event).await;
        assert_eq!(
            state
                .calls
                .lock()
                .await
                .iter()
                .filter(|call| call.path.ends_with("/messages"))
                .count(),
            expected_messages
        );
    }

    sequence += 1;
    push_event(
        &state,
        json!({
            "op":0,"s":sequence,"t":"GROUP_AT_MESSAGE_CREATE","d":{
                "id":"whitespace-command","group_openid":"GROUP001",
                "content":" \n\t解析 没有链接","author":{"username":"mock"}
            }
        }),
    )
    .await;
    let Some(BotEvent::Message(event)) = timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .unwrap()
    else {
        panic!("expected whitespace command");
    };
    Arc::clone(&app).handle_message(*event).await;
    expected_messages += 1;
    let calls = state.calls.lock().await.clone();
    let messages = calls
        .iter()
        .filter(|call| call.path.ends_with("/messages"))
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), expected_messages);
    assert_eq!(messages.last().unwrap().body["content"], expected_reply);

    sequence += 1;
    push_event(
        &state,
        json!({
            "op":0, "s":sequence, "t":"GROUP_AT_MESSAGE_CREATE", "d":{
                "id":"nested-mentioned-command", "group_openid":"GROUP001",
                "content":"<@!123456>", "msg_elements":[{"content":"解析 没有链接"}],
                "author":{"username":"mock"}
            }
        }),
    )
    .await;
    let Some(BotEvent::Message(event)) = timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .unwrap()
    else {
        panic!("expected aggregate command");
    };
    assert!(event.current_text.is_empty());
    assert_eq!(event.text, "解析 没有链接");
    Arc::clone(&app).handle_message(*event).await;
    assert_eq!(
        state
            .calls
            .lock()
            .await
            .iter()
            .filter(|call| call.path.ends_with("/messages"))
            .count(),
        expected_messages + 1
    );

    bot.stop();
    timeout(Duration::from_secs(2), bot_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn api_error_captures_err_code_and_trace_id_from_body_or_header() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/app/getAppAccessToken",
            post(|| async { Json(json!({ "access_token": "TEST_TOKEN", "expires_in": 7200 })) }),
        )
        .route(
            "/v2/groups/BODY_TRACE/messages",
            post(|| async {
                Json(json!({
                    "err_code": 40_034_006,
                    "message": "消息内容违规",
                    "trace_id": "body-trace-123"
                }))
            }),
        )
        .route(
            "/v2/groups/HEADER_TRACE/messages",
            post(|| async {
                (
                    StatusCode::OK,
                    [("X-Tps-trace-ID", "header-trace-456")],
                    Json(json!({
                        "err_code": 40_034_006,
                        "message": "消息内容违规"
                    })),
                )
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = QQBotClient::new(QQBotConfig {
        app_id: "app".into(),
        app_secret: "secret".into(),
        api_base: base,
        ..QQBotConfig::default()
    });

    let err_body = client
        .send_group_message("BODY_TRACE", &text_payload("test"))
        .await
        .unwrap_err();
    match &err_body {
        clipbot::bot::qqbot::QQBotError::Api(api) => {
            assert_eq!(api.err_code, 40_034_006);
            assert_eq!(api.trace_id.as_deref(), Some("body-trace-123"));
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(
        err_body.to_string(),
        "消息内容违规 (err_code: 40034006, trace_id: body-trace-123)"
    );

    let err_header = client
        .send_group_message("HEADER_TRACE", &text_payload("test"))
        .await
        .unwrap_err();
    match &err_header {
        clipbot::bot::qqbot::QQBotError::Api(api) => {
            assert_eq!(api.err_code, 40_034_006);
            assert_eq!(api.trace_id.as_deref(), Some("header-trace-456"));
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(
        err_header.to_string(),
        "消息内容违规 (err_code: 40034006, trace_id: header-trace-456)"
    );

    server.abort();
}

#[tokio::test]
async fn gateway_extracts_sender_openid_and_mentions_for_openid_and_blacklist() {
    let (state, server) = start_mock().await;
    let temp_dir = tempfile::tempdir().unwrap();
    let mut config = AppConfig {
        qqbot: QQBotConfig {
            app_id: "123456".into(),
            app_secret: "test-secret".into(),
            api_base: state.http_base.clone(),
            receive_all_messages: true,
            auto_parse: true,
            super_admin_openid: "ADMIN_HEX_001".into(),
            show_waiting_tip: false,
            ..QQBotConfig::default()
        },
        ..AppConfig::default()
    };
    config.output.temp_dir = temp_dir.path().to_string_lossy().into_owned();
    let config = Arc::new(config);
    let bot = Arc::new(QQBotClient::new(config.qqbot.clone()));
    let parser = Arc::new(VideoParserService::new((*config).clone()).unwrap());
    let app = Arc::new(AppState::new(config, parser, Arc::clone(&bot)));
    let (events_tx, mut events_rx) = mpsc::channel(32);
    let bot_task = tokio::spawn(Arc::clone(&bot).run(events_tx));
    wait_for_identify(&state).await;

    push_event(
        &state,
        json!({
            "op": 0, "s": 1, "t": "READY",
            "d": { "session_id": "bl-session", "user": { "id": "ready-bot", "username": "mockbot" } }
        }),
    )
    .await;
    assert!(matches!(
        timeout(Duration::from_secs(2), events_rx.recv())
            .await
            .unwrap(),
        Some(BotEvent::Ready(_))
    ));

    // C2C /openid returns user_openid
    push_event(
        &state,
        json!({
            "op": 0, "s": 2, "t": "C2C_MESSAGE_CREATE",
            "d": {
                "id": "dm-openid-1",
                "content": "/openid",
                "author": { "id": "USER_DM_01", "user_openid": "USER_DM_01", "username": "" }
            }
        }),
    )
    .await;
    let Some(BotEvent::Message(dm_event)) = timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .unwrap()
    else {
        panic!("expected C2C message event");
    };
    assert_eq!(dm_event.sender_id, "USER_DM_01");
    Arc::clone(&app).handle_message(*dm_event).await;

    // GROUP_MESSAGE_CREATE with @bot (via mentions[].is_you) + 拉黑@target
    push_event(
        &state,
        json!({
            "op": 0, "s": 3, "t": "GROUP_MESSAGE_CREATE",
            "d": {
                "id": "grp-ban-1",
                "group_openid": "GROUP001",
                "content": "<@BOT_GROUP_OPENID> 拉黑<@TARGET_01> 发广告",
                "author": {
                    "id": "ADMIN_HEX_001",
                    "member_openid": "ADMIN_HEX_001",
                    "member_nickname": "群主管理员",
                    "username": "admin"
                },
                "mentions": [
                    {
                        "id": "BOT_GROUP_OPENID",
                        "member_openid": "BOT_GROUP_OPENID",
                        "is_you": true,
                        "bot": true,
                        "scope": "single"
                    },
                    {
                        "id": "TARGET_01",
                        "member_openid": "TARGET_01",
                        "member_nickname": "违规群友",
                        "username": "target",
                        "is_you": false,
                        "bot": false,
                        "scope": "single"
                    }
                ]
            }
        }),
    )
    .await;
    let Some(BotEvent::Message(ban_event)) = timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .unwrap()
    else {
        panic!("expected group ban message event");
    };
    assert_eq!(ban_event.sender_id, "ADMIN_HEX_001");
    assert_eq!(ban_event.sender_name, "群主管理员");
    assert_eq!(ban_event.current_text, "拉黑<@TARGET_01> 发广告");
    Arc::clone(&app).handle_message(*ban_event).await;

    // TARGET_01 triggers parsing in GROUP_AT_MESSAGE_CREATE -> gets blocked notice
    push_event(
        &state,
        json!({
            "op": 0, "s": 4, "t": "GROUP_AT_MESSAGE_CREATE",
            "d": {
                "id": "grp-blocked-1",
                "group_openid": "GROUP001",
                "content": "解析 https://www.bilibili.com/video/BV1GJ411x7h7",
                "author": {
                    "id": "TARGET_01",
                    "member_openid": "TARGET_01",
                    "username": "target"
                }
            }
        }),
    )
    .await;
    let Some(BotEvent::Message(blocked_event)) = timeout(Duration::from_secs(2), events_rx.recv())
        .await
        .unwrap()
    else {
        panic!("expected blocked user event");
    };
    Arc::clone(&app).handle_message(*blocked_event).await;

    let calls = state.calls.lock().await;
    let dm_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.path == "/v2/users/USER_DM_01/messages")
        .collect();
    assert_eq!(dm_calls.len(), 1);
    assert_eq!(dm_calls[0].body["content"], "你的 OpenID: USER_DM_01");

    let group_calls: Vec<_> = calls
        .iter()
        .filter(|call| call.path == "/v2/groups/GROUP001/messages")
        .collect();
    assert_eq!(group_calls.len(), 2);
    assert_eq!(
        group_calls[0].body["content"],
        "已将 违规群友 (TARGET_01) 加入全局黑名单，理由: 发广告"
    );
    assert_eq!(
        group_calls[1].body["content"],
        "您已被加入全局黑名单，理由: 发广告"
    );

    bot.stop();
    timeout(Duration::from_secs(2), bot_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server.abort();
}
