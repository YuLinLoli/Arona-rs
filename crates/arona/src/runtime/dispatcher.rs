//! 命令上下文、命令注册表与分发（对应 mirai 的 `CommandManager` + `SimpleCommandDispatcher`）
//!
//! 框架侧只有**一张命令表 per 框架实例**：每个插件把自己的命令登记进来，表项记住归属插件 id。
//! 分发时逐个候选查门控（该插件全局/该群是否停用、该群是否关掉这个功能），
//! 因此多个插件可以并存，命令冲突按 [`CommandPriority`] 决定归属，
//! 停用某个插件只影响它名下的命令——不需要插件自己配合。
//!
//! 表本身是 [`CommandRegistry`] 实例，由 [`crate::framework::Framework`] 持有：
//! 进程默认实例走本模块的自由函数，测试用 `Framework::new()` 得到干净的一张表。
//!
//! 分发骨架对齐 mirai 的 `SimpleCommand`：
//! - 参数可以声明类型（[`crate::runtime::args`]），框架负责切分/转换/校验并回显用法；
//! - 命令可要求群身份（[`Permission`]），普通成员的 `/禁言` 不会走到插件代码里；
//! - 命令名可开最短前缀匹配（`/ban` 能命中 `/banuser`），歧义时回显候选；
//! - 处理器 panic 只废掉这一次调用，并记在归属插件名下（见 [`crate::plugin::health`]）。
use super::args::{self, ArgSpec, Args};
use super::message::{BoxFuture, MessageReceipt, MessageSender, MessageTarget, OutgoingMessage};
use super::priority::{CommandPriority, Priority};
use crate::framework::Framework;
use crate::plugin::health::HealthBoard;
use crate::runtime::config::Gating;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// 发送者在群里的身份（mirai 的 `MemberPermission`）。
/// 排序即高低：[`GroupRole::Member`] < [`GroupRole::Admin`] < [`GroupRole::Owner`]。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum GroupRole {
    #[default]
    Member,
    Admin,
    Owner,
}

impl GroupRole {
    /// 兼容 OneBot v11 的字符串写法（owner/admin/member）与 NTQQ 系实现端的数字（1 群主 / 2 管理员 / 3 成员）
    pub fn parse(raw: &str) -> Option<GroupRole> {
        match raw.trim().to_lowercase().as_str() {
            "owner" | "creator" | "群主" | "1" => Some(GroupRole::Owner),
            "admin" | "administrator" | "管理" | "2" => Some(GroupRole::Admin),
            "member" | "普通" | "3" => Some(GroupRole::Member),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            GroupRole::Member => "成员",
            GroupRole::Admin => "管理员",
            GroupRole::Owner => "群主",
        }
    }

    /// 从消息事件的 `sender` 对象读身份：`role` 是字符串（OneBot v11）还是数字（NTQQ 系实现端）都认
    pub fn from_sender(sender: &serde_json::Value) -> Option<GroupRole> {
        let raw = sender.get("role")?;
        match raw {
            serde_json::Value::String(text) => GroupRole::parse(text),
            serde_json::Value::Number(number) => GroupRole::parse(&number.to_string()),
            _ => None,
        }
    }
}

/// 命令要求的最低群身份（mirai 的 `Permission`）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Permission {
    /// 任何人（默认）
    #[default]
    Anyone,
    /// 群管理员或群主；私聊里没有群身份，只有框架管理员可用
    GroupAdmin,
    /// 仅群主
    GroupOwner,
}

impl Permission {
    /// 是否放行。框架管理员（`arona.yml` 的管理员名单）一律高于群身份。
    pub fn check(self, is_framework_admin: bool, role: Option<GroupRole>) -> bool {
        if is_framework_admin {
            return true;
        }
        match self {
            Permission::Anyone => true,
            Permission::GroupAdmin => role.is_some_and(|role| role >= GroupRole::Admin),
            Permission::GroupOwner => role == Some(GroupRole::Owner),
        }
    }

    /// 拒绝时回给用户的一句话
    pub fn deny_message(self) -> &'static str {
        match self {
            Permission::Anyone => "",
            Permission::GroupAdmin => "该命令需要群管理员（或群主）身份",
            Permission::GroupOwner => "该命令只有群主可以执行",
        }
    }
}

/// 命令上下文
pub struct CommandContext {
    pub user_id: i64,
    pub group_id: Option<i64>,
    /// 用于命令匹配的文本（群消息里已剥掉"@机器人 "前缀）
    pub text: String,
    pub sender_name: Option<String>,
    /// 框架管理员名单（`arona.yml` 的 managers）
    pub is_admin: bool,
    /// 事件里自带的群身份；私聊或实现端没给 sender.role 时为 None
    pub sender_role: Option<GroupRole>,
    pub sender: Arc<dyn MessageSender>,
}

/// 回查群身份的缓存有效期：同一人连发命令时不必每次都问实现端
const ROLE_CACHE_TTL: Duration = Duration::from_secs(60);

/// 身份回查结果缓存（(群号, QQ 号) -> (身份, 时刻)）
type RoleCacheKey = (i64, i64);
type RoleCacheEntry = (GroupRole, Instant);

fn role_cache() -> &'static Mutex<HashMap<RoleCacheKey, RoleCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<RoleCacheKey, RoleCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
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

    /// 群内身份：先读事件自带的 `sender.role`，没有再回查 `get_group_member_info`（缓存 60 秒）。
    /// 私聊、无连接或实现端不支持时返回 None——按"不满足"处理。
    pub async fn group_role(&self) -> Option<GroupRole> {
        if let Some(role) = self.sender_role {
            return Some(role);
        }
        let group_id = self.group_id?;
        let key = (group_id, self.user_id);
        if let Some((role, at)) = role_cache().lock().ok()?.get(&key).copied() {
            if at.elapsed() < ROLE_CACHE_TTL {
                return Some(role);
            }
        }
        let api = crate::onebot::api::OneBotApi::global();
        let wait = api.get_group_member_info(group_id, self.user_id, false);
        let info = match tokio::time::timeout(Duration::from_secs(3), wait).await {
            Ok(Ok(info)) => info,
            _ => return None,
        };
        let role = if info.is_owner() {
            GroupRole::Owner
        } else if info.is_admin() {
            GroupRole::Admin
        } else {
            GroupRole::Member
        };
        if let Ok(mut cache) = role_cache().lock() {
            cache.insert(key, (role, Instant::now()));
        }
        Some(role)
    }
}

/// 命令处理器（原始词表版）
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

/// 类型化命令处理器：拿到的已经是按声明解析、校验过的参数（见 [`crate::runtime::args`]）
pub trait TypedCommandHandler: Send + Sync {
    fn handle<'a>(
        &'a self,
        context: Arc<CommandContext>,
        arguments: Args,
    ) -> BoxFuture<'a, Option<OutgoingMessage>>;
}

pub struct FnTypedCommandHandler<F> {
    inner: F,
}

impl<F, Fut> FnTypedCommandHandler<F>
where
    F: Fn(Arc<CommandContext>, Args) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Option<OutgoingMessage>> + Send,
{
    pub fn new(inner: F) -> Self {
        FnTypedCommandHandler { inner }
    }
}

impl<F, Fut> TypedCommandHandler for FnTypedCommandHandler<F>
where
    F: Fn(Arc<CommandContext>, Args) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Option<OutgoingMessage>> + Send,
{
    fn handle<'a>(
        &'a self,
        context: Arc<CommandContext>,
        arguments: Args,
    ) -> BoxFuture<'a, Option<OutgoingMessage>> {
        Box::pin(async move { (self.inner)(context, arguments).await })
    }
}

pub fn typed_handler<F, Fut>(f: F) -> Arc<dyn TypedCommandHandler>
where
    F: Fn(Arc<CommandContext>, Args) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Option<OutgoingMessage>> + Send + 'static,
{
    Arc::new(FnTypedCommandHandler::new(f))
}

/// 表项实际持有的处理器形态
#[derive(Clone)]
enum CommandBody {
    Raw(Arc<dyn CommandHandler>),
    Typed(Arc<dyn TypedCommandHandler>),
}

/// 命令注册信息（插件侧只描述命令本身，归属插件由框架在登记时补上）
pub struct CommandRegistration {
    pub names: Vec<String>,
    pub description: String,
    /// 用法示例（帮助页展示，mirai 的 `Command.usage`）；留空且声明了参数时由声明自动生成
    pub usage: String,
    /// 所属分群功能开关 key（见 `Gating` 的功能清单）；空串表示不受分群开关限制
    pub feature: &'static str,
    /// 命令名撞上别家插件时，谁拿到这个名字
    pub priority: CommandPriority,
    /// 执行本命令所需的最低群身份
    pub permission: Permission,
    /// 参数声明；非空时 [`CommandRegistration::typed`] 的处理器拿到解析后的 [`Args`]
    pub args: Vec<ArgSpec>,
    /// 是否允许最短前缀匹配（None 表示跟随框架选项）
    pub prefix_match: Option<bool>,
    body: CommandBody,
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
            feature: "",
            priority: Priority::Normal,
            permission: Permission::Anyone,
            args: Vec::new(),
            prefix_match: None,
            body: CommandBody::Raw(command_handler),
        }
    }

    /// 声明了参数的命令：处理器拿到 [`Args`]，切分/转换/校验与用法回显都由框架完成
    pub fn typed(
        names: Vec<String>,
        description: impl Into<String>,
        command_handler: Arc<dyn TypedCommandHandler>,
    ) -> CommandRegistration {
        CommandRegistration {
            names,
            description: description.into(),
            usage: String::new(),
            feature: "",
            priority: Priority::Normal,
            permission: Permission::Anyone,
            args: Vec::new(),
            prefix_match: None,
            body: CommandBody::Typed(command_handler),
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

    /// 声明参数类型（与 [`CommandRegistration::typed`] 搭配）
    pub fn with_args(mut self, specs: Vec<ArgSpec>) -> CommandRegistration {
        self.args = specs;
        self
    }

    /// 要求群身份（mirai 的 `command { permission = Permission.Administrator }`）
    pub fn with_permission(mut self, permission: Permission) -> CommandRegistration {
        self.permission = permission;
        self
    }

    /// 本命令是否参与最短前缀匹配（不设则跟随 `FrameworkOptions::prefix_match_by_default`）
    pub fn with_prefix_match(mut self, enabled: bool) -> CommandRegistration {
        self.prefix_match = Some(enabled);
        self
    }

    /// 帮助页/诊断展示的一行用法
    pub fn usage_line(&self) -> String {
        if !self.usage.is_empty() {
            return self.usage.clone();
        }
        args::usage(&self.names, &self.args)
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
    permission: Permission,
    args: Vec<ArgSpec>,
    prefix_match: bool,
    body: CommandBody,
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
    pub permission: Permission,
}

/// 命令表：多个插件的命令并存，按名字索引到一组候选
#[derive(Default)]
struct CommandTable {
    by_name: HashMap<String, Vec<Arc<CommandEntry>>>,
    /// 所有表项的登记顺序，撤销某插件的命令时按它重排
    fallbacks: Vec<FallbackEntry>,
}

/// 一张命令表 + 判路由用的门控 + panic 记账
pub struct CommandRegistry {
    table: RwLock<CommandTable>,
    /// 登记序号发号器（表内单调，不必进程级唯一）
    seq: AtomicU64,
    gating: Arc<Gating>,
    health: Arc<HealthBoard>,
    /// 未声明 `with_prefix_match` 的命令是否也允许最短前缀匹配
    prefix_match_by_default: bool,
}

impl CommandRegistry {
    pub(crate) fn new(
        gating: Arc<Gating>,
        health: Arc<HealthBoard>,
        prefix_match_by_default: bool,
    ) -> CommandRegistry {
        CommandRegistry {
            table: RwLock::new(CommandTable::default()),
            seq: AtomicU64::new(0),
            gating,
            health,
            prefix_match_by_default,
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
            feature,
            priority,
            permission,
            args,
            prefix_match,
            body,
        } = registration;
        let mut problems: Vec<String> = Vec::new();
        if !args.is_empty() && matches!(body, CommandBody::Raw(_)) {
            problems.push(format!(
                "命令 {:?} 声明了参数但用的是原始处理器，参数校验不会生效（改用 CommandRegistration::typed）",
                names.first()
            ));
        }
        let usage = if usage.is_empty() {
            args::usage(&names, &args)
        } else {
            usage
        };
        let prefix_match = prefix_match.unwrap_or(self.prefix_match_by_default);
        let mut guard = self.table.write().unwrap();
        for name in names {
            let key = normalize(&name);
            if key.is_empty() {
                continue;
            }
            let candidates = guard.by_name.entry(key.clone()).or_default();
            let winner = Arc::new(CommandEntry {
                plugin: plugin.to_string(),
                name: key.clone(),
                description: description.clone(),
                usage: usage.clone(),
                feature,
                priority,
                permission,
                args: args.clone(),
                prefix_match,
                body: body.clone(),
                seq: self.next_seq(),
            });
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
                permission: entry.permission,
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

    /// 这个名字此刻由哪家插件拿到（同名候选按优先级排过，只认第一名）
    fn exact(&self, key: &str, group_id: Option<i64>) -> Option<Arc<CommandEntry>> {
        let guard = self.table.read().unwrap();
        let entries = guard.by_name.get(key)?;
        entries
            .iter()
            .find(|entry| self.entry_allowed(entry, group_id))
            .cloned()
    }

    /// 最短前缀匹配（mirai-console 的 `shortestPrefixMatch`）：
    /// 返回按名字去重后的候选，调用方据个数决定"命中"还是"回显歧义"。
    fn by_prefix(&self, key: &str, group_id: Option<i64>) -> Vec<Arc<CommandEntry>> {
        let guard = self.table.read().unwrap();
        let mut names: Vec<String> = Vec::new();
        let mut hits: Vec<Arc<CommandEntry>> = Vec::new();
        let mut entries: Vec<&String> = guard.by_name.keys().collect();
        entries.sort();
        for name in entries {
            if name == key || !name.starts_with(key) {
                continue;
            }
            let Some(candidates) = guard.by_name.get(name) else {
                continue;
            };
            let Some(entry) = candidates
                .iter()
                .find(|entry| entry.prefix_match && self.entry_allowed(entry, group_id))
            else {
                continue;
            };
            if names.contains(&entry.name) {
                continue;
            }
            names.push(entry.name.clone());
            hits.push(entry.clone());
        }
        hits
    }

    /// 把一条文本投给命令表；命中并执行返回 true
    pub async fn dispatch(&self, context: Arc<CommandContext>) -> bool {
        let mut tokens = context.text.split_whitespace();
        let Some(head) = tokens.next() else {
            return false;
        };
        let key = normalize(head);
        let raw: Vec<String> = tokens.map(str::to_string).collect();

        // 先摘出表项再执行：处理器里可能回头登记/撤销命令，持着读锁回调会自死锁
        let chosen = match self.exact(&key, context.group_id) {
            Some(entry) => Some(entry),
            // 只打了 `/` 时前缀匹配会命中全部命令，没有意义，所以要求至少两个字符
            None if key.chars().count() > 1 => match self.by_prefix(&key, context.group_id) {
                hits if hits.is_empty() => None,
                hits if hits.len() == 1 => hits.into_iter().next(),
                hits => {
                    // 歧义：把候选念给用户，算已经回应了（mirai 的 MultipleCommandMatchesException）
                    let list: Vec<String> = hits.iter().map(|entry| entry.name.clone()).collect();
                    let _ = context
                        .reply(format!(
                            "存在多个候选命令：{}，请输入更完整的前缀",
                            list.join(" ")
                        ))
                        .await;
                    return true;
                }
            },
            None => None,
        };
        let Some(entry) = chosen else {
            return self.run_fallbacks(context).await;
        };

        if entry.permission != Permission::Anyone
            && !entry
                .permission
                .check(context.is_admin, context.group_role().await)
        {
            let _ = context.reply(entry.permission.deny_message()).await;
            return true;
        }

        let outcome: Option<Option<OutgoingMessage>> = match &entry.body {
            CommandBody::Raw(command) => {
                crate::plugin::health::guarded(command.handle(context.clone(), raw)).await
            }
            CommandBody::Typed(command) => match args::parse(&entry.args, &raw) {
                Ok(parsed) => {
                    crate::plugin::health::guarded(command.handle(context.clone(), parsed)).await
                }
                Err(problem) => {
                    // 参数不对：回显用法 + 一句原因，不进插件代码（mirai 的 ArgException）
                    let _ = context
                        .reply(format!("{} ({})", entry.usage, problem.describe()))
                        .await;
                    return true;
                }
            },
        };
        match outcome {
            Some(message) => {
                self.health.record_success(&entry.plugin);
                // 处理器直接返回消息 = 让框架代发（mirai 的 `SimpleCommand` 回 String）
                if let Some(message) = message {
                    context.reply_message(message).await;
                }
            }
            None => {
                self.health.record_panic(&entry.plugin, "命令处理器");
            }
        }
        true
    }

    /// 未命中任何命令：按优先级依次调用各家登记的兜底
    async fn run_fallbacks(&self, context: Arc<CommandContext>) -> bool {
        let fallbacks: Vec<(String, Arc<dyn FallbackHandler>)> = {
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
                .map(|entry| (entry.plugin.clone(), entry.handler.clone()))
                .collect()
            // 兜底不判定命令是否命中：多个插件各自兜底时，谁都不该被别家的结果挡住
        };
        if fallbacks.is_empty() {
            return false;
        }
        for (plugin, fallback) in fallbacks {
            if crate::plugin::health::guarded(fallback.handle(context.clone()))
                .await
                .is_none()
            {
                self.health.record_panic(&plugin, "命令兜底");
            } else {
                self.health.record_success(&plugin);
            }
        }
        true
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
    use crate::framework::FrameworkOptions;
    use crate::runtime::args::arg;
    use crate::runtime::message::MessageSegment;
    use std::sync::atomic::AtomicBool;

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
            sender_role: None,
            sender: Arc::new(NullSender),
        })
    }

    fn health(threshold: u32, gating: &Arc<Gating>) -> Arc<HealthBoard> {
        Arc::new(HealthBoard::new(threshold, gating.clone()))
    }

    /// 一张独立的表：门控与健康度共用同一个 Gating，停用才真的能生效
    fn registry() -> CommandRegistry {
        let gating = Arc::new(Gating::default());
        CommandRegistry::new(
            gating.clone(),
            health(Framework::DEFAULT_PANIC_THRESHOLD, &gating),
            false,
        )
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
        let registry = registry();
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

    /// 声明式参数：类型转换、缺省值与用法回显都在框架侧完成
    #[tokio::test]
    async fn typed_arguments_are_parsed_by_the_framework() {
        let registry = registry();
        let seen = Arc::new(Mutex::new(String::new()));
        let recorder = seen.clone();
        registry.register(
            "TypedArgsPlugin",
            CommandRegistration::typed(
                vec!["/禁言".to_string()],
                "禁言某人",
                typed_handler(move |_ctx: Arc<CommandContext>, args: Args| {
                    let recorder = recorder.clone();
                    async move {
                        *recorder.lock().unwrap() = format!(
                            "{}|{}|{}",
                            args.i64("群号").unwrap_or_default(),
                            args.i64("分钟").unwrap_or_default(),
                            args.text("原因").unwrap_or("无")
                        );
                        None
                    }
                }),
            )
            .with_args(vec![
                arg::i64("群号"),
                arg::i64("分钟").with_default("10").range(1, 1440),
                arg::rest("原因").optional(),
            ]),
        );

        assert!(
            registry
                .dispatch(context("/禁言 42 30 骚扰", Some(1)))
                .await
        );
        assert_eq!(*seen.lock().unwrap(), "42|30|骚扰");
        // 缺省值：只给群号时分钟按声明取 10
        assert!(registry.dispatch(context("/禁言 43", Some(1))).await);
        assert_eq!(*seen.lock().unwrap(), "43|10|无");
        // 必填缺失：不进处理器，直接回用法
        *seen.lock().unwrap() = "不该被改".to_string();
        assert!(registry.dispatch(context("/禁言", Some(1))).await);
        assert_eq!(*seen.lock().unwrap(), "不该被改");
        // 类型不对也一样挡在门外
        assert!(registry.dispatch(context("/禁言 abc", Some(1))).await);
        assert_eq!(*seen.lock().unwrap(), "不该被改");
        registry.unregister_plugin("TypedArgsPlugin");
    }

    /// 最短前缀匹配：唯一命中才执行，多个候选时回显歧义
    #[tokio::test]
    async fn prefix_match_resolves_only_when_unique() {
        let registry = registry();
        let ban = Arc::new(AtomicBool::new(false));
        let buy = Arc::new(AtomicBool::new(false));
        registry.register(
            "PrefixPlugin",
            CommandRegistration::new(vec!["/banuser".to_string()], "封禁", mark(ban.clone()))
                .with_prefix_match(true),
        );
        registry.register(
            "PrefixPlugin2",
            CommandRegistration::new(vec!["/buy".to_string()], "购买", mark(buy.clone()))
                .with_prefix_match(true),
        );
        assert!(registry.dispatch(context("/ban", Some(1))).await);
        assert!(ban.load(Ordering::SeqCst), "唯一前缀应命中");
        assert!(!buy.load(Ordering::SeqCst), "前缀不该串到别的命令");
        // /b 同时是 /banuser 与 /buy 的前缀：歧义，谁都不执行
        assert!(registry.dispatch(context("/b", Some(1))).await);
        assert!(!buy.load(Ordering::SeqCst), "歧义时不该猜一家");
        // 没开前缀匹配的命令不参与
        registry.register(
            "PrefixPlugin3",
            CommandRegistration::new(vec!["/备份".to_string()], "备份", mark(buy.clone())),
        );
        assert!(
            !registry.dispatch(context("/备", Some(1))).await,
            "未声明前缀匹配的命令不该被 /备 命中"
        );
        for plugin in ["PrefixPlugin", "PrefixPlugin2", "PrefixPlugin3"] {
            registry.unregister_plugin(plugin);
        }
    }

    /// 框架选项可以全局打开前缀匹配
    #[tokio::test]
    async fn framework_option_enables_prefix_globally() {
        let framework = Framework::with_options(FrameworkOptions {
            prefix_match_by_default: true,
            ..FrameworkOptions::default()
        });
        let hit = Arc::new(AtomicBool::new(false));
        framework.commands().register(
            "GlobalPrefixPlugin",
            CommandRegistration::new(vec!["/帮助大全".to_string()], "帮助", mark(hit.clone())),
        );
        assert!(
            framework
                .commands()
                .dispatch(context("/帮助", Some(1)))
                .await
        );
        assert!(hit.load(Ordering::SeqCst));
    }

    /// 群身份门控：普通成员被框架挡在处理器之外，群主/管理员放行
    #[tokio::test]
    async fn permission_blocks_before_the_handler_runs() {
        let registry = registry();
        let hit = Arc::new(AtomicBool::new(false));
        registry.register(
            "PermissionPlugin",
            CommandRegistration::new(vec!["/踢人".to_string()], "踢人", mark(hit.clone()))
                .with_permission(Permission::GroupAdmin),
        );
        let plain = context("/踢人", Some(1));
        assert!(registry.dispatch(plain.clone()).await);
        assert!(!hit.load(Ordering::SeqCst), "私聊/无身份时不该放行");

        let mut member = context("/踢人", Some(1));
        Arc::get_mut(&mut member).unwrap().sender_role = Some(GroupRole::Member);
        assert!(registry.dispatch(member).await);
        assert!(!hit.load(Ordering::SeqCst), "普通成员不该放行");

        let mut admin = context("/踢人", Some(1));
        Arc::get_mut(&mut admin).unwrap().sender_role = Some(GroupRole::Admin);
        assert!(registry.dispatch(admin).await);
        assert!(hit.load(Ordering::SeqCst), "群管理员应放行");
        registry.unregister_plugin("PermissionPlugin");
    }

    /// 群主命令：管理员也不行；框架管理员名单可以越过一切
    #[tokio::test]
    async fn owner_only_and_framework_admin() {
        let registry = registry();
        let hit = Arc::new(AtomicBool::new(false));
        registry.register(
            "OwnerPlugin",
            CommandRegistration::new(vec!["/解散".to_string()], "解散", mark(hit.clone()))
                .with_permission(Permission::GroupOwner),
        );
        let mut admin = context("/解散", Some(1));
        Arc::get_mut(&mut admin).unwrap().sender_role = Some(GroupRole::Admin);
        assert!(registry.dispatch(admin).await);
        assert!(!hit.load(Ordering::SeqCst));

        let mut owner = context("/解散", Some(1));
        Arc::get_mut(&mut owner).unwrap().sender_role = Some(GroupRole::Owner);
        assert!(registry.dispatch(owner).await);
        assert!(hit.load(Ordering::SeqCst));

        hit.store(false, Ordering::SeqCst);
        let mut framework_admin = context("/解散", Some(1));
        {
            let mutable = Arc::get_mut(&mut framework_admin).unwrap();
            mutable.sender_role = Some(GroupRole::Member);
            mutable.is_admin = true;
        }
        assert!(registry.dispatch(framework_admin).await);
        assert!(hit.load(Ordering::SeqCst), "框架管理员应越过群身份要求");
        registry.unregister_plugin("OwnerPlugin");
    }

    /// 处理器 panic 只废掉本次调用，记账在归属插件名下；到阈值就停用
    #[tokio::test]
    async fn panic_is_isolated_and_counted() {
        let gating = Arc::new(Gating::default());
        let health = health(2, &gating);
        let registry = CommandRegistry::new(gating.clone(), health.clone(), false);
        registry.register(
            "PanicPlugin",
            CommandRegistration::new(
                vec!["/炸".to_string()],
                "必炸",
                handler(|_ctx: Arc<CommandContext>, _args: Vec<String>| async move {
                    panic!("插件里的炸弹")
                }),
            ),
        );
        assert!(registry.dispatch(context("/炸", Some(1))).await);
        assert_eq!(health.failures("PanicPlugin"), 1);
        // 第 2 次到阈值：本次仍然只废掉这一次调用
        assert!(registry.dispatch(context("/炸", Some(1))).await);
        assert!(!gating.plugin_enabled("PanicPlugin"), "达阈值应停用该插件");
        // 之后命令不再路由到它
        assert!(!registry.dispatch(context("/炸", Some(1))).await);
        registry.unregister_plugin("PanicPlugin");
    }

    /// 处理器直接返回消息时由框架代发（mirai 的 `SimpleCommand` 回 String）
    #[tokio::test]
    async fn returned_message_is_sent_by_the_framework() {
        #[derive(Default)]
        struct EchoSender {
            seen: Arc<Mutex<Vec<String>>>,
        }
        impl MessageSender for EchoSender {
            fn send<'a>(
                &'a self,
                _target: MessageTarget,
                message: OutgoingMessage,
            ) -> BoxFuture<'a, MessageReceipt> {
                let seen = &self.seen;
                Box::pin(async move {
                    let text: String = message
                        .segments
                        .iter()
                        .filter_map(|segment| match segment {
                            MessageSegment::Text(value) => Some(value.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    seen.lock().unwrap().push(text);
                    MessageReceipt { message_id: None }
                })
            }
        }
        let sender = Arc::new(EchoSender::default());
        let seen = sender.seen.clone();
        let registry = registry();
        registry.register(
            "ReturnPlugin",
            CommandRegistration::new(
                vec!["/ping".to_string()],
                "回 pong",
                handler(|_ctx: Arc<CommandContext>, _args: Vec<String>| async move {
                    Some(OutgoingMessage::text("pong"))
                }),
            ),
        );
        let context = Arc::new(CommandContext {
            user_id: 1,
            group_id: Some(2),
            text: "/ping".to_string(),
            sender_name: None,
            is_admin: false,
            sender_role: None,
            sender,
        });
        assert!(registry.dispatch(context).await);
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["pong".to_string()],
            "处理器返回的消息应由框架发给会话"
        );
        registry.unregister_plugin("ReturnPlugin");
    }
}
