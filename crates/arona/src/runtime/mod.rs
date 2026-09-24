//! 运行期模块（对应原版 runtime 包）
pub mod args;
/// 聊天记录缓存：进出记账、旧消息引用还原、按存储 id 撤回
pub mod chatlog;
pub mod config;
pub mod console;
pub mod crash;
pub mod dispatcher;
/// 管理员权限申请（Windows UAC 提权）
pub mod elevate;
/// 框架内置的 HTTP 出口：插件请宿主代发，自己不必带 reqwest
pub mod http;
pub mod log;
pub mod message;
pub mod paths;
/// 执行次序（事件监听与命令匹配共用）
pub mod priority;
/// 过期数据清理：框架统一排期与播报，插件只交删除动作
pub mod purge;
/// 进程 tokio 运行时句柄：GUI 线程等没有上下文的地方靠它投递后台任务
pub mod reactor;
/// 用另一套参数重新拉起自身（软件 OpenGL 兜底 / 渲染后端卡死后换一个）
#[cfg(feature = "gui")]
pub mod relaunch;
pub mod services;
#[cfg(feature = "gui")]
pub mod softgl;
pub mod value;

/// 构造 tokio 多线程运行时（机器人与 GUI 共用）。
///
/// 工作线程数至少 4：`new_multi_thread` 默认等于 CPU 核心数，单核/双核服务器上只有
/// 1~2 个线程，任何一个同步任务（SQLite 查询、文件写入、图片渲染兜底）都会把整条
/// 流水线堵死——收消息、写日志、定时任务全部停摆。多给几个线程，代价只是几个几乎
/// 一直空闲的栈。
pub fn runtime_builder() -> tokio::runtime::Builder {
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(2)
        .max(4);
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(workers).enable_all();
    builder
}
