//! 抽卡运行期配置（对应原版 RuntimeGachaConfig）
//! 独立模式下 /抽卡 修改的出货率/池子/限制/撤回时间保存在内存，重启即恢复默认（与原版一致）。

use once_cell::sync::OnceCell;
use std::sync::RwLock;

#[derive(Clone)]
pub struct GachaConfig {
    pub star1_rate: f32,
    pub star2_rate: f32,
    pub star3_rate: f32,
    pub star2_pickup_rate: f32,
    pub star3_pickup_rate: f32,
    /// 当前活动池子 id（旧式 DB 池，抽卡 V2 不使用，但 /抽卡 setpool 会维护）
    pub active_pool: i32,
    /// 抽卡结果自动撤回时间（秒），0 表示不撤回
    pub revoke_time: i32,
    /// 每日抽卡次数限制，0 表示不限制
    pub limit: i32,
    /// 今日（用于每日重置）
    pub day: i64,
    pub max_dot: i32,
}

static CONFIG: OnceCell<RwLock<GachaConfig>> = OnceCell::new();

fn instance() -> &'static RwLock<GachaConfig> {
    CONFIG.get_or_init(|| {
        RwLock::new(GachaConfig {
            star1_rate: 79.0,
            star2_rate: 18.5,
            star3_rate: 2.5,
            star2_pickup_rate: 3.0,
            star3_pickup_rate: 0.7,
            active_pool: 1,
            revoke_time: 10,
            limit: 0,
            day: 0,
            max_dot: 10,
        })
    })
}

pub fn get() -> GachaConfig {
    instance().read().unwrap().clone()
}

pub fn set(f: impl FnOnce(&mut GachaConfig)) {
    let mut guard = instance().write().unwrap();
    f(&mut guard);
}

pub fn revoke_time() -> i64 {
    get().revoke_time as i64
}

pub fn limit() -> i64 {
    get().limit as i64
}

pub fn active_pool() -> i32 {
    get().active_pool
}

pub fn recalc_max_dot() {
    let mut guard = instance().write().unwrap();
    let rates = [
        guard.star1_rate,
        guard.star2_rate,
        guard.star3_rate,
        guard.star2_pickup_rate,
        guard.star3_pickup_rate,
    ];
    let mut max = 1;
    for rate in rates {
        let s = format!("{rate}");
        let dot = match s.find('.') {
            None => 1,
            Some(idx) => s.len() - idx - 1,
        };
        if dot > max {
            max = dot;
        }
    }
    guard.max_dot = max as i32;
}
