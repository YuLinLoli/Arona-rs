//! 碧蓝档案插件自持有的业务配置（notify 每日推送 / trainer 攻略）。
//!
//! 这两块配置住在框架给本插件划的配置文件 `config/bluearchive/arona.yml` 里（顶层键
//! `notify` / `trainer`），框架不理解其内容：插件在 install 阶段用 [`register_sections`]
//! 把 [`NotifySection`] / [`TrainerSection`] 登记进去，框架据此生成带注释模板、加载原样
//! YAML 片段并在文件改动时热重载。运行期用 [`notify`] / [`trainer`] 反序列化读取，
//! `/config` 改这两区时用 [`set_notify`] 写回。

use arona::config::arona::ConfigSection;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::sync::Arc;

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

/// 从框架保存的插件配置原样片段读出 notify 配置（缺失或格式错时回退默认值）
pub fn notify() -> NotifyConfig {
    arona::config::plugin_config::section_value("notify")
        .and_then(|value| serde_yaml::from_value::<NotifyConfig>(value).ok())
        .unwrap_or_default()
}

/// 从框架保存的插件配置原样片段读出 trainer 配置（缺失或格式错时回退默认值）
pub fn trainer() -> TrainerConfig {
    arona::config::plugin_config::section_value("trainer")
        .and_then(|value| serde_yaml::from_value::<TrainerConfig>(value).ok())
        .unwrap_or_default()
}

/// 把修改后的 notify 配置写回 config/bluearchive/arona.yml（触发插件配置热重载）
pub fn set_notify(config: &NotifyConfig) -> Result<(), String> {
    let value = serde_yaml::to_value(config).map_err(|err| format!("序列化 notify 失败: {err}"))?;
    arona::config::plugin_config::set_section("notify", value)
}

fn yaml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// notify 配置区渲染器
struct NotifySection;

impl ConfigSection for NotifySection {
    fn key(&self) -> &'static str {
        "notify"
    }

    fn default_value(&self) -> Value {
        serde_yaml::to_value(NotifyConfig::default()).unwrap_or(Value::Null)
    }

    fn render(&self, value: &Value) -> String {
        let config = serde_yaml::from_value::<NotifyConfig>(value.clone()).unwrap_or_default();
        let mut out = String::new();
        out.push_str("# ==================== 每日活动推送 ====================\n");
        out.push_str("# 每天 every_day_hour 点向目标群推送国服/国际服/日服活动日历\n");
        out.push_str("notify:\n");
        out.push_str(&format!(
            "  # 是否启用每日活动防侠推送\n  enable: {}\n",
            config.enable
        ));
        out.push_str(&format!(
            "  # 每日推送的小时(0-23)\n  every_day_hour: {}\n",
            config.every_day_hour
        ));
        out.push_str(&format!(
            "  # 是否推送日服/国际服/国服活动\n  jp: {}\n",
            config.jp
        ));
        out.push_str(&format!("  global: {}\n", config.global));
        out.push_str(&format!("  cn: {}\n", config.cn));
        out.push_str(&format!(
            "  # 不推送的群号列表（黑名单），留空表示推送到全部允许的群\n  black_groups: {:?}\n",
            config.black_groups
        ));
        out.push_str(&format!(
            "  # 推送消息开头文字\n  notify_text: {}\n",
            yaml_quote(&config.notify_text)
        ));
        out
    }
}

/// trainer 配置区渲染器
struct TrainerSection;

impl ConfigSection for TrainerSection {
    fn key(&self) -> &'static str {
        "trainer"
    }

    fn default_value(&self) -> Value {
        serde_yaml::to_value(TrainerConfig::default()).unwrap_or(Value::Null)
    }

    fn render(&self, value: &Value) -> String {
        let config = serde_yaml::from_value::<TrainerConfig>(value.clone()).unwrap_or_default();
        let mut out = String::new();
        out.push_str("# ==================== 攻略(/攻略) ====================\n");
        out.push_str("trainer:\n");
        out.push_str(&format!(
            "  # 找不到精确匹配时是否提示模糊搜索结果\n  tip_when_null: {}\n",
            config.tip_when_null
        ));
        out.push_str(&format!(
            "  # 模糊搜索结果撤回时间(秒), 0 表示不撤回\n  tip_revoke_time: {}\n",
            config.tip_revoke_time
        ));
        out.push_str(&format!(
            "  # 等待用户回复数字选择的时间(秒), 0 表示关闭数字回复\n  tip_response_wait_time: {}\n",
            config.tip_response_wait_time
        ));
        out.push_str(
            "  # 覆盖 /攻略 行为: type=IMAGE(本地图片路径)/RAW(云端图片别名)/CODE(CQ 码原文)\n",
        );
        if config.r#override.is_empty() {
            out.push_str("  override: []\n");
        } else {
            out.push_str("  override:\n");
            if let Ok(yaml) = serde_yaml::to_string(&config.r#override) {
                for line in yaml.lines() {
                    out.push_str("  ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out
    }
}

/// install 阶段登记本插件的两块配置区：框架据此在 config/bluearchive/arona.yml 里
/// 按这里的顺序生成带注释模板（键名撞车时框架保留先登记的那家）
pub fn register_sections() {
    arona::config::arona::register_section(crate::PLUGIN_ID, Arc::new(NotifySection));
    arona::config::arona::register_section(crate::PLUGIN_ID, Arc::new(TrainerSection));
}

/// 串行化会改动进程级配置表（插件配置持有者与框架 arona.yml 持有者）的测试，
/// 避免并行互相覆盖。用 tokio 异步锁：async 测试要跨 `.await` 持有它。
#[cfg(test)]
pub(crate) static CONFIG_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端：notify 作为插件自持有配置区住在 config/bluearchive/arona.yml。
    /// 读 → 改 → 写回 → 再读；框架的通用项（groups/managers）不会混进这个文件，
    /// 未登记的 trainer 默认块照常渲染出来。
    #[tokio::test]
    async fn notify_roundtrips_through_plugin_config_file() {
        let _serial = CONFIG_TEST_LOCK.lock().await;
        register_sections();
        let file = arona::config::plugin_config::config_file(crate::PLUGIN_ID);
        std::fs::create_dir_all(file.parent().expect("插件配置文件应有父目录")).unwrap();
        let _ = std::fs::remove_file(&file);
        std::fs::write(
            &file,
            "notify:\n  enable: true\n  every_day_hour: 8\n  jp: true\n  global: false\n  cn: true\n  black_groups: [999]\n  notify_text: \"预警\"\n",
        )
        .unwrap();
        arona::config::plugin_config::init();

        let original = notify();
        assert_eq!(original.every_day_hour, 8);
        assert_eq!(original.black_groups, vec![999]);
        assert!(!original.global, "显式 false 不应被默认值覆盖");

        let mut updated = original.clone();
        updated.every_day_hour = 20;
        updated.notify_text = "改过的文案".to_string();
        set_notify(&updated).expect("写回 notify 应成功");

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("every_day_hour: 20"), "新小时未落盘: {text}");
        assert!(
            text.contains("notify_text: \"改过的文案\""),
            "文案未落盘: {text}"
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
            text.contains("id: bluearchive"),
            "模板应写明插件 id: {text}"
        );
        assert_eq!(notify().every_day_hour, 20, "内存值未随写回同步");

        let _ = std::fs::remove_file(&file);
    }

    /// 升级兼容：notify 还写在框架 arona.yml 顶层时，框架加载它就把它搬进
    /// config/bluearchive/arona.yml，并从框架那份文件里去掉，用户不用手改。
    #[tokio::test]
    async fn legacy_notify_in_framework_config_moves_to_plugin_file() {
        let _serial = CONFIG_TEST_LOCK.lock().await;
        register_sections();
        let dir = std::env::temp_dir().join("arona-cfg-legacy-notify");
        std::fs::create_dir_all(&dir).unwrap();
        let framework_file = dir.join("arona.yml");
        std::fs::write(
            &framework_file,
            "groups: [123]\nmanagers: [456]\nnotify:\n  every_day_hour: 21\n",
        )
        .unwrap();
        let plugin_file = arona::config::plugin_config::config_file(crate::PLUGIN_ID);
        let _ = std::fs::remove_file(&plugin_file);

        arona::config::standalone::init(framework_file.clone()).expect("框架配置应能加载");
        arona::config::plugin_config::init();

        assert_eq!(notify().every_day_hour, 21, "旧写法里的值应当接管插件配置");
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
