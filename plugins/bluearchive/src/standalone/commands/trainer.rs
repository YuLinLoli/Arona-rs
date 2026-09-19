//! /攻略 命令（对应原版 standalone/commands/StandaloneTrainer）
//!
//! 关键词与行为：
//! - 日服活动 / 国际服活动 / 国服活动：抓取 GameKee 当期活动攻略图并逐张发送
//! - 日程笔记：抓取 GameKee 日程笔记图并以合并转发发送
//! - 日服卡池 / 国服卡池 / 国际服卡池：合并转发当期卡池（卡池图 + 角色信息）
//! - 其它关键词：走 arona 云端图片库，精确命中直接发图，模糊命中列出建议并等待数字回复

use crate::config::{TrainerConfig, TrainerFileConfig, TrainerOverride};
use crate::data::arona_backend::{self, FUZZY_IMAGE_RESULT, ImageRequestResult, ImageResult};
use crate::data::game_kee::{self, GachaCharacter, GachaServer};
use crate::data::game_kee_guide::{self, GuideServer};
use crate::db::dao;
use arona::runtime::config as runtime_config;
use arona::runtime::dispatcher::CommandContext;
use arona::runtime::message::{ForwardMessage, MessageSegment, OutgoingMessage};
use once_cell::sync::OnceCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const USAGE: &str =
    "用法: /攻略 日服活动|国际服活动|国服活动|日程笔记|图片关键词|日服卡池|国服卡池|国际服卡池";
const NOT_FOUND_TIP: &str = "没有对应信息, 请联系作者添加别名或者在配置文件中指定";
const PENDING_TTL_MILLS: i64 = 60_000;

/// 等待用户从相近建议中回复数字选择的临时状态
struct PendingSuggestion {
    suggestions: Vec<ImageResult>,
    expire_at: i64,
}

static PENDING: OnceCell<Mutex<HashMap<String, PendingSuggestion>>> = OnceCell::new();

fn pending() -> &'static Mutex<HashMap<String, PendingSuggestion>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// /攻略 <关键词>
pub async fn trainer(
    context: Arc<CommandContext>,
    arguments: Vec<String>,
) -> Option<OutgoingMessage> {
    let keyword = arguments.join(" ").trim().to_string();
    if keyword.is_empty() {
        return Some(OutgoingMessage::text(USAGE));
    }
    if let Some(server) = parse_server_pool_arg(&keyword) {
        return Some(gacha_pool_forward(server).await);
    }
    match keyword.as_str() {
        "日服活动" => return Some(fetch_activity_guide(GuideServer::JP).await),
        "国际服活动" => return Some(fetch_activity_guide(GuideServer::GLOBAL).await),
        "国服活动" => return Some(fetch_activity_guide(GuideServer::CN).await),
        "日程笔记" => return Some(fetch_schedule_note().await),
        _ => {}
    }
    if let Some(over) = find_override(&keyword) {
        let override_type = over.override_type.to_uppercase();
        match override_type.as_str() {
            "IMAGE" => {
                let path = arona_backend::local_image_file(&over.value);
                if !path.exists() {
                    arona::runtime::log::warning(format!(
                        "处理攻略指令别名: {keyword} 时失败,没有找到对应的文件: {}",
                        over.value
                    ));
                    return None;
                }
                return Some(OutgoingMessage::image_file(path.display().to_string()));
            }
            "CODE" => {
                return Some(OutgoingMessage {
                    segments: arona::onebot::protocol::decode_cq_message(&over.value),
                    revoke_after_millis: None,
                });
            }
            _ => return handle_image_keyword(&context, &over.value).await,
        }
    }
    handle_image_keyword(&context, &keyword).await
}

/// 处理非命令消息：用户回复数字选择相近建议（对应原版 resolveNumericReply）
pub async fn resolve_numeric_reply(context: Arc<CommandContext>) -> Option<OutgoingMessage> {
    let key = selection_key(&context);
    let suggestions = {
        let mut map = pending().lock().unwrap();
        match map.get(&key) {
            Some(item) if item.expire_at >= now_millis() => item.suggestions.clone(),
            Some(_) => {
                map.remove(&key);
                return None;
            }
            None => return None,
        }
    };
    let select = context.text.trim().parse::<usize>().ok()?;
    if select < 1 || select > suggestions.len() {
        return None;
    }
    pending().lock().unwrap().remove(&key);
    let target = suggestions[select - 1].clone();
    match load_image_or_update(&target.name).await.file {
        Some(file) => Some(send_images(&[file], &target.name)),
        None => Some(OutgoingMessage::text(format!(
            "没有获取到「{}」的图片, 请重新查询",
            target.name
        ))),
    }
}

/// 私聊按 QQ 号、群聊按 群号+QQ 号 区分, 避免不同会话互相串台
fn selection_key(context: &CommandContext) -> String {
    match context.group_id {
        Some(group_id) => format!("g{group_id}:u{}", context.user_id),
        None => format!("p{}", context.user_id),
    }
}

async fn handle_image_keyword(context: &CommandContext, keyword: &str) -> Option<OutgoingMessage> {
    let trainer = trainer_config();
    let result = load_image_or_update(keyword).await;
    if let Some(file) = result.file {
        return Some(send_images(&[file], keyword));
    }
    if !trainer.tip_when_null {
        return None;
    }
    if result.list.is_empty() {
        return Some(OutgoingMessage::text(NOT_FOUND_TIP));
    }
    Some(register_pending(context, keyword, result.list, &trainer))
}

/// 登记待选建议并返回提示消息（对应原版 registerPending）
fn register_pending(
    context: &CommandContext,
    keyword: &str,
    suggestions: Vec<ImageResult>,
    trainer: &TrainerConfig,
) -> OutgoingMessage {
    let ttl = if trainer.tip_response_wait_time > 0 {
        trainer.tip_response_wait_time * 1000
    } else {
        PENDING_TTL_MILLS
    };
    pending().lock().unwrap().insert(
        selection_key(context),
        PendingSuggestion {
            suggestions: suggestions.clone(),
            expire_at: now_millis() + ttl,
        },
    );
    let list_text = suggestions
        .iter()
        .enumerate()
        .map(|(index, item)| format!("{}. {}", index + 1, item.name))
        .collect::<Vec<String>>()
        .join("\n");
    let message = OutgoingMessage::text(format!(
        "没有找到与「{keyword}」完全匹配的图片, 最接近的有:\n{list_text}\n回复对应数字查看图片"
    ));
    if trainer.tip_revoke_time > 0 {
        let seconds = trainer.tip_revoke_time.min(350) as u64;
        message.with_revoke(seconds * 1000)
    } else {
        message
    }
}

/// arona 云端图片库检索 + 本地缓存更新（对应原版 GeneralUtils.loadImageOrUpdate）
async fn load_image_or_update(name: &str) -> ImageRequestResult {
    let local_db = dao::find_image_by_name(name);
    let response = match arona_backend::request_image(name).await {
        Ok(response) => response,
        Err(err) => {
            arona::runtime::log::warning(format!("请求云端图片失败: {err}"));
            if let Some(row) = &local_db {
                let file = arona_backend::local_image_file(&row.path);
                if file.exists() {
                    return ImageRequestResult {
                        list: Vec::new(),
                        file: Some(file),
                    };
                }
            }
            return ImageRequestResult::default();
        }
    };
    let list = response.data;
    let Some(first) = list.first().cloned() else {
        return ImageRequestResult::default();
    };
    // 模糊查询结果：交给上级列建议
    if first.r#type == FUZZY_IMAGE_RESULT {
        return ImageRequestResult { list, file: None };
    }
    let local_file = arona_backend::local_image_file(&first.path);
    match &local_db {
        None => match arona_backend::download_image_file(&first.path, &local_file).await {
            Ok(()) => {
                dao::insert_image(name, &first.path, &first.hash, first.r#type);
                ImageRequestResult {
                    list: Vec::new(),
                    file: Some(local_file),
                }
            }
            Err(err) => {
                arona::runtime::log::warning(format!(
                    "在下载图片{}时失败,请查看控制台报错信息: {err}",
                    first.name
                ));
                ImageRequestResult::default()
            }
        },
        Some(row) => {
            if row.hash != first.hash || !local_file.exists() {
                if row.path != first.path {
                    let old_file = arona_backend::local_image_file(&row.path);
                    let _ = std::fs::remove_file(old_file);
                }
                match arona_backend::download_image_file(&first.path, &local_file).await {
                    Ok(()) => {
                        dao::update_image(row.id, &first.path, &first.hash, first.r#type);
                        ImageRequestResult {
                            list: Vec::new(),
                            file: Some(local_file),
                        }
                    }
                    Err(err) => {
                        arona::runtime::log::warning(format!(
                            "在下载图片{}时失败,请查看控制台报错信息: {err}",
                            first.name
                        ));
                        ImageRequestResult::default()
                    }
                }
            } else {
                ImageRequestResult {
                    list: Vec::new(),
                    file: Some(local_file),
                }
            }
        }
    }
}

/// 活动攻略：逐张发送（对应原版 sendImages）
async fn fetch_activity_guide(server: GuideServer) -> OutgoingMessage {
    let keyword = match server {
        GuideServer::JP => "日服活动",
        GuideServer::GLOBAL => "国际服活动",
        GuideServer::CN => "国服活动",
    };
    match game_kee_guide::get_activity_guide(server).await {
        Ok(files) => send_images(&files, keyword),
        Err(err) => {
            arona::runtime::log::warning(format!("获取{keyword}攻略失败: {err}"));
            OutgoingMessage::text(format!("获取{keyword}攻略失败，请稍后重试"))
        }
    }
}

/// 日程笔记：合并转发（对应原版 forwardImages）
async fn fetch_schedule_note() -> OutgoingMessage {
    match game_kee_guide::get_schedule_note_images().await {
        Ok(files) => forward_images(&files, "日程笔记", runtime_config::bot_id()),
        Err(err) => {
            arona::runtime::log::warning(format!("获取日程笔记攻略失败: {err}"));
            OutgoingMessage::text("获取日程笔记攻略失败，请稍后重试")
        }
    }
}

/// 三服当期卡池：合并转发，每个角色一个节点（卡池图 + 角色名/所属/时间）
async fn gacha_pool_forward(server: GachaServer) -> OutgoingMessage {
    let characters = match game_kee::fetch_pool(server).await {
        Ok(characters) => characters,
        Err(err) => {
            return OutgoingMessage::text(format!(
                "获取{}当期卡池失败: {err}",
                server.display_name()
            ));
        }
    };
    if characters.is_empty() {
        return OutgoingMessage::text(format!("当前没有{}的当期卡池", server.display_name()));
    }
    let image_dir = arona::runtime::paths::images_root()
        .join("gacha-pool")
        .join(server.display_name());
    // 并行解析每个角色的卡池图：命中缓存直接用，未命中才下载
    let image_dir_ref = &image_dir;
    let images: Vec<Option<PathBuf>> =
        futures_util::future::join_all(characters.iter().map(|character| async move {
            match game_kee::find_cached_image(character.id, image_dir_ref) {
                Some(path) => Some(path),
                None => game_kee::download_character_image(character, image_dir_ref).await,
            }
        }))
        .await;
    let uin = runtime_config::bot_id();
    let mut nodes: Vec<ForwardMessage> = Vec::new();
    for (character, image_file) in characters.iter().zip(images) {
        let mut content: Vec<MessageSegment> = Vec::new();
        if let Some(path) = image_file {
            content.push(image_segment(&path));
        }
        content.push(MessageSegment::Text(pool_text(character)));
        nodes.push(ForwardMessage {
            name: "Arona".to_string(),
            uin,
            content,
        });
    }
    OutgoingMessage::forward(format!("{}当期卡池", server.display_name()), nodes)
}

fn pool_text(character: &GachaCharacter) -> String {
    format!(
        "角色名：{}\n所属：{}\n开始时间：{}\n结束时间：{}",
        character.name,
        character.name_alias,
        format_pool_time(character.start_at),
        format_pool_time(character.end_at)
    )
}

/// 卡池时间戳(秒)换算为北京时间, 0/负数视为未知
fn format_pool_time(epoch_seconds: i64) -> String {
    if epoch_seconds <= 0 {
        return "未知".to_string();
    }
    match chrono::DateTime::from_timestamp(epoch_seconds, 0) {
        Some(time) => time
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8"))
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        None => "未知".to_string(),
    }
}

/// 解析 /攻略 日服卡池 这类参数
fn parse_server_pool_arg(keyword: &str) -> Option<GachaServer> {
    match keyword {
        "日服卡池" => Some(GachaServer::JP),
        "国服卡池" => Some(GachaServer::CN),
        "国际服卡池" => Some(GachaServer::GLOBAL),
        _ => None,
    }
}

fn image_segment(path: &Path) -> MessageSegment {
    MessageSegment::Image {
        url: None,
        file: Some(path.display().to_string()),
        data: None,
    }
}

fn send_images(files: &[PathBuf], keyword: &str) -> OutgoingMessage {
    if files.is_empty() {
        return OutgoingMessage::text(format!("没有获取到「{keyword}」的图片"));
    }
    let mut message = OutgoingMessage::text(format!("「{keyword}」共 {} 张图片", files.len()));
    for file in files {
        message = message + OutgoingMessage::image_file(file.display().to_string());
    }
    message
}

/// 合并转发：首节点为标题文本，后续每张图片一个节点
fn forward_images(files: &[PathBuf], title: &str, uin: i64) -> OutgoingMessage {
    if files.is_empty() {
        return OutgoingMessage::text(format!("没有获取到「{title}」的图片"));
    }
    let mut nodes = vec![ForwardMessage {
        name: "Arona".to_string(),
        uin,
        content: vec![MessageSegment::Text(title.to_string())],
    }];
    for file in files {
        nodes.push(ForwardMessage {
            name: "Arona".to_string(),
            uin,
            content: vec![image_segment(file)],
        });
    }
    OutgoingMessage::forward(title, nodes)
}

fn trainer_config() -> TrainerConfig {
    crate::config::trainer()
}

fn trainer_file_path() -> PathBuf {
    arona::runtime::paths::standalone_root().join("trainer_config.yml")
}

/// 读取独立的 trainer_config.yml（对应原版 TrainerCommand 的别名配置文件）
fn load_trainer_file_overrides() -> Vec<TrainerOverride> {
    let path = trainer_file_path();
    if !path.exists() {
        let _ = std::fs::write(&path, "override: []\n");
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    if text.trim().is_empty() {
        return Vec::new();
    }
    match serde_yaml::from_str::<TrainerFileConfig>(&text) {
        Ok(config) => config.overrides,
        Err(err) => {
            arona::runtime::log::warning(format!("序列化别名配置时失败: {err}"));
            Vec::new()
        }
    }
}

/// 按原版顺序合并别名配置：文件配置优先，arona.yml 中同名的忽略
fn find_override(keyword: &str) -> Option<TrainerOverride> {
    let mut list = load_trainer_file_overrides();
    for item in trainer_config().r#override.iter() {
        if !list.iter().any(|existing| existing.name == item.name) {
            list.push(item.clone());
        }
    }
    list.into_iter()
        .filter(|item| item.name.contains(keyword))
        .find(|item| item.name.split(',').any(|part| part.trim() == keyword))
}
