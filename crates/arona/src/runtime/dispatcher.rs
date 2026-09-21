//! 命令上下文、命令注册表与分发（对应 mirai 的 `CommandManager` + `SimpleCommandDispatcher`）
//!
//! 框架侧只有**一张命令表 per 框架实例**：每个插件把自己的命令登记进来，表项记住归属插件 id。
//! 分发时逐个候选查门控（该插件全局/该群是否停用、该群是否关掉这个功能），
//! 因此多个插件可以并存，命令冲突按 [`CommandPriority`] 决定归属，
//! 停用某个插件只影响它名下的命令——不需要插件自己配合。
//!
//! 表本身是 [`CommandRegistry`] 实例，由 [`crate::framework::Framework`] 持有：
//! 进程默认实例走本模块的自由函数，测试用 `Framework::new()` 得到干净的一张表。
use super::message::{BoxFuture, MessageReceipt, MessageSender, MessageTarget, OutgoingMessage};
use super::priority::{CommandPriority, Priority};
use crate::framework::Framework;
use crate::runtime::config::Gating;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

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

/// 命令注册信息（插件侧只描述命令本身，归属插件由框架在登记时补上）
pub struct CommandRegistration {
    pub names: Vec<String>,
    pub description: String,
    /// 用法示例（帮助页展示，mirai 的 `Command.usage`）
    pub usage: String,
    pub command_handler: Arc<dyn CommandHandler>,
    /// 所属分群功能开关 key（见 `Gating` 的功能清单）；空串表示不受分群开关限制
    pub feature: &'static str,
    /// 命令名撞上别家插件时，谁拿到这个名字
    pub priority: CommandPriority,
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
            usage: String::new(),
            command_handler,
            feature: "",
            priority: Priority::Normal,
        }
    }

    /// 绑定分群功能开关
    pub fn with_feature(mut self, feature: &'static str) -> CommandRegistration {
        self.feature = feature;
        self
    }

    /// 补充用法示例
    pub fn with_usage(mut self, usage: impl Into<String>) -> CommandRegistration {
        self.usage = usage.into();
        self
    }

    /// 指定命令名冲突时的胜出方
    pub fn with_priority(mut self, priority: CommandPriority) -> CommandRegistration {
        self.priority = priority;
        self
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

/// 表项：命令 + 归属插件 + 调度信息
struct CommandEntry {
    plugin: String,
    name: String,
    description: String,
    usage: String,
    feature: &'static str,
    priority: CommandPriority,
    handler: Arc<dyn CommandHandler>,
    /// 登记序号：同优先级下保持注册顺序稳定
    seq: u64,
}

struct FallbackEntry {
    plugin: String,
    priority: CommandPriority,
    handler: Arc<dyn FallbackHandler>,
    seq: u64,
}

/// 命令概览（帮助页/GUI/诊断用）
#[derive(Clone, Debug)]
pub struct CommandInfo {
    pub plugin: String,
    pub name: String,
    pub description: String,
    pub usage: String,
    pub feature: &'static str,
    pub priority: CommandPriority,
}

/// 命令表：多个插件的命令并存，按名字索引到一组候选
#[derive(Default)]
struct CommandTable {
    by_name: HashMap<String, Vec<CommandEntry>>,
    /// 所有表项的登记顺序，撤销某插件的命令时按它重排
    fallbacks: Vec<FallbackEntry>,
}

/// 一张命令表 + 判路由用的门控
pub struct CommandRegistry {
    table: RwLock<CommandTable>,
    /// 登记序号发号器（表内单调，不必进程级唯一）
    seq: AtomicU64,
    gating: Arc<Gating>,
}

impl CommandRegistry {
    pub(crate) fn new(gating: Arc<Gating>) -> CommandRegistry {
        CommandRegistry {
            table: RwLock::new(CommandTable::default()),
            seq: AtomicU64::new(0),
            gating,
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    /// 登记一条命令（归属插件由框架填好）。返回每个命令名的实际归属结果，
    /// 名字被别家占住时给出原因——调用方（插件）不必因此失败，框架会记日志。
    pub fn register(&self, plugin: &str, registration: CommandRegistration) -> Vec<String> {
        let CommandRegistration {
            names,
            description,
            usage,
            command_handler,
            feature,
            priority,
        } = registration;
        let mut problems: Vec<String> = Vec::new();
        let mut guard = self.table.write().unwrap();
        for name in names {
            let key = normalize(&name);
            if key.is_empty() {
                continue;
            }
            let candidates = guard.by_name.entry(key.clone()).or_default();
            let winner = CommandEntry {
                plugin: plugin.to_string(),
                name: key.clone(),
                description: description.clone(),
                usage: usage.clone(),
                feature,
                priority,
                handler: command_handler.clone(),
                seq: self.next_seq(),
            };
            // 同一家插件重复登记 = 重新装配，直接换掉它自己的旧实现
            if let Some(existing) = candidates
                .iter()
                .position(|entry| entry.plugin == winner.plugin)
            {
                candidates.remove(existing);
            }
            match candidates
                .iter()
                .position(|entry| entry.priority.order() <= winner.priority.order())
            {
                None => candidates.push(winner),
                Some(at) => {
                    let holder = &candidates[at];
                    if holder.priority.order() == winner.priority.order() {
                        problems.push(format!(
                            "命令「{key}」已由插件 {} 以相同优先级占用，本次登记被忽略",
                            holder.plugin
                        ));
                    } else {
                        candidates.push(winner);
                    }
                    candidates.sort_by_key(|entry| (entry.priority.order(), entry.seq));
                }
            }
        }
        drop(guard);
        problems
    }

    /// 登记一条兜底处理器（未命中任何命令时按优先级依次调用）
    pub fn register_fallback(
        &self,
        plugin: &str,
        handler: Arc<dyn FallbackHandler>,
        priority: CommandPriority,
    ) {
        let mut guard = self.table.write().unwrap();
        guard.fallbacks.retain(|entry| entry.plugin != plugin);
        guard.fallbacks.push(FallbackEntry {
            plugin: plugin.to_string(),
            priority,
            handler,
            seq: self.next_seq(),
        });
        guard
            .fallbacks
            .sort_by_key(|entry| (entry.priority.order(), entry.seq));
    }

    /// 撤销某个插件登记的全部命令与兜底（插件停用时由框架调用）
    pub fn unregister_plugin(&self, plugin: &str) {
        let mut guard = self.table.write().unwrap();
        guard.by_name.retain(|_, candidates| {
            candidates.retain(|entry| entry.plugin != plugin);
            !candidates.is_empty()
        });
        guard.fallbacks.retain(|entry| entry.plugin != plugin);
    }

    /// 某插件是否还占着命令（停用时用来确认已清空）
    pub fn has_commands_of(&self, plugin: &str) -> bool {
        let guard = self.table.read().unwrap();
        guard
            .by_name
            .values()
            .any(|candidates| candidates.iter().any(|entry| entry.plugin == plugin))
            || guard.fallbacks.iter().any(|entry| entry.plugin == plugin)
    }

    /// 全部命令概览（按插件、命令名排序）
    pub fn commands(&self) -> Vec<CommandInfo> {
        let mut list: Vec<CommandInfo> = self
            .table
            .read()
            .unwrap()
            .by_name
            .values()
            .flatten()
            .map(|entry| CommandInfo {
                plugin: entry.plugin.clone(),
                name: entry.name.clone(),
                description: entry.description.clone(),
                usage: entry.usage.clone(),
                feature: entry.feature,
                priority: entry.priority,
            })
            .collect();
        list.sort_by(|a, b| a.plugin.cmp(&b.plugin).then_with(|| a.name.cmp(&b.name)));
        list
    }

    /// 某个插件名下的命令概览
    pub fn commands_of(&self, plugin: &str) -> Vec<CommandInfo> {
        self.commands()
            .into_iter()
            .filter(|info| info.plugin == plugin)
            .collect()
    }

    /// 该命令此刻在这个会话里是否放行：归属插件未被停用（全局/该群），且该群没关掉这个功能
    fn entry_allowed(&self, entry: &CommandEntry, group_id: Option<i64>) -> bool {
        if !self.gating.plugin_enabled(&entry.plugin) {
            return false;
        }
        if !self.gating.plugin_enabled_in_group(&entry.plugin, group_id) {
            return false;
        }
        self.gating.feature_enabled(group_id, entry.feature)
    }

    /// 把一条文本投给命令表；命中并执行返回 true
    pub async fn dispatch(&self, context: Arc<CommandContext>) -> bool {
        let parts: Vec<&str> = context.text.trim().split_whitespace().collect();
        if parts.is_empty() {
            return false;
        }
        let key = normalize(parts[0]);
        let args: Vec<String> = parts[1..].iter().map(|part| part.to_string()).collect();

        // 先摘出候选再执行：处理器里可能回头登记/撤销命令，持着写锁回调会自死锁
        let candidates: Vec<Arc<dyn CommandHandler>> = {
            let guard = self.table.read().unwrap();
            match guard.by_name.get(&key) {
                Some(entries) => entries
                    .iter()
                    .filter(|entry| self.entry_allowed(entry, context.group_id))
                    .map(|entry| entry.handler.clone())
                    .collect(),
                None => Vec::new(),
            }
        };
        // 同名命令只交给排在最前的那家（表按优先级排过）：别家抢同一个命令名时不重复响应
        if let Some(candidate) = candidates.first() {
            candidate.handle(context.clone(), args.clone()).await;
            return true;
        }

        let fallbacks: Vec<Arc<dyn FallbackHandler>> = {
            let guard = self.table.read().unwrap();
            guard
                .fallbacks
                .iter()
                .filter(|entry| {
                    self.gating.plugin_enabled(&entry.plugin)
                        && self
                            .gating
                            .plugin_enabled_in_group(&entry.plugin, context.group_id)
                })
                .map(|entry| entry.handler.clone())
                .collect()
            // 兜底不判定命令是否命中：多个插件各自兜底时，谁都不该被别家的结果挡住
        };
        if fallbacks.is_empty() {
            return false;
        }
        for fallback in fallbacks {
            fallback.handle(context.clone()).await;
        }
        false
    }
}

fn global() -> &'static CommandRegistry {
    Framework::global().commands()
}

/// 登记一条命令（归属插件由框架填好），返回命令名占用冲突的原因列表
pub fn register(plugin: &str, registration: CommandRegistration) -> Vec<String> {
    global().register(plugin, registration)
}

/// 登记一条兜底处理器（未命中任何命令时按优先级依次调用）
pub fn register_fallback(
    plugin: &str,
    handler: Arc<dyn FallbackHandler>,
    priority: CommandPriority,
) {
    global().register_fallback(plugin, handler, priority);
}

/// 撤销某个插件登记的全部命令与兜底（插件停用时由框架调用）
pub fn unregister_plugin(plugin: &str) {
    global().unregister_plugin(plugin);
}

/// 某插件是否还占着命令（停用时用来确认已清空）
pub fn has_commands_of(plugin: &str) -> bool {
    global().has_commands_of(plugin)
}

/// 全部命令概览（按插件、命令名排序）
pub fn commands() -> Vec<CommandInfo> {
    global().commands()
}

/// 某个插件名下的命令概览
pub fn commands_of(plugin: &str) -> Vec<CommandInfo> {
    global().commands_of(plugin)
}

/// 命令分发器：持有它所属框架实例的命令表，因此插件装配顺序、启停状态变化
/// 都会立刻反映到路由结果上。`new()` 绑进程默认实例，`with_framework` 绑指定实例。
#[derive(Clone)]
pub struct CommandDispatcher {
    registry: Arc<CommandRegistry>,
}

impl CommandDispatcher {
    pub fn new() -> CommandDispatcher {
        CommandDispatcher {
            registry: global_arc(),
        }
    }

    /// 指定框架实例的分发器（测试/多实例宿主用）
    pub fn with_framework(framework: &Framework) -> CommandDispatcher {
        CommandDispatcher {
            registry: framework.commands().clone(),
        }
    }

    /// 直接持有一张命令表
    pub fn with_registry(registry: Arc<CommandRegistry>) -> CommandDispatcher {
        CommandDispatcher { registry }
    }

    /// 本分发器读的那张表
    pub fn registry(&self) -> &Arc<CommandRegistry> {
        &self.registry
    }

    /// 把一条文本投给命令表；命中并执行返回 true
    pub async fn dispatch(&self, context: Arc<CommandContext>) -> bool {
        self.registry.dispatch(context).await
    }
}

fn global_arc() -> Arc<CommandRegistry> {
    Framework::global().commands().clone()
}

impl Default for CommandDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

fn normalize(command: &str) -> String {
    command.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::message::{MessageReceipt, MessageSender};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct NullSender;

    impl MessageSender for NullSender {
        fn send<'a>(
            &'a self,
            _target: MessageTarget,
            _message: OutgoingMessage,
        ) -> BoxFuture<'a, MessageReceipt> {
            Box::pin(async { MessageReceipt { message_id: None } })
        }
    }

    fn context(text: &str, group_id: Option<i64>) -> Arc<CommandContext> {
        Arc::new(CommandContext {
            user_id: 100,
            group_id,
            text: text.to_string(),
            sender_name: None,
            is_admin: false,
            sender: Arc::new(NullSender),
        })
    }

    fn mark(flag: Arc<AtomicBool>) -> Arc<dyn CommandHandler> {
        handler(move |_ctx: Arc<CommandContext>, _args: Vec<String>| {
            let flag = flag.clone();
            async move {
                flag.store(true, Ordering::SeqCst);
                None
            }
        })
    }

    /// 回归 mirai 式 CommandManager 的核心诉求：两个插件登记同名命令不互相覆盖，
    /// 各自的命令都能路由，冲突的那个名字按优先级判给一家。
    #[tokio::test]
    async fn two_plugins_coexist_in_one_table() {
        let registry = CommandRegistry::new(Arc::new(Gating::default()));
        let first = "CommandTablePluginA";
        let second = "CommandTablePluginB";
        let hit_a = Arc::new(AtomicBool::new(false));
        let hit_b = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(AtomicBool::new(false));

        registry.register(
            first,
            CommandRegistration::new(vec!["/甲".to_string()], "甲功能", mark(hit_a.clone())),
        );
        registry.register(
            second,
            CommandRegistration::new(vec!["/乙".to_string()], "乙功能", mark(hit_b.clone())),
        );
        registry.register(
            first,
            CommandRegistration::new(vec!["/共用".to_string()], "甲的共用", mark(shared.clone())),
        );
        // 同名同优先级：后登记的应被忽略（返回原因），先登记的继续生效
        let problems = registry.register(
            second,
            CommandRegistration::new(vec!["/共用".to_string()], "乙的共用", mark(shared.clone())),
        );
        assert!(
            problems.iter().any(|text| text.contains("共用")),
            "同名冲突应回报原因: {problems:?}"
        );

        assert!(registry.dispatch(context("/甲", Some(1))).await);
        assert!(registry.dispatch(context("/乙", Some(1))).await);
        assert!(
            hit_a.load(Ordering::SeqCst) && hit_b.load(Ordering::SeqCst),
            "两家命令都应命中"
        );
        assert!(registry.dispatch(context("/共用", Some(1))).await);

        registry.unregister_plugin(first);
        registry.unregister_plugin(second);
        assert!(!registry.has_commands_of(first) && !registry.has_commands_of(second));
    }

    /// 停用插件只摘掉自己的命令，别家不受影响；一切都在实例内发生，不碰进程默认实例
    #[tokio::test]
    async fn disabling_one_plugin_keeps_the_other() {
        let framework = Framework::new();
        let first = "CommandGatePluginA";
        let second = "CommandGatePluginB";
        let flag_a = Arc::new(AtomicBool::new(false));
        let flag_b = Arc::new(AtomicBool::new(false));
        framework.commands().register(
            first,
            CommandRegistration::new(vec!["/停我".to_string()], "甲", mark(flag_a.clone())),
        );
        framework.commands().register(
            second,
            CommandRegistration::new(vec!["/留我".to_string()], "乙", mark(flag_b.clone())),
        );
        framework
            .gating()
            .set_disabled_plugins(vec![first.to_string()]);

        let dispatcher = CommandDispatcher::with_framework(&framework);
        assert!(
            !dispatcher.dispatch(context("/停我", None)).await,
            "已停用的插件命令不该路由"
        );
        assert!(!flag_a.load(Ordering::SeqCst));
        assert!(
            dispatcher.dispatch(context("/留我", None)).await,
            "另一家插件应照常工作"
        );
        assert!(flag_b.load(Ordering::SeqCst));

        // 进程默认实例那张表既没有这些命令，也不因上面的停用名单变化而受影响
        let global_dispatcher = CommandDispatcher::new();
        assert!(
            !global_dispatcher.dispatch(context("/停我", None)).await,
            "隔离实例登记的命令不该出现在默认实例里"
        );
        framework.gating().set_disabled_plugins(Vec::new());
        framework.commands().unregister_plugin(first);
        framework.commands().unregister_plugin(second);
    }
}
