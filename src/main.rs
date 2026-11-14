use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::task::JoinSet;
use videorust::bot::qqbot::QQBotClient;
use videorust::config::load_config;
use videorust::parser::api::{ParseResult, VideoParserService};
use videorust::parser::matcher::link_type_parser;
use videorust::parser::remux::download_parsed_temp;
use videorust::service::AppState;
use videorust::types::{AppConfig, BotEvent};

#[tokio::main(worker_threads = 2)]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!(
            "AllVideo Rust\n\n用法\n  videorust [--config config.yaml] bot\n  videorust [--config config.yaml] parse <分享链接>...\n  videorust [--config config.yaml] probe <分享链接>\n  videorust [--config config.yaml] check\n  videorust [--config config.yaml] login bilibili [--cookie-file 路径] [--qr-file 路径] [--timeout-seconds 180]\n\nparse 输出 JSON。probe 解析并流式下载校验，随后清理文件。默认命令为 bot。"
        );
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--version") {
        println!("allvideo {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let config_path = if let Some(index) = args.iter().position(|arg| arg == "--config") {
        if index + 1 >= args.len() {
            bail!("--config 缺少路径");
        }
        let path = PathBuf::from(args.remove(index + 1));
        args.remove(index);
        Some(path)
    } else {
        None
    };
    let config = load_config(config_path.as_deref())?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(if config.qqbot.debug { "debug" } else { "info" })
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
    let command = args.first().map(String::as_str).unwrap_or("bot");
    if command == "login" {
        return run_login(&config, &args[1..]).await;
    }
    let parser = Arc::new(VideoParserService::new(config.clone())?);
    match command {
        "check" => {
            println!(
                "{}",
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "parser": "native-rust",
                    "qqConfigured": !config.qqbot.app_id.is_empty() && !config.qqbot.app_secret.is_empty(),
                    "yuanbaoConfigured": !config.channels.yuanbao_cookie.is_empty(),
                    "cookiePlatforms": config.auth.cookies.iter().filter(|(_, value)| !value.trim().is_empty()).map(|(key, _)| key).collect::<Vec<_>>(),
                    "cookieFilePlatforms": config.auth.cookie_files.keys().collect::<Vec<_>>(),
                    "localYtdlpEnabled": config.ytdlp.enabled,
                    "parseConcurrency": config.api.max_concurrency.clamp(1,16),
                    "maxMessageTasks": config.resources.max_message_tasks.clamp(1,32),
                    "maxTempBytes": config.resources.max_temp_bytes,
                })
            );
            Ok(())
        }
        "parse" | "probe" => {
            if args.len() < 2 {
                bail!("请提供分享链接");
            }
            if args.len() > 17 {
                bail!("单次命令最多16个链接");
            }
            let mut failed = false;
            for url in &args[1..] {
                let hits = link_type_parser(url);
                let hit = hits.first().context("未识别到支持的分享链接")?;
                let result = parser.parse_url(&hit.url, &hit.media_type).await;
                if command == "probe" {
                    match &result {
                        ParseResult::Success(data) if !data.video.is_empty() => {
                            let file = download_parsed_temp(data, &config).await?;
                            println!(
                                "{}",
                                serde_json::json!({"status":"success", "platform":hit.media_type, "title":data.title, "downloadedBytes":file.size(), "cleanupOnExit":true})
                            );
                        }
                        ParseResult::Success(data) => println!(
                            "{}",
                            serde_json::json!({"status":"success", "images":data.images.len(), "title":data.title})
                        ),
                        ParseResult::Failure(_) => {
                            failed = true;
                            println!("{}", serde_json::to_string(&result)?);
                        }
                    }
                } else {
                    failed |= matches!(result, ParseResult::Failure(_));
                    println!("{}", serde_json::to_string(&result)?);
                }
            }
            if failed {
                bail!("部分链接解析失败");
            }
            Ok(())
        }
        "bot" => run_bot(config, parser).await,
        _ => bail!("未知命令 {command}，使用 --help 查看用法"),
    }
}

async fn run_login(config: &AppConfig, args: &[String]) -> Result<()> {
    use videorust::auth::bilibili::{LoginOptions, login};

    if args.first().map(String::as_str) != Some("bilibili") {
        bail!("扫码登录当前支持 bilibili，使用 login bilibili 启动");
    }
    let mut cookie_file = config
        .auth
        .cookie_files
        .get("bilibili")
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cookies/bilibili.txt"));
    let mut qr_file = PathBuf::from("cookies/bilibili-login.png");
    let mut timeout = Duration::from_secs(180);
    let (options, remainder) = args[1..].as_chunks::<2>();
    for pair in options {
        match pair[0].as_str() {
            "--cookie-file" => cookie_file = PathBuf::from(&pair[1]),
            "--qr-file" => qr_file = PathBuf::from(&pair[1]),
            "--timeout-seconds" => {
                let seconds: u64 = pair[1].parse().context("登录等待秒数必须为正整数")?;
                if seconds == 0 {
                    bail!("登录等待秒数必须大于零");
                }
                timeout = Duration::from_secs(seconds.min(600));
            }
            other => bail!("未知登录选项 {other}"),
        }
    }
    if let Some(option) = remainder.first() {
        bail!("登录选项 {option} 缺少值");
    }
    let outcome = login(LoginOptions {
        cookie_file,
        qr_file,
        timeout,
        proxy: config.api.proxy.clone(),
    })
    .await?;
    if config
        .auth
        .cookies
        .get("bilibili")
        .is_some_and(|cookie| !cookie.trim().is_empty())
    {
        eprintln!(
            "新 Cookie 已保存。请清空旧的 auth.cookies.bilibili，并将 auth.cookieFiles.bilibili 设为 {} 后重启。",
            outcome.cookie_file.display()
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "status":"success", "platform":"bilibili",
            "cookieFile": outcome.cookie_file,
            "verified": outcome.verified,
            "message": outcome.verification_message,
        })
    );
    Ok(())
}

async fn run_bot(config: AppConfig, parser: Arc<VideoParserService>) -> Result<()> {
    if config.qqbot.app_id.is_empty() || config.qqbot.app_secret.is_empty() {
        bail!(
            "请填写 qqbot.appId / qqbot.appSecret，或设置 QQBOT_APP_ID / QQBOT_APP_SECRET。可先使用 parse 命令测试解析。"
        );
    }
    let task_limit = config.resources.max_message_tasks.clamp(1, 32);
    let config = Arc::new(config);
    let bot = Arc::new(QQBotClient::new(config.qqbot.clone()));
    let state = Arc::new(AppState::new(config, parser, Arc::clone(&bot)));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut bot_task = tokio::spawn(Arc::clone(&bot).run(tx));
    let mut workers = JoinSet::new();
    let mut finished = false;
    let mut bot_result = Ok(());
    loop {
        tokio::select! {
            event = rx.recv(), if workers.len() < task_limit => {
                match event {
                    Some(BotEvent::Message(message)) => {
                        let state = Arc::clone(&state);
                        workers.spawn(async move { state.handle_message(message).await; });
                    }
                    Some(BotEvent::Ready(_)) => tracing::info!("QQ机器人已连接"),
                    Some(BotEvent::Resumed(_)) => tracing::info!("QQ会话已恢复"),
                    None => break,
                }
            }
            result = workers.join_next(), if !workers.is_empty() => {
                if let Some(Err(error)) = result { tracing::warn!("消息任务退出 {error}"); }
            }
            result = &mut bot_task => {
                finished = true;
                bot_result = result.context("QQ网关任务异常")?;
                break;
            }
            _ = shutdown_signal() => break,
        }
    }
    bot.stop();
    if !finished
        && tokio::time::timeout(Duration::from_secs(5), &mut bot_task)
            .await
            .is_err()
    {
        bot_task.abort();
        let _ = bot_task.await;
    }
    if tokio::time::timeout(Duration::from_secs(10), async {
        while workers.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        workers.abort_all();
        while workers.join_next().await.is_some() {}
    }
    bot_result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("无法注册SIGTERM处理器");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
