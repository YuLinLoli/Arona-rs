# Arona 插件开发指南

本项目拆成「框架 + 功能插件」两层。框架只提供 **OneBot 连接（含 v11 全量动作接口与事件钩子）、管理面板(GUI)、群授权/黑名单、插件与功能的启停门控、命令分发骨架** 与生命周期编排；具体玩法一律以**插件**形式实现并插入框架。

插件契约对齐 [mirai](https://docs.mirai.com) 的插件模型（`PluginManager` / `plugin.yml` / `ApiVersion` / `EventPriority` / `CommandManager` / `CoroutineScope` / `DiContainer` / `ConfigKey`），装载方式也一样：**用户在启动前把插件包放进 `plugins/` 目录，框架启动时扫描、握手、装配**。差别只在包的形态——mirai 是 jar 由 ClassLoader 隔离加载，这里是 `cdylib` 编出的 dll 直接映射进宿主进程，因此对构建工具链的要求比 mirai 严格（§22 是硬性发布要求，务必读）。

**想先跑起来再看文档：`plugins/hello/` 是一份完整的示例插件**，编译出 `hello_plugin.dll` 丢进 `plugins/` 就能装载；
命令、事件钩子、出站钩子、类型化配置、每日任务各演示了一发，本文讲的接口基本都能在那儿对着读。

## 1. 目录与版本

```
Cargo.toml                 # 虚拟 workspace（resolver=2）
crates/arona/              # 框架库 crate：包名 arona-rs（库名仍是 arona，见 §15），version=1.1.0
crates/arona-host/         # 宿主可执行：name=arona-host, version=1.1.0, 产物 bin=arona-rs
plugins/hello/             # 示例插件：name=hello-plugin, crate-type=["cdylib", "rlib"]
```

框架仓库里不含任何功能插件（示例除外），宿主也不链接插件：功能插件是**各自独立的 crate**，
编译出 `xxx_plugin.dll` 后由用户放进运行目录的 `plugins/`，框架启动时扫描装载（§22）。

版本约定：框架与 host 同为 `1.1.0`，插件版本号各自演进。
**插件接口的破坏性改动抬 `arona::plugin::FRAMEWORK_API_VERSION`**（与 crate 版本号无关），见 §6；
**插件会读的那个类型换了布局**（`Framework`、`CommandContext`、`EventBody`、`BodyFilter`、`OneBotApi`
这些，加字段/加变体/改字段类型）另抬 `arona::plugin::abi::ABI_LAYOUT`（当前 `5`），见 §22。

运行期的落盘目录由框架统一规定（见 §7），插件不自己挑地方：

```
<运行目录>/
  config/arona.yml          框架配置：groups / managers / global_blacklist / group_settings / disabled_plugins / framework
  config/onebot.yml         OneBot 连接配置
  config/<插件>/arona.yml   该插件自己的配置（如 config/hello/arona.yml）
  data/<插件>/…             该插件自己的数据（如 data/hello/image、arona.db、backups）
  logs/                     按天滚动的日志
  plugins/                  用户放的插件 dll —— 框架扫描这一层以及它的每个一级子目录
  plugins/<插件>/           框架按 id 渲染的 plugin.yml；插件的随包资源也放这里
```

## 2. 职责边界（依赖方向）

**框架 `arona` 绝不依赖任何插件**，插件单向依赖框架；宿主连插件长什么样都不知道，
只在运行期按 ABI 约定的四个导出符号跟 dll 打交道（§22）：

```
arona-host ──depends──▶ arona（框架）──启动时扫描──▶ plugins/*.dll ──depends──▶ arona（框架）
```

框架反向调用插件只能通过 `arona::plugin`（契约层：注册表 + 生命周期编排 + 目录/登记交接面），加上
`arona::onebot::hooks`（事件订阅）、`arona::runtime::dispatcher`（命令表）、`arona::container`（服务）、
`arona::config`（配置）与 `arona::quartz`（定时任务）。任何"框架或宿主里 `use 某插件::…`"都是设计违规——
框架仓库里连一个具体插件的名字都不该出现，否则就成了"谁写插件都要往框架里填一笔"。

### 注册表挂在哪：`Framework` 实例

上面那些表不是各模块各自抱一个 `static`，而是统一由 `arona::framework::Framework` 持有（对应 mirai 的 `MiraiInstance`）：

| 访问器 | 表 | mirai 里的对应物 |
| --- | --- | --- |
| `gating()` | 功能清单 + 停用/黑名单门控 | 插件启停判定 |
| `commands()` | 命令表（含兜底处理器） | `CommandManager` |
| `hooks()` | 事件钩子表 | `Listener` 注册表 |
| `container()` | 服务容器（按类型共享能力） | `DiContainer` |
| `services()` | 服务开关表（可单独关停的功能单元，GUI「服务管理」页读它） | — |
| `sections()` | 插件配置区登记表 | `ConfigKey` 声明 |
| `configs()` | 插件配置文件的加载/待补状态 | `ConfigManager` |
| `jobs()` | 定时任务表 | — |
| `loaders()` / `plugins()` | 装载器登记表与插件表 | `PluginManager` |
| `health()` | panic 隔离面板（按插件计失败次数、达阈值停用） | `broadcastAndDumpInterceptedExceptions` |

两轨入口：

- `Framework::global()` —— 进程默认实例。`arona::container::instance`、`arona::runtime::dispatcher::register`、
  GUI 用的 `runtime::config::*` 等自由函数统统转发到它，所以既有调用方（含 GUI）一行都不用改。
- `Framework::new()` —— 一套全空的隔离实例，返回 `Arc<Framework>`。测试与将来的多实例宿主用它；
  `PluginManager` / `PluginContext` 拿的是实例引用，不碰进程级状态。
- `Framework::builder()` —— 带构造选项的入口（`Framework::builder().panic_disable_threshold(3).build()`），
  对应 mirai 的 `MiraiInstance.new { .. }`。选项见 §20。

`PluginRegistrar::framework()` 与 `PluginContext::framework()` 把实例交给插件，插件登记出去的每一样东西
因此天然知道自己属于哪张表、归属哪家插件——按归属回收才有依据（§13）。

**刻意留在进程级的**：日志（`runtime::log`）、目录约定（`runtime::paths`）、OneBot 连接
（`onebot::application` / `onebot::connection`）、控制台（`runtime::console`）、运行期服务引用
（`runtime::services`：data root 与消息发送器）、框架自身 `config/arona.yml` 的持有者
（`config::settings`）、软渲染兜底（`runtime::softgl`）。它们是「一个进程只有一份」的宿主资源，
不是插件契约的注册表；拆成实例只会让 GUI 和连接层多出一堆无意义的参数。

## 3. 与 mirai 的对应关系

| mirai | Arona | 落在哪 |
| --- | --- | --- |
| `MiraiInstance.reference()` 拿到的那套全局注册表 | `arona::framework::Framework::global()`（进程默认实例）/ `Framework::new()`（隔离实例），持有下面所有表 | `framework.rs` |
| `PluginManager.loadPlugin / enablePlugin / disablePlugin` | `arona::plugin::PluginManager` + `install_all` / `configure_all` / `start_all` / `disable` / `stop_all` | `crates/arona/src/plugin/manager.rs` |
| jar 内 `plugin.yml`（`PluginDescriptor`） | `PluginMeta` → 框架渲染成 `plugins/<id>/plugin.yml` | `plugin/description.rs` |
| `ApiVersion.isCompatibleWith`（主版本相等 + 框架次版本不低于要求） | `ApiVersion::satisfies` + `check_api` 握手 | 同上 + `manager.rs` |
| `plugin.depend` / `softDepend` | `PluginMeta::depends` / `soft_depends`，`PluginManager::ordered()` 拓扑排序 | `manager.rs` |
| `CommandManager.registerCommand` | `ctx.command(..)` / `ctx.commands(..)`，按框架实例持有、条目带插件归属 | `runtime/dispatcher.rs` |
| `commandRegistry { string("...") int("分钟") optional() }`（`ArgParserCombinationDSL`） | `CommandRegistration::with_args(vec![arg::i64("分钟").optional() ..])`，缺参/类型错的用法回显由框架给 | `runtime/args.rs` |
| `SimpleCommandDispatcher.shortestPrefixMatch` | `CommandRegistration::with_prefix_match(true)` + `FrameworkOptions::prefix_match_by_default`（最短前缀唯一即命中） | 同上 + `framework.rs` |
| `PermissionService` / `MiraiPermission` | `Permission::{Anyone,GroupAdmin,GroupOwner}` + `GroupRole`（登记时挂在命令上：`CommandRegistration::with_permission(..)`，身份优先读 `sender.role`，回查带缓存） | `runtime/dispatcher.rs` |
| `MiraiInstance.new { .. }`（构造选项） | `Framework::builder().panic_disable_threshold(..).build()` / `Framework::with_options(..)` | `framework.rs` |
| `broadcastAndDumpInterceptedExceptions`（插件异常不冒泡到宿主） | 框架调用的插件入口全被兜住：命令/兜底、入站与出站钩子过 `guarded()`，生命周期回调与定时任务各自 `catch_unwind`，三处记账在同一张健康度面板上；连续 panic 达阈值自动停用该插件 | `plugin/health.rs` |
| `GroupMessageEvent` / `FriendMessageEvent` / `NudgedEvent` 事件族 | `EventBody`（强类型事件体，notice/request/meta 另带 `NoticeInfo`/`RequestInfo`/`MetaInfo` 细节：动作、操作者、禁言时长、处理凭据、生命周期状态）+ `BodyFilter` 子类型订阅（私聊分好友/临时会话，元事件分连接状态）：`ctx.on_group_message(..)`、`ctx.listen_where(&[BodyFilter::..], ..)` | `onebot/hooks.rs` |
| `MessageChain` / `Element`（At、Image、Source、QuoteReply、Face、FlashMessage…） | `MessageSegment` 15 段（文本/@/@全体/引用/图片/表情/语音/视频/文件/戳一戳/位置/json/xml/合并转发/未知段原样保留），收发双向同一套类型 | §19 |
| `AbstractMessage.source()` / `quoteReply`（引用的那条消息还能不能拿到） | 框架全包：`CommandContext::{quoted, segments, message_id, time}` 让引用对命令可见，协议层引用不到的旧消息由框架的聊天记录库还原（`context.reply_with_quote`），撤回按存下来的 id 换算（`context.recall`） | `runtime/dispatcher.rs` + `runtime/chatlog.rs`（§21） |
| `Bot` / `Group` / `Member` / `Friend` / `Temp` / `MessageSource`（先拿对象，再往对象上说话） | `arona::bot::{Bot, Group, Member, Friend, MessageSource}`：`ctx.bot().group(123).send_text(..)`、`member.mute(600).await`、`member.send_temp(..)`、`ctx.source()?.recall().await`。句柄只装一个所属 `Framework` 实例的引用，不缓存连接也不缓存资料 | `bot/mod.rs`（§14.2） |
| `EventPriority`（Monitor→High→Normal→Low→Lowest） | `ListenerPriority` / `CommandPriority`（同一份 `runtime::priority::Priority`） | `runtime/priority.rs` |
| `event.intercept()` | `HookFlow::Handled` | `onebot/hooks.rs` |
| `MessagePreSendEvent`（发出去之前改内容 / 整条拦下） | `OutboundContext`：`ctx.on_outgoing(..)` 订阅，`rewrite(..)` 改写、`cancel()` 拦停；挂在统一发送口上，合并转发拆分与控制台打印之前 | 同上 + `onebot/message_sender.rs` |
| `plugin.instance.coroutineScope.launch` | `ctx.spawn(..)` / `PluginScope` | `plugin/scope.rs` |
| `DiContainer.declare` / `instance<T>()` | `ctx.declare_service::<T>(..)` / `ctx.service::<T>()` | `container.rs` |
| —（原版 Arona 的 `StandaloneServiceInfo` 表） | `arona::services::ServiceManager`：`ctx.register_service(..)`，条目带归属、插件停用时一并撤销 | `services/mod.rs` |
| `ConfigKey<T>` + `configManager[key]` | `PluginConfig` + `ctx.config::<T>(key)` → `ConfigEntry::<T>::get/set/update` | `config/arona.rs`、`config/plugin_config.rs` |
| `JvmPluginManager` 扫描 `plugins/` 找 jar、独立 ClassLoader 隔离 | `plugin::dynamic::DynamicPluginLoader` 扫描 `plugins/` 找 dll，`libloading` 映射进宿主进程，按四个 C 导出符号握手，再把宿主的进程实例/日志出口/tokio 句柄交进 dll | `plugin/dynamic.rs`、`plugin/abi.rs` |

**与 mirai 最大的不同**：mirai 用独立 ClassLoader 把每个 jar 隔开，插件字节码与宿主各不相干；
这里是把 dll 直接映射进宿主进程、并跨边界交换 `Arc<dyn AronaPlugin>`。省掉了进程外通信的开销与
序列化边界，代价是两件事必须由框架自己补上：**"两边同一套工具链"这条前提由握手逐项把关**，
以及 **ClassLoader 隔离顺带解决的"全局状态只有一份"这里不成立**——dll 静态链接了自己的那份
`arona`，所以装载器在造实例前把宿主的进程默认实例、日志出口与 tokio 句柄交回插件
（§22「宿主的上行通道」）。对插件作者的实际影响：写法与静态编译时完全一样，不用绕路。
这份"一样"是框架换来的：凡是背后连着第三方库状态的能力（聊天记录库、OneBot 动作、HTTP 代发），
框架都只把**宿主生成的 vtable** 或 `extern "C"` 函数交出去，绝不让 dll 自己动手——§22 那两道关讲的就是这件事。

### 上面那张表没盖到的地方（从 mirai 移植时按这个对照）

只剩两处，而且都是**刻意不做**：

1. **一台机器人一个进程**。mirai 的 `BotFactory.newBot(qq, password)` 能在一个 JVM 里跑多账号；
   一个 `Framework` 实例只持**一个** OneBot 应用（`OnebotOutlet::application`）。多 QQ 号的做法是
   多份运行目录起多个宿主进程，插件因此不存在"跨账号操作"这回事。
2. **连接生命周期不在插件手里**。mirai 的 `bot.login()/logout()/close()` 与 `OtherClient`（多端设备）
   没有对应项；插件只能**读**状态——`bot.is_online()`，或者订阅框架在连接翻转为在线/离线时合成的
   那条 lifecycle 元事件（§9：`BodyFilter::meta(MetaKind::Lifecycle, "connect" | "disable")`），
   断开由宿主自己重连。刻意如此：插件关掉唯一那条连接，同进程的别的插件就全瞎了。

**曾经列在这里、现在已经补齐的几处**（留一句"当初为什么差"，方便对照旧资料）：

| 当初的缺口 | 现在对应什么 | 还差多少 |
| --- | --- | --- |
| 句柄不缓存资料，读成员要现查实现端 | `group.member_info(id)` 与 `member.info()` 走实例上的 `PeerCache`：同一个人在同一个群里 60 秒内只打一次网络，`group.forget_member(id)` 立即作废；`bot.user(id)` 那份陌生人资料留 10 分钟 | 只对齐到"按需读一次"这个粒度，**没有**做 mirai 那种整张对象图常驻内存——多实现端下没人能保证那份缓存不出错 |
| 没有 `group.history` 那种可分页的历史 | `group.history(20)` / `group.history_before(20, cursor)`（`Friend` 上同名），游标到头给 `None`（§14.2、§21） | 只读框架自己记的档，不是实现端的全量历史 |
| 图片不是可复用对象 | `arona::bot::Image`：`from_file_id` / `from_url` / `from_file` / `from_bytes`，`image.segment()` 或 `OutgoingMessage::from(image)`，`file_id()` 能把实现端给的回执号接着用第二次（§19） | 上传动作还是实现端在做，框架不代管一份图库 |
| 事件不是一事件一 class | 仍按 `BodyFilter` 找而不是按类名找，但私聊能按子类型分档了：`BodyFilter::private("friend")` 只看好友消息、`private("group")` 只看临时会话，v11 的字符串写法与 CQ 的数字写法（`0`/`1`/`2`）都认（§9） | 事件类本身没有，一家 OneBot 只有一个 `EventContext` |
| `Bot::of(隔离实例)` 上 `api()` 不隔离 | `OneBotApi` 现在持有所属 `Framework` 实例（`OneBotApi::of(&实例)`，`global()` 是进程默认实例那一份）。`Bot::api()`、`PluginContext::api()`、`EventContext::api()`、`CommandContext::api()` 一律落在**构造它的那套实例**上，`recall` / `reply_with_quote` 也跟着走实例上的 `chatlog_api()` | `arona::runtime::chatlog::*` 那批自由函数仍读进程默认实例（§22 末尾的遗留清单） |

## 4. 生命周期（`arona::run` → `run_bot` 的真实顺序）

`arona::run(args)` 内部按序驱动四阶段——插件不需要、也无法在 `run` 之前登记，装载完全由磁盘决定：

1. `plugin::install_all()` —— **早于 `arona.yml` 加载**。先跑装载器：`DynamicPluginLoader` 扫描
   `plugins/`（连同它的一级子目录）里的每个 dll，逐个 `LoadLibrary` → ABI 与工具链指纹握手 →
   取出实例，以 `loader: dynamic` 记进插件表；**单个文件握手失败只记一条日志，不影响其余插件**。
   然后按依赖拓扑序逐个插件：
   建好 `plugins/<id>/`、`config/<id>/`、`data/<id>/` 并写 `plugins/<id>/plugin.yml` → **契约版本握手**
   （不兼容即标 `Failed`，不跑 install）→ 调 `install(&PluginRegistrar)`：插件在此登记功能开关与配置区，
   使生成的配置模板认得这些键、注释里带完整功能清单。
   **install 阶段不过滤被禁用的插件**——否则 GUI 列不出它、模板也会丢掉它的配置块。
2. 加载 `config/arona.yml`（框架业务配置，含热更新）与 `config/onebot.yml`（协议配置）。
   旧版把插件配置键写在框架文件顶层的，这一步会被接管（`plugin_config::absorb_legacy`）。
3. `config::plugin_config::init()` —— 为每个登记了配置区的插件备好 `config/<id>/arona.yml`
   （缺文件就生成带注释模板，升级新增的配置区按默认值补进已有文件）并加载。
4. `plugin::configure_all(onebot_config, test_notify)` —— **只对生效启用的插件调用**（全局停用名单 +
   硬依赖可用性，见 §6）。跑 `configure(&PluginContext)`：登记命令与事件订阅、公布服务。
   状态走 `Installed → Ready`；被停用的停在 `Disabled`。
5. `plugin::start_all()` —— 对 `Ready` 的插件跑 `start(&PluginContext)`：开数据库、拉预热与定时任务。成功置 `Active`。
6. 框架装配 `BusinessHandler`（持无状态的 `CommandDispatcher` 句柄，真正的命令表挂在
   `Framework::global()` 上）并启动 OneBot 连接。
7. 运行中：
   - `config/arona.yml` 或某个 `config/<插件>/arona.yml` 变更 → 热重载后 `plugin::notify_config_reloaded()`
     （插件文件变更只回调该插件的 `on_config_reload`）。框架 arona.yml 重载还会先跑 `sync_enabled_state()`。首次加载不通知（那时 `start()` 已按配置建好任务）。
   - 停用名单变化 → `plugin::sync_enabled_state()`：先按反依赖序停用（被依赖者最后走），再按依赖序补装配
     （新启用的重跑 `configure()` + `start()`），最后再收一轮"依赖被停掉"的下游。GUI 开关与手改 `arona.yml` 都走这条路，**不需要重启**。
   - 收到事件 → 先过门控，再按优先级投给 `arona::onebot::hooks`（§9），无人 `Handled` 才走命令分发。
8. 退出（Ctrl+C / GUI 关窗）：`application.stop()` → `plugin::stop_all()`（反依赖序 `stop()` + 回收）→ `quartz::pause_all()`。

## 5. 插件接口

实现 `arona::plugin::AronaPlugin`，方法都有默认实现，只需覆写关心的阶段：

```rust
pub trait AronaPlugin: Send + Sync + 'static {
    fn meta(&self) -> PluginMeta;                                              // 必填
    fn install(&self, reg: &PluginRegistrar) -> Result<(), String> { Ok(()) }  // 登记功能开关/配置区
    fn configure(&self, ctx: &PluginContext) -> Result<(), String> { Ok(()) }  // 登记命令/事件/服务
    fn start(&self, ctx: &PluginContext) -> Result<(), String> { Ok(()) }      // DB/预热/定时任务
    fn on_config_reload(&self, ctx: &PluginContext) {}                         // 本插件配置热重载后
    fn stop(&self, ctx: &PluginContext) {}                                     // 关自己打开的句柄
}
```

`install`/`configure`/`start` 返回 `Err(reason)` 只影响本插件（标 `Failed` 并记日志），**不会拖累其他插件**。
每个阶段都可能被框架整个跳过（插件被停用、依赖不可用），所以实现里不要放"必须执行一次"的全局初始化；
那种事交给 `install` 阶段登记，由框架兜住顺序。

交接面分工（把接口用错阶段是最常见的踩坑）：

| 交接面 | 何时拿到 | 能做什么 |
| --- | --- | --- |
| `PluginRegistrar` | `install` | 登记 `Feature`、登记 `PluginConfig` 区、取目录路径。**此时框架配置还没加载**，读不到任何配置值 |
| `PluginContext` | `configure` / `start` / `on_config_reload` / `stop` | 命令、事件、任务、服务、配置读写、OneBot `api()`、目录路径。它 `Clone + Send + Sync`，可以搬进 `async` 闭包 |

## 6. 元数据：id、版本握手与依赖

### 硬性规范：插件必须有 id、name 与 version

`meta()` 返回的 `PluginMeta` 里 **`id`/`name`/`version` 为强制项**（`description`/`author` 可留空串）。
`version` 建议直接取 `env!("CARGO_PKG_VERSION")`，与 crate 版本保持一致：

```rust
use arona::plugin::{ApiVersion, PluginMeta};

fn meta(&self) -> PluginMeta {
    PluginMeta::new(
        "voice-room",                                   // 目录名与配置里的键都用它
        "VoiceRoomPlugin",                              // GUI/日志展示名
        env!("CARGO_PKG_VERSION"),
        "语音房管理：排队上麦、静音、房管投票",
    )
    .with_author("你的名字")
    // .requires_api(ApiVersion::new(1, 1, 0))          // 用到比当前框架新的接口时才抬
    // .depends_on(&["core-data"])                      // 硬依赖：任一缺失/停用则本插件不装配
    // .soft_depends_on(&["gacha"])                     // 软依赖：只决定装配先后，缺了照样跑
}
```

`id` 用小写短横线名（`voice-room`、`music` 这种），它是**磁盘上的身份**：
`plugins/<id>/`、`config/<id>/arona.yml`、`data/<id>/`、`disabled_plugins: [<id>]`、
`group_settings.<群号>.disabled_plugins` 全都用它。**改名等于换一份用户数据，定下来就别动。**
匹配时大小写不敏感。

**契约版本握手**（mirai 的 `ApiVerification`）：框架启动时对每个插件比一次
`FRAMEWORK_API_VERSION.satisfies(&meta.api_version)`，规则是**主版本号必须相等、框架次版本不低于要求**
（框架只加能力不删能力，所以要求低版本的插件在新框架上照常跑）。不通过的插件标 `Failed`、不跑 `install`，
日志写清"要求 api x.y.z，本程序提供 api a.b.c"。默认要求的正是当前契约版本，所以绝大多数插件不用管。
握手失败的插件**不会被运行期的开关重新拉起**（它恒为"不生效"），只能靠改代码/升框架解决。

**依赖解析**：`PluginManager::ordered()` 对 `depends + soft_depends` 做拓扑排序（依赖在前，同层保持清单顺序，
结果稳定）。成环的插件全部标 `Failed("插件依赖成环…")`。`depends` 是硬约束：被依赖者没编译进来、
被用户停用、或还没 `Installed`，本插件就不装配；用户停掉一个被依赖的插件时，框架会连锁停掉它的下游。
`soft_depends` 只排顺序。跨插件取能力还要靠服务容器（§11），依赖声明负责保证"取的时候对方已经就位"。

框架用 `plugin::metas()` / `meta_of(id)` / `state_of(id)` 汇总元信息，GUI「插件管理」页与诊断据此展示
（`manager::summary()` 给总数/运行数）。

## 7. 目录接口：config 与 data 统一由框架分配

插件**不要自己拼路径**。两个交接面都给出本插件的全部目录（框架按 §1 的约定拼）：

| 接口 | 返回 |
| --- | --- |
| `ctx.plugin_id()` / `reg.plugin_id()` | 本插件 id（= `meta().id`） |
| `ctx.plugin_dir()` / `reg.plugin_dir()` | `plugins/<id>/` —— 随包资源；`plugin.yml` 由框架维护 |
| `ctx.config_dir()` / `reg.config_dir()` | `config/<id>/` —— 插件自己的其它配置文件也放这里 |
| `ctx.config_file()` | `config/<id>/arona.yml` —— 框架负责生成模板与热重载 |
| `ctx.data_dir()` / `reg.data_dir()` | `data/<id>/` —— 数据库、备份等 |
| `ctx.image_dir()` / `reg.image_dir()` | `data/<id>/image/` —— 生成的图片、下载的缓存 |

这些接口都顺手 `create_dir_all`（`install_all()` 阶段已先建好三件套），插件取到路径就能直接写。
`plugins/<id>/plugin.yml` 是框架自动生成的 mirai 风格清单
（`id/name/version/author/description/apiVersion/loader/depends/softDepends/config/data/image`），
`plugins/<id>/` 是推荐投放位置：dll 本身放这里，随包资源也放这里，
`plugin.yml` 由框架按插件上报的元信息渲染出来（§22）。

id 目录名在阶段之外也要用时（比如模块级函数），直接用
`arona::runtime::paths::plugin_data_dir(PLUGIN_ID)` / `plugin_image_dir` / `plugin_config_dir`，
把 `PLUGIN_ID` 作为 `pub(crate) const` 与 `meta().id` 共用一个来源（见 `plugins/hello/src/lib.rs`）。
`runtime::paths` 只是拼路径 + 顺手建目录，属于刻意留在进程级的那一类，插件用它是安全的。

运行目录是**当前工作目录**（安装包的快捷方式把它设成安装目录）。旧版本把这些放在
`arona-standalone/` 下：框架的 `paths::prepare()` 会**复制**它认识的几项到新位置；
插件独有的旧文件（图片、数据库、备份）由插件自己在 `install()` 里用
`paths::migrate_file` / `paths::migrate_dir` 搬（只补缺、保留旧文件，旧目录不删）。

## 8. 登记功能与命令

### 功能开关（install 阶段）

```rust
use arona::runtime::config::Feature;

fn install(&self, reg: &PluginRegistrar) -> Result<(), String> {
    reg.feature(Feature {
        key: "gacha",                                   // 与命令的 with_feature 对齐
        name: "抽卡",
        description: "单抽/十连/抽卡服务器/狗叫/历史",
    });
    reg.config::<NotifyConfig>("notify");                // §12
    Ok(())
}
```

GUI「群管理 → 功能开关」与配置模板注释都据此生成。拿不到 `reg` 的地方（如自由函数）可用等价的
`arona::admin::register_feature(feature, PLUGIN_ID)`。

**这一层的字符串 key 全是 `&'static str`**：`Feature` 的三个字段、`reg.config::<T>(key)` 与 §12 的
`ctx.config::<T>(key)`、下面命令的 `with_feature(..)`、§9 的 `listen_feature(..)` 都是。
字符串字面量与 `const`/`static` 项天然满足，`format!` 出来的 `String` 编不过（`&String` 不行，
`&*some_string` 也不行）——key 必须在运行期拼出来就 `Box::leak` 成 ` &'static str `，
更常见的做法是把 key 定成 `const FEATURE: &str = "gacha";` 复用，顺便让命令与钩子写同一个常量。

### 命令（configure 阶段）

命令表挂在**框架实例**上（`ctx.framework().commands()`）、条目**按插件归属**（mirai 的 `CommandManager`）。
不再存在"插件各自一个分发器、框架只认一个槽位"
的写法——那正是"框架只能挂一个功能插件"的老根因。

```rust
use arona::runtime::args::arg;
use arona::runtime::dispatcher::{
    CommandRegistration, Permission, fallback, handler, typed_handler,
};
use arona::runtime::priority::CommandPriority;

fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
    ctx.commands(vec![
        // 写法一：自己拿原始词表
        CommandRegistration::new(
            vec!["/单抽".into(), "gacha_one".into()],
            "单抽一次, 可选服务器",
            handler(|_context, arguments| async move {
                // 原始词表版：arguments 是切好的裸词，返回 Some 就交框架发回原会话
                let server = arguments.first().cloned().unwrap_or_else(|| "jp".to_string());
                Some(arona::runtime::message::OutgoingMessage::text(format!(
                    "{server} 服务器单抽结果（示例）"
                )))
            }),
        )
        .with_feature("gacha")             // 绑分群功能开关；该群关掉时视为未匹配
        .with_usage("/单抽 [jp|global|cn]") // 帮助页展示
        .with_priority(CommandPriority::High), // 命令名撞车时的胜出方，默认 Normal

        // 写法二：声明参数与身份，切词/类型转换/范围校验/用法回显全交给框架
        CommandRegistration::typed(
            vec!["/禁言".into()],
            "禁言指定分钟数",
            typed_handler(|context, args| async move {
                let minutes = args.i64("分钟").unwrap_or(10);
                let reason = args.text("原因").unwrap_or("未填").to_string();
                /* 返回 Some(消息) 由框架发回，无需自己 reply */
                Some(arona::runtime::message::OutgoingMessage::text(format!(
                    "已禁言 {minutes} 分钟：{reason}"
                )))
            }),
        )
        .with_args(vec![
            arg::i64("分钟").optional().with_default("10").range(1, 1440),
            arg::rest("原因").optional(), // 吃掉剩下的全部文本
        ])
        .with_permission(Permission::GroupAdmin) // 群管理员/群主才能用，否则框架直接回绝
        .with_prefix_match(true),                // "/禁" 这种最短前缀也能命中

    ]);

    // 未命中任何命令时的兜底（同优先级下按登记顺序依次调用）
    ctx.fallback(fallback(|context| async move { /* 例如把纯数字回复解析成上次的选项 */ }));
    // 想让别人先接：ctx.fallback_at(handler, CommandPriority::Lowest)
    Ok(())
}
```

`arg` 模块给的类型：`text`（一个非空白词）、`rest`（吃掉剩余）、`i64`、`bool`
（`是/否`、`true/false`、`on/off`、`开/关`、`1/0`）、`choice(name, &["a","b"])`（大小写不敏感，命中后给候选表里的原样写法）。
修饰器 `optional()` / `with_default(v)` / `range(min, max)` / `placeholder(p)`。
声明了参数就不必再写 `with_usage`——框架按声明自动生成（`/禁言 <分钟> [原因]`）。

`Permission` 三档：`Anyone`（默认）、`GroupAdmin`、`GroupOwner`。判定顺序是
**框架管理员名单（`arona.yml` 的 managers）一律放行 → 事件自带的 `sender.role` → 都没有才回查
`get_group_member_info`（按 群号+QQ 缓存 60 秒、3 秒超时）**，所以正常群聊里权限门控不产生网络往返。
私聊没有群身份，`GroupAdmin`/`GroupOwner` 只对框架管理员开放。

要点：
- **不用（也没机会）填插件 id**：`ctx` 自带归属，框架按它做停用与回收。
- **群里 @机器人 后跟命令能直接命中**：框架在查命令表之前就把"@机器人本身""@全体成员""引用""图片"
  这些召唤性前导段剥掉了（`onebot::protocol::command_text`，对齐 mirai 进 `CommandManager` 前剥 `At(bot)`）。
  `context.text` 就是剥过的那份；没剥之前的原始段落见 `context.segments`（上面一条与 §19）。
- **处理器 `Some(OutgoingMessage)` 由框架发回**（发向 = 群聊回群、私聊回人），不用再自己 `reply`；
  已经自己 `reply` 过的返回 `None`，不会重复发。
- **命令处理器拿得到整条消息**：`CommandContext` 除了 `text`（剥过召唤前缀的命令文本）还带
  `message_id`（本条消息的 id）、`time`（秒级时间戳）、`quoted`（这条消息引用了哪条，无引用为 `None`）、
  `segments`（剥之前的完整消息段，@、引用、图片都在里面）。mirai 的 `Source`/`QuoteReply` 靠的就是这几项：
  插件要判断"用户引用了谁的话、引用的那条还在不在"，不必去钩子里另找一份。
  `text` 与 `segments` 的区别只在于前者剥了前导的 @机器人/引用/图片（`protocol::command_text`），
  后者原样保留（`protocol::extract_segments`）。
- 命令名撞上别家插件时框架只记告警、按 `priority` 定胜出方（同优先级先到先得），**其余命令照常登记**，
  不会因为一家冲突就整批失败。分发时同名命令只调用排在最前的那一家，别家不会跟着响应一遍。
- 前缀匹配默认关闭（一个字母能撞上一堆命令），单个命令用 `with_prefix_match(true)` 打开，
  或整套实例用 `Framework::builder().prefix_match_by_default(true)`；部署时也可以不改代码，
  在框架 `config/arona.yml` 的 `framework:` 段里设（见 §20，热生效，且对**已经登记好**的命令也生效——
  没显式声明过的命令在匹配那一刻才读这个开关）。歧义时框架回显候选列表，不猜。
- **命令名要带前缀**：`FrameworkOptions::command_prefixes`（默认 `["/"]`）规定了合法写法，登记时
  不带其中任何一个前缀的名字会被自动补上第一个（`"抽卡"` → `"/抽卡"`），并在 `problems` 里回报这次改写。
  目的是让"arona 好可爱""这个 config 怎么改"这类日常句子不触发机器人——裸名别名（`"gacha"`、`"谁叫"`）
  请写成 `"/gacha"`、`"/谁叫"`。想要纯聊天式命令（词就是命令、不写前缀）由宿主
  `Framework::builder().command_prefixes(Vec::<String>::new())` 关掉这条规则；列表可以给多个前缀
  （`["#", "/"]`），带任一者都算合规，裸名补第一个。规则只在**登记时**生效：名字是命令表的键，
  中途改设置不会给已登记的命令改名。
- `ctx.own_commands()` 拿本插件名下的命令概览（`CommandInfo`：名字、描述、用法、功能 key、优先级、
  所需身份，以及**登记序号 `seq`**）；全表看 `arona::runtime::dispatcher::commands()`
  （默认实例那份，等价于 `Framework::global().commands()`）。
  自绘 `/帮助` 就该从这里生成：一条命令的多个别名是**各自独立的表项**（描述相同），
  `seq` 最小的那个是主名——按 `seq` 排序再按描述去重，别名就不会重复占行。
  写死一份清单必然和 `registrations()` 漂移（本仓库的 `/帮助` 以前就列过不存在的命令）。
- `configure()` 里可拿到 `ctx.onebot_config`（协议配置快照，需要 self_id / nickname 时用）与 `ctx.test_notify`
  （命令行是否带 `--test-notify`）。

## 9. 事件钩子：听到 OneBot 的全部事件

插件入口不止"注册命令"——命令只在文本命中 `/抽卡` 这类前缀时才进插件。成员进群打招呼、被踢后清理数据、
有人申请加群、群名被改、别人引用了机器人的消息，这些属于 notice/request/meta 事件，
或者属于"命中命令之前先被看一眼"的消息事件。统一入口是 `ctx.listen(..)` / `ctx.listen_where(..)`
（底层是 `arona::onebot::hooks`）：

```rust
use arona::onebot::{EventContext, EventKind, HookFlow};
use arona::onebot::hooks::{BodyFilter, EventBody, NoticeKind, event_handler, ListenerPriority};
use arona::plugin::PluginContext;
use std::sync::Arc;

fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
    // 只订阅"成员进群"这一种子事件，框架先把别的过滤掉，插件里不用自己比字符串。
    // 优先用构造器而不是手写枚举：少写一层，子类型那一格也不会漏
    ctx.listen_where(
        &[BodyFilter::notice(NoticeKind::GroupIncrease, "")],
        ListenerPriority::Normal,
        event_handler(|context: Arc<EventContext>| async move {
            let _ = context.reply("老师好！").await;
            HookFlow::Pass                 // 通知事件一般继续往下走
        }),
    );

    // 群消息 / 私聊消息有专用入口，另有 on_message / on_notice / on_request / on_meta
    ctx.on_group_message(event_handler(|context| async move {
        // 需要更细的分支就 match 强类型事件体，而不是拼字符串
        if matches!(&context.body, EventBody::GroupMessage { sub_type: Some(s) } if s == "anonymous") {
            return HookFlow::Handled;          // 匿名消息直接吞掉
        }
        HookFlow::Pass
    }));

    ctx.listen(
        &[EventKind::Message],
        ListenerPriority::Monitor,         // 审计/风控类先看一眼
        event_handler(|context| async move {
            if context.text.contains("早安") {
                let _ = context.reply("老师早安！").await;
                return HookFlow::Handled;  // = mirai 的 event.intercept()：后续钩子与命令分发都不再跑
            }
            HookFlow::Pass
        }),
    );
    Ok(())
}
```

`event_handler(..)` / `handler(..)` 只要求那个闭包**返回一个 future**，所以 `|context| async move { .. }` 就够了。
旧写法 `|context| Box::pin(async move { .. })` 也编得过（`Pin<Box<dyn Future + Send>>` 本身实现了 `Future`），
但那一层包装在这里没有任何作用，本文档现在一律用直返的写法；`plugins/hello` 也是这一种。
反过来，若你要把处理器存进结构体或跨边界传递，才需要 `Arc<dyn EventHandler>` 那个形态——
`event_handler(..)` 的返回值就是它。

上面示例里 `ctx.listen_where(..)` / `ctx.listen(..)` 都写全了三个参数，实际还有一批单参数快捷入口
（`h` 就是 `event_handler(..)` 的返回值，档位一律 `ListenerPriority::Normal`）：

| 快捷入口 | 等价于 |
| --- | --- |
| `ctx.on_message(h)` | `ctx.listen(&[EventKind::Message], Normal, h)` |
| `ctx.on_group_message(h)` | `ctx.listen_where(&[BodyFilter::GroupMessage], Normal, h)` |
| `ctx.on_private_message(h)` | `ctx.listen_where(&[BodyFilter::PrivateMessage], Normal, h)` |
| `ctx.on_notice(h)` / `ctx.on_request(h)` / `ctx.on_meta(h)` | `ctx.listen(&[EventKind::对应大类], Normal, h)` |
| `ctx.on_outgoing(h)` | `ctx.on_outgoing_at(h, Normal)`（§9.1） |

要指定优先级、或要窄到子类型（`BodyFilter::notice(..)` / `private(..)` / `meta(..)`），就得把三个参数写全；
绑分群功能开关的订阅只有 `ctx.listen_feature(feature, filters, priority, h)` 一条路，没有对应的 `on_*` 简写。

`EventContext` 提供：`kind`、归位后的强类型 `body`（`EventBody::{GroupMessage,PrivateMessage,Notice,Request,Meta,Other}`，
其中 `Notice(.., NoticeInfo)` / `Request(.., RequestInfo)` / `Meta(.., MetaInfo)` 各带一份细节）、
原始 `event`、`text`、`command_text`（剥掉"@机器人/引用/图片"等召唤前缀、用来匹配命令的那份文本）、
`segments`（§19 的全部消息段）、`api()`（§14 的动作出口，**绑的是投递这条事件的那套实例**）、`bot()`（§14.2 的对象模型入口），
以及 `user_id()/group_id()/is_group()/is_private()/message_id()/field(key)/field_str(key)`、
`target()`、`reply(text)`、`reply_message(OutgoingMessage)`、`reply_with_quote(OutgoingMessage)`、`recall(message_id)`（后两条见 §21）。
发送者侧另有 `sender_name() -> Option<&str>`（群名片优先，退回昵称）、`sender_role() -> Option<GroupRole>`（§8 的权限枚举）、
`sender_is_admin() -> bool`（群身份是不是管理员或群主，身份未知时 `false`）、`quoted() -> Option<i64>`（这条消息引用了哪条：`command_text` 里已剥掉引用段，值在这里）——
全部读事件自带的 `sender`/`segments`，不发协议请求，实现端没给就是 `None`。

**`reply` 回到哪儿（上面进群示例靠的就是这一条）**：`target()` 的回退顺序是群 → 私聊发送者 → 无目标。
群那一档**只看 `group_id`**——OneBot 11 只在消息事件里给 `message_type`，进群/退群/改名片/加群请求这些
notice、request 事件都带 `group_id` 而不带 `message_type`，所以钩子里对它们 `context.reply(..)` 会发进那个群。
私聊那一档只认消息事件：加好友请求的 `user_id` 是对方的号，框架不会擅自往那儿发一句。
meta 事件（心跳、连接状态）没有可回复对象——这三类情况下 `reply` 返回 `Ok(None)`，
同时控制台打一条「这条事件没有可回复对象，已丢弃」，不会让你对着一个查不出原因的沉默。

**同名不同物，照抄时最容易撞**：这几项在 `EventContext` 上是**方法**，在 `CommandContext` 上是**字段**，
括号多打或漏打都编不过。
```rust
// 钩子里（context: Arc<EventContext>）——sender_name 是方法，借用回来的 Option<&str>
let who = context.sender_name().unwrap_or("新同学");
// 命令里（context: Arc<CommandContext>）——sender_name 是字段，Option<String>
let who = context.sender_name.as_deref().unwrap_or("陌生老师");
```
另有一处语义差别别看错：`CommandContext::is_admin` 说的是这个人在不在框架 `config/arona.yml` 的
`managers` 名单里，`EventContext::sender_is_admin()` 说的是这个人在**那个群**里的身份（管理员或群主），
两者完全独立，可以一个真一个假。

`BodyFilter` 的档位：`All`、`Kind(EventKind)`（大类）、`GroupMessage` / `PrivateMessage` /
`PrivateSubType(String)`、`Notice(NoticeKind)`、`NoticeAction(NoticeKind, String)`、`Request(RequestKind)`、
`Meta(MetaKind)`、`MetaState(MetaKind, String)`。后两档各有一个带子类型的构造器（`private(..)` / `meta(..)`），
构造器传空串就退化成整类，所以**优先用构造器而不是手写枚举**。

同一个 `notice_type` 下的不同动作在 QQ 上是完全不同的事，所以子类之外还有一层细节：

```rust
// 本块与下面两块沿用上一块的 use，这里只补新出现的枚举；
// 出现的 ctx 是 configure 的入参，context 是钩子闭包收到的那个 Arc<EventContext>
use arona::onebot::hooks::{BodyFilter, EventBody, NoticeKind, RequestKind};

// "被踢出群"与"自己退群"都是 group_decrease，靠动作分开订阅
ctx.listen_where(
    &[BodyFilter::notice(NoticeKind::GroupDecrease, "kick")],   // leave=自行退群, kick_me=被自家机器人踢
    ListenerPriority::Normal,
    event_handler(|_context| async move { /* 清理该成员的数据 */ HookFlow::Pass }),
);

// 钩子体内按强类型事件体分支（NoticeInfo 带动作/操作者/时长）
if let EventBody::Notice(NoticeKind::GroupBan, info) = &context.body {
    // info.action: ban / unban；info.operator_id: 谁干的；info.duration: 秒（0 = 解禁）
    if info.action.as_deref() == Some("ban") && info.duration == Some(0) {
        // ...
    }
}

// 请求侧：v11 把"申请加群"和"被邀请进群"都写成 request_type="group"，
// 框架读 sub_type 归位成两个子类——自动通过申请的插件必须只认 AddGroup
// info.flag 是处理凭据（§14 的 set_group_add_request 要原样带回），info.comment 是留言
ctx.listen_where(
    &[BodyFilter::Request(RequestKind::AddGroup)],
    ListenerPriority::Normal,
    event_handler(|context| async move {
        // 审核完拿 context.body 里的 flag 去调 set_group_add_request
        HookFlow::Pass
    }),
);
```

私聊与元事件还能再窄一档，对应 mirai 的 `FriendMessageEvent` / `TempMessageEvent` 和那族连接状态事件：

```rust
use arona::onebot::hooks::{BodyFilter, MetaKind, event_handler, ListenerPriority};
use arona::onebot::HookFlow;

// 私聊按子类型分档：只用 BodyFilter::PrivateMessage 的话，临时会话也会一起收进来
ctx.listen_where(
    &[BodyFilter::private("friend")],   // "group" = 从群里发起的临时会话，另有 "other"
    ListenerPriority::Normal,
    event_handler(|_context| async move { /* 只有好友消息进这一档 */ HookFlow::Pass }),
);

// 机器人上下线：框架在传输层 0↔1 翻转时合成一条 lifecycle 元事件
ctx.listen_where(
    &[BodyFilter::meta(MetaKind::Lifecycle, "connect")],   // "disable" 是掉线那一侧
    ListenerPriority::Normal,
    event_handler(|context| async move {
        // body 显示成「元事件:生命周期(connect)」，来源标识在 context.event.raw["url"]
        HookFlow::Pass
    }),
);
```

这两档都不额外发请求，但来路不一样，值得分清：

- `private(..)` 比的是事件自带的 `sub_type`。v11 的字符串写法（`friend`/`group`/`other`）与
  部分实现端的数字写法（`0`/`1`/`2`）按**同族**判定，`group` 与 `temp` 也算同族，
  所以宿主不替你猜"这家到底怎么标临时会话"，插件也不用自己写两套比较。
- 这两档在示例插件里都有活例子，也都在端到端里验过（原文见 §22 末段）：`plugins/hello` 各记一笔账，
  由 `/示例连接` 把计数读回来。判据是**命中次数**而不是"有没有回话"——过滤失效时三条私聊全会回话，
  只看有没有回话恰好会被骗过去。
- lifecycle 那条**完全由框架合成**：正向 WS 连上与读循环退出、反向 WS 的首条与最后一条 socket、
  反向 HTTP 心跳 `get_status` 的成败翻转。反向 WS 挂多条 socket 时只在 0↔N 之间各报一次，
  不会每接一条就刷屏。它**只投钩子，绝不回播给实现端**——`broadcast_except` 那条路是给
  多实现端同步真实事件用的，把合成事件塞进去等于对别家客户端撒谎。
  因此"此刻在不在线"仍以 `bot.is_online()` 为准，这条事件的用处是**掉线后重连上了要补投什么**。

`NoticeKind` 覆盖 `group_upload/group_decrease/group_increase/group_admin/group_ban/group_recall/poke/nudge/群名片/群头衔/notify`。
NapCat、LLOneBot、Lagrange 把戳一戳、名片与头衔变更都塞进 `notice_type="notify"`，真正的类型写在内层
`sub_type`/`type` 里——框架 `EventBody::from_event` 会把它们归回 `Poke`/`Nudge`/`GroupCardUpdate`/`GroupTitleUpdate`，
插件不必自己啃这层实现端差异；细分动作原样留在 `NoticeInfo::action`。
`RequestKind` 覆盖加好友、加群申请、被邀请加群（`InviteGroup`，靠 `sub_type=invite` 认出），`MetaKind` 覆盖心跳与生命周期。
实现端自定义的类型一律落到 `Other`，`field_str` 仍能拿到原始字符串——**优先用 `body`/`BodyFilter`，字符串比较是兜底**。

优先级序（`arona::runtime::priority::Priority`，数值越小越先执行）：
`Monitor → High → Normal（默认） → Low → Lowest`，同档按登记顺序。注意 `High` 排在 `Normal` 之前、
`Lowest` 是给兜底实现留的最后一档——这套序与 mirai 的 `EventPriority` 一致，别按英文字面意思猜。

门控语义（框架负责，插件不用自己判断）：
- `message`：先过「群授权 + 全局/群内黑名单」，再进钩子，最后才是命令分发；
- `notice` / `request` / `meta`：无条件投递（机器人被踢、黑名单用户申请加群这类事也会触发，插件自己要清楚）；
- 任何一条返回 `HookFlow::Handled` 即停止后续钩子并跳过命令分发；
- **所属插件被禁用（全局或该群）时，它的钩子一律跳过**；
- 用 `ctx.listen_feature("gacha", ..)` 绑过分群功能开关的订阅，该群关掉这个功能时同样跳过——
  和同名命令保持一致，不会出现"面板上功能已关闭、钩子还在收这个群的消息"。

`arona::onebot::hooks::{on_message, on_notice, on_request, on_meta, on_all, subscribe, subscribe_at, subscribe_where, subscribe_feature}`
这些自由函数仍在（第一个参数是插件 id），供框架自身与拿不到 `ctx` 的场景用；插件正常都走 `ctx`。

### 9.1 出站前钩子：消息交给实现端之前的最后一道（mirai 的 `MessagePreSendEvent`）

上面都是"收"。发出去那一侧同样有订阅点：`ctx.on_outgoing(..)` / `ctx.on_outgoing_at(.., priority)`。
凡是走框架发送口的消息——命令回复、钩子里的 `e.reply(..)`、`services::send_message` 的主动推送——
都会先过这道门：

```rust
use arona::onebot::hooks::{ListenerPriority, OutboundContext, outbound_handler};
use arona::runtime::message::MessageSegment;   // 消息段在 runtime::message，不在 onebot 下
use std::sync::Arc;

// 本插件发出的每条消息末尾加一行落款
ctx.on_outgoing(outbound_handler(|out: Arc<OutboundContext>| async move {
    out.rewrite(|message| {
        message.segments.push(MessageSegment::Text("\n—— 阿罗娜".into()));
    });
}));

// 风控：内容涉敏就整条拦下
ctx.on_outgoing_at(
    outbound_handler(|out| async move {
        let risky = out.message().segments.iter().any(|segment| {
            matches!(segment, MessageSegment::Text(text) if text.contains("密码"))
        });
        if risky {
            out.cancel();   // 发送方拿到空回执，实现端收不到任何动作
        }
    }),
    ListenerPriority::Monitor,   // 越早的档位越先改，后面的看到改完的结果
);
```

`OutboundContext` 一共四件事：`target`（发往哪个群/谁，公有字段）、`message()`（当前快照）、
`rewrite(closure)`（改写，闭包里拿到的是到目前为止各家改完的内容）、`cancel()`（拦停），
外加 `is_cancelled()`。处理器返回 `()`——拦停靠 `cancel()`，不靠返回值，所以一次 `await` 里
先改后拦、或者根据远端查询结果决定拦不拦都行。

`OutgoingMessage::revoke_after_millis`（§19 的自动撤回）也在这一步可写，且改写发生在
**合并转发拆分与控制台打印之前**——日志里看到的就是真正发出去的那份。

优先级序、同档按登记顺序、同一处理器重复订阅去重、单插件条数上限、panic 隔离（某家改崩了不会吞掉消息，
记账后带着当前内容继续走）、**所属插件被禁用（全局或该群）时一律跳过**——语义与入站钩子一致。
自由函数版是 `arona::onebot::hooks::{subscribe_outbound, dispatch_outbound}`；`subscribe_outbound` 的
`feature` 参数同 `ctx.listen_feature`（该群关掉这个功能时一并跳过），`ctx` 这两个入口不带功能 key。

三个边界要清楚：
- 钩子里 `e.reply(..)` / `e.reply_message(..)` 走的就是框架发送口，因此**会**再过一次出站门（含本插件自己的那条），
  写落款/过滤逻辑时不必为"钩子回的消息"开特例；
- 也正因为这条，**别在出站钩子里再发一条消息**——那是第二次过门，条件一触发就是自我递归。
  要动手就改本条（`rewrite`），或者 `cancel()` 之后换个时机（`ctx.spawn`）再发；
- 反过来，`OneBotApi` 的发送方法（`api.send(..)`、`send_group_msg(..)`、`send_private_msg(..)`）是
  **协议层直连出口**：不过这道门，也不打印、不拆合并转发、`revoke_after_millis` 会丢
  （`MessagePayload` 只带消息段）。它是留给 GUI 与"确实要绕开门控"的场景的后门，
  插件平时发消息请用 `context.reply*`、`services::send_message`、§12 的延时推送。

## 10. 后台任务与定时任务：作用域自动回收

mirai 靠 `plugin.coroutineScope` 消失来保证"停用即无残留"。Arona 的等价物是 `PluginScope`：
**每个插件一个，由框架持有并在线程间共享，停用时框架 `cancel_all()`**。

```rust
use arona::quartz::JobFn;   // = Arc<dyn Fn() + Send + Sync + 'static>

fn start(&self, ctx: &PluginContext) -> Result<(), String> {
    ctx.spawn(async { /* 长任务：插件停用即被 abort */ });        // = coroutineScope.launch

    let job: JobFn = Arc::new(|| { /* 同步的活计；要 await 就 ctx.spawn(async { .. }) */ });
    ctx.daily_job(8, "DailyNotify", job.clone());               // 每天 8 点
    ctx.repeat_job(3600, "HourlyWarm", job.clone());            // 固定间隔，首次立即
    ctx.single_job(ts_ms, "OnceAt", job.clone());               // 单次
    ctx.delay_job(20, "TestNotify", job);                       // 延迟 N 秒
    ctx.remove_job("TestNotify");                               // 撤掉自己的一个任务
    // 例行清库交给框架排期（回调返回删掉的条数，日志挂 [Arona] 而非插件名）
    ctx.purge_job("ImageCachePurge", "图片缓存", every_days, keep_days, Arc::new(|| 0));
    Ok(())
}
```

`purge_job` 是同一件事的框架侧版本：例行清库（「每 N 天删掉 M 天前的东西」）由 `runtime::purge`
统一排期与播报，插件只交出删除动作并回报条数，所以那两行日志挂 `[Arona]` 而不是某家的名字。
同名任务重复登记时周期没变就不重建——`Repeat` 型任务登记时会先跑一次，重建等于每改一次配置就清一遍库。

这些 helper 内部把 `quartz` 的**任务组**填成插件 id，所以 `revoke_resources` 一次就能整组取消。
硬约束：**不要用裸 `tokio::spawn` 起长命任务、不要把 `PLUGIN_ID` 之外的字符串传给 `quartz` 的 group 参数**，
否则框架收不到，用户把插件停掉后日历照样每天往群里发。
（`quartz::remove_group` 现在只由框架调用，插件不再需要自己记任务名。）

## 11. 服务容器：插件之间共享能力

对应 mirai 的 `DiContainer`。插件之间不靠全局 `static` 互相摸，而是各自 `declare`，别人按类型取用：

```rust
// 提供方（configure 阶段）
#[derive(Default)]
pub struct DiceRoller;
impl DiceRoller { pub fn roll(&self, sides: u64) -> u64 { .. } }
ctx.declare_service::<DiceRoller>(Arc::new(DiceRoller::default()));

// 使用方（自己的 meta 里声明 soft_depends = ["dice"]，保证装配先后）
match ctx.service::<DiceRoller>() {                 // 或 arona::container::instance::<T>()
    Some(dice) => { dice.roll(6); }
    None => { /* 提供方没装或被停用：走降级路径 */ }
}
```

服务按 `TypeId` 寻址，**键类型必须 `Sized`**（`Arc::downcast` 的限制）。要暴露的是接口而不是具体结构时，
用 `Arc<dyn Trait>` 当键类型——它本身是 `Sized` 的：

```rust
pub trait DiceRoller: Send + Sync { fn roll(&self, sides: u64) -> u64; }
pub type Dice = Arc<dyn DiceRoller>;

ctx.declare_service::<Dice>(Arc::new(Arc::new(MyDice) as Dice));
if let Some(dice) = ctx.service::<Dice>() { dice.roll(6); }
```

每个服务都记住登记它的插件 id：`revoke_resources` 会撤销停用插件名下的全部服务，所以取到的来自还在工作的插件
（`container::instance` 另外还会查 `plugin_enabled`）。`arona::container::{provider_of, list}` 供诊断/GUI 用。

### 别和「服务开关表」混为一谈

容器（`container()`）按**类型**共享能力实例，是给插件之间互相调用的；服务开关表（`services()`，
`arona::services::ServiceManager`）存的是**面向用户的功能单元**：一条 id + 名字 + `groupOnly`/`adminOnly`，
带一个可单独关停的 `AtomicBool`，GUI「服务管理」页与 `/服务`、`/紧急停止` 指令操作的是它。

```rust
// configure 阶段登记，条目自动记在本插件名下
ctx.register_service(arona::services::service_info(12, "活动推送", false, false));

// 拿不到 ctx 的地方（命令实现里）按实例取表，别自己 new 一张
let board = ctx.service_board();                 // 等价于 ctx.framework().services()
if let Some(service) = board.find_by_name("活动推送") {
    service.enable.store(false, std::sync::atomic::Ordering::SeqCst);
}
```

`ServiceManager` 的读法：`all()`（按 id 升序）、`find_by_name`、`owner_of`、`enable(name)` / `disable(name)`；
`register` 按名字覆盖，所以插件被重复 `configure` 不会多出一行。插件停用时框架 `revoke(plugin)` 撤掉它名下
全部条目——`/紧急停止` 那种遍历 `all()` 的逻辑因此不会看到僵尸服务。

## 12. 类型化配置 `config/<插件>/arona.yml`

框架的 `config/arona.yml` 只认 `groups` / `managers` / `global_blacklist` / `group_settings` / `disabled_plugins`。
插件的业务配置各住各的文件。对应 mirai-console 的 `ConfigKey<T>` + `configManager`：**插件只写一个 serde 结构 + 字段注释**，
模板渲染、默认值补齐、未知子键过滤全部由框架完成，不再手写 YAML。

```rust
use arona::config::arona::PluginConfig;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    pub enable: bool,
    pub every_day_hour: u32,
    pub black_groups: Vec<i64>,
    pub notify_text: String,
}

impl PluginConfig for NotifyConfig {
    const TITLE: &'static str = "每日活动推送";
    const DOC: &'static str = "每天 every_day_hour 点向已授权的群推送当期活动";
    /// 字段注释：路径不含区块自身键名，嵌套用点号，如 override.name
    fn comment(path: &str) -> Option<&'static str> {
        Some(match path {
            "enable" => "是否启用每日推送",
            "every_day_hour" => "每日推送的小时(0-23)",
            "black_groups" => "不接收推送的群",
            _ => return None,
        })
    }
}
```

登记（install 阶段）与读写：

```rust
reg.config::<NotifyConfig>("notify");            // "notify" 是文件里的顶层键

let notify = ctx.config::<NotifyConfig>("notify");   // configure/start/on_config_reload 里
let hour = notify.get().every_day_hour;
notify.update(|c| c.every_day_hour = 20)?;           // 读—改—写：落盘 + 回调本插件 on_config_reload
```

拿不到 `ctx` 的地方（模块级自由函数、命令实现）用同一个类型换个入口：
`arona::config::plugin_config::ConfigEntry::<NotifyConfig>::new(crate::PLUGIN_ID, "notify")`，
`plugin` 就填自己的 `meta().id`。读写方法：`get()`（静默回退默认值）、`try_get()`（解析失败给原因，
命令回显用户改坏的配置时用）、`set(&T)`、`update(closure)`、`file()`。
要指向某一套框架实例（隔离测试就该这样）用 `ConfigEntry::<T>::in_store(&framework.configs(), plugin, key)`；
`ctx.config::<T>(key)` 内部就是它。

登记入口也按实例走：插件的 `install` 阶段用 `reg.config::<T>("notify")`（即
`reg.framework().sections().register(..)`），在装配代码之外给某个实例补登记时用
`framework.sections().register(plugin, arona::config::arona::typed_section::<T>("notify"))`。
框架自己的 `config/arona.yml` 同理有两个入口：`arona::config::arona::load(file)` 走默认实例，
`load_in(&framework, file)` 走指定实例（认键、接管旧顶层插件配置键都按那套实例的表来）。

约束与行为：
- 键名要和插件功能对得上，且**不同插件之间不能撞 key**（撞名时保留先登记的；想区分就用带语义的前缀，如 `voice_room_notify`）。
- 空列表渲染成 `key: []`、空映射渲染成 `key: {}`；非空列表按块式缩进写出；字段注释按点分路径落在正确缩进上。
- 缺文件时框架在 `plugin_config::init()` 生成带注释模板；插件升级**新登记的顶层键**会在下次启动按默认值补进
  用户已有的文件（用户改过的值一律不动）；已有键里新增的**子字段**由强类型默认值兜着，下次写回时一起落盘。
- 用户手改 `config/<插件>/arona.yml` 也会被看到：框架的轮询任务发现 mtime 变了就重载该文件，
  并只回调该插件的 `on_config_reload`。`set/update` 之后必定回调，需要即时生效的定时任务在那里重建
  （判"真的变了才重建"，别每次热重载都重建一遍）。
- 旧写法兼容：把 `notify:` 直接写在框架 `arona.yml` 顶层的，加载时会被接管并搬进本插件的 `config/<插件>/arona.yml`，
  框架那份文件同步清掉该键，用户不用手改。
- GUI/`/config` 指令编辑框架自己的那几个键；插件配置区由插件的指令或手改 YAML 维护。
  GUI「插件管理」页会列出每个插件的配置文件与数据目录，并给「打开」按钮。

底层的 `ConfigSection` trait 与 `register_section(plugin, Arc<dyn ConfigSection>)` 仍然公开
（`TypedSection<T>` 就是它的适配器），只在你需要完全自定义渲染时才手写。

## 13. 停用是三层门控，且回收是框架的事

| 层级 | 开关来源 | 生效点 |
| --- | --- | --- |
| 功能（分群） | `arona.yml` → `group_settings.<群号>.disabled_features` | `runtime::config::feature_enabled(group_id, key)`：命令路由，以及 `ctx.listen_feature(..)` 绑了该 key 的钩子 |
| 插件（分群） | `arona.yml` → `group_settings.<群号>.disabled_plugins` | `plugin_enabled_in_group`，命令与钩子（入站、出站）一起停 |
| 插件（全局） | `arona.yml` → `disabled_plugins` | 不 configure、不 start、不路由、不收事件、也不改写别人发的消息 |

`feature_enabled` 先看功能归属插件是否启用（`feature_owner` → `plugin_enabled`），再看该群的功能/插件名单，
所以「整体停用插件」自动覆盖它名下所有功能。命令路由的最后一道兜底是
`plugin::dispatcher_active_in_group(group_id)`：插件被停用时，**没绑定功能 key 的命令也进不去**。

展示层跟着收口，但不改任何判定：GUI「群管理」的功能清单走 `runtime::config::features()`，它会跳过
被全局停用的插件的功能（摆一排永远勾不上的复选框没有意义，要恢复请到「插件管理」页）。
`features_of(plugin)`（插件卡片上"本插件提供哪些功能"，卡片自带 `enabled`）与 `feature_keys_text()`
（写进 `arona.yml` 的"可用功能 key"注释）**不过滤**——停用中也得看得见 key 叫什么名字。

GUI 里这一栏按**提供它的插件分组**（用 `feature_owner` 归类），每组默认收起、点插件名展开/收起：组头的
勾选框一次开关该插件在本群的全部功能（走 `admin::set_group_features`，一批 key 只落盘一次），展开后
仍可单项开关；插件只在本群被停用时整组灰掉，因为那种情况下勾了也不生效。

**`stop()` 只需关掉自己持有的句柄**（数据库连接、文件句柄）。后台任务、定时任务、事件订阅
（入站钩子与出站钩子一起收）、命令、共享能力（容器）、服务开关条目
由框架在 `stop()` 之后按归属统一回收（`manager::revoke_resources`），日志会写明各收了多少：

```rust
fn stop(&self, _ctx: &PluginContext) {
    db::close();      // 仅此而已；任务/钩子/命令/容器/服务开关框架会收
}
```

回收的正是装配的镜像，所以**运行中被停用与进程退出走同一条路**，不存在"退出时才清理"的特例。

## 14. OneBot 动作接口（发/撤/查/管，全量强类型）

框架把 OneBot v11 的动作封成 `arona::onebot::api::OneBotApi`，插件与 GUI 用同一份能力：

```rust
let api = ctx.api();                              // 绑本插件所属那套实例，每次动作现取该实例的首个可用连接
let members = api.get_group_member_list(group_id).await?;
api.send(MessageTarget::Group(group_id), OutgoingMessage::text("老师")).await?;
api.delete_msg(message_id).await?;
```

`CommandContext` 与 `EventContext` 上都有 `api()`，命令与钩子里直接 `ctx.api()` 就行。
**这条通道在插件 dll 里是真通的**：连接池挂在 `Framework` 实例上（`Framework::onebot()`），
dll 认领了宿主实例，所以动作落在宿主真正那条连接上，不必宿主侧替你转发（判据见 §22）。
出口本身只多揣着一个实例引用：`OneBotApi::of(&实例)` 给指定那套，`global()` 给进程默认那套，
`ctx.api()` / `bot.api()` 给**构造这个句柄的那套**——多实例宿主与测试里不会串到别家连接上。

`OneBotApi` 里不存连接，所以攥着它跨一次 `onebot.yml` 热重载没问题，下一次动作自然取到新连接；
要别缓存的是 `Arc<dyn OneBotConnection>` 那一层连接对象，热重载之后旧的已经销毁了。
这里所有发送方法都是**协议层直连**：不出站钩子（§9.1）、不打印、不拆合并转发、不带自动撤回。
要走框架那条完整的出站路（含 §9.1 钩子、控制台打印、合并转发拆分、`revoke_after_millis` 自动撤回、
出站记账），用 `arona::runtime::services::send_message(..)` 或 §14.2 的对象模型。
错误统一是 `OneBotError::{NoConnection, Timeout, Failed{..}, BadResponse(_)}`，实现了 `Display`，
直接 `?` 上抛或 `.map_err(|e| e.to_string())` 写日志都行。

按能力分类的方法（全部 `async`，返回强类型；细节看 `crates/arona/src/onebot/api.rs`）：

| 能力 | 方法 |
| --- | --- |
| 发消息 | `send(target, OutgoingMessage)`、`send_msg`、`send_private_msg`、`send_group_msg`（消息载荷接受 `&str`/`String`/`OutgoingMessage`/段数组/已拼好的 JSON —— `MessagePayload`） |
| 读消息 | `get_msg`、`get_forward_msg`、`can_send_private_msg`、`can_send_group_msg` |
| 删/赞/已读 | `delete_msg`（撤回）、`set_msg_emoji_like`、`send_like`、`mark_msg_as_read` |
| 语音 | `translate_voice(message_id)` 语音转文字（各家入参不统一，详见方法上的注释） |
| 资料 | `get_login_info`、`get_stranger_info`、`get_friend_list`、`get_version_info`、`get_status` |
| 群 | `get_group_info`、`get_group_list`、`get_group_member_info`、`get_group_member_list`、`get_group_honor_info`、`get_group_system_msg`、`get_group_at_all_remain` |
| 群管 | `set_group_kick`、`set_group_ban`、`set_group_whole_ban`、`set_group_admin`、`set_group_anonymous_ban`、`set_group_name`、`set_group_level`、`set_group_card`、`set_group_special_title`、`set_group_notice`、`get_group_notice` |
| 请求 | `set_group_add_request`、`set_friend_add_request` |
| 精华 | `get_essence_msg_list`、`add_essence_msg`、`delete_essence_msg` |
| 群文件 | `upload_group_file`、`get_group_root_files`、`get_group_files_by_folder`、`get_group_file_url`、`delete_group_file`、`create_group_folder`、`delete_group_folder` |
| 内部 | `get_cookie`、`get_csrf` |
| 兜底 | `call(action, params)` 拿原始 `data`；`call_typed::<T>(..)` 反序列成自己的类型；`call_ok(..)` 只要成功/失败 |

返回值结构体都用 `serde(default)` + `rest` 收未知字段：实现端少回某字段不会失败，多回的字段也拿得到。
`Option` 入参传 `None` 时**不会**塞进 JSON，免得实现端把 `null` 当成有效值。

### 14.1 出站 HTTP：用框架的客户端，别自带 reqwest

插件要访问普通网页或第三方 API（不是 OneBot 动作）时用 `arona::runtime::http`：

```rust
use std::time::Duration;

// GET 一段文本
let text = arona::runtime::http::get(url, &[("referer", referer)]).await?;
// GET 一段二进制（头像、攻略图）
let bytes = arona::runtime::http::get_bytes(url, &[] as &[(&str, &str)]).await?;
// 表单 POST / JSON POST
let reply = arona::runtime::http::post_form(url, &[] as &[(&str, &str)], &[("id", "12")]).await?;
let echo  = arona::runtime::http::post_json(url, &[] as &[(&str, &str)], r#"{"a":1}"#).await?;
// 要超时、要自定义正文类型就走 Request
let raw = arona::runtime::http::send(
    arona::runtime::http::Request::get(url)
        .with_header("x-game", "ba")
        .with_timeout(Duration::from_secs(8)),
)
.await?;
```

一律返回 `Result<_, String>`：非 2xx、连不上、超时都算失败，原因里带着 URL，`?` 和 `match` 都顺手。
未指定超时用的是宿主的 60 秒。

**为什么不让插件自己 `reqwest::Client`**：dll 把 reqwest/hyper/tokio 静态链接成了第二份，而 hyper
会自己 `tokio::spawn` 后台任务（补连接、空闲驱逐）——框架只能给"自己投出去的任务"逐帧补装上下文，
管不到第三方库内部派生的那些。实测结果是插件**开着连接池**发请求就炸 `there is no reactor running`，
而且炸在池子的锁里，之后每个请求都跟着废掉。走这条路的请求由**宿主那份**客户端发出、跑在宿主的常驻
runtime 上，连接池和 keepalive 全开也不会炸；插件的 `Cargo.toml` 因此不必带 `reqwest`，dll 还小一圈。
原理与实证见 §22。

宿主自己（以及从没被装载过的单元测试）没有这道边界，同一套函数直接落到本地客户端，写法一个字都不用改。

### 14.2 对象模型：`arona::bot` 里的 `Bot` / `Group` / `Member` / `Friend` / `MessageSource`

mirai 的手感是"先拿到一个对象，再往对象上说话"，而不是到处传 `GroupCode`/`UserOrBot`。
框架按同一套形状给了 `arona::bot`（实现见 `crates/arona/src/bot/mod.rs`）：一句 `ctx.bot()`
拿到机器人，之后的取资料、发言、禁言、撤回、引用都挂在对象上。

| mirai (Kotlin) | Arona (Rust) | 说明 |
| --- | --- | --- |
| `Bot.getInstance()` | `arona::bot::Bot::current()`，或 `ctx.bot()` | 句柄只装一个所属 `Framework` 实例的引用，克隆零成本 |
| `bot.getGroupOrFail(id)` | `bot.group(id)` | **不查网络**，只给句柄；群名一类资料用 `group.info().await` 现取 |
| `bot.groups` | `bot.groups().await` | `get_group_list` |
| `bot.getFriend(id)` / `bot.friends` | `bot.friend(id)` / `bot.friends().await` | 前者同样不查网络，所以没有昵称与备注 |
| `group.members` / `group[id]` | `group.members().await` / `group.member(id).await` | 后者会查一次实现端，确认这个人真在群里 |
| `member.nameCard`（读内存里的缓存，零网络） | `group.member_info(id).await` / `member.info().await` | 走实例上的 `PeerCache`：同一个人在同一个群里 60 秒内只打一次网络，`group.forget_member(id)` 立即作废；`bot.user(id)` 那份陌生人资料留 10 分钟。身份门控（§8 的 `group_role()`）读的是同一份，所以"先判身份再取昵称"只花一次往返 |
| `bot[qq]` 的陌生人/好友资料 | `bot.user(id).await` | 同上，NTQQ 系实现端这个动作偏慢，所以缓存给到 10 分钟 |
| `group.history`（可分页的历史） | `group.history(20)` / `group.history_before(20, cursor)` | 读的是 §21 那份框架留档，`HistoryPage::cursor()` 给 `None` 表示翻到头了；`Friend` 上同名 |
| `Image.upload()` 回来的 `Image`（同一张图不重传） | `arona::bot::Image`：`from_file_id` / `from_url` / `from_file` / `from_bytes` / `from_segment` / `from_stored` | 见 §19。OneBot 没有"先传上去换 id"这一步，能复用的 `file_id` 只可能来自入站消息或框架留档 |
| —— | `group.member_of(id)` | 只拿句柄不查网络，紧接着要禁言/踢人时省一次往返 |
| `subject.sendMessage(chain)` | `group.send(msg).await` / `friend.send_text("…").await` | 走框架的出站通道（§9.1 钩子、打印、拆分、自动撤回、记账），**不是**协议层旁路 |
| `subject.sendMessage(..)` 返回 `MessageSource` | 同上，返回 `MessageReceipt`，再 `receipt.source()` → `Option<MessageSource>` | mirai 那一步直接给凭据；这里发出去的东西先给回执（`message_id`/`real_id`），`.source()` 换成能撤回、能引用的凭据。一个号都没给时是 `None` |
| `message.source()` | `ctx.source()` → `Option<MessageSource>` | 实现端没给 `message_id` 时是 `None`，不编造 |
| `source.recall()` | `source.recall().await` | 内部按 §21 的库换成实现端认的那个号 |
| `QuoteReply(source.toQuote())` | `source.quote()` / `source.reply(msg).await` | 前者只给你一个 `MessageSegment::Reply`；后者过窗时自动还原成文字+图 |
| `member.mute(600)` / `unmute()` | `member.mute(600).await` / `member.unmute().await` | 秒；`0` 即解禁 |
| `member.kick()` / `member.nameCard = ..` | `member.kick(reject_add).await` / `member.set_card("…").await` | |
| `group.settings.isMuteAll = true` | `group.mute_all(true).await` | |
| `group.publishAnnouncement(..)` | `group.set_notice(content, image, pinned).await` | |
| `Temp` 会话 | `member.temp_target()` / `member.send_temp(msg).await` | 对应 `MessageTarget::Temp`，见下 |
| `source.markRead()` | `source.mark_read().await` | 实现端不支持时返回 `Failed`，消息本身不受影响 |
| `bot.callApi(..)` | `ctx.api()` / `bot.api()` | §14 那张全量动作表，本模块没包到的动作走它。出口**绑的是构造这个句柄的那套实例**（`OneBotApi::of(&实例)`），所以多实例宿主与测试里各套句柄拿到的是自己那套的连接与 `self_id` |

```rust
use arona::bot::Bot;
use arona::runtime::message::{MessageSegment, OutgoingMessage};

// 发：群、好友、临时会话
let bot = Bot::current();
bot.group(123456).send_text("老师们好").await;
bot.friend(10001).send_text("单独说一下").await;
// 对方不是好友时裸私聊常被实现端拒发，临时会话（带来源群号）才是那条正路
bot.group(123456).member_of(10001).send_temp(OutgoingMessage::text("在吗")).await;

// 查：句柄自己不存东西，但读回来的资料会落在实例的缓存上，60 秒内同一个人不再打网络
let group = bot.group(123456);
let name = group.info().await?.group_name;             // 群名不缓存，每次现查
for member in group.members().await? {                 // get_group_member_list 全量
    println!("{} {}", member.user_id, member.display());
}
let card = group.member_info(10001).await?.display();  // 首次现查，第二次起命中缓存
group.forget_member(10001);                            // 刚改完名片就要读回来，别等 60 秒

// 历史：读的是 §21 那份框架留档，游标为 None 就是翻到头了
let page = group.history(20);
for record in &page.records {
    println!("{}: {}", record.name, record.text);
}
if let Some(cursor) = page.cursor() {
    let older = group.history_before(20, cursor);      // 接着往前翻
}

// 管：mirai 的 member.mute(600) 一对一
group.mute(10001, 600).await?;
group.set_card(10001, "改过的名片").await?;
group.mute_all(true).await?;

// 自己发出去的那条也能立刻撤回
let receipt = group.send_text("这条是回执实验").await;
if let Some(mine) = receipt.source() {
    mine.recall().await.ok();
}

// 撤与引用：凭据从上下文来
async fn demo(ctx: &arona::runtime::dispatcher::CommandContext) {
    if let Some(source) = ctx.source() {
        source.reply_text("收到，这条是引用回复").await;   // 过窗自动还原成文字+图（§21）
        source.recall().await.ok();                        // 撤回触发这条
        let quoted: MessageSegment = source.quote();       // 只想要引用段，自己拼消息
        ctx.reply_message(OutgoingMessage::new(vec![quoted, MessageSegment::Text("好".into())])).await;
    }
}
```

三条边界值得记住：

1. **`send` 与 `api().send_*` 不是一回事**。对象模型上的 `send` 走框架出站通道，会被 §9.1 的
   出站钩子改写或拦停、会打控制台、会拆合并转发、会按 `revoke_after_millis` 自动撤回、会记进
   §21 的库；`ctx.api().send_group_msg(..)` 是协议层直连，这些一概不做。除非确实要绕开钩子，
   否则发消息一律用前者。
2. **句柄不代表"已确认存在"**。`bot.group(999999)` 永远成功，哪怕没这个群；真正的判据是
   `group.info().await` 这类会说话的调用。这么设计是因为实现端取资料都是网络动作，
   框架不该替你隐式发请求。
3. **凭据比 mirai 的 `MessageSource` 活得久**。mirai 那份只在内存在，机器人一重启就再也引用不到
   旧消息；这里的 `source.recall()` / `source.record()` 会回落到框架的聊天记录库，
   只要那条被记过档就还认得（库里也没有时 `record()` 给 `None`）。

`MessageTarget` 因此有三个变体：`Group(群号)`、`Private(QQ 号)`、
`Temp { group_id, user_id }`（临时会话）。入站事件里各家实现端都不标临时会话，所以
**收到的**消息只会是前两种；`Temp` 只在出站时用，`target.group()` 会把它报成来源群，
门控（§13）与出站记账都按那个群算。

## 15. 新增一个插件

插件是**独立仓库、独立 crate**：不进框架的 workspace，也不需要框架侧改任何文件。

1. 建 crate，`Cargo.toml` 里两处关键设置：

   ```toml
   [lib]
   crate-type = ["cdylib", "rlib"]   # cdylib 出 dll；rlib 那份只给 `cargo test` 用

   [dependencies]
   # 务必 default-features = false（§16）。arona 的版本必须与目标宿主完全一致
   arona = { package = "arona-rs", version = "1.1.0", default-features = false }
   # 类型化配置要 derive（§12）；命令/钩子的 async 体里要 tokio::time::sleep、select! 也得直依赖
   # （§22 的"两份 tokio"小节：框架按帧给这类任务补装运行时上下文）。
   # 起后台任务请用 ctx.spawn（§10），别裸 tokio::spawn，更不要 #[tokio::main]——运行时是宿主那一个
   serde = { version = "1", features = ["derive"] }
   tokio = { version = "1", features = ["time", "rt", "macros"] }
   ```

   `package = "arona-rs"` 那一行是本文档唯一需要解释的写法：**发布名是 `arona-rs`**
   （`arona` 这个名字在 crates.io 上早年被别的项目占了），而**库名仍是 `arona`**，
   所以源码照旧 `use arona::...`、feature 照旧写 `arona/gui`，只有 Cargo.toml 那一行带 `package`。
   不想依赖 crates.io 时改成 `arona = { package = "arona-rs", path = "../Arona-rs/crates/arona", … }`
   指到本地 clone（§17 的同源联调就是这么做的）；用 `git = "<框架仓库地址>", tag = "…"` 也行，
   但**标签必须 ≥ `v1.1.0`**：`v1.0.0` 里还没有 §14.2 的对象模型，`ABI_LAYOUT` 也停在 3，
   指着它写插件会一片 `unresolved import`、就算编过也会被装载时拒载。

   另两条口径：`serde_json` 框架**不 re-export**，插件要自己 `json!(...)` 就得直依赖。它并非"只在自己家里用"——
   卡片段 `MessageSegment::Json(Value)` / `Xml(Value)` 与兜底段 `Raw { data: Value }` 装的就是这个类型，
   于是它会跨过 dll 边界。握手核对的五项（§22）**不看**第三方 crate 版本，所以请自己跟框架同大版本
   （根 `Cargo.toml` 里是 `serde_json = "1"`）；两边版本差出布局就是两份不同的 `Value`，编译器不会拦。
   `reqwest` **不要装**，出站一律走 `arona::runtime::http`（§14.1）。

   再建一份 `.cargo/config.toml`，把框架仓库根目录那份的 `[target.*]` 段落**原样抄过来**：

   ```toml
   [target.x86_64-pc-windows-msvc]
   rustflags = ["-C", "target-feature=+crt-static"]
   ```

   CRT 口径不同会让两边各带一份运行库，跨模块分配/释放内存直接炸；框架的握手也会在这里拒载。
2. 实现 `AronaPlugin`（`meta` 必填 id + name + version）。
3. 在 `src/lib.rs` 末尾导出一行入口：

   ```rust
   arona::export_arona_plugin!(MyPlugin::default());
   ```

   宏负责导出四个 C 符号（`arona_plugin_abi` / `arona_plugin_toolchain` / `arona_plugin_host` /
   `arona_plugin_new`），框架靠它们完成握手、把宿主的进程实例、日志出口和 HTTP 代发通道交进 dll，
   再取出实例。参数只要是「造出实例的表达式」即可：`MyPlugin` 有字段时写 `::default()` 或自己的
   `::new(..)`，是空单位结构体就直接写类型名（否则 clippy 的 `default_constructed_unit_structs` 会念一句）。
   **插件不要写 `main`、也不提供任何可执行入口**——
   插件只能被框架装载、不能独立运行，这点与 mirai 一致。
4. 构建并投放：

   ```bash
   cargo build --release                              # 产物 target/release/my_plugin.dll
   cp target/release/my_plugin.dll <宿主运行目录>/plugins/my_plugin/
   ```

   启动框架即自动装载。推荐让每个插件独占一层以 id 命名的子目录（`plugins/<id>/`），
   这样随包资源能跟 dll 放在一起；直接把 dll 丢在 `plugins/` 根下同样能被发现。

装载成功的标志是这三行日志：

```
[Arona] 插件 ABI 握手通过: 布局 5，契约 1.0.0
[Arona] 已装载动态插件: HelloPlugin 0.1.0 (hello_plugin.dll)
[Arona] 插件已启动: HelloPlugin v0.1.0
```

那个「布局 5」是当前符号布局版本（§22），每次抬它这里的打印就跟着变，核对时以 §22 为准。

`plugins/<id>/plugin.yml`、`config/<id>/arona.yml` 由框架在装载时自动写出来，GUI「插件管理」页
立刻能开关它。换插件、删插件都不必重新编译框架，**但必须重启框架**——`plugins/` 只在启动时扫一遍，
运行期不做热插拔（卸载 dll 不安全；"停用"只回收该插件名下的资源，模块照常常驻）。

## 16. feature 统一陷阱：GUI 只在 host 打开

`arona` 的 `default = ["gui"]` 会拉进 `eframe`/`wgpu`。**插件必须以 `default-features = false` 依赖 arona**，
否则一个只会打日志的插件也要把整套 GUI 编译一遍。宿主那边由自身的 `gui = ["arona/gui"]` 打开面板，
`default = ["gui"]` 决定产物带不带管理界面。

静态链接时代这条只是"别让 feature 顺藤摸瓜扩散出去"；换成动态装载之后它同时是个 **ABI 问题**——
`Framework`、`PluginContext` 这些跨边界类型的字段布局，两边必须一模一样。
所以 `gui` 被特殊对待：它只门控 `arona::gui` 与两个 GUI 启动辅助模块，不参与插件可见的任何类型布局，
因此**不进 ABI 指纹**（见 `crates/arona/build.rs` 的 `ABI_NEUTRAL_FEATURES`，那里还放了它的别名
`default`——别名本身不改变代码）。
这条前提是被盯住的不变量——`plugin::abi` 里有用例扫遍源码，谁往 `plugin/`、`framework.rs`、`runtime/`
之类的地方塞 `#[cfg(feature = "gui")]` 成员，用例就会红。
除这两个之外的任何 feature 都算进指纹，两边差一个就直接拒载。

## 17. 开发与调试

框架仓库里（`cargo run` 的运行目录是 `target/debug`，`--release` 时是 `target/release`）：

```bash
cargo run -p arona-host                  # 默认打开管理面板 GUI
cargo run -p arona-host -- --nogui       # 纯命令行模式（黑窗口）
cargo run -p arona-host -- --test-notify # 20 秒后跑一次每日推送，便于联调（它走的是插件**主动**发消息那条路，正好用来验 §22 那两处状态是不是真的接进了宿主）
cargo build -p arona-host --no-default-features   # 精简命令行版
cargo build-plugin                       # 编示例插件：target/release/hello_plugin.dll
```

插件仓库里：

```bash
cargo build --release    # 改一行代码就是"重编 + 重拷 dll + 重启框架"这三步
cargo test               # rlib 那份给单测用，纯逻辑不必起宿主
```

日常联调就是 `cp target/release/<插件>.dll <宿主运行目录>/plugins/<id>/` 后重启框架，
看启动日志有没有那三行「装载成功」的输出（§15）。装载失败一定会有明确的中文原因，
不会静默不出现——先查日志再怀疑代码。

改动后的验证口径（本地 GUI 构建需要 ATL，见下节"零依赖"约束；纯逻辑改动用 `--no-default-features` 更快）：

```bash
cargo check -p arona-rs --features gui --all-targets   # 含 GUI 代码与测试（-p 用包名 arona-rs）
cargo test --workspace                              # 单测（生命周期门控/配置迁移/钩子/隔离实例）
cargo test --release --workspace                    # CI 口径：AutoUploadReleaseBuild.yml 跑的是 release
cargo clippy --workspace --all-targets              # 框架侧 error 级必须清零
cargo fmt --all
```

或用 `.cargo/config.toml` 里的别名：`cargo build-release` / `cargo build-nogui` / `cargo build-plugin` /
`cargo gui` / `cargo run-nogui` / `cargo dist`（整理交付产物）/ `cargo installer`（打安装包）。
本地联调 OneBot 时，ws-forward（框架连出去到一个 WS 服务）比 ws-reverse 好驱动：
后者在本项目里不把 `admin::call_api` 路由给已连接客户端。

断点与 panic 定位：dll 是被宿主进程映射的，IDE 里 **attach 到 `arona-rs.exe`** 就能停在插件源码上；
插件仓库若要调试发布构建，加一段 `[profile.release] debug = 1` 再重编 dll。
框架侧的启动路径（`arona-host`）与插件仓库不在同一个 workspace，直接把 host 的 `Cargo.toml`
用 `arona = { package = "arona-rs", path = "../Arona-rs/crates/arona" }` 指过来即可同源联调。
本地运行若被提权逻辑拦住，用 `ARONA_NO_ELEVATE=1` 绕过。

GUI 相关的落地细节（改动前务必先读）：
- Windows 下含 GUI 的构建带 `#![windows_subsystem = "windows"]`，**不弹控制台**；命令行模式由
  `runtime/console.rs` 接回上级终端或另建控制台。核对产物可看 PE 头 subsystem：GUI 版应为 2，
  `--no-default-features` 版应为 3。
- 依赖全部内置，目标机不装任何东西也能启动：`+crt-static` 静态链接 MSVC 运行库，
  `d3dcompiler_47.dll` / `opengl32.dll` 走 `/DELAYLOAD`，wgpu 用 **DynamicDxc**（按需 LoadLibrary
  exe 同级的 `dxcompiler.dll` / `dxil.dll`，由 `scripts/fetch-dxc.ps1` 拉取、安装包与 zip 一并分发）
  或系统 FXC，**绝不用静态 DXC**（静态 DXC 会引用 ATL 的 `_AtlBaseModule`，逼人装 VS 的
  「ATL/MFC」组件——**不要走这条路**）。
- 渲染后端按候选阶梯**从高到低**逐个降级：`glow`(硬件 OpenGL) → `wgpu(DX12/Vulkan+随包 DXC)` →
  `wgpu(全部后端+DXC)` → `wgpu(+FXC)` → `glow(软件 OpenGL llvmpipe，即随包的 softgl\)`，
  exe 同级没有 DXC 两个 dll 时自动跳过 DXC 两档。服务器/无显卡环境靠这个兜底。
- 两层兜底都要留着：进程内的**卡死看门狗**（后端超时不出窗口就换下一个后端另起进程）+
  `catch_unwind`；以及**进程级的闪退看门狗**（父进程盯子进程退出码，被驱动直接干掉时改用 CPU
  软件渲染重启，用 `ARONA_GUI_GUARDED` 防止自己盯自己）。只留前者不够——硬闪退时什么都写不出来。
- 窗口尺寸是「逻辑点」，由 winit 按屏幕缩放比换算；前提是 `assets/arona.manifest` 声明了
  PerMonitorV2 DPI 感知，别删那几行。

## 18. 写插件代码时的注意事项

- **不要给死代码加 `#[allow]`**：插件里保留了不少尚未接线的功能模块，`cargo` 报的
  `never used` 警告是预期的，不要为了"零警告"去删或压制。
- 契约面的九张表（命令、钩子、服务容器、服务开关、功能与停用名单、配置区登记、配置文件缓存、
  定时任务、插件表）都由 `Framework` 实例持有（§2 末），**不再是进程级全局**。
  所以测试的隔离手法是**造实例**而不是抢串行锁：

  ```rust
  use std::sync::Arc;

  let framework = arona::framework::Framework::new();      // 一套全空的注册表
  let manager = framework.plugins().clone();               // 或 PluginManager::new()，等价
  // register 收的是 Arc<dyn AronaPlugin>：裸实例编不过，`arona::plugin::register(..)` 同理
  manager.register(Arc::new(MyPlugin));
  assert!(manager.install_all().is_empty());
  // 停用名单/命令表按实例读写：framework.gating() / framework.commands()
  ```

  涉及落盘配置的用例再给每条一个**唯一 plugin id**——路径是 `config/<id>/arona.yml`，
  id 撞了才会互相删对方的文件；入口也指到隔离实例上：
  `ConfigEntry::<T>::in_store(&framework.configs(), id, key)`、`arona::config::arona::load_in(&framework, file)`、
  `framework.sections().register(..)`。这样用例之间真并行，`#[test]` 就够，不必 async。
- 只有**确实要测进程默认实例**的用例才需要串行锁（框架里 `runtime::log`、`runtime::console`
  各留了一把，锁的注释写明了守的是什么）。往默认实例里塞测试插件是错的写法。
  异步用例统一 `#[tokio::test(flavor = "current_thread")]`。
- 插件代码里取注册表走 `ctx`（`ctx.framework()` / `ctx.service_board()`），不要用 `X::global()` 那套
  自由函数——它们只转发到进程默认实例，用了就等于绕开了隔离。dll 插件里那份默认实例已被宿主的
  上行通道改指过来（§22），不会写进空表，但单测隔离照样做不出来。
- 生命周期里的闭包要 `'static`：需要 `ctx` 时先 `let context = ctx.clone();` 再 `move` 进去
  （`PluginContext` 是 `Arc` 包装的廉价克隆）。
- 插件代码里不要 `std::process::exit`，也不要长阻塞主线程；耗时活计用 `ctx.spawn(..)`（§10）或 `arona::quartz`。
- 日志走 `arona::runtime::log::{info, warning, error}`，不要用 `println!`（GUI 模式没有控制台）。
  调试用的 `runtime::log::debug(..)` 只在控制台开着时输出，不进日志文件。
- 行首前缀由框架按「当前来源」填，插件不要自己拼：插件代码里打的挂 `[插件显示名:动作]`（控制台与 GUI 染淡紫），
  框架自己打的挂 `[Arona]`（亮绿）。来源在框架进入插件代码的那一次调用上切换（生命周期回调、命令、钩子、
  定时任务），跨 `await` 的后台任务由框架逐次 poll 时重设——所以任务体里 `ctx.spawn(..)` 起的活儿同样认得归属。
- 框架给的动作为粗粒度（按入口）：`装配`、`启动`、`配置重载`、`命令 活动`、`事件 群消息`、`定时 每日任务名`…
  插件比框架清楚自己那一步在干什么，关键动作再包一层就精确到「发送消息」「同步生日」「踢人」这个粒度：
  - 同步段：`arona::plugin::action("踢人", || { .. })`
  - 一段异步活儿：`arona::plugin::action_async("发送消息", services::send_message(target, msg)).await`
    （整段每次轮询都带着动作名，中间的 `await` 不会把它丢掉）
  - 后台任务：`ctx.spawn_as("定时推送", async { .. })`，动作名随任务下发，比 `ctx.spawn` 多一个标签
  动作只在当前来源确实是已登记插件时生效，框架自己的日志始终是 `[Arona]`；嵌套时以最内层为准。

## 19. 消息段全谱：收与发用同一套类型

对应 mirai 的 `MessageChain` / `Element`。`arona::runtime::message::MessageSegment` 共 15 段，
入站（`EventContext::segments`）与出站（`OutgoingMessage`）都是它，插件不需要碰 JSON：

| 段 | 入站来自 | 出站映射（`onebot::protocol::segment_to_json`） |
| --- | --- | --- |
| `Text` | `text` 段 / CQ 码外的裸文本 | `{"type":"text","data":{"text":…}}` |
| `At(qq)` / `AtAll` | `at`（`qq` 是数字或字符串，`"all"` → `AtAll`） | `at` + `qq` |
| `Reply(id)` | `reply`（别人引用了某条消息） | `reply` + `id`（出站即"引用回复那条"） |
| `Image{url,file,data,file_id}` | `image`（`file_id` 与实现端别名 `substitute` 都还原到第四个字段） | `image` + `file`（取值优先级见下），有 `file_id` 时一并带上 |
| `Face(id)` | `face`（QQ 表情号，字符串保留） | `face`：能转 `i64` 就发数字，否则原样 |
| `Record{url,file}` | `record` / `voice`（两种写法都认） | `record` + `file` |
| `Video{url,file}` | `video` | `video` + `file` |
| `File{file_id,name,size}` | `file`（群文件上传） | `file` + `file_id`/`file_name`/`file_size` |
| `Poke{name,target}` | `poke`（`type`/`target_id`，也吃 `target`） | `poke` + `type`/`target_id` |
| `Location{name,address,latitude,longitude}` | `location`（协议键是 `lat`/`lon`，数字或字符串都行） | `location` + `name`/`address`/`lat`/`lon` |
| `Json(card)` / `Xml(card)` | `json`/`xml`：`data.data` 给对象直接用，给 JSON 字符串会先解析 | `json`/`xml` + `data` |
| `Forward{title,id,messages}` | `forward`/`node`（递归解析 `content`，空内容整段丢弃；`id` 是实现端给的 flag，可拿它调 `get_forward_msg` 取全量节点，`messages` 常为空） | 普通发送接口不支持合并转发 → 占位文本；真发送见下 |
| `Raw{kind,data}` | 框架还不认识的段（新版实现端自己加的类型）：不再丢弃，原样留在这里，显示成 `[kind]` | `{"type":kind,"data":data}` 原样拼回，转发/重发不丢信息 |

图片/语音/视频的统一取值（`media_value`）：**URL > 内存字节 > 本地文件**。
内存字节和本地文件都编成 `base64://…`；只有 `config/onebot.yml` 顶层的 `send_image_as_file: true`
（同机部署用的"图片按文件直传"）且文件确实存在时才改成 `file:///…` URI，免去一次大图 base64，
实现端按原始文件上传因此不被压缩。中文与空格路径按 RFC 8089 百分号编码，Windows 盘符冒号保持原样。

两张对照表读法：第一列是 **Rust 字段名**（`match` 时写的就是它），最后一列是 **OneBot JSON 键**，
两者不完全同名——位置段的 `latitude`/`longitude` 上线变成 `lat`/`lon`，图片段只有 `file_id` 两边同名。
另外"只给了 `file_id`、没有 URL/路径/字节"时框架会把这个值同时写进 `data.file`：
NapCat 读 `data.file_id`、LLOWeb 读 `data.file`，双写才两边都认。

`arona::bot::Image`（§14.2）是图片段那一行的**可复用句柄**：`Image::from_segment(&segment)` 把收到的
几种线索原样取出来（直链、本机路径、字节、`file_id`，在这一步不猜该用哪个、也不丢），`image.segment()`
挂回要发的消息，`OutgoingMessage::from(image)` 是它的简写。它不含连接状态、是 `Clone` 的，可以放心放进
插件自己的缓存长期持有。**框架没有 mirai 的 `Image.upload()` 那一步**：v11 没有"先传上去换个 id 回来"
的标准动作，发图时由实现端自己上传，所以能复用的 `file_id` 只可能来自入站消息或 §21 的留档
（`Image::from_stored`，认 `fileid://<id>`、http(s) 直链与本地路径三种写法）。同一张图第二次发可以带上
同一个 `file_id`，至于实现端是否因此省下重传，框架不替它保证——要确定性不重传就自己留 `from_file` 那条路。

出站构造：`OutgoingMessage::{new, text, at, at_all, quoted, image_file, image_data, image_reuse,
record_file, video_file, json_card, xml_card, forward}`；链式改动有两个：`.with_quote(消息 id)` 前置一个
引用段、`.with_revoke(毫秒)` 让框架发完自动撤回。
`ctx.reply(..)` / `context.reply_message(..)` / `api.send(target, msg)` 都吃它。
区别在于走哪条路：前两者（以及 §12 的延时推送）走框架统一发送口，会先过 §9.1 的出站钩子、`.with_revoke(..)` 生效；
`api.send(..)` 是协议层直连出口，不过门、`.with_revoke(..)` 在它身上无效。

合并转发（`Forward` 段）由发送器自动分流：`onebot::message_sender` 把一条消息里的 `Forward` 摘出来走
`send_forward_msg`，其余段照常发送，所以插件把合并转发当普通段拼进消息链就行，不用自己分两次调接口。

入站解析两条路都通：实现端给 `message` 数组（推荐，框架内部的逐段还原函数认不出的类型留成
`Raw{kind,data}` 而不是丢掉；该函数不对外，插件只管 `match` 段）或只给 `raw_message` CQ 码
（`decode_cq_message`：按 `[CQ:类型,键=值]`
还原，`&#44;/&#58;/&#93;/&amp;` 实体自动解转义，未知 CQ 码整段留成原文文本，绝不吞字）。
两条路都只是"不丢信息"，**不保证你认得**：要按类型分支处理，`match` 的兜底分支请写
`MessageSegment::Raw { kind, .. }`，别写 `_ => {}`。
带字段的变体同理请留 `..`：图片段的 `file_id` 是布局 5 才加上的第四个字段，写成
`MessageSegment::Image { url, file, .. }` 就吃得到下一次加字段，把字段列全则会让框架每抬一次布局
就把你插件打崩一轮（抬布局的规矩见 §22）。

`MessageSegment::is_mention_of(self_id)` 判断"这是在召唤机器人吗"（@机器人 与 @全体都算），
`command_text(event, self_id)` 就是靠它剥命令前缀的（§8）。

## 20. panic 隔离、自动停用与框架构造选项

mirai 用 `broadcastAndDumpInterceptedExceptions` 保证"一个订阅者炸了不影响别人"。
Arona 的等价物在框架侧：**框架会调用的插件入口**全被兜住——命令处理器与命令兜底、入站事件钩子
与出站钩子过 `guarded`（`poll_fn + catch_unwind(AssertUnwindSafe(..))`，把 async 处理器的 panic
就地吃掉，**既不拖垮 tokio task，也不中断这一批事件的其余订阅者**）；`install / configure / start`
的 panic 折算成该阶段失败并走回收，`stop / on_config_reload` 只记日志、绝不打断框架后续的回收流程；
定时任务 panic 只跳过本次，仍按表调度。三处都往同一张健康度面板记账。

一个有意的例外：`ctx.spawn(..)` 起的后台任务是插件自己的独立 task，框架**不**兜它的 panic——
那等于把插件里随便一段循环变成"看起来还在跑、其实早就停了"的僵尸，所以 panic 只结束那个任务，
也不计入健康度。要在任务里连续存活就自己 `while` 循环 + 捕获，或者干脆用定时任务（§10）。

记账的是 `arona::plugin::health::HealthBoard`（挂在框架实例上，`framework.health()`）：

```rust
framework.health().failures("voice-room");     // 当前连击数（成功一次即清零）
framework.health().forget("voice-room");       // 插件重新装配时清空
```

同一插件**连续** panic 到达阈值（默认 5 次）时，框架先把该插件写进停用名单（命令与钩子立刻不再路由，
不需要 `Framework` 已 attach），再登记隔离原因（`framework.quarantine(id, threshold)` 返回 `true` 表示
"本次从可用变隔离"，只有进程默认实例会把它写进 `arona.yml`），日志写明炸在哪个命令/钩子上。
`revoke_resources` 顺手 `health().forget(id)`，所以用户从 GUI 或 `arona.yml` 重新启用它时计数从零开始——
**不会因为一个老计数被秒停用**。插件自己不需要 `catch_unwind`，但也不该拿 panic 当控制流。

构造选项（mirai 的 `MiraiInstance.new { .. }`）：

```rust
let framework = arona::framework::Framework::builder()
    .panic_disable_threshold(3)      // 连续 3 次 panic 即停用；0 = 只记日志不停用
    .prefix_match_by_default(true)   // 全部命令开放最短前缀匹配（默认关）
    .command_prefixes(["/"])         // 命令名写法前缀；空列表 = 允许裸名（见 §8）
    .build();                        // 返回 Arc<Framework>，与 global() 那套完全隔离
```

读回来用 `framework.options()`，隔离名单看 `framework.health()`。

**进程默认实例的这两项由配置文件给**（部署时不必重新编译）——框架 `config/arona.yml` 末尾的
`framework:` 段，加载和热重载时写进默认实例：

```yaml
framework:
  panic_disable_threshold: 5      # 同一插件连续 panic 多少次就自动隔离停用（0 = 只记日志、不停用）
  prefix_match_by_default: false  # 未声明 with_prefix_match 的命令是否也参与最短前缀匹配
```

两项都是活的：阈值改完下一次 panic 就按新数计；前缀开关是在**匹配那一刻**读的，
所以改配置对启动时就登记好的命令同样生效（显式 `with_prefix_match(..)` 的命令不受全局开关摆布）。
`Framework::new()` / `builder()` 造出来的隔离实例不读这个段，也不受它影响。

## 21. 旧消息引用还原与撤回（框架侧聊天记录缓存）

**问题**：用户引用一条消息再触发指令时，那条被引用的消息**未必还拿得到**。QQ 的引用段（`reply`）只被
OneBot 实现端的本地缓存认账，NTQQ 系实现（NapCat / LLOWeb / Lagrange）对十几二十分钟前的 `message_id`
就查不到原消息了——要么丢掉引用段静默发送，要么整个 `send_group_msg` 报错。所以"引用一条半小时前的
消息再 `/攻略`"这件事，光靠协议层做不到。
另一半是**撤回**：同一家实现端给的 `real_id` 才是 `delete_msg` 认的号，插件手上往往只有事件里那个
`message_id`，直接拿去撤会报「消息不存在」。

**这两半现在都在框架**（`runtime::chatlog`，实现见 `crates/arona/src/runtime/chatlog.rs`）。留档这件事
和玩法无关，谁都得依赖它，交给插件各记一份只会互相看不见：A 插件记的库，B 插件读不到。

插件要用的话只有两个方法，都在命令上下文与事件上下文上（同一套东西在 dll 里也真读得到宿主的库：
连接与库都挂在 `Framework` 实例上，而插件侧一律经宿主那层 `Arc<dyn ChatLogFace>` 用它，
两道关的来由见 §22；mirai 那种对象写法见 §14.2 的 `MessageSource`）：

```rust
// 引用回复：被引用那条还在窗口内就挂原生 reply 段（QQ 上是真引用），
// 过窗则用框架的聊天记录把那条还原成文字头 + 图片，拼在回复最前面
context.reply_with_quote(message).await;
// 撤回：传你见过的任意一个号（触发消息的 message_id、自己那条回复的回执都行），
// 框架按库里的记录换成实现端认的那个
context.recall(message_id).await;
```

`reply_with_quote` 在触发消息本身没有引用时等同于 `reply_message`，所以命令实现里可以无条件用它。
`EventContext` 上另有同名的 `reply_with_quote` / `recall`（钩子里回消息、钩子里撤回）。

第三个入口在对象模型上，读的是同一张表：`group.history(20)` / `group.history_before(20, cursor)`
（`Friend` 上同名，`Member` 没有——临时会话本来就归到那个人所属的群）。`HistoryPage::cursor()` 给 `None`
即翻到头，`ChatRecord` 里的 `image` 列直接交给 `Image::from_stored` 就能再发一次（§19）。
分页只在本地做：库里默认只留 2 天（见下面的清理项），所以它**不是**实现端的全量历史。

框架侧还做了什么：

1. **进出两条路各记一笔**。听到的消息由 `onebot/business.rs` 记账（排在群授权与黑名单过滤之后，
   未授权的群和黑名单用户根本不产生记录），说出的消息由 `onebot/message_sender.rs` 记账——
   只有连出站一起记，才还原得了"用户引用了机器人的回复"。库在 `data/arona/chatlog.db`，
   图片**只存原链接**不另存副本。
2. **裁决 + 还原**。回复时看被引用那条的时间：还在窗口内（默认 30 分钟）挂原生引用，过窗就还原成
   `[引用 谁 时间]` + 原文 + 图片。库里查不到的 id 退回去问一次 `get_msg` 并顺手回填，
   机器人上线前的历史消息能被逐步补进库。图床直链会过期，发出前先探一次（`Range: bytes=0-0` 的 GET，
   带浏览器 UA，5 秒超时）；探不到就明确回一句「图片已过期」，而不是默默少发一张图。
   机器人自己渲染的图存的是本地路径，探测方式是文件还在不在。
3. **清理**。默认只留 2 天，每 4 天删一次 2 天前的记录（`runtime::purge`，见 §10；首次立即执行）。
   这两项组合起来的**实际**保留时长是 2~6 天，磁盘峰值按 6 天算。

配置住在框架的 `config/arona.yml`（不属于任何插件），改动即时生效、不必重启：

```yaml
chatlog:
  # 总开关：关掉后不再记账，引用还原与撤回时的 id 换算一起停
  enable: true
  # 本地保留几天的聊天消息
  keep_days: 2
  # 每隔几天清理一次过期记录
  purge_interval_days: 4
  # 多少分钟内的引用仍由 OneBot 实现端直接引用，超过才改走本地还原
  quote_ttl_minutes: 30
```

开关与保留期都是**现读现判**：记账点每条消息读一次，清理任务在配置热重载时按新值校准
（周期没变就不重建，避免每改一次配置就多清一遍库）。

> 这件事在旧版本里是插件自己实现的，现已上收到框架：
> 用例整套搬进了 `runtime::chatlog` 的测试。配置里如果还留着插件那份 `chatlog` 段，
> 框架会在启动日志里点名城到 `config/arona.yml`。

## 22. 动态装载与工具链要求（发布插件前必读）

mirai 靠 JVM ClassLoader 把插件隔离开，Rust 没有等价物。本框架的选择是：**dll 与宿主直接交换
`Arc<dyn AronaPlugin>`**，代价是把"两边必须是同一套工具链编出来的"这条前提变成硬性发布要求。
前提一旦不成立，症状是随机崩溃而不是干净的报错，所以框架在装载时逐项核对，**对不上就拒载**。

### 装载器做什么

`arona::plugin::dynamic` 在 `plugin::install_all()` 里跑一次：

1. 扫 `<运行目录>/plugins/` 这一层，以及它的每一个一级子目录，收所有 `*.dll`
   （Linux `.so`、macOS `.dylib`）。文件名以 `_` 开头的当作插件自带的第三方运行库，跳过。
2. 逐个 `LoadLibrary` → 取 `arona_plugin_abi` / `arona_plugin_toolchain` / `arona_plugin_host`
   / `arona_plugin_new` 四个符号。
3. 握手，全过后**先把宿主的上行通道交进 dll**（下一小节），再取实例登记进插件表，`loader` 记为 `dynamic`。
4. 模块句柄**故意不释放**（`std::mem::forget`）：插件实例的虚表指向该模块的代码段，
   `FreeLibrary` 之后任何一次虚调用都是野指针。因此框架不提供运行期卸载——「停用」只回收
   该插件名下的命令、钩子、任务与服务，dll 本身常驻到进程结束。

单个文件在任何一步失败都只影响它自己：日志里一条中文原因，其余插件照常装配。

### 宿主的上行通道（接管的就这四样，判据看状态挂在哪）

mirai 的插件和框架在同一个 ClassLoader 树里，`MiraiInstance` 只有一份。这里不行：dll 把 `arona`
**静态链接成了自己的第二份**，于是 `Framework::global()`、日志文件句柄、tokio 句柄这些进程级状态
天生有两套，插件写进自己那套宿主永远看不见——表现为"命令登记了却没人路由"这种最难查的静默失效。

装载器在握手通过之后、造实例之前，把宿主真正在跑的四样东西交给插件（`export_arona_plugin!` 生成的
`arona_plugin_host` 符号，接管失败直接拒载）：

| 交出去的东西 | 插件侧的效果 |
| --- | --- |
| 宿主的进程默认实例 | `arona::quartz::*`、`arona::container::instance()`、`arona::plugin::manager()` 等自由函数全部落到宿主那套注册表 |
| 宿主的日志出口 | `arona::runtime::log::info(..)` 进同一个日志文件、同一个 GUI 实时日志，来源标注仍按 `[插件名:动作]` |
| 宿主登记的 tokio 句柄 | 在 GUI 主线程上装配插件时（面板里关掉再打开），`ctx.spawn` / 定时任务照样投得出去 |
| 宿主的 HTTP 代发通道 | `arona::runtime::http::*`（§14.1）把请求递给宿主那份 reqwest，插件不必自带客户端 |

所以**插件写起来和同仓静态编译时一样**：能用 `ctx` 就用 `ctx`（下面那条禁令的真实含义）。
接管由宏与装载器完成，插件代码里没有任何对应的手续。

但"接管了四样"只意味着**这四样以及挂在它们身后的东西**是通的。判断某个进程级自由函数在插件里
能不能用，要**过两道关**，缺一道都不算通：

**① 状态挂在哪。** 挂在 `Framework` 实例上的才可能通（`quartz`、`container`、`plugin::manager`、
`config::plugin_config` 走 `Framework::global().configs()`），因为交出去的就是宿主那个实例；
另起 `static` 的，插件里就是独立的一份空表，而 `get_or_init` 让它**不 panic、只是静默失效**——
比崩掉难查得多。`runtime::config::*` 看起来像进程级，其实 `gating()` 就是
`Framework::global().gating()`，所以 `groups()` / `bot_id()` 这些一直是通的。

**② 由哪份代码动手。** 状态搬到实例上只解决了"去哪儿找它"，没解决"用哪份代码操作它"。
dll 把 `arona` **连同它的全部依赖**静态链接成了第二份，所以插件里 `impl ChatLog` 的固有方法是
**编译进 dll** 的那一份；让它去动宿主创建的对象里那些第三方库句柄，就是拿宿主的 `sqlite3*`
调 dll 自己那份 sqlite 引擎——两套引擎各有一批进程级全局，互不相认，**实测当场段错误、整个宿主
一起没**。天生安全的只有两类：经**宿主生成的 vtable** 调用（`Arc<dyn ..>`，函数指针指向宿主镜像
里的代码），以及 `extern "C"` 函数（上面那四道通道）。

框架里所有 `static` 已按第①道核过一遍，插件可见范围内共有**四处**不合规，已全部收进实例；
其中聊天记录库那处随后又被第②道拦下，改成了 trait 出口（见下两小节）。

#### 曾经静默失效的四处（已收进 `Framework` 实例）

| 入口 | 现在的形态 | 曾经的症状 |
| --- | --- | --- |
| `arona::runtime::services::sender_ready` / `send_message` / `data_root` | 状态是实例上的 `RuntimeServices`（`Framework::runtime_services()`），自由函数只是它的门面 | 宿主 `set_message_sender` 只写到宿主那份，插件这份恒 `None`：定时任务与事件钩子里**主动**发的消息永久不成立，返回默认回执、消息原地丢弃且不带错误。框架自带的示例插件就中招 —— `plugins/hello` 的「每日问好」与「进群欢迎语」两条都是这条路径，写成 `let _ = send_message(..)` 连失败都看不见 |
| `arona::config::settings::*`（框架那份 `groups`/`managers`） | 状态是实例上的 `Settings`（`Framework::settings()`），`init` 之后连热重载监听都跟着实例走 | 插件那份 `file` 恒为 `None`：读出来是默认值（`/config` 列出空的管理员表），写则报「配置文件尚未初始化」。插件自己的 `config/<插件>/arona.yml` 一直不受影响，那条本来就走在实例上 |
| `arona::onebot::api::OneBotApi::*`（全部 OneBot 动作） | 连接与 `self_id` 是实例上的 `OnebotOutlet`（`Framework::onebot()`），`OneBotApi` 自己就**持着所属实例**（`OneBotApi::of(&实例)`，`global()` 是进程默认那一份），每次调用现取该实例的首个可用连接；`connection::GLOBAL_REGISTRY` 那道进程级 `OnceCell` 已删 | 动作对象本身拿得到（当年的 `OneBotApi` 是零宽类型，不持状态），但登记表是 dll 自己那份空的 → 每一次动作都只回 `NoConnection`。插件里 `get_login_info`、`get_group_member_list`、`delete_msg` 全废，而且**不 panic**，只是永远"没有可用的 OneBot 连接" |
| `arona::runtime::chatlog::*`（引用还原与撤回） | 设置、库句柄、打开失败标记都收在实例上的 `ChatLog`（`Framework::chatlog()`），而**插件侧只能经 `Framework::chatlog_api()` 那层 `Arc<dyn ChatLogFace>` 用它**——见下一小节 | 这一处两道关都没过，症状也最阴，见下面两段 |

第①道搬进实例不新增 ABI 通道、**插件源码一行不改**，重编 dll 即可；代价是这几处对插件**彻底可用**
了：插件里 `arona::config::settings::init(...)` 现在真的会去重载宿主那份 arona.yml（示例：插件的
"恢复备份"流程就靠它），所以别把它当调试工具随手调。

**但只要插件会读的那个类型换了布局，就必须抬 `ABI_LAYOUT`**：`HostBridge::framework`
交出去的是擦成 `*const c_void` 的实例指针，dll 按**它自己编译期的字段偏移**读这个结构体，
插一个字段就会让旧 dll 读错位置——那是未定义行为，不是"握手失败"。所以旧 dll 一律拒载。
抬过的两次：`3 → 4` 是 `Framework` 多出 `chatlog_api` 一项；`4 → 5` 是插件会直接读的那几个类型
（`EventBody::Meta` 多带一个 `MetaInfo`、`BodyFilter` 多两档、`CommandContext` 末尾多一个 `framework`、
`OneBotApi` 由零宽单元类型变成持有实例引用的结构体）。**宿主侧的四个导出符号一个字都没改，
也算改 ABI**——这条最容易漏。同理，给跨镜像 `trait` 追加方法（vtable 变长）也要抬。

#### 还欠着的两处（不影响插件正常写法，但排查时值得知道）

1. `arona::runtime::chatlog::*` 那批**自由函数**仍然读进程默认实例上的库。`EventContext` 与
   `CommandContext` 上的 `recall` / `reply_with_quote`、`arona::bot` 那几个走留档的方法都已经改成
   走**所属实例**的 `chatlog_api()`，插件正常用法没有串台问题；只有直接调那几个自由函数、又跑在
   隔离实例上（写框架自己的测试）时才会看到"读到的不是这一套的库"。
2. 正向 WS 被宿主**主动** `stop()` 时**不报** `disable`：那条路径要 `abort()` 掉任务，尾部代码根本不执行，
   补发只会引出竞态和重复事件。插件把"掉线"当作重连补投的信号来用就够，别把它当作"宿主已经关了"的凭据；
   要当前真相就读 `bot.is_online()`。

这四处曾经的症状都是**实测确认**而非推断，而且是同一进程内的对照实验：同一个示例插件 dll，
`/你好`、`/示例延时`、`/示例联网` 三条回话都成功到了实现端（走 `CommandContext`），而它事件钩子里
那条主动 `send_message` 一条都没到；`--test-notify` 在启动 20 秒后（宿主侧发送器早已备好）照样报
「消息发送器尚未就绪」。同进程、同 dll、相隔十几秒 —— 时序与网络都解释不了，只能是两份内存。
**修好之后要复跑同一条链才算数**：假 OneBot 会下发一条 `group_increase` 并在收尾时按
`sent.log` 的新增部分打印「探针通过 / 探针失败」，只看命令能回话不足以宣布主动消息链路可用。

> **别拿"读得到东西"当判据 —— 聊天记录库那一段的原文。**
> 上面第三、四处是同一轮补的，探针就是 `plugins/hello` 的 `/示例自检`：一次问清四样——OneBot 动作
> 通不通、刚发那条在库里查不查得到、撤回回不回得来、引用窗口读回来是多少分钟。第一次跑出来的是
> `get_login_info 不通（没有可用的 OneBot 连接）；聊天记录库通（id=… 查得到）；撤回通；引用窗口读到 30 分钟`。
> 当时据此写的"库这一侧是通的"**结论是错的**：`query()` 懒打开的库路径按**当前工作目录**解析，
> 宿主和 dll 于是打开了**同一个** `data/arona/chatlog.db`，读得到记录纯属巧合；
> 而设置（当年的 `SETTINGS`）与连接登记表那两份确实是 dll 自己的空表。
> 换一个**宿主侧与默认值不同**的观测量才露馅：把宿主的 `quote_ttl_minutes` 配成 7，插件里仍读回 30。
> 两条经验：① 别用读写结果反推状态是否同一份，判据得看代码形态；② 探针要挑一个
> 宿主配置与硬默认值**不相等**的量，否则测不出两份内存。

> **收进实例不等于够得着 —— 第②道关的实测原文。**
> 把 `ChatLog` 搬上实例之后复跑同一条探针，读回来的确实**是宿主那一份**：`quote_ttl_minutes`
> 读回 7、库路径报出宿主的 `run\data\arona\chatlog.db`。然后**第一次真正执行 SQL 就段错误**，
> 宿主连同插件在 3 毫秒内一起没了。为了把这个界线钉死，示例插件多了一条 `/示例状态`
> （别名 `/状态探针`），按类型的性质分三问、**一问一条**发出去（合并成一条回话时崩了就什么都拿不到）：
>
> | 探针 | 读的东西 | 结果 |
> | --- | --- | --- |
> | 1/3 | `RwLock<ChatLogSettings>`（纯 Rust，std 的类型） | **通**：引用窗口 7 分钟、启用=true |
> | 2/3 | `RwLock<Option<PathBuf>>`（同上） | **通**：宿主那一份库路径 |
> | 3/3 | 拿库句柄执行 `SELECT` | **段错误**，进程当场死 |
>
> 也就是说：**纯 Rust 字段跨镜像读写完全正常，一碰宿主那份 `rusqlite::Connection` 就崩。**
> `rusqlite` 的 `bundled` 特性把 sqlite 静态编进了宿主与各 dll，一个进程里躺着好几份互不相认的
> 引擎；dll 手里那个 `sqlite3*` 是宿主那份引擎创建的，它却调自己这份引擎的 `sqlite3_prepare_v2`。
> 同一条道理管着 `ChatLog::prober` 里那份 reqwest 客户端。反过来，`get_login_info` 与
> `send_message` 一直是通的——它们最终落到 `Arc<dyn OneBotConnection>` 这类**宿主生成的 vtable** 上，
> 代码根本没进 dll。
>
> 修法是给这份库加一层 `ChatLogFace`（`Framework::chatlog_api()`，实现体在宿主镜像里），
> 插件侧那批 `chatlog::` 自由函数签名一个不改、只改成经它转发；async 的那几问也在宿主侧执行，
> 递回来的只是一个 `BoxFuture`。**给插件用的新接口要碰第三方库的内部状态时，一律走这条形状。**
> `arona::bot` 里那几个走留档的方法（`Bot::recall` / `Bot::message` / `MessageSource::reply` /
> `MessageSource::record`）同理，它们本来就是给插件用的，一律取 `chatlog_api()`。
>
> 改完复跑同一条探针，一次跑齐（假 OneBot 逐帧记下来的）：
> `自检 1/4 动作出口：get_login_info 通（self_id=10000）`、
> `自检 3/4 库：聊天记录库通（id=1023 查得到）；引用窗口读到 7 分钟`、`自检 4/4 撤回：撤回通`；
> `/示例对象` 报 `group.send_text 通（id=1018）；撤回通；临时会话目标=Temp { group_id: 20001,
> user_id: 100 }；ctx.source()=1017；引用窗口=7 分钟`；`group_increase` 那条探针也回到「探针通过」。
> 顺带收掉一个旧账：**上一轮 bluearchive 卡在 `initializing database…` 不动、CPU 完全空闲，
> 是同一个 UB 的另一种表现**（它 `await` 的那一步再也没返回，比崩掉更难查）。装上 trait 出口之后
> 两个插件同场启动，握手报的是当轮那个布局号（那轮是 4，本轮已抬到 5），预热流程一路走完。
>
> 还有一条经验给排查用：**同一次 `cargo build --release --workspace` 里出宿主与各插件**，
> 这样 `arona` 只编一份、两边的字段布局必然同源。分两条命令编出来的宿主与 dll 万一 feature 集
> 不同（宿主带 `gui`、插件 `default-features = false`），错位就会以同样的段错误出现，更难查。

> **布局 5 这一轮的新探针 —— 对象模型"读侧"与钩子投递的实测原文。**
> 上面那几轮验的都是"插件能不能动手"，这一轮验的是"插件从宿主身上读回来的东西对不对、
> 宿主投给它的事件到没到"：三处读侧出口加两档新过滤各钉一条命令，全在 `plugins/hello` 里，
> 假 OneBot 一次跑齐——
>
> | 探针 | 读的东西 | 实现端记下来的回话 |
> | --- | --- | --- |
> | `/示例图片` | `Image::from_segment` + 按同一份线索重发 | `图片段还原成功（file_id=Some("FD-E2E-001")；url=Some(...)；path=Some(...)）；按同一份线索重发一次 id=Some(1027)` |
> | `/示例历史` | `Group::history` / `history_before` 翻页 | `留档最近 5 条，最新一条 测试者：/示例历史；往前一页 5 条` |
> | `/示例缓存` | `Member::member_info` 的实例缓存 | `成员资料缓存：首次 417ms；紧接着 0ms（该命中缓存）；作废后 404ms。三次读回同一份资料` |
> | `/示例连接` | 框架**合成**的 lifecycle 元事件投没投进 dll | `连接事件：connect ×1；disable ×0；好友私聊命中 ×1` |
>
> 最后一行是这一轮才补上的：`BodyFilter::meta(MetaKind::Lifecycle, ..)` 与 `private("friend")`
> 两档过滤此前**只有单测**，没跑过真链路。示例插件为它们各记一笔账，假 OneBot 连着下发三条私聊
> （`sub_type` 分别是 `friend`/`group`/`other`），实现端那边只出现一条 `send_private_msg`——
> 判据是**命中次数**，不是"有没有回话"（过滤失效时三条全会回话，看后者恰好被骗过去）。
> `connect ×1` 则是 WS 握手那一刻框架合成投递的那一条，与 §22 前面"只投钩子、绝不回播给实现端"
> 那句正好互相印证。
>
> 第三条的**硬判据不是那三个毫秒数**（那只是佐证），而是实现端对 `get_group_member_info` 的
> **请求计数**：假 OneBot 把这一问延迟 400ms 应答并累加次数，脚本按本轮快照差值断言
> 「三问只打了 2 次网络」——中间那次确实没上网络。计时差会被机器负载骗，计数不会。
>
> 第一条刚跑出来是 `最新一条 ：/示例历史`，名字是空的。查下来在**宿主侧记账**那一步：
> `runtime::chatlog` 的 `sender_name` 用 `find_map(as_str())` 从 `card` 一路找到 `user_name`，
> 而 QQ 的群名片**默认就是空串**，空串照样是合法的 `&str`，于是它被当成"给了名片"、不再往昵称兜底。
> 已修（全空白一律算没给，与 `onebot::console` 里那一处的口径对齐），单测把
> 「空名片兜底昵称」「只有空白也算没给」两种 payload 钉住了。
> 这条经验值得单独记一句：**读回来的字段是空串时，先怀疑记账侧把空值当成了有值**，
> 别急着怀疑跨镜像链路——这一处跟 dll 一点关系都没有。

> **给插件自己的测试：手工构造 `CommandContext` 会编崩，这是设计如此。**
> 布局 5 这一轮 `CommandContext` 末尾多了 `framework` 字段，bluearchive 那两份用例里是自己拼这个
> 结构体的，于是当场 `E0063`。补一行 `framework: arona::framework::Framework::new()` 就好——
> 它造的是一套**完全隔离**的注册表，不会去动进程默认实例，并行跑用例也不会互相污染。
> 跨镜像类型加字段本该让下游编崩，而不是让它按旧偏移静默读错位；真嫌字段列表太长，
> 就别自己拼，改测那些不依赖上下文的纯函数。

#### 两份 tokio：哪些调用能直接用

tokio 也被静态链接成了两份，而"当前在哪个运行时里"这个上下文是**线程局部**的——宿主只能在
它自己那份 tokio 的 thread-local 上打标记，dll 那份天生是空的。框架因此在**每一个由宿主亲自 poll
的插件回调边界**（命令、类型化命令、兜底、事件钩子、出站钩子）和**每一个 `ctx.spawn` 投出去的
任务**上套了一层 `poll_fn`，**每轮询一帧**就按任务自己那份 tokio 短暂补装一次上下文（`EnterGuard`
不是 `Send`，跨 `await` 持有它会连任务一起变得不能发送）。于是：

- **命令与钩子的 async 体里**：`tokio::time::sleep`、`tokio::select!` 直接写。示例插件的
  `/示例延时` 就是在 dll 里睡三秒再回话，那条链路是端到端验过的。
- **`ctx.spawn` / `ctx.spawn_as` / `arona::quartz` 的任务体里**：同样点亮，要等待要重试的逻辑
  随时可以写在里面。

补装只发生在**框架投出去、或者框架亲自 poll 的任务**上。第三方库自己 `tokio::spawn` 的常驻后台
任务不在这条链上，它第二次醒来时脚下那份 tokio 又空了。实测的一例：`reqwest` 0.12（hyper-util
0.1.20）**开着连接池**时，会把"后台补连接""空闲驱逐"交给 `TokioExecutor::execute` → dll 那份
`tokio::spawn` → panic；panic 发生在池子的锁里，于是紧接着每次 `checkout` 都炸 `PoisonError`，
整条出站链路废掉。

这类坑不该靠插件侧调参数去绕。`Client::builder().pool_max_idle_per_host(0)`（关池，让 hyper 走
"直接连接"那条路，不再派生后台任务）是能绕过，代价是每个请求多一次 TCP+TLS 握手，而且只对
试过的这一版 reqwest 有效。所以框架把 HTTP 收成了一道预制方法（§14.1）：请求由宿主那份客户端
发出，跑在宿主常驻点亮的 runtime 上，插件不必带 `reqwest`。判断自己是不是踩了这类坑：panic 栈上
有 `tokio::task::spawn::spawn` 与某个第三方库的 `execute`，而**你的代码一帧都不在栈上**。结论一样：
凡"进程级只该有一份"的东西（连接池、全局定时器、驱动线程）都优先用框架暴露的方法拿。

### 握手核对的五项

| 项 | 来源 | 不一致的后果 |
| --- | --- | --- |
| 符号布局版本 | `arona::plugin::abi::ABI_LAYOUT`（当前 5） | 四个导出符号的签名、或**插件会读的那个类型**（`Framework`、`CommandContext`、`EventBody`、`BodyFilter`、`OneBotApi`）换了布局，新旧不可混用 |
| 框架契约版本 | `FRAMEWORK_API_VERSION`（主版本相等 + 框架次版本不低于插件要求） | 插件用到了宿主还没有的接口 |
| rustc 版本与 host 三件套 | 两边各自 `rustc -vV` 的 `release` / `host` 两行 | `dyn Trait` 胖指针布局、泛型单态化产物可能对不上 |
| 目标三元组 + profile + CRT 链接方式 | `TARGET` / `PROFILE` / `crt-static` | debug 与 release 混编会炸；两套 MSVC 运行库会各自分配释放内存 |
| arona-rs 版本 + 除 `gui`/`default` 外的 feature 集合 | `CARGO_PKG_VERSION` / `CARGO_FEATURE_*` | 两边看到的 `Framework` / `PluginContext` 不是同一个结构 |

指纹由 `crates/arona/build.rs` 算出、由 `export_arona_plugin!` 原样上报（末尾带 NUL，宿主按 C
字符串读），宿主按字符串全等比对。`gui` 与它的别名 `default` 是被放行的两个例外（§16）：
`gui` 有源码扫描用例盯着它不改共享类型布局，`default` 若不放行，`cargo build --workspace`
与插件作者的 `cargo build -p <插件>` 会算出不同指纹，同仓编出来的 dll 反而装不进自己的宿主。

### 工具链版本以仓库里那一行为准

上表第三项要比对的是 `rustc -vV` 的 `release` 行，而那一行只有版本号（形如 `1.98.0`），
不含 commit hash。版本本身不再靠自觉：框架仓库与插件模板仓库根的 `rust-toolchain.toml`
把它钉死，本地 `rustup` 与 CI 都按同一份走，`cargo build` 会自动选中它。

升级流程是四步一起走，缺一步就会有插件装不进宿主：

1. 改两个仓库的 `rust-toolchain.toml` 里同一个 `channel`
2. 用新工具链重编宿主
3. 用新工具链重编所有插件
4. 一起发版

只升框架不重编插件，老插件会全部拒载 —— 这是设计意图，不是回归。

### 发布插件时必须写明的三件事

```
适用于 Arona-rs 1.1.0（框架契约 1.0.0，符号布局 5）
构建工具链：rustc 1.98.0，x86_64-pc-windows-msvc，--release，crt-static
下载后放进 <框架运行目录>/plugins/<你的id>/，重启框架
```

第二行的版本号要和 `rust-toolchain.toml` 里那一行对得上。符号布局号抄 `arona::plugin::abi::ABI_LAYOUT`
当轮的值：它和工具链版本号一样是"差一个就不该混用"，而宿主报的永远是它自己那一份。第三件事别偷懒写成"任意 rustc
都能用"——那不是宽松，是把干净的拒载推迟成随机的崩溃。
真要跨编译器版本，得走进程外通信（子进程 + JSON-RPC）或 wasm 组件模型，
那是与本框架这套契约不同的另一条路。

### 插件侧禁止清单

- **不要 `main`、不要可执行入口**：插件只能被框架装载；作者要调试，自己去装一份框架。
- **不要把 `arona` 的进程级自由函数用在登记路径上**（`arona::plugin::manager()`、
  `arona::container::instance()`、`arona::quartz::*` 之类）。它们一律落到**进程默认实例**：
  静态编译进宿主时那个实例就是宿主自己，dll 里则被上一小节的上行通道接管——所以**登记路径本身
  不会静默失效**。仍然不建议用，是因为默认实例是进程级的：单测里造不出第二份，几个插件并存时
  也共用同一张表，回收和隔离都做不出来。命令、钩子、服务、配置区、定时任务的登记必须走 `install`
  阶段的 `&PluginRegistrar` 或 `configure`/`start` 阶段的 `&PluginContext`。
  （纯数据类型与无状态构造器——`runtime::message` 的消息段、`runtime::args` 的参数声明、
  `runtime::priority` 等——不在此列，它们不碰注册表。）
  注意"接管了默认实例"不等于"所有自由函数都通"：判据是上一小节那**两道关**——状态挂在实例上只是
  第一道，另起 `static` 的就是插件自己那份空表；第二道是**由哪份代码动手**，插件侧要操作宿主创建的
  对象里那些第三方库句柄（sqlite、reqwest），只能经宿主生成的 vtable（`Arc<dyn ..>`）或 `extern "C"`
  通道，在 dll 里直接调固有方法就是未定义行为（聊天记录库那一处实测当场段错误，上一小节有全文）。
- **`arona::runtime::services::send_message` 与 `arona::config::settings::*` 在插件里是真通的**：
  这两处以前各持一份进程级 `static`，dll 里那份是空的，主动消息会被静默丢弃、写配置会报
  「配置文件尚未初始化」；现在它们住在 `Framework` 实例上，随上行通道一起被接管（上一小节）。
  也正因为通了，`settings::init` 这类写盘入口对插件**不再无害** —— 它重载的是宿主那份 arona.yml。
  验这条链路必须真驱动一次主动消息（假 OneBot 的 `group_increase` 探针），只看命令能不能回话不算。
- **不要裸 `tokio::spawn`**：用 `ctx.spawn(..)` / `ctx.spawn_as(..)`，否则停用时收不掉它。
  确实需要 runtime 句柄时走 `arona::runtime::reactor`，那是刻意留在进程级的那一份。
- **不要自带 HTTP 客户端**：出站请求走 `arona::runtime::http`（§14.1）。dll 里那份 reqwest 只要
  开着连接池，就会在 hyper 自己派生的后台任务上炸「there is no reactor running」，上一小节的
  实测写着原因。命令与钩子的 async 体里 `tokio::time::sleep`、`select!` 这些**可以**直接写，
  框架在回调边界按帧补装过上下文；不行的只有库自己 `spawn` 出去的那一类。
- **不要 `std::panic::set_hook`、不要 `std::process::exit`、不要改全局 locale/时区**：
  你和宿主活在同一个进程里。框架确实在每个入口都 `catch_unwind`（§20），但那是兜底，不是许可。
- **不要假设自己会最后一个退出**：停用只是回收资源，模块常驻。

### 拒载日志对照表

| 日志里的原因 | 怎么办 |
| --- | --- |
| `缺少导出符号 arona_plugin_new，不是 Arona 插件或版本过旧` | 这个 dll 不是插件；或它漏写了 `export_arona_plugin!` |
| `缺少导出符号 arona_plugin_host` | 插件是用更早的框架（三符号 ABI）编的，用当前框架重新编译 |
| `插件没能接管宿主的上行通道（代码 1）` | 通道指针为空，基本只可能是 ABI 对不上——两边框架版本重编 |
| `插件没能接管宿主的上行通道（代码 2）` | dll 在握手之前就自己建了默认实例（多半是静态初始化里调了 `Framework::global()`），或它那份 `arona` 跟宿主不是同一份源码 |
| `无法加载模块: …` | 插件依赖的第三方 dll 没跟它放在同一个子目录 / 目标三元组不对（32 位包丢进 64 位宿主） |
| `插件 ABI 布局版本不匹配` | 插件是更早或更晚的框架接口编的，换配套版本 |
| `插件按框架契约 1.x.0 编译，当前框架只有 1.y.0` | 让插件作者降到宿主版本，或者升级宿主 |
| `插件与框架的构建工具链不一致` | 日志会把两边指纹逐段打出来，按差异项对齐（多数是 profile 或 rustc 版本） |
| `插件上报的工具链指纹满 4096 字节还没有结尾` | 四个导出符号是手写的、漏了 NUL 结尾——改用 `export_arona_plugin!` |
| `插件入口返回了空实例` | 插件的构造表达式返回了空指针 |
