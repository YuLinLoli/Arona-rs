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
        segments.push(segment_to_json(segment));
    }
    Value::Array(segments)
}

/// 单个消息段 -> OneBot segment。
/// 合并转发段在 `send_group_msg`/`send_private_msg` 里不被支持，发送器会另走
/// `send_forward_msg`（见 `onebot::message_sender`），这里只留文本占位兜底。
pub fn segment_to_json(segment: &MessageSegment) -> Value {
    match segment {
        MessageSegment::Text(text) => json!({ "type": "text", "data": { "text": text } }),
        MessageSegment::At(user_id) => json!({ "type": "at", "data": { "qq": user_id } }),
        MessageSegment::AtAll => json!({ "type": "at", "data": { "qq": "all" } }),
        MessageSegment::Reply(message_id) => {
            json!({ "type": "reply", "data": { "id": message_id } })
        }
        MessageSegment::Face(id) => match id.parse::<i64>() {
            Ok(number) => json!({ "type": "face", "data": { "id": number } }),
            Err(_) => json!({ "type": "face", "data": { "id": id } }),
        },
        MessageSegment::Record { url, file } => {
            json!({ "type": "record", "data": { "file": media_value(url, file, None) } })
        }
        MessageSegment::Video { url, file } => {
            json!({ "type": "video", "data": { "file": media_value(url, file, None) } })
        }
        MessageSegment::File {
            file_id,
            name,
            size,
        } => json!({
            "type": "file",
            "data": { "file_id": file_id, "file_name": name, "file_size": size },
        }),
        MessageSegment::Poke { name, target } => {
            json!({ "type": "poke", "data": { "type": name, "target_id": target } })
        }
        MessageSegment::Location {
            name,
            address,
            latitude,
            longitude,
        } => json!({
            "type": "location",
            "data": { "name": name, "address": address, "lat": latitude, "lon": longitude },
        }),
        MessageSegment::Json(card) => json!({ "type": "json", "data": { "data": card } }),
        MessageSegment::Xml(card) => json!({ "type": "xml", "data": { "data": card } }),
        // 只有凭据、没有节点的转发段：原样回一个 forward 段，实现端按 id 转发那份记录
        // （发送器要发节点内容时会另走 send_forward_msg，见 onebot::message_sender）
        MessageSegment::Forward {
            id,
            messages,
            title: _,
        } if messages.is_empty() && !id.is_empty() => {
            json!({ "type": "forward", "data": { "id": id } })
        }
        MessageSegment::Forward { title, .. } => json!({
            "type": "text",
            "data": { "text": format!("[合并转发:{title}]") }
        }),
        // 不认识的段原样拼回去：实现端加的新类型不该因为框架落后一版就消失
        MessageSegment::Raw { kind, data } => json!({ "type": kind, "data": data }),
        MessageSegment::Image { url, file, data } => json!({
            "type": "image",
            "data": { "file": media_value(url, file, data.as_deref()) },
        }),
    }
}

/// 图片/语音/视频的统一取值：URL > 内存字节(base64) > 本地文件
fn media_value(url: &Option<String>, file: &Option<String>, data: Option<&[u8]>) -> String {
    use base64::Engine;
    if let Some(url) = url {
        return url.clone();
    }
    if let Some(data) = data {
        return format!(
            "base64://{}",
            base64::engine::general_purpose::STANDARD.encode(data)
        );
    }
    let Some(file) = file else {
        return String::new();
    };
    // 同机部署时可配置直传 file:// 路径：免去大图 base64，实现端按原始文件上传不压缩
    if crate::runtime::config::send_image_as_file() && std::path::Path::new(file).is_file() {
        return local_file_uri(file);
    }
    match std::fs::read(file) {
        Ok(bytes) => format!(
            "base64://{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
        Err(_) => file.clone(),
    }
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

/// 消息段的展示文本（日志、控制台与 `EventContext::text` 用）
pub fn segment_display(segment: &MessageSegment) -> String {
    match segment {
        MessageSegment::Text(text) => text.clone(),
        MessageSegment::At(user_id) => format!("@{user_id}"),
        MessageSegment::AtAll => "@全体成员".to_string(),
        MessageSegment::Reply(message_id) => format!("[回复 {message_id}]"),
        MessageSegment::Face(id) => format_face(Some(id)),
        MessageSegment::Record { .. } => "[语音]".to_string(),
        MessageSegment::Video { .. } => "[视频]".to_string(),
        MessageSegment::File { name, .. } => format!("[文件 {name}]"),
        MessageSegment::Poke { name, .. } => format!("[戳一戳 {name}]"),
        MessageSegment::Location { name, .. } => format!("[位置 {name}]"),
        MessageSegment::Json(_) => "[json 卡片]".to_string(),
        MessageSegment::Xml(_) => "[xml 卡片]".to_string(),
        MessageSegment::Forward { title, .. } => format!("[合并转发:{title}]"),
        MessageSegment::Image { .. } => format_image(segment),
        MessageSegment::Raw { kind, .. } => format!("[{kind}]"),
    }
}

/// 命令文本里非文本段的**紧凑占位**：`segment_display` 的观感串（图片带整条 URL、
/// 文件带中文名）不该混进首词，命令表查的是 `/单抽`，不是 `/[arona:image,url=https://…]`。
fn command_token(segment: &MessageSegment) -> String {
    match segment {
        MessageSegment::Text(text) => text.clone(),
        MessageSegment::At(user_id) => format!("@{user_id}"),
        MessageSegment::AtAll => "@all".to_string(),
        MessageSegment::Reply(_) => "[reply]".to_string(),
        MessageSegment::Image { .. } => "[image]".to_string(),
        MessageSegment::Face(id) => format!("[face:{id}]"),
        MessageSegment::Record { .. } => "[record]".to_string(),
        MessageSegment::Video { .. } => "[video]".to_string(),
        MessageSegment::File { .. } => "[file]".to_string(),
        MessageSegment::Poke { .. } => "[poke]".to_string(),
        MessageSegment::Location { .. } => "[location]".to_string(),
        MessageSegment::Json(_) => "[json]".to_string(),
        MessageSegment::Xml(_) => "[xml]".to_string(),
        MessageSegment::Forward { .. } => "[forward]".to_string(),
        MessageSegment::Raw { kind, .. } => format!("[{kind}]"),
    }
}

/// 提取事件的纯文本内容（各段直接相连，与实现端的 raw_message 观感一致）
pub fn extract_text(event: &OneBotEvent) -> String {
    text_of(&extract_segments(event))
}

/// 从**已解析**的消息段拼文本：同一条消息被反复问到时不必再解一遍段
pub fn text_of(segments: &[MessageSegment]) -> String {
    segments.iter().map(segment_display).collect()
}

/// 提取**用来匹配命令**的文本。
///
/// 群里 "@机器人 /单抽" 的消息段是 `[At(机器人), Text(" /单抽")]`：直接拼文本会让首词变成
/// `@<机器人号>/单抽`，命令表因此永远查不中（mirai-console 在进 `CommandManager` 之前就把
/// `At(bot)` 从消息链里剥掉了，这里对齐它）。规则：从头剥掉召唤机器人的段
/// （@机器人、@全体成员、引用、图片），段与段之间补空格，最后 trim。
pub fn command_text(event: &OneBotEvent, self_id: i64) -> String {
    command_text_of(&extract_segments(event), self_id)
}

/// 从已解析的消息段取命令文本（规则同 [`command_text`]）
pub fn command_text_of(segments: &[MessageSegment], self_id: i64) -> String {
    let mut out = String::new();
    let mut leading = true;
    for segment in segments {
        // 纯空白的前导文本段（部分实现端在 @机器人 前面塞一个 `" "`）也算"还没开始说话"，
        // 否则它把 leading 关掉，紧跟其后的 @机器人 就被当成正文混进首词，命令表再也查不中
        let blank_text = match segment {
            MessageSegment::Text(text) => text.trim().is_empty(),
            _ => false,
        };
        if leading
            && (blank_text
                || segment.is_mention_of(self_id)
                || matches!(
                    segment,
                    MessageSegment::Reply(_) | MessageSegment::Image { .. }
                ))
        {
            continue;
        }
        leading = false;
        let part = command_token(segment);
        if !out.is_empty() && !out.ends_with(' ') && !part.starts_with(' ') {
            out.push(' ');
        }
        out.push_str(&part);
    }
    out.trim().to_string()
}

/// 一条消息解析一次的结果。
///
/// 段解析是事件热路径上最贵的一步（每段都要拷一份 `data` 映射出来），而展示文本、命令文本、
/// 引用编号全都只是它的派生值，所以事件入口解一次、三份结果一路带到命令分发与钩子。
#[derive(Clone, Debug, Default)]
pub struct ParsedMessage {
    /// 原样消息段
    pub segments: Vec<MessageSegment>,
    /// 展示文本（各段直接相连）
    pub text: String,
    /// 剥掉召唤前缀、用来匹配命令的文本
    pub command_text: String,
}

impl ParsedMessage {
    /// 解析事件携带的消息（`message` 数组段优先，退回 `raw_message` 里的 CQ 码）
    pub fn parse(event: &OneBotEvent, self_id: i64) -> ParsedMessage {
        let segments = extract_segments(event);
        let text = text_of(&segments);
        let command_text = command_text_of(&segments, self_id);
        ParsedMessage {
            segments,
            text,
            command_text,
        }
    }

    /// 这条消息引用了哪条（reply/quote 段）；`text` 与 `command_text` 里都已剥掉它
    pub fn quoted(&self) -> Option<i64> {
        self.segments.iter().find_map(|segment| match segment {
            MessageSegment::Reply(message_id) => Some(*message_id),
            _ => None,
        })
    }
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
    let str_field = |key: &str| -> Option<String> {
        data.get(key).and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    };
    match segment_type {
        "text" => Some(MessageSegment::Text(
            data.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        )),
        "at" => parse_at_value(data.get("qq")),
        // 引用段：v11 规范叫 reply，NapCat / Lagrange 等新实现会写成 quote，都是同一段
        "reply" | "quote" => get_i64(&data, "id")
            .or_else(|| str_field("id").and_then(|v| v.parse().ok()))
            .map(MessageSegment::Reply),
        "image" => {
            let (url, file, data) = media_parts(str_field("url"), str_field("file"));
            Some(MessageSegment::Image { url, file, data })
        }
        "face" => Some(MessageSegment::Face(str_field("id").unwrap_or_default())),
        "record" | "voice" => Some(MessageSegment::Record {
            url: str_field("url"),
            file: str_field("file"),
        }),
        "video" => Some(MessageSegment::Video {
            url: str_field("url"),
            file: str_field("file"),
        }),
        "file" => Some(MessageSegment::File {
            file_id: str_field("file_id").unwrap_or_default(),
            name: str_field("file_name")
                .or_else(|| str_field("name"))
                .unwrap_or_default(),
            size: get_i64(&data, "file_size")
                .or_else(|| get_i64(&data, "size"))
                .unwrap_or_default() as u64,
        }),
        "poke" => Some(MessageSegment::Poke {
            name: str_field("type").unwrap_or_else(|| "poke".to_string()),
            target: get_i64(&data, "target_id")
                .or_else(|| get_i64(&data, "target"))
                .unwrap_or(0),
        }),
        "location" => Some(MessageSegment::Location {
            name: str_field("name").unwrap_or_default(),
            address: str_field("address").unwrap_or_default(),
            latitude: f64_field(&data, &["lat", "latitude"]),
            longitude: f64_field(&data, &["lon", "longitude"]),
        }),
        "json" => card_value(&data).map(MessageSegment::Json),
        "xml" => card_value(&data).map(MessageSegment::Xml),
        "forward" | "node" => {
            let content = data
                .get("content")
                .or_else(|| data.get("nodes"))
                .or_else(|| data.get("messages"))
                .and_then(|v| v.as_array())
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(parse_segment)
                        .collect::<Vec<MessageSegment>>()
                })
                .unwrap_or_default();
            // id/flag 是实现端给的转发凭据：NTQQ 系（NapCat / LLOWeb / Lagrange）常只给
            // id 不给 content，此时段里一个节点都没有。旧实现"没 content 就整段丢弃"，
            // 于是插件连"这条消息是转发、可以用 get_forward_msg 去取"都看不出来。
            let id = str_field("id")
                .or_else(|| str_field("flag"))
                .unwrap_or_default();
            if content.is_empty() && id.is_empty() {
                return None;
            }
            let messages = if content.is_empty() {
                Vec::new()
            } else {
                vec![ForwardMessage {
                    name: str_field("name").unwrap_or_default(),
                    uin: get_i64(&data, "uin").unwrap_or_default(),
                    content,
                }]
            };
            Some(MessageSegment::Forward {
                title: str_field("title")
                    .or_else(|| str_field("uni"))
                    .unwrap_or_else(|| "转发消息".to_string()),
                id,
                messages,
            })
        }
        // 框架还不认识的段（实现端新版本加的类型）原样留着：整段丢掉的话，
        // 转发/重发时它就从消息里消失了，插件也看不到"这里还有个东西"
        _ => Some(MessageSegment::Raw {
            kind: segment_type.to_string(),
            data: Value::Object(data.clone()),
        }),
    }
}

/// 媒体段的取值槽：实现端把字节直接塞进 `file`（少数塞进 `url`）时写成 `base64://…`，
/// 既不给本地路径也不给 URL。就地解成字节，免得插件拿到一长串 base64 文本还得自己拆；
/// 前缀对不上或 Base64 解不动就原样留在槽里，不猜。
fn media_parts(
    url: Option<String>,
    file: Option<String>,
) -> (Option<String>, Option<String>, Option<Vec<u8>>) {
    let mut url = url;
    let mut file = file;
    let data = take_base64(&mut file).or_else(|| take_base64(&mut url));
    (url, file, data)
}

fn take_base64(slot: &mut Option<String>) -> Option<Vec<u8>> {
    use base64::Engine;
    let payload = slot.as_deref()?.strip_prefix("base64://")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    *slot = None;
    Some(bytes)
}

/// json/xml 卡片：实现端可能给对象，也可能给一段 JSON 字符串
fn card_value(data: &Map<String, Value>) -> Option<Value> {
    let value = data.get("data")?;
    match value {
        Value::Object(_) | Value::Array(_) => Some(value.clone()),
        Value::String(text) => Some(serde_json::from_str(text).unwrap_or_else(|_| value.clone())),
        _ => None,
    }
}

/// 按候选键名取浮点字段（实现端对 lat/lon 有的给数字有的给字符串）
fn f64_field(data: &Map<String, Value>, keys: &[&str]) -> f64 {
    for key in keys {
        match data.get(*key) {
            Some(Value::Number(n)) => {
                if let Some(value) = n.as_f64() {
                    return value;
                }
            }
            Some(Value::String(s)) => {
                if let Ok(value) = s.parse::<f64>() {
                    return value;
                }
            }
            _ => {}
        }
    }
    0.0
}

fn parse_at_value(value: Option<&Value>) -> Option<MessageSegment> {
    let raw = match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return None,
    };
    if raw == "all" {
        Some(MessageSegment::AtAll)
    } else {
        raw.parse::<i64>().ok().map(MessageSegment::At)
    }
}

/// CQ 码内的转义序列还原：参数里的逗号/冒号/右括号必须以实体形式出现，
/// `&amp;` 必须最后替换，否则 `&amp;#44;` 会被二次解码成逗号。
fn unescape_cq(value: &str) -> String {
    value
        .replace("&#44;", ",")
        .replace("&#58;", ":")
        .replace("&#93;", "]")
        .replace("&amp;", "&")
}

/// 解析 CQ 码消息（已知类型还原成段，未知类型按原文保留）
pub fn decode_cq_message(raw: &str) -> Vec<MessageSegment> {
    let mut result = Vec::new();
    let mut index = 0;
    let bytes = raw.as_bytes();
    while index < bytes.len() {
        // 查找 [CQ:
        let start = raw[index..].find("[CQ:").map(|p| p + index);
        let Some(start) = start else { break };
        if start > index {
            result.push(MessageSegment::Text(unescape_cq(&raw[index..start])));
        }
        let after = &raw[start + 4..];
        let Some(end) = after.find(']') else {
            result.push(MessageSegment::Text(unescape_cq(&raw[start..])));
            index = raw.len();
            break;
        };
        let inner = &after[..end];
        index = start + 4 + end + 1;
        // 规范形式是 `[CQ:类型,键=值,键=值]`（冒号只出现在 `[CQ:` 处），
        // 少数实现写成 `[CQ:类型:键=值]`，两种分隔符都在第一个分隔符处切开
        let (cq_type, params) = match inner.split_once([',', ':']) {
            Some((t, p)) => (t, p),
            None => (inner, ""),
        };
        let mut map = Map::new();
        for part in params.split(',') {
            if let Some((k, v)) = part.split_once('=') {
                map.insert(k.to_string(), Value::String(v.to_string()));
            }
        }
        let text_field = |key: &str| -> Option<String> {
            map.get(key)
                .map(|value| unescape_cq(value.as_str().unwrap_or("")))
        };
        let segment = match cq_type {
            "at" => match text_field("qq").as_deref() {
                Some("all") => Some(MessageSegment::AtAll),
                Some(qq) => qq.parse::<i64>().ok().map(MessageSegment::At),
                None => None,
            },
            "image" => {
                let (url, file, data) = media_parts(text_field("url"), text_field("file"));
                Some(MessageSegment::Image { url, file, data })
            }
            "face" => Some(MessageSegment::Face(text_field("id").unwrap_or_default())),
            "reply" | "quote" => text_field("id")
                .and_then(|v| v.parse::<i64>().ok())
                .map(MessageSegment::Reply),
            "record" => Some(MessageSegment::Record {
                url: text_field("url"),
                file: text_field("file"),
            }),
            "video" => Some(MessageSegment::Video {
                url: text_field("url"),
                file: text_field("file"),
            }),
            "file" => Some(MessageSegment::File {
                file_id: text_field("file_id").unwrap_or_default(),
                name: text_field("file_name").unwrap_or_default(),
                size: text_field("file_size")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or_default(),
            }),
            "location" => Some(MessageSegment::Location {
                name: text_field("name").unwrap_or_default(),
                address: text_field("address").unwrap_or_default(),
                latitude: text_field("lat")
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or_default(),
                longitude: text_field("lon")
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or_default(),
            }),
            "json" | "xml" => {
                // 卡片正文在 CQ 码里是转义过的（逗号/冒号/右括号写成实体），不还原就先
                // 让 serde_json 解析失败，插件拿到的是带 &#44; 的坏 JSON 字符串而不是卡片
                let card = match map.get("data").and_then(|v| v.as_str()) {
                    Some(raw) => {
                        let fixed = unescape_cq(raw);
                        map.insert("data".to_string(), Value::String(fixed));
                        card_value(&map)
                    }
                    None => card_value(&map),
                };
                card.map(|card| {
                    if cq_type == "json" {
                        MessageSegment::Json(card)
                    } else {
                        MessageSegment::Xml(card)
                    }
                })
            }
            // `[CQ:forward,id=…]`：实现端只给凭据不给节点，解成带 id 的转发段，
            // 插件才看得出"这是条转发、可以用 get_forward_msg 去取"
            "forward" => Some(MessageSegment::Forward {
                title: text_field("brief")
                    .or_else(|| text_field("title"))
                    .unwrap_or_else(|| "转发消息".to_string()),
                id: text_field("id").unwrap_or_default(),
                messages: Vec::new(),
            }),
            _ => Some(MessageSegment::Text(format!("[CQ:{inner}]"))),
        };
        if let Some(segment) = segment {
            result.push(segment);
        }
    }
    if index < raw.len() {
        result.push(MessageSegment::Text(unescape_cq(&raw[index..])));
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
    use super::*;

    /// 走一遍真实入站路径：JSON payload -> OneBotEvent
    fn event_of(raw: Value) -> OneBotEvent {
        match parse_payload(&raw.to_string()) {
            Some(ParsedPayload::Event(event)) => event,
            other => panic!("应解析为事件，实际 {other:?}"),
        }
    }

    fn kind_of(segment: &MessageSegment) -> &'static str {
        match segment {
            MessageSegment::Text(_) => "text",
            MessageSegment::At(_) => "at",
            MessageSegment::AtAll => "at_all",
            MessageSegment::Reply(_) => "reply",
            MessageSegment::Image { .. } => "image",
            MessageSegment::Face(_) => "face",
            MessageSegment::Record { .. } => "record",
            MessageSegment::Video { .. } => "video",
            MessageSegment::File { .. } => "file",
            MessageSegment::Poke { .. } => "poke",
            MessageSegment::Location { .. } => "location",
            MessageSegment::Json(_) => "json",
            MessageSegment::Xml(_) => "xml",
            MessageSegment::Forward { .. } => "forward",
            MessageSegment::Raw { .. } => "raw",
        }
    }

    fn message_event(message: Value) -> OneBotEvent {
        event_of(json!({
            "post_type": "message",
            "message_type": "group",
            "self_id": 10001,
            "group_id": 900000001,
            "user_id": 20002,
            "message": message,
        }))
    }

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

    #[test]
    fn outbound_segments_map_to_onebot_v11_shapes() {
        assert_eq!(
            segment_to_json(&MessageSegment::At(123)),
            json!({ "type": "at", "data": { "qq": 123 } })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::AtAll),
            json!({ "type": "at", "data": { "qq": "all" } })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::Reply(77)),
            json!({ "type": "reply", "data": { "id": 77 } })
        );
        // 表情号能转数字就转，转不了就原样带上（实现端两种都吃）
        assert_eq!(
            segment_to_json(&MessageSegment::Face("299".into())),
            json!({ "type": "face", "data": { "id": 299 } })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::Face("sweet".into())),
            json!({ "type": "face", "data": { "id": "sweet" } })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::Record {
                url: Some("http://a/b.amr".into()),
                file: None,
            }),
            json!({ "type": "record", "data": { "file": "http://a/b.amr" } })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::Video {
                url: None,
                file: Some("http://a/b.mp4".into()),
            }),
            json!({ "type": "video", "data": { "file": "http://a/b.mp4" } })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::File {
                file_id: "fid".into(),
                name: "n.txt".into(),
                size: 12,
            }),
            json!({
                "type": "file",
                "data": { "file_id": "fid", "file_name": "n.txt", "file_size": 12 },
            })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::Poke {
                name: "bubble".into(),
                target: 5,
            }),
            json!({ "type": "poke", "data": { "type": "bubble", "target_id": 5 } })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::Location {
                name: "阿拜多斯".into(),
                address: "沙漠".into(),
                latitude: 1.5,
                longitude: -2.5,
            }),
            json!({
                "type": "location",
                "data": { "name": "阿拜多斯", "address": "沙漠", "lat": 1.5, "lon": -2.5 },
            })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::Json(json!({ "app": 1 }))),
            json!({ "type": "json", "data": { "data": { "app": 1 } } })
        );
        assert_eq!(
            segment_to_json(&MessageSegment::Xml(json!({ "msg": 2 }))),
            json!({ "type": "xml", "data": { "data": { "msg": 2 } } })
        );
        // 合并转发在普通发送接口里不被支持，有节点内容时只留占位文本，真发送走 send_forward_msg
        assert_eq!(
            segment_to_json(&MessageSegment::Forward {
                title: "十连".into(),
                id: String::new(),
                messages: vec![],
            }),
            json!({ "type": "text", "data": { "text": "[合并转发:十连]" } })
        );
        // 只有凭据没有节点的转发段按原样回 forward/id：实现端认这个写法，可以整份转发出去
        assert_eq!(
            segment_to_json(&MessageSegment::Forward {
                title: "十连".into(),
                id: "F1234".into(),
                messages: vec![],
            }),
            json!({ "type": "forward", "data": { "id": "F1234" } })
        );
        // 不认识的段按 kind + data 原样拼回去，不会因为框架不认识就从天上消失
        assert_eq!(
            segment_to_json(&MessageSegment::Raw {
                kind: "markdown".into(),
                data: json!({ "content": "# 标题" }),
            }),
            json!({ "type": "markdown", "data": { "content": "# 标题" } })
        );
    }

    #[test]
    fn media_value_prefers_url_then_bytes_then_path() {
        assert_eq!(
            media_value(
                &Some("http://u".into()),
                &Some("f.png".into()),
                Some(&[0xFF, 0xD8])
            ),
            "http://u"
        );
        assert_eq!(
            media_value(&None, &None, Some(&[0xFF, 0xD8])),
            "base64:///9g="
        );
        // 读不到的本地路径原样回传，让实现端按 file_id/网络地址自行处理
        assert_eq!(
            media_value(&None, &Some("definitely-missing.png".into()), None),
            "definitely-missing.png"
        );
    }

    #[test]
    fn inbound_array_segments_are_parsed() {
        let event = message_event(json!([
            { "type": "text", "data": { "text": "你好" } },
            { "type": "at", "data": { "qq": "all" } },
            { "type": "at", "data": { "qq": 20002 } },
            { "type": "reply", "data": { "id": "123" } },
            { "type": "face", "data": { "id": 6 } },
            { "type": "voice", "data": { "url": "http://v" } },
            { "type": "video", "data": { "file": "http://m" } },
            { "type": "file", "data": { "file_id": "fid", "file_name": "n.txt", "file_size": "9" } },
            { "type": "poke", "data": { "type": "punch", "target": "7" } },
            { "type": "location", "data": { "name": "x", "address": "y", "lat": "1.25", "lon": 2.5 } },
            { "type": "json", "data": { "data": "{\"app\":\"1\"}" } },
            { "type": "unknown-type", "data": {} },
        ]));
        let segments = extract_segments(&event);
        let names: Vec<&str> = segments.iter().map(kind_of).collect();
        assert_eq!(
            names,
            vec![
                "text", "at_all", "at", "reply", "face", "record", "video", "file", "poke",
                "location", "json", "raw"
            ],
            "未知段应原样留成 raw、其余段全部还原：{segments:?}"
        );
        assert!(matches!(
            &segments[11],
            MessageSegment::Raw { kind, .. } if kind == "unknown-type"
        ));
        assert!(matches!(&segments[3], MessageSegment::Reply(123)));
        assert!(matches!(
            &segments[5],
            MessageSegment::Record { url: Some(url), file: None } if url == "http://v"
        ));
        assert!(matches!(
            &segments[7],
            MessageSegment::File { file_id, name, size }
                if file_id == "fid" && name == "n.txt" && *size == 9
        ));
        assert!(matches!(
            &segments[8],
            MessageSegment::Poke { name, target: 7 } if name == "punch"
        ));
        assert!(matches!(
            &segments[9],
            MessageSegment::Location { latitude, longitude, .. }
                if (*latitude - 1.25).abs() < 1e-9 && (*longitude - 2.5).abs() < 1e-9
        ));
        // json 卡片的 data 字段是一段 JSON 字符串，解析成对象再交给插件
        assert!(matches!(
            &segments[10],
            MessageSegment::Json(value) if value == &json!({ "app": "1" })
        ));
    }

    #[test]
    fn forward_node_content_is_parsed_recursively() {
        let event = message_event(json!([
            { "type": "text", "data": { "text": "看这个" } },
            {
                "type": "node",
                "data": {
                    "title": "聊天记录",
                    "name": "老师",
                    "uin": 20002,
                    "content": [
                        { "type": "text", "data": { "text": "第一条" } },
                        { "type": "at", "data": { "qq": 30003 } },
                    ],
                }
            },
        ]));
        let segments = extract_segments(&event);
        let MessageSegment::Forward {
            title,
            id,
            messages,
        } = &segments[1]
        else {
            panic!("应为合并转发段：{segments:?}");
        };
        assert_eq!(title, "聊天记录");
        assert_eq!(id, "");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].name, "老师");
        assert_eq!(messages[0].uin, 20002);
        assert!(matches!(&messages[0].content[1], MessageSegment::At(30003)));
    }

    /// 实现端只给凭据（NTQQ 系的常态）：段必须留下，否则插件连"这是条转发"都不知道
    #[test]
    fn forward_id_without_content_is_kept() {
        let event = message_event(json!([
            { "type": "forward", "data": { "id": "F-42", "title": "群聊的聊天记录" } },
        ]));
        let segments = extract_segments(&event);
        let MessageSegment::Forward {
            title,
            id,
            messages,
        } = &segments[0]
        else {
            panic!("应为合并转发段：{segments:?}");
        };
        assert_eq!(id, "F-42");
        assert_eq!(title, "群聊的聊天记录");
        assert!(messages.is_empty());

        // CQ 码写法同样还原成带 id 的转发段
        let decoded = decode_cq_message("[CQ:forward,id=F-42]");
        let MessageSegment::Forward { id, .. } = &decoded[0] else {
            panic!("应解出转发段：{decoded:?}");
        };
        assert_eq!(id, "F-42");
    }

    /// 未知类型不再被丢掉：原样留成 Raw，转发/重发时按 kind + data 拼回去
    #[test]
    fn unknown_segment_kinds_survive_as_raw() {
        let event = message_event(json!([
            { "type": "markdown", "data": { "content": "# 标题" } },
            { "type": "text", "data": { "text": "正文" } },
        ]));
        let segments = extract_segments(&event);
        assert_eq!(segments.len(), 2);
        let MessageSegment::Raw { kind, data } = &segments[0] else {
            panic!("应为原样保留段：{segments:?}");
        };
        assert_eq!(kind, "markdown");
        assert_eq!(data["content"], "# 标题");
        assert_eq!(
            segment_to_json(&segments[0]),
            json!({ "type": "markdown", "data": { "content": "# 标题" } })
        );
    }

    /// `file=base64://…` 就地解成字节：这类实现端既不给路径也不给 URL
    #[test]
    fn base64_media_decodes_into_image_bytes() {
        let event = message_event(json!([
            { "type": "image", "data": { "file": "base64:///9g=" } },
        ]));
        let segments = extract_segments(&event);
        let MessageSegment::Image { url, file, data } = &segments[0] else {
            panic!("应为图片段：{segments:?}");
        };
        assert!(url.is_none() && file.is_none(), "url={url:?} file={file:?}");
        assert_eq!(data.as_deref(), Some(&[0xFF, 0xD8][..]));

        // 解不动的字符串原样留在 file 槽，不猜成字节
        let event = message_event(json!([
            { "type": "image", "data": { "file": "base64://@@@" } },
        ]));
        let segments = extract_segments(&event);
        let MessageSegment::Image { file, data, .. } = &segments[0] else {
            panic!("应为图片段：{segments:?}");
        };
        assert!(data.is_none());
        assert_eq!(file.as_deref(), Some("base64://@@@"));
    }

    /// 实现端把引用段写成 `quote` 的也有（NapCat / Lagrange），要和 `reply` 认成同一段
    #[test]
    fn quote_segment_alias_decodes_as_reply() {
        let event = message_event(json!([
            { "type": "quote", "data": { "id": 12 } },
            { "type": "text", "data": { "text": "/查日志" } },
        ]));
        let segments = extract_segments(&event);
        assert!(
            matches!(&segments[0], MessageSegment::Reply(12)),
            "应解成引用段：{segments:?}"
        );
        assert_eq!(command_text(&event, 10001), "/查日志");
        let decoded = decode_cq_message("[CQ:quote,id=12]你好");
        assert!(
            matches!(&decoded[0], MessageSegment::Reply(12)),
            "CQ 写法也应解成引用段：{decoded:?}"
        );
    }

    /// CQ 卡片里的转义必须还原，否则插件拿到的 JSON 带着一串实体码解析不了
    #[test]
    fn cq_card_unescapes_its_payload() {
        let decoded =
            decode_cq_message("[CQ:json,data={\"app\":\"node\"&#44;\"text\":\"a&#44;b&#58;c\"}]");
        let MessageSegment::Json(card) = &decoded[0] else {
            panic!("应为 json 卡片：{decoded:?}");
        };
        assert_eq!(card["app"], "node");
        assert_eq!(card["text"], "a,b:c");
    }

    #[test]
    fn empty_forward_node_is_dropped() {
        let event = message_event(json!([
            { "type": "node", "data": { "title": "空" } },
            { "type": "text", "data": { "text": "还在" } },
        ]));
        let segments = extract_segments(&event);
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], MessageSegment::Text(text) if text == "还在"));
    }

    /// 事件入口改成一次性解析后，日志、命令匹配与钩子必须看到同一份东西：
    /// `ParsedMessage` 的三个字段与三个单用入口逐字相等
    #[test]
    fn parse_message_agrees_with_the_single_shot_helpers() {
        let event = message_event(json!([
            { "type": "reply", "data": { "id": 501 } },
            { "type": "at", "data": { "qq": 10001 } },
            { "type": "text", "data": { "text": "/单抽" } },
            { "type": "image", "data": { "url": "https://x/a.png" } },
        ]));
        let parsed = ParsedMessage::parse(&event, 10001);
        assert_eq!(parsed.text, extract_text(&event));
        assert_eq!(parsed.command_text, command_text(&event, 10001));
        assert_eq!(parsed.command_text, "/单抽 [image]");
        let again = extract_segments(&event);
        assert_eq!(parsed.segments.len(), again.len());
        assert_eq!(format!("{:?}", parsed.segments), format!("{:?}", again));
        // 引用段被 command_text 剥掉了，编号只能从段里取
        assert_eq!(parsed.quoted(), Some(501));
    }

    #[test]
    fn command_text_strips_mention_of_the_bot() {
        // 群里 @机器人 /单抽：段是 [At(bot), Text(" /单抽")]
        let event = message_event(json!([
            { "type": "at", "data": { "qq": 10001 } },
            { "type": "text", "data": { "text": " /单抽" } },
        ]));
        assert_eq!(command_text(&event, 10001), "/单抽");

        // @全体成员 也算召唤机器人
        let event = message_event(json!([
            { "type": "at", "data": { "qq": "all" } },
            { "type": "text", "data": { "text": "帮助" } },
        ]));
        assert_eq!(command_text(&event, 10001), "帮助");
    }

    #[test]
    fn command_text_keeps_mentions_of_others() {
        let event = message_event(json!([
            { "type": "at", "data": { "qq": 20002 } },
            { "type": "text", "data": { "text": "抽到了" } },
            { "type": "face", "data": { "id": 1 } },
        ]));
        // 非文本段用紧凑占位（不是日志里那种带整条 URL/中文名的观感串），
        // 免得展示串里的空格把首词切坏、命令表查不中
        assert_eq!(command_text(&event, 10001), "@20002 抽到了 [face:1]");
    }

    #[test]
    fn command_text_strips_only_leading_reply_and_image() {
        // 引用 + 图片 + @机器人 都在命令前，逐个剥掉
        let event = message_event(json!([
            { "type": "reply", "data": { "id": 5 } },
            { "type": "image", "data": { "url": "http://i" } },
            { "type": "at", "data": { "qq": 10001 } },
            { "type": "text", "data": { "text": "/查日志" } },
        ]));
        assert_eq!(command_text(&event, 10001), "/查日志");

        // 夹在中间的引用不是前缀，保留为紧凑占位
        let event = message_event(json!([
            { "type": "text", "data": { "text": "a" } },
            { "type": "reply", "data": { "id": 5 } },
        ]));
        assert_eq!(command_text(&event, 10001), "a [reply]");

        // 部分实现端在 @机器人 前面先塞一个纯空白文本段：它不算"正文开始"，
        // 否则 leading 被它关掉，紧跟其后的 @机器人 就当成正文混进首词了
        let event = message_event(json!([
            { "type": "text", "data": { "text": " " } },
            { "type": "at", "data": { "qq": 10001 } },
            { "type": "text", "data": { "text": "/查日志" } },
        ]));
        assert_eq!(command_text(&event, 10001), "/查日志");
    }

    #[test]
    fn extract_text_joins_displays_without_spaces() {
        let event = message_event(json!([
            { "type": "text", "data": { "text": "抽到 " } },
            { "type": "at", "data": { "qq": 20002 } },
            { "type": "image", "data": { "url": "http://i" } },
        ]));
        assert_eq!(
            extract_text(&event),
            "抽到 @20002[arona:image,url=http://i]"
        );
    }

    #[test]
    fn cq_codes_are_decoded_into_segments() {
        let event = event_of(json!({
            "post_type": "message",
            "message_type": "private",
            "self_id": 10001,
            "raw_message": "你好[CQ:at,qq=all][CQ:image,file=http://i/a.png][CQ:face,id=6]尾巴[CQ:mystery,x=1]",
        }));
        let segments = extract_segments(&event);
        assert!(matches!(&segments[0], MessageSegment::Text(t) if t == "你好"));
        assert!(matches!(&segments[1], MessageSegment::AtAll));
        assert!(matches!(
            &segments[2],
            MessageSegment::Image { file: Some(f), .. } if f == "http://i/a.png"
        ));
        assert!(matches!(&segments[3], MessageSegment::Face(id) if id == "6"));
        // 未知 CQ 码整段按原文留成文本，不会丢内容
        assert!(matches!(&segments[4], MessageSegment::Text(t) if t == "尾巴"));
        assert!(
            matches!(&segments[5], MessageSegment::Text(t) if t == "[CQ:mystery,x=1]"),
            "segments: {segments:?}"
        );
    }

    #[test]
    fn cq_entities_are_unescaped() {
        let segments = decode_cq_message("[CQ:file,file_id=abc&#44;def&#58;g&#93;h&amp;i]");
        let MessageSegment::File { file_id, size, .. } = &segments[0] else {
            panic!("应为文件段：{segments:?}");
        };
        assert_eq!(file_id, "abc,def:g]h&i");
        assert_eq!(*size, 0);

        // 普通文本里的实体同样还原，未知 CQ 码保留原文
        let segments = decode_cq_message("a&#44;b[CQ:mystery,x=1]c");
        assert!(matches!(&segments[0], MessageSegment::Text(t) if t == "a,b"));
        assert!(
            matches!(&segments[1], MessageSegment::Text(t) if t == "[CQ:mystery,x=1]"),
            "segments: {segments:?}"
        );
        assert!(matches!(&segments[2], MessageSegment::Text(t) if t == "c"));
    }

    #[test]
    fn forward_nodes_serialize_for_send_forward_msg() {
        let nodes = forward_messages_to_json(&[ForwardMessage {
            name: "诺亚".into(),
            uin: 20002,
            content: vec![
                MessageSegment::Text("十连结果".into()),
                MessageSegment::Image {
                    url: Some("http://i/1.png".into()),
                    file: None,
                    data: None,
                },
            ],
        }]);
        assert_eq!(
            nodes,
            json!([
                {
                    "type": "node",
                    "data": {
                        "name": "诺亚",
                        "uin": 20002,
                        "content": [
                            { "type": "text", "data": { "text": "十连结果" } },
                            { "type": "image", "data": { "file": "http://i/1.png" } },
                        ],
                    }
                }
            ])
        );
    }
}
