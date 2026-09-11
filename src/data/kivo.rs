//! kivo.wiki 学生数据源（对应原版 gacha/KivoStudentSource）
//! 按服务器 × 星级(1/2/3) 拉取全量已实装学生，带内存 TTL 缓存。

use crate::entity::ServerLocale;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;

const BASE_URL: &str = "https://api.kivo.wiki/api/v1/data/students/";
const PAGE_SIZE: i64 = 1000;
const CACHE_TTL_MS: i64 = 30 * 60 * 1000;

/// kivo 学生条目：name 即完整名(given_name_cn（skin_cn）)
#[derive(Clone, Debug)]
pub struct KivoStudent {
    pub id: i64,
    pub name: String,
    pub desc_name: String,
    pub star: i32,
    pub avatar: String,
}

struct CacheEntry {
    students: BTreeMap<i32, Vec<KivoStudent>>,
    expire_at: i64,
}

static CACHE: once_cell::sync::OnceCell<Mutex<BTreeMap<ServerLocale, CacheEntry>>> =
    once_cell::sync::OnceCell::new();

fn cache() -> &'static Mutex<BTreeMap<ServerLocale, CacheEntry>> {
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn is_install_key(server: ServerLocale) -> &'static str {
    match server {
        ServerLocale::JP => "is_install",
        ServerLocale::CN => "is_install_cn",
        ServerLocale::GLOBAL => "is_install_global",
    }
}

/// 获取某服务器按星级分类的学生列表（带内存缓存）
pub async fn fetch_students(
    server: ServerLocale,
) -> Result<BTreeMap<i32, Vec<KivoStudent>>, String> {
    {
        let guard = cache().lock().unwrap();
        if let Some(entry) = guard.get(&server) {
            if entry.expire_at > now_ms() {
                return Ok(entry.students.clone());
            }
        }
    }
    let result = fetch_uncached(server).await?;
    {
        let mut guard = cache().lock().unwrap();
        guard.insert(
            server,
            CacheEntry {
                students: result.clone(),
                expire_at: now_ms() + CACHE_TTL_MS,
            },
        );
    }
    Ok(result)
}

async fn fetch_uncached(server: ServerLocale) -> Result<BTreeMap<i32, Vec<KivoStudent>>, String> {
    let install = is_install_key(server);
    let mut map = BTreeMap::new();
    for rarity in 1..=3 {
        let students = fetch_rarity(install, rarity).await?;
        map.insert(rarity, students);
    }
    Ok(map)
}

async fn fetch_rarity(is_install: &str, rarity: i32) -> Result<Vec<KivoStudent>, String> {
    let url = format!("{BASE_URL}?page=1&page_size={PAGE_SIZE}&{is_install}=true&rarity={rarity}");
    let json = super::http::get(&url, "https://ba.kivo.wiki/", &[]).await?;
    let (students, max_page) = parse_page(&json, rarity)?;
    let mut result = students;
    let mut page = 1;
    while page < max_page {
        page += 1;
        let url = format!(
            "{BASE_URL}?page={page}&page_size={PAGE_SIZE}&{is_install}=true&rarity={rarity}"
        );
        let json = super::http::get(&url, "https://ba.kivo.wiki/", &[]).await?;
        let (more, _) = parse_page(&json, rarity)?;
        result.extend(more);
    }
    Ok(result)
}

/// 解析单页响应，返回 (学生列表, max_page)
pub fn parse_page(json: &str, rarity: i32) -> Result<(Vec<KivoStudent>, i64), String> {
    let root: Value =
        serde_json::from_str(json).map_err(|err| format!("kivo JSON 解析失败: {err}"))?;
    let code = root.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 2000 {
        let msg = root.get("message").and_then(|v| v.as_str()).unwrap_or("");
        return Err(format!("kivo 接口异常: code={code} msg={msg}"));
    }
    let data = root
        .get("data")
        .ok_or_else(|| "kivo 接口缺少 data".to_string())?;
    let students = data
        .get("students")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let max_page = data.get("max_page").and_then(|v| v.as_i64()).unwrap_or(1);
    let mut out = Vec::new();
    for item in students {
        if let Some(student) = to_kivo_student(&item, rarity) {
            out.push(student);
        }
    }
    Ok((out, max_page))
}

fn to_kivo_student(value: &Value, rarity: i32) -> Option<KivoStudent> {
    let id = value.get("id")?.as_i64()?;
    let given_cn = value
        .get("given_name_cn")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let given = value
        .get("given_name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let base = given_cn
        .clone()
        .or_else(|| given.clone())
        .unwrap_or_default();
    let skin_cn = value
        .get("skin_cn")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let name = if !base.is_empty() {
        match skin_cn {
            Some(skin) => format!("{base}（{skin}）"),
            None => base,
        }
    } else {
        let family = value
            .get("family_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let given = value
            .get("given_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        format!("{family}{given}").trim().to_string()
    };
    if name.is_empty() {
        return None;
    }
    let desc_name = value
        .get("given_name_jp")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| given.unwrap_or_default());
    Some(KivoStudent {
        id,
        name,
        desc_name,
        star: rarity,
        avatar: value
            .get("avatar")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 启动后台刷新：三服各拉一次并预热内存缓存，输出数量变化日志
pub async fn init() {
    for server in ServerLocale::ALL {
        let server_name = server.server_name();
        match fetch_uncached(server).await {
            Ok(students) => {
                let count: usize = students.values().map(|v| v.len()).sum();
                {
                    let mut guard = cache().lock().unwrap();
                    guard.insert(
                        server,
                        CacheEntry {
                            students,
                            expire_at: now_ms() + CACHE_TTL_MS,
                        },
                    );
                }
                crate::runtime::log::info(format!(
                    "kivo {server_name} 学生数据已预热: 共 {count} 名学生"
                ));
            }
            Err(err) => {
                crate::runtime::log::warning(format!("kivo {server_name} 学生数据刷新失败: {err}"))
            }
        }
    }
}
