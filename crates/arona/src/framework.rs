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
use crate::plugin::manager::{LoaderRegistry, PluginManager};
use crate::quartz::Scheduler;
use crate::runtime::config::Gating;
use crate::runtime::dispatcher::CommandRegistry;
use crate::services::ServiceManager;
use std::sync::{Arc, OnceLock};

/// 一套插件契约注册表。字段都是 `Arc`，克隆实例引用即可传给上下文。
pub struct Framework {
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
}

impl Framework {
    /// 新建一套完全隔离的注册表（测试/多实例宿主用）
    pub fn new() -> Arc<Framework> {
        let gating = Arc::new(Gating::default());
        let sections = Arc::new(SectionRegistry::default());
        let plugins = Arc::new(PluginManager::detached());
        let framework = Arc::new(Framework {
            gating: gating.clone(),
            commands: Arc::new(CommandRegistry::new(gating.clone())),
            hooks: Arc::new(HookRegistry::new(gating.clone())),
            container: Arc::new(ServiceContainer::new(gating.clone())),
            services: Arc::default(),
            sections: sections.clone(),
            configs: Arc::new(ConfigStore::new(sections, plugins.clone())),
            jobs: Arc::default(),
            loaders: Arc::default(),
            plugins: plugins.clone(),
        });
        // PluginManager 要按归属回收资源、给插件建上下文，得知道整套注册表；
        // 它先于 Framework 造出来，所以构造完再回填一次。
        plugins.attach(&framework);
        framework
    }

    /// 进程默认实例
    pub fn global() -> &'static Framework {
        static FRAMEWORK: OnceLock<Arc<Framework>> = OnceLock::new();
        FRAMEWORK.get_or_init(Framework::new)
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
