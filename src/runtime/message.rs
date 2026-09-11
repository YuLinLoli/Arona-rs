//! 消息模型与发送抽象（对应原版 runtime 包 MessageSender/OutgoingMessage 等）

/// 消息段（对应原版 MessageSegment）
#[derive(Clone, Debug)]
pub enum MessageSegment {
    Text(String),
    At(i64),
    Image {
        url: Option<String>,
        file: Option<String>,
        data: Option<Vec<u8>>,
    },
    /// 合并转发
    Forward {
        title: String,
        messages: Vec<ForwardMessage>,
    },
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

    pub fn forward(title: impl Into<String>, messages: Vec<ForwardMessage>) -> OutgoingMessage {
        OutgoingMessage {
            segments: vec![MessageSegment::Forward {
                title: title.into(),
                messages,
            }],
            revoke_after_millis: None,
        }
    }

    pub fn with_revoke(mut self, millis: u64) -> OutgoingMessage {
        self.revoke_after_millis = Some(millis);
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
