//! 塔罗牌命令（对应原版 StandaloneTarot）
//! image 配置开启时在文本后附带塔罗牌图片：优先本地 image/tarot/{编号}-{up|down}.png，
//! 缺失时从原版 CDN 下载；下载/读取失败自动回退为纯文本。

use crate::data::http;
use crate::db::dao;
use crate::gacha;
use crate::runtime::dispatcher::CommandContext;
use crate::runtime::message::OutgoingMessage;
use crate::runtime::paths;
use crate::runtime::tarot_config;
use crate::util::time;
use std::path::PathBuf;
use std::sync::Arc;

const TAROT_COUNT: i64 = 22;
const CDN_BASE: &str = "https://arona.cdn.diyigemt.com/image";
const TAROT_FOLDER: &str = "tarot";

/// /塔罗牌
pub async fn tarot(context: Arc<CommandContext>) -> Option<OutgoingMessage> {
    let user_id = context.user_id;
    let group0 = context.group_id.unwrap_or(user_id);
    let today = time::today();
    let record = dao::get_tarot_record(user_id, group0);
    if tarot_config::day_one() {
        if let Some(record) = &record {
            if record.day == today {
                if let Some(card) = dao::find_tarot(record.tarot) {
                    return Some(send_message(&context, &card, record.positive).await);
                }
            }
        }
    }
    let tarot_number = crate::util::random_int(TAROT_COUNT);
    let Some(card) = dao::find_tarot(tarot_number) else {
        return Some(OutgoingMessage::text("塔罗牌数据未初始化, 请联系管理员"));
    };
    // 与 Kotlin 一致：随机抽卡前稍作停顿，模拟仪式感
    let millis = crate::util::random_int(10) + 1;
    tokio::time::sleep(std::time::Duration::from_millis(millis as u64)).await;
    let positive = crate::util::random_bool();
    if tarot_config::day_one() {
        dao::set_tarot_record(user_id, group0, today, tarot_number, positive);
    }
    Some(send_message(&context, &card, positive).await)
}

/// 组装塔罗牌消息：纯文本 + 可选图片
async fn send_message(
    context: &CommandContext,
    card: &dao::TarotRow,
    positive: bool,
) -> OutgoingMessage {
    let result = if positive {
        &card.positive
    } else {
        &card.negative
    };
    let result_name = if positive { "正位" } else { "逆位" };
    let group0 = context.group_id.unwrap_or(context.user_id);
    let teacher_name = gacha::teacher_name(group0, context.user_id, context.sender_name.as_deref());
    let text = format!(
        "看看{teacher_name}抽到了什么:\n{}({result_name})\n{result}",
        card.name
    );
    if !tarot_config::image() {
        return OutgoingMessage::text(text);
    }
    match local_tarot_file(card.number, positive) {
        Some(file) if file.is_file() && file.metadata().map(|m| m.len() > 0).unwrap_or(false) => {
            OutgoingMessage::text(text) + OutgoingMessage::image_file(file.to_string_lossy())
        }
        _ => match download_tarot_image(card.number, positive).await {
            Some(file) => {
                OutgoingMessage::text(text) + OutgoingMessage::image_file(file.to_string_lossy())
            }
            None => {
                crate::runtime::log::warning(format!(
                    "塔罗牌图片下载失败, 仅发送文本 (编号 {})",
                    card.number
                ));
                OutgoingMessage::text(text)
            }
        },
    }
}

/// 本地塔罗牌图片路径（不存在则返回 None）
fn local_tarot_file(number: i64, positive: bool) -> Option<PathBuf> {
    let suffix = if positive { "up" } else { "down" };
    Some(
        paths::images_root()
            .join(TAROT_FOLDER)
            .join(format!("{}-{suffix}.png", number + 1)),
    )
}

/// 下载单张塔罗牌图片到本地缓存
async fn download_tarot_image(number: i64, positive: bool) -> Option<PathBuf> {
    let file = local_tarot_file(number, positive)?;
    let suffix = if positive { "up" } else { "down" };
    let path = format!("/tarot/{}-{suffix}.png", number + 1);
    let url = format!("{CDN_BASE}{path}");
    let bytes = http::get_bytes(&url, "https://arona.diyigemt.com/")
        .await
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&file, &bytes).ok()?;
    Some(file)
}

/// 启动时预下载全部 22x2 张塔罗牌图片（与原版 downloadAllImages 一致）
pub async fn download_all_images() {
    let mut downloaded = 0;
    let mut failed = 0;
    for number in 0..TAROT_COUNT {
        for positive in [true, false] {
            let cached = local_tarot_file(number, positive)
                .filter(|f| f.is_file())
                .filter(|f| f.metadata().map(|m| m.len() > 0).unwrap_or(false));
            if cached.is_some() {
                continue;
            }
            match download_tarot_image(number, positive).await {
                Some(_) => downloaded += 1,
                None => failed += 1,
            }
        }
    }
    if downloaded > 0 || failed > 0 {
        crate::runtime::log::info(format!(
            "塔罗牌图片预下载完成: 成功 {downloaded} 张, 失败 {failed} 张"
        ));
    }
}
