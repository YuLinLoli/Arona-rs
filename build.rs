//! 构建脚本：为 Windows 产物嵌入程序图标。
//!
//! 流程：`assets/arona.ico` -> 生成一行 `.rc` -> 用 Windows SDK 的 `rc.exe` 编译为 `.res`
//!       -> 通过 `cargo:rustc-link-arg` 交给链接器。
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

const ICON_FILE: &str = "assets/arona.ico";

fn main() {
    println!("cargo:rerun-if-changed={ICON_FILE}");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if let Err(err) = embed_icon() {
        println!("cargo:warning=嵌入程序图标失败(不影响编译): {err}");
    }
}

fn embed_icon() -> Result<(), String> {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?);
    let icon = manifest_dir.join(ICON_FILE);
    if !icon.is_file() {
        return Err(format!("找不到图标文件 {}", icon.display()));
    }
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").map_err(|e| e.to_string())?);

    // .rc 里路径要用双反斜杠转义
    let rc_file = out_dir.join("arona-icon.rc");
    let escaped = icon.display().to_string().replace('\\', "\\\\");
    std::fs::write(&rc_file, format!("1 ICON \"{escaped}\"\n")).map_err(|e| e.to_string())?;

    let res_file = out_dir.join("arona-icon.res");
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
    println!("cargo:rustc-link-arg={}", res_file.display());
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