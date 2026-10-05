//! 正向 WebSocket 连接（对应原版 OneBotWsForwardConnection）
use crate::config::onebot::ConnectionConfig;
use crate::onebot::connection::{
    ConnectionCore, OneBotConnection, SendWait, WRITE_TIMEOUT_MILLIS, begin_send, next_conn_id,
    receive_payload,
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
use tokio_tungstenite::tungstenite::Message;

pub struct RunState {
    pub id: u64,
    pub url: String,
    pub token: String,
    pub heartbeat_interval_ms: u64,
    pub reconnect_interval_ms: u64,
    pub core: Arc<ConnectionCore>,
    pub session_tx: Mutex<Option<UnboundedSender<Message>>>,
    pub stopped: AtomicBool,
    /// 当前是否握着一条活 socket：只在 0↔1 翻转时给插件发 lifecycle，重连过程中的每次失败不发
    pub online: AtomicBool,
    /// 读循环的唤醒信号：stop() 时用它立刻打断 select!（Notify 会保存一次许可，不会丢事件）
    pub stop_notify: tokio::sync::Notify,
    /// 写侧卡死的唤醒信号：写任务超时后用它让读循环退出，好让这条连接去重连
    pub stalled: tokio::sync::Notify,
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
            online: AtomicBool::new(false),
            stop_notify: tokio::sync::Notify::new(),
            stalled: tokio::sync::Notify::new(),
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

impl WsProxy {
    /// 把一帧消息直接塞进写队列（Pong 等控制帧），不阻塞读循环
    pub fn send_message(&self, message: Message) {
        if let Some(tx) = self.state.session_tx.lock().unwrap().clone() {
            let _ = tx.send(message);
        }
    }
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
                        tx.send(Message::Text(payload.to_string()))
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
            let _ = tx.send(Message::Text(payload.to_string()));
        }
    }

    fn start(&self) {}

    fn stop(&self) {
        self.state.stopped.store(true, Ordering::SeqCst);
        self.state.stop_notify.notify_one();
        if let Some(tx) = self.state.session_tx.lock().unwrap().clone() {
            let _ = tx.send(Message::Close(None));
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
        self.state.stop_notify.notify_one();
        if let Some(tx) = self.state.session_tx.lock().unwrap().clone() {
            let _ = tx.send(Message::Close(None));
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
        if !state.online.swap(true, Ordering::SeqCst) {
            state.core.business.notify_lifecycle("connect", &state.url);
        }
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = unbounded_channel::<Message>();
        *state.session_tx.lock().unwrap() = Some(tx);
        // 独立的写任务：所有待发 payload 都排在这里，socket 写只占用这个任务。
        // 原来 socket 写直接跑在读循环里，发送大图(base64)等大负载时 sink.send().await
        // 会把「收消息 + 打印日志」整条读循环一并堵住，控制台因此卡住不动。
        let writer_state = state.clone();
        let writer = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if matches!(&message, Message::Text(text) if text.is_empty()) {
                    break;
                }
                // 停用（GUI 关插件、配置热重载、panic 隔离）之后不再往外刷：这条 while 一退出，
                // `rx` 连同**已经积压的整段队列**一起被丢弃。原来这里没有检查，`stop()` 又是
                // abort 主任务，于是那句 `writer.abort()` 根本轮不到跑，僵尸写任务抱着队列常驻。
                if writer_state.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let closing = matches!(message, Message::Close(_));
                match tokio::time::timeout(
                    std::time::Duration::from_millis(WRITE_TIMEOUT_MILLIS),
                    sink.send(message),
                )
                .await
                {
                    Err(_) => {
                        // 对端整整 WRITE_TIMEOUT_MILLIS 都没读过 socket 才会走到这里：把它当已废的连接处理
                        log::warning(format!(
                            "[OneBot WS] 写出超时（{} 秒没写出去），断开这条连接以丢掉出站积压",
                            WRITE_TIMEOUT_MILLIS / 1000
                        ));
                        writer_state.stalled.notify_one();
                        break;
                    }
                    Ok(Err(_)) => break,
                    Ok(Ok(())) => {
                        if closing {
                            break;
                        }
                    }
                }
            }
        });
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
                        Some(Ok(Message::Text(text))) => {
                            receive_payload(&state.core, proxy.clone(), &text);
                        }
                        Some(Ok(Message::Binary(bin))) => {
                            if let Ok(text) = String::from_utf8(bin.to_vec()) {
                                receive_payload(&state.core, proxy.clone(), &text);
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            proxy.send_message(Message::Pong(payload));
                        }
                        Some(Ok(Message::Pong(_))) => {}
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
                // 心跳不能在本循环里 await 响应：get_status 最长等 15 秒(ACTION_TIMEOUT_MILLIS)，
                // 期间 select! 整体停摆，收到的消息既不打印也不处理 —— 「控制台卡住」的成因之一。
                // 这里丢给独立任务发送，读循环立刻回来继续收消息。
                _ = heartbeat.tick() => {
                    let proxy = proxy.clone();
                    tokio::spawn(async move {
                        let params = json!({ "status": true, "good": true });
                        let _ = proxy.send(protocol::action("get_status", params)).await;
                    });
                }
                // stop() 唤醒：立刻退出，不再傻等 stream
                _ = state.stop_notify.notified() => break,
                // 写任务判定这条 socket 已废（写出超时）：读循环跟着退出，走下面的正常收尾与重连
                _ = state.stalled.notified() => break,
            }
        }
        writer.abort();
        *state.session_tx.lock().unwrap() = None;
        if state.online.swap(false, Ordering::SeqCst) {
            state.core.business.notify_lifecycle("disable", &state.url);
        }
        if !state.stopped.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(
                state.reconnect_interval_ms,
            ))
            .await;
        }
    }
}
