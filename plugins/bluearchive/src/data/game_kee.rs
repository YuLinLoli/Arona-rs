//! GameKee 当期卡池数据源（对应原版 gacha/GameKeeGachaPoolSource）
//!
//! 请求 https://www.gamekee.com/v1/wiki/indexV2，取 data 里 module.name == "卡池" 的 list；
//! list 以 server_id 字符串为 key："15"=日服、"16"=国服、"17"=国际服。

use serde_json::Value;

pub const INDEX_V2_URL: &str = "https://www.gamekee.com/v1/wiki/indexV2";
pub const POOL_MODULE_NAME: &str = "卡池";
pub const REFERER: &str = "https://www.gamekee.com/ba/";

/// 服务器（对应原版 GachaServer）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GachaServer {
    JP,
    CN,
    GLOBAL,
}

impl GachaServer {
    pub fn server_id(self) -> i64 {
        match self {
            GachaServer::JP => 15,
            GachaServer::CN => 16,
            GachaServer::GLOBAL => 17,
        }
    }

    pub fn from_server_id(server_id: i64) -> Option<GachaServer> {
        match server_id {
            15 => Some(GachaServer::JP),
            16 => Some(GachaServer::CN),
            17 => Some(GachaServer::GLOBAL),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            GachaServer::JP => "日服",
            GachaServer::CN => "国服",
            GachaServer::GLOBAL => "国际服",
        }
    }
}

/// 当期卡池角色
#[derive(Clone, Debug)]
pub struct GachaCharacter {
    pub id: i64,
    pub name: String,
    pub name_alias: String,
    pub server_id: i64,
    pub star: i32,
    pub start_at: i64,
    pub end_at: i64,
    pub sort: i64,
    pub icon: String,
    pub image_list: String,
    pub link_url: String,
    pub tag_id: String,
}

/// 角色 + 所属服务器
#[derive(Clone, Debug)]
pub struct PoolEntry {
    pub server: GachaServer,
    pub character: GachaCharacter,
}

/// 拿取三个服务器当期卡池并合并为一个 List
pub async fn fetch_current_pools() -> Result<Vec<PoolEntry>, String> {
    let json = super::http::get_with(INDEX_V2_URL, &super::http::game_kee_headers(REFERER)).await?;
    parse_pools(&json)
}

/// 按角色 id 查找图片缓存（对应原版 GameKeeGachaPoolSource.findCachedImage）
pub fn find_cached_image(
    character_id: i64,
    directory: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let id = character_id.to_string();
    let entries = std::fs::read_dir(directory).ok()?;
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with(&format!("{id}.")))
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| {
            files
                .iter()
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name.starts_with(&format!("{id}-")))
                        .unwrap_or(false)
                })
                .cloned()
        })
}

/// 下载角色卡池图（image_list 字段）到缓存目录，保存为 <角色id>.<后缀>；
/// 未配置图片或下载失败返回 None（对应原版 downloadCharacterImage + downloadToCache）。
pub async fn download_character_image(
    character: &GachaCharacter,
    directory: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if let Some(cached) = find_cached_image(character.id, directory) {
        return Some(cached);
    }
    let url = normalize_image_url(&character.image_list);
    if url.is_empty() {
        return None;
    }
    let _ = std::fs::create_dir_all(directory);
    let suffix = image_suffix(&url);
    let file = directory.join(format!("{}{}", character.id, suffix));
    if file.is_file() && file.metadata().map(|m| m.len()).unwrap_or(0) > 0 {
        return Some(file);
    }
    let headers = super::http::game_kee_image_headers(REFERER);
    let bytes = super::http::get_bytes_with(&url, &headers).await.ok()?;
    if bytes.is_empty() {
        return None;
    }
    match std::fs::write(&file, &bytes) {
        Ok(()) => Some(file),
        Err(err) => {
            arona::runtime::log::warning(format!("下载卡池图失败 {}: {err}", character.name));
            None
        }
    }
}

/// image_list 可能是 // 开头的协议相对地址（对应原版 normalizeUrl）
pub fn normalize_image_url(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        String::new()
    } else if let Some(rest) = url.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

/// 从图片 URL 提取后缀（对应原版 downloadImages 中的后缀推断）
pub fn image_suffix(url: &str) -> String {
    let without_query = url.split('?').next().unwrap_or(url);
    let ext = without_query.rsplit('.').next().unwrap_or("");
    let candidate = ext.to_lowercase();
    if (2..=5).contains(&candidate.len()) && candidate.chars().all(|c| c.is_ascii_alphanumeric()) {
        format!(".{candidate}")
    } else {
        ".png".to_string()
    }
}

/// 只拿某个服务器的当期卡池
pub async fn fetch_pool(server: GachaServer) -> Result<Vec<GachaCharacter>, String> {
    let pools = fetch_current_pools().await?;
    Ok(pools
        .into_iter()
        .filter(|entry| entry.server == server)
        .map(|entry| entry.character)
        .collect())
}

/// 解析 indexV2 响应，只保留三个已知服务器的角色
pub fn parse_pools(json: &str) -> Result<Vec<PoolEntry>, String> {
    let root: Value = serde_json::from_str(json)
        .map_err(|err| format!("GameKee indexV2 JSON 解析失败: {err}"))?;
    let code = root.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        let msg = root.get("msg").and_then(|v| v.as_str()).unwrap_or("");
        return Err(format!("GameKee indexV2 返回错误: code={code} msg={msg}"));
    }
    let data = root
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "GameKee indexV2 缺少 data".to_string())?;
    let pool_module = data
        .iter()
        .find(|module| {
            module
                .get("module")
                .and_then(|m| m.get("name"))
                .and_then(|v| v.as_str())
                == Some(POOL_MODULE_NAME)
        })
        .ok_or_else(|| "GameKee indexV2 未找到卡池模块".to_string())?;
    let pool_list = pool_module
        .get("list")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "GameKee 卡池模块 list 为空".to_string())?;

    let mut result: Vec<PoolEntry> = Vec::new();
    for (server_id_key, characters) in pool_list {
        let Ok(server_id) = server_id_key.parse::<i64>() else {
            continue;
        };
        let Some(server) = GachaServer::from_server_id(server_id) else {
            continue;
        };
        let Some(characters) = characters.as_array() else {
            continue;
        };
        for element in characters {
            if !element.is_object() {
                continue;
            }
            let Some(character) = to_gacha_character(element) else {
                continue;
            };
            result.push(PoolEntry { server, character });
        }
    }
    Ok(result)
}

/// 从单个角色 JSON 提取字段，id/name 缺失视为无效数据
fn to_gacha_character(value: &Value) -> Option<GachaCharacter> {
    let id = value.get("id")?.as_i64()?;
    let name = value.get("name")?.as_str()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    // star 可能为字符串
    let star = value
        .get("star")
        .and_then(|v| v.as_str())
        .and_then(|s| s.trim().parse::<i32>().ok())
        .or_else(|| value.get("star").and_then(|v| v.as_i64()).map(|v| v as i32))
        .unwrap_or(0);
    let time = |key: &str| -> i64 {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| s.trim().parse::<i64>().ok())
            .or_else(|| value.get(key).and_then(|v| v.as_i64()))
            .unwrap_or(0)
    };
    Some(GachaCharacter {
        id,
        name,
        name_alias: value
            .get("name_alias")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        server_id: value.get("server_id").and_then(|v| v.as_i64()).unwrap_or(0),
        star,
        start_at: time("start_at"),
        end_at: time("end_at"),
        sort: value.get("sort").and_then(|v| v.as_i64()).unwrap_or(0),
        icon: value
            .get("icon")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        image_list: value
            .get("image_list")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        link_url: value
            .get("link_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        tag_id: value
            .get("tag_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}
