//! /活动 命令与活动日历同步（对应原版 StandaloneActivity + StandaloneActivitySync）
//! /活动 优先输出活动日历图片（纯 Rust 渲染, 对应原版 createActivityImage）；
//! 字体不可用等渲染失败时自动回退为纯文本日历。
//! 图片作为本地资源常驻 arona-standalone/images/activity/activity-<服务>.png, 只在每日 0 点与
//! 活动到期后 5 分钟(1小时预警 + 1小时5分钟)由定时任务刷新; /活动 命中本地图片即直接发送。
//! 刷新触发点: 程序启动时、每日 0 点、活动到期后 5 分钟。

use crate::data;
use crate::db::dao;
use crate::entity::{Activity, ActivityType, ServerLocale};
use crate::quartz;
use crate::runtime::dispatcher::CommandContext;
use crate::runtime::message::OutgoingMessage;
use crate::util::time::calc_time;
use std::sync::Arc;

const CACHE_VALID_MILLS: i64 = 12 * 60 * 60 * 1000;

/// /活动 [日服|国服|国际服]
pub async fn activity(
    _context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    let source = arguments.first().map(|s| s.as_str());
    let pre_match = ServerLocale::ALL.into_iter().find(|server| {
        source == Some(server.command_name()) || source == Some(server.server_name())
    });
    let (server, failure_prefix) = match source {
        None | Some("") => (ServerLocale::JP, "日服"),
        Some(_) => match pre_match {
            None => {
                return Some(OutgoingMessage::text(
                    "参数不匹配, 是否想要执行:\n/活动 日服 # 查询日服活动\n/活动 国服 # 查询国服活动\n/活动 国际服 # 查询国际服活动",
                ));
            }
            Some(ServerLocale::JP) => (ServerLocale::JP, "日服"),
            Some(ServerLocale::GLOBAL) => (ServerLocale::GLOBAL, "国际服"),
            Some(ServerLocale::CN) => (ServerLocale::CN, "国服"),
        },
    };
    // 本地资源图片由定时任务(每日0点/活动到期后5分钟)刷新: 命中直接发送, 不联网也不重新渲染
    if let Some(file) = crate::image::activity::cached_image(server) {
        return Some(OutgoingMessage::image_file(file.to_string_lossy()));
    }
    match fetch(server).await {
        Ok(pair) => Some(activity_message(&pair, server)),
        Err(err) => Some(OutgoingMessage::text(format!(
            "获取{failure_prefix}活动失败: {err}"
        ))),
    }
}

/// 生成活动日历消息: 优先渲染图片, 失败回退文本
fn activity_message(
    pair: &(Vec<Activity>, Vec<Activity>),
    server: ServerLocale,
) -> OutgoingMessage {
    match crate::image::activity::render(pair, server) {
        Ok(file) => OutgoingMessage::image_file(file.to_string_lossy()),
        Err(err) => {
            crate::runtime::log::warning(format!("活动图片生成失败, 回退文本: {err}"));
            OutgoingMessage::text(data::activity::format_calendar(pair, server))
        }
    }
}

/// 优先读取数据库缓存(12小时内同步过), 否则联网拉取并写入数据库
async fn fetch(server: ServerLocale) -> Result<(Vec<Activity>, Vec<Activity>), String> {
    if let Some(pair) = load_from_db(server).await {
        return Ok(pair);
    }
    match data::activity::fetch(server).await {
        Ok(pair) => {
            save_to_db(server, &pair);
            Ok(pair)
        }
        Err(_) => match load_from_db(server).await {
            Some(pair) => Ok(pair),
            // 数据库也没有缓存时, 至少给出学生生日列表(不依赖游戏活动接口)
            None => Ok((
                Vec::new(),
                data::birthday::upcoming_birthday_activities(server).await,
            )),
        },
    }
}


/// 将活动写入数据库, 覆盖该服务器旧数据
pub fn save_to_db(server: ServerLocale, pair: &(Vec<Activity>, Vec<Activity>)) {
    let now = chrono::Utc::now().timestamp_millis();
    let mut rows: Vec<Activity> = pair.0.clone();
    rows.extend(pair.1.clone());
    // 生日不落库缓存, 每次读取时按当天重新计算
    rows.retain(|row| row.activity_type != ActivityType::Birthday);
    dao::save_activities(server, &rows, now);
}

/// 从数据库读取活动日历; 缓存过期或不存在时返回 None
async fn load_from_db(server: ServerLocale) -> Option<(Vec<Activity>, Vec<Activity>)> {
    let latest = dao::max_updated_at(server)?;
    if chrono::Utc::now().timestamp_millis() - latest > CACHE_VALID_MILLS {
        return None;
    }
    let rows = dao::load_activities(server);
    if rows.is_empty() {
        return None;
    }
    let now = chrono::Utc::now().timestamp_millis();
    let mut active: Vec<Activity> = Vec::new();
    let mut pending: Vec<Activity> = Vec::new();
    for mut row in rows {
        // 生日不落库缓存, 读取时按当天重新计算, 避免过期/重复
        if row.activity_type == ActivityType::Birthday {
            continue;
        }
        if row.start_time > now {
            row.time = calc_time(row.start_time, true);
            pending.push(row);
        } else if row.end_time > now {
            row.time = calc_time(row.end_time, false);
            active.push(row);
        }
    }
    pending.extend(data::birthday::upcoming_birthday_activities(server).await);
    pending.sort_by_key(|row| row.start_time);
    Some((active, pending))
}

/// 强制联网拉取最新活动并刷新本地资源图片(同时更新数据库缓存)
/// 调用方: 每日 0 点的定时任务、活动到期后 5 分钟的定时任务(1小时预警 + 1小时5分钟)
pub async fn refresh_image(server: ServerLocale) {
    match data::activity::fetch(server).await {
        Ok(pair) => {
            save_to_db(server, &pair);
            match crate::image::activity::render(&pair, server) {
                Ok(file) => crate::runtime::log::info(format!(
                    "{}本地活动图片已刷新: {}",
                    server.server_name(),
                    file.display()
                )),
                Err(err) => crate::runtime::log::warning(format!(
                    "刷新{}本地活动图片失败: {err}",
                    server.server_name()
                )),
            }
        }
        Err(err) => crate::runtime::log::warning(format!(
            "刷新{}本地活动图片失败: {err}",
            server.server_name()
        )),
    }
}

/// 刷新 日服/国际服/国服 三个服务器的本地资源图片
pub async fn refresh_all_images() {
    for server in ServerLocale::ALL {
        refresh_image(server).await;
    }
}

/// 注册每日 0 点刷新本地活动图片的定时任务(独立启动时调用)
pub fn enable_image_refresh_job() {
    quartz::create_daily(
        0,
        "AronaActivityImageRefreshDaily",
        "AronaActivityImageRefresh",
        Arc::new(|| {
            tokio::spawn(async move {
                refresh_all_images().await;
            });
        }),
    );
    crate::runtime::log::info("本地活动图片刷新任务已启用: 每天 0 点");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    /// 每日 0 点刷新本地活动图片的任务: 注册成功且下次触发为 0 点整
    #[tokio::test]
    async fn image_refresh_job_registered_at_midnight() {
        enable_image_refresh_job();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let info = crate::quartz::list()
            .into_iter()
            .find(|task| task.name == "AronaActivityImageRefreshDaily")
            .expect("未注册每日图片刷新任务");
        let next = info.next_fire_ms.expect("每日图片刷新任务没有下次触发时间");
        let fire_at = chrono::DateTime::from_timestamp_millis(next)
            .expect("时间戳无效")
            .with_timezone(&chrono::Local);
        assert_eq!(fire_at.hour(), 0, "刷新小时应为 0 点: {fire_at}");
        assert_eq!(fire_at.minute(), 0);
        assert_eq!(fire_at.second(), 0);
        println!("[本地图片刷新] 下次触发: {fire_at}");
        crate::quartz::remove("AronaActivityImageRefreshDaily");
    }
}