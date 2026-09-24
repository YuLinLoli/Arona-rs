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
use crate::runtime::chatlog::{ChatRecord, HistoryCursor, HistoryPage, HistoryScope};
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
    /// [`OneBotApi`] 挂在**这套实例**上，取的是 `self.framework` 那份
    /// [`OnebotOutlet`](crate::onebot::application::OnebotOutlet)：`Bot::of(&另一套实例)`
    /// 之后动作仍然落在原来那条连接上的问题已经没有了，多实例宿主与测试里插件看到的
    /// 都是自己这一套的连接、self_id 与缓存。
    pub fn api(&self) -> OneBotApi {
        OneBotApi::of(&self.framework)
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

    /// `get_stranger_info` 查一个人的资料（昵称/性别/年龄）。
    /// 这个动作在 NTQQ 系实现端上偏慢，实例上的缓存留 10 分钟（[`PeerCache`](crate::runtime::peers::PeerCache)）
    pub async fn user(&self, user_id: i64) -> Result<StrangerInfo, OneBotError> {
        let cache = self.framework().peers();
        if let Some(info) = cache.strangers().get(&user_id) {
            return Ok(info);
        }
        let info = self.api().get_stranger_info(user_id, false).await?;
        cache.strangers().put(user_id, info.clone());
        Ok(info)
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

    /// mirai 的 `group.history`：往前翻这个群说过的话（§21 那份留档，从新到旧）。
    /// 不发协议请求，所以查到的是"机器人见过的"那部分。
    pub fn history(&self, limit: i64) -> HistoryPage {
        self.history_at(limit, None)
    }

    /// 接着上一页往前翻：`cursor` 用上一页 [`HistoryPage::cursor`] 给出的那个，`None` 表示到头了
    pub fn history_before(&self, limit: i64, cursor: HistoryCursor) -> HistoryPage {
        self.history_at(limit, Some(cursor))
    }

    fn history_at(&self, limit: i64, before: Option<HistoryCursor>) -> HistoryPage {
        self.bot
            .framework()
            .chatlog_api()
            .recent(HistoryScope::Group(self.group_id), limit, before)
    }

    /// `get_group_member_list` 全部成员
    pub async fn members(&self) -> Result<Vec<GroupMemberInfo>, OneBotError> {
        self.bot.api().get_group_member_list(self.group_id).await
    }

    /// `get_group_member_info` 取一个成员（会查一次实现端，确认这个人真在群里）
    pub async fn member(&self, user_id: i64) -> Result<Member, OneBotError> {
        self.member_info(user_id).await?;
        Ok(self.member_of(user_id))
    }

    /// mirai 的 `group[id]`：成员资料由实例上的 [`PeerCache`](crate::runtime::peers::PeerCache)
    /// 按需读一次、留 60 秒，同一个人在同一群里连续问不会反复打网络。
    /// 命令的权限门控（[`CommandContext::group_role`](crate::runtime::dispatcher::CommandContext::group_role)）
    /// 用的也是这一份，所以"先判身份再读昵称"只花一次往返。
    pub async fn member_info(&self, user_id: i64) -> Result<GroupMemberInfo, OneBotError> {
        let key = (self.group_id, user_id);
        let cache = self.bot.framework().peers();
        if let Some(info) = cache.members().get(&key) {
            return Ok(info);
        }
        let info = self
            .bot
            .api()
            .get_group_member_info(self.group_id, user_id, false)
            .await?;
        cache.members().put(key, info.clone());
        Ok(info)
    }

    /// 立刻作废这个人在本群的资料：改了群名片、升了降了管理员之后自己读自己写，
    /// 不这样做的话接下来 60 秒读到的还是旧身份
    pub fn forget_member(&self, user_id: i64) {
        self.bot
            .framework()
            .peers()
            .forget_member(self.group_id, user_id);
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
        let result = self
            .bot
            .api()
            .set_group_ban(self.group_id, user_id, seconds)
            .await;
        self.forget_member(user_id);
        result
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
        let result = self
            .bot
            .api()
            .set_group_kick(self.group_id, user_id, reject_add)
            .await;
        self.forget_member(user_id);
        result
    }

    /// `Member.permit` / 取消授权：设置管理员
    pub async fn set_admin(&self, user_id: i64, admin: bool) -> Result<(), OneBotError> {
        let result = self
            .bot
            .api()
            .set_group_admin(self.group_id, user_id, admin)
            .await;
        // 刚升上去的管理员不该在接下来 60 秒里仍被身份门控当普通成员
        self.forget_member(user_id);
        result
    }

    /// `Group.settings.name`：改群名
    pub async fn set_name(&self, name: &str) -> Result<(), OneBotError> {
        self.bot.api().set_group_name(self.group_id, name).await
    }

    /// `Member.nameCard`：改群名片（空串表示清除）
    pub async fn set_card(&self, user_id: i64, card: &str) -> Result<(), OneBotError> {
        let result = self
            .bot
            .api()
            .set_group_card(self.group_id, user_id, card)
            .await;
        self.forget_member(user_id);
        result
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

    /// `MemberInfo`：名片、昵称、入群时间、头衔与身份。
    /// 走实例上的按需读缓存（60 秒），见 [`Group::member_info`]
    pub async fn info(&self) -> Result<GroupMemberInfo, OneBotError> {
        self.group().member_info(self.user_id).await
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

    /// mirai 的 `user.history`：往前翻与这个人的私聊留档（含机器人自己说出去的）
    pub fn history(&self, limit: i64) -> HistoryPage {
        self.history_at(limit, None)
    }

    /// 接着上一页往前翻
    pub fn history_before(&self, limit: i64, cursor: HistoryCursor) -> HistoryPage {
        self.history_at(limit, Some(cursor))
    }

    fn history_at(&self, limit: i64, before: Option<HistoryCursor>) -> HistoryPage {
        self.bot.framework().chatlog_api().recent(
            HistoryScope::Private(self.info.user_id),
            limit,
            before,
        )
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

/// 一张可以反复发的图片（对应 mirai 的 `Image` / `ExternalImage` 消息元素）。
///
/// mirai 里图片是消息链上的一个元素，拿到就到处发；OneBot 各家实现端给的线索不一样
/// （v11 给直链、NTQQ 系给 `file_id`、也有实现端只给本地缓存路径），所以这个句柄把四种
/// 来源收在一起，[`Image::segment`] 时按「直链 > 本地路径 > 字节 > `file_id`」的优先级落地
/// （和协议层 [`crate::onebot::protocol`] 的取值顺序一致，几个都在时一并带上）。
///
/// 它是 `Clone` 的且不含任何连接状态，可以直接放进插件自己的缓存里长期持有。
/// **没有 `upload` 这一步**：OneBot v11 没有"先传上去换个 id 回来"的标准动作，
/// 发图时由实现端自己上传；能复用的 `file_id` 只可能来自入站消息
/// （[`Image::from_segment`]）或框架留档（[`Image::from_stored`]）。
#[derive(Clone, Debug, Default)]
pub struct Image {
    url: Option<String>,
    path: Option<String>,
    /// 图片字节：发出时框架直接转 base64 带走，不落机器人硬盘
    bytes: Option<Arc<[u8]>>,
    file_id: Option<String>,
}

impl Image {
    /// 从图片字节构造（渲染出来的攻略图/日历图这类现生成的内容）
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Image {
        Image {
            bytes: Some(Arc::from(bytes.as_ref())),
            ..Default::default()
        }
    }

    /// 从机器人本机的文件路径构造：实现端按路径读文件上传，路径必须在实现端进程读得到
    pub fn from_file(path: impl Into<String>) -> Image {
        Image {
            path: Some(path.into()),
            ..Default::default()
        }
    }

    /// 从直链构造。QQ 的图床直链带签名、会过期，长期持有请优先用 [`Image::from_file_id`]
    pub fn from_url(url: impl Into<String>) -> Image {
        Image {
            url: Some(url.into()),
            ..Default::default()
        }
    }

    /// 从实现端给的图片标识构造。**只有给过它的那个实现端认得**，
    /// 换实现端或跨账号都不保证还能取到
    pub fn from_file_id(file_id: impl Into<String>) -> Image {
        Image {
            file_id: Some(file_id.into()),
            ..Default::default()
        }
    }

    /// 从收到的图片段原样取出：几种线索有哪个留哪个，等下一并带回，不在这一步猜该用哪个
    pub fn from_segment(segment: &MessageSegment) -> Option<Image> {
        let MessageSegment::Image {
            url,
            file,
            data,
            file_id,
        } = segment
        else {
            return None;
        };
        let image = Image {
            url: non_empty(url.as_deref()),
            path: non_empty(file.as_deref()),
            bytes: data.as_ref().map(|bytes| Arc::from(bytes.as_slice())),
            file_id: non_empty(file_id.as_deref()),
        };
        (!image.is_empty()).then_some(image)
    }

    /// 从框架聊天记录库 `image` 列存的那个字符串还原。
    /// 那一列三种写法都可能出现：`fileid://<id>`（实现端只给了标识）、http(s) 直链、本地路径
    pub fn from_stored(value: &str) -> Option<Image> {
        if let Some(id) = value.strip_prefix(crate::runtime::chatlog::FILE_ID_SCHEME) {
            return non_empty(Some(id)).map(Image::from_file_id);
        }
        non_empty(Some(value)).map(|value| {
            if value.starts_with("http://") || value.starts_with("https://") {
                Image::from_url(value)
            } else {
                Image::from_file(value)
            }
        })
    }

    /// 一个线索都没有（收到的是既没链接、也没路径、还没字节的空图片段）
    pub fn is_empty(&self) -> bool {
        self.url.is_none() && self.path.is_none() && self.bytes.is_none() && self.file_id.is_none()
    }

    /// 实现端给的图片标识，没给过时 `None`
    pub fn file_id(&self) -> Option<&str> {
        self.file_id.as_deref()
    }

    /// 直链，没给过时 `None`
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    /// 机器人本机上的文件路径，没给过时 `None`
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// 转成消息段，挂进要发的消息里
    pub fn segment(&self) -> MessageSegment {
        MessageSegment::Image {
            url: self.url.clone(),
            file: self.path.clone(),
            data: self.bytes.as_ref().map(|bytes| bytes.to_vec()),
            file_id: self.file_id.clone(),
        }
    }
}

/// 实现端经常回空串而不是干脆不给这个字段，一律折成 `None`
fn non_empty(value: Option<&str>) -> Option<String> {
    Some(value?.to_string()).filter(|value| !value.is_empty())
}

impl From<Image> for MessageSegment {
    fn from(image: Image) -> MessageSegment {
        image.segment()
    }
}

impl From<Image> for OutgoingMessage {
    fn from(image: Image) -> OutgoingMessage {
        OutgoingMessage::new(vec![image.segment()])
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
    use crate::config::onebot::OneBotConfig;
    use crate::onebot::model::{OneBotAction, OneBotActionResponse};
    use crate::onebot::{BusinessHandler, ConnectionRegistry, OneBotApplication, OneBotConnection};
    use crate::runtime::dispatcher::CommandDispatcher;
    use crate::runtime::message::{BoxFuture, MessageSender};
    use serde_json::json;
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
            framework: Framework::new(),
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

    /// 资料缓存跟着实例走：搬走 `role_cache` 之前它是进程级 `static`，
    /// 隔离实例跑测试会看到别家读回来的身份。这条钉死"两套实例互不串台"。
    #[test]
    fn the_peer_cache_belongs_to_the_instance_not_the_process() {
        use crate::onebot::api::GroupMemberInfo;
        let one = Framework::new();
        let other = Framework::new();
        one.peers().members().put(
            (20001, 100),
            GroupMemberInfo {
                group_id: 20001,
                user_id: 100,
                ..Default::default()
            },
        );
        assert!(one.peers().members().get(&(20001, 100)).is_some());
        assert!(
            other.peers().members().get(&(20001, 100)).is_none(),
            "另一套实例不该读到这一套缓存回来的资料"
        );
        assert!(
            !std::ptr::eq(
                &**one.peers() as *const crate::runtime::peers::PeerCache,
                &**other.peers()
            ),
            "两套实例各挂各的缓存"
        );
    }

    /// 图片句柄的意义在于"收到的那张图，回头还发得出去"：
    /// 从入站段取出来时必须几种线索一个都不丢，落回消息段时也要原样带回去。
    #[test]
    fn inbound_image_round_trips_without_losing_a_lead() {
        let received = MessageSegment::Image {
            url: Some("https://x.example/a.png".to_string()),
            file: Some("".to_string()),
            data: None,
            file_id: Some("E2A1B3C4".to_string()),
        };
        let image = Image::from_segment(&received).expect("有 url 就不算空");
        assert_eq!(image.url(), Some("https://x.example/a.png"));
        assert_eq!(image.file_id(), Some("E2A1B3C4"));
        assert!(matches!(
            image.segment(),
            MessageSegment::Image { url, file, data, file_id }
                if url.as_deref() == Some("https://x.example/a.png")
                    && file.is_none()
                    && data.is_none()
                    && file_id.as_deref() == Some("E2A1B3C4")
        ));

        // 实现端回空串等同于没回，全空的那一段不该变成一张"能发出去"的图
        assert!(Image::from_segment(&MessageSegment::Text("不是图".to_string())).is_none());
        assert!(
            Image::from_segment(&MessageSegment::Image {
                url: Some(String::new()),
                file: Some(String::new()),
                data: None,
                file_id: Some(String::new()),
            })
            .is_none()
        );
    }

    /// 库里那一列的三种写法都得还原成对应那一种来源，不能把 `fileid://E2` 当成路径发出去
    #[test]
    fn stored_image_lead_decodes_by_scheme() {
        let by_id = Image::from_stored("fileid://E2A1B3C4").expect("fileid 前应还原成标识");
        assert_eq!(by_id.file_id(), Some("E2A1B3C4"));
        assert_eq!(by_id.url(), None);

        let by_url = Image::from_stored("https://x/a.png").expect("http 前缀是直链");
        assert_eq!(by_url.url(), Some("https://x/a.png"));
        assert!(Image::from_stored("http://x/a.png").is_some());

        let by_path = Image::from_stored("D:/arona/a.png").expect("剩下的是本机路径");
        assert_eq!(by_path.path(), Some("D:/arona/a.png"));

        assert!(Image::from_stored("").is_none(), "空串是无图，不是空图");
    }

    /// 字节那条路要一路带到消息段里：框架转 base64 发出，全程不落机器人硬盘
    #[test]
    fn generated_image_bytes_reach_the_segment() {
        let image = Image::from_bytes([0xFF, 0xD8, 0x01]);
        assert!(image.file_id().is_none() && image.url().is_none());
        let MessageSegment::Image { data, .. } = image.segment() else {
            panic!("from_bytes 出来的该是字节段");
        };
        assert_eq!(data.as_deref(), Some(&[0xFF, 0xD8, 0x01][..]));
        // 一句 `OutgoingMessage::from(image)` 就能发
        let message: OutgoingMessage = image.clone().into();
        assert_eq!(message.segments.len(), 1);
    }

    /// 回一包固定资料的假连接，顺手记下每笔动作名——用来数"这一套到底打了几次网络"
    struct Canned {
        nickname: &'static str,
        self_id: i64,
        seen: Mutex<Vec<String>>,
    }

    impl OneBotConnection for Canned {
        fn id(&self) -> u64 {
            7
        }

        fn send<'a>(&'a self, action: OneBotAction) -> BoxFuture<'a, Option<OneBotActionResponse>> {
            let name = action.action.clone();
            let data = match name.as_str() {
                "get_login_info" => json!({ "user_id": self.self_id, "nickname": self.nickname }),
                "get_group_member_info" => json!({
                    "group_id": action.params["group_id"],
                    "user_id": action.params["user_id"],
                    "nickname": "成员甲",
                }),
                _ => json!(null),
            };
            self.seen.lock().unwrap().push(name.clone());
            Box::pin(async move {
                Some(OneBotActionResponse {
                    status: "ok".to_string(),
                    retcode: 0,
                    data: Some(data),
                    message: None,
                    wording: None,
                    echo: Some(name),
                    raw: json!({}),
                })
            })
        }

        fn send_raw(&self, _payload: &str) {}
        fn start(&self) {}
        fn stop(&self) {}
    }

    /// 造一套"真连着实现端"的隔离实例，返回实例和它那条连接的记账本
    fn canned_instance(nickname: &'static str, self_id: i64) -> (Arc<Framework>, Arc<Canned>) {
        let framework = Framework::new();
        let registry = Arc::new(ConnectionRegistry::new());
        let canned = Arc::new(Canned {
            nickname,
            self_id,
            seen: Mutex::new(Vec::new()),
        });
        registry.register(canned.clone());
        let config = OneBotConfig {
            self_id,
            ..Default::default()
        };
        let business = Arc::new(BusinessHandler::new(
            config.clone(),
            framework.clone(),
            Arc::new(CommandDispatcher::new()),
            registry.clone(),
        ));
        framework
            .onebot()
            .attach(Arc::new(OneBotApplication::new(config, business, registry)));
        (framework, canned)
    }

    /// `api()` 曾每次现取进程默认实例，于是 `Bot::of(&隔离实例)` 的动作会打到别套的连接上，
    /// self_id 也跟着串台。句柄既然认了实例，出口就必须只从**这一套**取连接。
    /// 只断言两套各自的结果，刻意不断言全局实例——并行用例可能正往它上面挂连接。
    #[tokio::test]
    async fn the_api_handle_follows_the_instance_it_came_from() {
        let (one, one_conn) = canned_instance("这套的机器人", 10001);
        let (other, other_conn) = canned_instance("那套的机器人", 20002);

        assert_eq!(Bot::of(&one).self_id(), 10001);
        assert_eq!(Bot::of(&other).self_id(), 20002);
        assert_eq!(
            Bot::of(&one).api().get_login_info().await.unwrap().nickname,
            "这套的机器人"
        );
        assert_eq!(
            Bot::of(&other)
                .api()
                .get_login_info()
                .await
                .unwrap()
                .nickname,
            "那套的机器人"
        );

        assert!(
            Arc::ptr_eq(Bot::of(&one).api().framework(), &one),
            "api() 该握着自己这一套实例"
        );
        assert_eq!(
            one_conn.seen.lock().unwrap().as_slice(),
            ["get_login_info".to_string()],
            "动作只该落在自己的那条连接上"
        );
        assert_eq!(
            other_conn.seen.lock().unwrap().as_slice(),
            ["get_login_info".to_string()]
        );
    }

    /// 成员资料按 [`crate::runtime::peers::PeerCache`] 的读法：同一个人只在首次打网络，
    /// `forget_member` 作废之后才重新查——这两条都得实测钉住，否则"缓存"只是注释里的承诺。
    #[tokio::test]
    async fn member_profile_reaches_the_implementation_only_when_the_cache_is_cold() {
        let (one, conn) = canned_instance("这套的机器人", 10001);
        let group = Bot::of(&one).group(20001);

        let hits = || {
            conn.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|action| action.as_str() == "get_group_member_info")
                .count()
        };

        let first = group.member_info(100).await.unwrap();
        let second = group.member_info(100).await.unwrap();
        assert_eq!(first.nickname, "成员甲");
        assert_eq!(second.nickname, first.nickname);
        assert_eq!(hits(), 1, "第二次读该命中实例上的缓存，不再打网络");

        group.forget_member(100);
        group.member_info(100).await.unwrap();
        assert_eq!(hits(), 2, "作废之后该真的重查一次");

        // 缓存挂在实例上：另一套实例冷启动，同样要自己打一次
        let (other, other_conn) = canned_instance("那套的机器人", 10001);
        Bot::of(&other).group(20001).member_info(100).await.unwrap();
        assert_eq!(
            other_conn
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|action| action.as_str() == "get_group_member_info")
                .count(),
            1,
            "别家的缓存不该让这一套少跑一次"
        );
    }
}
