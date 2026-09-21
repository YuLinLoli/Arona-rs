//! 运行期门控状态（对应原版 RuntimeConfig）：功能清单 + 群授权/黑名单/停用名单。
//!
//! 状态住在 [`Gating`] 实例里，由 [`crate::framework::Framework`] 持有；本模块的自由函数
//! 是「进程默认实例」的转发层（GUI 与管理面板沿用它们）。
//! 测试要隔离时拿 `Framework::new()`，不必再和别的用例抢同一份全局名单。
use crate::config::arona::GroupSetting;
use crate::framework::Framework;
use std::collections::BTreeMap;
use std::sync::RwLock;

/// 可被分群开关控制的功能项（GUI 与配置模板共用同一份清单）
#[derive(Clone, Debug)]
pub struct Feature {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

/// 功能项 + 提供它的插件名。「插件级开关」（全局与分群）靠这个归属关系生效：
/// 禁用插件 = 它名下所有功能一起停掉，不需要插件自己配合。
struct RegisteredFeature {
    feature: Feature,
    plugin: String,
}

pub struct RuntimeConfig {
    /// 允许响应的群号列表，留空表示响应所有群
    pub groups: RwLock<Vec<i64>>,
    /// 管理员 QQ 号列表
    pub managers: RwLock<Vec<i64>>,
    /// 机器人 QQ
    pub bot_id: RwLock<i64>,
    /// 名称后缀（独立模式固定为“老师”）
    pub end_with_sensei: RwLock<String>,
    /// arona 云端鉴权用 UUID（对应原版 RuntimeConfig.uuid，独立模式默认空字符串）
    pub uuid: RwLock<String>,
    /// 全局用户黑名单
    pub global_blacklist: RwLock<Vec<i64>>,
    /// 本地图片改用 file:// 路径直传 OneBot 实现
    pub send_image_as_file: RwLock<bool>,
    /// 分群设置（群号 -> 功能开关/插件开关/群内黑名单）
    pub group_settings: RwLock<BTreeMap<String, GroupSetting>>,
    /// 全局禁用的插件名（arona.yml 的 disabled_plugins）
    pub disabled_plugins: RwLock<Vec<String>>,
}

impl Default for RuntimeConfig {
    fn default() -> RuntimeConfig {
        RuntimeConfig {
            groups: RwLock::new(Vec::new()),
            managers: RwLock::new(Vec::new()),
            bot_id: RwLock::new(0),
            end_with_sensei: RwLock::new(String::from("老师")),
            uuid: RwLock::new(String::new()),
            global_blacklist: RwLock::new(Vec::new()),
            send_image_as_file: RwLock::new(false),
            group_settings: RwLock::new(BTreeMap::new()),
            disabled_plugins: RwLock::new(Vec::new()),
        }
    }
}

/// 门控状态：功能清单（插件在 install 阶段登记）+ 运行期配置。一个实例一套，互不干扰。
#[derive(Default)]
pub struct Gating {
    /// key 用于 arona.yml 的 group_settings.disabled_features
    features: RwLock<Vec<RegisteredFeature>>,
    config: RuntimeConfig,
}

impl Gating {
    /// 登记一个可分群开关的功能（key 重复时忽略后来的，保留首个的展示信息）
    pub fn register_feature(&self, feature: Feature, plugin: &str) {
        let mut features = self.features.write().unwrap();
        if !features.iter().any(|f| f.feature.key == feature.key) {
            features.push(RegisteredFeature {
                feature,
                plugin: plugin.to_string(),
            });
        }
    }

    /// 功能清单（GUI 展示用）
    pub fn features(&self) -> Vec<Feature> {
        self.features
            .read()
            .unwrap()
            .iter()
            .map(|registered| registered.feature.clone())
            .collect()
    }

    /// 某个插件提供的功能清单（GUI「插件管理」页展示）
    pub fn features_of(&self, plugin: &str) -> Vec<Feature> {
        self.features
            .read()
            .unwrap()
            .iter()
            .filter(|registered| registered.plugin == plugin)
            .map(|registered| registered.feature.clone())
            .collect()
    }

    /// 功能 key -> 提供它的插件名
    pub fn feature_owner(&self, key: &str) -> Option<String> {
        self.features
            .read()
            .unwrap()
            .iter()
            .find(|registered| registered.feature.key == key)
            .map(|registered| registered.plugin.clone())
    }

    /// 全部功能归属的插件名（去重，保持登记顺序）
    pub fn feature_plugins(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for registered in self.features.read().unwrap().iter() {
            if !names.contains(&registered.plugin) {
                names.push(registered.plugin.clone());
            }
        }
        names
    }

    /// 功能 key 列表文本，如 "gacha, name, tarot"
    pub fn feature_keys_text(&self) -> String {
        self.features
            .read()
            .unwrap()
            .iter()
            .map(|registered| registered.feature.key)
            .collect::<Vec<&str>>()
            .join(", ")
    }

    /// 群功能开关默认开启；私聊(无群号)不受分群开关限制。
    ///
    /// 两道判断：提供这个功能的插件有没有被整体禁用（全局 + 该群），以及该群有没有单独关掉这个 key。
    pub fn feature_enabled(&self, group_id: Option<i64>, key: &str) -> bool {
        if key.is_empty() {
            return true;
        }
        let owner = self.feature_owner(key);
        if let Some(plugin) = &owner {
            if !self.plugin_enabled(plugin) {
                return false;
            }
        }
        let Some(group_id) = group_id else {
            return true;
        };
        match self.group_settings().get(&group_id.to_string()) {
            None => true,
            Some(setting) => {
                setting.feature_enabled(key)
                    && match &owner {
                        Some(plugin) => self.plugin_enabled_in_group(plugin, Some(group_id)),
                        // 没人登记过的 key（老配置里的残留）不做插件判断，按功能开关结论放行
                        None => true,
                    }
            }
        }
    }

    /// 插件是否全局启用（不在 arona.yml 的 disabled_plugins 里就是启用）
    pub fn plugin_enabled(&self, plugin: &str) -> bool {
        !self
            .disabled_plugins()
            .iter()
            .any(|name| name.eq_ignore_ascii_case(plugin))
    }

    /// 插件在某个群里是否启用（私聊无群号，一律按启用处理，与 [`Gating::feature_enabled`] 一致）
    pub fn plugin_enabled_in_group(&self, plugin: &str, group_id: Option<i64>) -> bool {
        let Some(group_id) = group_id else {
            return true;
        };
        match self.group_settings().get(&group_id.to_string()) {
            Some(setting) => !setting
                .disabled_plugins
                .iter()
                .any(|name| name.eq_ignore_ascii_case(plugin)),
            None => true,
        }
    }

    /// 用户是否被拉黑（管理员不参与判断，由调用方保证）
    pub fn is_blacklisted(&self, user_id: i64, group_id: Option<i64>) -> bool {
        if self.global_blacklist().contains(&user_id) {
            return true;
        }
        match group_id {
            Some(group_id) => match self.group_settings().get(&group_id.to_string()) {
                Some(setting) => setting.blacklist.contains(&user_id),
                None => false,
            },
            None => false,
        }
    }

    pub fn set_global_blacklist(&self, users: Vec<i64>) {
        *self.config.global_blacklist.write().unwrap() = users;
    }

    pub fn global_blacklist(&self) -> Vec<i64> {
        self.config.global_blacklist.read().unwrap().clone()
    }

    pub fn set_group_settings(&self, settings: BTreeMap<String, GroupSetting>) {
        *self.config.group_settings.write().unwrap() = settings;
    }

    pub fn group_settings(&self) -> BTreeMap<String, GroupSetting> {
        self.config.group_settings.read().unwrap().clone()
    }

    /// 取某个群的设置（不存在则返回默认值）
    pub fn group_setting(&self, group_id: i64) -> GroupSetting {
        self.group_settings()
            .get(&group_id.to_string())
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_send_image_as_file(&self, enabled: bool) {
        *self.config.send_image_as_file.write().unwrap() = enabled;
    }

    pub fn send_image_as_file(&self) -> bool {
        *self.config.send_image_as_file.read().unwrap()
    }

    pub fn set_groups(&self, groups: Vec<i64>) {
        *self.config.groups.write().unwrap() = groups;
    }

    pub fn groups(&self) -> Vec<i64> {
        self.config.groups.read().unwrap().clone()
    }

    pub fn set_managers(&self, managers: Vec<i64>) {
        *self.config.managers.write().unwrap() = managers;
    }

    pub fn managers(&self) -> Vec<i64> {
        self.config.managers.read().unwrap().clone()
    }

    pub fn is_manager(&self, user_id: i64) -> bool {
        self.config.managers.read().unwrap().contains(&user_id)
    }

    pub fn set_bot_id(&self, id: i64) {
        *self.config.bot_id.write().unwrap() = id;
    }

    pub fn bot_id(&self) -> i64 {
        *self.config.bot_id.read().unwrap()
    }

    pub fn end_with_sensei(&self) -> String {
        self.config.end_with_sensei.read().unwrap().clone()
    }

    pub fn set_end_with_sensei(&self, value: String) {
        *self.config.end_with_sensei.write().unwrap() = value;
    }

    pub fn uuid(&self) -> String {
        self.config.uuid.read().unwrap().clone()
    }

    pub fn set_uuid(&self, value: String) {
        *self.config.uuid.write().unwrap() = value;
    }

    /// 全局禁用的插件名列表（arona.yml 的 disabled_plugins）
    pub fn disabled_plugins(&self) -> Vec<String> {
        self.config.disabled_plugins.read().unwrap().clone()
    }

    pub fn set_disabled_plugins(&self, plugins: Vec<String>) {
        *self.config.disabled_plugins.write().unwrap() = plugins;
    }
}

fn gating() -> &'static Gating {
    Framework::global().gating()
}

/// 登记一个可分群开关的功能（key 重复时忽略后来的，保留首个的展示信息）
pub fn register_feature(feature: Feature, plugin: &str) {
    gating().register_feature(feature, plugin);
}

/// 功能清单（GUI 展示用）
pub fn features() -> Vec<Feature> {
    gating().features()
}

/// 某个插件提供的功能清单（GUI「插件管理」页展示）
pub fn features_of(plugin: &str) -> Vec<Feature> {
    gating().features_of(plugin)
}

/// 功能 key -> 提供它的插件名
pub fn feature_owner(key: &str) -> Option<String> {
    gating().feature_owner(key)
}

/// 全部功能归属的插件名（去重，保持登记顺序）
pub fn feature_plugins() -> Vec<String> {
    gating().feature_plugins()
}

/// 功能 key 列表文本，如 "gacha, name, tarot"
pub fn feature_keys_text() -> String {
    gating().feature_keys_text()
}

/// 群功能开关默认开启；私聊(无群号)不受分群开关限制
pub fn feature_enabled(group_id: Option<i64>, key: &str) -> bool {
    gating().feature_enabled(group_id, key)
}

/// 插件是否全局启用（不在 arona.yml 的 disabled_plugins 里就是启用）
pub fn plugin_enabled(plugin: &str) -> bool {
    gating().plugin_enabled(plugin)
}

/// 插件在某个群里是否启用（私聊无群号，一律按启用处理，与 [`feature_enabled`] 一致）
pub fn plugin_enabled_in_group(plugin: &str, group_id: Option<i64>) -> bool {
    gating().plugin_enabled_in_group(plugin, group_id)
}

/// 用户是否被拉黑（管理员不参与判断，由调用方保证）
pub fn is_blacklisted(user_id: i64, group_id: Option<i64>) -> bool {
    gating().is_blacklisted(user_id, group_id)
}

pub fn set_global_blacklist(users: Vec<i64>) {
    gating().set_global_blacklist(users);
}

pub fn global_blacklist() -> Vec<i64> {
    gating().global_blacklist()
}

pub fn set_group_settings(settings: BTreeMap<String, GroupSetting>) {
    gating().set_group_settings(settings);
}

pub fn set_send_image_as_file(enabled: bool) {
    gating().set_send_image_as_file(enabled);
}

pub fn send_image_as_file() -> bool {
    gating().send_image_as_file()
}

pub fn group_settings() -> BTreeMap<String, GroupSetting> {
    gating().group_settings()
}

/// 取某个群的设置（不存在则返回默认值）
pub fn group_setting(group_id: i64) -> GroupSetting {
    gating().group_setting(group_id)
}

pub fn set_groups(groups: Vec<i64>) {
    gating().set_groups(groups);
}

pub fn set_managers(managers: Vec<i64>) {
    gating().set_managers(managers);
}

pub fn groups() -> Vec<i64> {
    gating().groups()
}

pub fn managers() -> Vec<i64> {
    gating().managers()
}

pub fn is_manager(user_id: i64) -> bool {
    gating().is_manager(user_id)
}

pub fn set_bot_id(id: i64) {
    gating().set_bot_id(id);
}

pub fn bot_id() -> i64 {
    gating().bot_id()
}

pub fn end_with_sensei() -> String {
    gating().end_with_sensei()
}

pub fn set_end_with_sensei(value: String) {
    gating().set_end_with_sensei(value);
}

pub fn uuid() -> String {
    gating().uuid()
}

pub fn set_uuid(value: String) {
    gating().set_uuid(value);
}

/// 全局禁用的插件名列表（arona.yml 的 disabled_plugins）
pub fn disabled_plugins() -> Vec<String> {
    gating().disabled_plugins()
}

pub fn set_disabled_plugins(plugins: Vec<String>) {
    gating().set_disabled_plugins(plugins);
}
