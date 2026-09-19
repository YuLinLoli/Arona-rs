//! 致命错误上报（启动失败 / panic）
//!
//! 含 GUI 的 Windows 产物是 windows 子系统（没有控制台），双击启动时一旦失败
//! 就会表现为「没反应」。这里保证错误至少能被看见：
//! 1. 打印到 stderr（有控制台时）
//! 2. 写入统一日志 arona-standalone/logs/arona-yyyy-MM-dd.log
//! 3. 兜底写入 arona-standalone/logs/startup-error.log
//!
//! 这里**不**主动 AllocConsole：GUI 模式/机器人运行期间报个警告就弹出一个黑窗口，
//! 比看不到错误更糟。真要按命令行模式跑，由 `crate::run` 在进入命令行分支时调用
//! `runtime::console::attach_console()`（那时黑窗口是用户要的）。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 上报一个致命/启动错误（本身绝不 panic）
pub fn report(context: &str, message: &str) {
    let text = format!("{context}: {message}");

    // 1) 控制台输出 + 统一日志落盘（log::error 内部两者都会做；没有控制台时只落盘）
    crate::runtime::log::error(&text);
    // 2) 兜底：写一份固定的启动错误文件（日志目录尚未就绪时也能留下线索）
    let path = startup_error_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    append_error_file(&path, &text);
}

/// 弹一个系统消息框（仅 Windows）。
/// GUI 子系统双击启动时连控制台都没有，日志文件路径用户也不知道，只能靠它让错误被看见。
/// 仅在启动阶段的致命配置错误里调用，不进 panic 钩子，避免在无人值守/自动化环境里卡住。
#[cfg(windows)]
pub fn message_box(title: &str, message: &str) {
    use std::os::windows::ffi::OsStrExt;

    const MB_OK: u32 = 0x0000_0000;
    const MB_ICONERROR: u32 = 0x0000_0010;
    const MB_SETFOREGROUND: u32 = 0x0001_0000;
    const MB_TOPMOST: u32 = 0x0004_0000;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, u_type: u32) -> i32;
    }

    let wide = |value: &str| -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let text = wide(message);
    let caption = wide(title);
    // SAFETY: 两个字符串都以 NUL 结尾且在调用期间存活；仅调用 Win32 消息框 API
    unsafe {
        MessageBoxW(
            0,
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// 非 Windows 平台没有消息框，由调用方负责把错误写进日志
#[cfg(not(windows))]
pub fn message_box(_title: &str, _message: &str) {}

/// 启动错误文件路径
pub fn startup_error_path() -> PathBuf {
    crate::runtime::paths::logs_dir().join("startup-error.log")
}

/// 追加一行到指定错误文件（时间戳 + 内容）
fn append_error_file(path: &Path, text: &str) {
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let _ = writeln!(file, "{timestamp} {text}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_timestamped_error_line() {
        let path = std::env::temp_dir().join("arona-crash-report-test.log");
        let _ = std::fs::remove_file(&path);
        append_error_file(&path, "测试错误信息");
        let content = std::fs::read_to_string(&path).expect("错误文件应已写入");
        assert!(content.contains("测试错误信息"));
        let _ = std::fs::remove_file(&path);
    }
}
