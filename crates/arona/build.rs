//! 构建脚本：把"编译本 crate 用的是哪套配置"烙进二进制，供动态插件的 ABI 握手比对。
//!
//! Rust 没有稳定的跨编译器 ABI：`dyn Trait` 的胖指针布局、泛型单态化产物、panic
//! 与内存分配策略都随 rustc 版本与 profile 变化。本 crate 允许功能插件以 `cdylib`
//! 形式在运行期加载并直接交换 `Arc<dyn AronaPlugin>`，成立的前提是**宿主与插件由
//! 同一版 rustc、同一 target、同一 profile、同一 CRT 链接方式、同一份 arona 源码、
//! 同一组 feature** 编译。
//!
//! 这个前提没法靠编译器保证，就在这里算成指纹，由 `plugin::abi` 在装载每个 dll 时逐个核对，
//! 对不上就拒载（宁可当场报错，也不要"加载成功但随机崩溃"）。
//!
//! 两个例外是 `default` 与 `gui`：`default` 只是 feature 别名（本 crate 里它展开成 `gui`），
//! `gui` 只门控 `arona::gui` 与两个 GUI 启动辅助模块——两者都不改变任何共享类型的布局。
//! 不放行的话，`cargo build --workspace`（成员 arona 按 default 编）与插件作者用的
//! `cargo build -p <插件>` 会算出不同指纹，同一个仓库里编出来的 dll 都装不进自己的宿主。
//! `plugin::abi::tests` 里有守门用例盯着 `gui` 这条前提。

use std::process::Command;

/// 不参与 ABI 指纹的 feature（别名或只影响框架自身入口，不改变共享类型布局）
const ABI_NEUTRAL_FEATURES: &[&str] = &["gui", "default"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTC");

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown-target".to_string());
    let profile = if std::env::var("PROFILE").as_deref() == Ok("release") {
        "release"
    } else {
        "debug"
    };
    // CARGO_CFG_TARGET_FEATURE 是逗号分隔的列表，crt-static 只在配置里显式打开时出现
    let crt_static = std::env::var("CARGO_CFG_TARGET_FEATURE")
        .map(|features| features.split(',').any(|item| item == "crt-static"))
        .unwrap_or(false);

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let verbose = Command::new(&rustc)
        .arg("-vV")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_default();

    // `rustc -vV` 里 release/host 两行足以锁定编译器身份（含 commit hash）
    let release = field(&verbose, "release").unwrap_or("unknown");
    let host = field(&verbose, "host").unwrap_or("unknown");

    println!(
        "cargo:rustc-env=ARONA_ABI_FINGERPRINT=arona/{}|rustc/{}|host/{}|target/{}|profile/{}|crt-static/{}|features/{}",
        std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string()),
        release,
        host,
        target,
        profile,
        u8::from(crt_static),
        enabled_features(),
    );
}

/// 把本 crate 启用的 Cargo feature 排成稳定顺序拼成一串（feature 集合会改变代码与布局）
fn enabled_features() -> String {
    let mut names: Vec<String> = std::env::vars()
        .filter_map(|(key, value)| {
            let feature = key.strip_prefix("CARGO_FEATURE_")?;
            let name = feature.to_lowercase().replace('_', "-");
            (value == "1" && !ABI_NEUTRAL_FEATURES.contains(&name.as_str())).then_some(name)
        })
        .collect();
    names.sort();
    names.join(",")
}

/// 从 `rustc -vV` 的输出里取 `key: value` 一行（值里的空格保留，换行去掉）
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim())
    })
}
