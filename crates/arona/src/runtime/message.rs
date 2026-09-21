//! 消息模型与发送抽象（对应原版 runtime 包 MessageSender/OutgoingMessage 等）

use serde_json::Value;

/// 消息段（对应 mirai 的 `MessageMetadata`/`Element` 体系与 OneBot v11 的 segment）
#[derive(Clone, Debug)]
pub enum MessageSegment {
    Text(String),
    At(i64),
    /// @全体成员
    AtAll,
    /// 引用某条消息（入站是"别人引用了机器人/某人"，出站是"回复那条消息"）
    Reply(i64),
    Image {
        url: Option<String>,
        file: Option<String>,
        data: Option<Vec<u8>>,
    },
    /// QQ 表情（实现端给的表情号）
    Face(String),
    /// 语音
    Record {
        url: Option<String>,
        file: Option<String>,
    },
    /// 视频
    Video {
        url: Option<String>,
        file: Option<String>,
    },
    /// 群文件（入站带 file_id，出站按 file_id 分享已上传的文件）
    File {
        file_id: String,
        name: String,
        size: u64,
    },
    /// 戳一戳
    Poke {
        /// 动作名（"bubble"、"punch" 之类，由实现端定义）
        name: String,
        /// 被戳的人，0 表示戳自己
        target: i64,
    },
    /// 位置共享
    Location {
        name: String,
        address: String,
        latitude: f64,
        longitude: f64,
    },
    /// 卡片消息（OneBot 的 json 段，如音乐/小程序分享）
    Json(Value),
    /// 卡片消息（xml 段，机器人消息里的链接卡片多为这种）
    Xml(Value),
    /// 合并转发
    Forward {
        /// 转发标题（实现端多给 "群聊的聊天记录"）
        title: String,
        /// 转发的 flag/id：NapCat、LLOWeb 等把这个段的 `id` 给出来，
        /// 插件可以拿它调 `get_forward_msg` 取完整节点。空串表示实现端没给。
        id: String,
        /// 已经解析出来的节点（实现端把 `content` 一起塞进事件时才有，常为空）
        messages: Vec<ForwardMessage>,
    },
    /// 框架还不认识的消息段（新版实现端加的类型）。原样留着，别把它当没有：
    /// 转发/重发时要按 `kind` + `data` 拼回去，插件也能看到"这里有个 X 段"而不是空气。
    Raw {
        kind: String,
        data: Value,
    },
}

impl MessageSegment {
    /// 该段是否只是"召唤机器人"的前缀（群里 @机器人 后跟命令时的头几段）
    pub fn is_mention_of(&self, self_id: i64) -> bool {
        match self {
            MessageSegment::At(user_id) => *user_id == self_id,
            MessageSegment::AtAll => true,
            _ => false,
        }
    }
}

/// 合并转发中的一条节点消息
#[derive(Clone, Debug)]
pub struct ForwardMessage {
    pub name: String,
    pub uin: i64,
    pub content: Vec<MessageSegment>,
}

/// 待发送消息
#[derive(Clone, Debug)]
pub struct OutgoingMessage {
    pub segments: Vec<MessageSegment>,
    /// 发送成功后延迟自动撤回的毫秒数，None 表示不撤回
    pub revoke_after_millis: Option<u64>,
}

impl OutgoingMessage {
    /// 任意段组合
    pub fn new(segments: Vec<MessageSegment>) -> OutgoingMessage {
        OutgoingMessage {
            segments,
            revoke_after_millis: None,
        }
    }

    pub fn text(value: impl Into<String>) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::Text(value.into())],
            revoke_after_millis: None,
        }
    }

    pub fn at(user_id: i64) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::At(user_id)],
            revoke_after_millis: None,
        }
    }

    pub fn at_all() -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::AtAll],
            revoke_after_millis: None,
        }
    }

    /// 引用某条消息后再说这些内容（mirai 的 `QuoteReply`）
    pub fn quoted(message_id: i64, text: impl Into<String>) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![
                MessageSegment::Reply(message_id),
                MessageSegment::Text(text.into()),
            ],
            revoke_after_millis: None,
        }
    }

    pub fn image_file(file: impl Into<String>) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::Image {
                url: None,
                file: Some(file.into()),
                data: None,
            }],
            revoke_after_millis: None,
        }
    }

    pub fn image_data(data: Vec<u8>) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::Image {
                url: None,
                file: None,
                data: Some(data),
            }],
            revoke_after_millis: None,
        }
    }

    pub fn record_file(file: impl Into<String>) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::Record {
                url: None,
                file: Some(file.into()),
            }],
            revoke_after_millis: None,
        }
    }

    pub fn video_file(file: impl Into<String>) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::Video {
                url: None,
                file: Some(file.into()),
            }],
            revoke_after_millis: None,
        }
    }

    /// 卡片消息（json 段）
    pub fn json_card(card: Value) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::Json(card)],
            revoke_after_millis: None,
        }
    }

    /// 卡片消息（xml 段）
    pub fn xml_card(card: Value) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::Xml(card)],
            revoke_after_millis: None,
        }
    }

    pub fn forward(title: impl Into<String>, messages: Vec<ForwardMessage>) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::Forward {
                title: title.into(),
                id: String::new(),
                messages,
            }],
            revoke_after_millis: None,
        }
    }

    pub fn with_revoke(mut self, millis: u64) -> OutgoingMessage {
        self.revoke_after_millis = Some(millis);
        self
    }

    /// 在开头挂上引用（把已拼好的消息变成"回复某条消息"）
    pub fn with_quote(mut self, message_id: i64) -> OutgoingMessage {
        self.segments.insert(0, MessageSegment::Reply(message_id));
        self
    }
}

impl std::ops::Add for OutgoingMessage {
    type Output = OutgoingMessage;

    fn add(mut self, other: OutgoingMessage) -> OutgoingMessage {
        self.segments.extend(other.segments);
        if self.revoke_after_millis.is_none() {
            self.revoke_after_millis = other.revoke_after_millis;
        }
        self
    }
}

/// 发送目标
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageTarget {
    Group(i64),
    Private(i64),
}

impl MessageTarget {
    pub fn id(&self) -> i64 {
        match self {
            MessageTarget::Group(id) | MessageTarget::Private(id) => *id,
        }
    }
}

/// 发送回执
#[derive(Clone, Debug, Default)]
pub struct MessageReceipt {
    pub message_id: Option<i64>,
    /// 实现端另外给的那个号（NapCat / LLOWeb 的 `real_id`）：撤回与回查只认它时靠它。
    /// 多数实现端不给，留 None 即按 message_id 操作。
    pub real_id: Option<i64>,
}

impl MessageReceipt {
    /// 只有协议层 message_id 的回执（实现端没给 real_id 时用）
    pub fn new(message_id: Option<i64>) -> MessageReceipt {
        MessageReceipt {
            message_id,
            real_id: None,
        }
    }
}

/// 盒装 Future，用于让含 async 方法的 trait 可 dyn 化
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// 消息发送抽象（独立模式由 OneBot 实现）
pub trait MessageSender: Send + Sync {
    fn send<'a>(
        &'a self,
        target: MessageTarget,
        message: OutgoingMessage,
    ) -> BoxFuture<'a, MessageReceipt>;
}
