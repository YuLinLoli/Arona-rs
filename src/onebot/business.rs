//! 业务事件处理器（对应原版 StandaloneBusinessHandler）
use crate::config::onebot::OneBotConfig;
use crate::config::standalone;
use crate::onebot::connection::{ConnectionRegistry, OneBotConnection};
use crate::onebot::console;
use crate::onebot::message_sender::OneBotMessageSender;
use crate::onebot::model::{OneBotAction, OneBotEvent};
use crate::onebot::protocol;
use crate::runtime::dispatcher::{CommandContext, SimpleCommandDispatcher};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// 可变共享状态（群名缓存等）
pub struct HandlerState {
    group_names: Mutex<HashMap<i64, String>>,
    group_name_loading: Mutex<HashSet<i64>>,
    pending_group_messages: Mutex<HashMap<i64, Vec<OneBotEvent>>>,
}

impl HandlerState {
    fn new() -> HandlerState {
        HandlerState {
            group_names: Mutex::new(HashMap::new()),
            group_name_loading: Mutex::new(HashSet::new()),
            pending_group_messages: Mutex::new(HashMap::new()),
        }
    }
}

pub struct StandaloneBusinessHandler {
    config: RwLock<OneBotConfig>,
    pub dispatcher: Arc<SimpleCommandDispatcher>,
    pub registry: Arc<ConnectionRegistry>,
    pub state: Arc<HandlerState>,
}

impl StandaloneBusinessHandler {
    pub fn new(
        config: OneBotConfig,
        dispatcher: Arc<SimpleCommandDispatcher>,
        registry: Arc<ConnectionRegistry>,
    ) -> StandaloneBusinessHandler {
        StandaloneBusinessHandler {
            config: RwLock::new(config),
            dispatcher,
            registry,
            state: Arc::new(HandlerState::new()),
        }
    }

    /// 当前 OneBot 配置快照
    pub fn config(&self) -> OneBotConfig {
        self.config.read().unwrap().clone()
    }

    /// 热重载时更新配置（self_id / nickname 等）
    pub fn update_config(&self, config: OneBotConfig) {
        *self.config.write().unwrap() = config;
    }

    /// 收到事件
    pub fn on_event(&self, event: OneBotEvent, connection: Arc<dyn OneBotConnection>) {
        self.registry.broadcast_except(&event.raw, connection.id());
        if event.post_type == "notice" {
            self.handle_notice(&event);
            return;
        }
        if event.post_type != "message" {
            return;
        }
        let self_id = self.config().self_id;
        if event.message_type.as_deref() == Some("group") && event.group_id.is_some() {
            self.print_group_message(&event, connection.clone());
        } else {
            console::print_message(self_id, &event, None);
        }
        let text = protocol::extract_text(&event).trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(user_id) = event.user_id else { return };
        let is_admin = crate::runtime::config::is_manager(user_id);
        if !is_admin && !self.is_allowed_group(event.group_id) {
            return;
        }
        // 黑名单（全局 + 群内）：管理员不受限制
        if !is_admin && crate::runtime::config::is_blacklisted(user_id, event.group_id) {
            return;
        }
        let sender_name = event
            .sender
            .as_ref()
            .and_then(|v| v.as_object())
            .and_then(|o| o.get("card"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                event
                    .sender
                    .as_ref()
                    .and_then(|v| v.as_object())
                    .and_then(|o| o.get("nickname"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
        let sender = Arc::new(OneBotMessageSender {
            connection: Some(connection),
            self_id: self.config().self_id,
        });
        let context = Arc::new(CommandContext {
            user_id,
            group_id: event.group_id,
            text: text.clone(),
            sender_name,
            is_admin,
            sender,
        });
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            let handled = dispatcher.dispatch(context.clone()).await;
            if !handled {
                // 未匹配到命令时，尝试按 /攻略 模糊建议的数字回复处理
                let reply =
                    crate::standalone::commands::trainer::resolve_numeric_reply(context.clone())
                        .await;
                if let Some(message) = reply {
                    context.reply_message(message).await;
                }
            }
        });
    }

    fn handle_notice(&self, event: &OneBotEvent) {
        let self_id = self.config().self_id;
        let group_id = event.group_id;
        match event.notice_type.as_deref() {
            Some("group_increase") => {
                let text = if event.user_id == Some(self_id) {
                    format!(
                        "机器人已加入群 {}",
                        group_id
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".into())
                    )
                } else {
                    format!(
                        "成员 {} 加入了群 {}",
                        event
                            .user_id
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".into()),
                        group_id
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "?".into())
                    )
                };
                console::print_notice(self_id, &text);
            }
            Some("group_decrease") => {
                if event.user_id == Some(self_id) {
                    if let Some(group_id) = group_id {
                        console::print_notice(
                            self_id,
                            &format!("机器人已被移出群 {group_id}，正在从 groups 配置中移除"),
                        );
                        standalone::remove_group_if_present(group_id);
                    }
                } else {
                    console::print_notice(
                        self_id,
                        &format!(
                            "成员 {} 退出了群 {}",
                            event
                                .user_id
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "?".into()),
                            group_id
                                .map(|v| v.to_string())
                                .unwrap_or_else(|| "?".into())
                        ),
                    );
                }
            }
            _ => {}
        }
    }

    fn print_group_message(&self, event: &OneBotEvent, connection: Arc<dyn OneBotConnection>) {
        let self_id = self.config().self_id;
        let Some(group_id) = event.group_id else {
            return;
        };
        if let Some(name) = self
            .state
            .group_names
            .lock()
            .unwrap()
            .get(&group_id)
            .cloned()
        {
            console::print_message(self_id, event, Some(&name));
            return;
        }
        if let Some(name) = event
            .raw
            .get("group_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            self.state
                .group_names
                .lock()
                .unwrap()
                .insert(group_id, name.to_string());
            console::print_message(self_id, event, Some(name));
            return;
        }
        self.state
            .pending_group_messages
            .lock()
            .unwrap()
            .entry(group_id)
            .or_default()
            .push(event.clone());
        let mut loading = self.state.group_name_loading.lock().unwrap();
        if !loading.contains(&group_id) {
            loading.insert(group_id);
            drop(loading);
            self.spawn_load_group_name(group_id, connection);
        }
    }

    fn spawn_load_group_name(&self, group_id: i64, connection: Arc<dyn OneBotConnection>) {
        let state = self.state.clone();
        let self_id = self.config().self_id;
        tokio::spawn(async move {
            let params = json!({ "group_id": group_id });
            let response = {
                let action = protocol::action("get_group_info", params);
                let wait = connection.send(action);
                match tokio::time::timeout(Duration::from_secs(5), wait).await {
                    Ok(Some(response)) => Some(response),
                    _ => None,
                }
            };
            let name = response
                .and_then(|r| r.data)
                .and_then(|d| d.as_object().cloned())
                .and_then(|o| o.get("group_name").cloned())
                .and_then(|v| v.as_str().map(|s| s.to_string()));
            let name = name.unwrap_or_else(|| group_id.to_string());
            {
                let mut names = state.group_names.lock().unwrap();
                names.insert(group_id, name.clone());
            }
            state.group_name_loading.lock().unwrap().remove(&group_id);
            let pending = state
                .pending_group_messages
                .lock()
                .unwrap()
                .remove(&group_id)
                .unwrap_or_default();
            for event in pending {
                console::print_message(self_id, &event, Some(&name));
            }
        });
    }

    fn is_allowed_group(&self, group_id: Option<i64>) -> bool {
        let groups = crate::runtime::config::groups();
        match group_id {
            None => true,
            Some(group_id) => groups.is_empty() || groups.contains(&group_id),
        }
    }

    /// 处理 OneBot 实现发来的动作请求
    pub async fn handle_action(&self, action: &OneBotAction) -> Result<Option<Value>, String> {
        let data = match action.action.as_str() {
            "get_status" => json!({ "online": true, "good": true }),
            "get_version_info" => {
                json!({ "app_name": "arona", "app_version": "standalone", "protocol_version": "11" })
            }
            "get_login_info" => {
                let config = self.config();
                json!({ "user_id": config.self_id, "nickname": config.nickname })
            }
            _ => return Ok(None),
        };
        Ok(Some(data))
    }
}
