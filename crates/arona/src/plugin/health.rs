//! 插件健康度与 panic 隔离（对应 mirai 的 `broadcastAndDumpInterceptedExceptions`）
//!
//! 插件代码是外来的：它可能 panic。改造前一条钩子 panic 会让本次广播的后续钩子、
//! 乃至整条消息的命令分发一起丢掉，而且日志里只有一句裸 panic。现在插件的每个入口
//! 都被框架兜住，panic 记在**归属插件**名下，其余处理器照常跑；同一插件连续 panic
//! 到阈值（[`crate::framework::FrameworkOptions::panic_disable_threshold`]）时由框架
//! 把它隔离停用，不再让它继续捣乱。
//!
//! 兜法按入口形态分三处，记账统一走这张 [`HealthBoard`]：
//! - [`guarded`]：异步入口 —— 命令处理器与命令兜底（`runtime::dispatcher`）、
//!   入站事件钩子与出站钩子（`onebot::hooks`）
//! - `plugin::manager` 的 `guarded_call` / `guard_void`：同步生命周期回调。
//!   install/configure/start 的 panic 折算成该阶段失败并走回收；stop/on_config_reload
//!   只记日志，绝不中断框架后续的回收流程
//! - `quartz` 的 `TaskEntry::invoke`：定时任务 panic 只跳过本次，继续按表调度
use crate::framework::Framework;
use crate::runtime::config::Gating;
use crate::runtime::message::BoxFuture;
use std::collections::HashMap;
use std::future::poll_fn;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// 跑一个可能 panic 的插件 future：返回 `None` 表示它 panic 了（值被丢弃）
///
/// 用 `poll_fn` 手写而不是 `futures::FutureExt::catch_unwind`，是因为后者的
/// `Self: UnwindSafe` 约束对 `Pin<Box<dyn Future + Send>>` 这种 trait object 不成立。
/// 每次 poll 都把日志来源切成 `[插件名:动作]`：插件处理器里打的日志因此说得出是谁、
/// 在干哪件事，而不是和框架日志混成一片 `[Arona]`。
pub async fn guarded<'a, T>(plugin: &str, action: &str, future: BoxFuture<'a, T>) -> Option<T>
where
    T: Send + 'static,
{
    use std::task::Poll;

    let source = crate::plugin::log_source(plugin, action);
    let mut pending = Some(future);
    poll_fn(|context| {
        let Some(pinned) = pending.as_mut() else {
            // 已完成后再被轮询：当作 panic 处理，不能让 executor 拿到未定义的值
            return Poll::Ready(None);
        };
        // 这里的 AssertUnwindSafe 是安全的：panic 之后这个半路断掉的 future 会被直接丢弃，
        // 我们再也不会去 poll 它
        match crate::runtime::log::with_source(&source, || {
            std::panic::catch_unwind(AssertUnwindSafe(|| pinned.as_mut().poll(context)))
        }) {
            Ok(Poll::Ready(value)) => Poll::Ready(Some(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(None),
        }
    })
    .await
}

/// 每张插件契约表共用一份的健康度记账
pub struct HealthBoard {
    /// 连续 panic 次数（成功跑完一次就清零）
    failures: Mutex<HashMap<String, u32>>,
    threshold: AtomicU32,
    /// 停用名单：达阈值时先把插件从路由上摘下来，再走框架的完整回收
    gating: Arc<Gating>,
    framework: OnceLock<Weak<Framework>>,
}

impl HealthBoard {
    pub(crate) fn new(threshold: u32, gating: Arc<Gating>) -> HealthBoard {
        HealthBoard {
            failures: Mutex::new(HashMap::new()),
            threshold: AtomicU32::new(threshold),
            gating,
            framework: OnceLock::new(),
        }
    }

    /// 由 `Framework::new` 在整套注册表构造完成后回填（和 PluginManager 同一个手法）
    pub(crate) fn attach(&self, framework: &Arc<Framework>) {
        let _ = self.framework.set(Arc::downgrade(framework));
    }

    /// 连续多少次 panic 后隔离停用该插件（0 表示不因 panic 停用）
    pub fn threshold(&self) -> u32 {
        self.threshold.load(Ordering::SeqCst)
    }

    pub fn set_threshold(&self, threshold: u32) {
        self.threshold.store(threshold, Ordering::SeqCst);
    }

    /// 记一次成功：清零该插件的连续 panic 计数
    pub fn record_success(&self, plugin: &str) {
        let mut failures = self.failures.lock().unwrap();
        if failures.remove(plugin).is_some() {
            // 只在之前记过失败时才说一声，避免每次事件都刷日志
            crate::runtime::log::debug(format!("插件 {plugin} 恢复正常"));
        }
    }

    /// 记一次 panic。返回 true 表示已达阈值、框架已把该插件隔离停用。
    pub fn record_panic(&self, plugin: &str, site: &str) -> bool {
        let threshold = self.threshold();
        let count = {
            let mut failures = self.failures.lock().unwrap();
            let count = failures.entry(plugin.to_string()).or_insert(0);
            *count += 1;
            *count
        };
        crate::runtime::log::error(format!(
            "插件 {plugin} 的{site}panic（连续第 {count} 次），本次已跳过它并继续跑其余处理器"
        ));
        if threshold == 0 || count < threshold {
            return false;
        }
        self.failures.lock().unwrap().remove(plugin);
        // 先进停用名单：这一步就让它的命令与钩子从路由上消失，不必等下面的完整回收
        let mut list = self.gating.disabled_plugins();
        if !list.iter().any(|entry| entry == plugin) {
            list.push(plugin.to_string());
            self.gating.set_disabled_plugins(list);
        }
        match self.framework.get().and_then(|weak| weak.upgrade()) {
            Some(framework) => framework.quarantine(plugin, threshold),
            // 没回填框架实例（单独一张注册表/测试）时，门控层面的停用已经生效
            None => {
                crate::runtime::log::error(format!(
                    "插件 {plugin} 连续 {threshold} 次 panic，已停用其命令与事件订阅"
                ));
                true
            }
        }
    }

    /// 某插件当前的连续 panic 次数（诊断/GUI 用）
    pub fn failures(&self, plugin: &str) -> u32 {
        self.failures
            .lock()
            .unwrap()
            .get(plugin)
            .copied()
            .unwrap_or(0)
    }

    /// 撤销某插件的记账（停用时清干净）
    pub fn forget(&self, plugin: &str) {
        self.failures.lock().unwrap().remove(plugin);
    }
}

impl Default for HealthBoard {
    fn default() -> Self {
        HealthBoard::new(
            Framework::DEFAULT_PANIC_THRESHOLD,
            Arc::new(Gating::default()),
        )
    }
}
