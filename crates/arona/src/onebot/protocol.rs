//! OneBot 协议编解码（对应原版 OneBotProtocol）
use crate::runtime::message::{ForwardMessage, MessageSegment, OutgoingMessage};
use serde_json::{Map, Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

use super::model::{OneBotAction, OneBotActionResponse, OneBotEvent, ParsedPayload};

pub fn new_echo() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn action(name: &str, params: Value) -> OneBotAction {
    OneBotAction {
        action: name.to_string(),
        params,
        echo: new_echo(),
    }
}

pub fn serialize_action(action: &OneBotAction) -> String {
    json!({
        "action": action.action,
        "params": action.params,
        "echo": action.echo,
    })
    .to_string()
}

/// OutgoingMessage -> OneBot message 数组
pub fn message_to_json(message: &OutgoingMessage) -> Value {
    let mut segments = Vec::new();
    for segment in &message.segments {
        match segment {
            MessageSegment::Text(text) => {
                segments.push(json!({ "type": "text", "data": { "text": text } }));
            }
            MessageSegment::At(user_id) => {
                segments.push(json!({ "type": "at", "data": { "qq": user_id } }));
            }
            MessageSegment::Forward { title, .. } => {
                // send_group_msg/send_private_msg 不支持 forward 段，用文本占位
                segments.push(json!({
                    "type": "text",
                    "data": { "text": format!("[合并转发:{title}]") }
                }));
            }
            MessageSegment::Image { url, file, data } => {
                let mut value = String::new();
                if let Some(url) = url {
                    value = url.clone();
                } else if let Some(data) = data {
                    use base64::Engine;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
                    value = format!("base64://{encoded}");
                } else if let Some(file) = file {
                    // 同机部署时可配置直传 file:// 路径：免去大图 base64，实现端按原始文件上传不压缩
                    if crate::runtime::config::send_image_as_file()
                        && std::path::Path::new(file).is_file()
                    {
                        value = local_file_uri(file);
                    } else if let Ok(bytes) = std::fs::read(file) {
                        use base64::Engine;
                        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                        value = format!("base64://{encoded}");
                    } else {
                        value = file.clone();
                    }
                }
                segments.push(json!({ "type": "image", "data": { "file": value } }));
            }
        }
    }
    Value::Array(segments)
}

/// 本地路径转 file:// URI：`\` 归一为 `/`，非安全字节按 UTF-8 百分号编码（含中文路径）。
/// 盘符冒号保持原样（file:///C:/... 为 RFC 8089 标准形式，实现端兼容性最好）
fn local_file_uri(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    // 绝对路径自带前导 /（Linux），盘符路径(C:/)则补第三条斜杠
    let mut uri = if normalized.starts_with('/') {
        String::from("file://")
    } else {
        String::from("file:///")
    };
    for (index, part) in normalized.split('/').enumerate() {
        if index > 0 {
            uri.push('/');
        }
        for byte in part.as_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                    uri.push(*byte as char)
                }
                other => uri.push_str(&format!("%{other:02X}")),
            }
        }
    }
    uri
}

pub fn format_face(id: Option<&str>) -> String {
    match id {
        Some(id) if !id.trim().is_empty() => format!("[表情 [{id}]]"),
        _ => "[表情]".to_string(),
    }
}

pub fn format_image(image: &MessageSegment) -> String {
    match image {
        MessageSegment::Image { url: Some(url), .. } => format!("[arona:image,url={url}]"),
        MessageSegment::Image {
            file: Some(file), ..
        } => format!("[arona:image,file={file}]"),
        _ => "[arona:image]".to_string(),
    }
}

/// 提取事件的纯文本内容
pub fn extract_text(event: &OneBotEvent) -> String {
    extract_segments(event)
        .iter()
        .map(|segment| match segment {
            MessageSegment::Text(text) => text.clone(),
            MessageSegment::At(user_id) => format!("@{user_id}"),
            MessageSegment::Forward { title, .. } => format!("[合并转发:{title}]"),
            MessageSegment::Image { .. } => format_image(segment),
        })
        .collect()
}

/// 从 event.message（数组段）或 raw_message（CQ 码）提取消息段
pub fn extract_segments(event: &OneBotEvent) -> Vec<MessageSegment> {
    if let Some(Value::Array(segments)) = &event.message {
        let mut out = Vec::new();
        for element in segments {
            if let Some(segment) = parse_segment(element) {
                out.push(segment);
            }
        }
        return out;
    }
    match &event.raw_message {
        Some(raw) => decode_cq_message(raw),
        None => Vec::new(),
    }
}

fn parse_segment(element: &Value) -> Option<MessageSegment> {
    let obj = element.as_object()?;
    let segment_type = obj.get("type")?.as_str()?;
    let data = obj
        .get("data")
        .and_then(|d| d.as_object())
        .cloned()
        .unwrap_or_default();
    match segment_type {
        "text" => Some(MessageSegment::Text(
            data.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        )),
        "at" => parse_at_value(data.get("qq")),
        "image" => Some(MessageSegment::Image {
            url: data
                .get("url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            file: data
                .get("file")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            data: None,
        }),
        "face" => Some(MessageSegment::Text(format_face(
            data.get("id").and_then(|v| v.as_str()),
        ))),
        _ => None,
    }
}

fn parse_at_value(value: Option<&Value>) -> Option<MessageSegment> {
    let raw = match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return None,
    };
    if raw == "all" {
        Some(MessageSegment::Text("@全体成员".to_string()))
    } else {
        raw.parse::<i64>().ok().map(MessageSegment::At)
    }
}

/// 解析 CQ 码消息（text/at/image/face，未知类型按原文保留）
pub fn decode_cq_message(raw: &str) -> Vec<MessageSegment> {
    let mut result = Vec::new();
    let mut index = 0;
    let bytes = raw.as_bytes();
    while index < bytes.len() {
        // 查找 [CQ:
        let start = raw[index..].find("[CQ:").map(|p| p + index);
        let Some(start) = start else { break };
        if start > index {
            result.push(MessageSegment::Text(raw[index..start].to_string()));
        }
        let after = &raw[start + 4..];
        let Some(end) = after.find(']') else {
            result.push(MessageSegment::Text(raw[start..].to_string()));
            index = raw.len();
            break;
        };
        let inner = &after[..end];
        index = start + 4 + end + 1;
        let (cq_type, params) = match inner.split_once(':') {
            Some((t, p)) => (t, p),
            None => (inner, ""),
        };
        let mut map = Map::new();
        for part in params.split(',') {
            if let Some((k, v)) = part.split_once('=') {
                map.insert(k.to_string(), Value::String(v.to_string()));
            }
        }
        match cq_type {
            "at" => {
                let segment = map.get("qq").map(|v| v.as_str().unwrap_or("").to_string());
                match segment.as_deref() {
                    Some("all") => result.push(MessageSegment::Text("@全体成员".to_string())),
                    Some(qq) => {
                        if let Ok(qq) = qq.parse::<i64>() {
                            result.push(MessageSegment::At(qq));
                        }
                    }
                    None => {}
                }
            }
            "image" => result.push(MessageSegment::Image {
                url: map.get("url").map(|v| v.as_str().unwrap_or("").to_string()),
                file: map
                    .get("file")
                    .map(|v| v.as_str().unwrap_or("").to_string()),
                data: None,
            }),
            "face" => result.push(MessageSegment::Text(format_face(
                map.get("id").and_then(|v| v.as_str()),
            ))),
            _ => result.push(MessageSegment::Text(format!("[CQ:{inner}]"))),
        }
    }
    if index < raw.len() {
        result.push(MessageSegment::Text(raw[index..].to_string()));
    }
    result
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 解析收到的字符串 payload
pub fn parse_payload(payload: &str) -> Option<ParsedPayload> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let obj = value.as_object()?;
    if obj.contains_key("post_type") {
        return Some(ParsedPayload::Event(parse_event(&value)));
    }
    if obj.contains_key("action") && obj.contains_key("echo") {
        return Some(ParsedPayload::Action(parse_action(&value)));
    }
    if obj.contains_key("echo") {
        return Some(ParsedPayload::Response(parse_response(&value)));
    }
    None
}

fn get_str(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn get_i64(obj: &Map<String, Value>, key: &str) -> Option<i64> {
    match obj.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse::<i64>().ok(),
        _ => None,
    }
}

fn parse_event(value: &Value) -> OneBotEvent {
    let obj = value.as_object().cloned().unwrap_or_default();
    OneBotEvent {
        time: get_i64(&obj, "time").unwrap_or_else(now_secs),
        self_id: get_i64(&obj, "self_id").unwrap_or(0),
        post_type: get_str(&obj, "post_type").unwrap_or_default(),
        notice_type: get_str(&obj, "notice_type"),
        message_type: get_str(&obj, "message_type"),
        sub_type: get_str(&obj, "sub_type"),
        message_id: get_i64(&obj, "message_id"),
        user_id: get_i64(&obj, "user_id"),
        operator_id: get_i64(&obj, "operator_id"),
        group_id: get_i64(&obj, "group_id"),
        raw_message: get_str(&obj, "raw_message"),
        message: obj.get("message").cloned(),
        sender: obj.get("sender").cloned(),
        raw: value.clone(),
    }
}

fn parse_action(value: &Value) -> OneBotAction {
    let obj = value.as_object().cloned().unwrap_or_default();
    OneBotAction {
        action: get_str(&obj, "action").unwrap_or_default(),
        params: obj.get("params").cloned().unwrap_or_else(|| json!({})),
        echo: get_str(&obj, "echo").unwrap_or_default(),
    }
}

fn parse_response(value: &Value) -> OneBotActionResponse {
    let obj = value.as_object().cloned().unwrap_or_default();
    OneBotActionResponse {
        status: get_str(&obj, "status").unwrap_or_default(),
        retcode: get_i64(&obj, "retcode").unwrap_or(-1),
        data: obj.get("data").cloned(),
        message: get_str(&obj, "message"),
        wording: get_str(&obj, "wording"),
        echo: get_str(&obj, "echo"),
        raw: value.clone(),
    }
}

/// 合并转发节点 JSON（send_forward_msg 使用）
pub fn forward_messages_to_json(messages: &[ForwardMessage]) -> Value {
    let nodes: Vec<Value> = messages
        .iter()
        .map(|node| {
            let message = OutgoingMessage {
                segments: node.content.clone(),
                revoke_after_millis: None,
            };
            json!({
                "type": "node",
                "data": {
                    "name": node.name,
                    "uin": node.uin,
                    "content": message_to_json(&message),
                }
            })
        })
        .collect();
    Value::Array(nodes)
}

#[cfg(test)]
mod tests {
    use super::local_file_uri;

    #[test]
    fn local_file_uri_encodes_windows_path() {
        assert_eq!(
            local_file_uri("C:\\Users\\cheng\\images\\a b.png"),
            "file:///C:/Users/cheng/images/a%20b.png"
        );
    }

    #[test]
    fn local_file_uri_encodes_non_ascii() {
        assert_eq!(
            local_file_uri("D:\\arona\\images\\gacha-pool\\日服\\2980.webp"),
            "file:///D:/arona/images/gacha-pool/%E6%97%A5%E6%9C%8D/2980.webp"
        );
    }

    #[test]
    fn local_file_uri_keeps_plain_unix_path() {
        assert_eq!(
            local_file_uri("/home/arona/images/1.png"),
            "file:///home/arona/images/1.png"
        );
    }
}
