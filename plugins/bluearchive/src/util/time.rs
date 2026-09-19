//! 时间工具（对应原版 TimeUtil）
use chrono::{DateTime, Datelike, Local, Timelike};

/// 今天的“日”，用于每日重置逻辑（对应 Calendar.DAY_OF_MONTH）
pub fn today() -> i64 {
    Local::now().day() as i64
}

/// 生成活动展示时间文本，分钟为 59 时按原版行为向上取整到下一小时
pub fn calc_time(ts_ms: i64, future: bool) -> String {
    let naive = chrono::DateTime::from_timestamp_millis(ts_ms)
        .map(|d| d.with_timezone(&chrono::Local))
        .unwrap_or_else(Local::now);
    let mut minutes = naive.minute();
    let mut hour = naive.hour();
    if minutes == 59 {
        minutes = 0;
        hour = (hour + 1) % 24;
    }
    let head = format!(
        "{:02}月{:02}日 {:02}:{:02}",
        naive.month(),
        naive.day(),
        hour,
        minutes
    );
    let suffix = if future { "开始" } else { "结束" };
    format!("{head}{suffix}")
}

/// 转成“今天/明天/后天 X点 开始/结束”的易读文本（对应 TimeUtil.translateTimeMoreReadable）
/// 超出该范围时回退为 calc_time 的完整时间文本。
pub fn translate_readable_time(ts_ms: i64, future: bool) -> String {
    let fallback = calc_time(ts_ms, future);
    let Some(target) = round_to_display_time(ts_ms) else {
        return fallback;
    };
    let now = Local::now();
    if target <= now {
        return fallback;
    }
    let total_minutes = target.signed_duration_since(now).num_minutes();
    if total_minutes < 0 {
        return fallback;
    }
    let total_hours = total_minutes / 60;
    let mut day = total_hours / 24;
    let left_hour = total_hours - day * 24;
    // 与原版一致: 距目标时间不满 24 小时但已过当前整点时视为明天起算
    if left_hour - (24 - now.hour() as i64) >= 0 {
        day += 1;
    }
    if (0..=2).contains(&day) {
        let prefix = ["今天", "明天", "后天"][day as usize];
        let suffix = if future { "开始" } else { "结束" };
        return format!("{prefix}{}点{suffix}", target.hour());
    }
    fallback
}

/// 换算为本地时间并应用展示规则: 秒清零, 分钟为 59 时进位到下一整点
fn round_to_display_time(ts_ms: i64) -> Option<DateTime<Local>> {
    let local = DateTime::from_timestamp_millis(ts_ms)?.with_timezone(&Local);
    let local = local.with_second(0)?.with_nanosecond(0)?;
    if local.minute() == 59 {
        local.checked_add_signed(chrono::Duration::minutes(1))
    } else {
        Some(local)
    }
}
