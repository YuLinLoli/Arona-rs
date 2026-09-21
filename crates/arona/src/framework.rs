//! 框架实例（对应 mirai 的 `MiraiInstance`）：把插件契约面的注册表从进程级 `static`
//! 收拢成一个可持有的对象。
//!
//! 改造前每个注册表各自抱着一个 `static`，插件契约面因此只能存在一套，
//! 测试之间必须串行抢同一份命令表/钩子表/停用名单。现在它们都由 [`Framework`] 持有：
//!
//! - [`Framework::global()`]：进程默认实例。各模块原有的自由函数（`arona::container::instance`、
//!   `arona::runtime::dispatcher::register`、GUI 用的 `runtime::config::*` …）都转发到它，
//!   所以既有调用方一行不用改。
//! - [`Framework::new()`]：一套全空的隔离实例。测试与将来的多实例宿主用它，
//!   `PluginManager`/`PluginContext` 拿到的是实例引用，不再碰进程级状态。
//!
//! ```ignore
//! let framework = arona::framework::Framework::new();
//! let manager = framework.plugins().clone();
//! manager.register(MyPlugin);
//! assert!(manager.install_all().is_empty());
//! ```
//!
//! 刻意留在进程级的是**真正只有一份的进程资源**：日志、目录约定（`runtime::paths`）、
//! OneBot 连接与控制台。它们是宿主进程的属性，不是插件契约的注册表。
use crate::config::arona::SectionRegistry;
use crate::config::plugin_config::ConfigStore;
use crate::container::ServiceContainer;
use crate::onebot::hooks::HookRegistry;
use crate::plugin::health::HealthBoard;
use crate::plugin::manager::{LoaderRegistry, PluginManager};
use crate::quartz::Scheduler;
use crate::runtime::config::Gating;
use crate::runtime::dispatcher::CommandRegistry;
use crate::services::ServiceManager;
use std::sync::{Arc, OnceLock};

/// 一套插件契约注册表。字段都是 `Arc`，克隆实例引用即可传给上下文。
pub struct Framework {
    options: FrameworkOptions,
    gating: Arc<Gating>,
    commands: Arc<CommandRegistry>,
    hooks: Arc<HookRegistry>,
    container: Arc<ServiceContainer>,
    services: Arc<ServiceManager>,
    sections: Arc<SectionRegistry>,
    configs: Arc<ConfigStore>,
    jobs: Arc<Scheduler>,
    loaders: Arc<LoaderRegistry>,
    plugins: Arc<PluginManager>,
    health: Arc<HealthBoard>,
}

/// 框架实例的构造选项（对应 mirai 的 `MiraiInstance.new { }`）
#[derive(Clone, Debug)]
pub struct FrameworkOptions {
    /// 同一插件连续 panic 多少次就隔离停用；0 表示只记日志、不停用
    pub panic_disable_threshold: u32,
    /// 没有显式声明 `with_prefix_match` 的命令，是否也允许最短前缀匹配（默认关）
    pub prefix_match_by_default: bool,
}

impl Default for FrameworkOptions {
    fn default() -> Self {
        FrameworkOptions {
            panic_disable_threshold: Framework::DEFAULT_PANIC_THRESHOLD,
            prefix_match_by_default: false,
        }
    }
}

/// [`Framework::builder()`] 的返回值
pub struct FrameworkBuilder {
    options: FrameworkOptions,
}

impl FrameworkBuilder {
    /// 设定 panic 隔离阈值（0 表示不因 panic 停用插件）
    pub fn panic_disable_threshold(mut self, threshold: u32) -> FrameworkBuilder {
        self.options.panic_disable_threshold = threshold;
        self
    }

    /// 全局打开/关闭命令的最短前缀匹配
    pub fn prefix_match_by_default(mut self, enabled: bool) -> FrameworkBuilder {
        self.options.prefix_match_by_default = enabled;
        self
    }

    pub fn build(self) -> Arc<Framework> {
        Framework::with_options(self.options)
    }
}

impl Framework {
    /// 插件连续 panic 多少次就隔离停用
    pub const DEFAULT_PANIC_THRESHOLD: u32 = 5;

    /// 新建一套完全隔离的注册表（测试/多实例宿主用）
    pub fn new() -> Arc<Framework> {
        Self::with_options(FrameworkOptions::default())
    }

    /// 带选项地新建（mirai 的 `MiraiInstance.new { }` 对位）
    pub fn with_options(options: FrameworkOptions) -> Arc<Framework> {
        let gating = Arc::new(Gating::default());
        let sections = Arc::new(SectionRegistry::default());
        let plugins = Arc::new(PluginManager::detached());
        let health = Arc::new(HealthBoard::new(
            options.panic_disable_threshold,
            gating.clone(),
        ));
        let prefix_match_by_default = options.prefix_match_by_default;
        let framework = Arc::new(Framework {
            options,
            gating: gating.clone(),
            commands: Arc::new(CommandRegistry::new(
                gating.clone(),
                health.clone(),
                prefix_match_by_default,
            )),
            hooks: Arc::new(HookRegistry::new(gating.clone(), health.clone())),
            container: Arc::new(ServiceContainer::new(gating.clone())),
            services: Arc::default(),
            sections: sections.clone(),
            configs: Arc::new(ConfigStore::new(sections, plugins.clone())),
            jobs: Arc::default(),
            loaders: Arc::default(),
            plugins: plugins.clone(),
            health: health.clone(),
        });
        // PluginManager 与 HealthBoard 要按归属回收资源、隔离停用插件，得知道整套注册表；
        // 它们先于 Framework 造出来，所以构造完再回填一次。
        plugins.attach(&framework);
        health.attach(&framework);
        framework
    }

    /// 构造选项的入口（`Framework::builder().panic_disable_threshold(0).build()`）
    pub fn builder() -> FrameworkBuilder {
        FrameworkBuilder {
            options: FrameworkOptions::default(),
        }
    }

    /// 进程默认实例
    pub fn global() -> &'static Framework {
        static FRAMEWORK: OnceLock<Arc<Framework>> = OnceLock::new();
        FRAMEWORK.get_or_init(Framework::new)
    }

    /// 本实例是否就是进程默认实例（只有它可以写进程级的配置文件）
    fn is_process_default(&self) -> bool {
        std::ptr::eq(self, Framework::global())
    }

    /// 本实例的构造选项
    pub fn options(&self) -> &FrameworkOptions {
        &self.options
    }

    /// panic 记账与隔离停用判定
    pub fn health(&self) -> &Arc<HealthBoard> {
        &self.health
    }

    /// 隔离停用某个插件：进停用名单（内存立即生效）→ 回收它登记的一切 → 尽力落盘。
    /// 用于插件反复 panic 时止血；GUI 的「插件管理」页会看到它被关掉。
    pub fn quarantine(&self, plugin: &str, threshold: u32) -> bool {
        let mut list = self.gating.disabled_plugins();
        if !list.iter().any(|entry| entry == plugin) {
            list.push(plugin.to_string());
            self.gating.set_disabled_plugins(list);
        }
        let disabled = self.plugins.disable_id(plugin);
        self.health.forget(plugin);
        crate::runtime::log::error(format!(
            "插件 {plugin} 连续 {threshold} 次 panic，已隔离停用（回收它登记的全部资源）"
        ));
        // 落盘只在进程默认实例上做：config/standalone 是进程级资源，隔离实例不该碰真实配置
        if self.is_process_default() {
            if let Err(problem) = crate::config::standalone::set_plugin_enabled(plugin, false) {
                crate::runtime::log::debug(format!("停用状态未能写入 arona.yml: {problem}"));
            }
        }
        disabled
    }

    /// 功能清单与停用/黑名单门控
    pub fn gating(&self) -> &Gating {
        &self.gating
    }

    /// 命令表
    pub fn commands(&self) -> &Arc<CommandRegistry> {
        &self.commands
    }

    /// 事件钩子表
    pub fn hooks(&self) -> &Arc<HookRegistry> {
        &self.hooks
    }

    /// 服务容器（插件间共享能力实例，对应 mirai 的 `DiContainer`）
    pub fn container(&self) -> &Arc<ServiceContainer> {
        &self.container
    }

    /// 服务开关表（插件对外暴露、可单独关停的功能单元，GUI「服务管理」页读这张表）
    pub fn services(&self) -> &Arc<ServiceManager> {
        &self.services
    }

    /// 插件配置区登记表
    pub fn sections(&self) -> &Arc<SectionRegistry> {
        &self.sections
    }

    /// 插件配置文件（`config/<id>/arona.yml`）的加载状态
    pub fn configs(&self) -> &Arc<ConfigStore> {
        &self.configs
    }

    /// 定时任务表
    pub fn jobs(&self) -> &Arc<Scheduler> {
        &self.jobs
    }

    /// 插件装载器登记表
    pub fn loaders(&self) -> &Arc<LoaderRegistry> {
        &self.loaders
    }

    /// 插件表与生命周期编排
    pub fn plugins(&self) -> &Arc<PluginManager> {
        &self.plugins
    }
}
