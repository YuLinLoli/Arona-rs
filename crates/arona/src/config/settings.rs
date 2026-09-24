//! 框架那份业务配置的持有者（对应原版 standalone/StandaloneAronaConfig）
//! 负责加载框架的 config/arona.yml、监听文件变更自动热重载、为 /config 指令提供读写能力。
//! 插件自己的配置在 config/<插件>/arona.yml，由 [`super::plugin_config`] 管。
//!
//! 这份状态挂在 [`crate::framework::Framework`] 实例上，本模块的自由函数走进程默认实例——
//! 和 [`super::plugin_config`] 同一个判据。动态插件 dll 装载时宿主会把自己的实例交进 dll
//! （[`Framework::adopt_host`](crate::framework::Framework::adopt_host)），于是插件里的
//! `/config` 读到、写到的就是宿主真正在用的那份 arona.yml，而不是 dll 里那份永远空着的副本。

use super::arona::AronaConfig;
use crate::framework::Framework;
use crate::runtime::value::{self, ConfigValue};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};
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

/// `config/arona.yml` 的持有者，由 [`Framework`] 实例持有
pub struct Settings {
    state: RwLock<State>,
    /// 回填后，重载/写入的配置才落到**它所属那一套**门控与健康度上（见 [`Settings::attach`]）
    framework: OnceLock<Weak<Framework>>,
    /// 热重载轮询只起一次：`init` 如今对插件也开放，重复调用不该 spawn 第二个监听
    watching: AtomicBool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            state: RwLock::new(State {
                file: None,
                config: AronaConfig::default(),
                last_modified: None,
                applied_once: false,
            }),
            framework: OnceLock::new(),
            watching: AtomicBool::new(false),
        }
    }
}

impl Settings {
    pub(crate) fn attach(&self, framework: &Arc<Framework>) {
        let _ = self.framework.set(Arc::downgrade(framework));
    }

    /// 所属的框架实例；没回填过（例如手工构造的 `Settings`）时退回进程默认实例
    fn host(&self) -> Arc<Framework> {
        self.framework
            .get()
            .and_then(Weak::upgrade)
            .unwrap_or_else(Framework::global_arc)
    }

    fn file(&self) -> Option<PathBuf> {
        self.state.read().unwrap().file.clone()
    }

    /// 初始化业务配置并启动热重载监听。
    /// 返回 Err 表示 arona.yml 本身有问题（调用方负责提示用户，程序仍会按默认配置继续运行）
    pub fn init(self: &Arc<Self>, path: PathBuf) -> Result<(), String> {
        self.state.write().unwrap().file = Some(path);
        let result = self.reload_inner();
        if !self.watching.swap(true, Ordering::SeqCst) {
            spawn_watcher(self.clone());
        }
        result
    }

    /// 当前配置快照
    pub fn config(&self) -> AronaConfig {
        self.state.read().unwrap().config.clone()
    }

    /// 配置文件路径
    pub fn config_file(&self) -> Option<PathBuf> {
        self.file()
    }

    /// 重新从磁盘读取 arona.yml 并同步到运行期配置（热重载路径：失败只记日志、保留当前配置）
    pub fn reload(&self) {
        if let Err(err) = self.reload_inner() {
            crate::runtime::log::warning(format!("arona.yml 热重载失败，保留当前配置: {err}"));
        }
    }

    /// 重新读取并应用配置；返回 Err 表示文件有问题（此时不会改动运行期配置）
    fn reload_inner(&self) -> Result<(), String> {
        let Some(path) = self.file() else {
            return Ok(());
        };
        // 记下本次读到的修改时间：热重载监听据此判断文件是否又变了（少了这一步，启动后
        // 监听的第一个 tick 会认为「从没见过这个文件」，把整份配置再重载一次）
        let modified = std::fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok());
        let new_config = super::arona::load_in(&self.host(), &path)?;
        if let Some(modified) = modified {
            self.state.write().unwrap().last_modified = Some(modified);
        }
        self.apply(new_config);
        Ok(())
    }

    /// 将配置同步到运行期（groups/managers/黑名单/分群设置），并在热重载/写入后通知插件
    fn apply(&self, new_config: AronaConfig) {
        let framework = self.host();
        let gating = framework.gating();
        gating.set_groups(new_config.groups.clone());
        gating.set_managers(new_config.managers.clone());
        gating.set_global_blacklist(new_config.global_blacklist.clone());
        gating.set_group_settings(new_config.group_settings.clone());
        gating.set_disabled_plugins(new_config.disabled_plugins.clone());
        // 框架行为选项写进所属实例的活状态：阈值在健康度板上，前缀开关在命令表上
        framework.set_panic_disable_threshold(new_config.framework.panic_disable_threshold);
        framework.set_prefix_match_by_default(new_config.framework.prefix_match_by_default);
        // 聊天记录缓存：开关与保留期直接生效，热重载后不必重启
        crate::runtime::chatlog::apply(new_config.chatlog.clone());
        let first = {
            let mut st = self.state.write().unwrap();
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
            framework.plugins().sync_enabled_state();
            framework.plugins().notify_config_reloaded(None);
        }
        crate::runtime::log::info("arona.yml 配置已重载");
    }

    /// 机器人被移出群时从 groups 配置中移除该群并写回文件热重载
    pub fn remove_group_if_present(&self, group_id: i64) {
        let mut changed = false;
        {
            let mut st = self.state.write().unwrap();
            if st.config.groups.contains(&group_id) {
                st.config.groups.retain(|g| *g != group_id);
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let path = self.file();
        if let Some(path) = path {
            let config = self.config();
            if let Err(err) = super::arona::save(&path, &config) {
                crate::runtime::log::warning(format!("从 groups 移除群 {group_id} 失败: {err}"));
            }
        }
        self.reload();
    }

    /// 停止热重载监听（优雅关闭时使用）
    pub fn close(&self) {
        // 轮询任务由全局调度管理，进程退出即结束
        drop(self.state.write().unwrap());
    }

    /// /config <key> <value>：修改配置并写回文件后重载；show_value=false 时不回显（群聊防泄漏）
    pub fn update(&self, key: &str, raw_value: &str, show_value: bool) -> String {
        let idx = match find_field_index(key) {
            Some(i) => i,
            None => return format!("未找到配置项: {key}"),
        };
        let field = &FIELDS[idx];
        let current = {
            let st = self.state.read().unwrap();
            field_value(field, &st.config)
        };
        let parsed = match value::parse(raw_value, &current) {
            Ok(v) => v,
            Err(err) => return err,
        };
        let mut config = self.config();
        if let Err(err) = field_set(field, &mut config, parsed) {
            return err;
        }
        let Some(path) = self.file() else {
            return "配置文件尚未初始化".to_string();
        };
        if let Err(err) = super::arona::save(&path, &config) {
            return format!("写入配置文件失败: {err}");
        }
        self.apply(config.clone());
        let value_text = {
            let st = self.state.read().unwrap();
            field_value(field, &st.config).display_kt()
        };
        if show_value {
            format!("配置已更新: {} = {value_text}", field.key)
        } else {
            format!("配置已更新: {}", field.key)
        }
    }

    /// /config <key> add [value]：向列表配置项追加一个值
    pub fn add_to_list(&self, key: &str, value: i64, show_value: bool) -> String {
        let field_index = match find_field_index(key) {
            Some(i) => i,
            None => return format!("未找到配置项: {key}"),
        };
        let field = &FIELDS[field_index];
        let mut config = self.config();
        let list = match field.key {
            "groups" => &mut config.groups,
            "managers" => &mut config.managers,
            _ => return format!("该配置项不是列表: {key}"),
        };
        if list.contains(&value) {
            return format!("{key} 已包含 {value}");
        }
        list.push(value);
        let Some(path) = self.file() else {
            return "配置文件尚未初始化".to_string();
        };
        if let Err(err) = super::arona::save(&path, &config) {
            return format!("写入配置文件失败: {err}");
        }
        let list_text = match field.key {
            "groups" => format!("{:?}", config.groups),
            _ => format!("{:?}", config.managers),
        };
        self.apply(config);
        if show_value {
            format!("配置已更新: {key} = {list_text}")
        } else {
            format!("配置已更新: {key}")
        }
    }

    /// /config <key> del [value]：从列表配置项移除一个值
    pub fn remove_from_list(&self, key: &str, value: i64, show_value: bool) -> String {
        let field_index = match find_field_index(key) {
            Some(i) => i,
            None => return format!("未找到配置项: {key}"),
        };
        let field = &FIELDS[field_index];
        let mut config = self.config();
        let list = match field.key {
            "groups" => &mut config.groups,
            "managers" => &mut config.managers,
            _ => return format!("该配置项不是列表: {key}"),
        };
        if !list.contains(&value) {
            return format!("{key} 不包含 {value}");
        }
        list.retain(|v| *v != value);
        let Some(path) = self.file() else {
            return "配置文件尚未初始化".to_string();
        };
        if let Err(err) = super::arona::save(&path, &config) {
            return format!("写入配置文件失败: {err}");
        }
        let list_text = match field.key {
            "groups" => format!("{:?}", config.groups),
            _ => format!("{:?}", config.managers),
        };
        self.apply(config);
        if show_value {
            format!("配置已更新: {key} = {list_text}")
        } else {
            format!("配置已更新: {key}")
        }
    }

    // ==================== GUI 用的配置读写接口 ====================

    /// 写回 arona.yml 并立即应用到运行期（GUI 保存用）
    fn persist(&self, config: &AronaConfig) -> Result<(), String> {
        let path = self
            .file()
            .ok_or_else(|| "配置文件尚未初始化".to_string())?;
        super::arona::save(&path, config).map_err(|err| format!("写入 arona.yml 失败: {err}"))?;
        self.apply(config.clone());
        Ok(())
    }

    /// 启用/停用某个群（启用 = 加入 groups，停用 = 移出 groups）
    pub fn set_group_enabled(&self, group_id: i64, enabled: bool) -> Result<(), String> {
        let mut config = self.config();
        if enabled {
            if !config.groups.contains(&group_id) {
                config.groups.push(group_id);
                config.groups.sort();
            }
        } else {
            config.groups.retain(|group| *group != group_id);
        }
        self.persist(&config)
    }

    /// 开启/关闭某个群的某项功能
    pub fn set_group_feature(
        &self,
        group_id: i64,
        feature: &str,
        enabled: bool,
    ) -> Result<(), String> {
        self.set_group_features(group_id, &[feature], enabled)
    }

    /// 一次开启/关闭某个群的多项功能：GUI「功能开关」按插件整组开关时用，多个 key 只落盘一次
    pub fn set_group_features(
        &self,
        group_id: i64,
        features: &[&str],
        enabled: bool,
    ) -> Result<(), String> {
        let mut config = self.config();
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
        self.persist(&config)
    }

    /// 群成员黑名单：加入或移出
    pub fn set_group_blacklist(
        &self,
        group_id: i64,
        user_id: i64,
        blacklisted: bool,
    ) -> Result<(), String> {
        let mut config = self.config();
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
        self.persist(&config)
    }

    /// 全局启用/停用某个插件（GUI「插件管理」页的总开关）。
    /// 写盘后 apply 会调 [`crate::plugin::sync_enabled_state`]：停用即 stop()，启用即补 configure()+start()。
    pub fn set_plugin_enabled(&self, plugin: &str, enabled: bool) -> Result<(), String> {
        let mut config = self.config();
        config
            .disabled_plugins
            .retain(|name| !name.eq_ignore_ascii_case(plugin));
        if !enabled {
            config.disabled_plugins.push(plugin.to_string());
        }
        self.persist(&config)
    }

    /// 在某个群里启用/停用某个插件（GUI「群管理」页）。只影响路由，不动插件的后台任务。
    pub fn set_group_plugin_enabled(
        &self,
        group_id: i64,
        plugin: &str,
        enabled: bool,
    ) -> Result<(), String> {
        let mut config = self.config();
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
        self.persist(&config)
    }

    /// 全局黑名单：加入或移出
    pub fn set_global_blacklist(&self, user_id: i64, blacklisted: bool) -> Result<(), String> {
        let mut config = self.config();
        config.global_blacklist.retain(|id| *id != user_id);
        if blacklisted {
            config.global_blacklist.push(user_id);
            config.global_blacklist.sort();
        }
        self.persist(&config)
    }

    /// 清空某个群的全部设置（功能开关与群内黑名单）
    pub fn clear_group_setting(&self, group_id: i64) -> Result<(), String> {
        let mut config = self.config();
        config.group_settings.remove(&group_id.to_string());
        self.persist(&config)
    }

    /// 轮询用的当前状态快照：(配置文件路径, 上次读到的修改时间)
    fn watched(&self) -> (Option<PathBuf>, Option<SystemTime>) {
        let st = self.state.read().unwrap();
        (st.file.clone(), st.last_modified)
    }

    fn note_modified(&self, modified: SystemTime) {
        self.state.write().unwrap().last_modified = Some(modified);
    }
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

/// 清理空的分群设置
fn normalize_group_settings(config: &mut AronaConfig) {
    config
        .group_settings
        .retain(|_, setting| !setting.is_empty());
}

pub fn default_file() -> PathBuf {
    super::arona::default_file()
}

pub fn find_field_index(key: &str) -> Option<usize> {
    FIELDS.iter().position(|f| f.key == key)
}

fn host() -> &'static Settings {
    Framework::global().settings()
}

/// 初始化业务配置并启动热重载监听。
/// 返回 Err 表示 arona.yml 本身有问题（调用方负责提示用户，程序仍会按默认配置继续运行）
pub fn init(path: PathBuf) -> Result<(), String> {
    Framework::global().settings().init(path)
}

/// 当前配置快照
pub fn config() -> AronaConfig {
    host().config()
}

/// 重新从磁盘读取 arona.yml 并同步到运行期配置（热重载路径：失败只记日志、保留当前配置）
pub fn reload() {
    host().reload();
}

/// 机器人被移出群时从 groups 配置中移除该群并写回文件热重载
pub fn remove_group_if_present(group_id: i64) {
    host().remove_group_if_present(group_id);
}

/// 停止热重载监听（优雅关闭时使用）
pub fn close() {
    host().close();
}

/// /config <key> <value>：修改配置并写回文件后重载；show_value=false 时不回显（群聊防泄漏）
pub fn update(key: &str, raw_value: &str, show_value: bool) -> String {
    host().update(key, raw_value, show_value)
}

/// /config <key> add [value]：向列表配置项追加一个值
pub fn add_to_list(key: &str, value: i64, show_value: bool) -> String {
    host().add_to_list(key, value, show_value)
}

/// /config <key> del [value]：从列表配置项移除一个值
pub fn remove_from_list(key: &str, value: i64, show_value: bool) -> String {
    host().remove_from_list(key, value, show_value)
}

/// 配置文件路径
pub fn config_file() -> Option<PathBuf> {
    host().config_file()
}

/// 启用/停用某个群（启用 = 加入 groups，停用 = 移出 groups）
pub fn set_group_enabled(group_id: i64, enabled: bool) -> Result<(), String> {
    host().set_group_enabled(group_id, enabled)
}

/// 开启/关闭某个群的某项功能
pub fn set_group_feature(group_id: i64, feature: &str, enabled: bool) -> Result<(), String> {
    host().set_group_feature(group_id, feature, enabled)
}

/// 一次开启/关闭某个群的多项功能：GUI「功能开关」按插件整组开关时用，多个 key 只落盘一次
pub fn set_group_features(group_id: i64, features: &[&str], enabled: bool) -> Result<(), String> {
    host().set_group_features(group_id, features, enabled)
}

/// 群成员黑名单：加入或移出
pub fn set_group_blacklist(group_id: i64, user_id: i64, blacklisted: bool) -> Result<(), String> {
    host().set_group_blacklist(group_id, user_id, blacklisted)
}

/// 全局启用/停用某个插件（GUI「插件管理」页的总开关）。
/// 写盘后 apply 会调 [`crate::plugin::sync_enabled_state`]：停用即 stop()，启用即补 configure()+start()。
pub fn set_plugin_enabled(plugin: &str, enabled: bool) -> Result<(), String> {
    host().set_plugin_enabled(plugin, enabled)
}

/// 在某个群里启用/停用某个插件（GUI「群管理」页）。只影响路由，不动插件的后台任务。
pub fn set_group_plugin_enabled(group_id: i64, plugin: &str, enabled: bool) -> Result<(), String> {
    host().set_group_plugin_enabled(group_id, plugin, enabled)
}

/// 全局黑名单：加入或移出
pub fn set_global_blacklist(user_id: i64, blacklisted: bool) -> Result<(), String> {
    host().set_global_blacklist(user_id, blacklisted)
}

/// 清空某个群的全部设置（功能开关与群内黑名单）
pub fn clear_group_setting(group_id: i64) -> Result<(), String> {
    host().clear_group_setting(group_id)
}

/// 轮询监听配置文件修改（每 2 秒）：框架的 arona.yml 与各插件的 config/<插件>/arona.yml
fn spawn_watcher(settings: Arc<Settings>) {
    crate::runtime::reactor::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        interval.tick().await; // 首个 tick 立即返回，跳过
        loop {
            interval.tick().await;
            let (path, last) = settings.watched();
            let Some(path) = path else { continue };
            let modified = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            let is_first = last.is_none() && modified.is_some();
            let changed = match (last, modified) {
                (Some(old), Some(new)) => new != old,
                _ => false,
            };
            if changed || is_first {
                if let Some(m) = modified {
                    settings.note_modified(m);
                }
                settings.reload();
            }
            // 插件配置文件同一条链路上顺带检查：省得起第二个轮询任务
            super::plugin_config::poll_changed();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// arona.yml 的绑定与生效都必须跟着实例走。宿主把 `Framework` 交进插件 dll 之后，
    /// 插件侧 `/config` 读的、写的才是宿主真正在用的那份文件——改造前这里是一份进程级
    /// `static`，dll 里另存一份空的，于是插件写配置永远得到「配置文件尚未初始化」。
    #[test]
    fn arona_yml_state_is_scoped_to_the_framework_instance() {
        let host = Framework::new();
        let other = Framework::new();
        let dir = std::env::temp_dir().join(format!("arona-settings-scope-{}", std::process::id()));
        let path = dir.join("arona.yml");
        let _ = std::fs::remove_file(&path);
        host.settings()
            .init(path.clone())
            .expect("测试配置应能加载");
        assert_eq!(
            host.settings().config_file().as_deref(),
            Some(path.as_path())
        );

        // 写入落到**自己这一套**：磁盘、本实例门控都变了，进程默认实例一点没被波及
        // （改造前 apply 是写进 Framework::global() 的，那条断言当时正好反过来）
        let reply = host.settings().add_to_list("managers", 987654321, false);
        assert!(reply.contains("配置已更新"), "{reply}");
        assert!(host.settings().config().managers.contains(&987654321));
        assert!(host.gating().managers().contains(&987654321));
        assert!(
            !Framework::global().gating().managers().contains(&987654321),
            "隔离实例的配置不该漏进进程默认实例"
        );

        // 另一套实例没绑过文件：读默认值、写盘只会被拒绝，不会去动别人的 arona.yml
        assert_eq!(other.settings().config_file(), None);
        assert!(
            other
                .settings()
                .update("managers", "100", true)
                .contains("配置文件尚未初始化")
        );
        assert!(
            other
                .settings()
                .add_to_list("groups", 7, false)
                .contains("配置文件尚未初始化")
        );
        assert_eq!(
            other.settings().set_plugin_enabled("x", false),
            Err("配置文件尚未初始化".to_string())
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
