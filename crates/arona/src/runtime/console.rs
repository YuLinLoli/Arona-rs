//! 控制台颜色支持（对应原版 AronaStandalone.ColoredPrintStream + RuntimeLog 的 ANSI 常量）。
//!
//! 原版独立模式对标准输出/错误流做统一染色，判定顺序与优先级完全一致：
//! 1. 以 `[Arona` / `[OneBot` 开头的行                     -> 亮绿（优先，即使含 WARNING）
//! 2. 前缀是某个插件的显示名（`[BluearchivePlugin] …`）    -> 淡紫
//! 3. 含 `WARNING:` / `WARNING：` / `SLF4J`，
//!    或含被空白包裹的 `INFO|DEBUG|WARN|ERROR|TRACE`       -> 亮黄
//! 4. 其它行                                              -> 原样
//!
//! 与原版「无条件输出 ANSI 转义码」不同：Windows 控制台默认改用原生 API
//! `SetConsoleTextAttribute`，因此 cmd.exe(经典 conhost)、PowerShell、Windows Terminal
//! 都能正常上色，不依赖 VT(VirtualTerminal) 支持；只有在非控制台输出（IDE 管道等）
//! 或非 Windows 平台时才回退为 ANSI 转义码。
//!
//! 开关：`ARONA_CONSOLE_COLOR=1/0` 强制开/关，尊重 `NO_COLOR`，
//! 并识别 `FORCE_COLOR`/`CLICOLOR_FORCE`/`TERM`（IDE、CI 常用约定）。
//! 日志文件落盘前会剥离 ANSI，保持纯文本（见 `runtime::log::strip_ansi`）。

use once_cell::sync::OnceCell;

/// 控制台颜色（与原版 ANSI 色一致：亮绿 92 / 亮黄 93 / 普通黄 33；插件日志另用淡紫 95）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    BrightGreen,
    BrightYellow,
    Yellow,
    /// 插件打的日志（`[插件名] …`）：和框架的亮绿一眼分得开
    BrightMagenta,
}

impl Color {
    /// ANSI 转义码（非 Windows，或 Windows 非控制台输出时使用）
    pub fn ansi(self) -> &'static str {
        match self {
            Color::BrightGreen => "\u{1b}[92m",
            Color::BrightYellow => "\u{1b}[93m",
            Color::Yellow => "\u{1b}[33m",
            Color::BrightMagenta => "\u{1b}[95m",
        }
    }

    /// Windows 控制台属性（SetConsoleTextAttribute）
    #[cfg(windows)]
    fn win_attribute(self) -> u16 {
        const BLUE: u16 = 0x0001;
        const GREEN: u16 = 0x0002;
        const RED: u16 = 0x0004;
        const INTENSITY: u16 = 0x0008;
        match self {
            Color::BrightGreen => GREEN | INTENSITY,
            Color::BrightYellow => RED | GREEN | INTENSITY,
            Color::Yellow => RED | GREEN,
            Color::BrightMagenta => RED | BLUE | INTENSITY,
        }
    }
}

/// ANSI 重置
pub const RESET: &str = "\u{1b}[0m";

/// 单个流的染色方式
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Windows 原生控制台属性（cmd/PowerShell/Windows Terminal 通用）
    Native,
    /// ANSI 转义码
    Ansi,
    /// 纯文本
    Plain,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Out,
    Err,
}

static STDOUT_MODE: OnceCell<Mode> = OnceCell::new();
static STDERR_MODE: OnceCell<Mode> = OnceCell::new();

/// 启动时初始化：提前确定 stdout/stderr 的染色方式（需在首次输出前调用）
pub fn init() {
    let _ = stdout_mode();
    let _ = stderr_mode();
}

fn stdout_mode() -> Mode {
    *STDOUT_MODE.get_or_init(|| resolve(Stream::Out))
}

fn stderr_mode() -> Mode {
    *STDERR_MODE.get_or_init(|| resolve(Stream::Err))
}

/// 让 GUI 子系统(`windows_subsystem = "windows"`)的进程也能在命令行模式输出：
/// 优先附加到上级终端(cmd/PowerShell/cargo run)，失败则新建控制台，然后把
/// 标准输入/输出/错误接到 CONIN$/CONOUT$。若标准流本就有效(IDE 管道等)则保持不动。
///
/// 必须在任何 stdout/stderr 输出之前调用（`--nogui` 启动时由 main 调用）。
#[cfg(windows)]
pub fn attach_console() {
    win::attach_console();
}

/// 非 Windows 平台无需处理
#[cfg(not(windows))]
pub fn attach_console() {}

/// 当前染色方式摘要（`ARONA_CONSOLE_DEBUG=1` 时打印，便于排查终端差异）
pub fn mode_summary() -> String {
    fn name(mode: Mode) -> &'static str {
        match mode {
            Mode::Native => "native",
            Mode::Ansi => "ansi",
            Mode::Plain => "plain",
        }
    }
    format!(
        "stdout={} stderr={}",
        name(stdout_mode()),
        name(stderr_mode())
    )
}

fn resolve(stream: Stream) -> Mode {
    // 1. 显式开关优先
    if let Ok(value) = std::env::var("ARONA_CONSOLE_COLOR") {
        match value.trim().to_lowercase().as_str() {
            "0" | "false" | "off" | "no" => return Mode::Plain,
            "1" | "true" | "on" | "yes" => {
                return if console_attached(stream) {
                    Mode::Native
                } else {
                    Mode::Ansi
                };
            }
            _ => {}
        }
    }
    // 2. 业界约定：NO_COLOR 关闭
    if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        return Mode::Plain;
    }
    let forced = force_color_env();
    #[cfg(windows)]
    {
        let _ = forced;
        // 默认直接走原生属性：是控制台就上色(cmd/PowerShell/Windows Terminal 通用,
        // 不依赖 VT)；不是控制台(文件/管道)时设置属性会失败, 自动输出纯文本,
        // 因此不会产生裸转义码, 无需额外探测。
        Mode::Native
    }
    #[cfg(not(windows))]
    {
        use std::io::IsTerminal;
        let tty = match stream {
            Stream::Out => std::io::stdout().is_terminal(),
            Stream::Err => std::io::stderr().is_terminal(),
        };
        if forced || tty {
            Mode::Ansi
        } else {
            Mode::Plain
        }
    }
}

/// 识别常见的强制彩色环境变量
fn force_color_env() -> bool {
    for key in ["FORCE_COLOR", "CLICOLOR_FORCE"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().to_lowercase();
            if !value.is_empty() && value != "0" && value != "false" {
                return true;
            }
        }
    }
    matches!(
        std::env::var("TERM").as_deref(),
        Ok("xterm") | Ok("xterm-256color") | Ok("screen") | Ok("screen-256color") | Ok("vt100")
    )
}

/// 判定一行文本对应的颜色（复刻原版 `ColoredPrintStream.colorize` 的判定顺序，另加插件淡紫一档）
pub fn color_for_line(line: &str) -> Option<Color> {
    if line.is_empty() {
        return None;
    }
    // [Arona] / [OneBot] 统一亮绿，且优先级高于 WARNING 判定
    if line.starts_with("[Arona") || line.starts_with("[OneBot") {
        return Some(Color::BrightGreen);
    }
    // [BluearchivePlugin] 一类：插件自己的日志淡紫，跟框架的亮绿区分开
    if source_of(line).is_some_and(crate::plugin::is_plugin_name) {
        return Some(Color::BrightMagenta);
    }
    if is_warning_line(line) {
        return Some(Color::BrightYellow);
    }
    None
}

/// 取出行首 `[…]` 里的来源名（`[Arona] xxx` -> `Some("Arona")`），没有前缀时 None。
/// 只取第一个冒号之前：插件日志细化成 `[插件名:动作]` 后仍要认得插件名。
fn source_of(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let source = rest.split_once(']')?.0;
    Some(source.split(':').next().unwrap_or(source))
}

/// WARNING:xxx（中英文冒号）/ SLF4J / SLF4J 级别标记（原版正则 `\s(INFO|DEBUG|WARN|ERROR|TRACE)\s`）
fn is_warning_line(line: &str) -> bool {
    if line.contains("WARNING:") || line.contains("WARNING：") || line.contains("SLF4J") {
        return true;
    }
    const LEVELS: [&str; 5] = ["INFO", "DEBUG", "WARN", "ERROR", "TRACE"];
    LEVELS.iter().any(|level| {
        line.match_indices(level).any(|(index, _)| {
            let before = line[..index].chars().next_back();
            let after = line[index + level.len()..].chars().next();
            matches!(before, Some(ch) if ch.is_whitespace())
                && matches!(after, Some(ch) if ch.is_whitespace())
        })
    })
}

/// 安全输出一行到 stdout：GUI 子系统没有控制台时写句柄可能无效，
/// 这里忽略写入错误，绝不让“记日志”本身触发 panic（println! 写失败会 panic）。
pub fn print_safe(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// 安全输出一行到 stderr（同上）
pub fn eprint_safe(line: &str) {
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{line}");
    let _ = err.flush();
}

/// 按原版规则给一行上色后输出到 stdout
pub fn print_rule_line(line: &str) {
    write_line(stdout_mode(), Stream::Out, color_for_line(line), line);
}

/// 按原版规则给一行上色后输出到 stderr
pub fn eprint_rule_line(line: &str) {
    write_line(stderr_mode(), Stream::Err, color_for_line(line), line);
}

/// 用指定颜色输出一行到 stdout（启动横幅、自身发送的消息等）
pub fn print_colored_line(color: Color, line: &str) {
    write_line(stdout_mode(), Stream::Out, Some(color), line);
}

/// 用指定颜色一次性输出多行到 stdout（启动横幅等）。
///
/// 与逐行调用 `print_colored_line` 的区别：整批在同一个写锁里完成，多线程下
/// （GUI 主线程的输出与机器人后台线程的日志）不会被别的行从中间切开，
/// 等宽艺术字因此不会错位；每行仍然单独进入 GUI「实时日志」缓冲。
pub fn print_colored_lines(color: Color, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    for line in lines {
        crate::runtime::log::push_live(line, false);
    }
    match (stdout_mode(), color) {
        (Mode::Native, _) => native_write(Stream::Out, color, &lines.join("\n")),
        (Mode::Ansi, _) => print_safe(&format!("{}{}{RESET}", color.ansi(), lines.join("\n"))),
        _ => print_safe(&lines.join("\n")),
    }
}

fn write_line(mode: Mode, stream: Stream, color: Option<Color>, line: &str) {
    // 所有控制台输出统一进内存缓冲，供 GUI「实时日志」选项卡展示
    crate::runtime::log::push_live(line, matches!(stream, Stream::Err));
    match (mode, color) {
        (Mode::Native, Some(color)) => native_write(stream, color, line),
        (Mode::Ansi, Some(color)) => match stream {
            Stream::Out => print_safe(&format!("{}{line}{RESET}", color.ansi())),
            Stream::Err => eprint_safe(&format!("{}{line}{RESET}", color.ansi())),
        },
        _ => match stream {
            Stream::Out => print_safe(line),
            Stream::Err => eprint_safe(line),
        },
    }
}

/// Windows 原生染色：设置控制台属性 -> 输出 -> 恢复，全程加锁避免串色。
/// 句柄不是控制台时 `SetConsoleTextAttribute` 失败，此时按纯文本输出，绝不产生裸转义码。
#[cfg(windows)]
fn native_write(stream: Stream, color: Color, line: &str) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(handle) = win::std_handle(stream) else {
        match stream {
            Stream::Out => print_safe(line),
            Stream::Err => eprint_safe(line),
        }
        return;
    };
    // 先读取原色, 再设置新色; 设置失败说明不是控制台句柄(文件/管道), 退化为纯文本
    let restore = win::attributes(handle).unwrap_or(win::DEFAULT_ATTRIBUTES);
    if !win::set_attribute(handle, color.win_attribute()) {
        match stream {
            Stream::Out => print_safe(line),
            Stream::Err => eprint_safe(line),
        }
        return;
    }
    match stream {
        Stream::Out => print_safe(line),
        Stream::Err => eprint_safe(line),
    }
    win::set_attribute(handle, restore);
}

/// 非 Windows 不会进入 Native 模式，这里仅为满足编译
#[cfg(not(windows))]
fn native_write(stream: Stream, color: Color, line: &str) {
    match stream {
        Stream::Out => print_safe(&format!("{}{line}{RESET}", color.ansi())),
        Stream::Err => eprint_safe(&format!("{}{line}{RESET}", color.ansi())),
    }
}

#[cfg(windows)]
fn console_attached(stream: Stream) -> bool {
    win::console_handle(stream).is_some()
}

#[cfg(not(windows))]
fn console_attached(_stream: Stream) -> bool {
    false
}

#[cfg(windows)]
mod win {
    use super::Stream;

    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5; // (DWORD)-11
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4; // (DWORD)-12
    const INVALID_HANDLE_VALUE: isize = -1;
    pub const DEFAULT_ATTRIBUTES: u16 = 0x0007; // FOREGROUND_RED|GREEN|BLUE
    const FILE_TYPE_CHAR: u32 = 0x0002;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ConsoleScreenBufferInfo {
        size: Coord,
        cursor_position: Coord,
        attributes: u16,
        window: SmallRect,
        maximum_window_size: Coord,
    }

    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6; // (DWORD)-10
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF; // (DWORD)-1
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;

    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> *mut core::ffi::c_void;
        fn SetStdHandle(n_std_handle: u32, h_handle: *mut core::ffi::c_void) -> i32;
        fn AttachConsole(dw_process_id: u32) -> i32;
        fn AllocConsole() -> i32;
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut core::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn GetConsoleMode(h_console_handle: *mut core::ffi::c_void, lp_mode: *mut u32) -> i32;
        fn GetFileType(h_file: *mut core::ffi::c_void) -> u32;
        fn GetConsoleScreenBufferInfo(
            h_console_output: *mut core::ffi::c_void,
            lp_console_screen_buffer_info: *mut ConsoleScreenBufferInfo,
        ) -> i32;
        fn SetConsoleTextAttribute(
            h_console_output: *mut core::ffi::c_void,
            w_attributes: u16,
        ) -> i32;
    }

    /// 附加/新建控制台，并把标准流接到 CONIN$/CONOUT$（详见 `super::attach_console`）
    pub fn attach_console() {
        // SAFETY: 仅调用 Win32 控制台 API，句柄在进程生命周期内保持有效
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                AllocConsole();
            }
            if !std_handle_valid(STD_OUTPUT_HANDLE) {
                rebind("CONOUT$", STD_OUTPUT_HANDLE);
            }
            if !std_handle_valid(STD_ERROR_HANDLE) {
                rebind("CONOUT$", STD_ERROR_HANDLE);
            }
            if !std_handle_valid(STD_INPUT_HANDLE) {
                rebind("CONIN$", STD_INPUT_HANDLE);
            }
        }
    }

    /// 标准句柄是否已有效（IDE 管道等场景保持原样）
    fn std_handle_valid(std_handle: u32) -> bool {
        // SAFETY: 仅查询 Win32 标准句柄
        let handle = unsafe { GetStdHandle(std_handle) };
        !handle.is_null() && handle as isize != INVALID_HANDLE_VALUE
    }

    /// 打开 `name`(CONOUT$/CONIN$) 并把它设为指定标准流的句柄；句柄故意不关闭
    unsafe fn rebind(name: &str, std_handle: u32) {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: 调用方保证运行在 Windows 下，wide 以 NUL 结尾
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle as isize == INVALID_HANDLE_VALUE {
            return;
        }
        // SAFETY: handle 由 CreateFileW 返回且有效
        unsafe {
            SetStdHandle(std_handle, handle);
        }
    }

    /// 取标准流句柄（不判断是否为控制台）
    pub fn std_handle(stream: Stream) -> Option<*mut core::ffi::c_void> {
        let std_handle = match stream {
            Stream::Out => STD_OUTPUT_HANDLE,
            Stream::Err => STD_ERROR_HANDLE,
        };
        // SAFETY: 仅查询 Win32 标准句柄
        unsafe {
            let handle = GetStdHandle(std_handle);
            if handle.is_null() || handle as isize == INVALID_HANDLE_VALUE {
                None
            } else {
                Some(handle)
            }
        }
    }

    /// 取标准流句柄，并确认它确实连着一个控制台（而非文件/管道）。
    ///
    /// 优先用 `GetConsoleMode`（需要 GENERIC_READ）；部分宿主/重定向句柄没有读权限，
    /// 此时退化为「尝试写入控制台属性」探测（需要写权限），探测后立即还原颜色。
    pub fn console_handle(stream: Stream) -> Option<*mut core::ffi::c_void> {
        let std_handle = match stream {
            Stream::Out => STD_OUTPUT_HANDLE,
            Stream::Err => STD_ERROR_HANDLE,
        };
        // SAFETY: 仅调用 Win32 控制台 API，指针参数均为栈上有效地址
        unsafe {
            let handle = GetStdHandle(std_handle);
            if handle.is_null() || handle as isize == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut mode: u32 = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                return Some(handle);
            }
            let mut info = ConsoleScreenBufferInfo::default();
            let previous = if GetConsoleScreenBufferInfo(handle, &mut info) != 0 {
                info.attributes
            } else {
                DEFAULT_ATTRIBUTES
            };
            // 无读权限时以写属性探测；成功说明连的是可写控制台，探测值同时作为还原值
            if SetConsoleTextAttribute(handle, previous) != 0 {
                return Some(handle);
            }
            // 最后兜底：字符设备(控制台/终端)无需任何权限即可识别；
            // 即便属性设置失败，也只会退化为纯文本输出，不会产生裸转义码
            if GetFileType(handle) == FILE_TYPE_CHAR {
                return Some(handle);
            }
            None
        }
    }

    /// 当前控制台前景/背景色，用于打印后恢复；读取失败(无读权限)时返回 None
    pub fn attributes(handle: *mut core::ffi::c_void) -> Option<u16> {
        let mut info = ConsoleScreenBufferInfo::default();
        // SAFETY: handle 来自 GetStdHandle，指针参数为栈上有效地址
        unsafe {
            if GetConsoleScreenBufferInfo(handle, &mut info) != 0 {
                Some(info.attributes)
            } else {
                None
            }
        }
    }

    /// 设置控制台属性；返回是否成功（失败说明句柄不是可写控制台/终端）
    pub fn set_attribute(handle: *mut core::ffi::c_void, attributes: u16) -> bool {
        // SAFETY: 同上
        unsafe { SetConsoleTextAttribute(handle, attributes) != 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_for_line_matches_original_rules() {
        // [Arona] / [OneBot] -> 亮绿
        assert_eq!(color_for_line("[Arona] hello"), Some(Color::BrightGreen));
        assert_eq!(
            color_for_line("[OneBot 10000] hello"),
            Some(Color::BrightGreen)
        );
        // 优先级: [Arona] 开头即使含 WARNING 也走亮绿
        assert_eq!(
            color_for_line("[Arona] WARNING: disk full"),
            Some(Color::BrightGreen)
        );
        // WARNING / SLF4J -> 亮黄
        assert_eq!(
            color_for_line("WARNING: disk full"),
            Some(Color::BrightYellow)
        );
        assert_eq!(
            color_for_line("WARNING：磁盘已满"),
            Some(Color::BrightYellow)
        );
        assert_eq!(
            color_for_line("SLF4J: Failed to load class"),
            Some(Color::BrightYellow)
        );
        assert_eq!(
            color_for_line("2026-09-11 12:00:00 INFO  net.diyigemt.Arona"),
            Some(Color::BrightYellow)
        );
        // 不命中 -> 原样
        assert_eq!(color_for_line("plain line"), None);
        assert_eq!(color_for_line("INFOX not a level"), None);
        assert_eq!(color_for_line(""), None);
    }

    #[test]
    fn ansi_codes_match_original() {
        assert_eq!(Color::BrightGreen.ansi(), "\u{1b}[92m");
        assert_eq!(Color::BrightYellow.ansi(), "\u{1b}[93m");
        assert_eq!(Color::Yellow.ansi(), "\u{1b}[33m");
        assert_eq!(Color::BrightMagenta.ansi(), "\u{1b}[95m");
        assert_eq!(RESET, "\u{1b}[0m");
    }

    #[test]
    fn log_source_is_read_out_of_the_prefix() {
        // 插件日志按这个前缀去查已登记的显示名，命中才染淡紫
        assert_eq!(
            source_of("[BluearchivePlugin] 活动推送已启用"),
            Some("BluearchivePlugin")
        );
        assert_eq!(source_of("[Arona] hello"), Some("Arona"));
        // 细化到动作后仍要认得插件名，否则淡紫配色会跟着丢
        assert_eq!(
            source_of("[BluearchivePlugin:定时推送] 开始推送"),
            Some("BluearchivePlugin")
        );
        assert_eq!(source_of("plain line"), None);
        assert_eq!(source_of("[没有闭合的括号"), None);
        // 三方库的日志目标不是插件名，不该被当成插件日志
        assert_eq!(color_for_line("[wgpu_core::instance] Adapter"), None);
    }
}
