//! 命令参数声明与解析（对应 mirai-console 的 `ArgParser` / `ArgParserCombinationDsl`）
//!
//! 插件只声明「这条命令要几个什么类型的参数」，切分、类型转换、范围校验、
//! 缺参时的用法回显全部由框架完成（见 [`crate::runtime::dispatcher`]）。
//! 未声明参数的命令仍按老写法拿到原始词表，两者共存。
//!
//! ```ignore
//! use arona::runtime::args::arg;
//!
//! CommandRegistration::new(vec!["/ban".into()], "禁言", typed_handler(|ctx, args| async move {
//!     let minutes = args.i64("分钟").unwrap_or(10);
//!     ...
//! }))
//! .with_args(vec![
//!     arg::i64("群号"),                       // 必填，非数字直接报用法
//!     arg::i64("分钟").optional().with_default("10").range(1, 1440),
//!     arg::choice("单位", &["分钟", "小时"]).optional(),
//!     arg::rest("原因").optional(),           // 吃掉剩下的全部文本
//! ]);
//! ```

use std::fmt;

/// 参数类型
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// 一个非空白词
    Text,
    /// 吃掉剩余全部文本（一般放最后一个）
    Rest,
    /// 整数
    Int,
    /// 布尔：`是/否`、`true/false`、`on/off`、`开/关`、`1/0`
    Bool,
    /// 枚举：取值必须在候选里（大小写不敏感，命中后给出候选表里的原样写法）
    Choice(&'static [&'static str]),
}

/// 已解析的参数值
#[derive(Clone, Debug, PartialEq)]
pub enum ArgValue {
    Text(String),
    Int(i64),
    Bool(bool),
}

impl ArgValue {
    /// 只有文本型参数能免拷贝借出；数字/布尔要用 [`ArgValue::display`]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            ArgValue::Text(value) => Some(value),
            _ => None,
        }
    }

    pub fn display(&self) -> String {
        match self {
            ArgValue::Text(value) => value.clone(),
            ArgValue::Int(value) => value.to_string(),
            ArgValue::Bool(value) => (if *value { "true" } else { "false" }).to_string(),
        }
    }
}

impl fmt::Display for ArgValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.display())
    }
}

/// 一个参数声明
#[derive(Clone, Debug)]
pub struct ArgSpec {
    pub name: String,
    pub kind: ArgKind,
    pub optional: bool,
    pub default: Option<String>,
    /// 仅对 [`ArgKind::Int`] 生效
    pub range: Option<(i64, i64)>,
    /// 展示在用法里的占位符（默认用参数名）
    pub placeholder: Option<String>,
}

impl ArgSpec {
    fn new(name: &str, kind: ArgKind) -> ArgSpec {
        ArgSpec {
            name: name.to_string(),
            kind,
            optional: false,
            default: None,
            range: None,
            placeholder: None,
        }
    }

    /// 缺失时不报错（其后的参数也必须可选）
    pub fn optional(mut self) -> ArgSpec {
        self.optional = true;
        self
    }

    /// 缺省值（文本形式，仍会按类型转换）
    pub fn with_default(mut self, value: impl Into<String>) -> ArgSpec {
        self.default = Some(value.into());
        self.optional = true;
        self
    }

    /// 数值范围（含端点）
    pub fn range(mut self, min: i64, max: i64) -> ArgSpec {
        self.range = Some((min, max));
        self
    }

    /// 用法里的占位符，如 `<分钟>` 里的「分钟」
    pub fn placeholder(mut self, value: impl Into<String>) -> ArgSpec {
        self.placeholder = Some(value.into());
        self
    }

    /// 该参数在用法串里的片段：`<群号>` / `[分钟]`
    pub fn usage_part(&self) -> String {
        let name = self
            .placeholder
            .clone()
            .unwrap_or_else(|| self.name.clone());
        match self.kind {
            ArgKind::Rest => format!("[{name}...]"),
            _ if self.optional => format!("[{name}]"),
            _ => format!("<{name}>"),
        }
    }

    /// 类型说明（错误提示里用）
    pub fn type_hint(&self) -> String {
        match self.kind {
            ArgKind::Choice(options) => format!("{} 之一", options.join("/")),
            ArgKind::Int => match self.range {
                Some((min, max)) => format!("{min}~{max} 内的整数"),
                None => "整数".to_string(),
            },
            ArgKind::Bool => "是/否".to_string(),
            ArgKind::Text => "文本".to_string(),
            ArgKind::Rest => "文本".to_string(),
        }
    }

    /// 转换一个词；失败给出面向用户的说明
    pub fn convert(&self, token: &str) -> Result<ArgValue, ArgError> {
        let invalid = |reason: String| ArgError::Invalid {
            name: self.name.clone(),
            got: token.to_string(),
            reason,
        };
        match self.kind {
            ArgKind::Text | ArgKind::Rest => Ok(ArgValue::Text(token.to_string())),
            ArgKind::Int => match token.trim().parse::<i64>() {
                Ok(value) => {
                    if let Some((min, max)) = self.range {
                        if value < min || value > max {
                            return Err(invalid(format!("应在 {min}~{max} 之间")));
                        }
                    }
                    Ok(ArgValue::Int(value))
                }
                Err(_) => Err(invalid(format!("需要{}", self.type_hint()))),
            },
            ArgKind::Bool => match token.trim().to_lowercase().as_str() {
                "1" | "true" | "yes" | "on" | "是" | "开" | "启用" => Ok(ArgValue::Bool(true)),
                "0" | "false" | "no" | "off" | "否" | "关" | "停用" => {
                    Ok(ArgValue::Bool(false))
                }
                _ => Err(invalid(format!("需要{}", self.type_hint()))),
            },
            ArgKind::Choice(options) => options
                .iter()
                .find(|option| option.eq_ignore_ascii_case(token.trim()))
                .map(|option| Ok(ArgValue::Text((*option).to_string())))
                .unwrap_or_else(|| Err(invalid(format!("需要{}", self.type_hint())))),
        }
    }
}

/// 参数声明的构造入口（`arg::text(..)` / `arg::i64(..)` / …）
pub mod arg {
    use super::{ArgKind, ArgSpec};

    /// 一个非空白词
    pub fn text(name: &str) -> ArgSpec {
        ArgSpec::new(name, ArgKind::Text)
    }

    /// 吃掉剩余全部文本，通常作为最后一个参数
    pub fn rest(name: &str) -> ArgSpec {
        ArgSpec::new(name, ArgKind::Rest)
    }

    /// 整数
    pub fn i64(name: &str) -> ArgSpec {
        ArgSpec::new(name, ArgKind::Int)
    }

    /// 布尔（是/否、true/false、on/off、开/关、1/0）
    pub fn bool(name: &str) -> ArgSpec {
        ArgSpec::new(name, ArgKind::Bool)
    }

    /// 枚举取值
    pub fn choice(name: &str, options: &'static [&'static str]) -> ArgSpec {
        ArgSpec::new(name, ArgKind::Choice(options))
    }
}

/// 解析失败的原因：供框架统一回显用法
#[derive(Clone, Debug, PartialEq)]
pub enum ArgError {
    /// 少了必填参数
    Missing(String),
    /// 类型/范围不对
    Invalid {
        name: String,
        got: String,
        reason: String,
    },
    /// 多出来的参数
    TooMany(usize),
}

impl ArgError {
    /// 面向用户的一行说明（不含用法）
    pub fn describe(&self) -> String {
        match self {
            ArgError::Missing(name) => format!("缺少参数 <{name}>"),
            ArgError::Invalid { name, got, reason } => {
                format!("参数 <{name}>「{got}」不合法：{reason}")
            }
            ArgError::TooMany(extra) => format!("多了 {extra} 个参数"),
        }
    }
}

/// 按名字或按下标取值，[`Args::raw`] 始终是原始切分
#[derive(Clone, Debug, Default)]
pub struct Args {
    entries: Vec<(String, ArgValue)>,
    raw: Vec<String>,
}

impl Args {
    pub(crate) fn new(entries: Vec<(String, ArgValue)>, raw: Vec<String>) -> Args {
        Args { entries, raw }
    }

    /// 没有声明参数时的原始词表（含命令名之后的全部 token）
    pub fn raw(&self) -> &[String] {
        &self.raw
    }

    fn value(&self, name: &str) -> Option<&ArgValue> {
        self.entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    /// 纯文本取值：数字/布尔参数要用 [`Args::i64`] / [`Args::bool`] / [`Args::display`]
    pub fn text(&self, name: &str) -> Option<&str> {
        self.value(name).and_then(ArgValue::as_str)
    }

    /// 任意类型参数的文本形式
    pub fn display(&self, name: &str) -> Option<String> {
        self.value(name).map(ArgValue::display)
    }

    pub fn i64(&self, name: &str) -> Option<i64> {
        match self.value(name) {
            Some(ArgValue::Int(value)) => Some(*value),
            Some(ArgValue::Text(value)) => value.parse().ok(),
            Some(ArgValue::Bool(value)) => Some(i64::from(*value)),
            None => None,
        }
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.value(name) {
            Some(ArgValue::Bool(value)) => Some(*value),
            Some(ArgValue::Int(value)) => Some(*value != 0),
            Some(ArgValue::Text(value)) => match value.as_str() {
                "true" | "1" | "是" | "开" => Some(true),
                "false" | "0" | "否" | "关" => Some(false),
                _ => None,
            },
            None => None,
        }
    }

    /// 按下标取（声明了同名参数时也能按位置拿）
    pub fn at(&self, index: usize) -> Option<&ArgValue> {
        self.entries.get(index).map(|(_, value)| value)
    }

    /// 已解析出来的参数名，顺序与声明一致
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(name, _)| name.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 按声明解析参数。`tokens` 是命令名之后的全部词。
pub fn parse(specs: &[ArgSpec], tokens: &[String]) -> Result<Args, ArgError> {
    let mut entries: Vec<(String, ArgValue)> = Vec::new();
    let mut cursor = 0usize;
    for spec in specs {
        if matches!(spec.kind, ArgKind::Rest) {
            let tail = tokens[cursor..].join(" ");
            cursor = tokens.len();
            if tail.trim().is_empty() {
                match &spec.default {
                    Some(default) => entries.push((spec.name.clone(), spec.convert(default)?)),
                    None if spec.optional => {}
                    None => return Err(ArgError::Missing(spec.name.clone())),
                }
                continue;
            }
            entries.push((spec.name.clone(), spec.convert(tail.trim())?));
            continue;
        }
        let Some(token) = tokens.get(cursor) else {
            match &spec.default {
                Some(default) => {
                    entries.push((spec.name.clone(), spec.convert(default)?));
                    continue;
                }
                None if spec.optional => continue,
                None => return Err(ArgError::Missing(spec.name.clone())),
            }
        };
        cursor += 1;
        entries.push((spec.name.clone(), spec.convert(token)?));
    }
    if cursor < tokens.len() {
        return Err(ArgError::TooMany(tokens.len() - cursor));
    }
    Ok(Args::new(entries, tokens.to_vec()))
}

/// 由声明拼出用法串：`/ban <群号> [分钟] [原因...]`
pub fn usage(names: &[String], specs: &[ArgSpec]) -> String {
    let head = names
        .first()
        .cloned()
        .unwrap_or_else(|| "/命令".to_string());
    if specs.is_empty() {
        return head;
    }
    let parts: Vec<String> = specs.iter().map(ArgSpec::usage_part).collect();
    format!("{head} {}", parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn 必填缺失与用法回显() {
        let specs = vec![arg::i64("群号"), arg::rest("原因").optional()];
        assert_eq!(
            usage(&["/ban".to_string()], &specs),
            "/ban <群号> [原因...]"
        );
        let error = parse(&specs, &[]).unwrap_err();
        assert_eq!(error, ArgError::Missing("群号".to_string()));
        assert_eq!(error.describe(), "缺少参数 <群号>");
    }

    #[test]
    fn 类型转换与缺省值() {
        let specs = vec![
            arg::i64("群号"),
            arg::i64("分钟").with_default("10").range(1, 1440),
            arg::bool("静音").with_default("否"),
            arg::choice("单位", &["分钟", "小时"]).optional(),
            arg::rest("原因").optional(),
        ];
        let args = parse(&specs, &words("123 30 是 小时 骚扰新人")).unwrap();
        assert_eq!(args.i64("群号"), Some(123));
        assert_eq!(args.i64("分钟"), Some(30));
        assert_eq!(args.bool("静音"), Some(true));
        assert_eq!(args.text("单位"), Some("小时"));
        assert_eq!(args.text("原因"), Some("骚扰新人"));
        assert_eq!(args.display("群号").as_deref(), Some("123"));
    }

    #[test]
    fn 缺省值参与类型校验() {
        let specs = vec![arg::i64("分钟").with_default("abc")];
        let error = parse(&specs, &[]).unwrap_err();
        assert!(matches!(error, ArgError::Invalid { .. }));
    }

    #[test]
    fn 范围与非法值() {
        let specs = vec![arg::i64("分钟").range(1, 1440)];
        let error = parse(&specs, &words("99999")).unwrap_err();
        assert_eq!(
            error.describe(),
            "参数 <分钟>「99999」不合法：应在 1~1440 之间"
        );
        assert!(matches!(
            parse(&specs, &words("x")).unwrap_err(),
            ArgError::Invalid { .. }
        ));
    }

    #[test]
    fn 多余参数报错() {
        let specs = vec![arg::text("名字")];
        assert_eq!(
            parse(&specs, &words("a b c")).unwrap_err(),
            ArgError::TooMany(2)
        );
    }

    #[test]
    fn 枚举大小写不敏感并回填候选写法() {
        let specs = vec![arg::choice("模式", &["Fast", "Slow"])];
        let args = parse(&specs, &words("fast")).unwrap();
        assert_eq!(args.text("模式"), Some("Fast"));
        assert!(parse(&specs, &words("turbo")).is_err());
    }

    #[test]
    fn rest吃掉剩余文本() {
        let specs = vec![arg::text("命令"), arg::rest("全部").optional()];
        let args = parse(&specs, &words("look 你好 世界")).unwrap();
        assert_eq!(args.text("命令"), Some("look"));
        assert_eq!(args.text("全部"), Some("你好 世界"));
        assert_eq!(args.raw().len(), 3);
    }
}
