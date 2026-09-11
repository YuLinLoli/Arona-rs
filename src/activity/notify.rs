//! 每日活动推送与结束预警服务（对应原版 standalone/commands/StandaloneActivityNotify）
//! 每天定时向配置的群推送 国服/国际服/日服 活动日历图片（纯 Rust 渲染, 失败回退文本）；
//! 启动与每日运行时查询活动结束预警(提前1小时/双倍掉落提前5小时)，正负10分钟内立即发送，
//! 过期抛弃，未来添加定时任务(去重)。
//! 1 小时预警的同时安排「到期后 5 分钟」刷新本地资源图片任务；另有每日 0 点的全量刷新任务。

use crate::config::arona::NotifyConfig;
use crate::config::standalone;
use crate::data;
use crate::entity::{Activity, ActivityType, ServerLocale};
use crate::quartz;
use crate::runtime::message::{MessageTarget, OutgoingMessage};
use crate::services::{self, ServiceInfo};
use std::sync::Arc;

const NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS: i64 = 1;
const DROP_ACTIVITY_NOTIFY_BEFORE_HOURS: i64 = 5;
/// 正负 10 分钟窗口
const ALERT_IMMEDIATE_WINDOW_MILLIS: i64 = 10 * 60 * 1000;
/// 活动到期后刷新本地资源图片的延迟: 1 小时预警 + 1 小时 5 分钟
const IMAGE_REFRESH_AFTER_END_MILLIS: i64 = 5 * 60 * 1000;

/// 服务注册（对应原版 init/registerService）
pub fn register_service() {
    let service: Arc<ServiceInfo> = services::service_info(12, "活动推送", false, false);
    services::manager().register(&service);
}

/// 从配置读取推送小时并启用每日推送任务（独立模式启动时调用）
pub fn enable_service() {
    register_service();
    let hour = standalone::notify_config().every_day_hour.clamp(0, 23) as u32;
    enable_daily_job(hour);
    crate::runtime::log::info(format!("活动推送已启用, 每天 {hour} 点推送"));
}

/// 创建每天固定小时的活动推送任务（小时变更时由 quartz::reschedule_daily_notify 调用）
pub fn enable_daily_job(hour: u32) {
    quartz::create_daily(
        hour,
        "StandaloneActivityNotify",
        "StandaloneActivityNotify",
        Arc::new(|| {
            tokio::spawn(async move {
                push(false).await;
            });
        }),
    );
    // 启动后 20 秒执行一次初始化：只做预警调度，不推送日历
    if !quartz::exists("StandaloneActivityNotifyInit") {
        quartz::create_delay(
            20,
            "StandaloneActivityNotifyInit",
            Arc::new(|| {
                tokio::spawn(async move {
                    push(true).await;
                });
            }),
        );
    }
}

/// 推送目标群：全局允许的群去掉黑名单
fn resolve_targets(config: &NotifyConfig, global_groups: Vec<i64>) -> Vec<i64> {
    global_groups
        .into_iter()
        .filter(|group| !config.black_groups.contains(group))
        .collect()
}

/// 每日推送与预警调度入口
/// skip_image=true 表示启动时的初始化运行: 只做 5小时/1小时 预警调度, 不推送日历
pub async fn push(skip_image: bool) {
    let config = standalone::notify_config();
    if !config.enable {
        return;
    }
    if !crate::runtime::services::sender_ready() {
        crate::runtime::log::warning("活动推送未执行: 消息发送器尚未就绪");
        return;
    }
    let targets = resolve_targets(&config, crate::runtime::config::groups());
    if targets.is_empty() {
        crate::runtime::log::warning("活动推送未执行: 未配置推送目标群");
        return;
    }
    schedule_alerts().await;
    if skip_image {
        return;
    }
    let mut servers: Vec<ServerLocale> = Vec::new();
    if config.jp {
        servers.push(ServerLocale::JP);
    }
    if config.global {
        servers.push(ServerLocale::GLOBAL);
    }
    if config.cn {
        servers.push(ServerLocale::CN);
    }
    for server in servers {
        let prefix = format!("{}({})", config.notify_text, server.server_name());
        push_server(server, &prefix, &targets).await;
    }
}

/// 向目标群推送某个服务器的活动日历（文本前缀 + 活动图片, 图片渲染失败时回退文本）
async fn push_server(server: ServerLocale, prefix: &str, targets: &[i64]) {
    let pair = match data::activity::fetch(server).await {
        Ok(pair) => pair,
        Err(err) => {
            crate::runtime::log::warning(format!("拉取{}活动失败: {err}", server.server_name()));
            return;
        }
    };
    let message = match crate::image::activity::render(&pair, server) {
        Ok(file) => {
            OutgoingMessage::text(format!("{prefix}\n"))
                + OutgoingMessage::image_file(file.to_string_lossy())
        }
        Err(err) => {
            crate::runtime::log::warning(format!(
                "{}活动图片生成失败, 回退文本: {err}",
                server.server_name()
            ));
            let calendar = data::activity::format_calendar(&pair, server);
            OutgoingMessage::text(format!("{prefix}\n{calendar}"))
        }
    };
    for &group_id in targets {
        let receipt =
            crate::runtime::services::send_message(MessageTarget::Group(group_id), message.clone())
                .await;
        if receipt.message_id.is_none() {
            crate::runtime::log::warning(format!(
                "推送{}活动到群 {group_id} 失败",
                server.server_name()
            ));
        }
    }
}

/// 查询并安排各服务器的活动结束预警
async fn schedule_alerts() {
    let config = standalone::notify_config();
    let mut servers: Vec<ServerLocale> = Vec::new();
    if config.jp {
        servers.push(ServerLocale::JP);
    }
    if config.global {
        servers.push(ServerLocale::GLOBAL);
    }
    if config.cn {
        servers.push(ServerLocale::CN);
    }
    for server in servers {
        let pair = match data::activity::fetch(server).await {
            Ok(pair) => pair,
            Err(err) => {
                crate::runtime::log::warning(format!(
                    "拉取{}活动失败: {err}",
                    server.server_name()
                ));
                continue;
            }
        };
        for (activities, hours) in alert_plan(&pair.0) {
            schedule_alert_group(&activities, server, hours).await;
        }
    }
}

/// 计算某服务器活动的预警安排: 除生日外的所有活动在结束前 5 小时与 1 小时各提醒一次;
/// 维护单列只在 1 小时提醒(特殊文案)
fn alert_plan(active: &[Activity]) -> Vec<(Vec<Activity>, i64)> {
    let alertable: Vec<Activity> = active
        .iter()
        .filter(|it| it.activity_type != ActivityType::Birthday)
        .cloned()
        .collect();
    let maintenance: Vec<Activity> = alertable
        .iter()
        .filter(|it| is_maintenance(it))
        .cloned()
        .collect();
    let normal: Vec<Activity> = alertable
        .iter()
        .filter(|it| !is_maintenance(it))
        .cloned()
        .collect();
    vec![
        (normal.clone(), DROP_ACTIVITY_NOTIFY_BEFORE_HOURS),
        (normal, NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS),
        (maintenance, NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS),
    ]
}

/// 按提醒时间分组处理: 正负10分钟内立即发送, 过时抛弃, 未来添加定时任务(去重)
async fn schedule_alert_group(activities: &[Activity], locale: ServerLocale, before_hours: i64) {
    if activities.is_empty() {
        return;
    }
    let now = chrono::Utc::now().timestamp_millis();
    let window = ALERT_IMMEDIATE_WINDOW_MILLIS;
    let mut groups: std::collections::BTreeMap<i64, Vec<Activity>> =
        std::collections::BTreeMap::new();
    for activity in activities {
        if activity.end_time <= 0 || activity.activity_type == ActivityType::Birthday {
            continue;
        }
        groups
            .entry(activity.end_time - before_hours * 60 * 60 * 1000)
            .or_default()
            .push(activity.clone());
    }
    for (notify_at, group) in groups {
        if notify_at < now - window {
            // 已过期, 抛弃
        } else if notify_at <= now + window {
            send_alert(&group, locale, before_hours).await;
        } else {
            insert_alert(&group, notify_at, locale, before_hours);
        }
    }
    if before_hours == NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS {
        // 1 小时预警同时安排「1 小时 5 分钟后」刷新本地资源图片
        schedule_image_refresh(activities, locale);
    }
}

/// 创建单次定时任务用于活动结束预警; 已存在同 key 任务则跳过, 避免重复
fn insert_alert(
    activities: &[Activity],
    expected_ms: i64,
    locale: ServerLocale,
    before_hours: i64,
) {
    let key = format!(
        "StandaloneActivityNotifyOneHour-{}-{}-{before_hours}",
        locale.command_name(),
        expected_ms
    );
    if quartz::exists(&key) {
        return;
    }
    let group = activities.to_vec();
    quartz::create_single_at(
        expected_ms,
        &key,
        "StandaloneActivityNotifyOneHour",
        Arc::new(move || {
            let activities = group.clone();
            tokio::spawn(async move {
                send_alert(&activities, locale, before_hours).await;
            });
        }),
    );
}

/// 活动到期后 5 分钟刷新本地资源图片: 对每个尚未到期的结束时刻各安排一个单次任务
fn schedule_image_refresh(activities: &[Activity], locale: ServerLocale) {
    let now = chrono::Utc::now().timestamp_millis();
    let mut refresh_times: Vec<i64> = Vec::new();
    for activity in activities {
        if activity.end_time <= 0 || activity.activity_type == ActivityType::Birthday {
            continue;
        }
        let refresh_at = activity.end_time + IMAGE_REFRESH_AFTER_END_MILLIS;
        if refresh_at > now && !refresh_times.contains(&refresh_at) {
            refresh_times.push(refresh_at);
        }
    }
    for refresh_at in refresh_times {
        insert_image_refresh(refresh_at, locale);
    }
}

/// 创建单次定时任务用于刷新本地资源图片; 已存在同 key 任务则跳过, 避免重复
fn insert_image_refresh(expected_ms: i64, locale: ServerLocale) {
    let key = format!(
        "AronaActivityImageRefresh-{}-{expected_ms}",
        locale.command_name()
    );
    if quartz::exists(&key) {
        return;
    }
    quartz::create_single_at(
        expected_ms,
        &key,
        "AronaActivityImageRefresh",
        Arc::new(move || {
            tokio::spawn(async move {
                crate::standalone::commands::activity::refresh_image(locale).await;
            });
        }),
    );
}

/// 发送活动结束预警(定时任务与立即发送共用)
async fn send_alert(activity: &[Activity], locale: ServerLocale, before_hours: i64) {
    if !crate::runtime::services::sender_ready() {
        return;
    }
    let config = standalone::notify_config();
    let targets = resolve_targets(&config, crate::runtime::config::groups());
    if targets.is_empty() {
        return;
    }
    let mut remaining: Vec<Activity> = activity.to_vec();
    let maintenance: Option<Activity> = {
        let found: Vec<Activity> = remaining
            .iter()
            .filter(|it| is_maintenance(it))
            .cloned()
            .collect();
        if found.is_empty() {
            None
        } else {
            remaining.retain(|it| !is_maintenance(it));
            found.first().cloned()
        }
    };
    let server_name = locale.server_name();
    let mut texts: Vec<String> = Vec::new();
    if let Some(_maintenance) = maintenance {
        texts.push(format!("距离{server_name}维护还有{before_hours}小时"));
    }
    if !remaining.is_empty() {
        let mut body = String::new();
        for item in &remaining {
            body.push_str(&format!("{}\n", item.content));
        }
        texts.push(format!(
            "{}({server_name})\n{body}将会在{before_hours}小时后结束",
            config.notify_text
        ));
    }
    for text in texts {
        let message = OutgoingMessage::text(text);
        for &group_id in &targets {
            let receipt = crate::runtime::services::send_message(
                MessageTarget::Group(group_id),
                message.clone(),
            )
            .await;
            if receipt.message_id.is_none() {
                crate::runtime::log::warning(format!("预警发送到群 {group_id} 失败"));
            }
        }
    }
}

fn is_maintenance(activity: &Activity) -> bool {
    activity.activity_type == ActivityType::Maintenance
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::message::{
        BoxFuture, MessageReceipt, MessageSegment, MessageSender, MessageTarget,
    };
    use chrono::{Local, Timelike};
    use std::sync::Mutex;
    use std::time::Duration;

    struct CaptureSender {
        sent: Mutex<Vec<(MessageTarget, OutgoingMessage)>>,
    }

    impl CaptureSender {
        fn new() -> CaptureSender {
            CaptureSender {
                sent: Mutex::new(Vec::new()),
            }
        }

        fn take(&self) -> Vec<(MessageTarget, OutgoingMessage)> {
            self.sent.lock().unwrap().drain(..).collect()
        }
    }

    impl MessageSender for CaptureSender {
        fn send<'a>(
            &'a self,
            target: MessageTarget,
            message: OutgoingMessage,
        ) -> BoxFuture<'a, MessageReceipt> {
            Box::pin(async move {
                self.sent.lock().unwrap().push((target, message));
                MessageReceipt {
                    message_id: Some(1),
                }
            })
        }
    }

    fn message_text(message: &OutgoingMessage) -> String {
        message
            .segments
            .iter()
            .filter_map(|segment| match segment {
                MessageSegment::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<String>>()
            .join("")
    }

    fn activity(kind: ActivityType, end_ms: i64, content: &str) -> Activity {
        Activity {
            content: content.to_string(),
            time: String::new(),
            activity_type: kind,
            server: ServerLocale::JP,
            start_time: end_ms - 7 * 24 * 3600 * 1000,
            end_time: end_ms,
        }
    }

    fn write_test_config() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("arona-notify-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("arona.yml");
        std::fs::write(
            &path,
            "groups: [910001, 910002]\n\
             managers: []\n\
             notify:\n\
             \x20 enable: true\n\
             \x20 every_day_hour: 8\n\
             \x20 jp: true\n\
             \x20 global: true\n\
             \x20 cn: true\n\
             \x20 black_groups: [910002]\n\
             \x20 notify_text: \"碧蓝档案预警\"\n",
        )
        .unwrap();
        path
    }

    /// 同时校验：预警分组(5小时/1小时/维护/生日)、黑名单、即时发送、未来定时、过期抛弃、任务去重、每日任务时刻
    #[tokio::test]
    async fn notify_logic_plan_and_alerts() {
        crate::runtime::config::set_bot_id(10000);
        crate::config::standalone::init(write_test_config());
        let sender = Arc::new(CaptureSender::new());
        crate::runtime::services::set_message_sender(sender.clone());

        let now = chrono::Utc::now().timestamp_millis();
        let normal = activity(ActivityType::Activity, now + 2 * 3600_000, "普通活动");
        let drop = activity(ActivityType::PickUp, now + 6 * 3600_000, "双倍掉落活动");
        let maintenance = activity(ActivityType::Maintenance, now + 2 * 3600_000, "维护预告");
        let birthday = activity(ActivityType::Birthday, now + 2 * 3600_000, "角色生日");
        let plan = alert_plan(&[normal, drop, maintenance, birthday]);

        assert_eq!(plan.len(), 3, "预警计划应有 3 组");
        // 第 1 组：除维护/生日外的活动，提前 5 小时
        assert_eq!(plan[0].1, DROP_ACTIVITY_NOTIFY_BEFORE_HOURS);
        assert!(plan[0].0.iter().any(|a| a.content == "普通活动"));
        assert!(plan[0].0.iter().any(|a| a.content == "双倍掉落活动"));
        assert!(!plan[0].0.iter().any(|a| a.content == "角色生日"), "生日不应预警");
        assert!(!plan[0].0.iter().any(|a| a.content == "维护预告"), "维护不走 5 小时");
        // 第 2 组：普通活动提前 1 小时
        assert_eq!(plan[1].1, NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS);
        assert_eq!(plan[1].0.len(), 2);
        // 第 3 组：维护单列，只提前 1 小时
        assert_eq!(plan[2].1, NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS);
        assert_eq!(plan[2].0.len(), 1);
        assert_eq!(plan[2].0[0].content, "维护预告");

        // 黑名单：910002 被排除
        let targets = resolve_targets(&standalone::notify_config(), vec![910001, 910002]);
        assert_eq!(targets, vec![910001]);

        // 立即发送（1 小时组：1 分钟后到点，落在正负 10 分钟窗口内）
        let soon = activity(ActivityType::Activity, now + 3600_000 + 60_000, "即将结束");
        schedule_alert_group(&[soon], ServerLocale::JP, NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS).await;
        let sent = sender.take();
        let texts: Vec<String> = sent.iter().map(|(_, m)| message_text(m)).collect();
        assert!(
            texts.iter().any(|t| t.contains("即将结束") && t.contains("将会在1小时后结束")),
            "1 小时预警未发送: {texts:?}"
        );
        assert!(
            sent.iter()
                .all(|(target, _)| *target == MessageTarget::Group(910001)),
            "预警发送到了黑名单群: {sent:?}"
        );

        // 立即发送（5 小时组：双倍掉落）
        let drop_soon = activity(ActivityType::PickUp, now + 5 * 3600_000 + 60_000, "双倍掉落即将结束");
        schedule_alert_group(
            &[drop_soon],
            ServerLocale::JP,
            DROP_ACTIVITY_NOTIFY_BEFORE_HOURS,
        )
        .await;
        let texts: Vec<String> = sender
            .take()
            .iter()
            .map(|(_, m)| message_text(m))
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("双倍掉落即将结束") && t.contains("将会在5小时后结束")),
            "5 小时预警未发送: {texts:?}"
        );

        // 维护活动：特殊文案
        let maintenance_soon =
            activity(ActivityType::Maintenance, now + 3600_000 + 60_000, "维护安排");
        schedule_alert_group(
            &[maintenance_soon],
            ServerLocale::CN,
            NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS,
        )
        .await;
        let texts: Vec<String> = sender
            .take()
            .iter()
            .map(|(_, m)| message_text(m))
            .collect();
        assert!(
            texts.iter().any(|t| t == "距离国服维护还有1小时"),
            "维护预警文案异常: {texts:?}"
        );

        // 未来活动 -> 建定时任务；同一活动重复调度 -> 去重
        let far = activity(ActivityType::Activity, now + 100 * 3600_000, "很久以后的活动");
        schedule_alert_group(&[far.clone()], ServerLocale::JP, NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS)
            .await;
        schedule_alert_group(&[far.clone()], ServerLocale::JP, NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS)
            .await;
        let key = format!(
            "StandaloneActivityNotifyOneHour-{}-{}-{}",
            ServerLocale::JP.command_name(),
            far.end_time - NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS * 3600_000,
            NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS
        );
        assert!(crate::quartz::exists(&key), "未来活动未创建预警定时任务");
        let count = crate::quartz::list()
            .iter()
            .filter(|task| task.name == key)
            .count();
        assert_eq!(count, 1, "预警定时任务未去重");

        // 1 小时预警同时安排「到期后 5 分钟」的本地资源图片刷新任务(1小时 + 5分钟)
        let refresh_key = format!(
            "AronaActivityImageRefresh-{}-{}",
            ServerLocale::JP.command_name(),
            far.end_time + IMAGE_REFRESH_AFTER_END_MILLIS
        );
        assert!(
            crate::quartz::exists(&refresh_key),
            "活动到期后未安排本地资源图片刷新任务"
        );
        let refresh_count = crate::quartz::list()
            .iter()
            .filter(|task| task.name == refresh_key)
            .count();
        assert_eq!(refresh_count, 1, "本地资源图片刷新任务未去重");
        // 单次任务的下次触发时间由任务协程写入, 稍等片刻再读取
        tokio::time::sleep(Duration::from_millis(200)).await;
        let refresh = crate::quartz::list()
            .into_iter()
            .find(|task| task.name == refresh_key)
            .expect("未找到本地资源图片刷新任务");
        assert_eq!(
            refresh.next_fire_ms,
            Some(far.end_time + IMAGE_REFRESH_AFTER_END_MILLIS),
            "本地资源图片刷新时间应为活动到期后 5 分钟"
        );
        crate::quartz::remove(&refresh_key);

        // 已过期活动 -> 抛弃，不发送也不建任务
        let expired = activity(ActivityType::Activity, now - 10 * 3600_000, "早就结束的活动");
        schedule_alert_group(
            &[expired],
            ServerLocale::JP,
            NORMAL_ACTIVITY_NOTIFY_BEFORE_HOURS,
        )
        .await;
        assert!(sender.take().is_empty(), "过期活动不应发送预警");

        // 每日推送任务：注册在配置的小时，且下次触发为整点
        enable_daily_job(8);
        assert!(crate::quartz::exists("StandaloneActivityNotify"), "每日推送任务未注册");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let info = crate::quartz::list()
            .into_iter()
            .find(|task| task.name == "StandaloneActivityNotify")
            .expect("未找到每日推送任务");
        let next = info.next_fire_ms.expect("每日推送任务没有下次触发时间");
        let fire_at = chrono::DateTime::from_timestamp_millis(next)
            .expect("时间戳无效")
            .with_timezone(&Local);
        assert_eq!(fire_at.hour(), 8, "每日推送小时不符: {fire_at}");
        assert_eq!(fire_at.minute(), 0);
        assert_eq!(fire_at.second(), 0);
        println!("[每日推送] 下次触发: {fire_at}");

        crate::quartz::remove("StandaloneActivityNotify");
        crate::quartz::remove("StandaloneActivityNotifyInit");
    }

    /// 端到端：真正执行一次每日推送（联网拉 GameKee 活动 + 渲染日历图）
    #[ignore = "联网自检, 运行: cargo test notify_daily -- --ignored --nocapture --test-threads=1"]
    #[tokio::test]
    async fn notify_daily_push_network() {
        crate::runtime::config::set_bot_id(10000);
        crate::config::standalone::init(write_test_config());
        let sender = Arc::new(CaptureSender::new());
        crate::runtime::services::set_message_sender(sender.clone());

        push(false).await;

        let sent = sender.take();
        println!("[每日推送] 共发送 {} 条", sent.len());
        for (target, message) in &sent {
            println!("[每日推送] -> {target:?}");
            for segment in &message.segments {
                match segment {
                    MessageSegment::Text(text) => println!("   文本: {text}"),
                    MessageSegment::Image { file, .. } => {
                        if let Some(file) = file {
                            assert!(
                                std::path::Path::new(file).exists(),
                                "推送图片不存在: {file}"
                            );
                            println!("   图片: {file}");
                        }
                    }
                    other => println!("   其它: {other:?}"),
                }
            }
        }
        assert!(!sent.is_empty(), "每日推送未发送任何消息（可能未配置目标群或拉取失败）");
        assert!(
            sent.iter()
                .all(|(target, _)| *target == MessageTarget::Group(910001)),
            "推送发送到了黑名单群"
        );
    }
}