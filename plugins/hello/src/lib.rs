//! Arona 插件开发示例（配套文档：仓库根 `PLUGIN_DEVELOPMENT.md`，见 §15「新增一个插件」）
//!
//! 这个 crate 刻意**不进 `plugins.toml`、也不被 host 依赖**：它只保证 `cargo check -p hello-plugin`
//! 过得去，装着它机器人不会长出别的功能。要真跑起来，把 `hello_plugin::HelloPlugin` 加进
//! `plugins.toml` 的清单，并在 `crates/arona-host/Cargo.toml` 里加一条同名 feature。
//!
//! 一个插件会用到的接口都在这里各演示一遍：
//! - `install`：登记一个分群功能开关 + 一块强类型配置（落在 `config/hello/arona.yml`）
//! - `configure`：四条命令（参数类型化、身份门控、自己回话与交给框架回话两种写法）、
//!   一个进群欢迎钩子、一个出站改写钩子
//! - `start`：一个每日定时任务
//! - `on_config_reload`：配置改完重排任务
//! - `stop`：什么都不做——回收本来就是框架的事

mod config;

use std::sync::Arc;

use arona::onebot::hooks::{BodyFilter, HookFlow, NoticeKind, event_handler, outbound_handler};
use arona::plugin::{AronaPlugin, PluginContext, PluginMeta, PluginRegistrar};
use arona::runtime::args::arg;
use arona::runtime::config::Feature;
use arona::runtime::dispatcher::{CommandRegistration, Permission, handler, typed_handler};
use arona::runtime::log;
use arona::runtime::message::{MessageSegment, MessageTarget, OutgoingMessage};
use arona::runtime::priority::ListenerPriority;
use arona::runtime::services::send_message;

use crate::config::{HelloConfig, SECTION};

/// 功能开关的 key：命令的 `with_feature` 与钩子的 `listen_feature` 都用它，
/// 群管理页里关掉「示例」这一项时，下面的命令与钩子会一起停止投递。
const FEATURE: &str = "hello";

/// 每日问好的任务名（重复登记时框架按这个名字替换）
const GREET_JOB: &str = "HelloDailyGreeting";

pub struct HelloPlugin;

impl HelloPlugin {
    pub fn new() -> Self {
        HelloPlugin
    }

    /// 排一次每日问好。`quartz` 里同名任务会被直接替换，所以配置每热重载一次就照新值重排，
    /// 插件不必自己记"上次是几点"来判断要不要重建。
    fn schedule_greeting(ctx: &PluginContext) {
        let hour = ctx
            .config::<HelloConfig>(SECTION)
            .get()
            .greet_hour
            .clamp(0, 23) as u32;
        // JobFn 是同步闭包：真要发消息得在这里把异步工作丢进本插件的任务作用域，
        // 用 ctx.spawn_as 而不是 tokio::spawn，否则插件被停用时任务收不回去。
        let scheduled = ctx.clone();
        ctx.daily_job(
            hour,
            GREET_JOB,
            Arc::new(move || {
                // spawn_as 要借用 context，async 块又要把整份 ctx 搬进去：一份发起、一份搬走
                let context = scheduled.clone();
                scheduled.spawn_as("每日问好", async move {
                    // 群名单每次触发时重新读：中途改配置不必重排任务
                    let groups = context.config::<HelloConfig>(SECTION).get().greet_groups;
                    for group_id in groups {
                        let message = OutgoingMessage::text("老师早上好（示例插件的每日问好）");
                        let _ = send_message(MessageTarget::Group(group_id), message).await;
                    }
                });
            }),
        );
        log::info(format!("每日问好已排定: 每天 {hour} 点"));
    }
}

impl Default for HelloPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl AronaPlugin for HelloPlugin {
    /// 只有 id、name、version 是规范强制的；id 决定目录与配置键，定了就别改
    fn meta(&self) -> PluginMeta {
        PluginMeta::new(
            "hello",
            "HelloPlugin",
            env!("CARGO_PKG_VERSION"),
            "插件开发示例：命令/事件/配置/定时任务各来一发",
        )
        .with_author("Arona-rs")
    }

    /// 登记阶段：此刻框架配置还没加载，只能往外登记东西
    fn install(&self, reg: &PluginRegistrar) -> Result<(), String> {
        reg.feature(Feature {
            key: FEATURE,
            name: "示例",
            description: "打招呼/引用回显/出站前缀/每日问好",
        });
        // 必须在这里登记：框架加载配置时按登记表生成 config/hello/arona.yml 的带注释模板
        reg.config::<HelloConfig>(SECTION);
        Ok(())
    }

    /// 装配阶段：两份配置已就绪、OneBot 还没连上，命令与钩子都在这一步交出去
    fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
        let prefix_config = ctx.config::<HelloConfig>(SECTION);

        ctx.commands(vec![
            // 写法一：声明参数，回话内容 `Some(..)` 交给框架发（群聊回群、私聊回人）
            CommandRegistration::typed(
                vec!["/你好".into(), "/hello".into()],
                "打个招呼，可选要问候的对象",
                typed_handler(|context, arguments| async move {
                    let who = arguments.text("对象").unwrap_or("老师");
                    let speaker = context.sender_name.as_deref().unwrap_or("陌生老师");
                    Some(OutgoingMessage::text(format!(
                        "你好呀，{who}！说话的人是 {speaker}。"
                    )))
                }),
            )
            .with_args(vec![arg::text("对象").optional().with_default("老师")])
            .with_feature(FEATURE),
            // 写法二：自己发回去就返回 `None`，框架不会再补一条，不会重复发
            CommandRegistration::new(
                vec!["/引用".into()],
                "回显这条消息引用了哪一条",
                handler(|context, _| async move {
                    match context.quoted {
                        // 引用回复：协议层还引用得到就挂原生引用，来不及了由框架的
                        // 聊天记录缓存（runtime::chatlog）把那条还原成文字+图。
                        Some(quoted) => {
                            let text = format!("你引用的是消息 {quoted}，示例插件收到这份引用了。");
                            let _ = context.reply_with_quote(OutgoingMessage::text(text)).await;
                        }
                        None => {
                            let _ = context.reply("这条消息没有引用任何内容。").await;
                        }
                    }
                    None
                }),
            )
            .with_feature(FEATURE),
            // 要改配置的命令：声明身份门控，判定顺序（管理员名单 → sender.role → 回查）全在框架里
            CommandRegistration::typed(
                vec!["/示例前缀".into()],
                "给机器人每条出站消息加前缀，留空即取消",
                typed_handler({
                    let config = prefix_config.clone();
                    move |_context, arguments| {
                        let config = config.clone();
                        async move {
                            let prefix = arguments.text("文字").unwrap_or("").trim().to_string();
                            let result =
                                config.update(|stored| stored.outbound_prefix = prefix.clone());
                            // 这条回话本身也要发出去，于是正好被下面的出站钩子加上刚设的前缀
                            Some(match result {
                                Ok(()) if prefix.is_empty() => {
                                    OutgoingMessage::text("已取消出站前缀。")
                                }
                                Ok(()) => OutgoingMessage::text(format!(
                                    "出站前缀已设为「{prefix}」，之后的每条消息都会带上。"
                                )),
                                Err(reason) => {
                                    OutgoingMessage::text(format!("写配置失败：{reason}"))
                                }
                            })
                        }
                    }
                }),
            )
            .with_args(vec![arg::rest("文字").optional()])
            .with_permission(Permission::GroupAdmin)
            .with_feature(FEATURE),
            CommandRegistration::typed(
                vec!["/示例问好".into()],
                "开/关本群的每日定点问好",
                typed_handler({
                    let config = prefix_config.clone();
                    move |context, arguments| {
                        let config = config.clone();
                        async move {
                            // 私聊里没有"当前群"可取，只能显式给群号
                            let Some(group) = arguments.i64("群号").or(context.group_id) else {
                                return Some(OutgoingMessage::text(
                                    "请在群里发这条命令，或者在末尾补一个群号。",
                                ));
                            };
                            let on = arguments.text("开关") == Some("开");
                            let mut changed = false;
                            if let Err(reason) = config.update(|stored| {
                                changed = set_greet(&mut stored.greet_groups, group, on)
                            }) {
                                return Some(OutgoingMessage::text(format!(
                                    "写配置失败：{reason}"
                                )));
                            }
                            let stored = config.get();
                            let list = if stored.greet_groups.is_empty() {
                                "（空）".to_string()
                            } else {
                                stored
                                    .greet_groups
                                    .iter()
                                    .map(|id| id.to_string())
                                    .collect::<Vec<_>>()
                                    .join("、")
                            };
                            let words = match (changed, on) {
                                (true, true) => "已加入",
                                (true, false) => "已移出",
                                (false, true) => "本来就在",
                                (false, false) => "本来就没开",
                            };
                            Some(OutgoingMessage::text(format!(
                                "群 {group} {words}问好名单；每天 {} 点向这些群问好：{list}",
                                stored.greet_hour
                            )))
                        }
                    }
                }),
            )
            .with_args(vec![
                arg::choice("开关", &["开", "关"]),
                arg::i64("群号").optional(),
            ])
            .with_permission(Permission::GroupAdmin)
            .with_feature(FEATURE),
        ]);

        // 事件钩子：按**子类**订阅，且绑在功能开关上——这个群关掉「示例」时它一起停。
        // 返回 HookFlow::Pass 表示"我看过了，别人接着看"；Handled 会短路后续钩子与命令分发。
        ctx.listen_feature(
            FEATURE,
            &[BodyFilter::notice(NoticeKind::GroupIncrease, "")],
            ListenerPriority::default(),
            event_handler(|context| async move {
                let (Some(group_id), newcomer) = (context.group_id(), context.user_id()) else {
                    return HookFlow::Pass;
                };
                // 实现端没给 user_id 时它是 0，@0 会发出一条谁也看不懂的空气泡
                if newcomer <= 0 {
                    return HookFlow::Pass;
                }
                let message = OutgoingMessage::new(vec![
                    MessageSegment::At(newcomer),
                    MessageSegment::Text(
                        " 欢迎进群，这条欢迎语来自示例插件的事件钩子。".to_string(),
                    ),
                ]);
                // notice 事件未必带 message_type，目标显式给群号，别依赖 reply 的回退判定
                let _ = send_message(MessageTarget::Group(group_id), message).await;
                HookFlow::Pass
            }),
        );

        // 出站钩子：机器人每条要说的话在交给实现端之前都会先过这里。
        // 这里只改写（rewrite）不发消息——在出站钩子里再发一条会把自己递归进去。
        ctx.on_outgoing(outbound_handler(move |context| {
            let config = prefix_config.clone();
            async move {
                let prefix = config.get().outbound_prefix;
                if prefix.is_empty() || context.is_cancelled() {
                    return;
                }
                if should_add_prefix(&context.message(), &prefix) {
                    context.rewrite(|message| {
                        message
                            .segments
                            .insert(0, MessageSegment::Text(format!("{prefix} ")));
                    });
                }
            }
        }));
        Ok(())
    }

    fn start(&self, ctx: &PluginContext) -> Result<(), String> {
        Self::schedule_greeting(ctx);
        Ok(())
    }

    /// 本插件的 arona.yml 每次热重载后回调；重排一次即可，不必比较新旧值
    fn on_config_reload(&self, ctx: &PluginContext) {
        Self::schedule_greeting(ctx);
    }

    fn stop(&self, _ctx: &PluginContext) {
        // 定时任务、后台任务、命令、钩子都不用在这里逐个撤：框架按插件归属整组回收
        // （plugin::manager::revoke_resources）。插件只关自己打开的句柄，这里一个都没有。
    }
}

/// 该不该给这条出站消息加前缀：前缀空、没内容、或头一段已经带着前缀时都不加
fn should_add_prefix(message: &OutgoingMessage, prefix: &str) -> bool {
    if prefix.is_empty() || message.segments.is_empty() {
        return false;
    }
    !matches!(message.segments.first(),
        Some(MessageSegment::Text(head)) if head.starts_with(prefix))
}

/// 开/关某个群的每日问好；返回名单是否真的变了（重复开、没开却关都算没变）
fn set_greet(groups: &mut Vec<i64>, group: i64, on: bool) -> bool {
    match (on, groups.iter().position(|id| *id == group)) {
        (true, None) => {
            groups.push(group);
            true
        }
        (false, Some(index)) => {
            groups.remove(index);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_added_once() {
        let plain = OutgoingMessage::text("今天也要加油");
        assert!(should_add_prefix(&plain, "[示例]"));

        // 别家钩子已经加过就不再叠一层
        let prefixed = OutgoingMessage::text("[示例] 今天也要加油");
        assert!(!should_add_prefix(&prefixed, "[示例]"));

        // 默认配置的前缀是空串：不该动任何消息
        assert!(!should_add_prefix(&plain, ""));
        assert!(!should_add_prefix(
            &OutgoingMessage::new(Vec::new()),
            "[示例]"
        ));
    }

    #[test]
    fn greet_list_stays_unique() {
        let mut groups = Vec::new();
        assert!(set_greet(&mut groups, 123, true));
        assert!(!set_greet(&mut groups, 123, true), "重复开不该插第二份");
        assert!(set_greet(&mut groups, 456, true));
        assert!(set_greet(&mut groups, 123, false));
        assert!(
            !set_greet(&mut groups, 123, false),
            "本就没开，关它不算变更"
        );
        assert_eq!(groups, vec![456]);
    }
}
