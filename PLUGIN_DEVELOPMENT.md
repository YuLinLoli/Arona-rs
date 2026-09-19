# Arona 插件开发指南

本项目已拆成「框架 + 插件」两部分。框架只提供 **OneBot 连接（含 v11 全量动作接口与事件钩子）、管理面板(GUI)、群授权/黑名单、插件与功能的启停门控、命令分发骨架** 与启动/关闭的生命周期编排；具体功能（抽卡、活动日历、攻略、塔罗……）一律以**插件**形式实现并插入框架。

## 1. 目录与版本

```
Cargo.toml                 # 虚拟 workspace（resolver=2）
plugins.toml               # 功能插件清单：host 编译期据此静态注册插件（build.rs 读取）
crates/arona/              # 框架库 crate：name=arona, version=1.0.0（发布版从 1.0.0 起）
crates/arona-host/         # 宿主可执行：name=arona-host, version=1.0.0, 产物 bin=arona-rs
plugins/bluearchive/       # 碧蓝档案功能插件：name=bluearchive-plugin, version=0.3.4, lib=bluearchive_plugin
```

版本约定：框架与 host 同为 `1.0.0`；功能插件 `BluearchivePlugin` 的版本号（`0.3.4`）接替拆分前本项目的版本号，随插件功能演进单独递增。

运行期的落盘目录由框架统一规定（见 §5），插件不自己挑地方：

```
<运行目录>/
  config/arona.yml          框架配置：groups / managers / global_blacklist / group_settings / disabled_plugins
  config/onebot.yml         OneBot 连接配置
  config/<插件>/arona.yml   该插件自己的配置（如 config/bluearchive/arona.yml）
  data/<插件>/…             该插件自己的数据（如 data/bluearchive/image、arona.db、backups）
  logs/                     按天滚动的日志
  plugins/<插件>/           该插件的目录（框架写 plugin.yml，随包资源放这里）
```

## 2. 职责边界（依赖方向）

**框架 `arona` 绝不依赖任何插件**，插件单向依赖框架：

```
arona-host  ──depends──▶  arona (框架)
     │                        ▲
     └──depends──▶ bluearchive-plugin
                            │
                            └──depends──▶ arona (框架)
```

框架反向调用插件只能通过 `arona::plugin` 提供的一套接口（注册表 + 生命周期编排 + 分发器槽位 + 功能开关/配置区登记），加上 `arona::onebot::hooks`（事件订阅）与 `arona::onebot::api`（动作出口）。任何“框架里 `use bluearchive_plugin::…`”都是设计违规。

## 3. 生命周期（框架 `arona::run` → `run_bot` 的真实顺序）

宿主在调用 `arona::run(args)` **之前**注册插件（`register_plugins()` 由 `crates/arona-host/build.rs`
依据 `plugins.toml` 生成，见 §11）；`run` 内部按序驱动各阶段：

1. `plugin::install_all()` —— **早于 `arona.yml` 加载**。逐个插件建好 `plugins/<id>/`、`config/<id>/`、`data/<id>/`
   并写 `plugins/<id>/plugin.yml`，然后调 `install()`：插件在此登记功能开关（`register_feature`）
   与自持有的配置区（`register_section`），使生成的配置模板认得这些键并带完整功能清单。
   **install 阶段不过滤被禁用的插件**——否则 GUI 列不出它、模板也会丢掉它的配置块。
2. 加载 `config/arona.yml`（框架业务配置，含热更新）与 `config/onebot.yml`（协议配置）。
   旧版把插件配置键写在框架文件顶层的，这一步会被接管（`plugin_config::absorb_legacy`）。
3. `config::plugin_config::init()` —— 为每个登记了配置区的插件备好 `config/<id>/arona.yml`
   （缺文件就生成带注释模板）并加载成原样 YAML 片段。
4. `plugin::configure_all(&PluginContext)` —— **只对启用的插件调用**。插件构建自己的
   `SimpleCommandDispatcher` 并 `ctx.set_dispatcher(..)`（框架记下归属插件 id，禁用时整体停路由）。
5. `plugin::start_all()` —— 只拉起已装配插件的后台任务（数据库、数据预热、定时推送）。
6. 框架用 `plugin::dispatcher()`（缺省空表兜底）装配 `StandaloneBusinessHandler` 并启动 OneBot 连接。
7. 运行中：
   - `config/arona.yml` 或某个 `config/<插件>/arona.yml` 变更 → 热重载后回调 `plugin::notify_config_reloaded()`
     （插件文件变更时只回调该插件的 `on_config_reload`）。首次加载不通知（那时 `start()` 已按配置建任务）。
   - 禁用名单变化 → `plugin::sync_enabled_state()`：新禁用的调 `stop()`，新启用的补跑 `configure()` + `start()`。
     GUI 开关与手改 `arona.yml` 都走这条路，**不需要重启**。
   - 收到事件 → 先投给 `arona::onebot::hooks`（§10），无人消费才走命令分发。
8. 退出（Ctrl+C / GUI 关窗）：`application.stop()` → `plugin::stop_all()` → `quartz::pause_all()`。

## 4. 插件接口

实现 `arona::plugin::AronaPlugin`，方法都有默认实现，只需覆写关心的阶段：

```rust
pub trait AronaPlugin: Send + Sync + 'static {
    fn meta(&self) -> PluginMeta;                                   // 必填
    fn install(&self) -> Result<(), String> { .. }                  // 登记功能开关/配置区/服务
    fn configure(&self, ctx: &PluginContext) -> Result<(), String>  // 构建并 ctx.set_dispatcher
    fn start(&self) { .. }                                          // DB/预热/定时任务
    fn on_config_reload(&self) { .. }                               // 配置热重载回调
    fn stop(&self) { .. }                                           // 关 DB/取消定时任务/注销钩子
}
```

### 硬性规范：插件必须有 id、name 与 version

`meta()` 返回的 `PluginMeta { id, name, version, description }` 里 **`id`/`name`/`version` 为强制项**
（`description` 可留空串）。建议 `version` 直接取 `env!("CARGO_PKG_VERSION")`，与 crate 版本保持一致：

```rust
fn meta(&self) -> PluginMeta {
    PluginMeta {
        id: "bluearchive",                 // 目录名与配置里的键都用它
        name: "BluearchivePlugin",         // GUI/日志展示名
        version: env!("CARGO_PKG_VERSION"),
        description: "碧蓝档案功能插件",
    }
}
```

`id` 用小写短横线名（`bluearchive`、`voice-room` 这种），它是**磁盘上的身份**：
`plugins/<id>/`、`config/<id>/arona.yml`、`data/<id>/`、`disabled_plugins: [<id>]`、
`group_settings.<群号>.disabled_plugins` 全都用它。**改名等于换一份用户数据，定下来就别动。**
匹配时大小写不敏感。

框架用 `plugin::metas()` / `plugin::meta_of(id)` 汇总元信息（GUI「插件管理」页 / 诊断）。

## 5. 目录接口：config 与 data 统一由框架分配

插件**不要自己拼路径**。`PluginContext` 给出本插件的全部目录（框架按 §1 的约定拼）：

| 接口 | 返回 |
| --- | --- |
| `ctx.plugin_id()` | 本插件 id（= `meta().id`） |
| `ctx.plugin_dir()` | `plugins/<id>/` —— 随包资源；`plugin.yml` 由框架维护 |
| `ctx.config_dir()` | `config/<id>/` —— 插件自己的其它配置文件也放这里 |
| `ctx.config_file()` | `config/<id>/arona.yml` —— 框架负责生成模板与热重载 |
| `ctx.data_dir()` | `data/<id>/` —— 数据库、备份等 |
| `ctx.image_dir()` | `data/<id>/image/` —— 生成的图片、下载的缓存 |

这些接口都顺手 `create_dir_all`（`install_all()` 阶段已先建好 `plugins/<id>/`、`config/<id>/`、`data/<id>/`），
所以插件取到路径就能直接写，不用自己建目录。
`plugins/<id>/plugin.yml` 是框架自动生成的清单（id/name/version/description + 配置与数据位置），
静态编译模式下插件不单独出包，这个目录就是它在磁盘上的“存在证明”。

id 目录名在 `install()`/`configure()` 之外也要用时（比如模块级函数），直接用
`arona::runtime::paths::plugin_data_dir(PLUGIN_ID)` / `plugin_image_dir` / `plugin_config_dir`，
把 `PLUGIN_ID` 作为 `pub(crate) const` 与 `meta().id` 共用一个来源（见 `plugins/bluearchive/src/lib.rs`）。

运行目录是**当前工作目录**（安装包的快捷方式把它设成安装目录）。旧版本把这些放在
`arona-standalone/` 下：框架的 `paths::prepare()` 会**复制**它认识的几项到新位置；
插件独有的旧文件（图片、数据库、备份）由插件自己在 `install()` 里用
`paths::migrate_file` / `paths::migrate_dir` 搬（只补缺、保留旧文件，旧目录不删）。

## 6. 群授权、功能开关与插件开关

权限骨架留在框架，插件通过下面几个接口接入：

- **功能清单**：`arona::admin::register_feature(Feature { key, name, description }, PLUGIN_ID)`
  （等价于 `arona::runtime::config::register_feature`）。GUI「群管理 → 功能开关」与配置模板注释据此生成。
  在 `install()` 阶段调用。
- **命令归属**：注册命令时 `CommandRegistration::new(..).with_feature("gacha")` 绑定某功能 key；
  某群关闭该功能时，分发器把该命令视为“未匹配”，交给兜底逻辑。功能 key 用 `&'static str`，
  与 `register_feature` 的 `key` 对齐。
- **管理员/黑名单**：命令上下文 `CommandContext.is_admin`，以及 `runtime::config::is_manager/is_blacklisted`
  由框架在分发前统一判定，插件无需自己实现群授权。

### 停用是三层门控，插件不用配合就能被停掉

| 层级 | 开关来源 | 生效点 |
| --- | --- | --- |
| 功能（分群） | `arona.yml` → `group_settings.<群号>.disabled_features` | `runtime::config::feature_enabled(group_id, key)` |
| 插件（分群） | `arona.yml` → `group_settings.<群号>.disabled_plugins` | `plugin_enabled_in_group`，命令与钩子一起停 |
| 插件（全局） | `arona.yml` → `disabled_plugins` | 不 configure、不 start、不路由、不收事件 |

`feature_enabled` 先看功能归属插件是否启用（`feature_owner` → `plugin_enabled`），再看该群的功能/插件名单，
所以「整体停用插件」自动覆盖它名下所有功能。命令路由的最后一道兜底是
`plugin::dispatcher_active_in_group(group_id)`：插件被停用时，**没绑定功能 key 的命令也进不去**。
事件钩子同理，见 §10。

GUI 入口：「插件管理」页 = 全局开关；「群管理 → 插件开关」= 分群开关（已在插件管理页整体停用的插件在这里显示为灰色）。

### 硬性规范：取消定时任务要写在 `stop()` 里

`stop()` 同时在**进程退出**和**运行中被禁用**两条路径上被调用。定时推送若不在这里取消，
用户把插件停掉了，日历照样每天往群里发。注销事件钩子（`hooks::unsubscribe(ctx.plugin_id())`）也一样要在这里做。

```rust
fn stop(&self) {
    for group in ["StandaloneActivityNotify", "AronaActivityImageRefresh"] {
        arona::quartz::remove_group(group);   // create_daily/create_repeat/create_single_at 的 group 参数
    }
    arona::quartz::remove("StandaloneActivityNotifyInit");   // create_delay 没有 group（固定 "Delay"），只能按名字删
    arona::onebot::hooks::unsubscribe("bluearchive");
    db::close();
}
```

## 7. 注册命令（插件的典型做法）

在 `configure()` 里构造分发器，把命令表交给框架：

```rust
use arona::runtime::dispatcher::{
    CommandRegistration, SimpleCommandDispatcher, fallback, handler,
};

fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
    let registrations = vec![
        CommandRegistration::new(
            vec!["/单抽".into(), "gacha_one".into()],
            "单抽一次, 可选服务器",
            handler(|context, arguments| async move { /* 返回 Some(OutgoingMessage) 或 None */ }),
        )
        .with_feature("gacha"),
    ];
    // 未匹配任何命令时的兜底（例如把纯数字回复解析成上一次模糊建议的选项）
    let fb = fallback(|context| async move { /* .. */ });
    let dispatcher = std::sync::Arc::new(SimpleCommandDispatcher::new(registrations, Some(fb)));
    ctx.set_dispatcher(dispatcher);   // 框架据此记下归属插件 id，停用即停路由
    Ok(())
}
```

`configure()` 里可拿到 `ctx.onebot_config`（协议配置快照，构建分发器/需要 self_id 时用）、`ctx.test_notify`
（命令行是否带 `--test-notify`）与 §5 的目录接口。

## 8. OneBot 动作接口（发/撤/查/管，全量强类型）

框架把 OneBot v11 的动作封成 `arona::onebot::api::OneBotApi`，插件与 GUI 用同一份能力：

```rust
let api = arona::onebot::api();                 // = OneBotApi::global()，每次现取首个可用连接
let members = api.get_group_member_list(group_id).await?;
api.send(MessageTarget::Group(group_id), OutgoingMessage::text("老师")).await?;
api.delete_msg(message_id).await?;
```

不要缓存 `OneBotApi` 之外的连接对象：`onebot.yml` 热重载后旧连接已销毁，而 `global()` 每次现取。
错误统一是 `OneBotError::{NoConnection, Timeout, Failed{..}, BadResponse(_)}`，实现了 `Display`，
直接 `?` 上抛或 `.map_err(|e| e.to_string())` 写日志都行。

按能力分类的方法（全部 `async`，返回强类型；细节看 `crates/arona/src/onebot/api.rs`）：

| 能力 | 方法 |
| --- | --- |
| 发消息 | `send(target, OutgoingMessage)`、`send_msg`、`send_private_msg`、`send_group_msg`（消息载荷接受 `&str`/`String`/`OutgoingMessage`/段数组/已拼好的 JSON —— `MessagePayload`） |
| 读消息 | `get_msg`、`get_forward_msg`、`can_send_private_msg`、`can_send_group_msg` |
| 删/赞 | `delete_msg`（撤回）、`set_msg_emoji_like`、`send_like` |
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

## 9. 插件自己的配置文件 `config/<插件>/arona.yml`

框架的 `config/arona.yml` 只认 `groups` / `managers` / `global_blacklist` / `group_settings` /
`disabled_plugins`。插件的业务配置各住各的文件，以**原样 YAML 片段**存在
`arona::config::plugin_config` 里，框架不理解内容，靠插件登记的 `ConfigSection` 渲染器完成
“认键 + 生成带注释模板”。

```rust
use arona::config::arona::ConfigSection;
use serde_yaml::Value;

/// notify 配置区渲染器：一个 unit struct 即可，无状态
struct NotifySection;

impl ConfigSection for NotifySection {
    fn key(&self) -> &'static str {
        "notify"                                   // 本插件文件里的顶层键名
    }
    fn default_value(&self) -> Value {
        // 文件里缺这个键时，模板用这里渲染
        serde_yaml::to_value(NotifyConfig::default()).unwrap_or(Value::Null)
    }
    fn render(&self, value: &Value) -> String {
        // 通常先把 value 反序列化成自己的强类型配置（顺带过滤未知子键），再手写带注释片段
        let config = serde_yaml::from_value::<NotifyConfig>(value.clone()).unwrap_or_default();
        let mut out = String::new();
        out.push_str("# ==================== 每日推送 ====================\n");
        out.push_str("notify:\n");
        out.push_str(&format!("  # 是否启用每日推送\n  enable: {}\n", config.enable));
        out
        // 片段以 "key:" 开头、末尾不留空行
    }
}
```

`install()` 阶段登记（第一个参数是**你自己的插件 id**）：

```rust
fn install(&self) -> Result<(), String> {
    arona::config::arona::register_section(
        "bluearchive",
        std::sync::Arc::new(NotifySection),
    );
    Ok(())
}
```

读写自己的配置：

```rust
/// 读：拿到原样 YAML 值，反序列化成强类型，缺失/格式错回退默认值
pub fn notify() -> NotifyConfig {
    arona::config::plugin_config::section_value("notify")
        .and_then(|value| serde_yaml::from_value::<NotifyConfig>(value).ok())
        .unwrap_or_default()
}

/// 写：序列化后写回，框架负责落盘 config/bluearchive/arona.yml 并回调本插件的 on_config_reload
pub fn set_notify(config: &NotifyConfig) -> Result<(), String> {
    let value = serde_yaml::to_value(config).map_err(|err| err.to_string())?;
    arona::config::plugin_config::set_section("notify", value)
}
```

约束与行为：
- 键名要和插件功能对得上，且**不同插件之间不能撞 key**（`register_section` 撞名时保留先登记的；
  想区分就用带语义的前缀，如 `bluearchive_notify`）。
- `section_value` / `set_section` 按顶层键寻址，框架自己查归属插件（`arona::config::arona::section_owner`），
  插件不用重复传 id。
- 缺文件时框架在 `plugin_config::init()` 生成带注释模板；插件升级**新登记的顶层键**会在下次启动
  按 `default_value()` 补进用户已有的文件（用户改过的值一律不动）；已有键里新增的**子字段**
  由插件的强类型默认值兜着，下次写回时一起落盘。
- 用户手改 `config/<插件>/arona.yml` 也会被看到：框架的轮询任务发现 mtime 变了就重新加载该文件，
  并只回调该插件的 `on_config_reload`。
- `set_section` 之后必定回调 `on_config_reload`，需要即时生效的定时任务在那里重建（判“真的变了才重建”，
  别每次热重载都重建一遍）。
- GUI/`/config` 指令编辑框架自己的那几个键；插件配置区由插件的指令或手改 YAML 维护。
  GUI「插件管理」页会列出每个插件的配置文件与数据目录，并给「打开」按钮。

## 10. 事件钩子：听到 OneBot 的全部事件

插件入口不止“注册命令”——命令只在文本命中 `/抽卡` 这类前缀时才进插件。成员进群打招呼、被踢后清理数据、
有人申请加群、群名被改、别人引用了机器人的消息，这些属于 notice/request/meta 事件，
或者属于“命中命令之前先被看一眼”的消息事件。统一入口是 `arona::onebot::hooks`：

```rust
use arona::onebot::{EventContext, HookFlow, event_handler, on_message, on_notice};

fn configure(&self, ctx: &PluginContext) -> Result<(), String> {
    let id = ctx.plugin_id().to_string();

    on_notice(&id, event_handler(|ctx: std::sync::Arc<EventContext>| {
        Box::pin(async move {
            if ctx.field_str("notice_type") == Some("group_increase") {
                let _ = ctx.reply("老师好！").await;
            }
            HookFlow::Pass          // 通知事件一般继续往下走
        })
    }));

    on_message(&id, event_handler(|ctx| Box::pin(async move {
        if ctx.text.contains("早安") {
            let _ = ctx.reply("老师早安！").await;
            return HookFlow::Handled;   // 已处理，别再走命令分发
        }
        HookFlow::Pass
    })));
    Ok(())
}
```

`EventContext` 提供：`kind`、原始 `event`、`text`、`segments`、`api: OneBotApi`（§8），
以及 `user_id()/group_id()/is_group()/is_private()/message_id()/field(key)/field_str(key)`、
`target()`、`reply(text)`、`reply_message(OutgoingMessage)`、`recall(message_id)`。

订阅入口：`subscribe(plugin, &[EventKind::..], handler)`，或 `on_message` / `on_notice` / `on_request` /
`on_meta` / `on_all`（第一个参数都是插件 id）。注册顺序即执行顺序。

门控语义（框架负责，插件不用自己判断）：
- `message`：先过「群授权 + 全局/群内黑名单」，再进钩子，最后才是命令分发；
- `notice` / `request` / `meta`：无条件投递（机器人被踢、黑名单用户申请加群这类事也会触发，
  插件自己要清楚这一点）；
- 任何一条钩子返回 `HookFlow::Handled` 就停止后续钩子，并且不再走命令分发；
- **所属插件被禁用（全局或该群）时，它的钩子一律跳过**。

`stop()` 里记得 `arona::onebot::hooks::unsubscribe(PLUGIN_ID)`（§6）。

## 11. 新增一个插件

1. 在 workspace 里建 crate，`Cargo.toml` 依赖框架（**务必 `default-features = false`**，见下节）：
   ```toml
   [dependencies]
   arona = { path = "../../crates/arona", version = "1.0.0", default-features = false }
   ```
2. 实现 `AronaPlugin`（`meta` 必填 id+name+version），并提供无参 `::new()`（注册代码要调它）。
3. 在仓库根目录 `plugins.toml` 的数组里加一行类型路径：
   ```toml
   plugins = [
       "bluearchive_plugin::BluearchivePlugin",
       "your_plugin::YourPlugin",
   ]
   ```
4. 在 host `crates/arona-host/Cargo.toml` 的 `[dependencies]` 里加上该 crate
   （同样 `default-features = false`）。
5. 把新 crate 加进根 `Cargo.toml` 的 `members`。

`crates/arona-host/build.rs` 读 `plugins.toml` 生成 `OUT_DIR/plugins.rs`（一个
`register_plugins()`，按清单顺序 `arona::plugin::register(..)`），`main.rs` 用 `include!`
引回并在 `arona::run(..)` 之前调用它 —— **不要再去 main.rs 里硬编码注册**。改了 `plugins.toml`
无需改任何 Rust 代码，build.rs 已 `rerun-if-changed` 该文件。

首次启动后 `plugins/<id>/`、`config/<id>/arona.yml` 会自动出现，GUI「插件管理」页立刻能开关它。

## 12. feature 统一陷阱：GUI 只在 host 打开

`arona` 的 `default = ["gui"]` 会拉进 `eframe`/`wgpu`。插件与 host 都以 `default-features = false` 依赖 `arona`，
**只有 host 通过自身的 `gui = ["arona/gui"]` feature 打开 GUI**。这样插件不参与 GUI 编译、也不改变框架 GUI 开关的归属，
避免 Cargo feature 统一把 `gui` 意外扩散。host 的 `default = ["gui"]` 决定了产物是否含管理面板。

## 13. 开发与调试

```bash
cargo run -p arona-host                  # 默认打开管理面板 GUI
cargo run -p arona-host -- --nogui       # 纯命令行模式（黑窗口）
cargo run -p arona-host -- --test-notify # 20 秒后跑一次每日推送，便于联调
cargo build -p arona-host --no-default-features   # 精简命令行版
```

改动后的验证口径（本地 GUI 构建需要 ATL，见下节“零依赖”约束；纯逻辑改动用 `--no-default-features` 更快）：

```bash
cargo check -p arona --features gui --all-targets   # 含 GUI 代码与测试
cargo test --workspace --no-default-features        # 单测（生命周期门控/配置迁移/钩子）
cargo fmt --all
```

或用 `.cargo/config.toml` 里的别名：`cargo build-release` / `cargo build-nogui` / `cargo gui` /
`cargo run-nogui` / `cargo dist`（整理交付产物）/ `cargo installer`（打安装包）/ `cargo smoke-*`（各链路自检）。
本地联调 OneBot 时，ws-forward（框架连出去到一个 WS 服务）比 ws-reverse 好驱动：
后者在本项目里不把 `admin::call_api` 路由给已连接客户端。

因为插件与框架在同一 workspace，断点可直接打在插件源码里。
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

## 14. 写插件代码时的注意事项

- **不要给死代码加 `#[allow]`**：插件里保留了不少尚未接线的功能模块，`cargo` 报的
  `never used` 警告是预期的，不要为了“零警告”去删或压制。
- 涉及进程级全局（分发器槽位、钩子表、配置缓存）的测试必须串行：用
  `static LOCK: Mutex<()> = Mutex::new(());` + `let _serial = LOCK.lock().unwrap_or_else(|p| p.into_inner());`
  的写法，测试里改过全局状态要在结尾还原。异步的用 `#[tokio::test(flavor = "current_thread")]`。
- 插件代码里不要 `std::process::exit`，也不要长阻塞主线程；耗时活计 `tokio::spawn` 或用 `arona::quartz` 定时任务。
- 日志走 `arona::runtime::log::{info, warning, error}`，不要用 `println!`（GUI 模式没有控制台）。
