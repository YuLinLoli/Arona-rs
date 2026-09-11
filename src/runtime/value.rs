//! /config 指令的配置值解析（对应原版 ConfigValueParser）

use std::fmt;

/// 配置项值类型（独立模式 /config 涉及的字段类型）
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigValue {
    Bool(bool),
    Int(i32),
    Long(i64),
    Text(String),
    /// 长整型列表（groups/managers/notify.black_groups）
    ListLong(Vec<i64>),
}

impl ConfigValue {
    pub fn display(&self) -> String {
        match self {
            ConfigValue::Bool(v) => v.to_string(),
            ConfigValue::Int(v) => v.to_string(),
            ConfigValue::Long(v) => v.to_string(),
            ConfigValue::Text(v) => v.clone(),
            ConfigValue::ListLong(v) => format!("{:?}", v),
        }
    }

    /// 原版 Kotlin List<*> 的 toString 形如 [1, 2, 3]
    pub fn display_kt(&self) -> String {
        match self {
            ConfigValue::ListLong(v) => {
                let inner = v
                    .iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[{inner}]")
            }
            other => other.display(),
        }
    }
}

impl fmt::Display for ConfigValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_kt())
    }
}

fn parse_bool(raw: &str) -> Result<bool, String> {
    match raw.trim().to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" | "是" | "开" => Ok(true),
        "false" | "0" | "no" | "off" | "否" | "关" => Ok(false),
        _ => Err(format!("无法解析为布尔值: {raw} (可选 true/false 或 1/0)")),
    }
}

fn parse_long(raw: &str) -> Result<i64, String> {
    raw.trim()
        .parse::<i64>()
        .map_err(|_| format!("无法解析为长整数: {raw}"))
}

fn parse_int(raw: &str) -> Result<i32, String> {
    raw.trim()
        .parse::<i32>()
        .map_err(|_| format!("无法解析为整数: {raw}"))
}

/// 列表支持 "1,2,3"、"[1, 2, 3]"、"1 2 3" 三种写法
fn parse_list_long(raw: &str) -> Result<Vec<i64>, String> {
    let cleaned = raw
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if cleaned.is_empty() {
        return Ok(Vec::new());
    }
    let tokens: Vec<&str> = cleaned
        .split(|c: char| c == ',' || c == '，' || c.is_whitespace())
        .filter(|s| !s.trim().is_empty())
        .collect();
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        out.push(parse_long(token)?);
    }
    Ok(out)
}

/// 根据当前值的类型解析字符串
pub fn parse(raw: &str, current: &ConfigValue) -> Result<ConfigValue, String> {
    match current {
        ConfigValue::Bool(_) => parse_bool(raw).map(ConfigValue::Bool),
        ConfigValue::Int(_) => parse_int(raw).map(ConfigValue::Int),
        ConfigValue::Long(_) => parse_long(raw).map(ConfigValue::Long),
        ConfigValue::Text(_) => Ok(ConfigValue::Text(raw.to_string())),
        ConfigValue::ListLong(_) => parse_list_long(raw).map(ConfigValue::ListLong),
    }
}
