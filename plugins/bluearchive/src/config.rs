//! 碧蓝档案插件自持有的业务配置（notify 每日推送 / trainer 攻略）。
//!
//! 这两块配置住在框架给本插件划的配置文件 `config/bluearchive/arona.yml` 里（顶层键
//! `notify` / `trainer`），框架不理解其内容：插件只写 serde 结构 + 实现
//! [`arona::config::arona::PluginConfig`] 给出字段注释，[`register_sections`] 把它们登记
//! 给框架，带注释模板的生成、原样 YAML 的加载与文件改动后的热重载全部由框架完成。
//! 运行期用 [`notify`] / [`trainer`] 读取，`/config` 改这两区时用 [`set_notify`] 写回。

use arona::config::arona::{PluginConfig, typed_section};
use arona::config::plugin_config::ConfigEntry;
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}
fn default_hour() -> i32 {
    8
}
fn default_notify_text() -> String {
    "碧蓝档案预警".to_string()
}

/// 每日活动推送配置（原框架 config/arona.rs 迁入，属插件私有语义）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default = "default_hour")]
    #[serde(rename = "every_day_hour")]
    pub every_day_hour: i32,
    #[serde(default = "default_true")]
    pub jp: bool,
    #[serde(default = "default_true")]
    pub global: bool,
    #[serde(default = "default_true")]
    pub cn: bool,
    #[serde(default)]
    #[serde(rename = "black_groups")]
    pub black_groups: Vec<i64>,
    #[serde(default = "default_notify_text")]
    #[serde(rename = "notify_text")]
    pub notify_text: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        NotifyConfig {
            enable: true,
            every_day_hour: 8,
            jp: true,
            global: true,
            cn: true,
            black_groups: Vec::new(),
            notify_text: default_notify_text(),
        }
    }
}

/// /攻略 指令别名覆盖项（对应原版 entity/TrainerOverride）
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrainerOverride {
    /// IMAGE(本地图片路径) / RAW(云端图片别名) / CODE(CQ 码原文)
    #[serde(rename = "type", default)]
    pub override_type: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
}

/// /攻略 指令配置（对应原版 config/AronaTrainerConfig）
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TrainerConfig {
    /// 找不到精确匹配时是否提示模糊搜索结果
    pub tip_when_null: bool,
    /// 模糊搜索结果撤回时间(秒), 0 表示不撤回
    pub tip_revoke_time: i64,
    /// 等待用户回复数字选择的时间(秒), 0 表示关闭数字回复
    pub tip_response_wait_time: i64,
    /// 覆盖 /攻略 行为
    pub r#override: Vec<TrainerOverride>,
}

impl Default for TrainerConfig {
    fn default() -> Self {
        TrainerConfig {
            tip_when_null: true,
            tip_revoke_time: 10,
            tip_response_wait_time: 10,
            r#override: Vec::new(),
        }
    }
}

/// 独立的 trainer_config.yml（对应原版 TrainerCommand.TrainerFileConfig）
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrainerFileConfig {
    #[serde(default, rename = "override")]
    pub overrides: Vec<TrainerOverride>,
}

/// notify 配置项：框架按 (插件 id, 顶层键) 定位到本插件的配置文件
fn notify_entry() -> ConfigEntry<NotifyConfig> {
    ConfigEntry::new(crate::PLUGIN_ID, "notify")
}

/// trainer 配置项
fn trainer_entry() -> ConfigEntry<TrainerConfig> {
    ConfigEntry::new(crate::PLUGIN_ID, "trainer")
}

/// 读 notify 配置（缺失或格式错时回退默认值）
pub fn notify() -> NotifyConfig {
    notify_entry().get()
}

/// 读 trainer 配置（缺失或格式错时回退默认值）
pub fn trainer() -> TrainerConfig {
    trainer_entry().get()
}

/// 把修改后的 notify 配置写回 config/bluearchive/arona.yml（触发插件配置热重载）
pub fn set_notify(config: &NotifyConfig) -> Result<(), String> {
    notify_entry().set(config)
}

impl PluginConfig for NotifyConfig {
    const TITLE: &'static str = "每日活动推送";
    const DOC: &'static str = "每天 every_day_hour 点向目标群推送国服/国际服/日服活动日历";

    fn comment(path: &str) -> Option<&'static str> {
        Some(match path {
            "enable" => "是否启用每日活动防侠推送",
            "every_day_hour" => "每日推送的小时(0-23)",
            "jp" => "是否推送日服活动",
            "global" => "是否推送国际服活动",
            "cn" => "是否推送国服活动",
            "black_groups" => "不推送的群号列表（黑名单），留空表示推送到全部允许的群",
            "notify_text" => "推送消息开头文字",
            _ => return None,
        })
    }
}

impl PluginConfig for TrainerConfig {
    const TITLE: &'static str = "攻略(/攻略)";

    fn comment(path: &str) -> Option<&'static str> {
        Some(match path {
            "tip_when_null" => "找不到精确匹配时是否提示模糊搜索结果",
            "tip_revoke_time" => "模糊搜索结果撤回时间(秒), 0 表示不撤回",
            "tip_response_wait_time" => "等待用户回复数字选择的时间(秒), 0 表示关闭数字回复",
            "override" => {
                "覆盖 /攻略 行为: type=IMAGE(本地图片路径)/RAW(云端图片别名)/CODE(CQ 码原文)"
            }
            _ => return None,
        })
    }
}

/// install 阶段登记本插件的两块配置区：框架据此在 config/bluearchive/arona.yml 里
/// 按这里的顺序生成带注释模板（键名撞车时框架保留先登记的那家）
pub fn register_sections(framework: &arona::framework::Framework) {
    framework
        .sections()
        .register(crate::PLUGIN_ID, typed_section::<NotifyConfig>("notify"));
    framework
        .sections()
        .register(crate::PLUGIN_ID, typed_section::<TrainerConfig>("trainer"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一套**独立的框架实例**给用例用：配置区登记表、插件配置文件表都是它自己的，
    /// 再按 `plugin_id` 登记本插件的两块配置区。
    /// 插件配置按 `config/<插件>/arona.yml` 分家，所以不同 `plugin_id` 的用例连磁盘都不共用，
    /// 可以并行跑——不必为进程默认实例串行列。
    fn isolated(plugin_id: &'static str) -> std::sync::Arc<arona::framework::Framework> {
        let framework = arona::framework::Framework::new();
        framework
            .sections()
            .register(plugin_id, typed_section::<NotifyConfig>("notify"));
        framework
            .sections()
            .register(plugin_id, typed_section::<TrainerConfig>("trainer"));
        framework
    }

    /// 端到端：notify 作为插件自持有配置区住在 config/<插件>/arona.yml。
    /// 读 → 改 → 写回 → 再读；框架的通用项（groups/managers）不会混进这个文件，
    /// 没写过的 trainer 默认块照常补齐渲染出来。
    #[test]
    fn notify_roundtrips_through_plugin_config_file() {
        const PLUGIN: &str = "bluearchive-cfg-roundtrip";
        let framework = isolated(PLUGIN);
        let store = framework.configs();
        let file = store.config_file(PLUGIN);
        std::fs::create_dir_all(file.parent().expect("插件配置文件应有父目录")).unwrap();
        let _ = std::fs::remove_file(&file);
        std::fs::write(
            &file,
            "notify:\n  enable: true\n  every_day_hour: 8\n  jp: true\n  global: false\n  cn: true\n  black_groups: [999]\n  notify_text: \"预警\"\n",
        )
        .unwrap();
        store.init();

        let notify = ConfigEntry::<NotifyConfig>::in_store(store, PLUGIN, "notify");
        let original = notify.get();
        assert_eq!(original.every_day_hour, 8);
        assert_eq!(original.black_groups, vec![999]);
        assert!(!original.global, "显式 false 不应被默认值覆盖");

        let mut updated = original.clone();
        updated.every_day_hour = 20;
        updated.notify_text = "改过的文案".to_string();
        notify.set(&updated).expect("写回 notify 应成功");

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("every_day_hour: 20"), "新小时未落盘: {text}");
        assert!(
            text.contains("notify_text: 改过的文案"),
            "文案未落盘: {text}"
        );
        assert!(
            text.contains("# 每日推送的小时(0-23)"),
            "字段注释应由框架按 PluginConfig::comment 渲染: {text}"
        );
        assert!(
            text.contains("trainer:"),
            "未登记的 trainer 默认块未渲染: {text}"
        );
        assert!(
            !text.contains("managers:"),
            "框架的通用项不该出现在插件配置里: {text}"
        );
        assert!(
            text.contains(&format!("id: {PLUGIN}")),
            "模板应写明插件 id: {text}"
        );
        assert_eq!(notify.get().every_day_hour, 20, "内存值未随写回同步");

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(store.config_file(PLUGIN).parent().unwrap());
    }

    /// 升级兼容：notify 还写在框架 arona.yml 顶层时，加载它就把这一区搬进
    /// config/<插件>/arona.yml，并从框架那份文件里去掉，用户不用手改。
    #[test]
    fn legacy_notify_in_framework_config_moves_to_plugin_file() {
        const PLUGIN: &str = "bluearchive-cfg-legacy";
        let framework = isolated(PLUGIN);
        let dir = std::env::temp_dir().join("arona-cfg-legacy-notify");
        std::fs::create_dir_all(&dir).unwrap();
        let framework_file = dir.join("arona.yml");
        std::fs::write(
            &framework_file,
            "groups: [123]\nmanagers: [456]\nnotify:\n  every_day_hour: 21\n",
        )
        .unwrap();
        let plugin_file = framework.configs().config_file(PLUGIN);
        let _ = std::fs::remove_file(&plugin_file);

        // 加载框架那份配置：认出 notify 属于本插件，攒进待接管，再把这一区从文件里清掉
        let loaded =
            arona::config::arona::load_in(&framework, &framework_file).expect("框架配置应能加载");
        assert_eq!(loaded.groups, vec![123]);
        framework.configs().init();

        let notify = ConfigEntry::<NotifyConfig>::in_store(framework.configs(), PLUGIN, "notify");
        assert_eq!(
            notify.get().every_day_hour,
            21,
            "旧写法里的值应当接管插件配置"
        );
        let plugin_text = std::fs::read_to_string(&plugin_file).expect("插件配置文件应已生成");
        assert!(
            plugin_text.contains("every_day_hour: 21"),
            "旧写法应写进插件自己的文件: {plugin_text}"
        );
        let framework_text = std::fs::read_to_string(&framework_file).expect("框架配置应已重写");
        assert!(
            !framework_text.contains("notify:"),
            "框架 arona.yml 里不该再留插件的键: {framework_text}"
        );
        assert!(
            framework_text.contains("groups: [123]"),
            "重写框架配置不该丢通用项: {framework_text}"
        );

        let _ = std::fs::remove_file(&framework_file);
        let _ = std::fs::remove_file(&plugin_file);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
