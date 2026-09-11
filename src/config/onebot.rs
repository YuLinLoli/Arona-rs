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
    #[serde(default, rename = "type")]
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
    let mut legacy_removed = false;
    if let Some(map) = value.as_mapping_mut() {
        for key in ["groups", "managers", "notify"] {
            if map
                .remove(serde_yaml::Value::String(key.to_string()))
                .is_some()
            {
                legacy_removed = true;
            }
        }
    }
    let config: OneBotConfig = serde_yaml::from_value(value)
        .map_err(|e| format!("onebot.yml 解析失败，请检查格式（参考同目录说明）: {e}"))?;
    for name in config.connections.keys() {
        ConnectionType::from_name(name)?;
    }
    if legacy_removed {
        save(file, &config).map_err(|e| format!("重写 onebot.yml 失败: {e}"))?;
        crate::runtime::log::info(
            "[OneBot] 检测到 onebot.yml 中的 groups/managers/notify 残留，已移除，请统一在 arona.yml 中配置",
        );
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
    out.push_str(&format!("    type: {}\n", conn.connection_type));
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
    out.push_str("# 文件位置：与可执行文件同级的 arona-standalone/onebot.yml，保存后热重载生效。\n");
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
