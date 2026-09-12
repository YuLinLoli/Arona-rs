//! arona 业务配置（对应原版 runtime/AronaConfig + AronaConfigLoader）
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
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

fn default_true() -> bool {
    true
}
fn default_hour() -> i32 {
    8
}
fn default_notify_text() -> String {
    "碧蓝档案预警".to_string()
}

/// 单个群的业务设置（群号 -> 设置）
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupSetting {
    /// 该群关闭的功能 key（见 runtime::config::FEATURES），未列出的功能保持开启
    pub disabled_features: Vec<String>,
    /// 该群内的用户黑名单（这些 QQ 在本群不触发机器人）
    pub blacklist: Vec<i64>,
}

impl GroupSetting {
    pub fn feature_enabled(&self, key: &str) -> bool {
        !self.disabled_features.iter().any(|item| item == key)
    }
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AronaConfig {
    /// 允许响应的群号列表，留空表示响应所有群
    pub groups: Vec<i64>,
    /// 管理员 QQ 号列表
    pub managers: Vec<i64>,
    pub notify: NotifyConfig,
    pub trainer: TrainerConfig,
    /// 全局用户黑名单：这些 QQ 在任何群/私聊都不触发机器人（管理员不受限）
    pub global_blacklist: Vec<i64>,
    /// 分群设置：群号(字符串) -> 功能开关 / 群内成员黑名单
    pub group_settings: BTreeMap<String, GroupSetting>,
}

impl Default for AronaConfig {
    fn default() -> Self {
        AronaConfig {
            groups: Vec::new(),
            managers: Vec::new(),
            notify: NotifyConfig::default(),
            trainer: TrainerConfig::default(),
            global_blacklist: Vec::new(),
            group_settings: BTreeMap::new(),
        }
    }
}

fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

fn write_text(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

/// 加载 arona.yml：不存在时从旧后缀/旧 onebot.yml 迁移并生成模板
pub fn load(file: &Path) -> Result<AronaConfig, String> {
    if !file.exists() {
        // 1) 旧后缀 arona.yaml 已存在则迁移
        let old_arona = file.parent().map(|p| p.join("arona.yaml"));
        if let Some(old) = old_arona {
            if old.exists() {
                if let Ok(config) = parse(&old) {
                    save(file, &config).map_err(|e| format!("写入 arona.yml 失败: {e}"))?;
                    crate::runtime::log::info(format!(
                        "[Arona] 检测到旧版 arona.yaml，已迁移到 {}",
                        file.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    return Ok(config);
                }
            }
        }
        // 2) 从旧 onebot.yml/yaml 迁移 groups/managers/notify
        let legacy = read_legacy_from_onebot(file);
        let config = AronaConfig {
            groups: legacy
                .as_ref()
                .map(|l| l.groups.clone())
                .unwrap_or_default(),
            managers: legacy
                .as_ref()
                .map(|l| l.managers.clone())
                .unwrap_or_default(),
            notify: legacy.map(|l| l.notify).unwrap_or_default(),
            trainer: TrainerConfig::default(),
            global_blacklist: Vec::new(),
            group_settings: BTreeMap::new(),
        };
        save(file, &config).map_err(|e| format!("写入 arona.yml 失败: {e}"))?;
        return Ok(config);
    }
    parse(file)
}

/// arona.yml 允许出现的顶层键，其它键一律提示（避免写错后静默失效）
const KNOWN_TOP_KEYS: [&str; 6] = [
    "groups",
    "managers",
    "notify",
    "trainer",
    "global_blacklist",
    "group_settings",
];

/// 曾经写在 arona.yml、现已迁到 onebot.yml 的键：单独提示，避免和普通笔误混在一条日志里
const MOVED_TOP_KEYS: [&str; 1] = ["send_image_as_file"];

fn parse(file: &Path) -> Result<AronaConfig, String> {
    let text = read_text(file).map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
    let value: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|e| format!("arona.yml 解析失败，请检查格式（参考同目录说明）: {e}"))?;
    // 未知顶层键：serde 默认静默忽略（例如把 onebot.yml 的 connections 写进本文件），
    // 用户会以为配置生效了，这里逐个写进日志提示。
    if let Some(map) = value.as_mapping() {
        for key in map.keys() {
            let Some(name) = key.as_str() else { continue };
            if MOVED_TOP_KEYS.contains(&name) {
                crate::runtime::log::warning(format!(
                    "arona.yml 的「{name}」已移动到 onebot.yml（本项已忽略），请在 onebot.yml 里设置，或用管理面板「OneBot 连接」页的「发送设置」勾选"
                ));
            } else if !KNOWN_TOP_KEYS.contains(&name) {
                crate::runtime::log::warning(format!(
                    "arona.yml 存在无法识别的配置项「{name}」，已忽略；可用配置项: {}",
                    KNOWN_TOP_KEYS.join(" / ")
                ));
            }
        }
    }
    let config: AronaConfig = serde_yaml::from_value(value)
        .map_err(|e| format!("arona.yml 解析失败，请检查格式（参考同目录说明）: {e}"))?;
    Ok(config)
}

pub fn save(file: &Path, config: &AronaConfig) -> std::io::Result<()> {
    write_text(file, &template(config))
}

pub fn default_file() -> std::path::PathBuf {
    crate::runtime::paths::default_arona_file()
}

#[derive(Default)]
struct LegacyExtra {
    groups: Vec<i64>,
    managers: Vec<i64>,
    notify: NotifyConfig,
}

/// 旧版 onebot.yml 中可能存在的业务字段（notify.groups 为旧语义，直接丢弃）
fn read_legacy_from_onebot(arona_file: &Path) -> Option<LegacyExtra> {
    let parent = arona_file.parent()?;
    let new_file = parent.join("onebot.yml");
    let onebot = if new_file.exists() {
        new_file
    } else {
        parent.join("onebot.yaml")
    };
    if !onebot.exists() {
        return None;
    }
    let text = read_text(&onebot).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    let map = value.as_mapping()?;
    let mut extra = LegacyExtra::default();
    if let Some(groups) = map
        .get("groups")
        .and_then(|v| serde_yaml::from_value::<Vec<i64>>(v.clone()).ok())
    {
        extra.groups = groups;
    }
    if let Some(managers) = map
        .get("managers")
        .and_then(|v| serde_yaml::from_value::<Vec<i64>>(v.clone()).ok())
    {
        extra.managers = managers;
    }
    if let Some(notify) = map.get("notify") {
        if let Some(notify_map) = notify.as_mapping() {
            let mut notify_map = notify_map.clone();
            notify_map.remove(&serde_yaml::Value::String("groups".to_string()));
            if let Ok(n) =
                serde_yaml::from_value::<NotifyConfig>(serde_yaml::Value::Mapping(notify_map))
            {
                extra.notify = n;
            }
        }
    }
    Some(extra)
}

fn yaml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// 生成带注释的模板文本
fn template(config: &AronaConfig) -> String {
    let mut out = String::new();
    out.push_str("# ==================== Arona 业务配置 ====================\n");
    out.push_str("# Rust 移植版（arona-rs）独立运行模式使用本文件，修改后保存即自动热重载。\n");
    out.push_str("# OneBot 协议连接配置见同目录 onebot.yml；本文件只放非 OneBot 的业务配置。\n\n");
    out.push_str("# 允许响应的群号列表，留空表示响应所有群\n");
    out.push_str(&format!("groups: {:?}\n", config.groups));
    out.push_str("# 管理员 QQ 号列表，可执行管理命令\n");
    out.push_str(&format!("managers: {:?}\n", config.managers));
    out.push('\n');
    out.push_str("# ==================== 每日活动推送 ====================\n");
    out.push_str("# 每天 every_day_hour 点向目标群推送国服/国际服/日服活动日历\n");
    out.push_str("notify:\n");
    out.push_str(&format!(
        "  # 是否启用每日活动防侠推送\n  enable: {}\n",
        config.notify.enable
    ));
    out.push_str(&format!(
        "  # 每日推送的小时(0-23)\n  every_day_hour: {}\n",
        config.notify.every_day_hour
    ));
    out.push_str(&format!(
        "  # 是否推送日服/国际服/国服活动\n  jp: {}\n",
        config.notify.jp
    ));
    out.push_str(&format!("  global: {}\n", config.notify.global));
    out.push_str(&format!("  cn: {}\n", config.notify.cn));
    out.push_str(&format!(
        "  # 不推送的群号列表（黑名单），留空表示推送到全部允许的群\n  black_groups: {:?}\n",
        config.notify.black_groups
    ));
    out.push_str(&format!(
        "  # 推送消息开头文字\n  notify_text: {}\n",
        yaml_quote(&config.notify.notify_text)
    ));
    out.push('\n');
    out.push_str("# ==================== 黑名单与分群功能开关 ====================\n");
    out.push_str("# 全局用户黑名单：这些 QQ 在任何群/私聊都不触发机器人（管理员不受限）\n");
    out.push_str(&format!(
        "global_blacklist: {:?}\n",
        config.global_blacklist
    ));
    out.push_str("# 分群设置：群号 -> 关闭的功能(disabled_features) / 群内成员黑名单(blacklist)\n");
    out.push_str("# 可用功能 key: ");
    out.push_str(&crate::runtime::config::feature_keys_text());
    out.push('\n');
    if config.group_settings.is_empty() {
        out.push_str("# group_settings:\n");
        out.push_str("#   \"123456789\":\n");
        out.push_str("#     disabled_features: [tarot]\n");
        out.push_str("#     blacklist: [10001]\n");
        out.push_str("group_settings: {}\n");
    } else {
        out.push_str("group_settings:\n");
        if let Ok(yaml) = serde_yaml::to_string(&config.group_settings) {
            for line in yaml.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out.push('\n');
    out.push_str("# 提示: 本地图片的发送方式(send_image_as_file)属于 onebot.yml，在那里配置。\n");
    out.push('\n');
    out.push_str("# ==================== 攻略(/攻略) ====================\n");
    out.push_str("trainer:\n");
    out.push_str(&format!(
        "  # 找不到精确匹配时是否提示模糊搜索结果\n  tip_when_null: {}\n",
        config.trainer.tip_when_null
    ));
    out.push_str(&format!(
        "  # 模糊搜索结果撤回时间(秒), 0 表示不撤回\n  tip_revoke_time: {}\n",
        config.trainer.tip_revoke_time
    ));
    out.push_str(&format!(
        "  # 等待用户回复数字选择的时间(秒), 0 表示关闭数字回复\n  tip_response_wait_time: {}\n",
        config.trainer.tip_response_wait_time
    ));
    out.push_str("  # 覆盖 /攻略 行为: type=IMAGE(本地图片路径)/RAW(云端图片别名)/CODE(CQ 码原文)\n");
    if config.trainer.r#override.is_empty() {
        out.push_str("  override: []\n");
    } else {
        out.push_str("  override:\n");
        if let Ok(yaml) = serde_yaml::to_string(&config.trainer.r#override) {
            for line in yaml.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：不认识的顶层键只记日志、不报错（例如把 onebot.yml 的 connections 写进 arona.yml），
    /// 一个笔误不该让程序起不来。
    #[test]
    fn unknown_top_level_keys_are_ignored() {
        let file = std::env::temp_dir().join("arona-arona-parse-test.yml");
        std::fs::write(
            &file,
            "groups: [10001]\nconnections:\n  ws-forward:\n    enable: false\nmanagers: [20002]\n",
        )
        .expect("写入测试配置失败");
        let config = load(&file).expect("未知顶层键不应导致加载失败");
        assert_eq!(config.groups, vec![10001]);
        assert_eq!(config.managers, vec![20002]);
        let _ = std::fs::remove_file(&file);
    }

    /// 回归：send_image_as_file 已经搬到 onebot.yml，旧 arona.yml 里残留的写法
    /// 只提示、不报错，也不该让程序起不来
    #[test]
    fn moved_send_image_as_file_key_is_tolerated() {
        let file = std::env::temp_dir().join("arona-arona-parse-test2.yml");
        std::fs::write(&file, "send_image_as_file: true\ngroups: [10001]\n")
            .expect("写入测试配置失败");
        let config = load(&file).expect("已迁移的旧键不应导致加载失败");
        assert_eq!(config.groups, vec![10001]);
        let _ = std::fs::remove_file(&file);
    }
}
