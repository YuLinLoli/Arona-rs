//! 运行期模块（对应原版 runtime 包）
pub mod args;
/// 框架随包资源：安装到运行目录的那份中文字体，GUI 与所有出图插件共用同一个出口
pub mod assets;
/// 带有效期的读缓存原语：远端事实（身份、资料）按需读一次就靠它留住
pub mod cache;
/// 聊天记录缓存：进出记账、旧消息引用还原、按存储 id 撤回
pub mod chatlog;
pub mod config;
pub mod console;
pub mod crash;
/// 原生「打开文件」对话框：只有 GUI 的「升级插件」用它
#[cfg(feature = "gui")]
pub mod dialog;
pub mod dispatcher;
/// 管理员权限申请（Windows UAC 提权）
pub mod elevate;
/// 框架内置的 HTTP 出口：插件请宿主代发，自己不必带 reqwest
pub mod http;
pub mod log;
/// 进程自己的内存占用（面板右上角那一格，5 秒一采样）
pub mod memory;
pub mod message;
pub mod paths;
/// 远端资料的按需读缓存（群成员/陌生人），挂在框架实例上
pub mod peers;
/// 平台体检：发行版信息与运行期动态库依赖（Linux 上是实活，其它平台是空实现）
pub mod platform;
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
/// 惰性字面：出图插件共用的「一份字体字节 + 有界字形缓存」，见模块头注释
pub mod text;
pub mod value;

/// 构造 tokio 多线程运行时（机器人与 GUI 共用）。
///
/// 工作线程数至少 4：`new_multi_thread` 默认等于 CPU 核心数，单核/双核服务器上只有
/// 1~2 个线程，任何一个同步任务（SQLite 查询、文件写入、图片渲染兜底）都会把整条
/// 流水线堵死——收消息、写日志、定时任务全部停摆。多给几个线程，代价只是几个几乎
/// 一直空闲的栈。
///
/// 再按 `plugins/` 里待装载的插件数每家加两个：定时任务体是**同步**调用的
/// （`quartz` 在 async 循环里直接跑插件闭包），一家插件卡住就吃掉一个 worker，而
/// `worker_threads` 建好之后改不了。留一份跑、一份等，剩下的仍归事件与代发用。
/// 这一步只能扫目录：runtime 就在这条线上建，插件要等它建好之后才装载。
pub fn runtime_builder() -> tokio::runtime::Builder {
    let cores = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(2);
    let plugins = crate::plugin::dynamic::discover(&paths::plugins_dir()).len();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder
        .worker_threads(worker_count(cores, plugins))
        .enable_all();
    builder
}

/// worker 数：核心数与「按插件数留的余量」取大，余量本身封顶在 32。
///
/// 封顶只能作用在余量上：对结果封顶会把高核心机器的既有档位压下去（64 核被砍成 32），
/// 那是纯粹的退化。
fn worker_count(cores: usize, plugins: usize) -> usize {
    const MIN_WORKERS: usize = 4;
    const MAX_HEADROOM: usize = 32;
    cores.max((MIN_WORKERS + 2 * plugins).min(MAX_HEADROOM))
}

#[cfg(test)]
mod tests {
    use super::worker_count;

    /// 表驱动钉住三个方向：低核机器补到下限、插件多按家加线程、高核机器**不许被压低**。
    /// 最后那条是这条算式最容易写错的地方——对结果封顶就会把 64 核砍成 32。
    #[test]
    fn plugin_headroom_never_lowers_a_high_core_machine() {
        for (cores, plugins, want) in [
            (1, 0, 4),
            (2, 0, 4),
            (4, 0, 4),
            (2, 1, 6),
            (4, 2, 8),
            (8, 3, 10),
            (16, 20, 32),
            (64, 0, 64),
            (64, 20, 64),
        ] {
            assert_eq!(
                worker_count(cores, plugins),
                want,
                "{cores} 核 / {plugins} 个插件"
            );
        }
    }
}
