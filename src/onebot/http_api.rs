//! 正向 HTTP API 服务（对应原版 OneBotHttpApiServer）
use crate::config::onebot::ConnectionConfig;
use crate::onebot::business::StandaloneBusinessHandler;
use crate::onebot::connection::ConnectionCore;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

pub struct HttpApiState {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub token: String,
    pub core: Arc<ConnectionCore>,
}

pub struct HttpApiServer {
    pub state: Arc<HttpApiState>,
    pub task: Mutex<Option<JoinHandle<()>>>,
}

impl HttpApiServer {
    pub fn new(endpoint: &ConnectionConfig, core: Arc<ConnectionCore>) -> HttpApiServer {
        HttpApiServer {
            state: Arc::new(HttpApiState {
                host: endpoint.host.clone(),
                port: endpoint.port,
                path: if endpoint.path.is_empty() {
                    "/".to_string()
                } else {
                    endpoint.path.clone()
                },
                token: endpoint.token.clone(),
                core,
            }),
            task: Mutex::new(None),
        }
    }

    pub fn start(&self) {
        let state = self.state.clone();
        let mut task = self.task.lock().unwrap();
        *task = Some(tokio::spawn(async move {
            run_http_server(state).await;
        }));
    }

    pub fn stop(&self) {
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

async fn api_handler(
    headers: HeaderMap,
    State(state): State<Arc<HttpApiState>>,
    body: Bytes,
) -> Response {
    if !authorized(&headers, &state.token) {
        return (
            StatusCode::UNAUTHORIZED,
            "{\"status\":\"failed\",\"retcode\":1403,\"data\":null,\"message\":\"unauthorized\"}",
        )
            .into_response();
    }
    let text = String::from_utf8_lossy(&body).to_string();
    let json: Option<Value> = if text.trim().is_empty() {
        Some(json!({}))
    } else {
        serde_json::from_str(&text).ok()
    };
    let Some(json) = json else {
        return (
            StatusCode::BAD_REQUEST,
            "{\"status\":\"failed\",\"retcode\":10001,\"data\":null,\"message\":\"invalid json\"}",
        )
            .into_response();
    };
    let action_name = json
        .get("action")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "get_status".to_string());
    let params = json.get("params").cloned().unwrap_or_else(|| json!({}));
    let echo = json
        .get("echo")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let action = crate::onebot::model::OneBotAction {
        action: action_name,
        params,
        echo,
    };
    let data = match tokio::time::timeout(
        Duration::from_secs(30),
        state.core.business.handle_action(&action),
    )
    .await
    {
        Ok(Ok(Some(data))) => data,
        Ok(Ok(None)) => Value::Null,
        Ok(Err(_)) => Value::Null,
        Err(_) => Value::Null,
    };
    let response = json!({
        "status": "ok",
        "retcode": 0,
        "data": data,
        "echo": action.echo,
    });
    (StatusCode::OK, response.to_string()).into_response()
}

async fn run_http_server(state: Arc<HttpApiState>) {
    let address = format!("{}:{}", state.host, state.port);
    let path = state.path.clone();
    let app = Router::new()
        .route(&path, post(api_handler))
        .with_state(state.clone());
    let listener = loop {
        match tokio::net::TcpListener::bind(&address).await {
            Ok(listener) => break listener,
            Err(err) => {
                crate::runtime::log::warning(format!("[OneBot HTTP] 启动失败: {err}，5 秒后重试"));
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };
    crate::runtime::log::info(format!("[OneBot HTTP] 已启动: {address}{path}"));
    if let Err(err) = axum::serve(listener, app).await {
        crate::runtime::log::warning(format!("[OneBot HTTP] 服务退出: {err}"));
    }
}
