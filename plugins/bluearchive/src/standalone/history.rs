//! 聊天记录与「旧消息引用还原」。
//!
//! QQ 的引用（`reply` 段）只认 OneBot 实现端本地缓存里的近期消息：NTQQ 系实现
//! （NapCat / LLOWeb / Lagrange）对十几二十分钟前的 message_id 就查不到原消息了，
//! 要么丢弃引用段要么整个发送请求报错。所以「引用一条半小时前的消息再触发指令」这件事，
//! 光靠协议层做不到——必须自己留一份聊天记录。
//!
//! 本模块干三件事：
//! 1. 把机器人**听到**（消息钩子，框架已按群授权/黑名单过滤过）与**说出**（命令回复）
//!    的消息记进 arona.db 的 `chat_message` 表；图片按用户要求**只存原链接**，不另存副本；
//! 2. 回复时决定挂什么：引用的那条还在 `quote_ttl_minutes` 窗口内就挂真引用（原生引用效果），
//!    过窗则用本地记录把那条消息还原成文字+图片随回复一起发出；
//!    还原时先探一次图片，图床直链已失效就回「图片已过期」；
//! 3. 按 `chatlog` 配置定时清理（默认每 4 天删掉 2 天前的记录）。
//!
//! 库里查不到的 id 会退回去问一次实现端（`get_msg`），问到就顺手回填——这样老用户的历史消息
//! 能被逐步补进库，而不必等机器人重新听过一遍。

use crate::data;
use crate::db::dao::{self, ChatMessageRow};
use arona::onebot::api::OneBotApi;
use arona::onebot::hooks::{BodyFilter, EventContext, HookFlow, ListenerPriority, event_handler};
use arona::onebot::{OneBotEvent, protocol};
use arona::plugin::PluginContext;
use arona::runtime::config::bot_id;
use arona::runtime::dispatcher::CommandContext;
use arona::runtime::message::{MessageSegment, OutgoingMessage};
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 清理任务名（插件被停用时框架按归属整组回收）
pub const PURGE_JOB: &str = "ChatLogPurge";

/// 上一次生效的清理间隔（天，-1 = 尚未初始化）：热重载后据此判断要不要重建任务
static LAST_PURGE_DAYS: AtomicI64 = AtomicI64::new(-1);

/// 机器人自己的展示名：出站记录要用，configure 时从 OneBot 配置里取一次
static BOT_NAME: OnceLock<String> = OnceLock::new();

/// 记录文本的截断长度：引用还原只是给人看的旁证，没必要把长公告整条存进库
const MAX_TEXT: usize = 500;

/// configure 阶段挂上记录钩子与清理任务。
/// 钩子恒常注册（`chatlog.enable` 在每条事件里现读），这样用户改开配置后不必重启。
pub fn install(ctx: &PluginContext) {
    let _ = BOT_NAME.set(ctx.onebot_config.nickname.clone());
    ctx.listen_where(
        &[BodyFilter::GroupMessage, BodyFilter::PrivateMessage],
        // Monitor：排在所有业务钩子之前，别人才短路不掉记账
        ListenerPriority::Monitor,
        event_handler(|context| async move {
            record_inbound(&context);
            HookFlow::Pass
        }),
    );
    on_config_reload(ctx);
}

/// 热重载后按新配置校准清理任务（间隔变了或开关变了才重建）
pub fn on_config_reload(ctx: &PluginContext) {
    let config = crate::config::chat_log();
    let days = config.purge_interval_days.max(1);
    let previous = LAST_PURGE_DAYS.swap(days, Ordering::SeqCst);
    if !config.enable {
        ctx.remove_job(PURGE_JOB);
        return;
    }
    if previous == days && arona::quartz::exists(PURGE_JOB) {
        return;
    }
    ctx.remove_job(PURGE_JOB);
    let interval_secs = days as u64 * 86400;
    ctx.repeat_job(interval_secs, PURGE_JOB, Arc::new(purge_once));
    arona::runtime::log::info(format!(
        "聊天记录清理已启用: 每 {days} 天删除 {} 天前的记录",
        config.keep_days.max(1)
    ));
}

/// 删掉超出保留期的聊天记录
pub fn purge_once() {
    let config = crate::config::chat_log();
    let keep_days = config.keep_days.max(1);
    let cutoff = now_secs() - keep_days * 86400;
    let deleted = dao::forget_chat_messages(cutoff);
    if deleted > 0 {
        arona::runtime::log::info(format!(
            "聊天记录清理: 删除 {keep_days} 天前的 {deleted} 条"
        ));
    }
}

/// 把机器人听到的一条消息记进库（无 message_id、空白消息或已停用记录时跳过）
pub fn record_inbound(context: &EventContext) {
    if !crate::config::chat_log().enable {
        return;
    }
    let Some(message_id) = context.message_id() else {
        return;
    };
    let text = truncate(&record_text(&context.segments));
    let image = first_image(&context.segments).unwrap_or_default();
    if text.is_empty() && image.is_empty() {
        return;
    }
    dao::remember_chat_message(&ChatMessageRow {
        message_id,
        group: context.group_id().unwrap_or_default(),
        qq: context.user_id(),
        from_bot: false,
        name: sender_name(&context.event),
        time: to_secs(context.event.time),
        text,
        image,
    });
}

/// 把机器人刚发出的一条消息记进库：只有连出站一起记，才还原得了「用户引用了机器人的回复」
fn record_outbound(context: &CommandContext, message: &OutgoingMessage, message_id: i64) {
    if !crate::config::chat_log().enable {
        return;
    }
    let text = truncate(&record_text(&message.segments));
    let image = first_image(&message.segments).unwrap_or_default();
    if text.is_empty() && image.is_empty() {
        return;
    }
    dao::remember_chat_message(&ChatMessageRow {
        message_id,
        group: context.group_id.unwrap_or_default(),
        qq: bot_id(),
        from_bot: true,
        name: BOT_NAME
            .get()
            .cloned()
            .unwrap_or_else(|| "Arona".to_string()),
        time: now_secs(),
        text,
        image,
    });
}

/// 插件的统一回复出口：还原用户引用的那条消息，发出后把它自己记进库。
/// 命令实现只管返回内容，引用这件事由包装器统一负责。
pub async fn reply(context: &Arc<CommandContext>, message: OutgoingMessage) {
    let mut outgoing = message.clone();
    if let Some(prefix) = quote_prefix(context).await {
        outgoing.segments.splice(0..0, prefix);
    }
    let receipt = context.reply_message(outgoing).await;
    if let Some(message_id) = receipt.message_id {
        record_outbound(context, &message, message_id);
    }
}

/// 这次回复该在开头挂什么：真引用段、还原出来的引用，或者什么都不挂
async fn quote_prefix(context: &CommandContext) -> Option<Vec<MessageSegment>> {
    let config = crate::config::chat_log();
    if !config.enable {
        return None;
    }
    let quoted = context.quoted?;
    let row = lookup(quoted).await?;
    match plan(config.quote_ttl_minutes, row.time, referenced_at(context)) {
        QuotePlan::Attach => Some(vec![MessageSegment::Reply(quoted)]),
        QuotePlan::Restore => Some(restored_segments(&row, image_segment(&row).await)),
    }
}

/// 引用还来不来得及让实现端直接引用
enum QuotePlan {
    /// 还在窗口内：挂原生 reply 段，QQ 上就是真引用
    Attach,
    /// 已过窗：协议层引用不到，用本地记录把那条消息还原出来
    Restore,
}

fn plan(ttl_minutes: i64, quoted_time: i64, at: i64) -> QuotePlan {
    if quoted_time + ttl_minutes.max(1) * 60 >= at {
        QuotePlan::Attach
    } else {
        QuotePlan::Restore
    }
}

/// 还原出来的引用：文字头 +（探到了就补上图，探不到就说图片已过期）
fn restored_segments(row: &ChatMessageRow, image: Option<MessageSegment>) -> Vec<MessageSegment> {
    let mut segments = vec![MessageSegment::Text(restore_text(row))];
    match image {
        Some(segment) => segments.push(segment),
        // 图床直链过期是常态（QQ 的图片 URL 带签名）：明确告诉触发者，而不是默默少发一张图
        None if !row.image.is_empty() => {
            segments.push(MessageSegment::Text("\n图片已过期".to_string()))
        }
        None => {}
    }
    segments
}

/// 查这条被引用的消息：先查本地库，查不到就去问实现端并回填
async fn lookup(message_id: i64) -> Option<ChatMessageRow> {
    if let Some(row) = dao::find_chat_message(message_id) {
        return Some(row);
    }
    backfill(message_id).await
}

/// `get_msg` 回查并回填：库里没有的旧 id（机器人上线前发的）问一次实现端，
/// 问到就记进库，下次同样的引用就不必再走网络。实现端答不上来时给 None。
async fn backfill(message_id: i64) -> Option<ChatMessageRow> {
    let api = OneBotApi::global();
    let info = match tokio::time::timeout(Duration::from_secs(3), api.get_msg(message_id)).await {
        Ok(Ok(info)) => info,
        // 超时或实现端答不上来：这条消息我们无从还原
        _ => return None,
    };
    let event = OneBotEvent {
        time: to_secs(info.time),
        self_id: bot_id(),
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
    let row = ChatMessageRow {
        message_id,
        group: info.group_id,
        qq: info.user_id,
        from_bot: info.user_id != 0 && info.user_id == bot_id(),
        name: sender_name(&event),
        time: event.time,
        text: truncate(&record_text(&segments)),
        image: first_image(&segments).unwrap_or_default(),
    };
    dao::remember_chat_message(&row);
    Some(row)
}

/// 还原出来的引用头：谁在什么时候说了什么
fn restore_text(row: &ChatMessageRow) -> String {
    let who = if row.name.is_empty() {
        row.qq.to_string()
    } else {
        format!("{}({})", row.name, row.qq)
    };
    let text = if row.text.is_empty() {
        "（无文字内容）".to_string()
    } else {
        row.text.clone()
    };
    format!("[引用 {who} {} 的消息]\n{text}", display_time(row.time))
}

/// 还原引用里的图片：本地文件看还在不在，网络直链先探一次活
async fn image_segment(row: &ChatMessageRow) -> Option<MessageSegment> {
    if row.image.is_empty() {
        return None;
    }
    if is_web(&row.image) {
        return data::http::url_alive(&row.image)
            .await
            .then(|| MessageSegment::Image {
                url: Some(row.image.clone()),
                file: None,
                data: None,
            });
    }
    local_alive(&row.image).then(|| MessageSegment::Image {
        url: None,
        file: Some(row.image.clone()),
        data: None,
    })
}

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
fn to_secs(time: i64) -> i64 {
    if time > 1_000_000_000_000 {
        time / 1000
    } else {
        time
    }
}

/// 引用的那条消息的时间：以触发消息的时间为准（两者只差几秒），没有才用本机时钟
fn referenced_at(context: &CommandContext) -> i64 {
    if context.time > 0 {
        to_secs(context.time)
    } else {
        now_secs()
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
    use arona::runtime::message::{BoxFuture, MessageReceipt, MessageSender, MessageTarget};
    use std::sync::Mutex;

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
        let row = ChatMessageRow {
            message_id: 1,
            group: 900000001,
            qq: 20002,
            from_bot: false,
            name: "老师".to_string(),
            time: 1_760_000_000,
            text: "这句很久了".to_string(),
            image: String::new(),
        };
        let text = restore_text(&row);
        assert!(text.starts_with("[引用 老师(20002) "), "{text}");
        assert!(text.ends_with("这句很久了"), "{text}");
    }

    #[test]
    fn anonymous_sender_falls_back_to_qq_number() {
        let row = ChatMessageRow {
            name: String::new(),
            qq: 123,
            text: "x".to_string(),
            ..Default::default()
        };
        assert!(
            restore_text(&row).starts_with("[引用 123 "),
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
        let row = ChatMessageRow {
            text: "看图".to_string(),
            image: "https://x/a.png".to_string(),
            ..Default::default()
        };
        let alive = Some(MessageSegment::Image {
            url: Some("https://x/a.png".to_string()),
            file: None,
            data: None,
        });
        let segments = restored_segments(&row, alive);
        assert_eq!(segments.len(), 2, "文字头 + 图");
        assert!(
            matches!(&segments[1], MessageSegment::Image { url, .. } if url.as_deref() == Some("https://x/a.png")),
            "图片按原链接发出"
        );
    }

    #[test]
    fn expired_image_is_reported_to_the_sender() {
        let row = ChatMessageRow {
            text: "看图".to_string(),
            image: "https://x/a.png".to_string(),
            ..Default::default()
        };
        let segments = restored_segments(&row, None);
        assert_eq!(segments.len(), 2);
        assert!(
            matches!(&segments[1], MessageSegment::Text(text) if text.contains("图片已过期")),
            "取不到图时必须回一句图片已过期"
        );
    }

    #[test]
    fn text_only_quote_gets_no_expiry_notice() {
        let row = ChatMessageRow {
            text: "纯文字".to_string(),
            ..Default::default()
        };
        let segments = restored_segments(&row, None);
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

    /// 端到端：写库 → 按 id 取回 → 按保留期清掉，字段（含图片原链接）应原样回来。
    ///
    /// 用 `time = 0` 的测试行，收尾时 `time < 1` 的清理正好只删掉它们，
    /// 真实聊天记录的时间戳都远大于 1，不会被误删。
    /// 运行: cargo test chat_log_store -- --ignored --nocapture --test-threads=1
    #[ignore = "写真实 data/bluearchive/arona.db, 运行: cargo test chat_log_store -- --ignored --nocapture --test-threads=1"]
    #[test]
    fn chat_log_store_roundtrips_and_honours_the_cutoff() {
        const OLD: i64 = 9_000_000_000_001;
        const FRESH: i64 = 9_000_000_000_002;
        arona::runtime::services::set_data_root(arona::runtime::paths::data_root());
        assert!(crate::db::start(), "数据库初始化失败");

        let now = now_secs();
        dao::remember_chat_message(&ChatMessageRow {
            message_id: FRESH,
            group: 900000001,
            qq: 20002,
            from_bot: false,
            name: "测试老师".to_string(),
            time: now,
            text: "引用我".to_string(),
            image: "https://x.example/a.png".to_string(),
        });
        let found = dao::find_chat_message(FRESH).expect("写进去的记录应能读回");
        assert_eq!(found.name, "测试老师");
        assert_eq!(found.text, "引用我");
        assert_eq!(found.image, "https://x.example/a.png", "图片按原链接存");
        assert!(!found.from_bot);

        // 保留期边界：cutoff 之前的删掉，之后的留着
        dao::remember_chat_message(&ChatMessageRow {
            message_id: OLD,
            time: 0,
            text: "早就该清掉".to_string(),
            ..Default::default()
        });
        assert!(
            dao::forget_chat_messages(now) >= 1,
            "超出保留期的记录应被清掉"
        );
        assert!(dao::find_chat_message(OLD).is_none());
        assert!(
            dao::find_chat_message(FRESH).is_some(),
            "保留期内的记录不该被清理误伤"
        );

        // 收尾：把测试行的时间挪到 0，再按 time < 1 精确删掉它们
        // （真实记录的时间戳都远大于 1，不会被这一步带走）
        wipe(&[FRESH]);
        assert!(dao::find_chat_message(FRESH).is_none(), "测试行应已清干净");
    }

    /// 端到端：引用还原的三条分支都走真实记录链路（写库 → 裁决 → 拼回复）。
    /// 只用本地可判定的图片（缺文件的本地路径），不联网。
    /// 运行: cargo test quote_restoration -- --ignored --nocapture --test-threads=1
    #[ignore = "写真实 data/bluearchive/arona.db, 运行: cargo test quote_restoration -- --ignored --nocapture --test-threads=1"]
    #[tokio::test]
    async fn quote_restoration_picks_the_right_shape() {
        const RECENT: i64 = 9_000_000_000_011;
        const STALE: i64 = 9_000_000_000_012;
        const DEAD_IMAGE: i64 = 9_000_000_000_013;
        const ECHOED: i64 = 9_000_000_000_099;
        arona::runtime::paths::prepare();
        arona::runtime::services::set_data_root(arona::runtime::paths::data_root());
        arona::runtime::config::set_bot_id(10000);
        assert!(crate::db::start(), "数据库初始化失败");
        let now = now_secs();

        // 1) 二十分钟前的引用：实现端还引用得到，挂原生 reply 段
        dao::remember_chat_message(&ChatMessageRow {
            message_id: RECENT,
            time: now - 20 * 60,
            text: "还来得及".to_string(),
            ..Default::default()
        });
        let sender = echo_sender();
        reply(
            &context(sender.clone(), Some(RECENT), now),
            OutgoingMessage::text("答复一"),
        )
        .await;
        let sent = sender.take();
        assert!(
            matches!(sent[0].segments[0], MessageSegment::Reply(id) if id == RECENT),
            "窗口内的引用应挂原生引用"
        );

        // 2) 一小时前的引用：协议层引用不到，改用本地记录还原，且不再挂原生引用
        dao::remember_chat_message(&ChatMessageRow {
            message_id: STALE,
            qq: 20002,
            name: "测试老师".to_string(),
            time: now - 3600,
            text: "很久以前说的".to_string(),
            ..Default::default()
        });
        let sender = echo_sender();
        reply(
            &context(sender.clone(), Some(STALE), now),
            OutgoingMessage::text("答复二"),
        )
        .await;
        let sent = sender.take();
        let head = record_text(&sent[0].segments);
        assert!(head.contains("[引用 测试老师(20002) "), "{head}");
        assert!(head.contains("很久以前说的"), "{head}");
        assert!(head.contains("答复二"), "还原内容应与回复正文同条发出");
        assert!(
            !sent[0]
                .segments
                .iter()
                .any(|segment| matches!(segment, MessageSegment::Reply(_))),
            "过期的引用不该再挂原生 reply 段"
        );

        // 3) 机器人自己发的消息也入了库，所以"引用机器人的回复"同样能还原
        let echoed = dao::find_chat_message(ECHOED).expect("出站消息应入库");
        assert!(echoed.from_bot, "出站记录要标明是机器人说的");
        assert_eq!(echoed.text, "答复二");

        // 4) 还原时图片已经拿不到：明确回一句图片已过期
        dao::remember_chat_message(&ChatMessageRow {
            message_id: DEAD_IMAGE,
            time: now - 3600,
            text: "看图".to_string(),
            image: "Z:/arona-missing.png".to_string(),
            ..Default::default()
        });
        let sender = echo_sender();
        reply(
            &context(sender.clone(), Some(DEAD_IMAGE), now),
            OutgoingMessage::text("答复三"),
        )
        .await;
        let sent = sender.take();
        assert!(
            record_text(&sent[0].segments).contains("图片已过期"),
            "取不到图时要告诉触发者"
        );

        wipe(&[RECENT, STALE, DEAD_IMAGE, ECHOED]);
    }

    /// 记下发出的消息并回一个固定的 message_id（出站记录要靠它）
    struct EchoSender {
        sent: Mutex<Vec<OutgoingMessage>>,
    }

    impl MessageSender for EchoSender {
        fn send<'a>(
            &'a self,
            target: MessageTarget,
            message: OutgoingMessage,
        ) -> BoxFuture<'a, MessageReceipt> {
            let _ = target;
            Box::pin(async move {
                self.sent.lock().unwrap().push(message);
                MessageReceipt {
                    message_id: Some(9_000_000_000_099),
                }
            })
        }
    }

    impl EchoSender {
        fn take(&self) -> Vec<OutgoingMessage> {
            self.sent.lock().unwrap().clone()
        }
    }

    fn echo_sender() -> Arc<EchoSender> {
        Arc::new(EchoSender {
            sent: Mutex::new(Vec::new()),
        })
    }

    fn context(sender: Arc<EchoSender>, quoted: Option<i64>, time: i64) -> Arc<CommandContext> {
        Arc::new(CommandContext {
            user_id: 20002,
            group_id: Some(900000001),
            text: "/活动".to_string(),
            sender_name: Some("测试老师".to_string()),
            is_admin: false,
            sender_role: None,
            message_id: Some(9_000_000_000_000),
            time,
            quoted,
            segments: Vec::new(),
            sender,
        })
    }

    /// 清掉测试行：先把时间改成 0，再按 `time < 1` 删——真实记录的时间戳都远大于 1
    fn wipe(ids: &[i64]) {
        for message_id in ids {
            dao::remember_chat_message(&ChatMessageRow {
                message_id: *message_id,
                time: 0,
                ..Default::default()
            });
        }
        dao::forget_chat_messages(1);
    }
}
