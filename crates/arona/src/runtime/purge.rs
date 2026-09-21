//! 过期数据清理（框架侧的例行清库）
//!
//! 机器人跑久了什么都留得下：聊天记录、图片缓存、临时导出……「每 N 天删掉 M 天前的东西」
//! 是每个带存储的插件都要抄一遍的样板。排期与播报因此收进框架：插件只交出删除动作，
//! 日志挂 `[Arona]` 而不是某家的名字——清库是框架的例行维护，不属于任何玩法。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// 删除动作：清掉过期数据，返回删掉的条数
pub type PurgeFn = Arc<dyn Fn() -> i64 + Send + Sync + 'static>;

/// 已登记的周期（任务名 → `(每几天, 删几天前的)`）：热重载时据此决定要不要重建任务
static SCHEDULES: OnceLock<Mutex<HashMap<String, (i64, i64)>>> = OnceLock::new();

fn schedules() -> &'static Mutex<HashMap<String, (i64, i64)>> {
    SCHEDULES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember(name: &str, period: (i64, i64)) {
    schedules()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(name.to_string(), period);
}

fn forget(name: &str) {
    schedules()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(name);
}

/// 某条清理任务当前登记的周期：`(每几天, 删几天前的)`，没登记过或已取消时 None
pub fn schedule_of(name: &str) -> Option<(i64, i64)> {
    schedules()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(name)
        .copied()
}

/// 周期没变、任务也还在表里时不必重建：`Repeat` 型任务登记时会先跑一次，
/// 重建等于每改一次配置就清一遍库。插件被停用后定时任务整组没了，这里要重新建回来。
fn needs_rebuild(name: &str, period: (i64, i64)) -> bool {
    let unchanged = schedules()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(name)
        .is_some_and(|current| *current == period);
    !(unchanged && crate::quartz::exists(name))
}

/// 登记一条过期数据清理任务：每 `every_days` 天删掉 `keep_days` 天前的数据。
///
/// 同名任务重复登记时，周期没变就什么都不做。`owner` 传插件 id，插件被停用时
/// 框架按归属把定时任务整组回收；配置里关掉时调 [`cancel`]。
pub fn schedule(
    owner: &str,
    name: &str,
    title: &str,
    every_days: i64,
    keep_days: i64,
    purge: PurgeFn,
) {
    let (every_days, keep_days) = (every_days.max(1), keep_days.max(1));
    if !needs_rebuild(name, (every_days, keep_days)) {
        return;
    }
    remember(name, (every_days, keep_days));
    let label = title.to_string();
    let runner = Arc::clone(&purge);
    crate::quartz::create_repeat(
        every_days as u64 * 86_400,
        name,
        owner,
        Arc::new(move || {
            let deleted = runner();
            if deleted > 0 {
                announce(format!(
                    "清理「{label}」: 删掉 {keep_days} 天前的 {deleted} 条"
                ));
            }
        }),
    );
    announce(format!(
        "数据清理「{title}」已启用: 每 {every_days} 天删除 {keep_days} 天前的记录"
    ));
}

/// 取消一条清理任务（配置里关掉清理时用）
pub fn cancel(name: &str) {
    forget(name);
    crate::quartz::remove(name);
}

/// 框架自己的播报：任务体跑在「插件来源」的作用域里，这里显式顶回 `[Arona]`
fn announce(message: String) {
    let _ = crate::runtime::log::with_source("Arona", || crate::runtime::log::info(message));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 同一个周期重复登记（配置热重载）不该重建任务：重建会立刻再清一遍库
    #[tokio::test]
    async fn same_period_is_not_scheduled_twice() {
        static FIRES: AtomicUsize = AtomicUsize::new(0);
        let purge: PurgeFn = Arc::new(|| {
            FIRES.fetch_add(1, Ordering::SeqCst);
            0
        });
        schedule(
            "PurgePlugin",
            "PurgeTestJob",
            "测试清理",
            2,
            1,
            purge.clone(),
        );
        settle().await;
        assert_eq!(FIRES.load(Ordering::SeqCst), 1, "登记时应先跑一次");
        schedule("PurgePlugin", "PurgeTestJob", "测试清理", 2, 1, purge);
        settle().await;
        assert_eq!(FIRES.load(Ordering::SeqCst), 1, "周期没变不该重建");
        assert!(needs_rebuild("PurgeTestJob", (3, 1)), "周期变了要重建");
        cancel("PurgeTestJob");
        assert!(!crate::quartz::exists("PurgeTestJob"));
    }

    /// 让首次「立即执行」的那次触发跑完（当前线程运行时里 await 一次才轮到它）
    async fn settle() {
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    }

    /// 插件停用会把定时任务整组收走，重新启用时必须重建
    #[test]
    fn rebuilds_after_the_job_was_revoked() {
        remember("RevokedJob", (1, 1));
        assert!(
            needs_rebuild("RevokedJob", (1, 1)),
            "任务已不在表里（被回收）时，同周期也要重建"
        );
        forget("RevokedJob");
    }
}
