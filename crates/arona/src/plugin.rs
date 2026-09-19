//! 插件接口：功能部分以插件形式插入框架。
//!
//! 框架只负责 OneBot 连接、管理面板、群授权/黑名单与命令分发骨架；
//! 具体功能（抽卡/活动/攻略……）由插件提供。开发插件时把本 crate 作为依赖引入，
//! 用 host（或直接 `cargo run`）即可开发与运行调试。
//!
//! 规范：每个插件必须提供 [`PluginMeta`]（id + name + version 为硬性要求）。
//! 落盘位置由框架统一规定（`plugins/<id>/`、`config/<id>/arona.yml`、`data/<id>/`），
//! 插件通过 [`PluginContext`] 上的目录接口取用，不要自己拼路径。
use crate::config::onebot::OneBotConfig;
use crate::runtime::dispatcher::SimpleCommandDispatcher;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

/// 插件元信息（规范要求：必须有 id、name 与 version）
#[derive(Clone, Debug)]
pub struct PluginMeta {
    /// 短横线/小写的稳定标识（如 `bluearchive`）：目录名与配置里的键都用它，
    /// 即 `plugins/<id>/`、`config/<id>/arona.yml`、`data/<id>/`、`disabled_plugins: [<id>]`。
    /// 改名等于换一份用户数据，所以定下来就别动。
    pub id: &'static str,
    /// 展示名（GUI 列表、日志里用）
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
}

/// 传给插件 configure 阶段的上下文
#[derive(Clone)]
pub struct PluginContext {
    /// 已加载的 OneBot 协议配置（构建命令分发器时需要 self_id 等信息）
    pub onebot_config: OneBotConfig,
    /// 命令行是否带 `--test-notify`（插件据此安排一次测试推送）
    pub test_notify: bool,
    /// 本上下文属于哪个插件的 id（框架填充，插件不必关心）
    plugin: String,
}

impl PluginContext {
    /// 框架内部构造：`plugin` 由 [`configure_all`] 按轮到的插件逐个填上，
    /// 插件不需要（也无法）自己伪造一份上下文。
    pub(crate) fn new(onebot_config: OneBotConfig, test_notify: bool) -> PluginContext {
        PluginContext {
            onebot_config,
            test_notify,
            plugin: String::new(),
        }
    }

    fn for_plugin(&self, id: &str) -> PluginContext {
        let mut context = self.clone();
        context.plugin = id.to_string();
        context
    }

    /// 本插件的 id（= 目录名与 `disabled_plugins` 里的写法）
    pub fn plugin_id(&self) -> &str {
        &self.plugin
    }

    /// 本插件的目录 `plugins/<id>/`（随包资源；框架会自动创建）
    pub fn plugin_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_dir(&self.plugin)
    }

    /// 本插件的配置目录 `config/<id>/`
    pub fn config_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_config_dir(&self.plugin)
    }

    /// 本插件的配置文件 `config/<id>/arona.yml`（框架负责生成模板与热重载）
    pub fn config_file(&self) -> PathBuf {
        crate::runtime::paths::plugin_config_file(&self.plugin)
    }

    /// 本插件的数据目录 `data/<id>/`（数据库、备份等）
    pub fn data_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_data_dir(&self.plugin)
    }

    /// 本插件的图片目录 `data/<id>/image/`
    pub fn image_dir(&self) -> PathBuf {
        crate::runtime::paths::plugin_image_dir(&self.plugin)
    }

    /// 登记本插件的命令分发器。框架会记下归属插件，从而在该插件被全局禁用时
    /// 把它名下的命令一并停掉（不必等重启）。
    pub fn set_dispatcher(&self, dispatcher: Arc<SimpleCommandDispatcher>) {
        *DISPATCHER.write().unwrap() = Some((self.plugin.clone(), dispatcher));
    }
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

    /// 装配阶段：构建命令分发器并 `ctx.set_dispatcher(..)`。在两份配置加载之后、连接启动之前调用。
    /// 插件被禁用时框架不会调用这里。
    fn configure(&self, _ctx: &PluginContext) -> Result<(), String> {
        Ok(())
    }

    /// 启动阶段：打开数据库、拉起数据预热与定时推送等后台任务。
    fn start(&self) {}

    /// arona.yml 热重载（推送小时变更等）后回调。
    fn on_config_reload(&self) {}

    /// 退出阶段：关闭数据库、释放资源。
    /// 运行中被切换为「禁用」时框架也会调用这里，所以**取消定时任务要写在 stop 里**，
    /// 不然禁用后定时推送照旧发得出去。
    fn stop(&self) {}
}

static PLUGINS: RwLock<Vec<Arc<dyn AronaPlugin>>> = RwLock::new(Vec::new());
/// 插件登记的分发器 + 登记它的插件名
static DISPATCHER: RwLock<Option<(String, Arc<SimpleCommandDispatcher>)>> = RwLock::new(None);

/// 已经装配并启动过的插件名（= 当前处于启用状态的插件）。
/// GUI/配置文件切换「禁用插件」后由 [`sync_enabled_state`] 做差集，决定该补 start 还是该 stop。
static ACTIVE: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// 装配阶段用过的上下文快照：运行中新启用某个插件时，用它补跑 configure
static LAST_CTX: RwLock<Option<PluginContext>> = RwLock::new(None);

/// 注册一个插件（host 在调用 [`crate::run`] 之前完成）
pub fn register(plugin: Arc<dyn AronaPlugin>) {
    PLUGINS.write().unwrap().push(plugin);
}

fn plugins() -> Vec<Arc<dyn AronaPlugin>> {
    PLUGINS.read().unwrap().clone()
}

/// 已注册插件的元信息列表（GUI「插件管理」页/诊断用）
pub fn metas() -> Vec<PluginMeta> {
    plugins().iter().map(|plugin| plugin.meta()).collect()
}

/// 某个插件的元信息（按 id 查，GUI「插件管理」页用）
pub fn meta_of(id: &str) -> Option<PluginMeta> {
    plugins()
        .into_iter()
        .find(|plugin| plugin.meta().id == id)
        .map(|plugin| plugin.meta())
}

pub fn install_all() -> Result<(), String> {
    // install 阶段不过滤禁用插件：功能清单与配置区必须始终登记，
    // 否则 GUI 列不出被禁用的插件、配置模板也会丢掉它的块。
    for plugin in plugins() {
        let meta = plugin.meta();
        prepare_plugin_home(&meta);
        plugin.install()?;
    }
    Ok(())
}

/// 建好插件的三件套目录并写下 `plugins/<id>/plugin.yml`：
/// 静态编译模式下插件不单独出包，这个目录就是它在磁盘上的“存在证明”，
/// 用户从中能看出程序里装了谁、它的配置与数据放在哪。
fn prepare_plugin_home(meta: &PluginMeta) {
    let dir = crate::runtime::paths::plugin_dir(meta.id);
    let file = dir.join("plugin.yml");
    let content = format!(
        "# 本文件由框架启动时自动生成，描述编译进本程序的插件。\n\
         # 插件目录只放随包资源；配置与数据按统一约定各归其位：\n\
         id: {}\n\
         name: {}\n\
         version: {}\n\
         description: {}\n\
         config: config/{}/arona.yml\n\
         data: data/{}\n\
         image: data/{}/image\n",
        meta.id, meta.name, meta.version, meta.description, meta.id, meta.id, meta.id
    );
    let unchanged = std::fs::read_to_string(&file)
        .map(|text| text == content)
        .unwrap_or(false);
    if !unchanged {
        if let Err(err) = std::fs::write(&file, content) {
            crate::runtime::log::warning(format!("写入插件清单失败: {} —— {err}", file.display()));
        }
    }
    // 目录先建出来，插件不写任何文件时也保持可见
    crate::runtime::paths::plugin_config_dir(meta.id);
    crate::runtime::paths::plugin_data_dir(meta.id);
}

/// 插件是否处于启用状态（arona.yml 的 disabled_plugins，按 id 匹配）
fn is_enabled(id: &str) -> bool {
    crate::runtime::config::plugin_enabled(id)
}

/// 装配阶段：只为启用的插件构建命令分发器；被禁用的插件整体跳过（不路由的前提）
pub fn configure_all(ctx: &PluginContext) -> Result<(), String> {
    *LAST_CTX.write().unwrap() = Some(ctx.clone());
    // 先攒出本轮装配成功的名单再一次性写入：configure 里插件会回调 set_dispatcher，
    // 持着 ACTIVE 写锁跨插件代码会自死锁。
    let mut started: Vec<String> = Vec::new();
    for plugin in plugins() {
        let id = plugin.meta().id;
        if !is_enabled(id) {
            crate::runtime::log::info(format!("插件已禁用，跳过装配: {}", plugin.meta().name));
            continue;
        }
        plugin.configure(&ctx.for_plugin(id))?;
        started.push(id.to_string());
    }
    {
        let mut active = ACTIVE.write().unwrap();
        for id in started {
            if !active.iter().any(|item| *item == id) {
                active.push(id);
            }
        }
    }
    Ok(())
}

/// 启动阶段：只拉起已装配（即启用）的插件的后台任务
pub fn start_all() {
    for plugin in plugins() {
        let id = plugin.meta().id;
        if ACTIVE.read().unwrap().iter().any(|item| item == id) {
            plugin.start();
        }
    }
}

/// 按当前「禁用插件」名单协调插件生命周期：
/// 新禁用的调 [`AronaPlugin::stop`]（插件在这里取消自己的定时任务），
/// 新启用的补跑 configure + start。返回状态发生变化的插件 id。
///
/// arona.yml 热重载与 GUI 开关都走这里；启动阶段不需要调用（configure_all 已按名单跳过）。
pub fn sync_enabled_state() -> Vec<String> {
    let mut changed = Vec::new();
    for plugin in plugins() {
        let id = plugin.meta().id.to_string();
        let name = plugin.meta().name;
        let active = ACTIVE.read().unwrap().iter().any(|item| *item == id);
        let want = is_enabled(&id);
        if active == want {
            continue;
        }
        if want {
            let Some(context) = LAST_CTX.read().unwrap().clone() else {
                crate::runtime::log::warning(format!("插件 {name} 无法启用：OneBot 配置尚未加载"));
                continue;
            };
            // 装配失败就保持禁用态，不让半个插件跑起来
            if let Err(err) = plugin.configure(&context.for_plugin(&id)) {
                crate::runtime::log::error(format!("插件 {name} 启用时装配失败: {err}"));
                continue;
            }
            ACTIVE.write().unwrap().push(id.clone());
            plugin.start();
            crate::runtime::log::info(format!("插件已启用: {name}"));
        } else {
            plugin.stop();
            ACTIVE.write().unwrap().retain(|item| *item != id);
            crate::runtime::log::info(format!("插件已禁用: {name}"));
        }
        changed.push(id);
    }
    changed
}

/// 当前处于启用状态的插件 id
pub fn active_plugins() -> Vec<String> {
    ACTIVE.read().unwrap().clone()
}

pub fn notify_config_reloaded() {
    for plugin in plugins() {
        plugin.on_config_reload();
    }
}

/// 只通知某个插件（它的 config/<id>/arona.yml 变了）
pub fn notify_config_reloaded_of(plugin_id: &str) {
    for plugin in plugins() {
        if plugin.meta().id == plugin_id {
            plugin.on_config_reload();
        }
    }
}

pub fn stop_all() {
    for plugin in plugins() {
        plugin.stop();
    }
    ACTIVE.write().unwrap().clear();
}

/// 框架读取插件登记的分发器来构造业务处理器；未登记时返回 None（框架用空表兜底）
pub fn dispatcher() -> Option<Arc<SimpleCommandDispatcher>> {
    DISPATCHER
        .read()
        .unwrap()
        .as_ref()
        .map(|(_, dispatcher)| dispatcher.clone())
}

/// 分发器对某个群是否生效：登记它的插件被全局禁用、或在该群被禁用，名下命令都不再路由。
/// group_id 为 None（私聊）时只看全局开关。
pub fn dispatcher_active_in_group(group_id: Option<i64>) -> bool {
    match DISPATCHER.read().unwrap().as_ref() {
        None => true,
        Some((plugin, _)) => {
            crate::runtime::config::plugin_enabled(plugin)
                && crate::runtime::config::plugin_enabled_in_group(plugin, group_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::arona::GroupSetting;
    use crate::runtime::dispatcher::{CommandContext, CommandRegistration, handler};
    use crate::runtime::message::{
        BoxFuture, MessageReceipt, MessageSender, MessageTarget, OutgoingMessage,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 分发器归属是进程级全局：用例必须串行
    static DISPATCHER_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct NullSender;

    impl MessageSender for NullSender {
        fn send<'a>(
            &'a self,
            _target: MessageTarget,
            _message: OutgoingMessage,
        ) -> BoxFuture<'a, MessageReceipt> {
            Box::pin(async { MessageReceipt { message_id: None } })
        }
    }

    fn context(group_id: Option<i64>) -> Arc<CommandContext> {
        Arc::new(CommandContext {
            user_id: 100,
            group_id,
            text: "/门控".to_string(),
            sender_name: None,
            is_admin: false,
            sender: Arc::new(NullSender),
        })
    }

    /// 分群/全局禁用插件对命令路由的兜底：命令没绑定功能 key 时也要停得下来
    #[tokio::test(flavor = "current_thread")]
    async fn dispatcher_gate_follows_plugin_switches() {
        let _serial = DISPATCHER_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let plugin = "DispatcherGateTestPlugin";
        let ran = Arc::new(AtomicBool::new(false));
        let dispatcher = Arc::new(SimpleCommandDispatcher::new(
            vec![CommandRegistration::new(
                vec!["/门控".to_string()],
                "门控自检",
                {
                    let ran = ran.clone();
                    handler(move |_ctx: Arc<CommandContext>, _args: Vec<String>| {
                        let ran = ran.clone();
                        async move {
                            ran.store(true, Ordering::SeqCst);
                            None
                        }
                    })
                },
            )],
            None,
        ));
        let onebot = OneBotConfig {
            self_id: 1,
            nickname: "Arona".to_string(),
            send_image_as_file: false,
            connections: BTreeMap::new(),
        };
        PluginContext::new(onebot, false)
            .for_plugin(plugin)
            .set_dispatcher(dispatcher.clone());

        let mut settings = BTreeMap::new();
        settings.insert(
            "1".to_string(),
            GroupSetting {
                disabled_plugins: vec![plugin.to_string()],
                ..Default::default()
            },
        );
        crate::runtime::config::set_group_settings(settings);

        ran.store(false, Ordering::SeqCst);
        assert!(
            !dispatcher_active_in_group(Some(1)),
            "该群已禁用插件，不应路由"
        );
        assert!(dispatcher_active_in_group(Some(2)), "未设置的群应保持可用");
        assert!(
            !dispatcher.dispatch(context(Some(1))).await,
            "禁用群里命令不该被处理"
        );
        assert!(!ran.load(Ordering::SeqCst), "禁用群里命令处理器不该执行");

        assert!(
            dispatcher.dispatch(context(Some(2))).await,
            "未禁用的群应正常命中命令"
        );
        assert!(ran.load(Ordering::SeqCst), "处理器未执行");

        // 全局禁用：连私聊（无群号）也不路由
        ran.store(false, Ordering::SeqCst);
        crate::runtime::config::set_disabled_plugins(vec![plugin.to_string()]);
        assert!(!dispatcher_active_in_group(None));
        assert!(!dispatcher.dispatch(context(None)).await);
        assert!(!ran.load(Ordering::SeqCst), "全局禁用后处理器不该执行");

        *DISPATCHER.write().unwrap() = None;
        crate::runtime::config::set_group_settings(BTreeMap::new());
        crate::runtime::config::set_disabled_plugins(Vec::new());
    }
}
