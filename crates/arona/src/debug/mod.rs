//! 运行期自省：把框架此刻的内部状态取成一份结构化快照。
//!
//! 面向的问题只有一个：**程序跑起来之后，除了翻日志，还能怎么看它现在在干什么**。
//! 插件装了几个、各自什么状态、注册了哪些命令、订了哪些事件、排了哪些定时任务、
//! 有没有谁 panic 过、连接在不在、内存多少、最近几百行日志是什么——这些在框架里本来就
//! 各有一份，只是散在 `PluginManager` / `CommandRegistry` / `HookRegistry` / `Scheduler` /
//! `ViewBoard` / `HealthBoard` / `runtime::log` 里，没有一个统一出口。这个模块就是那个出口。
//!
//! ## 为什么是运行期开关，而不是 cargo feature
//!
//! 「debug 版本」听起来该做成 `--features debug`，但**不能**：插件 ABI 的工具链指纹里
//! 含 feature 集合（见 `crates/arona/build.rs` 的 `ABI_NEUTRAL_FEATURES`，只有 `gui`/`default`
//! 被豁免）。多一个 feature，同一个 rustc 编出来的宿主与插件就会算出两份指纹，两边互相拒载，
//! 等于把「调试版」变成「只能配调试版插件」的另一套生态。
//!
//! 所以这里不新增任何 feature：**同一份二进制**，默认不开调试口；启动时加
//! `--debug-api`（或 `ARONA_DEBUG_API=1`）才监听。生产用户什么都不用做，也就没有任何
//! 额外攻击面；要排障时同一个 exe 重启一次就能开。
//!
//! ## 也不碰 `arona.yml`
//!
//! 开关全部走启动参数与环境变量，没有往 [`AronaConfig`](crate::config::arona::AronaConfig)
//! 里加字段。那个结构体是插件会直接读的 `pub` 类型——加字段就得抬 `ABI_LAYOUT`，
//! 所有插件重编。调试口这点便利不值得让五个仓库跟着发版。
//!
//! ## 暴露面
//!
//! 默认只绑 `127.0.0.1:8787`，且要求 token。绑到非回环地址却没给 token 时**拒绝启动**
//! ——一份能读出全部插件状态、命令表与最近日志的东西挂在公网上，等于把管理面板的钥匙
//! 丢在门口。
//!
//! ## 端点
//!
//! | 路径 | 内容 |
//! | --- | --- |
//! | `GET /debug/snapshot` | 完整快照（JSON），`?tail=N` 控制日志行数 |
//! | `GET /debug/logs` | 最近日志（纯文本，一行一条） |
//! | `GET /debug/send-test` | 往调试群发一条带固定前缀的测试消息，确认发送链路通不通 |
//! | `GET /debug/healthz` | `ok`，存活探测 |
//!
//! 鉴权：`Authorization: Bearer <token>`，或 `?token=<token>`（给浏览器直接点开用）。
//!
//! ## 调试群是干什么的
//!
//! 调试接口绑在回环地址上，只有**本机**能连。但排障的往往不是本机的终端：Agent 跑在
//! 别处、人在手机上、或者只是不想为了看一眼状态去开个 SSH 隧道。所以设置里留一个
//! 「调试群」——`/debug/send-test` 会让机器人往那个群发一条带 `【Arona 调试】` 前缀的
//! 测试消息，Agent 拿到 message_id、主人在群里看到实物，两边一对就知道**发送链路**
//! 断在哪一层：插件没发 / 端口没通 / OneBot 掉线 / 群号错 / 机器人不在群里。
//!
//! **读**状态永远走端口（Agent 自己拉 JSON），群里只做这一件事：确认「消息发出去没有」。
//! 固定前缀是安全边界，不是装饰——令牌万一泄露，持有者也只能在调试群刷带标记的测试
//! 消息，没法让机器人说出像人说的话。
//!
//! ## GUI 开关
//!
//! 「关于」页有一块调试设置（启用开关 / 端口 / 调试群 / 令牌），点「确认生效」当场启停，
//! 不用重启程序。持久化写在 `config/debug.yml`——**不是** `arona.yml`，理由同上：
//! 那个结构体跨边界，加字段要抬 ABI。这里单独一个小文件，框架自己的事自己存。

use crate::framework::Framework;
use crate::runtime::log::LiveLine;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

/// 快照格式版本。外部工具（脚本、面板）按它判断字段含义，加字段不动它，
/// 删字段/改语义才抬。
pub const SCHEMA: u32 = 1;

/// 一份快照默认带多少行日志
pub const DEFAULT_LOG_TAIL: usize = 200;

static BOOTED: OnceLock<Instant> = OnceLock::new();

/// 记下启动时刻。`run` 里尽早调用；不调用也不会出错，只是 uptime 从首次访问算起。
pub fn boot() {
    let _ = BOOTED.set(Instant::now());
}

fn uptime_secs() -> u64 {
    BOOTED.get_or_init(Instant::now).elapsed().as_secs()
}

// ==================== 快照结构 ====================

/// 运行期状态的一份快照。
///
/// 全部字段都是**值**（`String`/数字/`Vec`），不带任何跨边界的指针或句柄：这份结构
/// 会直接序列化成 JSON 交给外部进程，也可能被插件拿去贴进消息里。
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    /// 快照格式版本（[`SCHEMA`]）
    pub schema: u32,
    /// 采集时刻（RFC3339，本地时区）
    pub taken_at: String,
    pub process: Process,
    pub connection: Connection,
    pub plugins: Vec<Plugin>,
    pub commands: Vec<Command>,
    /// 事件订阅。框架只存订阅方，不存"每个插件收了多少次"——那个计数要改热路径，
    /// 不在这一版里做（要做得先抬 ABI，见模块头）。
    pub hooks: Vec<Hook>,
    pub jobs: Vec<Job>,
    pub views: Vec<View>,
    pub config: Config,
    /// 最近若干次 OneBot 动作（发送是否成功、耗时多久）
    pub outbox: Vec<OutgoingCall>,
    /// 最近日志（最旧在前）
    pub logs: Vec<LogLine>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Process {
    pub pid: u32,
    /// 框架 crate 版本
    pub version: String,
    /// 符号布局号：插件与宿主的握手判据
    pub abi_layout: u32,
    /// 工具链指纹（宿主这一份）
    pub toolchain: String,
    pub uptime_secs: u64,
    /// 专用工作集字节数（与任务管理器同口径）；取不到就是 `null`
    pub memory_bytes: Option<u64>,
    pub cpus: usize,
    pub platform: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Connection {
    pub online: bool,
    pub self_id: i64,
    pub nickname: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Plugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub loader: String,
    /// `运行中` / `已装配` / `已装载` / `已登记` / `已停用` / `失败: <原因>`
    pub state: String,
    /// 装配完成且此刻在跑（含 `Ready`）
    pub running: bool,
    /// 通过 `/bili` 那类总开关判定后是否生效（`arona.yml` 的 `disabledPlugins` 为假）
    pub enabled: bool,
    /// 按群生效（`PluginMeta::group_scoped`）
    pub group_scoped: bool,
    /// 累计失败次数（panic 与任务超时都记在这里；隔离停用后会被清空）
    pub panics: u32,
    /// 在 `arona.yml` 的 `disabledPlugins` 里（用户手动停用或框架隔离停用都写这一格）
    pub disabled_in_config: bool,
    pub commands: usize,
    pub hooks: usize,
    pub jobs: usize,
    pub depends: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Command {
    pub plugin: String,
    pub name: String,
    pub usage: String,
    pub description: String,
    pub feature: String,
    pub priority: String,
    pub permission: String,
    /// 登记序号：一条命令的多个别名里序号最小的是主名
    pub seq: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Hook {
    pub plugin: String,
    pub feature: String,
    pub priority: String,
    /// 订阅范围（`All`、`群消息`、`notice:...` 之类，取框架自己的说法）
    pub filters: Vec<String>,
    /// 出站钩子（发消息之前改内容/拦停的那一类）
    pub outbound: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub name: String,
    /// 归属分组；插件排的任务用插件 id
    pub group: String,
    pub next_fire_ms: Option<i64>,
    pub last_fire_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct View {
    /// 会话号（群号或好友号）
    pub peer: i64,
    pub plugin: String,
    pub rows: Vec<ViewRow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ViewRow {
    pub label: String,
    pub items: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Config {
    /// 允许响应的群（空 = 全部）
    pub groups: Vec<i64>,
    pub managers: Vec<i64>,
    pub global_blacklist: usize,
    pub group_settings: usize,
    pub disabled_plugins: Vec<String>,
    pub disabled_features: Vec<String>,
    pub prefix_match_by_default: bool,
    pub command_prefixes: Vec<String>,
    pub panic_disable_threshold: u32,
    /// 各插件登记的功能开关个数
    pub features: usize,
    pub config_file: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    pub time: String,
    pub stderr: bool,
    pub text: String,
}

/// 一次 OneBot 动作的结局。
///
/// 主人说"机器人没反应"时，光看插件日志判断不了是**插件压根没发**、还是**发了但实现端拒了**。
/// 这里把每一次动作的成败与耗时都留一份，Agent 一眼能看出卡在哪一层。
#[derive(Clone, Debug, Serialize)]
pub struct OutgoingCall {
    /// 本机时间 HH:MM:SS
    pub at: String,
    /// OneBot 动作名（`send_group_msg` / `get_msg` / …）
    pub action: String,
    pub ok: bool,
    /// 失败原因；成功时是空串
    pub detail: String,
    pub millis: u64,
}

/// 动作记录留多少条：够看清"最近有没有发出去"，又不至于把内存吃掉
const OUTBOX_CAPACITY: usize = 200;

fn outbox() -> &'static std::sync::Mutex<std::collections::VecDeque<OutgoingCall>> {
    static OUTBOX: OnceLock<std::sync::Mutex<std::collections::VecDeque<OutgoingCall>>> =
        OnceLock::new();
    OUTBOX.get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
}

/// 记一次 OneBot 动作（成功/失败/耗时）。调用方是 `OneBotApi::raw`——所有动作都过它。
pub fn record_call(action: &str, ok: bool, detail: &str, elapsed: std::time::Duration) {
    let Ok(mut queue) = outbox().lock() else {
        return;
    };
    if queue.len() >= OUTBOX_CAPACITY {
        queue.pop_front();
    }
    queue.push_back(OutgoingCall {
        at: chrono::Local::now().format("%H:%M:%S").to_string(),
        action: action.to_string(),
        ok,
        detail: detail.to_string(),
        millis: elapsed.as_millis().min(u64::MAX as u128) as u64,
    });
}

/// 最近若干次动作（最旧在前）
pub fn recent_calls(limit: usize) -> Vec<OutgoingCall> {
    let Ok(queue) = outbox().lock() else {
        return Vec::new();
    };
    let skip = queue.len().saturating_sub(limit);
    queue.iter().skip(skip).cloned().collect()
}

/// 清空动作记录（"我要重新看一次"时用）
pub fn clear_calls() {
    if let Ok(mut queue) = outbox().lock() {
        queue.clear();
    }
}

/// 「发送自检」那条消息的正文。
///
/// 固定前缀是**安全边界**，不是装饰：令牌万一泄露，持有者也只能在调试群里刷带标记的
/// 测试消息，没法让机器人说出像人说的话（不能冒充主人在别的群发指令）。
pub fn test_message(extra: &str) -> String {
    let extra: String = extra.trim().chars().take(300).collect();
    let head = format!(
        "【Arona 调试】发送链路自检 · {}",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    if extra.is_empty() {
        head
    } else {
        format!("{head}\n{extra}")
    }
}

// ==================== 采集 ====================

/// 取一份当前快照（进程默认实例）
pub fn snapshot() -> Snapshot {
    snapshot_of(&Framework::global_arc())
}

/// 取一份指定实例的快照。插件侧直接调 [`snapshot`] 即可（`attach_host` 已经把
/// 插件里那份 `Framework::global()` 改指宿主实例，取到的就是宿主的状态）。
pub fn snapshot_of(framework: &Framework) -> Snapshot {
    let plugins = collect_plugins(framework);
    let commands = collect_commands(framework);
    let hooks = collect_hooks(framework);
    let jobs = framework
        .jobs()
        .list()
        .into_iter()
        .map(|task| Job {
            name: task.name,
            group: task.group,
            next_fire_ms: task.next_fire_ms,
            last_fire_ms: task.last_fire_ms,
        })
        .collect();

    Snapshot {
        schema: SCHEMA,
        taken_at: chrono::Local::now().to_rfc3339(),
        process: collect_process(framework),
        connection: collect_connection(framework),
        plugins,
        commands,
        hooks,
        jobs,
        views: collect_views(framework),
        config: collect_config(framework),
        outbox: recent_calls(50),
        logs: collect_logs(DEFAULT_LOG_TAIL),
    }
}

fn collect_process(_framework: &Framework) -> Process {
    Process {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        abi_layout: crate::plugin::abi::ABI_LAYOUT,
        toolchain: crate::plugin::abi::TOOLCHAIN_FINGERPRINT.to_string(),
        uptime_secs: uptime_secs(),
        memory_bytes: crate::runtime::memory::process_memory_bytes(),
        cpus: std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(0),
        platform: crate::runtime::platform::describe(),
    }
}

fn collect_connection(framework: &Framework) -> Connection {
    let bot = crate::bot::Bot::of(&framework_instance(framework));
    Connection {
        online: bot.is_online(),
        self_id: bot.self_id(),
        nickname: bot.nickname(),
    }
}

/// `Bot::of` 要的是 `&Arc<Framework>`；快照只拿到 `&Framework`，这里换一份出来。
///
/// 进程默认实例那条路是零成本的（`global_arc` 只 clone 一次 Arc）；非默认实例
/// （测试里手工搭的那种）借不到 Arc，只能退回默认实例——连接状态本来就跟着默认实例走，
/// 这个退化不影响结论。
fn framework_instance(_framework: &Framework) -> std::sync::Arc<Framework> {
    Framework::global_arc()
}

fn collect_plugins(framework: &Framework) -> Vec<Plugin> {
    let manager = framework.plugins();
    let health = framework.health();
    let hooks = framework.hooks();
    let gating = framework.gating();
    let disabled = framework.settings().config().disabled_plugins;

    manager
        .ordered()
        .iter()
        .map(|plugin| {
            let meta = plugin.meta();
            let id = meta.id;
            // `PluginState` 不是 `Copy`（`Failed` 里带原因串），
            // 两处判断各取一次，别指望能复用同一个值
            let running = plugin.state().is_running();
            Plugin {
                id: id.to_string(),
                name: meta.name.to_string(),
                version: meta.version.to_string(),
                author: meta.author.to_string(),
                loader: plugin.loader().to_string(),
                state: plugin.state().display_name().to_string(),
                running,
                // 停用可能是 arona.yml 的总开关，也可能是分群名单，这里报的是"全局那份"
                enabled: gating.plugin_enabled(id),
                group_scoped: meta.group_scoped,
                panics: health.failures(id),
                disabled_in_config: disabled.iter().any(|entry| entry.eq_ignore_ascii_case(id)),
                commands: framework.commands().commands_of(id).len(),
                hooks: hooks.count_of_plugin(id),
                jobs: framework
                    .jobs()
                    .list()
                    .iter()
                    .filter(|task| task.group == id)
                    .count(),
                depends: meta.depends.iter().map(|id| id.to_string()).collect(),
            }
        })
        .collect()
}

fn collect_commands(framework: &Framework) -> Vec<Command> {
    framework
        .commands()
        .commands()
        .into_iter()
        .map(|info| Command {
            plugin: info.plugin,
            name: info.name,
            usage: info.usage,
            description: info.description,
            feature: info.feature.to_string(),
            priority: info.priority.display_name().to_string(),
            permission: format!("{:?}", info.permission),
            seq: info.seq,
        })
        .collect()
}

fn collect_hooks(framework: &Framework) -> Vec<Hook> {
    framework
        .hooks()
        .describe()
        .into_iter()
        .map(|info| Hook {
            plugin: info.plugin,
            feature: info.feature.to_string(),
            priority: info.priority.display_name().to_string(),
            filters: info.filters,
            outbound: info.outbound,
        })
        .collect()
}

fn collect_views(framework: &Framework) -> Vec<View> {
    framework
        .views()
        .snapshot()
        .into_iter()
        .map(|(peer, plugin, rows)| View {
            peer,
            plugin,
            rows: rows
                .into_iter()
                .map(|row| ViewRow {
                    label: row.label,
                    items: row.items,
                })
                .collect(),
        })
        .collect()
}

fn collect_config(framework: &Framework) -> Config {
    let settings = framework.settings();
    let conf = settings.config();
    Config {
        groups: conf.groups.clone(),
        managers: conf.managers.clone(),
        global_blacklist: conf.global_blacklist.len(),
        group_settings: conf.group_settings.len(),
        disabled_plugins: conf.disabled_plugins.clone(),
        disabled_features: conf.disabled_features.clone(),
        prefix_match_by_default: framework.commands().prefix_match_by_default(),
        command_prefixes: framework.commands().command_prefixes(),
        panic_disable_threshold: framework.health().threshold(),
        features: framework.gating().features().len(),
        config_file: settings
            .config_file()
            .map(|path| path.display().to_string()),
    }
}

fn collect_logs(tail: usize) -> Vec<LogLine> {
    let mut lines = crate::runtime::log::live_lines();
    if tail > 0 && lines.len() > tail {
        lines.drain(..lines.len() - tail);
    }
    lines
        .into_iter()
        .map(|LiveLine { time, text, stderr }| LogLine { time, stderr, text })
        .collect()
}

/// 快照的 JSON 文本（美化过，方便人直接看）
pub fn snapshot_json() -> String {
    to_json(&snapshot())
}

/// 把一份快照编成 JSON。序列化不该失败——全是值类型，失败了说明这个模块本身写错了。
pub fn to_json(snapshot: &Snapshot) -> String {
    serde_json::to_string_pretty(snapshot)
        .unwrap_or_else(|err| format!("{{\"error\":\"快照序列化失败: {err}\"}}"))
}

// ==================== 落盘 ====================

/// 把当前快照写进 `dir`，文件名带时间戳；返回写出的路径。
///
/// 给"程序卡住了、来不及连调试口"的场景用：先 dump 一份在盘上再慢慢看。
pub fn write_snapshot(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("snapshot-{stamp}.json"));
    std::fs::write(&path, snapshot_json())?;
    Ok(path)
}

// ==================== 持久化设置与启停 ====================

/// 调试接口的持久化设置（`config/debug.yml`）。
///
/// 单独一个文件、而不是塞进 `arona.yml`：后者是插件会直接读的 `pub` 类型，加字段要抬
/// `ABI_LAYOUT`，而这一份只有框架自己看。改动它不影响任何插件。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// 是否启用。默认**关**——生产用户不该被动多开一个端点
    pub enabled: bool,
    /// 监听端口（绑定地址由 [`Settings::lan`] 决定）
    pub port: u16,
    /// 是否允许局域网访问：`true` 绑 `0.0.0.0`，`false` 只绑回环。
    ///
    /// 默认 `false`。远程 Agent 读状态需要它；绑非回环地址**必须**有令牌，
    /// 所以 [`apply`] 会在令牌为空时先生成一个。
    pub lan: bool,
    /// 调试群号：`/debug/send-test` 把发送自检消息发到这里；`0` 表示没配
    pub group: i64,
    /// 访问令牌。首次启用时自动生成，GUI 里可以复制
    pub token: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            port: DEFAULT_PORT,
            lan: false,
            group: 0,
            token: String::new(),
        }
    }
}

impl Settings {
    /// 该绑哪个地址。
    ///
    /// 这一条以前写死在 [`apply`] 里（永远是 `127.0.0.1`），于是 GUI 开的调试口
    /// 在局域网里**永远连不进来**——远程 Agent 只能干看着。现在由 [`Settings::lan`]
    /// 决定，默认仍是回环：不主动扩大暴露面，但至少给了条路。
    pub fn bind(&self) -> String {
        if self.lan {
            format!("0.0.0.0:{}", self.port)
        } else {
            format!("127.0.0.1:{}", self.port)
        }
    }
}

/// 设置文件：`config/debug.yml`
pub fn settings_file() -> PathBuf {
    crate::runtime::paths::config_root().join("debug.yml")
}

/// 读设置。文件不在、读坏了都退回默认值——而默认是「关」，所以坏文件等于没开，
/// 不会因为手抖写坏一个 YAML 就把端点暴露出去。
pub fn load_settings() -> Settings {
    load_settings_from(&settings_file())
}

/// 指定路径的读（测试用；生产走 [`load_settings`]）
fn load_settings_from(path: &Path) -> Settings {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    match serde_yaml::from_str::<Settings>(&text) {
        Ok(mut settings) => {
            if settings.port == 0 {
                settings.port = DEFAULT_PORT;
            }
            settings
        }
        Err(err) => {
            crate::runtime::log::warning(format!(
                "调试设置 {} 读不出来，按「关闭」处理: {err}",
                path.display()
            ));
            Settings::default()
        }
    }
}

/// 写设置。文件头带两行注释：用户手改时不用回来翻文档。
pub fn save_settings(settings: &Settings) -> Result<(), String> {
    save_settings_to(&settings_file(), settings)
}

fn save_settings_to(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("建配置目录失败: {err}"))?;
    }
    let text = format!(
        "# Arona 调试接口（GUI「关于」页也能改）。enabled 只影响这个只读端点，不影响机器人本身；\n\
         # lan=true 时绑 0.0.0.0（局域网可访问，必须有 token），false 只绑 127.0.0.1；\n\
         # group 是发送自检的调试群号（0 表示不配）。\n\
         enabled: {}\nport: {}\nlan: {}\ngroup: {}\ntoken: \"{}\"\n",
        settings.enabled, settings.port, settings.lan, settings.group, settings.token
    );
    std::fs::write(path, text).map_err(|err| format!("写 {} 失败: {err}", path.display()))
}

/// 生成够长的令牌：两个 UUID v4 拼成 64 个十六进制字符
fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

struct Running {
    addr: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
}

fn running() -> &'static std::sync::Mutex<Option<Running>> {
    static RUNNING: OnceLock<std::sync::Mutex<Option<Running>>> = OnceLock::new();
    RUNNING.get_or_init(|| std::sync::Mutex::new(None))
}

/// 当前监听地址；没开就是 `None`（GUI 用它显示状态）
pub fn listening_at() -> Option<SocketAddr> {
    running()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|run| run.addr))
}

/// 停掉当前监听。没开时是空操作，可以随便调。
pub fn stop() {
    if let Ok(mut guard) = running().lock() {
        if let Some(run) = guard.take() {
            let _ = run.stop.send(());
        }
    }
}

/// 按设置启停：先关旧的，再按 `enabled` 决定起不起；成功后落盘。
///
/// GUI 的「确认生效」与启动参数走同一条路，两边行为不会分叉。
/// 返回一句给人看的结果，失败是说得出原因的那种（端口被占、权限不足等）。
pub async fn apply(settings: &Settings) -> Result<String, String> {
    let mut settings = settings.clone();
    if settings.token.trim().is_empty() {
        settings.token = new_token();
    }
    if settings.port == 0 {
        return Err("端口不能写 0".to_string());
    }
    stop();
    if !settings.enabled {
        save_settings(&settings)?;
        crate::runtime::log::info("调试接口已关闭");
        return Ok("调试接口已关闭".to_string());
    }
    let config = ServeConfig {
        // 绑哪个地址由设置决定：默认回环，勾了「允许局域网访问」才开 0.0.0.0
        bind: settings.bind(),
        token: Some(settings.token.clone()),
        log_tail: DEFAULT_LOG_TAIL,
        group: settings.group,
    };
    let addr = serve(&config).await?;
    save_settings(&settings)?;
    crate::runtime::log::info_green(format!("调试接口已启用: http://{addr}/debug/snapshot"));
    Ok(format!("已启用：http://{addr}/debug/snapshot"))
}

// ==================== 启动参数 ====================

/// 调试接口的启动配置
#[derive(Clone, Debug, PartialEq)]
pub struct ServeConfig {
    pub bind: String,
    pub token: Option<String>,
    pub log_tail: usize,
    /// 调试群号：`/debug/to-group` 的收件地址，`0` 表示没配
    pub group: i64,
}

/// 默认端口与绑定地址（只绑回环：这份东西能读出全部插件状态与最近日志）
pub const DEFAULT_PORT: u16 = 8787;
pub const DEFAULT_BIND: &str = "127.0.0.1:8787";

impl Default for ServeConfig {
    fn default() -> Self {
        ServeConfig {
            bind: DEFAULT_BIND.to_string(),
            token: None,
            log_tail: DEFAULT_LOG_TAIL,
            group: 0,
        }
    }
}

/// 从启动参数 + 环境变量里读调试接口配置。
///
/// 返回 `None` 表示没要求开（绝大多数情况）。参数优先于环境变量：
/// `--debug-api` / `ARONA_DEBUG_API=1`、`--debug-bind=127.0.0.1:8787` / `ARONA_DEBUG_BIND`、
/// `--debug-token=xxx` / `ARONA_DEBUG_TOKEN`。
pub fn requested(args: &[String]) -> Option<ServeConfig> {
    let flag = args.iter().any(|arg| arg == "--debug-api") || env_flag("ARONA_DEBUG_API");
    if !flag {
        return None;
    }
    let mut config = ServeConfig::default();
    if let Some(value) = arg_value(args, "--debug-bind=").or_else(|| env_string("ARONA_DEBUG_BIND"))
    {
        config.bind = value;
    }
    if let Some(value) =
        arg_value(args, "--debug-token=").or_else(|| env_string("ARONA_DEBUG_TOKEN"))
    {
        config.token = Some(value);
    }
    Some(config)
}

fn arg_value(args: &[String], prefix: &str) -> Option<String> {
    args.iter()
        .find_map(|arg| arg.strip_prefix(prefix).map(str::trim))
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_flag(key: &str) -> bool {
    env_string(key).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

/// 地址是不是回环（`127.0.0.0/8`、`::1`、`localhost`）
pub fn is_loopback(bind: &str) -> bool {
    let host = bind.rsplit_once(':').map(|(host, _)| host).unwrap_or(bind);
    let host = host.trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

// ==================== HTTP 接口 ====================

mod http {
    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::{Html, IntoResponse, Response};
    use axum::routing::get;

    #[derive(Clone)]
    struct Api {
        token: Option<String>,
        log_tail: usize,
        /// 调试群号：发送自检的默认收件地址
        group: i64,
    }

    /// 起调试口：绑定、鉴权、挂路由。调用方必须已在 tokio 运行时里。
    ///
    /// 会先停掉已有的监听再绑：同一份进程不该同时开两个口，而且旧 listener 不先放开，
    /// 改端口后重绑同一个端口会直接失败。
    pub async fn serve(config: &ServeConfig) -> Result<SocketAddr, String> {
        // 参数不合法就别动现有的监听：配置写错不该把正在跑的那个也停掉
        if config.token.is_none() && !is_loopback(&config.bind) {
            return Err(format!(
                "调试接口绑在 {} 上却没有 token；绑非回环地址必须给 --debug-token=<令牌>",
                config.bind
            ));
        }
        stop();
        let listener = tokio::net::TcpListener::bind(&config.bind)
            .await
            .map_err(|err| format!("调试接口绑定 {} 失败: {err}", config.bind))?;
        let addr = listener
            .local_addr()
            .map_err(|err| format!("读不到调试接口的本地地址: {err}"))?;

        let api = Api {
            token: config.token.clone(),
            log_tail: config.log_tail,
            group: config.group,
        };
        let app = Router::new()
            .route("/", get(index))
            .route("/debug/snapshot", get(snapshot_route))
            .route("/debug/logs", get(logs))
            .route("/debug/send-test", get(send_test))
            .route("/debug/healthz", get(healthz))
            .with_state(api);

        // 关停靠这条 oneshot：GUI 点「关闭」或改了端口，都得让旧 listener 真正退出。
        // 否则端口一直占着，会出现"旧的还能连、新的也起不来"的鬼现象。
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async {
                let _ = stop_rx.await;
            });
            if let Err(err) = server.await {
                crate::runtime::log::error(format!("调试接口已停止: {err}"));
            }
        });
        if let Ok(mut guard) = super::running().lock() {
            *guard = Some(super::Running {
                addr,
                stop: stop_tx,
            });
        }
        Ok(addr)
    }

    /// 令牌对不对。比较用整串相等——这里防的是"随手扫到的端口"，不是抗时序攻击的场景，
    /// 而且令牌本身要求够长（见 [`ServeConfig::token`] 的说明）。
    fn allowed(api: &Api, headers: &HeaderMap, uri: &Uri) -> bool {
        let Some(expected) = api.token.as_deref() else {
            return true;
        };
        if let Some(value) = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        {
            if value == expected {
                return true;
            }
        }
        uri.query()
            .into_iter()
            .flat_map(|query| query.split('&'))
            .filter_map(|pair| pair.split_once('='))
            .any(|(key, value)| key == "token" && value == expected)
    }

    fn unauthorized() -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [("content-type", "text/plain; charset=utf-8")],
            "缺少或错误的 token：加 Authorization: Bearer <token>，或 ?token=<token>",
        )
            .into_response()
    }

    async fn index() -> Html<&'static str> {
        Html(
            r#"<!doctype html><meta charset="utf-8"><title>Arona 运行期自省</title>
<style>body{font:14px/1.6 system-ui,sans-serif;margin:2rem;max-width:52rem}
input{width:22rem;padding:.3rem}pre{background:#f4f4f6;padding:1rem;overflow:auto;max-height:60vh}</style>
<h2>Arona 运行期自省</h2>
<p>令牌：<input id="t" placeholder="--debug-token 给的那个"> <button onclick="load()">取快照</button></p>
<pre id="out">（尚未加载）</pre>
<script>
async function load(){
  const t=document.getElementById('t').value.trim();
  const r=await fetch('/debug/snapshot?token='+encodeURIComponent(t));
  document.getElementById('out').textContent=await r.text();
}
</script>"#,
        )
    }

    async fn healthz() -> &'static str {
        "ok"
    }

    /// 从查询串里取一个参数（`?a=1&b=2`）。不引 `Query` 提取器：这里只要几个可选值，
    /// 手写一遍比给每个 handler 定义结构体省事，也避免 `serde_urlencoded` 那一串依赖。
    fn query_param(uri: &Uri, key: &str) -> Option<String> {
        uri.query()
            .into_iter()
            .flat_map(|query| query.split('&'))
            .filter_map(|pair| pair.split_once('='))
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.to_string())
    }

    /// 往调试群发一条带固定前缀的测试消息，用来确认**发送链路**通不通。
    ///
    /// 读状态走 `/debug/snapshot`（Agent 直接读端口），这条只回答一个问句：
    /// 「现在这一秒，机器人能不能把消息送进这个群」。返回里带 message_id 与耗时，
    /// 主人那边看群有没有弹出消息，两边一对就知道断在哪一层（插件没发 / 端口 / OneBot / 群号错）。
    async fn send_test(State(api): State<Api>, headers: HeaderMap, uri: Uri) -> Response {
        if !allowed(&api, &headers, &uri) {
            return unauthorized();
        }
        let group = query_param(&uri, "group")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(api.group);
        if group <= 0 {
            return (
                StatusCode::BAD_REQUEST,
                "没有配置调试群：GUI「关于」页填一个群号，或请求时带 ?group=<群号>",
            )
                .into_response();
        }
        let text = super::test_message(&query_param(&uri, "text").unwrap_or_default());
        let started = std::time::Instant::now();
        match crate::bot::Bot::current()
            .api()
            .send_group_msg(group, text)
            .await
        {
            Ok(id) => (
                StatusCode::OK,
                format!(
                    "已投递到群 {group}：message_id={id}，耗时 {} ms。请主人在群里确认是否收到。",
                    started.elapsed().as_millis()
                ),
            )
                .into_response(),
            Err(err) => (
                StatusCode::BAD_GATEWAY,
                format!("投递到群 {group} 失败：{err}。先确认 OneBot 在线、机器人确实在这个群里。"),
            )
                .into_response(),
        }
    }

    async fn snapshot_route(State(api): State<Api>, headers: HeaderMap, uri: Uri) -> Response {
        if !allowed(&api, &headers, &uri) {
            return unauthorized();
        }
        let tail = query_param(&uri, "tail")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(api.log_tail);
        let mut snap = super::snapshot();
        snap.logs = collect_logs(tail);
        (
            StatusCode::OK,
            [("content-type", "application/json; charset=utf-8")],
            to_json(&snap),
        )
            .into_response()
    }

    async fn logs(State(api): State<Api>, headers: HeaderMap, uri: Uri) -> Response {
        if !allowed(&api, &headers, &uri) {
            return unauthorized();
        }
        let tail = query_param(&uri, "tail")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(api.log_tail);
        let text = collect_logs(tail)
            .into_iter()
            .map(|line| format!("{} {}", line.time, line.text))
            .collect::<Vec<_>>()
            .join("\n");
        (
            StatusCode::OK,
            [("content-type", "text/plain; charset=utf-8")],
            text,
        )
            .into_response()
    }
}

pub use http::serve;

// ==================== 启动入口 ====================

/// 按启动参数决定要不要起调试口，并把结果写进日志。`run_bot` 里调一次。
pub async fn start(args: &[String]) {
    boot();

    // 启动参数优先（临时排障用）；没有参数就按 config/debug.yml 里存的那份走——
    // 否则 GUI 里打开开关、重启之后又变回关，用户会以为设置没保存。
    if let Some(config) = requested(args) {
        match serve(&config).await {
            Ok(addr) => {
                let token = match config.token.as_deref() {
                    Some(token) => format!("token={}", mask(token)),
                    None => "无 token（仅回环地址）".to_string(),
                };
                crate::runtime::log::info_green(format!(
                    "调试接口已启动: http://{addr}/debug/snapshot  （{token}）"
                ));
            }
            Err(err) => {
                crate::runtime::log::warning(format!("调试接口未启动: {err}"));
            }
        }
        return;
    }

    let settings = load_settings();
    if !settings.enabled {
        return;
    }
    match apply(&settings).await {
        Ok(message) => crate::runtime::log::info_green(format!("调试接口: {message}")),
        Err(err) => crate::runtime::log::warning(format!("调试接口未启动: {err}")),
    }
}

/// 日志里只露令牌的头尾，避免整串进日志文件
fn mask(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() <= 6 {
        return "***".to_string();
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 3..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 监听地址是进程级单例，两个用例同时起会互相 `stop()`。串起来跑。
    /// （同 `runtime::log` 里那个 `LIVE_LOG_TEST_LOCK` 的理由。）
    static HTTP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 等某个端口的连接真的被拒。
    ///
    /// 关停是**异步**的：`stop()` 只发一个信号，listener 在下一个调度周期才退出。
    /// 所以断言"立刻连不上"会偶发失败——这里轮询到 1 秒，验的是"最终真的关了"，
    /// 这恰好也是用户能感知的承诺（改端口后旧端口不该还能用）。
    async fn eventually_refuses(addr: std::net::SocketAddr) -> bool {
        // 必须带超时：端口还没关时连接会被接受，而 reqwest 默认无限等响应，
        // 一挂就是一轮 CI 超时
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(300))
            .build()
            .expect("建不了测试客户端");
        for _ in 0..40 {
            if client
                .get(format!("http://{addr}/debug/healthz"))
                .send()
                .await
                .is_err()
            {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        false
    }

    #[test]
    fn loopback_detection_covers_the_usual_spellings() {
        assert!(is_loopback("127.0.0.1:8787"));
        assert!(is_loopback("localhost:8787"));
        assert!(is_loopback("[::1]:8787"));
        assert!(!is_loopback("0.0.0.0:8787"));
        assert!(!is_loopback("192.168.1.10:8787"));
    }

    #[test]
    fn args_win_over_env_and_absent_flag_means_off() {
        // 没写 --debug-api：一律不开，哪怕别的参数给了
        assert!(requested(&["arona-rs.exe".to_string()]).is_none());

        let args = vec![
            "arona-rs.exe".to_string(),
            "--debug-api".to_string(),
            "--debug-bind=127.0.0.1:9999".to_string(),
            "--debug-token=abcdef123456".to_string(),
        ];
        let config = requested(&args).expect("给了 --debug-api 就该开");
        assert_eq!(config.bind, "127.0.0.1:9999");
        assert_eq!(config.token.as_deref(), Some("abcdef123456"));
    }

    #[test]
    fn token_is_masked_in_logs() {
        assert_eq!(mask("abcdef123456"), "abc…456");
        assert_eq!(mask("短"), "***");
    }

    /// 快照必须能序列化成 JSON，且顶层字段齐全——外部脚本按这些键取值
    #[test]
    fn snapshot_serializes_with_the_expected_keys() {
        let json = to_json(&snapshot());
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("快照不是合法 JSON");
        for key in [
            "schema",
            "taken_at",
            "process",
            "connection",
            "plugins",
            "commands",
            "hooks",
            "jobs",
            "views",
            "config",
            "outbox",
            "logs",
        ] {
            assert!(parsed.get(key).is_some(), "快照缺字段 {key}：{json}");
        }
        assert_eq!(parsed["schema"], SCHEMA);
        assert_eq!(
            parsed["process"]["abi_layout"],
            crate::plugin::abi::ABI_LAYOUT,
            "快照里的布局号必须与握手用的那个常量一致"
        );
    }

    #[test]
    fn log_tail_is_capped_not_truncated_from_the_front() {
        // 只留最后 N 行：调小 tail 不该把最新的那几行丢掉
        crate::runtime::log::push_live("调试口自测-1", false);
        crate::runtime::log::push_live("调试口自测-2", false);
        let tail = collect_logs(1);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].text, "调试口自测-2");
    }

    /// 端到端：真的起一个监听、真的发 HTTP。端口给 0 让系统挑，避免和本机别的东西撞。
    #[tokio::test]
    async fn http_serves_snapshot_and_enforces_the_token() {
        let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let config = ServeConfig {
            bind: "127.0.0.1:0".to_string(),
            token: Some("secret-token-123".to_string()),
            log_tail: 5,
            group: 0,
        };
        let addr = serve(&config).await.expect("调试口应当能在回环地址上起来");

        let bare = reqwest::get(format!("http://{addr}/debug/snapshot"))
            .await
            .expect("请求失败");
        assert_eq!(
            bare.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "没带 token 必须拒绝"
        );

        let with_token = reqwest::get(format!(
            "http://{addr}/debug/snapshot?token=secret-token-123&tail=3"
        ))
        .await
        .expect("请求失败");
        assert!(with_token.status().is_success());
        let body = with_token.text().await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("返回的不是 JSON");
        assert_eq!(parsed["schema"], SCHEMA);
        assert!(
            parsed["logs"]
                .as_array()
                .is_some_and(|logs| logs.len() <= 3),
            "tail 参数没生效：{body}"
        );

        let health = reqwest::get(format!("http://{addr}/debug/healthz"))
            .await
            .expect("请求失败");
        assert_eq!(health.text().await.unwrap(), "ok");
    }

    /// 绑到非回环地址却不给 token：宁可不启动，也不把管理面板的钥匙丢在门口
    #[tokio::test]
    async fn refuses_to_listen_off_loopback_without_a_token() {
        let config = ServeConfig {
            bind: "0.0.0.0:0".to_string(),
            token: None,
            log_tail: 5,
            group: 0,
        };
        let err = serve(&config)
            .await
            .expect_err("没有 token 就不该绑到公网地址");
        assert!(err.contains("token"), "{err}");
    }

    /// 设置落盘再读回来必须一模一样：GUI 点完「确认生效」重启后看到的值不该变
    #[test]
    fn settings_roundtrip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("arona-debug-settings-{}", std::process::id()));
        let path = dir.join("debug.yml");
        let settings = Settings {
            enabled: true,
            port: 8899,
            lan: true,
            group: 123456,
            token: "abcdef123456".to_string(),
        };
        save_settings_to(&path, &settings).expect("写设置失败");
        assert_eq!(load_settings_from(&path), settings);

        // 监听地址跟着 lan 走：true 才是 0.0.0.0
        assert_eq!(settings.bind(), "0.0.0.0:8899");
        assert_eq!(
            Settings {
                lan: false,
                ..settings.clone()
            }
            .bind(),
            "127.0.0.1:8899"
        );

        // 老文件没有 lan 这一行：必须读成「只绑回环」，不能默认对外
        std::fs::write(&path, "enabled: true\nport: 8899\ngroup: 0\ntoken: \"x\"\n").unwrap();
        assert!(!load_settings_from(&path).lan, "旧配置不该被读成对外开放");

        // 文件不存在 → 默认（且默认必须是「关」）
        let missing = load_settings_from(&dir.join("nope.yml"));
        assert!(!missing.enabled, "读不到就按关处理，不能默认开");

        // 文件写坏了 → 同样退回关，不能因为 YAML 坏了就把端点开出来
        std::fs::write(&path, "enabled: [这不是布尔]").unwrap();
        assert!(!load_settings_from(&path).enabled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 启停要真的换端口：旧 listener 必须退出，否则用户会以为"改了没生效"
    #[tokio::test]
    async fn serve_moves_to_a_new_port_and_stop_closes_it() {
        let _guard = HTTP_TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let first = ServeConfig {
            bind: "127.0.0.1:0".to_string(),
            token: Some("t0ken-aaaa".to_string()),
            log_tail: 5,
            group: 0,
        };
        let first_addr = serve(&first).await.expect("起第一个");
        assert_eq!(listening_at(), Some(first_addr));

        let second = ServeConfig {
            bind: "127.0.0.1:0".to_string(),
            token: Some("t0ken-bbbb".to_string()),
            log_tail: 5,
            group: 0,
        };
        let second_addr = serve(&second).await.expect("起第二个");
        assert_ne!(first_addr, second_addr);
        assert_eq!(listening_at(), Some(second_addr));
        assert!(
            eventually_refuses(first_addr).await,
            "旧端口还能连，说明旧的 listener 没关掉"
        );

        stop();
        assert!(listening_at().is_none());
        assert!(eventually_refuses(second_addr).await, "停掉之后不该还能连");
    }

    /// 自检消息的固定前缀是安全边界：令牌泄露也只能发这种带标记的消息
    #[test]
    fn test_message_keeps_its_marker_and_truncates() {
        let plain = test_message("");
        assert!(plain.starts_with("【Arona 调试】"), "{plain}");

        let custom = test_message("  看这里  ");
        assert!(custom.starts_with("【Arona 调试】"));
        assert!(custom.ends_with("看这里"), "{custom}");

        let long = test_message(&"啊".repeat(1000));
        assert!(long.chars().count() < 400, "超长正文没截断");
    }

    /// 动作记录要能被快照读到（"主人说没收到"时靠它判断是没发还是发失败）
    #[test]
    fn outbox_records_calls_in_order() {
        clear_calls();
        record_call(
            "send_group_msg",
            false,
            "实现端拒绝",
            std::time::Duration::from_millis(42),
        );
        record_call(
            "send_group_msg",
            true,
            "",
            std::time::Duration::from_millis(7),
        );
        let calls = recent_calls(10);
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(!calls[0].ok);
        assert_eq!(calls[0].millis, 42);
        assert_eq!(calls[0].detail, "实现端拒绝");
        assert!(calls[1].ok);
        // 只要最后一条时也按"最旧在前"给
        let last = recent_calls(1);
        assert_eq!(last.len(), 1);
        assert!(last[0].ok);
    }
}
