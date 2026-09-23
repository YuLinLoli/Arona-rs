//! arona-rs 可执行入口：只装载 arona 框架本体。
//!
//! 启动模式：默认打开管理 GUI；加 `--nogui` 只启动命令行(黑窗口)模式。
//! 含 GUI 的 Windows 构建使用 windows 子系统（GUI 模式不弹控制台），`--nogui`
//! 时会由框架接回上级终端或新建控制台。
//!
//! 本 crate 不链接、也不注册任何功能插件：功能插件是编译成 dll 的独立 crate，
//! 用户在启动前把它放进运行目录的 `plugins/`，框架启动时自动扫描并装配
//! （见 `arona::plugin::dynamic`）。因此这里的产物永远是框架本体，
//! 装了什么插件只取决于用户的磁盘，不取决于这次构建。插件开发见 PLUGIN_DEVELOPMENT.md。

// 含 GUI 时用 windows 子系统，避免 GUI 模式多出一个控制台窗口；测试目标保持控制台。
#![cfg_attr(
    all(windows, feature = "gui", not(test)),
    windows_subsystem = "windows"
)]

#[cfg(test)]
mod dist_test;

fn main() {
    // 交给框架：提权 / 软件渲染兜底 / GUI 或命令行模式 / 扫描 plugins/ 装载插件 / 机器人主流程
    arona::run(std::env::args().collect());
}
