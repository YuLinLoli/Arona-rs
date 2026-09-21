//! 插件配置：每个插件一份 `config/<插件>/arona.yml`。
//!
//! 所有插件的落盘位置由框架统一规定，插件不自己挑地方：它在 install 阶段用
//! [`crate::config::arona::register_section`] 登记自己那几块配置（传自己的插件 id），
//! 框架便在 `config/<id>/arona.yml` 生成带注释的模板、把它加载成原样 YAML 片段，
//! 并在文件被改动时热重载 + 回调该插件的 `on_config_reload`。
//!
//! 插件读写自己配置的入口是 [`section_value`] / [`set_section`]（按顶层键名寻址，
//! 归属哪个插件由框架查登记表）。旧版把这些键写在框架 arona.yml 顶层，[`absorb_legacy`]
//! 会把它们搬进插件自己的文件，用户不必手改。
//!
//! 已加载的配置文件与待接管的旧写法都住在 [`ConfigStore`] 实例里（由
//! [`crate::framework::Framework`] 持有）；本模块的自由函数走进程默认实例。
use crate::config::arona::{self, SectionRegistry};
use crate::framework::Framework;
use crate::plugin::manager::PluginManager;
use crate::runtime::paths;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

/// 某个插件配置文件的状态
struct PluginFile {
    path: PathBuf,
    /// 顶层键 -> 原样 YAML 片段（框架不理解内容，交回插件渲染）
    sections: BTreeMap<String, Value>,
    last_modified: Option<SystemTime>,
}

/// 一套插件配置文件表 + 待接管的旧写法
pub struct ConfigStore {
    /// 配置区的归属与渲染器（决定要给谁生成文件、文件里认哪些键）
    sections: Arc<SectionRegistry>,
    /// 回写配置后要通知归属插件的 `on_config_reload`
    plugins: Arc<PluginManager>,
    files: RwLock<BTreeMap<String, PluginFile>>,
    /// 从框架 arona.yml（或旧 onebot.yml）顶层接管的旧写法：键 -> (归属插件, 值)。
    /// 先攒着，等 [`ConfigStore::init`] 建好该插件的文件时并进去；之后到达的立即合并。
    pending: RwLock<BTreeMap<String, (String, Value)>>,
}

impl ConfigStore {
    pub(crate) fn new(sections: Arc<SectionRegistry>, plugins: Arc<PluginManager>) -> ConfigStore {
        ConfigStore {
            sections,
            plugins,
            files: RwLock::new(BTreeMap::new()),
            pending: RwLock::new(BTreeMap::new()),
        }
    }

    /// 某个插件的配置文件路径（框架按约定拼出来，插件也拿它做展示/诊断）
    pub fn config_file(&self, plugin: &str) -> PathBuf {
        paths::plugin_config_file(plugin)
    }

    /// 已加载配置文件的插件 id 列表
    pub fn loaded_plugins(&self) -> Vec<String> {
        self.files.read().unwrap().keys().cloned().collect()
    }

    /// 接管框架 arona.yml 里的旧写法：安排进对应插件的配置文件
    pub fn absorb_legacy(&self, key: &str, value: Value, plugin: &str) {
        self.pending
            .write()
            .unwrap()
            .insert(key.to_string(), (plugin.to_string(), value));
        // 热重载路径（文件已经加载过了）当场合并，不然这份旧写法要到下次启动才生效
        if self.loaded_plugins().iter().any(|id| id == plugin) {
            self.merge_pending(plugin);
        }
    }

    /// 启动阶段：为每个登记了配置区的插件备好 `config/<id>/arona.yml` 并加载。
    /// 缺文件时写完整模板；已有文件里缺的配置区（插件升级新增的）按默认值补齐，用户改过的值不动。
    /// 要在 [`crate::config::arona::load`] 之后调用——旧写法的值得先被接管进来。
    pub fn init(&self) {
        for plugin in self.sections.owners() {
            let path = self.config_file(&plugin);
            let existed = path.exists();
            let mut file = parse(&self.sections, &plugin, &path).unwrap_or_else(|err| {
                crate::runtime::log::warning(format!("{err}；本插件按默认配置继续"));
                self.placeholder_file(&plugin)
            });
            let mut changed = self.merge_pending_into(&mut file, &plugin);
            // 插件升级后新增的配置区：补上默认值写回，用户才会看见它带注释的模板
            let mut added: Vec<String> = Vec::new();
            for section in self.sections.sections_of(&plugin) {
                if !file.sections.contains_key(section.key()) {
                    file.sections
                        .insert(section.key().to_string(), section.default_value());
                    added.push(section.key().to_string());
                }
            }
            changed = changed || !added.is_empty();
            if changed || !existed {
                if !existed {
                    crate::runtime::log::info(format!(
                        "已生成插件配置文件: {}（改完保存即热重载）",
                        path.display()
                    ));
                } else if !added.is_empty() {
                    crate::runtime::log::info(format!(
                        "插件 {plugin} 的配置补齐了新增项: {}",
                        added.join(" / ")
                    ));
                }
                if let Err(err) = self.write_file(&plugin, &mut file) {
                    crate::runtime::log::warning(err);
                }
            }
            file.last_modified = modified(&path);
            self.files.write().unwrap().insert(plugin, file);
        }
    }

    /// 读取某插件配置区键的原样值（未登记/文件里没有时返回 None）
    pub fn section_value(&self, key: &str) -> Option<Value> {
        let owner = self.sections.owner(key)?;
        self.value_of(&owner, key)
    }

    /// 按 (插件, 键) 读原样值：配置文件本就按插件分家，这里不依赖全局键归属表
    pub fn value_of(&self, plugin: &str, key: &str) -> Option<Value> {
        self.files
            .read()
            .unwrap()
            .get(plugin)
            .and_then(|file| file.sections.get(key))
            .cloned()
    }

    /// 写回某插件配置区：落到该插件自己的配置文件，并回调它的 `on_config_reload`
    pub fn set_section(&self, key: &str, value: Value) -> Result<(), String> {
        let owner = self
            .sections
            .owner(key)
            .ok_or_else(|| format!("配置区「{key}」未登记，插件 id 也没对上"))?;
        self.set_plugin_section(&owner, key, value)
    }

    /// 按 (插件, 键) 写回：写进 `config/<插件>/arona.yml` 并回调该插件
    pub fn set_plugin_section(&self, plugin: &str, key: &str, value: Value) -> Result<(), String> {
        {
            let mut guard = self.files.write().unwrap();
            let file = guard
                .get_mut(plugin)
                .ok_or_else(|| format!("插件 {plugin} 的配置文件尚未加载"))?;
            file.sections.insert(key.to_string(), value);
        }
        self.persist(plugin)?;
        self.plugins.notify_config_reloaded(Some(plugin));
        Ok(())
    }

    /// 重新读取全部插件配置文件（GUI「重载配置」用）
    pub fn reload_all(&self) {
        for plugin in self.loaded_plugins() {
            self.reload(&plugin);
        }
    }

    /// 文件监听：mtime 变了的插件配置重新加载并通知该插件（由框架的轮询任务调用）
    pub fn poll_changed(&self) {
        let snapshot: Vec<(String, PathBuf, Option<SystemTime>)> = {
            self.files
                .read()
                .unwrap()
                .iter()
                .map(|(plugin, file)| (plugin.clone(), file.path.clone(), file.last_modified))
                .collect()
        };
        for (plugin, path, last) in snapshot {
            let current = modified(&path);
            if current.is_some() && current != last {
                if let Some(file) = self.files.write().unwrap().get_mut(&plugin) {
                    file.last_modified = current;
                }
                self.reload(&plugin);
            }
        }
    }

    /// 重新加载某插件的配置文件；失败只记日志、保留当前配置
    pub fn reload(&self, plugin: &str) {
        let path = self.config_file(plugin);
        match parse(&self.sections, plugin, &path) {
            Ok(mut file) => {
                file.last_modified = modified(&path);
                self.files.write().unwrap().insert(plugin.to_string(), file);
                crate::runtime::log::info(format!("插件配置已重载: {}", path.display()));
            }
            Err(err) => {
                crate::runtime::log::warning(format!("{err}；保留当前插件配置"));
                if let Some(file) = self.files.write().unwrap().get_mut(plugin) {
                    file.last_modified = modified(&path);
                }
            }
        }
        self.plugins.notify_config_reloaded(Some(plugin));
    }

    /// 把接管的旧写法并入内存对象，返回是否有改动。
    /// 只消费真正并进去的键：插件文件里已经有了就把旧写法留着，等那份文件哪天空出这个键
    /// （或被删掉重建）再接管，不然热重载路径上会把它悄悄吞掉。
    fn merge_pending_into(&self, file: &mut PluginFile, plugin: &str) -> bool {
        let mut guard = self.pending.write().unwrap();
        let mine: Vec<(String, Value)> = guard
            .iter()
            .filter(|(_, (owner, _))| owner == plugin)
            .filter(|(key, _)| !file.sections.contains_key(*key))
            .map(|(key, (_, value))| (key.clone(), value.clone()))
            .collect();
        let mut changed = false;
        for (key, value) in mine {
            guard.remove(&key);
            file.sections.insert(key, value);
            changed = true;
        }
        changed
    }

    /// 接管到的旧写法合并进已加载的插件文件并落盘
    fn merge_pending(&self, plugin: &str) {
        let mut guard = self.files.write().unwrap();
        let Some(file) = guard.get_mut(plugin) else {
            return;
        };
        if self.merge_pending_into(file, plugin) {
            let _ = self.write_file(plugin, file);
        }
    }

    /// 渲染并写出插件配置文件（顺带刷新 mtime，让文件监听忽略这次自写）
    fn write_file(&self, plugin: &str, file: &mut PluginFile) -> Result<(), String> {
        let meta = self.plugins.find(plugin).map(|p| p.meta().clone());
        let text = template(&self.sections, meta.as_ref(), plugin, &file.sections);
        std::fs::write(&file.path, text)
            .map_err(|err| format!("写入 {} 失败: {err}", file.path.display()))?;
        file.last_modified = modified(&file.path);
        Ok(())
    }

    /// 写盘（set_section 之后的路径）
    fn persist(&self, plugin: &str) -> Result<(), String> {
        let mut guard = self.files.write().unwrap();
        let file = guard
            .get_mut(plugin)
            .ok_or_else(|| format!("插件 {plugin} 的配置文件尚未加载"))?;
        self.write_file(plugin, file)
    }

    /// 待接管的旧写法（键 -> 归属插件），测试与诊断用
    pub fn pending_owner(&self, key: &str) -> Option<String> {
        self.pending
            .read()
            .unwrap()
            .get(key)
            .map(|(owner, _)| owner.clone())
    }

    pub fn has_pending(&self, key: &str) -> bool {
        self.pending.read().unwrap().contains_key(key)
    }

    /// 直接攒一条待接管的旧写法（[`ConfigStore::absorb_legacy`] 对已加载的插件会当场合并；
    /// 这个入口只写进 pending，便于单测验证合并规则）
    pub fn stage_pending(&self, key: &str, plugin: &str, value: Value) {
        self.pending
            .write()
            .unwrap()
            .insert(key.to_string(), (plugin.to_string(), value));
    }

    /// 某个插件当前已加载的配置区内容（测试与诊断用）
    pub fn sections_of(&self, plugin: &str) -> Option<BTreeMap<String, Value>> {
        self.files
            .read()
            .unwrap()
            .get(plugin)
            .map(|f| f.sections.clone())
    }

    /// 一个还没加载过的空文件对象（解析失败的降级路径、测试用）
    fn placeholder_file(&self, plugin: &str) -> PluginFile {
        PluginFile {
            path: self.config_file(plugin),
            sections: BTreeMap::new(),
            last_modified: None,
        }
    }

    /// 把某插件的配置区整体塞进内存对象（测试用；不落盘）
    pub fn set_sections(&self, plugin: &str, sections: BTreeMap<String, Value>) {
        let path = self.config_file(plugin);
        self.files.write().unwrap().insert(
            plugin.to_string(),
            PluginFile {
                path,
                sections,
                last_modified: None,
            },
        );
    }
}

/// 解析插件配置文件：只认该插件登记过的顶层键
fn parse(registry: &SectionRegistry, plugin: &str, path: &Path) -> Result<PluginFile, String> {
    let mut sections = BTreeMap::new();
    if path.exists() {
        let text = read_text(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
        match serde_yaml::from_str::<Value>(&text) {
            Ok(value) => {
                let registered = registry
                    .sections_of(plugin)
                    .into_iter()
                    .map(|section| section.key().to_string())
                    .collect::<Vec<_>>();
                if let Some(map) = value.as_mapping() {
                    for (key, val) in map {
                        let Some(name) = key.as_str() else { continue };
                        if registered.iter().any(|k| *k == name) {
                            sections.insert(name.to_string(), val.clone());
                        } else if let Some(owner) = registry.owner(name) {
                            crate::runtime::log::warning(format!(
                                "{} 里的「{name}」属于插件 {owner}，写在 {plugin} 的配置里不会生效，已忽略",
                                path.display()
                            ));
                        } else {
                            crate::runtime::log::warning(format!(
                                "{} 存在无法识别的配置项「{name}」，已忽略；本插件可用配置项: {}",
                                path.display(),
                                if registered.is_empty() {
                                    "(无)".to_string()
                                } else {
                                    registered.join(" / ")
                                }
                            ));
                        }
                    }
                }
            }
            Err(err) => {
                return Err(format!(
                    "{} 解析失败，请检查 YAML 格式: {err}",
                    path.display()
                ));
            }
        }
    }
    Ok(PluginFile {
        path: path.to_path_buf(),
        sections,
        last_modified: None,
    })
}

fn read_text(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
}

/// 生成插件配置文件的完整文本（带注释模板）
fn template(
    registry: &SectionRegistry,
    meta: Option<&crate::plugin::description::PluginMeta>,
    plugin: &str,
    sections: &BTreeMap<String, Value>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# ==================== 插件配置: {} ====================\n",
        meta.map(|m| m.name).unwrap_or(plugin)
    ));
    if let Some(meta) = &meta {
        if !meta.description.is_empty() {
            out.push_str(&format!("# {}\n", meta.description));
        }
    }
    out.push_str(&format!("# 插件 id: {plugin}\n"));
    out.push_str("# 修改本文件保存后自动热重载，不必重启。\n");
    out.push_str("# 框架自身的配置（群授权/黑名单/插件开关）在同目录上一级的 arona.yml。\n\n");
    let registered = registry.sections_of(plugin);
    if registered.is_empty() {
        out.push_str("# 本插件没有需要用户配置的项目。\n");
        return out;
    }
    for (index, section) in registered.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let value = sections
            .get(section.key())
            .cloned()
            .unwrap_or_else(|| section.default_value());
        out.push_str(&section.render(&value));
    }
    out
}

fn store() -> &'static ConfigStore {
    Framework::global().configs()
}

/// 某个插件的配置文件路径（框架按约定拼出来，插件也拿它做展示/诊断）
pub fn config_file(plugin: &str) -> PathBuf {
    store().config_file(plugin)
}

/// 已加载配置文件的插件 id 列表
pub fn loaded_plugins() -> Vec<String> {
    store().loaded_plugins()
}

/// 接管框架 arona.yml 里的旧写法：安排进对应插件的配置文件
pub fn absorb_legacy(key: &str, value: Value, plugin: &str) {
    store().absorb_legacy(key, value, plugin);
}

/// 启动阶段：为每个登记了配置区的插件备好配置文件并加载
pub fn init() {
    store().init();
}

/// 读取某插件配置区键的原样值（未登记/文件里没有时返回 None）
pub fn section_value(key: &str) -> Option<Value> {
    store().section_value(key)
}

/// 写回某插件配置区：落到该插件自己的配置文件，并回调它的 `on_config_reload`
pub fn set_section(key: &str, value: Value) -> Result<(), String> {
    store().set_section(key, value)
}

/// 按 (插件, 键) 写回：写进 `config/<插件>/arona.yml` 并回调该插件
pub fn set_plugin_section(plugin: &str, key: &str, value: Value) -> Result<(), String> {
    store().set_plugin_section(plugin, key, value)
}

/// 重新读取全部插件配置文件（GUI「重载配置」用）
pub fn reload_all() {
    store().reload_all();
}

/// 文件监听：mtime 变了的插件配置重新加载并通知该插件（由框架的轮询任务调用）
pub fn poll_changed() {
    store().poll_changed();
}

/// 强类型配置项（对应 mirai-console 的 `ConfigKey<T>` 委托）
///
/// 插件用 `ctx.config::<T>("notify")` 拿到它，之后读写都是自己的类型，
/// 落盘位置固定在自己的 `config/<插件>/arona.yml`：
///
/// ```ignore
/// let notify = ctx.config::<NotifyConfig>("notify");
/// let hour = notify.get().every_day_hour;
/// notify.update(|config| config.every_day_hour = 20)?;   // 写回 + 热重载回调
/// ```
pub struct ConfigEntry<T: arona::PluginConfig> {
    store: Arc<ConfigStore>,
    plugin: String,
    key: String,
    marker: std::marker::PhantomData<T>,
}

impl<T: arona::PluginConfig> Clone for ConfigEntry<T> {
    fn clone(&self) -> Self {
        ConfigEntry {
            store: self.store.clone(),
            plugin: self.plugin.clone(),
            key: self.key.clone(),
            marker: std::marker::PhantomData,
        }
    }
}

impl<T: arona::PluginConfig> ConfigEntry<T> {
    /// 指向进程默认实例里某个插件配置文件的一块配置。`plugin` 填自己的 `meta().id`。
    pub fn new(plugin: &str, key: &str) -> ConfigEntry<T> {
        ConfigEntry {
            store: Framework::global().configs().clone(),
            plugin: plugin.to_string(),
            key: key.to_string(),
            marker: std::marker::PhantomData,
        }
    }

    /// 指向指定框架实例的配置（插件走 `ctx.config::<T>()`，一般不必自己调）
    pub fn in_store(store: &Arc<ConfigStore>, plugin: &str, key: &str) -> ConfigEntry<T> {
        ConfigEntry {
            store: store.clone(),
            plugin: plugin.to_string(),
            key: key.to_string(),
            marker: std::marker::PhantomData,
        }
    }

    /// 配置区键名（文件里的顶层键）
    pub fn key(&self) -> &str {
        &self.key
    }

    /// 所属插件 id
    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// 所在文件 `config/<插件>/arona.yml`（展示/诊断用）
    pub fn file(&self) -> PathBuf {
        self.store.config_file(&self.plugin)
    }

    /// 当前值：文件里没有这一区、或格式不符时回退 `T::default()`
    pub fn get(&self) -> T {
        self.try_get().unwrap_or_default()
    }

    /// 严格读取：解析失败返回原因（`get` 静默回退默认值，适合运行期；命令回显用户改坏的
    /// 配置时用这个）
    pub fn try_get(&self) -> Result<T, String> {
        match self.store.value_of(&self.plugin, &self.key) {
            None => Ok(T::default()),
            Some(value) => serde_yaml::from_value(value)
                .map_err(|err| format!("配置区「{}」格式不正确: {err}", self.key)),
        }
    }

    /// 整区写回（落盘 + 回调本插件的 `on_config_reload`）
    pub fn set(&self, value: &T) -> Result<(), String> {
        let node = serde_yaml::to_value(value)
            .map_err(|err| format!("序列化「{}」失败: {err}", self.key))?;
        self.store.set_plugin_section(&self.plugin, &self.key, node)
    }

    /// 读—改—写（mirai 的 `configManager[key] = value` 语义）
    pub fn update(&self, edit: impl FnOnce(&mut T)) -> Result<(), String> {
        let mut current = self.get();
        edit(&mut current);
        self.set(&current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::arona::ConfigSection;

    /// 测试用的最简配置区
    struct DemoSection;

    impl ConfigSection for DemoSection {
        fn key(&self) -> &'static str {
            "demo"
        }
        fn default_value(&self) -> Value {
            serde_yaml::from_str("hour: 8").expect("默认值应为合法 YAML")
        }
        fn render(&self, value: &Value) -> String {
            let hour = value.get("hour").and_then(|v| v.as_i64()).unwrap_or(8);
            format!("# 演示配置区\ndemo:\n  hour: {hour}\n")
        }
    }

    /// 模拟「插件升级后新登记的配置区」
    struct DemoTwoSection;

    impl ConfigSection for DemoTwoSection {
        fn key(&self) -> &'static str {
            "demotwo"
        }
        fn default_value(&self) -> Value {
            serde_yaml::from_str("enable: true").expect("默认值应为合法 YAML")
        }
        fn render(&self, value: &Value) -> String {
            let enable = value
                .get("enable")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            format!("# 升级新增的配置区\ndemotwo:\n  enable: {enable}\n")
        }
    }

    /// 一套隔离的配置登记表 + 配置文件表。
    /// `plugin` 每条用例给不同的值：落盘路径按 `config/<插件>/arona.yml` 分家，
    /// 各自的临时文件互不相干，用例可以并行跑。
    fn demo_store(plugin: &str) -> (Arc<SectionRegistry>, Arc<ConfigStore>) {
        let sections = Arc::new(SectionRegistry::default());
        let store = Arc::new(ConfigStore::new(sections.clone(), PluginManager::new()));
        store.sections.register(plugin, Arc::new(DemoSection));
        (sections, store)
    }

    /// 模板生成：文件不存在时按登记顺序写出带注释片段，且只认自己那几个键
    #[test]
    fn generates_template_and_ignores_foreign_keys() {
        const PLUGIN: &str = "demotemplate";
        let (sections, store) = demo_store(PLUGIN);
        // 约定路径就是 config/<插件>/arona.yml：测试也照这个位置放，顺带验证 helper 拼出的路径
        crate::runtime::paths::prepare();
        let path = store.config_file(PLUGIN);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::remove_file(&path);

        // 未登记的键与别的插件的键都只提示、不报错
        std::fs::write(&path, "demo:\n  hour: 21\nunknown_key: 1\n").expect("写入失败");
        let file = parse(&sections, PLUGIN, &path).expect("解析应成功");
        assert_eq!(
            file.sections
                .get("demo")
                .and_then(|v| v.get("hour"))
                .and_then(|v| v.as_i64()),
            Some(21)
        );
        assert!(!file.sections.contains_key("unknown_key"));

        let meta = store.plugins.find(PLUGIN).map(|p| p.meta().clone());
        let text = template(&sections, meta.as_ref(), PLUGIN, &file.sections);
        assert!(
            text.contains("demo:\n  hour: 21"),
            "模板应保留用户改过的值: {text}"
        );
        assert!(
            text.contains(&format!("# 插件 id: {PLUGIN}")),
            "模板应说明插件 id: {text}"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(paths::plugin_config_dir(PLUGIN));
    }

    /// 升级兼容：插件新登记的配置区要补进用户已有的文件，同时不覆盖他改过的值
    #[test]
    fn backfills_newly_registered_section_into_existing_file() {
        const PLUGIN: &str = "demoupgrade";
        let (sections, store) = demo_store(PLUGIN);
        sections.register(PLUGIN, Arc::new(DemoTwoSection));

        let path = store.config_file(PLUGIN);
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        std::fs::write(&path, "demo:\n  hour: 21\n").expect("写入失败");

        store.init();

        let text = std::fs::read_to_string(&path).expect("补齐后应落盘");
        assert!(
            text.contains("demo:\n  hour: 21"),
            "用户改过的值不该被默认值覆盖: {text}"
        );
        assert!(
            text.contains("demotwo:\n  enable: true"),
            "新登记的配置区没补进文件: {text}"
        );
        assert_eq!(
            store
                .section_value("demotwo")
                .and_then(|v| v.get("enable").cloned())
                .and_then(|v| v.as_bool()),
            Some(true),
            "补齐的配置区要能立刻读到"
        );
        // 再 init 一次不该继续改动（内容已稳定）
        fn mtime(path: &Path) -> Option<SystemTime> {
            std::fs::metadata(path).ok()?.modified().ok()
        }
        let before = mtime(&path);
        std::thread::sleep(std::time::Duration::from_millis(20));
        store.init();
        assert_eq!(mtime(&path), before, "没有新东西时不该重写文件");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(paths::plugin_config_dir(PLUGIN));
    }

    /// 旧写法迁移：框架 arona.yml 顶层的插件键被搬进 config/<id>/arona.yml
    #[test]
    fn absorbs_legacy_key_from_framework_config() {
        const PLUGIN: &str = "demolegacy";
        let (_sections, store) = demo_store(PLUGIN);
        // 记下接管到的值：absorb_legacy 只是攒进 pending，init 时并进去
        store.absorb_legacy("demo", serde_yaml::from_str("hour: 9").unwrap(), PLUGIN);
        assert_eq!(
            store.pending_owner("demo").as_deref(),
            Some(PLUGIN),
            "旧写法应登记归属插件"
        );

        let mut file = store.placeholder_file(PLUGIN);
        assert!(
            store.merge_pending_into(&mut file, PLUGIN),
            "init 时应把旧写法并进插件配置"
        );
        assert!(file.sections.contains_key("demo"));
        // 归属插件对不上时不该动别人的值
        assert!(
            !store.merge_pending_into(&mut file, "otherplugin"),
            "别的插件不应重复消费"
        );
        // 已经并进来了就不该重复消费
        assert!(!store.merge_pending_into(&mut file, PLUGIN));
    }

    /// 旧写法先攒着、插件文件里同名配置区已经存在时不吞掉：文件重建后还能接管回来
    #[test]
    fn keeps_pending_when_plugin_already_has_the_key() {
        const PLUGIN: &str = "demopending";
        let (_sections, store) = demo_store(PLUGIN);
        let value: Value = serde_yaml::from_str("hour: 7").unwrap();
        // 内存里已经有 demo 这一项：pending 不该被消费掉
        let mut occupied = store.placeholder_file(PLUGIN);
        occupied.sections.insert("demo".to_string(), value.clone());
        // 直接攒 pending：absorb_legacy 对已加载的插件会当场合并，这里只验证合并规则本身
        store.stage_pending("demo", PLUGIN, serde_yaml::from_str("hour: 3").unwrap());
        assert!(
            !store.merge_pending_into(&mut occupied, PLUGIN),
            "插件文件里已有的配置区应以它为准"
        );
        assert_eq!(occupied.sections.get("demo"), Some(&value));
        assert!(
            store.has_pending("demo"),
            "没消费掉的旧写法要留着，配置文件重建后再接管"
        );
        let mut fresh = store.placeholder_file(PLUGIN);
        assert!(
            store.merge_pending_into(&mut fresh, PLUGIN),
            "重建配置文件后旧写法应接管"
        );
        assert_eq!(
            fresh
                .sections
                .get("demo")
                .and_then(|v| v.get("hour"))
                .and_then(|v| v.as_i64()),
            Some(3)
        );
        assert!(!store.has_pending("demo"));
    }

    /// 隔离性：另一套框架实例看不见这套登记的配置区与文件
    #[test]
    fn stores_are_isolated_per_framework() {
        const PLUGIN: &str = "demoisolated";
        let (_sections, store) = demo_store(PLUGIN);
        store.set_sections(PLUGIN, BTreeMap::from([("demo".to_string(), Value::Null)]));
        assert!(store.section_value("demo").is_some());
        assert!(store.sections_of(PLUGIN).is_some());
        let other = ConfigStore::new(Arc::new(SectionRegistry::default()), PluginManager::new());
        assert!(other.section_value("demo").is_none());
        assert!(other.sections_of(PLUGIN).is_none());
        assert!(other.loaded_plugins().is_empty());
    }
}
