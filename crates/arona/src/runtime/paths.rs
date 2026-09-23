//! 运行期路径规划。根目录是**当前工作目录**（安装包快捷方式已把它设成安装目录），
//! 下面四类目录各管一摊，所有插件一律遵守：
//!
//! ```text
//! <工作目录>/
//!   config/arona.yml          框架配置（授权 / 黑名单 / 分群 / 禁用插件）
//!   config/onebot.yml         OneBot 连接配置
//!   config/<插件>/arona.yml   某个插件自己的配置（如 config/hello/arona.yml）
//!   data/arona/               框架自己的数据（聊天记录缓存 chatlog.db）
//!   data/<插件>/…             某个插件自己的数据（如 data/hello/image、arona.db）
//!   logs/                     按天滚动的日志与 startup-error.log
//!   plugins/<插件>/           某个插件的目录（清单 plugin.yml 与随包资源）
//! ```
//!
//! 插件目录名/配置目录名统一用 [`crate::plugin::PluginMeta::id`]（小写短名），
//! 插件不自己拼路径：一律向框架要 [`plugin_config_file`] / [`plugin_data_dir`] / [`plugin_image_dir`]，
//! 框架因此能把所有插件的落盘位置收敛在 config 与 data 之下。
//!
//! 旧版本把这一切放在 `arona-standalone/` 下。[`prepare`] 首次运行时会把框架认识的
//! 几项**复制**到新位置（旧目录原样保留，用户确认无误后自行删除）；
//! 插件独有的旧目录（图片、数据库、备份）由插件自己在 install 阶段用 [`migrate_file`] /
//! [`migrate_dir`] 迁移，框架不需要知道它们叫什么。

use once_cell::sync::OnceCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 实际使用的 onebot.yml 路径（命令行 --config= 可覆盖，GUI 需要写回同一文件）
static ONE_BOT_FILE: OnceCell<PathBuf> = OnceCell::new();
/// 实际使用的 arona.yml 路径
static ARONA_FILE: OnceCell<PathBuf> = OnceCell::new();

pub fn set_onebot_file(path: PathBuf) {
    let _ = ONE_BOT_FILE.set(path);
}

pub fn set_arona_file(path: PathBuf) {
    let _ = ARONA_FILE.set(path);
}

/// 当前生效的 onebot.yml 路径
pub fn onebot_file() -> PathBuf {
    ONE_BOT_FILE
        .get()
        .cloned()
        .unwrap_or_else(default_onebot_file)
}

/// 当前生效的 arona.yml 路径
pub fn arona_file() -> PathBuf {
    ARONA_FILE.get().cloned().unwrap_or_else(default_arona_file)
}

/// 运行期根目录（当前工作目录）
pub fn root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 创建 config/data/logs/plugins 四类目录，并完成旧目录的一次性迁移；返回根目录。
/// 各路径函数都会先调用它，所以插件不必显式初始化目录。
pub fn prepare() -> PathBuf {
    let root = root();
    for sub in ["config", "data", "logs", "plugins"] {
        let _ = std::fs::create_dir_all(root.join(sub));
    }
    // 只迁一次。用 AtomicBool 而不是 std::sync::Once：迁移过程要写日志，而 log 又会回来取
    // logs_dir()→prepare()，Once 遇到这种重入会永久阻塞（首次运行时整个进程直接卡死）。
    static MIGRATED: AtomicBool = AtomicBool::new(false);
    if !MIGRATED.swap(true, Ordering::SeqCst) {
        migrate_legacy(&root);
    }
    root
}

/// 配置根目录（所有 YAML 都在这下面，含各插件的 config/<插件>/）
pub fn config_root() -> PathBuf {
    prepare().join("config")
}

/// 数据根目录（含各插件的 data/<插件>/；软件渲染兜底的 softgl 也在这里找）
pub fn data_root() -> PathBuf {
    prepare().join("data")
}

/// 日志目录
pub fn logs_dir() -> PathBuf {
    prepare().join("logs")
}

/// 已安装插件的根目录
pub fn plugins_dir() -> PathBuf {
    prepare().join("plugins")
}

/// onebot.yml
pub fn default_onebot_file() -> PathBuf {
    config_root().join("onebot.yml")
}

/// arona.yml（框架自己的）
pub fn default_arona_file() -> PathBuf {
    config_root().join("arona.yml")
}

/// GUI 外观偏好文件
pub fn gui_preference_file() -> PathBuf {
    config_root().join("gui.txt")
}

// ==================== 插件目录接口 ====================

/// 框架自己的数据目录（data/arona）：聊天记录缓存等框架级存储放这里，
/// 与插件的 data/<插件>/ 分开，删插件不会带走框架的数据，反之亦然
pub fn framework_data_dir() -> PathBuf {
    let dir = data_root().join("arona");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 框架侧聊天记录库（data/arona/chatlog.db）
pub fn chatlog_file() -> PathBuf {
    framework_data_dir().join("chatlog.db")
}

/// 某个插件的目录（plugins/<id>，放清单与随包资源）
pub fn plugin_dir(plugin: &str) -> PathBuf {
    let dir = plugins_dir().join(plugin);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 某个插件的配置目录（config/<id>）
pub fn plugin_config_dir(plugin: &str) -> PathBuf {
    let dir = config_root().join(plugin);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 某个插件的配置文件（config/<id>/arona.yml）
pub fn plugin_config_file(plugin: &str) -> PathBuf {
    plugin_config_dir(plugin).join("arona.yml")
}

/// 某个插件的数据目录（data/<id>）
pub fn plugin_data_dir(plugin: &str) -> PathBuf {
    let dir = data_root().join(plugin);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 某个插件的图片目录（data/<id>/image）：本地缓存图与渲染出的结果图都放这里
pub fn plugin_image_dir(plugin: &str) -> PathBuf {
    let dir = plugin_data_dir(plugin).join("image");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ==================== 旧目录（arona-standalone/）一次性迁移 ====================

/// 旧版本的数据根目录：只用于迁移，新代码不要再往里写东西
pub fn legacy_root() -> PathBuf {
    root().join("arona-standalone")
}

/// 把旧位置的文件补到新位置：目标已存在或源不存在时什么都不做（不删源文件）
pub fn migrate_file(from: &Path, to: &Path) -> bool {
    if !from.is_file() || to.exists() {
        return false;
    }
    let Some(parent) = to.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    if std::fs::copy(from, to).is_err() {
        return false;
    }
    crate::runtime::log::info(format!(
        "已迁移旧文件: {} -> {}",
        from.display(),
        to.display()
    ));
    true
}

/// 把旧目录下的整棵子树补到新目录（已存在的文件跳过），返回复制的文件数
pub fn migrate_dir(from: &Path, to: &Path) -> usize {
    if !from.is_dir() {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(from) else {
        return 0;
    };
    let mut copied = 0;
    for entry in entries.flatten() {
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copied += migrate_dir(&source, &target);
        } else if migrate_file(&source, &target) {
            copied += 1;
        }
    }
    copied
}

/// 框架自身文件的迁移：配置两份、GUI 偏好、历史日志
/// 参数是 [`prepare`] 已经算好的根目录：这里不能再走 config_root()/logs_dir()，
/// 它们会回到 prepare()，而迁移正发生在 prepare() 里面。
fn migrate_legacy(root: &Path) {
    let legacy = root.join("arona-standalone");
    if !legacy.is_dir() {
        return;
    }
    let config = root.join("config");
    // 旧后缀 .yaml 一并接管：新位置只认 .yml，不迁就等于丢配置
    for (from, name) in [
        ("arona.yml", "arona.yml"),
        ("arona.yaml", "arona.yml"),
        ("onebot.yml", "onebot.yml"),
        ("onebot.yaml", "onebot.yml"),
        ("gui.txt", "gui.txt"),
    ] {
        migrate_file(&legacy.join(from), &config.join(name));
    }
    migrate_dir(&legacy.join("logs"), &root.join("logs"));
}
