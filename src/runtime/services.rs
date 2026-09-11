//! 运行期服务引用（对应原版 RuntimeServices）
use crate::runtime::message::{MessageReceipt, MessageSender, MessageTarget, OutgoingMessage};
use once_cell::sync::OnceCell;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub struct RuntimeServices {
    /// 是否独立模式（本移植版恒为 true）
    pub is_standalone: bool,
    /// 数据根目录
    pub data_root: RwLock<Option<PathBuf>>,
    /// 全局消息发送器（活动推送等主动消息使用，取当前首个可用连接）
    pub message_sender: RwLock<Option<Arc<dyn MessageSender>>>,
}

static SERVICES: OnceCell<RuntimeServices> = OnceCell::new();

fn instance() -> &'static RuntimeServices {
    SERVICES.get_or_init(|| RuntimeServices {
        is_standalone: true,
        data_root: RwLock::new(None),
        message_sender: RwLock::new(None),
    })
}

pub fn set_data_root(path: PathBuf) {
    *instance().data_root.write().unwrap() = Some(path);
}

pub fn data_root() -> Option<PathBuf> {
    instance().data_root.read().unwrap().clone()
}

/// 消息发送器是否已就绪
pub fn sender_ready() -> bool {
    instance().message_sender.read().unwrap().is_some()
}

pub fn set_message_sender(sender: Arc<dyn MessageSender>) {
    *instance().message_sender.write().unwrap() = Some(sender);
}

/// 主动发送消息（活动推送等）。没有可用发送器时返回空回执。
pub async fn send_message(target: MessageTarget, message: OutgoingMessage) -> MessageReceipt {
    let sender = instance().message_sender.read().unwrap().clone();
    match sender {
        Some(s) => s.send(target, message).await,
        None => MessageReceipt { message_id: None },
    }
}
