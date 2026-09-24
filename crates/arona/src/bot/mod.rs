//! 机器人对象模型（对应 mirai 的 `Bot` / `Group` / `Member` / `Friend` / `MessageSource`）。
//!
//! mirai 的手感是"先拿到一个对象，再往对象上发消息"：
//!
//! ```kotlin
//! val group = bot.getGroupOrFail(123)
//! group.sendMessage("你好")            // 发群消息
//! member.mute(600)                     // 禁言
//! val source = message.source()
//! source.recall()                      // 撤回
//! chain.quoteReplyTo(source)           // 引用
//! ```
//!
//! 这里给的是同一套形状，只是把 Kotlin 扩展属性换成 Rust 方法：
//!
//! ```ignore
//! let bot = arona::bot::Bot::current();
//! let group = bot.group(123);
//! group.send_text("你好").await;
//! group.member(456).await?.mute(600).await?;
//! let source = context.source().unwrap();
//! source.recall().await?;              // 撤回
//! source.reply(OutgoingMessage::text("收到")).await;  // 引用回复
//! ```
//!
//! 这些句柄都是**轻量的**：只装一个所属 [`Framework`] 实例的引用加几个 id，克隆零成本。
//! 它们刻意只经实例出口拿状态（`onebot()` / `chatlog_api()` / `runtime_services()`），
//! 不碰任何进程级 `static`，所以插件 dll 里用它们和宿主里用完全一样——
//! dll 认领的是宿主那套实例（判据见仓库根 `PLUGIN_DEVELOPMENT.md` §22）。
//! 碰 sqlite 的那几个方法一律走 `chatlog_api()` 而不是 `chatlog()`，原因见
//! [`crate::runtime::chatlog::ChatLogFace`]。

use crate::framework::Framework;
use crate::onebot::api::{
    FriendInfo, GroupInfo, GroupMemberInfo, LoginInfo, OneBotApi, OneBotError, StrangerInfo,
};
use crate::runtime::chatlog::ChatRecord;
use crate::runtime::message::{MessageReceipt, MessageSegment, MessageTarget, OutgoingMessage};
use std::sync::Arc;

/// 一台机器人（对应 mirai 的 `Bot`）。句柄轻量，随处可克隆。
#[derive(Clone)]
pub struct Bot {
    framework: Arc<Framework>,
}

impl Bot {
    /// 进程默认实例上那台机器人。绝大多数插件只要这个。
    pub fn current() -> Bot {
        Bot {
            framework: Framework::global_arc(),
        }
    }

    /// 指定实例上的机器人：多实例宿主与测试用，插件不必自己调
    pub fn of(framework: &Arc<Framework>) -> Bot {
        Bot {
            framework: framework.clone(),
        }
    }

    /// 所属的框架实例
    pub fn framework(&self) -> &Arc<Framework> {
        &self.framework
    }

    /// 底层 OneBot 动作出口：本模块没包到的动作走它（`call`/`call_typed` 是万能通道）。
    ///
    /// 注意 [`OneBotApi`] 是无状态零宽类型，它取连接时读的是**进程默认实例**上的
    /// [`OnebotOutlet`](crate::onebot::application::OnebotOutlet)。宿主与插件 dll（经
    /// `adopt_host` 认领了宿主实例）拿到的都是同一条真实连接，所以这里没问题；
    /// 只有测试里 `Bot::of(&另一套实例)` 时，`api()` 仍会落到默认实例——
    /// 需要严格隔离时用 [`Bot::send`] / [`Bot::recall`] / [`Bot::message`]，它们读的是 self 这套。
    pub fn api(&self) -> OneBotApi {
        OneBotApi::global()
    }

    /// 机器人 QQ 号（onebot.yml 的 `self_id`）
    pub fn self_id(&self) -> i64 {
        self.framework.onebot().self_id()
    }

    /// 昵称（onebot.yml 里那份，热重载后跟着变）
    pub fn nickname(&self) -> String {
        self.framework
            .onebot()
            .application()
            .map(|application| application.config().nickname)
            .unwrap_or_default()
    }

    /// 当前是否连着 OneBot 实现端
    pub fn is_online(&self) -> bool {
        self.framework.onebot().is_online()
    }

    /// `get_login_info` 向实现端确认登录身份
    pub async fn login_info(&self) -> Result<LoginInfo, OneBotError> {
        self.api().get_login_info().await
    }

    // ==================== 会话对象 ====================

    /// `get_group_list` 已加入的全部群
    pub async fn groups(&self) -> Result<Vec<Group>, OneBotError> {
        Ok(self
            .api()
            .get_group_list()
            .await?
            .into_iter()
            .map(|info| Group::new(self.clone(), info.group_id))
            .collect())
    }

    /// 取一个群句柄。不查网络：mirai 的 `bot.getGroup(id)` 同样只给对象，
    /// 群名一类的资料要用 [`Group::info`] 现取。
    pub fn group(&self, group_id: i64) -> Group {
        Group::new(self.clone(), group_id)
    }

    /// `get_friend_list` 好友列表
    pub async fn friends(&self) -> Result<Vec<Friend>, OneBotError> {
        Ok(self
            .api()
            .get_friend_list()
            .await?
            .into_iter()
            .map(|info| Friend::new(self.clone(), info))
            .collect())
    }

    /// 取一个好友句柄（不查网络，所以没有昵称与备注；要资料用 [`Bot::friends`] 或 [`Bot::user`]）
    pub fn friend(&self, user_id: i64) -> Friend {
        Friend::new(
            self.clone(),
            FriendInfo {
                user_id,
                ..Default::default()
            },
        )
    }

    /// `get_stranger_info` 查一个人的资料（昵称/性别/年龄）
    pub async fn user(&self, user_id: i64) -> Result<StrangerInfo, OneBotError> {
        self.api().get_stranger_info(user_id, false).await
    }

    // ==================== 收发 ====================

    /// 发一条消息（对应 mirai 的 `subject.sendMessage`）。
    ///
    /// 走框架的出站通道：出站钩子、控制台打印、合并转发拆分、`revoke_after_millis`
    /// 自动撤回与出站记账都在这条路上。直连 `api().send_group_msg(..)` 是**协议层旁路**，
    /// 这些一概不做，除非你确实要绕开钩子。
    pub async fn send(&self, target: MessageTarget, message: OutgoingMessage) -> MessageReceipt {
        self.framework
            .runtime_services()
            .send_message(target, message)
            .await
    }

    /// 发一条纯文本
    pub async fn send_text(
        &self,
        target: MessageTarget,
        text: impl Into<String>,
    ) -> MessageReceipt {
        self.send(target, OutgoingMessage::text(text)).await
    }

    /// 撤回一条消息（对应 `messageSource.recall()`）。
    /// 传你见过的任意一个号即可：框架的聊天记录库会把它换成实现端认的那个 id。
    ///
    /// 这里一律取 `chatlog_api()` 而不是 `chatlog()`：本模块的方法会被静态编译进插件 dll，
    /// 而 dll 自带一份 bundled sqlite——只有经宿主生成的 vtable 调用，那条连接才始终由
    /// 宿主那份引擎操作（详见 [`crate::runtime::chatlog::ChatLogFace`]）。
    pub async fn recall(&self, message_id: i64) -> Result<(), OneBotError> {
        self.framework.chatlog_api().recall(message_id).await
    }

    /// 查框架留档里的这条消息（引用还原与撤回的依据，`data/arona/chatlog.db`）
    pub fn message(&self, message_id: i64) -> Option<ChatRecord> {
        self.framework.chatlog_api().find(message_id)
    }
}

/// 一个群（对应 mirai 的 `Group`，同时也是 `Subject`）。
#[derive(Clone)]
pub struct Group {
    bot: Bot,
    group_id: i64,
}

impl Group {
    fn new(bot: Bot, group_id: i64) -> Group {
        Group { bot, group_id }
    }

    /// 群号
    pub fn id(&self) -> i64 {
        self.group_id
    }

    /// 所属机器人
    pub fn bot(&self) -> &Bot {
        &self.bot
    }

    /// `get_group_info` 群名、成员数、群主
    pub async fn info(&self) -> Result<GroupInfo, OneBotError> {
        self.bot.api().get_group_info(self.group_id, false).await
    }

    /// 本会话的发送目标
    pub fn target(&self) -> MessageTarget {
        MessageTarget::Group(self.group_id)
    }

    /// `Group.sendMessage`：往群里发一条消息
    pub async fn send(&self, message: OutgoingMessage) -> MessageReceipt {
        self.bot.send(self.target(), message).await
    }

    /// 往群里发一条纯文本
    pub async fn send_text(&self, text: impl Into<String>) -> MessageReceipt {
        self.bot
            .send(self.target(), OutgoingMessage::text(text))
            .await
    }

    /// `get_group_member_list` 全部成员
    pub async fn members(&self) -> Result<Vec<GroupMemberInfo>, OneBotError> {
        self.bot.api().get_group_member_list(self.group_id).await
    }

    /// `get_group_member_info` 取一个成员（会查一次实现端，确认这个人真在群里）
    pub async fn member(&self, user_id: i64) -> Result<Member, OneBotError> {
        self.bot
            .api()
            .get_group_member_info(self.group_id, user_id, false)
            .await?;
        Ok(self.member_of(user_id))
    }

    /// 拿成员句柄但不查网络：紧接着要操作（禁言/踢人）时用它更省一次往返
    pub fn member_of(&self, user_id: i64) -> Member {
        Member {
            bot: self.bot.clone(),
            group_id: self.group_id,
            user_id,
        }
    }

    /// `Member.mute`：禁言某人 `seconds` 秒（0 表示解禁）
    pub async fn mute(&self, user_id: i64, seconds: u64) -> Result<(), OneBotError> {
        self.bot
            .api()
            .set_group_ban(self.group_id, user_id, seconds)
            .await
    }

    /// 解除某人的禁言
    pub async fn unmute(&self, user_id: i64) -> Result<(), OneBotError> {
        self.mute(user_id, 0).await
    }

    /// `GroupSettings.isMuteAll`：全体禁言
    pub async fn mute_all(&self, enabled: bool) -> Result<(), OneBotError> {
        self.bot
            .api()
            .set_group_whole_ban(self.group_id, enabled)
            .await
    }

    /// `Member.kick`：踢出成员，`reject_add` 表示同时拒绝其后续加群申请
    pub async fn kick(&self, user_id: i64, reject_add: bool) -> Result<(), OneBotError> {
        self.bot
            .api()
            .set_group_kick(self.group_id, user_id, reject_add)
            .await
    }

    /// `Member.permit` / 取消授权：设置管理员
    pub async fn set_admin(&self, user_id: i64, admin: bool) -> Result<(), OneBotError> {
        self.bot
            .api()
            .set_group_admin(self.group_id, user_id, admin)
            .await
    }

    /// `Group.settings.name`：改群名
    pub async fn set_name(&self, name: &str) -> Result<(), OneBotError> {
        self.bot.api().set_group_name(self.group_id, name).await
    }

    /// `Member.nameCard`：改群名片（空串表示清除）
    pub async fn set_card(&self, user_id: i64, card: &str) -> Result<(), OneBotError> {
        self.bot
            .api()
            .set_group_card(self.group_id, user_id, card)
            .await
    }

    /// `Group.announce`：发群公告。`image` 是本地图片路径，`None` 表示纯文字公告
    pub async fn set_notice(
        &self,
        content: &str,
        image: Option<&str>,
        pinned: bool,
    ) -> Result<(), OneBotError> {
        self.bot
            .api()
            .set_group_notice(self.group_id, content, image, pinned)
            .await
    }
}

/// 群成员（对应 mirai 的 `Member`）。
#[derive(Clone)]
pub struct Member {
    bot: Bot,
    group_id: i64,
    user_id: i64,
}

impl Member {
    /// 所属群
    pub fn group(&self) -> Group {
        Group::new(self.bot.clone(), self.group_id)
    }

    /// QQ 号
    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// 群号
    pub fn group_id(&self) -> i64 {
        self.group_id
    }

    /// `MemberInfo`：名片、昵称、入群时间、头衔与身份
    pub async fn info(&self) -> Result<GroupMemberInfo, OneBotError> {
        self.bot
            .api()
            .get_group_member_info(self.group_id, self.user_id, false)
            .await
    }

    /// `Member.mute`
    pub async fn mute(&self, seconds: u64) -> Result<(), OneBotError> {
        self.group().mute(self.user_id, seconds).await
    }

    /// `Member.unmute`
    pub async fn unmute(&self) -> Result<(), OneBotError> {
        self.group().unmute(self.user_id).await
    }

    /// `Member.kick`
    pub async fn kick(&self, reject_add: bool) -> Result<(), OneBotError> {
        self.group().kick(self.user_id, reject_add).await
    }

    /// `Member.permit`：设为管理员（`false` 取消）
    pub async fn set_admin(&self, admin: bool) -> Result<(), OneBotError> {
        self.group().set_admin(self.user_id, admin).await
    }

    /// `Member.nameCard`
    pub async fn set_card(&self, card: &str) -> Result<(), OneBotError> {
        self.group().set_card(self.user_id, card).await
    }

    /// 私聊这个人（mirai 里对应 `User.asFriend().sendMessage`）：裸私聊，对方不是好友时
    /// 不少实现端会拒发，那种场合用 [`Member::send_temp`]
    pub async fn send(&self, message: OutgoingMessage) -> MessageReceipt {
        self.bot
            .send(MessageTarget::Private(self.user_id), message)
            .await
    }

    /// mirai 的 `Temp` 会话目标：以"同群成员"的身份单聊这个人
    pub fn temp_target(&self) -> MessageTarget {
        MessageTarget::Temp {
            group_id: self.group_id,
            user_id: self.user_id,
        }
    }

    /// `Temp.sendMessage`：走临时会话发一条。对方不是好友时这是唯一发得进去的路
    pub async fn send_temp(&self, message: OutgoingMessage) -> MessageReceipt {
        self.bot.send(self.temp_target(), message).await
    }
}

/// 好友（对应 mirai 的 `Friend`，也是 `Subject`）。
/// 资料就是 `get_friend_list` 那一项，`Bot::friend(id)` 现取的句柄则只有 QQ 号。
#[derive(Clone)]
pub struct Friend {
    bot: Bot,
    info: FriendInfo,
}

impl Friend {
    fn new(bot: Bot, info: FriendInfo) -> Friend {
        Friend { bot, info }
    }

    /// QQ 号
    pub fn user_id(&self) -> i64 {
        self.info.user_id
    }

    /// 所属机器人
    pub fn bot(&self) -> &Bot {
        &self.bot
    }

    /// 原始资料（性别、年龄、实现端多回的字段都在 `rest` 里）
    pub fn info(&self) -> &FriendInfo {
        &self.info
    }

    /// 昵称（`Bot::friend(id)` 现取的句柄没查过网络，这里是空串）
    pub fn nickname(&self) -> &str {
        &self.info.nickname
    }

    /// 备注优先，其次昵称；两个都没有时给个占位，避免回复里出现空名字
    pub fn display(&self) -> &str {
        let name = self.info.display();
        if name.is_empty() {
            "未知用户"
        } else {
            name
        }
    }

    /// `Friend.sendMessage`
    pub async fn send(&self, message: OutgoingMessage) -> MessageReceipt {
        self.bot
            .send(MessageTarget::Private(self.info.user_id), message)
            .await
    }

    /// 给好友发一条纯文本
    pub async fn send_text(&self, text: impl Into<String>) -> MessageReceipt {
        self.send(OutgoingMessage::text(text)).await
    }
}

/// 一条消息的凭据（对应 mirai 的 `MessageSource`）：撤回与引用都拿它。
///
/// mirai 的 `MessageSource` 只活在进程内存里，机器人一重启就再也引用不到旧消息；
/// 这里多一层框架的聊天记录库（`runtime::chatlog`），所以只要那条消息被记过档，
/// 隔天引用也还原得出来。
#[derive(Clone)]
pub struct MessageSource {
    bot: Bot,
    /// 实现端给的消息号（可能只是临时号，撤回时框架会换成库里那个）
    pub message_id: i64,
    /// 所在群；`None` 表示私聊
    pub group_id: Option<i64>,
    /// 发送者
    pub sender: i64,
    /// 消息时间（秒）；实现端没给时是 0，引用窗口按"现在"算
    pub time: i64,
}

impl MessageSource {
    /// 手上只有消息号时构造：群号与时间从框架的留档里补
    pub fn of(message_id: i64) -> MessageSource {
        let bot = Bot::current();
        let record = bot.message(message_id);
        MessageSource {
            bot,
            message_id,
            group_id: record
                .as_ref()
                .and_then(|r| (r.group > 0).then_some(r.group)),
            sender: record.as_ref().map(|r| r.qq).unwrap_or_default(),
            time: record.map(|r| r.time).unwrap_or_default(),
        }
    }

    /// 所属机器人
    pub fn bot(&self) -> &Bot {
        &self.bot
    }

    /// 这条消息所在的会话
    pub fn target(&self) -> MessageTarget {
        match self.group_id {
            Some(group) => MessageTarget::Group(group),
            None => MessageTarget::Private(self.sender),
        }
    }

    /// `source.toQuote()`：拿去挂进要发的消息里（对应 mirai 的 `QuoteReply`）
    pub fn quote(&self) -> MessageSegment {
        MessageSegment::Reply(self.message_id)
    }

    /// `source.recall()`：撤回这条消息
    pub async fn recall(&self) -> Result<(), OneBotError> {
        self.bot.recall(self.message_id).await
    }

    /// 引用回复这条消息：协议层还引用得到就挂原生引用，来不及了由框架的留档还原成文字+图。
    /// 这条消息本身没有被引用时等同于 [`Bot::send`]。
    pub async fn reply(&self, mut message: OutgoingMessage) -> MessageReceipt {
        if let Some(prefix) = self
            .bot
            .framework()
            .chatlog_api()
            .quote_prefix(Some(self.message_id), self.time)
            .await
        {
            message.segments.splice(0..0, prefix);
        }
        self.bot.send(self.target(), message).await
    }

    /// 引用回复一段纯文本
    pub async fn reply_text(&self, text: impl Into<String>) -> MessageReceipt {
        self.reply(OutgoingMessage::text(text)).await
    }

    /// 框架留档里的那条记录（没有说明没记过档，或已被清理）
    pub async fn record(&self) -> Option<ChatRecord> {
        self.bot
            .framework()
            .chatlog_api()
            .lookup(self.message_id)
            .await
    }

    /// mirai 的 `messageSource.markRead()`：把这条标成已读（清红点）。
    /// 实现端不支持时返回 [`OneBotError::Failed`]，消息本身不受影响。
    pub async fn mark_read(&self) -> Result<(), OneBotError> {
        self.bot.api().mark_msg_as_read(self.message_id).await
    }
}

/// mirai 的 `subject.sendMessage(): MessageSource`：发出去的那条也能直接撤回、引用。
///
/// 实现端不给号时（发送器未就绪、被出站钩子拦下）是 `None`——那条消息到底有没有落进
/// 会话都不确定，此时不该凭"发过"就撤回。NapCat / LLOWeb 只回 `real_id` 的那些按它取。
impl MessageReceipt {
    /// 这条回执对应消息的凭据
    pub fn source(&self) -> Option<MessageSource> {
        self.message_id.or(self.real_id).map(MessageSource::of)
    }
}

/// mirai 的 `message.source()`：从命令上下文取出这条触发消息的凭据
impl crate::runtime::dispatcher::CommandContext {
    /// 触发本次命令的那条消息的凭据；实现端没给 `message_id` 时是 `None`
    pub fn source(&self) -> Option<MessageSource> {
        let message_id = self.message_id?;
        Some(MessageSource {
            bot: Bot::current(),
            message_id,
            group_id: self.group_id,
            sender: self.user_id,
            time: crate::runtime::chatlog::to_secs(self.time),
        })
    }
}

/// mirai 的 `message.source()`：从事件上下文取出这条消息的凭据
impl crate::onebot::hooks::EventContext {
    /// 本事件那条消息的凭据；事件不带 `message_id`（notice/request 多为如此）时是 `None`
    pub fn source(&self) -> Option<MessageSource> {
        let message_id = self.message_id()?;
        Some(MessageSource {
            bot: Bot::current(),
            message_id,
            group_id: self.group_id(),
            sender: self.user_id(),
            time: crate::runtime::chatlog::to_secs(self.event.time),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::message::{BoxFuture, MessageSender};
    use std::sync::Mutex;

    /// 只用来填满 `CommandContext.sender`：这几个用例不发消息，只想看凭据怎么拼
    struct SilentSender;

    impl MessageSender for SilentSender {
        fn send<'a>(
            &'a self,
            _target: MessageTarget,
            _message: OutgoingMessage,
        ) -> BoxFuture<'a, MessageReceipt> {
            Box::pin(async { MessageReceipt::default() })
        }
    }

    /// 记下每次出站的目标与首段文本，用来判断消息到底交给了哪套实例
    #[derive(Default)]
    struct RecordingSender(Mutex<Vec<(MessageTarget, String)>>);

    impl MessageSender for RecordingSender {
        fn send<'a>(
            &'a self,
            target: MessageTarget,
            message: OutgoingMessage,
        ) -> BoxFuture<'a, MessageReceipt> {
            let text = match message.segments.first() {
                Some(MessageSegment::Text(text)) => text.clone(),
                Some(other) => format!("{other:?}"),
                None => String::new(),
            };
            self.0.lock().unwrap().push((target, text));
            Box::pin(async { MessageReceipt::new(Some(1)) })
        }
    }

    /// 句柄必须挂在调用它的那套实例上：插件 dll 里 `Bot::current()` 经 `adopt_host`
    /// 拿到的是宿主实例，测试里 `Bot::of` 给的是隔离实例，两者互不串台。
    #[test]
    fn handles_carry_the_instance_they_were_built_from() {
        let one = Framework::new();
        let other = Framework::new();
        let bot = Bot::of(&one);
        assert_eq!(Arc::as_ptr(bot.framework()), Arc::as_ptr(&one));
        assert_ne!(
            Arc::as_ptr(bot.framework()),
            Arc::as_ptr(&other),
            "另一套实例不该被别人的句柄拖进来"
        );
        // 未认领应用的实例上，机器人在"不在线"这一侧，且不会去碰全局状态
        assert_eq!(bot.self_id(), 0);
        assert!(!bot.is_online());
        let group = bot.group(20001);
        assert_eq!(group.id(), 20001);
        assert!(matches!(group.target(), MessageTarget::Group(20001)));
        assert_eq!(group.bot().self_id(), 0);
    }

    /// 目标解析：群里的那条→群，私聊的那条→人。引用与撤回都靠它落回原会话。
    #[test]
    fn source_resolves_the_original_conversation() {
        let bot = Bot::of(&Framework::new());
        let in_group = MessageSource {
            bot: bot.clone(),
            message_id: 5,
            group_id: Some(20001),
            sender: 100,
            time: 0,
        };
        assert_eq!(in_group.target(), MessageTarget::Group(20001));
        assert!(matches!(in_group.quote(), MessageSegment::Reply(5)));
        let in_private = MessageSource {
            message_id: 6,
            group_id: None,
            ..in_group.clone()
        };
        assert_eq!(in_private.target(), MessageTarget::Private(100));
    }

    /// 毫秒时间戳要折成秒：不然引用窗口永远判成"还来得及"，留档也清不掉
    #[test]
    fn command_source_folds_millisecond_timestamps() {
        let context = crate::runtime::dispatcher::CommandContext {
            user_id: 100,
            group_id: Some(20001),
            text: String::new(),
            sender_name: None,
            is_admin: false,
            sender_role: None,
            message_id: Some(7),
            time: 1_700_000_000_000,
            quoted: None,
            segments: Vec::new(),
            sender: Arc::new(SilentSender),
        };
        let source = context.source().expect("带了 message_id 就该有凭据");
        assert_eq!(source.message_id, 7);
        assert_eq!(source.time, 1_700_000_000);
        assert_eq!(source.group_id, Some(20001));

        let mute = crate::runtime::dispatcher::CommandContext {
            message_id: None,
            ..context
        };
        assert!(
            mute.source().is_none(),
            "没给 message_id 就不该编出一个凭据"
        );
    }

    /// 出站一律交给**建出这个句柄的那套实例**上登记的发送器：Group/Friend/Member 三个
    /// 会话对象拼的目标要各自对得上，且都不碰进程默认实例。哪天对象模型又绕过
    /// `framework.runtime_services()` 去读 global，这条先红。
    #[tokio::test]
    async fn every_subject_sends_through_the_sender_of_its_own_instance() {
        let one = Framework::new();
        let recorder = Arc::new(RecordingSender::default());
        one.runtime_services().set_message_sender(recorder.clone());
        let bot = Bot::of(&one);

        let receipt = bot.group(20001).send_text("早").await;
        assert_eq!(receipt.message_id, Some(1), "回执来自本实例登记的发送器");
        bot.friend(100).send_text("在吗").await;
        bot.group(20001)
            .member_of(100)
            .send(OutgoingMessage::text("私聊你一下"))
            .await;
        bot.send_text(MessageTarget::Private(100), "直发").await;
        bot.group(20001)
            .member_of(100)
            .send_temp(OutgoingMessage::text("单聊你"))
            .await;

        let sent = recorder.0.lock().unwrap();
        assert_eq!(
            sent.len(),
            5,
            "五次出站都该记在同一份台账上，一次都没漏给别的实例"
        );
        assert_eq!(
            **sent,
            vec![
                (MessageTarget::Group(20001), "早".to_string()),
                (MessageTarget::Private(100), "在吗".to_string()),
                (MessageTarget::Private(100), "私聊你一下".to_string()),
                (MessageTarget::Private(100), "直发".to_string()),
                (
                    MessageTarget::Temp {
                        group_id: 20001,
                        user_id: 100
                    },
                    "单聊你".to_string()
                ),
            ]
        );
        // 另一套没装配过的实例照原行为给空回执，不会借用这一家的发送器
        let idle = Bot::of(&Framework::new());
        assert_eq!(idle.group(20001).send_text("早").await.message_id, None);
    }

    /// 回执要能直接变回凭据：mirai 的 `sendMessage()` 返回的就是 `MessageSource`，
    /// 「发完就撤」不该让插件自己拆字段。只给 `real_id` 的实现端也要认。
    #[test]
    fn receipts_become_the_source_of_the_message_they_acknowledge() {
        let sent = MessageReceipt {
            message_id: Some(1024),
            real_id: Some(2048),
        };
        assert_eq!(sent.source().expect("给了号就该有凭据").message_id, 1024);

        let only_real = MessageReceipt {
            message_id: None,
            real_id: Some(2048),
        };
        assert_eq!(only_real.source().expect("real_id 也算号").message_id, 2048);

        assert!(
            MessageReceipt::default().source().is_none(),
            "一个号都没给时不该编出凭据"
        );
    }
}
