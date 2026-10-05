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
use crate::framework::Framework;
use crate::onebot::OneBotEvent;
use crate::onebot::api::{OneBotApi, OneBotError};
use crate::onebot::protocol;
use crate::runtime::message::{MessageReceipt, MessageSegment, MessageTarget, OutgoingMessage};
use futures_util::future::BoxFuture;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
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
CREATE INDEX IF NOT EXISTS idx_chat_grp ON chat_message(grp, time);
CREATE INDEX IF NOT EXISTS idx_chat_qq ON chat_message(qq, time);
"#;

/// 聊天记录库的持有者。挂在 [`Framework`] 实例上而不是进程级 `static`，理由与
/// [`OnebotOutlet`](crate::onebot::application::OnebotOutlet) 相同：插件 dll 静态链接了自己
/// 一份框架代码，`static` 配置在 dll 里读到的永远是默认值——实测过：宿主 arona.yml 把
/// `quote_ttl_minutes` 改成 7，插件侧仍读到 30。库文件倒是"碰巧"能读到（dll 按当前工作目录
/// 自己开了同一个 SQLite 文件），但那是走运而非设计：换启动目录、装成服务、用户关掉记录开关，
/// 三条里任何一条都会让它分叉，而且每个 dll 各开一条连接在同一文件上抢写锁。
#[derive(Default)]
pub struct ChatLog {
    settings: RwLock<ChatLogSettings>,
    /// 当前库文件；None 表示还没打开过，下次用到时按 [`crate::runtime::paths::chatlog_file`] 自动打开
    file: RwLock<Option<PathBuf>>,
    db: Mutex<Option<Connection>>,
    /// 开库失败过一次就只提示一次：不能每条消息都刷一遍错误日志
    open_failure_logged: AtomicBool,
    /// 探图片直链是否还活着用的客户端（还原引用时才建，超时按 5 秒）。
    /// QQ 的图床对没有浏览器 UA 的请求会直接回 403，所以 UA 要带
    prober: OnceLock<reqwest::Client>,
}

impl ChatLog {
    /// 当前生效的聊天记录配置
    pub fn settings(&self) -> ChatLogSettings {
        self.settings.read().unwrap().clone()
    }

    /// 是否启用（关掉后既不写库也不还原引用）
    pub fn enabled(&self) -> bool {
        self.settings.read().unwrap().enable
    }

    /// 应用配置并校准清理任务（arona.yml 加载与热重载时调用）。
    /// 钩子与记账点恒常生效、在这里现读开关，用户改开配置后不必重启。
    ///
    /// 清理排期只由**进程默认实例**那份库维护：`quartz` 的登记表是进程级的，隔离实例
    /// （测试、多实例宿主）跟着排同名任务会互相盖掉排期，而 [`purge`] 清的又始终是默认
    /// 实例那个文件——等于替别家清库。所以非默认实例只存设置，不碰排期。
    pub fn apply(&self, new_settings: ChatLogSettings) {
        let purge_interval = new_settings.purge_interval_days;
        let keep_days = new_settings.keep_days;
        *self.settings.write().unwrap() = new_settings;
        let is_default_instance = std::ptr::eq(
            Arc::as_ptr(Framework::global().chatlog()),
            self as *const ChatLog,
        );
        if !is_default_instance {
            return;
        }
        if !self.enabled() {
            crate::runtime::purge::cancel(PURGE_JOB);
            return;
        }
        // 清库是框架的例行维护，不属于任何插件：归属留空，日志顶在 [Arona] 名下。
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
    pub fn purge(&self) -> i64 {
        let keep_days = self.settings().keep_days.max(1);
        self.forget_before(now_secs() - keep_days * 86400)
    }
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

impl ChatLog {
    /// 打开（或换到）指定文件的聊天记录库并建表，返回是否成功。
    /// 运行期一般不必调用：第一次用到时框架会按 [`crate::runtime::paths::chatlog_file`] 自动打开。
    pub fn open(&self, file: &Path) -> bool {
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
                        self.fail_once(format!("聊天记录建表失败: {err}"));
                        None
                    }
                }
            }
            Err(err) => {
                self.fail_once(format!("聊天记录库打开失败: {err}"));
                None
            }
        };
        let Some(conn) = opened else {
            return false;
        };
        *self.file.write().unwrap() = Some(file.to_path_buf());
        let mut slot = self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(old) = slot.replace(conn) {
            let _ = old.close();
        }
        true
    }

    /// 当前用的库文件；None 表示还没开过（下次用到时按路径约定自动开）
    pub fn file(&self) -> Option<PathBuf> {
        self.file.read().unwrap().clone()
    }

    /// 关闭库连接（备份/恢复文件前释放占用用；下次用到时会自动按原路径重开）
    pub fn close(&self) {
        let mut slot = self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(conn) = slot.take() {
            let _ = conn.close();
        }
        *self.file.write().unwrap() = None;
    }

    fn fail_once(&self, message: String) {
        if !self.open_failure_logged.swap(true, Ordering::SeqCst) {
            crate::runtime::log::error(message);
        }
    }

    /// 惰性开库后的同步查询：库没就绪或执行出错时返回 None（记账失败绝不影响收发消息）
    fn query<T>(&self, read: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
        if self.file.read().unwrap().is_none() {
            self.open(&crate::runtime::paths::chatlog_file());
        }
        let guard = self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    pub fn remember(&self, record: &ChatRecord) {
        let real_id = record.real_id;
        let from_bot = record.from_bot as i64;
        let _ = self.query(|conn| {
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
    pub fn find(&self, message_id: i64) -> Option<ChatRecord> {
        self.query(|conn| {
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
    pub fn forget_before(&self, cutoff: i64) -> i64 {
        self.query(|conn| conn.execute("DELETE FROM chat_message WHERE time < ?1", params![cutoff]))
            .unwrap_or(0) as i64
    }

    /// 往前翻某个会话的留档（mirai 的 `group.history` / `user.history`）。
    /// `before` 传上一页的游标，`None` 表示从最新一条开始；结果按时间**从新到旧**排。
    ///
    /// 只读框架自己这份库，不发协议请求：实现端的历史它从没听过，`chatlog.enable=false`
    /// 时更是空表，所以这条查的是"机器人上线以来见过的话"，不是QQ 服务器上的聊天记录。
    pub fn recent(
        &self,
        scope: HistoryScope,
        limit: i64,
        before: Option<HistoryCursor>,
    ) -> HistoryPage {
        let limit = limit.clamp(1, HISTORY_LIMIT);
        // 两个维度都要"精确匹配或不设限"：私聊就是 grp=0 那几条，不能把同一个人在群里
        // 说的话也捞进来，所以"不限"用 -1 表示，而不是拿 0 当通配（0 正是私聊的 grp 值）。
        let (grp, qq) = match scope {
            HistoryScope::Group(group) => (group, -1),
            HistoryScope::Private(user) => (0, user),
        };
        // 游标同样用 -1 当"不限制"的哨兵：0 是个合法时间戳，拿它当哨兵会把第一页判成翻完了。
        // 同一秒内按 message_id 倒着走，否则同秒的几条会被游标跳过或重复取到。
        let (at, last_id) = before.map_or((-1, 0), |cursor| (cursor.time, cursor.message_id));
        let records = self
            .query(|conn| {
                let mut statement = conn.prepare(
                    "SELECT message_id, real_id, grp, qq, from_bot, name, time, text, image
             FROM chat_message
             WHERE grp = ?1 AND (?2 < 0 OR qq = ?2)
               AND (?3 < 0 OR time < ?3 OR (time = ?3 AND message_id < ?4))
             ORDER BY time DESC, message_id DESC
             LIMIT ?5",
                )?;
                let rows =
                    statement.query_map(params![grp, qq, at, last_id, limit], row_to_record)?;
                rows.collect::<rusqlite::Result<Vec<ChatRecord>>>()
            })
            .unwrap_or_default();
        HistoryPage { records }
    }
}

/// 单次翻页的条数上限：一句插件不该能把整库拉进内存
const HISTORY_LIMIT: i64 = 200;

/// 历史翻页的游标：上一页最后（也就是最旧）那条的时间与消息号
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryCursor {
    pub time: i64,
    pub message_id: i64,
}

/// 翻历史的目标会话（mirai 的 `Group` 与 `User` 两档）。
/// 私聊在库里记成 `grp=0` + 发送者 qq，群内某人说的话要用 [`HistoryScope::Group`] 再自己筛。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryScope {
    /// 某个群的消息
    Group(i64),
    /// 与某个人的私聊
    Private(i64),
}

/// 一次 [`ChatLog::recent`] 拿回的那页，按时间从新到旧排
pub struct HistoryPage {
    pub records: Vec<ChatRecord>,
}

impl HistoryPage {
    /// 翻下一页的游标；空页给 None，据此就能判断"到头了"
    pub fn cursor(&self) -> Option<HistoryCursor> {
        self.records.last().map(|record| HistoryCursor {
            time: record.time,
            message_id: record.message_id,
        })
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
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

impl ChatLog {
    /// 把机器人听到的一条消息记进库（未启用、无 message_id 或空白消息时跳过）。
    /// 调用点在 [`crate::onebot::business`]，群授权与黑名单已经过滤过。
    pub fn record_inbound(&self, event: &OneBotEvent, segments: &[MessageSegment]) {
        if !self.enabled() {
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
        self.remember(&ChatRecord {
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
    pub fn record_outgoing(
        &self,
        target: MessageTarget,
        message: &OutgoingMessage,
        receipt: &MessageReceipt,
    ) {
        if !self.enabled() {
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
        self.remember(&ChatRecord {
            message_id,
            real_id: receipt.real_id.filter(|real| *real != message_id),
            // 临时会话记在来源群名下：撤回与回查要能在群上下文里找到它
            group: target.group().unwrap_or_default(),
            qq: crate::runtime::config::bot_id(),
            from_bot: true,
            name: bot_name(),
            time: now_secs(),
            text,
            image,
        });
    }
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

impl ChatLog {
    /// 这次回复该在开头挂什么：真引用段、还原出来的引用，或者什么都不挂。
    /// `quoted` 是用户引用的那条消息 id，`at` 是触发这次回复的消息时间戳（秒或毫秒都可，
    /// 实现端没给时间时传 0，按"现在"算——否则拿 0 去比窗口会永远判成还来得及）。
    pub async fn quote_prefix(&self, quoted: Option<i64>, at: i64) -> Option<Vec<MessageSegment>> {
        let config = self.settings();
        if !config.enable {
            return None;
        }
        let quoted = quoted?;
        let record = self.lookup(quoted).await?;
        let at = if at > 0 { to_secs(at) } else { now_secs() };
        match plan(config.quote_ttl_minutes, record.time, at) {
            QuotePlan::Attach => Some(vec![MessageSegment::Reply(quoted)]),
            QuotePlan::Restore => Some(restored_segments(
                &record,
                self.image_segment(&record).await,
            )),
        }
    }

    /// 查这条被引用的消息：先查本地库，查不到就去问实现端并回填。
    /// 公开是因为 mirai 的 `MessageSource.getRecord()` 要的就是这个语义
    /// （只看 [`ChatLog::find`] 会漏掉机器人上线前发的那批）。
    pub async fn lookup(&self, message_id: i64) -> Option<ChatRecord> {
        if let Some(record) = self.find(message_id) {
            return Some(record);
        }
        self.backfill(message_id).await
    }

    /// `get_msg` 回查并回填：库里没有的旧 id（机器人上线前发的）问一次实现端，
    /// 问到就记进库，下次同样的引用就不必再走网络。实现端答不上来时给 None。
    async fn backfill(&self, message_id: i64) -> Option<ChatRecord> {
        let api = OneBotApi::global();
        let info = match tokio::time::timeout(Duration::from_secs(3), api.get_msg(message_id)).await
        {
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
        self.remember(&record);
        Some(record)
    }

    /// 还原引用里的图片：实现端给过图片标识的直接带回去，本地文件看还在不在，网络直链先探一次活
    async fn image_segment(&self, record: &ChatRecord) -> Option<MessageSegment> {
        if record.image.is_empty() {
            return None;
        }
        if let Some(id) = record.image.strip_prefix(FILE_ID_SCHEME) {
            // 这不是一条可探活的 URL：取不取得到只有实现端自己知道，原样带回就是最稳的写法
            return Some(MessageSegment::Image {
                url: None,
                file: None,
                data: None,
                file_id: Some(id.to_string()),
            });
        }
        if is_web(&record.image) {
            return self
                .url_alive(&record.image)
                .await
                .then(|| MessageSegment::Image {
                    url: Some(record.image.clone()),
                    file: None,
                    data: None,
                    file_id: None,
                });
        }
        local_alive(&record.image).then(|| MessageSegment::Image {
            url: None,
            file: Some(record.image.clone()),
            data: None,
            file_id: None,
        })
    }

    /// 图床直链此刻还取不取得到：QQ 的图片 URL 带签名会过期，还原引用前得先问一次。
    /// 只索要第一个字节，5 秒内没有成功响应就当作已过期。
    async fn url_alive(&self, url: &str) -> bool {
        let request = self.prober().get(url).header("range", "bytes=0-0").header(
            "accept",
            "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
        );
        matches!(request.send().await, Ok(response) if response.status().is_success())
    }

    /// 探活客户端挂在实例上惰性建：宿主那份连接池与 TLS 上下文因此能被插件共用
    fn prober(&self) -> &reqwest::Client {
        self.prober.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .user_agent(USER_AGENT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
    }

    /// 撤回用的 id：把调用方手上的任意一个号换成库里存的**真实** id。
    /// NapCat / LLOWeb 一类实现端只认 real_id，拿事件里的 message_id 去 delete_msg 会报「消息不存在」。
    pub fn recall_id(&self, message_id: i64) -> i64 {
        self.find(message_id)
            .and_then(|record| record.real_id)
            .unwrap_or(message_id)
    }

    /// 撤回一条消息（自己发的，或有管理权限时撤别人的），按库里存的 id 发 `delete_msg`
    pub async fn recall(&self, message_id: i64) -> Result<(), OneBotError> {
        OneBotApi::global()
            .delete_msg(self.recall_id(message_id))
            .await
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

// ==================== 撤回 ====================

// ==================== 对外门面 ====================
//
// 下面这些自由函数是既有调用点（business/message_sender/dispatcher/GUI）与插件一直使用的
// 写法，签名一个都不改；它们只把调用转到**进程默认实例**那份库上。
//
// 但"转到实例上"只解决了去哪儿找这份状态，没解决用哪份代码碰它——所以这里一律经
// [`ChatLogFace`] 的 trait 对象，而不是直接调 [`ChatLog`] 的同名方法。

/// [`ChatLog`] 的**跨镜像出口**：动态插件 dll 只能通过它读这份库。
///
/// 一个 dll 里静态链接了整整一份框架代码，也带着**自己那一份 bundled sqlite**。
/// 若在插件侧直接 `ChatLog::find`，编译进 dll 的那份实现会拿着**宿主创建的** `sqlite3*`
/// 句柄去调 dll 自己那份引擎——两套引擎各有一批进程级全局（内存分配、互斥锁池、配置），
/// 互不相认，实测是当场段错误、整个宿主一起没（纯 Rust 字段则完全正常：同一份 arona.yml
/// 的 `quote_ttl_minutes: 7` 能读对，库路径也报出宿主那一份）。
/// 经**宿主生成**的 vtable 调用才是把执行搬回宿主镜像：函数指针指向宿主的代码，
/// sqlite 连接自始至终只被宿主那份引擎触碰。`prober` 里那份 reqwest 客户端同理，
/// 所以 async 的几问也在宿主侧执行、只把 future 递回来。
///
/// 方法名与 [`ChatLog`] 的固有方法刻意重合，实现体因此用 `ChatLog::x(self, ..)` 的
/// 完全限定写法——不然 `self.x()` 会选中固有方法以外的另一条路，读起来分不清谁调谁。
pub trait ChatLogFace: Send + Sync {
    fn settings(&self) -> ChatLogSettings;
    fn enabled(&self) -> bool;
    fn apply(&self, new_settings: ChatLogSettings);
    fn purge(&self) -> i64;
    fn open(&self, file: &Path) -> bool;
    fn file(&self) -> Option<PathBuf>;
    fn close(&self);
    fn remember(&self, record: &ChatRecord);
    fn find(&self, message_id: i64) -> Option<ChatRecord>;
    fn lookup(&self, message_id: i64) -> BoxFuture<'_, Option<ChatRecord>>;
    fn forget_before(&self, cutoff: i64) -> i64;
    fn record_inbound(&self, event: &OneBotEvent, segments: &[MessageSegment]);
    fn record_outgoing(
        &self,
        target: MessageTarget,
        message: &OutgoingMessage,
        receipt: &MessageReceipt,
    );
    fn quote_prefix(
        &self,
        quoted: Option<i64>,
        at: i64,
    ) -> BoxFuture<'_, Option<Vec<MessageSegment>>>;
    fn recall_id(&self, message_id: i64) -> i64;
    fn recall(&self, message_id: i64) -> BoxFuture<'_, Result<(), OneBotError>>;
    /// 追加在**末尾**：vtable 前面那批槽位不能因为加方法而挪位（旧 dll 是按编译期下标调的）
    fn recent(&self, scope: HistoryScope, limit: i64, before: Option<HistoryCursor>)
    -> HistoryPage;
}

impl ChatLogFace for ChatLog {
    fn settings(&self) -> ChatLogSettings {
        ChatLog::settings(self)
    }
    fn enabled(&self) -> bool {
        ChatLog::enabled(self)
    }
    fn apply(&self, new_settings: ChatLogSettings) {
        ChatLog::apply(self, new_settings)
    }
    fn purge(&self) -> i64 {
        ChatLog::purge(self)
    }
    fn open(&self, file: &Path) -> bool {
        ChatLog::open(self, file)
    }
    fn file(&self) -> Option<PathBuf> {
        ChatLog::file(self)
    }
    fn close(&self) {
        ChatLog::close(self)
    }
    fn remember(&self, record: &ChatRecord) {
        ChatLog::remember(self, record)
    }
    fn find(&self, message_id: i64) -> Option<ChatRecord> {
        ChatLog::find(self, message_id)
    }
    fn lookup(&self, message_id: i64) -> BoxFuture<'_, Option<ChatRecord>> {
        Box::pin(ChatLog::lookup(self, message_id))
    }
    fn forget_before(&self, cutoff: i64) -> i64 {
        ChatLog::forget_before(self, cutoff)
    }
    fn record_inbound(&self, event: &OneBotEvent, segments: &[MessageSegment]) {
        ChatLog::record_inbound(self, event, segments)
    }
    fn record_outgoing(
        &self,
        target: MessageTarget,
        message: &OutgoingMessage,
        receipt: &MessageReceipt,
    ) {
        ChatLog::record_outgoing(self, target, message, receipt)
    }
    fn quote_prefix(
        &self,
        quoted: Option<i64>,
        at: i64,
    ) -> BoxFuture<'_, Option<Vec<MessageSegment>>> {
        Box::pin(ChatLog::quote_prefix(self, quoted, at))
    }
    fn recall_id(&self, message_id: i64) -> i64 {
        ChatLog::recall_id(self, message_id)
    }
    fn recall(&self, message_id: i64) -> BoxFuture<'_, Result<(), OneBotError>> {
        Box::pin(ChatLog::recall(self, message_id))
    }
    fn recent(
        &self,
        scope: HistoryScope,
        limit: i64,
        before: Option<HistoryCursor>,
    ) -> HistoryPage {
        ChatLog::recent(self, scope, limit, before)
    }
}

fn host() -> &'static dyn ChatLogFace {
    &**Framework::global().chatlog_api()
}

/// 当前生效的聊天记录配置
pub fn settings() -> ChatLogSettings {
    host().settings()
}

/// 是否启用（关掉后既不写库也不还原引用）
pub fn enabled() -> bool {
    host().enabled()
}

/// 应用配置并校准清理任务
pub fn apply(new_settings: ChatLogSettings) {
    host().apply(new_settings)
}

/// 删掉超出保留期的聊天记录，回报条数
pub fn purge() -> i64 {
    host().purge()
}

/// 打开（或换到）指定文件的聊天记录库并建表
pub fn open(file: &Path) -> bool {
    host().open(file)
}

/// 当前用的库文件
pub fn file() -> Option<PathBuf> {
    host().file()
}

/// 关闭库连接
pub fn close() {
    host().close()
}

/// 记一条消息
pub fn remember(record: &ChatRecord) {
    host().remember(record)
}

/// 按 message_id 或 real_id 查一条记录
pub fn find(message_id: i64) -> Option<ChatRecord> {
    host().find(message_id)
}

/// 查这条被引用的消息：先查本地库，查不到就去问实现端并回填
pub async fn lookup(message_id: i64) -> Option<ChatRecord> {
    host().lookup(message_id).await
}

/// 删掉 `time < cutoff` 的记录，返回条数
pub fn forget_before(cutoff: i64) -> i64 {
    host().forget_before(cutoff)
}

/// 把机器人听到的一条消息记进库
pub fn record_inbound(event: &OneBotEvent, segments: &[MessageSegment]) {
    host().record_inbound(event, segments)
}

/// 把机器人刚发出的一条消息记进库
pub fn record_outgoing(target: MessageTarget, message: &OutgoingMessage, receipt: &MessageReceipt) {
    host().record_outgoing(target, message, receipt)
}

/// 这次回复该在开头挂什么（见 [`ChatLog::quote_prefix`]）
pub async fn quote_prefix(quoted: Option<i64>, at: i64) -> Option<Vec<MessageSegment>> {
    host().quote_prefix(quoted, at).await
}

/// 撤回用的 id（见 [`ChatLog::recall_id`]）
pub fn recall_id(message_id: i64) -> i64 {
    host().recall_id(message_id)
}

/// 撤回一条消息（见 [`ChatLog::recall`]）
pub async fn recall(message_id: i64) -> Result<(), OneBotError> {
    host().recall(message_id).await
}

/// 往前翻某个会话的留档（见 [`ChatLog::recent`]）
pub fn recent(scope: HistoryScope, limit: i64, before: Option<HistoryCursor>) -> HistoryPage {
    host().recent(scope, limit, before)
}

// ==================== 小工具 ====================

/// 内联负载（`base64://…` / `data:…`）不是可复用的地址：还原引用时既没法当网络直链探活，
/// 也不是本地路径，原样入库只会让每条消息在 SQLite 里占几百 KB～几 MB，读回来还会被误报成
/// 「图片已过期」。`first_image` 在这里把它们滤掉。
///
/// 消息段里第一张图的地址（原链接优先，其次实现端给的本地路径，再退到它给的图片标识）。
/// 只有 `file_id` 时存成 `fileid://<id>`：库里那一列永远是一个可直接还原的字符串，
/// 而 QQ 的图床直链会过期，`file_id` 反而是三者里最耐用的那个。
fn first_image(segments: &[MessageSegment]) -> Option<String> {
    segments.iter().find_map(|segment| match segment {
        MessageSegment::Image {
            url, file, file_id, ..
        } => [url.as_deref(), file.as_deref()]
            .into_iter()
            .flatten()
            // url 优先，但内联 base64 既不是直链也不是路径，换下一个槽再试
            .find(|value| !value.is_empty() && !crate::onebot::protocol::is_inline_media(value))
            .map(str::to_string)
            .or_else(|| {
                file_id
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("{FILE_ID_SCHEME}{value}"))
            }),
        _ => None,
    })
}

/// 库里那一列用来标记"这是实现端给的图片标识，不是 URL 也不是本地路径"。
/// 框架内两处要认它：本模块还原引用时，和 [`crate::bot::Image::from_stored`]。
pub(crate) const FILE_ID_SCHEME: &str = "fileid://";

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

/// 事件里带的发送者名片/昵称，没有就留空由 qq 号顶上。
/// 空串按「没给」处理：QQ 的群名片默认就是空，此刻要往昵称兜底，
/// 否则翻出来的历史记录全都署不出名字。
fn sender_name(event: &OneBotEvent) -> String {
    let sender = event.sender.as_ref();
    ["card", "card_name", "nickname", "user_name"]
        .iter()
        .find_map(|key| {
            sender?
                .get(*key)
                .and_then(|value| value.as_str())
                .filter(|text| !text.trim().is_empty())
        })
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
                file_id: Some("FD1".to_string()),
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
            file_id: None,
        }];
        assert_eq!(first_image(&segments).as_deref(), Some("D:/arona/a.png"));
    }

    /// 三家实现端里只有 NTQQ 系会顺手给 file_id，且常常只给这个（URL 与路径都空）。
    /// 库里那一列必须能把它还原回来，否则"图片过期回复"对这类实现端等于失明。
    #[test]
    fn inbound_image_with_only_a_file_id_is_kept_recoverable() {
        let segments = vec![MessageSegment::Image {
            url: None,
            file: None,
            data: None,
            file_id: Some("E2A1B3C4".to_string()),
        }];
        assert_eq!(first_image(&segments).as_deref(), Some("fileid://E2A1B3C4"));

        // 三个槽全空（只给了 base64 或什么都没给）时不该存出一堆前缀空串
        let empty = vec![MessageSegment::Image {
            url: Some(String::new()),
            file: None,
            data: None,
            file_id: Some(String::new()),
        }];
        assert_eq!(first_image(&empty), None);
    }

    /// 内联 base64 不是可还原的地址：存进库只会撑爆磁盘，引用时还会被误报成「图片已过期」。
    #[test]
    fn inbound_image_drops_inline_payloads() {
        let inline = vec![MessageSegment::Image {
            url: Some("base64://AAAA".to_string()),
            file: None,
            data: Some(vec![0, 1, 2]),
            file_id: None,
        }];
        assert_eq!(first_image(&inline), None);

        // URL 是内联负载、但实现端另外给了 file_id：退回 file_id，仍然可还原
        let with_id = vec![MessageSegment::Image {
            url: Some("data:image/png;base64,AAAA".to_string()),
            file: None,
            data: None,
            file_id: Some("E2A1B3C4".to_string()),
        }];
        assert_eq!(first_image(&with_id).as_deref(), Some("fileid://E2A1B3C4"));
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
                file_id: None,
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

    /// 库里那一列带 `fileid://` 前缀时，还原出来必须是**只带 file_id** 的图片段：
    /// 既没有可探活的 URL，也不该去文件系统里找一个不存在的路径。
    #[tokio::test]
    async fn stored_file_id_restores_without_probing() {
        let record = ChatRecord {
            text: "看图".to_string(),
            image: "fileid://E2A1B3C4".to_string(),
            ..Default::default()
        };
        let segment = ChatLog::default()
            .image_segment(&record)
            .await
            .expect("fileid 前应直接还原，不看网络也不看磁盘");
        assert!(
            matches!(&segment, MessageSegment::Image { file_id, url, file, .. }
                if file_id.as_deref() == Some("E2A1B3C4") && url.is_none() && file.is_none()),
            "还原结果：{segment:?}"
        );
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
            file_id: None,
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
        // QQ 的群名片默认就是空串：端到端实测里翻历史全是署不出名的记录，就是这里把空串当成了「有名片」
        let event = event_with_sender(serde_json::json!({
            "card": "",
            "nickname": "好友昵称",
        }));
        assert_eq!(sender_name(&event), "好友昵称", "空名片要往昵称兜底");
        let event = event_with_sender(serde_json::json!({
            "card": "   ",
            "user_name": "另一个名字",
        }));
        assert_eq!(sender_name(&event), "另一个名字", "只有空白也算没给");
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

    /// mirai 的 `group.history` 那套语义：按会话翻、从新到旧、游标续页不重不漏、别家会话不串台。
    /// 用**私有实例**而不是 `open()` 那组自由函数：全局库只有一个 `Connection`，
    /// 和同模块别的用它的用例并行跑会互相把连接换掉，谁先跑完全看调度。
    #[test]
    fn history_pages_backwards_without_gaps_or_overlaps() {
        let file = std::env::temp_dir().join(format!(
            "arona-chatlog-hist-{}-{}.db",
            std::process::id(),
            now_secs()
        ));
        let chatlog = ChatLog::default();
        assert!(chatlog.open(&file), "临时库应能打开");
        let now = now_secs();
        for (id, group, qq, time) in [
            (5001, 900000001, 20002, now),
            (5002, 900000001, 20003, now),
            (5003, 900000001, 20002, now - 10),
            (5004, 900000002, 20002, now - 1),
            (5005, 0, 20002, now - 2),
        ] {
            chatlog.remember(&ChatRecord {
                message_id: id,
                group,
                qq,
                time,
                text: format!("m{id}"),
                ..Default::default()
            });
        }

        let first = chatlog.recent(HistoryScope::Group(900000001), 2, None);
        assert_eq!(first.len(), 2, "一页两条");
        assert_eq!(
            first
                .records
                .iter()
                .map(|record| record.message_id)
                .collect::<Vec<_>>(),
            vec![5002, 5001],
            "从新到旧；同一秒里按 message_id 倒着走"
        );

        let second = chatlog.recent(HistoryScope::Group(900000001), 2, first.cursor());
        assert_eq!(second.records[0].message_id, 5003, "游标续上下一页");
        assert!(
            chatlog
                .recent(HistoryScope::Group(900000001), 2, second.cursor())
                .is_empty(),
            "翻到头该给空页，cursor() 也随之为 None"
        );
        assert_eq!(second.cursor().map(|cursor| cursor.message_id), Some(5003));

        assert_eq!(
            chatlog
                .recent(HistoryScope::Private(20002), 10, None)
                .records
                .iter()
                .map(|record| record.message_id)
                .collect::<Vec<_>>(),
            vec![5005],
            "私聊只回 grp=0 那几条，别把同一个人在群里说的话捞进来"
        );
        assert_eq!(
            chatlog
                .recent(HistoryScope::Group(900000002), 10, None)
                .len(),
            1,
            "别的群不该串进来"
        );

        // limit 收到上限：一句插件不该能把整库拉进内存
        assert!(
            chatlog
                .recent(HistoryScope::Group(900000001), 100_000, None)
                .len()
                <= HISTORY_LIMIT as usize,
            "翻页条数应被 HISTORY_LIMIT 夹住"
        );

        chatlog.close();
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
        *Framework::global().chatlog().settings.write().unwrap() = original;
    }

    /// 库与配置必须挂在实例上，而不是进程级 `static`：插件 dll 经 `adopt_host` 认领宿主的
    /// `Framework` 之后，`arona::runtime::chatlog::*` 这些自由函数读到的才是宿主真正在用的
    /// 那份配置与那条连接。改造前这里是一枚 `Lazy` 全局，dll 里另有一份默认值——实测过：
    /// 宿主 arona.yml 写 `quote_ttl_minutes: 7`，插件侧仍读到出厂的 30。
    #[test]
    fn library_and_settings_are_scoped_to_the_framework_instance() {
        let one = Framework::new();
        let other = Framework::new();
        assert!(
            !std::ptr::eq(Arc::as_ptr(one.chatlog()), Arc::as_ptr(other.chatlog())),
            "两套实例不该共用同一份聊天记录状态"
        );
        assert!(
            other.chatlog().file().is_none(),
            "新实例不该带着别人的库文件"
        );
        // 跨镜像出口必须指向**同一份**库：插件侧那批自由函数只认 `chatlog_api`，
        // 另起一份就又回到"两边各持一份状态"那个老问题。
        // 只比数据指针：两处 unsize 转换各生成一份 vtable 常量是允许的，要锁的是对象。
        let concrete = Arc::as_ptr(one.chatlog()).cast::<u8>();
        let shared = Arc::as_ptr(one.chatlog_api()).cast::<u8>();
        assert!(
            std::ptr::eq(concrete, shared),
            "chatlog_api 该是 chatlog 的另一副面孔，不是第二份库"
        );
        other.chatlog().apply(ChatLogSettings {
            quote_ttl_minutes: 7,
            ..ChatLogSettings::default()
        });
        assert_eq!(other.chatlog().settings().quote_ttl_minutes, 7);
        assert_ne!(
            one.chatlog().settings().quote_ttl_minutes,
            7,
            "改一套实例的配置不该波及另一套"
        );
    }
}
