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
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};

/// 本插件的稳定 id：框架按它规定落盘位置（`plugins/bluearchive/`、`config/bluearchive/arona.yml`、
/// `data/bluearchive/`），`disabled_plugins` 里也写它。改名等于换一份用户数据，别动。
pub(crate) const PLUGIN_ID: &str = "bluearchive";

/// 本插件的数据目录（data/bluearchive：数据库与备份都在这）
pub(crate) fn data_dir() -> PathBuf {
    arona::runtime::paths::plugin_data_dir(PLUGIN_ID)
}

/// 本插件的图片目录（data/bluearchive/image：活动日历图、抽卡卡面、攻略缓存…）
pub(crate) fn image_dir() -> PathBuf {
    arona::runtime::paths::plugin_image_dir(PLUGIN_ID)
}

/// 本插件的 SQLite 数据库文件（data/bluearchive/arona.db）
pub(crate) fn db_file() -> PathBuf {
    data_dir().join("arona.db")
}

/// 本插件的备份目录（data/bluearchive/backups）
pub(crate) fn backups_dir() -> PathBuf {
    let dir = data_dir().join("backups");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 本插件的配置目录（config/bluearchive：插件自己的 arona.yml 由框架生成，其它随带配置放这里）
pub(crate) fn config_dir() -> PathBuf {
    arona::runtime::paths::plugin_config_dir(PLUGIN_ID)
}

/// 旧版本把图片/数据库/备份放在 `arona-standalone/` 下。升级后一次性**复制**到
/// `data/bluearchive/` 与 `config/bluearchive/`（只补缺、保留旧文件），用户不必重新拉数据。
fn migrate_legacy_dirs() {
    let legacy = arona::runtime::paths::legacy_root();
    if !legacy.is_dir() {
        return;
    }
    let data = data_dir();
    arona::runtime::paths::migrate_dir(&legacy.join("images"), &image_dir());
    for name in ["arona.db", "arona.db-wal", "arona.db-shm"] {
        arona::runtime::paths::migrate_file(&legacy.join("data").join(name), &data.join(name));
    }
    arona::runtime::paths::migrate_dir(&legacy.join("backups"), &backups_dir());
    arona::runtime::paths::migrate_file(
        &legacy.join("trainer_config.yml"),
        &config_dir().join("trainer_config.yml"),
    );
}

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

/// 启动 20 秒后的首次预警任务名（create_delay 固定归在 "Delay" 组，只能按名字取消）
const DAILY_NOTIFY_INIT_JOB: &str = "StandaloneActivityNotifyInit";

/// 上一次生效的推送小时（-1 = 尚未初始化）：arona.yml 热重载后据此判断是否需要重建每日任务
static LAST_NOTIFY_HOUR: AtomicI32 = AtomicI32::new(-1);

impl AronaPlugin for BluearchivePlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta {
            id: PLUGIN_ID,
            name: "BluearchivePlugin",
            version: env!("CARGO_PKG_VERSION"),
            description: "碧蓝档案功能插件（抽卡/活动/攻略/塔罗/名字记录/备份恢复）",
        }
    }

    fn install(&self) -> Result<(), String> {
        // 先把旧目录里的数据补迁到统一位置，后面的建库/读图才找得到东西
        migrate_legacy_dirs();
        // 登记功能开关：框架 GUI 的「功能开关」页与配置模板注释据此生成
        for feature in FEATURES {
            arona::admin::register_feature(feature, PLUGIN_ID);
        }
        // 登记本插件自持有的配置区（notify / trainer）：必须早于框架加载配置，
        // 否则 config/bluearchive/arona.yml 生成不出它们的带注释模板。
        config::register_sections();
        Ok(())
    }

    fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
        // 构建本插件的命令分发器（内部会注册全部服务），交给框架装配业务处理器
        let dispatcher = standalone::dispatcher::build(ctx.onebot_config.clone());
        ctx.set_dispatcher(dispatcher);

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
        // 进程退出与「被禁用」都走这里：先取消本插件的全部定时任务，
        // 不然禁用了还照点在群里推日历/预警。
        for group in [
            DAILY_NOTIFY_JOB,
            "StandaloneActivityNotifyOneHour",
            "AronaActivityImageRefresh",
        ] {
            arona::quartz::remove_group(group);
        }
        arona::quartz::remove(DAILY_NOTIFY_INIT_JOB);
        db::close();
    }
}
