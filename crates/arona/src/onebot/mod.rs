//! OneBot v11 协议支持（对应原版 onebot 包，完全剥离 mirai）

pub mod api;
pub mod application;
pub mod business;
pub mod connection;
pub mod console;
pub mod hooks;
pub mod http_api;
pub mod http_reverse;
pub mod message_sender;
pub mod model;
pub mod protocol;
pub mod ws_forward;
pub mod ws_reverse;

pub use api::{MessagePayload, OneBotApi, OneBotError};
pub use application::OneBotApplication;
pub use business::BusinessHandler;
pub use connection::{ConnectionRegistry, OneBotConnection};
pub use hooks::{EventContext, EventKind, HookFlow, subscribe};
pub use message_sender::{DeferredMessageSender, OneBotMessageSender};
pub use model::{OneBotAction, OneBotActionResponse, OneBotEvent, ParsedPayload};

/// 全局 OneBot 动作出口（插件与框架共用）
pub fn api() -> OneBotApi {
    OneBotApi::global()
}
