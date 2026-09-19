//! 运行期日志（对应原版 RuntimeLog + StandaloneLogFile）
//! 独立模式输出到 stdout，并附带写入 logs/arona-yyyy-MM-dd.log（按天滚动）。

use once_cell::sync::OnceCell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

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

pub fn warning(message: impl Into<String>) {
    let message = message.into();
    log("WARNING", &message);
}

pub fn error(message: impl Into<String>) {
    let message = message.into();
    log("ERROR", &message);
}

fn log(level: &str, message: &str) {
    let line = format!("[Arona] {message}");
    // 控制台按原版 ColoredPrintStream 规则染色（[Arona]/[OneBot] 亮绿, WARNING/SLF4J 亮黄）
    if level == "ERROR" {
        crate::runtime::console::eprint_rule_line(&line);
    } else {
        crate::runtime::console::print_rule_line(&line);
    }
    // 日志文件写入前会剥离 ANSI, 保持纯文本
    append_file(&line);
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
