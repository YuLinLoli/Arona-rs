//! 管理面板后端（GUI 与命令行共用）
//! 负责：群列表/群成员查询（走 OneBot API）、分群功能开关与黑名单读写、
//! OneBot 连接配置读写与热重载。
use crate::config::onebot::{self, ConnectionType, OneBotConfig};
use crate::config::standalone;
use crate::onebot::application;
use crate::onebot::model::OneBotActionResponse;
use crate::onebot::protocol;
use crate::runtime::config as runtime_config;
use crate::runtime::paths;
use serde_json::{Value, json};
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

/// 功能清单
pub fn features() -> &'static [runtime_config::Feature] {
    runtime_config::features()
}

// ==================== OneBot API ====================

/// 调用 OneBot API（使用首个可用连接）
pub async fn call_api(action: &str, params: Value, timeout_ms: u64) -> Result<Value, String> {
    let connection = application::global()
        .and_then(|app| app.first_connection())
        .or_else(|| crate::onebot::connection::global_registry().and_then(|r| r.first()))
        .ok_or_else(|| "当前没有可用的 OneBot 连接，请先在「OneBot 连接」里启用一个连接".to_string())?;
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
    let config = standalone::config();
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
            item.get("member_count").and_then(|v| v.as_i64()).unwrap_or(0),
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
    standalone::set_group_enabled(group_id, enabled)
}

pub fn set_group_feature(group_id: i64, feature: &str, enabled: bool) -> Result<(), String> {
    standalone::set_group_feature(group_id, feature, enabled)
}

pub fn set_group_blacklist(group_id: i64, user_id: i64, blacklisted: bool) -> Result<(), String> {
    standalone::set_group_blacklist(group_id, user_id, blacklisted)
}

pub fn set_global_blacklist(user_id: i64, blacklisted: bool) -> Result<(), String> {
    standalone::set_global_blacklist(user_id, blacklisted)
}

/// 清空某个群的功能开关与群内黑名单
pub fn clear_group_setting(group_id: i64) -> Result<(), String> {
    standalone::clear_group_setting(group_id)
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
    standalone::config_file()
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
    let enabled = config.connections.values().filter(|conn| conn.enable).count();
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
        config.as_ref().map(|c| c.nickname.clone()).unwrap_or_default(),
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
            list.iter().filter(|info| info.type_key == "ws-forward").count(),
            3
        );
        assert!(remove_connection(&mut config, &second));
        assert!(!remove_connection(&mut config, &second));
    }

    /// 功能开关与黑名单共用全局 group_settings，两个场景必须串行在同一个测试里，
    /// 否则并行测试互相重置全局状态会随机挂
    #[test]
    fn group_settings_gate_feature_and_blacklist() {
        crate::runtime::config::set_group_settings(Default::default());
        // 场景一：分群功能开关
        assert!(runtime_config::feature_enabled(Some(1), "tarot"));
        let mut setting = crate::config::arona::GroupSetting::default();
        setting.disabled_features.push("tarot".to_string());
        let mut map = std::collections::BTreeMap::new();
        map.insert("1".to_string(), setting);
        crate::runtime::config::set_group_settings(map);
        assert!(!runtime_config::feature_enabled(Some(1), "tarot"));
        assert!(runtime_config::feature_enabled(Some(2), "tarot"), "其它群不受影响");
        assert!(runtime_config::feature_enabled(None, "tarot"), "私聊不受分群开关限制");
        // 场景二：群内黑名单与全局黑名单
        crate::runtime::config::set_global_blacklist(vec![10001]);
        assert!(runtime_config::is_blacklisted(10001, Some(1)));
        crate::runtime::config::set_global_blacklist(Vec::new());
        let mut setting = crate::config::arona::GroupSetting::default();
        setting.blacklist.push(10002);
        let mut map = std::collections::BTreeMap::new();
        map.insert("1".to_string(), setting);
        crate::runtime::config::set_group_settings(map);
        assert!(runtime_config::is_blacklisted(10002, Some(1)));
        assert!(!runtime_config::is_blacklisted(10002, Some(2)));
        assert!(!runtime_config::is_blacklisted(10002, None));
        crate::runtime::config::set_group_settings(Default::default());
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
        use crate::onebot::business::StandaloneBusinessHandler;
        use crate::onebot::connection::ConnectionRegistry;
        use std::sync::Arc;

        crate::runtime::config::set_bot_id(10000);
        let base = 30000 + (std::process::id() % 1000) as u16 * 2;
        let registry = Arc::new(ConnectionRegistry::new());
        let dispatcher = crate::standalone::dispatcher::build(OneBotConfig::default());
        let business = Arc::new(StandaloneBusinessHandler::new(
            OneBotConfig::default(),
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
        let app = Arc::new(OneBotApplication::new(config.clone(), business, registry.clone()));
        app.start();
        assert_eq!(registry.count(), 1, "启动后应注册一个连接");
        assert!(wait_port(base, true).await, "热重载前反向 ws 未监听 {base}");

        let mut reloaded = config.clone();
        reloaded
            .connections
            .get_mut("ws-reverse")
            .unwrap()
            .port = base + 1;
        app.reload(reloaded);
        assert!(wait_port(base + 1, true).await, "热重载后新端口未监听");
        assert!(wait_port(base, false).await, "热重载后旧端口未释放");
        assert_eq!(app.registry.count(), 1, "热重载后连接数应为 1（旧的已注销）");

        app.stop();
        assert_eq!(app.registry.count(), 0, "停止后注册表应清空");
    }

    /// GUI「OneBot 连接」页的「发送设置」勾选框走的就是这条链路：
    /// 写入 onebot.yml 并立即热生效（不需要「保存并热重载」或重启）
    #[test]
    fn image_send_switch_writes_onebot_yml_and_applies_immediately() {
        let dir = std::env::temp_dir().join("arona-admin-send-image-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("onebot.yml");
        let _ = std::fs::remove_file(&file);
        // 进程内只允许设置一次；指向临时目录，避免动到真实的 arona-standalone/onebot.yml
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
}