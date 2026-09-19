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

use crate::config::arona;
use crate::runtime::paths;
use once_cell::sync::OnceCell;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

/// 某个插件配置文件的状态
struct PluginFile {
    path: PathBuf,
    /// 顶层键 -> 原样 YAML 片段（框架不理解内容，交回插件渲染）
    sections: BTreeMap<String, Value>,
    last_modified: Option<SystemTime>,
}

static FILES: OnceCell<RwLock<BTreeMap<String, PluginFile>>> = OnceCell::new();

/// 从框架 arona.yml（或旧 onebot.yml）顶层接管的旧写法：键 -> (归属插件, 值)。
/// 先攒着，等 [`init`] 建好该插件的文件时并进去；[`init`] 之后到达的立即合并。
static PENDING: OnceCell<RwLock<BTreeMap<String, (String, Value)>>> = OnceCell::new();

fn files() -> &'static RwLock<BTreeMap<String, PluginFile>> {
    FILES.get_or_init(|| RwLock::new(BTreeMap::new()))
}

fn pending() -> &'static RwLock<BTreeMap<String, (String, Value)>> {
    PENDING.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// 某个插件的配置文件路径（框架按约定拼出来，插件也拿它做展示/诊断）
pub fn config_file(plugin: &str) -> PathBuf {
    paths::plugin_config_file(plugin)
}

/// 已加载配置文件的插件 id 列表
pub fn loaded_plugins() -> Vec<String> {
    files().read().unwrap().keys().cloned().collect()
}

/// 接管框架 arona.yml 里的旧写法：安排进对应插件的配置文件
pub fn absorb_legacy(key: &str, value: Value, plugin: &str) {
    pending()
        .write()
        .unwrap()
        .insert(key.to_string(), (plugin.to_string(), value));
    // 热重载路径（文件已经加载过了）当场合并，不然这份旧写法要到下次启动才生效
    if loaded_plugins().iter().any(|id| id == plugin) {
        merge_pending(plugin);
    }
}

/// 启动阶段：为每个登记了配置区的插件备好 `config/<id>/arona.yml` 并加载。
/// 缺文件时写完整模板；已有文件里缺的配置区（插件升级新增的）按默认值补齐，用户改过的值不动。
/// 要在 [`crate::config::arona::load`] 之后调用——旧写法的值得先被接管进来。
pub fn init() {
    for plugin in arona::section_owners() {
        let path = config_file(&plugin);
        let existed = path.exists();
        let mut file = parse(&plugin, &path).unwrap_or_else(|err| {
            crate::runtime::log::warning(format!("{err}；本插件按默认配置继续"));
            PluginFile {
                path: path.clone(),
                sections: BTreeMap::new(),
                last_modified: None,
            }
        });
        let mut changed = merge_pending_into(&mut file, &plugin);
        // 插件升级后新增的配置区：补上默认值写回，用户才会看见它带注释的模板
        let mut added: Vec<String> = Vec::new();
        for section in arona::sections_of(&plugin) {
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
            if let Err(err) = write_file(&plugin, &mut file) {
                crate::runtime::log::warning(err);
            }
        }
        file.last_modified = modified(&path);
        files().write().unwrap().insert(plugin, file);
    }
}

/// 读取某插件配置区键的原样值（未登记/文件里没有时返回 None）
pub fn section_value(key: &str) -> Option<Value> {
    let owner = arona::section_owner(key)?;
    files()
        .read()
        .unwrap()
        .get(&owner)
        .and_then(|file| file.sections.get(key))
        .cloned()
}

/// 写回某插件配置区：落到该插件自己的配置文件，并回调它的 `on_config_reload`
pub fn set_section(key: &str, value: Value) -> Result<(), String> {
    let owner = arona::section_owner(key)
        .ok_or_else(|| format!("配置区「{key}」未登记，插件 id 也没对上"))?;
    {
        let mut guard = files().write().unwrap();
        let file = guard
            .get_mut(&owner)
            .ok_or_else(|| format!("插件 {owner} 的配置文件尚未加载"))?;
        file.sections.insert(key.to_string(), value);
    }
    persist(&owner)?;
    crate::plugin::notify_config_reloaded_of(&owner);
    Ok(())
}

/// 重新读取全部插件配置文件（GUI「重载配置」用）
pub fn reload_all() {
    for plugin in loaded_plugins() {
        reload(&plugin);
    }
}

/// 文件监听：mtime 变了的插件配置重新加载并通知该插件（由框架的轮询任务调用）
pub fn poll_changed() {
    let snapshot: Vec<(String, PathBuf, Option<SystemTime>)> = {
        files()
            .read()
            .unwrap()
            .iter()
            .map(|(plugin, file)| (plugin.clone(), file.path.clone(), file.last_modified))
            .collect()
    };
    for (plugin, path, last) in snapshot {
        let current = modified(&path);
        if current.is_some() && current != last {
            if let Some(file) = files().write().unwrap().get_mut(&plugin) {
                file.last_modified = current;
            }
            reload(&plugin);
        }
    }
}

/// 重新加载某插件的配置文件；失败只记日志、保留当前配置
fn reload(plugin: &str) {
    let path = config_file(plugin);
    match parse(plugin, &path) {
        Ok(mut file) => {
            file.last_modified = modified(&path);
            files().write().unwrap().insert(plugin.to_string(), file);
            crate::runtime::log::info(format!("插件配置已重载: {}", path.display()));
        }
        Err(err) => {
            crate::runtime::log::warning(format!("{err}；保留当前插件配置"));
            if let Some(file) = files().write().unwrap().get_mut(plugin) {
                file.last_modified = modified(&path);
            }
        }
    }
    crate::plugin::notify_config_reloaded_of(plugin);
}

/// 解析插件配置文件：只认该插件登记过的顶层键
fn parse(plugin: &str, path: &Path) -> Result<PluginFile, String> {
    let mut sections = BTreeMap::new();
    if path.exists() {
        let text = read_text(path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
        match serde_yaml::from_str::<Value>(&text) {
            Ok(value) => {
                let registered = arona::sections_of(plugin)
                    .into_iter()
                    .map(|section| section.key().to_string())
                    .collect::<Vec<_>>();
                if let Some(map) = value.as_mapping() {
                    for (key, val) in map {
                        let Some(name) = key.as_str() else { continue };
                        if registered.iter().any(|k| *k == name) {
                            sections.insert(name.to_string(), val.clone());
                        } else if let Some(owner) = arona::section_owner(name) {
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

/// 把接管的旧写法并入内存对象，返回是否有改动。
/// 只消费真正并进去的键：插件文件里已经有了就把旧写法留着，等那份文件哪天空出这个键
/// （或被删掉重建）再接管，不然热重载路径上会把它悄悄吞掉。
fn merge_pending_into(file: &mut PluginFile, plugin: &str) -> bool {
    let mut guard = pending().write().unwrap();
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
fn merge_pending(plugin: &str) {
    let mut guard = files().write().unwrap();
    let Some(file) = guard.get_mut(plugin) else {
        return;
    };
    if merge_pending_into(file, plugin) {
        let _ = write_file(plugin, file);
    }
}

/// 渲染并写出插件配置文件（顺带刷新 mtime，让文件监听忽略这次自写）
fn write_file(plugin: &str, file: &mut PluginFile) -> Result<(), String> {
    let text = template(plugin, &file.sections);
    std::fs::write(&file.path, text)
        .map_err(|err| format!("写入 {} 失败: {err}", file.path.display()))?;
    file.last_modified = modified(&file.path);
    Ok(())
}

/// 写盘（set_section 之后的路径）
fn persist(plugin: &str) -> Result<(), String> {
    let mut guard = files().write().unwrap();
    let file = guard
        .get_mut(plugin)
        .ok_or_else(|| format!("插件 {plugin} 的配置文件尚未加载"))?;
    write_file(plugin, file)
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
fn template(plugin: &str, sections: &BTreeMap<String, Value>) -> String {
    let meta = crate::plugin::meta_of(plugin);
    let mut out = String::new();
    out.push_str(&format!(
        "# ==================== 插件配置: {} ====================\n",
        meta.as_ref().map(|m| m.name).unwrap_or(plugin)
    ));
    if let Some(meta) = &meta {
        if !meta.description.is_empty() {
            out.push_str(&format!("# {}\n", meta.description));
        }
    }
    out.push_str(&format!("# 插件 id: {plugin}\n"));
    out.push_str("# 修改本文件保存后自动热重载，不必重启。\n");
    out.push_str("# 框架自身的配置（群授权/黑名单/插件开关）在同目录上一级的 arona.yml。\n\n");
    let registered = arona::sections_of(plugin);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::arona::ConfigSection;
    use std::sync::{Arc, Mutex};

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

    /// 插件配置表是进程级全局：涉及它的用例必须串行
    static LOCK: Mutex<()> = Mutex::new(());

    fn serialize_demo() {
        arona::register_section("demoplugin", Arc::new(DemoSection));
    }

    /// 模板生成：文件不存在时按登记顺序写出带注释片段，且只认自己那几个键
    #[test]
    fn generates_template_and_ignores_foreign_keys() {
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        serialize_demo();
        // 约定路径就是 config/<插件>/arona.yml：测试也照这个位置放，顺带验证 helper 拼出的路径
        crate::runtime::paths::prepare();
        let path = config_file("demoplugin");
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::remove_file(&path);

        // 未登记的键与别的插件的键都只提示、不报错
        std::fs::write(&path, "demo:\n  hour: 21\nunknown_key: 1\n").expect("写入失败");
        let file = parse("demoplugin", &path).expect("解析应成功");
        assert_eq!(
            file.sections
                .get("demo")
                .and_then(|v| v.get("hour"))
                .and_then(|v| v.as_i64()),
            Some(21)
        );
        assert!(!file.sections.contains_key("unknown_key"));

        let text = template("demoplugin", &file.sections);
        assert!(
            text.contains("demo:\n  hour: 21"),
            "模板应保留用户改过的值: {text}"
        );
        assert!(
            text.contains("# 插件 id: demoplugin"),
            "模板应说明插件 id: {text}"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_config_dir("demoplugin"));
    }

    /// 升级兼容：插件新登记的配置区要补进用户已有的文件，同时不覆盖他改过的值
    #[test]
    fn backfills_newly_registered_section_into_existing_file() {
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        serialize_demo();
        arona::register_section("demoplugin", Arc::new(DemoTwoSection));

        let path = config_file("demoplugin");
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        std::fs::write(&path, "demo:\n  hour: 21\n").expect("写入失败");

        init();

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
            section_value("demotwo")
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
        init();
        assert_eq!(mtime(&path), before, "没有新东西时不该重写文件");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_config_dir("demoplugin"));
    }

    /// 旧写法迁移：框架 arona.yml 顶层的插件键被搬进 config/<id>/arona.yml
    #[test]
    fn absorbs_legacy_key_from_framework_config() {
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        serialize_demo();

        // 记下接管到的值：absorb_legacy 只是攒进 pending，init 时并进去
        absorb_legacy(
            "demo",
            serde_yaml::from_str("hour: 9").unwrap(),
            "demoplugin",
        );
        let picked = pending()
            .read()
            .unwrap()
            .get("demo")
            .map(|(owner, _)| owner.clone());
        assert_eq!(
            picked.as_deref(),
            Some("demoplugin"),
            "旧写法应登记归属插件"
        );

        let mut file = PluginFile {
            path: config_file("demoplugin"),
            sections: BTreeMap::new(),
            last_modified: None,
        };
        assert!(
            merge_pending_into(&mut file, "demoplugin"),
            "init 时应把旧写法并进插件配置"
        );
        assert!(file.sections.contains_key("demo"));
        // 归属插件对不上时不该动别人的值
        assert!(
            !merge_pending_into(&mut file, "otherplugin"),
            "别的插件不应重复消费"
        );
        // 已经并进来了就不该重复消费
        assert!(!merge_pending_into(&mut file, "demoplugin"));
    }

    /// 旧写法先攒着、插件文件里同名配置区已经存在时不吞掉：文件重建后还能接管回来
    #[test]
    fn keeps_pending_when_plugin_already_has_the_key() {
        let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        serialize_demo();
        let value: Value = serde_yaml::from_str("hour: 7").unwrap();
        {
            // 内存里已经有 demo 这一项：pending 不该被消费掉
            let mut occupied = PluginFile {
                path: config_file("demoplugin"),
                sections: BTreeMap::from([("demo".to_string(), value.clone())]),
                last_modified: None,
            };
            // 直接攒 pending：absorb_legacy 对已加载的插件会当场合并，这里只验证合并规则本身
            pending().write().unwrap().insert(
                "demo".to_string(),
                (
                    "demoplugin".to_string(),
                    serde_yaml::from_str("hour: 3").unwrap(),
                ),
            );
            assert!(
                !merge_pending_into(&mut occupied, "demoplugin"),
                "插件文件里已有的配置区应以它为准"
            );
            assert_eq!(occupied.sections.get("demo"), Some(&value));
        }
        assert!(
            pending().read().unwrap().contains_key("demo"),
            "没消费掉的旧写法要留着，配置文件重建后再接管"
        );
        let mut fresh = PluginFile {
            path: config_file("demoplugin"),
            sections: BTreeMap::new(),
            last_modified: None,
        };
        assert!(
            merge_pending_into(&mut fresh, "demoplugin"),
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
        assert!(!pending().read().unwrap().contains_key("demo"));
    }
}
