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
    /// 这条任务是插件递过来的（见 [`PluginGuard`]）：宿主只握着不透明句柄，摘掉这一格时
    /// 由它把插件那个 `Box<JobFn>` 还给插件那一侧。框架自己排的任务是 None。
    ///
    /// 没有哪行代码读它——它的用处就是**被 drop 的那一刻**，所以别让编译器把它当死字段删掉。
    #[allow(dead_code)]
    plugin: Option<PluginGuard>,
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

/// 插件递过来的那次回调：宿主不解引用 `user`，只在到点时把它原样交回插件编译的 `run`。
#[derive(Clone, Copy)]
struct PluginThunk {
    user: *mut std::ffi::c_void,
    /// 返回 false 表示插件那一侧的处理器炸了（`extern "C"` 不让 panic 穿出去，见 `relay_run`）
    run: extern "C" fn(*mut std::ffi::c_void) -> bool,
}

unsafe impl Send for PluginThunk {}
unsafe impl Sync for PluginThunk {}

impl PluginThunk {
    /// 到点把控制权交回插件那一侧。
    ///
    /// 特意做成一个方法而不是在闭包里写 `call.run`/`call.user`：Rust 2021 的闭包按字段精确
    /// 捕获，直接点字段捕获的就是那个 `*mut c_void` 本身，`Sync`/`Send` 立刻不满足——
    /// 而我给 `PluginThunk` 写的那两行 unsafe impl 也就白写了。
    fn fire(&self) -> bool {
        (self.run)(self.user)
    }
}

/// [`PluginThunk`] 的所有权凭证：`Drop` 里把插件那个 `Box<JobFn>` 还回去。
///
/// 为什么必须确定性地放：这个 Box 是**插件的内存**，闭包里通常握着它那一轮的上下文
/// （`PluginContext`、配置句柄、上一轮的缓存）。宿主这边只要还留着条目，插件那头停用之后
/// 这些东西就一样都收不回去——正是这一轮刚修掉的那类滞留，只是换了个持有人。
///
/// 反过来，回调本身是安全的：装载器 `std::mem::forget` 掉了 `Library` 句柄，模块随进程常驻
/// （见 `plugin::dynamic::instantiate` 的注释「虚表与代码段随模块常驻」），所以 `dispose`
/// 指向的那段代码在宿主还留着条目的任何时候都还在。热重载也只是再映射一次，不会卸载。
struct PluginGuard {
    user: *mut std::ffi::c_void,
    dispose: extern "C" fn(*mut std::ffi::c_void),
}

unsafe impl Send for PluginGuard {}
unsafe impl Sync for PluginGuard {}

impl Drop for PluginGuard {
    fn drop(&mut self) {
        (self.dispose)(self.user);
    }
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

    fn register_with(
        &self,
        name: &str,
        group: &str,
        kind: TaskKind,
        run: JobFn,
        plugin: Option<PluginGuard>,
    ) -> Arc<TaskEntry> {
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
            plugin,
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
            //
            // 注意这里是在**持着写锁**丢掉条目的：插件那条 `dispose` 会在这里跑，
            // 所以闭包的 Drop 里不能再回调排期接口（`ctx.remove_job` 之类），那是自己
            // 等自己的锁。现实中这个 Drop 只是放掉几个 Arc，够不着这条。
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
        self.register_with(name, group, TaskKind::Daily { hour }, run, None);
    }

    /// 创建固定间隔循环任务（首次立即执行）。间隔至少 1 秒：传 0 会退化成空转刷屏的死循环。
    pub fn create_repeat(&self, interval_secs: u64, name: &str, group: &str, run: JobFn) {
        self.register_with(
            name,
            group,
            TaskKind::Repeat {
                interval_secs: interval_secs.max(1),
            },
            run,
            None,
        );
    }

    /// 创建单次定时任务
    pub fn create_single_at(&self, ts_ms: i64, name: &str, group: &str, run: JobFn) {
        self.register_with(name, group, TaskKind::SingleAt { ts_ms }, run, None);
    }

    /// 创建延迟任务（秒）。group 传插件 id，插件停用时整组取消。
    pub fn create_delay(&self, delay_secs: u64, name: &str, group: &str, run: JobFn) {
        let ts = chrono::Utc::now().timestamp_millis() + delay_secs as i64 * 1000;
        self.register_with(name, group, TaskKind::SingleAt { ts_ms: ts }, run, None);
    }

    /// 插件递过来的那条任务（见 [`crate::plugin::abi::HostBridge::jobs`]）。
    ///
    /// 处理器是插件镜像里的一个 `extern "C"` 回调，宿主这一侧只握着一个不透明句柄；
    /// `guard` 负责在摘掉这一格时把插件那个 `Box<JobFn>` 还回去。
    fn create_plugin(
        &self,
        kind: TaskKind,
        name: &str,
        group: &str,
        thunk: PluginThunk,
        guard: PluginGuard,
    ) {
        let run: JobFn = {
            // 闭包只带着两个 Copy 的值，不碰 guard——释放时机钉死在条目被摘掉那一刻
            let call = thunk;
            Arc::new(move || {
                if !call.fire() {
                    // 插件那一侧已经把 panic 拦在它的 `extern "C"` 出口里了（穿出去就是
                    // abort），这里换成一条 panic，好让 invoke 照常记账：吞一次、连续炸到
                    // 阈值就隔离停用，跟框架自己的任务同一个口径。
                    panic!("插件的定时任务处理器 panic");
                }
            })
        };
        self.register_with(name, group, kind, run, Some(guard));
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

// ==================== 跨镜像的那条通道 ====================

use crate::plugin::abi::{
    JOB_CREATE, JOB_DAILY, JOB_EXISTS, JOB_LIST, JOB_PAUSE_ALL, JOB_REMOVE, JOB_REMOVE_GROUP,
    JOB_REPEAT, JOB_RESUME_ALL, JOB_SINGLE_AT, JOB_TRIGGER, JobInfo, JobSpec, Jobs,
};
use std::ffi::{CStr, CString, c_void};
use std::os::raw::c_char;

/// dll 侧记下的宿主排期入口。宿主自己那份恒空，所以框架内部的调用落的就是本进程这张表
/// （`plugin::abi` 的自检用例会刻意让宿主接管一次，那正是为了验这条转发链）。
static JOBS: OnceLock<Jobs> = OnceLock::new();

/// dll 侧：记下宿主的通道（由 `plugin::abi::attach_host` 调用）
pub(crate) fn attach_host(jobs: Jobs) {
    let _ = JOBS.set(jobs);
}

/// 插件镜像里的两个出口：把宿主持着的那个不透明句柄还原成 `Box<JobFn>` 再用/再放。
///
/// 这两个函数编在 dll 里，所以解引用一定用的是**它自己那侧**的 `JobFn` 布局与分配器——
/// 宿主全程不知道也不关心 `user` 指向什么，跨界只有这两个它给过的函数指针。
extern "C" fn relay_run(user: *mut c_void) -> bool {
    // SAFETY: user 由本镜像的 bridge_create 造出来，指向一个 Box<JobFn>，在 relay_dispose
    // 之前一直有效；这里只借（不收回所有权）。
    let job = unsafe { &*user.cast::<JobFn>() };
    // 处理器炸了不能让它穿出 extern "C"（那是整个进程 abort），所以在这儿拦下来、
    // 用返回 false 告诉宿主，宿主的 invoke 照旧记账并吞掉这一次。
    std::panic::catch_unwind(AssertUnwindSafe(|| (job)())).is_ok()
}

extern "C" fn relay_dispose(user: *mut c_void) {
    // SAFETY: 宿主对同一个 user 只会调这一次，且必在 dll 还映射期间（装载器先回收再卸载）。
    let job = unsafe { Box::from_raw(user.cast::<JobFn>()) };
    // Drop 里再炸也绝不能穿出去
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| drop(job)));
}

/// 转成 C 字符串。名字里带 NUL 的任务本来就当不了键，当场念一句、这次操作跳过。
fn c_of(text: &str) -> Option<CString> {
    let value = CString::new(text).ok();
    if value.is_none() {
        crate::runtime::log::warning(format!(
            "定时任务的名字或分组里有 NUL 字节，这次操作已跳过：{text}"
        ));
    }
    value
}

/// 摆一次只需要 name/group 的操作。两个 CString 只在本次调用期间有效——宿主在返回之前
/// 就把它们拷成自己的值，这一条与 `runtime::http` 的 `HttpCall` 同一个约定。
fn bridge_call(op: Jobs, what: u32, name: &str, group: &str) -> i64 {
    let (name, group) = match (c_of(name), c_of(group)) {
        (Some(name), Some(group)) => (name, group),
        _ => return 0,
    };
    let spec = JobSpec {
        op: what,
        kind: 0,
        at: 0,
        name: name.as_ptr(),
        group: group.as_ptr(),
        run: None,
        dispose: None,
        sink: None,
        user: std::ptr::null_mut(),
    };
    op(&spec)
}

fn bridge_create(op: Jobs, kind: u32, at: i64, name: &str, group: &str, run: JobFn) {
    let (name, group) = match (c_of(name), c_of(group)) {
        (Some(name), Some(group)) => (name, group),
        _ => return,
    };
    // 闭包交出去的是本镜像 Box 出来的裸指针；宿主只当句柄握着
    let user = Box::into_raw(Box::new(run)).cast::<c_void>();
    let spec = JobSpec {
        op: JOB_CREATE,
        kind,
        at,
        name: name.as_ptr(),
        group: group.as_ptr(),
        run: Some(relay_run),
        dispose: Some(relay_dispose),
        sink: None,
        user,
    };
    if op(&spec) != 0 {
        // 宿主把这次排期判废了：闭包还归自己，别漏
        let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
            drop(unsafe { Box::from_raw(user.cast::<JobFn>()) })
        }));
    }
}

/// 创建每天固定小时触发的任务（同名存在则替换）
pub fn create_daily(hour: u32, name: &str, group: &str, run: JobFn) {
    submit_daily(
        crate::framework::Framework::global(),
        hour,
        name,
        group,
        run,
    );
}

/// 创建固定间隔循环任务（首次立即执行）
pub fn create_repeat(interval_secs: u64, name: &str, group: &str, run: JobFn) {
    submit_repeat(
        crate::framework::Framework::global(),
        interval_secs,
        name,
        group,
        run,
    );
}

/// 创建单次定时任务
pub fn create_single_at(ts_ms: i64, name: &str, group: &str, run: JobFn) {
    submit_single_at(
        crate::framework::Framework::global(),
        ts_ms,
        name,
        group,
        run,
    );
}

/// 创建延迟任务（秒）。group 传插件 id，插件停用时整组取消。
pub fn create_delay(delay_secs: u64, name: &str, group: &str, run: JobFn) {
    submit_delay(
        crate::framework::Framework::global(),
        delay_secs,
        name,
        group,
        run,
    );
}

// 下面这组 `submit_*` 是 `PluginContext` 的落点。**该不该跨界**要看两件事，缺一不可：
// 接到了宿主通道，且这套实例就是进程默认实例。dll 里 ctx 带过来的那一份正是 `adopt_host`
// 认领进来的宿主实例，两条一起成立；宿主自己的隔离实例（`Framework::new()`，单测与多实例
// 场景用的都是它）必须留在本地，否则它排的任务会跑到进程默认那张表里去。
fn bridge_for(target: &crate::framework::Framework) -> Option<Jobs> {
    JOBS.get()
        .copied()
        .filter(|_| std::ptr::eq(target, crate::framework::Framework::global()))
}

/// 见 [`create_daily`]
pub(crate) fn submit_daily(
    target: &crate::framework::Framework,
    hour: u32,
    name: &str,
    group: &str,
    run: JobFn,
) {
    match bridge_for(target) {
        Some(op) => bridge_create(op, JOB_DAILY, hour as i64, name, group, run),
        None => target.jobs().create_daily(hour, name, group, run),
    }
}

/// 见 [`create_repeat`]
pub(crate) fn submit_repeat(
    target: &crate::framework::Framework,
    interval_secs: u64,
    name: &str,
    group: &str,
    run: JobFn,
) {
    match bridge_for(target) {
        Some(op) => bridge_create(op, JOB_REPEAT, interval_secs as i64, name, group, run),
        None => target.jobs().create_repeat(interval_secs, name, group, run),
    }
}

/// 见 [`create_single_at`]
pub(crate) fn submit_single_at(
    target: &crate::framework::Framework,
    ts_ms: i64,
    name: &str,
    group: &str,
    run: JobFn,
) {
    match bridge_for(target) {
        Some(op) => bridge_create(op, JOB_SINGLE_AT, ts_ms, name, group, run),
        None => target.jobs().create_single_at(ts_ms, name, group, run),
    }
}

/// 见 [`create_delay`]
pub(crate) fn submit_delay(
    target: &crate::framework::Framework,
    delay_secs: u64,
    name: &str,
    group: &str,
    run: JobFn,
) {
    match bridge_for(target) {
        Some(op) => {
            let ts = chrono::Utc::now().timestamp_millis() + delay_secs as i64 * 1000;
            bridge_create(op, JOB_SINGLE_AT, ts, name, group, run)
        }
        None => target.jobs().create_delay(delay_secs, name, group, run),
    }
}

/// 见 [`remove`]
pub(crate) fn submit_remove(target: &crate::framework::Framework, name: &str) -> bool {
    match bridge_for(target) {
        Some(op) => bridge_call(op, JOB_REMOVE, name, "") == 1,
        None => target.jobs().remove(name),
    }
}

pub fn exists(name: &str) -> bool {
    match JOBS.get() {
        Some(op) => bridge_call(*op, JOB_EXISTS, name, "") == 1,
        None => scheduler().exists(name),
    }
}

pub fn remove(name: &str) -> bool {
    submit_remove(crate::framework::Framework::global(), name)
}

/// 按任务组整体取消（插件被禁用时用）
pub fn remove_group(group: &str) -> usize {
    match JOBS.get() {
        Some(op) => bridge_call(*op, JOB_REMOVE_GROUP, "", group).max(0) as usize,
        None => scheduler().remove_group(group),
    }
}

pub fn trigger(name: &str) -> Result<(), String> {
    match JOBS.get() {
        Some(op) => {
            let code = bridge_call(*op, JOB_TRIGGER, name, "");
            if code == 0 {
                Ok(())
            } else {
                Err("宿主没有按这个名字找到定时任务".to_string())
            }
        }
        None => scheduler().trigger(name),
    }
}

pub fn pause_all() {
    if let Some(op) = JOBS.get() {
        bridge_call(*op, JOB_PAUSE_ALL, "", "");
        return;
    }
    scheduler().pause_all();
}

pub fn resume_all() {
    if let Some(op) = JOBS.get() {
        bridge_call(*op, JOB_RESUME_ALL, "", "");
        return;
    }
    scheduler().resume_all();
}

pub fn list() -> Vec<TaskInfo> {
    let Some(op) = JOBS.get() else {
        return scheduler().list();
    };
    // 收集盒由本镜像 new、本镜像收回：宿主只在回调期间把它交给 collect_info
    let user = Box::into_raw(Box::new(Vec::<TaskInfo>::new())).cast::<c_void>();
    let spec = JobSpec {
        op: JOB_LIST,
        kind: 0,
        at: 0,
        name: std::ptr::null(),
        group: std::ptr::null(),
        run: None,
        dispose: None,
        sink: Some(collect_info),
        user,
    };
    (*op)(&spec);
    // 收集盒从头到尾只被本镜像的 collect_info 用过，宿主只是把指针交回来
    unsafe { *Box::from_raw(user.cast::<Vec<TaskInfo>>()) }
}

/// dll 侧的收件口：把宿主报过来的一条任务概览拷成本镜像的 `TaskInfo`。
extern "C" fn collect_info(info: *const JobInfo, user: *mut c_void) -> bool {
    // SAFETY: 两个指针都由本次调用给出，且在返回之前有效
    let (Some(info), Some(sink)) = (unsafe { info.as_ref() }, unsafe {
        user.cast::<Vec<TaskInfo>>().as_mut()
    }) else {
        return false;
    };
    sink.push(TaskInfo {
        name: read_c(info.name),
        group: read_c(info.group),
        next_fire_ms: if info.next_ms >= 0 {
            Some(info.next_ms)
        } else {
            None
        },
        last_fire_ms: if info.last_ms >= 0 {
            Some(info.last_ms)
        } else {
            None
        },
    });
    true
}

/// 读一个 C 字符串：空指针当空串，非法 UTF-8 走 lossy（跨边界不该因为对面手滑而崩）。
/// 跨边界的几条通道都用这一份（[`crate::views`] 的展示板上报也一样），免得两处各写一遍
/// "怎么才不算读坏别人的内存"。
pub(crate) fn read_c(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: 约定这一项指向 NUL 结尾的合法地址，本次调用期间有效
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

// ==================== 宿主侧的入口 ====================

/// 宿主侧：接住插件递来的一次排期操作。返回值含义见 [`crate::plugin::abi`] 里各操作号的说明。
pub(crate) extern "C" fn host_jobs(spec: *const JobSpec) -> i64 {
    // dll 直接调进来的入口：判空、非法 UTF-8、锁中毒这些都得就地变成返回码——
    // panic 穿出 extern "C" 是整个进程 abort，插件手滑不该把宿主带崩。
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| match unsafe { spec.as_ref() } {
        None => -1,
        Some(spec) => host_job_op(spec),
    }));
    match outcome {
        Ok(code) => code,
        Err(_) => {
            crate::runtime::log::error("定时任务通道内部异常，这次操作已丢弃");
            -2
        }
    }
}

fn host_job_op(spec: &JobSpec) -> i64 {
    let table = crate::framework::Framework::global().jobs();
    match spec.op {
        JOB_CREATE => {
            let (Some(run), Some(dispose)) = (spec.run, spec.dispose) else {
                return -1;
            };
            if spec.user.is_null() {
                return -1;
            }
            let kind = match spec.kind {
                JOB_DAILY => TaskKind::Daily {
                    hour: spec.at.clamp(0, 23) as u32,
                },
                JOB_REPEAT => TaskKind::Repeat {
                    interval_secs: spec.at.max(1) as u64,
                },
                JOB_SINGLE_AT => TaskKind::SingleAt { ts_ms: spec.at },
                _ => return -1,
            };
            let (name, group) = (read_c(spec.name), read_c(spec.group));
            if name.is_empty() {
                crate::runtime::log::warning("插件排期缺任务名，已拒");
                return -1;
            }
            table.create_plugin(
                kind,
                &name,
                &group,
                PluginThunk {
                    user: spec.user,
                    run,
                },
                PluginGuard {
                    user: spec.user,
                    dispose,
                },
            );
            0
        }
        JOB_REMOVE => i64::from(table.remove(&read_c(spec.name))),
        JOB_EXISTS => i64::from(table.exists(&read_c(spec.name))),
        JOB_REMOVE_GROUP => table.remove_group(&read_c(spec.group)) as i64,
        JOB_TRIGGER => match table.trigger(&read_c(spec.name)) {
            Ok(()) => 0,
            Err(reason) => {
                crate::runtime::log::warning(format!("手动触发定时任务失败：{reason}"));
                1
            }
        },
        JOB_PAUSE_ALL => {
            table.pause_all();
            0
        }
        JOB_RESUME_ALL => {
            table.resume_all();
            0
        }
        JOB_LIST => {
            let Some(sink) = spec.sink else { return -1 };
            let mut sent = 0i64;
            for info in table.list() {
                // 两条 C 字符串只在这次回调期间有效，插件那侧必须当场拷走（collect_info 就是）
                let (Ok(name), Ok(group)) = (CString::new(info.name), CString::new(info.group))
                else {
                    continue;
                };
                let row = JobInfo {
                    name: name.as_ptr(),
                    group: group.as_ptr(),
                    next_ms: info.next_fire_ms.unwrap_or(-1),
                    last_ms: info.last_fire_ms.unwrap_or(-1),
                };
                if !(sink)(&row, spec.user) {
                    break;
                }
                sent += 1;
            }
            sent
        }
        other => {
            crate::runtime::log::warning(format!("认不出的定时任务操作 {other}，已拒"));
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// 排定时刻与实际触发时刻的偏差。服务器上出现过「8:00 的推送 8:24 才发」与
    /// 「23:00 的预警 23:32 才发」，先钉住调度器本身准不准。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn single_at_fires_on_the_scheduled_moment() {
        let framework = crate::framework::Framework::new();
        let base = chrono::Utc::now().timestamp_millis();
        let drift = Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        for step in 1..=3i64 {
            let at = base + step * 1_200;
            let sink = drift.clone();
            framework.jobs().create_single_at(
                at,
                &format!("QuartzProbe{step}"),
                "QuartzProbePlugin",
                Arc::new(move || {
                    sink.lock().unwrap()
                        .push(chrono::Utc::now().timestamp_millis() - at);
                }),
            );
        }
        tokio::time::sleep(Duration::from_millis(5_000)).await;
        let got = drift.lock().unwrap().clone();
        println!("单次任务偏差(ms) = {got:?}");
        assert_eq!(got.len(), 3, "三个单次任务只触发了 {} 个", got.len());
        for d in &got {
            assert!(d.abs() < 300, "单次任务比排定时刻偏了 {d} ms");
        }
    }

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

    /// 只需要 name/group 的那几档，摆一份描述出来。
    fn simple_spec(op: u32, name: &CString, group: &CString) -> JobSpec {
        JobSpec {
            op,
            kind: 0,
            at: 0,
            name: name.as_ptr(),
            group: group.as_ptr(),
            run: None,
            dispose: None,
            sink: None,
            user: std::ptr::null_mut(),
        }
    }

    /// 插件递过来的闭包必须**落在宿主的表里**，并且摘掉那一格时**确实被放掉**。
    ///
    /// 这条通道就是这次重构的全部理由：跨界只传一个不透明句柄，宿主既不读它的布局也不碰它的
    /// 分配器，释放靠插件自己编译出来的 `dispose`。少了 `dispose` 那一半就是换个地方漏闭包，
    /// 所以两头都要量：落表（不是安静写进 dll 自己那份空表）与放掉。
    #[test]
    fn a_plugin_job_lands_in_the_host_table_and_is_freed_on_removal() {
        // 探针：闭包里揣着一个 Sender。Box<JobFn> 被收回的那一刻它消失，接收端立刻报断。
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let job: JobFn = Arc::new(move || {
            let _hold = &tx;
        });
        let user = Box::into_raw(Box::new(job)).cast::<c_void>();
        let name = CString::new("BridgeProbe").unwrap();
        let group = CString::new("bridge-probe").unwrap();
        let create = JobSpec {
            op: JOB_CREATE,
            kind: JOB_REPEAT,
            at: 3600,
            name: name.as_ptr(),
            group: group.as_ptr(),
            run: Some(relay_run),
            dispose: Some(relay_dispose),
            sink: None,
            user,
        };
        assert_eq!(host_jobs(&create), 0, "这次排期该被宿主接走");
        assert!(
            crate::framework::Framework::global()
                .jobs()
                .exists("BridgeProbe"),
            "插件递的任务必须落在宿主真正在跑的那张表里"
        );
        assert_eq!(
            host_jobs(&simple_spec(JOB_EXISTS, &name, &group)),
            1,
            "在表里就该报 1"
        );
        assert_eq!(
            host_jobs(&simple_spec(JOB_REMOVE, &name, &group)),
            1,
            "取消必须报「确实删掉了一条」"
        );
        // 只认「通道已断开」：等超时也是 Err，但那种是闭包还活着，正是这条要拒的状态
        match rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!(
                "dispose 没被调用，插件那个 Box 漏在宿主手里（recv 实际返回 {:?}）",
                other.err()
            ),
        }
        // 缺句柄的排期必须当场拒掉，不能把空指针存进表里等着到点回调
        let hollow = JobSpec {
            user: std::ptr::null_mut(),
            ..create
        };
        assert_eq!(host_jobs(&hollow), -1, "没有句柄的排期该当场拒");
    }

    /// 插件的处理器炸了必须**报回来**，不能把宿主带崩：panic 穿出 `extern "C"` 是整个进程
    /// abort，所以 `relay_run` 自己拦住、报 false，宿主的 `invoke` 照旧吞掉这一次并记健康度。
    #[test]
    fn a_panicking_plugin_job_reports_back_instead_of_aborting_the_host() {
        let job: JobFn = Arc::new(|| panic!("插件里炸了"));
        let user = Box::into_raw(Box::new(job)).cast::<c_void>();
        assert!(
            !relay_run(user),
            "panic 必须被拦在 extern \"C\" 出口里，并用 false 报给宿主"
        );
        // 报回来之后同一个句柄还得能正常放掉
        relay_dispose(user);
    }
}
