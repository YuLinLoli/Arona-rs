//! 控制台输出（对应原版 OneBotConsole / ConsoleEmoji）

use crate::onebot::model::OneBotEvent;
use crate::onebot::protocol;
use crate::runtime::console as color;
use crate::runtime::message::{MessageSegment, MessageTarget, OutgoingMessage};
use chrono::Local;

const BANNER: [&str; 6] = [
    " █████╗ ██████╗ ██████╗ ███╗   ██╗ █████╗ ",
    " ██╔══██╗██╔══██╗██╔═══██╗████╗  ██║██╔══██╗",
    " ███████║██████╔╝██║   ██║██╔██╗ ██║███████║",
    " ██╔══██║██╔══██╗██║   ██║██║╚██╗██║██╔══██║",
    " ██║  ██║██║  ██║╚██████╔╝██║ ╚████║██║  ██║",
    " ╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝ ╚═╝  ╚═══╝╚═╝  ╚═╝",
];

pub fn print_banner(version: &str) {
    // 原版 OneBotConsole.printBanner: 启动横幅统一黄色(33)
    // 整批一次性输出：横幅在 GUI 主线程与后台机器人线程之间不会被日志行切开而错位；
    // 同时每行仍单独进入 GUI「实时日志」，等宽艺术字在日志选项卡里也不会被折成一行
    let last_index = BANNER.len() - 1;
    let lines: Vec<String> = BANNER
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if index == last_index {
                format!("{line}     v{version}")
            } else {
                (*line).to_string()
            }
        })
        .collect();
    color::print_colored_lines(color::Color::Yellow, &lines);
}

pub fn emoji_supported() -> bool {
    // 独立模式控制台默认支持 emoji；可用环境变量 ARONA_CONSOLE_EMOJI=0 关闭降级
    match std::env::var("ARONA_CONSOLE_EMOJI") {
        Ok(value) => matches!(value.to_lowercase().as_str(), "1" | "true" | "on"),
        Err(_) => true,
    }
}

/// 不支持 emoji 时把文本中的 emoji 替换为 [emoji:码点]
pub fn sanitize(text: &str) -> String {
    if emoji_supported() || text.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let next = chars.peek().copied();
        let cp = ch as u32;
        let is_emoji = match cp {
            0x1F000..=0x1FAFF => true,
            0x2600..=0x27BF | 0x2B00..=0x2BFF => {
                next == Some('\u{fe0f}')
                    || matches!(
                        cp,
                        0x2705
                            | 0x2728
                            | 0x274C
                            | 0x274E
                            | 0x2753
                            | 0x2754
                            | 0x2755
                            | 0x2757
                            | 0x2764
                            | 0x2795
                            | 0x2796
                            | 0x2797
                            | 0x2B50
                            | 0x2B55
                    )
            }
            _ => false,
        };
        if cp == 0xFE0F || cp == 0x200D || cp == 0x20E3 {
            continue;
        }
        if is_emoji {
            out.push_str(&format!("[emoji:{cp:X}]"));
        } else {
            out.push(ch);
        }
    }
    out
}

fn sender_name(event: &OneBotEvent) -> String {
    let card = event
        .sender
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("card"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    let nickname = event
        .sender
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("nickname"))
        .and_then(|v| v.as_str());
    if let Some(card) = card {
        card.to_string()
    } else if let Some(nickname) = nickname {
        nickname.to_string()
    } else if let Some(user_id) = event.user_id {
        user_id.to_string()
    } else {
        "未知".to_string()
    }
}

pub fn format_message(self_id: i64, event: &OneBotEvent, group_name: Option<&str>) -> String {
    format_message_text(self_id, event, group_name, &protocol::extract_text(event))
}

/// 已经解析过消息段的调用方（事件入口）走这条：文本现成，不必为打印再解一遍段
pub fn format_message_text(
    self_id: i64,
    event: &OneBotEvent,
    group_name: Option<&str>,
    text: &str,
) -> String {
    let sender = format!(
        "{}({})",
        sender_name(event),
        event
            .user_id
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string())
    );
    let line = if event.message_type.as_deref() == Some("group") && event.group_id.is_some() {
        let group_id = event.group_id.unwrap();
        let group = group_name
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| group_id.to_string());
        format!("V/Bot.{self_id}: [{group}({group_id})] {sender} -> {text}")
    } else {
        format!("V/Bot.{self_id}: {sender} -> {text}")
    };
    sanitize(&line)
}

pub fn print_message(self_id: i64, event: &OneBotEvent, group_name: Option<&str>) {
    print_message_text(self_id, event, group_name, &protocol::extract_text(event));
}

/// 同 [`print_message`]，但文本由调用方提供（同一条事件只解一次消息段）
pub fn print_message_text(self_id: i64, event: &OneBotEvent, group_name: Option<&str>, text: &str) {
    color::print_rule_line(&format_message_text(self_id, event, group_name, text));
}

fn format_segment(segment: &MessageSegment) -> String {
    match segment {
        MessageSegment::Text(text) => text.clone(),
        MessageSegment::At(user_id) => format!("[at:qq={user_id}]"),
        MessageSegment::AtAll => "[at:qq=all]".to_string(),
        MessageSegment::Reply(message_id) => format!("[reply:id={message_id}]"),
        MessageSegment::Face(id) => format!("[face:id={id}]"),
        MessageSegment::Record { url, file } => format!("[record:{}]", media_hint(url, file)),
        MessageSegment::Video { url, file } => format!("[video:{}]", media_hint(url, file)),
        MessageSegment::File { name, size, .. } => format!("[file:{name}({size})]"),
        MessageSegment::Poke { name, target } => format!("[poke:{name}(qq={target})]"),
        MessageSegment::Location { name, .. } => format!("[location:{name}]"),
        MessageSegment::Json(_) => "[json]".to_string(),
        MessageSegment::Xml(_) => "[xml]".to_string(),
        MessageSegment::Forward { title, id, .. } if !id.is_empty() => {
            format!("[合并转发:{title}(id={id})]")
        }
        MessageSegment::Forward { title, .. } => format!("[合并转发:{title}]"),
        MessageSegment::Raw { kind, .. } => format!("[{kind}]"),
        MessageSegment::Image { url, file, data } => {
            if data.is_some() {
                "[arona:image,url=base64://...]".to_string()
            } else {
                match media_hint(url, file) {
                    hint if hint.is_empty() => "[arona:image]".to_string(),
                    hint => format!("[arona:image,{hint}]"),
                }
            }
        }
    }
}

/// 控制台里媒体段的取值提示：URL 优先，其次文件路径
fn media_hint(url: &Option<String>, file: &Option<String>) -> String {
    match (url, file) {
        (Some(url), _) => format!("url={url}"),
        (_, Some(file)) => format!("file={file}"),
        _ => String::new(),
    }
}

pub fn print_outgoing(self_id: i64, target: MessageTarget, message: &OutgoingMessage) {
    let address = match target {
        MessageTarget::Group(id) => format!("Group({id})"),
        MessageTarget::Private(id) => format!("Friend({id})"),
    };
    let content: String = message.segments.iter().map(format_segment).collect();
    let line = format!(
        "{} V/Bot.{self_id}: {address} <- {}",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        sanitize(&content)
    );
    // 原版 OneBotConsole.printOutgoing: 自身发送的消息统一黄色(33)
    color::print_colored_line(color::Color::Yellow, &line);
}

pub fn print_notice(self_id: i64, text: &str) {
    color::print_rule_line(&format!(
        "{} V/Bot.{self_id}: {text}",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
}
