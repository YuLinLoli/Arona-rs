//! BluearchivePlugin —— 碧蓝档案功能插件。
//!
//! 本 crate 把「功能部分」从 arona 框架里独立出来：抽卡、活动日历、攻略、塔罗、
//! 名字记录、定时任务、备份恢复等命令与它们的数据/图片/数据库都在这里。
//! 框架（OneBot 连接、管理面板、群授权/黑名单、命令分发骨架）通过 [`arona`] 提供，
//! 本插件实现 [`arona::plugin::AronaPlugin`]，由 host 在启动前注册进框架。
//!
//! 开发/调试：把本 crate 与 `arona` 一起放进 workspace，`cargo run -p arona-host`
//! 即可带着插件跑起来（GUI 或 `--nogui`），断点直接打在插件代码里。

// 功能模块都是插件私有；对外只暴露 BluearchivePlugin 这一个入口。
mod activity;
mod config;
mod data;
mod db;
mod entity;
mod gacha;
mod image;
mod runtime;
mod standalone;
mod util;

use arona::plugin::{AronaPlugin, PluginContext, PluginMeta};
use arona::runtime::config::Feature;
use std::sync::atomic::{AtomicI32, Ordering};

/// 碧蓝档案功能插件。
pub struct BluearchivePlugin;

impl BluearchivePlugin {
    pub fn new() -> Self {
        BluearchivePlugin
    }
}

impl Default for BluearchivePlugin {
    fn default() -> Self {
        Self::new()
    }
}

/// 可被分群开关控制的功能清单（key 与 standalone::dispatcher::feature_for 对齐）。
const FEATURES: [Feature; 10] = [
    Feature {
        key: "gacha",
        name: "抽卡",
        description: "单抽/十连/抽卡服务器/狗叫/历史",
    },
    Feature {
        key: "name",
        name: "游戏名",
        description: "游戏名记录/谁是/叫我",
    },
    Feature {
        key: "tarot",
        name: "塔罗牌",
        description: "塔罗牌占卜",
    },
    Feature {
        key: "activity",
        name: "活动",
        description: "活动日历查询与本地日历图",
    },
    Feature {
        key: "trainer",
        name: "攻略",
        description: "活动攻略/日程笔记/当期卡池查询",
    },
    Feature {
        key: "task",
        name: "定时任务",
        description: "查看/触发定时任务(管理员)",
    },
    Feature {
        key: "backup",
        name: "备份恢复",
        description: "备份/恢复配置与数据库(管理员)",
    },
    Feature {
        key: "config",
        name: "配置管理",
        description: "/config 查看与修改配置(管理员)",
    },
    Feature {
        key: "emergency",
        name: "紧急停止",
        description: "非管理员投票制停止服务",
    },
    Feature {
        key: "help",
        name: "帮助",
        description: "帮助与运行状态查询",
    },
];

/// 每日活动推送任务名（与 activity::notify::enable_daily_job 保持一致）
const DAILY_NOTIFY_JOB: &str = "StandaloneActivityNotify";

/// 上一次生效的推送小时（-1 = 尚未初始化）：arona.yml 热重载后据此判断是否需要重建每日任务
static LAST_NOTIFY_HOUR: AtomicI32 = AtomicI32::new(-1);

impl AronaPlugin for BluearchivePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            name: "BluearchivePlugin",
            version: env!("CARGO_PKG_VERSION"),
            description: "碧蓝档案功能插件（抽卡/活动/攻略/塔罗/名字记录/备份恢复）",
        }
    }

    fn install(&self) -> Result<(), String> {
        // 登记功能开关：框架 GUI 的「功能开关」页与 arona.yml 模板注释据此生成
        for feature in FEATURES {
            arona::admin::register_feature(feature);
        }
        // 登记本插件自持有的配置区（notify / trainer）：必须早于框架加载 arona.yml，
        // 否则加载时这两个顶层键会被当成未知键忽略、模板里也不会写出它们的注释块。
        config::register_sections();
        Ok(())
    }

    fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
        // 构建本插件的命令分发器（内部会注册全部服务），交给框架装配业务处理器
        let dispatcher = standalone::dispatcher::build(ctx.onebot_config.clone());
        arona::plugin::set_dispatcher(dispatcher);

        // --test-notify：20 秒后完整跑一次每日推送，便于联调验证
        if ctx.test_notify {
            arona::quartz::create_delay(
                20,
                "TestNotify",
                std::sync::Arc::new(|| {
                    tokio::spawn(async move {
                        activity::notify::push(false).await;
                    });
                }),
            );
            arona::runtime::log::info("测试推送已安排, 将在 20 秒后执行");
        }
        Ok(())
    }

    fn start(&self) {
        arona::runtime::log::info("initializing database...");
        if !db::start() {
            arona::runtime::log::error("database init failed");
        }
        util::tarot::ensure_initialized();

        // 后台预热：kivo 学生数据 + 启动刷新本地资源图片（活动日历图 / 塔罗图）
        tokio::spawn(async move {
            data::kivo::init().await;
        });
        tokio::spawn(async move {
            standalone::commands::activity::refresh_all_images().await;
            standalone::commands::tarot::download_all_images().await;
        });

        // 每日活动推送（含启动 20 秒后的预警初始化）+ 每天 0 点的本地资源图片刷新
        activity::notify::enable_service();
        LAST_NOTIFY_HOUR.store(
            config::notify().every_day_hour.clamp(0, 23),
            Ordering::SeqCst,
        );
        standalone::commands::activity::enable_image_refresh_job();
    }

    fn on_config_reload(&self) {
        // 框架在每次热重载/写入后都会回调这里；只有推送小时真的变了才重建每日任务。
        let hour = config::notify().every_day_hour.clamp(0, 23);
        let previous = LAST_NOTIFY_HOUR.swap(hour, Ordering::SeqCst);
        if previous == hour {
            return;
        }
        // enable_service 在启动阶段已经建过一次；任务还在才需要按新小时重建
        if !arona::quartz::exists(DAILY_NOTIFY_JOB) {
            return;
        }
        arona::quartz::remove(DAILY_NOTIFY_JOB);
        activity::notify::enable_daily_job(hour as u32);
        arona::runtime::log::info(format!("推送小时变更，重建每日任务: 每天 {hour} 点"));
    }

    fn stop(&self) {
        db::close();
    }
}
