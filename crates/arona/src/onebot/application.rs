//! OneBot 应用（对应原版 OneBotApplication）
//! 支持热重载：修改 onebot.yml 后调用 reload 会停止全部连接/服务并按新配置重新启动。
use crate::config::onebot::{ConnectionConfig, ConnectionType, OneBotConfig};
use crate::onebot::business::BusinessHandler;
use crate::onebot::connection::{ConnectionCore, ConnectionRegistry, OneBotConnection};
use crate::onebot::http_api::HttpApiServer;
use crate::onebot::http_reverse::{HttpReverseConnection, HttpReverseState};
use crate::onebot::ws_forward::{RunState, WsForwardConnection};
use crate::onebot::ws_reverse::{WsReverseConnection, WsReverseState};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub struct OneBotApplication {
    config: RwLock<OneBotConfig>,
    pub business: Arc<BusinessHandler>,
    pub registry: Arc<ConnectionRegistry>,
    connections: Mutex<Vec<Arc<dyn OneBotConnection>>>,
    http_servers: Mutex<Vec<Arc<HttpApiServer>>>,
}

impl OneBotApplication {
    pub fn new(
        config: OneBotConfig,
        business: Arc<BusinessHandler>,
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

/// OneBot 应用与连接池的持有者，挂在 [`Framework`](crate::framework::Framework) 实例上。
///
/// 这里曾经是一枚 `static GLOBAL: OnceCell`。进程级 `static` 对插件是致命的：动态插件 dll
/// 静态链接了自己一整份框架代码，`OnceCell` 于是各进程镜像里有一份、彼此不通，宿主连上了
/// 实现端，插件里 `OneBotApi` 的每一次动作仍然只拿得到 `NoConnection`。挂到实例上之后，
/// dll 经 `Framework::adopt_host` 接到的是宿主这一份，连接、self_id 都是宿主真正在用的。
#[derive(Default)]
pub struct OnebotOutlet {
    application: RwLock<Option<Arc<OneBotApplication>>>,
    self_id: AtomicI64,
}

impl OnebotOutlet {
    /// 认领一份 OneBot 应用（宿主启动时调用；热重载换对象时再次覆盖即可）
    pub fn attach(&self, application: Arc<OneBotApplication>) {
        let self_id = application.config().self_id;
        *self.application.write().unwrap() = Some(application);
        self.self_id.store(self_id, Ordering::SeqCst);
    }

    /// 当前挂着的 OneBot 应用（GUI 管理面板与连接热重载用它）
    pub fn application(&self) -> Option<Arc<OneBotApplication>> {
        self.application.read().unwrap().clone()
    }

    /// 连接登记表；没有应用对象时就没有登记簿
    pub fn registry(&self) -> Option<Arc<ConnectionRegistry>> {
        self.application().map(|app| app.registry.clone())
    }

    /// 首个可用连接：所有 OneBot 动作的实际出口
    pub fn first_connection(&self) -> Option<Arc<dyn OneBotConnection>> {
        self.application().and_then(|app| app.first_connection())
    }

    /// 机器人 QQ 号（onebot.yml 的 self_id）
    pub fn self_id(&self) -> i64 {
        self.self_id.load(Ordering::SeqCst)
    }

    /// 此刻是否连着实现端
    pub fn is_online(&self) -> bool {
        self.first_connection().is_some()
    }
}

/// 发布 OneBot 应用句柄：交给**进程默认实例**的那份 [`OnebotOutlet`]
pub fn set_global(application: Arc<OneBotApplication>) {
    crate::framework::Framework::global()
        .onebot()
        .attach(application);
}

/// 当前挂着的 OneBot 应用
pub fn global() -> Option<Arc<OneBotApplication>> {
    crate::framework::Framework::global().onebot().application()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::Framework;

    /// 连接句柄属于实例：插件 dll 里 `OneBotApi` 取的就是它认领的那套 `Framework` 上的
    /// outlet。换成进程级 `static OnceCell` 后，dll 那份从来没人 `set`，宿主明明连着，
    /// 插件每次动作却只拿得到 `NoConnection`——这条断言把"没认领就是空"钉住。
    #[test]
    fn outlet_only_sees_the_application_its_own_instance_holds() {
        let one = Framework::new();
        let other = Framework::new();
        assert!(
            !std::ptr::eq(Arc::as_ptr(one.onebot()), Arc::as_ptr(other.onebot())),
            "两套实例不该共用同一份连接句柄"
        );
        assert!(one.onebot().application().is_none(), "新实例一开始谁也没连");
        assert!(one.onebot().registry().is_none());
        assert!(!one.onebot().is_online());
        assert_eq!(one.onebot().self_id(), 0);
    }
}
