//! 业务事件处理器（对应原版 StandaloneBusinessHandler）
use crate::config::onebot::OneBotConfig;
use crate::config::standalone;
use crate::onebot::connection::{ConnectionRegistry, OneBotConnection};
use crate::onebot::console;
use crate::onebot::message_sender::OneBotMessageSender;
use crate::onebot::model::{OneBotAction, OneBotEvent};
use crate::onebot::protocol;
use crate::runtime::dispatcher::{CommandContext, CommandDispatcher};
use crate::runtime::message::MessageSegment;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// 可变共享状态（群名缓存等）
pub struct HandlerState {
    group_names: Mutex<HashMap<i64, String>>,
    group_name_loading: Mutex<HashSet<i64>>,
}

impl HandlerState {
    fn new() -> HandlerState {
        HandlerState {
            group_names: Mutex::new(HashMap::new()),
            group_name_loading: Mutex::new(HashSet::new()),
        }
    }
}

pub struct StandaloneBusinessHandler {
    config: RwLock<OneBotConfig>,
    /// 无状态分发句柄：真正的命令表是进程级的，由各插件在 configure 阶段按归属登记
    pub dispatcher: Arc<CommandDispatcher>,
    pub registry: Arc<ConnectionRegistry>,
    pub state: Arc<HandlerState>,
}

impl StandaloneBusinessHandler {
    pub fn new(
        config: OneBotConfig,
        dispatcher: Arc<CommandDispatcher>,
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
        if event.post_type != "message" {
            // 框架自己的通知处理（机器人被踢时清理 groups 配置）
            if event.post_type == "notice" {
                self.handle_notice(&event);
            }
            // 通知/请求/元事件无条件投给插件钩子：黑名单用户退群、加群申请这类事同样要能响应
            tokio::spawn(async move {
                crate::onebot::hooks::dispatch(&event).await;
            });
            return;
        }
        let self_id = self.config().self_id;
        if event.message_type.as_deref() == Some("group") && event.group_id.is_some() {
            self.print_group_message(&event, connection.clone());
        } else {
            console::print_message(self_id, &event, None);
        }
        // 命令匹配读的是剥掉"@机器人 前缀"的文本：群消息里 @Arona /单抽 的首词才是命令名
        let text = protocol::command_text(&event, self_id);
        let segments = protocol::extract_segments(&event);
        // 引用段被 command_text 剥掉了，插件要靠这个值才知道"用户引用了哪条消息"
        let quoted = segments.iter().find_map(|segment| match segment {
            MessageSegment::Reply(message_id) => Some(*message_id),
            _ => None,
        });
        // 纯图片/表情这类没有文本的消息，没有可分发的命令，但事件钩子照样要收到
        let has_command = !text.is_empty();
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
        // 群身份直接取事件自带的 sender.role：命令的权限门控靠它，不必再回查一次实现端
        let sender_role = event
            .sender
            .as_ref()
            .and_then(crate::runtime::dispatcher::GroupRole::from_sender);
        let context = Arc::new(CommandContext {
            user_id,
            group_id: event.group_id,
            text: text.clone(),
            sender_name,
            is_admin,
            sender_role,
            message_id: event.message_id,
            time: event.time,
            quoted,
            segments,
            sender,
        });
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            // 消息钩子先于命令分发：插件有机会整条接管（返回 Handled 时不再走命令）。
            // 未命中任何命令时的兜底（如 /攻略 模糊建议的数字回复）由各插件自己登记的
            // FallbackHandler 按优先级依次尝试，框架这里不感知具体功能。
            if crate::onebot::hooks::dispatch(&event).await {
                return;
            }
            if !has_command {
                return;
            }
            dispatcher.dispatch(context.clone()).await;
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
        // 群名还没拿到：立刻按群号打印，绝不把「收到消息」这件事拖到一次网络请求之后。
        // （原版在这里把消息排队等 get_group_info 返回：OneBot 端慢、没实现 get_group_info
        //   或网络抖动时，控制台会先卡住最多 5 秒，收到的指令要等命令都执行完才显示出来）
        console::print_message(self_id, event, None);
        // 后台补一次群名，只影响后续消息的显示，不阻塞任何打印
        let mut loading = self.state.group_name_loading.lock().unwrap();
        if !loading.contains(&group_id) {
            loading.insert(group_id);
            drop(loading);
            self.spawn_load_group_name(group_id, connection);
        }
    }

    /// 后台获取群名并写入缓存（只用于后续消息的显示，绝不影响当前这行日志的实时性）
    fn spawn_load_group_name(&self, group_id: i64, connection: Arc<dyn OneBotConnection>) {
        let state = self.state.clone();
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
                names.insert(group_id, name);
            }
            state.group_name_loading.lock().unwrap().remove(&group_id);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onebot::model::OneBotActionResponse;
    use crate::runtime::message::BoxFuture;

    /// 永不响应的连接：模拟 OneBot 端很慢 / 没实现 get_group_info，
    /// 任何动作都只会挂在那里，不会返回。
    struct NeverResponding;

    impl OneBotConnection for NeverResponding {
        fn id(&self) -> u64 {
            1
        }

        fn send<'a>(
            &'a self,
            _action: OneBotAction,
        ) -> BoxFuture<'a, Option<OneBotActionResponse>> {
            Box::pin(std::future::pending::<Option<OneBotActionResponse>>())
        }

        fn send_raw(&self, _payload: &str) {}
        fn start(&self) {}
        fn stop(&self) {}
    }

    fn group_event(self_id: i64, group_id: i64, user_id: i64, text: &str) -> OneBotEvent {
        OneBotEvent {
            time: 0,
            self_id,
            post_type: "message".into(),
            notice_type: None,
            message_type: Some("group".into()),
            sub_type: None,
            message_id: Some(1),
            user_id: Some(user_id),
            operator_id: None,
            group_id: Some(group_id),
            raw_message: Some(text.into()),
            message: Some(json!([{ "type": "text", "data": { "text": text } }])),
            sender: Some(json!({ "nickname": "测试用户" })),
            raw: json!({}),
        }
    }

    /// 回归：群名还没缓存时，「收到消息」也必须在本函数返回前就打印出来，
    /// 不能排队等 get_group_info（原来会等最多 5 秒：控制台先卡住，
    /// 而并行的命令几秒就出结果，于是出现「结果先出、收到指令后到」）。
    #[tokio::test]
    async fn group_message_is_printed_before_group_name_is_resolved() {
        // 断言读的是全局实时日志缓冲：持锁串行，避免 log.rs 里 clear_live()/推满缓冲
        // 的测试在本用例读之前把标记行抹掉（并行执行时的随机失败）。
        let _serial = crate::runtime::log::LIVE_LOG_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let self_id = 1493074321_i64;
        let group_id = 987_654_321_i64;
        let user_id = 20_001_i64;
        let marker = "打印时序回归标记-0x5f3759df";
        let registry = Arc::new(ConnectionRegistry::new());
        let handler = StandaloneBusinessHandler::new(
            OneBotConfig::default(),
            // 分发句柄是无状态的，命令由插件按归属登记在全局表里；
            // 框架测试只验证「收到消息先打印」的时序，不需要任何命令。
            Arc::new(crate::runtime::dispatcher::CommandDispatcher::new()),
            registry,
        );

        handler.on_event(
            group_event(self_id, group_id, user_id, marker),
            Arc::new(NeverResponding),
        );

        let lines = crate::runtime::log::live_lines();
        let hit = lines
            .iter()
            .any(|line| line.text.contains(marker) && line.text.contains(&group_id.to_string()));
        assert!(
            hit,
            "群名未解析时也必须立刻打印收到的消息；实际日志尾部: {:?}",
            lines
                .iter()
                .rev()
                .take(5)
                .map(|line| line.text.clone())
                .collect::<Vec<_>>()
        );
    }
}
