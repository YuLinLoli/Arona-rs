//! 运行期日志（对应原版 RuntimeLog + StandaloneLogFile）
//! 独立模式输出到 stdout，并附带写入 arona-standalone/logs/arona-yyyy-MM-dd.log（按天滚动）。

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
