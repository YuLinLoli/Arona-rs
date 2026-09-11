//! 构建任务：`cargo dist` —— 发布构建并把产物重命名为 `arona-rs-<版本号>[.exe]`。
//!
//! Cargo 的 bin/crate 名不允许包含 `.`（版本号形如 `0.2.0`），因此无法用 `[[bin]] name`
//! 直接把版本号写进产物名，这里在构建完成后复制一份带版本号的副本。
//!
//! 运行: cargo dist
//! （等价于 cargo test dist_build --release -- --ignored --nocapture --test-threads=1）

use std::path::PathBuf;
use std::process::Command;

/// 构建发布版并生成带版本号的产物副本
#[ignore = "构建任务, 运行: cargo dist"]
#[test]
fn dist_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let version = env!("CARGO_PKG_VERSION");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    println!("[dist] 开始发布构建 (arona-rs {version})");
    let status = Command::new(&cargo)
        .args(["build", "--release"])
        .current_dir(&root)
        .status()
        .expect("启动 cargo build --release 失败");
    assert!(status.success(), "cargo build --release 失败");

    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let suffix = std::env::consts::EXE_SUFFIX;
    let release_dir = target_dir.join("release");
    let source = release_dir.join(format!("arona-rs{suffix}"));
    let dest = release_dir.join(format!("arona-rs-{version}{suffix}"));
    std::fs::copy(&source, &dest).unwrap_or_else(|err| {
        panic!(
            "复制构建产物失败 {} -> {}: {err}",
            source.display(),
            dest.display()
        )
    });
    println!("[dist] 构建产物: {}", dest.display());
}