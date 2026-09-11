//! 命令上下文与命令分发（对应原版 runtime 包 CommandContext/SimpleCommandDispatcher）
use super::message::{BoxFuture, MessageReceipt, MessageSender, MessageTarget, OutgoingMessage};
use std::collections::HashMap;
use std::sync::Arc;

/// 命令上下文
pub struct CommandContext {
    pub user_id: i64,
    pub group_id: Option<i64>,
    pub text: String,
    pub sender_name: Option<String>,
    pub is_admin: bool,
    pub sender: Arc<dyn MessageSender>,
}

impl CommandContext {
    pub fn target(&self) -> MessageTarget {
        match self.group_id {
            Some(group_id) => MessageTarget::Group(group_id),
            None => MessageTarget::Private(self.user_id),
        }
    }

    pub async fn reply_message(&self, message: OutgoingMessage) -> MessageReceipt {
        self.sender.send(self.target(), message).await
    }

    pub async fn reply(&self, text: impl Into<String>) -> MessageReceipt {
        self.reply_message(OutgoingMessage::text(text.into())).await
    }
}

/// 命令处理器
pub trait CommandHandler: Send + Sync {
    fn handle<'a>(
        &'a self,
        context: Arc<CommandContext>,
        arguments: Vec<String>,
    ) -> BoxFuture<'a, Option<OutgoingMessage>>;
}

/// 将异步函数包装为命令处理器
pub struct FnCommandHandler<F> {
    inner: F,
}

impl<F, Fut> FnCommandHandler<F>
where
    F: Fn(Arc<CommandContext>, Vec<String>) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Option<OutgoingMessage>> + Send,
{
    pub fn new(inner: F) -> Self {
        FnCommandHandler { inner }
    }
}

impl<F, Fut> CommandHandler for FnCommandHandler<F>
where
    F: Fn(Arc<CommandContext>, Vec<String>) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Option<OutgoingMessage>> + Send,
{
    fn handle<'a>(
        &'a self,
        context: Arc<CommandContext>,
        arguments: Vec<String>,
    ) -> BoxFuture<'a, Option<OutgoingMessage>> {
        Box::pin(async move { (self.inner)(context, arguments).await })
    }
}

pub fn handler<F, Fut>(f: F) -> Arc<dyn CommandHandler>
where
    F: Fn(Arc<CommandContext>, Vec<String>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<OutgoingMessage>> + Send + 'static,
{
    Arc::new(FnCommandHandler::new(f))
}

/// 命令注册信息
pub struct CommandRegistration {
    pub names: Vec<String>,
    pub description: String,
    pub command_handler: Arc<dyn CommandHandler>,
}

impl CommandRegistration {
    pub fn new(
        names: Vec<String>,
        description: impl Into<String>,
        command_handler: Arc<dyn CommandHandler>,
    ) -> CommandRegistration {
        CommandRegistration {
            names,
            description: description.into(),
            command_handler,
        }
    }
}

/// 未匹配命令时的兜底处理
pub trait FallbackHandler: Send + Sync {
    fn handle<'a>(&'a self, context: Arc<CommandContext>) -> BoxFuture<'a, ()>;
}

pub struct FnFallbackHandler<F> {
    inner: F,
}

impl<F, Fut> FallbackHandler for FnFallbackHandler<F>
where
    F: Fn(Arc<CommandContext>) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = ()> + Send,
{
    fn handle<'a>(&'a self, context: Arc<CommandContext>) -> BoxFuture<'a, ()> {
        Box::pin(async move { (self.inner)(context).await })
    }
}

pub fn fallback<F, Fut>(f: F) -> Arc<dyn FallbackHandler>
where
    F: Fn(Arc<CommandContext>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    Arc::new(FnFallbackHandler { inner: f })
}

/// 简单命令分发器（对应 SimpleCommandDispatcher）
pub struct SimpleCommandDispatcher {
    commands: HashMap<String, Arc<dyn CommandHandler>>,
    fallback: Option<Arc<dyn FallbackHandler>>,
}

impl SimpleCommandDispatcher {
    pub fn new(
        registrations: Vec<CommandRegistration>,
        fallback: Option<Arc<dyn FallbackHandler>>,
    ) -> SimpleCommandDispatcher {
        let mut commands = HashMap::new();
        for registration in registrations {
            for name in registration.names {
                commands.insert(normalize(&name), registration.command_handler.clone());
            }
        }
        SimpleCommandDispatcher { commands, fallback }
    }

    pub async fn dispatch(&self, context: Arc<CommandContext>) -> bool {
        let parts: Vec<String> = context
            .text
            .trim()
            .split_whitespace()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if parts.is_empty() {
            return false;
        }
        let key = normalize(&parts[0]);
        let args = parts[1..].to_vec();
        match self.commands.get(&key) {
            Some(cmd) => {
                cmd.handle(context, args).await;
                true
            }
            None => {
                if let Some(fallback) = &self.fallback {
                    fallback.handle(context).await;
                }
                false
            }
        }
    }
}

fn normalize(command: &str) -> String {
    command.trim().to_lowercase()
}
