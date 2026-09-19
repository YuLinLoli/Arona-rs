//! OneBot 消息发送器（对应原版 OneBotMessageSender）
use crate::onebot::connection::{ConnectionRegistry, OneBotConnection};
use crate::onebot::console;
use crate::onebot::model::OneBotAction;
use crate::onebot::protocol;
use crate::runtime::message::{
    BoxFuture, ForwardMessage, MessageReceipt, MessageSegment, MessageSender, MessageTarget,
    OutgoingMessage,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub struct OneBotMessageSender {
    pub connection: Option<Arc<dyn OneBotConnection>>,
    pub self_id: i64,
}

/// 每次发送时从注册表取首个可用连接的发送器（对应 AronaStandalone 设置的 RuntimeServices.messageSender）
pub struct DeferredMessageSender {
    pub registry: Arc<ConnectionRegistry>,
    pub self_id: i64,
}

impl MessageSender for OneBotMessageSender {
    fn send<'a>(
        &'a self,
        target: MessageTarget,
        message: OutgoingMessage,
    ) -> BoxFuture<'a, MessageReceipt> {
        Box::pin(
            async move { send_via(self.connection.clone(), self.self_id, target, message).await },
        )
    }
}

impl MessageSender for DeferredMessageSender {
    fn send<'a>(
        &'a self,
        target: MessageTarget,
        message: OutgoingMessage,
    ) -> BoxFuture<'a, MessageReceipt> {
        let registry = self.registry.clone();
        let self_id = self.self_id;
        Box::pin(async move {
            let connection = registry.first();
            send_via(connection, self_id, target, message).await
        })
    }
}

async fn send_via(
    connection: Option<Arc<dyn OneBotConnection>>,
    self_id: i64,
    target: MessageTarget,
    message: OutgoingMessage,
) -> MessageReceipt {
    console::print_outgoing(self_id, target, &message);
    let forward = message.segments.iter().find_map(|segment| match segment {
        MessageSegment::Forward { title, messages } => Some((title.clone(), messages.clone())),
        _ => None,
    });
    let receipt = match forward {
        Some((_title, messages)) => match &connection {
            Some(conn) => send_forward(conn.clone(), target, &messages).await,
            None => MessageReceipt { message_id: None },
        },
        None => match &connection {
            Some(conn) => send_normal(conn.clone(), target, &message).await,
            None => MessageReceipt { message_id: None },
        },
    };
    schedule_revoke(connection, receipt.clone(), message.revoke_after_millis);
    receipt
}

fn build_send_params(
    target: MessageTarget,
    message: Option<&OutgoingMessage>,
) -> (serde_json::Value, String) {
    match target {
        MessageTarget::Group(group_id) => {
            let params = match message {
                Some(message) => {
                    json!({ "group_id": group_id, "message": protocol::message_to_json(message) })
                }
                None => json!({ "group_id": group_id }),
            };
            (params, "send_group_msg".to_string())
        }
        MessageTarget::Private(user_id) => {
            let params = match message {
                Some(message) => {
                    json!({ "user_id": user_id, "message": protocol::message_to_json(message) })
                }
                None => json!({ "user_id": user_id }),
            };
            (params, "send_private_msg".to_string())
        }
    }
}

async fn send_normal(
    connection: Arc<dyn OneBotConnection>,
    target: MessageTarget,
    message: &OutgoingMessage,
) -> MessageReceipt {
    let (params, action_name) = build_send_params(target, Some(message));
    let response = connection
        .send(OneBotAction {
            action: action_name,
            params,
            echo: protocol::new_echo(),
        })
        .await;
    let message_id = response
        .and_then(|r| r.data)
        .and_then(|d| d.as_object().cloned())
        .and_then(|o| o.get("message_id").cloned())
        .and_then(|v| v.as_i64());
    MessageReceipt { message_id }
}

async fn send_forward(
    connection: Arc<dyn OneBotConnection>,
    target: MessageTarget,
    messages: &[ForwardMessage],
) -> MessageReceipt {
    let (params, _) = build_send_params(target, None);
    let mut map = params.as_object().cloned().unwrap_or_default();
    map.insert(
        "messages".into(),
        protocol::forward_messages_to_json(messages),
    );
    let response = connection
        .send(OneBotAction {
            action: "send_forward_msg".to_string(),
            params: json!(map),
            echo: protocol::new_echo(),
        })
        .await;
    // 合并转发节点常含大图 base64，实现端上传处理可能超过 15 秒才响应；
    // 只有实现端明确报错才降级为平铺发送，超时/异步受理时转发往往已实际发出，再发会重复
    match response {
        Some(response) if response.success() => {
            let message_id = response
                .data
                .and_then(|d| d.as_object().cloned())
                .and_then(|o| o.get("message_id").cloned())
                .and_then(|v| v.as_i64());
            MessageReceipt { message_id }
        }
        // retcode=1 / status=async：已提交处理，消息会随后发出
        Some(response) if response.async_accepted() => MessageReceipt { message_id: None },
        Some(response) => {
            crate::runtime::log::warning(format!(
                "发送合并转发失败(status={}, retcode={})，降级为平铺消息重发",
                response.status, response.retcode
            ));
            let flat_segments: Vec<MessageSegment> = messages
                .iter()
                .flat_map(|node| node.content.clone())
                .collect();
            let flat = OutgoingMessage {
                segments: flat_segments,
                revoke_after_millis: None,
            };
            send_normal(connection, target, &flat).await
        }
        // 超时/连接中断：无法确认转发是否已发出，不能再平铺重发一遍
        None => {
            crate::runtime::log::warning(
                "发送合并转发后未收到响应(可能超时)，为避免重复消息不再平铺重发",
            );
            MessageReceipt { message_id: None }
        }
    }
}

fn schedule_revoke(
    connection: Option<Arc<dyn OneBotConnection>>,
    receipt: MessageReceipt,
    revoke_after_millis: Option<u64>,
) {
    let Some(message_id) = receipt.message_id else {
        return;
    };
    let Some(delay_millis) = revoke_after_millis else {
        return;
    };
    if delay_millis == 0 {
        return;
    }
    let Some(connection) = connection else { return };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_millis)).await;
        let params = json!({ "message_id": message_id });
        let _ = connection
            .send(OneBotAction {
                action: "delete_msg".to_string(),
                params,
                echo: protocol::new_echo(),
            })
            .await;
    });
}
