//! 构建任务（cargo 别名见 .cargo/config.toml）
//!
//!   cargo dist       发布构建: target/release/arona-rs[.exe]
//!   cargo installer  在上面基础上调用 Inno Setup，打出 Windows 安装包:
//!                    target/release/arona-rs-<版本号>-setup-win-x64.exe
//!
//! 主程序固定叫 `arona-rs[.exe]`、**不带版本号**：升级/自动更新时可以直接覆盖同名文件，
//! 快捷方式、计划任务、注册表里的 ExeName 也都不用跟着版本号改。版本号只出现在安装包名、
//! 便携 zip 名与「关于」页面里（安装包命名见 installer/arona-rs.iss 的 OutputBaseFilename）。
//! 历史构建留下的 `arona-rs-<版本号>[.exe]` 会在构建结束时清理掉。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 发布构建（默认含管理 GUI），返回 target/release/arona-rs[.exe]
fn build_release(root: &Path) -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    println!("[dist] 开始发布构建");
    // 发布产物包含管理 GUI（默认启动即打开，--nogui 走命令行模式）
    let status = Command::new(&cargo)
        .args(["build", "--release", "--features", "gui"])
        .current_dir(root)
        .status()
        .expect("启动 cargo build --release 失败");
    assert!(status.success(), "cargo build --release 失败");
    target_release_dir(root).join(format!("arona-rs{}", std::env::consts::EXE_SUFFIX))
}

fn target_release_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
        .join("release")
}

/// 交付前的产物整理：主程序固定为 arona-rs[.exe]（不带版本号，便于覆盖升级），
/// 顺带清掉历史构建留下的 `arona-rs-<版本号>[.exe]`（安装包 arona-rs-*-setup-*.exe 保留）。
fn finalize_binary(root: &Path) -> PathBuf {
    let release_dir = target_release_dir(root);
    let suffix = std::env::consts::EXE_SUFFIX;
    let exe = release_dir.join(format!("arona-rs{suffix}"));
    if !exe.is_file() {
        panic!("发布产物不存在: {}", exe.display());
    }
    if let Ok(entries) = std::fs::read_dir(&release_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let stale =
                name.starts_with("arona-rs-") && name.ends_with(suffix) && !name.contains("-setup");
            if !stale {
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => println!("[dist] 已删除带版本号的旧产物: {name}"),
                Err(err) => eprintln!("[dist] 删除 {name} 失败: {err}"),
            }
        }
    }
    exe
}

/// 附带软件 OpenGL 兜底库：仓库根 softgl/ -> target/release/softgl/
///
/// 没有显卡驱动 / 没有 DX12 的服务器靠它用 llvmpipe 把管理面板画出来（见 runtime::softgl）。
/// 返回 false 表示仓库里没有 softgl/（未运行 scripts/fetch-softgl.ps1）。
fn copy_softgl(root: &Path) -> bool {
    let source = root.join("softgl");
    let dest = target_release_dir(root).join("softgl");
    if !source.is_dir() {
        return false;
    }
    match copy_dir(&source, &dest) {
        Ok(()) => {
            println!("[dist] 已附带软件 OpenGL(softgl): {}", dest.display());
            true
        }
        Err(err) => {
            eprintln!("[dist] 复制 softgl 失败: {err}");
            false
        }
    }
}

/// 构建任务: 发布构建并整理产物（主程序 arona-rs[.exe] 不带版本号，交付推荐）
#[ignore = "构建任务, 运行: cargo dist"]
#[test]
fn dist_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    build_release(&root);
    let exe = finalize_binary(&root);
    println!("[dist] 构建产物: {}", exe.display());

    if !copy_softgl(&root) {
        println!(
            "[dist] 未附带软件 OpenGL; 无显卡驱动/无 DX12 的服务器需要先运行 scripts/fetch-softgl.ps1"
        );
    }
}

/// 构建任务: 发布构建 + 打包 Windows 安装包（需要 Inno Setup 6，安装包名带版本号）
///
/// ISCC 查找顺序: 环境变量 ARONA_ISCC -> PATH -> 注册表 InstallLocation -> 常见安装目录。
/// 安装包内容与目录规划见 installer/arona-rs.iss。
#[ignore = "构建任务, 运行: cargo installer"]
#[test]
fn installer_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let version = env!("CARGO_PKG_VERSION");
    let release_dir = target_release_dir(&root);
    println!("[installer] 开始构建安装包 (arona-rs {version})");

    build_release(&root);
    let exe = finalize_binary(&root);
    println!("[installer] 主程序: {}", exe.display());

    let has_softgl = copy_softgl(&root);
    if !has_softgl {
        eprintln!(
            "[installer] 未找到 softgl/，安装包将不含「软件 OpenGL 兜底库」组件；\
             需要的话先运行 scripts/fetch-softgl.ps1 再重新执行 cargo installer"
        );
    }

    let iscc = find_iscc().unwrap_or_else(|| {
        panic!(
            "未找到 Inno Setup 的 ISCC.exe。请先安装 Inno Setup 6：\n\
             \x20   winget install --id JRSoftware.InnoSetup\n\
             \x20 或用环境变量 ARONA_ISCC 直接指定 ISCC.exe 的完整路径"
        )
    });
    println!("[installer] 使用 Inno Setup: {}", iscc.display());

    // ISCC 必须在 installer/ 目录下运行：.iss 里的 Source: "intro.txt" 都是相对路径。
    // 这个目录曾经漏提交过（没被 .gitignore 忽略，只是没 git add），CI 干净检出里没有它，
    // 结果 Command 只报一句难懂的 "The directory name is invalid"，这里先给出明确原因。
    let installer_dir = root.join("installer");
    assert!(
        installer_dir.join("arona-rs.iss").is_file(),
        "找不到安装包脚本 {}：installer/ 必须一起提交到仓库（git add installer）",
        installer_dir.display()
    );

    // 宏值里可能有空格，Command 会自动加引号
    let mut command = Command::new(&iscc);
    command
        .arg(format!("/DAppVersion={version}"))
        .arg(format!("/DAppSourceDir={}", release_dir.display()))
        .arg(format!("/DOutputDir={}", release_dir.display()))
        .current_dir(&installer_dir);
    if has_softgl {
        command.arg("/DHasSoftgl=1");
    }
    command.arg("arona-rs.iss");

    let status = command.status().expect("启动 ISCC.exe 失败");
    assert!(status.success(), "Inno Setup 编译安装包失败");

    let setup = release_dir.join(format!("arona-rs-{version}-setup-win-x64.exe"));
    let size = std::fs::metadata(&setup)
        .map(|meta| meta.len())
        .unwrap_or_default();
    println!(
        "[installer] 安装包: {} ({:.1} MiB)",
        setup.display(),
        size as f64 / 1024.0 / 1024.0
    );
}

/// 查找 Inno Setup 的命令行编译器 ISCC.exe
fn find_iscc() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ARONA_ISCC") {
        let candidate = PathBuf::from(value);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("ISCC.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    for key in [
        r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1",
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1",
        // 32 位版 Inno Setup 装在 64 位系统上会写到 WOW6432Node
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1",
    ] {
        if let Some(dir) = reg_install_location(key) {
            candidates.push(dir.join("ISCC.exe"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local)
                .join("Programs")
                .join("Inno Setup 6")
                .join("ISCC.exe"),
        );
    }
    candidates.push(PathBuf::from(r"C:\Program Files (x86)\Inno Setup 6\ISCC.exe"));
    candidates.push(PathBuf::from(r"C:\Program Files\Inno Setup 6\ISCC.exe"));
    candidates.into_iter().find(|path| path.is_file())
}

/// 从注册表卸载项里读 InstallLocation
fn reg_install_location(key: &str) -> Option<PathBuf> {
    let output = Command::new("reg")
        .args(["query", key, "/v", "InstallLocation"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        // 形如:  InstallLocation    REG_SZ    C:\Program Files (x86)\Inno Setup 6\
        if let Some(index) = line.find("REG_SZ") {
            let value = line[index + "REG_SZ".len()..].trim();
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

/// 递归复制目录（目标目录不存在则创建）
fn copy_dir(source: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let target = dest.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}