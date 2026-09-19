//! 学生生日数据源（对应原版 util/scbaleDB/SchaleDBUtil.getBirthdayData +
//! SchaleDBDataSyncService.BirthdayJob）。
//!
//! SchaleDB 的 `data/cn/students.min.json` 中 `BirthDay` 形如 "3/12"；
//! 取「明天起 7 天内」的生日，生成 `ActivityType::Birthday` 活动
//! （`start == end == 生日当天 00:00`），并入活动日历的「即将开始」。
//!
//! 生日只用于日历展示，不参与结束预警（`notify::alert_plan` 已排除生日）。

use crate::entity::{Activity, ActivityType, ServerLocale};
use chrono::TimeZone;
use once_cell::sync::OnceCell;
use serde_json::Value;
use std::sync::Mutex;

/// 数据源顺序与超时策略对齐原版：GitHub -> schale.gg -> 国内镜像
const SOURCES: [&str; 3] = [
    "https://raw.githubusercontent.com/SchaleDB/SchaleDB/main/",
    "https://schale.gg/",
    "https://schaledb.brightsu.cn/",
];
const STUDENTS_PATH: &str = "data/cn/students.min.json";
const CACHE_TTL_MS: i64 = 6 * 60 * 60 * 1000;
/// 展示窗口（天）：原版为「明天起一周内」，不含恰好第 7 天
const WINDOW_DAYS: i64 = 7;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Birthday {
    pub name: String,
    pub month: u32,
    pub day: u32,
}

static CACHE: OnceCell<Mutex<Option<(Vec<Birthday>, i64)>>> = OnceCell::new();

fn cache() -> &'static Mutex<Option<(Vec<Birthday>, i64)>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 拉取全部学生生日（带 6 小时内存缓存，三服共用同一份数据，与原版一致）
pub async fn fetch_birthdays() -> Result<Vec<Birthday>, String> {
    {
        let guard = cache().lock().unwrap();
        if let Some((list, expire_at)) = guard.as_ref() {
            if *expire_at > now_ms() {
                return Ok(list.clone());
            }
        }
    }
    let mut last_error = "没有可用的 SchaleDB 数据源".to_string();
    for base in SOURCES {
        match fetch_from(base).await {
            Ok(list) if !list.is_empty() => {
                arona::runtime::log::info(format!(
                    "SchaleDB 学生生日已同步: {} 条 (来源 {base})",
                    list.len()
                ));
                *cache().lock().unwrap() = Some((list.clone(), now_ms() + CACHE_TTL_MS));
                return Ok(list);
            }
            Ok(_) => last_error = format!("{base} 未解析到生日数据"),
            Err(err) => last_error = err,
        }
    }
    Err(last_error)
}

async fn fetch_from(base: &str) -> Result<Vec<Birthday>, String> {
    let url = format!("{base}{STUDENTS_PATH}");
    let text = super::http::get(&url, base, &[]).await?;
    parse_students(&text)
}

/// 解析 students.min.json：取 `Name` + `BirthDay`，跳过皮肤别名与非法日期
pub fn parse_students(json: &str) -> Result<Vec<Birthday>, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|err| format!("SchaleDB 学生数据解析失败: {err}"))?;
    let array = root
        .as_array()
        .ok_or_else(|| "SchaleDB 学生数据不是数组".to_string())?;
    let mut out: Vec<Birthday> = Vec::new();
    for item in array {
        let Some(name) = item
            .get("Name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        // 原版过滤: 名字带括号的是皮肤/别名，不重复计入
        if name.contains('(') || name.contains('（') {
            continue;
        }
        let Some((month, day)) = item
            .get("BirthDay")
            .and_then(|v| v.as_str())
            .and_then(parse_month_day)
        else {
            continue;
        };
        let birthday = Birthday {
            name: name.to_string(),
            month,
            day,
        };
        if !out.contains(&birthday) {
            out.push(birthday);
        }
    }
    Ok(out)
}

fn parse_month_day(value: &str) -> Option<(u32, u32)> {
    let (month, day) = value.trim().split_once('/')?;
    let month: u32 = month.trim().parse().ok()?;
    let day: u32 = day.trim().parse().ok()?;
    if (1..=12).contains(&month) && (1..=31).contains(&day) {
        Some((month, day))
    } else {
        None
    }
}

/// 生成某服务器「即将到来」的生日活动（对应原版 getBirthdayData）
pub async fn upcoming_birthday_activities(server: ServerLocale) -> Vec<Activity> {
    let list = match fetch_birthdays().await {
        Ok(list) => list,
        Err(err) => {
            arona::runtime::log::warning(format!("学生生日数据获取失败: {err}"));
            return Vec::new();
        }
    };
    birthday_activities_for(&list, server, chrono::Local::now().date_naive())
}

/// 纯函数版本(便于测试): 以 today 为基准取明天起 WINDOW_DAYS 天内的生日
pub fn birthday_activities_for(
    list: &[Birthday],
    server: ServerLocale,
    today: chrono::NaiveDate,
) -> Vec<Activity> {
    let mut activities: Vec<Activity> = Vec::new();
    for birthday in list {
        let Some(date) = next_occurrence(today, birthday) else {
            continue;
        };
        let days = (date - today).num_days();
        if days < 1 || days >= WINDOW_DAYS {
            continue;
        }
        let Some(naive) = date.and_hms_opt(0, 0, 0) else {
            continue;
        };
        let Some(start) = chrono::Local.from_local_datetime(&naive).earliest() else {
            continue;
        };
        let ts = start.timestamp_millis();
        let mut activity = Activity::new(
            format!("{}的生日", birthday.name),
            server,
            ts,
            ts,
            ActivityType::Birthday,
        );
        activity.time = crate::util::time::calc_time(ts, true);
        activities.push(activity);
    }
    activities.sort_by_key(|activity| activity.start_time);
    activities
}

/// 下一个生日日期：当年已过（或就是今天）则顺延到明年，正确处理跨年的一周窗口
fn next_occurrence(today: chrono::NaiveDate, birthday: &Birthday) -> Option<chrono::NaiveDate> {
    use chrono::Datelike;
    let mut date = chrono::NaiveDate::from_ymd_opt(today.year(), birthday.month, birthday.day)?;
    if date <= today {
        date = chrono::NaiveDate::from_ymd_opt(today.year() + 1, birthday.month, birthday.day)?;
    }
    Some(date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    const SAMPLE: &str = r#"[
      {"Name":"爱露","BirthDay":"3/12","Birthday":"3月12日"},
      {"Name":"爱露（泳装）","BirthDay":"3/12"},
      {"Name":"玛丽","BirthDay":"9/12"},
      {"Name":"坏数据","BirthDay":"-"},
      {"Name":"缺生日"},
      {"Name":"越界","BirthDay":"13/40"}
    ]"#;

    #[test]
    fn parse_filters_skins_and_invalid_dates() {
        let list = parse_students(SAMPLE).unwrap();
        assert_eq!(list.len(), 2, "应只保留 爱露/玛丽 两条: {list:?}");
        assert!(list.iter().any(|b| b.name == "爱露"));
        assert!(list.iter().any(|b| b.name == "玛丽"));
        assert!(!list.iter().any(|b| b.name.contains('（')));
    }

    #[test]
    fn window_covers_next_week_only() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 11).unwrap();
        let list = vec![
            Birthday {
                name: "今天".into(),
                month: 9,
                day: 11,
            },
            Birthday {
                name: "明天".into(),
                month: 9,
                day: 12,
            },
            Birthday {
                name: "第六天".into(),
                month: 9,
                day: 17,
            },
            Birthday {
                name: "第七天".into(),
                month: 9,
                day: 18,
            },
            Birthday {
                name: "下个月".into(),
                month: 10,
                day: 1,
            },
        ];
        let activities = birthday_activities_for(&list, ServerLocale::GLOBAL, today);
        let names: Vec<String> = activities
            .iter()
            .map(|activity| activity.content.clone())
            .collect();
        assert!(names.contains(&"明天的生日".to_string()));
        assert!(names.contains(&"第六天的生日".to_string()));
        assert!(
            !names.contains(&"今天的生日".to_string()),
            "当天生日不入列表"
        );
        assert!(
            !names.contains(&"第七天的生日".to_string()),
            "第 7 天超出窗口"
        );
        assert!(!names.contains(&"下个月的生日".to_string()));
        for activity in &activities {
            assert_eq!(activity.activity_type, ActivityType::Birthday);
            assert_eq!(activity.start_time, activity.end_time);
            assert_eq!(activity.server, ServerLocale::GLOBAL);
        }
    }

    #[test]
    fn window_handles_year_rollover() {
        let today = NaiveDate::from_ymd_opt(2026, 12, 30).unwrap();
        let list = vec![Birthday {
            name: "新年".into(),
            month: 1,
            day: 2,
        }];
        let activities = birthday_activities_for(&list, ServerLocale::CN, today);
        assert_eq!(activities.len(), 1, "跨年窗口应包含 1 月 2 日生日");
        assert_eq!(activities[0].content, "新年的生日");
    }

    #[ignore = "联网自检, 运行: cargo test birthday -- --ignored --nocapture --test-threads=1"]
    #[tokio::test]
    async fn fetch_real_birthdays() {
        let list = fetch_birthdays().await.expect("拉取 SchaleDB 生日失败");
        println!("生日条目总数: {}", list.len());
        let today = chrono::Local::now().date_naive();
        for server in ServerLocale::ALL {
            let activities = birthday_activities_for(&list, server, today);
            println!(
                "{} 未来一周生日 {} 条",
                server.server_name(),
                activities.len()
            );
            for activity in &activities {
                println!("  {} {}", activity.content, activity.time);
            }
        }
        assert!(!list.is_empty(), "生日列表为空");
    }

    /// 端到端自检: GameKee 活动 + SchaleDB 生日 -> 文本日历 + 渲染图片。
    /// 运行: cargo smoke-birthday
    #[ignore = "联网自检, 运行: cargo smoke-birthday"]
    #[tokio::test]
    async fn calendar_includes_birthday_end_to_end() {
        for server in ServerLocale::ALL {
            let pair = crate::data::activity::fetch(server)
                .await
                .expect("拉取活动日历失败");
            let expected = upcoming_birthday_activities(server).await;
            println!(
                "==== {} 即将开始 {} 条(其中生日 {} 条) ====",
                server.server_name(),
                pair.1.len(),
                expected.len()
            );
            for activity in &expected {
                println!("  生日: {} {}", activity.content, activity.time);
                assert!(
                    pair.1.iter().any(|it| {
                        it.activity_type == ActivityType::Birthday && it.content == activity.content
                    }),
                    "{} 日历数据流缺少生日: {}",
                    server.server_name(),
                    activity.content
                );
            }
            let text = crate::data::activity::format_calendar(&pair, server);
            println!("{text}");
            for activity in &expected {
                assert!(
                    text.contains(&activity.content),
                    "{} 文本日历缺少生日: {}",
                    server.server_name(),
                    activity.content
                );
            }
            let file = crate::image::activity::render(&pair, server).expect("渲染活动日历图片失败");
            println!("图片: {}", file.display());
            assert!(file.exists(), "活动日历图片未生成: {}", file.display());
        }
    }
}
