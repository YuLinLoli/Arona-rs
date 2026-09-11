//! 运行期模块（对应原版 runtime 包）
pub mod config;
pub mod console;
pub mod crash;
pub mod dispatcher;
/// 管理员权限申请（Windows UAC 提权）
pub mod elevate;
pub mod gacha_config;
pub mod log;
pub mod message;
pub mod paths;
pub mod services;
#[cfg(feature = "gui")]
pub mod softgl;
pub mod tarot_config;
pub mod value;
