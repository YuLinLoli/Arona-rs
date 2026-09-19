//! 抽卡命令（对应原版 StandaloneGacha + StandaloneGachaAdmin）
//! 抽卡结果默认渲染为 2340x1080 结果图（image/gacha/result），头像本地缓存于
//! image/gacha/avatar；渲染失败（字体缺失/无权限等）时回退为纯文本结果。

use crate::db::dao;
use crate::gacha;
use crate::image::gacha as gacha_image;
use crate::runtime::gacha_config;
use arona::runtime::dispatcher::CommandContext;
use arona::runtime::message::OutgoingMessage;
use std::sync::Arc;

/// /单抽
pub async fn single_draw(
    context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    let group_id = context.group_id?;
    let user_id = context.user_id;
    let teacher_name = gacha::teacher_name(group_id, user_id, context.sender_name.as_deref());
    let server = match gacha::resolve_draw_server(user_id, arguments.first().map(|s| s.as_str())) {
        Some(server) => server,
        None => return Some(OutgoingMessage::text("未知服务器, 可用: 日服/国服/国际服")),
    };
    match gacha::perform_draw(user_id, group_id, 1, server).await {
        Ok(None) => Some(
            OutgoingMessage::at(user_id)
                + OutgoingMessage::text(format!("{teacher_name},石头不够了哦,明天再来抽吧")),
        ),
        Ok(Some(report)) => Some(result_message(&report).await),
        Err(err) => Some(OutgoingMessage::text(format!("抽卡失败: {err}"))),
    }
}

/// /十连
pub async fn multi_draw(
    context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    let group_id = context.group_id?;
    let user_id = context.user_id;
    let teacher_name = gacha::teacher_name(group_id, user_id, context.sender_name.as_deref());
    let server = match gacha::resolve_draw_server(user_id, arguments.first().map(|s| s.as_str())) {
        Some(server) => server,
        None => return Some(OutgoingMessage::text("未知服务器, 可用: 日服/国服/国际服")),
    };
    match gacha::perform_draw(user_id, group_id, 10, server).await {
        Ok(None) => Some(
            OutgoingMessage::at(user_id)
                + OutgoingMessage::text(format!("{teacher_name},石头不够了哦,明天再来抽吧")),
        ),
        Ok(Some(report)) => Some(result_message(&report).await),
        Err(err) => Some(OutgoingMessage::text(format!("抽卡失败: {err}"))),
    }
}

/// 抽卡结果消息: 渲染结果图并附带撤回; 渲染失败回退文本(与原版 Kotlin 的
/// image 生成失败 -> text 回退一致, 撤回时间由 /抽卡 time 控制)
async fn result_message(report: &gacha::DrawReport) -> OutgoingMessage {
    let seconds = gacha_config::revoke_time();
    let mut message = match gacha_image::render_result(report).await {
        Ok(file) => OutgoingMessage::image_file(file.to_string_lossy()),
        Err(err) => {
            arona::runtime::log::warning(format!("抽卡结果图生成失败, 回退文本: {err}"));
            OutgoingMessage::text(gacha::format_report(report))
        }
    };
    if seconds > 0 {
        message = message.with_revoke(seconds as u64 * 1000);
    }
    message
}

/// /抽卡服务器
pub async fn set_server(
    context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    let raw = arguments.first().map(|s| s.as_str());
    match raw {
        None | Some("") => {
            let server = gacha::get_user_server(context.user_id);
            Some(OutgoingMessage::text(format!(
                "当前默认抽卡服务器: {}\n用法: /抽卡服务器 日服|国服|国际服",
                server.server_name()
            )))
        }
        Some(raw) => match gacha::resolve_server(Some(raw)) {
            Some(server) => {
                gacha::set_user_server(context.user_id, server);
                Some(OutgoingMessage::text(format!(
                    "抽卡服务器已设置为 {}",
                    server.server_name()
                )))
            }
            None => Some(OutgoingMessage::text(format!(
                "未知服务器: {raw}, 可用: 日服/国服/国际服"
            ))),
        },
    }
}

/// /狗叫
pub async fn dog_ranking(context: Arc<CommandContext>) -> Option<OutgoingMessage> {
    let group_id = context.group_id?;
    let dog_calls: Vec<dao::HistoryRow> = dao::dog_calls(group_id)
        .into_iter()
        .filter(|row| row.dog != 0)
        .collect();
    if dog_calls.is_empty() {
        return Some(OutgoingMessage::text("还没有老师抽出来哦"));
    }
    let mut text = String::from("狗叫排行:\n");
    for (index, row) in dog_calls.iter().enumerate() {
        let name = gacha::teacher_name(group_id, row.qq, None);
        text.push_str(&format!(
            "{}. {name}({}): {}抽\n",
            index + 1,
            row.qq,
            row.dog
        ));
    }
    text.pop();
    Some(OutgoingMessage::text(text))
}

/// /历史
pub async fn history_ranking(context: Arc<CommandContext>) -> Option<OutgoingMessage> {
    let group_id = context.group_id?;
    let history = dao::history_all(group_id);
    if history.is_empty() {
        return Some(OutgoingMessage::text("还没有记录哦"));
    }
    let mut text = String::from("历史排行:\n");
    for (index, row) in history.iter().take(6).enumerate() {
        let name = gacha::teacher_name(group_id, row.qq, None);
        let rate = if row.count3 == 0 {
            0
        } else {
            row.points / row.count3
        };
        text.push_str(&format!(
            "{}. {name}({}): {}抽/{}个3星 = {rate}\n",
            index + 1,
            row.qq,
            row.points,
            row.count3
        ));
    }
    text.pop();
    Some(OutgoingMessage::text(text))
}

/// /抽卡（管理）
pub async fn gacha_admin(
    context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    let sub = arguments
        .first()
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    match sub.as_str() {
        "" => Some(usage()),
        "list" | "列表" => Some(list_pools()),
        "setpool" | "设置池子" => {
            let pool = arguments.get(1).and_then(|v| v.parse::<i64>().ok());
            Some(set_pool(pool))
        }
        "reset" | "重置" => Some(reset(
            context.group_id,
            arguments.get(1).and_then(|v| v.parse::<i64>().ok()),
        )),
        "1s" => Some(set_rate(
            arguments.get(1).and_then(|v| v.parse::<f32>().ok()),
            "1星",
            "1s",
            |c, rate| c.star1_rate = rate,
        )),
        "2s" => Some(set_rate(
            arguments.get(1).and_then(|v| v.parse::<f32>().ok()),
            "2星",
            "2s",
            |c, rate| c.star2_rate = rate,
        )),
        "3s" => Some(set_rate(
            arguments.get(1).and_then(|v| v.parse::<f32>().ok()),
            "3星",
            "3s",
            |c, rate| c.star3_rate = rate,
        )),
        "p2s" => Some(set_rate(
            arguments.get(1).and_then(|v| v.parse::<f32>().ok()),
            "2星PickUp",
            "p2s",
            |c, rate| c.star2_pickup_rate = rate,
        )),
        "p3s" => Some(set_rate(
            arguments.get(1).and_then(|v| v.parse::<f32>().ok()),
            "3星PickUp",
            "p3s",
            |c, rate| c.star3_pickup_rate = rate,
        )),
        "time" => Some(set_time(
            arguments.get(1).and_then(|v| v.parse::<i32>().ok()),
        )),
        "limit" => Some(set_limit(
            arguments.get(1).and_then(|v| v.parse::<i32>().ok()),
        )),
        "update" | "更新" => Some(
            update(
                arguments.get(1).and_then(|v| v.parse::<i64>().ok()),
                arguments.get(2).cloned(),
            )
            .await,
        ),
        _ => Some(usage()),
    }
}

fn usage() -> OutgoingMessage {
    OutgoingMessage::text(
        "用法: /抽卡 list|setpool <id>|reset [pool]|1s|2s|3s|p2s|p3s|time|limit|update <id> [新池子名字]",
    )
}

fn list_pools() -> OutgoingMessage {
    let pools = dao::list_pools_desc(2);
    if pools.is_empty() {
        return OutgoingMessage::text("没有任何池子信息");
    }
    let mut lines: Vec<String> = Vec::new();
    for pool in pools {
        let students = dao::pool_characters(pool.id);
        if students.is_empty() {
            lines.push(format!("{}(id: {}): 没有关联的学生", pool.name, pool.id));
        } else {
            let joined: Vec<String> = students
                .iter()
                .map(|(name, star)| format!("{name}({star}★)"))
                .collect();
            lines.push(format!(
                "{}(id: {}): {}",
                pool.name,
                pool.id,
                joined.join(", ")
            ));
        }
    }
    lines.reverse();
    OutgoingMessage::text(lines.join("\n"))
}

fn set_pool(pool: Option<i64>) -> OutgoingMessage {
    let Some(pool) = pool else {
        return OutgoingMessage::text("用法: /抽卡 setpool <id>");
    };
    match dao::find_pool_by_id(pool) {
        Some(target) => {
            gacha_config::set(|config| config.active_pool = target.id as i32);
            OutgoingMessage::text(format!("池子设置为:{}", target.name))
        }
        None => OutgoingMessage::text("没有找到池子"),
    }
}

fn reset(group_id: Option<i64>, pool: Option<i64>) -> OutgoingMessage {
    let pool0 = pool.unwrap_or(gacha_config::active_pool() as i64);
    let Some(group_id) = group_id else {
        return OutgoingMessage::text("该功能仅限群聊使用");
    };
    dao::delete_history_of_group(group_id, pool0 as i32);
    dao::reset_limit_of_group(group_id);
    OutgoingMessage::text("历史记录重置成功")
}

fn set_rate(
    rate: Option<f32>,
    label: &str,
    usage_text: &str,
    setter: impl Fn(&mut gacha_config::GachaConfig, f32),
) -> OutgoingMessage {
    let Some(rate) = rate else {
        return OutgoingMessage::text(format!("用法: /抽卡 {usage_text} <rate>"));
    };
    gacha_config::set(|config| setter(config, rate));
    gacha_config::recalc_max_dot();
    OutgoingMessage::text(format!("{label}出货率设置为{rate}%"))
}

fn set_time(time: Option<i32>) -> OutgoingMessage {
    let Some(time) = time else {
        return OutgoingMessage::text("用法: /抽卡 time <秒>");
    };
    let enabled = time > 0;
    gacha_config::set(|config| config.revoke_time = if enabled { time } else { 0 });
    if enabled {
        OutgoingMessage::text(format!("撤回时间设置为{time}"))
    } else {
        OutgoingMessage::text("关闭抽卡结果撤回")
    }
}

fn set_limit(time: Option<i32>) -> OutgoingMessage {
    let Some(time) = time else {
        return OutgoingMessage::text("用法: /抽卡 limit <次数>");
    };
    let enabled = time > 0;
    gacha_config::set(|config| config.limit = if enabled { time } else { 0 });
    if enabled {
        OutgoingMessage::text(format!("每日限制次数设置为{time}"))
    } else {
        OutgoingMessage::text("每日限制次数设置为不限制每日抽卡次数")
    }
}

/// 远端卡池更新角色（对应原版 remote/action/GachaPoolUpdateRemoteService.GachaCharacter）
#[derive(Clone, Debug, serde::Deserialize)]
struct RemotePoolCharacter {
    #[serde(default)]
    name: String,
    #[serde(default)]
    star: i32,
    #[serde(default)]
    limit: i32,
}

/// 远端卡池更新数据（对应原版 GachaPoolUpdateData）
#[derive(Clone, Debug, serde::Deserialize)]
struct RemotePoolUpdateData {
    #[serde(default)]
    name: String,
    #[serde(default)]
    character: Vec<RemotePoolCharacter>,
}

/// /抽卡 update <id> [新池子名字]：从 arona 远端拉取卡池数据并落库
/// （对应原版 StandaloneGachaAdmin.update）
async fn update(id: Option<i64>, pool_name: Option<String>) -> OutgoingMessage {
    let Some(id) = id else {
        return OutgoingMessage::text("用法: /抽卡 update <id> [新池子名字]");
    };
    let response = match crate::data::arona_backend::fetch_remote_action(id).await {
        Ok(response) => response,
        Err(err) => {
            arona::runtime::log::warning(format!("从远端获取池子信息失败: {err}"));
            return OutgoingMessage::text("从远端获取池子信息失败");
        }
    };
    if response.data.action != "poolUpdate" {
        return OutgoingMessage::text("从远端获取池子信息失败");
    }
    let payload: RemotePoolUpdateData = match serde_json::from_str(&response.data.content) {
        Ok(payload) => payload,
        Err(err) => {
            arona::runtime::log::warning(format!("远端池子数据解析失败: {err}"));
            return OutgoingMessage::text("从远端获取池子信息失败");
        }
    };
    let target = match dao::find_pool_by_name(&payload.name) {
        None => dao::create_pool(&payload.name),
        Some(_) => {
            let Some(name) = pool_name
                .as_deref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
            else {
                return OutgoingMessage::text(format!(
                    "同名池子: {} 已经存在, 请使用\n/抽卡 update {id} 新池子名字\n来更新",
                    payload.name
                ));
            };
            dao::create_pool(name)
        }
    };
    let mut inserted: Vec<String> = Vec::new();
    for character in &payload.character {
        let character_id =
            dao::upsert_character(&character.name, character.star as i64, character.limit == 1);
        dao::add_pool_character(target, character_id);
        inserted.push(format!("{}({}★)", character.name, character.star));
    }
    let pool = dao::find_pool_by_id(target);
    let pool_name = pool
        .as_ref()
        .map(|pool| pool.name.clone())
        .unwrap_or(payload.name.clone());
    OutgoingMessage::text(format!(
        "新池子: {pool_name} 已添加, id: {target}\n{}\n使用指令\n/抽卡 setpool {target}\n来切换到这个池子",
        inserted.join(", ")
    ))
}
