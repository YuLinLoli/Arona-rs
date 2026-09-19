//! arona-rs 可执行入口：装载 arona 框架 + plugins.toml 里登记的插件。
//!
//! 启动模式：默认打开管理 GUI；加 `--nogui` 只启动命令行(黑窗口)模式。
//! 含 GUI 的 Windows 构建使用 windows 子系统（GUI 模式不弹控制台），`--nogui`
//! 时会由框架接回上级终端或新建控制台。
//!
//! 想换/加功能包，只改仓库根目录的 `plugins.toml`（外加 host 的 Cargo 依赖）即可，
//! `register_plugins()` 由 build.rs 依据该清单自动生成，本文件不再硬编码任何插件。

// 含 GUI 时用 windows 子系统，避免 GUI 模式多出一个控制台窗口；测试目标保持控制台。
#![cfg_attr(
    all(windows, feature = "gui", not(test)),
    windows_subsystem = "windows"
)]

#[cfg(test)]
mod dist_test;

// build.rs 依据 plugins.toml 生成的 `register_plugins()`（提供本 crate 的插件注册入口）。
include!(concat!(env!("OUT_DIR"), "/plugins.rs"));

fn main() {
    // 注册功能插件（编译期静态注册到框架全局）
    register_plugins();
    // 交给框架：提权 / 软件渲染兜底 / GUI 或命令行模式 / 机器人主流程
    arona::run(std::env::args().collect());
}
