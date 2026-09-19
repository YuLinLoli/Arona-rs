//! OneBot 应用（对应原版 OneBotApplication）
//! 支持热重载：修改 onebot.yml 后调用 reload 会停止全部连接/服务并按新配置重新启动。
use crate::config::onebot::{ConnectionConfig, ConnectionType, OneBotConfig};
use crate::onebot::business::StandaloneBusinessHandler;
use crate::onebot::connection::{ConnectionCore, ConnectionRegistry, OneBotConnection};
use crate::onebot::http_api::HttpApiServer;
use crate::onebot::http_reverse::{HttpReverseConnection, HttpReverseState};
use crate::onebot::ws_forward::{RunState, WsForwardConnection};
use crate::onebot::ws_reverse::{WsReverseConnection, WsReverseState};
use once_cell::sync::OnceCell;
use std::sync::{Arc, Mutex, RwLock};

pub struct OneBotApplication {
    config: RwLock<OneBotConfig>,
    pub business: Arc<StandaloneBusinessHandler>,
    pub registry: Arc<ConnectionRegistry>,
    connections: Mutex<Vec<Arc<dyn OneBotConnection>>>,
    http_servers: Mutex<Vec<Arc<HttpApiServer>>>,
}

impl OneBotApplication {
    pub fn new(
        config: OneBotConfig,
        business: Arc<StandaloneBusinessHandler>,
        registry: Arc<ConnectionRegistry>,
    ) -> OneBotApplication {
        // 图片发送方式(内嵌 base64 / file:// 直传)存在 onebot.yml，启动时同步到运行期开关
        crate::runtime::config::set_send_image_as_file(config.send_image_as_file);
        crate::runtime::log::info(format!(
            "[OneBot] 图片发送方式: {}",
            if config.send_image_as_file {
                "file:// 直传(原图上传, 不压缩)"
            } else {
                "内嵌 base64(兼容所有部署方式)"
            }
        ));
        OneBotApplication {
            config: RwLock::new(config),
            business,
            registry,
            connections: Mutex::new(Vec::new()),
            http_servers: Mutex::new(Vec::new()),
        }
    }

    /// 图片发送方式热生效：同步到内存配置与运行期开关（GUI「发送设置」勾选后立即调用）
    pub fn apply_send_image_as_file(&self, enabled: bool) {
        {
            let mut guard = self.config.write().unwrap();
            guard.send_image_as_file = enabled;
        }
        crate::runtime::config::set_send_image_as_file(enabled);
        crate::runtime::log::info(format!(
            "[OneBot] 图片发送方式已切换为{}",
            if enabled {
                "file:// 直传(原图上传, 不压缩)"
            } else {
                "内嵌 base64(兼容所有部署方式)"
            }
        ));
    }

    /// 当前 OneBot 配置快照
    pub fn config(&self) -> OneBotConfig {
        self.config.read().unwrap().clone()
    }

    pub fn start(&self) {
        let enabled: Vec<(String, ConnectionConfig)> = self
            .config()
            .connections
            .iter()
            .filter(|(_, conn)| conn.enable)
            .map(|(name, conn)| (name.clone(), conn.clone()))
            .collect();
        for (name, endpoint) in enabled {
            let conn_type = match endpoint.resolve_type(&name) {
                Ok(t) => t,
                Err(err) => {
                    crate::runtime::log::warning(format!("[OneBot] 连接 {name} 类型无效: {err}"));
                    continue;
                }
            };
            let address = endpoint.address(conn_type);
            crate::runtime::log::info(format!(
                "[OneBot] 启动连接 {} ({name}): {address}",
                conn_type.display_name()
            ));
            match conn_type {
                ConnectionType::WebSocket => {
                    let core = Arc::new(ConnectionCore::new(self.business.clone()));
                    let conn = Arc::new(WsForwardConnection {
                        state: RunState::new(&endpoint, core),
                        task: Mutex::new(None),
                    });
                    conn.start();
                    self.registry.register(conn.clone());
                    self.connections.lock().unwrap().push(conn);
                }
                ConnectionType::WebSocketReverse => {
                    let core = Arc::new(ConnectionCore::new(self.business.clone()));
                    let conn = Arc::new(WsReverseConnection {
                        state: WsReverseState::new(&endpoint, core),
                        task: Mutex::new(None),
                    });
                    conn.start();
                    self.registry.register(conn.clone());
                    self.connections.lock().unwrap().push(conn);
                }
                ConnectionType::Http => {
                    let core = Arc::new(ConnectionCore::new(self.business.clone()));
                    let server = Arc::new(HttpApiServer::new(&endpoint, core));
                    server.start();
                    self.http_servers.lock().unwrap().push(server);
                }
                ConnectionType::HttpReverse => {
                    let core = Arc::new(ConnectionCore::new(self.business.clone()));
                    let conn = Arc::new(HttpReverseConnection {
                        state: HttpReverseState::new(&endpoint, core),
                        task: Mutex::new(None),
                    });
                    conn.start();
                    self.registry.register(conn.clone());
                    self.connections.lock().unwrap().push(conn);
                }
            }
        }
    }

    /// 停止全部连接与 HTTP 服务，并清空注册表
    pub fn stop(&self) {
        let connections: Vec<Arc<dyn OneBotConnection>> = {
            let mut guard = self.connections.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        for connection in connections.iter() {
            connection.stop();
        }
        let servers: Vec<Arc<HttpApiServer>> = {
            let mut guard = self.http_servers.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        for server in servers.iter() {
            server.stop();
        }
        self.registry.clear();
    }

    /// 热重载：按新配置重启全部连接（必须在 tokio 运行期内调用）
    pub fn reload(&self, new_config: OneBotConfig) {
        crate::runtime::log::info("[OneBot] 正在应用新配置（热重载连接）...");
        self.stop();
        {
            let mut guard = self.config.write().unwrap();
            *guard = new_config.clone();
        }
        // 图片发送方式也在 onebot.yml 里，热重载时一并生效
        crate::runtime::config::set_send_image_as_file(new_config.send_image_as_file);
        self.business.update_config(new_config.clone());
        self.start();
        crate::runtime::log::info("[OneBot] 连接热重载完成");
    }

    /// 当前首个可用连接（对应 firstConnection）
    pub fn first_connection(&self) -> Option<Arc<dyn OneBotConnection>> {
        self.registry.first()
    }
}

/// 全局 OneBot 应用句柄（GUI 管理面板与热重载使用）
static GLOBAL: OnceCell<Arc<OneBotApplication>> = OnceCell::new();

pub fn set_global(application: Arc<OneBotApplication>) {
    let _ = GLOBAL.set(application);
}

pub fn global() -> Option<Arc<OneBotApplication>> {
    GLOBAL.get().cloned()
}
