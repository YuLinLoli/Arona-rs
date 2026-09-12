//! 软件 OpenGL（Mesa llvmpipe）兜底方案。
//!
//! 背景：Windows Server / 没有显卡驱动的虚拟机只有 GDI 自带的 OpenGL 1.1，而 egui 需要
//! OpenGL 2.0+；如果 wgpu 的 DX12/WARP 也不可用（例如系统缺少 `d3d12.dll`），管理面板
//! 在这类机器上就完全打不开。解决办法是把 Mesa 的软件渲染版 `opengl32.dll`（llvmpipe）
//! 放进 exe 同级的 `softgl\` 目录，用 CPU 把界面画出来。
//!
//! 为什么要“重启自己”：`glutin_wgl_sys` 是以 `#[link(name = "opengl32")]` 静态导入
//! opengl32.dll 的（本 exe 的延迟加载表里也有它）。进程一旦真的调用过 GL，系统那份
//! opengl32.dll 就已被加载并常驻，之后再改 DLL 搜索路径也不会生效。所以主进程在所有
//! 渲染后端都失败后，会以 `--softgl` 重新拉起自身；子进程在 main 最开头调用 [`activate`]，
//! 先把 `softgl\` 塞进 DLL 搜索路径，再交给 eframe 用 glow 渲染。
//!
//! 获取依赖文件：`powershell -ExecutionPolicy Bypass -File scripts/fetch-softgl.ps1`

use std::path::{Path, PathBuf};

/// 存放软件 OpenGL 的目录名（相对 exe 目录 / 工作目录）
const SOFTGL_DIR_NAME: &str = "softgl";

/// opengl32.dll 是否存在（Mesa 的 WGL 前端，配合同目录的 libgallium_wgl.dll 使用）
fn has_opengl(dir: &Path) -> bool {
    dir.join("opengl32.dll").is_file()
}

/// 定位 softgl 目录：`ARONA_SOFTGL_DIR` -> exe 同级 -> 工作目录 -> 数据目录
pub fn dir() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ARONA_SOFTGL_DIR") {
        let path = PathBuf::from(value);
        if has_opengl(&path) {
            return Some(path);
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    roots.push(crate::runtime::paths::data_root());
    for root in roots {
        let candidate = root.join(SOFTGL_DIR_NAME);
        if has_opengl(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// 是否处于软件 OpenGL 模式：命令行带 `--softgl`，或环境变量 `ARONA_SOFTGL=1`
pub fn requested(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--softgl") || env_flag("ARONA_SOFTGL")
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

/// activate 的结果（幂等：多次调用只生效一次，也只记一条日志）
static ACTIVATED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// 把 softgl 目录加入 DLL 搜索路径，并强制 Mesa 走 llvmpipe 软件光栅化。
///
/// 必须在任何 opengl32 调用（eframe/glutin 创建上下文）之前调用：main 最开头（--softgl）
/// 或 gui::run 创建线程之前。重复调用是安全的（幂等）。
pub fn activate() -> bool {
    *ACTIVATED.get_or_init(activate_once)
}

fn activate_once() -> bool {
    let Some(dir) = dir() else {
        crate::runtime::console::eprint_safe(
            "[Arona] 请求了软件 OpenGL 模式，但没找到 softgl 目录（应含有 opengl32.dll）",
        );
        return false;
    };
    set_default_env("GALLIUM_DRIVER", "llvmpipe");
    set_default_env("LIBGL_ALWAYS_SOFTWARE", "1");
    set_default_env("MESA_LOADER_DRIVER_OVERRIDE", "llvmpipe");

    #[cfg(windows)]
    {
        if win::set_dll_directory(&dir) {
            crate::runtime::log::info(format!("已启用软件 OpenGL(llvmpipe): {}", dir.display()));
            true
        } else {
            crate::runtime::log::warning(format!("设置 DLL 搜索目录失败: {}", dir.display()));
            false
        }
    }
    #[cfg(not(windows))]
    {
        let _ = dir;
        false
    }
}

/// 设置环境变量（仅当用户没自己指定时）
fn set_default_env(key: &str, value: &str) {
    if std::env::var_os(key).is_none() {
        // SAFETY: 只在启动阶段（main 最开头或 gui::run 创建线程之前）调用，此后不再改动环境
        unsafe { std::env::set_var(key, value) };
    }
}

/// 以 `--softgl` 重新拉起自身。
///
/// 成功返回 true，调用方应立即退出：此时机器人随后会在子进程里启动，避免两个实例同时跑。
pub fn relaunch(args: &[String]) -> bool {
    // 带上 --softgl 与 ARONA_SOFTGL 双保险；已试过的后端进度(--gui-attempt=)一并丢弃，
    // 让新进程从「软件 OpenGL」这条候选重新开始
    let extra = vec!["--softgl".to_string()];
    let envs = [("ARONA_SOFTGL", "1")];
    let remove_envs = ["ARONA_RENDERER"];
    match crate::runtime::relaunch::spawn_self(
        args,
        &extra,
        &envs,
        &remove_envs,
        &["--softgl", "--renderer="],
        &["--gui-attempt=", "--gui-handoff=", "--renderer="],
    ) {
        Ok(()) => true,
        Err(err) => {
            crate::runtime::log::warning(format!("以软件 OpenGL 模式重新启动失败: {err}"));
            false
        }
    }
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetDllDirectoryW(lp_path_name: *const u16) -> i32;
    }

    /// 把一个目录插入 DLL 搜索顺序（位于 exe 目录之后、System32 之前），
    /// 这样延迟加载的 opengl32.dll 会优先命中 softgl 里的 Mesa 版本。
    pub fn set_dll_directory(dir: &Path) -> bool {
        let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
        wide.push(0);
        unsafe { SetDllDirectoryW(wide.as_ptr()) != 0 }
    }
}