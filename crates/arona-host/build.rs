//! 构建脚本：为 Windows 产物嵌入程序图标与应用程序清单。
//!
//! 流程：`assets/arona.ico` + `assets/arona.manifest` -> 生成 `.rc` -> 用 Windows SDK 的
//!       `rc.exe` 编译为 `.res` -> 通过 `cargo:rustc-link-arg` 交给链接器。
//!
//! 本脚本不涉及功能插件：插件是运行期从 `plugins/` 目录扫描加载的 dll
//! （见 `arona::plugin::dynamic`），宿主不链接、不注册任何插件，产物永远是框架本体。
//!
//! 清单（RT_MANIFEST, 资源 id 1）里声明的是 `asInvoker`（保持系统默认的启动级别），
//! 管理员权限由程序在运行期通过 `crates/arona/src/runtime/elevate.rs` 自提权（弹 UAC）获得，
//! 这样 cargo test 等从同一 bin 目标构建的测试进程不会被强制要求提权。
//!
//! `rc.exe` 的查找顺序：
//!   1. 环境变量 `ARONA_RC`（显式指定，便于特殊环境或 CI 覆盖）
//!   2. 环境变量 `WindowsSdkVerBinPath` / `WindowsSdkDir`
//!   3. 注册表 `Microsoft SDKs\Windows\v10.0` 的 `InstallationFolder`
//!   4. 常见安装路径（`%ProgramFiles(x86)%\Windows Kits\10`、各盘符下的 `Windows Kits\10`）
//!   5. `PATH`
//!
//! 全部找不到时只输出 `cargo:warning`，不中断构建（非 Windows 目标直接跳过）。
//! 重新生成图标见 `scripts/make-icon.ps1`。

use std::path::{Path, PathBuf};
use std::process::Command;

const ICON_FILE: &str = "../../assets/arona.ico";
/// 应用程序清单：声明 DPI 感知与 asInvoker 启动级别（图标另见 assets/arona.ico）
const MANIFEST_FILE: &str = "../../assets/arona.manifest";

fn main() {
    println!("cargo:rerun-if-changed={ICON_FILE}");
    println!("cargo:rerun-if-changed={MANIFEST_FILE}");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // GUI 构建会经 wgpu 触发 D3DCompile（未编入静态 DXC，运行期动态取着色器编译器），
    // 因此把 d3dcompiler_47.dll 改成“延迟加载”：exe 启动时不再强依赖它（Windows Server /
    // 精简系统上常常没有这个 DLL），只有真的调用到才去加载。
    // 注意：只有带 GUI(即启用 wgpu/dx12) 的构建才会有 d3dcompiler_47.dll 导入，
    // 无 GUI 构建加上 /DELAYLOAD 反而会让链接器报 LNK4199，所以这里按特性门控。
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
        && std::env::var_os("CARGO_FEATURE_GUI").is_some()
    {
        println!("cargo:rustc-link-arg=/DELAYLOAD:d3dcompiler_47.dll");
        // glutin_wgl_sys 用 #[link(name = "opengl32")] 静态导入 opengl32.dll，进程启动
        // 就会去加载系统那份（服务器上往往只有 GDI 的 OpenGL 1.1）。改成延迟加载后，
        // 只要在第一次调用 GL 之前 SetDllDirectory 指向 softgl 目录，就能换成 Mesa 的
        // 软件 OpenGL(llvmpipe)，见 crates/arona/src/runtime/softgl.rs。
        println!("cargo:rustc-link-arg=/DELAYLOAD:opengl32.dll");
        println!("cargo:rustc-link-lib=delayimp");
    }

    if let Err(err) = embed_resources() {
        println!("cargo:warning=嵌入图标/清单失败(不影响编译): {err}");
    }
}

fn embed_resources() -> Result<(), String> {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?);
    let icon = manifest_dir.join(ICON_FILE);
    if !icon.is_file() {
        return Err(format!("找不到图标文件 {}", icon.display()));
    }
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").map_err(|e| e.to_string())?);

    // .rc 里路径要用双反斜杠转义
    let rc_file = out_dir.join("arona-res.rc");
    let escaped_icon = icon.display().to_string().replace('\\', "\\\\");
    let mut rc = format!("1 ICON \"{escaped_icon}\"\n");

    // 应用程序清单：1 = CREATEPROCESS_MANIFEST_RESOURCE_ID，24 = RT_MANIFEST。
    // 清单保持 asInvoker，管理员权限由 crates/arona/src/runtime/elevate.rs 在启动时自提权申请。
    let manifest = manifest_dir.join(MANIFEST_FILE);
    if manifest.is_file() {
        let escaped_manifest = manifest.display().to_string().replace('\\', "\\\\");
        rc.push_str(&format!("1 24 \"{escaped_manifest}\"\n"));
    } else {
        println!(
            "cargo:warning=找不到清单文件 {}，产物不会申请管理员权限",
            manifest.display()
        );
    }
    std::fs::write(&rc_file, rc).map_err(|e| e.to_string())?;

    let res_file = out_dir.join("arona-res.res");
    let rc = find_rc().ok_or_else(|| {
        "未找到 rc.exe（可设置环境变量 ARONA_RC 指向 Windows SDK 的 rc.exe）".to_string()
    })?;
    let output = Command::new(&rc)
        .arg("/nologo")
        .arg(format!("/fo{}", res_file.display()))
        .arg(&rc_file)
        .output()
        .map_err(|e| format!("运行 {} 失败: {e}", rc.display()))?;
    if !output.status.success() {
        return Err(format!(
            "rc.exe 返回 {:?}: {}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    if !res_file.is_file() {
        return Err(format!("rc.exe 未生成 {}", res_file.display()));
    }
    // 只给 bin 目标挂资源：`rustc-link-arg` 会连测试/示例二进制一起带上，
    // 那样 cargo test 出来的测试进程也会要求管理员权限，CI 里跑不起来。
    println!("cargo:rustc-link-arg-bins={}", res_file.display());
    Ok(())
}

/// 按优先级查找 rc.exe
fn find_rc() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ARONA_RC") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    for key in ["WindowsSdkVerBinPath", "WindowsSdkDir"] {
        if let Ok(dir) = std::env::var(key) {
            if let Some(found) = rc_in(&PathBuf::from(dir)) {
                return Some(found);
            }
        }
    }
    if let Some(root) = registry_sdk_root() {
        if let Some(found) = rc_in(&root.join("bin")) {
            return Some(found);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
        candidates.push(PathBuf::from(pf86).join("Windows Kits\\10\\bin"));
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        candidates.push(PathBuf::from(pf).join("Windows Kits\\10\\bin"));
    }
    for drive in ["C", "D", "E"] {
        candidates.push(PathBuf::from(format!("{drive}:\\Windows Kits\\10\\bin")));
    }
    for dir in &candidates {
        if let Some(found) = rc_in(dir) {
            return Some(found);
        }
    }
    rc_in_path()
}

/// 在 `<dir>` 或 `<dir>\<版本>\x64\` 下找 rc.exe，版本目录按名称倒序（取最新 SDK）
fn rc_in(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join("x64").join("rc.exe");
    if direct.is_file() {
        return Some(direct);
    }
    let mut versions: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_dir())
        .collect();
    versions.sort();
    for version in versions.iter().rev() {
        for arch in ["x64", "x86"] {
            let candidate = version.join(arch).join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// 从注册表读取 Windows SDK 安装根目录（64 位进程需要显式查 WOW6432Node）
fn registry_sdk_root() -> Option<PathBuf> {
    for key in [
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Microsoft SDKs\Windows\v10.0",
        r"HKLM\SOFTWARE\Microsoft\Microsoft SDKs\Windows\v10.0",
    ] {
        let output = Command::new("reg")
            .args(["query", key, "/v", "InstallationFolder"])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            if let Some(index) = line.find("REG_SZ") {
                let value = line[index + "REG_SZ".len()..].trim();
                if !value.is_empty() {
                    return Some(PathBuf::from(value));
                }
            }
        }
    }
    None
}

fn rc_in_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("rc.exe"))
        .find(|candidate| candidate.is_file())
}
