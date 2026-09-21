//! 独立模式 arona 业务配置持有者（对应原版 standalone/StandaloneAronaConfig）
//! 负责加载框架那份 config/arona.yml、监听文件变更自动热重载、为 /config 指令提供读写能力。
//! 插件自己的配置在 config/<插件>/arona.yml，由 [`super::plugin_config`] 管。

use super::arona::AronaConfig;
use crate::runtime::value::{self, ConfigValue};
use once_cell::sync::OnceCell;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

pub struct ConfigField {
    pub key: &'static str,
    pub description: &'static str,
}

/// 框架自身管理的通用配置项（授权/黑名单）。插件自己持有的配置区（notify/trainer…）
/// 住在各自的 config/<插件>/arona.yml 里，由插件通过 [`super::plugin_config::section_value`]
/// 读写，不进本表。
pub const FIELDS: [ConfigField; 2] = [
    ConfigField {
        key: "groups",
        description: "允许响应的群号列表，留空表示响应所有群",
    },
    ConfigField {
        key: "managers",
        description: "管理员 QQ 号列表，可执行管理命令",
    },
];

struct State {
    file: Option<PathBuf>,
    config: AronaConfig,
    last_modified: Option<SystemTime>,
    /// 首次 apply（init 加载）不通知插件重建任务；之后每次热重载/写入都通知
    applied_once: bool,
}

static STATE: OnceCell<RwLock<State>> = OnceCell::new();

fn state() -> &'static RwLock<State> {
    STATE.get_or_init(|| {
        RwLock::new(State {
            file: None,
            config: AronaConfig::default(),
            last_modified: None,
            applied_once: false,
        })
    })
}

pub fn default_file() -> PathBuf {
    super::arona::default_file()
}

/// 初始化业务配置并启动热重载监听。
/// 返回 Err 表示 arona.yml 本身有问题（调用方负责提示用户，程序仍会按默认配置继续运行）
pub fn init(path: PathBuf) -> Result<(), String> {
    {
        let mut st = state().write().unwrap();
        st.file = Some(path);
    }
    let result = reload_inner();
    spawn_watcher();
    result
}

/// 当前配置快照
pub fn config() -> AronaConfig {
    state().read().unwrap().config.clone()
}

pub fn find_field_index(key: &str) -> Option<usize> {
    FIELDS.iter().position(|f| f.key == key)
}

fn field_value(field: &ConfigField, config: &AronaConfig) -> ConfigValue {
    match field.key {
        "groups" => ConfigValue::ListLong(config.groups.clone()),
        "managers" => ConfigValue::ListLong(config.managers.clone()),
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
        _ => return Err("未知配置项".to_string()),
    }
    Ok(())
}

fn expect_list(value: ConfigValue, key: &str) -> Result<Vec<i64>, String> {
    match value {
        ConfigValue::ListLong(v) => Ok(v),
        _ => Err(format!("{key} 不是列表配置项")),
    }
}

/// 重新从磁盘读取 arona.yml 并同步到运行期配置（热重载路径：失败只记日志、保留当前配置）
pub fn reload() {
    if let Err(err) = reload_inner() {
        crate::runtime::log::warning(format!("arona.yml 热重载失败，保留当前配置: {err}"));
    }
}

/// 重新读取并应用配置；返回 Err 表示文件有问题（此时不会改动运行期配置）
fn reload_inner() -> Result<(), String> {
    let (path, current) = {
        let st = state().read().unwrap();
        (st.file.clone(), st.config.clone())
    };
    let Some(path) = path else {
        return Ok(());
    };
    // 记下本次读到的修改时间：热重载监听据此判断文件是否又变了（少了这一步，启动后
    // 监听的第一个 tick 会认为「从没见过这个文件」，把整份配置再重载一次）
    let modified = std::fs::metadata(&path)
        .ok()
        .and_then(|meta| meta.modified().ok());
    let new_config = super::arona::load(&path)?;
    let _ = current;
    if let Some(modified) = modified {
        state().write().unwrap().last_modified = Some(modified);
    }
    apply(new_config);
    Ok(())
}

/// 将配置同步到运行期（groups/managers/黑名单/分群设置），并在热重载/写入后通知插件
fn apply(new_config: AronaConfig) {
    crate::runtime::config::set_groups(new_config.groups.clone());
    crate::runtime::config::set_managers(new_config.managers.clone());
    crate::runtime::config::set_global_blacklist(new_config.global_blacklist.clone());
    crate::runtime::config::set_group_settings(new_config.group_settings.clone());
    crate::runtime::config::set_disabled_plugins(new_config.disabled_plugins.clone());
    // 框架行为选项写进所属实例的活状态：阈值在健康度板上，前缀开关在命令表上
    let framework = crate::framework::Framework::global();
    framework.set_panic_disable_threshold(new_config.framework.panic_disable_threshold);
    framework.set_prefix_match_by_default(new_config.framework.prefix_match_by_default);
    // 聊天记录缓存：开关与保留期直接生效，热重载后不必重启
    crate::runtime::chatlog::apply(new_config.chatlog.clone());
    let first = {
        let mut st = state().write().unwrap();
        st.config = new_config;
        let first = !st.applied_once;
        st.applied_once = true;
        first
    };
    // 首次加载不通知：那时插件还没装配，configure/start 会按这份名单自己跳过禁用的插件。
    // 之后每次重载/写入都通知，且写锁已释放（插件会回读本配置的读锁）。
    if !first {
        // 先协调启用状态（禁用→stop / 启用→configure+start），再通知热重载：
        // 刚启用的插件会同时收到一次 on_config_reload，自己的定时任务才是按新配置建的。
        crate::plugin::sync_enabled_state();
        crate::plugin::notify_config_reloaded();
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
        _ => format!("{:?}", config.managers),
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
    let list_text = match field.key {
        "groups" => format!("{:?}", config.groups),
        _ => format!("{:?}", config.managers),
    };
    apply(config);
    if show_value {
        format!("配置已更新: {key} = {list_text}")
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
    config
        .group_settings
        .retain(|_, setting| !setting.is_empty());
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
    set_group_features(group_id, &[feature], enabled)
}

/// 一次开启/关闭某个群的多项功能：GUI「功能开关」按插件整组开关时用，多个 key 只落盘一次
pub fn set_group_features(group_id: i64, features: &[&str], enabled: bool) -> Result<(), String> {
    let mut config = config();
    {
        let setting = config
            .group_settings
            .entry(group_id.to_string())
            .or_default();
        for feature in features {
            setting.disabled_features.retain(|key| key != *feature);
            if !enabled {
                setting.disabled_features.push((*feature).to_string());
            }
        }
        setting.disabled_features.sort();
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

/// 全局启用/停用某个插件（GUI「插件管理」页的总开关）。
/// 写盘后 apply 会调 [`crate::plugin::sync_enabled_state`]：停用即 stop()，启用即补 configure()+start()。
pub fn set_plugin_enabled(plugin: &str, enabled: bool) -> Result<(), String> {
    let mut config = config();
    config
        .disabled_plugins
        .retain(|name| !name.eq_ignore_ascii_case(plugin));
    if !enabled {
        config.disabled_plugins.push(plugin.to_string());
    }
    persist(&config)
}

/// 在某个群里启用/停用某个插件（GUI「群管理」页）。只影响路由，不动插件的后台任务。
pub fn set_group_plugin_enabled(group_id: i64, plugin: &str, enabled: bool) -> Result<(), String> {
    let mut config = config();
    {
        let setting = config
            .group_settings
            .entry(group_id.to_string())
            .or_default();
        setting
            .disabled_plugins
            .retain(|name| !name.eq_ignore_ascii_case(plugin));
        if !enabled {
            setting.disabled_plugins.push(plugin.to_string());
            setting.disabled_plugins.sort();
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

/// 轮询监听配置文件修改（每 2 秒）：框架的 arona.yml 与各插件的 config/<插件>/arona.yml
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
            // 插件配置文件同一条链路上顺带检查：省得起第二个轮询任务
            super::plugin_config::poll_changed();
        }
    });
}
