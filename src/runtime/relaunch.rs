//! 用另一套参数/环境变量重新拉起自身进程。
//!
//! 有两处需要「换个姿势再来一次」，都走这里：
//! - [`crate::runtime::softgl`]：所有渲染后端都失败后，改成软件 OpenGL(llvmpipe) 再启动一次；
//! - [`crate::gui`] 的看门狗：某个渲染后端**卡死**（eframe/wgpu 不返回错误也不出窗口）时，
//!   换下一个后端另起一个进程。
//!
//! 用普通 `Command` 拉起：子进程直接继承当前进程的令牌（已经提权的仍是管理员），
//! 不会再次弹 UAC，也不用担心 `ShellExecuteW("runas")` 在某些环境下的怪行为。

use std::process::Command;

/// 重新拉起自身（返回 Ok 表示新进程已启动，调用方应尽快结束自己）。
///
/// - `extra`：附加在末尾的参数
/// - `envs`：附加/覆盖的环境变量
/// - `remove_envs`：要删掉的环境变量（例如改走软渲染时的 `ARONA_RENDERER`，否则会被拽回卡死的后端）
/// - `strip_args`：要从原参数里剔除的参数（整段比较）
/// - `strip_prefixes`：要从原参数里剔除的参数前缀（例如换后端重启时的 `--gui-attempt=`）
pub fn spawn_self(
    args: &[String],
    extra: &[String],
    envs: &[(&str, &str)],
    remove_envs: &[&str],
    strip_args: &[&str],
    strip_prefixes: &[&str],
) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("获取自身路径失败: {err}"))?;
    let mut command = Command::new(&exe);
    // 继承工作目录：子进程也要按同样的规则找 softgl\ 与 arona-standalone\
    if let Ok(cwd) = std::env::current_dir() {
        command.current_dir(cwd);
    }
    for arg in args.iter().skip(1) {
        if strip_args.iter().any(|strip| arg == strip) {
            continue;
        }
        if strip_prefixes.iter().any(|prefix| arg.starts_with(prefix)) {
            continue;
        }
        command.arg(arg);
    }
    for arg in extra {
        command.arg(arg);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    for key in remove_envs {
        command.env_remove(key);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("拉起自身的副本失败({}): {err}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只看参数拼装规则：这里把 exe 换成 `cmd`（不会真的重启本程序）
    fn build_args(args: &[String], extra: &[String], strip_args: &[&str], strip_prefixes: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for arg in args.iter().skip(1) {
            if strip_args.iter().any(|strip| arg == strip) {
                continue;
            }
            if strip_prefixes.iter().any(|prefix| arg.starts_with(prefix)) {
                continue;
            }
            out.push(arg.clone());
        }
        out.extend(extra.iter().cloned());
        out
    }

    #[test]
    fn strips_softgl_flag_and_gui_attempt_progress() {
        let args: Vec<String> = ["arona-rs.exe", "--nogui", "--softgl", "--gui-attempt=2"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let extra = vec!["--softgl".to_string()];
        let built = build_args(&args, &extra, &["--softgl"], &["--gui-attempt="]);
        assert_eq!(built, vec!["--nogui".to_string(), "--softgl".to_string()]);
    }

    #[test]
    fn resumes_from_next_renderer() {
        let args: Vec<String> = ["arona-rs.exe", "--renderer=wgpu"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let extra = vec!["--gui-attempt=1".to_string(), "--arona-elevated".to_string()];
        let built = build_args(&args, &extra, &[], &["--gui-attempt="]);
        assert_eq!(
            built,
            vec![
                "--renderer=wgpu".to_string(),
                "--gui-attempt=1".to_string(),
                "--arona-elevated".to_string()
            ]
        );
    }
}