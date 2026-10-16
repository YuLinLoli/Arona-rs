//! arona 框架业务配置（`config/arona.yml`）
//!
//! 本文件只管 [`AronaConfig`] 的字段所对应的那几项顶层键（键名以 [`generic_top_keys`] 现取的为准，
//! 不在注释里另抄一份）。功能插件的配置不住这里——每个插件各有一份 `config/<插件>/arona.yml`，
//! 由 [`super::plugin_config`] 负责生成模板、加载与热重载。
//! 插件在 install 阶段用 [`register_section`] 登记自己那几块配置（带上自己的插件 id），
//! 框架据此识别合法键、生成带注释模板，并把旧版还写在框架 arona.yml 顶层的同名键
//! 搬进插件自己的文件（见 [`super::plugin_config::absorb_legacy`]）。
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

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

/// 框架自身的行为选项（arona.yml 的 `framework:` 段）。
///
/// 对位 mirai 的 `MiraiInstance.new { }`：以前 [`crate::framework::FrameworkOptions`]
/// 只能由代码在构造时传入，配置文件里没有落点，等于没接通。放在这里就能热重载。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameworkSettings {
    /// 同一插件连续 panic 多少次就隔离停用；0 表示只记日志、不停用
    pub panic_disable_threshold: u32,
    /// 未显式声明 `with_prefix_match` 的命令，是否也允许最短前缀匹配
    pub prefix_match_by_default: bool,
    /// 单次定时任务执行超过多少秒就判定为「卡住」：通报管理员并停用该插件。
    /// 0 表示关掉看门狗（只按原样一直等）。
    pub slow_job_secs: u64,
}

impl Default for FrameworkSettings {
    fn default() -> Self {
        let options = crate::framework::FrameworkOptions::default();
        FrameworkSettings {
            panic_disable_threshold: options.panic_disable_threshold,
            prefix_match_by_default: options.prefix_match_by_default,
            slow_job_secs: crate::quartz::SLOW_JOB_LIMIT_SECS,
        }
    }
}

/// 聊天记录缓存（arona.yml 的 `chatlog:` 段，实现见 [`crate::runtime::chatlog`]）。
///
/// 这件事不属于任何一家插件：QQ 的引用只认实现端本地缓存里的近期消息，旧消息要能
/// 引用回复、要能按存下来的 id 撤回，机器人都得先自己记一份。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChatLogSettings {
    /// 是否记录聊天记录（关掉后既不写库也不还原引用、不再撤回时换算 id）
    pub enable: bool,
    /// 本地留几天的聊天消息
    pub keep_days: i64,
    /// 每隔几天清一次过期记录
    pub purge_interval_days: i64,
    /// 多少分钟内的引用仍能让 OneBot 实现端直接引用（超过就改走本地还原）
    pub quote_ttl_minutes: i64,
}

impl Default for ChatLogSettings {
    fn default() -> Self {
        ChatLogSettings {
            enable: true,
            keep_days: 2,
            purge_interval_days: 4,
            quote_ttl_minutes: 30,
        }
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
    /// 全局禁用的功能 key（GUI「插件管理」页里按插件列出的那些功能开关）。
    ///
    /// 与 `disabled_plugins` 的差别只在粒度：停插件是整家停，停功能是这个插件的某一项
    /// 在**所有会话**里都不响应——包括私聊，那一分群名单管不到。
    /// 与 `group_settings.*.disabled_features` 的差别在方向：全局这一道是主开关，
    /// 群名单只能在它之上再往下减，**不能把全局关掉的 key 在某个群里翻案**。
    pub disabled_features: Vec<String>,
    /// 框架行为选项（panic 隔离阈值、命令前缀匹配）
    pub framework: FrameworkSettings,
    /// 聊天记录缓存（引用还原与按存储 id 撤回的依据）
    pub chatlog: ChatLogSettings,
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
                // 带换行的字符串不能交给 serde_yaml 排版：它出的是块标量（`|2` 再跟内容行），
                // 而那些内容行只带相对缩进，拼到本层就成了非法 YAML——框架自己生成的配置文件
                // 会读不回来。双引号转义是 YAML 兼容的，一行写得下。
                Value::String(raw) if raw.contains(['\n', '\r']) => {
                    serde_json::to_string(raw).unwrap_or_default()
                }
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
    /// 登记一个插件配置区（install 阶段调用；键重复时保留先登记的）
    pub fn register(&self, plugin: &str, section: Arc<dyn ConfigSection>) {
        let mut items = self.items.write().unwrap();
        if !items.iter().any(|(_, item)| item.key() == section.key()) {
            items.push((plugin.to_string(), section));
        }
    }

    /// 某个插件登记的配置区（按登记顺序）
    pub fn sections_of(&self, plugin: &str) -> Vec<Arc<dyn ConfigSection>> {
        self.items
            .read()
            .unwrap()
            .iter()
            .filter(|(owner, _)| owner == plugin)
            .map(|(_, section)| section.clone())
            .collect()
    }

    /// 登记了配置区的插件 id（去重，按首次登记顺序）——框架据此决定要给谁生成配置文件
    pub fn owners(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for (owner, _) in self.items.read().unwrap().iter() {
            if !ids.contains(owner) {
                ids.push(owner.clone());
            }
        }
        ids
    }

    /// 某块配置属于哪个插件
    pub fn owner(&self, key: &str) -> Option<String> {
        let items = self.items.read().unwrap();
        items
            .iter()
            .find(|(_, section)| section.key() == key)
            .map(|(owner, _)| owner.clone())
    }

    /// 已登记的插件配置区键名列表
    pub fn keys(&self) -> Vec<String> {
        self.items
            .read()
            .unwrap()
            .iter()
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

/// 框架自身认识的顶层键（除这些之外的顶层键交给插件配置区/未知键逻辑处理）。
///
/// 名单问 `AronaConfig` 的序列化器要，不手抄：`disabled_features` 那一轮就是加了字段忘了加键，
/// 于是框架自己生成的模板里写着这一行、加载器又回头报「无法识别的配置项，已忽略」。值照样生效，
/// 只有日志在撒谎——而读日志的人正是靠这句话判断该改哪。
fn generic_top_keys() -> &'static BTreeSet<String> {
    static KEYS: OnceLock<BTreeSet<String>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let value = serde_yaml::to_value(AronaConfig::default()).expect("AronaConfig 应可序列化");
        value
            .as_mapping()
            .expect("AronaConfig 序列化后应是映射")
            .keys()
            .filter_map(|key| key.as_str().map(str::to_string))
            .collect()
    })
}

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
            if generic_top_keys().contains(name) {
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
                    generic_top_keys()
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" / "),
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
    out.push_str("# 框架启动时读本文件，改完保存即自动热重载。\n");
    out.push_str("# 本文件只放框架自身的项：授权、黑名单、分群开关、插件开关、全局功能开关与聊天记录缓存。\n");
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
    out.push_str("# ==================== 全局功能开关 ====================\n");
    out.push_str("# 列在这里的功能 key 在**所有群与私聊**都不响应（key 见各插件的「功能」清单，改动即时生效）。\n");
    out.push_str("# 这是主开关：group_settings 里的群名单只能在此基础上再往下减，\n");
    out.push_str("# 不会把这里关掉的 key 在某个群里重新打开。\n");
    out.push_str(&format!(
        "disabled_features: {:?}\n",
        config.disabled_features
    ));
    out.push('\n');
    out.push_str("# ==================== 框架行为 ====================\n");
    out.push_str("# 同一插件连续 panic 多少次就自动隔离停用（0 = 只记日志、不停用）\n");
    out.push_str(&format!(
        "framework:\n  panic_disable_threshold: {}\n",
        config.framework.panic_disable_threshold
    ));
    out.push_str(
        "  # 未声明 with_prefix_match 的命令是否也允许最短前缀匹配（开着时输入 /stat 也能命中 /status）\n",
    );
    out.push_str(&format!(
        "  prefix_match_by_default: {}\n",
        config.framework.prefix_match_by_default
    ));
    out.push_str(
        "  # 单次定时任务执行超过多少秒判定为卡住：停用该插件并把说明发给 managers（0 = 关掉看门狗）\n",
    );
    out.push_str(&format!(
        "  slow_job_secs: {}\n",
        config.framework.slow_job_secs
    ));
    out.push('\n');
    out.push_str("# ==================== 聊天记录缓存 ====================\n");
    out.push_str("# QQ 的引用只认 OneBot 实现端缓存里的近期消息，久了就引用不到。\n");
    out.push_str("# 框架因此自己记一份聊天数据（data/arona/chatlog.db，图片只存原链接），\n");
    out.push_str("# 供插件做「引用回复」与按存储 id 的「撤回消息」。改动即时生效，不必重启。\n");
    out.push_str("chatlog:\n");
    out.push_str("  # 总开关：关掉后不再记账，引用还原与撤回时的 id 换算一起停\n");
    out.push_str(&format!("  enable: {}\n", config.chatlog.enable));
    out.push_str("  # 本地保留几天的聊天消息（超出的由下面的清理任务删掉）\n");
    out.push_str(&format!("  keep_days: {}\n", config.chatlog.keep_days));
    out.push_str("  # 每隔几天清一次过期记录\n");
    out.push_str(&format!(
        "  purge_interval_days: {}\n",
        config.chatlog.purge_interval_days
    ));
    out.push_str("  # 多少分钟内的引用仍由实现端直接挂原生引用，超过才用本地记录还原成文字+图片\n");
    out.push_str(&format!(
        "  quote_ttl_minutes: {}\n",
        config.chatlog.quote_ttl_minutes
    ));
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

    /// framework 段必须写得出、读得回；老文件没有这一段时按 FrameworkOptions 的默认值补，
    /// 而不是 0 / false 那种"看起来像用户显式关掉了隔离"的假值。
    #[test]
    fn framework_section_round_trips_through_the_template() {
        let file = std::env::temp_dir().join("arona-framework-section-test.yml");
        let mut config = AronaConfig::default();
        config.groups = vec![10001];
        config.framework.panic_disable_threshold = 2;
        config.framework.prefix_match_by_default = true;
        save(&file, &config).expect("写入模板失败");

        let reloaded = load(&file).expect("framework 段应能读回强类型");
        assert_eq!(reloaded.framework.panic_disable_threshold, 2);
        assert!(reloaded.framework.prefix_match_by_default);

        // 段缺失 = 用框架默认，而不是 0（0 在语义上是"只记日志、永不停用"）
        std::fs::write(&file, "groups: [10001]\n").expect("写入老格式配置失败");
        let legacy = load(&file).expect("缺少 framework 段不应导致加载失败");
        assert_eq!(
            legacy.framework.panic_disable_threshold,
            crate::framework::Framework::DEFAULT_PANIC_THRESHOLD
        );
        assert!(!legacy.framework.prefix_match_by_default);

        // 只写一个键时另一个仍按默认补，serde(default) 是逐字段生效的
        std::fs::write(&file, "framework:\n  prefix_match_by_default: true\n")
            .expect("写入片段配置失败");
        let partial = load(&file).expect("framework 段缺键不应导致加载失败");
        assert!(partial.framework.prefix_match_by_default);
        assert_eq!(
            partial.framework.panic_disable_threshold,
            crate::framework::Framework::DEFAULT_PANIC_THRESHOLD
        );
        let _ = std::fs::remove_file(&file);
    }

    /// 全局功能开关要写得出、读得回；老文件没有这一行时读成**空名单**（全放行），
    /// 绝不能缺省成"全关"——那会让升级一次就把所有插件的功能停干净。
    #[test]
    fn disabled_features_round_trips_through_the_template() {
        let file = std::env::temp_dir().join("arona-disabled-features-test.yml");
        let mut config = AronaConfig::default();
        config.disabled_features = vec!["gacha".to_string(), "tarot".to_string()];
        save(&file, &config).expect("写入模板失败");
        let reloaded = load(&file).expect("disabled_features 应能读回强类型");
        assert_eq!(
            reloaded.disabled_features,
            vec!["gacha".to_string(), "tarot".to_string()]
        );

        std::fs::write(&file, "groups: [10001]\n").expect("写入老格式配置失败");
        let legacy = load(&file).expect("缺 disabled_features 这一行不该导致加载失败");
        assert!(legacy.disabled_features.is_empty());
        let _ = std::fs::remove_file(&file);
    }

    /// 上一条只证得了「值读得回」，证不了日志不说谎：未知顶层键这一路只写日志，serde 仍把生效值
    /// 填进结构体。`disabled_features` 那一轮就是加了字段没加白名单，于是全新运行目录一启动就被
    /// 自己的模板报「无法识别的配置项，已忽略」。这里把口径钉死：**模板写得出的顶层键，加载器必须认**。
    #[test]
    fn the_template_only_writes_keys_the_loader_admits() {
        let text = template(&AronaConfig::default());
        let value: Value = serde_yaml::from_str(&text).expect("默认模板应是合法 YAML");
        let unknown: Vec<&str> = value
            .as_mapping()
            .expect("模板顶层应是映射")
            .keys()
            .filter_map(|key| key.as_str())
            .filter(|name| !generic_top_keys().contains(*name))
            .collect();
        assert!(
            unknown.is_empty(),
            "模板写出了加载器不认的顶层键: {unknown:?}"
        );
        assert!(
            generic_top_keys().contains("disabled_features"),
            "顶层 disabled_features 必须算认下的键"
        );
    }

    /// 聊天记录段要写得出、读得回；老文件没有这一段时按默认值补，而不是
    /// enable=false / keep_days=0 那种"看起来像用户显式关掉了记账"的假值。
    #[test]
    fn chatlog_section_round_trips_through_the_template() {
        let file = std::env::temp_dir().join("arona-chatlog-section-test.yml");
        let mut config = AronaConfig::default();
        config.chatlog.keep_days = 7;
        config.chatlog.quote_ttl_minutes = 10;
        save(&file, &config).expect("写入模板失败");

        let reloaded = load(&file).expect("chatlog 段应能读回强类型");
        assert!(reloaded.chatlog.enable);
        assert_eq!(reloaded.chatlog.keep_days, 7);
        assert_eq!(reloaded.chatlog.purge_interval_days, 4);
        assert_eq!(reloaded.chatlog.quote_ttl_minutes, 10);

        std::fs::write(&file, "groups: [10001]\n").expect("写入老格式配置失败");
        let legacy = load(&file).expect("缺少 chatlog 段不应导致加载失败");
        assert_eq!(legacy.chatlog, ChatLogSettings::default());
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

    /// 回归：值里带换行时不能让 serde_yaml 自己排版——它出块标量（`|2` 再跟内容行），
    /// 那些内容行只带相对缩进，拼到当前层级就是非法 YAML。实测踩过：
    /// 框架生成的插件配置里 `cutLine: |2` 那一段，框架自己读不回来。
    #[test]
    fn multiline_string_renders_as_one_escaped_line() {
        let section = typed_section::<DemoConfig>("demo");
        let filled = serde_yaml::to_value(DemoConfig {
            alert: DemoAlert {
                hour: 3,
                text: "第一行\n第二行\r第三行".to_string(),
            },
            ..Default::default()
        })
        .unwrap();
        let text = section.render(&filled);
        assert!(
            !text.contains("|2") && !text.contains("|-"),
            "不该出现块标量: {text}"
        );
        assert!(
            text.contains("text: \"第一行\\n第二行\\r第三行\""),
            "多行字符串该转义成一行: {text}"
        );
        let back: BTreeMap<String, DemoConfig> =
            serde_yaml::from_str(&text).expect("带多行值的模板该能读回");
        assert_eq!(back["demo"].alert.text, "第一行\n第二行\r第三行");
        assert_eq!(back["demo"].alert.hour, 3);
    }
}
