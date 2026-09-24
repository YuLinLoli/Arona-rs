//! 进程 tokio 运行时的登记处
//!
//! GUI 的事件循环跑在主线程，那里没有 runtime 上下文：在面板上把插件停用再启用时，
//! 重新装配（`configure` + `start`）是在这条线程上同步做的，插件的定时任务与后台任务
//! 一旦走到 `tokio::spawn` 就会炸出「there is no reactor running」。所以后台任务统一
//! 用启动时登记下来的句柄投递，而不是依赖「当前线程恰好有上下文」。
//!
//! 动态插件还要多一层：dll 把自己那份 tokio 静态链接了进去，而上下文是 thread-local 的，
//! 宿主只能给它自己那份打标记。所以 [`spawn`] 在每一帧轮询里按任务自己那份 tokio 短暂补装
//! 一次上下文，否则插件任务里一句 `tokio::time::sleep` 就会在宿主的 worker 线程上 panic。
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

/// 宿主那份登记好的句柄指针，插件 dll 的 [`HostBridge`](crate::plugin::abi::HostBridge)
/// 直接取它；没登记过时返回空指针。
pub extern "C" fn host_handle() -> *const std::ffi::c_void {
    RUNTIME
        .get()
        .map_or(std::ptr::null(), |handle| std::ptr::from_ref(handle).cast())
}

/// dll 侧：把宿主登记的句柄补进自己这份 `RUNTIME`。
///
/// GUI 主线程上没有 runtime 上下文，插件在那条线程上被装配（例如面板上把插件关掉再打开）时，
/// 只有宿主转交进来的这个句柄能让它的 `ctx.spawn` / 定时任务投递成功。
///
/// # Safety
/// `handle` 必须指向宿主 `reactor` 里那份长期有效的 `Handle`，空指针表示宿主也还没登记。
pub unsafe fn adopt_host(handle: *const std::ffi::c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: 约定由调用方保证；Handle 只是个引用计数的句柄，克隆它不多一份所有权
    let _ = RUNTIME.set(unsafe { &*handle.cast::<Handle>() }.clone());
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
    let mut inner = Box::pin(task);
    let context = handle.clone();
    // 每一帧轮询都在本线程上短暂装一次上下文。这不是多余的：投递时进的 guard 管不到 worker
    // 线程，而动态插件 dll 静态链接了自己的那份 tokio——上下文是 thread-local 的，宿主只能
    // 给它自己那份打标记，dll 那份在 worker 线程上始终是空的，任务体里一句
    // `tokio::time::sleep` 就会炸「there is no reactor running」。
    // guard 不能存进 future 跨 await 持有（它不是 `Send`），所以按帧进出。
    Some(handle.spawn(std::future::poll_fn(move |cx| {
        let _context = context.enter();
        inner.as_mut().poll(cx)
    })))
}

/// 给**插件交上来的** future 套一层「每帧补装上下文」，用于所有由宿主 poll 的插件回调边界
/// （命令、类型化命令、兜底、事件钩子、出站钩子）。
///
/// 这些包装代码写在泛型 `impl` 里，会随插件的闭包类型在 **dll 那份** `arona` 中单态化，
/// 于是 [`Handle::enter`] 点亮的是插件自己那份 tokio 的 thread-local；宿主在自己的 worker
/// 线程上 poll 这段 future 时，它里面的 `tokio::time::sleep` / `tokio::select!` 不再看见空上下文。
/// 少了这一层，插件命令体里一句 `sleep` 就有概率炸「there is no reactor running」。
///
/// 拿不到句柄（纯同步测试里）时原样透传，行为与包装前一致。
pub fn scoped<F>(task: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    let context = current();
    let mut inner = Box::pin(task);
    std::future::poll_fn(move |cx| match &context {
        Some(handle) => {
            let _guard = handle.enter();
            inner.as_mut().poll(cx)
        }
        None => inner.as_mut().poll(cx),
    })
}
