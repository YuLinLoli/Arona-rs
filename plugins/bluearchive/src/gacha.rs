//! 抽卡核心逻辑（对应原版 gacha/GachaV2Service）
//! 基于 kivo.wiki 学生数据构建常驻池、GameKee 当期卡池构建 pickup 池，
//! 按官方概率抽卡(3★=0.7%pickup+2.3%常驻, 2★=18%, 1★=79%, 彩蛋0.05%)，
//! 十连第10抽保底2星；Rust 移植版只输出文本结果。

use crate::data::game_kee::{self, GachaCharacter, GachaServer, PoolEntry};
use crate::db::dao;
use crate::entity::ServerLocale;
use rand::Rng;
use std::collections::BTreeMap;

/// 星星符号, 下标即星级(1~3)
const STAR_STRING: [&str; 4] = ["☆☆☆", "★☆☆", "★★☆", "★★★"];

/// 彩蛋角色(彩奈), 概率 0.05%
const EASTER_EGG_NAME: &str = "彩奈";
const EASTER_EGG_DESC_NAME: &str = "Arona";
const EASTER_EGG_DEV_NAME: &str = "Arona";

/// 200 抽保底: 距上次 pickup 累计满 200 抽时, 当次必出 pickup
const PITY_LIMIT: i64 = 200;

/// 抽卡用学生信息(轻量投影)
#[derive(Clone, Debug)]
pub struct GachaStudent {
    pub id: i64,
    pub name: String,
    pub desc_name: String,
    pub dev_name: String,
    pub star: i32,
    pub avatar: String,
}

/// 单个服务器的抽卡池
#[derive(Clone, Debug)]
pub struct GachaPool {
    pub server: ServerLocale,
    /// 常驻池, key 为星级 1/2/3
    pub common: BTreeMap<i32, Vec<GachaStudent>>,
    /// 当期 pickup 池(3 星, 含限定)
    pub pickup: Vec<GachaStudent>,
    pub pickup_start: i64,
    pub pickup_end: i64,
}

impl GachaPool {
    pub fn is_empty(&self) -> bool {
        self.common.values().all(|list| list.is_empty()) && self.pickup.is_empty()
    }
}

/// 单次抽卡结果
#[derive(Clone, Debug)]
pub struct DrawResult {
    pub star: i32,
    pub name: String,
    pub desc_name: String,
    pub dev_name: String,
    pub id: i64,
    pub avatar: String,
    pub custom: bool,
    pub is_pickup: bool,
}

/// 一次抽卡的完整结果
#[derive(Clone, Debug)]
pub struct DrawReport {
    pub server: ServerLocale,
    pub times: i64,
    pub results: Vec<DrawResult>,
    pub hit_pickup: bool,
    pub star1: i64,
    pub star2: i64,
    pub star3: i64,
    pub points: i64,
    pub pity_count: i64,
}

/// 解析服务器参数: 支持 日服/国服/国际服 及 jp/cn/global 等别名
pub fn resolve_server(raw: Option<&str>) -> Option<ServerLocale> {
    match raw?.trim().to_lowercase().as_str() {
        "日服" | "jp" | "jpn" => Some(ServerLocale::JP),
        "国服" | "cn" => Some(ServerLocale::CN),
        "国际服" | "global" | "gl" | "glb" | "en" => Some(ServerLocale::GLOBAL),
        _ => None,
    }
}

/// 解析抽卡服务器: 命令参数优先, 未提供时用该用户保存的偏好
pub fn resolve_draw_server(user_id: i64, raw: Option<&str>) -> Option<ServerLocale> {
    if raw.is_none() || raw.unwrap_or("").trim().is_empty() {
        return Some(get_user_server(user_id));
    }
    resolve_server(raw)
}

/// 读取用户默认抽卡服务器, 未设置时默认日服
pub fn get_user_server(user_id: i64) -> ServerLocale {
    let stored = dao::get_user_server(user_id);
    resolve_server(stored.as_deref()).unwrap_or(ServerLocale::JP)
}

/// 保存用户默认抽卡服务器
pub fn set_user_server(user_id: i64, server: ServerLocale) {
    dao::set_user_server(user_id, server.server_name());
}

fn to_gacha_server(server: ServerLocale) -> GachaServer {
    match server {
        ServerLocale::JP => GachaServer::JP,
        ServerLocale::CN => GachaServer::CN,
        ServerLocale::GLOBAL => GachaServer::GLOBAL,
    }
}

fn to_pickup_student(character: &GachaCharacter) -> GachaStudent {
    let name_alias = character.name_alias.trim();
    GachaStudent {
        id: character.id,
        name: character.name.clone(),
        desc_name: if name_alias.is_empty() {
            character.name.clone()
        } else {
            name_alias.to_string()
        },
        dev_name: String::new(),
        star: 3,
        avatar: character.icon.clone(),
    }
}

fn to_gacha_student(student: crate::data::kivo::KivoStudent) -> GachaStudent {
    GachaStudent {
        id: student.id,
        name: student.name,
        desc_name: student.desc_name,
        dev_name: String::new(),
        star: student.star,
        avatar: student.avatar,
    }
}

fn to_result(student: &GachaStudent, is_pickup: bool) -> DrawResult {
    DrawResult {
        id: student.id,
        star: student.star,
        name: student.name.clone(),
        desc_name: student.desc_name.clone(),
        dev_name: student.dev_name.clone(),
        avatar: student.avatar.clone(),
        custom: false,
        is_pickup,
    }
}

/// 当期卡池时间窗判断: 仅允许抽取已经开始且未结束的卡池; 时间为 0(未知)时放行
fn in_pickup_window(start_at: i64, end_at: i64) -> bool {
    if start_at <= 0 || end_at <= 0 {
        return true;
    }
    let now = chrono::Utc::now().timestamp();
    start_at <= now && now <= end_at
}

/// 构建指定服务器的抽卡池
pub async fn build_pool(
    server: ServerLocale,
    game_kee_pool: &[PoolEntry],
) -> Result<GachaPool, String> {
    let kivo = crate::data::kivo::fetch_students(server).await?;
    let common = kivo
        .into_iter()
        .map(|(star, students)| (star, students.into_iter().map(to_gacha_student).collect()))
        .collect();
    let target = to_gacha_server(server);
    let current_entries: Vec<&GachaCharacter> = game_kee_pool
        .iter()
        .filter(|entry| {
            entry.server == target
                && in_pickup_window(entry.character.start_at, entry.character.end_at)
        })
        .map(|entry| &entry.character)
        .collect();
    let pickup: Vec<GachaStudent> = current_entries
        .iter()
        .map(|c| to_pickup_student(c))
        .collect();
    let pickup_start = current_entries
        .iter()
        .map(|c| c.start_at)
        .min()
        .unwrap_or(0);
    let pickup_end = current_entries.iter().map(|c| c.end_at).max().unwrap_or(0);
    Ok(GachaPool {
        server,
        common,
        pickup,
        pickup_start,
        pickup_end,
    })
}

/// GameKee 当期卡池兜底池: kivo 不可用时使用, 当期角色全部视为 3 星 pickup
fn build_fallback_pool(
    server: ServerLocale,
    characters: &[GachaCharacter],
) -> Result<GachaPool, String> {
    if characters.is_empty() {
        return Err(format!(
            "{}抽卡池为空: kivo 学生数据与 GameKee 当期卡池均无数据, 请稍后再试",
            server.server_name()
        ));
    }
    let pickup = characters.iter().map(to_pickup_student).collect();
    Ok(GachaPool {
        server,
        common: BTreeMap::new(),
        pickup,
        pickup_start: characters.iter().map(|c| c.start_at).min().unwrap_or(0),
        pickup_end: characters.iter().map(|c| c.end_at).max().unwrap_or(0),
    })
}

/// 单抽/十连的落点分类
#[derive(Clone, Copy)]
enum RollType {
    EasterEgg,
    Pickup3,
    Common3,
    Common2,
    Common1,
}

/// 根据 [0,100) 的随机数决定落点
fn decide_roll(r_num: f64, has_pickup: bool) -> RollType {
    if r_num <= 0.05 {
        RollType::EasterEgg
    } else if r_num <= 0.7 && has_pickup {
        RollType::Pickup3
    } else if r_num <= 3.0 {
        RollType::Common3
    } else if r_num <= 21.0 {
        RollType::Common2
    } else {
        RollType::Common1
    }
}

/// 十连第10抽保底: 限定 r_num 到 [0,21), 保证至少 2 星
fn apply_guarantee(r_num: f64) -> f64 {
    r_num % 21.0
}

fn roll_common(pool: &GachaPool, star: i32) -> Result<DrawResult, String> {
    let target = pool.common.get(&star).cloned().unwrap_or_default();
    let mut rng = rand::thread_rng();
    if !target.is_empty() {
        let index = rng.gen_range(0..target.len());
        return Ok(to_result(&target[index], false));
    }
    let mut all: Vec<&GachaStudent> = Vec::new();
    for star in [3, 2, 1] {
        if let Some(list) = pool.common.get(&star) {
            all.extend(list.iter());
        }
    }
    if !all.is_empty() {
        let index = rng.gen_range(0..all.len());
        return Ok(to_result(all[index], false));
    }
    if !pool.pickup.is_empty() {
        let index = rng.gen_range(0..pool.pickup.len());
        return Ok(to_result(&pool.pickup[index], true));
    }
    Err("常驻池为空".to_string())
}

/// 抽卡: times=1 单抽, times=10 十连(第10抽保底2星)
pub fn draw(
    server: ServerLocale,
    times: i64,
    pool: &GachaPool,
    pity_count: i64,
) -> Result<Vec<DrawResult>, String> {
    if !(1..=10).contains(&times) {
        return Err(format!("times 必须在 1..10 之间: {times}"));
    }
    if pool.is_empty() {
        let common_count: usize = pool.common.values().map(|v| v.len()).sum();
        return Err(format!(
            "{}抽卡池为空(常驻池{}人, pickup{}人), 请确认 kivo 学生数据与 GameKee 当期卡池可用",
            server.server_name(),
            common_count,
            pool.pickup.len()
        ));
    }
    let mut results: Vec<DrawResult> = Vec::new();
    let mut must = true;
    let mut running_pity = pity_count;
    for index in 1..=times {
        let mut rng = rand::thread_rng();
        let mut r_num = rng.gen_range(0.0..100.0);
        if index == times && times == 10 && must {
            r_num = apply_guarantee(r_num);
        }
        let forced_pickup = running_pity + 1 >= PITY_LIMIT && !pool.pickup.is_empty();
        let result = if forced_pickup {
            let mut rng = rand::thread_rng();
            let index = rng.gen_range(0..pool.pickup.len());
            to_result(&pool.pickup[index], true)
        } else {
            match decide_roll(r_num, !pool.pickup.is_empty()) {
                RollType::EasterEgg => DrawResult {
                    star: 3,
                    name: EASTER_EGG_NAME.to_string(),
                    desc_name: EASTER_EGG_DESC_NAME.to_string(),
                    dev_name: EASTER_EGG_DEV_NAME.to_string(),
                    id: 0,
                    avatar: String::new(),
                    custom: true,
                    is_pickup: false,
                },
                RollType::Pickup3 => {
                    let mut rng = rand::thread_rng();
                    let index = rng.gen_range(0..pool.pickup.len());
                    to_result(&pool.pickup[index], true)
                }
                RollType::Common3 => roll_common(pool, 3)?,
                RollType::Common2 => roll_common(pool, 2)?,
                RollType::Common1 => roll_common(pool, 1)?,
            }
        };
        running_pity = if result.is_pickup || running_pity + 1 >= PITY_LIMIT {
            0
        } else {
            running_pity + 1
        };
        if result.star != 1 {
            must = false;
        }
        results.push(result);
    }
    Ok(results)
}

/// 当期 pickup 角色名改用 GameKee 当期卡池名称
pub fn apply_pickup_names(results: &mut Vec<DrawResult>, characters: &[GachaCharacter]) {
    if !results.iter().any(|result| result.is_pickup) {
        return;
    }
    if characters.is_empty() {
        return;
    }
    for result in results.iter_mut() {
        if !result.is_pickup {
            continue;
        }
        let game_kee_name = characters
            .iter()
            .find(|c| c.name == result.name)
            .map(|c| c.name.clone())
            .or_else(|| {
                characters
                    .iter()
                    .find(|c| c.name.contains(&result.name) || result.name.contains(&c.name))
                    .map(|c| c.name.clone())
            });
        if let Some(name) = game_kee_name {
            result.name = name;
        }
    }
}

/// 完整抽卡流程: 校验每日次数 → 建池抽卡 → 写入历史
pub async fn perform_draw(
    user_id: i64,
    group_id: i64,
    times: i64,
    server: ServerLocale,
) -> Result<Option<DrawReport>, String> {
    let game_kee_pool = match game_kee::fetch_current_pools().await {
        Ok(pool) => pool,
        Err(err) => {
            arona::runtime::log::warning(format!("GameKee 当期卡池获取失败: {err}"));
            Vec::new()
        }
    };
    let pool = match build_pool(server, &game_kee_pool).await {
        Ok(pool) if !pool.is_empty() => pool,
        _ => {
            let target = to_gacha_server(server);
            let characters: Vec<GachaCharacter> = game_kee_pool
                .iter()
                .filter(|entry| entry.server == target)
                .map(|entry| entry.character.clone())
                .collect();
            build_fallback_pool(server, &characters)?
        }
    };
    let allowed = dao::check_time(user_id, group_id, times);
    if allowed <= 0 {
        return Ok(None);
    }
    let actual_times = allowed.min(times);
    let pity_before = dao::get_pity(user_id, server.server_name());
    let mut results = draw(server, actual_times, &pool, pity_before)?;
    let characters: Vec<GachaCharacter> = game_kee_pool
        .iter()
        .map(|entry| entry.character.clone())
        .collect();
    apply_pickup_names(&mut results, &characters);
    let star1 = results.iter().filter(|r| r.star == 1).count() as i64;
    let star2 = results.iter().filter(|r| r.star == 2).count() as i64;
    let star3 = results.iter().filter(|r| r.star == 3).count() as i64;
    let hit = results.iter().any(|r| r.is_pickup);
    let pity_after = compute_pity_after(pity_before, &results);
    dao::set_pity(user_id, server.server_name(), pity_after);
    let history = dao::history_get_or_create(user_id, group_id);
    dao::history_add(user_id, group_id, actual_times, star3, hit);
    let points = history.as_ref().map(|h| h.points).unwrap_or(0) + actual_times;
    Ok(Some(DrawReport {
        server,
        times: actual_times,
        results,
        hit_pickup: hit,
        star1,
        star2,
        star3,
        points,
        pity_count: pity_after,
    }))
}

/// 依据抽卡结果推进保底计数
pub fn compute_pity_after(pity_before: i64, results: &[DrawResult]) -> i64 {
    let mut pity = pity_before;
    for result in results {
        if result.is_pickup || pity + 1 >= PITY_LIMIT {
            pity = 0;
        } else {
            pity += 1;
        }
    }
    pity
}

/// 单条结果文本, 形如 (★★★)UP角色
pub fn format_result(result: &DrawResult) -> String {
    format!("({}){}", STAR_STRING[result.star as usize], result.name)
}

/// 多条结果文本
pub fn format_results(results: &[DrawResult]) -> String {
    results
        .iter()
        .map(format_result)
        .collect::<Vec<String>>()
        .join("\n")
}

/// 抽卡结果文案: 单抽输出结果行, 十连输出汇总+逐行结果
pub fn format_report(report: &DrawReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("({}卡池)\n", report.server.server_name()));
    if report.times == 1 {
        out.push_str(&format_result(&report.results[0]));
        out.push('\n');
        out.push_str(&format!("{} points", report.points));
    } else {
        out.push_str("————————十连结果————————\n");
        out.push_str(&format!(
            "3星:{} 2星:{} 1星:{} {} points\n",
            report.star3, report.star2, report.star1, report.points
        ));
        out.push_str(&format_results(&report.results));
    }
    out
}

/// 老师名称：数据库记录优先, 否则用发送者昵称/QQ
pub fn teacher_name(group_id: i64, user_id: i64, sender_name: Option<&str>) -> String {
    dao::query_teacher_name(group_id, user_id).unwrap_or_else(|| {
        sender_name
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| user_id.to_string())
    })
}
