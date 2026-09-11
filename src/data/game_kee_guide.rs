//! GameKee 活动攻略 / 日程笔记图片抓取（对应原版 util/GameKeeUtil）
//!
//! - 目录树 `entry/treesByPidV1?pid=137392`：定位活动攻略/日程笔记的 content_id
//! - 内容详情 `content/detail/<id>`：活动攻略取 `data.thumb_list`
//! - 日程笔记正文 `api-cdn.gamekee.com/.../<id>.json?v=<version>`：解析 content 数组里的图片节点

use crate::data::game_kee::normalize_image_url;
use serde_json::Value;
use std::path::{Path, PathBuf};

const ENTRY_TREE_URL: &str = "https://www.gamekee.com/v1/entry/treesByPidV1?pid=137392";
const CONTENT_DETAIL_URL: &str = "https://www.gamekee.com/v1/content/detail/";
const CONTENT_CDN_URL: &str = "https://api-cdn.gamekee.com/wiki2.0/pro/829/content";
const ENTRY_REFERER: &str = "https://www.gamekee.com/ba/second/137392";
/// 日程笔记正文里标识"从这里开始才是图片"的文本节点
const SCHEDULE_MARKER: &str = "日程笔记再见";

/// 活动攻略服务器（对应原版 getJpActivityGuide / getGlobalActivityGuide / getCnActivityGuide）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuideServer {
    JP,
    GLOBAL,
    CN,
}

impl GuideServer {
    /// 目录树中的条目名
    pub fn guide_name(self) -> &'static str {
        match self {
            GuideServer::JP => "日服活动攻略",
            GuideServer::GLOBAL => "国际服活动攻略",
            GuideServer::CN => "国服活动攻略",
        }
    }

    /// 缓存目录名
    pub fn dir_name(self) -> &'static str {
        match self {
            GuideServer::JP => "jp",
            GuideServer::GLOBAL => "global",
            GuideServer::CN => "cn",
        }
    }
}

/// 拉取 GameKee 目录树（对应原版 getEntryTree）
pub async fn entry_tree() -> Result<Value, String> {
    let text = super::http::get_with(
        ENTRY_TREE_URL,
        &super::http::game_kee_headers(ENTRY_REFERER),
    )
    .await?;
    serde_json::from_str(&text).map_err(|err| format!("GameKee 目录树解析失败: {err}"))
}

/// 在目录树节点数组里按名字找子节点
fn find_child_by_name<'a>(node: &'a Value, predicate: impl Fn(&str) -> bool) -> Option<&'a Value> {
    node.get("child")?
        .as_array()?
        .iter()
        .find(|child| {
            child
                .get("name")
                .and_then(|v| v.as_str())
                .map(&predicate)
                .unwrap_or(false)
        })
}

fn content_id_of(node: &Value) -> Option<i64> {
    node.get("content_id")
        .and_then(|v| v.as_i64())
        .filter(|id| *id > 0)
        .or_else(|| {
            node.get("content_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.trim().parse::<i64>().ok())
                .filter(|id| *id > 0)
        })
}

/// 活动攻略/日程笔记缓存根目录（对应原版 cacheDirectory）
fn cache_directory(relative: &str) -> PathBuf {
    crate::runtime::paths::images_root()
        .join("gamekee")
        .join(relative)
}

/// 命中缓存则返回按序号排序的图片；contentId 不匹配时清空目录并返回 None
fn get_cached_images(directory: &Path, content_id: i64) -> Option<Vec<PathBuf>> {
    let entries = std::fs::read_dir(directory).ok()?;
    let prefix = format!("{content_id}-");
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect();
    if files.is_empty() {
        if let Ok(entries) = std::fs::read_dir(directory) {
            for entry in entries.filter_map(|entry| entry.ok()) {
                if entry.path().is_file() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        return None;
    }
    files.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| {
                name.split_once('-')
                    .map(|(_, rest)| rest)
                    .unwrap_or(name)
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .parse::<i64>()
                    .unwrap_or(i64::MAX)
            })
            .unwrap_or(i64::MAX)
    });
    Some(files)
}

/// 下载一组图片到目录，文件名 `<contentId>-<序号>.<后缀>`（对应原版 downloadImages）
async fn download_images(
    image_urls: &[String],
    content_id: i64,
    referer: &str,
    directory: &Path,
) -> Result<Vec<PathBuf>, String> {
    let _ = std::fs::create_dir_all(directory);
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.filter_map(|entry| entry.ok()) {
            if entry.path().is_file() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let headers = super::http::game_kee_image_headers(referer);
    let mut files: Vec<PathBuf> = Vec::new();
    for (index, image_url) in image_urls.iter().enumerate() {
        let suffix = crate::data::game_kee::image_suffix(image_url);
        let image_file = directory.join(format!("{}-{}{}", content_id, index + 1, suffix));
        match super::http::get_bytes_with(image_url, &headers).await {
            Ok(bytes) if !bytes.is_empty() => {
                if let Err(err) = std::fs::write(&image_file, bytes) {
                    for file in &files {
                        let _ = std::fs::remove_file(file);
                    }
                    return Err(format!("写入攻略图片失败: {err}"));
                }
                files.push(image_file);
            }
            Ok(_) => {
                for file in &files {
                    let _ = std::fs::remove_file(file);
                }
                return Err(format!("下载攻略图片为空: {image_url}"));
            }
            Err(err) => {
                for file in &files {
                    let _ = std::fs::remove_file(file);
                }
                return Err(err);
            }
        }
    }
    Ok(files)
}

/// 活动攻略图片（对应原版 getActivityGuide）
pub async fn get_activity_guide(server: GuideServer) -> Result<Vec<PathBuf>, String> {
    let root = entry_tree().await?;
    let data = root
        .get("data")
        .ok_or_else(|| "GameKee 目录树缺少 data".to_string())?;
    let content_id = find_child_by_name(data, |name| name == "当期活动 | 当期卡池")
        .and_then(|node| find_child_by_name(node, |name| name == server.guide_name()))
        .and_then(content_id_of)
        .ok_or_else(|| format!("GameKee 未找到活动攻略条目: {}", server.guide_name()))?;

    let directory = cache_directory("activity-guides").join(server.dir_name());
    if let Some(cached) = get_cached_images(&directory, content_id) {
        crate::runtime::log::info_green(format!(
            "[GameKee] {}：命中缓存，contentId={content_id}，数量={}",
            server.guide_name(),
            cached.len()
        ));
        return Ok(cached);
    }

    crate::runtime::log::info_green(format!(
        "[GameKee] {}：无缓存，去 GameKee 提取图片，contentId={content_id}",
        server.guide_name()
    ));
    let referer = format!("https://www.gamekee.com/ba/{content_id}.html");
    let detail_text = super::http::get_with(
        &format!("{CONTENT_DETAIL_URL}{content_id}"),
        &super::http::game_kee_headers(&referer),
    )
    .await?;
    let detail: Value = serde_json::from_str(&detail_text)
        .map_err(|err| format!("GameKee 内容详情解析失败: {err}"))?;
    let image_urls: Vec<String> = detail
        .get("data")
        .and_then(|data| data.get("thumb_list"))
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|item| item.as_str())
                .map(normalize_image_url)
                .filter(|url| url.contains("/pro/"))
                .collect()
        })
        .unwrap_or_default();
    if image_urls.is_empty() {
        return Err(format!(
            "GameKee 未找到活动攻略图片: {}",
            server.guide_name()
        ));
    }
    download_images(&image_urls, content_id, &referer, &directory).await
}

/// 日程笔记图片（对应原版 getScheduleNoteImages）
pub async fn get_schedule_note_images() -> Result<Vec<PathBuf>, String> {
    let root = entry_tree().await?;
    let data = root
        .get("data")
        .ok_or_else(|| "GameKee 目录树缺少 data".to_string())?;
    let content_id = find_child_by_name(data, |name| name.contains("玩法"))
        .and_then(|node| find_child_by_name(node, |name| name.contains("日程笔记")))
        .and_then(content_id_of)
        .ok_or_else(|| "GameKee 未找到日程笔记条目".to_string())?;

    let directory = cache_directory("schedule-note");
    if let Some(cached) = get_cached_images(&directory, content_id) {
        crate::runtime::log::info_green(format!(
            "[GameKee] 日程笔记：命中缓存，contentId={content_id}，数量={}",
            cached.len()
        ));
        return Ok(cached);
    }

    crate::runtime::log::info_green(format!(
        "[GameKee] 日程笔记：无缓存或 contentId 已变化，正在从 GameKee 提取，contentId={content_id}"
    ));
    let referer = format!("https://www.gamekee.com/ba/{content_id}.html");
    let detail_text = super::http::get_with(
        &format!("{CONTENT_DETAIL_URL}{content_id}"),
        &super::http::game_kee_headers(&referer),
    )
    .await?;
    let detail: Value = serde_json::from_str(&detail_text)
        .map_err(|err| format!("GameKee 内容详情解析失败: {err}"))?;
    let version = detail
        .get("data")
        .and_then(|data| data.get("version"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "GameKee 日程笔记版本号缺失".to_string())?;

    let content_url = format!("{CONTENT_CDN_URL}/{content_id}.json?v={version}");
    let content_text = super::http::get_with(
        &content_url,
        &super::http::game_kee_content_headers("https://www.gamekee.com/"),
    )
    .await?;
    let content_root: Value = serde_json::from_str(&content_text)
        .map_err(|err| format!("GameKee 日程笔记响应解析失败: {err}"))?;
    let content = content_root
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "GameKee 日程笔记 content 字段缺失".to_string())?;
    let nodes: Value = serde_json::from_str(content)
        .map_err(|err| format!("GameKee 日程笔记正文解析失败: {err}"))?;

    let mut image_urls: Vec<String> = Vec::new();
    let mut marker_found = false;
    if let Some(array) = nodes.as_array() {
        for node in array {
            collect_schedule_images(node, &mut marker_found, &mut image_urls);
        }
    }
    if image_urls.is_empty() {
        return Err("GameKee 日程笔记未找到图片".to_string());
    }
    download_images(&image_urls, content_id, &referer, &directory).await
}

/// 递归遍历日程笔记节点：遇到标记文本后开始收集 image 节点的 src
/// （对应原版 getScheduleNoteImages 里的 collect 函数）
fn collect_schedule_images(node: &Value, marker_found: &mut bool, out: &mut Vec<String>) {
    let Some(object) = node.as_object() else {
        return;
    };
    if !*marker_found
        && object
            .get("text")
            .and_then(|v| v.as_str())
            .map(|text| text.contains(SCHEDULE_MARKER))
            .unwrap_or(false)
    {
        *marker_found = true;
    }
    if *marker_found
        && object.get("type").and_then(|v| v.as_str()) == Some("image")
        && let Some(src) = object.get("src").and_then(|v| v.as_str())
    {
        out.push(normalize_image_url(src));
    }
    if let Some(children) = object.get("children").and_then(|v| v.as_array()) {
        for child in children {
            collect_schedule_images(child, marker_found, out);
        }
    }
}