//! 数据库（对应原版 db 包的 DataBaseProvider + BaseDataBase）
//!
//! 原版使用两个 SQLite 文件（arona.db / schale.db），Rust 移植版将独立模式用到的
//! 业务表统一放在数据目录下的 arona.db 中。

use once_cell::sync::OnceCell;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

pub mod dao;

static DB: OnceCell<Mutex<Option<Connection>>> = OnceCell::new();

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS teacher_name (
  grp INTEGER NOT NULL,
  qq INTEGER NOT NULL,
  name TEXT NOT NULL,
  PRIMARY KEY (grp, qq)
);
CREATE TABLE IF NOT EXISTS game_name (
  qq INTEGER PRIMARY KEY,
  name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS gacha_limit (
  qq INTEGER NOT NULL,
  grp INTEGER NOT NULL,
  count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (qq, grp)
);
CREATE TABLE IF NOT EXISTS gacha_history (
  qq INTEGER NOT NULL,
  grp INTEGER NOT NULL,
  pool INTEGER NOT NULL DEFAULT 1,
  points INTEGER NOT NULL DEFAULT 0,
  count3 INTEGER NOT NULL DEFAULT 0,
  dog INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (qq, grp, pool)
);
CREATE TABLE IF NOT EXISTS gacha_user_setting (
  qq INTEGER PRIMARY KEY,
  server TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS gacha_pity (
  qq INTEGER NOT NULL,
  server TEXT NOT NULL,
  count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (qq, server)
);
CREATE TABLE IF NOT EXISTS gacha_pool (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS gacha_character (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  star INTEGER NOT NULL DEFAULT 3,
  is_limit INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS gacha_pool_character (
  pool_id INTEGER NOT NULL,
  character_id INTEGER NOT NULL,
  PRIMARY KEY (pool_id, character_id)
);
CREATE TABLE IF NOT EXISTS activity_calendar (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  server TEXT NOT NULL,
  content TEXT NOT NULL,
  type TEXT NOT NULL,
  start INTEGER NOT NULL,
  end INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tarot (
  number INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  positive TEXT NOT NULL,
  negative TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS tarot_record (
  qq INTEGER NOT NULL,
  grp INTEGER NOT NULL,
  day INTEGER NOT NULL,
  tarot INTEGER NOT NULL,
  positive INTEGER NOT NULL,
  PRIMARY KEY (qq, grp)
);
CREATE TABLE IF NOT EXISTS image (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  path TEXT NOT NULL,
  hash TEXT NOT NULL,
  type INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_activity_server ON activity_calendar(server);
CREATE INDEX IF NOT EXISTS idx_image_name ON image(name);
CREATE INDEX IF NOT EXISTS idx_history_group_pool ON gacha_history(grp, pool);
"#;

/// 初始化数据库连接并建表，返回是否成功
pub fn start() -> bool {
    let path = crate::runtime::paths::database_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = match Connection::open(&path) {
        Ok(conn) => conn,
        Err(err) => {
            crate::runtime::log::error(format!("database open failed: {err}"));
            return false;
        }
    };
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    if let Err(err) = conn.execute_batch(SCHEMA) {
        crate::runtime::log::error(format!("database schema init failed: {err}"));
        return false;
    }
    if let Some(mutex) = DB.get() {
        let mut guard = mutex.lock().unwrap();
        if let Some(old) = guard.take() {
            let _ = old.close();
        }
        *guard = Some(conn);
        return true;
    }
    let _ = DB.set(Mutex::new(Some(conn)));
    true
}

/// 关闭数据库连接（供备份/恢复前释放文件占用）
pub fn close() {
    if let Some(mutex) = DB.get() {
        if let Ok(mut guard) = mutex.lock() {
            if let Some(conn) = guard.take() {
                let _ = conn.close();
            }
        }
    }
}

/// 在数据库连接上执行同步查询。
/// 连接未就绪或执行出错时记录日志并返回 None（对应原版 disconnected 时返回 null 的行为）。
pub fn query<T>(f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
    let mutex = DB.get()?;
    let guard = mutex.lock().ok()?;
    let conn = guard.as_ref()?;
    match f(conn) {
        Ok(value) => Some(value),
        Err(err) => {
            crate::runtime::log::error(format!("database query failed: {err}"));
            None
        }
    }
}

/// 不依赖已存在连接的文件级操作（备份恢复用）
pub fn file_exists() -> bool {
    Path::new(&crate::runtime::paths::database_file()).exists()
}
