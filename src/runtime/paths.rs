//! 运行期路径（对应原版 RuntimePaths + StandaloneLogFile 的目录规划）
//! 独立模式数据目录：可执行文件当前目录下 arona-standalone/

use std::path::PathBuf;

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
