//! 构建任务（cargo 别名见 .cargo/config.toml）
//!
//!   cargo dist       发布构建: target/release/arona-rs[.exe]
//!   cargo installer  在上面基础上调用 Inno Setup，打出 Windows 安装包:
//!                    target/release/arona-rs-<版本号>-setup-win-x64.exe（仅 Windows）
//!   cargo linux-dist 在 Linux 上打出两个交付包（仅 Linux）:
//!                    arona-rs-<版本号>-linux-x64.tar.gz        （动态链接 glibc）
//!                    arona-rs-<版本号>-linux-x64-static.tar.gz （musl 静态，无任何依赖）
//!
//! **平台是构建出来的，不是写死的**：产物布局、随包组件、打包步骤全部按
//! `cfg!(windows)` / `cfg!(unix)` 分流，同一份 `cargo dist` 在 Windows 上产出
//! exe + softgl/ + DXC，在 Linux 上产出无扩展名的 ELF + fonts/，不需要改命令。
//!
//! 主程序固定叫 `arona-rs[.exe]`、**不带版本号**：升级/自动更新时可以直接覆盖同名文件，
//! 快捷方式与计划任务也都不用跟着版本号改。版本号只出现在包名与「关于」页面里。
//! 历史构建留下的 `arona-rs-<版本号>[.exe]` 会在构建结束时清理掉。
//!
//! 渲染运行库（softgl/ 与 DXC）**只属于 Windows**：Linux 版只有命令行模式，
//! 不链接 eframe/wgpu，也就没有显卡后端要兜底。反过来，随包中文字体两平台都带，
//! 因为出图插件在 Linux 上同样要用它画字。
//!
//! 产物永远是框架本体：功能插件不链接进 exe，而是编译成 dll/so 由用户在启动前放进
//! `plugins/`，框架启动时扫描装载（见 `arona::plugin::dynamic`）。
//! 因此这里不存在"带插件的宿主"这条分支，也不需要给包名加区分后缀。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 发布构建（默认含管理 GUI），返回 target/release/arona-rs[.exe]
///
/// 目标三元组由调用方的 `--target` 决定；这里不改目录约定，只按当前构建的 target
/// 去找产物。Linux 静态包构建时会传 `--target x86_64-unknown-linux-musl`，
/// 产物落在 `target/<三元组>/release/`。
fn build_release(root: &Path) -> PathBuf {
    build_release_for(root, None)
}

/// 可选目标三元组的发布构建（`None` = 当前主机）
///
/// 什么时候必须改用 `cargo zigbuild` 而不是 `cargo build`，有两个互不相干的原因：
///
/// 1. **目标带 glibc 版本后缀**（`x86_64-unknown-linux-gnu.2.17`）。这个写法是
///    cargo-zigbuild 自己的语法，普通 cargo 会直接报
///    「could not find specification for target "…"」。**与构建机是什么系统无关**——
///    Linux 原生机器上同样得走 zigbuild。
/// 2. **在非 Linux 主机上构建 Linux 目标**：给 Linux 编 C 代码（`ring` 的加密实现、
///    `libsqlite3-sys` 的 sqlite3.c）需要一套 Linux 的 libc 头与链接器，其它平台上
///    没有。zig 自带这些，`cargo zigbuild` 会把 `zig cc` 包装成 cc-rs 认得的形态
///    （直接用 `CC="zig cc -target ..."` 会被 cc-rs 的编译器家族探测打回，它探测时
///    只调 `zig -E`，把 `cc` 子命令丢了）。
///
/// 其余情况走普通 `cargo build`，链接器由系统提供。
fn build_release_for(root: &Path, target: Option<&str>) -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let zigbuild = needs_zigbuild_for(target, cfg!(target_os = "linux"));
    let subcommand = if zigbuild {
        println!("[dist] 这一档需要用 cargo zigbuild");
        "zigbuild"
    } else {
        "build"
    };
    println!(
        "[dist] 开始发布构建 (cargo {subcommand}{})",
        target.map(|t| format!(" {t}")).unwrap_or_default()
    );
    // workspace 根是虚拟清单，`--features` 必须落到具体包上：产物就是 host 的 arona-rs
    let mut command = Command::new(&cargo);
    command
        .arg(subcommand)
        .arg("--release")
        .arg("-p")
        .arg("arona-host");
    // Linux 只交付命令行版：不编 eframe/wgpu，产物既小又不依赖任何图形库
    if cfg!(windows) {
        command.args(["--features", "gui"]);
    } else {
        command.args(["--no-default-features"]);
    }
    if let Some(target) = target {
        command.arg("--target").arg(target);
    }
    let status = command
        .current_dir(root)
        .status()
        .expect("启动 cargo build --release 失败");
    assert!(status.success(), "cargo build --release 失败");
    release_dir(root, target).join(program_name())
}

/// 主程序文件名：Windows 带 .exe，Linux 不需要扩展名
fn program_name() -> String {
    format!("arona-rs{}", std::env::consts::EXE_SUFFIX)
}

/// workspace 根目录（仓库根）：本测试在 crates/arona-host 包里，需向上两级。
/// target/、installer/、softgl/、assets/ 都在仓库根。
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or(manifest)
}

/// 某个目标三元组（或当前主机）的 release 目录。
///
/// 先过 [`cargo_target_dir_name`] 把 zigbuild 的 glibc 版本后缀剥掉——不剥的话
/// 会去 `target/x86_64-unknown-linux-gnu.2.17/release/` 找产物，而 cargo 实际把它
/// 放在 `target/x86_64-unknown-linux-gnu/release/`（版本后缀只传给链接器）。
fn release_dir(root: &Path, target: Option<&str>) -> PathBuf {
    let base = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    match target {
        Some(target) => base.join(cargo_target_dir_name(target)).join("release"),
        None => base.join("release"),
    }
}

/// `cargo-zigbuild` 的目标写法 `x86_64-unknown-linux-gnu.2.17` 里，`.2.17` 只影响链接
/// 阶段用的 glibc 版本，**产物目录仍然是基础三元组**。这里把它剥掉，否则打包时会在
/// 一个不存在的目录里找二进制。
///
/// 判据是「第一个点及其后面的部分」：Rust 的 target 三元组用 `-` 分段，自身不含点，
/// 所以出现点就一定是 zigbuild 加上的版本后缀。
fn cargo_target_dir_name(target: &str) -> &str {
    match target.split_once('.') {
        Some((base, version))
            if !base.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.') =>
        {
            base
        }
        _ => target,
    }
}

/// 这一档要不要走 `cargo zigbuild`（详见 [`build_release_for`] 的注释）。
///
/// `host_is_linux` 由调用方传入而不是直接读 `cfg!`，这样两个方向都能在任意平台上被单测覆盖。
fn needs_zigbuild_for(target: Option<&str>, host_is_linux: bool) -> bool {
    let Some(target) = target else {
        return false;
    };
    // ① 带 glibc 版本后缀的写法只有 cargo-zigbuild 认，跟构建机无关
    if cargo_target_dir_name(target) != target {
        return true;
    }
    // ② 在非 Linux 主机上编 Linux 目标 = 交叉编译，需要 zig 提供 libc 头与链接器
    target.contains("linux") && !host_is_linux
}

fn target_release_dir(root: &Path) -> PathBuf {
    release_dir(root, None)
}

/// 交付前的产物整理：主程序固定为 arona-rs[.exe]（不带版本号，便于覆盖升级），
/// 顺带清掉历史构建留下的 `arona-rs-<版本号>[.exe]`（安装包与压缩包保留）。
fn finalize_binary(root: &Path) -> PathBuf {
    let release_dir = target_release_dir(root);
    let exe = release_dir.join(program_name());
    if !exe.is_file() {
        panic!("发布产物不存在: {}", exe.display());
    }
    if let Ok(entries) = std::fs::read_dir(&release_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let stale = is_stale_binary(&name);
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

/// 是不是该清理的旧产物：`arona-rs-<版本号>` 形态的**可执行文件**。
///
/// 不能靠「文件名里有没有点」来判断有没有扩展名——版本号 `1.2.0` 自己就带两个点，
/// 那样写会把 Linux 上真正的旧产物 `arona-rs-1.2.0` 判成"有扩展名的交付包"而漏掉
/// （这条是 CI 的 Linux job 抓出来的：同一段逻辑在 Windows 上恰好因为产物带 `.exe`
/// 而侥幸成立）。所以改成按结构认：前缀之后必须**恰好是一段三段式版本号**，
/// Windows 形态再允许跟一个 `.exe`。交付包（`-linux-x64.tar.gz`、`-setup-…`、`.zip`）
/// 都在版本号后面还有东西，自然落不进来。
fn is_stale_binary(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("arona-rs-") else {
        return false;
    };
    if name.contains("-setup") {
        return false;
    }
    // Windows 的旧产物带 .exe，Linux 的不带；两种都认
    let version = rest.strip_suffix(".exe").unwrap_or(rest);
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// zigbuild 的版本后缀必须从**目录名**里剥掉，否则打包时会在一个不存在的
    /// `target/x86_64-unknown-linux-gnu.2.17/release/` 里找二进制而扑空。
    /// 这条钉住的是「`.2.17` 只影响链接、不影响产物落点」这个容易记错的事实。
    #[test]
    fn the_glibc_suffix_never_lands_in_the_target_directory_name() {
        assert_eq!(
            cargo_target_dir_name("x86_64-unknown-linux-gnu.2.17"),
            "x86_64-unknown-linux-gnu"
        );
        // 没后缀的原样返回
        assert_eq!(
            cargo_target_dir_name("x86_64-unknown-linux-gnu"),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            cargo_target_dir_name("x86_64-pc-windows-msvc"),
            "x86_64-pc-windows-msvc"
        );
        // 点后面不是纯版本号时不动它：宁可原样保留，也别把未知写法悄悄改坏
        assert_eq!(
            cargo_target_dir_name("weird.target-name"),
            "weird.target-name"
        );
    }

    /// 什么时候必须走 `cargo zigbuild`。这条是 CI 抓出来的教训：最初只按「是否跨平台」
    /// 判断，于是在 Linux 原生机器上构建 compat 包时走了普通 `cargo build`，cargo 当场
    /// 报 `could not find specification for target "x86_64-unknown-linux-gnu.2.17"`——
    /// 那个后缀**本来就只有 cargo-zigbuild 认**，跟构建机是什么系统无关。
    #[test]
    fn zigbuild_is_required_for_the_glibc_suffix_and_for_cross_builds() {
        // ① 带 glibc 版本后缀：两种主机上都得走 zigbuild
        assert!(needs_zigbuild_for(
            Some("x86_64-unknown-linux-gnu.2.17"),
            true
        ));
        assert!(needs_zigbuild_for(
            Some("x86_64-unknown-linux-gnu.2.17"),
            false
        ));

        // ② 不带后缀的 Linux 目标：Linux 主机走普通 build，跨平台走 zigbuild
        assert!(!needs_zigbuild_for(Some("x86_64-unknown-linux-gnu"), true));
        assert!(needs_zigbuild_for(Some("x86_64-unknown-linux-gnu"), false));

        // ③ 主机自身平台一律普通 build
        assert!(!needs_zigbuild_for(None, true));
        assert!(!needs_zigbuild_for(None, false));
        // Windows 目标不涉及 zig
        assert!(!needs_zigbuild_for(Some("x86_64-pc-windows-msvc"), false));
    }

    /// 旧产物清理的两个方向都得钉住：
    /// 交付包（`.tar.gz` / `.zip` / `-setup`）一律不许动，而 `arona-rs-<版本号>` 形态的
    /// 可执行文件该被清掉——**两个平台的形态都要认**。
    ///
    /// 这条测试原先是按 `cfg!(windows)` 分叉断言的，Windows 上恰好因为产物带 `.exe`
    /// 而过关，Linux 上才暴露「版本号自己带点」那个判断错误。所以现在两种形态都断言。
    #[test]
    fn packaging_artifacts_are_never_mistaken_for_stale_binaries() {
        // 交付包：版本号后面还有东西，都不算旧产物
        assert!(!is_stale_binary("arona-rs-1.2.0-linux-x64.tar.gz"));
        assert!(!is_stale_binary("arona-rs-1.2.0-linux-x64-compat.tar.gz"));
        assert!(!is_stale_binary("arona-rs-1.2.0-setup-win-x64.exe"));
        assert!(!is_stale_binary("arona-rs-1.2.0-win-x64.zip"));
        // 真正的旧可执行文件：Windows 带 .exe、Linux 不带，两种都得认出来
        assert!(is_stale_binary("arona-rs-1.2.0"));
        assert!(is_stale_binary("arona-rs-1.2.0.exe"));
        // 三段式版本号才认，其它形状原样保留
        assert!(!is_stale_binary("arona-rs-1.2"));
        assert!(!is_stale_binary("arona-rs-notes"));
        // 主程序本体与无关文件永远不动
        assert!(!is_stale_binary(&program_name()));
        assert!(!is_stale_binary("readme.txt"));
    }
}

/// 附带软件 OpenGL 兜底库：仓库根 softgl/ -> target/release/softgl/
///
/// 没有显卡驱动 / 没有 DX12 的服务器靠它用 llvmpipe 把管理面板画出来（见 runtime::softgl）。
/// 返回 false 表示仓库里没有 softgl/（未运行 scripts/fetch-softgl.ps1）。
#[cfg(windows)]
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

/// 附带 DXC 着色器编译器：仓库根 dxc/ -> exe 同级（target/release/）
///
/// wgpu 的 DX12 后端默认用「静态 DXC」，但那需要 VS 的 ATL 组件才能链接；本项目改用
/// 动态加载的 DXC（`Dx12Compiler::DynamicDxc`），运行时从 exe 同级读 dxcompiler.dll/dxil.dll，
/// 所以**随包分发这两个 DLL** 才能保住最快的着色器编译路径（见 gui::wgpu_options）。
/// 返回 false 表示仓库里没有 dxc/（未运行 scripts/fetch-dxc.ps1）——渲染降级链会跳过 DXC 档。
#[cfg(windows)]
fn copy_dxc(root: &Path) -> bool {
    let source = root.join("dxc");
    if !source.is_dir() {
        return false;
    }
    let release_dir = target_release_dir(root);
    let mut copied = 0usize;
    for name in ["dxcompiler.dll", "dxil.dll"] {
        let from = source.join(name);
        if !from.is_file() {
            continue;
        }
        match std::fs::copy(&from, release_dir.join(name)) {
            Ok(_) => copied += 1,
            Err(err) => eprintln!("[dist] 复制 {name} 失败: {err}"),
        }
    }
    if copied == 0 {
        return false;
    }
    println!(
        "[dist] 已附带 DXC 着色器编译器 ({copied} 个 DLL) 到 {}",
        release_dir.display()
    );
    true
}

/// 随包中文字体：installer/fonts/ -> 产物目录 fonts/
///
/// 出图插件画卡片、以及（Windows 上）管理面板显示中文，用的都是这一份思源宋体。
/// 两平台都要带：**没有它，Linux 上的出图插件只能退回各发行版的系统字体候选，
/// 同一句话在不同机器上观感不一致**，这正是当初把它收进框架的理由。
///
/// Windows 的安装包由 Inno Setup 自己释放 `fonts\`，所以那一侧不在这里重复；
/// 便携 zip 与 Linux 包则需要它躺在运行目录里。
/// Windows 的安装包由 Inno Setup 自己释放 `fonts\`，便携 zip 由 CI 直接并入，
/// 所以那一侧不在这里重复；只有 Linux 需要它跟着产物目录走。
#[cfg(unix)]
fn copy_fonts(root: &Path) -> bool {
    let source = root.join("installer").join("fonts");
    let dest = target_release_dir(root).join("fonts");
    if !source.is_dir() {
        return false;
    }
    match copy_dir(&source, &dest) {
        Ok(()) => {
            println!("[dist] 已附带随包中文字体: {}", dest.display());
            true
        }
        Err(err) => {
            eprintln!("[dist] 复制 fonts 失败: {err}");
            false
        }
    }
}

/// 构建任务: 发布构建并整理产物（主程序 arona-rs[.exe] 不带版本号，交付推荐）
///
/// 同一份命令在两个平台上产出各自的布局，随包组件按 `cfg!` 分流：
/// - Windows: arona-rs.exe + softgl/ + dxcompiler.dll + dxil.dll
/// - Linux:   arona-rs（无扩展名）+ fonts/
#[ignore = "构建任务, 运行: cargo dist"]
#[test]
fn dist_build() {
    let root = workspace_root();
    build_release(&root);
    let exe = finalize_binary(&root);
    println!("[dist] 构建产物: {}", exe.display());

    #[cfg(windows)]
    {
        if !copy_softgl(&root) {
            println!(
                "[dist] 未附带软件 OpenGL; 无显卡驱动/无 DX12 的服务器需要先运行 scripts/fetch-softgl.ps1"
            );
        }
        if !copy_dxc(&root) {
            println!(
                "[dist] 未附带 DXC 着色器编译器; 渲染降级链会跳过 DXC 档, 需要先运行 scripts/fetch-dxc.ps1"
            );
        }
    }
    #[cfg(not(windows))]
    {
        // Linux 版不含 GUI：没有渲染后端，softgl/DXC 一律不参与，只需要随包字体
        if !copy_fonts(&root) {
            eprintln!(
                "[dist] 未附带随包中文字体（缺 installer/fonts/）：出图插件会退回系统字体候选"
            );
        }
    }
}

/// 构建任务: 发布构建 + 打包 Windows 安装包（需要 Inno Setup 6，安装包名带版本号）
///
/// ISCC 查找顺序: 环境变量 ARONA_ISCC -> PATH -> 注册表 InstallLocation -> 常见安装目录。
/// 安装包内容与目录规划见 installer/arona-rs.iss。
///
/// 只在 Windows 上有意义：Inno Setup 与 .iss 脚本都是 Windows 专属的。Linux 侧的交付
/// 包由 [`linux_dist`] 负责，两者是同一条流水线的两个平台分支。
#[cfg(windows)]
#[ignore = "构建任务, 运行: cargo installer"]
#[test]
fn installer_build() {
    let root = workspace_root();
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

    let has_dxc = copy_dxc(&root);
    if !has_dxc {
        eprintln!(
            "[installer] 未找到 dxc/，安装包将不含 DXC 着色器编译器；\
             这样管理面板只能用系统 FXC 兜底档，建议先运行 scripts/fetch-dxc.ps1 再重新执行 cargo installer"
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
    if has_dxc {
        command.arg("/DHasDxc=1");
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
#[cfg(windows)]
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
    candidates.push(PathBuf::from(
        r"C:\Program Files (x86)\Inno Setup 6\ISCC.exe",
    ));
    candidates.push(PathBuf::from(r"C:\Program Files\Inno Setup 6\ISCC.exe"));
    candidates.into_iter().find(|path| path.is_file())
}

/// 从注册表卸载项里读 InstallLocation
#[cfg(windows)]
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

// ==================== Linux 交付包 ====================

/// 宽兼容包的目标三元组：给 glibc 钉一个老基线。
///
/// `x86_64-unknown-linux-gnu.2.17` 是 cargo-zigbuild 的写法——目录仍落在基础三元组下，
/// 但链接时按 glibc 2.17 的符号表走。2.17 是 CentOS 7 / Ubuntu 16.04 时代的水位，
/// 钉在这条线上意味着**之后所有发行版都能直接跑**，不必关心构建机装的是哪年的系统。
#[cfg(unix)]
const COMPAT_TARGET: &str = "x86_64-unknown-linux-gnu.2.17";

/// 构建任务: 在 Linux 上打出两个交付包（仅 Linux）
///
///   arona-rs-<版本>-linux-x64.tar.gz         标准包，按构建机的 glibc 基线
///   arona-rs-<版本>-linux-x64-compat.tar.gz  宽兼容包，glibc 2.17 基线，几乎任何发行版都能跑
///
/// ## 为什么是两个包，且都不做「静态」
///
/// 静态一度是首选，但它和本项目的基本前提冲突，实测确认了两条死路：
///
/// 1. **musl 静态二进制 `dlopen` 不了任何东西**——musl 给静态构建留的桩就是
///    直接返回 "Dynamic loading not supported"。而框架的全部功能都来自运行期装载的插件，
///    一个装不了插件的宿主等于什么命令都不响应。
/// 2. **rustc 不支持 musl 目标的 `cdylib`**（构建时会警告
///    `dropping unsupported crate type`），插件根本编不出 `.so`，等于把插件作者也堵死。
///
/// gnu 的全静态同样走不通：zig 明确拒绝（"libc of the specified target requires
/// dynamic linking"）。所以两个包都基于 glibc 动态链接——**能装插件是硬前提**。
///
/// 两个包的差别落在 glibc 基线上：标准包跟着构建机走，宽兼容包钉 2.17。
/// 这覆盖了两类真实场景——追新发行版的人拿到的是原生形态，而在老系统 / 各种容器里
/// 部署的人拿 compat 包即可，不必为了 glibc 版本去升级整个系统。
///
/// 兼容包需要 zig + cargo-zigbuild。没有时**不报错**：标准包照常产出，兼容包跳过并说明
/// 缺什么——构建脚本不该因为少一个可选工具链就让整条流水线红掉。
#[cfg(unix)]
#[ignore = "构建任务, 运行: cargo linux-dist"]
#[test]
fn linux_dist() {
    let root = workspace_root();
    let version = env!("CARGO_PKG_VERSION");
    println!("[linux-dist] 开始打包 (arona-rs {version})");

    // ---- 1) 标准包：当前主机 target（Linux 上就是 gnu） ----
    build_release(&root);
    let exe = finalize_binary(&root);
    let standard = pack_linux(&root, &exe, version, None);
    println!("[linux-dist] 标准包: {}", standard.display());

    // ---- 2) 宽兼容包：glibc 2.17 基线 ----
    if !compat_ready() {
        eprintln!(
            "[linux-dist] 跳过宽兼容包：需要 zig + cargo-zigbuild。\n\
             \x20   cargo install cargo-zigbuild   并装 zig（https://ziglang.org/download）\n\
             \x20   （只想发标准包时设 ARONA_SKIP_COMPAT=1 直接跳过）"
        );
        return;
    }
    let compat_exe = build_release_for(&root, Some(COMPAT_TARGET));
    let compat = pack_linux(&root, &compat_exe, version, Some("compat"));
    println!("[linux-dist] 宽兼容包: {}", compat.display());
}

/// 宽兼容包能不能编：只认 zig + cargo-zigbuild 这条路（`glibc.2.17` 是它们提供的）。
///
/// 目标三元组本身不需要 `rustup target add`——`x86_64-unknown-linux-gnu.2.17` 用的还是
/// 基础 gnu target 的 std，版本后缀只在链接阶段生效。
///
/// `ARONA_SKIP_COMPAT=1` 可以明确跳过（只想发标准包时用）。
#[cfg(unix)]
fn compat_ready() -> bool {
    if env_flag("ARONA_SKIP_COMPAT") {
        println!("[linux-dist] ARONA_SKIP_COMPAT 已设置，不构建宽兼容包");
        return false;
    }
    let zig = in_path("zig");
    if !zig {
        eprintln!("[linux-dist] 没找到 zig（https://ziglang.org/download）");
    }
    let zigbuild = in_path("cargo-zigbuild");
    if !zigbuild {
        eprintln!("[linux-dist] 没找到 cargo-zigbuild（cargo install cargo-zigbuild）");
    }
    zig && zigbuild
}

/// 可执行文件在不在 PATH 里
#[cfg(unix)]
fn in_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            dir.join(name).is_file() || dir.join(format!("{name}.exe")).is_file()
        })
    })
}

/// 环境变量开关（`1`/`true`/`yes`/`on`）
#[cfg(unix)]
fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

/// 把一个可执行文件 + 随包字体打成 tar.gz 交付包，返回包路径。
///
/// 包内布局就是运行目录本身，用户解压后直接 `./arona-rs`，配置与数据会在当前工作目录
/// 下自动生成（见 `runtime::paths`）：
///
/// ```text
/// arona-rs-1.2.0-linux-x64/
///   arona-rs          可执行
///   fonts/            随包中文字体（出图插件用）
///   plugins/          空目录：把插件 .so 放这里
///   README.txt        怎么跑、去哪放插件
/// ```
#[cfg(unix)]
fn pack_linux(root: &Path, exe: &Path, version: &str, flavor: Option<&str>) -> PathBuf {
    let suffix = match flavor {
        Some(flavor) => format!("-{flavor}"),
        None => String::new(),
    };
    let package = format!("arona-rs-{version}-linux-x64{suffix}");
    let staging = target_release_dir(root).join("package").join(&package);

    // 每次重新铺：旧的残留会让包里混进上一次的文件
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join("plugins"))
        .unwrap_or_else(|err| panic!("创建 {} 失败: {err}", staging.display()));

    let target_exe = staging.join(program_name());
    std::fs::copy(exe, &target_exe)
        .unwrap_or_else(|err| panic!("复制 {} 到暂存目录失败: {err}", exe.display()));
    set_executable(&target_exe);

    // 字体：与 Windows 便携版同一份（出图插件在 Linux 上同样要用它画字）
    let fonts_src = root.join("installer").join("fonts");
    if fonts_src.is_dir() {
        copy_dir(&fonts_src, &staging.join("fonts"))
            .unwrap_or_else(|err| panic!("复制 fonts 失败: {err}"));
    } else {
        eprintln!("[linux-dist] 缺少 installer/fonts/，包内不含随包中文字体");
    }

    std::fs::write(staging.join("README.txt"), linux_readme(version, flavor))
        .unwrap_or_else(|err| panic!("写 README.txt 失败: {err}"));

    // tar -czf <包> -C <package 父目录> <package 目录>
    // 用系统的 tar 而不是自造归档：它是每个 Linux 都有的，也省得为打包引入依赖
    let archive = target_release_dir(root).join(format!("{package}.tar.gz"));
    let _ = std::fs::remove_file(&archive);
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(staging.parent().expect("暂存目录一定有父目录"))
        .arg(&package)
        .status()
        .expect("启动 tar 失败（Linux 上应当总是存在）");
    assert!(status.success(), "tar 打包失败");

    let size = std::fs::metadata(&archive).map(|meta| meta.len()).unwrap_or(0);
    println!(
        "[linux-dist] {package}.tar.gz ({:.1} MiB)",
        size as f64 / 1024.0 / 1024.0
    );
    archive
}

/// 给文件加可执行位（0755）。`std::fs::copy` 会连权限一起复制，但经过暂存目录与 CI
/// 缓存之后不该指望它，这里显式设一次。
#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}

/// 包内 README：说清楚怎么跑、插件放哪、两个包有什么区别
#[cfg(unix)]
fn linux_readme(version: &str, flavor: Option<&str>) -> String {
    let kind = match flavor {
        Some(_) => {
            "宽兼容包：按 glibc 2.17 基线编译。\n\
             CentOS 7 / Ubuntu 16.04 及之后的所有发行版都能直接跑，\
             不必关心是谁在哪台机器上编的。\n\
             拿不准选哪个就用这个。"
        }
        None => {
            "标准包：按构建机的 glibc 基线编译，是发行版的原生形态。\n\
             目标机的 glibc 不能比构建机旧；报 `GLIBC_2.xx not found` 就换宽兼容包。"
        }
    };
    format!(
        "Arona-rs {version} — Linux x64\n\
         \n\
         {kind}\n\
         \n\
         运行：\n\
         \x20   ./arona-rs           只启动命令行模式（Linux 版不提供管理面板）\n\
         \x20   ./arona-rs --config=路径/onebot.yml\n\
         \n\
         首次运行会在**当前工作目录**下生成 config/ data/ logs/ plugins/，\n\
         编辑 config/onebot.yml 填机器人账号与连接方式，config/arona.yml 填服务群与管理员。\n\
         \n\
         功能插件是独立的 .so，编译后放进 plugins/ 即可，框架启动时扫描装载。\n\
         插件必须与框架用同一套工具链构建（rustc 版本、target、profile、链接方式一致），\n\
         详见 PLUGIN_DEVELOPMENT.md。\n\
         \n\
         依赖：只需要系统自带的 glibc。启动时框架会把发行版与内核写进日志，\n\
         并检查关键动态库；缺了会直接打印各发行版的安装命令，照抄一条即可。\n\
         \n\
         按 Ctrl+C 停止。\n\
         \n\
         许可证：GNU AGPLv3。\n"
    )
}
