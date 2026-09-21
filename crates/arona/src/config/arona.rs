//! arona 框架业务配置（`config/arona.yml`）
//!
//! 本文件只管框架自己认识的几项：groups / managers / global_blacklist / group_settings /
//! disabled_plugins。功能插件的配置不住这里——每个插件各有一份 `config/<插件>/arona.yml`，
//! 由 [`super::plugin_config`] 负责生成模板、加载与热重载。
//! 插件在 install 阶段用 [`register_section`] 登记自己那几块配置（带上自己的插件 id），
//! 框架据此识别合法键、生成带注释模板，并把旧版还写在框架 arona.yml 顶层的同名键
//! 搬进插件自己的文件（见 [`super::plugin_config::absorb_legacy`]）。
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
    /// 该群整体禁用的插件 id：插件在该群的事件/命令一律不响应（GUI「群管理」里的开关）
    pub disabled_plugins: Vec<String>,
}

impl GroupSetting {
    pub fn feature_enabled(&self, key: &str) -> bool {
        !self.disabled_features.iter().any(|item| item == key)
    }

    /// 该设置里是否还有任何内容（清理空条目时用）
    pub fn is_empty(&self) -> bool {
        self.disabled_features.is_empty()
            && self.blacklist.is_empty()
            && self.disabled_plugins.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    /// 全局禁用的插件 id（GUI「插件管理」的总开关）：列在这里的插件不装配、不接收任何事件。
    /// 取插件的 `meta().id`，大小写不敏感。
    pub disabled_plugins: Vec<String>,
}

/// 插件持有的一块配置（写在该插件自己的 `config/<插件>/arona.yml` 里）。
/// 框架不理解内容，只负责：识别合法顶层键、生成模板时回调 [`ConfigSection::render`]
/// 写出带注释片段。插件在 install 阶段用 [`register_section`] 登记，
/// 之后自己用 [`super::plugin_config::section_value`] 读原样值、
/// [`super::plugin_config::set_section`] 写回。
pub trait ConfigSection: Send + Sync + 'static {
    /// 顶层键名（如 "notify"）
    fn key(&self) -> &'static str;
    /// 生成模板时若文件里没有该键，写这里的默认值
    fn default_value(&self) -> Value;
    /// 渲染该区的带注释 YAML 片段（以 "key:" 开头，末尾不留空行）。实现里通常把 `value`
    /// 反序列化成自己的强类型配置后按字段输出，从而顺带过滤掉未知子键。
    fn render(&self, value: &Value) -> String;
}

/// 插件的强类型配置（对应 mirai-console 的 `ConfigKey<T>` + `byConfigManager`）
///
/// 实现者只写一个 serde 结构 + 字段注释，模板渲染、默认值补齐、未知子键过滤全部由
/// 框架完成（经 [`typed_section`] 变成 [`ConfigSection`]），插件不再手写 YAML。
/// 读写入口见 [`super::plugin_config::ConfigEntry`]。
pub trait PluginConfig:
    Serialize + serde::de::DeserializeOwned + Default + Send + Sync + 'static
{
    /// 区块标题：写在配置区最前面的一行注释
    const TITLE: &'static str = "";
    /// 区块说明：跟在标题后面的若干行
    const DOC: &'static str = "";
    /// 字段注释。路径不含区块自身的键名，嵌套用点号，如 `override.name`
    fn comment(_path: &str) -> Option<&'static str> {
        None
    }
}

/// [`PluginConfig`] 到 [`ConfigSection`] 的适配器
pub struct TypedSection<T: PluginConfig> {
    key: &'static str,
    marker: std::marker::PhantomData<T>,
}

/// 把一个强类型配置登记成框架认识的配置区
pub fn typed_section<T: PluginConfig>(key: &'static str) -> Arc<dyn ConfigSection> {
    Arc::new(TypedSection::<T> {
        key,
        marker: std::marker::PhantomData,
    })
}

impl<T: PluginConfig> ConfigSection for TypedSection<T> {
    fn key(&self) -> &'static str {
        self.key
    }

    fn default_value(&self) -> Value {
        serde_yaml::to_value(T::default()).unwrap_or(Value::Null)
    }

    fn render(&self, value: &Value) -> String {
        // 先过一遍强类型：用户手写的未知子键在这一步就被丢掉，落盘的永远是干净的默认结构
        let config: T = serde_yaml::from_value(value.clone()).unwrap_or_default();
        let node = serde_yaml::to_value(&config).unwrap_or(Value::Null);
        let mut out = String::new();
        if !T::TITLE.is_empty() {
            out.push_str(&format!(
                "# ==================== {} ====================\n",
                T::TITLE
            ));
        }
        for line in T::DOC.lines() {
            out.push_str(&format!("# {line}\n"));
        }
        render_node(&mut out, self.key, "", &node, 0, T::comment);
        out
    }
}

fn write_comment(
    out: &mut String,
    pad: &str,
    path: &str,
    comment: impl Fn(&str) -> Option<&'static str>,
) {
    if let Some(text) = comment(path) {
        for line in text.lines() {
            out.push_str(&format!("{pad}# {line}\n"));
        }
    }
}

/// 递归写出一个 YAML 节点，注释按点分路径查。
/// `comment` 要 `Copy`：递归时按值往下传，`&impl Fn` 的写法会让每层多套一层引用而爆类型推断。
fn render_node(
    out: &mut String,
    key: &str,
    path: &str,
    node: &Value,
    indent: usize,
    comment: impl Fn(&str) -> Option<&'static str> + Copy,
) {
    let pad = "  ".repeat(indent);
    match node {
        Value::Mapping(map) if !map.is_empty() => {
            write_comment(out, &pad, path, comment);
            out.push_str(&format!("{pad}{key}:\n"));
            for (child_key, child_value) in map {
                let name = child_key.as_str().unwrap_or("?");
                let child_path = if path.is_empty() {
                    name.to_string()
                } else {
                    format!("{path}.{name}")
                };
                render_node(out, name, &child_path, child_value, indent + 1, comment);
            }
        }
        Value::Sequence(items) if !items.is_empty() => {
            write_comment(out, &pad, path, comment);
            // 元素结构由类型定义决定，注释只能给到数组本身；逐条按块式 YAML 缩进写出
            match serde_yaml::to_string(node) {
                Ok(text) => {
                    out.push_str(&format!("{pad}{key}:\n"));
                    for line in text.lines() {
                        out.push_str(&format!("{pad}  {line}\n"));
                    }
                }
                Err(_) => out.push_str(&format!("{pad}{key}: []\n")),
            }
        }
        other => {
            write_comment(out, &pad, path, comment);
            let text = match other {
                Value::Mapping(_) => "{}".to_string(),
                Value::Sequence(_) => "[]".to_string(),
                Value::Null => "null".to_string(),
                scalar => serde_yaml::to_string(scalar)
                    .unwrap_or_default()
                    .trim_end_matches('\n')
                    .to_string(),
            };
            out.push_str(&format!("{pad}{key}: {text}\n"));
        }
    }
}

/// 已登记的配置区：(归属插件 id, 渲染器)，按登记顺序写出
#[derive(Default)]
pub struct SectionRegistry {
    items: RwLock<Vec<(String, Arc<dyn ConfigSection>)>>,
}

impl SectionRegistry {
    fn snapshot(&self) -> Vec<(String, Arc<dyn ConfigSection>)> {
        self.items.read().unwrap().clone()
    }

    /// 登记一个插件配置区（install 阶段调用；键重复时保留先登记的）
    pub fn register(&self, plugin: &str, section: Arc<dyn ConfigSection>) {
        let mut items = self.items.write().unwrap();
        if !items.iter().any(|(_, item)| item.key() == section.key()) {
            items.push((plugin.to_string(), section));
        }
    }

    /// 某个插件登记的配置区（按登记顺序）
    pub fn sections_of(&self, plugin: &str) -> Vec<Arc<dyn ConfigSection>> {
        self.snapshot()
            .into_iter()
            .filter(|(owner, _)| owner == plugin)
            .map(|(_, section)| section)
            .collect()
    }

    /// 登记了配置区的插件 id（去重，按首次登记顺序）——框架据此决定要给谁生成配置文件
    pub fn owners(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for (owner, _) in self.snapshot() {
            if !ids.contains(&owner) {
                ids.push(owner);
            }
        }
        ids
    }

    /// 某块配置属于哪个插件
    pub fn owner(&self, key: &str) -> Option<String> {
        self.snapshot()
            .into_iter()
            .find(|(_, section)| section.key() == key)
            .map(|(owner, _)| owner)
    }

    /// 已登记的插件配置区键名列表
    pub fn keys(&self) -> Vec<String> {
        self.snapshot()
            .into_iter()
            .map(|(_, section)| section.key().to_string())
            .collect()
    }
}

fn registry() -> &'static SectionRegistry {
    crate::framework::Framework::global().sections()
}

/// 登记一个插件配置区（install 阶段调用；键重复时保留先登记的）
pub fn register_section(plugin: &str, section: Arc<dyn ConfigSection>) {
    registry().register(plugin, section);
}

/// 某个插件登记的配置区（按登记顺序）
pub fn sections_of(plugin: &str) -> Vec<Arc<dyn ConfigSection>> {
    registry().sections_of(plugin)
}

/// 登记了配置区的插件 id（去重，按首次登记顺序）——框架据此决定要给谁生成配置文件
pub fn section_owners() -> Vec<String> {
    registry().owners()
}

/// 某块配置属于哪个插件
pub fn section_owner(key: &str) -> Option<String> {
    registry().owner(key)
}

/// 已登记的插件配置区键名列表
pub fn section_keys() -> Vec<String> {
    registry().keys()
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
const GENERIC_TOP_KEYS: [&str; 5] = [
    "groups",
    "managers",
    "global_blacklist",
    "group_settings",
    "disabled_plugins",
];

/// 曾经写在 arona.yml、现已迁到 onebot.yml 的键：单独提示，避免和普通笔误混在一条日志里
const MOVED_TOP_KEYS: [&str; 1] = ["send_image_as_file"];

/// 加载 arona.yml：不存在时从旧后缀/旧 onebot.yml 迁移并生成模板
///
/// 认哪些键、旧写法搬到哪个插件的文件，都取自**进程默认框架实例**。
/// 要在自己的实例上跑（多实例、测试隔离）用 [`load_in`]。
pub fn load(file: &Path) -> Result<AronaConfig, String> {
    load_in(crate::framework::Framework::global(), file)
}

/// 在指定框架实例上加载 arona.yml：配置区归属查它的登记表，接管到的旧写法也并进它的配置文件表。
pub fn load_in(
    framework: &crate::framework::Framework,
    file: &Path,
) -> Result<AronaConfig, String> {
    if !file.exists() {
        // 1) 旧后缀 arona.yaml 已存在则迁移
        let old_arona = file.parent().map(|p| p.join("arona.yaml"));
        if let Some(old) = old_arona {
            if old.exists() {
                if let Ok(config) = parse_in(framework, &old) {
                    save(file, &config).map_err(|e| format!("写入 arona.yml 失败: {e}"))?;
                    crate::runtime::log::info(format!(
                        "[Arona] 检测到旧版 arona.yaml，已迁移到 {}",
                        file.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    return Ok(config);
                }
            }
        }
        // 2) 从旧 onebot.yml/yaml 迁移 groups/managers（插件配置区同样搬进各自的文件）
        let legacy = read_legacy_from_onebot(framework, file);
        let config = AronaConfig {
            groups: legacy
                .as_ref()
                .map(|l| l.groups.clone())
                .unwrap_or_default(),
            managers: legacy
                .as_ref()
                .map(|l| l.managers.clone())
                .unwrap_or_default(),
            ..Default::default()
        };
        save(file, &config).map_err(|e| format!("写入 arona.yml 失败: {e}"))?;
        return Ok(config);
    }
    parse_in(framework, file)
}

fn parse_in(framework: &crate::framework::Framework, file: &Path) -> Result<AronaConfig, String> {
    let text = read_text(file).map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
    let value: Value = serde_yaml::from_str(&text)
        .map_err(|e| format!("arona.yml 解析失败，请检查格式（参考同目录说明）: {e}"))?;
    let config: AronaConfig = serde_yaml::from_value(value.clone())
        .map_err(|e| format!("arona.yml 解析失败，请检查格式（参考同目录说明）: {e}"))?;
    // 未知顶层键：serde 默认静默忽略（例如把 onebot.yml 的 connections 写进本文件），
    // 用户会以为配置生效了，这里逐个写进日志提示；已登记的插件配置区键则搬去插件自己的文件。
    let registry = framework.sections();
    let registered = registry.keys();
    let mut moved: Vec<String> = Vec::new();
    if let Some(map) = value.as_mapping() {
        for (key, val) in map {
            let Some(name) = key.as_str() else { continue };
            if GENERIC_TOP_KEYS.contains(&name) {
                continue;
            } else if MOVED_TOP_KEYS.contains(&name) {
                crate::runtime::log::warning(format!(
                    "arona.yml 的「{name}」已移动到 onebot.yml（本项已忽略），请在 onebot.yml 里设置，或用管理面板「OneBot 连接」页的「发送设置」勾选"
                ));
            } else if let Some(owner) = registry.owner(name) {
                // 旧版把插件配置写在框架 arona.yml 顶层：交给插件配置模块搬进它自己的文件
                framework.configs().absorb_legacy(name, val.clone(), &owner);
                moved.push(name.to_string());
            } else {
                crate::runtime::log::warning(format!(
                    "arona.yml 存在无法识别的配置项「{name}」，已忽略；可用配置项: {} / 插件配置区: {}",
                    GENERIC_TOP_KEYS.join(" / "),
                    registered.join(" / ")
                ));
            }
        }
    }
    // 搬完就把本文件里的插件键清掉：留着下次加载还会再“迁移”一遍，用户也看不出改哪
    if !moved.is_empty() {
        save(file, &config).map_err(|e| format!("重写 arona.yml 失败: {e}"))?;
        crate::runtime::log::info(format!(
            "arona.yml 里的插件配置区（{}）已搬进各自 config/<插件>/arona.yml",
            moved.join(" / ")
        ));
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
}

/// 旧版 onebot.yml 中可能存在的业务字段：迁移框架的 groups/managers；
/// 已登记插件配置区的同名顶层键交给插件配置模块接管
fn read_legacy_from_onebot(
    framework: &crate::framework::Framework,
    arona_file: &Path,
) -> Option<LegacyExtra> {
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
    let registry = framework.sections();
    for key in registry.keys() {
        if let (Some(val), Some(owner)) = (map.get(&key), registry.owner(&key)) {
            framework.configs().absorb_legacy(&key, val.clone(), &owner);
        }
    }
    Some(extra)
}

/// 生成带注释的模板文本（只含框架自身的项；插件配置在 config/<插件>/arona.yml）
fn template(config: &AronaConfig) -> String {
    let mut out = String::new();
    out.push_str("# ==================== Arona 框架配置 ====================\n");
    out.push_str("# Rust 移植版（arona-rs）独立运行模式使用本文件，修改后保存即自动热重载。\n");
    out.push_str("# 本文件只放框架自身的项：授权、黑名单、分群开关与插件开关。\n");
    out.push_str("# OneBot 协议连接配置见同目录 onebot.yml；功能插件的配置在各自的 config/<插件>/arona.yml。\n\n");
    out.push_str("# 允许响应的群号列表，留空表示响应所有群\n");
    out.push_str(&format!("groups: {:?}\n", config.groups));
    out.push_str("# 管理员 QQ 号列表，可执行管理命令\n");
    out.push_str(&format!("managers: {:?}\n", config.managers));
    out.push('\n');
    out.push_str("# ==================== 黑名单与分群功能开关 ====================\n");
    out.push_str("# 全局用户黑名单：这些 QQ 在任何群/私聊都不触发机器人（管理员不受限）\n");
    out.push_str(&format!(
        "global_blacklist: {:?}\n",
        config.global_blacklist
    ));
    out.push_str("# 分群设置：群号 -> 关闭的功能(disabled_features) / 禁用的插件(disabled_plugins) / 群内成员黑名单(blacklist)\n");
    out.push_str("# 可用功能 key: ");
    out.push_str(&crate::runtime::config::feature_keys_text());
    out.push('\n');
    if config.group_settings.is_empty() {
        out.push_str("# group_settings:\n");
        out.push_str("#   \"123456789\":\n");
        out.push_str("#     disabled_features: [tarot]\n");
        out.push_str(&format!(
            "#     disabled_plugins: [{}]\n",
            crate::plugin::metas()
                .first()
                .map(|meta| meta.id)
                .unwrap_or("插件id")
        ));
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
    out.push_str("# ==================== 插件开关 ====================\n");
    out.push_str("# 全局禁用的插件：列在这里的插件启动时不装配、也不接收任何事件\n");
    out.push_str("# 已安装的插件(id): ");
    out.push_str(&plugin_ids_text());
    out.push('\n');
    if config.disabled_plugins.is_empty() {
        out.push_str("disabled_plugins: []\n");
    } else {
        out.push_str(&format!(
            "disabled_plugins: {:?}\n",
            config.disabled_plugins
        ));
    }
    out.push('\n');
    out.push_str("# 提示: 本地图片的发送方式(send_image_as_file)属于 onebot.yml，在那里配置。\n");
    out
}

/// 已安装插件的 id 列表（模板注释里据此告诉用户能禁用谁）
fn plugin_ids_text() -> String {
    let ids: Vec<&str> = crate::plugin::metas().iter().map(|meta| meta.id).collect();
    if ids.is_empty() {
        "(无)".to_string()
    } else {
        ids.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：不认识的顶层键只记日志、不报错（例如把 onebot.yml 的 connections 写进本文件），
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
        assert!(config.disabled_plugins.is_empty());
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

    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    #[serde(default)]
    struct DemoAlert {
        hour: u32,
        text: String,
    }

    #[derive(Clone, Debug, Default, Serialize, Deserialize)]
    #[serde(default)]
    struct DemoConfig {
        enable: bool,
        black_groups: Vec<i64>,
        extra: BTreeMap<String, String>,
        alert: DemoAlert,
    }

    impl PluginConfig for DemoConfig {
        const TITLE: &'static str = "渲染契约自检";
        const DOC: &'static str = "第一行\n第二行";
        fn comment(path: &str) -> Option<&'static str> {
            Some(match path {
                "black_groups" => "空的也要渲染成 []",
                "alert" => "嵌套块",
                "alert.hour" => "推送小时(0-23)",
                _ => return None,
            })
        }
    }

    /// 回归：空列表过去会被渲染成 `{}`，空映射才是 `{}`。
    /// 渲染出的 YAML 必须能原样读回强类型，且注释按点分路径落在正确的缩进上。
    #[test]
    fn typed_section_renders_empty_collections_with_yaml_flows() {
        let section = typed_section::<DemoConfig>("demo");
        let text = section.render(&Value::Null);

        assert!(text.starts_with("# ==================== 渲染契约自检 ====================\n# 第一行\n# 第二行\ndemo:\n"), "{text}");
        assert!(
            text.contains("\n  black_groups: []\n"),
            "空列表应渲染为 []: {text}"
        );
        assert!(
            text.contains("\n  extra: {}\n"),
            "空映射应渲染为 {{}}: {text}"
        );
        assert!(
            text.contains("  # 空的也要渲染成 []\n  black_groups:"),
            "数组注释应紧贴键: {text}"
        );
        assert!(
            text.contains("    # 推送小时(0-23)\n    hour: 0\n"),
            "嵌套字段注释应按 alert.hour 命中并多缩进一层: {text}"
        );

        let file: BTreeMap<String, DemoConfig> =
            serde_yaml::from_str(&text).expect("模板应能读回强类型");
        assert_eq!(file["demo"].black_groups, Vec::<i64>::new());
        assert!(file["demo"].extra.is_empty());
        assert!(!file["demo"].enable);

        let filled = serde_yaml::to_value(DemoConfig {
            enable: true,
            black_groups: vec![999, 1001],
            extra: BTreeMap::from([("a".to_string(), "b".to_string())]),
            alert: DemoAlert {
                hour: 8,
                text: "预警".to_string(),
            },
        })
        .unwrap();
        let text = section.render(&filled);
        assert!(
            text.contains("  black_groups:\n    - 999\n    - 1001\n"),
            "非空列表应块式缩进: {text}"
        );
        let file: BTreeMap<String, DemoConfig> =
            serde_yaml::from_str(&text).expect("有值模板应能读回强类型");
        assert_eq!(file["demo"].black_groups, vec![999, 1001]);
        assert_eq!(file["demo"].alert.text, "预警");
        assert_eq!(file["demo"].extra["a"], "b");
    }
}
