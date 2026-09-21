//! 独立模式命令登记（对应原版 standalone/StandaloneCommandDispatcher）

use crate::standalone::api;
use crate::standalone::commands::{self, emergency::EmergencyStop};
use arona::config::onebot::{ConnectionConfig, ConnectionType, OneBotConfig};
use arona::plugin::PluginContext;
use arona::runtime::dispatcher::{
    CommandContext, CommandHandler, CommandRegistration, FallbackHandler, fallback, handler,
};
use arona::runtime::message::OutgoingMessage;
use arona::services;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// 把本插件的全部命令与兜底登记进框架的命令表（configure 阶段调用）。
/// 命令名与别家插件撞车时由框架按优先级裁决并记日志，本插件其余命令照常生效。
pub fn register(ctx: &PluginContext, config: OneBotConfig) {
    ctx.commands(registrations(config, ctx.service_board().clone()));
    ctx.fallback(numeric_reply());
}

/// 本插件的全部命令登记项（装配与自测共用一份，避免两处漂移）
pub fn registrations(
    config: OneBotConfig,
    board: Arc<services::ServiceManager>,
) -> Vec<CommandRegistration> {
    api::register_all(&board);
    let service = |name: &str| {
        board
            .find_by_name(name)
            .unwrap_or_else(|| services::ServiceInfo::new(0, "未注册"))
    };
    let emergency = Arc::new(Mutex::new(EmergencyStop::new(board.clone())));

    let status_config = config.clone();
    let status_board = board.clone();
    let arona_status = plain_arg_handler(move |context, arguments| {
        let config = status_config.clone();
        let board = status_board.clone();
        let arguments = arguments.clone();
        async move {
            let text = handle_arona(&config, &board, &context, arguments).await;
            Some(OutgoingMessage::text(text))
        }
    });
    let help_config = config.clone();
    let help_handler = plain_arg_handler(move |_context, arguments| {
        let _ = arguments;
        let config = help_config.clone();
        async move { Some(OutgoingMessage::text(help_text(&config))) }
    });

    let registrations = vec![
        CommandRegistration::new(
            vec!["/arona".into(), "arona".into()],
            "查看 Arona 状态与帮助",
            arona_status,
        ),
        CommandRegistration::new(
            vec!["/帮助".into(), "/help".into()],
            "查看独立模式帮助",
            help_handler,
        ),
        CommandRegistration::new(
            vec!["/单抽".into(), "gacha_one".into()],
            "单抽一次, 可选服务器: /单抽 日服|国服|国际服",
            commands::guarded_arg_handler(service("抽卡单抽"), |context, arguments| async move {
                commands::gacha_cmd::single_draw(context, arguments).await
            }),
        ),
        CommandRegistration::new(
            vec!["/十连".into(), "gacha_multi".into()],
            "模拟十连, 可选服务器: /十连 日服|国服|国际服",
            commands::guarded_arg_handler(service("抽卡十连"), |context, arguments| async move {
                commands::gacha_cmd::multi_draw(context, arguments).await
            }),
        ),
        CommandRegistration::new(
            vec!["/抽卡服务器".into(), "gacha_server".into()],
            "设置默认抽卡服务器",
            commands::guarded_arg_handler(
                service("抽卡服务器设置"),
                |context, arguments| async move {
                    commands::gacha_cmd::set_server(context, arguments).await
                },
            ),
        ),
        CommandRegistration::new(
            vec!["/狗叫".into(), "gacha_dog".into()],
            "查看抽出pick的人",
            commands::guarded_handler(service("抽卡狗叫查询"), |context| async move {
                commands::gacha_cmd::dog_ranking(context).await
            }),
        ),
        CommandRegistration::new(
            vec!["/历史".into(), "gacha_history".into()],
            "抽卡历史记录",
            commands::guarded_handler(service("抽卡历史查询"), |context| async move {
                commands::gacha_cmd::history_ranking(context).await
            }),
        ),
        CommandRegistration::new(
            vec!["/游戏名".into(), "game_name".into()],
            "记录游戏名与群名的对应关系",
            commands::guarded_arg_handler(service("游戏名记录"), |context, arguments| async move {
                commands::name::game_name(context, arguments).await
            }),
        ),
        CommandRegistration::new(
            vec!["/谁是".into(), "谁叫".into(), "game_name_search".into()],
            "根据游戏名反查群友",
            commands::guarded_arg_handler(service("游戏名反查"), |context, arguments| async move {
                commands::name::search(context, arguments).await
            }),
        ),
        CommandRegistration::new(
            vec!["/叫我".into(), "call_me".into()],
            "给自己自定义昵称",
            commands::guarded_arg_handler(service("自定义昵称"), |context, arguments| async move {
                commands::name::call_me(context, arguments).await
            }),
        ),
        CommandRegistration::new(
            vec!["/塔罗牌".into(), "tarot".into()],
            "抽一张塔罗牌",
            commands::guarded_handler(service("塔罗牌"), |context| async move {
                commands::tarot::tarot(context).await
            }),
        ),
        CommandRegistration::new(
            vec!["/活动".into(), "active".into()],
            "通过wiki获取活动列表",
            commands::guarded_arg_handler(service("活动查询"), |context, arguments| async move {
                commands::activity::activity(context, arguments).await
            }),
        ),
        CommandRegistration::new(
            vec!["/攻略".into(), "trainer".into()],
            "活动攻略/日程笔记/卡池/图片别名查询",
            commands::guarded_arg_handler(
                service("地图与学生攻略"),
                |context, arguments| async move { commands::trainer::trainer(context, arguments).await },
            ),
        ),
        CommandRegistration::new(
            vec!["/紧急停止".into(), "emergency_stop".into()],
            "非管理员投票制停止服务",
            {
                let emergency = emergency.clone();
                commands::guarded_handler(service("紧急停止"), move |context| {
                    let emergency = emergency.clone();
                    async move {
                        let mut guard = emergency.lock().unwrap();
                        guard.vote(&context)
                    }
                })
            },
        ),
        CommandRegistration::new(
            vec!["/config".into(), "config".into()],
            "查看/修改 Arona 配置",
            commands::guarded_arg_handler(service("配置管理"), |context, arguments| async move {
                commands::config_cmd::handle(context, arguments).await
            }),
        ),
        CommandRegistration::new(
            vec!["/抽卡".into(), "gacha".into()],
            "抽卡配置管理",
            commands::guarded_arg_handler(service("抽卡配置"), |context, arguments| async move {
                commands::gacha_cmd::gacha_admin(context, arguments).await
            }),
        ),
        CommandRegistration::new(
            vec!["/任务".into(), "task".into()],
            "查看/触发定时任务(管理员)",
            commands::guarded_arg_handler(service("定时任务"), |_context, arguments| async move {
                Some(commands::task_cmd::handle(&arguments))
            }),
        ),
        CommandRegistration::new(
            vec!["/备份".into(), "backup".into()],
            "备份配置与数据库(管理员)",
            commands::guarded_arg_handler(service("备份恢复"), |_context, arguments| async move {
                Some(commands::backup::backup(&arguments))
            }),
        ),
        CommandRegistration::new(
            vec!["/恢复".into(), "restore".into()],
            "从备份恢复配置与数据库(管理员)",
            commands::guarded_arg_handler(service("备份恢复"), |_context, arguments| async move {
                Some(commands::backup::restore(
                    arguments.first().map(|s| s.as_str()),
                ))
            }),
        ),
    ];
    let registrations: Vec<CommandRegistration> = registrations
        .into_iter()
        .map(|registration| {
            let feature = registration
                .names
                .first()
                .map(|name| feature_for(name))
                .unwrap_or("");
            registration.with_feature(feature)
        })
        .collect();
    registrations
}

/// 未命中任何命令时的兜底：把纯数字回复解析成上一次 /攻略 模糊建议的选项。
/// 这条逻辑原先硬编码在框架的业务处理器里，现在由插件登记自己的 FallbackHandler。
pub fn numeric_reply() -> Arc<dyn FallbackHandler> {
    fallback(|context| async move {
        if let Some(message) =
            crate::standalone::commands::trainer::resolve_numeric_reply(context.clone()).await
        {
            context.reply_message(message).await;
        }
    })
}

/// 自测用：没有框架装配出来的上下文，直接按本插件 id 登进全局命令表
#[cfg(test)]
pub(crate) fn register_into_table(config: OneBotConfig) {
    use arona::runtime::priority::CommandPriority;
    for registration in registrations(config, services::global_board()) {
        arona::runtime::dispatcher::register(crate::PLUGIN_ID, registration);
    }
    arona::runtime::dispatcher::register_fallback(
        crate::PLUGIN_ID,
        numeric_reply(),
        CommandPriority::default(),
    );
}

/// 命令 -> 分群功能开关 key（见 runtime::config::FEATURES）；空串表示不受分群开关限制
fn feature_for(command: &str) -> &'static str {
    match command {
        "/单抽" | "/十连" | "/抽卡服务器" | "/狗叫" | "/历史" | "/抽卡" => "gacha",
        "/游戏名" | "/谁是" | "/叫我" => "name",
        "/塔罗牌" => "tarot",
        "/活动" => "activity",
        "/攻略" => "trainer",
        "/任务" => "task",
        "/备份" | "/恢复" => "backup",
        "/config" => "config",
        "/紧急停止" => "emergency",
        "/帮助" | "/help" => "help",
        _ => "",
    }
}

/// 无守卫的命令包装器（对应 Kotlin 直接 CommandHandler 的命令）
fn plain_arg_handler<F, Fut>(f: F) -> Arc<dyn CommandHandler>
where
    F: Fn(Arc<CommandContext>, Vec<String>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<OutgoingMessage>> + Send + 'static,
{
    let f = Arc::new(f);
    handler(move |context, arguments| {
        let f = f.clone();
        async move {
            let reply = f(context.clone(), arguments).await;
            if let Some(message) = reply {
                context.reply_message(message).await;
            }
            None
        }
    })
}

/// /arona 子命令处理
async fn handle_arona(
    config: &OneBotConfig,
    board: &services::ServiceManager,
    context: &CommandContext,
    arguments: Vec<String>,
) -> String {
    match arguments
        .first()
        .map(|s| s.to_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "" | "status" | "状态" => status_text(config),
        "version" | "版本" => version_text(),
        "help" | "帮助" => help_text(config),
        "service" | "services" | "连接" => {
            handle_service(config, board, context, &arguments).await
        }
        _ => "未知子命令。使用 /arona help 查看帮助。".to_string(),
    }
}

async fn handle_service(
    config: &OneBotConfig,
    board: &services::ServiceManager,
    context: &CommandContext,
    arguments: &[String],
) -> String {
    match arguments
        .get(1)
        .map(|s| s.to_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "list" | "列表" => service_list_text(board),
        "enable" | "启用" => {
            if !context.is_admin {
                return "权限不足".to_string();
            }
            let name = arguments.get(2);
            let Some(name) = name.map(|s| s.as_str()) else {
                return "用法: /arona service enable <名称>".to_string();
            };
            match board.enable(name) {
                Some(service) => format!("服务已启用: {}", service.name),
                None => format!("未找到服务: {name}"),
            }
        }
        "disable" | "停用" => {
            if !context.is_admin {
                return "权限不足".to_string();
            }
            let name = arguments.get(2);
            let Some(name) = name.map(|s| s.as_str()) else {
                return "用法: /arona service disable <名称>".to_string();
            };
            match board.disable(name) {
                Some(service) => format!("服务已停用: {}", service.name),
                None => format!("未找到服务: {name}"),
            }
        }
        _ => connection_text(config),
    }
}

fn status_text(config: &OneBotConfig) -> String {
    let enabled = config
        .connections
        .values()
        .filter(|conn| conn.enable)
        .count();
    let data_root = arona::runtime::services::data_root()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "未设置".to_string());
    format!(
        "Arona 独立模式运行中\n账号: {} ({})\n连接: {enabled}/{} 已启用\n数据目录: {data_root}",
        config.nickname,
        config.self_id,
        config.connections.len()
    )
}

fn help_text(config: &OneBotConfig) -> String {
    let _ = config;
    "Arona 独立模式命令\n/arona status - 查看运行状态\n/arona services - 查看 OneBot 连接\n/arona service list - 查看已注册服务\n/arona version - 查看版本信息\n/单抽 /十连 /狗叫 /历史 /抽卡服务器 - 抽卡\n/游戏名 /谁是 /叫我 - 名字记录\n/塔罗牌 /活动 /攻略 - 娱乐与攻略查询\n/紧急停止 - 投票停止服务\n/config - 查看/修改配置(管理员)\n/任务 /备份 /恢复 - 定时任务与备份恢复(管理员)\n/arona help - 查看本帮助".to_string()
}

fn version_text() -> String {
    format!(
        "Arona 版本信息\n版本: {}\n插件 ID: arona-rs\n名称: arona-rs (Rust 移植版)",
        env!("CARGO_PKG_VERSION")
    )
}

fn connection_text(config: &OneBotConfig) -> String {
    let enabled: Vec<(String, ConnectionConfig)> = config
        .connections
        .iter()
        .filter(|(_, conn)| conn.enable)
        .map(|(name, conn)| (name.clone(), conn.clone()))
        .collect();
    if enabled.is_empty() {
        return "当前没有启用 OneBot 连接。".to_string();
    }
    let mut lines: Vec<String> = Vec::new();
    for (index, (name, connection)) in enabled.iter().enumerate() {
        let conn_type = connection
            .resolve_type(name)
            .unwrap_or(ConnectionType::WebSocket);
        lines.push(format!(
            "{}. {} {}",
            index + 1,
            conn_type.display_name(),
            address(&connection, conn_type)
        ));
    }
    format!("已启用的 OneBot 连接:\n{}", lines.join("\n"))
}

fn address(conn: &ConnectionConfig, conn_type: ConnectionType) -> String {
    match conn_type {
        ConnectionType::WebSocket | ConnectionType::HttpReverse => conn.url.clone(),
        ConnectionType::WebSocketReverse | ConnectionType::Http => {
            format!("{}:{}{}", conn.host, conn.port, conn.path)
        }
    }
}

fn service_list_text(board: &services::ServiceManager) -> String {
    let registered = board.all();
    if registered.is_empty() {
        return "当前没有注册服务".to_string();
    }
    let lines: Vec<String> = registered
        .iter()
        .map(|service| {
            let state = if service.enable.load(Ordering::SeqCst) {
                "已启用"
            } else {
                "已停用"
            };
            format!("{}({}): {state}", service.name, service.id)
        })
        .collect();
    format!("已注册服务:\n{}", lines.join("\n"))
}
