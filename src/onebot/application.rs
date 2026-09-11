//! OneBot 应用（对应原版 OneBotApplication）
use crate::config::onebot::{ConnectionConfig, ConnectionType, OneBotConfig};
use crate::onebot::business::StandaloneBusinessHandler;
use crate::onebot::connection::{ConnectionCore, ConnectionRegistry, OneBotConnection};
use crate::onebot::http_api::HttpApiServer;
use crate::onebot::http_reverse::{HttpReverseConnection, HttpReverseState};
use crate::onebot::ws_forward::{RunState, WsForwardConnection};
use crate::onebot::ws_reverse::{WsReverseConnection, WsReverseState};
use std::sync::{Arc, Mutex};

pub struct OneBotApplication {
    pub config: OneBotConfig,
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
        OneBotApplication {
            config,
            business,
            registry,
            connections: Mutex::new(Vec::new()),
            http_servers: Mutex::new(Vec::new()),
        }
    }

    pub fn start(&self) {
        let enabled: Vec<(String, ConnectionConfig)> = self
            .config
            .connections
            .iter()
            .filter(|(_, conn)| conn.enable)
            .map(|(name, conn)| (name.clone(), conn.clone()))
            .collect();
        for (name, endpoint) in enabled {
            let conn_type = match ConnectionType::from_name(&name) {
                Ok(t) => t,
                Err(err) => {
                    crate::runtime::log::warning(format!("[OneBot] 连接 {name} 类型无效: {err}"));
                    continue;
                }
            };
            let address = match conn_type {
                ConnectionType::WebSocket | ConnectionType::HttpReverse => endpoint.url.clone(),
                _ => format!("{}:{}{}", endpoint.host, endpoint.port, endpoint.path),
            };
            crate::runtime::log::info(format!(
                "[OneBot] 启动连接 {}: {address}",
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

    pub fn stop(&self) {
        let connections = self.connections.lock().unwrap();
        for connection in connections.iter() {
            connection.stop();
        }
        drop(connections);
        let http_servers = self.http_servers.lock().unwrap();
        for server in http_servers.iter() {
            server.stop();
        }
    }

    /// 当前首个可用连接（对应 firstConnection）
    pub fn first_connection(&self) -> Option<Arc<dyn OneBotConnection>> {
        self.registry.first()
    }
}
