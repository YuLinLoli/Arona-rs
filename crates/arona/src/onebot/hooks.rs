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
//! - 钩子先按 [`ListenerPriority`]（Monitor -> High -> Normal -> Low -> Lowest）、
//!   同优先级按注册顺序执行，任何一条返回 [`HookFlow::Handled`] 就停止后续钩子，
//!   并且不再走命令分发（等价于 mirai 的 `event.intercept()`）；
//! - 所属插件被禁用时（全局或该群），它的钩子一律跳过；
//!   订阅时用 `ctx.listen_feature(..)` 绑过分群功能开关的，关掉那个功能就等于
//!   把这条钩子也一起关掉（和同名命令保持一致，不会出现"功能显示已关、钩子还在收消息"）。
//!
//! 发送那一侧另有一道**出站前**订阅（mirai 的 `MessagePreSendEvent`）：`ctx.on_outgoing(..)`
//! 拿到 [`OutboundContext`]，用 [`rewrite`](OutboundContext::rewrite) 改写内容、
//! 用 [`cancel`](OutboundContext::cancel) 整条拦停，门控与优先级语义和入站一致。
//! 它挂在框架统一发送口上（命令回复、钩子内 `reply`、主动推送都过），
//! `OneBotApi` 的直连发送方法不过——那条路留给 GUI 与确实需要绕门的场景。
use crate::onebot::api::OneBotApi;
use crate::onebot::model::OneBotEvent;
use crate::onebot::protocol;
use crate::runtime::config::Gating;
use crate::runtime::message::{BoxFuture, MessageSegment, MessageTarget, OutgoingMessage};
pub use crate::runtime::priority::ListenerPriority;
use serde_json::Value;
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

/// 通知事件的子类（OneBot 的 `notice_type`，对应 mirai 的 `GroupNoticeEvent` 家族）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// 群文件上传
    FileUpload,
    /// 群成员退群（含被踢）
    GroupDecrease,
    /// 群成员进群
    GroupIncrease,
    /// 群管理员变更
    GroupAdmin,
    /// 群成员被禁言/解除禁言
    GroupBan,
    /// 群内消息撤回
    GroupRecall,
    /// 戳一戳
    Poke,
    /// 窗一戳（QQ 的 poke 变体）
    Nudge,
    /// 群名片变更
    GroupCardUpdate,
    /// 群头衔变更
    GroupTitleUpdate,
    /// 好友/群内的普通通知（如被 @）
    Notify,
    /// 其余（实现端自定义的 notice_type）
    Other,
}

impl NoticeKind {
    /// 按 OneBot 的 `notice_type` 归位。`notify` 这一族在这里分不出具体是哪一种，
    /// 要用 [`NoticeKind::parse_with_action`] 把动作细分一起传进来。
    pub fn parse(value: &str) -> NoticeKind {
        NoticeKind::parse_with_action(value, "")
    }

    /// `notice_type` + 动作细分归位。戳一戳、群名片与头衔变更在 NapCat / LLOneBot 上
    /// 都是 `notice_type="notify"` 发出来的，真正的类型写在内层的 `sub_type`/`type` 里：
    /// 只看 notice_type 的话它们全落在 [`NoticeKind::Notify`]，插件又得自己比字符串。
    pub fn parse_with_action(notice_type: &str, action: &str) -> NoticeKind {
        let action = action.trim().to_ascii_lowercase();
        match notice_type {
            "group_upload" => NoticeKind::FileUpload,
            "group_decrease" => NoticeKind::GroupDecrease,
            "group_increase" => NoticeKind::GroupIncrease,
            "group_admin" => NoticeKind::GroupAdmin,
            "group_ban" => NoticeKind::GroupBan,
            "group_recall" => NoticeKind::GroupRecall,
            "poke" => NoticeKind::Poke,
            "nudge" => NoticeKind::Nudge,
            "group_card" | "group_card_update" => NoticeKind::GroupCardUpdate,
            "group_title" | "group_title_update" => NoticeKind::GroupTitleUpdate,
            "notify" | "friend_notify" => match action.as_str() {
                "poke" | "is_self_poke" => NoticeKind::Poke,
                "nudge" => NoticeKind::Nudge,
                "card" | "group_card" => NoticeKind::GroupCardUpdate,
                "title" | "group_title" | "honor" => NoticeKind::GroupTitleUpdate,
                _ => NoticeKind::Notify,
            },
            _ => NoticeKind::Other,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            NoticeKind::FileUpload => "群文件上传",
            NoticeKind::GroupDecrease => "成员退群",
            NoticeKind::GroupIncrease => "成员进群",
            NoticeKind::GroupAdmin => "管理员变更",
            NoticeKind::GroupBan => "禁言",
            NoticeKind::GroupRecall => "群消息撤回",
            NoticeKind::Poke => "戳一戳",
            NoticeKind::Nudge => "戳一戳（nudge）",
            NoticeKind::GroupCardUpdate => "群名片变更",
            NoticeKind::GroupTitleUpdate => "群头衔变更",
            NoticeKind::Notify => "通知",
            NoticeKind::Other => "其他通知",
        }
    }
}

/// 请求事件的子类（OneBot 的 `request_type`，对应 mirai 的 `VerifyMessage` 家族）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    /// 加好友
    Friend,
    /// 加群（用户申请）
    AddGroup,
    /// 被邀请加群
    InviteGroup,
    /// 其余
    Other,
}

impl RequestKind {
    /// 按 OneBot 的 `request_type` 归位。群请求的"申请 / 邀请"之分不在这一层，
    /// 要用 [`RequestKind::parse_with_via`]。
    pub fn parse(value: &str) -> RequestKind {
        RequestKind::parse_with_via(value, "")
    }

    /// `request_type` + 来路归位。v11 把群邀请和主动申请都写成 `request_type="group"`，
    /// 只用 `sub_type`（add / invite）区分——各实现端字段名略有出入，`via` 由调用方
    /// 从 `sub_type`、`type` 里挑第一个非空值传进来。漏了这一步，"别人邀请机器人进群"
    /// 会被当成"某人申请加群"，自动通过申请的插件因此会把邀请放进来。
    pub fn parse_with_via(request_type: &str, via: &str) -> RequestKind {
        let via = via.trim().to_ascii_lowercase();
        match request_type {
            "friend" => RequestKind::Friend,
            "group" | "add_group" => {
                if via == "invite" || via == "invited" || via == "be_invite" {
                    RequestKind::InviteGroup
                } else {
                    RequestKind::AddGroup
                }
            }
            _ => RequestKind::Other,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            RequestKind::Friend => "好友申请",
            RequestKind::AddGroup => "加群申请",
            RequestKind::InviteGroup => "群邀请",
            RequestKind::Other => "其他申请",
        }
    }
}

/// 元事件的子类（OneBot 的 `meta_event_type`）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetaKind {
    Heartbeat,
    /// 客户端掉线、服务恢复等平台级事件（对应 mirai 的 `ClientLaunchFinishedStateEvent` 等）
    Lifecycle,
    Other,
}

impl MetaKind {
    pub fn parse(value: &str) -> MetaKind {
        match value {
            "heartbeat" => MetaKind::Heartbeat,
            "lifecycle" => MetaKind::Lifecycle,
            _ => MetaKind::Other,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            MetaKind::Heartbeat => "心跳",
            MetaKind::Lifecycle => "生命周期",
            MetaKind::Other => "其他元事件",
        }
    }
}

/// `notice` 事件的细节（mirai 的各个 `*NoticeEvent` 子类带的字段）。
///
/// 光有 [`NoticeKind`] 分不开"自行退群"和"被踢"、"被禁言"和"解禁"，
/// 而这三者对插件来说是完全不同的事，所以把 OneBot 写在同一层的动作与 id 一起归位。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoticeInfo {
    /// 同一 notice_type 下的动作：approve / leave / kick / kick_me / ban / unban /
    /// promote / demote / poke …（实现端没给时为 None）
    pub action: Option<String>,
    /// 操作者：谁踢的人、谁禁的言、谁撤回的消息、戳一戳的发起方
    pub operator_id: Option<i64>,
    /// 动作对象：戳一戳被戳的人、名片/头衔变更的那个成员
    pub target_id: Option<i64>,
    /// 禁言时长（秒），0 表示解除禁言
    pub duration: Option<i64>,
}

/// `request` 事件的细节：处理请求要回传的凭据与留言都在这。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestInfo {
    /// 处理凭据，`set_group_add_request` / `set_friend_add_request` 要求原样带回
    pub flag: Option<String>,
    /// 申请人留言（实现端的字段名 comment / message 两种都有）
    pub comment: Option<String>,
    /// 来路：add（主动申请）/ invite（被邀请）
    pub via: Option<String>,
}

/// 强类型的事件体（对应 mirai 的 `GroupMessageEvent`/`NudgeEvent` 这一族）。
///
/// 插件不再靠 `field_str("notice_type") == Some("group_increase")` 这种字符串比较分支，
/// 而是 `match ctx.body` 或直接订阅 [`BodyFilter`]。原始字段仍可从 `ctx.event.raw` 取。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventBody {
    /// 群消息（`sub_type`：normal / anonymous / offline / online）
    GroupMessage {
        sub_type: Option<String>,
    },
    /// 私聊消息（`sub_type`：friend / group(临时会话) 等）
    PrivateMessage {
        sub_type: Option<String>,
    },
    Notice(NoticeKind, NoticeInfo),
    Request(RequestKind, RequestInfo),
    Meta(MetaKind),
    /// 框架没认出大类的事件（实现端自定义 post_type）
    Other {
        post_type: String,
        sub_type: Option<String>,
    },
}

impl EventBody {
    /// 从 OneBot 事件归位
    pub fn from_event(event: &OneBotEvent) -> EventBody {
        let sub_type = {
            let value = detail(event, &["sub_type"]);
            (!value.is_empty()).then_some(value)
        };
        match event.post_type.as_str() {
            "message" => {
                if event.message_type.as_deref() == Some("group") || event.group_id.is_some() {
                    EventBody::GroupMessage { sub_type }
                } else {
                    EventBody::PrivateMessage { sub_type }
                }
            }
            "notice" => {
                let action = detail(event, &["sub_type", "type"]);
                EventBody::Notice(
                    NoticeKind::parse_with_action(
                        event.notice_type.as_deref().unwrap_or_default(),
                        &action,
                    ),
                    NoticeInfo {
                        action: (!action.is_empty()).then_some(action),
                        operator_id: event.operator_id.or_else(|| raw_i64(event, "operator_id")),
                        target_id: raw_i64(event, "target_id"),
                        duration: raw_i64(event, "duration"),
                    },
                )
            }
            "request" => {
                let via = detail(event, &["sub_type", "type"]);
                EventBody::Request(
                    RequestKind::parse_with_via(raw_str(event, "request_type"), &via),
                    RequestInfo {
                        flag: non_empty_string(event, "flag"),
                        comment: non_empty_string(event, "comment")
                            .or_else(|| non_empty_string(event, "message")),
                        via: (!via.is_empty()).then_some(via),
                    },
                )
            }
            "meta_event" | "meta" => {
                EventBody::Meta(MetaKind::parse(raw_str(event, "meta_event_type")))
            }
            other => EventBody::Other {
                post_type: other.to_string(),
                sub_type,
            },
        }
    }

    /// 所属事件大类
    pub fn kind(&self) -> Option<EventKind> {
        match self {
            EventBody::GroupMessage { .. } | EventBody::PrivateMessage { .. } => {
                Some(EventKind::Message)
            }
            EventBody::Notice(..) => Some(EventKind::Notice),
            EventBody::Request(..) => Some(EventKind::Request),
            EventBody::Meta(_) => Some(EventKind::Meta),
            EventBody::Other { .. } => None,
        }
    }

    /// notice 事件的子类（非通知事件为 None）
    pub fn notice_kind(&self) -> Option<NoticeKind> {
        match self {
            EventBody::Notice(kind, _) => Some(*kind),
            _ => None,
        }
    }

    /// request 事件的子类（非请求事件为 None）
    pub fn request_kind(&self) -> Option<RequestKind> {
        match self {
            EventBody::Request(kind, _) => Some(*kind),
            _ => None,
        }
    }

    /// 展示名（GUI 与日志用）
    pub fn display_name(&self) -> String {
        match self {
            EventBody::GroupMessage { .. } => "群消息".to_string(),
            EventBody::PrivateMessage { .. } => "私聊消息".to_string(),
            EventBody::Notice(kind, info) => match &info.action {
                Some(action) => format!("通知:{}({})", kind.display_name(), action),
                None => format!("通知:{}", kind.display_name()),
            },
            EventBody::Request(kind, info) => match &info.via {
                Some(via) => format!("申请:{}({})", kind.display_name(), via),
                None => format!("申请:{}", kind.display_name()),
            },
            EventBody::Meta(kind) => format!("元事件:{}", kind.display_name()),
            EventBody::Other { post_type, .. } => format!("其他:{post_type}"),
        }
    }
}

/// 从事件原始 JSON 里取字符串字段（缺失/非字符串都给空串）
fn raw_str<'a>(event: &'a OneBotEvent, key: &str) -> &'a str {
    event
        .raw
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or("")
}

/// 事件的细分值：v11 写字符串 `sub_type`，有的实现端用数字，也有的把动作放在内层 `type`。
/// 按 `keys` 的顺序取第一个非空、且不是 "unknown" 的写法。
fn detail(event: &OneBotEvent, keys: &[&str]) -> String {
    for key in keys {
        let value = match event.raw.get(*key) {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => match number.as_i64() {
                // 实现端的数字码各家不通用，留原文让上层按字符串匹配已知的几种写法
                Some(number) => number.to_string(),
                None => String::new(),
            },
            _ => String::new(),
        };
        let value = value.trim().to_string();
        if !value.is_empty() && !value.eq_ignore_ascii_case("unknown") {
            return value;
        }
    }
    String::new()
}

/// 原始 JSON 里的整数字段（数字与数字字符串都收）
fn raw_i64(event: &OneBotEvent, key: &str) -> Option<i64> {
    match event.raw.get(key)? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn non_empty_string(event: &OneBotEvent, key: &str) -> Option<String> {
    event
        .raw
        .get(key)
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 订阅过滤器：比 [`EventKind`] 更细一层，直接对上事件子类
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BodyFilter {
    /// 全部事件
    All,
    /// 某个大类
    Kind(EventKind),
    /// 群消息 / 私聊消息
    GroupMessage,
    PrivateMessage,
    Notice(NoticeKind),
    /// 通知子类 + 动作（[`NoticeInfo::action`]）：
    /// `BodyFilter::notice(NoticeKind::GroupDecrease, "kick")` 只收"被踢出群"，
    /// 自行退群是 "leave"、被自家机器人踢走是 "kick_me"。
    NoticeAction(NoticeKind, String),
    Request(RequestKind),
    Meta(MetaKind),
}

impl BodyFilter {
    /// 通知子类；`action` 非空时只收该动作（如 `("group_decrease", "kick")`）
    pub fn notice(kind: NoticeKind, action: &str) -> BodyFilter {
        if action.is_empty() {
            return BodyFilter::Notice(kind);
        }
        BodyFilter::NoticeAction(kind, action.trim().to_ascii_lowercase())
    }

    /// 这个过滤器是否收该事件
    pub fn matches(&self, body: &EventBody) -> bool {
        match self {
            BodyFilter::All => true,
            BodyFilter::Kind(kind) => body.kind() == Some(*kind),
            BodyFilter::GroupMessage => matches!(body, EventBody::GroupMessage { .. }),
            BodyFilter::PrivateMessage => matches!(body, EventBody::PrivateMessage { .. }),
            BodyFilter::Notice(kind) => {
                matches!(body, EventBody::Notice(found, _) if found == kind)
            }
            BodyFilter::NoticeAction(kind, action) => matches!(
                body,
                EventBody::Notice(found, info)
                    if found == kind
                        && info.action.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(action))
            ),
            BodyFilter::Request(kind) => {
                matches!(body, EventBody::Request(found, _) if found == kind)
            }
            BodyFilter::Meta(kind) => matches!(body, EventBody::Meta(found) if found == kind),
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            BodyFilter::All => "全部事件".to_string(),
            BodyFilter::Kind(kind) => kind.display_name().to_string(),
            BodyFilter::GroupMessage => "群消息".to_string(),
            BodyFilter::PrivateMessage => "私聊消息".to_string(),
            BodyFilter::Notice(kind) => format!("通知:{}", kind.display_name()),
            BodyFilter::NoticeAction(kind, action) => {
                format!("通知:{}({})", kind.display_name(), action)
            }
            BodyFilter::Request(kind) => format!("申请:{}", kind.display_name()),
            BodyFilter::Meta(kind) => format!("元事件:{}", kind.display_name()),
        }
    }
}

/// 投递给钩子的上下文
pub struct EventContext {
    pub kind: EventKind,
    /// 原始事件（`raw` 里能取到 notice_type / request_type 等细节字段）
    pub event: OneBotEvent,
    /// 强类型事件体：`match ctx.body` 就能按子类分支，不必再比字符串
    pub body: EventBody,
    /// 消息纯文本（非消息事件为空串；@ 段渲染成 `@qq号`）
    pub text: String,
    /// 剥掉"@机器人 前缀"的文本。插件想自己在钩子里匹配命令时用这个而不是 `text`
    pub command_text: String,
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

    /// 发送者在事件里给出的群名片，没有则退回昵称（命令上下文用的是同一套优先级）
    pub fn sender_name(&self) -> Option<&str> {
        let sender = self.event.sender.as_ref()?;
        let text = |key: &str| {
            sender
                .get(key)
                .and_then(|value| value.as_str())
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
        };
        text("card").or_else(|| text("nickname"))
    }

    /// 发送者的群身份（`sender.role`）：私聊、notice 事件没有 sender 对象时为 None
    pub fn sender_role(&self) -> Option<crate::runtime::dispatcher::GroupRole> {
        self.event
            .sender
            .as_ref()
            .and_then(crate::runtime::dispatcher::GroupRole::from_sender)
    }

    /// 发送者是不是群管理员或群主（身份未知时为 false，宁可少做判断）
    pub fn sender_is_admin(&self) -> bool {
        self.sender_role()
            .is_some_and(|role| role >= crate::runtime::dispatcher::GroupRole::Admin)
    }

    /// 这条消息引用了哪条消息（reply/quote 段）。`text` 与 `command_text` 里都已剥掉它，
    /// 想区分"引用回复"和"直接发言"的插件看这个
    pub fn quoted(&self) -> Option<i64> {
        self.segments.iter().find_map(|segment| match segment {
            MessageSegment::Reply(message_id) => Some(*message_id),
            _ => None,
        })
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
    pub fn field(&self, key: &str) -> Option<&Value> {
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
    ///
    /// 走框架的统一出口（`runtime::services` 的发送器）而不是直连 `api.send`：
    /// 出站钩子、控制台打印、合并转发拆分与自动撤回都在那条路上，
    /// 直连会让钩子里回的消息变成出站门控的漏网之鱼。
    pub async fn reply_message(
        &self,
        message: OutgoingMessage,
    ) -> Result<Option<i64>, crate::onebot::api::OneBotError> {
        let Some(target) = self.target() else {
            return Ok(None);
        };
        if !crate::runtime::services::sender_ready() {
            return Err(crate::onebot::api::OneBotError::NoConnection);
        }
        Ok(crate::runtime::services::send_message(target, message)
            .await
            .message_id)
    }

    pub async fn reply(
        &self,
        text: impl Into<String>,
    ) -> Result<Option<i64>, crate::onebot::api::OneBotError> {
        self.reply_message(OutgoingMessage::text(text)).await
    }

    /// 撤回指定消息（自己发的，或有管理权限时撤回别人的）。
    /// 先按框架的聊天记录库把 id 换成实现端认的那个号：NapCat / LLOWeb 的 `delete_msg`
    /// 只吃 `real_id`，拿事件里的 message_id 直接去撤会报「消息不存在」。
    pub async fn recall(&self, message_id: i64) -> Result<(), crate::onebot::api::OneBotError> {
        crate::runtime::chatlog::recall(message_id).await
    }

    /// 引用回复：把这条事件引用的那条消息一起带回去。协议层还引用得到就挂原生 reply 段，
    /// 来不及了就由框架的聊天记录库还原成文字加图；这条事件本身没引用时等同于
    /// [`reply_message`](Self::reply_message)。
    pub async fn reply_with_quote(
        &self,
        mut message: OutgoingMessage,
    ) -> Result<Option<i64>, crate::onebot::api::OneBotError> {
        let at = self.event.time;
        if let Some(prefix) = crate::runtime::chatlog::quote_prefix(self.quoted(), at).await {
            message.segments.splice(0..0, prefix);
        }
        self.reply_message(message).await
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
    /// 订阅范围；空表示 [`BodyFilter::All`]
    filters: Vec<BodyFilter>,
    /// 绑定的分群功能开关 key（install 阶段 `ctx.feature` 登记的那个）；
    /// 空串表示这条钩子不属于任何开关，只跟插件本身的启停走
    feature: &'static str,
    priority: ListenerPriority,
    handler: Arc<dyn EventHandler>,
    /// 登记序号：同优先级下保持注册顺序稳定
    seq: u64,
}

/// 单个插件允许持有的事件订阅条数上限（防重复装配泄漏，正常插件远用不到）
const MAX_HOOKS_PER_PLUGIN: usize = 64;

/// 投递给出站钩子的上下文（mirai 的 `MessagePreSendEvent`）。
///
/// 两个动作：**改写**（[`rewrite`](Self::rewrite)：加统一后缀、换掉失效图链、删掉某一段）
/// 与 **拦停**（[`cancel`](Self::cancel)：这条别发出去）。多家订阅者按优先级依次过，
/// 后面的能看到前面改写后的结果。
pub struct OutboundContext {
    pub target: MessageTarget,
    message: RwLock<OutgoingMessage>,
    cancelled: std::sync::atomic::AtomicBool,
}

impl OutboundContext {
    fn new(target: MessageTarget, message: OutgoingMessage) -> OutboundContext {
        OutboundContext {
            target,
            message: RwLock::new(message),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 当前这份待发消息（快照；要改请用 [`Self::rewrite`]）
    pub fn message(&self) -> OutgoingMessage {
        self.message.read().unwrap().clone()
    }

    /// 改写待发消息：闭包拿到的就是到目前为止各家订阅者改过的内容
    pub fn rewrite(&self, edit: impl FnOnce(&mut OutgoingMessage)) {
        edit(&mut self.message.write().unwrap());
    }

    /// 拦停这条消息（发送方拿到空回执，实现端不会收到任何动作）
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// 出站处理器（用 [`outbound_handler`] 包异步函数即可，不必自己实现 trait）
pub trait OutboundHandler: Send + Sync {
    /// 返回值没有语义：改写用 `context.rewrite(..)`，拦停用 `context.cancel()`
    fn handle<'a>(&'a self, context: Arc<OutboundContext>) -> BoxFuture<'a, ()>;
}

struct FnOutboundHandler<F> {
    inner: F,
}

impl<F, Fut> OutboundHandler for FnOutboundHandler<F>
where
    F: Fn(Arc<OutboundContext>) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = ()> + Send,
{
    fn handle<'a>(&'a self, context: Arc<OutboundContext>) -> BoxFuture<'a, ()> {
        Box::pin(async move { (self.inner)(context).await })
    }
}

/// 把异步闭包包装成出站处理器
pub fn outbound_handler<F, Fut>(inner: F) -> Arc<dyn OutboundHandler>
where
    F: Fn(Arc<OutboundContext>) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    Arc::new(FnOutboundHandler { inner })
}

#[derive(Clone)]
struct OutboundHook {
    plugin: String,
    /// 绑定的分群功能开关 key，语义同 [`Hook::feature`]
    feature: &'static str,
    priority: ListenerPriority,
    handler: Arc<dyn OutboundHandler>,
    seq: u64,
}

/// 一张事件钩子表：订阅者按归属插件记账，投递时先过门控
pub struct HookRegistry {
    hooks: RwLock<Vec<Hook>>,
    /// 出站订阅表（mirai 的 MessagePreSend 侧）
    outbound: RwLock<Vec<OutboundHook>>,
    /// 登记序号发号器（表内单调即可）
    seq: AtomicU64,
    gating: Arc<Gating>,
    health: Arc<crate::plugin::health::HealthBoard>,
}

impl HookRegistry {
    pub(crate) fn new(
        gating: Arc<Gating>,
        health: Arc<crate::plugin::health::HealthBoard>,
    ) -> HookRegistry {
        HookRegistry {
            hooks: RwLock::new(Vec::new()),
            outbound: RwLock::new(Vec::new()),
            seq: AtomicU64::new(0),
            gating,
            health,
        }
    }

    /// 带优先级的订阅：先按 [`ListenerPriority`]（Monitor -> High -> Normal -> Low -> Lowest），
    /// 同优先级按注册顺序。多个插件共存时靠它决定谁先看事件、谁能短路谁。
    pub fn subscribe_at(
        &self,
        plugin: &str,
        kinds: &[EventKind],
        priority: ListenerPriority,
        handler: Arc<dyn EventHandler>,
    ) {
        self.subscribe_where(
            plugin,
            &kinds
                .iter()
                .copied()
                .map(BodyFilter::Kind)
                .collect::<Vec<_>>(),
            priority,
            handler,
        );
    }

    /// 订阅事件（`kinds` 为空表示全部类型），默认 [`ListenerPriority::Normal`]。
    pub fn subscribe(&self, plugin: &str, kinds: &[EventKind], handler: Arc<dyn EventHandler>) {
        self.subscribe_at(plugin, kinds, ListenerPriority::default(), handler);
    }

    /// 按**事件子类**订阅（mirai 的 `GroupMessageEvent`/`NudgeEvent` 这一族）：
    /// `&[BodyFilter::Notice(NoticeKind::GroupIncrease)]` 就只在有人进群时被叫到，
    /// 框架过滤，插件不必自己从 raw 里比字符串。不绑功能开关，见 [`subscribe_feature`]。
    ///
    /// [`subscribe_feature`]: HookRegistry::subscribe_feature
    pub fn subscribe_where(
        &self,
        plugin: &str,
        filters: &[BodyFilter],
        priority: ListenerPriority,
        handler: Arc<dyn EventHandler>,
    ) {
        self.subscribe_feature(plugin, "", filters, priority, handler);
    }

    /// 按子类订阅，并绑到本插件的某个分群功能开关上：那个开关在群里被关掉时，
    /// 这条钩子在该群就不投递（命令侧早就有这个门，钩子侧以前只查插件启停，
    /// 于是"关掉抽卡"只关掉了 `/抽卡`，还留着一条在监听所有消息的统计钩子）。
    ///
    /// 同一个处理器重复登记会被忽略（多半是热重载漏了撤销），每家插件的订阅数量另有上限：
    /// 钩子表是逐条 append 的，不设限的话一次泄漏就永久拖慢每一条事件。
    pub fn subscribe_feature(
        &self,
        plugin: &str,
        feature: &'static str,
        filters: &[BodyFilter],
        priority: ListenerPriority,
        handler: Arc<dyn EventHandler>,
    ) {
        let mut guard = self.hooks.write().unwrap();
        if guard
            .iter()
            .any(|hook| hook.plugin == plugin && Arc::ptr_eq(&hook.handler, &handler))
        {
            crate::runtime::log::warning(format!(
                "插件 {plugin} 重复订阅同一事件处理器（{} 档），本次忽略",
                priority.display_name()
            ));
            return;
        }
        let owned = guard.iter().filter(|hook| hook.plugin == plugin).count();
        if owned >= MAX_HOOKS_PER_PLUGIN {
            crate::runtime::log::error(format!(
                "插件 {plugin} 的事件订阅已达上限 {MAX_HOOKS_PER_PLUGIN}，本次丢弃——请检查是否重复装配未撤销"
            ));
            return;
        }
        guard.push(Hook {
            plugin: plugin.to_string(),
            filters: filters.to_vec(),
            feature,
            priority,
            handler,
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
        });
    }

    /// 订阅**出站**消息（机器人要说出去的话，mirai 的 `MessagePreSendEvent`）：
    /// 每条消息在发给实现端之前先过一遍订阅者，可改写可拦停。
    /// 去重与每家的条数上限和入站订阅同一套规则。
    pub fn subscribe_outbound(
        &self,
        plugin: &str,
        feature: &'static str,
        priority: ListenerPriority,
        handler: Arc<dyn OutboundHandler>,
    ) {
        let mut guard = self.outbound.write().unwrap();
        if guard
            .iter()
            .any(|hook| hook.plugin == plugin && Arc::ptr_eq(&hook.handler, &handler))
        {
            crate::runtime::log::warning(format!(
                "插件 {plugin} 重复订阅同一出站处理器（{} 档），本次忽略",
                priority.display_name()
            ));
            return;
        }
        let owned = guard.iter().filter(|hook| hook.plugin == plugin).count();
        if owned >= MAX_HOOKS_PER_PLUGIN {
            crate::runtime::log::error(format!(
                "插件 {plugin} 的出站订阅已达上限 {MAX_HOOKS_PER_PLUGIN}，本次丢弃——请检查是否重复装配未撤销"
            ));
            return;
        }
        guard.push(OutboundHook {
            plugin: plugin.to_string(),
            feature,
            priority,
            handler,
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
        });
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
    /// 注销某个插件的全部钩子（入站 + 出站），返回收回的订阅数（框架停用插件时统一回收）
    pub fn unsubscribe(&self, plugin: &str) -> usize {
        let mut hooks = self.hooks.write().unwrap();
        let before = hooks.len();
        hooks.retain(|hook| hook.plugin != plugin);
        let mut outbound = self.outbound.write().unwrap();
        let before_out = outbound.len();
        outbound.retain(|hook| hook.plugin != plugin);
        (before - hooks.len()) + (before_out - outbound.len())
    }

    /// 已订阅的钩子数（入站 + 出站，GUI「插件管理」页展示）
    pub fn hook_count(&self, plugin: &str) -> usize {
        self.hooks
            .read()
            .unwrap()
            .iter()
            .filter(|hook| hook.plugin == plugin)
            .count()
            + self
                .outbound
                .read()
                .unwrap()
                .iter()
                .filter(|hook| hook.plugin == plugin)
                .count()
    }

    /// 全部钩子的订阅概览：(插件名, 订阅范围展示名)
    pub fn subscriptions(&self) -> Vec<(String, Vec<String>)> {
        let mut result: Vec<(String, Vec<String>)> = Vec::new();
        let mut record = |plugin: &str, scopes: Vec<String>| match result
            .iter_mut()
            .find(|(name, _)| *name == plugin)
        {
            Some((_, existing)) => {
                for scope in scopes {
                    if !existing.contains(&scope) {
                        existing.push(scope);
                    }
                }
            }
            None => result.push((plugin.to_string(), scopes)),
        };
        for hook in self.hooks.read().unwrap().iter() {
            let scopes: Vec<String> = if hook.filters.is_empty() {
                vec![BodyFilter::All.display_name()]
            } else {
                hook.filters.iter().map(BodyFilter::display_name).collect()
            };
            record(&hook.plugin, scopes);
        }
        for hook in self.outbound.read().unwrap().iter() {
            record(&hook.plugin, vec!["出站消息".to_string()]);
        }
        result
    }

    /// 该插件此刻是否允许收事件（框架门控，见模块注释）：插件全局/群内启停，
    /// 外加这条钩子自己绑的功能开关（`feature` 为空时不看）
    fn gate_open(&self, plugin: &str, group_id: Option<i64>, feature: &str) -> bool {
        self.gating.plugin_enabled(plugin)
            && self.gating.plugin_enabled_in_group(plugin, group_id)
            && self.gating.feature_enabled(group_id, feature)
    }

    /// 把事件投给订阅者；返回 true 表示已被插件消费（消息事件不再走命令分发）
    pub async fn dispatch(&self, event: &OneBotEvent) -> bool {
        let Some((kind, body, hooks)) = self.matching_hooks(event) else {
            return false;
        };
        let self_id = if event.self_id != 0 {
            event.self_id
        } else {
            self.gating.bot_id()
        };
        let parsed = protocol::ParsedMessage::parse(event, self_id);
        self.invoke(kind, body, hooks, event, &parsed).await
    }

    /// 同上，但消息段由调用方**预先解析**好了。
    ///
    /// 事件入口本来就要取命令文本与引用编号（同一条消息解两三遍段是白付的钱），
    /// `business::on_event` 走这条；`EventContext` 里的 `text`/`command_text`/`segments`
    /// 因此和分发给命令的那份完全一致。
    pub async fn dispatch_parsed(
        &self,
        event: &OneBotEvent,
        parsed: &protocol::ParsedMessage,
    ) -> bool {
        let Some((kind, body, hooks)) = self.matching_hooks(event) else {
            return false;
        };
        self.invoke(kind, body, hooks, event, parsed).await
    }

    /// 命中本次事件的订阅者（已按优先级排好）；没有可跑的钩子时 `None`
    fn matching_hooks(&self, event: &OneBotEvent) -> Option<(EventKind, EventBody, Vec<Hook>)> {
        let body = EventBody::from_event(event);
        let kind = body
            .kind()
            .or_else(|| EventKind::from_post_type(&event.post_type))?;
        // 先把要跑的钩子取出来再执行：钩子里很可能回头 subscribe/unsubscribe，
        // 持着读锁回调会自死锁。
        let hooks = {
            let guard = self.hooks.read().unwrap();
            let mut hooks: Vec<Hook> = guard
                .iter()
                .filter(|hook| hook.filters.is_empty() || hook_wants(&hook.filters, &body))
                .filter(|hook| self.gate_open(&hook.plugin, event.group_id, hook.feature))
                .cloned()
                .collect();
            hooks.sort_by_key(|hook| (hook.priority.order(), hook.seq));
            hooks
        };
        (!hooks.is_empty()).then_some((kind, body, hooks))
    }

    async fn invoke(
        &self,
        kind: EventKind,
        body: EventBody,
        hooks: Vec<Hook>,
        event: &OneBotEvent,
        parsed: &protocol::ParsedMessage,
    ) -> bool {
        let context = Arc::new(EventContext {
            kind,
            text: parsed.text.clone(),
            command_text: parsed.command_text.clone(),
            segments: parsed.segments.clone(),
            body,
            event: event.clone(),
            api: OneBotApi::global(),
        });
        // 本次分发内已被隔离停用的插件：它们剩下的钩子直接跳过，但别家照跑
        let action = format!("事件 {}", context.body.display_name());
        let mut quarantined: Vec<String> = Vec::new();
        for hook in hooks {
            if quarantined.iter().any(|p| p == &hook.plugin) {
                continue;
            } // 一次 panic 只废掉这一个处理器（mirai 的 broadcastAndDumpInterceptedExceptions）
            let flow = crate::plugin::health::guarded(
                &hook.plugin,
                &action,
                hook.handler.handle(context.clone()),
            )
            .await;
            match flow {
                Some(HookFlow::Handled) => {
                    self.health.record_success(&hook.plugin);
                    return true;
                }
                Some(HookFlow::Pass) => self.health.record_success(&hook.plugin),
                None => {
                    if self.health.record_panic(&hook.plugin, "事件处理器") {
                        // 该插件已被隔离停用：本次事件里它剩下的钩子不必再问。
                        // 只跳过这一家，别家插件的钩子必须照跑（这里曾经是 break）。
                        quarantined.push(hook.plugin.clone());
                    }
                }
            }
        }
        false
    }

    /// 消息发给实现端**之前**过一遍出站订阅者（mirai 的 `MessagePreSendEvent`）：
    /// 返回 true 表示可以发（`message` 里是改写后的内容），false 表示被某家插件拦停。
    ///
    /// 门控与入站一致：插件在该群被停用、或订阅绑的功能开关被关掉时，它的出站处理不参与。
    /// 处理器 panic 按"放行"处理并记账——不能因为一家出事就把机器人的话全掐了。
    pub async fn dispatch_outbound(
        &self,
        target: MessageTarget,
        message: &mut OutgoingMessage,
    ) -> bool {
        let group_id = match target {
            MessageTarget::Group(group_id) => Some(group_id),
            MessageTarget::Private(_) => None,
        };
        let hooks: Vec<OutboundHook> = {
            let guard = self.outbound.read().unwrap();
            let mut hooks: Vec<OutboundHook> = guard
                .iter()
                .filter(|hook| self.gate_open(&hook.plugin, group_id, hook.feature))
                .cloned()
                .collect();
            hooks.sort_by_key(|hook| (hook.priority.order(), hook.seq));
            hooks
        };
        if hooks.is_empty() {
            return true;
        }
        let context = Arc::new(OutboundContext::new(target, message.clone()));
        let mut quarantined: Vec<String> = Vec::new();
        for hook in hooks {
            if quarantined.iter().any(|p| p == &hook.plugin) {
                continue;
            }
            let outcome = crate::plugin::health::guarded(
                &hook.plugin,
                "出站",
                hook.handler.handle(context.clone()),
            )
            .await;
            match outcome {
                Some(()) => self.health.record_success(&hook.plugin),
                None => {
                    if self.health.record_panic(&hook.plugin, "出站处理器") {
                        quarantined.push(hook.plugin.clone());
                    }
                }
            }
            // 已经被拦停就不必再问后面的订阅者，同时把"是谁拦的"留在日志里
            if context.is_cancelled() {
                crate::runtime::log::info(format!(
                    "插件 {} 拦下了一条发往 {target:?} 的消息",
                    hook.plugin
                ));
                break;
            }
        }
        *message = context.message();
        !context.is_cancelled()
    }
}

/// 订阅过滤器是否命中（`filters` 为空由调用方按"全收"处理）
fn hook_wants(filters: &[BodyFilter], body: &EventBody) -> bool {
    filters.iter().any(|filter| filter.matches(body))
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

/// 全部钩子的订阅概览：(插件名, 订阅范围展示名)
pub fn subscriptions() -> Vec<(String, Vec<String>)> {
    global().subscriptions()
}

/// 按事件子类订阅（进程默认实例）
pub fn subscribe_where(
    plugin: &str,
    filters: &[BodyFilter],
    priority: ListenerPriority,
    handler: Arc<dyn EventHandler>,
) {
    global().subscribe_where(plugin, filters, priority, handler);
}

/// 按事件子类订阅并绑功能开关（进程默认实例）
pub fn subscribe_feature(
    plugin: &str,
    feature: &'static str,
    filters: &[BodyFilter],
    priority: ListenerPriority,
    handler: Arc<dyn EventHandler>,
) {
    global().subscribe_feature(plugin, feature, filters, priority, handler);
}

/// 把事件投给订阅者；返回 true 表示已被插件消费（消息事件不再走命令分发）
pub async fn dispatch(event: &OneBotEvent) -> bool {
    global().dispatch(event).await
}

/// 订阅出站消息（进程默认实例）
pub fn subscribe_outbound(
    plugin: &str,
    feature: &'static str,
    priority: ListenerPriority,
    handler: Arc<dyn OutboundHandler>,
) {
    global().subscribe_outbound(plugin, feature, priority, handler);
}

/// 消息发给实现端前的最后一道订阅（进程默认实例）
pub async fn dispatch_outbound(target: MessageTarget, message: &mut OutgoingMessage) -> bool {
    global().dispatch_outbound(target, message).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
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

    /// 一张独立的钩子表：门控与健康度共用同一个 Gating，达阈值时的停用才真的作用在它身上
    fn registry(gating: Arc<Gating>) -> Arc<HookRegistry> {
        let health = Arc::new(crate::plugin::health::HealthBoard::new(
            crate::framework::Framework::DEFAULT_PANIC_THRESHOLD,
            gating.clone(),
        ));
        Arc::new(HookRegistry::new(gating, health))
    }

    /// 回归：钩子执行时必须已经释放钩子表读锁，否则钩子里再 subscribe 会自死锁
    #[tokio::test]
    async fn hook_may_touch_registry_while_running() {
        let registry = registry(Arc::new(Gating::default()));
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
        let registry = registry(gating.clone());
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
        let registry = registry(gating.clone());
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
        let registry = registry(Arc::new(Gating::default()));
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
        let registry = registry(Arc::new(Gating::default()));
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

    /// 一家插件 panic 到达阈值被隔离时，只该跳过它自己剩下的钩子，别家必须照跑。
    /// 回归点：这里曾经是 `break`，一家出事把整条事件链上所有插件一起掐了。
    #[tokio::test]
    async fn quarantining_one_plugin_does_not_silence_the_other() {
        use crate::runtime::priority::ListenerPriority;
        let gating = Arc::new(Gating::default());
        let health = Arc::new(crate::plugin::health::HealthBoard::new(1, gating.clone()));
        let registry = Arc::new(HookRegistry::new(gating.clone(), health));
        let crasher = "HookQuarantineCrasher";
        let bystander = "HookQuarantineBystander";
        let survivor = Arc::new(AtomicBool::new(false));
        let same_plugin_second_hook = Arc::new(AtomicBool::new(false));

        registry.subscribe_at(
            crasher,
            &[EventKind::Message],
            ListenerPriority::Monitor,
            event_handler(|_ctx: Arc<EventContext>| {
                Box::pin(async move {
                    panic!("测试用炸弹");
                })
            }),
        );
        registry.subscribe_at(
            crasher,
            &[EventKind::Message],
            ListenerPriority::Normal,
            passes(same_plugin_second_hook.clone()),
        );
        registry.subscribe_at(
            bystander,
            &[EventKind::Message],
            ListenerPriority::Low,
            passes(survivor.clone()),
        );

        assert!(!registry.dispatch(&event("message", Some(11))).await);
        assert!(
            survivor.load(Ordering::SeqCst),
            "被隔离的是出事那一家，别家插件的钩子必须照跑"
        );
        assert!(
            !same_plugin_second_hook.load(Ordering::SeqCst),
            "出事插件在本次事件里剩下的钩子应跳过"
        );
        assert!(!gating.plugin_enabled(crasher), "达阈值应停用该插件");
        assert!(gating.plugin_enabled(bystander), "别家不该被牵连");
    }

    /// 同一个处理器重复订阅只算一条（热重载漏撤销时的兜底），换处理器才算新增；
    /// 每家的订阅条数另有上限，防止无界增长把每条事件都拖慢。
    #[tokio::test]
    async fn duplicate_subscription_collapses_and_count_is_capped() {
        let registry = registry(Arc::new(Gating::default()));
        let plugin = "HookDedupeTestPlugin";
        let shared = passes(Arc::new(AtomicBool::new(false)));
        registry.subscribe(plugin, &[EventKind::Message], shared.clone());
        registry.subscribe(plugin, &[EventKind::Message], shared.clone());
        assert_eq!(registry.hook_count(plugin), 1, "同一处理器重复登记应被忽略");
        registry.subscribe(
            plugin,
            &[EventKind::Message],
            passes(Arc::new(AtomicBool::new(false))),
        );
        assert_eq!(registry.hook_count(plugin), 2, "不同处理器各算一条");
        for _ in 0..(MAX_HOOKS_PER_PLUGIN + 8) {
            registry.subscribe(
                plugin,
                &[EventKind::Notice],
                passes(Arc::new(AtomicBool::new(false))),
            );
        }
        assert_eq!(
            registry.hook_count(plugin),
            MAX_HOOKS_PER_PLUGIN,
            "超出上限的订阅应被丢弃而不是无限增长"
        );
    }

    /// 非消息事件也要能投递；未知 post_type 直接忽略
    #[tokio::test]
    async fn notice_dispatches_and_unknown_post_type_does_not() {
        let registry = registry(Arc::new(Gating::default()));
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

    /// 事件入口已经解过一遍消息段，`dispatch_parsed` 必须用调用方那份结果，别再解一次
    #[tokio::test]
    async fn dispatch_parsed_reuses_the_callers_parse() {
        let registry = registry(Arc::new(Gating::default()));
        let plugin = "ParsedReuseTestPlugin";
        let seen = Arc::new(Mutex::new((String::new(), Vec::new())));
        let sink = seen.clone();
        registry.subscribe_where(
            plugin,
            &[BodyFilter::Kind(EventKind::Message)],
            ListenerPriority::Normal,
            event_handler(move |context: Arc<EventContext>| {
                let sink = sink.clone();
                Box::pin(async move {
                    *sink.lock().unwrap() = (
                        context.command_text.clone(),
                        context
                            .segments
                            .iter()
                            .map(|segment| format!("{segment:?}"))
                            .collect(),
                    );
                    HookFlow::Pass
                })
            }),
        );
        // 事件自带的是「早安」，传进去的解析结果是另一个值——钩子看到的必须是后者
        let parsed = protocol::ParsedMessage {
            segments: vec![MessageSegment::Text("入口解出来的那份".into())],
            text: "入口解出来的那份".into(),
            command_text: "/入口解出来的那份".into(),
        };
        assert!(
            !registry
                .dispatch_parsed(&event("message", Some(4)), &parsed)
                .await
        );
        let (command_text, segments) = seen.lock().unwrap().clone();
        assert_eq!(command_text, "/入口解出来的那份");
        assert_eq!(segments.len(), 1);
        assert!(segments[0].contains("入口解出来的那份"), "{segments:?}");
    }

    /// 群名/名片之外的细节字段从 raw 里取（GUI 与插件都靠这个判断子类型）
    #[test]
    fn context_reads_raw_detail_fields() {
        let context = EventContext {
            kind: EventKind::Notice,
            event: event("notice", Some(6)),
            body: EventBody::Notice(NoticeKind::GroupIncrease, NoticeInfo::default()),
            text: String::new(),
            command_text: String::new(),
            segments: Vec::new(),
            api: OneBotApi::global(),
        };
        assert_eq!(context.field_str("notice_type"), Some("group_increase"));
        assert_eq!(context.field_str("missing"), None);
        assert_eq!(context.group_id(), Some(6));
        assert_eq!(context.message_id(), Some(7));
        assert_eq!(context.body.kind(), Some(EventKind::Notice));
        assert_eq!(context.body.notice_kind(), Some(NoticeKind::GroupIncrease));
    }

    /// 用一段原始 JSON 造事件（细节字段全看 raw，正是实现端发来的样子）
    fn raw_event(raw: serde_json::Value) -> OneBotEvent {
        match protocol::parse_payload(&raw.to_string()).expect("应能解析成事件") {
            crate::onebot::ParsedPayload::Event(found) => found,
            other => panic!("应为事件：{other:?}"),
        }
    }

    /// NapCat / LLOneBot 的戳一戳、名片与头衔变更都写成 `notice_type="notify"`，
    /// 真正的类型在内层 sub_type/type：只看 notice_type 的话插件只能自己比字符串。
    #[test]
    fn notify_notices_fall_into_their_real_kind() {
        let cases: [(&str, NoticeKind); 4] = [
            (
                r#""sub_type":"poke","target_id":1,"operator_id":2"#,
                NoticeKind::Poke,
            ),
            (r#""type":"nudge""#, NoticeKind::Nudge),
            (r#""sub_type":"group_card""#, NoticeKind::GroupCardUpdate),
            (r#""sub_type":"honor""#, NoticeKind::GroupTitleUpdate),
        ];
        for (extra, expected) in cases {
            let raw: serde_json::Value = serde_json::from_str(&format!(
                r#"{{"post_type":"notice","notice_type":"notify",{extra}}}"#
            ))
            .unwrap();
            let body = EventBody::from_event(&raw_event(raw));
            assert_eq!(body.notice_kind(), Some(expected), "原始 JSON: {extra}");
        }
        // 认不出的 notify 仍归 Notify，不要瞎猜成别的子类
        let raw: serde_json::Value = serde_json::from_str(
            r#"{"post_type":"notice","notice_type":"notify","sub_type":"gift"}"#,
        )
        .unwrap();
        assert_eq!(
            EventBody::from_event(&raw_event(raw)).notice_kind(),
            Some(NoticeKind::Notify)
        );
    }

    /// 群邀请与主动申请在 v11 里同为 `request_type="group"`：漏了 sub_type 就会把邀请
    /// 当成申请，自动通过申请的插件因此会把机器人被人拉进群这件事放过去。
    #[test]
    fn invited_group_request_is_not_an_application() {
        let invite = EventBody::from_event(&raw_event(json!({
            "post_type": "request",
            "request_type": "group",
            "sub_type": "invite",
            "group_id": 123,
            "user_id": 456,
            "flag": "flag-1",
        })));
        let (kind, info) = match &invite {
            EventBody::Request(kind, info) => (*kind, info.clone()),
            other => panic!("应为请求事件：{other:?}"),
        };
        assert_eq!(kind, RequestKind::InviteGroup);
        assert_eq!(info.flag.as_deref(), Some("flag-1"), "处理凭据要能直接回传");
        assert_eq!(info.via.as_deref(), Some("invite"));

        let applied = EventBody::from_event(&raw_event(json!({
            "post_type": "request",
            "request_type": "group",
            "sub_type": "add",
            "comment": " 我是来加群的 ",
        })));
        let (kind, info) = match &applied {
            EventBody::Request(kind, info) => (*kind, info.clone()),
            other => panic!("应为请求事件：{other:?}"),
        };
        assert_eq!(kind, RequestKind::AddGroup);
        assert_eq!(
            info.comment.as_deref(),
            Some("我是来加群的"),
            "留言顺手裁掉空白"
        );

        // 实现端没给 sub_type 时退回"申请"，但绝不回到不可达的旧行为
        assert_eq!(
            RequestKind::parse("group"),
            RequestKind::AddGroup,
            "只有 request_type 时按主动申请处理"
        );
    }

    /// notice 的操作者/时长/动作要落到 EventBody：封禁类插件靠"谁干的"和"多久"分支
    #[test]
    fn notice_details_reach_the_body() {
        let body = EventBody::from_event(&raw_event(json!({
            "post_type": "notice",
            "notice_type": "group_ban",
            "sub_type": "ban",
            "group_id": 1,
            "user_id": 2,
            "operator_id": 3,
            "duration": 600,
        })));
        let info = match &body {
            EventBody::Notice(kind, info) => {
                assert_eq!(*kind, NoticeKind::GroupBan);
                info.clone()
            }
            other => panic!("应为通知事件：{other:?}"),
        };
        assert_eq!(info.action.as_deref(), Some("ban"));
        assert_eq!(info.operator_id, Some(3));
        assert_eq!(info.duration, Some(600));
        assert_eq!(body.display_name(), "通知:禁言(ban)");

        // 被踢与自行退群靠动作分开订阅
        let kicked = EventBody::from_event(&raw_event(json!({
            "post_type": "notice",
            "notice_type": "group_decrease",
            "sub_type": "kick",
            "user_id": 2,
            "operator_id": 3,
        })));
        assert!(
            BodyFilter::notice(NoticeKind::GroupDecrease, "kick").matches(&kicked),
            "被踢应命中 kick 订阅"
        );
        assert!(
            !BodyFilter::notice(NoticeKind::GroupDecrease, "leave").matches(&kicked),
            "被踢不该命中「自行退群」的订阅"
        );
        assert!(
            BodyFilter::Notice(NoticeKind::GroupDecrease).matches(&kicked),
            "整类订阅仍然命中"
        );
        assert_eq!(
            BodyFilter::notice(NoticeKind::GroupDecrease, "").display_name(),
            "通知:成员退群",
            "空动作就是整类"
        );
    }

    /// 发送者身份与引用：钩子侧要和命令侧看到同样的信息
    #[test]
    fn context_reads_sender_and_quote() {
        let raw = raw_event(json!({
            "post_type": "message",
            "message_type": "group",
            "group_id": 1,
            "user_id": 2,
            "sender": { "card": " 小雨 ", "nickname": "小雨的昵称", "role": "admin" },
            "message": [
                { "type": "reply", "data": { "id": 501 } },
                { "type": "text", "data": { "text": "抽卡" } },
            ],
        }));
        let context = EventContext {
            kind: EventKind::Message,
            event: raw,
            body: EventBody::GroupMessage {
                sub_type: Some("normal".into()),
            },
            text: "抽卡".into(),
            command_text: "抽卡".into(),
            segments: vec![
                MessageSegment::Reply(501),
                MessageSegment::Text("抽卡".into()),
            ],
            api: OneBotApi::global(),
        };
        assert_eq!(context.sender_name(), Some("小雨"), "名片优先于昵称");
        assert_eq!(
            context.sender_role(),
            Some(crate::runtime::dispatcher::GroupRole::Admin)
        );
        assert!(context.sender_is_admin());
        assert_eq!(context.quoted(), Some(501));
    }

    /// 私聊没有 sender 对象时各读取器要给 None，而不是空串或 panic
    #[test]
    fn context_reads_survive_missing_sender() {
        let raw = raw_event(json!({
            "post_type": "message",
            "message_type": "private",
            "user_id": 2,
            "message": [{ "type": "text", "data": { "text": "在吗" } }],
        }));
        let context = EventContext {
            kind: EventKind::Message,
            event: raw,
            body: EventBody::PrivateMessage { sub_type: None },
            text: "在吗".into(),
            command_text: "在吗".into(),
            segments: Vec::new(),
            api: OneBotApi::global(),
        };
        assert_eq!(context.sender_name(), None);
        assert_eq!(context.sender_role(), None);
        assert!(!context.sender_is_admin());
        assert_eq!(context.quoted(), None);
    }

    /// 绑了功能开关的钩子要跟着开关一起关：只关掉同名命令、留着钩子在收事件，
    /// 面板上显示"功能已关闭"就是假的
    #[tokio::test]
    async fn feature_switch_silences_the_hooks_bound_to_it() {
        let gating = Arc::new(Gating::default());
        gating.register_feature(
            crate::runtime::config::Feature {
                key: "history",
                name: "聊天记账",
                description: "记录群消息",
            },
            "HookFeaturePlugin",
        );
        let registry = registry(gating.clone());
        let plugin = "HookFeaturePlugin";
        let bound = Arc::new(AtomicBool::new(false));
        let unbound = Arc::new(AtomicBool::new(false));
        registry.subscribe_feature(
            plugin,
            "history",
            &[BodyFilter::GroupMessage],
            ListenerPriority::Normal,
            passes(bound.clone()),
        );
        registry.subscribe(plugin, &[EventKind::Notice], passes(unbound.clone()));
        let group_id = 777_002_i64;
        let mut settings = gating.group_settings();
        settings.insert(
            group_id.to_string(),
            crate::config::arona::GroupSetting {
                disabled_features: vec!["history".to_string()],
                ..Default::default()
            },
        );
        gating.set_group_settings(settings);

        let message = event("message", Some(group_id));
        assert!(!registry.dispatch(&message).await);
        assert!(
            !bound.load(Ordering::SeqCst),
            "该群关掉了 history 功能，绑它的钩子不该再收消息"
        );

        // 没绑开关的钩子不受牵连
        gating.set_group_settings(BTreeMap::new());
        assert!(!registry.dispatch(&event("notice", Some(group_id))).await);
        assert!(
            unbound.load(Ordering::SeqCst),
            "同一插件未绑开关的订阅仍要执行"
        );
    }

    /// 拼出全部文本段（断言改写结果用）
    fn text_of(message: &OutgoingMessage) -> String {
        message
            .segments
            .iter()
            .filter_map(|segment| match segment {
                MessageSegment::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .concat()
    }

    /// 追加一段文本的出站处理器（用来观察改写与顺序）
    fn outbound_append(marker: &'static str) -> Arc<dyn OutboundHandler> {
        outbound_handler(move |context: Arc<OutboundContext>| {
            Box::pin(async move {
                context.rewrite(|message| {
                    message
                        .segments
                        .push(MessageSegment::Text(marker.to_string()));
                });
            })
        })
    }

    fn outbound_cancel() -> Arc<dyn OutboundHandler> {
        outbound_handler(|context: Arc<OutboundContext>| Box::pin(async move { context.cancel() }))
    }

    /// 出站订阅者按优先级依次改写，任何一家 Cancel 就整条不发；改写结果仍要交回调用方
    #[tokio::test]
    async fn outbound_hooks_rewrite_in_priority_order_and_can_veto() {
        let registry = registry(Arc::new(Gating::default()));
        let plugin = "HookOutboundPlugin";
        registry.subscribe_outbound(plugin, "", ListenerPriority::High, outbound_append("[高]"));
        registry.subscribe_outbound(plugin, "", ListenerPriority::Low, outbound_append("[低]"));
        let mut message = OutgoingMessage::text("正文");
        assert!(
            registry
                .dispatch_outbound(MessageTarget::Group(21), &mut message)
                .await,
            "没人拦停时应放行"
        );
        assert_eq!(text_of(&message), "正文[高][低]", "改写应按优先级顺序叠加");

        registry.subscribe_outbound(plugin, "", ListenerPriority::Lowest, outbound_cancel());
        let mut vetoed = OutgoingMessage::text("正文");
        assert!(
            !registry
                .dispatch_outbound(MessageTarget::Private(7), &mut vetoed)
                .await,
            "Cancel 应让这条消息发不出去"
        );
        assert_eq!(
            text_of(&vetoed),
            "正文[高][低]",
            "拦停也要把已改写到什么程度告诉调用方（日志/记账要用）"
        );
    }

    /// 被停用的插件不该还有拦停别人出站消息的权力
    #[tokio::test]
    async fn disabled_plugin_cannot_touch_outbound() {
        let gating = Arc::new(Gating::default());
        let registry = registry(gating.clone());
        let blocker = "HookOutboundBlocker";
        let editor = "HookOutboundEditor";
        registry.subscribe_outbound(blocker, "", ListenerPriority::Monitor, outbound_cancel());
        registry.subscribe_outbound(
            editor,
            "",
            ListenerPriority::Normal,
            outbound_append("[改]"),
        );
        gating.set_disabled_plugins(vec![blocker.to_string()]);
        let mut message = OutgoingMessage::text("正文");
        assert!(
            registry
                .dispatch_outbound(MessageTarget::Group(22), &mut message)
                .await,
            "被停用插件的出站订阅不该参与"
        );
        assert_eq!(text_of(&message), "正文[改]");
    }

    /// 一家出站处理器 panic 不能把机器人的话整条吞掉；达阈值只停用出事那家
    #[tokio::test]
    async fn panicking_outbound_handler_does_not_swallow_the_message() {
        let gating = Arc::new(Gating::default());
        let health = Arc::new(crate::plugin::health::HealthBoard::new(1, gating.clone()));
        let registry = Arc::new(HookRegistry::new(gating.clone(), health));
        let crasher = "HookOutboundCrasher";
        registry.subscribe_outbound(crasher, "", ListenerPriority::Monitor, {
            outbound_handler(|_context: Arc<OutboundContext>| {
                Box::pin(async move {
                    panic!("测试用炸弹");
                })
            })
        });
        registry.subscribe_outbound(
            "HookOutboundBystander",
            "",
            ListenerPriority::Normal,
            outbound_append("[后]"),
        );
        let mut message = OutgoingMessage::text("正文");
        assert!(
            registry
                .dispatch_outbound(MessageTarget::Group(23), &mut message)
                .await,
            "一家出事不该让消息消失"
        );
        assert_eq!(text_of(&message), "正文[后]");
        assert!(!gating.plugin_enabled(crasher), "达阈值应停用出事那家");
    }

    /// 停用插件时入站与出站订阅一起收回（框架回收资源按返回值记日志）
    #[tokio::test]
    async fn unsubscribe_collects_both_directions() {
        let registry = registry(Arc::new(Gating::default()));
        let plugin = "HookOutboundRevokePlugin";
        registry.subscribe(
            plugin,
            &[EventKind::Message],
            passes(Arc::new(AtomicBool::new(false))),
        );
        registry.subscribe_outbound(plugin, "", ListenerPriority::Normal, outbound_append("[x]"));
        assert_eq!(registry.hook_count(plugin), 2, "出站订阅也要计入");
        assert_eq!(registry.unsubscribe(plugin), 2, "两个方向一起收回");
        assert_eq!(registry.hook_count(plugin), 0);
    }
}
