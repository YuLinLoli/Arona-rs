//! OneBot v11 动作的强类型出口（框架与管理面板共用，插件直接拿同一份能力）。
//!
//! 为什么不只给一个 `call(action, params)`：插件绝大多数场景要的是「拿到结构化的返回值」
//! （群名片、成员列表、文件列表…），裸 JSON 会把字段拼写错误推到运行期。这里每个动作
//! 都是一个 async 方法：入参用类型约束（`Option` 参数传 `None` 时**不会**塞进 JSON，
//! 免得实现端把 null 当成有效值），返回值用 `serde(default)` 反序列化 —— 实现端少回
//! 某个字段不会失败，多回的字段收进 `rest` 原样保留。
//!
//! 冷门/新增动作仍有兜底通道：[`OneBotApi::call`] 拿原始 `data`，[`OneBotApi::call_typed`]
//! 直接把 `data` 反序列化成自己的类型。
//!
//! 拿连接：一律走 [`OneBotApi::global`]，每次调用现取首个可用连接 —— 连接热重载
//! （改 onebot.yml）之后不会拿着已销毁的连接不放。没有可用连接时返回
//! [`OneBotError::NoConnection`]，超时返回 [`OneBotError::Timeout`]。
use crate::onebot::application;
use crate::onebot::connection::OneBotConnection;
use crate::onebot::model::OneBotActionResponse;
use crate::onebot::protocol;
use crate::runtime::message::{MessageSegment, MessageTarget, OutgoingMessage};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// 动作调用的失败原因。
#[derive(Clone, Debug)]
pub enum OneBotError {
    /// 当前没有可用连接（未启动、全部断开或热重载中）
    NoConnection,
    /// 实现端在超时内没有响应，或连接在投递前失效
    Timeout,
    /// 实现端明确返回失败
    Failed {
        status: String,
        retcode: i64,
        message: String,
    },
    /// 返回的 data 结构与预期不符（或实现端压根没回 data）
    BadResponse(String),
}

impl std::fmt::Display for OneBotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OneBotError::NoConnection => write!(f, "没有可用的 OneBot 连接"),
            OneBotError::Timeout => write!(f, "OneBot 动作超时或连接已失效"),
            OneBotError::Failed {
                status,
                retcode,
                message,
            } => write!(
                f,
                "OneBot 返回失败(status={status}, retcode={retcode}): {message}"
            ),
            OneBotError::BadResponse(detail) => write!(f, "OneBot 响应格式异常: {detail}"),
        }
    }
}

impl std::error::Error for OneBotError {}

/// 消息载荷：允许直接写 `"文本"`、`OutgoingMessage`、消息段数组或已拼好的 JSON 数组。
#[derive(Clone, Debug)]
pub struct MessagePayload(pub Value);

impl From<&str> for MessagePayload {
    fn from(text: &str) -> MessagePayload {
        MessagePayload(json!([{"type": "text", "data": {"text": text}}]))
    }
}

impl From<String> for MessagePayload {
    fn from(text: String) -> MessagePayload {
        MessagePayload::from(text.as_str())
    }
}

impl From<&OutgoingMessage> for MessagePayload {
    fn from(message: &OutgoingMessage) -> MessagePayload {
        MessagePayload(protocol::message_to_json(message))
    }
}

impl From<OutgoingMessage> for MessagePayload {
    fn from(message: OutgoingMessage) -> MessagePayload {
        MessagePayload::from(&message)
    }
}

impl From<Vec<MessageSegment>> for MessagePayload {
    fn from(segments: Vec<MessageSegment>) -> MessagePayload {
        MessagePayload::from(OutgoingMessage {
            segments,
            revoke_after_millis: None,
        })
    }
}

impl From<Value> for MessagePayload {
    fn from(value: Value) -> MessagePayload {
        MessagePayload(value)
    }
}

/// `Option` 参数：`None` 序列化成 null，由 [`params`] 宏剔除（OneBot 的可选字段不该传 null）
fn opt<T: serde::Serialize>(value: &Option<T>) -> Value {
    match value {
        Some(value) => json!(value),
        None => Value::Null,
    }
}

/// 拼动作参数：值为 null 的键直接丢掉（可选字段不该以 null 发给实现端）
macro_rules! params {
    // 无参数动作写 params!()。这一条必须排在前面：下面那条带重复的臂匹配零个元素也成立，
    // 排在后面永远轮不到，展开出来的 `let mut map` 就是一堆 unused_mut 警告。
    () => {{ Value::Object(Map::new()) }};
    ($($key:literal => $value:expr),* $(,)?) => {{
        let mut map = Map::new();
        $(
            let value: Value = $value;
            if !value.is_null() {
                map.insert($key.to_string(), value);
            }
        )*
        Value::Object(map)
    }};
}

/// 实现端可能在 retcode=1/async 时只回受理不回 data，这类结构把所有字段都当可选
fn deserialize<T: for<'de> Deserialize<'de>>(
    response: OneBotActionResponse,
    action: &str,
) -> Result<T, OneBotError> {
    let data = response.data.unwrap_or(Value::Null);
    serde_json::from_value(data)
        .map_err(|err| OneBotError::BadResponse(format!("{action} 的 data 无法解析: {err}")))
}

/// 群身份字段各家写法不一：OneBot v11 回字符串（owner/admin/member），NTQQ 系回数字
/// （1 群主 / 2 管理员 / 3 成员）。统一折成数字，认不出的按成员。
fn role_as_number<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(match &value {
        Value::Number(number) => number.as_i64().unwrap_or(3),
        Value::String(text) => match text.to_ascii_lowercase().as_str() {
            "owner" | "creator" | "群主" => 1,
            "admin" | "administrator" | "管理员" => 2,
            _ => 3,
        },
        _ => 3,
    })
}

/// OneBot 动作出口（无状态：每次调用现取可用连接）
#[derive(Clone, Copy, Debug, Default)]
pub struct OneBotApi;

impl OneBotApi {
    /// 全局出口（框架与插件都用这个）
    pub fn global() -> OneBotApi {
        OneBotApi
    }

    fn connection(&self) -> Option<Arc<dyn OneBotConnection>> {
        application::global()
            .and_then(|app| app.first_connection())
            .or_else(|| crate::onebot::connection::global_registry().and_then(|r| r.first()))
    }

    /// 万能通道：发出动作并取回 `data`（实现端没回 data 时是 `Value::Null`）
    pub async fn call(&self, action: &str, params: Value) -> Result<Value, OneBotError> {
        Ok(self.raw(action, params).await?.data.unwrap_or(Value::Null))
    }

    /// 万能通道 + 自定义返回类型
    pub async fn call_typed<T>(&self, action: &str, params: Value) -> Result<T, OneBotError>
    where
        T: for<'de> Deserialize<'de>,
    {
        let response = self.raw(action, params).await?;
        deserialize(response, action)
    }

    /// 只要成功、不关心 data
    pub async fn call_ok(&self, action: &str, params: Value) -> Result<(), OneBotError> {
        self.raw(action, params).await?;
        Ok(())
    }

    /// 内部：拿到完整响应（含 async 受理），交给具体方法解析
    async fn raw(&self, action: &str, params: Value) -> Result<OneBotActionResponse, OneBotError> {
        let Some(connection) = self.connection() else {
            return Err(OneBotError::NoConnection);
        };
        let response = connection
            .send(protocol::action(action, params))
            .await
            .ok_or(OneBotError::Timeout)?;
        if !response.success() && !response.async_accepted() {
            return Err(OneBotError::Failed {
                status: response.status.clone(),
                retcode: response.retcode,
                message: response
                    .message
                    .clone()
                    .or_else(|| response.wording.clone())
                    .unwrap_or_default(),
            });
        }
        Ok(response)
    }

    /// 发一条消息到指定目标（群/私聊），返回 message_id
    pub async fn send(
        &self,
        target: MessageTarget,
        message: impl Into<MessagePayload>,
    ) -> Result<i64, OneBotError> {
        let payload = message.into();
        match target {
            MessageTarget::Group(group_id) => self.send_group_msg(group_id, payload).await,
            MessageTarget::Private(user_id) => self.send_private_msg(user_id, payload).await,
        }
    }

    // ==================== 消息 ====================

    /// `send_private_msg` 发送私聊消息
    pub async fn send_private_msg(
        &self,
        user_id: i64,
        message: impl Into<MessagePayload>,
    ) -> Result<i64, OneBotError> {
        let data = self
            .call(
                "send_private_msg",
                params! { "user_id" => json!(user_id), "message" => message.into().0 },
            )
            .await?;
        Ok(message_id(&data))
    }

    /// `send_group_msg` 发送群消息
    pub async fn send_group_msg(
        &self,
        group_id: i64,
        message: impl Into<MessagePayload>,
    ) -> Result<i64, OneBotError> {
        let data = self
            .call(
                "send_group_msg",
                params! { "group_id" => json!(group_id), "message" => message.into().0 },
            )
            .await?;
        Ok(message_id(&data))
    }

    /// `send_msg` 发送消息（群/私聊二选一，都不填时由实现端决定）
    pub async fn send_msg(
        &self,
        group_id: Option<i64>,
        user_id: Option<i64>,
        message: impl Into<MessagePayload>,
    ) -> Result<i64, OneBotError> {
        let data = self
            .call(
                "send_msg",
                params! {
                    "group_id" => opt(&group_id),
                    "user_id" => opt(&user_id),
                    "message" => message.into().0,
                },
            )
            .await?;
        Ok(message_id(&data))
    }

    /// `get_msg` 取消息原文（含发送者资料；实现端普遍不支持跨 bot 查询）
    pub async fn get_msg(&self, message_id: i64) -> Result<MessageInfo, OneBotError> {
        self.call_typed("get_msg", params! { "message_id" => json!(message_id) })
            .await
    }

    /// `get_forward_msg` 取合并转发的消息节点列表
    pub async fn get_forward_msg(&self, message_id: i64) -> Result<Vec<MessageNode>, OneBotError> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            messages: Vec<MessageNode>,
        }
        let data: Wrapper = self
            .call_typed(
                "get_forward_msg",
                params! { "message_id" => json!(message_id) },
            )
            .await?;
        Ok(data.messages)
    }

    /// `delete_msg` 撤回消息（自己发的，或有权限撤回的）
    pub async fn delete_msg(&self, message_id: i64) -> Result<(), OneBotError> {
        self.call_ok("delete_msg", params! { "message_id" => json!(message_id) })
            .await
    }

    /// `set_msg_emoji_like` 给消息贴表情；`present=false` 表示取消
    pub async fn set_msg_emoji_like(
        &self,
        message_id: i64,
        emoji_id: &str,
        present: bool,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_msg_emoji_like",
            params! {
                "message_id" => json!(message_id),
                "emoji_id" => json!(emoji_id),
                "emoji_present" => json!(present),
            },
        )
        .await
    }

    /// `can_send_private_msg` 检查是否可以发私聊
    pub async fn can_send_private_msg(&self, user_id: i64) -> Result<CanSend, OneBotError> {
        self.call_typed(
            "can_send_private_msg",
            params! { "user_id" => json!(user_id) },
        )
        .await
    }

    /// `can_send_group_msg` 检查某人在该群能否发言
    pub async fn can_send_group_msg(
        &self,
        group_id: i64,
        sender_user_id: i64,
    ) -> Result<CanSend, OneBotError> {
        self.call_typed(
            "can_send_group_msg",
            params! { "group_id" => json!(group_id), "user_id" => json!(sender_user_id) },
        )
        .await
    }

    // ==================== 账号 / 好友 / 群资料 ====================

    /// `get_login_info` 当前登录账号
    pub async fn get_login_info(&self) -> Result<LoginInfo, OneBotError> {
        self.call_typed("get_login_info", params!()).await
    }

    /// `get_stranger_info` 陌生人资料
    pub async fn get_stranger_info(
        &self,
        user_id: i64,
        refresh: bool,
    ) -> Result<StrangerInfo, OneBotError> {
        self.call_typed(
            "get_stranger_info",
            params! { "user_id" => json!(user_id), "no_cache" => json!(refresh) },
        )
        .await
    }

    /// `get_friend_list` 好友列表
    pub async fn get_friend_list(&self) -> Result<Vec<FriendInfo>, OneBotError> {
        self.call_typed("get_friend_list", params!()).await
    }

    /// `get_group_info` 群资料（`refresh=true` 强制走服务端）
    pub async fn get_group_info(
        &self,
        group_id: i64,
        refresh: bool,
    ) -> Result<GroupInfo, OneBotError> {
        self.call_typed(
            "get_group_info",
            params! { "group_id" => json!(group_id), "no_cache" => json!(refresh) },
        )
        .await
    }

    /// `get_group_list` 已加入的群列表
    pub async fn get_group_list(&self) -> Result<Vec<GroupInfo>, OneBotError> {
        self.call_typed("get_group_list", params!()).await
    }

    /// `get_group_member_info` 群成员资料
    pub async fn get_group_member_info(
        &self,
        group_id: i64,
        user_id: i64,
        refresh: bool,
    ) -> Result<GroupMemberInfo, OneBotError> {
        self.call_typed(
            "get_group_member_info",
            params! {
                "group_id" => json!(group_id),
                "user_id" => json!(user_id),
                "no_cache" => json!(refresh),
            },
        )
        .await
    }

    /// `get_group_member_list` 群成员列表
    pub async fn get_group_member_list(
        &self,
        group_id: i64,
    ) -> Result<Vec<GroupMemberInfo>, OneBotError> {
        self.call_typed(
            "get_group_member_list",
            params! { "group_id" => json!(group_id) },
        )
        .await
    }

    /// `get_group_honor_info` 群荣耀信息（聊王/群主/管理员）
    pub async fn get_group_honor_info(&self, group_id: i64) -> Result<GroupHonorInfo, OneBotError> {
        self.call_typed(
            "get_group_honor_info",
            params! { "group_id" => json!(group_id) },
        )
        .await
    }

    /// `get_group_system_msg` 群系统通知（加群申请等）；多数实现端未支持
    pub async fn get_group_system_msg(&self) -> Result<Vec<SystemMsg>, OneBotError> {
        self.call_typed("get_group_system_msg", params!()).await
    }

    /// `get_group_at_all_remain` 该群还能 @全体 几次
    pub async fn get_group_at_all_remain(&self, group_id: i64) -> Result<AtAllRemain, OneBotError> {
        self.call_typed(
            "get_group_at_all_remain",
            params! { "group_id" => json!(group_id) },
        )
        .await
    }

    // ==================== 群管理 ====================

    /// `set_group_kick` 踢出成员；`reject_add_request` 表示同时拒绝其后续加群申请
    pub async fn set_group_kick(
        &self,
        group_id: i64,
        user_id: i64,
        reject_add_request: bool,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_kick",
            params! {
                "group_id" => json!(group_id),
                "user_id" => json!(user_id),
                "reject_add_request" => json!(reject_add_request),
            },
        )
        .await
    }

    /// `set_group_ban` 禁言（`duration=0` 解禁；负数在部分实现端表示永久）
    pub async fn set_group_ban(
        &self,
        group_id: i64,
        user_id: i64,
        duration: u64,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_ban",
            params! {
                "group_id" => json!(group_id),
                "user_id" => json!(user_id),
                "duration" => json!(duration),
            },
        )
        .await
    }

    /// `set_group_whole_ban` 全体禁言
    pub async fn set_group_whole_ban(
        &self,
        group_id: i64,
        enable: bool,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_whole_ban",
            params! { "group_id" => json!(group_id), "enable" => json!(enable) },
        )
        .await
    }

    /// `set_group_admin` 设置/取消管理员
    pub async fn set_group_admin(
        &self,
        group_id: i64,
        user_id: i64,
        enable: bool,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_admin",
            params! {
                "group_id" => json!(group_id),
                "user_id" => json!(user_id),
                "enable" => json!(enable),
            },
        )
        .await
    }

    /// `set_group_anonymous_ban` 禁言群内匿名发言者（按 anonymous_flag 或 user_id）
    pub async fn set_group_anonymous_ban(
        &self,
        group_id: i64,
        anonymous_flag: Option<&str>,
        user_id: Option<i64>,
        duration: u64,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_anonymous_ban",
            params! {
                "group_id" => json!(group_id),
                "anonymous_flag" => opt(&anonymous_flag.map(str::to_string)),
                "user_id" => opt(&user_id),
                "duration" => json!(duration),
            },
        )
        .await
    }

    /// `set_group_name` 改群名
    pub async fn set_group_name(&self, group_id: i64, group_name: &str) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_name",
            params! { "group_id" => json!(group_id), "group_name" => json!(group_name) },
        )
        .await
    }

    /// `set_group_level` 设置群内等级（多数实现端已不支持）
    pub async fn set_group_level(
        &self,
        group_id: i64,
        user_id: i64,
        new_level: i64,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_level",
            params! {
                "group_id" => json!(group_id),
                "user_id" => json!(user_id),
                "new_level" => json!(new_level),
            },
        )
        .await
    }

    /// `set_group_card` 改群名片（空串表示清除）
    pub async fn set_group_card(
        &self,
        group_id: i64,
        user_id: i64,
        card: &str,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_card",
            params! {
                "group_id" => json!(group_id),
                "user_id" => json!(user_id),
                "card" => json!(card),
            },
        )
        .await
    }

    /// `set_group_special_title` 设置群头衔；`duration=-1` 为永久
    pub async fn set_group_special_title(
        &self,
        group_id: i64,
        user_id: i64,
        special_title: &str,
        duration: i64,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_special_title",
            params! {
                "group_id" => json!(group_id),
                "user_id" => json!(user_id),
                "special_title" => json!(special_title),
                "duration" => json!(duration),
            },
        )
        .await
    }

    /// `set_group_add_request` 处理加群/邀请申请（`approve=false` 表示拒绝）
    pub async fn set_group_add_request(
        &self,
        flag: &str,
        sub_type: &str,
        approve: bool,
        reason: Option<&str>,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_add_request",
            params! {
                "flag" => json!(flag),
                "type" => json!(sub_type),
                "approve" => json!(approve),
                "reason" => opt(&reason.map(str::to_string)),
            },
        )
        .await
    }

    /// `set_friend_add_request` 处理好友申请
    pub async fn set_friend_add_request(
        &self,
        flag: &str,
        approve: bool,
        remark: Option<&str>,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_friend_add_request",
            params! {
                "flag" => json!(flag),
                "approve" => json!(approve),
                "remark" => opt(&remark.map(str::to_string)),
            },
        )
        .await
    }

    /// `send_like` 给好友点赞
    pub async fn send_like(&self, user_id: i64, times: u64) -> Result<(), OneBotError> {
        self.call_ok(
            "send_like",
            params! { "user_id" => json!(user_id), "times" => json!(times) },
        )
        .await
    }

    // ==================== 群公告与精华 ====================

    /// `set_group_notice` 发群公告（非 v11 正式列表，但主流实现端都支持）
    pub async fn set_group_notice(
        &self,
        group_id: i64,
        notice: &str,
        image: Option<&str>,
        pinned: bool,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "set_group_notice",
            params! {
                "group_id" => json!(group_id),
                "content" => json!(notice),
                "image" => opt(&image.map(str::to_string)),
                "is_pinned" => json!(pinned),
                "show_gag" => json!(false),
            },
        )
        .await
    }

    /// `get_group_notice` 拉取群公告列表（原样返回，字段随实现端差异较大）
    pub async fn get_group_notice(&self, group_id: i64) -> Result<Vec<Value>, OneBotError> {
        self.call_typed(
            "get_group_notice",
            params! { "group_id" => json!(group_id) },
        )
        .await
    }

    /// `get_essence_msg_list` 精华消息列表
    pub async fn get_essence_msg_list(
        &self,
        group_id: i64,
    ) -> Result<Vec<EssenceMsg>, OneBotError> {
        self.call_typed(
            "get_essence_msg_list",
            params! { "group_id" => json!(group_id) },
        )
        .await
    }

    /// `add_essence_msg` 设为精华
    pub async fn add_essence_msg(&self, message_id: i64) -> Result<(), OneBotError> {
        self.call_ok(
            "add_essence_msg",
            params! { "message_id" => json!(message_id) },
        )
        .await
    }

    /// `delete_essence_msg` 取消精华
    pub async fn delete_essence_msg(&self, message_id: i64) -> Result<(), OneBotError> {
        self.call_ok(
            "delete_essence_msg",
            params! { "message_id" => json!(message_id) },
        )
        .await
    }

    // ==================== 群文件 ====================

    /// `upload_group_file` 上传群文件：`file` 可以是本地路径、URL 或 base64
    pub async fn upload_group_file(
        &self,
        group_id: i64,
        file: &str,
        name: &str,
        folder_id: Option<&str>,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "upload_group_file",
            params! {
                "group_id" => json!(group_id),
                "file" => json!(file),
                "name" => json!(name),
                "folder_id" => opt(&folder_id.map(str::to_string)),
            },
        )
        .await
    }

    /// `get_group_root_files` 群根目录
    pub async fn get_group_root_files(&self, group_id: i64) -> Result<GroupFiles, OneBotError> {
        self.call_typed(
            "get_group_root_files",
            params! { "group_id" => json!(group_id) },
        )
        .await
    }

    /// `get_group_files_by_folder` 群文件子目录
    pub async fn get_group_files_by_folder(
        &self,
        group_id: i64,
        folder_id: &str,
    ) -> Result<GroupFiles, OneBotError> {
        self.call_typed(
            "get_group_files_by_folder",
            params! { "group_id" => json!(group_id), "folder_id" => json!(folder_id) },
        )
        .await
    }

    /// `get_group_file_url` 取群文件的下载直链
    pub async fn get_group_file_url(
        &self,
        group_id: i64,
        file_id: &str,
        busid: &str,
    ) -> Result<String, OneBotError> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            url: String,
        }
        let data: Wrapper = self
            .call_typed(
                "get_group_file_url",
                params! {
                    "group_id" => json!(group_id),
                    "file_id" => json!(file_id),
                    "busid" => json!(busid),
                },
            )
            .await?;
        Ok(data.url)
    }

    /// `delete_group_file` 删除群文件
    pub async fn delete_group_file(
        &self,
        group_id: i64,
        file_id: &str,
        busid: &str,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "delete_group_file",
            params! {
                "group_id" => json!(group_id),
                "file_id" => json!(file_id),
                "busid" => json!(busid),
            },
        )
        .await
    }

    /// `create_group_folder` 新建群文件夹，返回 folder_id（实现端不回时为空串）
    pub async fn create_group_folder(
        &self,
        group_id: i64,
        name: &str,
        parent_folder_id: Option<&str>,
    ) -> Result<String, OneBotError> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            folder_id: String,
        }
        let data: Wrapper = self
            .call_typed(
                "create_group_folder",
                params! {
                    "group_id" => json!(group_id),
                    "folder_name" => json!(name),
                    "parent_folder_id" => opt(&parent_folder_id.map(str::to_string)),
                },
            )
            .await?;
        Ok(data.folder_id)
    }

    /// `delete_group_folder` 删除群文件夹
    pub async fn delete_group_folder(
        &self,
        group_id: i64,
        folder_id: &str,
    ) -> Result<(), OneBotError> {
        self.call_ok(
            "delete_group_folder",
            params! { "group_id" => json!(group_id), "folder_id" => json!(folder_id) },
        )
        .await
    }

    // ==================== 系统 ====================

    /// `get_status` 运行状态（`online`/`good` 由实现端决定给哪个）
    pub async fn get_status(&self) -> Result<StatusInfo, OneBotError> {
        self.call_typed("get_status", params!()).await
    }

    /// `get_version_info` 实现端版本信息
    pub async fn get_version_info(&self) -> Result<VersionInfo, OneBotError> {
        self.call_typed("get_version_info", params!()).await
    }

    /// `get_cookie` 客户端 cookie（部分实现端字段名为 cookies / set_cookie）
    pub async fn get_cookie(&self, domain: Option<&str>) -> Result<Value, OneBotError> {
        self.call(
            "get_cookie",
            params! { "domain" => opt(&domain.map(str::to_string)) },
        )
        .await
    }

    /// `get_csrf` CSRF token
    pub async fn get_csrf(&self, domain: Option<&str>) -> Result<i64, OneBotError> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, alias = "csrf", alias = "token")]
            csrf_token: i64,
        }
        let data: Wrapper = self
            .call_typed(
                "get_csrf",
                params! { "domain" => opt(&domain.map(str::to_string)) },
            )
            .await?;
        Ok(data.csrf_token)
    }
}

fn message_id(data: &Value) -> i64 {
    data.get("message_id")
        .and_then(|v| v.as_i64())
        .unwrap_or_default()
}

// ==================== 返回值类型 ====================

/// `get_status`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct StatusInfo {
    pub online: Option<bool>,
    pub good: Option<bool>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `get_version_info`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct VersionInfo {
    pub app_name: String,
    pub app_version: String,
    pub protocol_version: String,
}

/// `get_login_info`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct LoginInfo {
    pub user_id: i64,
    pub nickname: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `get_stranger_info`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct StrangerInfo {
    pub user_id: i64,
    pub nickname: String,
    pub sex: String,
    pub age: i64,
    pub card: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `get_friend_list` 的单项
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct FriendInfo {
    pub user_id: i64,
    pub nickname: String,
    pub remark: String,
    pub sex: String,
    pub age: i64,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// 显示名：备注优先，其次昵称
impl FriendInfo {
    pub fn display(&self) -> &str {
        if !self.remark.trim().is_empty() {
            self.remark.as_str()
        } else {
            self.nickname.as_str()
        }
    }
}

/// `get_group_list` / `get_group_info`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct GroupInfo {
    pub group_id: i64,
    pub group_name: String,
    pub group_memo: String,
    pub group_level: i64,
    pub member_count: i64,
    pub max_member_count: i64,
    pub owner_id: i64,
    pub shuffle: bool,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `get_group_member_list` / `get_group_member_info`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct GroupMemberInfo {
    pub group_id: i64,
    pub user_id: i64,
    pub nickname: String,
    pub card: String,
    pub sex: String,
    pub age: i64,
    pub level: String,
    pub title: String,
    pub join_time: i64,
    pub last_sent_time: i64,
    /// 实现端的身份字段名不统一（group_rank / role / permission），都收下
    pub group_rank: i64,
    #[serde(
        default,
        alias = "role",
        alias = "permission",
        deserialize_with = "role_as_number"
    )]
    pub role: i64,
    pub unimportant_flag: bool,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// 群名片优先，其次昵称
impl GroupMemberInfo {
    pub fn display(&self) -> &str {
        if !self.card.trim().is_empty() {
            self.card.as_str()
        } else {
            self.nickname.as_str()
        }
    }

    /// QQ 群身份：1=群主 2=管理员 3=成员（部分实现端用 0/1 表示普通/管理员，这里保守判断）
    pub fn is_owner(&self) -> bool {
        self.role == 1 || self.group_rank >= 5
    }

    pub fn is_admin(&self) -> bool {
        self.role == 2 || self.group_rank == 4
    }
}

/// `can_send_*_msg`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CanSend {
    pub can_send: bool,
    pub gap: i64,
}

/// `get_msg`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MessageInfo {
    pub time: i64,
    pub message_id: i64,
    pub real_params: Map<String, Value>,
    pub message_type: String,
    pub sub_type: String,
    pub user_id: i64,
    pub group_id: i64,
    pub message: Value,
    pub original_message: Value,
    pub sender: Value,
}

/// `get_forward_msg` 的节点
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MessageNode {
    pub user_id: i64,
    pub nickname: String,
    pub content: Value,
    pub time: i64,
    pub message_id: i64,
}

/// `get_group_honor_info` 里的单项
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct HonorMember {
    pub user_id: i64,
    pub category_id: i64,
    pub nickname: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `get_group_honor_info`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct GroupHonorInfo {
    pub group_id: i64,
    #[serde(default, alias = "current_owner")]
    pub owner: HonorMember,
    #[serde(default, alias = "chat_speaker")]
    pub speaker: HonorMember,
    #[serde(default)]
    pub admins: Vec<HonorMember>,
    #[serde(default)]
    pub special_admins: Vec<HonorMember>,
    #[serde(default)]
    pub love: Vec<HonorMember>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `get_group_at_all_remain`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct AtAllRemain {
    pub can_at_all: bool,
    pub remain_at_all_count_for_group: i64,
    pub remain_at_all_count_for_uin: i64,
}

/// `get_group_system_msg` 的单项（字段随实现端差异大，原样保留）
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SystemMsg {
    pub group_id: i64,
    pub message_id: i64,
    pub detail_id: i64,
    pub user_id: i64,
    pub time: i64,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `get_essence_msg_list` 的单项
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct EssenceMsg {
    pub sender_id: i64,
    pub sender_nick: String,
    pub sender_time: i64,
    pub consumer_id: i64,
    pub consumer_nick: String,
    pub message_id: i64,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// 群文件条目
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct GroupFile {
    pub file_id: String,
    pub file_name: String,
    pub file_size: i64,
    pub busid: String,
    pub file_node: String,
    pub folder_id: String,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// 群文件夹条目
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct GroupFolder {
    pub folder_id: String,
    pub folder_name: String,
    pub folder_create_time: i64,
    pub creator_uin: i64,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

/// `get_group_root_files` / `get_group_files_by_folder`
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct GroupFiles {
    pub files: Vec<GroupFile>,
    pub folders: Vec<GroupFolder>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 可选参数为 None 时不能出现在 JSON 里（实现端会把 null 当有效值）
    #[test]
    fn params_macro_drops_nulls() {
        let built = params! {
            "group_id" => json!(1_i64),
            "folder_id" => opt(&Option::<String>::None),
            "name" => opt(&Some("x".to_string())),
        };
        assert_eq!(built, json!({ "group_id": 1, "name": "x" }));
    }

    /// 实现端字段少给/多给都不能让插件崩：缺字段走 default，未知字段进 rest
    #[test]
    fn tolerant_deserialization() {
        let member: GroupMemberInfo =
            serde_json::from_value(json!({ "user_id": 1, "unknown_field": true })).unwrap();
        assert_eq!(member.user_id, 1);
        assert!(member.display().is_empty());
        let status: StatusInfo = serde_json::from_value(json!({ "online": true, "x": 1 })).unwrap();
        assert_eq!(status.online, Some(true));
        assert!(status.rest.contains_key("x"));
    }

    /// 名片优先于昵称（GUI 与命令回复都按这个显示）
    #[test]
    fn member_display_prefers_card() {
        let member = GroupMemberInfo {
            card: "  ".into(),
            nickname: "小明".into(),
            ..Default::default()
        };
        assert_eq!(member.display(), "小明");
    }

    /// 文本与 OutgoingMessage 都能当消息载荷，且拼出的 JSON 与发送器一致
    #[test]
    fn payload_conversions() {
        let from_text = MessagePayload::from("你好").0;
        assert_eq!(
            from_text,
            json!([{"type": "text", "data": {"text": "你好"}}])
        );
        let from_message = MessagePayload::from(OutgoingMessage::at(42)).0;
        assert_eq!(from_message[0]["type"], "at");
    }
}
