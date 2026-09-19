//! 反向 HTTP 连接（对应原版 OneBotHttpReverseConnection）
use crate::config::onebot::ConnectionConfig;
use crate::onebot::connection::{ConnectionCore, OneBotConnection, next_conn_id};
use crate::onebot::model::{OneBotAction, OneBotActionResponse, ParsedPayload};
use crate::onebot::protocol;
use crate::runtime::log;
use crate::runtime::message::BoxFuture;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

pub struct HttpReverseState {
    pub id: u64,
    pub url: String,
    pub token: String,
    pub heartbeat_interval_ms: u64,
    pub core: Arc<ConnectionCore>,
    pub stopped: AtomicBool,
}

impl HttpReverseState {
    pub fn new(endpoint: &ConnectionConfig, core: Arc<ConnectionCore>) -> Arc<HttpReverseState> {
        Arc::new(HttpReverseState {
            id: next_conn_id(),
            url: endpoint.url.clone(),
            token: endpoint.token.clone(),
            heartbeat_interval_ms: endpoint.heartbeat_interval,
            core,
            stopped: AtomicBool::new(false),
        })
    }
}

pub struct HttpReverseConnection {
    pub state: Arc<HttpReverseState>,
    pub task: Mutex<Option<JoinHandle<()>>>,
}

pub struct HttpReverseProxy {
    pub state: Arc<HttpReverseState>,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn post(state: &Arc<HttpReverseState>, payload: &str) -> Option<OneBotActionResponse> {
    let mut request = client()
        .post(&state.url)
        .header("Content-Type", "application/json; charset=utf-8");
    if !state.token.is_empty() {
        request = request.header("Authorization", format!("Bearer {}", state.token));
    }
    let response = match request.body(payload.to_string()).send().await {
        Ok(response) => response,
        Err(err) => {
            log::warning(format!("[OneBot reverse HTTP] {err}"));
            return None;
        }
    };
    let body = response.text().await.unwrap_or_default();
    match protocol::parse_payload(&body) {
        Some(ParsedPayload::Response(response)) => Some(response),
        _ => None,
    }
}

impl OneBotConnection for HttpReverseProxy {
    fn id(&self) -> u64 {
        self.state.id
    }

    fn send<'a>(&'a self, action: OneBotAction) -> BoxFuture<'a, Option<OneBotActionResponse>> {
        let state = self.state.clone();
        Box::pin(async move { post(&state, &protocol::serialize_action(&action)).await })
    }

    fn send_raw(&self, payload: &str) {
        let state = self.state.clone();
        let payload = payload.to_string();
        tokio::spawn(async move {
            let _ = post(&state, &payload).await;
        });
    }

    fn start(&self) {}

    fn stop(&self) {
        self.state.stopped.store(true, Ordering::SeqCst);
    }
}

impl OneBotConnection for HttpReverseConnection {
    fn id(&self) -> u64 {
        self.state.id
    }

    fn send<'a>(&'a self, action: OneBotAction) -> BoxFuture<'a, Option<OneBotActionResponse>> {
        let proxy = HttpReverseProxy {
            state: self.state.clone(),
        };
        Box::pin(async move { proxy.send(action).await })
    }

    fn send_raw(&self, payload: &str) {
        let proxy = HttpReverseProxy {
            state: self.state.clone(),
        };
        proxy.send_raw(payload);
    }

    fn start(&self) {
        log::info(format!("[OneBot reverse HTTP] 目标: {}", self.state.url));
        let state = self.state.clone();
        let mut task = self.task.lock().unwrap();
        *task = Some(tokio::spawn(async move {
            // 心跳：HTTP 反向同样周期性发送 get_status
            let heartbeat_ms = state.heartbeat_interval_ms.max(1000);
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(heartbeat_ms));
            interval.tick().await;
            while !state.stopped.load(Ordering::SeqCst) {
                interval.tick().await;
                let params = json!({ "status": true, "good": true });
                let action = crate::onebot::model::OneBotAction {
                    action: "get_status".to_string(),
                    params,
                    echo: protocol::new_echo(),
                };
                let proxy = HttpReverseProxy {
                    state: state.clone(),
                };
                let _ = proxy.send(action).await;
            }
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
