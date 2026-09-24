//! 运行期服务引用（对应原版 RuntimeServices）：数据根目录 + 主动发消息的出口。
//!
//! 这份状态由 [`crate::framework::Framework`] 实例持有，本模块的自由函数一律走进程默认实例。
//! 动态插件 dll 静态链接了另一份框架代码，装载时 [`Framework::adopt_host`] 会把宿主实例
//! 整个接过来，所以插件里调 [`send_message`] 用的是宿主真正在跑的那条连接，而不是 dll
//! 自己那份永远空着的。
use crate::framework::Framework;
use crate::runtime::message::{MessageReceipt, MessageSender, MessageTarget, OutgoingMessage};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub struct RuntimeServices {
    /// 数据根目录
    data_root: RwLock<Option<PathBuf>>,
    /// 全局消息发送器（活动推送等主动消息使用，取当前首个可用连接）
    message_sender: RwLock<Option<Arc<dyn MessageSender>>>,
}

impl Default for RuntimeServices {
    fn default() -> Self {
        RuntimeServices {
            data_root: RwLock::new(None),
            message_sender: RwLock::new(None),
        }
    }
}

impl RuntimeServices {
    pub fn set_data_root(&self, path: PathBuf) {
        *self.data_root.write().unwrap() = Some(path);
    }

    pub fn data_root(&self) -> Option<PathBuf> {
        self.data_root.read().unwrap().clone()
    }

    /// 消息发送器是否已就绪
    pub fn sender_ready(&self) -> bool {
        self.message_sender.read().unwrap().is_some()
    }

    pub fn set_message_sender(&self, sender: Arc<dyn MessageSender>) {
        *self.message_sender.write().unwrap() = Some(sender);
    }

    /// 主动发送消息（活动推送等）。没有可用发送器时返回空回执。
    pub async fn send_message(
        &self,
        target: MessageTarget,
        message: OutgoingMessage,
    ) -> MessageReceipt {
        let sender = self.message_sender.read().unwrap().clone();
        match sender {
            Some(s) => s.send(target, message).await,
            None => MessageReceipt::default(),
        }
    }
}

fn host() -> &'static RuntimeServices {
    Framework::global().runtime_services()
}

pub fn set_data_root(path: PathBuf) {
    host().set_data_root(path);
}

pub fn data_root() -> Option<PathBuf> {
    host().data_root()
}

/// 消息发送器是否已就绪
pub fn sender_ready() -> bool {
    host().sender_ready()
}

pub fn set_message_sender(sender: Arc<dyn MessageSender>) {
    host().set_message_sender(sender);
}

/// 主动发送消息（活动推送等）。没有可用发送器时返回空回执。
pub async fn send_message(target: MessageTarget, message: OutgoingMessage) -> MessageReceipt {
    host().send_message(target, message).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::message::BoxFuture;

    struct StubSender;

    impl MessageSender for StubSender {
        fn send<'a>(
            &'a self,
            _target: MessageTarget,
            _message: OutgoingMessage,
        ) -> BoxFuture<'a, MessageReceipt> {
            Box::pin(async { MessageReceipt::new(Some(4242)) })
        }
    }

    /// 出口必须跟着实例走：动态插件 dll 装载时宿主把 [`Framework`] 交进来，
    /// 插件里的自由函数因此落到宿主这一份。哪天它又变回进程级 `static`，这条先红。
    #[test]
    fn outlets_are_scoped_to_their_framework_instance() {
        let host = Framework::new();
        let other = Framework::new();
        host.runtime_services()
            .set_message_sender(Arc::new(StubSender));
        host.runtime_services()
            .set_data_root(PathBuf::from("data-host"));

        assert!(host.runtime_services().sender_ready());
        assert_eq!(
            host.runtime_services().data_root(),
            Some(PathBuf::from("data-host"))
        );
        // 另一套实例（以及没装配过的默认实例）都看不见它
        assert!(!other.runtime_services().sender_ready());
        assert_eq!(other.runtime_services().data_root(), None);
        assert!(!Framework::global().runtime_services().sender_ready());
    }

    /// 消息要真的交给**同一套实例**上登记的发送器；没登记时按原行为返回空回执，不 panic
    #[tokio::test]
    async fn send_message_uses_the_sender_of_the_same_instance() {
        let services = RuntimeServices::default();
        assert_eq!(
            services
                .send_message(MessageTarget::Group(20001), OutgoingMessage::text("早"))
                .await
                .message_id,
            None
        );
        services.set_message_sender(Arc::new(StubSender));
        assert_eq!(
            services
                .send_message(MessageTarget::Group(20001), OutgoingMessage::text("早"))
                .await
                .message_id,
            Some(4242)
        );
    }
}
