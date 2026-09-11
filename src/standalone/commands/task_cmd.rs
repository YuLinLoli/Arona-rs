//! /任务 命令（对应原版 StandaloneTaskCommand）

use crate::quartz;
use crate::runtime::message::OutgoingMessage;

/// /任务 [list|trigger <任务名>]
pub fn handle(arguments: &[String]) -> OutgoingMessage {
    let sub = arguments
        .first()
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    match sub.as_str() {
        "" | "list" | "列表" => list(),
        "trigger" | "触发" => trigger(arguments.get(1).map(|s| s.as_str())),
        _ => usage(),
    }
}

fn usage() -> OutgoingMessage {
    OutgoingMessage::text("用法: /任务 查看全部定时任务; /任务 触发 <任务名> 手动触发")
}

fn list() -> OutgoingMessage {
    let tasks = quartz::list();
    if tasks.is_empty() {
        return OutgoingMessage::text("当前没有定时任务");
    }
    let mut text = format!("定时任务({}):\n", tasks.len());
    for task in tasks {
        text.push_str(&format!("· {} [{}]", task.name, task.group));
        if let Some(next) = task.next_fire_ms {
            text.push_str(&format!(" 下次:{}", format_time(next)));
        }
        if let Some(last) = task.last_fire_ms {
            text.push_str(&format!(" 上次:{}", format_time(last)));
        }
        text.push('\n');
    }
    text.pop();
    OutgoingMessage::text(text)
}

fn trigger(name: Option<&str>) -> OutgoingMessage {
    let Some(name) = name else {
        return OutgoingMessage::text("用法: /任务 触发 <任务名>");
    };
    let name = name.trim();
    match quartz::trigger(name) {
        Ok(()) => OutgoingMessage::text(format!("已触发任务: {name}")),
        Err(err) => OutgoingMessage::text(err),
    }
}

fn format_time(ms: i64) -> String {
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(time) => time
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => "未知".to_string(),
    }
}
