//! 正向 WebSocket 连接（对应原版 OneBotWsForwardConnection）
use crate::config::onebot::ConnectionConfig;
use crate::onebot::business::StandaloneBusinessHandler;
use crate::onebot::connection::{
    ConnectionCore, OneBotConnection, SendWait, begin_send, next_conn_id, receive_payload,
};
use crate::onebot::model::OneBotAction;
use crate::onebot::protocol;
use crate::runtime::log;
use crate::runtime::message::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

pub struct RunState {
    pub id: u64,
    pub url: String,
    pub token: String,
    pub heartbeat_interval_ms: u64,
    pub reconnect_interval_ms: u64,
    pub core: Arc<ConnectionCore>,
    pub session_tx: Mutex<Option<UnboundedSender<String>>>,
    pub stopped: AtomicBool,
}

impl RunState {
    pub fn new(endpoint: &ConnectionConfig, core: Arc<ConnectionCore>) -> Arc<RunState> {
        Arc::new(RunState {
            id: next_conn_id(),
            url: endpoint.url.clone(),
            token: endpoint.token.clone(),
            heartbeat_interval_ms: endpoint.heartbeat_interval,
            reconnect_interval_ms: endpoint.reconnect_interval.max(1000),
            core,
            session_tx: Mutex::new(None),
            stopped: AtomicBool::new(false),
        })
    }
}

/// 正向 WS 连接（对外暴露的 OneBotConnection）
pub struct WsForwardConnection {
    pub state: Arc<RunState>,
    pub task: Mutex<Option<JoinHandle<()>>>,
}

/// 供后台循环/事件处理复用的连接代理（共享同一 state）
pub struct WsProxy {
    pub state: Arc<RunState>,
}

impl OneBotConnection for WsProxy {
    fn id(&self) -> u64 {
        self.state.id
    }

    fn send<'a>(
        &'a self,
        action: OneBotAction,
    ) -> BoxFuture<'a, Option<crate::onebot::model::OneBotActionResponse>> {
        let state = self.state.clone();
        Box::pin(async move {
            let tx = state.session_tx.lock().unwrap().clone();
            let wait: SendWait = match tx {
                Some(tx) => begin_send(
                    &state.core.pending,
                    move |payload| {
                        tx.send(payload.to_string())
                            .map_err(|_| "WebSocket is not connected".to_string())
                    },
                    action,
                ),
                None => SendWait::empty(),
            };
            wait.wait(std::time::Duration::from_millis(
                crate::onebot::connection::ACTION_TIMEOUT_MILLIS,
            ))
            .await
        })
    }

    fn send_raw(&self, payload: &str) {
        if let Some(tx) = self.state.session_tx.lock().unwrap().clone() {
            let _ = tx.send(payload.to_string());
        }
    }

    fn start(&self) {}

    fn stop(&self) {
        self.state.stopped.store(true, Ordering::SeqCst);
        if let Some(tx) = self.state.session_tx.lock().unwrap().clone() {
            let _ = tx.send(String::new());
        }
    }
}

impl OneBotConnection for WsForwardConnection {
    fn id(&self) -> u64 {
        self.state.id
    }

    fn send<'a>(
        &'a self,
        action: OneBotAction,
    ) -> BoxFuture<'a, Option<crate::onebot::model::OneBotActionResponse>> {
        let proxy = WsProxy {
            state: self.state.clone(),
        };
        Box::pin(async move { proxy.send(action).await })
    }

    fn send_raw(&self, payload: &str) {
        let proxy = WsProxy {
            state: self.state.clone(),
        };
        proxy.send_raw(payload);
    }

    fn start(&self) {
        let state = self.state.clone();
        let mut task = self.task.lock().unwrap();
        *task = Some(tokio::spawn(async move {
            run_forward(state).await;
        }));
    }

    fn stop(&self) {
        self.state.stopped.store(true, Ordering::SeqCst);
        if let Some(tx) = self.state.session_tx.lock().unwrap().clone() {
            let _ = tx.send(String::new());
        }
        if let Ok(mut task) = self.task.lock() {
            if let Some(handle) = task.take() {
                handle.abort();
            }
        }
    }
}

async fn connect(
    state: &Arc<RunState>,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = state
        .url
        .as_str()
        .into_client_request()
        .map_err(|e| e.to_string())?;
    if !state.token.is_empty() {
        let value = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!(
            "Bearer {}",
            state.token
        ))
        .map_err(|e| e.to_string())?;
        request.headers_mut().insert("Authorization", value);
    }
    let (ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ws)
}

async fn run_forward(state: Arc<RunState>) {
    let mut fail_count: u32 = 0;
    while !state.stopped.load(Ordering::SeqCst) {
        let ws = match connect(&state).await {
            Ok(ws) => ws,
            Err(err) => {
                log::warning(format!("[OneBot WS] 连接失败: {err}"));
                fail_count += 1;
                let rest = fail_count % 5 == 0;
                let delay_ms = if rest {
                    30_000
                } else {
                    state.reconnect_interval_ms
                };
                if rest {
                    log::warning("[OneBot WS] 连续 5 次连接失败，休息 30 秒后重试");
                } else {
                    log::warning(format!(
                        "[OneBot WS] 连接失败（第 {fail_count} 次），{} 秒后重试",
                        delay_ms / 1000
                    ));
                }
                if state.stopped.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                continue;
            }
        };
        fail_count = 0;
        log::info(format!("[OneBot WS] 已连接: {}", state.url));
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = unbounded_channel::<String>();
        *state.session_tx.lock().unwrap() = Some(tx);
        let proxy = Arc::new(WsProxy {
            state: state.clone(),
        });
        let heartbeat_ms = state.heartbeat_interval_ms.max(1000);
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_millis(heartbeat_ms));
        heartbeat.tick().await;
        loop {
            if state.stopped.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                msg = stream.next() => {
                    match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                            receive_payload(&state.core, proxy.clone(), &text);
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(bin))) => {
                            if let Ok(text) = String::from_utf8(bin.to_vec()) {
                                receive_payload(&state.core, proxy.clone(), &text);
                            }
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(payload))) => {
                            let _ = sink.send(tokio_tungstenite::tungstenite::Message::Pong(payload)).await;
                        }
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Pong(_))) => {}
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            log::warning(format!("[OneBot WS] 连接错误: {err}"));
                            break;
                        }
                        None => {
                            log::info("[OneBot WS] 连接已断开");
                            break;
                        }
                    }
                }
                payload = rx.recv() => {
                    match payload {
                        Some(payload) => {
                            if payload.is_empty() { break; }
                            if sink.send(tokio_tungstenite::tungstenite::Message::Text(payload)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = heartbeat.tick() => {
                    let params = json!({ "status": true, "good": true });
                    let _ = proxy.send(protocol::action("get_status", params)).await;
                }
            }
        }
        *state.session_tx.lock().unwrap() = None;
        if !state.stopped.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(
                state.reconnect_interval_ms,
            ))
            .await;
        }
    }
}
