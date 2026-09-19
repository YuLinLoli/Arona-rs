//! GameKee 活动日历数据源与分类（对应原版 util/ActivityUtil 的 GameKee 通道）
//! 三服活动统一使用 GameKee 活动查询接口；Rust 移植版不渲染图片，只产出文本。

use crate::entity::{Activity, ActivityType, ServerLocale};
use crate::util::time::calc_time;
use serde_json::Value;

pub const ACTIVITY_QUERY_URL: &str = "https://www.gamekee.com/v1/activity/query";

/// 拉取某服务器活动：返回 (进行中, 即将开始)
pub async fn fetch(server: ServerLocale) -> Result<(Vec<Activity>, Vec<Activity>), String> {
    let now_secs = chrono::Utc::now().timestamp();
    // 原版 GameKeeUtil.getEventData 用 Jsoup 的 .data(...).get()，即以 GET 查询参数传参
    let url = format!("{ACTIVITY_QUERY_URL}?active_at={now_secs}");
    let json = super::http::get_with(
        &url,
        &super::http::game_kee_headers("https://www.gamekee.com/ba/"),
    )
    .await?;
    let mut pair = analyze(&json, server)?;
    merge_upcoming_birthdays(&mut pair, server).await;
    Ok(pair)
}

/// 把「明天起一周内」的学生生日并入「即将开始」列表。
/// 对应原版 ActivityUtil 的 fetch{JP,EN,CN}Activity：把 SchaleDBUtil.getBirthdayData
/// 的结果 addAll 到 second(即将开始) 列表；生日不参与结束预警(notify::alert_plan 已排除)。
pub async fn merge_upcoming_birthdays(
    pair: &mut (Vec<Activity>, Vec<Activity>),
    server: ServerLocale,
) {
    let mut birthdays = super::birthday::upcoming_birthday_activities(server).await;
    if birthdays.is_empty() {
        return;
    }
    pair.1.append(&mut birthdays);
    sort_and_package(&mut pair.0, &mut pair.1);
}

/// 解析活动查询响应（只收 pub_area == server.serverName 的条目）
pub fn analyze(json: &str, server: ServerLocale) -> Result<(Vec<Activity>, Vec<Activity>), String> {
    let root: Value =
        serde_json::from_str(json).map_err(|err| format!("活动 JSON 解析失败: {err}"))?;
    let data = root
        .get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut active: Vec<Activity> = Vec::new();
    let mut pending: Vec<Activity> = Vec::new();
    for item in data {
        let Some(pub_area) = item.get("pub_area").and_then(|v| v.as_str()) else {
            continue;
        };
        if pub_area != server.server_name() {
            continue;
        }
        let Some(title) = item.get("title").and_then(|v| v.as_str()) else {
            continue;
        };
        let begin_at = item.get("begin_at").and_then(|v| v.as_i64()).unwrap_or(0) * 1000;
        let end_at = item.get("end_at").and_then(|v| v.as_i64()).unwrap_or(0) * 1000;
        insert(server, title, begin_at, end_at, &mut active, &mut pending);
    }
    sort_and_package(&mut active, &mut pending);
    Ok((active, pending))
}

/// 插入并分类一条活动（对应 doInsert0 + extraActivityTypeFromGameKee）
fn insert(
    server: ServerLocale,
    content_source: &str,
    start_ms: i64,
    end_ms: i64,
    active: &mut Vec<Activity>,
    pending: &mut Vec<Activity>,
) {
    let mut content = content_source
        .replace(&format!("【{}】", server.server_name()), "")
        .replace('（', "(")
        .replace('）', ")");
    let mut activity_type = ActivityType::Activity;
    if content.contains("卡池") {
        activity_type = ActivityType::PickUp;
        content = content.replace(&format!("【{}卡池】", server.server_name()), "Pick Up: ");
    }
    if content.contains("指名手配") || content.contains("悬赏通缉") || content.contains("懸賞通緝")
    {
        activity_type = ActivityType::WantedDrop;
    }
    if content.contains("学院交流")
        || content.contains("学園交流会")
        || content.contains("學院交流會")
    {
        activity_type = ActivityType::CollegeExchangeDrop;
    }
    if content.contains("特别依赖")
        || content.contains("特殊任务")
        || content.contains("特别委托")
        || content.contains("特別依賴")
        || content.contains("特別委託")
        || content.contains("特殊依賴")
    {
        activity_type = ActivityType::SpecialDrop;
    }
    if content.contains("日程")
        || content.contains("课程表")
        || content.contains("課程表")
        || content.contains("スケジュール")
    {
        activity_type = ActivityType::Schedule;
    }
    if content.contains("合同火力演习")
        || content.contains("合同火力演習")
        || content.contains("综合战术考试")
        || content.contains("綜合戰術考試")
        || content.contains("综合战术测试")
        || content.contains("綜合戰術測驗")
    {
        activity_type = ActivityType::JointExercises;
    }
    if content.contains("总力战")
        || content.contains("総力戦")
        || content.contains("總力戰")
        || content.contains("大决战")
        || content.contains("大決戰")
        || content.contains("无限制决战")
        || content.contains("無限制決戰")
        || content.contains("制约解除决战")
        || content.contains("制約解除決戰")
        || content.contains("决战")
        || content.contains("決戰")
    {
        activity_type = ActivityType::DecisiveBattle;
    }
    if content.contains("Normal")
        || content.contains("普通难度")
        || content.contains("普通任務")
        || content.contains("普通任务")
    {
        activity_type = ActivityType::N2_3;
    }
    if content.contains("Hard")
        || content.contains("困难难度")
        || content.contains("困难任务")
        || content.contains("困難難度")
        || content.contains("困難任務")
    {
        activity_type = ActivityType::H2_3;
    }
    if content.contains("掉落量2倍")
        || content.contains("掉落2倍")
        || content.contains("獎勵2倍")
        || content.contains("奖励2倍")
        || content.contains("經驗值2倍")
        || content.contains("经验值2倍")
        || content.contains("報酬2倍")
        || content.contains("报酬2倍")
    {
        activity_type = ActivityType::N2_3;
    }

    let now = chrono::Utc::now().timestamp_millis();
    if now < start_ms {
        let mut activity = Activity::new(content, server, start_ms, end_ms, activity_type);
        activity.time = calc_time(start_ms, true);
        pending.push(activity);
    } else if now < end_ms {
        let mut activity = Activity::new(content, server, start_ms, end_ms, activity_type);
        activity.time = calc_time(end_ms, false);
        active.push(activity);
    }
}

/// 按活动类型权重降序整理（对应 sortAndPackage）
fn sort_and_package(active: &mut Vec<Activity>, pending: &mut Vec<Activity>) {
    active.sort_by_key(|a| std::cmp::Reverse(type_level(a.activity_type)));
    pending.sort_by_key(|a| std::cmp::Reverse(type_level(a.activity_type)));
}

/// 生成文本形式的活动日历（对应原版 createActivityImage 的文字部分）
pub fn format_calendar(pair: &(Vec<Activity>, Vec<Activity>), server: ServerLocale) -> String {
    let (active, pending) = pair;
    let mut active = active.clone();
    let mut pending = pending.clone();
    active.sort_by_key(|a| a.end_time);
    pending.sort_by_key(|a| a.start_time);
    let mut out = String::new();
    out.push_str(&format!("{}活动日历\n", server.server_name()));
    out.push_str(&format!("{}\n", chrono::Local::now().format("%Y/%m/%d")));
    out.push_str("正在进行:\n");
    if active.is_empty() {
        out.push_str("无\n");
    } else {
        for activity in &active {
            out.push_str(&format!("{}\t{}\n", activity.content, activity.time));
        }
    }
    out.push_str("即将开始:\n");
    if pending.is_empty() {
        out.push_str("无\n");
    } else {
        for activity in &pending {
            out.push_str(&format!("{}\t{}\n", activity.content, activity.time));
        }
    }
    out.push_str("数据来源: https://ba.gamekee.com/");
    out
}

pub fn type_level(activity_type: ActivityType) -> i32 {
    match activity_type {
        ActivityType::Null
        | ActivityType::N2_3
        | ActivityType::H2_3
        | ActivityType::SpecialDrop
        | ActivityType::WantedDrop
        | ActivityType::CollegeExchangeDrop
        | ActivityType::Schedule
        | ActivityType::JointExercises => 1,
        ActivityType::Kabala | ActivityType::Activity => 2,
        ActivityType::DecisiveBattle | ActivityType::PickUp => 3,
        ActivityType::Maintenance => 4,
        ActivityType::Birthday => 5,
    }
}
