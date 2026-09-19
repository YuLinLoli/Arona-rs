//! 轻量定时任务（对应原版 quartz/QuartzProvider + StandaloneTaskCommand 需要的能力）
//!
//! 原版基于 Quartz 支持完整 cron；独立模式只用到了三种形态：
//! 每天固定小时、固定间隔循环（首次立即执行）、单次延时/定时。这里用 tokio 实现同等语义，
//! 任务以名称注册，支持列出、手动触发、暂停/恢复与删除。

use chrono::{DateTime, Local, Timelike};
use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
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
    paused: AtomicBool,
    canceled: AtomicBool,
    last_fire: RwLock<Option<i64>>,
    next_fire: RwLock<Option<i64>>,
}

impl TaskEntry {
    fn schedule_cycle(self: &Arc<Self>) {
        let entry = self.clone();
        tokio::spawn(async move {
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
                (entry.run)();
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
        });
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

struct Scheduler {
    tasks: RwLock<HashMap<String, Arc<TaskEntry>>>,
}

static SCHEDULER: OnceCell<Scheduler> = OnceCell::new();

fn scheduler() -> &'static Scheduler {
    SCHEDULER.get_or_init(|| Scheduler {
        tasks: RwLock::new(HashMap::new()),
    })
}

fn register(name: &str, group: &str, kind: TaskKind, run: JobFn) -> Arc<TaskEntry> {
    let existing = scheduler().tasks.read().unwrap().get(name).cloned();
    if let Some(entry) = existing {
        entry.canceled.store(true, Ordering::SeqCst);
    }
    let entry = Arc::new(TaskEntry {
        name: name.to_string(),
        group: group.to_string(),
        kind,
        run,
        paused: AtomicBool::new(false),
        canceled: AtomicBool::new(false),
        last_fire: RwLock::new(None),
        next_fire: RwLock::new(None),
    });
    scheduler()
        .tasks
        .write()
        .unwrap()
        .insert(name.to_string(), entry.clone());
    entry.schedule_cycle();
    entry
}

/// 创建每天固定小时触发的任务（同名存在则替换）
pub fn create_daily(hour: u32, name: &str, group: &str, run: JobFn) {
    register(name, group, TaskKind::Daily { hour }, run);
}

/// 创建固定间隔循环任务（首次立即执行）
pub fn create_repeat(interval_secs: u64, name: &str, group: &str, run: JobFn) {
    register(name, group, TaskKind::Repeat { interval_secs }, run);
}

/// 创建单次定时任务
pub fn create_single_at(ts_ms: i64, name: &str, group: &str, run: JobFn) {
    register(name, group, TaskKind::SingleAt { ts_ms }, run);
}

/// 创建延迟任务（秒）
pub fn create_delay(delay_secs: u64, name: &str, run: JobFn) {
    let ts = chrono::Utc::now().timestamp_millis() + delay_secs as i64 * 1000;
    register(name, "Delay", TaskKind::SingleAt { ts_ms: ts }, run);
}

pub fn exists(name: &str) -> bool {
    scheduler().tasks.read().unwrap().contains_key(name)
}

pub fn remove(name: &str) -> bool {
    if let Some(entry) = scheduler().tasks.write().unwrap().remove(name) {
        entry.canceled.store(true, Ordering::SeqCst);
        true
    } else {
        false
    }
}

/// 按任务组整体取消（插件被禁用时用：一次停掉自己全部的定时任务，不必逐个记名字）
pub fn remove_group(group: &str) -> usize {
    let removed: Vec<Arc<TaskEntry>> = scheduler()
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

pub fn trigger(name: &str) -> Result<(), String> {
    let entry = scheduler()
        .tasks
        .read()
        .unwrap()
        .get(name)
        .cloned()
        .ok_or_else(|| format!("任务不存在: {name}"))?;
    tokio::spawn(async move {
        let run = entry.run.clone();
        run();
    });
    Ok(())
}

pub fn pause_all() {
    let tasks = scheduler().tasks.read().unwrap();
    for entry in tasks.values() {
        entry.paused.store(true, Ordering::SeqCst);
    }
}

pub fn resume_all() {
    let tasks = scheduler().tasks.read().unwrap();
    for entry in tasks.values() {
        entry.paused.store(false, Ordering::SeqCst);
    }
}

pub fn list() -> Vec<TaskInfo> {
    let tasks = scheduler().tasks.read().unwrap();
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
