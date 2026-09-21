//! 各业务表的访问封装（对应原版各 Table/DAO）
use rusqlite::{Connection, OptionalExtension, params};

use crate::entity::{Activity, ActivityType, ServerLocale};
use crate::runtime::gacha_config;

pub const POOL_DEFAULT: i32 = 1;

// ---------- teacher_name ----------

pub fn query_teacher_name(group: i64, qq: i64) -> Option<String> {
    super::query(|conn| {
        conn.query_row(
            "SELECT name FROM teacher_name WHERE grp = ?1 AND qq = ?2",
            params![group, qq],
            |row| row.get::<_, String>(0),
        )
        .optional()
    })
    .flatten()
}

pub fn set_teacher_name(group: i64, qq: i64, name: &str) {
    let _ = super::query(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO teacher_name (grp, qq, name) VALUES (?1, ?2, ?3)",
            params![group, qq, name],
        )
    });
}

// ---------- game_name ----------

pub fn set_game_name(qq: i64, name: &str) {
    let _ = super::query(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO game_name (qq, name) VALUES (?1, ?2)",
            params![qq, name],
        )
    });
}

pub fn get_game_name(qq: i64) -> Option<String> {
    super::query(|conn| {
        conn.query_row(
            "SELECT name FROM game_name WHERE qq = ?1",
            params![qq],
            |row| row.get::<_, String>(0),
        )
        .optional()
    })
    .flatten()
}

/// 模糊反查游戏名（对应原版 GameNameTable.name like）
pub fn search_game_name(keyword: &str) -> Vec<(String, i64)> {
    let pattern = format!("%{keyword}%");
    super::query(|conn| {
        let mut stmt = conn.prepare("SELECT name, qq FROM game_name WHERE name LIKE ?1")?;
        let rows = stmt.query_map(params![pattern], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(item) = row {
                out.push(item);
            }
        }
        Ok(out)
    })
    .unwrap_or_default()
}

// ---------- gacha_limit / 每日次数 ----------

fn ensure_day_reset(conn: &Connection) -> rusqlite::Result<()> {
    let today = crate::util::time::today();
    let day = gacha_config::get().day;
    if day == today {
        return Ok(());
    }
    gacha_config::set(|c| c.day = today);
    conn.execute("UPDATE gacha_limit SET count = 0", [])?;
    Ok(())
}

/// 对应原版 GachaUtil.checkTime：返回本次实际可抽次数（0 表示次数耗尽）
pub fn check_time(user_id: i64, group: i64, times: i64) -> i64 {
    let limit = gacha_config::limit();
    if limit <= 0 {
        return times;
    }
    super::query(|conn| {
        ensure_day_reset(conn)?;
        let current: i64 = conn
            .query_row(
                "SELECT count FROM gacha_limit WHERE qq = ?1 AND grp = ?2",
                params![user_id, group],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let add = if limit - (current + times) >= 0 {
            times
        } else {
            (limit - current).max(0)
        };
        if add > 0 {
            conn.execute(
                "INSERT INTO gacha_limit (qq, grp, count) VALUES (?1, ?2, ?3)
                 ON CONFLICT(qq, grp) DO UPDATE SET count = count + ?3",
                params![user_id, group, add],
            )?;
        }
        Ok(add)
    })
    .unwrap_or(0)
}

pub fn reset_limit_of_group(group: i64) {
    let _ = super::query(|conn| {
        conn.execute(
            "UPDATE gacha_limit SET count = 0 WHERE grp = ?1",
            params![group],
        )
    });
}

pub fn reset_all_limits() {
    let _ = super::query(|conn| conn.execute("UPDATE gacha_limit SET count = 0", []));
}

// ---------- gacha_history ----------

#[derive(Clone, Debug)]
pub struct HistoryRow {
    pub qq: i64,
    pub group: i64,
    pub pool: i32,
    pub points: i64,
    pub count3: i64,
    pub dog: i64,
}

fn history_pool() -> i32 {
    let pool = gacha_config::active_pool();
    if pool <= 0 { POOL_DEFAULT } else { pool }
}

pub fn history_get_or_create(user_id: i64, group: i64) -> Option<HistoryRow> {
    let pool = history_pool();
    super::query(|conn| {
        let row = conn
            .query_row(
                "SELECT qq, grp, pool, points, count3, dog FROM gacha_history WHERE qq = ?1 AND grp = ?2 AND pool = ?3",
                params![user_id, group, pool],
                |row| {
                    Ok(HistoryRow {
                        qq: row.get(0)?,
                        group: row.get(1)?,
                        pool: row.get(2)?,
                        points: row.get(3)?,
                        count3: row.get(4)?,
                        dog: row.get(5)?,
                    })
                },
            )
            .optional()?;
        let row = match row {
            Some(r) => r,
            None => {
                conn.execute(
                    "INSERT OR IGNORE INTO gacha_history (qq, grp, pool, points, count3, dog) VALUES (?1, ?2, ?3, 0, 0, 0)",
                    params![user_id, group, pool],
                )?;
                HistoryRow {
                    qq: user_id,
                    group,
                    pool,
                    points: 0,
                    count3: 0,
                    dog: 0,
                }
            }
        };
        Ok(row)
    })
}

/// 对应原版 updateHistory：points+=addPoints、count3+=addCount3，狗叫在首次命中 pickup 时记下当前 points
pub fn history_add(user_id: i64, group: i64, add_points: i64, add_count3: i64, dog_hit: bool) {
    let _ = super::query(|conn| {
        conn.execute(
            "UPDATE gacha_history SET points = points + ?1, count3 = count3 + ?2 WHERE qq = ?3 AND grp = ?4 AND pool = ?5",
            params![add_points, add_count3, user_id, group, history_pool()],
        )?;
        if dog_hit {
            conn.execute(
                "UPDATE gacha_history SET dog = points WHERE qq = ?1 AND grp = ?2 AND pool = ?3 AND dog = 0",
                params![user_id, group, history_pool()],
            )?;
        }
        Ok(())
    });
}

fn query_history_all(group: i64, extra_where: &str) -> Vec<HistoryRow> {
    let pool = history_pool();
    let sql = format!(
        "SELECT qq, grp, pool, points, count3, dog FROM gacha_history WHERE grp = ?1 AND pool = ?2 {extra_where}"
    );
    super::query(|conn| {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![group, pool], |row| {
            Ok(HistoryRow {
                qq: row.get(0)?,
                group: row.get(1)?,
                pool: row.get(2)?,
                points: row.get(3)?,
                count3: row.get(4)?,
                dog: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(item) = row {
                out.push(item);
            }
        }
        Ok(out)
    })
    .unwrap_or_default()
}

/// 抽到过 pickup（狗叫记录非 0）的记录，按抽数升序
pub fn dog_calls(group: i64) -> Vec<HistoryRow> {
    let mut rows = query_history_all(group, "AND dog <> 0");
    rows.sort_by_key(|r| r.dog);
    rows
}

/// 历史排行：按“平均多少抽出 3 星”升序（0 个 3 星按 999 处理，与原版一致）
pub fn history_all(group: i64) -> Vec<HistoryRow> {
    let mut rows = query_history_all(group, "");
    rows.sort_by_key(|r| {
        if r.count3 == 0 {
            999
        } else {
            r.points / r.count3
        }
    });
    rows
}

pub fn delete_history_of_group(group: i64, pool: i32) {
    let _ = super::query(|conn| {
        conn.execute(
            "DELETE FROM gacha_history WHERE grp = ?1 AND pool = ?2",
            params![group, pool],
        )
    });
}

// ---------- gacha_user_setting ----------

pub fn get_user_server(qq: i64) -> Option<String> {
    super::query(|conn| {
        conn.query_row(
            "SELECT server FROM gacha_user_setting WHERE qq = ?1",
            params![qq],
            |row| row.get::<_, String>(0),
        )
        .optional()
    })
    .flatten()
}

pub fn set_user_server(qq: i64, server_name: &str) {
    let _ = super::query(|conn| {
        conn.execute(
            "INSERT INTO gacha_user_setting (qq, server) VALUES (?1, ?2)
             ON CONFLICT(qq) DO UPDATE SET server = ?2",
            params![qq, server_name],
        )
    });
}

// ---------- gacha_pity ----------

pub fn get_pity(qq: i64, server_name: &str) -> i64 {
    super::query(|conn| {
        conn.query_row(
            "SELECT count FROM gacha_pity WHERE qq = ?1 AND server = ?2",
            params![qq, server_name],
            |row| row.get(0),
        )
        .optional()
    })
    .flatten()
    .unwrap_or(0)
}

pub fn set_pity(qq: i64, server_name: &str, count: i64) {
    let _ = super::query(|conn| {
        conn.execute(
            "INSERT INTO gacha_pity (qq, server, count) VALUES (?1, ?2, ?3)
             ON CONFLICT(qq, server) DO UPDATE SET count = ?3",
            params![qq, server_name, count],
        )
    });
}

// ---------- activity_calendar ----------

pub fn save_activities(server: ServerLocale, activities: &[Activity], updated_at: i64) {
    let db_name = server.db_name();
    let _ = super::query(|conn| {
        conn.execute(
            "DELETE FROM activity_calendar WHERE server = ?1",
            params![db_name],
        )?;
        for activity in activities {
            if activity.start_time <= 0 || activity.end_time <= 0 {
                continue;
            }
            conn.execute(
                "INSERT INTO activity_calendar (server, content, type, start, end, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    db_name,
                    activity.content,
                    activity.activity_type.name(),
                    activity.start_time,
                    activity.end_time,
                    updated_at
                ],
            )?;
        }
        Ok(())
    });
}

pub fn load_activities(server: ServerLocale) -> Vec<Activity> {
    super::query(|conn| {
        let mut stmt = conn.prepare(
            "SELECT content, type, start, end, updated_at FROM activity_calendar WHERE server = ?1",
        )?;
        let rows = stmt.query_map(params![server.db_name()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok((content, type_name, start, end, _updated)) = row {
                out.push(Activity {
                    content,
                    time: String::new(),
                    activity_type: ActivityType::from_name(&type_name),
                    server,
                    start_time: start,
                    end_time: end,
                });
            }
        }
        Ok(out)
    })
    .unwrap_or_default()
}

pub fn max_updated_at(server: ServerLocale) -> Option<i64> {
    super::query(|conn| {
        conn.query_row(
            "SELECT MAX(updated_at) FROM activity_calendar WHERE server = ?1",
            params![server.db_name()],
            |row| row.get::<_, Option<i64>>(0),
        )
    })
    .flatten()
}

// ---------- tarot ----------

pub fn count_tarot() -> i64 {
    super::query(|conn| {
        conn.query_row("SELECT COUNT(*) FROM tarot", [], |row| row.get::<_, i64>(0))
    })
    .unwrap_or(0)
}

pub fn insert_tarot(number: i64, name: &str, positive: &str, negative: &str) {
    let _ = super::query(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO tarot (number, name, positive, negative) VALUES (?1, ?2, ?3, ?4)",
            params![number, name, positive, negative],
        )
    });
}

#[derive(Clone, Debug)]
pub struct TarotRow {
    pub number: i64,
    pub name: String,
    pub positive: String,
    pub negative: String,
}

pub fn find_tarot(number: i64) -> Option<TarotRow> {
    super::query(|conn| {
        conn.query_row(
            "SELECT number, name, positive, negative FROM tarot WHERE number = ?1",
            params![number],
            |row| {
                Ok(TarotRow {
                    number: row.get(0)?,
                    name: row.get(1)?,
                    positive: row.get(2)?,
                    negative: row.get(3)?,
                })
            },
        )
        .optional()
    })
    .flatten()
}

#[derive(Clone, Debug)]
pub struct TarotRecordRow {
    pub qq: i64,
    pub group: i64,
    pub day: i64,
    pub tarot: i64,
    pub positive: bool,
}

pub fn get_tarot_record(qq: i64, group: i64) -> Option<TarotRecordRow> {
    super::query(|conn| {
        conn.query_row(
            "SELECT qq, grp, day, tarot, positive FROM tarot_record WHERE qq = ?1 AND grp = ?2",
            params![qq, group],
            |row| {
                Ok(TarotRecordRow {
                    qq: row.get(0)?,
                    group: row.get(1)?,
                    day: row.get(2)?,
                    tarot: row.get(3)?,
                    positive: row.get::<_, i64>(4)? != 0,
                })
            },
        )
        .optional()
    })
    .flatten()
}

pub fn set_tarot_record(qq: i64, group: i64, day: i64, tarot: i64, positive: bool) {
    let positive = if positive { 1 } else { 0 };
    let _ = super::query(|conn| {
        conn.execute(
            "INSERT INTO tarot_record (qq, grp, day, tarot, positive) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(qq, grp) DO UPDATE SET day = ?3, tarot = ?4, positive = ?5",
            params![qq, group, day, tarot, positive],
        )
    });
}

// ---------- gacha pool / character（/抽卡 管理指令） ----------

#[derive(Clone, Debug)]
pub struct PoolRow {
    pub id: i64,
    pub name: String,
}

pub fn list_pools_desc(limit: i64) -> Vec<PoolRow> {
    super::query(|conn| {
        let mut stmt = conn.prepare("SELECT id, name FROM gacha_pool ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(PoolRow {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(item) = row {
                out.push(item);
            }
        }
        Ok(out)
    })
    .unwrap_or_default()
}

pub fn create_pool(name: &str) -> i64 {
    super::query(|conn| {
        conn.execute("INSERT INTO gacha_pool (name) VALUES (?1)", params![name])?;
        Ok(conn.last_insert_rowid())
    })
    .unwrap_or(0)
}

pub fn delete_pool(pool_id: i64) {
    let _ = super::query(|conn| {
        conn.execute(
            "DELETE FROM gacha_pool_character WHERE pool_id = ?1",
            params![pool_id],
        )?;
        conn.execute("DELETE FROM gacha_pool WHERE id = ?1", params![pool_id])
    });
}

pub fn find_pool_by_name(name: &str) -> Option<PoolRow> {
    super::query(|conn| {
        conn.query_row(
            "SELECT id, name FROM gacha_pool WHERE name = ?1",
            params![name],
            |row| {
                Ok(PoolRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            },
        )
        .optional()
    })
    .flatten()
}

pub fn find_pool_by_id(pool_id: i64) -> Option<PoolRow> {
    super::query(|conn| {
        conn.query_row(
            "SELECT id, name FROM gacha_pool WHERE id = ?1",
            params![pool_id],
            |row| {
                Ok(PoolRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            },
        )
        .optional()
    })
    .flatten()
}

pub fn pool_characters(pool_id: i64) -> Vec<(String, i64)> {
    super::query(|conn| {
        let mut stmt = conn.prepare(
            "SELECT c.name, c.star FROM gacha_character c
             JOIN gacha_pool_character pc ON pc.character_id = c.id
             WHERE pc.pool_id = ?1",
        )?;
        let rows = stmt.query_map(params![pool_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(item) = row {
                out.push(item);
            }
        }
        Ok(out)
    })
    .unwrap_or_default()
}

/// 按名字查找或创建角色，返回角色 id
pub fn upsert_character(name: &str, star: i64, limit: bool) -> i64 {
    let is_limit = if limit { 1 } else { 0 };
    let existing: Option<i64> = super::query(|conn| {
        conn.query_row(
            "SELECT id FROM gacha_character WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()
    })
    .flatten();
    if let Some(id) = existing {
        return id;
    }
    super::query(|conn| {
        conn.execute(
            "INSERT INTO gacha_character (name, star, is_limit) VALUES (?1, ?2, ?3)",
            params![name, star, is_limit],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .unwrap_or(0)
}

pub fn add_pool_character(pool_id: i64, character_id: i64) {
    let _ = super::query(|conn| {
        conn.execute(
            "INSERT OR IGNORE INTO gacha_pool_character (pool_id, character_id) VALUES (?1, ?2)",
            params![pool_id, character_id],
        )
    });
}

// ---------- image（arona 云端图片库本地缓存，对应原版 db/image/ImageTable） ----------

#[derive(Clone, Debug)]
pub struct ImageRow {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub hash: String,
    pub r#type: i64,
}

pub fn find_image_by_name(name: &str) -> Option<ImageRow> {
    super::query(|conn| {
        conn.query_row(
            "SELECT id, name, path, hash, type FROM image WHERE name = ?1",
            params![name],
            |row| {
                Ok(ImageRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    hash: row.get(3)?,
                    r#type: row.get(4)?,
                })
            },
        )
        .optional()
    })
    .flatten()
}

pub fn insert_image(name: &str, path: &str, hash: &str, image_type: i64) -> i64 {
    super::query(|conn| {
        conn.execute(
            "INSERT INTO image (name, path, hash, type) VALUES (?1, ?2, ?3, ?4)",
            params![name, path, hash, image_type],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .unwrap_or(0)
}

pub fn update_image(id: i64, path: &str, hash: &str, image_type: i64) {
    let _ = super::query(|conn| {
        conn.execute(
            "UPDATE image SET path = ?2, hash = ?3, type = ?4 WHERE id = ?1",
            params![id, path, hash, image_type],
        )
    });
}
