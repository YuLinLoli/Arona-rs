//! 插件接口：功能部分以插件形式插入框架。
//!
//! 框架只负责 OneBot 连接、管理面板、群授权/黑名单与命令分发骨架；
//! 具体功能（抽卡/活动/攻略……）由插件提供。开发插件时把本 crate 作为依赖引入，
//! 用 host（或直接 `cargo run`）即可开发与运行调试。
//!
//! 规范：每个插件必须提供 [`PluginMeta`]（name + version 为硬性要求）。
use crate::config::onebot::OneBotConfig;
use crate::runtime::dispatcher::SimpleCommandDispatcher;
use std::sync::Arc;
use std::sync::RwLock;

/// 插件元信息（规范要求：必须有 name 与 version）
#[derive(Clone)]
pub struct PluginMeta {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
}

/// 传给插件 configure 阶段的上下文
pub struct PluginContext {
    /// 已加载的 OneBot 协议配置（构建命令分发器时需要 self_id 等信息）
    pub onebot_config: OneBotConfig,
    /// 命令行是否带 `--test-notify`（插件据此安排一次测试推送）
    pub test_notify: bool,
}

/// 插件生命周期。所有方法都有默认实现，插件只需实现自己关心的阶段。
pub trait AronaPlugin: Send + Sync + 'static {
    /// 元信息（name、version 必填）
    fn meta(&self) -> PluginMeta;

    /// 登记阶段：注册功能开关与服务。在加载 arona.yml 之前调用，
    /// 这样生成的配置模板注释里能带上完整功能清单。
    fn install(&self) -> Result<(), String> {
        Ok(())
    }

    /// 装配阶段：构建命令分发器并 [`set_dispatcher`]。在两份配置加载之后、连接启动之前调用。
    fn configure(&self, _ctx: &PluginContext) -> Result<(), String> {
        Ok(())
    }

    /// 启动阶段：打开数据库、拉起数据预热与定时推送等后台任务。
    fn start(&self) {}

    /// arona.yml 热重载（推送小时变更等）后回调。
    fn on_config_reload(&self) {}

    /// 退出阶段：关闭数据库、释放资源。
    fn stop(&self) {}
}

static PLUGINS: RwLock<Vec<Arc<dyn AronaPlugin>>> = RwLock::new(Vec::new());
static DISPATCHER: RwLock<Option<Arc<SimpleCommandDispatcher>>> = RwLock::new(None);

/// 注册一个插件（host 在调用 [`crate::run`] 之前完成）
pub fn register(plugin: Arc<dyn AronaPlugin>) {
    PLUGINS.write().unwrap().push(plugin);
}

fn plugins() -> Vec<Arc<dyn AronaPlugin>> {
    PLUGINS.read().unwrap().clone()
}

/// 已注册插件的元信息列表（GUI「关于」页/诊断用）
pub fn metas() -> Vec<PluginMeta> {
    plugins().iter().map(|plugin| plugin.meta()).collect()
}

pub fn install_all() -> Result<(), String> {
    for plugin in plugins() {
        plugin.install()?;
    }
    Ok(())
}

pub fn configure_all(ctx: &PluginContext) -> Result<(), String> {
    for plugin in plugins() {
        plugin.configure(ctx)?;
    }
    Ok(())
}

pub fn start_all() {
    for plugin in plugins() {
        plugin.start();
    }
}

pub fn notify_config_reloaded() {
    for plugin in plugins() {
        plugin.on_config_reload();
    }
}

pub fn stop_all() {
    for plugin in plugins() {
        plugin.stop();
    }
}

/// 插件在 configure 阶段登记自己要用的命令分发器
pub fn set_dispatcher(dispatcher: Arc<SimpleCommandDispatcher>) {
    *DISPATCHER.write().unwrap() = Some(dispatcher);
}

/// 框架读取插件登记的分发器来构造业务处理器；未登记时返回 None（框架用空表兜底）
pub fn dispatcher() -> Option<Arc<SimpleCommandDispatcher>> {
    DISPATCHER.read().unwrap().clone()
}
