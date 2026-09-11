//! 名字记录命令（对应原版 StandaloneName）

use crate::db::dao;
use crate::gacha;
use crate::runtime::dispatcher::CommandContext;
use crate::runtime::message::OutgoingMessage;
use std::sync::Arc;

/// /游戏名 [名称]
pub async fn game_name(
    context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    let user_id = context.user_id;
    let name = arguments.first().cloned().unwrap_or_default();
    if name.is_empty() {
        return match dao::get_game_name(user_id) {
            Some(current) => Some(OutgoingMessage::text(format!("当前游戏名为: {current}"))),
            None => Some(OutgoingMessage::text("游戏名未设置")),
        };
    }
    if name.chars().count() > 50 {
        return Some(OutgoingMessage::at(user_id) + OutgoingMessage::text("太长了, 爬"));
    }
    dao::set_game_name(user_id, &name);
    Some(OutgoingMessage::text(format!("游戏名已记录: {name}")))
}

/// /谁是 <游戏名>
pub async fn search(
    _context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    let my_name = arguments.first().cloned().unwrap_or_default();
    if my_name.is_empty() {
        return Some(OutgoingMessage::text("用法: /谁是 <游戏名>"));
    }
    let result = dao::search_game_name(&my_name);
    if result.is_empty() {
        return Some(OutgoingMessage::text(format!(
            "没有游戏名叫 '{my_name}' 的群友"
        )));
    }
    let lines: Vec<String> = result
        .iter()
        .map(|(name, qq)| format!("{name}({qq})"))
        .collect();
    Some(OutgoingMessage::text(format!(
        "查询结果:\n{}",
        lines.join("\n")
    )))
}

/// /叫我 [昵称]
pub async fn call_me(
    context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    let user_id = context.user_id;
    let group_id = context.group_id?;
    let name = arguments.first().cloned().unwrap_or_default();
    if name.is_empty() {
        let teacher_name = gacha::teacher_name(group_id, user_id, context.sender_name.as_deref());
        return Some(
            OutgoingMessage::at(user_id) + OutgoingMessage::text(format!("怎么了, {teacher_name}")),
        );
    }
    if name.chars().count() > 20 {
        return Some(OutgoingMessage::at(user_id) + OutgoingMessage::text("太长了, 爬"));
    }
    dao::set_teacher_name(group_id, user_id, &name);
    let display = if name.ends_with("老师") {
        name
    } else {
        format!("{name}老师")
    };
    Some(OutgoingMessage::text(format!("好的, {display}")))
}
