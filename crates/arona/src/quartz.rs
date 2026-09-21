//! 轻量定时任务（对应原版 quartz/QuartzProvider + StandaloneTaskCommand 需要的能力）
//!
//! 原版基于 Quartz 支持完整 cron；独立模式只用到了三种形态：
//! 每天固定小时、固定间隔循环（首次立即执行）、单次延时/定时。这里用 tokio 实现同等语义，
//! 任务以名称注册，支持列出、手动触发、暂停/恢复与删除。

use crate::plugin::health::HealthBoard;
use chrono::{DateTime, Local, Timelike};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};
use std::time::Duration;

pub type JobFn = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone)]
enum TaskKind {
    /// 每天 hour(0-23) 触发一次
    Daily { hour: u32 },
    /// 每 interval_secs 触发一次（首次立即执行）
    Repeat { interval_secs: u64 },
    /// 单次：在指定毫秒时间戳触发
    SingleAt { ts_ms: i64 },
}

pub struct TaskInfo {
    pub name: String,
    pub group: String,
    pub next_fire_ms: Option<i64>,
    pub last_fire_ms: Option<i64>,
}

struct TaskEntry {
    name: String,
    group: String,
    kind: TaskKind,
    run: JobFn,
    /// 归属插件的健康度记账（拿不到框架实例时为 None，只吞 panic 不停用）
    health: Option<Arc<HealthBoard>>,
    paused: AtomicBool,
    canceled: AtomicBool,
    last_fire: RwLock<Option<i64>>,
    next_fire: RwLock<Option<i64>>,
}

impl TaskEntry {
    /// 跑一次任务体，返回是否发生了 panic。
    ///
    /// 不吞的话异常会顺着循环把整个 tokio 任务打死，而表里还留着一条
    /// 看起来正常、永远不会再触发的僵尸任务。
    fn invoke(&self) -> bool {
        // 任务体属于插件：它打的日志挂 `[插件名:定时 任务名]`，不混进 [Arona]
        let source = crate::plugin::log_source(&self.group, &format!("定时 {}", self.name));
        let outcome = crate::runtime::log::with_source(&source, || {
            std::panic::catch_unwind(AssertUnwindSafe(|| (self.run)()))
        });
        if outcome.is_ok() {
            return false;
        }
        crate::runtime::log::error(format!(
            "定时任务「{}」的处理器 panic，本次已跳过并继续按表调度",
            self.name
        ));
        if let Some(health) = &self.health {
            // group 就是归属插件 id：连续 panic 到阈值同样隔离停用
            health.record_panic(&self.group, "定时任务");
        }
        true
    }

    fn schedule_cycle(self: &Arc<Self>) {
        let entry = self.clone();
        let name = self.name.clone();
        // 走进程登记的运行时：GUI 主线程上重新装配插件时（停用→启用）这里没有上下文，
        // 裸 tokio::spawn 会当场 panic「there is no reactor running」
        if crate::runtime::reactor::spawn(async move {
            loop {
                let now_ms = chrono::Utc::now().timestamp_millis();
                let Some(delay_ms) = entry.delay_until_next(now_ms) else {
                    return;
                };
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms as u64)).await;
                }
                if entry.canceled.load(Ordering::SeqCst) {
                    return;
                }
                if entry.paused.load(Ordering::SeqCst) {
                    // 暂停时保持等待，直到恢复或下一轮
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    continue;
                }
                let fired_at = chrono::Utc::now().timestamp_millis();
                {
                    let mut last = entry.last_fire.write().unwrap();
                    *last = Some(fired_at);
                }
                entry.invoke();
                let after = chrono::Utc::now().timestamp_millis();
                if let Some(next) = entry.compute_next_after(after) {
                    let mut guard = entry.next_fire.write().unwrap();
                    *guard = Some(next);
                } else {
                    let mut guard = entry.next_fire.write().unwrap();
                    *guard = None;
                    return;
                }
            }
        })
        .is_none()
        {
            crate::runtime::log::warning(format!(
                "定时任务「{name}」未能启动：tokio 运行时还没就绪"
            ));
        }
    }

    /// 返回距下一次触发需要等待的毫秒数
    fn delay_until_next(&self, now_ms: i64) -> Option<i64> {
        match &self.kind {
            TaskKind::Daily { hour } => {
                let next = next_daily_ms(*hour, now_ms);
                {
                    let mut guard = self.next_fire.write().unwrap();
                    *guard = Some(next);
                }
                Some(next - now_ms)
            }
            TaskKind::Repeat { interval_secs } => {
                let interval = *interval_secs as i64 * 1000;
                let last = self.last_fire.read().unwrap().unwrap_or(0);
                let delay = if last == 0 {
                    0
                } else {
                    (last + interval) - now_ms
                };
                Some(delay.max(0))
            }
            TaskKind::SingleAt { ts_ms } => {
                {
                    let mut guard = self.next_fire.write().unwrap();
                    *guard = Some(*ts_ms);
                }
                if now_ms >= *ts_ms {
                    None
                } else {
                    Some(*ts_ms - now_ms)
                }
            }
        }
    }

    fn compute_next_after(&self, after_ms: i64) -> Option<i64> {
        match &self.kind {
            TaskKind::Daily { hour } => Some(next_daily_ms(*hour, after_ms)),
            TaskKind::Repeat { interval_secs } => Some(after_ms + *interval_secs as i64 * 1000),
            TaskKind::SingleAt { .. } => None,
        }
    }
}

/// 计算下一次到达 hour 的毫秒时间戳
fn next_daily_ms(hour: u32, now_ms: i64) -> i64 {
    let now: DateTime<Local> = DateTime::from_timestamp_millis(now_ms)
        .map(|d| d.with_timezone(&Local))
        .unwrap_or_else(Local::now);
    let candidate = now
        .with_hour(hour)
        .and_then(|d| d.with_minute(0))
        .and_then(|d| d.with_second(0))
        .and_then(|d| d.with_nanosecond(0));
    let candidate = candidate.unwrap_or(now);
    let candidate_ms = candidate.timestamp_millis();
    if candidate_ms <= now_ms {
        // 明天同一时刻
        let tomorrow = candidate + chrono::Duration::days(1);
        tomorrow.timestamp_millis()
    } else {
        candidate_ms
    }
}

/// 定时任务表：任务按名称注册、按组（= 插件 id）整体回收。
/// 表实例由 [`crate::framework::Framework`] 持有，本模块的自由函数走进程默认实例。
#[derive(Default)]
pub struct Scheduler {
    tasks: RwLock<HashMap<String, Arc<TaskEntry>>>,
    /// 回填后，任务 panic 才能记到归属插件的健康度上（见 [`Scheduler::attach`]）
    framework: OnceLock<Weak<crate::framework::Framework>>,
}

impl Scheduler {
    /// 由 `Framework::new` 在整套注册表构造完成后回填（与 PluginManager/HealthBoard 同一手法）
    pub(crate) fn attach(&self, framework: &Arc<crate::framework::Framework>) {
        let _ = self.framework.set(Arc::downgrade(framework));
    }

    fn register(&self, name: &str, group: &str, kind: TaskKind, run: JobFn) -> Arc<TaskEntry> {
        let existing = self.tasks.read().unwrap().get(name).cloned();
        if let Some(entry) = existing {
            entry.canceled.store(true, Ordering::SeqCst);
        }
        let health = self
            .framework
            .get()
            .and_then(|weak| weak.upgrade())
            .map(|framework| framework.health().clone());
        let entry = Arc::new(TaskEntry {
            name: name.to_string(),
            group: group.to_string(),
            kind,
            run,
            health,
            paused: AtomicBool::new(false),
            canceled: AtomicBool::new(false),
            last_fire: RwLock::new(None),
            next_fire: RwLock::new(None),
        });
        self.tasks
            .write()
            .unwrap()
            .insert(name.to_string(), entry.clone());
        entry.schedule_cycle();
        entry
    }

    /// 创建每天固定小时触发的任务（同名存在则替换）
    pub fn create_daily(&self, hour: u32, name: &str, group: &str, run: JobFn) {
        self.register(name, group, TaskKind::Daily { hour }, run);
    }

    /// 创建固定间隔循环任务（首次立即执行）。间隔至少 1 秒：传 0 会退化成空转刷屏的死循环。
    pub fn create_repeat(&self, interval_secs: u64, name: &str, group: &str, run: JobFn) {
        self.register(
            name,
            group,
            TaskKind::Repeat {
                interval_secs: interval_secs.max(1),
            },
            run,
        );
    }

    /// 创建单次定时任务
    pub fn create_single_at(&self, ts_ms: i64, name: &str, group: &str, run: JobFn) {
        self.register(name, group, TaskKind::SingleAt { ts_ms }, run);
    }

    /// 创建延迟任务（秒）。group 传插件 id，插件停用时整组取消。
    pub fn create_delay(&self, delay_secs: u64, name: &str, group: &str, run: JobFn) {
        let ts = chrono::Utc::now().timestamp_millis() + delay_secs as i64 * 1000;
        self.register(name, group, TaskKind::SingleAt { ts_ms: ts }, run);
    }

    pub fn exists(&self, name: &str) -> bool {
        self.tasks.read().unwrap().contains_key(name)
    }

    pub fn remove(&self, name: &str) -> bool {
        if let Some(entry) = self.tasks.write().unwrap().remove(name) {
            entry.canceled.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// 按任务组整体取消（插件被禁用时用：一次停掉自己全部的定时任务，不必逐个记名字）
    pub fn remove_group(&self, group: &str) -> usize {
        let removed: Vec<Arc<TaskEntry>> = self
            .tasks
            .write()
            .unwrap()
            .extract_if(|_, entry| entry.group == group)
            .map(|(_, entry)| entry)
            .collect();
        let count = removed.len();
        for entry in removed {
            entry.canceled.store(true, Ordering::SeqCst);
        }
        count
    }

    pub fn trigger(&self, name: &str) -> Result<(), String> {
        let entry = self
            .tasks
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("任务不存在: {name}"))?;
        crate::runtime::reactor::spawn(async move {
            // 手动触发同样要过隔离：这里以前是裸调 run()，插件 panic 会打死临时任务
            entry.invoke();
        })
        .map(|_| ())
        .ok_or_else(|| "tokio 运行时还没就绪，无法触发任务".to_string())
    }

    pub fn pause_all(&self) {
        let tasks = self.tasks.read().unwrap();
        for entry in tasks.values() {
            entry.paused.store(true, Ordering::SeqCst);
        }
    }

    pub fn resume_all(&self) {
        let tasks = self.tasks.read().unwrap();
        for entry in tasks.values() {
            entry.paused.store(false, Ordering::SeqCst);
        }
    }

    pub fn list(&self) -> Vec<TaskInfo> {
        let tasks = self.tasks.read().unwrap();
        let mut out: Vec<TaskInfo> = tasks
            .values()
            .map(|entry| TaskInfo {
                name: entry.name.clone(),
                group: entry.group.clone(),
                next_fire_ms: *entry.next_fire.read().unwrap(),
                last_fire_ms: *entry.last_fire.read().unwrap(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

fn scheduler() -> &'static Scheduler {
    crate::framework::Framework::global().jobs()
}

/// 创建每天固定小时触发的任务（同名存在则替换）
pub fn create_daily(hour: u32, name: &str, group: &str, run: JobFn) {
    scheduler().create_daily(hour, name, group, run);
}

/// 创建固定间隔循环任务（首次立即执行）
pub fn create_repeat(interval_secs: u64, name: &str, group: &str, run: JobFn) {
    scheduler().create_repeat(interval_secs, name, group, run);
}

/// 创建单次定时任务
pub fn create_single_at(ts_ms: i64, name: &str, group: &str, run: JobFn) {
    scheduler().create_single_at(ts_ms, name, group, run);
}

/// 创建延迟任务（秒）。group 传插件 id，插件停用时整组取消。
pub fn create_delay(delay_secs: u64, name: &str, group: &str, run: JobFn) {
    scheduler().create_delay(delay_secs, name, group, run);
}

pub fn exists(name: &str) -> bool {
    scheduler().exists(name)
}

pub fn remove(name: &str) -> bool {
    scheduler().remove(name)
}

/// 按任务组整体取消（插件被禁用时用）
pub fn remove_group(group: &str) -> usize {
    scheduler().remove_group(group)
}

pub fn trigger(name: &str) -> Result<(), String> {
    scheduler().trigger(name)
}

pub fn pause_all() {
    scheduler().pause_all();
}

pub fn resume_all() {
    scheduler().resume_all();
}

pub fn list() -> Vec<TaskInfo> {
    scheduler().list()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// 任务体 panic 只废掉那一次触发：循环必须继续按表走。
    /// 回归点：以前 panic 会顺着 async 块打死整个 tokio 任务，
    /// 表里却留一条状态正常、永远不会再触发的僵尸任务。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panicking_job_keeps_being_scheduled() {
        static FIRES: AtomicUsize = AtomicUsize::new(0);
        let framework = crate::framework::Framework::new();
        framework.jobs().create_repeat(
            1,
            "QuartzPanicJob",
            "QuartzPanicPlugin",
            Arc::new(|| {
                if FIRES.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("任务炸弹");
                }
            }),
        );
        // 首次立即触发（这次炸），1 秒后还应再来一次
        tokio::time::sleep(Duration::from_millis(1_600)).await;
        framework.jobs().remove("QuartzPanicJob");
        assert!(
            FIRES.load(Ordering::SeqCst) >= 2,
            "panic 之后调度循环不该消失，实际触发 {FIRES:?}"
        );
    }

    /// GUI 的事件循环在主线程，那里没有 tokio 上下文：在面板上把插件停用再启用时，
    /// 装配是在这条线程上同步做的，登记定时任务不能炸「there is no reactor running」
    #[test]
    fn registering_a_job_without_runtime_context() {
        let runtime = crate::runtime::runtime_builder().build().expect("建运行时");
        crate::runtime::reactor::set(runtime.handle().clone());
        let framework = crate::framework::Framework::new();
        framework
            .jobs()
            .create_repeat(1, "NoContextJob", "NoContextPlugin", Arc::new(|| {}));
        assert!(framework.jobs().remove("NoContextJob"));
        runtime.shutdown_background();
    }

    /// 间隔传 0 会退化成空转刷屏，登记时就得夹住
    #[tokio::test]
    async fn zero_interval_is_clamped() {
        let scheduler = Scheduler::default();
        scheduler.create_repeat(0, "QuartzClampJob", "QuartzClampPlugin", Arc::new(|| {}));
        let entry = scheduler
            .tasks
            .read()
            .unwrap()
            .get("QuartzClampJob")
            .unwrap()
            .clone();
        assert!(
            matches!(entry.kind, TaskKind::Repeat { interval_secs } if interval_secs >= 1),
            "0 秒间隔应被夹到至少 1 秒"
        );
        entry.canceled.store(true, Ordering::SeqCst);
    }
}
