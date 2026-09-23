//! 插件契约层：功能部分以插件形式插入框架。
//!
//! 框架只负责 OneBot 连接、管理面板、群授权/黑名单与命令分发骨架；
//! 具体功能由插件提供，形态是**放在 `plugins/` 目录下的动态库**（Windows 上是 dll）：
//! 用户启动前把插件丢进去，框架启动时自动发现并装配（见 [`dynamic`]）。
//! 框架本体不含任何功能插件，也不为某个插件网开后门。
//!
//! 对齐 mirai 的插件模型，一次装配合约按四个阶段推进，每个阶段能做什么被接口卡死：
//!
//! | 阶段 | mirai 对应 | 插件该做什么 | 交接面 |
//! | --- | --- | --- | --- |
//! | load | `PluginManager.loadPlugins`（扫描 `plugins/`） | 无（框架扫描目录并握手，见 [`abi`]） | [`register`] |
//! | install | `Plugin.onEnable` 前的 DI 声明 | 登记功能开关、配置区 | [`PluginRegistrar`] |
//! | configure | `CommandManager.registerCommand` | 登记命令/事件订阅、公布服务 | [`PluginContext`] |
//! | start | `CoroutineScope.launch` | 开数据库、拉后台与定时任务 | [`PluginContext`] |
//!
//! 规范：每个插件必须提供 [`PluginMeta`]（id + name + version 为硬性要求）。
//! 落盘位置由框架统一规定（`plugins/<id>/`、`config/<id>/arona.yml`、`data/<id>/`），
//! 插件通过 [`PluginContext`] 上的目录接口取用，不要自己拼路径。
//!
//! 停用是「装配的镜像」：插件的 [`AronaPlugin::stop`] 跑完后，框架再把该插件名下的命令、
//! 事件订阅、定时任务、后台任务与服务一并收回（见 [`manager::revoke_resources`]），
//! 所以插件不必（也无法）靠自己在 stop 里清理干净来保证停用生效。
pub mod abi;
pub mod context;
pub mod description;
pub mod dynamic;
pub mod health;
pub mod manager;
pub mod scope;

use crate::runtime::log;
use std::future::Future;
use std::sync::Arc;

pub use abi::DYNAMIC_LOADER;
pub use context::{PluginContext, PluginRegistrar};
pub use description::{ApiVersion, FRAMEWORK_API_VERSION, PluginMeta};
pub use dynamic::DynamicPluginLoader;
pub use manager::{
    BUILTIN_LOADER, LifecycleOptions, LoaderRegistry, ManagedPlugin, PluginLoader, PluginManager,
    PluginState,
};
pub use scope::PluginScope;

/// 插件生命周期。所有方法都有默认实现，插件只需实现自己关心的阶段。
///
/// 每个阶段都可能被框架跳过（插件被停用、依赖不可用），因此实现里不要放
/// "必须执行一次"的全局初始化；那种事交给 install 阶段登记，由框架兜住顺序。
pub trait AronaPlugin: Send + Sync + 'static {
    /// 元信息（id、name、version 必填；要求框架接口版本或依赖别家插件时一并声明）
    fn meta(&self) -> PluginMeta;

    /// 登记阶段：注册功能开关与配置区。在加载 arona.yml 之前调用，
    /// 这样生成的配置模板注释里能带上完整功能清单。
    /// 返回 Err 只影响本插件（标为失败状态），不拖累其他插件。
    fn install(&self, _registrar: &PluginRegistrar) -> Result<(), String> {
        Ok(())
    }

    /// 装配阶段：登记命令与事件订阅、公布对外能力。
    /// 两份配置已加载、OneBot 连接尚未启动；插件被停用时框架不会调用这里。
    fn configure(&self, _ctx: &PluginContext) -> Result<(), String> {
        Ok(())
    }

    /// 启动阶段：打开数据库、拉起数据预热与定时推送等后台任务。
    /// 后台任务请用 `ctx.spawn(..)`、定时任务用 `ctx.daily_job(..)`，
    /// 否则插件被停用时框架收不回它们。
    fn start(&self, _ctx: &PluginContext) -> Result<(), String> {
        Ok(())
    }

    /// 本插件的 `config/<id>/arona.yml` 热重载后回调（推送小时变更等）。
    fn on_config_reload(&self, _ctx: &PluginContext) {}

    /// 退出阶段：关掉自己持有的连接与文件句柄。
    /// 运行中被切为「停用」时框架同样调用这里；之后框架会回收该插件名下的一切。
    fn stop(&self, _ctx: &PluginContext) {}
}

/// 进程默认实例的插件管理器（`Framework::global()` 名下那一个）
pub fn manager() -> Arc<PluginManager> {
    PluginManager::global()
}

/// 注册一个插件实例（host 在调用 [`crate::run`] 之前完成）
pub fn register(plugin: Arc<dyn AronaPlugin>) -> bool {
    manager().register(plugin)
}

/// 登记一个额外的插件装载器（下次 [`install_all`] 时由框架消费）
pub fn add_loader(loader: Arc<dyn PluginLoader>) {
    manager::add_loader(loader);
}

/// 已注册插件的元信息列表（GUI「插件管理」页/诊断用）
pub fn metas() -> Vec<PluginMeta> {
    manager()
        .snapshot()
        .iter()
        .map(|plugin| plugin.meta().clone())
        .collect()
}

/// 某个插件的元信息（按 id 查，GUI「插件管理」页用）
pub fn meta_of(id: &str) -> Option<PluginMeta> {
    manager().find(id).map(|plugin| plugin.meta().clone())
}

/// 插件 id → 显示名（日志前缀用）：插件代码里打的日志挂它自己的名字，不再顶着 `[Arona]`。
/// 空 id 视为框架自己，未知 id 原样回显。
pub fn display_name(id: &str) -> String {
    if id.is_empty() {
        return "Arona".to_string();
    }
    meta_of(id)
        .map(|meta| meta.name.to_string())
        .unwrap_or_else(|| id.to_string())
}

/// 插件 id + 动作 → 日志来源（`HelloPlugin:定时推送`）：框架在进入插件代码的每个
/// 入口用它换掉 `[Arona]`，动作名按入口给粗粒度默认值，插件可在内部再细化（[`action`]）。
pub fn log_source(id: &str, action: &str) -> String {
    if id.is_empty() {
        // 框架自己的任务：不带动作，仍是 `[Arona]`
        return display_name(id);
    }
    format!("{}:{action}", display_name(id))
}

/// 把当前插件的日志前缀细化成 `[插件名:动作]`，只在 `f` 执行期间有效（同步段）。
///
/// 框架已按入口给了粗粒度动作名（`装配`、`命令 活动`、`事件 群消息`、`定时 DailyNotify`…），
/// 插件在关键动作上包一层就更精确：`action("定时推送", || ..)`、`action("踢人", || ..)`。
/// 跨 `await` 的后台任务请用 [`PluginContext::spawn_as`]（线程局部的来源撑不过 await）。
pub fn action<R>(name: &str, f: impl FnOnce() -> R) -> R {
    crate::runtime::log::with_action(name, f)
}

/// [`action`] 的异步版：动作名覆盖返回 future 的每次轮询，中间的 await 不会丢掉它。
///
/// 一次外部调用就是一个动作：`action_async("发送消息", services::send_message(..)).await`。
pub fn action_async<F>(name: &str, future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    crate::runtime::log::with_action_async(name, future)
}

/// 这个名字是不是某个已登记插件的显示名（控制台与 GUI 按它把插件日志染成淡紫）
pub fn is_plugin_name(name: &str) -> bool {
    manager()
        .snapshot()
        .iter()
        .any(|plugin| plugin.meta().name == name)
}

/// 按 id 取回该插件的后台任务作用域：定时任务体里要起异步活儿时用它。
/// 裸 `tokio::spawn` 起的任务不受插件停用回收，日志也认不出归属。
pub fn scope_of(id: &str) -> Option<Arc<PluginScope>> {
    manager().find(id).map(|plugin| plugin.scope_handle())
}

/// 某个插件当前状态（按 id 查）
pub fn state_of(id: &str) -> Option<PluginState> {
    manager().find(id).map(|plugin| plugin.state())
}

/// install 阶段：先扫描 `plugins/` 目录并消化其它装载器，再登记功能清单与配置区。
/// 单个插件失败只记日志，不影响其余插件。
pub fn install_all() -> Result<(), String> {
    ensure_dynamic_loader();
    manager().load_registered_loaders();
    for (id, reason) in manager().install_all() {
        log::error(format!("插件 {id} 登记失败: {reason}"));
    }
    Ok(())
}

/// 登记内置的目录扫描装载器（幂等：`plugins/` 只在启动时扫一遍）
fn ensure_dynamic_loader() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| add_loader(Arc::new(dynamic::DynamicPluginLoader)));
}

/// 装配阶段：只为启用的插件登记命令与事件订阅
pub fn configure_all(
    onebot_config: crate::config::onebot::OneBotConfig,
    test_notify: bool,
) -> Result<(), String> {
    let options = LifecycleOptions {
        onebot_config,
        test_notify,
    };
    for (id, reason) in manager().configure_all(&options) {
        log::error(format!("插件 {id} 装配失败: {reason}"));
    }
    Ok(())
}

/// 启动阶段：拉起已装配插件的后台任务
pub fn start_all() -> Result<(), String> {
    for (id, reason) in manager().start_all() {
        log::error(format!("插件 {id} 启动失败: {reason}"));
    }
    Ok(())
}

/// 按当前「停用插件」名单协调生命周期：停用者收干净，新启用的补装配并拉起。
/// arona.yml 热重载与 GUI 开关都走这里；返回状态发生变化的插件 id。
pub fn sync_enabled_state() -> Vec<String> {
    manager().sync_enabled_state()
}

/// 当前运行中的插件 id
pub fn active_plugins() -> Vec<String> {
    manager().active_ids()
}

/// 配置热重载：通知全部运行中的插件
pub fn notify_config_reloaded() {
    manager().notify_config_reloaded(None);
}

/// 只通知某个插件（它的 `config/<id>/arona.yml` 变了）
pub fn notify_config_reloaded_of(plugin_id: &str) {
    manager().notify_config_reloaded(Some(plugin_id));
}

pub fn stop_all() {
    manager().stop_all();
}

/// 命令名在某群是否该被路由：提供它的插件被全局禁用、或在该群被禁用时一律不路由。
/// 门控细节已在分发器里按归属做过，这里留给 GUI/诊断做前置判断。
pub fn dispatcher_active_in_group(group_id: Option<i64>) -> bool {
    manager().snapshot().iter().any(|plugin| {
        plugin.state().is_running() && {
            let id = plugin.id();
            crate::runtime::config::plugin_enabled(id)
                && crate::runtime::config::plugin_enabled_in_group(id, group_id)
        }
    })
}

/// 建好插件的三件套目录并写下 `plugins/<id>/plugin.yml`：
/// 内容直接由插件 dll 的 `meta()` 渲染，磁盘上看到的描述与代码里的完全一致（对齐
/// mirai 从 jar 里读 `plugin.yml` 的做法），用户从中能看出装了谁、它的配置与数据放在哪。
pub(crate) fn prepare_plugin_home(plugin: &Arc<ManagedPlugin>) {
    let id = plugin.id();
    let dir = crate::runtime::paths::plugin_dir(id);
    let file = dir.join("plugin.yml");
    let content = manager::describe(plugin);
    let unchanged = std::fs::read_to_string(&file)
        .map(|text| text == content)
        .unwrap_or(false);
    if !unchanged {
        if let Err(err) = std::fs::write(&file, content) {
            log::warning(format!("写入插件清单失败: {} —— {err}", file.display()));
        }
    }
    // 目录先建出来，插件不写任何文件时也保持可见
    crate::runtime::paths::plugin_config_dir(id);
    crate::runtime::paths::plugin_data_dir(id);
}

pub(crate) use manager::check_api;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::arona::GroupSetting;
    use crate::config::onebot::OneBotConfig;
    use crate::runtime::dispatcher::{CommandContext, CommandRegistration, handler};
    use crate::runtime::message::{
        BoxFuture, MessageReceipt, MessageSender, MessageTarget, OutgoingMessage,
    };
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NullSender;

    impl MessageSender for NullSender {
        fn send<'a>(
            &'a self,
            _target: MessageTarget,
            _message: OutgoingMessage,
        ) -> BoxFuture<'a, MessageReceipt> {
            Box::pin(async { MessageReceipt::default() })
        }
    }

    fn context(text: &str, group_id: Option<i64>) -> Arc<CommandContext> {
        Arc::new(CommandContext {
            user_id: 100,
            group_id,
            text: text.to_string(),
            sender_name: None,
            is_admin: false,
            sender_role: None,
            message_id: None,
            time: 0,
            quoted: None,
            segments: Vec::new(),
            sender: Arc::new(NullSender),
        })
    }

    fn onebot_config() -> OneBotConfig {
        OneBotConfig {
            self_id: 1,
            nickname: "Arona".to_string(),
            send_image_as_file: false,
            connections: BTreeMap::new(),
        }
    }

    /// 全生命周期契约：install/configure/start 跑齐，停用后命令不再路由，stop 后资源归零。
    /// 这条用例同时是"多插件并存"的回归——两家插件各自的命令都要能路由。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lifecycle_runs_two_plugins_independently() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        /// 最近命中的命令：1=甲，2=乙
        static LAST_HIT: AtomicUsize = AtomicUsize::new(0);

        struct Counting(&'static str);

        impl AronaPlugin for Counting {
            fn meta(&self) -> PluginMeta {
                PluginMeta::new(self.0, "契约自检插件", "1.0.0", "生命周期自检")
            }

            fn install(&self, _reg: &PluginRegistrar) -> Result<(), String> {
                CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }

            fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
                CALLS.fetch_add(1, Ordering::SeqCst);
                let slot = match self.0 {
                    "ContractAlpha" => 1,
                    _ => 2,
                };
                ctx.command(CommandRegistration::new(
                    vec![format!("/契约自检{slot}")],
                    "契约自检",
                    handler(
                        move |_ctx: Arc<CommandContext>, _args: Vec<String>| async move {
                            LAST_HIT.store(slot, Ordering::SeqCst);
                            None
                        },
                    ),
                ));
                Ok(())
            }

            fn start(&self, _ctx: &PluginContext) -> Result<(), String> {
                CALLS.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }

            fn stop(&self, _ctx: &PluginContext) {
                CALLS.fetch_add(1, Ordering::SeqCst);
            }
        }

        // 整套注册表都用局部实例：插件表、命令表、停用名单都不碰进程默认实例
        let framework = crate::framework::Framework::new();
        let manager = framework.plugins().clone();
        assert!(manager.register(Arc::new(Counting("ContractAlpha"))));
        assert!(manager.register(Arc::new(Counting("ContractBeta"))));
        assert!(
            !manager.register(Arc::new(Counting("ContractAlpha"))),
            "同 id 重复登记应被拒绝"
        );

        let options = LifecycleOptions {
            onebot_config: onebot_config(),
            test_notify: false,
        };
        assert!(manager.install_all().is_empty());
        // install 阶段要在磁盘上写出 mirai 风格的插件清单，并备好配置/数据目录
        let descriptor = std::fs::read_to_string(
            crate::runtime::paths::plugin_dir("ContractAlpha").join("plugin.yml"),
        )
        .expect("install 阶段应生成 plugins/<id>/plugin.yml");
        for line in [
            "id: ContractAlpha",
            "name: 契约自检插件",
            "version: 1.0.0",
            "apiVersion: 1.0.0",
            "loader: builtin",
            "depends: []",
            "softDepends: []",
            "config: config/ContractAlpha/arona.yml",
            "data: data/ContractAlpha",
        ] {
            assert!(
                descriptor.contains(&format!("{line}\n")),
                "清单缺少 {line}: {descriptor}"
            );
        }
        assert!(
            crate::runtime::paths::plugin_config_dir("ContractAlpha").is_dir()
                && crate::runtime::paths::plugin_data_dir("ContractAlpha").is_dir(),
            "配置与数据目录应在 install 阶段建好"
        );
        assert!(manager.configure_all(&options).is_empty());
        assert!(manager.start_all().is_empty());
        // install + configure + start 各两家
        assert_eq!(CALLS.load(Ordering::SeqCst), 6);
        assert_eq!(manager.active_ids().len(), 2);

        let dispatcher = crate::runtime::dispatcher::CommandDispatcher::with_framework(&framework);
        LAST_HIT.store(0, Ordering::SeqCst);
        assert!(dispatcher.dispatch(context("/契约自检1", None)).await);
        assert_eq!(LAST_HIT.load(Ordering::SeqCst), 1);
        assert!(dispatcher.dispatch(context("/契约自检2", None)).await);
        assert_eq!(LAST_HIT.load(Ordering::SeqCst), 2);

        // 停用一家：它的命令立即不再路由，另一家照旧（旧实现里整张表跟着一起没了）
        framework
            .gating()
            .set_disabled_plugins(vec!["ContractAlpha".to_string()]);
        manager.sync_enabled_state();
        assert_eq!(
            manager.find("ContractAlpha").unwrap().state(),
            PluginState::Disabled
        );
        assert!(!dispatcher.dispatch(context("/契约自检1", None)).await);
        assert!(dispatcher.dispatch(context("/契约自检2", None)).await);

        // 分群停用：只有那个群不路由
        let mut settings = BTreeMap::new();
        settings.insert(
            "1".to_string(),
            GroupSetting {
                disabled_plugins: vec!["ContractBeta".to_string()],
                ..Default::default()
            },
        );
        framework.gating().set_group_settings(settings);
        assert!(!dispatcher.dispatch(context("/契约自检2", Some(1))).await);
        assert!(dispatcher.dispatch(context("/契约自检2", Some(2))).await);
        framework.gating().set_group_settings(BTreeMap::new());

        // 重新启用：补装配 + start，不需要重启进程
        framework.gating().set_disabled_plugins(Vec::new());
        manager.sync_enabled_state();
        assert_eq!(
            manager.find("ContractAlpha").unwrap().state(),
            PluginState::Active
        );
        assert!(dispatcher.dispatch(context("/契约自检1", None)).await);

        manager.stop_all();
        // 6（两家各 install/configure/start）+ 1（Alpha 停用时的 stop）
        // + 2（Alpha 重新启用补的 configure/start）+ 2（退出时各停一次）
        assert_eq!(CALLS.load(Ordering::SeqCst), 6 + 1 + 2 + 2);
        assert!(!dispatcher.dispatch(context("/契约自检1", None)).await);
        assert!(manager.active_ids().is_empty());

        // install 阶段会在磁盘上留下插件目录三件套，跑完抹掉，别污染工作区
        for id in ["ContractAlpha", "ContractBeta"] {
            let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_dir(id));
            let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_config_dir(id));
            let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_data_dir(id));
        }
    }

    /// configure 里 panic 的插件要折算成"该家装配失败"（标 Failed + 回收它已登记的命令），
    /// 既不许掀翻宿主，也不许连累同批装配的另一家。
    /// 回归点：五个生命周期回调以前都是裸调，panic 会顺着 configure_all 冒到宿主。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panicking_configure_fails_only_that_plugin() {
        const CRASHER: &str = "LifecyclePanicCrasher";
        const BYSTANDER: &str = "LifecyclePanicBystander";

        struct Panics(&'static str);

        impl AronaPlugin for Panics {
            fn meta(&self) -> PluginMeta {
                PluginMeta::new(self.0, "生命周期 panic 自检", "1.0.0", "panic 隔离自检")
            }

            fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
                let slot = if self.0 == CRASHER { 1 } else { 2 };
                // 先登记一条命令再 panic：回收必须把它一起带走
                ctx.command(CommandRegistration::new(
                    vec![format!("/panic自检{slot}")],
                    "panic 隔离自检",
                    handler(|_c: Arc<CommandContext>, _a: Vec<String>| async move { None }),
                ));
                if self.0 == CRASHER {
                    panic!("装配阶段炸弹");
                }
                Ok(())
            }
        }

        let framework = crate::framework::Framework::new();
        let manager = framework.plugins().clone();
        assert!(manager.register(Arc::new(Panics(CRASHER))));
        assert!(manager.register(Arc::new(Panics(BYSTANDER))));
        let options = LifecycleOptions {
            onebot_config: onebot_config(),
            test_notify: false,
        };
        assert!(manager.install_all().is_empty());

        let failures = manager.configure_all(&options);
        assert_eq!(
            failures
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec![CRASHER],
            "只该报出事那一家: {failures:?}"
        );
        assert!(
            failures[0].1.contains("panic"),
            "失败原因要点名 panic: {:?}",
            failures[0]
        );
        assert!(
            matches!(
                manager.find(CRASHER).unwrap().state(),
                PluginState::Failed(_)
            ),
            "出事插件应标 Failed"
        );
        assert_eq!(manager.find(BYSTANDER).unwrap().state(), PluginState::Ready);
        assert!(
            !framework.commands().has_commands_of(CRASHER),
            "出事插件 panic 前登记的命令应被 revoke 收干净"
        );
        assert!(framework.commands().has_commands_of(BYSTANDER));
        manager.stop_all();

        for id in [CRASHER, BYSTANDER] {
            let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_dir(id));
            let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_config_dir(id));
            let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_data_dir(id));
        }
    }

    struct ProbeService;

    /// 停用即回收：插件经 ctx 登记的命令/事件订阅/定时任务/后台任务/共享能力/服务开关，
    /// 都要在框架跑完 stop() 后一律消失——插件自己不必（也没法）记干净。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabling_a_plugin_revokes_everything_it_registered() {
        use crate::onebot::hooks::{ListenerPriority, event_handler};
        use crate::onebot::{EventContext, EventKind, HookFlow};
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        const ID: &str = "ContractResource";
        static STOPPED: AtomicBool = AtomicBool::new(false);

        struct Resourceful;
        impl AronaPlugin for Resourceful {
            fn meta(&self) -> PluginMeta {
                PluginMeta::new(ID, "资源回收自检", "1.0.0", "停用回收自检")
            }

            fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
                ctx.command(CommandRegistration::new(
                    vec!["/回收自检".into()],
                    "资源回收自检",
                    handler(|_c: Arc<CommandContext>, _a: Vec<String>| async move { None }),
                ));
                ctx.listen(
                    &[EventKind::Notice],
                    ListenerPriority::Normal,
                    event_handler(|_e: Arc<EventContext>| Box::pin(async { HookFlow::Pass })),
                );
                ctx.daily_job(3, "ContractResourceDaily", Arc::new(|| {}));
                ctx.declare_service::<ProbeService>(Arc::new(ProbeService));
                ctx.register_service(crate::services::service_info(
                    901,
                    "回收自检服务",
                    false,
                    false,
                ));
                ctx.spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    }
                });
                Ok(())
            }

            fn stop(&self, _ctx: &PluginContext) {
                STOPPED.store(true, Ordering::SeqCst);
            }
        }

        let framework = crate::framework::Framework::new();
        let manager = framework.plugins().clone();
        assert!(manager.register(Arc::new(Resourceful)));
        let options = LifecycleOptions {
            onebot_config: onebot_config(),
            test_notify: false,
        };
        assert!(manager.install_all().is_empty());
        assert!(manager.configure_all(&options).is_empty());
        assert!(manager.start_all().is_empty());

        let plugin = manager.find(ID).expect("插件应已登记");
        assert_eq!(
            framework.hooks().hook_count(ID),
            1,
            "事件订阅应记在插件名下"
        );
        assert!(framework.jobs().exists("ContractResourceDaily"));
        assert!(framework.container().instance::<ProbeService>().is_some());
        assert_eq!(plugin.scope().running_tasks(), 1, "后台任务应由作用域记账");
        assert!(
            framework.services().find_by_name("回收自检服务").is_some(),
            "服务开关应记在插件名下"
        );
        assert!(
            !framework.commands().commands_of(ID).is_empty(),
            "命令应已登记"
        );

        manager.disable(&plugin);
        assert!(
            STOPPED.load(Ordering::SeqCst),
            "停用时框架要调用插件的 stop()"
        );
        assert_eq!(framework.hooks().hook_count(ID), 0, "事件订阅应随停用注销");
        assert!(
            !framework.jobs().exists("ContractResourceDaily"),
            "定时任务应整组取消"
        );
        assert!(
            framework.container().instance::<ProbeService>().is_none(),
            "服务应被撤销"
        );
        assert_eq!(plugin.scope().running_tasks(), 0, "后台任务应被取消");
        assert!(
            framework.services().find_by_name("回收自检服务").is_none(),
            "服务开关应随停用人一起撤掉"
        );
        assert!(!framework.commands().has_commands_of(ID), "命令应全部撤回");
        assert_eq!(plugin.state(), PluginState::Disabled);
        // 回收只作用于本实例：进程默认实例那套注册表从头到尾没被碰过
        assert_eq!(crate::onebot::hooks::hook_count(ID), 0);
        assert!(crate::container::instance::<ProbeService>().is_none());
        assert!(!crate::quartz::exists("ContractResourceDaily"));

        let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_dir(ID));
        let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_config_dir(ID));
        let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_data_dir(ID));
    }

    /// 依赖与契约握手的落地行为：装配顺序按 depends 拓扑排（与登记顺序无关）、
    /// 契约版本不兼容只淘汰这一家、硬依赖被停用时下游连锁停用。
    #[tokio::test(flavor = "current_thread")]
    async fn dependencies_install_in_topological_order_and_cascade() {
        struct Spec {
            id: &'static str,
            depends: &'static [&'static str],
            api: ApiVersion,
        }

        struct Depending(Spec);

        impl AronaPlugin for Depending {
            fn meta(&self) -> PluginMeta {
                PluginMeta::new(self.0.id, "依赖自检插件", "1.0.0", "依赖与握手自检")
                    .depends_on(self.0.depends)
                    .requires_api(self.0.api)
            }
        }

        let current = crate::plugin::FRAMEWORK_API_VERSION;
        let framework = crate::framework::Framework::new();
        let manager = framework.plugins().clone();
        // 故意先登记"依赖方"：拓扑排序要把被依赖者提到前面
        assert!(manager.register(Arc::new(Depending(Spec {
            id: "DependChild",
            depends: &["DependParent"],
            api: current,
        }))));
        assert!(manager.register(Arc::new(Depending(Spec {
            id: "DependParent",
            depends: &[],
            api: current,
        }))));
        // 要求一个还不存在的契约版本：只该淘汰它自己
        assert!(manager.register(Arc::new(Depending(Spec {
            id: "DependStale",
            depends: &[],
            api: ApiVersion::new(current.major + 1, 0, 0),
        }))));

        let ids: Vec<&str> = manager.ordered().iter().map(|plugin| plugin.id()).collect();
        assert!(
            ids.iter().position(|id| *id == "DependParent")
                < ids.iter().position(|id| *id == "DependChild"),
            "被依赖者要排在前面: {ids:?}"
        );

        let options = LifecycleOptions {
            onebot_config: onebot_config(),
            test_notify: false,
        };
        let failures = manager.install_all();
        assert_eq!(
            failures
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["DependStale"],
            "只有契约不兼容的插件该被淘汰"
        );
        assert!(
            matches!(
                manager.find("DependStale").unwrap().state(),
                PluginState::Failed(_)
            ),
            "握手失败要写明原因"
        );
        assert!(manager.configure_all(&options).is_empty());
        assert!(manager.start_all().is_empty());
        assert_eq!(manager.active_ids().len(), 2, "别家插件不受一家失败的影响");

        // 停用被依赖者：依赖方连锁停用
        framework
            .gating()
            .set_disabled_plugins(vec!["DependParent".to_string()]);
        let changed = manager.sync_enabled_state();
        assert!(changed.contains(&"DependParent".to_string()));
        assert_eq!(
            manager.find("DependChild").unwrap().state(),
            PluginState::Disabled
        );
        framework.gating().set_disabled_plugins(Vec::new());
        manager.sync_enabled_state();
        assert_eq!(
            manager.find("DependChild").unwrap().state(),
            PluginState::Active,
            "依赖恢复后下游要能重新装配"
        );
        assert!(
            matches!(
                manager.find("DependStale").unwrap().state(),
                PluginState::Failed(_)
            ),
            "契约不兼容的插件不该被运行期开关重新拉起"
        );
        assert_eq!(manager.active_ids().len(), 2);

        for id in ["DependChild", "DependParent", "DependStale"] {
            let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_dir(id));
            let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_config_dir(id));
            let _ = std::fs::remove_dir_all(crate::runtime::paths::plugin_data_dir(id));
        }
    }
}
