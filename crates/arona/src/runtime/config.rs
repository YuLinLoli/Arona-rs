//! 运行期全局配置（对应原版 RuntimeConfig）
use crate::config::arona::GroupSetting;
use once_cell::sync::OnceCell;
use std::collections::BTreeMap;
use std::sync::RwLock;

/// 可被分群开关控制的功能项（GUI 与配置模板共用同一份清单）
#[derive(Clone)]
pub struct Feature {
    pub key: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

/// 功能清单：由插件在 install 阶段通过 `register_feature` 动态登记。
/// key 用于 arona.yml 的 group_settings.disabled_features。
static FEATURES: RwLock<Vec<Feature>> = RwLock::new(Vec::new());

/// 登记一个可分群开关的功能（key 重复时忽略后来的，保留首个的展示信息）
pub fn register_feature(feature: Feature) {
    let mut features = FEATURES.write().unwrap();
    if !features.iter().any(|f| f.key == feature.key) {
        features.push(feature);
    }
}

/// 功能清单（GUI 展示用）
pub fn features() -> Vec<Feature> {
    FEATURES.read().unwrap().clone()
}

/// 功能 key 列表文本，如 "gacha, name, tarot"
pub fn feature_keys_text() -> String {
    FEATURES
        .read()
        .unwrap()
        .iter()
        .map(|feature| feature.key)
        .collect::<Vec<&str>>()
        .join(", ")
}

/// 群功能开关默认开启；私聊(无群号)不受分群开关限制
pub fn feature_enabled(group_id: Option<i64>, key: &str) -> bool {
    if key.is_empty() {
        return true;
    }
    let Some(group_id) = group_id else {
        return true;
    };
    match group_settings().get(&group_id.to_string()) {
        Some(setting) => setting.feature_enabled(key),
        None => true,
    }
}

/// 用户是否被拉黑（管理员不参与判断，由调用方保证）
pub fn is_blacklisted(user_id: i64, group_id: Option<i64>) -> bool {
    if global_blacklist().contains(&user_id) {
        return true;
    }
    match group_id {
        Some(group_id) => match group_settings().get(&group_id.to_string()) {
            Some(setting) => setting.blacklist.contains(&user_id),
            None => false,
        },
        None => false,
    }
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
    /// 分群设置（群号 -> 功能开关/群内黑名单）
    pub group_settings: RwLock<BTreeMap<String, GroupSetting>>,
}

static CONFIG: OnceCell<RuntimeConfig> = OnceCell::new();

fn instance() -> &'static RuntimeConfig {
    CONFIG.get_or_init(|| RuntimeConfig {
        groups: RwLock::new(Vec::new()),
        managers: RwLock::new(Vec::new()),
        bot_id: RwLock::new(0),
        end_with_sensei: RwLock::new(String::from("老师")),
        uuid: RwLock::new(String::new()),
        global_blacklist: RwLock::new(Vec::new()),
        send_image_as_file: RwLock::new(false),
        group_settings: RwLock::new(BTreeMap::new()),
    })
}

pub fn set_global_blacklist(users: Vec<i64>) {
    *instance().global_blacklist.write().unwrap() = users;
}

pub fn global_blacklist() -> Vec<i64> {
    instance().global_blacklist.read().unwrap().clone()
}

pub fn set_group_settings(settings: BTreeMap<String, GroupSetting>) {
    *instance().group_settings.write().unwrap() = settings;
}

pub fn set_send_image_as_file(enabled: bool) {
    *instance().send_image_as_file.write().unwrap() = enabled;
}

pub fn send_image_as_file() -> bool {
    *instance().send_image_as_file.read().unwrap()
}

pub fn group_settings() -> BTreeMap<String, GroupSetting> {
    instance().group_settings.read().unwrap().clone()
}

/// 取某个群的设置（不存在则返回默认值）
pub fn group_setting(group_id: i64) -> GroupSetting {
    group_settings()
        .get(&group_id.to_string())
        .cloned()
        .unwrap_or_default()
}

pub fn set_groups(groups: Vec<i64>) {
    *instance().groups.write().unwrap() = groups;
}

pub fn set_managers(managers: Vec<i64>) {
    *instance().managers.write().unwrap() = managers;
}

pub fn groups() -> Vec<i64> {
    instance().groups.read().unwrap().clone()
}

pub fn managers() -> Vec<i64> {
    instance().managers.read().unwrap().clone()
}

pub fn is_manager(user_id: i64) -> bool {
    instance().managers.read().unwrap().contains(&user_id)
}

pub fn set_bot_id(id: i64) {
    *instance().bot_id.write().unwrap() = id;
}

pub fn bot_id() -> i64 {
    *instance().bot_id.read().unwrap()
}

pub fn end_with_sensei() -> String {
    instance().end_with_sensei.read().unwrap().clone()
}

pub fn set_end_with_sensei(value: String) {
    *instance().end_with_sensei.write().unwrap() = value;
}

pub fn uuid() -> String {
    instance().uuid.read().unwrap().clone()
}

pub fn set_uuid(value: String) {
    *instance().uuid.write().unwrap() = value;
}
