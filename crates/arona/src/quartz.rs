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

/// 调度循环分段睡的步长：取消标志最多隔这么久就看得见。
/// 任务数量是「几个到几十」这个量级，每秒醒一次的开销可以忽略。
const CANCEL_POLL_MS: i64 = 1000;

struct TaskEntry {
    name: String,
    group: String,
    kind: TaskKind,
    run: JobFn,
    /// 归属插件的健康度记账（拿不到框架实例时为 None，只吞 panic 不停用）
    health: Option<Arc<HealthBoard>>,
    paused: AtomicBool,
    /// 取消标志。调度循环按 [`CANCEL_POLL_MS`] 分段睡，所以这个标志最多一秒就看得见，
    /// 不必等睡完这一觉（Daily 是最长 24 小时）——这期间这条任务连同 `run` 闭包（里面挂着
    /// 插件那一轮的上下文）一直占着内存，反复热重载就是照着这条攒出来的。
    canceled: AtomicBool,
    last_fire: RwLock<Option<i64>>,
    next_fire: RwLock<Option<i64>>,
}

impl TaskEntry {
    /// 这条任务是否已经跑到头了：单次/延时任务不再有下一次。
    ///
    /// 表是按任务名长的，而延时任务的名字通常带着消息号，于是**每排一次就在表里留下一个
    /// `TaskEntry`**，连它 `run` 闭包里握着的东西一起不退——长跑几天就是几百 MB 的那个形状。
    /// 判完由 [`Scheduler::register`] 那次写锁顺手清扫，条目自己不动表。
    ///
    /// 为什么不叫任务自己从表里摘掉：那要么给 `TaskEntry` 加一个指回表的字段，要么把表包进
    /// `Arc`——两者都改了插件 dll 会直接读的结构体布局。dll 里编译着框架同一份源码的**另一份**
    /// 副本，它按自己编译期的偏移读写这两个结构；布局一错，旧 dll 排的任务在新宿主里就全落空，
    /// 而握手照样放行（实测：旧 dll + 改过布局的宿主 → 装载成功、插件启动、零条代发）。
    fn is_finished(&self) -> bool {
        matches!(self.kind, TaskKind::SingleAt { .. }) && self.next_fire.read().unwrap().is_none()
    }

    /// 取消这条任务：置标志，正在分段睡的那条循环下一格就看见。
    fn cancel(&self) {
        self.canceled.store(true, Ordering::SeqCst);
    }

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
                    // 单次任务到点却没有下一次：标成「不再有下一次」，条目等下一次排期时清扫
                    *entry.next_fire.write().unwrap() = None;
                    return;
                };
                // 分段睡：一觉睡到点的话（Daily 是最长 24 小时），被取消的任务在这期间不会醒
                let mut remain = delay_ms;
                while remain > 0 {
                    let step = remain.min(CANCEL_POLL_MS);
                    tokio::time::sleep(Duration::from_millis(step as u64)).await;
                    remain -= step;
                    if entry.canceled.load(Ordering::SeqCst) {
                        return;
                    }
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
                let mut guard = entry.next_fire.write().unwrap();
                *guard = entry.compute_next_after(after);
                if guard.is_none() {
                    // 单次任务已经触发完：标成「不再有下一次」，条目等下一次排期时清扫
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
        let health = self
            .framework
            .get()
            .and_then(|weak| weak.upgrade())
            .map(|framework| framework.health().clone());
        // 单次/延时任务一造出来就有下一次：条目不能有一瞬间看着像「跑完了」，
        // 否则下面那次清扫会把刚排上的它当成空壳删掉
        let seed_next = match &kind {
            TaskKind::SingleAt { ts_ms } => Some(*ts_ms),
            _ => None,
        };
        let entry = Arc::new(TaskEntry {
            name: name.to_string(),
            group: group.to_string(),
            kind,
            run,
            health,
            paused: AtomicBool::new(false),
            canceled: AtomicBool::new(false),
            last_fire: RwLock::new(None),
            next_fire: RwLock::new(seed_next),
        });
        {
            let mut tasks = self.tasks.write().unwrap();
            // 排期这个动作本来就是「表会变长」的时刻，顺手把跑完的单次条目收掉。
            // 条目自己摘自己要么加一个指回表的字段、要么把表包进 Arc，两者都会让
            // 旧插件 dll 里的同一份读写代码错位（见 [`TaskEntry::is_finished`]）。
            tasks.retain(|_, live| !live.is_finished());
            if let Some(previous) = tasks.get(name) {
                previous.cancel();
            }
            tasks.insert(name.to_string(), entry.clone());
        }
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
            entry.cancel();
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
            entry.cancel();
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

    /// 取消必须**即时**生效，不能等下一次触发才醒。
    ///
    /// 回归点：原先 `canceled` 只是个标志，那条循环一觉睡到点（Daily 就是最长 24 小时），
    /// 取消之后还要挂到下一个触发点才醒；这期间它连同 `run` 闭包里握着的插件上下文都不肯放手，
    /// 反复热重载就是照这条攒出内存台阶的。这里用一只"闭包被放掉时响一声"的哨子，
    /// 量的正是放手的时刻，而不是有没有响过。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_task_releases_its_job_at_once() {
        struct Watchdog(std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>);
        impl Drop for Watchdog {
            fn drop(&mut self) {
                if let Some(tx) = self.0.get_mut().unwrap().take() {
                    let _ = tx.send(());
                }
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let watch = Watchdog(std::sync::Mutex::new(Some(tx)));
        let framework = crate::framework::Framework::new();
        // 一小时一跳：取消不即时生效的话，这条要再挂一个小时才放手
        framework.jobs().create_repeat(
            3600,
            "QuartzCancelNow",
            "QuartzCancelPlugin",
            Arc::new(move || {
                let _ = &watch;
            }),
        );
        // 先确认循环真跑起来了（首次立即触发），否则下面的断言会空过
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(framework.jobs().remove("QuartzCancelNow"));
        rx.recv_timeout(std::time::Duration::from_secs(3))
            .expect("取消之后 3 秒内没放掉任务闭包——取消仍然要等到下一次触发");
    }

    /// 单次任务触发完就是一次排期清扫的当口：跑完的那格要被收掉，刚排上的那格不许被误收。
    ///
    /// 回归点：原来那条循环只是 `return`，条目连它 `run` 闭包里握着的东西一起永远留在表里。
    /// 而延时任务的名字通常带着消息号（撤回占位、延迟清理都是按条消息起的），于是
    /// **每延一次时表里就长一格**——长跑几天攒出几百 MB 就是这条，跟热重载无关。
    /// 清扫放在 `register` 里而不是让条目自己摘自己：后者要么给 `TaskEntry` 加一个指回表的字段、
    /// 要么把表包进 `Arc`，两者都改了插件 dll 会直接读的结构体布局，旧 dll 排的任务会在新宿主里全落空。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_single_shot_task_is_swept_by_the_next_registration() {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let scheduler = Scheduler::default();
        scheduler.create_delay(
            1,
            "QuartzOnceJob",
            "QuartzOncePlugin",
            Arc::new(move || {
                flag.store(true, Ordering::SeqCst);
            }),
        );
        assert!(
            scheduler.exists("QuartzOnceJob"),
            "登记之后表里就该有这一格"
        );
        tokio::time::sleep(Duration::from_millis(1_600)).await;
        assert!(
            fired.load(Ordering::SeqCst),
            "延时任务压根没触发，这条测试就成了空过"
        );
        // 再排一条：这一趟 register 会把跑完的那格收掉
        scheduler.create_delay(
            60,
            "QuartzSweepTrigger",
            "QuartzOncePlugin",
            Arc::new(|| {}),
        );
        assert!(
            !scheduler.exists("QuartzOnceJob"),
            "单次任务跑完之后条目还留在表里——那就是那条只会变长的泄漏"
        );
        // 刚排上的单次任务有一瞬间 next_fire 还没被循环填过，误收就等于任务压根没排上
        assert!(
            scheduler.exists("QuartzSweepTrigger"),
            "清扫把刚登记、还没触发的那格一起收了"
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
