//! arona 业务配置（对应原版 runtime/AronaConfig + AronaConfigLoader）
//!
//! 框架只理解「通用授权/黑名单/分群开关」这几项（groups/managers/global_blacklist/group_settings）；
//! 各功能插件自己的配置区（如碧蓝档案的 notify / trainer）以原始 YAML 形式保存在
//! [`AronaConfig::sections`] 里，框架不认识其内容，只在生成 arona.yml 时回调插件注册的
//! [`ConfigSection`] 渲染器写出带注释片段，并据此避免把插件的键当成未知键。
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AronaConfig {
    /// 允许响应的群号列表，留空表示响应所有群
    pub groups: Vec<i64>,
    /// 管理员 QQ 号列表
    pub managers: Vec<i64>,
    /// 全局用户黑名单：这些 QQ 在任何群/私聊都不触发机器人（管理员不受限）
    pub global_blacklist: Vec<i64>,
    /// 分群设置：群号(字符串) -> 功能开关 / 群内成员黑名单
    pub group_settings: BTreeMap<String, GroupSetting>,
    /// 插件自持有的顶层配置区（notify/trainer…）：原样保存的 YAML 片段，框架不理解内容。
    /// 派生序列化里跳过——由 [`save`] 通过注册的 [`ConfigSection`] 渲染器手动写出。
    #[serde(skip)]
    pub sections: BTreeMap<String, Value>,
}

impl Default for AronaConfig {
    fn default() -> Self {
        AronaConfig {
            groups: Vec::new(),
            managers: Vec::new(),
            global_blacklist: Vec::new(),
            group_settings: BTreeMap::new(),
            sections: BTreeMap::new(),
        }
    }
}

/// 插件持有的 arona.yml 顶层配置区。框架据此识别合法键、生成带注释模板并回调渲染。
/// 插件在 install 阶段用 [`register_section`] 登记，之后自己用 [`super::standalone::section_value`]
/// 读原样值、[`super::standalone::set_section`] 写回。
pub trait ConfigSection: Send + Sync + 'static {
    /// 顶层键名（如 "notify"）
    fn key(&self) -> &'static str;
    /// 生成模板时若文件里没有该键，写这里的默认值
    fn default_value(&self) -> Value;
    /// 渲染该区的带注释 YAML 片段（以 "key:" 开头，末尾不留空行）。实现里通常把 `value`
    /// 反序列化成自己的强类型配置后按字段输出，从而顺带过滤掉未知子键。
    fn render(&self, value: &Value) -> String;
    /// 这块配置写在 arona.yml 里哪个框架顶层键的后面（`None` = 文件末尾）。
    /// 同一位置上按 [`register_section`] 的注册顺序依次写出。
    ///
    /// 插件用它把自己那块放回原来的位置（例如碧蓝档案的 notify 一直紧跟 managers），
    /// 框架因此不必知道任何插件的键名。
    fn after_key(&self) -> Option<&'static str> {
        None
    }
}

static SECTIONS: RwLock<Vec<Arc<dyn ConfigSection>>> = RwLock::new(Vec::new());

/// 登记一个插件配置区（install 阶段调用；key 重复时保留首个）
pub fn register_section(section: Arc<dyn ConfigSection>) {
    let mut sections = SECTIONS.write().unwrap();
    if !sections.iter().any(|s| s.key() == section.key()) {
        sections.push(section);
    }
}

fn sections() -> Vec<Arc<dyn ConfigSection>> {
    SECTIONS.read().unwrap().clone()
}

/// 已登记的插件配置区键名列表
pub fn section_keys() -> Vec<String> {
    sections().iter().map(|s| s.key().to_string()).collect()
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

/// 框架自身认识的顶层键（除这些之外的顶层键交给插件配置区/未知键逻辑处理）
const GENERIC_TOP_KEYS: [&str; 4] = ["groups", "managers", "global_blacklist", "group_settings"];

/// 曾经写在 arona.yml、现已迁到 onebot.yml 的键：单独提示，避免和普通笔误混在一条日志里
const MOVED_TOP_KEYS: [&str; 1] = ["send_image_as_file"];

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
        // 2) 从旧 onebot.yml/yaml 迁移 groups/managers 与已登记插件配置区
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
            global_blacklist: Vec::new(),
            group_settings: BTreeMap::new(),
            sections: legacy.map(|l| l.sections).unwrap_or_default(),
        };
        save(file, &config).map_err(|e| format!("写入 arona.yml 失败: {e}"))?;
        return Ok(config);
    }
    parse(file)
}

fn parse(file: &Path) -> Result<AronaConfig, String> {
    let text = read_text(file).map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
    let value: Value = serde_yaml::from_str(&text)
        .map_err(|e| format!("arona.yml 解析失败，请检查格式（参考同目录说明）: {e}"))?;
    let mut config: AronaConfig = serde_yaml::from_value(value.clone())
        .map_err(|e| format!("arona.yml 解析失败，请检查格式（参考同目录说明）: {e}"))?;
    // 未知顶层键：serde 默认静默忽略（例如把 onebot.yml 的 connections 写进本文件），
    // 用户会以为配置生效了，这里逐个写进日志提示；已登记插件配置区的键原样收进 sections。
    let registered = section_keys();
    if let Some(map) = value.as_mapping() {
        for (key, val) in map {
            let Some(name) = key.as_str() else { continue };
            if GENERIC_TOP_KEYS.contains(&name) {
                continue;
            } else if MOVED_TOP_KEYS.contains(&name) {
                crate::runtime::log::warning(format!(
                    "arona.yml 的「{name}」已移动到 onebot.yml（本项已忽略），请在 onebot.yml 里设置，或用管理面板「OneBot 连接」页的「发送设置」勾选"
                ));
            } else if registered.iter().any(|k| k == name) {
                config.sections.insert(name.to_string(), val.clone());
            } else {
                crate::runtime::log::warning(format!(
                    "arona.yml 存在无法识别的配置项「{name}」，已忽略；可用配置项: {} / {}",
                    GENERIC_TOP_KEYS.join(" / "),
                    registered.join(" / ")
                ));
            }
        }
    }
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
    sections: BTreeMap<String, Value>,
}

/// 旧版 onebot.yml 中可能存在的业务字段：迁移 groups/managers，以及已登记插件配置区的同名顶层键
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
    let value: Value = serde_yaml::from_str(&text).ok()?;
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
    for key in section_keys() {
        if let Some(val) = map.get(&key) {
            extra.sections.insert(key, val.clone());
        }
    }
    Some(extra)
}

/// 生成带注释的模板文本
fn template(config: &AronaConfig) -> String {
    let mut out = String::new();
    out.push_str("# ==================== Arona 框架配置 ====================\n");
    out.push_str("# Rust 移植版（arona-rs）独立运行模式使用本文件，修改后保存即自动热重载。\n");
    out.push_str("# OneBot 协议连接配置见同目录 onebot.yml；本文件只放非 OneBot 的配置。\n");
    out.push_str("# 下面框架自身的项是固定的；功能插件的配置区由插件自己补在它声明的位置。\n\n");
    out.push_str("# 允许响应的群号列表，留空表示响应所有群\n");
    out.push_str(&format!("groups: {:?}\n", config.groups));
    out.push_str("# 管理员 QQ 号列表，可执行管理命令\n");
    out.push_str(&format!("managers: {:?}\n", config.managers));
    render_sections(&mut out, Some("managers"), config);
    out.push('\n');
    out.push_str("# ==================== 黑名单与分群功能开关 ====================\n");
    out.push_str("# 全局用户黑名单：这些 QQ 在任何群/私聊都不触发机器人（管理员不受限）\n");
    out.push_str(&format!(
        "global_blacklist: {:?}\n",
        config.global_blacklist
    ));
    render_sections(&mut out, Some("global_blacklist"), config);
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
    // 插件自持有的配置区（notify/trainer…）：各自按 after_key 插回原位，
    // 没声明位置的按注册顺序排在文件末尾。
    render_sections(&mut out, Some("group_settings"), config);
    render_sections(&mut out, None, config);
    out
}

/// 写出紧跟在某个框架顶层键之后的插件配置区（同一位置按注册顺序）
fn render_sections(out: &mut String, after: Option<&str>, config: &AronaConfig) {
    for section in sections() {
        if section.after_key() != after {
            continue;
        }
        let value = config
            .sections
            .get(section.key())
            .cloned()
            .unwrap_or_else(|| section.default_value());
        out.push('\n');
        out.push_str(&section.render(&value));
    }
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
        // 未登记的顶层键（connections）不进 sections，会被丢弃
        assert!(config.sections.is_empty());
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
        // 已迁移的旧键不进 sections（不写回，避免残留）
        assert!(config.sections.is_empty());
        let _ = std::fs::remove_file(&file);
    }
}
