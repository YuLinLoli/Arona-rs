//! 运行期路径（对应原版 RuntimePaths + StandaloneLogFile 的目录规划）
//! 独立模式数据目录：可执行文件当前目录下 arona-standalone/

use once_cell::sync::OnceCell;
use std::path::PathBuf;

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

/// arona-standalone 根目录（当前工作目录下）
pub fn standalone_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("arona-standalone")
}

/// 准备根目录并创建 images/data/logs/backups 等子目录
pub fn prepare_standalone_root() -> PathBuf {
    let root = standalone_root();
    for sub in ["images", "data", "logs", "backups"] {
        let _ = std::fs::create_dir_all(root.join(sub));
    }
    root
}

/// 业务数据目录（arona-standalone/data）
pub fn data_root() -> PathBuf {
    prepare_standalone_root().join("data")
}

/// SQLite 数据库文件
pub fn database_file() -> PathBuf {
    data_root().join("arona.db")
}

/// 本地图片目录（塔罗牌等缓存用，本移植版仅保留目录结构）
pub fn images_root() -> PathBuf {
    prepare_standalone_root().join("images")
}

/// onebot.yml
pub fn default_onebot_file() -> PathBuf {
    prepare_standalone_root().join("onebot.yml")
}

/// arona.yml
pub fn default_arona_file() -> PathBuf {
    prepare_standalone_root().join("arona.yml")
}

/// 备份目录
pub fn backups_dir() -> PathBuf {
    prepare_standalone_root().join("backups")
}

/// 日志目录
pub fn logs_dir() -> PathBuf {
    prepare_standalone_root().join("logs")
}
