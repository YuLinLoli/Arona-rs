//! 聊天记录缓存（框架侧，对应 arona.yml 的 `chatlog:` 段）。
//!
//! QQ 的引用（`reply` 段）只认 OneBot 实现端本地缓存里的近期消息：NTQQ 系实现
//! （NapCat / LLOWeb / Lagrange）对十几二十分钟前的 message_id 就查不到原消息了，
//! 要么丢弃引用段要么整个发送请求报错。所以「引用一条半小时前的消息再触发指令」这件事，
//! 光靠协议层做不到——机器人自己必须留一份聊天记录。
//!
//! 这件事和「哪家插件在玩什么」无关，所以记账、还原、撤回都存在框架里
//! （`data/arona/chatlog.db`，图片按约定**只存原链接**，不另存副本）：
//! 1. 进出两条路各记一笔：听到的消息由 [`crate::onebot::business`] 记账，
//!    说出的消息由 [`crate::onebot::message_sender`] 记账。只有连出站一起记，
//!    才还原得了「用户引用了机器人的回复」；
//! 2. [`quote_prefix`] 决定这次回复挂什么：还在 `quote_ttl_minutes` 窗口内就挂真引用
//!    （原生引用效果），过窗则用本地记录还原成文字+图片；还原时先探一次图片，
//!    图床直链已失效就明确回一句「图片已过期」；
//! 3. [`recall`] 按**库里存的那个 id** 发 `delete_msg`：实现端给的 `real_id` 才是它认的号，
//!    插件手上往往只有事件里的 message_id。
//!
//! 库里查不到的 id 会退回去问一次实现端（`get_msg`），问到就顺手回填——这样老用户的历史消息
//! 能被逐步补进库，而不必等机器人重新听过一遍。过期记录由框架按配置定时清理（见 [`apply`]）。

use crate::config::arona::ChatLogSettings;
use crate::onebot::OneBotEvent;
use crate::onebot::api::{OneBotApi, OneBotError};
use crate::onebot::protocol;
use crate::runtime::message::{MessageReceipt, MessageSegment, MessageTarget, OutgoingMessage};
use once_cell::sync::Lazy;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// 清理任务名（框架按它排期；归属为空，插件停用不会带走它）
pub const PURGE_JOB: &str = "ChatLogPurge";

/// 记录文本的截断长度：引用还原只是给人看的旁证，没必要把长公告整条存进库
const MAX_TEXT: usize = 500;

/// 探活用的浏览器 UA
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS chat_message (
  message_id INTEGER PRIMARY KEY,
  real_id INTEGER,
  grp INTEGER NOT NULL DEFAULT 0,
  qq INTEGER NOT NULL DEFAULT 0,
  from_bot INTEGER NOT NULL DEFAULT 0,
  name TEXT NOT NULL DEFAULT '',
  time INTEGER NOT NULL,
  text TEXT NOT NULL DEFAULT '',
  image TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_chat_time ON chat_message(time);
CREATE INDEX IF NOT EXISTS idx_chat_real ON chat_message(real_id);
"#;

static SETTINGS: Lazy<RwLock<ChatLogSettings>> =
    Lazy::new(|| RwLock::new(ChatLogSettings::default()));
/// 当前库文件；None 表示还没打开过，下次用到时按默认路径自动打开
static FILE: Lazy<RwLock<Option<PathBuf>>> = Lazy::new(|| RwLock::new(None));
static DB: Lazy<Mutex<Option<Connection>>> = Lazy::new(|| Mutex::new(None));
/// 开库失败过一次就只提示一次：不能每条消息都刷一遍错误日志
static OPEN_FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);

/// 探图片直链是否还活着用的客户端（还原引用时才用，超时按 5 秒）。
/// QQ 的图床对没有浏览器 UA 的请求会直接回 403，所以 UA 要带
static PROBER: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent(USER_AGENT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

// ==================== 配置 ====================

/// 当前生效的聊天记录配置
pub fn settings() -> ChatLogSettings {
    SETTINGS.read().unwrap().clone()
}

/// 是否启用（关掉后既不写库也不还原引用）
pub fn enabled() -> bool {
    SETTINGS.read().unwrap().enable
}

/// 应用配置并校准清理任务（arona.yml 加载与热重载时调用）。
/// 钩子与记账点恒常生效、在这里现读开关，用户改开配置后不必重启。
pub fn apply(new_settings: ChatLogSettings) {
    let purge_interval = new_settings.purge_interval_days;
    let keep_days = new_settings.keep_days;
    *SETTINGS.write().unwrap() = new_settings;
    if !enabled() {
        crate::runtime::purge::cancel(PURGE_JOB);
        return;
    }
    // 清库是框架的例行维护，不属于任何插件：归属留空，日志顶在 [Arona] 名下
    crate::runtime::purge::schedule(
        "",
        PURGE_JOB,
        "聊天记录",
        purge_interval,
        keep_days,
        Arc::new(purge),
    );
}

/// 删掉超出保留期的聊天记录，回报条数（由框架的清理排期播报）
pub fn purge() -> i64 {
    let keep_days = settings().keep_days.max(1);
    forget_before(now_secs() - keep_days * 86400)
}

// ==================== 记录 ====================

/// 一条聊天记录。`group` 为 0 表示私聊，`real_id` 是实现端认的另一个号（可能没有）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChatRecord {
    pub message_id: i64,
    /// NapCat / LLOWeb 等实现端的「真实消息 id」：撤回与回查认它，
    /// 事件里带的 message_id 有时只是个临时号
    pub real_id: Option<i64>,
    pub group: i64,
    pub qq: i64,
    /// 机器人自己发的那条
    pub from_bot: bool,
    pub name: String,
    /// 消息时间（秒）
    pub time: i64,
    pub text: String,
    /// 图片的原始链接（入站）或本地路径（机器人生成的），空表示没有图。
    /// 原始链接会过期，取用时得先探一次
    pub image: String,
}

/// 打开（或换到）指定文件的聊天记录库并建表，返回是否成功。
/// 运行期一般不必调用：第一次用到时框架会按 [`crate::runtime::paths::chatlog_file`] 自动打开。
pub fn open(file: &Path) -> bool {
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let opened = match Connection::open(file) {
        Ok(conn) => {
            let _ = conn.pragma_update(None, "journal_mode", "WAL");
            let _ = conn.pragma_update(None, "busy_timeout", 5000);
            match conn.execute_batch(SCHEMA) {
                Ok(()) => Some(conn),
                Err(err) => {
                    fail_once(format!("聊天记录建表失败: {err}"));
                    None
                }
            }
        }
        Err(err) => {
            fail_once(format!("聊天记录库打开失败: {err}"));
            None
        }
    };
    let Some(conn) = opened else {
        return false;
    };
    *FILE.write().unwrap() = Some(file.to_path_buf());
    let mut slot = DB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(old) = slot.replace(conn) {
        let _ = old.close();
    }
    true
}

/// 关闭库连接（备份/恢复文件前释放占用用；下次用到时会自动按原路径重开）
pub fn close() {
    let mut slot = DB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(conn) = slot.take() {
        let _ = conn.close();
    }
    *FILE.write().unwrap() = None;
}

fn fail_once(message: String) {
    if !OPEN_FAILURE_LOGGED.swap(true, Ordering::SeqCst) {
        crate::runtime::log::error(message);
    }
}

/// 惰性开库后的同步查询：库没就绪或执行出错时返回 None（记账失败绝不影响收发消息）
fn query<T>(read: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
    if FILE.read().unwrap().is_none() {
        open(&crate::runtime::paths::chatlog_file());
    }
    let guard = DB.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let conn = guard.as_ref()?;
    match read(conn) {
        Ok(value) => Some(value),
        Err(err) => {
            crate::runtime::log::error(format!("聊天记录读写失败: {err}"));
            None
        }
    }
}

/// 记一条消息（同 id 覆盖重记；`real_id` 只补不抹，免得自己的消息回显把撤回依据洗掉）
pub fn remember(record: &ChatRecord) {
    let real_id = record.real_id;
    let from_bot = record.from_bot as i64;
    let _ = query(|conn| {
        conn.execute(
            "INSERT INTO chat_message
             (message_id, real_id, grp, qq, from_bot, name, time, text, image)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(message_id) DO UPDATE SET
               real_id = COALESCE(excluded.real_id, chat_message.real_id),
               grp = excluded.grp,
               qq = excluded.qq,
               from_bot = excluded.from_bot,
               name = excluded.name,
               time = excluded.time,
               text = excluded.text,
               image = excluded.image",
            params![
                record.message_id,
                real_id,
                record.group,
                record.qq,
                from_bot,
                record.name,
                record.time,
                record.text,
                record.image
            ],
        )
    });
}

/// 按 message_id 或 real_id 查一条记录
pub fn find(message_id: i64) -> Option<ChatRecord> {
    query(|conn| {
        conn.query_row(
            "SELECT message_id, real_id, grp, qq, from_bot, name, time, text, image
             FROM chat_message WHERE message_id = ?1 OR real_id = ?1",
            params![message_id],
            row_to_record,
        )
        .optional()
    })
    .flatten()
}

/// 删掉 `time < cutoff` 的记录，返回条数
pub fn forget_before(cutoff: i64) -> i64 {
    query(|conn| conn.execute("DELETE FROM chat_message WHERE time < ?1", params![cutoff]))
        .unwrap_or(0) as i64
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChatRecord> {
    Ok(ChatRecord {
        message_id: row.get(0)?,
        real_id: row.get(1)?,
        group: row.get(2)?,
        qq: row.get(3)?,
        from_bot: row.get::<_, i64>(4)? != 0,
        name: row.get(5)?,
        time: row.get(6)?,
        text: row.get(7)?,
        image: row.get(8)?,
    })
}

// ==================== 记账点 ====================

/// 把机器人听到的一条消息记进库（未启用、无 message_id 或空白消息时跳过）。
/// 调用点在 [`crate::onebot::business`]，群授权与黑名单已经过滤过。
pub fn record_inbound(event: &OneBotEvent, segments: &[MessageSegment]) {
    if !enabled() {
        return;
    }
    let Some(message_id) = event.message_id else {
        return;
    };
    let text = truncate(&record_text(segments));
    let image = first_image(segments).unwrap_or_default();
    if text.is_empty() && image.is_empty() {
        return;
    }
    let qq = event.user_id.unwrap_or_default();
    let from_bot = qq != 0 && Some(qq) == Some(crate::runtime::config::bot_id());
    remember(&ChatRecord {
        message_id,
        real_id: real_id_of(&event.raw),
        group: event.group_id.unwrap_or_default(),
        qq,
        from_bot,
        name: sender_name(event),
        time: to_secs(event.time),
        text,
        image,
    });
}

/// 把机器人刚发出的一条消息记进库
pub fn record_outgoing(target: MessageTarget, message: &OutgoingMessage, receipt: &MessageReceipt) {
    if !enabled() {
        return;
    }
    let Some(message_id) = receipt.message_id else {
        return;
    };
    let text = truncate(&record_text(&message.segments));
    let image = first_image(&message.segments).unwrap_or_default();
    if text.is_empty() && image.is_empty() {
        return;
    }
    remember(&ChatRecord {
        message_id,
        real_id: receipt.real_id.filter(|real| *real != message_id),
        group: match target {
            MessageTarget::Group(group_id) => group_id,
            MessageTarget::Private(_) => 0,
        },
        qq: crate::runtime::config::bot_id(),
        from_bot: true,
        name: bot_name(),
        time: now_secs(),
        text,
        image,
    });
}

/// 机器人自己的展示名：出站记录要用，取 onebot.yml 的 nickname（热重载后自动跟着变）
fn bot_name() -> String {
    crate::onebot::application::global()
        .map(|application| application.config().nickname)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "Arona".to_string())
}

// ==================== 引用回复 ====================

/// 引用还来不来得及让实现端直接引用
enum QuotePlan {
    /// 还在窗口内：挂原生 reply 段，QQ 上就是真引用
    Attach,
    /// 已过窗：协议层引用不到，用本地记录把那条消息还原出来
    Restore,
}

/// 这次回复该在开头挂什么：真引用段、还原出来的引用，或者什么都不挂。
/// `quoted` 是用户引用的那条消息 id，`at` 是触发这次回复的消息时间戳（秒或毫秒都可，
/// 实现端没给时间时传 0，按"现在"算——否则拿 0 去比窗口会永远判成还来得及）。
pub async fn quote_prefix(quoted: Option<i64>, at: i64) -> Option<Vec<MessageSegment>> {
    let config = settings();
    if !config.enable {
        return None;
    }
    let quoted = quoted?;
    let record = lookup(quoted).await?;
    let at = if at > 0 { to_secs(at) } else { now_secs() };
    match plan(config.quote_ttl_minutes, record.time, at) {
        QuotePlan::Attach => Some(vec![MessageSegment::Reply(quoted)]),
        QuotePlan::Restore => Some(restored_segments(&record, image_segment(&record).await)),
    }
}

fn plan(ttl_minutes: i64, quoted_time: i64, at: i64) -> QuotePlan {
    if quoted_time + ttl_minutes.max(1) * 60 >= at {
        QuotePlan::Attach
    } else {
        QuotePlan::Restore
    }
}

/// 还原出来的引用：文字头 +（探到了就补上图，探不到就说图片已过期）
fn restored_segments(record: &ChatRecord, image: Option<MessageSegment>) -> Vec<MessageSegment> {
    let mut segments = vec![MessageSegment::Text(restore_text(record))];
    match image {
        Some(segment) => segments.push(segment),
        // 图床直链过期是常态（QQ 的图片 URL 带签名）：明确告诉触发者，而不是默默少发一张图
        None if !record.image.is_empty() => {
            segments.push(MessageSegment::Text("\n图片已过期".to_string()))
        }
        None => {}
    }
    segments
}

/// 查这条被引用的消息：先查本地库，查不到就去问实现端并回填
async fn lookup(message_id: i64) -> Option<ChatRecord> {
    if let Some(record) = find(message_id) {
        return Some(record);
    }
    backfill(message_id).await
}

/// `get_msg` 回查并回填：库里没有的旧 id（机器人上线前发的）问一次实现端，
/// 问到就记进库，下次同样的引用就不必再走网络。实现端答不上来时给 None。
async fn backfill(message_id: i64) -> Option<ChatRecord> {
    let api = OneBotApi::global();
    let info = match tokio::time::timeout(Duration::from_secs(3), api.get_msg(message_id)).await {
        Ok(Ok(info)) => info,
        // 超时或实现端答不上来：这条消息我们无从还原
        _ => return None,
    };
    let event = OneBotEvent {
        time: to_secs(info.time),
        self_id: crate::runtime::config::bot_id(),
        post_type: "message".to_string(),
        notice_type: None,
        message_type: Some(info.message_type.clone()),
        sub_type: None,
        message_id: Some(info.message_id),
        user_id: Some(info.user_id),
        operator_id: None,
        group_id: (info.group_id > 0).then_some(info.group_id),
        raw_message: None,
        message: Some(info.message.clone()),
        sender: Some(info.sender.clone()),
        raw: serde_json::Value::Null,
    };
    let segments = protocol::extract_segments(&event);
    let record = ChatRecord {
        message_id,
        real_id: info.real_id.filter(|real| *real != message_id),
        group: info.group_id,
        qq: info.user_id,
        from_bot: info.user_id != 0 && info.user_id == crate::runtime::config::bot_id(),
        name: sender_name(&event),
        time: event.time,
        text: truncate(&record_text(&segments)),
        image: first_image(&segments).unwrap_or_default(),
    };
    remember(&record);
    Some(record)
}

/// 还原出来的引用头：谁在什么时候说了什么
fn restore_text(record: &ChatRecord) -> String {
    let who = if record.name.is_empty() {
        record.qq.to_string()
    } else {
        format!("{}({})", record.name, record.qq)
    };
    let text = if record.text.is_empty() {
        "（无文字内容）".to_string()
    } else {
        record.text.clone()
    };
    format!("[引用 {who} {} 的消息]\n{text}", display_time(record.time))
}

/// 还原引用里的图片：本地文件看还在不在，网络直链先探一次活
async fn image_segment(record: &ChatRecord) -> Option<MessageSegment> {
    if record.image.is_empty() {
        return None;
    }
    if is_web(&record.image) {
        return url_alive(&record.image)
            .await
            .then(|| MessageSegment::Image {
                url: Some(record.image.clone()),
                file: None,
                data: None,
            });
    }
    local_alive(&record.image).then(|| MessageSegment::Image {
        url: None,
        file: Some(record.image.clone()),
        data: None,
    })
}

/// 图床直链此刻还取不取得到：QQ 的图片 URL 带签名会过期，还原引用前得先问一次。
/// 只索要第一个字节，5 秒内没有成功响应就当作已过期。
async fn url_alive(url: &str) -> bool {
    let request = PROBER.get(url).header("range", "bytes=0-0").header(
        "accept",
        "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
    );
    matches!(request.send().await, Ok(response) if response.status().is_success())
}

// ==================== 撤回 ====================

/// 撤回用的 id：把调用方手上的任意一个号换成库里存的**真实** id。
/// NapCat / LLOWeb 一类实现端只认 real_id，拿事件里的 message_id 去 delete_msg 会报「消息不存在」。
pub fn recall_id(message_id: i64) -> i64 {
    find(message_id)
        .and_then(|record| record.real_id)
        .unwrap_or(message_id)
}

/// 撤回一条消息（自己发的，或有管理权限时撤别人的），按库里存的 id 发 `delete_msg`
pub async fn recall(message_id: i64) -> Result<(), OneBotError> {
    OneBotApi::global().delete_msg(recall_id(message_id)).await
}

// ==================== 小工具 ====================

/// 消息段里第一张图的地址（原链接优先，其次实现端给的本地路径）
fn first_image(segments: &[MessageSegment]) -> Option<String> {
    segments.iter().find_map(|segment| match segment {
        MessageSegment::Image { url, file, .. } => url
            .clone()
            .or_else(|| file.clone())
            .filter(|value| !value.is_empty()),
        _ => None,
    })
}

/// 记录用文本：图片另存 `image` 列，引用段只是"回复谁"，都不必混进正文
fn record_text(segments: &[MessageSegment]) -> String {
    segments
        .iter()
        .filter(|segment| {
            !matches!(
                segment,
                MessageSegment::Image { .. } | MessageSegment::Reply(_)
            )
        })
        .map(protocol::segment_display)
        .collect()
}

/// 事件里带的发送者名片/昵称，没有就留空由 qq 号顶上
fn sender_name(event: &OneBotEvent) -> String {
    let sender = event.sender.as_ref();
    ["card", "card_name", "nickname", "user_name"]
        .iter()
        .find_map(|key| sender?.get(*key).and_then(|value| value.as_str()))
        .unwrap_or_default()
        .to_string()
}

/// 实现端在事件里另外给的那个号（NapCat 的 `real_id`），没有时 None
fn real_id_of(raw: &serde_json::Value) -> Option<i64> {
    raw.get("real_id").and_then(|value| value.as_i64())
}

fn is_web(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

/// 本地图片是否还在（机器人渲染的攻略图/日历图可能被后续刷新清掉）
fn local_alive(value: &str) -> bool {
    if Path::new(value).exists() {
        return true;
    }
    value
        .strip_prefix("file://")
        .is_some_and(|path| Path::new(path).exists())
}

fn truncate(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_TEXT {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MAX_TEXT).collect();
    format!("{head}…")
}

fn now_secs() -> i64 {
    chrono::Local::now().timestamp()
}

/// 实现端偶尔把 `time` 写成毫秒，统一折成秒：不然保留期算不清、引用窗口也永远判成过期
pub fn to_secs(time: i64) -> i64 {
    if time > 1_000_000_000_000 {
        time / 1000
    } else {
        time
    }
}

/// 秒级时间戳转展示文本（还原引用头用）
fn display_time(secs: i64) -> String {
    chrono::DateTime::from_timestamp_millis(secs * 1000)
        .map(|moment| {
            moment
                .with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "未知时间".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_image_prefers_original_url() {
        let segments = vec![
            MessageSegment::Text("看图".to_string()),
            MessageSegment::Image {
                url: Some("https://x.example/a.png".to_string()),
                file: Some("/tmp/cache.png".to_string()),
                data: None,
            },
        ];
        assert_eq!(
            first_image(&segments).as_deref(),
            Some("https://x.example/a.png")
        );
    }

    #[test]
    fn inbound_image_falls_back_to_local_path() {
        let segments = vec![MessageSegment::Image {
            url: None,
            file: Some("D:/arona/a.png".to_string()),
            data: None,
        }];
        assert_eq!(first_image(&segments).as_deref(), Some("D:/arona/a.png"));
    }

    #[test]
    fn restored_quote_names_the_sender_and_time() {
        let record = ChatRecord {
            message_id: 1,
            group: 900000001,
            qq: 20002,
            name: "老师".to_string(),
            time: 1_760_000_000,
            text: "这句很久了".to_string(),
            ..Default::default()
        };
        let text = restore_text(&record);
        assert!(text.starts_with("[引用 老师(20002) "), "{text}");
        assert!(text.ends_with("这句很久了"), "{text}");
    }

    #[test]
    fn anonymous_sender_falls_back_to_qq_number() {
        let record = ChatRecord {
            name: String::new(),
            qq: 123,
            text: "x".to_string(),
            ..Default::default()
        };
        assert!(
            restore_text(&record).starts_with("[引用 123 "),
            "没有名片时应只用 qq 号"
        );
    }

    #[test]
    fn record_text_drops_quote_and_image_segments() {
        let message = OutgoingMessage::new(vec![
            MessageSegment::Reply(7),
            MessageSegment::Text("收到".to_string()),
            MessageSegment::Image {
                url: Some("https://x/a.png".to_string()),
                file: None,
                data: None,
            },
            MessageSegment::At(20002),
        ]);
        assert_eq!(record_text(&message.segments), "收到@20002");
    }

    #[test]
    fn quote_inside_the_ttl_is_attached_natively() {
        assert!(matches!(
            plan(30, 1_000, 1_000 + 29 * 60),
            QuotePlan::Attach
        ));
    }

    #[test]
    fn quote_past_the_ttl_is_restored_locally() {
        assert!(matches!(
            plan(30, 1_000, 1_000 + 31 * 60),
            QuotePlan::Restore
        ));
    }

    #[test]
    fn restored_quote_reuses_the_stored_image_link() {
        let record = ChatRecord {
            text: "看图".to_string(),
            image: "https://x/a.png".to_string(),
            ..Default::default()
        };
        let alive = Some(MessageSegment::Image {
            url: Some("https://x/a.png".to_string()),
            file: None,
            data: None,
        });
        let segments = restored_segments(&record, alive);
        assert_eq!(segments.len(), 2, "文字头 + 图");
        assert!(
            matches!(&segments[1], MessageSegment::Image { url, .. } if url.as_deref() == Some("https://x/a.png")),
            "图片按原链接发出"
        );
    }

    #[test]
    fn expired_image_is_reported_to_the_sender() {
        let record = ChatRecord {
            text: "看图".to_string(),
            image: "https://x/a.png".to_string(),
            ..Default::default()
        };
        let segments = restored_segments(&record, None);
        assert_eq!(segments.len(), 2);
        assert!(
            matches!(&segments[1], MessageSegment::Text(text) if text.contains("图片已过期")),
            "取不到图时必须回一句图片已过期"
        );
    }

    #[test]
    fn text_only_quote_gets_no_expiry_notice() {
        let record = ChatRecord {
            text: "纯文字".to_string(),
            ..Default::default()
        };
        let segments = restored_segments(&record, None);
        assert_eq!(segments.len(), 1);
        assert!(!record_text(&segments).contains("过期"));
    }

    #[test]
    fn long_text_is_cut_for_storage() {
        let long = "啊".repeat(MAX_TEXT + 10);
        let stored = truncate(&long);
        assert_eq!(stored.chars().count(), MAX_TEXT + 1);
        assert!(stored.ends_with('…'));
    }

    #[test]
    fn web_links_are_the_only_ones_probed_over_http() {
        assert!(is_web("https://x/a.png"));
        assert!(is_web("http://x/a.png"));
        assert!(!is_web("file:///D:/arona/a.png"));
        assert!(!is_web("D:\\arona\\a.png"));
    }

    #[test]
    fn millisecond_timestamps_are_folded_to_seconds() {
        // 少数实现把 time 写成毫秒：不折算的话保留期内永远删不掉，引用也永远判成"没过期"
        assert_eq!(to_secs(1_760_000_000), 1_760_000_000);
        assert_eq!(to_secs(1_760_000_000_123), 1_760_000_000);
    }

    #[test]
    fn sender_card_beats_nickname() {
        let event = event_with_sender(serde_json::json!({
            "card": "群名片",
            "nickname": "好友昵称",
        }));
        assert_eq!(sender_name(&event), "群名片");
        let event = event_with_sender(serde_json::json!({ "nickname": "好友昵称" }));
        assert_eq!(sender_name(&event), "好友昵称");
        let event = event_with_sender(serde_json::Value::Null);
        assert_eq!(sender_name(&event), "", "没有 sender 时留空，由 qq 号顶上");
    }

    #[test]
    fn napcat_real_id_is_kept_for_recall() {
        let event = serde_json::json!({ "message_id": 11, "real_id": 22 });
        assert_eq!(real_id_of(&event), Some(22));
        assert_eq!(real_id_of(&serde_json::json!({ "message_id": 11 })), None);
    }

    /// 只关心 sender 字段的事件骨架
    fn event_with_sender(sender: serde_json::Value) -> OneBotEvent {
        OneBotEvent {
            time: 0,
            self_id: 10001,
            post_type: "message".to_string(),
            notice_type: None,
            message_type: Some("group".to_string()),
            sub_type: None,
            message_id: Some(1),
            user_id: Some(20002),
            operator_id: None,
            group_id: Some(900000001),
            raw_message: None,
            message: None,
            sender: Some(sender),
            raw: serde_json::Value::Null,
        }
    }

    /// 端到端：换到临时库 → 写记录 → 按 message_id / real_id 都能查回 → 按保留期清掉。
    /// 用例结束时把库换回默认路径，别影响同一次 `cargo test` 里的其他用例。
    #[test]
    fn chat_store_roundtrips_and_honours_the_cutoff() {
        let file = std::env::temp_dir().join(format!(
            "arona-chatlog-{}-{}.db",
            std::process::id(),
            now_secs()
        ));
        assert!(open(&file), "临时库应能打开");

        let now = now_secs();
        remember(&ChatRecord {
            message_id: 4100,
            real_id: Some(9),
            group: 900000001,
            qq: 20002,
            name: "测试老师".to_string(),
            time: now,
            text: "引用我".to_string(),
            image: "https://x.example/a.png".to_string(),
            ..Default::default()
        });
        let found = find(4100).expect("写进去的记录应能读回");
        assert_eq!(found.name, "测试老师");
        assert_eq!(found.text, "引用我");
        assert_eq!(found.image, "https://x.example/a.png", "图片按原链接存");
        assert_eq!(found.real_id, Some(9));
        assert!(
            find(9).is_some(),
            "按实现端的 real_id 也该查得到（撤回时手上往往只有那个号）"
        );

        // 自己的消息回显不带 real_id：重记不能把撤回依据洗掉
        record_inbound(
            &group_event(4100, now, "引用我"),
            &[MessageSegment::Text("引用我".to_string())],
        );
        assert_eq!(find(4100).and_then(|r| r.real_id), Some(9));

        // 保留期边界：cutoff 之前的删掉，之后的留着
        remember(&ChatRecord {
            message_id: 4101,
            time: 0,
            text: "早就该清掉".to_string(),
            ..Default::default()
        });
        assert!(forget_before(now) >= 1, "超出保留期的记录应被清掉");
        assert!(find(4101).is_none());
        assert!(find(4100).is_some(), "保留期内的记录不该被清理误伤");

        close();
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(format!("{}-wal", file.display()));
        let _ = std::fs::remove_file(format!("{}-shm", file.display()));
    }

    fn group_event(message_id: i64, time: i64, text: &str) -> OneBotEvent {
        OneBotEvent {
            time,
            self_id: 10001,
            post_type: "message".to_string(),
            notice_type: None,
            message_type: Some("group".to_string()),
            sub_type: None,
            message_id: Some(message_id),
            user_id: Some(20002),
            operator_id: None,
            group_id: Some(900000001),
            raw_message: Some(text.to_string()),
            message: Some(serde_json::json!([
                { "type": "text", "data": { "text": text } }
            ])),
            sender: Some(serde_json::json!({ "card": "测试老师" })),
            raw: serde_json::Value::Null,
        }
    }

    /// 记账开关与清理排期：enable=false 必须停掉任务，改回 true 要能按新周期重新建回来
    #[test]
    fn apply_schedules_and_cancels_the_purge_job() {
        let original = settings();
        apply(ChatLogSettings {
            enable: false,
            ..Default::default()
        });
        assert!(!enabled());
        assert!(
            !crate::quartz::exists(PURGE_JOB),
            "关掉聊天记录后清理任务不该还在"
        );
        apply(ChatLogSettings {
            enable: true,
            purge_interval_days: 9,
            keep_days: 3,
            quote_ttl_minutes: 30,
        });
        assert!(crate::quartz::exists(PURGE_JOB));
        assert_eq!(
            crate::runtime::purge::schedule_of(PURGE_JOB),
            Some((9, 3)),
            "排期应按新配置重建"
        );
        // 收尾：先把任务停掉再恢复配置，否则 Repeat 型任务登记即执行，
        // 会在本用例之外又清一次库、把并行用例的记录带走
        apply(ChatLogSettings {
            enable: false,
            ..original.clone()
        });
        *SETTINGS.write().unwrap() = original;
    }
}
