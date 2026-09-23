//! 插件管理器（对应 mirai 的 `PluginManager` + `JvmPluginManager`）
//!
//! 框架在这里回答三个问题：
//! 1. **谁算装上了** —— [`PluginLoader`] 交出实例，管理器按 id 记账并跑契约版本握手；
//! 2. **谁先装配** —— 按 `depends`/`soft_depends` 拓扑排序（mirai 的依赖解析），
//!    硬依赖缺失或被停用时连锁停用依赖方；
//! 3. **停用要收干净** —— `stop()` 之后由框架统一回收该插件的命令、事件订阅、
//!    定时任务、后台任务与服务（mirai 靠 CoroutineScope 消失达成的同一效果）。
//!
//! 正路装载器是 [`dynamic::DynamicPluginLoader`]：启动时扫描 `plugins/` 目录，把用户
//! 放进去的插件 dll 装配进来（对应 mirai 扫描 `plugins/` 找 jar）。
//! [`BUILTIN_LOADER`] 只是"编译进了本程序的插件实例"这个兜底口子——框架自身的产物
//! 永远走动态装载，宿主不链接任何功能插件。
use crate::framework::Framework;
use crate::plugin::AronaPlugin;
use crate::plugin::context::{PluginContext, PluginRegistrar};
use crate::plugin::description::PluginMeta;
use crate::plugin::scope::PluginScope;
use crate::runtime::log;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock, Weak};

/// 插件在管理器里的状态（mirai 的 `PluginBase` 生命周期）
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginState {
    /// 已装载，未跑过 install
    Loaded,
    /// install 成功，等待装配
    Installed,
    /// configure 成功，等待 start
    Ready,
    /// 已启动并接管事件/命令
    Active,
    /// 用户停用（不是错误，随时可再启用）
    Disabled,
    /// 握手/装配/启动失败，原因附在里面
    Failed(String),
}

impl PluginState {
    /// 正在对外提供服务（命令/事件/定时任务都活着）
    pub fn is_running(self) -> bool {
        matches!(self, PluginState::Ready | PluginState::Active)
    }

    /// 能不能被再次装配
    pub fn can_assemble(self) -> bool {
        matches!(
            self,
            PluginState::Installed | PluginState::Disabled | PluginState::Failed(_)
        )
    }

    /// 中文名（日志/诊断）
    pub fn display_name(self) -> &'static str {
        match self {
            PluginState::Loaded => "已装载",
            PluginState::Installed => "已登记",
            PluginState::Ready => "已装配",
            PluginState::Active => "运行中",
            PluginState::Disabled => "已停用",
            PluginState::Failed(_) => "失败",
        }
    }
}

/// 一个被管理器接管住的插件
pub struct ManagedPlugin {
    meta: PluginMeta,
    loader: &'static str,
    instance: Arc<dyn AronaPlugin>,
    state: RwLock<PluginState>,
    scope: Arc<PluginScope>,
    context: RwLock<Option<PluginContext>>,
}

impl ManagedPlugin {
    fn new(loader: &'static str, instance: Arc<dyn AronaPlugin>) -> Arc<ManagedPlugin> {
        let meta = instance.meta();
        let scope = Arc::new(PluginScope::new(meta.id));
        Arc::new(ManagedPlugin {
            meta,
            loader,
            instance,
            state: RwLock::new(PluginState::Loaded),
            scope,
            context: RwLock::new(None),
        })
    }

    pub fn meta(&self) -> &PluginMeta {
        &self.meta
    }

    pub fn id(&self) -> &'static str {
        self.meta.id
    }

    /// 本插件的后台任务作用域（给 [`crate::plugin::scope_of`] 用）
    pub(crate) fn scope_handle(&self) -> Arc<PluginScope> {
        self.scope.clone()
    }

    pub fn loader(&self) -> &'static str {
        self.loader
    }

    pub fn state(&self) -> PluginState {
        self.state.read().unwrap().clone()
    }

    fn set_state(&self, state: PluginState) {
        *self.state.write().unwrap() = state;
    }

    fn state_text(&self) -> String {
        match self.state() {
            PluginState::Failed(reason) => reason,
            other => other.display_name().to_string(),
        }
    }

    /// 当前上下文（从未装配过时为 None）
    pub fn context(&self) -> Option<PluginContext> {
        self.context.read().unwrap().clone()
    }

    pub fn scope(&self) -> &Arc<PluginScope> {
        &self.scope
    }

    fn attach_context(&self, context: PluginContext) {
        *self.context.write().unwrap() = Some(context);
    }

    /// 用户开关 + 契约兼容 + 硬依赖是否都可用（mirai 的 depends 传播）。
    /// 停用名单查本管理器所属框架实例的门控，依赖查表用传入的管理器，
    /// 所以测试可以拿一个局部实例跑完整生命周期。
    pub fn is_effectively_enabled(&self, manager: &PluginManager) -> bool {
        let framework = manager.framework();
        if !framework.gating().plugin_enabled(self.id()) {
            return false;
        }
        // 契约握手失败的插件不该被运行期的开关重新拉起
        if !self.meta.is_compatible() {
            return false;
        }
        self.meta.depends.iter().all(|need| {
            match manager.find(need) {
                // 硬依赖的插件根本没编译进来 / 已被停用 / 还没登记好
                None => false,
                Some(target) => {
                    framework.gating().plugin_enabled(target.id())
                        && matches!(
                            target.state(),
                            PluginState::Installed | PluginState::Ready | PluginState::Active
                        )
                }
            }
        })
    }
}

/// 插件装载器：把"从哪弄到插件实例"这件事从生命周期里剥出来
pub trait PluginLoader: Send + Sync {
    /// 装载器名（写进 plugins/<id>/plugin.yml 便于诊断）
    fn name(&self) -> &'static str;
    /// 交出本装载器发现的全部插件实例
    fn load(&self) -> Vec<Arc<dyn AronaPlugin>>;
}

/// 编译进本程序的插件（静态注册）归在这个装载器名下
pub const BUILTIN_LOADER: &str = "builtin";

/// 额外插件装载器的登记表（内置插件走 register/register_from，不必登记 loader）
#[derive(Default)]
pub struct LoaderRegistry {
    loaders: RwLock<Vec<Arc<dyn PluginLoader>>>,
}

impl LoaderRegistry {
    /// 登记一个额外的插件装载器（同名保留首个）
    pub fn add(&self, loader: Arc<dyn PluginLoader>) {
        let mut loaders = self.loaders.write().unwrap();
        if !loaders.iter().any(|item| item.name() == loader.name()) {
            loaders.push(loader);
        }
    }

    /// 取出并清空已登记的装载器（启动阶段一次性消费）
    pub fn drain(&self) -> Vec<Arc<dyn PluginLoader>> {
        self.loaders.write().unwrap().drain(..).collect()
    }
}

/// 管理器本体：插件表 + 生命周期编排
pub struct PluginManager {
    plugins: RwLock<Vec<Arc<ManagedPlugin>>>,
    /// configure 阶段见过的框架入参：运行中新启用插件时要靠它补装配
    options: RwLock<Option<LifecycleOptions>>,
    /// 所属框架实例：回收资源、给插件建上下文都要按它来。
    /// 用弱引用，免得管理器反过来续命整套注册表。
    framework: OnceLock<Weak<Framework>>,
}

/// 装配/启动阶段的框架入参
#[derive(Clone)]
pub struct LifecycleOptions {
    pub onebot_config: crate::config::onebot::OneBotConfig,
    pub test_notify: bool,
}

impl PluginManager {
    /// 新建一个自带隔离框架实例的管理器（测试用；运行期请用 [`PluginManager::global`]）
    pub fn new() -> Arc<PluginManager> {
        Framework::new().plugins().clone()
    }

    /// 进程默认实例的管理器
    pub fn global() -> Arc<PluginManager> {
        Framework::global().plugins().clone()
    }

    /// 还没挂到框架实例上的管理器（只给 [`Framework::new`] 的构造期用）
    pub(crate) fn detached() -> PluginManager {
        PluginManager {
            plugins: RwLock::new(Vec::new()),
            options: RwLock::new(None),
            framework: OnceLock::new(),
        }
    }

    /// 由 [`Framework::new`] 在整套注册表构造完成后回填一次
    pub(crate) fn attach(&self, framework: &Arc<Framework>) {
        let _ = self.framework.set(Arc::downgrade(framework));
    }

    /// 本管理器所属的框架实例
    pub fn framework(&self) -> Arc<Framework> {
        self.framework
            .get()
            .and_then(|weak| weak.upgrade())
            .expect("PluginManager 尚未挂到框架实例上")
    }

    /// 登记一个插件实例（host 静态注册路径）
    pub fn register(&self, instance: Arc<dyn AronaPlugin>) -> bool {
        self.register_from(BUILTIN_LOADER, instance)
    }

    /// 以指定装载器名义登记一个插件实例
    pub fn register_from(&self, loader: &'static str, instance: Arc<dyn AronaPlugin>) -> bool {
        let managed = ManagedPlugin::new(loader, instance);
        let mut plugins = self.plugins.write().unwrap();
        if plugins.iter().any(|item| item.id() == managed.id()) {
            log::warning(format!(
                "插件 id 重复，已忽略后登记的一份: {}",
                managed.id()
            ));
            return false;
        }
        plugins.push(managed);
        true
    }

    /// 跑一遍额外装载器；返回新登记的插件数
    pub fn load_registered_loaders(&self) -> usize {
        let mut added = 0;
        for loader in self.framework().loaders().drain() {
            for instance in loader.load() {
                if self.register_from(loader.name(), instance) {
                    added += 1;
                }
            }
        }
        added
    }

    pub fn snapshot(&self) -> Vec<Arc<ManagedPlugin>> {
        self.plugins.read().unwrap().clone()
    }

    pub fn find(&self, id: &str) -> Option<Arc<ManagedPlugin>> {
        self.plugins
            .read()
            .unwrap()
            .iter()
            .find(|plugin| plugin.id().eq_ignore_ascii_case(id))
            .cloned()
    }

    /// 装配顺序：按依赖拓扑排序（依赖在前），结果稳定不随调用次数漂移
    pub fn ordered(&self) -> Vec<Arc<ManagedPlugin>> {
        let plugins = self.snapshot();
        let keys: Vec<String> = plugins
            .iter()
            .map(|plugin| plugin.id().to_lowercase())
            .collect();
        // needs[i] = 第 i 个插件必须排在谁后面（下标）
        let mut needs: Vec<Vec<usize>> = Vec::with_capacity(plugins.len());
        for (index, plugin) in plugins.iter().enumerate() {
            let mut list: Vec<usize> = Vec::new();
            for id in plugin
                .meta()
                .depends
                .iter()
                .chain(plugin.meta().soft_depends.iter())
            {
                let key = id.to_lowercase();
                let Some(at) = keys.iter().position(|k| *k == key) else {
                    continue; // 依赖的插件没编译进来：由 is_effectively_enabled 判停用
                };
                if at != index {
                    list.push(at);
                }
            }
            needs.push(list);
        }
        let mut done: HashSet<usize> = HashSet::new();
        let mut ordered: Vec<Arc<ManagedPlugin>> = Vec::with_capacity(plugins.len());
        while ordered.len() < plugins.len() {
            // 每轮按登记原顺序挑依赖已满足者：清单顺序即展示顺序，结果可预期
            let mut progressed = false;
            for (index, plugin) in plugins.iter().enumerate() {
                if done.contains(&index) {
                    continue;
                }
                if needs[index].iter().all(|need| done.contains(need)) {
                    done.insert(index);
                    ordered.push(plugin.clone());
                    progressed = true;
                }
            }
            if !progressed {
                for (index, plugin) in plugins.iter().enumerate() {
                    if !done.contains(&index) {
                        plugin.set_state(PluginState::Failed(
                            "插件依赖成环，无法确定装配顺序".into(),
                        ));
                        log::error(format!(
                            "插件 {} 装载失败: {}",
                            plugin.meta().name,
                            plugin.state_text()
                        ));
                    }
                }
                break;
            }
        }
        ordered
    }

    /// install 阶段：契约握手 + 功能/配置区登记。单家失败不拖累别人。
    pub fn install_all(&self) -> Vec<(String, String)> {
        let mut failures: Vec<(String, String)> = Vec::new();
        let framework = self.framework();
        for plugin in self.ordered() {
            crate::plugin::prepare_plugin_home(&plugin);
            if let Err(reason) = crate::plugin::check_api(plugin.meta()) {
                self.fail(&plugin, reason, &mut failures);
                continue;
            }
            let registrar = PluginRegistrar::new(plugin.id(), &framework);
            let outcome = guarded_call(&plugin, "登记", || plugin.instance.install(&registrar));
            if let Err(reason) = outcome {
                self.fail(&plugin, reason, &mut failures);
                continue;
            }
            plugin.set_state(PluginState::Installed);
        }
        failures
    }

    /// configure 阶段：为启用的插件建上下文并登记命令/事件订阅
    pub fn configure_all(&self, options: &LifecycleOptions) -> Vec<(String, String)> {
        *self.options.write().unwrap() = Some(options.clone());
        let mut failures: Vec<(String, String)> = Vec::new();
        for plugin in self.ordered() {
            if plugin.state() != PluginState::Installed {
                continue; // 还没 install，或握手/install 已失败
            }
            if !plugin.is_effectively_enabled(self) {
                plugin.set_state(PluginState::Disabled);
                log::info(format!("插件已停用，跳过装配: {}", plugin.meta().name));
                continue;
            }
            if let Err(reason) = self.assemble(&plugin) {
                failures.push((plugin.id().to_string(), reason));
            }
        }
        failures
    }

    /// configure 阶段见过的框架入参（运行期补装配时用）
    pub fn current_options(&self) -> Option<LifecycleOptions> {
        self.options.read().unwrap().clone()
    }

    /// start 阶段：拉起后台任务
    pub fn start_all(&self) -> Vec<(String, String)> {
        let mut failures: Vec<(String, String)> = Vec::new();
        for plugin in self.ordered() {
            if plugin.state() != PluginState::Ready {
                continue;
            }
            if let Err(reason) = self.start_one(&plugin) {
                failures.push((plugin.id().to_string(), reason));
            }
        }
        failures
    }

    /// 装配单个插件：先收回上一次可能残留的登记，再跑 configure。
    /// 框架入参取 configure_all 存下的那份（运行期补装配时插件拿不到新入参）。
    fn assemble(&self, plugin: &Arc<ManagedPlugin>) -> Result<(), String> {
        let options = self
            .current_options()
            .ok_or_else(|| "框架配置尚未加载，无法装配插件".to_string())?;
        self.revoke(plugin);
        let context = PluginContext::new(
            plugin.id(),
            options.onebot_config.clone(),
            options.test_notify,
            plugin.scope.clone(),
            &self.framework(),
        );
        plugin.attach_context(context.clone());
        let outcome = guarded_call(plugin, "装配", || plugin.instance.configure(&context));
        outcome
            .map(|_| plugin.set_state(PluginState::Ready))
            .inspect_err(|reason| {
                self.revoke(plugin);
                plugin.set_state(PluginState::Failed(reason.clone()));
                log::error(format!("插件 {} 装配失败: {reason}", plugin.meta().name));
            })
    }

    fn start_one(&self, plugin: &Arc<ManagedPlugin>) -> Result<(), String> {
        let Some(context) = plugin.context() else {
            let reason = "装配未完成，缺少上下文".to_string();
            plugin.set_state(PluginState::Failed(reason.clone()));
            return Err(reason);
        };
        match guarded_call(plugin, "启动", || plugin.instance.start(&context)) {
            Ok(()) => {
                plugin.set_state(PluginState::Active);
                log::info(format!(
                    "插件已启动: {} v{}",
                    plugin.meta().name,
                    plugin.meta().version
                ));
                Ok(())
            }
            Err(reason) => {
                self.revoke(plugin);
                plugin.set_state(PluginState::Failed(reason.clone()));
                log::error(format!("插件 {} 启动失败: {reason}", plugin.meta().name));
                Err(reason)
            }
        }
    }

    fn fail(
        &self,
        plugin: &Arc<ManagedPlugin>,
        reason: String,
        failures: &mut Vec<(String, String)>,
    ) {
        failures.push((plugin.id().to_string(), reason.clone()));
        plugin.set_state(PluginState::Failed(reason));
        log::error(format!(
            "插件 {} 装载失败: {}",
            plugin.meta().name,
            plugin.state_text()
        ));
    }

    /// 停用并回收（插件的 stop() 之后框架强制回收）
    pub fn disable(&self, plugin: &Arc<ManagedPlugin>) {
        if plugin.state().is_running() {
            if let Some(context) = plugin.context() {
                guard_void(plugin, "停用", || plugin.instance.stop(&context));
            }
        }
        self.revoke(plugin);
        plugin.set_state(PluginState::Disabled);
    }

    /// 按 id 停用并回收（框架自动隔离 panic 插件时用；id 未知时返回 false）
    pub fn disable_id(&self, plugin: &str) -> bool {
        match self.find(plugin) {
            Some(plugin) => {
                if plugin.state() != PluginState::Disabled {
                    self.disable(&plugin);
                }
                true
            }
            None => false,
        }
    }

    /// 单个插件"配好并跑起来"（供启动与运行期开关共用）
    fn bring_up(&self, plugin: &Arc<ManagedPlugin>) {
        if self.assemble(plugin).is_err() {
            return; // assemble 已记日志并标 Failed
        }
        let _ = self.start_one(plugin);
    }

    /// 按当前停用名单协调生命周期：先按反依赖序停用（被依赖者最后走），
    /// 再按依赖序启用（依赖者先就位）。返回状态发生变化的插件 id。
    pub fn sync_enabled_state(&self) -> Vec<String> {
        let mut changed = Vec::new();
        let ordered = self.ordered();
        for plugin in ordered.iter().rev() {
            if plugin.state().is_running() && !plugin.is_effectively_enabled(self) {
                self.disable(plugin);
                log::info(format!("插件已停用: {}", plugin.meta().name));
                changed.push(plugin.id().to_string());
            }
        }
        for plugin in &ordered {
            let state = plugin.state();
            if plugin.is_effectively_enabled(self)
                && (state == PluginState::Disabled || matches!(state, PluginState::Failed(_)))
            {
                self.bring_up(plugin);
                if plugin.state() == PluginState::Active {
                    changed.push(plugin.id().to_string());
                }
            }
        }
        // 依赖被停掉的下游：再来一轮停用（is_effectively_enabled 已能算出）
        for plugin in ordered.iter().rev() {
            if plugin.state().is_running() && !plugin.is_effectively_enabled(self) {
                self.disable(plugin);
                log::info(format!("插件 {} 因依赖不可用而停用", plugin.meta().name));
                changed.push(plugin.id().to_string());
            }
        }
        changed
    }

    /// 配置热重载回调（only 为 Some 时只通知那一家）
    pub fn notify_config_reloaded(&self, only: Option<&str>) {
        let ordered = self.ordered();
        for plugin in ordered.iter() {
            if let Some(id) = only {
                if !plugin.id().eq_ignore_ascii_case(id) {
                    continue;
                }
            }
            if !plugin.state().is_running() {
                continue;
            }
            if let Some(context) = plugin.context() {
                guard_void(plugin, "配置重载", || {
                    plugin.instance.on_config_reload(&context)
                });
            }
        }
    }

    /// 退出：反依赖序 stop，并回收全部资源
    pub fn stop_all(&self) {
        for plugin in self.ordered().iter().rev() {
            if plugin.state().is_running() {
                if let Some(context) = plugin.context() {
                    guard_void(plugin, "停用", || plugin.instance.stop(&context));
                }
            }
            self.revoke(plugin);
            plugin.set_state(PluginState::Disabled);
        }
    }

    /// 当前运行中的插件 id
    pub fn active_ids(&self) -> Vec<String> {
        self.snapshot()
            .iter()
            .filter(|plugin| plugin.state() == PluginState::Active)
            .map(|plugin| plugin.id().to_string())
            .collect()
    }

    /// 状态快照（诊断/GUI）
    pub fn statuses(&self) -> Vec<(PluginMeta, PluginState)> {
        self.ordered()
            .iter()
            .map(|plugin| (plugin.meta().clone(), plugin.state()))
            .collect()
    }

    /// 把停用的插件名下的一切都收回去（mirai 里由作用域消失自动完成，这里显式做）。
    /// 收的是本管理器所属框架实例那套注册表，因此隔离实例上的插件不会去动进程默认实例。
    pub fn revoke(&self, plugin: &Arc<ManagedPlugin>) {
        let id = plugin.id();
        let framework = self.framework();
        let tasks = plugin.scope().cancel_all();
        let jobs = framework.jobs().remove_group(id);
        let hooks = framework.hooks().unsubscribe(id);
        framework.commands().unregister_plugin(id);
        let services = framework.container().revoke_plugin(id);
        let boards = framework.services().revoke(id);
        framework.health().forget(id);
        if tasks + jobs + hooks + services + boards > 0 {
            log::info(format!(
                "已回收插件 {id} 的资源: 后台任务 {tasks} / 定时任务 {jobs} / 事件订阅 {hooks} / 共享能力 {services} / 服务开关 {boards}"
            ));
        }
    }
}

/// 跑一个有返回值的同步生命周期回调（install/configure/start）。
///
/// 插件代码是外来的，panic 不许掀翻宿主，也不许让框架跳过后续的回收：
/// panic 一律折算成该阶段的失败原因，走调用方已有的 `revoke` + `Failed` 分支。
/// 顺带把日志来源切成 `[插件名:阶段]`——插件里打的日志不该顶着 `[Arona]` 分不清是谁。
fn guarded_call(
    plugin: &Arc<ManagedPlugin>,
    site: &str,
    f: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    crate::runtime::log::with_source(&crate::plugin::log_source(plugin.id(), site), || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
            .unwrap_or_else(|_| Err(format!("在{site}阶段 panic（已被框架隔离，栈见上方日志）")))
    })
}

/// 跑一个无返回值的同步生命周期回调（stop/on_config_reload）：
/// panic 只记日志，绝不中断框架后续的回收流程。
fn guard_void(plugin: &Arc<ManagedPlugin>, site: &str, f: impl FnOnce()) {
    let panicked =
        crate::runtime::log::with_source(&crate::plugin::log_source(plugin.id(), site), || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err()
        });
    if panicked {
        // 这句是框架自己的隔离声明，所以打在来源作用域之外，仍挂 [Arona]
        log::error(format!(
            "插件 {} 在{site}阶段 panic（已被框架隔离，流程继续，栈见上方日志）",
            plugin.meta().name
        ));
    }
}

/// 与框架契约版本握手（mirai 的 `ApiVerification`）
pub(crate) fn check_api(meta: &PluginMeta) -> Result<(), String> {
    if meta.is_compatible() {
        return Ok(());
    }
    Err(format!(
        "插件契约版本不兼容: 要求 api {}，本程序提供 api {}",
        meta.api_version,
        crate::plugin::description::FRAMEWORK_API_VERSION
    ))
}

/// 把停用的插件名下的一切都收回去（走进程默认实例；插件生命周期里由框架自动调用）
pub fn revoke_resources(plugin: &Arc<ManagedPlugin>) {
    PluginManager::global().revoke(plugin);
}

/// 登记一个额外的插件装载器（同名保留首个；进程默认实例）
pub fn add_loader(loader: Arc<dyn PluginLoader>) {
    Framework::global().loaders().add(loader);
}

/// `plugins/<id>/plugin.yml` 的内容（mirai 风格描述文件）
pub(crate) fn describe(plugin: &ManagedPlugin) -> String {
    let meta = plugin.meta();
    let list = |items: &[&'static str]| {
        if items.is_empty() {
            "[]".to_string()
        } else {
            format!("[{}]", items.join(", "))
        }
    };
    format!(
        "# 本文件由框架装载插件时自动生成，内容与插件上报的元信息一致（mirai 风格的 plugin.yml）。\n\
         # 插件目录只放随包资源；配置与数据按统一约定各归其位：\n\
         id: {id}\n\
         name: {name}\n\
         version: {version}\n\
         author: {author}\n\
         description: {description}\n\
         apiVersion: {api}\n\
         loader: {loader}\n\
         depends: {depends}\n\
         softDepends: {soft}\n\
         config: config/{id}/arona.yml\n\
         data: data/{id}\n\
         image: data/{id}/image\n",
        id = meta.id,
        name = meta.name,
        version = meta.version,
        author = if meta.author.is_empty() {
            "\"(未署名)\""
        } else {
            meta.author
        },
        description = meta.description,
        api = meta.api_version,
        loader = plugin.loader(),
        depends = list(meta.depends),
        soft = list(meta.soft_depends),
    )
}

/// 插件表为空时的兜底：GUI 文案要区分"没装插件"和"全被停用"
pub fn summary() -> HashMap<&'static str, usize> {
    let manager = PluginManager::global();
    let mut map = HashMap::new();
    map.insert("total", manager.snapshot().len());
    map.insert("active", manager.active_ids().len());
    map
}
