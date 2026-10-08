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
//! 刻意留在进程级的是**真正只有一份、且插件不会经自由函数去读**的进程资源：日志落地
//! 队列与目录约定（`runtime::paths`）。它们是宿主进程的属性，不是插件契约的注册表。
//!
//! 反过来，凡是**插件也会经自由函数碰到**的状态都必须挂在这里，不能另起 `static`：
//! 框架配置的持有者（[`Framework::settings`]）、主动发消息的出口
//! （[`Framework::runtime_services`]）、OneBot 连接池（[`Framework::onebot`]）、聊天记录库
//! （[`Framework::chatlog`]）与远端资料缓存（[`Framework::peers`]）都是这么被收进来的——
//! 动态插件 dll 静态链接了自己那份框架代码，另起的 `static` 在 dll 里是一份空表，只有跟着
//! 实例走才会随 [`Framework::adopt_host`] 一起接到宿主上。
//!
//! 但**挂上实例只是第一道关**。[`HostBridge::framework`](crate::plugin::abi::HostBridge::framework)
//! 交出去的是擦成裸指针的实例地址，插件 dll 里那些 `framework.chatlog().open(..)` 形式的
//! 固有方法会被静态编译进 dll、由 **dll 自己那份代码**去动手：里面是纯 Rust 状态
//! （`Mutex`/`HashMap`/`Arc`）时读写的是同一块系统堆，行为与宿主一致；里面握着 C 语言句柄时
//! （sqlite 的 `sqlite3*`）就是拿宿主的句柄调 dll 自带的另一份引擎——实测直接段错误。
//! 所以 [`Framework::chatlog_api`] 那样再挂一份 `Arc<dyn Trait>` 是第二道关的解法：
//! **vtable 由宿主生成，跨边界跳进来的才是宿主的代码。**
//!
//! 判据由此是两条，缺一不可：① 状态挂在实例上，不另起 `static`；
//! ② 动手的那份代码要么在宿主侧（trait 出口），要么根本不碰跨镜像资源。
//! 另外别以为"反正是纯 Rust 字段就能随便加"：字段序与字段类型本身就是跨边界 ABI，
//! 往中间插一个字段、或改动它任何字段的类型，旧 dll 会按编译期偏移读到错的位置——
//! 那未定义行为的表现是宿主带崩，不是"握手失败"。加字段一律**追加在末尾**并抬
//! `ABI_LAYOUT`（见 [`crate::plugin::abi`]）。
use crate::config::arona::SectionRegistry;
use crate::config::plugin_config::ConfigStore;
use crate::config::settings::Settings;
use crate::container::ServiceContainer;
use crate::onebot::application::OnebotOutlet;
use crate::onebot::hooks::HookRegistry;
use crate::plugin::health::HealthBoard;
use crate::plugin::manager::{LoaderRegistry, PluginManager};
use crate::quartz::Scheduler;
use crate::runtime::chatlog::{ChatLog, ChatLogFace};
use crate::runtime::config::Gating;
use crate::runtime::dispatcher::CommandRegistry;
use crate::runtime::peers::PeerCache;
use crate::runtime::services::RuntimeServices;
use crate::services::ServiceManager;
use std::sync::{Arc, OnceLock};

/// 一套插件契约注册表。字段都是 `Arc`，克隆实例引用即可传给上下文。
/// 进程默认实例（`Framework::global` / `global_arc` 共用同一份）
static GLOBAL: OnceLock<Arc<Framework>> = OnceLock::new();

pub struct Framework {
    gating: Arc<Gating>,
    commands: Arc<CommandRegistry>,
    hooks: Arc<HookRegistry>,
    container: Arc<ServiceContainer>,
    services: Arc<ServiceManager>,
    sections: Arc<SectionRegistry>,
    configs: Arc<ConfigStore>,
    settings: Arc<Settings>,
    runtime_services: Arc<RuntimeServices>,
    onebot: Arc<OnebotOutlet>,
    chatlog: Arc<ChatLog>,
    /// 同一份库的**跨镜像出口**：插件 dll 里那批 `chatlog::` 自由函数只经它调用。
    /// 不能让 dll 直接调 [`ChatLog`] 的固有方法——dll 自带一份 bundled sqlite，
    /// 拿着宿主创建的 `sqlite3*` 句柄调自己那份引擎会当场段错误（见 [`ChatLogFace`]）。
    chatlog_api: Arc<dyn ChatLogFace>,
    jobs: Arc<Scheduler>,
    loaders: Arc<LoaderRegistry>,
    plugins: Arc<PluginManager>,
    health: Arc<HealthBoard>,
    /// 群成员/陌生人资料的按需读缓存。**追加在末尾**：字段序是跨边界 ABI 的一部分。
    peers: Arc<PeerCache>,
    /// 会话展示板（插件上报"这个会话里我订了什么"，GUI 的群管理页读这张表）。
    /// **同样追加在末尾**，理由同上；插件侧不许直接读写它，只经
    /// [`HostBridge::views`](crate::plugin::abi::HostBridge::views) 递交。
    views: Arc<crate::views::ViewBoard>,
}

/// 框架实例的构造选项（对应 mirai 的 `MiraiInstance.new { }`）
#[derive(Clone, Debug)]
pub struct FrameworkOptions {
    /// 同一插件连续 panic 多少次就隔离停用；0 表示只记日志、不停用
    pub panic_disable_threshold: u32,
    /// 没有显式声明 `with_prefix_match` 的命令，是否也允许最短前缀匹配（默认关）
    pub prefix_match_by_default: bool,
    /// 命令名允许的写法前缀。**只影响登记之后的规范化**：不带其中任何一个前缀的命令名，
    /// 登记时会被补上列表里的第一个（`"arona"` → `"/arona"`），于是"arona 好可爱"这种
    /// 日常句子不再被当成命令。设为空列表 = 关掉这条规则（纯聊天式命令）。
    pub command_prefixes: Vec<String>,
}

impl Default for FrameworkOptions {
    fn default() -> Self {
        FrameworkOptions {
            panic_disable_threshold: Framework::DEFAULT_PANIC_THRESHOLD,
            prefix_match_by_default: false,
            command_prefixes: vec![Framework::DEFAULT_COMMAND_PREFIX.to_string()],
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

    /// 设定命令名的写法前缀（第一个用于自动补全；空列表表示不做前缀规范化）
    pub fn command_prefixes(
        mut self,
        prefixes: impl IntoIterator<Item = impl Into<String>>,
    ) -> FrameworkBuilder {
        self.options.command_prefixes = prefixes.into_iter().map(Into::into).collect();
        self
    }

    pub fn build(self) -> Arc<Framework> {
        Framework::with_options(self.options)
    }
}

impl Framework {
    /// 插件连续 panic 多少次就隔离停用
    pub const DEFAULT_PANIC_THRESHOLD: u32 = 5;

    /// 命令名的默认写法前缀
    pub const DEFAULT_COMMAND_PREFIX: &'static str = "/";

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
        let command_prefixes = options.command_prefixes.clone();
        // 同一份库、两个名字：宿主内部按具体类型用，插件侧只能按 trait 对象用
        let chatlog: Arc<ChatLog> = Arc::default();
        let framework = Arc::new(Framework {
            gating: gating.clone(),
            commands: Arc::new(CommandRegistry::new(
                gating.clone(),
                health.clone(),
                prefix_match_by_default,
                command_prefixes,
            )),
            hooks: Arc::new(HookRegistry::new(gating.clone(), health.clone())),
            container: Arc::new(ServiceContainer::new(gating.clone())),
            services: Arc::default(),
            sections: sections.clone(),
            configs: Arc::new(ConfigStore::new(sections, plugins.clone())),
            settings: Arc::default(),
            runtime_services: Arc::default(),
            onebot: Arc::default(),
            chatlog: chatlog.clone(),
            chatlog_api: chatlog,
            jobs: Arc::default(),
            loaders: Arc::default(),
            plugins: plugins.clone(),
            health: health.clone(),
            peers: Arc::default(),
            views: Arc::default(),
        });
        // PluginManager 与 HealthBoard 要按归属回收资源、隔离停用插件，得知道整套注册表；
        // 它们先于 Framework 造出来，所以构造完再回填一次。
        plugins.attach(&framework);
        health.attach(&framework);
        // 钩子表同理：投递时要给插件上下文带上**这一套**实例的连接与聊天记录库
        framework.hooks().attach(&framework);
        // 定时任务表同理：处理器 panic 时要记到归属插件名下，而不是让调度循环静默消失
        framework.jobs().attach(&framework);
        // arona.yml 的持有者同理：重载之后要把配置落到**它所属这一套**门控/健康度上
        framework.settings().attach(&framework);
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
        GLOBAL.get_or_init(Framework::new)
    }

    /// 进程默认实例的 owned 句柄：要给别的对象长期持有、又不想拖 `'static` 生命周期时用
    pub fn global_arc() -> Arc<Framework> {
        GLOBAL.get_or_init(Framework::new).clone()
    }

    /// 把一个外部实例认领成**本份代码里的**进程默认实例。
    ///
    /// 动态插件 dll 静态链接了整整一份框架代码，`quartz::exists`、`container::instance()`
    /// 这类自由函数读的是 dll 自己的 `GLOBAL`——那套表是空的，写进去的东西宿主永远看不见。
    /// 装载器在造实例之前调它，把宿主的实例接过来，此后插件里的自由函数落的都是宿主真正
    /// 在跑的那套注册表。只能认领一次（`GLOBAL` 是 `OnceLock`），且必须早于任何插件代码。
    ///
    /// # Safety
    /// `host` 必须指向一个由宿主 `Arc` 持有、且在进程结束前不会被释放的 `Framework`。
    pub unsafe fn adopt_host(host: *const std::ffi::c_void) -> bool {
        if host.is_null() {
            return false;
        }
        let host = host.cast::<Framework>();
        // 指的已经是本份代码的默认实例：接管等于没改变，算成功（宿主的单测走这条）
        let current = GLOBAL.get().map_or(std::ptr::null(), Arc::as_ptr);
        if std::ptr::eq(host, current) {
            return true;
        }
        // 计数 +1 后包成 Arc 交给 GLOBAL：宿主那份永不释放，这一份也就不必归还
        unsafe { Arc::increment_strong_count(host) };
        GLOBAL.set(unsafe { Arc::from_raw(host) }).is_ok()
    }

    /// 本实例是否就是进程默认实例（只有它背后那份 arona.yml 是真实配置文件）
    fn is_process_default(&self) -> bool {
        std::ptr::eq(self, Framework::global())
    }

    /// 本实例当前的选项快照。
    /// 值是从各注册表的活状态读出来的——构造完之后再改（arona.yml 热重载）也不会读到旧副本。
    pub fn options(&self) -> FrameworkOptions {
        FrameworkOptions {
            panic_disable_threshold: self.health.threshold(),
            prefix_match_by_default: self.commands.prefix_match_by_default(),
            command_prefixes: self.commands.command_prefixes(),
        }
    }

    /// 设定 panic 隔离阈值（0 表示只记日志、不因 panic 停用插件）
    pub fn set_panic_disable_threshold(&self, threshold: u32) {
        self.health.set_threshold(threshold);
    }

    /// 设定「未显式声明的命令也允许最短前缀匹配」
    pub fn set_prefix_match_by_default(&self, enabled: bool) {
        self.commands.set_prefix_match_by_default(enabled);
    }

    /// panic 记账与隔离停用判定
    pub fn health(&self) -> &Arc<HealthBoard> {
        &self.health
    }

    /// 群成员与陌生人资料的按需读缓存（对应 mirai 的 `MemberRead` 那一层）
    pub fn peers(&self) -> &Arc<PeerCache> {
        &self.peers
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
        // 落盘只在进程默认实例上做：隔离实例那份 Settings 没绑定任何真实文件，不该去动磁盘
        if self.is_process_default() {
            if let Err(problem) = crate::config::settings::set_plugin_enabled(plugin, false) {
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

    /// 框架自己那份 `config/arona.yml` 的持有者（加载、热重载监听、`/config` 读写）
    pub fn settings(&self) -> &Arc<Settings> {
        &self.settings
    }

    /// 运行期服务引用：数据根目录与主动发消息的出口
    pub fn runtime_services(&self) -> &Arc<RuntimeServices> {
        &self.runtime_services
    }

    /// OneBot 应用与连接池的持有者：所有 OneBot 动作（含插件侧）都从这里取当前可用连接
    pub fn onebot(&self) -> &Arc<OnebotOutlet> {
        &self.onebot
    }

    /// 聊天记录库的持有者：记账、引用还原与按存储 id 撤回的依据
    /// （宿主内部用这一份具体类型）
    pub fn chatlog(&self) -> &Arc<ChatLog> {
        &self.chatlog
    }

    /// 同一份库的跨镜像出口：插件 dll 侧的 `arona::runtime::chatlog::*` 只走它，
    /// 好让 sqlite 连接始终由宿主那份引擎操作（原因见 [`ChatLogFace`]）
    pub fn chatlog_api(&self) -> &Arc<dyn ChatLogFace> {
        &self.chatlog_api
    }

    /// 定时任务表
    pub fn jobs(&self) -> &Arc<Scheduler> {
        &self.jobs
    }

    /// 插件装载器登记表
    pub fn loaders(&self) -> &Arc<LoaderRegistry> {
        &self.loaders
    }

    /// 会话展示板（插件上报"这个会话里我订了什么"，GUI 的群管理页读这一张表）。
    /// 插件侧不许直接读写它——那份表在 dll 里会按自己编译期的偏移被操作，见 [`crate::views`]。
    pub fn views(&self) -> &Arc<crate::views::ViewBoard> {
        &self.views
    }

    /// 插件表与生命周期编排
    pub fn plugins(&self) -> &Arc<PluginManager> {
        &self.plugins
    }
}
