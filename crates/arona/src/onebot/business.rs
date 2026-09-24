//! 业务事件处理器（对应原版 StandaloneBusinessHandler）
use crate::config::onebot::OneBotConfig;
use crate::config::settings;
use crate::framework::Framework;
use crate::onebot::connection::{ConnectionRegistry, OneBotConnection};
use crate::onebot::console;
use crate::onebot::hooks::NoticeKind;
use crate::onebot::message_sender::OneBotMessageSender;
use crate::onebot::model::{OneBotAction, OneBotEvent};
use crate::onebot::protocol;
use crate::runtime::dispatcher::{CommandContext, CommandDispatcher};
use crate::runtime::peers::PeerCache;
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

pub struct BusinessHandler {
    config: RwLock<OneBotConfig>,
    /// 本处理器所属的框架实例：事件钩子与门控都读它，不绕回进程默认实例
    framework: Arc<Framework>,
    /// 无状态分发句柄：真正的命令表按归属登记在所属实例的 `CommandRegistry` 上
    pub dispatcher: Arc<CommandDispatcher>,
    pub registry: Arc<ConnectionRegistry>,
    pub state: Arc<HandlerState>,
}

impl BusinessHandler {
    pub fn new(
        config: OneBotConfig,
        framework: Arc<Framework>,
        dispatcher: Arc<CommandDispatcher>,
        registry: Arc<ConnectionRegistry>,
    ) -> BusinessHandler {
        BusinessHandler {
            config: RwLock::new(config),
            framework,
            dispatcher,
            registry,
            state: Arc::new(HandlerState::new()),
        }
    }

    /// 当前 OneBot 配置快照
    pub fn config(&self) -> OneBotConfig {
        self.config.read().unwrap().clone()
    }

    /// 只取机器人号：每条事件都要用它，不该为此克隆整份配置
    fn self_id(&self) -> i64 {
        self.config.read().unwrap().self_id
    }

    /// 热重载时更新配置（self_id / nickname 等），并同步门控里的机器人号
    pub fn update_config(&self, config: OneBotConfig) {
        // 机器人号不同步的话，热重载换了号，"@机器人 才剥前缀"和身份判断都还在用旧号
        self.framework.gating().set_bot_id(config.self_id);
        *self.config.write().unwrap() = config;
    }

    /// 传输层连上/断开时合成的 lifecycle 元事件：**只投插件钩子，不回播给其它实现端**。
    ///
    /// 框架可以在一条连接上同时挂多个实现端，`on_event` 开头那道 `broadcast_except` 是给
    /// 它们同步真实事件用的；把框架自己造的事件塞进去等于对着别家的客户端撒谎。
    /// `endpoint` 只作展示（日志与插件自查），不参与匹配。
    pub fn notify_lifecycle(&self, state: &str, endpoint: &str) {
        let self_id = self.self_id();
        let time = chrono::Local::now().timestamp();
        let mut raw = serde_json::Map::new();
        raw.insert("time".into(), json!(time));
        raw.insert("self_id".into(), json!(self_id));
        raw.insert("post_type".into(), json!("meta_event"));
        raw.insert("meta_event_type".into(), json!("lifecycle"));
        // v11 把状态写在 sub_type，部分实现端另写一份 state：插件按哪个读都能命中
        raw.insert("sub_type".into(), json!(state));
        raw.insert("state".into(), json!(state));
        raw.insert("url".into(), json!(endpoint));
        let event = OneBotEvent {
            time,
            self_id,
            post_type: "meta_event".into(),
            notice_type: None,
            message_type: None,
            sub_type: Some(state.into()),
            message_id: None,
            user_id: None,
            operator_id: None,
            group_id: None,
            raw_message: None,
            message: None,
            sender: None,
            raw: Value::Object(raw),
        };
        let hooks = self.framework.hooks().clone();
        tokio::spawn(async move {
            hooks.dispatch(&event).await;
        });
    }

    /// 收到事件
    pub fn on_event(&self, event: OneBotEvent, connection: Arc<dyn OneBotConnection>) {
        self.registry.broadcast_except(&event.raw, connection.id());
        if event.post_type != "message" {
            // 通知/请求/元事件无条件投给插件钩子：黑名单用户退群、加群申请这类事同样要能响应。
            // 框架自己的通知处理（机器人被踢时清理 groups 配置）排在钩子**之后**：
            // 插件的 Monitor 档钩子应先看到原样的事件，日志顺序也才和实际处理顺序一致。
            let hooks = self.framework.hooks().clone();
            let peers = self.framework.peers().clone();
            let self_id = self.self_id();
            tokio::spawn(async move {
                hooks.dispatch(&event).await;
                if event.post_type == "notice" {
                    Self::handle_notice(self_id, &peers, &event);
                }
            });
            return;
        }
        let self_id = self.self_id();
        // 消息段只解一次：打印用文本、命令匹配用命令文本、钩子与命令上下文用同一份段，
        // 引用编号也是从这份段里取的（段解析要拷出每段的 data 映射，重复解是白付的钱）
        let parsed = protocol::ParsedMessage::parse(&event, self_id);
        if event.message_type.as_deref() == Some("group") && event.group_id.is_some() {
            self.print_group_message(&event, &parsed.text, connection.clone());
        } else {
            console::print_message_text(self_id, &event, None, &parsed.text);
        }
        // 命令匹配读的是剥掉"@机器人 前缀"的文本：群消息里 @Arona /单抽 的首词才是命令名
        let text = parsed.command_text.clone();
        // 引用段被 command_text 剥掉了，插件要靠这个值才知道"用户引用了哪条消息"
        let quoted = parsed.quoted();
        // 纯图片/表情这类没有文本的消息，没有可分发的命令，但事件钩子照样要收到
        let has_command = !text.is_empty();
        let Some(user_id) = event.user_id else { return };
        let gating = self.framework.gating();
        let is_admin = gating.is_manager(user_id);
        // 黑名单（全局 + 群内）：管理员不受限制
        if !is_admin
            && (!gating.group_authorized(event.group_id)
                || gating.is_blacklisted(user_id, event.group_id))
        {
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
            self_id,
        });
        // 群身份直接取事件自带的 sender.role：命令的权限门控靠它，不必再回查一次实现端
        let sender_role = event
            .sender
            .as_ref()
            .and_then(crate::runtime::dispatcher::GroupRole::from_sender);
        // 进出都要留档（出站那半在 onebot::message_sender）：NTQQ 系实现端十几二十分钟前的
        // message_id 协议层就引用不到了，引用回复与撤回只能靠本地这份库
        self.framework
            .chatlog_api()
            .record_inbound(&event, &parsed.segments);
        let context = Arc::new(CommandContext {
            user_id,
            group_id: event.group_id,
            text,
            sender_name,
            is_admin,
            sender_role,
            message_id: event.message_id,
            time: event.time,
            quoted,
            segments: parsed.segments.clone(),
            sender,
            framework: self.framework.clone(),
        });
        let dispatcher = self.dispatcher.clone();
        let hooks = self.framework.hooks().clone();
        tokio::spawn(async move {
            // 消息钩子先于命令分发：插件有机会整条接管（返回 Handled 时不再走命令）。
            // 未命中任何命令时的兜底（如 /攻略 模糊建议的数字回复）由各插件自己登记的
            // FallbackHandler 按优先级依次尝试，框架这里不感知具体功能。
            if hooks.dispatch_parsed(&event, &parsed).await {
                return;
            }
            if !has_command {
                return;
            }
            dispatcher.dispatch(context.clone()).await;
        });
    }

    /// 框架自己对通知事件的善后：打日志 + 机器人被移出群时把该群从 groups 配置里摘掉
    /// + 让实例上的成员资料缓存跟着作废。只在插件钩子跑完之后调用（见 `on_event`）。
    fn handle_notice(self_id: i64, peers: &PeerCache, event: &OneBotEvent) {
        let group_id = event.group_id;
        Self::expire_member_cache(peers, event);
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
                        settings::remove_group_if_present(group_id);
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

    /// 成员资料变了就把缓存里那一条作废，否则"刚升上去的管理员发命令仍被判成普通成员"
    /// 会一直持续到 60 秒到期。归位口径与插件钩子看到的完全一样（[`NoticeKind`]），
    /// 所以 NapCat 那批写成 `notice_type="notify"` + 内层动作的名片变更也跑得掉。
    fn expire_member_cache(peers: &PeerCache, event: &OneBotEvent) {
        let action = ["sub_type", "type"]
            .iter()
            .find_map(|key| event.raw.get(*key).and_then(|v| v.as_str()))
            .unwrap_or_default();
        let kind =
            NoticeKind::parse_with_action(event.notice_type.as_deref().unwrap_or_default(), action);
        let member_scoped = matches!(
            kind,
            NoticeKind::GroupIncrease
                | NoticeKind::GroupDecrease
                | NoticeKind::GroupAdmin
                | NoticeKind::GroupBan
                | NoticeKind::GroupCardUpdate
                | NoticeKind::GroupTitleUpdate
        );
        if !member_scoped {
            return;
        }
        match (event.group_id, event.user_id) {
            (Some(group_id), Some(user_id)) => peers.forget_member(group_id, user_id),
            // 实现端没带当事人 qq 号的那条通知（少数写法）只能整群作废：
            // 少缓存几条只是多问一次，留着旧的则是判错身份
            (Some(group_id), None) => peers.forget_group(group_id),
            (None, Some(user_id)) => peers.forget_everywhere(user_id),
            (None, None) => {}
        }
    }

    fn print_group_message(
        &self,
        event: &OneBotEvent,
        text: &str,
        connection: Arc<dyn OneBotConnection>,
    ) {
        let self_id = self.self_id();
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
            console::print_message_text(self_id, event, Some(&name), text);
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
            console::print_message_text(self_id, event, Some(name), text);
            return;
        }
        // 群名还没拿到：立刻按群号打印，绝不把「收到消息」这件事拖到一次网络请求之后。
        // （原版在这里把消息排队等 get_group_info 返回：OneBot 端慢、没实现 get_group_info
        //   或网络抖动时，控制台会先卡住最多 5 秒，收到的指令要等命令都执行完才显示出来）
        console::print_message_text(self_id, event, None, text);
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

    /// 处理 OneBot 实现发来的动作请求
    pub async fn handle_action(&self, action: &OneBotAction) -> Result<Option<Value>, String> {
        let data = match action.action.as_str() {
            "get_status" => json!({ "online": true, "good": true }),
            "get_version_info" => {
                json!({ "app_name": "arona", "app_version": env!("CARGO_PKG_VERSION"), "protocol_version": "11" })
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
        let handler = BusinessHandler::new(
            OneBotConfig::default(),
            // 分发句柄是无状态的，命令由插件按归属登记在命令表里；
            // 框架测试只验证「收到消息先打印」的时序，不需要任何命令。
            crate::framework::Framework::global_arc(),
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

    /// 造一条通知事件。`user_id` 与 `self_id` 刻意不相等：走到"机器人自己被踢"那条分支
    /// 会去改磁盘上的 arona.yml，单元测试不该碰它。
    fn notice(notice_type: &str, group_id: i64, user_id: i64, action: Option<&str>) -> OneBotEvent {
        let mut raw = serde_json::Map::new();
        raw.insert("group_id".into(), json!(group_id));
        raw.insert("user_id".into(), json!(user_id));
        if let Some(action) = action {
            raw.insert("sub_type".into(), json!(action));
        }
        OneBotEvent {
            time: 0,
            self_id: 10001,
            post_type: "notice".into(),
            notice_type: Some(notice_type.into()),
            message_type: None,
            sub_type: None,
            message_id: None,
            user_id: Some(user_id),
            operator_id: None,
            group_id: Some(group_id),
            raw_message: None,
            message: None,
            sender: None,
            raw: Value::Object(raw),
        }
    }

    fn cached_member(cache: &PeerCache, group_id: i64, user_id: i64) {
        cache.members().put(
            (group_id, user_id),
            crate::onebot::api::GroupMemberInfo {
                group_id,
                user_id,
                card: "老师".to_string(),
                ..Default::default()
            },
        );
    }

    /// 成员资料缓存必须跟着通知事件作废：不然"刚升上去的管理员发命令仍被判成普通成员"
    /// 会一直持续到 60 秒到期。这里钉的是作废的口径，不是缓存本身（那归 `runtime::cache`）。
    #[test]
    fn member_notices_expire_the_cached_profile() {
        let peers = PeerCache::new();
        for notice_type in [
            "group_increase",
            "group_decrease",
            "group_admin",
            "group_ban",
            "group_card",
        ] {
            cached_member(&peers, 20001, 100);
            BusinessHandler::handle_notice(10001, &peers, &notice(notice_type, 20001, 100, None));
            assert!(
                peers.members().get(&(20001, 100)).is_none(),
                "{notice_type} 之后这个人该重新问过"
            );
        }

        // 与成员资料无关的通知不该顺手清掉，否则缓存等于没有
        cached_member(&peers, 20001, 100);
        BusinessHandler::handle_notice(10001, &peers, &notice("group_recall", 20001, 100, None));
        assert!(
            peers.members().get(&(20001, 100)).is_some(),
            "群内撤回不该作废成员资料"
        );

        // NapCat / LLOneBot 把名片变更写成 notice_type="notify" + 内层动作
        cached_member(&peers, 20001, 100);
        BusinessHandler::handle_notice(10001, &peers, &notice("notify", 20001, 100, Some("card")));
        assert!(
            peers.members().get(&(20001, 100)).is_none(),
            "notify+card 也该作废，否则内层写法的名片变更缓存就旧下去了"
        );

        // 同一群里的另一个人不该被牵连
        cached_member(&peers, 20001, 101);
        cached_member(&peers, 20001, 100);
        BusinessHandler::handle_notice(10001, &peers, &notice("group_ban", 20001, 100, None));
        assert!(peers.members().get(&(20001, 101)).is_some());
    }

    /// 只把 payload 收下来的连接：用来钉「合成事件没有回播给实现端」
    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<String>>,
    }

    impl OneBotConnection for Recorder {
        fn id(&self) -> u64 {
            2
        }

        fn send<'a>(
            &'a self,
            _action: OneBotAction,
        ) -> BoxFuture<'a, Option<OneBotActionResponse>> {
            Box::pin(std::future::pending::<Option<OneBotActionResponse>>())
        }

        fn send_raw(&self, payload: &str) {
            self.sent.lock().unwrap().push(payload.to_string());
        }

        fn start(&self) {}
        fn stop(&self) {}
    }

    /// 连接状态翻转要变成插件能订阅的一条 lifecycle 元事件，而且只投钩子：
    /// 把自己造的事件回播给实现端，等于对着别家客户端撒谎。
    #[tokio::test]
    async fn lifecycle_flips_reach_hooks_without_reaching_the_implementation() {
        use crate::framework::Framework;
        use crate::onebot::hooks::{
            BodyFilter, EventContext, HookFlow, ListenerPriority, MetaKind, event_handler,
        };

        let framework = Framework::new();
        let registry = Arc::new(ConnectionRegistry::new());
        let recorder = Arc::new(Recorder::default());
        registry.register(recorder.clone());
        let handler = BusinessHandler::new(
            OneBotConfig::default(),
            framework.clone(),
            Arc::new(crate::runtime::dispatcher::CommandDispatcher::new()),
            registry,
        );

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        for state in ["connect", "disable"] {
            let sink = seen.clone();
            framework.hooks().subscribe_where(
                "LifecycleProbePlugin",
                &[BodyFilter::meta(MetaKind::Lifecycle, state)],
                ListenerPriority::default(),
                event_handler(move |ctx: Arc<EventContext>| {
                    let sink = sink.clone();
                    Box::pin(async move {
                        assert_eq!(ctx.body.display_name(), format!("元事件:生命周期({state})"));
                        sink.lock().unwrap().push(
                            ctx.event.raw["url"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        );
                        HookFlow::Pass
                    })
                }),
            );
        }

        handler.notify_lifecycle("connect", "ws://127.0.0.1:9001");
        handler.notify_lifecycle("disable", "ws://127.0.0.1:9001");
        // 合成事件是 spawn 出去的，等两条钩子都跑到
        for _ in 0..200 {
            if seen.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec!["ws://127.0.0.1:9001".to_string(); 2],
            "两种状态各命中自己那一档，并带上来源标识"
        );
        assert!(
            recorder.sent.lock().unwrap().is_empty(),
            "框架合成的事件不该发给实现端"
        );
    }
}
