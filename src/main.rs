//! arona-rs：Arona 的 Rust 移植版（onebot 独立模式，完全剥离 mirai）
//! 对应原版 standalone/AronaStandalone 的启动流程。

mod activity;
mod config;
mod data;
mod db;
mod entity;
mod gacha;
mod image;
mod onebot;
mod quartz;
mod runtime;
mod services;
mod standalone;
mod util;

use onebot::application::OneBotApplication;
use onebot::business::StandaloneBusinessHandler;
use onebot::connection::ConnectionRegistry;
use onebot::message_sender::DeferredMessageSender;
use std::path::PathBuf;
use std::sync::Arc;

fn find_arg(args: &[String], prefix: &str) -> Option<String> {
    args.iter()
        .find(|arg| arg.starts_with(prefix))
        .map(|arg| arg[prefix.len()..].to_string())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    // 提前探测终端能力(Windows 开启 VT 处理), 保证横幅/日志颜色正常渲染
    runtime::console::init();
    if std::env::var_os("ARONA_CONSOLE_DEBUG").is_some() {
        runtime::log::info(format!(
            "控制台颜色模式: {}",
            runtime::console::mode_summary()
        ));
    }
    onebot::console::print_banner(env!("CARGO_PKG_VERSION"));

    let config_file = find_arg(&args, "--config=")
        .map(PathBuf::from)
        .unwrap_or_else(runtime::paths::default_onebot_file);
    let arona_config_file = find_arg(&args, "--arona-config=")
        .map(PathBuf::from)
        .unwrap_or_else(runtime::paths::default_arona_file);
    let test_notify = args.iter().any(|arg| arg == "--test-notify");

    runtime::services::set_data_root(runtime::paths::data_root());

    // 先加载业务配置（含热更新；首次会从旧 onebot.yaml 迁移 groups/managers/notify），再加载协议配置
    config::standalone::init(arona_config_file.clone());
    let onebot_config = match config::onebot::load(&config_file) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("[Arona] onebot.yml 加载失败: {err}");
            std::process::exit(1);
        }
    };
    runtime::config::set_bot_id(onebot_config.self_id);
    runtime::config::set_end_with_sensei("老师".to_string());

    runtime::log::info("initializing database...");
    if !db::start() {
        runtime::log::error("database init failed");
    }
    util::tarot::ensure_initialized();

    // 后台任务：kivo 学生数据预热 + 启动刷新本地资源图片
    tokio::spawn(async move {
        data::kivo::init().await;
    });
    tokio::spawn(async move {
        // 启动时刷新一次本地资源图片: 拉取三服活动 + 写入数据库 + 重新渲染活动日历图
        standalone::commands::activity::refresh_all_images().await;
        standalone::commands::tarot::download_all_images().await;
    });

    // 启用每日活动推送（含启动 20 秒后的预警初始化）
    activity::notify::enable_service();
    // 本地资源图片: 每天 0 点刷新一次(另有活动到期后 5 分钟的定向刷新)
    standalone::commands::activity::enable_image_refresh_job();
    if test_notify {
        quartz::create_delay(
            20,
            "TestNotify",
            Arc::new(|| {
                tokio::spawn(async move {
                    // 完整执行一次每日推送（活动日历图 + 预警调度），便于联调验证
                    activity::notify::push(false).await;
                });
            }),
        );
        runtime::log::info("测试推送已安排, 将在 20 秒后执行");
    }

    let registry = Arc::new(ConnectionRegistry::new());
    let dispatcher = standalone::dispatcher::build(onebot_config.clone());
    let business = Arc::new(StandaloneBusinessHandler::new(
        onebot_config.clone(),
        dispatcher,
        registry.clone(),
    ));
    let application = Arc::new(OneBotApplication::new(
        onebot_config.clone(),
        business.clone(),
        registry.clone(),
    ));
    application.start();

    // 就绪后设置全局消息发送器（活动推送/预警使用首个可用连接）
    let sender = Arc::new(DeferredMessageSender {
        registry: registry.clone(),
        self_id: onebot_config.self_id,
    });
    runtime::services::set_message_sender(sender);

    println!("Arona standalone started");
    println!("Config: {}", config_file.to_string_lossy());
    println!("Arona 业务配置: {}", arona_config_file.to_string_lossy());
    println!(
        "日志文件: {}",
        runtime::paths::logs_dir()
            .join("arona-yyyy-MM-dd.log")
            .to_string_lossy()
    );

    // 优雅关闭：停止 OneBot 连接、定时任务与数据库
    tokio::signal::ctrl_c().await.ok();
    runtime::log::info("正在关闭 Arona...");
    application.stop();
    quartz::pause_all();
    db::close();
    println!("Arona standalone stopped");
}
