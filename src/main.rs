use std::sync::Arc;

use videorust::bot::qqbot::QQBotClient;
use videorust::config::load_config;
use videorust::parser::api::VideoParserService;
use videorust::service::AppState;
use videorust::types::{BotEvent, QQChatEvent};

#[tokio::main]
async fn main() {
    let config = load_config(None);
    init_logging(config.qqbot.debug);
    info_banner();

    if config.qqbot.app_id.is_empty() || config.qqbot.app_secret.is_empty() {
        tracing::error!(
            "未配置 qqbot.appId / qqbot.appSecret，请编辑 config.yaml 填入 QQ 开放平台机器人的 AppID 与 AppSecret"
        );
        std::process::exit(1);
    }

    let config = Arc::new(config);
    let parser = Arc::new(VideoParserService::new((*config).clone()));
    let bot = Arc::new(QQBotClient::new(config.qqbot.clone()));
    let state = Arc::new(AppState::new(
        Arc::clone(&config),
        Arc::clone(&parser),
        Arc::clone(&bot),
    ));
    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(256);
    let mut bot_task = tokio::spawn(Arc::clone(&bot).run(events_tx));
    let mut bot_finished = false;

    loop {
        tokio::select! {
            event = events_rx.recv() => {
                match event {
                    Some(BotEvent::Message(message)) => {
                        spawn_message(Arc::clone(&state), message);
                    }
                    Some(BotEvent::Ready(_)) => {
                        tracing::info!("🎉 QQ 官方视频解析机器人已就绪");
                    }
                    Some(BotEvent::Resumed(_)) => {
                        tracing::info!("QQ 机器人会话已恢复");
                    }
                    None => break,
                }
            }
            result = &mut bot_task => {
                bot_finished = true;
                match result {
                    Ok(Ok(())) => tracing::warn!("QQ 机器人连接循环已退出"),
                    Ok(Err(err)) => tracing::error!("启动 QQ 机器人失败: {err:#}"),
                    Err(err) => tracing::error!("QQ 机器人任务异常: {err}"),
                }
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("收到退出信号，正在停止机器人...");
                bot.stop();
                break;
            }
        }
    }

    if !bot_finished {
        bot.stop();
        let _ = bot_task.await;
    }
}

fn spawn_message(state: Arc<AppState>, message: QQChatEvent) {
    tokio::spawn(async move {
        state.handle_message(message).await;
    });
}

fn init_logging(debug: bool) {
    use tracing_subscriber::EnvFilter;
    let fallback = if debug { "debug" } else { "info" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn info_banner() {
    tracing::info!("========================================================");
    tracing::info!("  QQ 官方机器人 全平台视频解析机器人 启动中...");
    tracing::info!("========================================================");
}
