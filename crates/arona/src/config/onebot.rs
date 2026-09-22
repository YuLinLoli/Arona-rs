//! OneBot 协议配置（对应原版 onebot 包的 OneBotConfig/OneBotConfigLoader）
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 单个连接的配置
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ConnectionConfig {
    /// 连接类型（ws-forward / ws-reverse / http / http-reverse）；
    /// 旧配置留空时回退用连接键名判断，便于一个类型配置多个实例
    #[serde(
        default,
        rename = "type",
        deserialize_with = "deserialize_connection_type"
    )]
    pub connection_type: String,
    #[serde(default)]
    pub enable: bool,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_path")]
    pub path: String,
    #[serde(default)]
    pub token: String,
    #[serde(default = "default_heartbeat")]
    pub heartbeat_interval: u64,
    #[serde(default = "default_reconnect")]
    pub reconnect_interval: u64,
}

/// `type` 字段容错：缺失或空值(null)一律当空串，之后回退用连接键名判断类型。
///
/// 老版本模板/手工编辑会写出 `type:`（值为 null），严格按 `String` 解析会直接让整份
/// onebot.yml 加载失败——表现就是启动时弹「配置错误」、程序起不来。
fn deserialize_connection_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_yaml::Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(serde_yaml::Value::Null) => String::new(),
        Some(serde_yaml::Value::String(text)) => text,
        // 顺手容错：写成数字/布尔等标量时按字面量处理，交给 resolve_type 报「类型未知」
        Some(serde_yaml::Value::Number(number)) => number.to_string(),
        Some(serde_yaml::Value::Bool(flag)) => flag.to_string(),
        // 列表/映射等非法写法一律当空，回退用键名判断（避免把多行内容写回配置文件）
        Some(_) => String::new(),
    })
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}
fn default_path() -> String {
    "/".to_string()
}
fn default_heartbeat() -> u64 {
    30_000
}
fn default_reconnect() -> u64 {
    5_000
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        ConnectionConfig {
            connection_type: String::new(),
            enable: false,
            host: default_host(),
            port: 0,
            url: String::new(),
            path: default_path(),
            token: String::new(),
            heartbeat_interval: default_heartbeat(),
            reconnect_interval: default_reconnect(),
        }
    }
}

/// onebot.yml
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OneBotConfig {
    #[serde(default)]
    pub self_id: i64,
    #[serde(default = "default_nickname")]
    pub nickname: String,
    /// 来自本机磁盘的图片（插件生成的结果图、上传的图片文件等）改用 `file://` 路径直传 OneBot 实现，
    /// 不再内嵌 base64。仅当 OneBot 实现(如 NapCat)与机器人同机部署、能访问相同磁盘时开启；默认 false(内嵌 base64)。
    #[serde(default)]
    pub send_image_as_file: bool,
    #[serde(default)]
    pub connections: BTreeMap<String, ConnectionConfig>,
}

fn default_nickname() -> String {
    "Arona".to_string()
}

/// 连接类型
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionType {
    WebSocket,
    WebSocketReverse,
    Http,
    HttpReverse,
}

impl ConnectionType {
    pub fn key(self) -> &'static str {
        match self {
            ConnectionType::WebSocket => "ws-forward",
            ConnectionType::WebSocketReverse => "ws-reverse",
            ConnectionType::Http => "http",
            ConnectionType::HttpReverse => "http-reverse",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            ConnectionType::WebSocket => "WebSocket 正向",
            ConnectionType::WebSocketReverse => "WebSocket 反向",
            ConnectionType::Http => "HTTP 正向",
            ConnectionType::HttpReverse => "HTTP 反向",
        }
    }

    pub fn from_name(name: &str) -> Result<ConnectionType, String> {
        match name {
            "ws-forward" => Ok(ConnectionType::WebSocket),
            "ws-reverse" => Ok(ConnectionType::WebSocketReverse),
            "http" => Ok(ConnectionType::Http),
            "http-reverse" => Ok(ConnectionType::HttpReverse),
            _ => Err(format!(
                "未知的连接类型: {name}（可选: ws-forward、ws-reverse、http、http-reverse）"
            )),
        }
    }

    pub fn all() -> Vec<ConnectionType> {
        vec![
            ConnectionType::WebSocket,
            ConnectionType::WebSocketReverse,
            ConnectionType::Http,
            ConnectionType::HttpReverse,
        ]
    }
}

/// 某种连接类型的默认配置
pub fn default_connection(conn_type: ConnectionType) -> ConnectionConfig {
    let mut conn = ConnectionConfig::default();
    conn.connection_type = conn_type.key().to_string();
    match conn_type {
        ConnectionType::WebSocket => {
            conn.host = "127.0.0.1".into();
            conn.url = "ws://127.0.0.1:6700".into();
        }
        ConnectionType::WebSocketReverse => {
            conn.port = 6701;
            conn.path = "/onebot/v11".into();
        }
        ConnectionType::Http => {
            conn.port = 5700;
            conn.path = "/".into();
        }
        ConnectionType::HttpReverse => {
            conn.url = "http://127.0.0.1:5701/onebot".into();
        }
    }
    conn
}

impl ConnectionConfig {
    /// 连接类型：优先取本项的 type 字段，缺省回退到连接键名（兼容旧配置）
    pub fn resolve_type(&self, key: &str) -> Result<ConnectionType, String> {
        let name = self.connection_type.trim();
        let name = if name.is_empty() { key } else { name };
        ConnectionType::from_name(name)
    }

    /// 展示用地址
    pub fn address(&self, conn_type: ConnectionType) -> String {
        match conn_type {
            ConnectionType::WebSocket | ConnectionType::HttpReverse => self.url.clone(),
            _ => format!("{}:{}{}", self.host, self.port, self.path),
        }
    }
}

impl OneBotConfig {
    /// 生成不冲突的连接键名：ws-forward、ws-forward-2、ws-forward-3 ...
    pub fn unique_connection_key(&self, base: &str) -> String {
        if !self.connections.contains_key(base) {
            return base.to_string();
        }
        for index in 2..1000 {
            let candidate = format!("{base}-{index}");
            if !self.connections.contains_key(&candidate) {
                return candidate;
            }
        }
        format!("{base}-{}", chrono::Utc::now().timestamp())
    }
}

impl Default for OneBotConfig {
    fn default() -> Self {
        let mut connections = BTreeMap::new();
        for conn_type in ConnectionType::all() {
            connections.insert(conn_type.key().to_string(), default_connection(conn_type));
        }
        OneBotConfig {
            self_id: 0,
            nickname: default_nickname(),
            send_image_as_file: false,
            connections,
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

/// onebot.yml 允许出现的顶层键，其它键一律提示（避免写错文件后静默失效）
const KNOWN_TOP_KEYS: [&str; 4] = ["self_id", "nickname", "send_image_as_file", "connections"];
/// 旧版本曾写在本文件、现已迁到 arona.yml 的框架字段（下面会清理并单独提示）
const FRAMEWORK_LEGACY_KEYS: [&str; 2] = ["groups", "managers"];

/// 需要清理的残留键：框架自己的 + 各插件已登记的配置区。
/// 插件的键名不写死在框架里，`arona.yml` 缺失时由 [`super::arona::load`] 先迁走，这里只清残留。
fn legacy_top_keys() -> Vec<String> {
    let mut keys: Vec<String> = FRAMEWORK_LEGACY_KEYS
        .iter()
        .map(|key| key.to_string())
        .collect();
    keys.extend(super::arona::section_keys());
    keys
}

/// 加载 onebot.yml：不存在时生成模板；兼容旧后缀 onebot.yaml 迁移；清理业务字段残留
pub fn load(file: &Path) -> Result<OneBotConfig, String> {
    if !file.exists() {
        // 旧后缀 onebot.yaml 已存在则迁移
        let old_file = file.parent().map(|p| p.join("onebot.yaml"));
        if let Some(old) = old_file {
            if old.exists() {
                if let Ok(config) = parse(&old) {
                    save(file, &config).map_err(|e| format!("写入 onebot.yml 失败: {e}"))?;
                    crate::runtime::log::info(format!(
                        "[OneBot] 检测到旧版 onebot.yaml，已迁移到 {}",
                        file.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    return Ok(config);
                }
            }
        }
        let config = OneBotConfig::default();
        save(file, &config).map_err(|e| format!("写入 onebot.yml 失败: {e}"))?;
        return Ok(config);
    }
    parse(file)
}

fn parse(file: &Path) -> Result<OneBotConfig, String> {
    let text = read_text(file).map_err(|e| format!("读取 {} 失败: {e}", file.display()))?;
    let mut value: serde_yaml::Value =
        serde_yaml::from_str(&text).map_err(|e| format!("onebot.yml 解析失败，请检查格式: {e}"))?;
    // 未知顶层键：serde 默认会静默忽略，用户以为配置生效了其实没有（例如把
    // arona.yml 的 groups / 插件配置区写进了本文件），这里逐个提示。
    let legacy_keys = legacy_top_keys();
    if let Some(map) = value.as_mapping() {
        for key in map.keys() {
            let Some(name) = key.as_str() else { continue };
            if !KNOWN_TOP_KEYS.contains(&name) && !legacy_keys.iter().any(|legacy| legacy == name) {
                crate::runtime::log::warning(format!(
                    "[OneBot] onebot.yml 存在无法识别的配置项「{name}」，已忽略；本文件只支持 self_id / nickname / send_image_as_file / connections，业务设置请写在 arona.yml"
                ));
            }
        }
    }
    let mut legacy_removed: Vec<String> = Vec::new();
    if let Some(map) = value.as_mapping_mut() {
        for key in &legacy_keys {
            if map.remove(serde_yaml::Value::String(key.clone())).is_some() {
                legacy_removed.push(key.clone());
            }
        }
    }
    let config: OneBotConfig = serde_yaml::from_value(value).map_err(|e| {
        format!(
            "onebot.yml 解析失败，请检查格式（参考同目录说明）: {e}；\
             常见原因：冒号后忘了写值（例如 `type:` 后面是空的）或缩进不一致"
        )
    })?;
    // 连接类型由 type 字段决定，键名任意（模板里就是这么写的）。无法识别的连接以前会让
    // 整份文件加载失败（GUI 子系统下双击启动=毫无反应且不进日志），现在只警告：
    // 未启用的直接跳过，已启用的由 OneBotApplication::start 忽略，一处笔误不再毁掉整份配置。
    for (name, conn) in &config.connections {
        if let Err(err) = conn.resolve_type(name) {
            crate::runtime::log::warning(format!(
                "[OneBot] onebot.yml 的连接「{name}」无法识别类型（{err}）；该项已忽略，请补上 type: ws-forward / ws-reverse / http / http-reverse"
            ));
        }
    }
    if !legacy_removed.is_empty() {
        save(file, &config).map_err(|e| format!("重写 onebot.yml 失败: {e}"))?;
        crate::runtime::log::info(format!(
            "[OneBot] 检测到 onebot.yml 里的业务配置残留（{}），已移除；这些项归 arona.yml",
            legacy_removed.join(" / ")
        ));
    }
    Ok(config)
}

pub fn save(file: &Path, config: &OneBotConfig) -> std::io::Result<()> {
    write_text(file, &template(config))
}

pub fn default_file() -> std::path::PathBuf {
    crate::runtime::paths::default_onebot_file()
}

fn connection_block(key: &str, conn: &ConnectionConfig, comment: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("  # {comment}\n"));
    out.push_str(&format!("  {key}:\n"));
    // 一定要写出「解析后的类型」：老配置的连接没有 type 字段（靠键名判断类型），
    // 直接写 conn.connection_type 会写出空的 `type:`（YAML null），下次启动就解析失败。
    // 认不出类型时写空串（合法 YAML 字符串），避免再生成坏配置。
    let type_name = conn
        .resolve_type(key)
        .map(|conn_type| conn_type.key().to_string())
        .unwrap_or_else(|_| conn.connection_type.trim().to_string());
    out.push_str(&format!("    type: \"{type_name}\"\n"));
    out.push_str(&format!("    enable: {}\n", conn.enable));
    out.push_str(&format!("    host: \"{}\"\n", conn.host));
    out.push_str(&format!("    port: {}\n", conn.port));
    out.push_str(&format!("    url: \"{}\"\n", conn.url));
    out.push_str(&format!("    path: \"{}\"\n", conn.path));
    out.push_str(&format!("    token: \"{}\"\n", conn.token));
    out.push_str(&format!(
        "    heartbeat_interval: {}\n",
        conn.heartbeat_interval
    ));
    out.push_str(&format!(
        "    reconnect_interval: {}\n",
        conn.reconnect_interval
    ));
    out
}

/// 生成带注释的模板文本
fn template(config: &OneBotConfig) -> String {
    let mut out = String::new();
    out.push_str("# ==================== Arona OneBot 配置文件 ====================\n");
    out.push_str("# Rust 移植版（arona-rs）独立运行模式使用本文件。\n");
    out.push_str(
        "# 文件位置：运行目录下的 config/onebot.yml（与框架的 config/arona.yml 同目录），保存后热重载生效。\n",
    );
    out.push_str("# 每个连接的 type 取值：ws-forward / ws-reverse / http / http-reverse；\n");
    out.push_str("# 同一类型可以配置多个实例（键名任意，例如 ws-reverse-2）。\n\n");
    out.push_str(&format!(
        "# 机器人 QQ 号，用于 get_login_info 等 API 的返回\nself_id: {}\n",
        config.self_id
    ));
    out.push_str(&format!(
        "# 机器人昵称\nnickname: \"{}\"\n",
        config.nickname
    ));
    out.push_str("\n# ==================== 消息发送 ====================\n");
    out.push_str(
        "# 发到群里的图片如果来自本机磁盘(插件生成的结果图、文件分享等)，是否改用 file:// 路径直传 OneBot 实现。\n",
    );
    out.push_str(
        "# 开启后上行消息不再内嵌十几 MB 的 base64，发送更快；图片按原始文件上传，不做任何压缩。\n",
    );
    out.push_str("# 注意：仅当 OneBot 实现(如 NapCat)与机器人部署在同一台机器/可直接访问相同磁盘时才能开启，\n");
    out.push_str("#       跨机器/容器部署开启会导致图片发不出去。默认 false(内嵌 base64, 兼容所有部署方式)。\n");
    out.push_str(&format!(
        "send_image_as_file: {}\n",
        config.send_image_as_file
    ));
    out.push_str("\nconnections:\n");
    // 配置里的全部连接 + 四种标准连接（缺失时补默认值）
    let mut connections = config.connections.clone();
    for conn_type in ConnectionType::all() {
        connections
            .entry(conn_type.key().to_string())
            .or_insert_with(|| default_connection(conn_type));
    }
    for (key, conn) in &connections {
        let comment = match conn.resolve_type(key) {
            Ok(ConnectionType::WebSocket) => "正向 WebSocket：主动连接 OneBot 实现（填写 url）",
            Ok(ConnectionType::WebSocketReverse) => {
                "反向 WebSocket：监听端口等待 OneBot 实现连接（填写 host/port/path）"
            }
            Ok(ConnectionType::Http) => "正向 HTTP：开放 API 端口，供 OneBot 实现调用",
            Ok(ConnectionType::HttpReverse) => "反向 HTTP：主动向 url 推送动作（不支持接收事件）",
            Err(_) => "自定义连接（type 字段无效）",
        };
        out.push_str(&connection_block(key, conn, comment));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：连接键名与类型无关（模板里就写着「键名任意，例如 ws-reverse-2」）。
    /// 以前拿键名逐个 `from_name` 校验，任何自定义键名、或误写进 connections 的业务
    /// 字段（例如把图片发送方式 send_image_as_file 写在这里）都会让整份 onebot.yml
    /// 加载失败；含 GUI 的构建是 windows 子系统，双击启动时表现为「毫无反应且不进日志」。
    #[test]
    fn custom_connection_keys_do_not_break_config_load() {
        let file = std::env::temp_dir().join("arona-onebot-parse-test.yml");
        let text = r#"self_id: 123
nickname: "Arona"
send_image_as_file: true
connections:
  arona-1482580133-6701:
    type: ws-reverse
    enable: false
    port: 6701
  send_image_as_file:
    enable: false
"#;
        std::fs::write(&file, text).expect("写入测试配置失败");
        let config = load(&file).expect("自定义键名不应导致加载失败");
        let custom = config
            .connections
            .get("arona-1482580133-6701")
            .expect("自定义键名的连接应保留");
        assert_eq!(
            custom.resolve_type("arona-1482580133-6701").unwrap(),
            ConnectionType::WebSocketReverse
        );
        assert!(config.connections.contains_key("send_image_as_file"));
        // 顶层 send_image_as_file 是本文件的正式配置项（GUI「发送设置」写这里）
        assert!(config.send_image_as_file);
        let _ = std::fs::remove_file(&file);
    }

    /// 图片发送方式存在 onebot.yml：解析 -> 保存 -> 再解析 必须原样保留，
    /// 且新生成的模板里带这一项（GUI 勾选框读写的就是这个字段）
    #[test]
    fn send_image_as_file_round_trips_through_onebot_yml() {
        let file = std::env::temp_dir().join("arona-onebot-send-image-test.yml");
        std::fs::write(&file, "send_image_as_file: true\n").expect("写入测试配置失败");
        let config = load(&file).expect("send_image_as_file 应可解析");
        assert!(config.send_image_as_file);

        save(&file, &config).expect("写回配置失败");
        let text = std::fs::read_to_string(&file).expect("读取配置失败");
        assert!(
            text.contains("send_image_as_file: true"),
            "模板应带上该配置项"
        );
        assert!(load(&file).expect("重新解析失败").send_image_as_file);
        let _ = std::fs::remove_file(&file);

        // 默认值必须是 false（内嵌 base64，兼容所有部署方式）
        let fresh = std::env::temp_dir().join("arona-onebot-send-image-test2.yml");
        let _ = std::fs::remove_file(&fresh);
        let config = load(&fresh).expect("生成默认模板失败");
        assert!(!config.send_image_as_file);
        let text = std::fs::read_to_string(&fresh).expect("读取模板失败");
        assert!(text.contains("send_image_as_file: false"));
        let _ = std::fs::remove_file(&fresh);
    }

    /// 回归：老配置的连接项没有 type 字段（靠键名判断类型），GUI「保存并热重载」
    /// 曾经把它写成空的 `type:`（YAML null），下次启动直接解析失败、程序起不来。
    /// 现在空 type 当作「用键名判断类型」，保存后也必须写出合法值。
    #[test]
    fn null_type_field_is_tolerated_and_saved_back_as_a_real_type() {
        let file = std::env::temp_dir().join("arona-onebot-null-type-test.yml");
        // 与用户现场一致：type 冒号后为空 + 靠键名判断类型
        let text = r#"self_id: 1493074321
nickname: "樱兰羽枫"
send_image_as_file: true
connections:
  http:
    type: 
    enable: false
    port: 5700
  ws-forward:
    type: 
    enable: true
    url: "ws://127.0.0.1:3001"
"#;
        std::fs::write(&file, text).expect("写入测试配置失败");
        let config = load(&file).expect("type 为空不应导致加载失败");
        assert_eq!(config.connections["http"].connection_type, "");
        assert_eq!(
            config.connections["ws-forward"]
                .resolve_type("ws-forward")
                .unwrap(),
            ConnectionType::WebSocket
        );
        assert!(config.send_image_as_file);

        // 保存后必须写出真实类型，不能再产出空 type（否则下次启动又会坏）
        save(&file, &config).expect("写回配置失败");
        let saved = std::fs::read_to_string(&file).expect("读取配置失败");
        assert!(saved.contains("type: \"http\""), "{saved}");
        assert!(saved.contains("type: \"ws-forward\""), "{saved}");
        assert!(
            !saved.lines().any(|line| line.trim() == "type:"),
            "不应再写出空的 type:\n{saved}"
        );
        let reloaded = load(&file).expect("重新解析失败");
        assert_eq!(
            reloaded.connections["http"].resolve_type("http").unwrap(),
            ConnectionType::Http
        );
        assert!(reloaded.send_image_as_file, "保存后图片发送方式不应丢失");
        let _ = std::fs::remove_file(&file);
    }

    /// 类型完全无法识别的连接也只是被忽略（OneBotApplication::start 会跳过它），不再报错
    #[test]
    fn unresolvable_connection_type_is_tolerated() {
        let file = std::env::temp_dir().join("arona-onebot-parse-test2.yml");
        std::fs::write(&file, "connections:\n  whatever:\n    enable: false\n")
            .expect("写入测试配置失败");
        let config = load(&file).expect("无法识别类型的连接不应导致加载失败");
        assert!(config.connections.contains_key("whatever"));
        let _ = std::fs::remove_file(&file);
    }
}
