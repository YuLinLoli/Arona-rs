//! /config 命令（对应原版 StandaloneConfigCommand）
//! /config             查看全部配置
//! /config <配置名>     查看单个配置
//! /config <配置名> <值> 修改配置并热重载

use crate::config::arona::AronaConfig;
use crate::config::standalone;
use crate::runtime::dispatcher::CommandContext;
use crate::runtime::message::OutgoingMessage;
use std::sync::Arc;

const USAGE: &str = "用法: /config 查看全部配置; /config <配置名> 查看单个; /config <配置名> <值> 修改配置; /config groups|managers|notify.black_groups add|del [数值] 追加/删除(群内省略数值时用当前群号)";

/// /config
pub async fn handle(
    context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    match arguments.len() {
        0 => Some(view_all()),
        1 => Some(view_one(&arguments[0])),
        _ => {
            if is_list_mutation(&arguments) {
                Some(mutate_list(&context, &arguments))
            } else {
                let value = arguments[1..].join(" ");
                let show_value = context.group_id.is_none();
                Some(OutgoingMessage::text(standalone::update(
                    &arguments[0],
                    &value,
                    show_value,
                )))
            }
        }
    }
}

fn is_list_mutation(arguments: &[String]) -> bool {
    if arguments.len() < 2 {
        return false;
    }
    let op = arguments[1].to_lowercase();
    if op != "add" && op != "del" {
        return false;
    }
    matches!(
        arguments[0].as_str(),
        "groups" | "managers" | "notify.black_groups"
    )
}

fn mutate_list(context: &Arc<CommandContext>, arguments: &[String]) -> OutgoingMessage {
    let is_add = arguments[1].eq_ignore_ascii_case("add");
    let key = arguments[0].clone();
    let value = if arguments.len() >= 3 {
        match arguments[2].parse::<i64>() {
            Ok(value) => value,
            Err(_) => return OutgoingMessage::text(format!("数值格式错误: {}", arguments[2])),
        }
    } else if let Some(group_id) = context.group_id {
        group_id
    } else {
        let op = if is_add { "添加" } else { "删除" };
        return OutgoingMessage::text(format!(
            "请在群里发送 /config {key} {}，或指定要{op}的数值",
            arguments[1]
        ));
    };
    let show_value = context.group_id.is_none();
    let text = if is_add {
        standalone::add_to_list(&key, value, show_value)
    } else {
        standalone::remove_from_list(&key, value, show_value)
    };
    OutgoingMessage::text(text)
}

fn view_all() -> OutgoingMessage {
    let config = standalone::config();
    let mut text = String::from(
        "Arona 全部配置\n配置文件: arona-standalone/arona.yml (修改保存后自动热重载)\n",
    );
    text.push_str(USAGE);
    text.push('\n');
    for field in standalone::FIELDS.iter() {
        text.push_str(&field_line(
            field.key,
            field_value(&config, field.key),
            field.description,
        ));
    }
    OutgoingMessage::text(text)
}

fn view_one(key: &str) -> OutgoingMessage {
    let config = standalone::config();
    let Some(field) = standalone::FIELDS
        .iter()
        .find(|f| f.key == key || f.key.strip_prefix("notify.") == Some(key))
    else {
        return OutgoingMessage::text(format!("未找到配置项: {key}\n{USAGE}"));
    };
    OutgoingMessage::text(field_line(
        field.key,
        field_value(&config, field.key),
        field.description,
    ))
}

fn field_value(config: &AronaConfig, key: &str) -> String {
    match key {
        "groups" => format!("{:?}", config.groups),
        "managers" => format!("{:?}", config.managers),
        "notify.enable" => config.notify.enable.to_string(),
        "notify.every_day_hour" => config.notify.every_day_hour.to_string(),
        "notify.jp" => config.notify.jp.to_string(),
        "notify.global" => config.notify.global.to_string(),
        "notify.cn" => config.notify.cn.to_string(),
        "notify.black_groups" => format!("{:?}", config.notify.black_groups),
        "notify.notify_text" => format!("{:?}", config.notify.notify_text),
        _ => String::new(),
    }
}

fn field_line(key: &str, value: String, description: &str) -> String {
    format!("【{key}】{value}\n说明: {description}\n")
}
