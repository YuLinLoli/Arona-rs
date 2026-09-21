//! arona 框架：OneBot v11 连接、管理面板(GUI)、群授权/黑名单与命令分发骨架，
//! 以及启动/关闭的生命周期编排。具体功能由插件（实现 [`plugin::AronaPlugin`]）提供，
//! host 在 [`run`] 之前用 [`plugin::register`] 注册插件。
//!
//! 这个 crate 就是插件开发时引入的依赖：`arona = { path = "..." }`，
//! 在插件里通过 [`runtime`] / [`config`] / [`onebot`] / [`services`] / [`quartz`] 等
//! 复用框架能力，并可 `cargo run`（走 host）开发与运行调试。

#[cfg(feature = "gui")]
pub mod gui;

pub mod admin;
pub mod config;
/// 插件间能力共享容器（对应 mirai 的 `DiContainer`）
pub mod container;
/// 框架实例：插件契约面的注册表按实例持有（对应 mirai 的 `MiraiInstance`）
pub mod framework;
pub mod onebot;
pub mod plugin;
pub mod quartz;
pub mod runtime;
pub mod services;

use crate::onebot::application::OneBotApplication;
use crate::onebot::business::StandaloneBusinessHandler;
use crate::onebot::connection::ConnectionRegistry;
use crate::onebot::message_sender::DeferredMessageSender;
use std::path::PathBuf;
use std::sync::Arc;

/// 取 `--key=value` 形式的命令行参数
pub(crate) fn find_arg(args: &[String], prefix: &str) -> Option<String> {
    args.iter()
        .find(|arg| arg.starts_with(prefix))
        .map(|arg| arg[prefix.len()..].to_string())
}

/// 框架总入口：完成提权/软件渲染兜底/GUI 或命令行模式的选择，随后运行机器人主流程。
/// 调用前应已通过 [`plugin::register`] 注册好全部插件。
pub fn run(args: Vec<String>) {
    // 第一件事就是接上 log 门面：wgpu/glutin 的失败原因（比如 DX12 后端创建失败）
    // 只在 debug 级别记录，不装 logger 会完全看不到，GUI 起不来时等于两眼一抹黑
    runtime::log::install_logger();

    // 换渲染后端重启（GUI 看门狗 / 软渲染兜底）时带的启动延迟：让上一个可能还卡着的
    // 进程先彻底退出，避免端口/数据库句柄被占住。正常启动不会设置这个变量。
    if let Some(ms) = std::env::var("ARONA_START_DELAY_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
    {
        std::thread::sleep(std::time::Duration::from_millis(ms.min(10_000)));
    }

    // 申请管理员权限（UAC）：需要时用提权实例重新拉起自己并立刻退出。
    if runtime::elevate::request_admin(&args) {
        runtime::console::print_safe(
            "[Arona] 正在以管理员身份重新启动，请在 UAC 弹窗中选择“是”...",
        );
        return;
    }

    // 软件 OpenGL 模式（--softgl / ARONA_SOFTGL=1）：必须在任何 eframe/glutin 调用之前
    // 把 softgl 目录塞进 DLL 搜索路径，否则系统那份 OpenGL 1.1 会先被加载
    #[cfg(feature = "gui")]
    if runtime::softgl::requested(&args) {
        runtime::softgl::activate();
    }

    // 任何 panic 都要留下痕迹：GUI 子系统没有控制台，双击启动时默认会表现成“没反应”
    std::panic::set_hook(Box::new(|info| {
        runtime::crash::report("程序异常(panic)", &info.to_string());
    }));

    // 默认 GUI 模式；--nogui 只启动命令行(黑窗口)
    let nogui = args.iter().any(|arg| arg == "--nogui");

    if !nogui {
        #[cfg(feature = "gui")]
        {
            match gui::run(args.clone()) {
                Ok(()) => return,
                Err(err) => {
                    // 窗口创建失败（服务器/虚拟机常见：缺少可用的 OpenGL）时回退到命令行模式
                    runtime::crash::report("GUI 启动失败", &err);
                    // 所有渲染后端都失败：若本地带了软件 OpenGL(softgl)，用 --softgl 重启自己再试一次
                    if err.contains("后端启动失败")
                        && !runtime::softgl::requested(&args)
                        && runtime::softgl::dir().is_some()
                        && runtime::softgl::relaunch(&args)
                    {
                        runtime::console::eprint_safe(
                            "[Arona] 已用软件 OpenGL(llvmpipe) 模式重新启动管理面板",
                        );
                        return;
                    }
                    runtime::console::eprint_safe(
                        "[Arona] 已回退到命令行模式继续运行（下次可直接加 --nogui 跳过 GUI）",
                    );
                    if runtime::softgl::dir().is_none() {
                        runtime::console::eprint_safe(
                            "[Arona] 提示: 无显卡驱动/无 DX12 的服务器上可先运行 scripts/fetch-softgl.ps1 获取软件 OpenGL",
                        );
                    }
                }
            }
        }
    }

    // 精简构建(--no-default-features)不含 GUI：显式请求 --gui 时给出提示
    #[cfg(not(feature = "gui"))]
    if args.iter().any(|arg| arg == "--gui") {
        runtime::console::eprint_safe(
            "[Arona] 当前构建未包含 GUI（使用了 --no-default-features 构建）。",
        );
    }

    // 含 GUI 的 Windows 产物是 windows 子系统：命令行模式先接回控制台，保证日志/颜色正常
    #[cfg(all(windows, feature = "gui"))]
    runtime::console::attach_console();

    let runtime = match runtime::runtime_builder().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            runtime::console::eprint_safe(&format!("[Arona] 创建 tokio 运行时失败: {err}"));
            return;
        }
    };
    runtime::reactor::set(runtime.handle().clone());
    if let Err(err) = runtime.block_on(run_bot(args, None)) {
        runtime::console::eprint_safe(&format!("[Arona] 运行失败: {err}"));
    }
}

/// 启动阶段的配置问题：写进统一日志 + startup-error.log + 控制台，GUI 模式再弹一个消息框。
fn report_config_problem(file: &std::path::Path, context: &str, err: &str, show_message_box: bool) {
    let hint = format!(
        "{context}\n\n配置文件: {}\n原因: {err}\n\n日志: {}\n\n程序已按默认配置继续运行；修正该文件后会自动热重载。",
        file.display(),
        runtime::crash::startup_error_path().display()
    );
    runtime::crash::report(context, &format!("{} —— {err}", file.display()));
    runtime::console::eprint_safe(&format!("[Arona] {hint}"));
    if show_message_box {
        runtime::crash::message_box("Arona 配置错误", &hint);
    }
}

/// 无法继续运行的配置错误（onebot.yml 坏掉时连不上任何 OneBot 实现）：上报后退出
fn report_config_error(
    file: &std::path::Path,
    context: &str,
    err: &str,
    show_message_box: bool,
) -> ! {
    let hint = format!(
        "{context}\n\n配置文件: {}\n原因: {err}\n\n日志: {}\n\n程序无法启动，请修正该文件后重试；删除该文件可让它重新生成默认模板。",
        file.display(),
        runtime::crash::startup_error_path().display()
    );
    runtime::crash::report(context, &format!("{} —— {err}", file.display()));
    runtime::console::eprint_safe(&format!("[Arona] {hint}"));
    if show_message_box {
        runtime::crash::message_box("Arona 配置错误", &hint);
    }
    std::process::exit(1);
}

/// 机器人主流程；shutdown 为 GUI 关闭窗口时触发的退出信号
pub(crate) async fn run_bot(
    args: Vec<String>,
    shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<(), String> {
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

    // GUI 模式下弹框提示；--nogui 已经有控制台，直接看控制台/日志即可，避免无人值守时被弹框卡住
    let show_message_box = !args.iter().any(|arg| arg == "--nogui");

    // 插件登记阶段：注册功能开关与服务（必须早于 arona.yml 加载，模板注释要完整功能清单）
    plugin::install_all()?;

    // 先加载业务配置（含热更新；首次会从旧 onebot.yaml 迁移 groups/managers 及已登记的插件配置区），再加载协议配置。
    if let Err(err) = config::standalone::init(arona_config_file.clone()) {
        report_config_problem(
            &arona_config_file,
            "arona.yml 加载失败",
            &err,
            show_message_box,
        );
    }
    let onebot_config = match config::onebot::load(&config_file) {
        Ok(config) => config,
        Err(err) => {
            report_config_error(&config_file, "onebot.yml 加载失败", &err, show_message_box)
        }
    };
    runtime::paths::set_onebot_file(config_file.clone());
    runtime::paths::set_arona_file(arona_config_file.clone());
    runtime::config::set_bot_id(onebot_config.self_id);
    runtime::config::set_end_with_sensei("老师".to_string());
    // 聊天记录库随启动建好：留档从第一条消息就有，路径不可写这类问题也当场报在启动日志里，
    // 而不是等第一条消息记账时才暴露
    runtime::chatlog::open(&runtime::paths::chatlog_file());

    // 插件配置：每个插件一份 config/<插件>/arona.yml（旧写法已在上面被接管，这里落盘并加载）
    config::plugin_config::init();

    // 装配阶段：插件登记命令、事件订阅与服务
    plugin::configure_all(onebot_config.clone(), test_notify)?;

    // 启动阶段：数据库、数据预热、定时推送等由插件负责
    plugin::start_all()?;

    let registry = Arc::new(ConnectionRegistry::new());
    let business = Arc::new(StandaloneBusinessHandler::new(
        onebot_config.clone(),
        framework::Framework::global_arc(),
        Arc::new(runtime::dispatcher::CommandDispatcher::new()),
        registry.clone(),
    ));
    let application = Arc::new(OneBotApplication::new(
        onebot_config.clone(),
        business.clone(),
        registry.clone(),
    ));
    // 发布全局句柄：GUI 管理面板与连接热重载使用
    onebot::application::set_global(application.clone());
    application.start();

    // 就绪后设置全局消息发送器（活动推送/预警使用首个可用连接）
    let sender = Arc::new(DeferredMessageSender {
        registry: registry.clone(),
        self_id: onebot_config.self_id,
    });
    runtime::services::set_message_sender(sender);

    runtime::console::print_rule_line("Arona standalone started");
    runtime::console::print_rule_line(&format!("Config: {}", config_file.to_string_lossy()));
    runtime::console::print_rule_line(&format!(
        "Arona 业务配置: {}",
        arona_config_file.to_string_lossy()
    ));
    runtime::console::print_rule_line(&format!(
        "日志文件: {}",
        runtime::paths::logs_dir()
            .join("arona-yyyy-MM-dd.log")
            .to_string_lossy()
    ));

    // 优雅关闭：Ctrl+C 或 GUI 窗口关闭
    match shutdown {
        Some(receiver) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = receiver => {}
            }
        }
        None => {
            tokio::signal::ctrl_c().await.ok();
        }
    }
    runtime::log::info("正在关闭 Arona...");
    application.stop();
    plugin::stop_all();
    quartz::pause_all();
    runtime::console::print_rule_line("Arona standalone stopped");
    Ok(())
}
