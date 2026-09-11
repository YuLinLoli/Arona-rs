//! 独立模式 arona 业务配置持有者（对应原版 standalone/StandaloneAronaConfig）
//! 负责加载 arona.yml、监听文件变更自动热重载、为 /config 指令提供读写能力。

use super::arona::{AronaConfig, NotifyConfig};
use crate::runtime::value::{self, ConfigValue};
use once_cell::sync::OnceCell;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::SystemTime;

pub struct ConfigField {
    pub key: &'static str,
    pub description: &'static str,
}

pub const FIELDS: [ConfigField; 9] = [
    ConfigField {
        key: "groups",
        description: "允许响应的群号列表，留空表示响应所有群",
    },
    ConfigField {
        key: "managers",
        description: "管理员 QQ 号列表，可执行管理命令",
    },
    ConfigField {
        key: "notify.enable",
        description: "是否启用每日活动防侠推送",
    },
    ConfigField {
        key: "notify.every_day_hour",
        description: "每日推送的小时(0-23)",
    },
    ConfigField {
        key: "notify.jp",
        description: "是否推送日服活动",
    },
    ConfigField {
        key: "notify.global",
        description: "是否推送国际服活动",
    },
    ConfigField {
        key: "notify.cn",
        description: "是否推送国服活动",
    },
    ConfigField {
        key: "notify.black_groups",
        description: "不推送的群号列表（黑名单），留空表示推送到全部允许的群",
    },
    ConfigField {
        key: "notify.notify_text",
        description: "推送消息开头文字",
    },
];

struct State {
    file: Option<PathBuf>,
    config: AronaConfig,
    last_modified: Option<SystemTime>,
    last_notify_hour: i64,
}

static STATE: OnceCell<RwLock<State>> = OnceCell::new();

fn state() -> &'static RwLock<State> {
    STATE.get_or_init(|| {
        RwLock::new(State {
            file: None,
            config: AronaConfig::default(),
            last_modified: None,
            last_notify_hour: -1,
        })
    })
}

pub fn default_file() -> PathBuf {
    super::arona::default_file()
}

pub fn init(path: PathBuf) {
    {
        let mut st = state().write().unwrap();
        st.file = Some(path);
    }
    reload();
    spawn_watcher();
}

/// 当前配置快照
pub fn config() -> AronaConfig {
    state().read().unwrap().config.clone()
}

pub fn notify_config() -> NotifyConfig {
    config().notify
}

pub fn find_field_index(key: &str) -> Option<usize> {
    FIELDS
        .iter()
        .position(|f| f.key == key || f.key.strip_prefix("notify.") == Some(key))
}

fn field_value(field: &ConfigField, config: &AronaConfig) -> ConfigValue {
    match field.key {
        "groups" => ConfigValue::ListLong(config.groups.clone()),
        "managers" => ConfigValue::ListLong(config.managers.clone()),
        "notify.enable" => ConfigValue::Bool(config.notify.enable),
        "notify.every_day_hour" => ConfigValue::Int(config.notify.every_day_hour),
        "notify.jp" => ConfigValue::Bool(config.notify.jp),
        "notify.global" => ConfigValue::Bool(config.notify.global),
        "notify.cn" => ConfigValue::Bool(config.notify.cn),
        "notify.black_groups" => ConfigValue::ListLong(config.notify.black_groups.clone()),
        "notify.notify_text" => ConfigValue::Text(config.notify.notify_text.clone()),
        _ => ConfigValue::Text(String::new()),
    }
}

fn field_set(
    field: &ConfigField,
    config: &mut AronaConfig,
    value: ConfigValue,
) -> Result<(), String> {
    match field.key {
        "groups" => {
            config.groups = expect_list(value, "groups")?;
        }
        "managers" => {
            config.managers = expect_list(value, "managers")?;
        }
        "notify.enable" => {
            config.notify.enable = expect_bool(value)?;
        }
        "notify.every_day_hour" => {
            let hour = expect_int(value)?;
            if !(0..=23).contains(&hour) {
                return Err("推送小时必须在 0-23 之间".to_string());
            }
            config.notify.every_day_hour = hour;
        }
        "notify.jp" => config.notify.jp = expect_bool(value)?,
        "notify.global" => config.notify.global = expect_bool(value)?,
        "notify.cn" => config.notify.cn = expect_bool(value)?,
        "notify.black_groups" => {
            config.notify.black_groups = expect_list(value, "notify.black_groups")?;
        }
        "notify.notify_text" => {
            config.notify.notify_text = expect_text(value)?;
        }
        _ => return Err("未知配置项".to_string()),
    }
    Ok(())
}

fn expect_bool(value: ConfigValue) -> Result<bool, String> {
    match value {
        ConfigValue::Bool(v) => Ok(v),
        _ => Err("类型错误：应为布尔值".to_string()),
    }
}

fn expect_int(value: ConfigValue) -> Result<i32, String> {
    match value {
        ConfigValue::Int(v) => Ok(v),
        _ => Err("类型错误：应为整数".to_string()),
    }
}

fn expect_text(value: ConfigValue) -> Result<String, String> {
    match value {
        ConfigValue::Text(v) => Ok(v),
        _ => Err("类型错误：应为字符串".to_string()),
    }
}

fn expect_list(value: ConfigValue, key: &str) -> Result<Vec<i64>, String> {
    match value {
        ConfigValue::ListLong(v) => Ok(v),
        _ => Err(format!("{key} 不是列表配置项")),
    }
}

/// 重新从磁盘读取 arona.yml 并同步到运行期配置
pub fn reload() {
    let (path, current) = {
        let st = state().read().unwrap();
        (st.file.clone(), st.config.clone())
    };
    let path = match path {
        Some(p) => p,
        None => return,
    };
    let new_config = match super::arona::load(&path) {
        Ok(c) => c,
        Err(err) => {
            crate::runtime::log::warning(format!("arona.yml 热重载失败，保留当前配置: {err}"));
            return;
        }
    };
    let _ = current;
    apply(new_config);
}

/// 将配置同步到运行期（groups/managers/notify 与活动推送定时）
fn apply(new_config: AronaConfig) {
    crate::runtime::config::set_groups(new_config.groups.clone());
    crate::runtime::config::set_managers(new_config.managers.clone());
    crate::runtime::config::set_global_blacklist(new_config.global_blacklist.clone());
    crate::runtime::config::set_group_settings(new_config.group_settings.clone());
    let hour = new_config.notify.every_day_hour as i64;
    {
        let mut st = state().write().unwrap();
        let hour_changed = st.last_notify_hour != -1 && st.last_notify_hour != hour;
        st.last_notify_hour = hour;
        st.config = new_config;
        if hour_changed {
            crate::quartz::reschedule_daily_notify(hour);
            crate::runtime::log::info(format!("活动推送小时变更，定时任务已重建: 每天 {hour} 点"));
        }
    }
    crate::runtime::log::info("arona.yml 配置已重载");
}

/// 机器人被移出群时从 groups 配置中移除该群并写回文件热重载
pub fn remove_group_if_present(group_id: i64) {
    let mut changed = false;
    {
        let mut st = state().write().unwrap();
        if st.config.groups.contains(&group_id) {
            st.config.groups.retain(|g| *g != group_id);
            changed = true;
        }
    }
    if !changed {
        return;
    }
    let path = state().read().unwrap().file.clone();
    if let Some(path) = path {
        let config = config();
        if let Err(err) = super::arona::save(&path, &config) {
            crate::runtime::log::warning(format!("从 groups 移除群 {group_id} 失败: {err}"));
        }
    }
    reload();
}

/// 停止热重载监听（优雅关闭时使用）
pub fn close() {
    // 轮询任务由全局调度管理，进程退出即结束
    drop(state().write().unwrap());
}

/// /config <key> <value>：修改配置并写回文件后重载；show_value=false 时不回显（群聊防泄漏）
pub fn update(key: &str, raw_value: &str, show_value: bool) -> String {
    let idx = match find_field_index(key) {
        Some(i) => i,
        None => return format!("未找到配置项: {key}"),
    };
    let field = &FIELDS[idx];
    let current = {
        let st = state().read().unwrap();
        field_value(field, &st.config)
    };
    let parsed = match value::parse(raw_value, &current) {
        Ok(v) => v,
        Err(err) => return err,
    };
    let mut config = config();
    if let Err(err) = field_set(field, &mut config, parsed) {
        return err;
    }
    let path = match state().read().unwrap().file.clone() {
        Some(p) => p,
        None => return "配置文件尚未初始化".to_string(),
    };
    if let Err(err) = super::arona::save(&path, &config) {
        return format!("写入配置文件失败: {err}");
    }
    apply(config.clone());
    let value_text = {
        let st = state().read().unwrap();
        field_value(field, &st.config).display_kt()
    };
    if show_value {
        format!("配置已更新: {} = {value_text}", field.key)
    } else {
        format!("配置已更新: {}", field.key)
    }
}

/// /config <key> add [value]：向列表配置项追加一个值
pub fn add_to_list(key: &str, value: i64, show_value: bool) -> String {
    let field_index = match find_field_index(key) {
        Some(i) => i,
        None => return format!("未找到配置项: {key}"),
    };
    let field = &FIELDS[field_index];
    let mut config = config();
    let list = match field.key {
        "groups" => &mut config.groups,
        "managers" => &mut config.managers,
        "notify.black_groups" => &mut config.notify.black_groups,
        _ => return format!("该配置项不是列表: {key}"),
    };
    if list.contains(&value) {
        return format!("{key} 已包含 {value}");
    }
    list.push(value);
    let path = match state().read().unwrap().file.clone() {
        Some(p) => p,
        None => return "配置文件尚未初始化".to_string(),
    };
    if let Err(err) = super::arona::save(&path, &config) {
        return format!("写入配置文件失败: {err}");
    }
    let list_text = match field.key {
        "groups" => format!("{:?}", config.groups),
        "managers" => format!("{:?}", config.managers),
        _ => format!("{:?}", config.notify.black_groups),
    };
    apply(config);
    if show_value {
        format!("配置已更新: {key} = {list_text}")
    } else {
        format!("配置已更新: {key}")
    }
}

/// /config <key> del [value]：从列表配置项移除一个值
pub fn remove_from_list(key: &str, value: i64, show_value: bool) -> String {
    let field_index = match find_field_index(key) {
        Some(i) => i,
        None => return format!("未找到配置项: {key}"),
    };
    let field = &FIELDS[field_index];
    let mut config = config();
    let list = match field.key {
        "groups" => &mut config.groups,
        "managers" => &mut config.managers,
        "notify.black_groups" => &mut config.notify.black_groups,
        _ => return format!("该配置项不是列表: {key}"),
    };
    if !list.contains(&value) {
        return format!("{key} 不包含 {value}");
    }
    list.retain(|v| *v != value);
    let path = match state().read().unwrap().file.clone() {
        Some(p) => p,
        None => return "配置文件尚未初始化".to_string(),
    };
    if let Err(err) = super::arona::save(&path, &config) {
        return format!("写入配置文件失败: {err}");
    }
    apply(config);
    if show_value {
        format!("配置已更新: {key}")
    } else {
        format!("配置已更新: {key}")
    }
}

// ==================== GUI 用的配置读写接口 ====================

/// 配置文件路径
pub fn config_file() -> Option<PathBuf> {
    state().read().unwrap().file.clone()
}

/// 写回 arona.yml 并立即应用到运行期（GUI 保存用）
fn persist(config: &AronaConfig) -> Result<(), String> {
    let path = state()
        .read()
        .unwrap()
        .file
        .clone()
        .ok_or_else(|| "配置文件尚未初始化".to_string())?;
    super::arona::save(&path, config).map_err(|err| format!("写入 arona.yml 失败: {err}"))?;
    apply(config.clone());
    Ok(())
}

/// 清理空的分群设置
fn normalize_group_settings(config: &mut AronaConfig) {
    config.group_settings.retain(|_, setting| {
        !(setting.disabled_features.is_empty() && setting.blacklist.is_empty())
    });
}

/// 启用/停用某个群（启用 = 加入 groups，停用 = 移出 groups）
pub fn set_group_enabled(group_id: i64, enabled: bool) -> Result<(), String> {
    let mut config = config();
    if enabled {
        if !config.groups.contains(&group_id) {
            config.groups.push(group_id);
            config.groups.sort();
        }
    } else {
        config.groups.retain(|group| *group != group_id);
    }
    persist(&config)
}

/// 开启/关闭某个群的某项功能
pub fn set_group_feature(group_id: i64, feature: &str, enabled: bool) -> Result<(), String> {
    let mut config = config();
    {
        let setting = config
            .group_settings
            .entry(group_id.to_string())
            .or_default();
        setting.disabled_features.retain(|key| key != feature);
        if !enabled {
            setting.disabled_features.push(feature.to_string());
            setting.disabled_features.sort();
        }
    }
    normalize_group_settings(&mut config);
    persist(&config)
}

/// 群成员黑名单：加入或移出
pub fn set_group_blacklist(group_id: i64, user_id: i64, blacklisted: bool) -> Result<(), String> {
    let mut config = config();
    {
        let setting = config
            .group_settings
            .entry(group_id.to_string())
            .or_default();
        setting.blacklist.retain(|id| *id != user_id);
        if blacklisted {
            setting.blacklist.push(user_id);
            setting.blacklist.sort();
        }
    }
    normalize_group_settings(&mut config);
    persist(&config)
}

/// 全局黑名单：加入或移出
pub fn set_global_blacklist(user_id: i64, blacklisted: bool) -> Result<(), String> {
    let mut config = config();
    config.global_blacklist.retain(|id| *id != user_id);
    if blacklisted {
        config.global_blacklist.push(user_id);
        config.global_blacklist.sort();
    }
    persist(&config)
}

/// 清空某个群的全部设置（功能开关与群内黑名单）
pub fn clear_group_setting(group_id: i64) -> Result<(), String> {
    let mut config = config();
    config.group_settings.remove(&group_id.to_string());
    persist(&config)
}

/// 轮询监听 arona.yml 修改（每 2 秒），触发热重载
fn spawn_watcher() {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        interval.tick().await; // 首个 tick 立即返回，跳过
        loop {
            interval.tick().await;
            let path = state().read().unwrap().file.clone();
            let Some(path) = path else { continue };
            let modified = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            let last = state().read().unwrap().last_modified;
            let is_first = last.is_none() && modified.is_some();
            let changed = match (last, modified) {
                (Some(old), Some(new)) => new != old,
                _ => false,
            };
            if changed || is_first {
                if let Some(m) = modified {
                    state().write().unwrap().last_modified = Some(m);
                }
                reload();
            }
        }
    });
}
