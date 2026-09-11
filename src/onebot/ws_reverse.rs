//! 反向 WebSocket 连接（对应原版 OneBotWsReverseConnection）
use crate::config::onebot::ConnectionConfig;
use crate::onebot::business::StandaloneBusinessHandler;
use crate::onebot::connection::{
    ConnectionCore, OneBotConnection, SendWait, begin_send, next_conn_id, receive_payload,
};
use crate::onebot::model::OneBotAction;
use crate::runtime::log;
use crate::runtime::message::BoxFuture;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

pub struct WsReverseState {
    pub id: u64,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub token: String,
    pub heartbeat_interval_ms: u64,
    pub core: Arc<ConnectionCore>,
    pub clients: Mutex<Vec<UnboundedSender<String>>>,
    pub stopped: AtomicBool,
}

impl WsReverseState {
    pub fn new(endpoint: &ConnectionConfig, core: Arc<ConnectionCore>) -> Arc<WsReverseState> {
        Arc::new(WsReverseState {
            id: next_conn_id(),
            host: endpoint.host.clone(),
            port: endpoint.port,
            path: if endpoint.path.is_empty() {
                "/".to_string()
            } else {
                endpoint.path.clone()
            },
            token: endpoint.token.clone(),
            heartbeat_interval_ms: endpoint.heartbeat_interval,
            core,
            clients: Mutex::new(Vec::new()),
            stopped: AtomicBool::new(false),
        })
    }
}

pub struct WsReverseConnection {
    pub state: Arc<WsReverseState>,
    pub task: Mutex<Option<JoinHandle<()>>>,
}

/// 代理（供事件处理与动作回复使用，与连接对象共享 state）
pub struct WsReverseProxy {
    pub state: Arc<WsReverseState>,
}

impl OneBotConnection for WsReverseProxy {
    fn id(&self) -> u64 {
        self.state.id
    }

    fn send<'a>(
        &'a self,
        action: OneBotAction,
    ) -> BoxFuture<'a, Option<crate::onebot::model::OneBotActionResponse>> {
        let state = self.state.clone();
        Box::pin(async move {
            let clients = state.clients.lock().unwrap().clone();
            let wait: SendWait = begin_send(
                &state.core.pending,
                move |payload| {
                    let mut sent = false;
                    for client in &clients {
                        let _ = client.send(payload.to_string());
                        sent = true;
                    }
                    if sent {
                        Ok(())
                    } else {
                        Err("Reverse WebSocket has no clients".to_string())
                    }
                },
                action,
            );
            wait.wait(std::time::Duration::from_millis(
                crate::onebot::connection::ACTION_TIMEOUT_MILLIS,
            ))
            .await
        })
    }

    fn send_raw(&self, payload: &str) {
        let clients = self.state.clients.lock().unwrap();
        for client in clients.iter() {
            let _ = client.send(payload.to_string());
        }
    }

    fn start(&self) {}

    fn stop(&self) {
        self.state.stopped.store(true, Ordering::SeqCst);
    }
}

impl OneBotConnection for WsReverseConnection {
    fn id(&self) -> u64 {
        self.state.id
    }

    fn send<'a>(
        &'a self,
        action: OneBotAction,
    ) -> BoxFuture<'a, Option<crate::onebot::model::OneBotActionResponse>> {
        let proxy = WsReverseProxy {
            state: self.state.clone(),
        };
        Box::pin(async move { proxy.send(action).await })
    }

    fn send_raw(&self, payload: &str) {
        let proxy = WsReverseProxy {
            state: self.state.clone(),
        };
        proxy.send_raw(payload);
    }

    fn start(&self) {
        let state = self.state.clone();
        let mut task = self.task.lock().unwrap();
        *task = Some(tokio::spawn(async move {
            run_reverse_server(state).await;
        }));
    }

    fn stop(&self) {
        self.state.stopped.store(true, Ordering::SeqCst);
        if let Ok(mut task) = self.task.lock() {
            if let Some(handle) = task.take() {
                handle.abort();
            }
        }
    }
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    if token.is_empty() {
        return true;
    }
    headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("Bearer {token}"))
        .unwrap_or(false)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<Arc<WsReverseState>>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state.clone()))
        .into_response()
}

async fn handle_socket(socket: WebSocket, state: Arc<WsReverseState>) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = unbounded_channel::<String>();
    {
        let mut clients = state.clients.lock().unwrap();
        clients.push(tx);
    }
    log::info(format!("[OneBot reverse WS] 客户端已连接: {}", state.id));
    let proxy = Arc::new(WsReverseProxy {
        state: state.clone(),
    });
    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        receive_payload(&state.core, proxy.clone(), text.as_str());
                    }
                    Some(Ok(Message::Binary(bin))) => {
                        if let Ok(text) = String::from_utf8(bin.to_vec()) {
                            receive_payload(&state.core, proxy.clone(), &text);
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = sink.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        log::warning(format!("[OneBot reverse WS] 错误: {err}"));
                        break;
                    }
                    None => break,
                }
            }
            payload = rx.recv() => {
                match payload {
                    Some(payload) => {
                        if payload.is_empty() { break; }
                        if sink.send(Message::Text(payload.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    let mut clients = state.clients.lock().unwrap();
    clients.retain(|c| !c.is_closed());
    log::info("[OneBot reverse WS] 客户端断开");
}

async fn run_reverse_server(state: Arc<WsReverseState>) {
    let address = format!("{}:{}", state.host, state.port);
    let app: axum::Router = Router::new()
        .route(&state.path, get(ws_handler))
        .with_state(state.clone());
    let listener = match tokio::net::TcpListener::bind(&address).await {
        Ok(listener) => listener,
        Err(err) => {
            log::warning(format!("[OneBot reverse WS] 启动失败: {err}"));
            // 重试绑定
            while !state.stopped.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                match tokio::net::TcpListener::bind(&address).await {
                    Ok(listener) => {
                        run_server(state.clone(), listener).await;
                        return;
                    }
                    Err(_) => continue,
                }
            }
            return;
        }
    };
    run_server(state.clone(), listener).await;
}

async fn run_server(state: Arc<WsReverseState>, listener: tokio::net::TcpListener) {
    log::info(format!(
        "[OneBot reverse WS] 已启动: {}:{}",
        state.host, state.port
    ));
    let app = Router::new()
        .route(&state.path, get(ws_handler))
        .with_state(state.clone());
    if let Err(err) = axum::serve(listener, app).await {
        log::warning(format!("[OneBot reverse WS] 服务退出: {err}"));
    }
    log::warning(format!(
        "[OneBot reverse WS] 服务已停止: {}:{}",
        state.host, state.port
    ));
}
