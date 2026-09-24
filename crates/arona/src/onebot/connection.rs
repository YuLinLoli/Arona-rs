//! OneBot 连接抽象（对应原版 OneBotConnection / AbstractOneBotConnection）
use crate::onebot::business::BusinessHandler;
use crate::onebot::model::{OneBotAction, OneBotActionResponse, ParsedPayload};
use crate::onebot::protocol;
use crate::runtime::message::BoxFuture;
use once_cell::sync::OnceCell;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub const ACTION_TIMEOUT_MILLIS: u64 = 15_000;

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// 等待动作响应的句柄
pub struct SendWait {
    rx: Option<tokio::sync::oneshot::Receiver<Value>>,
}

impl SendWait {
    pub fn empty() -> SendWait {
        SendWait { rx: None }
    }

    pub async fn wait(self, timeout: Duration) -> Option<OneBotActionResponse> {
        let rx = match self.rx {
            Some(rx) => rx,
            None => return None,
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(value)) => match protocol::parse_payload(&value.to_string()) {
                Some(ParsedPayload::Response(response)) => Some(response),
                _ => None,
            },
            _ => None,
        }
    }
}

/// 动作 echo -> 响应的等待表，带超时清理
pub struct Pending {
    map: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<Value>>>>,
}

impl Pending {
    pub fn new() -> Pending {
        Pending {
            map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn track(&self, echo: &str, sender: tokio::sync::oneshot::Sender<Value>) {
        self.map.lock().unwrap().insert(echo.to_string(), sender);
        let echo = echo.to_string();
        let map = self.map.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ACTION_TIMEOUT_MILLIS)).await;
            if let Ok(mut guard) = map.lock() {
                guard.remove(&echo);
            }
        });
    }

    /// 收到响应时按 echo 完成等待
    pub fn resolve(&self, response: &OneBotActionResponse) {
        let Some(echo) = &response.echo else { return };
        let sender = self.map.lock().unwrap().remove(echo);
        if let Some(sender) = sender {
            let _ = sender.send(response.raw.clone());
        }
    }

    /// 传输失败时移除（丢弃等待）
    pub fn forget(&self, echo: &str) {
        self.map.lock().unwrap().remove(echo);
    }
}

/// 连接注册表（对应原版 OneBotApplication 持有的连接列表）
pub struct ConnectionRegistry {
    connections: RwLock<Vec<Arc<dyn OneBotConnection>>>,
}

impl ConnectionRegistry {
    pub fn new() -> ConnectionRegistry {
        ConnectionRegistry {
            connections: RwLock::new(Vec::new()),
        }
    }

    pub fn register(&self, connection: Arc<dyn OneBotConnection>) {
        self.connections.write().unwrap().push(connection);
    }

    pub fn first(&self) -> Option<Arc<dyn OneBotConnection>> {
        self.connections.read().unwrap().first().cloned()
    }

    pub fn count(&self) -> usize {
        self.connections.read().unwrap().len()
    }

    pub fn all(&self) -> Vec<Arc<dyn OneBotConnection>> {
        self.connections.read().unwrap().clone()
    }

    /// 清空注册表（连接热重载时使用）
    pub fn clear(&self) {
        self.connections.write().unwrap().clear();
    }

    /// 广播事件给其它连接（对应 broadcastExcept）
    pub fn broadcast_except(&self, event: &Value, except_id: u64) {
        let payload = event.to_string();
        let connections = self.connections.read().unwrap();
        for connection in connections.iter() {
            if connection.id() != except_id {
                connection.send_raw(&payload);
            }
        }
    }
}

/// OneBot 连接
pub trait OneBotConnection: Send + Sync {
    fn id(&self) -> u64;

    /// 发送动作并等待响应（15 秒超时，失败/超时返回 None）
    fn send<'a>(&'a self, action: OneBotAction) -> BoxFuture<'a, Option<OneBotActionResponse>>;

    fn send_raw(&self, payload: &str);

    fn start(&self);

    fn stop(&self);
}

/// 共享的连接核心：pending 表 + 业务处理器
pub struct ConnectionCore {
    pub pending: Arc<Pending>,
    pub business: Arc<BusinessHandler>,
}

impl ConnectionCore {
    pub fn new(business: Arc<BusinessHandler>) -> ConnectionCore {
        ConnectionCore {
            pending: Arc::new(Pending::new()),
            business,
        }
    }
}

impl Clone for ConnectionCore {
    fn clone(&self) -> ConnectionCore {
        ConnectionCore {
            pending: self.pending.clone(),
            business: self.business.clone(),
        }
    }
}

pub fn next_conn_id() -> u64 {
    NEXT_CONN_ID.fetch_add(1, Ordering::SeqCst)
}

/// 注册发送等待并尝试投递文本，返回 SendWait
pub fn begin_send(
    pending: &Arc<Pending>,
    deliver: impl FnOnce(&str) -> Result<(), String>,
    action: OneBotAction,
) -> SendWait {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let echo = action.echo.clone();
    pending.track(&echo, tx);
    let payload = protocol::serialize_action(&action);
    match deliver(&payload) {
        Ok(()) => SendWait { rx: Some(rx) },
        Err(_) => {
            pending.forget(&echo);
            SendWait { rx: None }
        }
    }
}

/// 处理收到的 payload（事件/动作/响应）
pub fn receive_payload(core: &ConnectionCore, conn: Arc<dyn OneBotConnection>, payload: &str) {
    let Some(parsed) = protocol::parse_payload(payload) else {
        crate::runtime::log::warning(format!("[OneBot invalid JSON] {payload}"));
        return;
    };
    match parsed {
        ParsedPayload::Event(event) => core.business.on_event(event, conn),
        ParsedPayload::Action(action) => {
            let business = core.business.clone();
            let conn = conn.clone();
            let echo = action.echo.clone();
            tokio::spawn(async move {
                let result = business.handle_action(&action).await;
                let response = match result {
                    Ok(data) => {
                        let data = data.unwrap_or(Value::Null);
                        serde_json::json!({
                            "status": "ok",
                            "retcode": 0,
                            "data": data,
                            "echo": echo,
                        })
                    }
                    Err(err) => serde_json::json!({
                        "status": "failed",
                        "retcode": 1,
                        "data": Value::Null,
                        "message": err,
                        "echo": echo,
                    }),
                };
                conn.send_raw(&response.to_string());
            });
        }
        ParsedPayload::Response(response) => {
            core.pending.resolve(&response);
        }
    }
}

// 为 pending/registry 提供共享全局（简化跨模块访问）
static GLOBAL_REGISTRY: OnceCell<Arc<ConnectionRegistry>> = OnceCell::new();

pub fn set_global_registry(registry: Arc<ConnectionRegistry>) {
    let _ = GLOBAL_REGISTRY.set(registry);
}

pub fn global_registry() -> Option<Arc<ConnectionRegistry>> {
    GLOBAL_REGISTRY.get().cloned()
}
