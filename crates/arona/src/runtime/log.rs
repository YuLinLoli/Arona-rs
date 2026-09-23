//! 运行期日志（对应原版 RuntimeLog + StandaloneLogFile）
//! 独立模式输出到 stdout，并附带写入 logs/arona-yyyy-MM-dd.log（按天滚动）。

use once_cell::sync::OnceCell;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

struct FileLog {
    file: PathBuf,
    date: String,
}

static FILE_LOG: OnceCell<Mutex<FileLog>> = OnceCell::new();

fn ensure_file_log() -> Option<&'static Mutex<FileLog>> {
    FILE_LOG
        .get_or_init(|| {
            let dir = crate::runtime::paths::logs_dir();
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let file = dir.join(format!("arona-{today}.log"));
            Mutex::new(FileLog { file, date: today })
        })
        .into()
}

fn append_file(line: &str) {
    let Some(guard) = ensure_file_log() else {
        return;
    };
    let Ok(mut file_log) = guard.lock() else {
        return;
    };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    if today != file_log.date {
        file_log.date = today.clone();
        file_log.file = crate::runtime::paths::logs_dir().join(format!("arona-{today}.log"));
    }
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let line = strip_ansi(line);
    if line.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(file_log.file.parent().unwrap_or(std::path::Path::new(".")));
    if let Ok(mut writer) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_log.file)
    {
        let _ = writeln!(writer, "{timestamp} {line}");
    }
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // 跳过 CSI 序列直到字母
            while let Some(n) = chars.next() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 绿色提示日志（对应原版 RuntimeLog.infoGreen）。
/// `[Arona]` 开头的行会由控制台染色规则统一渲染为亮绿（见 runtime::console），此处无需额外处理。
pub fn info_green(message: impl Into<String>) {
    info(message);
}

pub fn info(message: impl Into<String>) {
    let message = message.into();
    log("INFO", &message);
}

/// 诊断级日志（插件恢复、配置回写失败这类不值得惊动运维的细节）
pub fn debug(message: impl Into<String>) {
    let message = message.into();
    log("DEBUG", &message);
}

pub fn warning(message: impl Into<String>) {
    let message = message.into();
    log("WARNING", &message);
}

pub fn error(message: impl Into<String>) {
    let message = message.into();
    log("ERROR", &message);
}

// 日志来源：`[Arona]` 是框架自己打的，插件代码里打的要挂插件名。
// 用线程局部而不是给每个日志函数加参数——插件里几十处调用点不该为此改动。
thread_local! {
    static SOURCE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// 在 `f` 执行期间把日志来源换成 `source`（`插件名` 或 `插件名:动作`），结束后还原。
///
/// 只在同步段可靠：`f` 里 await 之后被换到别的线程续跑时来源会退回 `[Arona]`，
/// 所以包裹点放在框架进入插件代码的那一次同步调用上（生命周期回调、命令、钩子、定时任务），
/// 要覆盖一段异步活儿就用 `with_action_async` / `PluginScope::spawn_as` 的外壳。
#[must_use = "来源只在闭包执行期间有效，别把结果丢掉"]
pub fn with_source<R>(source: &str, f: impl FnOnce() -> R) -> R {
    let previous = SOURCE.with(|slot| slot.borrow_mut().replace(source.to_string()));
    let outcome = f();
    SOURCE.with(|slot| *slot.borrow_mut() = previous);
    outcome
}

/// 给 future 套一层"每次轮询都设好日志来源"的外壳。
///
/// 来源是线程局部的，而任务会在 runtime 的各工作线程之间搬动，起头设一次会在第一次
/// await 之后丢掉——所以必须在 poll 里重设。`source` 为 None 时原样轮询。
pub(crate) struct Sourced<F> {
    source: Option<String>,
    inner: Pin<Box<F>>,
}

impl<F: Future> Sourced<F> {
    pub(crate) fn new(source: Option<String>, future: F) -> Sourced<F> {
        Sourced {
            source,
            inner: Box::pin(future),
        }
    }
}

impl<F: Future> Future for Sourced<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match &this.source {
            Some(source) => with_source(source, || this.inner.as_mut().poll(context)),
            None => this.inner.as_mut().poll(context),
        }
    }
}

/// 当前来源是已登记的插件时，把它细化成 `[插件名:动作]`（已有的动作段被换掉，只留插件名）。
/// 返回 None 表示别动来源：框架自己的日志一直是 `[Arona]`，不挂动作。
fn refined_source(action: &str) -> Option<String> {
    let current = SOURCE
        .with(|slot| slot.borrow().clone())
        .unwrap_or_default();
    // 冒号前才是插件名：框架给的粗动作（`装配`、`命令 活动`）在这一层被换掉
    let plugin = current.split(':').next().unwrap_or_default();
    crate::plugin::is_plugin_name(plugin).then(|| format!("{plugin}:{action}"))
}

/// 把当前来源细化成 `[插件名:动作]`（如 `[HelloPlugin:定时推送]`），只在 `f` 期间有效。
///
/// 框架按入口给的是粗动作（`装配` / `命令 活动` / `事件 群消息` / `定时 ChatLogPurge`），
/// 插件比框架清楚自己那一步在干什么，包一层就精确到「发送消息」「踢人」这个粒度；
/// 已经带过动作的来源会被替换掉，只留插件名。
///
/// 当前来源不是已登记的插件时原样执行：框架自己的日志一直是 `[Arona]`，不挂动作。
/// 被包住的活儿要跨 `await` 时用 [`with_action_async`]。
#[must_use = "来源只在闭包执行期间有效，别把结果丢掉"]
pub fn with_action<R>(action: &str, f: impl FnOnce() -> R) -> R {
    match refined_source(action) {
        Some(source) => with_source(&source, f),
        None => f(),
    }
}

/// [`with_action`] 的异步版：动作名覆盖整个 future 的每次轮询，中间的 await 不会丢掉它。
///
/// `plugin::action_async("发送消息", services::send_message(target, msg)).await`
#[must_use = "来源只在返回的 future 运行期间有效，别把它丢掉"]
pub fn with_action_async<F: Future>(action: &str, future: F) -> impl Future<Output = F::Output> {
    Sourced::new(refined_source(action), future)
}

/// 当前线程显式设置的日志来源；没设置过时是 `None`（区别于回落到 `[Arona]`）
fn explicit_source() -> Option<String> {
    SOURCE.with(|slot| slot.borrow().clone())
}

fn source() -> String {
    explicit_source().unwrap_or_else(|| "Arona".to_string())
}

fn log(level: &str, message: &str) {
    // 动态插件 dll 里静态链接了另一份本模块，日志文件与 GUI 缓冲都只在宿主那侧存在：
    // 接管过就必须把这条交回宿主落地，否则插件日志等于没打。
    let bridge = HOST.get().copied();
    if let Some(bridge) = bridge.filter(|_| !RELAYING.with(|flag| flag.get())) {
        let _guard = RelayGuard::enter();
        emit_foreign(&bridge, level, message);
        return;
    }
    let line = format!("[{}] {message}", source());
    write_line(level, &line);
}

/// dll 侧：把一条日志拆成 C 字符串交给宿主。来源也带上，宿主没标注时用它兜底。
fn emit_foreign(bridge: &HostBridge, level: &str, message: &str) {
    let (Ok(level), Ok(message)) = (CString::new(level), CString::new(message)) else {
        return;
    };
    let source = explicit_source().and_then(|text| CString::new(text).ok());
    (bridge.log)(
        level.as_ptr(),
        message.as_ptr(),
        source
            .as_ref()
            .map_or(std::ptr::null(), |text| text.as_ptr()),
    );
}

/// 落地一行已经组好来源的日志（宿主自己走这条，插件转发行由 [`relay`] 走这条）
fn write_line(level: &str, line: &str) {
    // 控制台按原版 ColoredPrintStream 规则染色（[Arona]/[OneBot] 亮绿, WARNING/SLF4J 亮黄）
    if level == "ERROR" {
        crate::runtime::console::eprint_rule_line(line);
    } else {
        crate::runtime::console::print_rule_line(line);
    }
    // 日志文件写入前会剥离 ANSI, 保持纯文本
    append_file(line);
}

// ==================== 动态插件的上行出口 ====================

/// 宿主交给插件的那份通道（定义在契约层，这里只是缩写）
type HostBridge = crate::plugin::abi::HostBridge;

/// 已接管时指向宿主的通道；插件 dll 里这份由 [`attach_host`] 写入，宿主自己那份正常恒空
/// （`plugin::abi` 的自检用例会刻意让宿主接管一次，那正是为了验这条转发链）
static HOST: OnceCell<HostBridge> = OnceCell::new();

thread_local! {
    /// 正在往宿主转发：宿主自己的 `log()` 不能再抛回去，否则无限递归
    static RELAYING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 转发中标记的作用域守卫：进入时置位，退出时还原前值（宿主侧落地可能嵌套在 dll 的转发里）
struct RelayGuard(bool);

impl RelayGuard {
    fn enter() -> Self {
        Self(RELAYING.with(|flag| flag.replace(true)))
    }
}

impl Drop for RelayGuard {
    fn drop(&mut self) {
        RELAYING.with(|flag| flag.set(self.0));
    }
}

/// dll 侧：把自己打的日志改交宿主落地（由 `plugin::abi::attach_host` 调用）
pub(crate) fn attach_host(bridge: HostBridge) {
    let _ = HOST.set(bridge);
}

/// 宿主侧：接住插件转来的一条日志。
///
/// 来源以宿主这一侧为准——进入插件代码的每个入口（生命周期回调、命令、事件钩子、定时任务）
/// 都由框架在宿主侧标好了；只有宿主没设置来源时，才用 dll 带上来的那个。
/// 控制台出口会顺手把这行推进 GUI 的实时缓冲（见 `runtime::console`），这里不再单独 `push_live`。
extern "C" fn relay(
    level: *const std::os::raw::c_char,
    message: *const std::os::raw::c_char,
    source: *const std::os::raw::c_char,
) {
    let text = |ptr: *const std::os::raw::c_char| {
        if ptr.is_null() {
            return String::new();
        }
        // SAFETY: 按约定，指针指向 NUL 结尾的合法 UTF-8，且在本次调用期间有效
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    };
    let message = text(message);
    if message.is_empty() {
        return;
    }
    let level = if level.is_null() {
        "INFO".to_string()
    } else {
        text(level)
    };
    let fallback = if source.is_null() {
        None
    } else {
        Some(text(source))
    };
    let line = format!(
        "[{}] {message}",
        explicit_source()
            .or(fallback)
            .unwrap_or_else(|| "Arona".to_string())
    );
    let _guard = RelayGuard::enter();
    write_line(&level, &line);
}

/// 宿主侧：进程默认实例指针，交给插件 dll 认领
extern "C" fn host_framework() -> *const std::ffi::c_void {
    std::ptr::from_ref(crate::framework::Framework::global()).cast()
}

/// 宿主侧：交给插件 dll 的上行通道，进程内一份、指针常驻
pub(crate) fn host_bridge() -> &'static HostBridge {
    static BRIDGE: OnceCell<HostBridge> = OnceCell::new();
    BRIDGE.get_or_init(|| HostBridge {
        log: relay,
        framework: host_framework,
        runtime: crate::runtime::reactor::host_handle,
    })
}

// ==================== 实时日志缓冲（GUI 日志选项卡用） ====================

/// 实时日志的一行
#[derive(Clone, Debug)]
pub struct LiveLine {
    /// 本机时间 HH:MM:SS
    pub time: String,
    /// 行内容（不含 ANSI 转义码）
    pub text: String,
    /// 是否来自 stderr
    pub stderr: bool,
}

/// 缓冲上限（超出后丢弃最旧的行）
const LIVE_CAPACITY: usize = 3000;

struct LiveLog {
    lines: std::collections::VecDeque<LiveLine>,
    version: u64,
}

static LIVE_LOG: OnceCell<Mutex<LiveLog>> = OnceCell::new();
static LIVE_VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn live_log() -> &'static Mutex<LiveLog> {
    LIVE_LOG.get_or_init(|| {
        Mutex::new(LiveLog {
            lines: std::collections::VecDeque::new(),
            version: 0,
        })
    })
}

/// 记录一行到内存缓冲（由 `runtime::console` 统一调用，覆盖所有控制台输出）
pub fn push_live(text: &str, stderr: bool) {
    if text.is_empty() {
        return;
    }
    let Ok(mut log) = live_log().lock() else {
        return;
    };
    while log.lines.len() >= LIVE_CAPACITY {
        log.lines.pop_front();
    }
    log.lines.push_back(LiveLine {
        time: chrono::Local::now().format("%H:%M:%S").to_string(),
        text: text.to_string(),
        stderr,
    });
    log.version += 1;
    LIVE_VERSION.store(log.version, std::sync::atomic::Ordering::Relaxed);
}

/// 当前缓冲版本号；GUI 用它判断是否需要重新取快照
pub fn live_version() -> u64 {
    LIVE_VERSION.load(std::sync::atomic::Ordering::Relaxed)
}

/// 仅测试用：实时日志缓冲是**全局**状态，会读写它的测试必须持有这把锁串行执行。
/// 否则 `clear_live()` 或缓冲上限淘汰（`live_log_buffer_is_bounded` 一次推 3000+ 行）
/// 会在别的测试（如 business 的「收到消息先打印」时序回归）读取前，把它刚写入的标记行抹掉，
/// 导致并行执行时随机失败。用 `.unwrap_or_else(|p| p.into_inner())` 容忍别的测试 panic 后的中毒锁。
#[cfg(test)]
pub(crate) static LIVE_LOG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取全部实时日志快照（最多 LIVE_CAPACITY 行）
pub fn live_lines() -> Vec<LiveLine> {
    match live_log().lock() {
        Ok(log) => log.lines.iter().cloned().collect(),
        Err(_) => Vec::new(),
    }
}

/// 清空实时日志缓冲
pub fn clear_live() {
    let Ok(mut log) = live_log().lock() else {
        return;
    };
    log.lines.clear();
    log.version += 1;
    LIVE_VERSION.store(log.version, std::sync::atomic::Ordering::Relaxed);
}

// ==================== 三方库日志桥（log 门面） ====================

/// 把 Rust 生态的 `log` 门面（wgpu / glutin / winit / naga ...）接到本项目的
/// 控制台 + 按天日志文件 + GUI 实时日志上。
///
/// 不装这个的话，像「DX12 后端创建失败」这种关键诊断会被直接丢掉：wgpu-core 只在
/// `debug` 级别记录 `Instance::new: failed to create Dx12 backend: ...`，表现出来
/// 就是「没有可用的 wgpu 适配器」却完全不知道原因。
pub fn install_logger() {
    let level = std::env::var("ARONA_LOG")
        .ok()
        .and_then(|value| value.trim().parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Debug);
    log::set_max_level(level);
    // 已经装过（例如测试里重复调用）时忽略错误即可
    let _ = log::set_logger(&ARONA_LOGGER);
}

static ARONA_LOGGER: AronaLogger = AronaLogger;

struct AronaLogger;

/// 每个日志目标允许的最高级别。
///
/// `wgpu` 的「实例/适配器」阶段 debug 里带着后端创建失败与适配器枚举结果（例如
/// `Instance::new: failed to create Dx12 backend: ...`），是排障的关键，必须放行；
/// 「设备」阶段（`wgpu_core::device`、`wgpu_hal::*::device`）的 debug 则是着色器编译
/// 细节，压到 info。
/// 注意：所有致命错误都是 warn/error 级别，永远会输出，不受此规则影响。
fn allowed_level(target: &str) -> log::Level {
    if target.starts_with("wgpu") && !target.contains("device") {
        log::Level::Debug
    } else {
        log::Level::Info
    }
}

/// wgpu-hal 在着色器编译「成功」时也用 `Info` 级别把整段生成的着色器源码打出来
/// （`wgpu_hal::dx12::device`、`wgpu_hal::gles::device`），对使用者是纯噪声；
/// 编译失败走的是 `Error`，不会命中这条规则，仍会照常输出。
fn is_noisy_shader_dump(level: log::Level, target: &str, message: &str) -> bool {
    level == log::Level::Info
        && target.contains("device")
        && message.starts_with("Naga generated shader")
}

impl log::Log for AronaLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= allowed_level(metadata.target())
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if record.level() == log::Level::Info
            && is_noisy_shader_dump(record.level(), record.target(), &record.args().to_string())
        {
            return;
        }
        let target = record.target();
        let message = record.args();
        // WARNING/ERROR 用原版配色规则（亮黄）渲染，其余保持原样
        let line = match record.level() {
            log::Level::Error => format!("[{target}] ERROR {message}"),
            log::Level::Warn => format!("[{target}] WARNING {message}"),
            _ => format!("[{target}] {message}"),
        };
        if record.level() == log::Level::Error {
            crate::runtime::console::eprint_rule_line(&line);
        } else {
            crate::runtime::console::print_rule_line(&line);
        }
        append_file(&line);
    }

    fn flush(&self) {}
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logger_policy_allows_wgpu_debug_only() {
        use log::Log as _;
        let wgpu_debug = log::Metadata::builder()
            .level(log::Level::Debug)
            .target("wgpu_core::instance")
            .build();
        let wgpu_device_debug = log::Metadata::builder()
            .level(log::Level::Debug)
            .target("wgpu_core::device::global")
            .build();
        let wgpu_hal_device_debug = log::Metadata::builder()
            .level(log::Level::Debug)
            .target("wgpu_hal::dx12::device")
            .build();
        let other_debug = log::Metadata::builder()
            .level(log::Level::Debug)
            .target("some_other_crate")
            .build();
        let other_info = log::Metadata::builder()
            .level(log::Level::Info)
            .target("some_other_crate")
            .build();
        assert!(AronaLogger.enabled(&wgpu_debug));
        assert!(!AronaLogger.enabled(&wgpu_device_debug));
        assert!(!AronaLogger.enabled(&wgpu_hal_device_debug));
        assert!(!AronaLogger.enabled(&other_debug));
        assert!(AronaLogger.enabled(&other_info));
    }

    #[test]
    fn noisy_shader_dump_is_recognized() {
        // wgpu-hal 编译成功时的 Info 级源码转储 -> 过滤
        assert!(is_noisy_shader_dump(
            log::Level::Info,
            "wgpu_hal::dx12::device",
            "Naga generated shader for \"main\" at Compute:\nstruct ..."
        ));
        // 着色器编译失败用的是同一句消息，但级别是 Error -> 必须放行
        assert!(!is_noisy_shader_dump(
            log::Level::Error,
            "wgpu_hal::dx12::device",
            "Naga generated shader for \"main\" at Compute:\nerror ..."
        ));
        // 其它目标/其它消息不误伤
        assert!(!is_noisy_shader_dump(
            log::Level::Info,
            "wgpu_core::instance",
            "Naga generated shader for something"
        ));
        assert!(!is_noisy_shader_dump(
            log::Level::Info,
            "wgpu_hal::dx12::device",
            "\tCompiled shader"
        ));
    }

    #[test]
    fn source_switches_per_plugin_and_restores_on_exit() {
        // 嵌套进出插件代码时来源不能串味：出来还得是 [Arona]
        assert_eq!(source(), "Arona");
        assert_eq!(with_source("HelloPlugin", source), "HelloPlugin");
        assert_eq!(source(), "Arona");
    }

    #[test]
    fn action_leaves_framework_source_alone() {
        // 没有插件在名下（测试里没有任何已登记插件）时动作不生效：框架日志始终是 [Arona]
        assert_eq!(with_action("发送消息", source), "Arona");
        assert_eq!(refined_source("发送消息"), None);
        // 插件给的粗动作也一样只看冒号前的插件名，未登记就不挂动作
        assert_eq!(
            with_source("HelloPlugin:装配", || with_action("发送消息", || {
                source()
            })),
            "HelloPlugin:装配"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sourced_future_keeps_action_across_awaits() {
        // 线程局部的来源撑不过挂起，外壳必须每次轮询重设，否则 await 之后又退回 [Arona]
        let seen = tokio::task::spawn(Sourced::new(
            Some("HelloPlugin:发送消息".to_string()),
            async {
                tokio::task::yield_now().await;
                source()
            },
        ))
        .await
        .unwrap();
        assert_eq!(seen, "HelloPlugin:发送消息");
    }

    #[test]
    fn live_log_buffer_records_and_clears() {
        // 缓冲是全局状态：持锁串行，避免与同样读写缓冲的其它测试相互抹除
        let _serial = LIVE_LOG_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let marker = "[Arona] live-log-test-marker";
        push_live(marker, false);
        assert!(live_lines().iter().any(|line| line.text == marker));
        assert!(live_version() > 0);
        clear_live();
        assert!(!live_lines().iter().any(|line| line.text == marker));
    }

    /// 宿主替插件落地日志：来源以宿主这一侧为准，宿主没标注时才用 dll 带上来的那个。
    /// 搞反的话插件日志会全部顶着 `[Arona]`，或把框架自己标的动作名（`命令 活动`）抹掉。
    #[test]
    fn relayed_plugin_line_keeps_the_host_source() {
        let _serial = LIVE_LOG_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_live();
        let level = CString::new("INFO").unwrap();
        let message = CString::new("来自 dll 的一行").unwrap();
        let dll_source = CString::new("HelloPlugin:自检").unwrap();

        // 宿主自己标了来源 -> 用宿主的；转发期间宿主标注的动作名不能被 dll 带上来那个盖掉
        let marked = with_source("HelloPlugin:命令 自检", || {
            relay(level.as_ptr(), message.as_ptr(), dll_source.as_ptr());
            source()
        });
        assert_eq!(marked, "HelloPlugin:命令 自检");
        // 宿主没标（插件自己的后台线程）-> 用 dll 带上来的
        relay(level.as_ptr(), message.as_ptr(), dll_source.as_ptr());
        // 两边都没有 -> 回落到 [Arona]
        relay(level.as_ptr(), message.as_ptr(), std::ptr::null());
        // 正文为空的不该留下一行
        let empty = CString::new("").unwrap();
        relay(level.as_ptr(), empty.as_ptr(), std::ptr::null());

        let lines = live_lines();
        // 只认本用例的行：缓冲是共享的，别的测试随时会往里推
        let texts = lines
            .iter()
            .filter(|line| line.text.contains("来自 dll 的一行"))
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "[HelloPlugin:命令 自检] 来自 dll 的一行",
                "[HelloPlugin:自检] 来自 dll 的一行",
                "[Arona] 来自 dll 的一行",
            ],
            "转发行落地结果不对: {texts:?}"
        );
        clear_live();
    }

    /// `RelayGuard` 必须真的把标记立起来：漏了它，宿主在转发途中打的日志会再抛给宿主自己，
    /// 变成无限递归的栈溢出。嵌套时退出内层不能提前把外层的标记清掉。
    #[test]
    fn relay_guard_marks_its_scope() {
        let off = || RELAYING.with(|flag| flag.get());
        assert!(!off());
        {
            let _outer = RelayGuard::enter();
            assert!(off());
            {
                let inner = RelayGuard::enter();
                drop(inner);
                assert!(off(), "退出内层不该把外层的标记一起清掉");
            }
        }
        assert!(!off());
    }

    #[test]
    fn live_log_buffer_is_bounded() {
        // 一次推满缓冲并触发淘汰：必须与读缓冲的其它测试互斥，否则会抹掉它们的行
        let _serial = LIVE_LOG_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for i in 0..(LIVE_CAPACITY + 50) {
            push_live(&format!("[Arona] bounded-{i}"), false);
        }
        assert!(live_lines().len() <= LIVE_CAPACITY);
    }
}
