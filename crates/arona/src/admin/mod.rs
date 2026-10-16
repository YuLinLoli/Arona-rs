//! 管理面板后端（GUI 与命令行共用）
//! 负责：群列表/群成员查询（走 OneBot API）、分群功能开关与黑名单读写、
//! OneBot 连接配置读写与热重载。
use crate::config::onebot::{self, ConnectionType, OneBotConfig};
use crate::config::settings;
use crate::onebot::application;
use crate::onebot::model::OneBotActionResponse;
use crate::onebot::protocol;
use crate::runtime::config as runtime_config;
use crate::runtime::paths;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

/// 群信息（OneBot get_group_list 与本地配置合并）
#[derive(Clone, Debug)]
pub struct GroupInfo {
    pub group_id: i64,
    pub name: String,
    pub member_count: i64,
    /// 机器人是否响应该群
    pub enabled: bool,
    /// arona.yml 的 groups 为空时表示响应所有群
    pub all_groups: bool,
    /// 该群黑名单人数
    pub blacklist_count: usize,
    /// 该群关闭的功能数
    pub disabled_count: usize,
    /// 是否只来自本地配置（OneBot 群列表里没有，例如机器人已退群）
    pub local_only: bool,
}

/// 群成员信息
#[derive(Clone, Debug)]
pub struct MemberInfo {
    pub user_id: i64,
    pub nickname: String,
    pub card: String,
    pub role: String,
    pub level: String,
    /// 在该群黑名单中
    pub blacklisted: bool,
    /// 在全局黑名单中
    pub global_blacklisted: bool,
    pub is_manager: bool,
}

/// 连接展示信息
#[derive(Clone, Debug)]
pub struct ConnectionInfo {
    pub key: String,
    pub type_key: String,
    pub type_name: String,
    pub address: String,
    pub enable: bool,
}

/// 某个群在会话展示板上的行（群列表那一行的预览与「插件管理」页的详情都用它）。每一项是
/// `(提供它的插件展示名, 一行)`；展示名解析不到时退回插件 id。
///
/// 框架这边只搬运，不解释：标题与条目文本都是插件经
/// [`HostBridge::views`](crate::plugin::abi::HostBridge::views) 交上来的，
/// 所以这里既不认识"UP 主"也不认识"番剧"。
pub fn peer_rows(group_id: i64) -> Vec<(String, crate::views::ViewRow)> {
    let names = crate::plugin::metas();
    crate::framework::Framework::global()
        .views()
        .rows_of(group_id)
        .into_iter()
        .map(|(plugin, row)| {
            let display = names
                .iter()
                .find(|meta| meta.id == plugin)
                .map(|meta| meta.name.to_string())
                .unwrap_or(plugin);
            (display, row)
        })
        .collect()
}

/// 某个插件在所有群交上来的条目（GUI「插件管理」页详情）。同样只搬运不解释。
pub fn plugin_rows(plugin: &str) -> Vec<(i64, crate::views::ViewRow)> {
    crate::framework::Framework::global()
        .views()
        .rows_of_plugin(plugin)
}

/// 一个能在 GUI 里直接改的插件配置项：`path` 是点号分隔的键路径（首段是区名），
/// `value` 是文件里的原样值。
pub struct Field {
    pub path: String,
    pub value: serde_yaml::Value,
}

/// 摊平某插件配置里的标量叶子，给 GUI 生成控件。
///
/// 框架仍然不认识任何插件的键名，这里只有两条框架自己的规矩：
///
/// 1. 只摊标量（布尔 / 数字 / 字符串），表与数组交给插件的指令或手改 YAML——
///    没有类型信息就没有可信的控件；含换行的字符串也挡掉，单行框会把多行模板改坏。
/// 2. **键名像凭据的那一个叶子一律不摊**（`cookie`/`token`/`secret`/`password`/`key`/`appid`/`ticket`）。
///    这条是框架的安全线不是插件语义：这类值一旦显示出来就会进截图与录屏。
///    挡的是那一个键本身，不牵连兄弟——`account.cookie` 不上屏，`account.uid` 照改。
pub fn plugin_fields(plugin: &str) -> Vec<Field> {
    let mut out = Vec::new();
    let Some(sections) = crate::config::plugin_config::plugin_sections(plugin) else {
        return out;
    };
    for (section, value) in sections {
        flatten_fields(&section, &value, &mut out);
    }
    out
}

fn is_credential(key: &str) -> bool {
    let low = key.to_lowercase();
    [
        "cookie", "token", "secret", "password", "passwd", "key", "appid", "ticket",
    ]
    .iter()
    .any(|needle| low.contains(needle))
}

fn flatten_fields(prefix: &str, node: &serde_yaml::Value, out: &mut Vec<Field>) {
    match node {
        serde_yaml::Value::Mapping(map) => {
            for (key, value) in map {
                let Some(name) = key.as_str() else { continue };
                if is_credential(name) {
                    continue;
                }
                flatten_fields(&format!("{prefix}.{name}"), value, out);
            }
        }
        // 空值与列表不给控件：填错了没有类型可校验，列表更是整段结构
        serde_yaml::Value::Null | serde_yaml::Value::Sequence(_) => {}
        scalar => {
            let is_multiline = scalar.as_str().is_some_and(|text| text.contains('\n'));
            if !is_multiline {
                out.push(Field {
                    path: prefix.to_string(),
                    value: scalar.clone(),
                });
            }
        }
    }
}

/// 写回一个插件配置项：读出它所在的那一整区 → 只改这一条路径 → 整区写回。
/// 落盘走 [`plugin_config::set_plugin_section`]，注释由插件自己的模板重新生成，
/// 写完即回调插件的 `on_config_reload` 热生效，不需要重启。
pub fn set_plugin_field(
    plugin: &str,
    path: &str,
    value: serde_yaml::Value,
) -> Result<String, String> {
    let mut keys = path.split('.');
    let section = keys
        .next()
        .filter(|first| !first.is_empty())
        .ok_or("键路径是空的，不知道写去哪一区")?;
    let mut root = crate::config::plugin_config::plugin_section_value(plugin, section)
        .ok_or_else(|| format!("插件 {plugin} 的 {section} 区还没加载"))?;
    patch_path(&mut root, keys, value)?;
    crate::config::plugin_config::set_plugin_section(plugin, section, root)?;
    Ok(format!("已写入 {path}"))
}

/// 沿着一串键名走到叶子并赋值，中间缺的表顺手补上。抽成纯函数是为了能单独验落点。
fn patch_path<'a>(
    root: &mut serde_yaml::Value,
    keys: impl Iterator<Item = &'a str>,
    value: serde_yaml::Value,
) -> Result<(), String> {
    let mut node = root;
    for key in keys {
        if node.is_null() {
            *node = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        }
        let map = node
            .as_mapping_mut()
            .ok_or_else(|| "这一层的上一层不是表，写不进去".to_string())?;
        node = map
            .entry(serde_yaml::Value::String(key.to_string()))
            .or_insert(serde_yaml::Value::Null);
    }
    *node = value;
    Ok(())
}

/// 登记一个可分群开关的功能：插件在 install 阶段调用，GUI 的「功能开关」页会自动列出。
/// 这就是框架留给插件的权限/开关接口——功能属于插件，开关与黑名单的存取由框架统一管理。
///
/// `plugin` 传自己的 `meta().id`：插件被整体禁用（全局或某个群）时，它名下功能一起停掉。
pub fn register_feature(feature: runtime_config::Feature, plugin: &str) {
    runtime_config::register_feature(feature, plugin);
}

// ==================== 插件管理 ====================

/// 插件条目（GUI「插件管理」页）
#[derive(Clone, Debug)]
pub struct PluginInfo {
    /// 插件 id：目录名与 `disabled_plugins` 里的写法
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    /// 全局启用状态（框架 arona.yml 的 disabled_plugins）
    pub enabled: bool,
    /// 该插件是否按群生效（`PluginMeta::group_scoped`）。false 时这一页不画「启用的群」，
    /// 因为分群停用名单对它不起作用——摆一排勾了也没用的复选框，只会让人以为关掉就能拦住它
    pub group_scoped: bool,
    /// 该插件登记的功能开关
    pub features: Vec<runtime_config::Feature>,
    /// 该插件订阅的事件范围中文名（消息/通知:成员进群/请求/元事件）
    pub hooks: Vec<String>,
    /// 插件目录 plugins/<id>/
    pub plugin_dir: PathBuf,
    /// 配置文件 config/<id>/arona.yml（没有登记配置区时文件不存在）
    pub config_file: PathBuf,
    /// 数据目录 data/<id>/
    pub data_dir: PathBuf,
}

/// 已注册插件的概览列表
pub fn plugin_list() -> Vec<PluginInfo> {
    let subscriptions = crate::onebot::hooks::subscriptions();
    crate::plugin::metas()
        .into_iter()
        .map(|meta| {
            let hooks = subscriptions
                .iter()
                .find(|(plugin, _)| *plugin == meta.id)
                .map(|(_, kinds)| kinds.clone())
                .unwrap_or_default();
            PluginInfo {
                id: meta.id.to_string(),
                name: meta.name.to_string(),
                version: meta.version.to_string(),
                description: meta.description.to_string(),
                enabled: runtime_config::plugin_enabled(meta.id),
                group_scoped: meta.group_scoped,
                features: runtime_config::features_of(meta.id),
                hooks,
                plugin_dir: paths::plugin_dir(meta.id),
                config_file: paths::plugin_config_file(meta.id),
                data_dir: paths::plugin_data_dir(meta.id),
            }
        })
        .collect()
}

/// 全局启用/停用一个插件（按 id）：写配置文件后立即协调生命周期（停用 stop()、启用补 configure+start）
pub fn set_plugin_enabled(plugin: &str, enabled: bool) -> Result<String, String> {
    settings::set_plugin_enabled(plugin, enabled)?;
    Ok(format!(
        "插件 {plugin} 已{}（写入 {}）",
        if enabled { "启用" } else { "禁用" },
        arona_file()
    ))
}

/// 某个群里启用/停用插件（按 id；只影响该群的路由，不动插件后台任务）。
/// 声明过不按群生效的插件在这里直接拒写：GUI 不给它画那一格，别的入口也不该偷偷写进名单
pub fn set_group_plugin_enabled(group_id: i64, plugin: &str, enabled: bool) -> Result<(), String> {
    if runtime_config::group_exempt(plugin) {
        return Err(format!(
            "插件 {plugin} 声明了自己不按群生效，没有「在这个群里停用」这一说；要停就停整体"
        ));
    }
    settings::set_group_plugin_enabled(group_id, plugin, enabled)
}

/// 某个功能 key 是否全局启用（GUI「插件管理」页画开关用）
pub fn feature_enabled_globally(key: &str) -> bool {
    !runtime_config::feature_disabled_globally(key)
}

/// 全局启用/停用某个功能 key：所有群加私聊一起受影响，比停一家插件细一档、比按群开关宽一档
pub fn set_feature_enabled_globally(key: &str, enabled: bool) -> Result<String, String> {
    settings::set_feature_enabled_globally(key, enabled)?;
    Ok(format!(
        "功能 {key} 已{}（写入 {}）",
        if enabled { "启用" } else { "停用" },
        arona_file()
    ))
}

/// 某个群禁用的插件 id 列表（GUI「群管理」页画开关用）
pub fn group_disabled_plugins(group_id: i64) -> Vec<String> {
    runtime_config::group_setting(group_id).disabled_plugins
}

/// 哪些群在群名单里单独关掉了这个功能 key（GUI「插件管理」页的功能行展开用）。
/// 只看群这一层的减法，不含全局名单——全局那一层由 [`feature_enabled_globally`] 单独表达。
pub fn feature_off_groups(key: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = crate::framework::Framework::global()
        .gating()
        .group_settings()
        .iter()
        .filter(|(_, setting)| setting.disabled_features.iter().any(|item| item == key))
        .filter_map(|(group, _)| group.parse::<i64>().ok())
        .collect();
    ids.sort_unstable();
    ids
}

/// 某个插件在一个群里的状态（GUI「插件管理」页的「启用群」列表）
pub struct PluginGroupRow {
    pub group_id: i64,
    /// 这个群在不在框架的授权名单里（名单为空 = 所有群都授权）。
    /// 不在名单里的群是"填了群号预添加、或只为这个插件写过例外"的那类，机器人还不会理它
    pub authorized: bool,
    /// 该插件在这个群是不是启用的
    pub enabled: bool,
}

/// 某个插件在各群的启用状态。群号集合 = **授权名单 ∪ 写过分群设置的群**：前者是"机器人在
/// 哪些群响应"，后者是"哪些群对这个插件有例外"，只看一边都会漏。
///
/// 机器人还没进去的群可以先填群号预添加——填的就是 `config.groups`，走 [`set_group_enabled`]，
/// 不另造一份插件自己的群名单（那样就得在门控上叠第 7 层判定，代价远大于收益）。
///
/// 这里克隆整张分群表：只在面板打开与改完开关之后调用。事件热路径不走这里，
/// 那条用的是 [`crate::runtime::config::Gating::plugin_enabled_in_group`] 的持锁就地读。
pub fn plugin_group_rows(plugin: &str) -> Vec<PluginGroupRow> {
    let gating = crate::framework::Framework::global().gating();
    rows_for(gating.groups(), gating.group_settings(), plugin)
}

/// 上面那层的纯计算部分：拿授权名单 + 分群表算出每个群的状态。
/// 单独拆出来是为了能用表驱动用例钉住（群号并集、空名单=全授权、插件名大小写），
/// 而不必去动进程默认实例那份真值。
fn rows_for(
    allowed: Vec<i64>,
    settings: std::collections::BTreeMap<String, crate::config::arona::GroupSetting>,
    plugin: &str,
) -> Vec<PluginGroupRow> {
    let mut ids = allowed.clone();
    for key in settings.keys() {
        if let Ok(id) = key.parse::<i64>() {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    ids.into_iter()
        .map(|group_id| PluginGroupRow {
            // 名单为空 = 所有群都授权，与运行时那条判据同一个口径
            authorized: allowed.is_empty() || allowed.contains(&group_id),
            enabled: settings
                .get(&group_id.to_string())
                .map(|setting| {
                    !setting
                        .disabled_plugins
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(plugin))
                })
                .unwrap_or(true),
            group_id,
        })
        .collect()
}

// ==================== OneBot API ====================

/// 调用 OneBot API（使用首个可用连接）
pub async fn call_api(action: &str, params: Value, timeout_ms: u64) -> Result<Value, String> {
    let connection = crate::framework::Framework::global()
        .onebot()
        .first_connection()
        .ok_or_else(|| {
            "当前没有可用的 OneBot 连接，请先在「OneBot 连接」里启用一个连接".to_string()
        })?;
    let action = protocol::action(action, params);
    let wait = connection.send(action);
    match tokio::time::timeout(Duration::from_millis(timeout_ms), wait).await {
        Ok(Some(response)) => check_response(response),
        Ok(None) => Err("OneBot 连接未响应（已断开或超时）".to_string()),
        Err(_) => Err("OneBot 调用超时".to_string()),
    }
}

fn check_response(response: OneBotActionResponse) -> Result<Value, String> {
    if !response.success() {
        let detail = response
            .message
            .clone()
            .or_else(|| response.wording.clone())
            .unwrap_or_else(|| response.raw.to_string());
        return Err(format!(
            "OneBot 返回失败 (retcode={}): {detail}",
            response.retcode
        ));
    }
    response
        .data
        .ok_or_else(|| "OneBot 未返回 data 字段".to_string())
}

/// 群列表：OneBot get_group_list 与 arona.yml 配置合并
pub async fn group_list() -> Result<Vec<GroupInfo>, String> {
    let data = call_api("get_group_list", json!({}), 10_000).await?;
    let remote = data.as_array().cloned().unwrap_or_default();
    let config = settings::config();
    let all_groups = config.groups.is_empty();
    let mut infos: Vec<GroupInfo> = Vec::new();
    for item in remote {
        let Some(group_id) = item.get("group_id").and_then(|v| v.as_i64()) else {
            continue;
        };
        infos.push(build_group_info(
            group_id,
            item.get("group_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            item.get("member_count")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            &config,
            all_groups,
            false,
        ));
    }
    // 配置里存在但 OneBot 未返回的群（机器人可能已退群/离线）
    for group_id in &config.groups {
        if !infos.iter().any(|info| info.group_id == *group_id) {
            infos.push(build_group_info(
                *group_id,
                "(不在群列表中)".to_string(),
                0,
                &config,
                all_groups,
                true,
            ));
        }
    }
    infos.sort_by_key(|info| info.group_id);
    Ok(infos)
}

fn build_group_info(
    group_id: i64,
    name: String,
    member_count: i64,
    config: &crate::config::arona::AronaConfig,
    all_groups: bool,
    local_only: bool,
) -> GroupInfo {
    let setting = config.group_settings.get(&group_id.to_string());
    GroupInfo {
        group_id,
        name: if name.is_empty() {
            group_id.to_string()
        } else {
            name
        },
        member_count,
        enabled: all_groups || config.groups.contains(&group_id),
        all_groups,
        blacklist_count: setting.map(|s| s.blacklist.len()).unwrap_or(0),
        disabled_count: setting.map(|s| s.disabled_features.len()).unwrap_or(0),
        local_only,
    }
}

/// 群成员列表（OneBot get_group_member_list）
pub async fn group_members(group_id: i64) -> Result<Vec<MemberInfo>, String> {
    let data = call_api(
        "get_group_member_list",
        json!({ "group_id": group_id, "no_cache": true }),
        20_000,
    )
    .await?;
    let list = data.as_array().cloned().unwrap_or_default();
    let setting = runtime_config::group_setting(group_id);
    let global_blacklist = runtime_config::global_blacklist();
    let mut members: Vec<MemberInfo> = list
        .into_iter()
        .filter_map(|item| {
            let user_id = item.get("user_id").and_then(|v| v.as_i64())?;
            Some(MemberInfo {
                user_id,
                nickname: item
                    .get("nickname")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                card: item
                    .get("card")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                role: item
                    .get("role")
                    .and_then(|v| v.as_str())
                    .unwrap_or("member")
                    .to_string(),
                level: item
                    .get("level")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                blacklisted: setting.blacklist.contains(&user_id),
                global_blacklisted: global_blacklist.contains(&user_id),
                is_manager: runtime_config::is_manager(user_id),
            })
        })
        .collect();
    members.sort_by(|a, b| {
        role_rank(&a.role)
            .cmp(&role_rank(&b.role))
            .then_with(|| a.user_id.cmp(&b.user_id))
    });
    Ok(members)
}

fn role_rank(role: &str) -> i32 {
    match role {
        "owner" => 0,
        "admin" => 1,
        _ => 2,
    }
}

/// 角色显示名
pub fn role_name(role: &str) -> &'static str {
    match role {
        "owner" => "群主",
        "admin" => "管理员",
        _ => "成员",
    }
}

// ==================== 分群功能 / 黑名单 ====================

pub fn set_group_enabled(group_id: i64, enabled: bool) -> Result<(), String> {
    settings::set_group_enabled(group_id, enabled)
}

pub fn set_group_feature(group_id: i64, feature: &str, enabled: bool) -> Result<(), String> {
    settings::set_group_feature(group_id, feature, enabled)
}

/// 整组开启/关闭某个群的一批功能（GUI「功能开关」按插件折叠时的组头开关）
pub fn set_group_features(group_id: i64, features: &[&str], enabled: bool) -> Result<(), String> {
    settings::set_group_features(group_id, features, enabled)
}

pub fn set_group_blacklist(group_id: i64, user_id: i64, blacklisted: bool) -> Result<(), String> {
    settings::set_group_blacklist(group_id, user_id, blacklisted)
}

pub fn set_global_blacklist(user_id: i64, blacklisted: bool) -> Result<(), String> {
    settings::set_global_blacklist(user_id, blacklisted)
}

/// 清空某个群的功能开关与群内黑名单
pub fn clear_group_setting(group_id: i64) -> Result<(), String> {
    settings::clear_group_setting(group_id)
}

/// 本地图片改用 file:// 直传开关。
/// 该配置存在 onebot.yml（与连接配置同一个文件），写完立即热生效，无需重启。
pub fn set_send_image_as_file(enabled: bool) -> Result<String, String> {
    let file = paths::onebot_file();
    let mut config = onebot::load(&file)?;
    if config.send_image_as_file != enabled {
        config.send_image_as_file = enabled;
        onebot::save(&file, &config).map_err(|err| format!("写入 onebot.yml 失败: {err}"))?;
    }
    // 立即生效：同步内存配置与运行期开关
    match application::global() {
        Some(app) => app.apply_send_image_as_file(enabled),
        None => crate::runtime::config::set_send_image_as_file(enabled),
    }
    Ok(format!(
        "图片发送方式已{}（写入 {}）",
        if enabled {
            "改为 file:// 直传，大图不再内嵌 base64"
        } else {
            "改回内嵌 base64"
        },
        file.display()
    ))
}

/// arona.yml 路径（GUI 展示用）
pub fn arona_file() -> String {
    settings::config_file()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "未初始化".to_string())
}

// ==================== OneBot 连接配置 ====================

/// 读取 onebot.yml
pub fn load_onebot_config() -> Result<OneBotConfig, String> {
    onebot::load(&paths::onebot_file())
}

/// 连接展示列表
pub fn connection_list(config: &OneBotConfig) -> Vec<ConnectionInfo> {
    config
        .connections
        .iter()
        .map(|(key, conn)| {
            let resolved = conn.resolve_type(key);
            ConnectionInfo {
                key: key.clone(),
                type_key: resolved
                    .as_ref()
                    .map(|t| t.key().to_string())
                    .unwrap_or_else(|_| conn.connection_type.clone()),
                type_name: match &resolved {
                    Ok(t) => t.display_name().to_string(),
                    Err(_) => "未知类型".to_string(),
                },
                address: match &resolved {
                    Ok(t) => conn.address(*t),
                    Err(_) => conn.url.clone(),
                },
                enable: conn.enable,
            }
        })
        .collect()
}

/// 新增一个连接（自动生成不冲突的键名），返回键名
pub fn add_connection(config: &mut OneBotConfig, conn_type: ConnectionType) -> String {
    let key = config.unique_connection_key(conn_type.key());
    let mut conn = onebot::default_connection(conn_type);
    conn.enable = true;
    config.connections.insert(key.clone(), conn);
    key
}

/// 删除连接
pub fn remove_connection(config: &mut OneBotConfig, key: &str) -> bool {
    config.connections.remove(key).is_some()
}

/// 保存 onebot.yml 并热重载全部连接（必须在 tokio 运行期内调用）
pub fn save_and_reload(config: &OneBotConfig) -> Result<String, String> {
    let file = paths::onebot_file();
    onebot::save(&file, config).map_err(|err| format!("写入 onebot.yml 失败: {err}"))?;
    let app = application::global().ok_or_else(|| "OneBot 应用尚未初始化".to_string())?;
    app.reload(config.clone());
    let enabled = config
        .connections
        .values()
        .filter(|conn| conn.enable)
        .count();
    Ok(format!(
        "saved {} · 已热重载，启用连接 {enabled}/{}",
        file.display(),
        config.connections.len()
    ))
}

/// 当前运行状态摘要
pub fn status_text() -> String {
    let app = application::global();
    let connections = app.as_ref().map(|app| app.registry.count()).unwrap_or(0);
    let config = app.as_ref().map(|app| app.config());
    format!(
        "版本 {} · 账号 {:?}({}) · 活动连接 {}",
        env!("CARGO_PKG_VERSION"),
        config
            .as_ref()
            .map(|c| c.nickname.clone())
            .unwrap_or_default(),
        config.as_ref().map(|c| c.self_id).unwrap_or(0),
        connections
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_helpers_cover_multiple_instances() {
        let mut config = OneBotConfig::default();
        let first = add_connection(&mut config, ConnectionType::WebSocket);
        assert_eq!(first, "ws-forward-2", "同名连接应自动生成不冲突键名");
        let second = add_connection(&mut config, ConnectionType::WebSocket);
        assert_eq!(second, "ws-forward-3");
        let list = connection_list(&config);
        assert_eq!(
            list.iter()
                .filter(|info| info.type_key == "ws-forward")
                .count(),
            3
        );
        assert!(remove_connection(&mut config, &second));
        assert!(!remove_connection(&mut config, &second));
    }

    /// 分群功能开关：只关掉那一组的那个功能，别群与私聊不受影响
    #[test]
    fn group_switch_gates_feature() {
        let gating = crate::runtime::config::Gating::default();
        assert!(gating.feature_enabled(Some(1), "tarot"));
        let mut setting = crate::config::arona::GroupSetting::default();
        setting.disabled_features.push("tarot".to_string());
        let mut map = std::collections::BTreeMap::new();
        map.insert("1".to_string(), setting);
        gating.set_group_settings(map);
        assert!(!gating.feature_enabled(Some(1), "tarot"));
        assert!(gating.feature_enabled(Some(2), "tarot"), "其它群不受影响");
        assert!(
            gating.feature_enabled(None, "tarot"),
            "私聊不受分群开关限制"
        );
    }

    /// 插件详情页那张群表：群号是**授权名单 ∪ 写过分群设置的群**（只看一边都会漏），
    /// 名单为空 = 所有群都授权，插件名比对忽略大小写。
    #[test]
    fn plugin_rows_merge_allowlist_and_group_exceptions() {
        use crate::config::arona::GroupSetting;
        let mut settings = std::collections::BTreeMap::new();
        let mut disabled = GroupSetting::default();
        disabled.disabled_plugins.push("Bilibili".to_string());
        settings.insert("200".to_string(), disabled);
        // 300 只写过群设置（比如拉黑过谁），并不在授权名单里：它照样得出现在表上
        settings.insert("300".to_string(), GroupSetting::default());

        let rows = rows_for(vec![100, 200], settings.clone(), "bilibili");
        assert_eq!(
            rows.iter().map(|row| row.group_id).collect::<Vec<_>>(),
            vec![100, 200, 300],
            "群号该取两边的并集并按序排"
        );
        let find = |id: i64| rows.iter().find(|row| row.group_id == id).unwrap();
        assert!(find(100).enabled);
        // 名单里存的是 "Bilibili"，查的是 "bilibili"：大小写不该改变结论
        assert!(!find(200).enabled, "该群把这个插件关掉了");
        assert!(find(200).authorized);
        // 只为这个插件写过例外、还没进授权名单的群，要在表上如实标出来
        assert!(!find(300).authorized, "预添加的群还没被授权");
        assert!(find(300).enabled);

        // 名单为空 = 全放行（与运行时那条判据同一个口径），此时表里只剩写过设置的群
        let open = rows_for(Vec::new(), settings, "bilibili");
        assert!(open.iter().all(|row| row.authorized), "空名单该全放行");
        assert_eq!(open.len(), 2, "空名单时表里就只有写过设置的那两个群");
    }

    /// 黑名单：全局名单命中所有会话，群内名单只管那个群
    #[test]
    fn blacklist_gates_global_and_per_group() {
        let gating = crate::runtime::config::Gating::default();
        gating.set_global_blacklist(vec![10001]);
        assert!(gating.is_blacklisted(10001, Some(1)));
        gating.set_global_blacklist(Vec::new());
        let mut setting = crate::config::arona::GroupSetting::default();
        setting.blacklist.push(10002);
        let mut map = std::collections::BTreeMap::new();
        map.insert("1".to_string(), setting);
        gating.set_group_settings(map);
        assert!(gating.is_blacklisted(10002, Some(1)));
        assert!(!gating.is_blacklisted(10002, Some(2)));
        assert!(!gating.is_blacklisted(10002, None));
    }

    /// 等待端口处于期望状态（最多 2 秒）
    async fn wait_port(port: u16, expect_open: bool) -> bool {
        for _ in 0..40 {
            let open = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok();
            if open == expect_open {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// 热重载：改端口后新端口监听、旧端口释放（反向 ws 真正重启）
    #[tokio::test]
    async fn onebot_reload_rebinds_reverse_listener() {
        use crate::onebot::application::OneBotApplication;
        use crate::onebot::business::BusinessHandler;
        use crate::onebot::connection::ConnectionRegistry;
        use std::sync::Arc;

        // 这里建的 app 会按自己那份 config 覆写进程默认实例的图片直传开关，
        // 与 admin 那个"写完立即生效"的用例必须错开
        let _serial = crate::runtime::config::process_state_lock();
        crate::runtime::config::set_bot_id(10000);
        let base = 30000 + (std::process::id() % 1000) as u16 * 2;
        let registry = Arc::new(ConnectionRegistry::new());
        // 框架自测：连接热重载与命令无关，命令表按归属登记在进程级，这里给个空句柄即可
        let dispatcher = Arc::new(crate::runtime::dispatcher::CommandDispatcher::new());
        let business = Arc::new(BusinessHandler::new(
            OneBotConfig::default(),
            crate::framework::Framework::global_arc(),
            dispatcher,
            registry.clone(),
        ));
        let mut config = OneBotConfig::default();
        {
            let conn = config
                .connections
                .get_mut("ws-reverse")
                .expect("默认配置应包含 ws-reverse");
            conn.enable = true;
            conn.host = "127.0.0.1".into();
            conn.port = base;
            conn.path = "/onebot/v11".into();
        }
        let app = Arc::new(OneBotApplication::new(
            config.clone(),
            business,
            registry.clone(),
        ));
        app.start();
        assert_eq!(registry.count(), 1, "启动后应注册一个连接");
        assert!(wait_port(base, true).await, "热重载前反向 ws 未监听 {base}");

        let mut reloaded = config.clone();
        reloaded.connections.get_mut("ws-reverse").unwrap().port = base + 1;
        app.reload(reloaded);
        assert!(wait_port(base + 1, true).await, "热重载后新端口未监听");
        assert!(wait_port(base, false).await, "热重载后旧端口未释放");
        assert_eq!(
            app.registry.count(),
            1,
            "热重载后连接数应为 1（旧的已注销）"
        );

        app.stop();
        assert_eq!(app.registry.count(), 0, "停止后注册表应清空");
    }

    /// GUI「OneBot 连接」页的「发送设置」勾选框走的就是这条链路：
    /// 写入 onebot.yml 并立即热生效（不需要「保存并热重载」或重启）
    #[test]
    fn image_send_switch_writes_onebot_yml_and_applies_immediately() {
        // 断言的是进程默认实例那一份开关，与建 app 的用例串行（见 process_state_lock）
        let _serial = crate::runtime::config::process_state_lock();
        let dir = std::env::temp_dir().join("arona-admin-send-image-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("onebot.yml");
        let _ = std::fs::remove_file(&file);
        // 进程内只允许设置一次；指向临时目录，避免动到真实的 config/onebot.yml
        paths::set_onebot_file(file.clone());
        crate::runtime::config::set_send_image_as_file(false);

        let note = set_send_image_as_file(true).expect("切换到 file:// 直传应成功");
        assert!(note.contains("file://"), "提示应说明已切换: {note}");
        assert!(
            crate::runtime::config::send_image_as_file(),
            "运行期开关应立即变为 true"
        );
        let text = std::fs::read_to_string(&file).expect("onebot.yml 应已写入");
        assert!(
            text.contains("send_image_as_file: true"),
            "配置应落盘到 onebot.yml: {text}"
        );

        let note = set_send_image_as_file(false).expect("切回内嵌 base64 应成功");
        assert!(!note.contains("file://"), "提示应说明已切回: {note}");
        assert!(
            !crate::runtime::config::send_image_as_file(),
            "运行期开关应立即变回 false"
        );
        let text = std::fs::read_to_string(&file).expect("onebot.yml 应已写入");
        assert!(
            text.contains("send_image_as_file: false"),
            "配置应落盘到 onebot.yml: {text}"
        );

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 键名像凭据的一律不摊给 GUI；结构（表/数组/空值）也不摊；多行字符串不摊
    #[test]
    fn only_safe_scalars_reach_the_config_panel() {
        let tree: serde_yaml::Value = serde_yaml::from_str(
            r#"
image:
  quality: 1000w
  drawEnable: true
  badgeEnable:
    left: true
    right: false
  colorGenerator:
    hueStep: 30
account:
  uid: 123
  cookie: 'SECRETCOOKIE'
  nickname: 老师
template:
  one: "第一行\n第二行"
  list: [1, 2]
  nothing: null
"#,
        )
        .unwrap();
        let mut out = Vec::new();
        flatten_fields("image", &tree["image"], &mut out);
        flatten_fields("account", &tree["account"], &mut out);
        flatten_fields("template", &tree["template"], &mut out);
        let paths: Vec<&str> = out.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"image.quality"), "{paths:?}");
        assert!(paths.contains(&"image.drawEnable"), "{paths:?}");
        assert!(paths.contains(&"image.badgeEnable.left"), "{paths:?}");
        assert!(paths.contains(&"image.colorGenerator.hueStep"), "{paths:?}");
        // 挡的是凭据那一个叶子：cookie 不上屏，但它旁边的 uid/nickname 照给控件
        assert!(paths.contains(&"account.uid"), "{paths:?}");
        assert!(paths.contains(&"account.nickname"), "{paths:?}");
        assert!(!paths.iter().any(|p| p.contains("cookie")), "{paths:?}");
        // 多行模板、数组、空值都没有控件
        assert!(!paths.contains(&"template.one"), "{paths:?}");
        assert!(!paths.contains(&"template.list"), "{paths:?}");
        assert!(!paths.contains(&"template.nothing"), "{paths:?}");
        assert!(
            !out.iter()
                .any(|f| format!("{:?}", f.value).contains("SECRETCOOKIE")),
            "凭据值漏进来了"
        );
    }

    /// 写回只改那一条路径，同区其它键必须原样留着
    #[test]
    fn a_patch_touches_only_the_named_leaf() {
        let mut root: serde_yaml::Value =
            serde_yaml::from_str("quality: 1000w\nbadgeEnable:\n  left: true\n  right: false\n")
                .unwrap();
        patch_path(
            &mut root,
            ["badgeEnable", "right"].into_iter(),
            serde_yaml::Value::Bool(true),
        )
        .unwrap();
        assert_eq!(root["badgeEnable"]["right"], serde_yaml::Value::Bool(true));
        assert_eq!(root["badgeEnable"]["left"], serde_yaml::Value::Bool(true));
        assert_eq!(root["quality"], serde_yaml::Value::from("1000w"));
        // 中间缺的那一层顺手补出来，而不是报错
        let mut sparse: serde_yaml::Value = serde_yaml::from_str("quality: 800w").unwrap();
        patch_path(
            &mut sparse,
            ["footer", "footerAlign"].into_iter(),
            serde_yaml::Value::from("LEFT"),
        )
        .unwrap();
        assert_eq!(
            sparse["footer"]["footerAlign"],
            serde_yaml::Value::from("LEFT")
        );
        assert_eq!(sparse["quality"], serde_yaml::Value::from("800w"));
        // 拿一个标量当表往下走要报错，不能悄悄覆盖
        let mut scalar: serde_yaml::Value = serde_yaml::from_str("quality: 800w").unwrap();
        assert!(
            patch_path(
                &mut scalar,
                ["quality", "oops"].into_iter(),
                serde_yaml::Value::Bool(false),
            )
            .is_err()
        );
    }
}
