//! 独立模式命令实现（对应原版 standalone/commands 包）

pub mod activity;
pub mod backup;
pub mod config_cmd;
pub mod emergency;
pub mod gacha_cmd;
pub mod name;
pub mod tarot;
pub mod task_cmd;
pub mod trainer;

use arona::runtime::dispatcher::{CommandContext, CommandHandler, handler};
use arona::services::ServiceInfo;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// 服务开关/群聊/权限校验（对应原版 GuardedHandler.guarded）
pub async fn guarded(service: &ServiceInfo, context: &CommandContext) -> bool {
    if !service.enable.load(Ordering::SeqCst) {
        context.reply("功能未启用").await;
        return false;
    }
    if service.group_only && context.group_id.is_none() {
        context.reply("该功能仅限群聊使用").await;
        return false;
    }
    if service.admin_only && !context.is_admin {
        context.reply("权限不足").await;
        return false;
    }
    true
}

/// 构造带服务校验的命令处理器：业务函数返回待发送消息，由包装器统一回复
pub fn guarded_arg_handler<F, Fut>(service: Arc<ServiceInfo>, f: F) -> Arc<dyn CommandHandler>
where
    F: Fn(Arc<CommandContext>, Vec<String>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<arona::runtime::message::OutgoingMessage>>
        + Send
        + 'static,
{
    let f = Arc::new(f);
    handler(move |context, arguments| {
        let f = f.clone();
        let service = service.clone();
        async move {
            if !guarded(&service, &context).await {
                return None;
            }
            let reply = f(context.clone(), arguments).await;
            if let Some(message) = reply {
                crate::standalone::history::reply(&context, message).await;
            }
            None
        }
    })
}

/// 无参数版本（对应 GuardedHandler）
pub fn guarded_handler<F, Fut>(service: Arc<ServiceInfo>, f: F) -> Arc<dyn CommandHandler>
where
    F: Fn(Arc<CommandContext>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<arona::runtime::message::OutgoingMessage>>
        + Send
        + 'static,
{
    let f = Arc::new(f);
    handler(move |context, _arguments| {
        let f = f.clone();
        let service = service.clone();
        async move {
            if !guarded(&service, &context).await {
                return None;
            }
            let reply = f(context.clone()).await;
            if let Some(message) = reply {
                crate::standalone::history::reply(&context, message).await;
            }
            None
        }
    })
}
