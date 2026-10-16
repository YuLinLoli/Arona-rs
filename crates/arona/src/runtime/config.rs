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
    /// 全局禁用的功能 key（arona.yml 的 disabled_features）
    pub disabled_features: RwLock<Vec<String>>,
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
            disabled_features: RwLock::new(Vec::new()),
        }
    }
}

/// 门控状态：功能清单（插件在 install 阶段登记）+ 运行期配置。一个实例一套，互不干扰。
#[derive(Default)]
pub struct Gating {
    /// key 用于 arona.yml 的 group_settings.disabled_features
    features: RwLock<Vec<RegisteredFeature>>,
    /// 声明了自己不按群生效的插件 id（小写）。install 阶段由框架从
    /// [`PluginMeta::group_scoped`](crate::plugin::PluginMeta) 记进来：分群停用名单
    /// 对它们一律放行，所以手改 arona.yml 塞一个进去也不会起作用——那一格本该不存在。
    group_exempt: RwLock<Vec<String>>,
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

    /// 功能清单（GUI 展示用）：被整体禁用的插件，它的功能不再列出
    ///
    /// 只是展示层收口，不改任何判定 —— [`Gating::feature_enabled`] 早就让插件级停用连带
    /// 关掉它名下的功能，这里省得在群功能页摆一排永远勾不上的复选框（要恢复请去「插件管理」页
    /// 重新启用该插件）。因此 `features_of` 与 `feature_keys_text` **不做**同样的过滤：
    /// 前者是插件卡片上"本插件提供哪些功能"的清单（卡片自己带 `enabled`），
    /// 后者要写进配置文件模板，停用中也得让人看见 key 叫什么名字。
    pub fn features(&self) -> Vec<Feature> {
        let disabled = self.disabled_plugins();
        self.features
            .read()
            .unwrap()
            .iter()
            .filter(|registered| {
                !disabled
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&registered.plugin))
            })
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

    /// 功能开关默认开启。私聊(无群号)不受**分群**那一层限制，但仍受全局这一层约束。
    ///
    /// 三道判断，写的顺序就是语义：① 这个 key 被全局关掉了没有；② 提供它的插件有没有被
    /// 整体禁用；③ 该群自己再往下减（群功能名单 + 群插件名单，一次持锁读完）。
    /// 群名单只能减、不能翻案：全局关掉的 key 不会因为某个群还留着「启用」就活过来。
    pub fn feature_enabled(&self, group_id: Option<i64>, key: &str) -> bool {
        if key.is_empty() {
            return true;
        }
        if self.feature_disabled_globally(key) {
            return false;
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
        // 先在锁外算好豁免，免得持着分群设置的读锁再去拿豁免表的锁
        let owner_exempt = owner
            .as_deref()
            .is_some_and(|plugin| self.group_exempt(plugin));
        // 功能开关与该群的插件名单在同一次持锁里读完
        self.with_group_setting(group_id, |setting| {
            setting.feature_enabled(key)
                && match &owner {
                    // 主人声明过不按群生效 → 这一档不参与判定，只看法上这个功能开没开
                    Some(_) if owner_exempt => true,
                    Some(plugin) => setting_plugin_enabled(setting, plugin),
                    // 没人登记过的 key（老配置里的残留）不做插件判断，按功能开关结论放行
                    None => true,
                }
        })
        .unwrap_or(true)
    }

    /// 插件是否全局启用（不在 arona.yml 的 disabled_plugins 里就是启用）
    pub fn plugin_enabled(&self, plugin: &str) -> bool {
        let disabled = self.config.disabled_plugins.read().unwrap();
        !disabled
            .iter()
            .any(|name| name.eq_ignore_ascii_case(plugin))
    }

    /// 记下插件自己声明的分群作用域（install 阶段由框架写入）。
    /// `group_scoped = false` 的插件进豁免表：分群名单再也管不到它。
    pub fn set_group_scoped(&self, plugin: &str, group_scoped: bool) {
        let name = plugin.to_ascii_lowercase();
        let mut exempt = self.group_exempt.write().unwrap();
        exempt.retain(|item| *item != name);
        if !group_scoped {
            exempt.push(name);
        }
    }

    /// 该插件是否声明过"不按群生效"
    pub fn group_exempt(&self, plugin: &str) -> bool {
        let name = plugin.to_ascii_lowercase();
        self.group_exempt
            .read()
            .unwrap()
            .iter()
            .any(|item| *item == name)
    }

    /// 插件在某个群里是否启用（私聊无群号，一律按启用处理，与 [`Gating::feature_enabled`] 一致）
    pub fn plugin_enabled_in_group(&self, plugin: &str, group_id: Option<i64>) -> bool {
        let Some(group_id) = group_id else {
            return true;
        };
        // 声明过不按群生效的插件不看分群名单：那一格开关在 GUI 里已经不给它画了，
        // 老配置里残留的条目也不该继续管事
        if self.group_exempt(plugin) {
            return true;
        }
        self.with_group_setting(group_id, |setting| setting_plugin_enabled(setting, plugin))
            .unwrap_or(true)
    }

    /// 用户是否被拉黑（管理员不参与判断，由调用方保证）
    pub fn is_blacklisted(&self, user_id: i64, group_id: Option<i64>) -> bool {
        if self
            .config
            .global_blacklist
            .read()
            .unwrap()
            .contains(&user_id)
        {
            return true;
        }
        match group_id {
            Some(group_id) => self
                .with_group_setting(group_id, |setting| setting.blacklist.contains(&user_id))
                .unwrap_or(false),
            None => false,
        }
    }

    /// 该群在不在授权名单里（名单为空 = 所有群都授权）；私聊没有群号，一律放行
    pub fn group_authorized(&self, group_id: Option<i64>) -> bool {
        let Some(group_id) = group_id else {
            return true;
        };
        let groups = self.config.groups.read().unwrap();
        groups.is_empty() || groups.contains(&group_id)
    }

    /// 持锁**就地**读某个群的设置：这几道判断在热路径上（每条事件、每个钩子都会问一遍），
    /// 不该为此把整张 `group_settings` 克隆出来
    fn with_group_setting<T>(
        &self,
        group_id: i64,
        read: impl FnOnce(&GroupSetting) -> T,
    ) -> Option<T> {
        let settings = self.config.group_settings.read().unwrap();
        settings.get(&group_id.to_string()).map(read)
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
        let settings = self.config.group_settings.read().unwrap();
        settings
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

    /// 某个功能 key 是否被全局关掉（只看这一层，不看归属插件）
    ///
    /// key 比对是**精确**的，与分群那一份同口径；插件 id 的名单才忽略大小写。
    pub fn feature_disabled_globally(&self, key: &str) -> bool {
        self.config
            .disabled_features
            .read()
            .unwrap()
            .iter()
            .any(|item| item == key)
    }

    /// 全局禁用的功能 key 列表（arona.yml 的 disabled_features）
    pub fn disabled_features(&self) -> Vec<String> {
        self.config.disabled_features.read().unwrap().clone()
    }

    pub fn set_disabled_features(&self, features: Vec<String>) {
        *self.config.disabled_features.write().unwrap() = features;
    }
}

/// 某份群设置里，这个插件是不是启用的（名单比对忽略大小写）
fn setting_plugin_enabled(setting: &GroupSetting, plugin: &str) -> bool {
    !setting
        .disabled_plugins
        .iter()
        .any(|name| name.eq_ignore_ascii_case(plugin))
}

fn gating() -> &'static Gating {
    Framework::global().gating()
}

/// 只给测试用的串行闸门：`send_image_as_file` 这类值仍是进程默认实例上的一份真值，
/// 而 `OneBotApplication::new` 会按自己那份 config 覆写它（application.rs 里那一步）。
/// 单测并行跑时，"我刚设成 true" 与"别处的用例正在建 app" 会交错，断言就偶发红。
/// 会碰这份进程状态的用例共拿这一把锁；中毒也要继续跑，否则一个用例失败会级联。
#[cfg(test)]
pub(crate) fn process_state_lock() -> std::sync::MutexGuard<'static, ()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 登记一个可分群开关的功能（key 重复时忽略后来的，保留首个的展示信息）
pub fn register_feature(feature: Feature, plugin: &str) {
    gating().register_feature(feature, plugin);
}

/// 功能清单（GUI 展示用）：被全局停用的插件不在此列，判定请用 [`feature_enabled`]
///
/// 想知道"注册过哪些功能"（包括停用中的）请用 [`features_of`] 或 [`feature_keys_text`]。
pub fn features() -> Vec<Feature> {
    gating().features()
}

/// 某个插件提供的功能清单（GUI「插件管理」页展示，停用中也照常列出）
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

/// 群功能开关默认开启；私聊(无群号)不受分群开关限制，但受全局那一层约束
pub fn feature_enabled(group_id: Option<i64>, key: &str) -> bool {
    gating().feature_enabled(group_id, key)
}

/// 某个功能 key 是否被全局关掉（`feature_enabled` 的第一道，不看归属插件）
pub fn feature_disabled_globally(key: &str) -> bool {
    gating().feature_disabled_globally(key)
}

/// 全局禁用的功能 key 列表（arona.yml 的 disabled_features）
pub fn disabled_features() -> Vec<String> {
    gating().disabled_features()
}

/// 插件是否全局启用（不在 arona.yml 的 disabled_plugins 里就是启用）
pub fn plugin_enabled(plugin: &str) -> bool {
    gating().plugin_enabled(plugin)
}

/// 插件在某个群里是否启用（私聊无群号，一律按启用处理，与 [`feature_enabled`] 一致）
pub fn plugin_enabled_in_group(plugin: &str, group_id: Option<i64>) -> bool {
    gating().plugin_enabled_in_group(plugin, group_id)
}

/// 该插件是否声明过"不按群生效"（`PluginMeta::group_scoped = false`）
pub fn group_exempt(plugin: &str) -> bool {
    gating().group_exempt(plugin)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 门控改成「持锁就地读」之后，判断结果必须和整体克隆的版本一致：
    /// 这几道是每条事件、每个钩子都要过的，改坏了就是全线误放行或误杀
    #[test]
    fn group_gating_reads_in_place() {
        let gating = Gating::default();
        gating.register_feature(
            Feature {
                key: "gacha",
                name: "抽卡",
                description: "",
            },
            "hello",
        );
        let mut settings = BTreeMap::new();
        settings.insert(
            "100".to_string(),
            GroupSetting {
                disabled_features: vec!["gacha".to_string()],
                blacklist: vec![7],
                disabled_plugins: vec!["other".to_string()],
            },
        );
        gating.set_group_settings(settings);

        // 该群关掉的 key 不放行，没登记过的残留 key 不做插件判断
        assert!(!gating.feature_enabled(Some(100), "gacha"));
        assert!(gating.feature_enabled(Some(100), "leftover"));
        // 私聊没有群号，一律不受分群开关限制
        assert!(gating.feature_enabled(None, "gacha"));
        // 群内插件名单（比对忽略大小写），没设置的群一律放行
        assert!(!gating.plugin_enabled_in_group("Other", Some(100)));
        assert!(gating.plugin_enabled_in_group("hello", Some(100)));
        assert!(gating.plugin_enabled_in_group("other", Some(200)));
        assert!(gating.feature_enabled(Some(200), "gacha"));
        // 声明过自己不按群生效（纯私聊那一类）：分群停用名单再也管不到它，比对也忽略大小写
        gating.set_group_scoped("other", false);
        assert!(
            gating.plugin_enabled_in_group("Other", Some(100)),
            "豁免之后，100 群那份名单不该再挡住它"
        );
        // 功能那一档跟着主人一起豁免：只剩功能名单自己管事，200 群没关过 gacha
        gating.set_group_scoped("hello", false);
        assert!(gating.feature_enabled(Some(200), "gacha"));
        // 改回按群生效，名单立刻重新管事——豁免是登记阶段按上报值写的，不是一次进门永久免检
        gating.set_group_scoped("other", true);
        assert!(!gating.plugin_enabled_in_group("other", Some(100)));
        // 群内黑名单只在那个群里成立
        assert!(gating.is_blacklisted(7, Some(100)));
        assert!(!gating.is_blacklisted(7, Some(200)));
        gating.set_global_blacklist(vec![7]);
        assert!(gating.is_blacklisted(7, None));
        // 全局停用插件连带它名下的功能（名单比对同样忽略大小写）
        gating.set_disabled_plugins(vec!["HELLO".to_string()]);
        assert!(!gating.plugin_enabled("hello"));
        assert!(!gating.feature_enabled(Some(200), "gacha"));
        // 取单群设置不必克隆整张表
        assert_eq!(
            gating.group_setting(100).disabled_features,
            vec!["gacha".to_string()]
        );
        assert!(gating.group_setting(200).is_empty());
    }

    /// 全局功能开关是**主开关**：关掉之后所有群加私聊一起不响应，而群里没把这个 key 列进
    /// 名单（等于"启用着"）也不能翻案。方向写反就是全线漏放或全线误杀，所以正反两头都钉。
    #[test]
    fn the_global_feature_switch_applies_everywhere_and_groups_cannot_override_it() {
        let gating = Gating::default();
        gating.register_feature(
            Feature {
                key: "gacha",
                name: "抽卡",
                description: "",
            },
            "hello",
        );
        let mut settings = BTreeMap::new();
        // 100 什么都没关；200 自己在群名单里关了 gacha
        settings.insert("100".to_string(), GroupSetting::default());
        settings.insert(
            "200".to_string(),
            GroupSetting {
                disabled_features: vec!["gacha".to_string()],
                ..Default::default()
            },
        );
        gating.set_group_settings(settings);
        assert!(gating.feature_enabled(Some(100), "gacha"));
        assert!(gating.feature_enabled(None, "gacha"));

        gating.set_disabled_features(vec!["gacha".to_string()]);
        assert!(gating.feature_disabled_globally("gacha"));
        // 群 100 一条减法都没写，也照样停：这一层不分群
        assert!(!gating.feature_enabled(Some(100), "gacha"));
        // 私聊以前完全不看分群名单，现在归全局这一层管
        assert!(!gating.feature_enabled(None, "gacha"));
        // 没上名单的 key 不受牵连
        assert!(gating.feature_enabled(Some(100), "name"));

        // 恢复之后回到原状态：群 200 自己的那份减法仍然有效
        gating.set_disabled_features(Vec::new());
        assert!(gating.feature_enabled(Some(100), "gacha"));
        assert!(!gating.feature_enabled(Some(200), "gacha"));
    }

    /// 授权名单为空 = 所有群都放行；非空时只认名单里的群
    #[test]
    fn group_authorized_follows_the_allowlist() {
        let gating = Gating::default();
        assert!(gating.group_authorized(Some(100)));
        assert!(gating.group_authorized(None));
        gating.set_groups(vec![100]);
        assert!(gating.group_authorized(Some(100)));
        assert!(!gating.group_authorized(Some(200)));
        assert!(gating.group_authorized(None));
    }

    /// 功能清单只在「展示用」的那一份上过滤停用插件：另两处（插件卡片、配置文件模板）
    /// 停用中也要报出 key，否则人就再也不知道这个功能叫什么名字了
    #[test]
    fn features_display_skips_disabled_plugins() {
        let gating = Gating::default();
        gating.register_feature(
            Feature {
                key: "gacha",
                name: "抽卡",
                description: "",
            },
            "hello",
        );
        gating.register_feature(
            Feature {
                key: "whoami",
                name: "谁叫",
                description: "",
            },
            "who-is-arona",
        );
        assert_eq!(gating.features().len(), 2);
        gating.set_disabled_plugins(vec!["Hello".to_string()]);
        assert_eq!(
            gating
                .features()
                .iter()
                .map(|feature| feature.key)
                .collect::<Vec<&str>>(),
            vec!["whoami"]
        );
        assert_eq!(gating.features_of("hello").len(), 1);
        assert!(gating.feature_keys_text().contains("gacha"));
    }
}
