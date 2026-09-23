//! 插件作用域（对应 mirai 插件实例自带的 `CoroutineScope`）
//!
//! 插件不许再裸 `tokio::spawn`：那是"关掉插件但它的后台任务还在跑"的根源。
//! 经 [`PluginScope::spawn`] 起跑的任务都被记在插件名下，插件停用（GUI 开关、
//! 配置热重载、进程退出）时由框架一次性取消。
//!
//! 定时任务不走这里：`quartz` 以插件 id 作为任务组，停用时整组摘除（见 [`crate::quartz`]）。
use std::future::Future;
use std::sync::Mutex;
use tokio::task::{AbortHandle, JoinHandle};

/// 能取消/探询的后台任务句柄。用 `AbortHandle` 而不是 `JoinHandle`：
/// 前者可复制，插件拿得到 await 用的 JoinHandle，框架同时留一份取消权。
trait Abortable: Send {
    fn cancel(&self);
    fn is_finished(&self) -> bool;
}

impl Abortable for AbortHandle {
    fn cancel(&self) {
        self.abort();
    }

    fn is_finished(&self) -> bool {
        AbortHandle::is_finished(self)
    }
}

/// 一个插件的后台任务集合
pub struct PluginScope {
    plugin: String,
    tasks: Mutex<Vec<Box<dyn Abortable>>>,
}

impl PluginScope {
    pub(crate) fn new(plugin: &str) -> PluginScope {
        PluginScope {
            plugin: plugin.to_string(),
            tasks: Mutex::new(Vec::new()),
        }
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin
    }

    /// 在本插件作用域内跑后台任务：插件被停用时一并取消
    pub fn spawn<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn_as("后台任务", task)
    }

    /// 同上，但日志前缀细化成 `[插件名:动作]`（如 `[HelloPlugin:定时推送]`）。
    ///
    /// 后台任务会跨 `await` 在 runtime 的工作线程之间搬动，线程局部的来源撑不过一次挂起，
    /// 所以动作名必须随任务一起交给日志外壳、每次轮询重设，而不是在 spawn 前 `with_action`。
    pub fn spawn_as<F>(&self, action: &str, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let handle = crate::runtime::reactor::spawn(crate::runtime::log::Sourced::new(
            Some(crate::plugin::log_source(&self.plugin, action)),
            task,
        ))
        .expect("插件后台任务需要 tokio 运行时（进程启动时登记，见 runtime::reactor）");
        let abort = handle.abort_handle();
        let mut tasks = self.tasks.lock().unwrap();
        // 先丢掉已经结束的，免得长期运行的进程里记账表只增不减
        tasks.retain(|entry| !entry.is_finished());
        tasks.push(Box::new(abort));
        handle
    }

    /// 只关心副作用、不取结果的便捷写法
    pub fn spawn_detached<F>(&self, task: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.spawn(task);
    }

    /// 仍在运行的任务数（诊断用）
    pub fn running_tasks(&self) -> usize {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| !entry.is_finished())
            .count()
    }

    /// 取消全部后台任务，返回取消时仍在运行的数量。框架在插件 `stop()` 之后调用。
    pub fn cancel_all(&self) -> usize {
        let mut tasks = self.tasks.lock().unwrap();
        let running = tasks.iter().filter(|entry| !entry.is_finished()).count();
        for entry in tasks.iter() {
            entry.cancel();
        }
        tasks.clear();
        running
    }
}

impl Drop for PluginScope {
    fn drop(&mut self) {
        // 正常路径由框架显式 cancel_all；这里兜住"插件对象整个消失"的情况
        self.cancel_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// 回归 mirai 式作用域语义：插件停用后它起的后台任务不该继续跑
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_all_stops_every_spawned_task() {
        let scope = PluginScope::new("ScopeTestPlugin");
        let looping = Arc::new(AtomicBool::new(false));
        let slow = Arc::new(AtomicBool::new(false));
        {
            let looping = looping.clone();
            scope.spawn(async move {
                looping.store(true, Ordering::SeqCst);
                loop {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
        }
        {
            let slow = slow.clone();
            scope.spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                slow.store(true, Ordering::SeqCst);
            });
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(looping.load(Ordering::SeqCst), "任务应已被调度");
        assert_eq!(scope.running_tasks(), 2);

        assert_eq!(scope.cancel_all(), 2, "两个任务都该被记账并取消");
        assert_eq!(scope.running_tasks(), 0);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!slow.load(Ordering::SeqCst), "被取消的任务不该在延迟后完成");
    }
}
