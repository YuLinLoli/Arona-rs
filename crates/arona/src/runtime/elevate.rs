//! 管理员权限申请（Windows UAC 提权）。
//!
//! 主程序启动时会申请管理员权限：机器人要写数据目录、监听端口、必要时替换 DLL，
//! 统一以管理员身份运行能避免权限不足导致的各种失败。
//!
//! ## 为什么不用清单里的 `requireAdministrator`
//!
//! [`build.rs`](build.rs) 用 `cargo:rustc-link-arg-bins` 把图标/清单资源挂到 bin 目标上，
//! 而 cargo 的测试运行器（`cargo test` 的 libtest 进程）也是从同一个 bin 目标构建出来的，
//! 会一起带上同一份资源。清单里写 `requireAdministrator` 之后，测试进程同样要求提权，
//! cargo 的 `CreateProcess` 会直接失败（os error 740），CI 和本地的 `cargo test` 全部跑不起来。
//!
//! 所以改成**运行期自提权**：exe 自身仍是 `asInvoker`，启动后自己用
//! `ShellExecuteW("runas")` 拉起一个提权实例再退出。对用户来说效果和清单一致
//! （启动时弹 UAC，之后以管理员身份运行），但不影响测试与其它构建任务。
//!
//! 环境变量 `ARONA_NO_ELEVATE=1` 可以临时跳过提权（调试 / 自动化测试用）。

/// 提权重启时附加的标记，用来防止无限重启
const ELEVATED_FLAG: &str = "--arona-elevated";

/// 是否已经以管理员身份运行
#[cfg(windows)]
pub fn is_elevated() -> bool {
    win::is_process_elevated()
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// 申请管理员权限。
///
/// 返回 true 表示已经用提权实例重新启动了自身，调用方应**立即退出**，
/// 否则会出现两个实例同时跑（重复响应消息、抢数据库）。
/// 返回 false 表示不需要提权、已经提权、用户拒绝 UAC 或提权失败，可以照常继续。
pub fn request_admin(args: &[String]) -> bool {
    // 关掉提权开关（调试 / 自动化测试）
    if env_flag("ARONA_NO_ELEVATE") {
        return false;
    }
    // 已经是被自己拉起来的提权实例：不能再提一次，否则会无限重启
    if args.iter().any(|arg| arg == ELEVATED_FLAG) {
        return false;
    }
    if is_elevated() {
        return false;
    }

    #[cfg(windows)]
    match win::relaunch_elevated(args) {
        Ok(()) => true,
        Err(err) => {
            crate::runtime::log::warning(format!(
                "申请管理员权限失败({err})，将以普通权限继续运行；需要时请右键“以管理员身份运行”"
            ));
            false
        }
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        false
    }
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;

    /// 需要查询令牌信息
    const TOKEN_QUERY: u32 = 0x0008;
    /// TokenElevation 的信息类别编号
    const TOKEN_ELEVATION: u32 = 20;
    const SW_SHOWNORMAL: i32 = 1;
    /// ShellExecuteW 返回值大于该值才算成功
    const SHELL_EXECUTE_SUCCESS_LIMIT: isize = 32;

    #[repr(C)]
    struct TokenElevationInfo {
        token_is_elevated: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn CloseHandle(handle: isize) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: isize, desired_access: u32, token: *mut isize) -> i32;
        fn GetTokenInformation(
            token: isize,
            information_class: u32,
            information: *mut u8,
            information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show_command: i32,
        ) -> isize;
    }

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// 用 TokenElevation 判断当前进程是否已提权
    pub fn is_process_elevated() -> bool {
        unsafe {
            let mut token: isize = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut info = TokenElevationInfo {
                token_is_elevated: 0,
            };
            let mut returned: u32 = 0;
            let ok = GetTokenInformation(
                token,
                TOKEN_ELEVATION,
                &mut info as *mut TokenElevationInfo as *mut u8,
                std::mem::size_of::<TokenElevationInfo>() as u32,
                &mut returned,
            );
            CloseHandle(token);
            ok != 0 && info.token_is_elevated != 0
        }
    }

    /// 用 `runas` 动词重新拉起自身（会弹 UAC）；工作目录一并传过去，
    /// 因为数据目录 `arona-standalone/` 是相对工作目录创建的
    pub fn relaunch_elevated(args: &[String]) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|err| err.to_string())?;
        let mut params: Vec<String> = args.iter().skip(1).cloned().collect();
        params.push(super::ELEVATED_FLAG.to_string());
        let parameters = params
            .iter()
            .map(|arg| quote_arg(arg))
            .collect::<Vec<_>>()
            .join(" ");

        let operation = wide("runas");
        let file = wide(&exe.to_string_lossy());
        let parameters = wide(&parameters);
        let directory = std::env::current_dir()
            .ok()
            .map(|dir| wide(&dir.to_string_lossy()));
        let directory_ptr = directory
            .as_ref()
            .map_or(std::ptr::null(), |value| value.as_ptr());

        let result = unsafe {
            ShellExecuteW(
                0,
                operation.as_ptr(),
                file.as_ptr(),
                parameters.as_ptr(),
                directory_ptr,
                SW_SHOWNORMAL,
            )
        };
        if result > SHELL_EXECUTE_SUCCESS_LIMIT {
            Ok(())
        } else {
            Err(format!("ShellExecuteW 返回 {}", result))
        }
    }

    /// 按 Windows 命令行解析规则给参数加引号
    fn quote_arg(arg: &str) -> String {
        if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
            return arg.to_string();
        }
        let mut quoted = String::with_capacity(arg.len() + 2);
        quoted.push('"');
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => {
                    backslashes += 1;
                    quoted.push('\\');
                }
                '"' => {
                    // 引号前的反斜杠要翻倍，再加一个转义引号本身
                    for _ in 0..backslashes {
                        quoted.push('\\');
                    }
                    backslashes = 0;
                    quoted.push('\\');
                    quoted.push('"');
                }
                _ => {
                    backslashes = 0;
                    quoted.push(ch);
                }
            }
        }
        // 结尾反斜杠翻倍，避免把收尾的引号转义掉
        for _ in 0..backslashes {
            quoted.push('\\');
        }
        quoted.push('"');
        quoted
    }

    #[cfg(test)]
    mod tests {
        use super::quote_arg;

        #[test]
        fn quote_arg_keeps_simple_args() {
            assert_eq!(quote_arg("--nogui"), "--nogui");
            assert_eq!(
                quote_arg("--config=C:\\arona\\onebot.yml"),
                "--config=C:\\arona\\onebot.yml"
            );
        }

        #[test]
        fn quote_arg_wraps_paths_with_spaces() {
            assert_eq!(
                quote_arg("C:\\Arona rs\\arona.yml"),
                "\"C:\\Arona rs\\arona.yml\""
            );
            assert_eq!(quote_arg(""), "\"\"");
            // 路径以反斜杠结尾时，结尾引号前的反斜杠必须翻倍
            assert_eq!(quote_arg("C:\\Arona rs\\"), "\"C:\\Arona rs\\\\\"");
        }
    }
}
