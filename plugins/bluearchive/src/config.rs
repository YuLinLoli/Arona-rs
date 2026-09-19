//! 碧蓝档案插件自持有的业务配置（notify 每日推送 / trainer 攻略）。
//!
//! 这些配置住在 arona.yml 的顶层键 `notify` / `trainer` 里，但框架不理解其内容：
//! 插件在 install 阶段用 [`register_sections`] 把 [`NotifySection`] / [`TrainerSection`]
//! 登记进框架，框架据此（1）识别合法顶层键、（2）生成 arona.yml 时回调 [`ConfigSection::render`]
//! 写出带注释片段。运行期用 [`notify`] / [`trainer`] 从框架保存的原样 YAML 反序列化读取，
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

/// 从框架保存的 arona.yml 原样片段读出 notify 配置（缺失或格式错时回退默认值）
pub fn notify() -> NotifyConfig {
    arona::config::standalone::section_value("notify")
        .and_then(|value| serde_yaml::from_value::<NotifyConfig>(value).ok())
        .unwrap_or_default()
}

/// 从框架保存的 arona.yml 原样片段读出 trainer 配置（缺失或格式错时回退默认值）
pub fn trainer() -> TrainerConfig {
    arona::config::standalone::section_value("trainer")
        .and_then(|value| serde_yaml::from_value::<TrainerConfig>(value).ok())
        .unwrap_or_default()
}

/// 把修改后的 notify 配置写回 arona.yml（触发框架热重载与插件 on_config_reload）
pub fn set_notify(config: &NotifyConfig) -> Result<(), String> {
    let value = serde_yaml::to_value(config).map_err(|err| format!("序列化 notify 失败: {err}"))?;
    arona::config::standalone::set_section("notify", value)
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

    /// 回到拆分前 arona.yml 里的老位置：紧跟 managers，在黑名单/分群那段之前
    fn after_key(&self) -> Option<&'static str> {
        Some("managers")
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

/// trainer 配置区渲染器（拆分前就写在 arona.yml 末尾，所以不声明 after_key）
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

/// install 阶段登记本插件的两块配置区（同一位置上按这里的顺序写出，
/// 各自具体落在 arona.yml 哪一段后面由 `after_key` 声明）
pub fn register_sections() {
    arona::config::arona::register_section(Arc::new(NotifySection));
    arona::config::arona::register_section(Arc::new(TrainerSection));
}

/// 串行化会改动全局 arona 配置持有者（`standalone::init`）的测试，避免并行互相覆盖。
/// 用 tokio 异步锁：async 测试要跨 `.await` 持有它。
#[cfg(test)]
pub(crate) static CONFIG_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端：notify 段作为插件自持有配置区，读 → 改 → 写回 arona.yml → 重载，
    /// 且写回时通用项(groups/managers)与其它 notify 子项都不丢，trainer 默认块照常渲染。
    #[tokio::test]
    async fn notify_roundtrips_through_arona_yml() {
        let _serial = CONFIG_TEST_LOCK.lock().await;
        register_sections();
        let dir = std::env::temp_dir().join("arona-cfg-notify-roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("arona.yml");
        let _ = std::fs::remove_file(&file);
        std::fs::write(
            &file,
            "groups: [123]\nmanagers: [456]\n\
             notify:\n  enable: true\n  every_day_hour: 8\n  jp: true\n  global: false\n  cn: true\n  black_groups: [999]\n  notify_text: \"预警\"\n",
        )
        .unwrap();
        arona::config::standalone::init(file.clone()).expect("测试配置应能加载");

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
        assert!(text.contains("groups: [123]"), "通用项 groups 丢失: {text}");
        assert!(
            text.contains("managers: [456]"),
            "通用项 managers 丢失: {text}"
        );
        assert!(
            text.contains("trainer:"),
            "未登记的 trainer 默认块未渲染: {text}"
        );
        // 两块配置区各回各位：notify 紧跟 managers（拆分前就在那儿），trainer 留在末尾
        let managers_at = text.find("managers: [456]").expect("managers 段");
        let notify_at = text.find("notify:").expect("notify 段");
        let blacklist_at = text.find("黑名单与分群功能开关").expect("框架黑名单段");
        let trainer_at = text.find("trainer:").expect("trainer 段");
        assert!(
            managers_at < notify_at && notify_at < blacklist_at,
            "notify 应紧跟 managers、排在框架黑名单段之前:\n{text}"
        );
        assert!(
            blacklist_at < trainer_at,
            "trainer 应排在框架配置段之后:\n{text}"
        );
        assert_eq!(notify().every_day_hour, 20, "内存值未随写回同步");

        let _ = std::fs::remove_file(&file);
    }
}
