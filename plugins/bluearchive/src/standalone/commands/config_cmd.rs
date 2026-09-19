//! /config 命令（对应原版 StandaloneConfigCommand）
//! /config             查看全部配置
//! /config <配置名>     查看单个配置
//! /config <配置名> <值> 修改配置并热重载
//!
//! groups/managers 由框架通用配置持有者读写；notify.* 属本插件自持有的配置区，
//! 走 [`crate::config`] 的原样片段读写路径。

use crate::config::NotifyConfig;
use arona::config::standalone;
use arona::runtime::dispatcher::CommandContext;
use arona::runtime::message::OutgoingMessage;
use arona::runtime::value::{self, ConfigValue};
use std::sync::Arc;

const USAGE: &str = "用法: /config 查看全部配置; /config <配置名> 查看单个; /config <配置名> <值> 修改配置; /config groups|managers|notify.black_groups add|del [数值] 追加/删除(群内省略数值时用当前群号)";

/// (点号键, 说明) —— 框架通用项
fn generic_fields() -> Vec<(&'static str, &'static str)> {
    standalone::FIELDS
        .iter()
        .map(|f| (f.key, f.description))
        .collect()
}

/// (点号键, 说明) —— 本插件 notify 配置项
fn notify_fields() -> [(&'static str, &'static str); 7] {
    [
        ("notify.enable", "是否启用每日活动防侠推送"),
        ("notify.every_day_hour", "每日推送的小时(0-23)"),
        ("notify.jp", "是否推送日服活动"),
        ("notify.global", "是否推送国际服活动"),
        ("notify.cn", "是否推送国服活动"),
        (
            "notify.black_groups",
            "不推送的群号列表（黑名单），留空表示推送到全部允许的群",
        ),
        ("notify.notify_text", "推送消息开头文字"),
    ]
}

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
                Some(OutgoingMessage::text(update_field(
                    &arguments[0],
                    &value,
                    show_value,
                )))
            }
        }
    }
}

/// 点号键 -> 去掉 "notify." 前缀后的子键（非 notify 项返回 None）
fn notify_sub_key(key: &str) -> Option<&str> {
    key.strip_prefix("notify.")
}

fn update_field(key: &str, raw_value: &str, show_value: bool) -> String {
    if standalone::find_field_index(key).is_some() {
        return standalone::update(key, raw_value, show_value);
    }
    if let Some(sub) = notify_sub_key(key) {
        let mut config = crate::config::notify();
        let current = notify_value(&config, sub);
        let parsed = match value::parse(raw_value, &current) {
            Ok(v) => v,
            Err(err) => return err,
        };
        if let Err(err) = notify_set(&mut config, sub, parsed) {
            return err;
        }
        return match crate::config::set_notify(&config) {
            Ok(()) => {
                let value_text = notify_value(&config, sub).display_kt();
                if show_value {
                    format!("配置已更新: {key} = {value_text}")
                } else {
                    format!("配置已更新: {key}")
                }
            }
            Err(err) => format!("写入配置文件失败: {err}"),
        };
    }
    format!("未找到配置项: {key}")
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
    if key == "notify.black_groups" {
        return OutgoingMessage::text(mutate_notify_black_groups(value, is_add, show_value));
    }
    OutgoingMessage::text(if is_add {
        standalone::add_to_list(&key, value, show_value)
    } else {
        standalone::remove_from_list(&key, value, show_value)
    })
}

fn mutate_notify_black_groups(value: i64, is_add: bool, show_value: bool) -> String {
    let mut config = crate::config::notify();
    if is_add {
        if config.black_groups.contains(&value) {
            return format!("notify.black_groups 已包含 {value}");
        }
        config.black_groups.push(value);
    } else {
        if !config.black_groups.contains(&value) {
            return format!("notify.black_groups 不包含 {value}");
        }
        config.black_groups.retain(|v| *v != value);
    }
    match crate::config::set_notify(&config) {
        Ok(()) => {
            if show_value {
                format!(
                    "配置已更新: notify.black_groups = {:?}",
                    config.black_groups
                )
            } else {
                "配置已更新: notify.black_groups".to_string()
            }
        }
        Err(err) => format!("写入配置文件失败: {err}"),
    }
}

fn view_all() -> OutgoingMessage {
    let config = standalone::config();
    let notify = crate::config::notify();
    let mut text = String::from(
        "Arona 全部配置\n配置文件: arona-standalone/arona.yml (修改保存后自动热重载)\n",
    );
    text.push_str(USAGE);
    text.push('\n');
    for (key, description) in generic_fields() {
        let value = match key {
            "groups" => format!("{:?}", config.groups),
            "managers" => format!("{:?}", config.managers),
            _ => String::new(),
        };
        text.push_str(&field_line(key, value, description));
    }
    for (key, description) in notify_fields() {
        let sub = notify_sub_key(key).unwrap_or(key);
        text.push_str(&field_line(
            key,
            notify_value(&notify, sub).display(),
            description,
        ));
    }
    OutgoingMessage::text(text)
}

fn view_one(key: &str) -> OutgoingMessage {
    if standalone::find_field_index(key).is_some() {
        let config = standalone::config();
        let description = standalone::FIELDS
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.description)
            .unwrap_or("");
        let value = match key {
            "groups" => format!("{:?}", config.groups),
            "managers" => format!("{:?}", config.managers),
            _ => String::new(),
        };
        return OutgoingMessage::text(field_line(key, value, description));
    }
    if let Some(entry) = notify_fields().iter().find(|(k, _)| *k == key) {
        let notify = crate::config::notify();
        let sub = notify_sub_key(key).unwrap_or(key);
        return OutgoingMessage::text(field_line(
            key,
            notify_value(&notify, sub).display(),
            entry.1,
        ));
    }
    OutgoingMessage::text(format!("未找到配置项: {key}\n{USAGE}"))
}

fn notify_value(config: &NotifyConfig, sub: &str) -> ConfigValue {
    match sub {
        "enable" => ConfigValue::Bool(config.enable),
        "every_day_hour" => ConfigValue::Int(config.every_day_hour),
        "jp" => ConfigValue::Bool(config.jp),
        "global" => ConfigValue::Bool(config.global),
        "cn" => ConfigValue::Bool(config.cn),
        "black_groups" => ConfigValue::ListLong(config.black_groups.clone()),
        "notify_text" => ConfigValue::Text(config.notify_text.clone()),
        _ => ConfigValue::Text(String::new()),
    }
}

fn notify_set(config: &mut NotifyConfig, sub: &str, value: ConfigValue) -> Result<(), String> {
    match sub {
        "enable" => config.enable = expect_bool(value)?,
        "every_day_hour" => {
            let hour = expect_int(value)?;
            if !(0..=23).contains(&hour) {
                return Err("推送小时必须在 0-23 之间".to_string());
            }
            config.every_day_hour = hour;
        }
        "jp" => config.jp = expect_bool(value)?,
        "global" => config.global = expect_bool(value)?,
        "cn" => config.cn = expect_bool(value)?,
        "black_groups" => config.black_groups = expect_list(value)?,
        "notify_text" => config.notify_text = expect_text(value)?,
        _ => return Err("未知配置项".to_string()),
    }
    Ok(())
}

fn expect_bool(value: ConfigValue) -> Result<bool, String> {
    match value {
        ConfigValue::Bool(v) => Ok(v),
        _ => Err("类型错误：应为布尔值".to_string()),
    }
}

fn expect_int(value: ConfigValue) -> Result<i32, String> {
    match value {
        ConfigValue::Int(v) => Ok(v),
        _ => Err("类型错误：应为整数".to_string()),
    }
}

fn expect_text(value: ConfigValue) -> Result<String, String> {
    match value {
        ConfigValue::Text(v) => Ok(v),
        _ => Err("类型错误：应为字符串".to_string()),
    }
}

fn expect_list(value: ConfigValue) -> Result<Vec<i64>, String> {
    match value {
        ConfigValue::ListLong(v) => Ok(v),
        _ => Err("notify.black_groups 不是列表配置项".to_string()),
    }
}

fn field_line(key: &str, value: String, description: &str) -> String {
    format!("【{key}】{value}\n说明: {description}\n")
}
