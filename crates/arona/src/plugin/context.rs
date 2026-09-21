//! 插件与框架之间的两个交接面：登记阶段用的 [`PluginRegistrar`]、
//! 装配/运行阶段用的 [`PluginContext`]。
//!
//! 两者都自带"我是谁"（插件 id）和"我属于哪套框架实例"（[`Framework`]），
//! 所以登记出去的功能开关、命令、事件订阅、定时任务、后台任务与服务**天然带上归属**，
//! 框架才能按归属做停用与回收——插件不需要（也没机会）把 id 手填进每个接口。
use crate::config::arona::PluginConfig;
use crate::config::onebot::OneBotConfig;
use crate::config::plugin_config::ConfigEntry;
use crate::framework::Framework;
use crate::onebot::api::OneBotApi;
use crate::onebot::hooks::{BodyFilter, EventHandler, EventKind, ListenerPriority};
use crate::plugin::scope::PluginScope;
use crate::runtime::config::Feature;
use crate::runtime::dispatcher::{CommandRegistration, FallbackHandler};
use std::path::PathBuf;
use std::sync::Arc;

/// install 阶段的登记表（此时框架配置还没加载，故只有登记能力）
pub struct PluginRegistrar {
    plugin: String,
    framework: Arc<Framework>,
}

impl PluginRegistrar {
    pub(crate) fn new(plugin: &str, framework: &Arc<Framework>) -> PluginRegistrar {
        PluginRegistrar {
            plugin: plugin.to_string(),
            framework: framework.clone(),
        }
    }

    /// 本插件 id
    pub fn plugin_id(&self) -> &str {
        &self.plugin
    }

    /// 本插件所属的框架实例
    pub fn framework(&self) -> &Arc<Framework> {
        &self.framework
    }

    /// 登记一个可分群开关的功能：GUI「功能开关」页与配置模板注释据此生成
    pub fn feature(&self, feature: Feature) {
        self.framework
            .gating()
            .register_feature(feature, &self.plugin);
    }

    /// 登记一块强类型配置区（框架在 `config/<id>/arona.yml` 生成带注释模板并负责热重载）
    pub fn config<T: PluginConfig>(&self, key: &'static str) {
        self.framework
            .sections()
            .register(&self.plugin, crate::config::arona::typed_section::<T>(key));
    }

    /// 本插件的目录 `plugins/<id>/`
    pub fn plugin_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_dir(&self.plugin)
    }

    /// 本插件的配置目录 `config/<id>/`
    pub fn config_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_config_dir(&self.plugin)
    }

    /// 本插件的配置文件 `config/<id>/arona.yml`
    pub fn config_file(&self) -> PathBuf {
        crate::runtime::paths::plugin_config_file(&self.plugin)
    }

    /// 本插件的数据目录 `data/<id>/`
    pub fn data_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_data_dir(&self.plugin)
    }

    /// 本插件的图片目录 `data/<id>/image/`
    pub fn image_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_image_dir(&self.plugin)
    }
}

struct ContextInner {
    plugin: String,
    scope: Arc<PluginScope>,
    framework: Arc<Framework>,
}

/// 装配（configure）与运行（start/reload/stop）阶段交给插件的上下文
#[derive(Clone)]
pub struct PluginContext {
    /// 已加载的 OneBot 协议配置（需要 self_id / nickname 时用）
    pub onebot_config: OneBotConfig,
    /// 命令行是否带 `--test-notify`（插件据此安排一次测试推送）
    pub test_notify: bool,
    inner: Arc<ContextInner>,
}

impl PluginContext {
    pub(crate) fn new(
        plugin: &str,
        onebot_config: OneBotConfig,
        test_notify: bool,
        scope: Arc<PluginScope>,
        framework: &Arc<Framework>,
    ) -> PluginContext {
        PluginContext {
            onebot_config,
            test_notify,
            inner: Arc::new(ContextInner {
                plugin: plugin.to_string(),
                scope,
                framework: framework.clone(),
            }),
        }
    }

    /// 本插件的 id（= 目录名与 `disabled_plugins` 里的写法）
    pub fn plugin_id(&self) -> &str {
        &self.inner.plugin
    }

    /// 本插件的后台任务作用域（框架据此在停用时取消任务）
    pub fn scope(&self) -> &Arc<PluginScope> {
        &self.inner.scope
    }

    /// 本插件所属的框架实例：注册表都按它读写
    pub fn framework(&self) -> &Arc<Framework> {
        &self.inner.framework
    }

    // ==================== 目录约定 ====================

    /// 本插件的目录 `plugins/<id>/`（随包资源；框架会自动创建）
    pub fn plugin_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_dir(self.plugin_id())
    }

    /// 本插件的配置目录 `config/<id>/`
    pub fn config_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_config_dir(self.plugin_id())
    }

    /// 本插件的配置文件 `config/<id>/arona.yml`（框架负责生成模板与热重载）
    pub fn config_file(&self) -> PathBuf {
        crate::runtime::paths::plugin_config_file(self.plugin_id())
    }

    /// 本插件的数据目录 `data/<id>/`（数据库、备份等）
    pub fn data_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_data_dir(self.plugin_id())
    }

    /// 本插件的图片目录 `data/<id>/image/`
    pub fn image_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_image_dir(self.plugin_id())
    }

    // ==================== 命令 ====================

    /// 登记一条命令。命令名撞上别家插件时框架只记告警、保留胜出方，不影响其余命令。
    pub fn command(&self, registration: CommandRegistration) {
        self.commands(vec![registration]);
    }

    /// 一批登记命令
    pub fn commands(&self, registrations: Vec<CommandRegistration>) {
        for registration in registrations {
            for problem in self
                .framework()
                .commands()
                .register(self.plugin_id(), registration)
            {
                crate::runtime::log::warning(format!("[{}] {problem}", self.plugin_id()));
            }
        }
    }

    /// 未命中任何命令时的兜底（同优先级下按登记顺序依次调用）
    pub fn fallback(&self, handler: Arc<dyn FallbackHandler>) {
        self.fallback_at(handler, ListenerPriority::default());
    }

    /// 带优先级的兜底：想让别人先看就用 [`ListenerPriority::Lowest`]
    pub fn fallback_at(&self, handler: Arc<dyn FallbackHandler>, priority: ListenerPriority) {
        self.framework()
            .commands()
            .register_fallback(self.plugin_id(), handler, priority);
    }

    /// 本插件名下的命令概览（自绘帮助页用）
    pub fn own_commands(&self) -> Vec<crate::runtime::dispatcher::CommandInfo> {
        self.framework().commands().commands_of(self.plugin_id())
    }

    // ==================== 事件 ====================

    /// 订阅事件（`kinds` 为空表示全部类型）
    pub fn listen(
        &self,
        kinds: &[EventKind],
        priority: ListenerPriority,
        handler: Arc<dyn EventHandler>,
    ) {
        self.framework()
            .hooks()
            .subscribe_at(self.plugin_id(), kinds, priority, handler);
    }

    /// 只订阅消息事件（默认优先级）
    pub fn on_message(&self, handler: Arc<dyn EventHandler>) {
        self.listen(&[EventKind::Message], ListenerPriority::default(), handler);
    }

    /// 按**事件子类**订阅（mirai 的 `GroupMessageEvent`/`NudgeEvent` 这一族）：
    /// 例如 `&[BodyFilter::Notice(NoticeKind::GroupIncrease)]` 只在有人进群时被叫到，
    /// 子类判定由框架完成，插件不必自己从 `ctx.event.raw` 里比字符串。
    pub fn listen_where(
        &self,
        filters: &[BodyFilter],
        priority: ListenerPriority,
        handler: Arc<dyn EventHandler>,
    ) {
        self.framework()
            .hooks()
            .subscribe_where(self.plugin_id(), filters, priority, handler);
    }

    /// 只订阅群消息（默认优先级）
    pub fn on_group_message(&self, handler: Arc<dyn EventHandler>) {
        self.listen_where(
            &[BodyFilter::GroupMessage],
            ListenerPriority::default(),
            handler,
        );
    }

    /// 只订阅私聊消息（默认优先级）
    pub fn on_private_message(&self, handler: Arc<dyn EventHandler>) {
        self.listen_where(
            &[BodyFilter::PrivateMessage],
            ListenerPriority::default(),
            handler,
        );
    }

    pub fn on_notice(&self, handler: Arc<dyn EventHandler>) {
        self.listen(&[EventKind::Notice], ListenerPriority::default(), handler);
    }

    pub fn on_request(&self, handler: Arc<dyn EventHandler>) {
        self.listen(&[EventKind::Request], ListenerPriority::default(), handler);
    }

    pub fn on_meta(&self, handler: Arc<dyn EventHandler>) {
        self.listen(&[EventKind::Meta], ListenerPriority::default(), handler);
    }

    // ==================== 后台任务与定时任务 ====================

    /// 在本插件作用域内跑后台任务：插件停用时自动取消
    pub fn spawn<F>(&self, task: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.inner.scope.spawn(task)
    }

    /// 每天固定小时触发（任务组自动归属本插件，停用即整组取消）
    pub fn daily_job(&self, hour: u32, name: &str, run: crate::quartz::JobFn) {
        self.framework()
            .jobs()
            .create_daily(hour, name, self.plugin_id(), run);
    }

    /// 固定间隔循环任务（首次立即执行）
    pub fn repeat_job(&self, interval_secs: u64, name: &str, run: crate::quartz::JobFn) {
        self.framework()
            .jobs()
            .create_repeat(interval_secs, name, self.plugin_id(), run);
    }

    /// 单次定时任务
    pub fn single_job(&self, ts_ms: i64, name: &str, run: crate::quartz::JobFn) {
        self.framework()
            .jobs()
            .create_single_at(ts_ms, name, self.plugin_id(), run);
    }

    /// 延迟任务（秒）
    pub fn delay_job(&self, delay_secs: u64, name: &str, run: crate::quartz::JobFn) {
        self.framework()
            .jobs()
            .create_delay(delay_secs, name, self.plugin_id(), run);
    }

    /// 取消本插件的一个定时任务
    pub fn remove_job(&self, name: &str) -> bool {
        self.framework().jobs().remove(name)
    }

    // ==================== 配置 ====================

    /// 取自己的一块强类型配置（须已在 install 阶段用 [`PluginRegistrar::config`] 登记）
    pub fn config<T: PluginConfig>(&self, key: &'static str) -> ConfigEntry<T> {
        ConfigEntry::in_store(self.framework().configs(), self.plugin_id(), key)
    }

    // ==================== 服务（插件间能力共享） ====================

    /// 对外公布一项能力，供别的插件按类型取用
    pub fn declare_service<T: Send + Sync + 'static>(&self, service: Arc<T>) {
        self.framework()
            .container()
            .declare(self.plugin_id(), service);
    }

    /// 取别家插件公布的能力；提供方未安装或已停用时返回 None（软依赖降级路径）
    pub fn service<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.framework().container().instance::<T>()
    }

    // ==================== 服务开关表 ====================

    /// 本框架实例的服务开关表（GUI「服务管理」页与 `/服务`、`/紧急停止` 操作的那张表）
    pub fn service_board(&self) -> &Arc<crate::services::ServiceManager> {
        self.framework().services()
    }

    /// 登记一条本插件对外暴露、可单独关停的服务单元。
    /// 条目记在本插件名下：插件被停用时框架一并撤销，GUI 不会留下僵尸行。
    pub fn register_service(&self, info: Arc<crate::services::ServiceInfo>) {
        self.service_board().register(self.plugin_id(), &info);
    }

    // ==================== OneBot ====================

    /// OneBot 动作出口（发/撤/查/管）。每次现取，连接热重载后旧句柄会失效。
    pub fn api(&self) -> OneBotApi {
        OneBotApi::global()
    }
}
