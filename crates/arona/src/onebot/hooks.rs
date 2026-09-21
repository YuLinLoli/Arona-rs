//! 插件事件钩子：让插件听到 OneBot 的**全部**事件，而不是只等命令分发叫到名字。
//!
//! 框架原有的插件入口是「注册命令」——只有文本命中 `/抽卡` 这类前缀才会进插件。
//! 但很多功能要的是别的东西：成员进群打招呼、被踢后清理数据、有人申请加群、
//! 群名被改、别人引用了机器人的消息……这些都属于 notice/request/meta 事件，
//! 或者属于「命中命令之前先被看一眼」的消息事件。这里给出统一的订阅入口：
//!
//! ```ignore
//! // 插件 configure 阶段订阅（推荐入口：ctx 自带插件归属，门控按它匹配 disabled_plugins）
//! ctx.listen(
//!     &[arona::onebot::EventKind::Message],
//!     arona::onebot::hooks::ListenerPriority::Normal,
//!     arona::onebot::hooks::event_handler(|ctx| {
//!         Box::pin(async move {
//!             if ctx.text.contains("早安") {
//!                 let _ = ctx.reply("老师早安！").await;
//!                 return arona::onebot::HookFlow::Handled; // 已处理，别再走命令分发
//!             }
//!             arona::onebot::HookFlow::Pass
//!         })
//!     }),
//! );
//!
//! // 拿不到 ctx 时的等价底层入口（第一个参数是插件 id）
//! arona::onebot::hooks::on_message("bluearchive", handler);
//! ```
//!
//! 门控语义（框架负责，插件不用自己判断）：
//! - `message`：先过「群授权 + 全局/群内黑名单」，再进钩子，最后才是命令分发；
//! - `notice` / `request` / `meta`：无条件投递（机器人被踢、有人申请加群这类事，
//!   黑名单用户也会触发，插件自己要清楚这一点）；
//! - 钩子先按 [`ListenerPriority`]（Monitor -> Normal -> High -> Low -> Lowest）、
//!   同优先级按注册顺序执行，任何一条返回 [`HookFlow::Handled`] 就停止后续钩子，
//!   并且不再走命令分发（等价于 mirai 的 `event.intercept()`）；
//! - 所属插件被禁用时（全局或该群），它的钩子一律跳过。
use crate::onebot::api::OneBotApi;
use crate::onebot::model::OneBotEvent;
use crate::onebot::protocol;
use crate::runtime::config::Gating;
use crate::runtime::message::{BoxFuture, MessageSegment, MessageTarget, OutgoingMessage};
pub use crate::runtime::priority::ListenerPriority;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// 事件大类（对应 OneBot 的 `post_type`）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// 群/私聊消息
    Message,
    /// 群成员增减、撤回、群名变更等通知
    Notice,
    /// 加群/加好友请求
    Request,
    /// 心跳与生命周期
    Meta,
}

impl EventKind {
    fn from_post_type(post_type: &str) -> Option<EventKind> {
        match post_type {
            "message" => Some(EventKind::Message),
            "notice" => Some(EventKind::Notice),
            "request" => Some(EventKind::Request),
            "meta_event" | "meta" => Some(EventKind::Meta),
            _ => None,
        }
    }

    /// 中文名（GUI 展示用）
    pub fn display_name(self) -> &'static str {
        match self {
            EventKind::Message => "消息",
            EventKind::Notice => "通知",
            EventKind::Request => "请求",
            EventKind::Meta => "元事件",
        }
    }
}

/// 投递给钩子的上下文
pub struct EventContext {
    pub kind: EventKind,
    /// 原始事件（`raw` 里能取到 notice_type / request_type 等细节字段）
    pub event: OneBotEvent,
    /// 消息纯文本（非消息事件为空串）
    pub text: String,
    /// 消息段（非消息事件为空）
    pub segments: Vec<MessageSegment>,
    /// OneBot 动作出口：回复、撤回、查资料都走它
    pub api: OneBotApi,
}

impl EventContext {
    pub fn user_id(&self) -> i64 {
        self.event.user_id.unwrap_or_default()
    }

    pub fn group_id(&self) -> Option<i64> {
        self.event.group_id
    }

    pub fn is_group(&self) -> bool {
        self.event.message_type.as_deref() == Some("group")
    }

    pub fn is_private(&self) -> bool {
        self.event.message_type.as_deref() == Some("private")
    }

    pub fn message_id(&self) -> Option<i64> {
        self.event.message_id
    }

    /// notice_type / request_type / meta_event_type / sub_type 等字符串字段
    pub fn field_str(&self, key: &str) -> Option<&str> {
        self.event
            .raw
            .get(key)
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
    }

    /// 某个 notice/request/meta 的原始字段（例如 flag、target_id）
    pub fn field(&self, key: &str) -> Option<&serde_json::Value> {
        self.event.raw.get(key)
    }

    /// 发送目标的回退顺序：群 -> 私聊发送者 -> None（meta 事件没有可回复对象）
    pub fn target(&self) -> Option<MessageTarget> {
        match self.group_id() {
            Some(group_id) if self.kind == EventKind::Message || self.is_group() => {
                Some(MessageTarget::Group(group_id))
            }
            _ => match self.event.user_id {
                Some(user_id) if self.kind == EventKind::Message => {
                    Some(MessageTarget::Private(user_id))
                }
                _ => None,
            },
        }
    }

    /// 回到事件来源（群或私聊发送者）；meta 事件没有来源时返回 None
    pub async fn reply_message(
        &self,
        message: OutgoingMessage,
    ) -> Result<Option<i64>, crate::onebot::api::OneBotError> {
        let Some(target) = self.target() else {
            return Ok(None);
        };
        Ok(Some(self.api.send(target, message).await?))
    }

    pub async fn reply(
        &self,
        text: impl Into<String>,
    ) -> Result<Option<i64>, crate::onebot::api::OneBotError> {
        self.reply_message(OutgoingMessage::text(text)).await
    }

    /// 撤回指定消息（自己发的，或有管理权限时撤回别人的）
    pub async fn recall(&self, message_id: i64) -> Result<(), crate::onebot::api::OneBotError> {
        self.api.delete_msg(message_id).await
    }
}

/// 钩子对事件的处置
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookFlow {
    /// 不处理，交给下一个钩子（消息事件最后还会走命令分发）
    Pass,
    /// 已处理：后续钩子与命令分发都不再执行
    Handled,
}

/// 事件处理器（用 [`event_handler`] 包异步函数即可，不必自己实现 trait）
pub trait EventHandler: Send + Sync {
    fn handle<'a>(&'a self, context: Arc<EventContext>) -> BoxFuture<'a, HookFlow>;
}

struct FnEventHandler<F> {
    inner: F,
}

impl<F, Fut> EventHandler for FnEventHandler<F>
where
    F: Fn(Arc<EventContext>) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = HookFlow> + Send,
{
    fn handle<'a>(&'a self, context: Arc<EventContext>) -> BoxFuture<'a, HookFlow> {
        Box::pin(async move { (self.inner)(context).await })
    }
}

/// 把异步闭包包装成事件处理器
pub fn event_handler<F, Fut>(inner: F) -> Arc<dyn EventHandler>
where
    F: Fn(Arc<EventContext>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = HookFlow> + Send + 'static,
{
    Arc::new(FnEventHandler { inner })
}

#[derive(Clone)]
struct Hook {
    plugin: String,
    kinds: Vec<EventKind>,
    priority: ListenerPriority,
    handler: Arc<dyn EventHandler>,
    /// 登记序号：同优先级下保持注册顺序稳定
    seq: u64,
}

/// 一张事件钩子表：订阅者按归属插件记账，投递时先过门控
pub struct HookRegistry {
    hooks: RwLock<Vec<Hook>>,
    /// 登记序号发号器（表内单调即可）
    seq: AtomicU64,
    gating: Arc<Gating>,
}

impl HookRegistry {
    pub(crate) fn new(gating: Arc<Gating>) -> HookRegistry {
        HookRegistry {
            hooks: RwLock::new(Vec::new()),
            seq: AtomicU64::new(0),
            gating,
        }
    }

    /// 带优先级的订阅：先按 [`ListenerPriority`]（Monitor -> Normal -> High -> Low -> Lowest），
    /// 同优先级按注册顺序。多个插件共存时靠它决定谁先看事件、谁能短路谁。
    pub fn subscribe_at(
        &self,
        plugin: &str,
        kinds: &[EventKind],
        priority: ListenerPriority,
        handler: Arc<dyn EventHandler>,
    ) {
        self.hooks.write().unwrap().push(Hook {
            plugin: plugin.to_string(),
            kinds: kinds.to_vec(),
            priority,
            handler,
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
        });
    }

    /// 订阅事件（`kinds` 为空表示全部类型），默认 [`ListenerPriority::Normal`]。
    pub fn subscribe(&self, plugin: &str, kinds: &[EventKind], handler: Arc<dyn EventHandler>) {
        self.subscribe_at(plugin, kinds, ListenerPriority::default(), handler);
    }
}

/// 便捷入口：只订阅某一类事件
pub fn on_message(plugin: &str, handler: Arc<dyn EventHandler>) {
    subscribe(plugin, &[EventKind::Message], handler);
}

pub fn on_notice(plugin: &str, handler: Arc<dyn EventHandler>) {
    subscribe(plugin, &[EventKind::Notice], handler);
}

pub fn on_request(plugin: &str, handler: Arc<dyn EventHandler>) {
    subscribe(plugin, &[EventKind::Request], handler);
}

pub fn on_meta(plugin: &str, handler: Arc<dyn EventHandler>) {
    subscribe(plugin, &[EventKind::Meta], handler);
}

pub fn on_all(plugin: &str, handler: Arc<dyn EventHandler>) {
    subscribe(
        plugin,
        &[
            EventKind::Message,
            EventKind::Notice,
            EventKind::Request,
            EventKind::Meta,
        ],
        handler,
    );
}

impl HookRegistry {
    /// 注销某个插件的全部钩子，返回收回的订阅数（框架停用插件时统一回收）
    pub fn unsubscribe(&self, plugin: &str) -> usize {
        let mut hooks = self.hooks.write().unwrap();
        let before = hooks.len();
        hooks.retain(|hook| hook.plugin != plugin);
        before - hooks.len()
    }

    /// 已订阅的钩子数（GUI「插件管理」页展示）
    pub fn hook_count(&self, plugin: &str) -> usize {
        self.hooks
            .read()
            .unwrap()
            .iter()
            .filter(|hook| hook.plugin == plugin)
            .count()
    }

    /// 全部钩子的订阅概览：(插件名, 事件类型中文名)
    pub fn subscriptions(&self) -> Vec<(String, Vec<&'static str>)> {
        let mut result: Vec<(String, Vec<&'static str>)> = Vec::new();
        for hook in self.hooks.read().unwrap().iter() {
            let kinds = hook
                .kinds
                .iter()
                .map(|kind| kind.display_name())
                .collect::<Vec<_>>();
            match result.iter_mut().find(|(name, _)| *name == hook.plugin) {
                Some((_, existing)) => {
                    for kind in kinds {
                        if !existing.contains(&kind) {
                            existing.push(kind);
                        }
                    }
                }
                None => result.push((hook.plugin.clone(), kinds)),
            }
        }
        result
    }

    /// 该插件此刻是否允许收事件（框架门控，见模块注释）
    fn gate_open(&self, plugin: &str, group_id: Option<i64>) -> bool {
        self.gating.plugin_enabled(plugin) && self.gating.plugin_enabled_in_group(plugin, group_id)
    }

    /// 把事件投给订阅者；返回 true 表示已被插件消费（消息事件不再走命令分发）
    pub async fn dispatch(&self, event: &OneBotEvent) -> bool {
        let Some(kind) = EventKind::from_post_type(&event.post_type) else {
            return false;
        };
        // 先把要跑的钩子取出来再执行：钩子里很可能回头 subscribe/unsubscribe，
        // 持着读锁回调会自死锁。
        let hooks: Vec<Hook> = {
            let guard = self.hooks.read().unwrap();
            let mut hooks: Vec<Hook> = guard
                .iter()
                .filter(|hook| hook.kinds.contains(&kind))
                .filter(|hook| self.gate_open(&hook.plugin, event.group_id))
                .cloned()
                .collect();
            hooks.sort_by_key(|hook| (hook.priority.order(), hook.seq));
            hooks
        };
        if hooks.is_empty() {
            return false;
        }
        let context = Arc::new(EventContext {
            kind,
            text: protocol::extract_text(event),
            segments: protocol::extract_segments(event),
            event: event.clone(),
            api: OneBotApi::global(),
        });
        for hook in hooks {
            if hook.handler.handle(context.clone()).await == HookFlow::Handled {
                return true;
            }
        }
        false
    }
}

fn global() -> &'static HookRegistry {
    crate::framework::Framework::global().hooks()
}

/// 订阅事件（`kinds` 为空表示全部类型），默认 [`ListenerPriority::Normal`]。
pub fn subscribe(plugin: &str, kinds: &[EventKind], handler: Arc<dyn EventHandler>) {
    global().subscribe(plugin, kinds, handler);
}

/// 带优先级的订阅（进程默认实例）
pub fn subscribe_at(
    plugin: &str,
    kinds: &[EventKind],
    priority: ListenerPriority,
    handler: Arc<dyn EventHandler>,
) {
    global().subscribe_at(plugin, kinds, priority, handler);
}

/// 注销某个插件的全部钩子，返回收回的订阅数（框架停用插件时统一回收）
pub fn unsubscribe(plugin: &str) -> usize {
    global().unsubscribe(plugin)
}

/// 已订阅的钩子数（GUI「插件管理」页展示）
pub fn hook_count(plugin: &str) -> usize {
    global().hook_count(plugin)
}

/// 全部钩子的订阅概览：(插件名, 事件类型中文名)
pub fn subscriptions() -> Vec<(String, Vec<&'static str>)> {
    global().subscriptions()
}

/// 把事件投给订阅者；返回 true 表示已被插件消费（消息事件不再走命令分发）
pub async fn dispatch(event: &OneBotEvent) -> bool {
    global().dispatch(event).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn event(post_type: &str, group_id: Option<i64>) -> OneBotEvent {
        OneBotEvent {
            time: 0,
            self_id: 1,
            post_type: post_type.into(),
            notice_type: None,
            message_type: Some("group".into()),
            sub_type: None,
            message_id: Some(7),
            user_id: Some(100),
            operator_id: None,
            group_id,
            raw_message: None,
            message: Some(json!([{"type": "text", "data": {"text": "早安"}}])),
            sender: None,
            raw: json!({ "notice_type": "group_increase" }),
        }
    }

    /// 只负责「我被执行了」的标记，返回 Pass 让链路继续
    fn passes(flag: Arc<AtomicBool>) -> Arc<dyn EventHandler> {
        event_handler(move |_ctx: Arc<EventContext>| {
            let flag = flag.clone();
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
                HookFlow::Pass
            })
        })
    }

    /// 回归：钩子执行时必须已经释放钩子表读锁，否则钩子里再 subscribe 会自死锁
    #[tokio::test]
    async fn hook_may_touch_registry_while_running() {
        let registry = Arc::new(HookRegistry::new(Arc::new(Gating::default())));
        let plugin = "HookReentryTestPlugin";
        let notice_flag = Arc::new(AtomicBool::new(false));
        registry.subscribe(
            plugin,
            &[EventKind::Message],
            event_handler({
                let notice_flag = notice_flag.clone();
                let registry = registry.clone();
                move |_ctx: Arc<EventContext>| {
                    let notice_flag = notice_flag.clone();
                    let registry = registry.clone();
                    Box::pin(async move {
                        registry.subscribe(
                            plugin,
                            &[EventKind::Notice],
                            passes(notice_flag.clone()),
                        );
                        assert_eq!(registry.hook_count(plugin), 2, "钩子内应立刻看到自己的注册");
                        HookFlow::Pass
                    })
                }
            }),
        );
        assert!(
            !registry.dispatch(&event("message", Some(1))).await,
            "返回 Pass 时不应判定为已消费"
        );
        assert!(
            !notice_flag.load(Ordering::SeqCst),
            "message 事件不该命中 notice 订阅"
        );
        assert!(!registry.dispatch(&event("notice", Some(1))).await);
        assert!(
            notice_flag.load(Ordering::SeqCst),
            "钩子内注册的订阅应当立刻可用（说明执行时没有持着钩子表读锁）"
        );
    }

    /// 某个插件被全局禁用后，它的钩子必须一条都不执行
    #[tokio::test]
    async fn disabled_plugin_gets_no_events() {
        let gating = Arc::new(Gating::default());
        let registry = Arc::new(HookRegistry::new(gating.clone()));
        let plugin = "HookDisabledTestPlugin";
        let flag = Arc::new(AtomicBool::new(false));
        registry.subscribe(plugin, &[EventKind::Message], passes(flag.clone()));
        gating.set_disabled_plugins(vec![plugin.to_string()]);
        assert!(
            !registry.dispatch(&event("message", Some(2))).await,
            "禁用插件不该收到事件"
        );
        assert!(!flag.load(Ordering::SeqCst), "禁用插件的钩子不应被执行");
    }

    /// 群内禁用只影响那个群
    #[tokio::test]
    async fn group_switch_only_affects_that_group() {
        let gating = Arc::new(Gating::default());
        let registry = Arc::new(HookRegistry::new(gating.clone()));
        let plugin = "HookGroupSwitchTestPlugin";
        let group_id = 777_001_i64;
        let flag = Arc::new(AtomicBool::new(false));
        registry.subscribe(plugin, &[EventKind::Message], passes(flag.clone()));
        let mut settings = gating.group_settings();
        settings.insert(
            group_id.to_string(),
            crate::config::arona::GroupSetting {
                disabled_plugins: vec![plugin.to_string()],
                ..Default::default()
            },
        );
        gating.set_group_settings(settings);
        assert!(
            !registry.dispatch(&event("message", Some(group_id))).await,
            "该群禁用了此插件"
        );
        assert!(!flag.load(Ordering::SeqCst), "被禁用的群里不该执行钩子");
        assert!(
            !registry
                .dispatch(&event("message", Some(group_id + 1)))
                .await,
            "别的群仍应放行（返回值是 Pass）"
        );
        assert!(flag.load(Ordering::SeqCst), "别的群的钩子应正常执行");
    }

    /// 返回 Handled 的钩子要短路：后面的钩子与命令分发都不再执行
    #[tokio::test]
    async fn handled_flow_short_circuits() {
        let registry = Arc::new(HookRegistry::new(Arc::new(Gating::default())));
        let plugin = "HookHandledTestPlugin";
        let second = Arc::new(AtomicBool::new(false));
        registry.subscribe(
            plugin,
            &[EventKind::Message],
            event_handler(|_ctx: Arc<EventContext>| Box::pin(async { HookFlow::Handled })),
        );
        registry.subscribe(plugin, &[EventKind::Message], passes(second.clone()));
        assert!(
            registry.dispatch(&event("message", Some(3))).await,
            "Handled 应让框架判定事件已被消费"
        );
        assert!(!second.load(Ordering::SeqCst), "短路后的钩子不该再执行");
    }

    /// 多插件共存时靠优先级定序，而不是靠谁先注册（mirai EventPriority 语义）
    #[tokio::test]
    async fn priority_orders_listeners_ahead_of_registration() {
        use crate::runtime::priority::ListenerPriority;
        let registry = Arc::new(HookRegistry::new(Arc::new(Gating::default())));
        let plugin = "HookPriorityTestPlugin";
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let record = |name: &'static str| {
            let order = order.clone();
            event_handler(move |_ctx: Arc<EventContext>| {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().unwrap().push(name);
                    HookFlow::Pass
                })
            })
        };
        // Normal 先注册，Monitor 后注册：执行顺序仍应是 Monitor 在前
        registry.subscribe_at(
            plugin,
            &[EventKind::Message],
            ListenerPriority::Normal,
            record("normal"),
        );
        registry.subscribe_at(
            plugin,
            &[EventKind::Message],
            ListenerPriority::Monitor,
            record("monitor"),
        );
        registry.subscribe_at(
            plugin,
            &[EventKind::Message],
            ListenerPriority::Low,
            record("low"),
        );
        assert!(!registry.dispatch(&event("message", Some(9))).await);
        assert_eq!(
            *order.lock().unwrap(),
            vec!["monitor", "normal", "low"],
            "执行顺序应按优先级而非注册顺序"
        );
    }

    /// 非消息事件也要能投递；未知 post_type 直接忽略
    #[tokio::test]
    async fn notice_dispatches_and_unknown_post_type_does_not() {
        let registry = Arc::new(HookRegistry::new(Arc::new(Gating::default())));
        let plugin = "HookNoticeTestPlugin";
        let flag = Arc::new(AtomicBool::new(false));
        registry.subscribe(plugin, &[EventKind::Notice], passes(flag.clone()));
        assert!(!registry.dispatch(&event("notice", Some(4))).await);
        assert!(flag.load(Ordering::SeqCst), "notice 应投递给订阅者");
        assert_eq!(registry.unsubscribe(plugin), 1);
        assert!(
            !registry
                .dispatch(&event("unknown_post_type", Some(5)))
                .await,
            "未知 post_type 不该命中任何钩子"
        );
    }

    /// 群名/名片之外的细节字段从 raw 里取（GUI 与插件都靠这个判断子类型）
    #[test]
    fn context_reads_raw_detail_fields() {
        let context = EventContext {
            kind: EventKind::Notice,
            event: event("notice", Some(6)),
            text: String::new(),
            segments: Vec::new(),
            api: OneBotApi::global(),
        };
        assert_eq!(context.field_str("notice_type"), Some("group_increase"));
        assert_eq!(context.field_str("missing"), None);
        assert_eq!(context.group_id(), Some(6));
        assert_eq!(context.message_id(), Some(7));
    }
}
