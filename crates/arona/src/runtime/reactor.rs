//! 进程 tokio 运行时的登记处
//!
//! GUI 的事件循环跑在主线程，那里没有 runtime 上下文：在面板上把插件停用再启用时，
//! 重新装配（`configure` + `start`）是在这条线程上同步做的，插件的定时任务与后台任务
//! 一旦走到 `tokio::spawn` 就会炸出「there is no reactor running」。所以后台任务统一
//! 用启动时登记下来的句柄投递，而不是依赖「当前线程恰好有上下文」。
use std::future::Future;
use std::sync::OnceLock;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

static RUNTIME: OnceLock<Handle> = OnceLock::new();

/// 运行时建好后登记一次（`main` 与 `gui::run` 各自只走一条）
pub fn set(handle: Handle) {
    let _ = RUNTIME.set(handle);
}

/// 可用于投递后台任务的句柄：当前线程有上下文就用它，否则退回进程登记的那个
/// （顺序反过来的话，测试与机器人线程自带的 runtime 会被全局句柄抢走）
pub fn current() -> Option<Handle> {
    Handle::try_current()
        .ok()
        .or_else(|| RUNTIME.get().cloned())
}

/// 在进程运行时上跑一个后台任务；确实没有运行时（纯同步测试）时返回 None
pub fn spawn<F>(task: F) -> Option<JoinHandle<F::Output>>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = current()?;
    // 任务体里再调 tokio::time::sleep 等也需要上下文，进入 guard 后再投递
    let _guard = handle.enter();
    Some(handle.spawn(task))
}
