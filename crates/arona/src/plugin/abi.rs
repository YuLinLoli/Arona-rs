//! 动态插件的 ABI 约定（对应 mirai 的 `plugin.yml` + `@FilePluginMain` 入口）。
//!
//! ## 为什么"直接交换 Rust trait 对象"是可行的
//!
//! 一个 `cdylib` 插件会把自己那份 `arona` 静态链接进去，于是宿主与插件里各有一套
//! 全局静态量（`Framework::global()`、日志 sink、任务表）。若插件用裸 `tokio::spawn`
//! 或自行 `arona::plugin::manager()`，写的就是 dll 自己那份表，宿主看不见——静默失效。
//! 因此本框架的契约是：装载 dll 后，宿主第一时间把自己的**进程默认实例、日志出口、
//! tokio 句柄**通过 [`HostBridge`] 交回插件（[`export_arona_plugin!`] 生成的
//! `arona_plugin_host` 符号），插件里那一份静态状态当场改指宿主。此后插件无论走
//! [`PluginContext`](super::PluginContext) 还是走 `arona::quartz::*` 这类自由函数，
//! 读写的都是宿主真正在跑的那套表——跨边界传递的 `Arc<dyn AronaPlugin>`
//! 因此只在"同一版 rustc + 同一 target + 同一 profile + 同一 CRT + 同一份 arona 源码"
//! 的前提下才成立。
//!
//! 这个前提编译器管不了，就由 [`abi`](self) 在装载每个 dll 时逐项核对工具链指纹，
//! 对不上直接拒载。请把它当作硬性发布要求：**发布插件时必须连带说明它编译所用的
//! rustc 版本，且必须用 `--release` 与宿主同档构建**（详见 `PLUGIN_DEVELOPMENT.md`）。
//!
//! ## dll 必须导出的四个符号
//!
//! | 符号 | 签名 | 作用 |
//! | --- | --- | --- |
//! | `arona_plugin_abi` | `extern "C" fn() -> u64` | 符号布局版本 + 编译期框架契约版本 |
//! | `arona_plugin_toolchain` | `extern "C" fn() -> *const u8` | 以 NUL 结尾的工具链指纹 |
//! | `arona_plugin_host` | `extern "C" fn(*const HostBridge) -> u8` | 接受宿主的上行通道，0 才算成功 |
//! | `arona_plugin_new` | `extern "C" fn() -> *const dyn AronaPlugin` | 造实例，交出唯一一份强引用 |
//!
//! 插件作者不需要手写它们，一行 [`export_arona_plugin!`] 就够。

use crate::plugin::ApiVersion;
use crate::plugin::description::FRAMEWORK_API_VERSION;
use std::ffi::c_void;
use std::os::raw::c_char;

/// 本模块约定的符号布局版本。签名有任何改动都要抬它，宿主只接受相等的 dll。
pub const ABI_LAYOUT: u32 = 2;

/// 装载器名（写进 `plugins/<id>/plugin.yml` 的 `loader:` 与 GUI 插件列表）
pub const DYNAMIC_LOADER: &str = "dynamic";

/// 握手符号名
pub const ABI_SYMBOL: &str = "arona_plugin_abi";
/// 工具链指纹符号名
pub const TOOLCHAIN_SYMBOL: &str = "arona_plugin_toolchain";
/// 上行通道交接符号名
pub const HOST_SYMBOL: &str = "arona_plugin_host";
/// 实例入口符号名
pub const ENTRY_SYMBOL: &str = "arona_plugin_new";

/// [`attach_host`] 的返回值：接管完成
pub const HOST_ACCEPTED: u8 = 0;

/// 编译本 crate 所用工具链的指纹，由 `crates/arona/build.rs` 注入。
/// 插件通过 [`export_arona_plugin!`] 原样上报，宿主按字符串全等比对。
pub const TOOLCHAIN_FINGERPRINT: &str = env!("ARONA_ABI_FINGERPRINT");

/// [`TOOLCHAIN_FINGERPRINT`] 的 C 字符串版本：末尾多一个 NUL。
///
/// dll 导出的 `arona_plugin_toolchain` 交出去的是裸指针，宿主只能按 C 字符串读到 NUL 为止。
/// Rust 的字符串字面量**不带**结尾 NUL，直接 `.as_ptr()` 会让宿主读过字符串末尾继续找 0——
/// 撞上 0 就算"指纹不同"，撞上非 UTF-8 字节就报编码错误，运气差则踩进未映射内存。
/// 所以对外一律用这一份。
pub const TOOLCHAIN_FINGERPRINT_C: &str = concat!(env!("ARONA_ABI_FINGERPRINT"), "\0");

/// 把布局版本与框架契约版本打包成一个 `u64`：高 32 位是 [`ABI_LAYOUT`]，
/// 低 32 位是 `major << 16 | minor`（patch 不参与握手，与 mirai 的 `ApiVersion` 一致）。
pub const fn pack_abi(api: &ApiVersion) -> u64 {
    ((ABI_LAYOUT as u64) << 32) | ((api.major as u64) << 16) | api.minor as u64
}

/// 解出 [`pack_abi`] 里的布局版本
pub const fn unpack_layout(packed: u64) -> u32 {
    (packed >> 32) as u32
}

/// 解出 [`pack_abi`] 里的框架契约版本（patch 恒为 0，握手不看它）
pub const fn unpack_api(packed: u64) -> ApiVersion {
    ApiVersion::new(
        ((packed >> 16) & 0xFFFF) as u32,
        (packed & 0xFFFF) as u32,
        0,
    )
}

/// 插件 dll 上报的握手信息是否被当前框架接受。
/// 返回 `Ok(())` 只代表"可以试着造实例"，业务级的依赖检查仍归 `PluginManager`。
pub fn check_abi(packed: u64, toolchain: &str) -> Result<(), String> {
    let layout = unpack_layout(packed);
    if layout != ABI_LAYOUT {
        return Err(format!(
            "插件 ABI 布局版本不匹配：插件为 {layout}，本框架为 {ABI_LAYOUT}。请用同版本框架重新编译插件"
        ));
    }
    let built = unpack_api(packed);
    if !FRAMEWORK_API_VERSION.satisfies(&built) {
        return Err(format!(
            "插件按框架契约 {built} 编译，当前框架只有 {FRAMEWORK_API_VERSION}"
        ));
    }
    if toolchain != TOOLCHAIN_FINGERPRINT {
        return Err(format!(
            "插件与框架的构建工具链不一致，无法安全装载。\n  框架: {TOOLCHAIN_FINGERPRINT}\n  插件: {toolchain}\n\
             插件必须与宿主使用同一个 rustc、同一个目标三元组、同一档 profile（release/debug）、\n  同一 CRT 链接方式以及同一版本的 arona 依赖编译。"
        ));
    }
    Ok(())
}

/// 宿主交给插件的**上行通道**：插件 dll 里静态链接的那份框架状态是空的，
/// 装载时把宿主真正在跑的三样东西接过来，插件里的自由函数才不落空表。
///
/// 字段顺序与签名就是 ABI 的一部分，改动必须抬 [`ABI_LAYOUT`]。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostBridge {
    /// 把插件打的一条日志交给宿主落地：`level` 是 `INFO`/`DEBUG`/`WARNING`/`ERROR`，
    /// `message` 是不带来源的正文，`source` 是 dll 这一侧的线程局部来源（可为空指针）。
    /// 三者都是 NUL 结尾 UTF-8；来源以宿主为准，宿主没标注时才用它兜底。
    pub log: extern "C" fn(level: *const c_char, message: *const c_char, source: *const c_char),
    /// 宿主的进程默认实例（`*const Framework`，擦成 `c_void`）
    pub framework: extern "C" fn() -> *const c_void,
    /// 宿主登记的 tokio 句柄（`*const Handle`），没有则返回空指针
    pub runtime: extern "C" fn() -> *const c_void,
}

/// [`export_arona_plugin!`] 生成的 `arona_plugin_host` 的实现：把插件里的
/// 进程默认实例、日志出口、tokio 句柄一次性改指宿主。必须在造实例之前调用。
///
/// 返回 0 表示接管完成；非 0 表示失败（宿主据此拒载），因为「各持一份状态」的插件
/// 只是安静地失效，比装载失败更难查。
///
/// # Safety
/// `bridge` 必须是宿主给出、且在进程结束前保持有效的 [`HostBridge`]；
/// 其中的 `framework` 指针指向的实例必须永不被释放。
pub unsafe fn attach_host(bridge: *const HostBridge) -> u8 {
    if bridge.is_null() {
        return 1;
    }
    let bridge = unsafe { *bridge };
    // SAFETY: 约定 `framework` 指向宿主永不被释放的默认实例，`runtime` 指向它登记的句柄
    // （或空指针表示宿主还没有句柄），插件这份代码只在此后借用它们。
    unsafe {
        if !crate::framework::Framework::adopt_host((bridge.framework)()) {
            return 2;
        }
        crate::runtime::reactor::adopt_host((bridge.runtime)());
    }
    crate::runtime::log::attach_host(bridge);
    HOST_ACCEPTED
}

/// 导出动态插件入口：在 `crate-type = ["cdylib"]` 的插件 crate 里写一行即可。
///
/// ```ignore
/// arona::export_arona_plugin!(MyPlugin::default());
/// ```
///
/// 参数是构造插件实例的表达式（每次装载只求值一次，由宿主接管其强引用）。
#[macro_export]
macro_rules! export_arona_plugin {
    ($instance:expr) => {
        $crate::export_arona_plugin!(@impl $crate::plugin::abi::private_arc($instance));
    };
    (@impl $ctor:expr) => {
        /// 符号布局版本 + 本 dll 编译时所链接的框架契约版本
        #[unsafe(no_mangle)]
        pub extern "C" fn arona_plugin_abi() -> u64 {
            $crate::plugin::abi::pack_abi(&$crate::plugin::FRAMEWORK_API_VERSION)
        }

        /// 本 dll 的构建工具链指纹（以 NUL 结尾，宿主按字符串全等校验）
        #[unsafe(no_mangle)]
        pub extern "C" fn arona_plugin_toolchain() -> *const u8 {
            $crate::plugin::abi::TOOLCHAIN_FINGERPRINT_C.as_ptr()
        }

        /// 接受宿主的上行通道：把本 dll 里的进程默认实例、日志出口、tokio 句柄改指宿主。
        /// 框架在造实例之前调用它，返回非 0 即拒载。
        ///
        /// # Safety
        /// 由框架装载器调用，`bridge` 指向一个进程内长期有效的
        /// [`HostBridge`](::arona::plugin::abi::HostBridge)。
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn arona_plugin_host(
            bridge: *const $crate::plugin::abi::HostBridge,
        ) -> u8 {
            // SAFETY: 框架只在握手通过后调用，并保证指针指向有效的 HostBridge
            unsafe { $crate::plugin::abi::attach_host(bridge) }
        }

        /// 造出插件实例，交出唯一一份 `Arc`（宿主用 `Arc::from_raw` 接管）
        #[unsafe(no_mangle)]
        pub extern "C" fn arona_plugin_new() -> *const dyn $crate::plugin::AronaPlugin {
            let instance: ::std::sync::Arc<dyn $crate::plugin::AronaPlugin> = $ctor;
            ::std::sync::Arc::into_raw(instance)
        }
    };
}

/// 宏内部用：把构造表达式先绑成 `Arc<dyn AronaPlugin>`，避免各插件漏写类型标注。
pub fn private_arc<P>(plugin: P) -> std::sync::Arc<dyn crate::plugin::AronaPlugin>
where
    P: crate::plugin::AronaPlugin,
{
    std::sync::Arc::new(plugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HostBridge` 要跨 dll 边界传给插件，字段数量与顺序本身就是 ABI 的一部分：
    /// 布局漂了（多一个字段、忘了 `repr(C)`）宿主就会按错位的方式取函数指针。
    #[test]
    fn host_bridge_is_exactly_three_function_pointers() {
        use std::mem::{align_of, size_of};
        assert_eq!(size_of::<HostBridge>(), 3 * size_of::<usize>());
        assert_eq!(align_of::<HostBridge>(), align_of::<usize>());
    }

    /// 空通道必须被拒收：接管失败还继续装配，插件就会把状态写进自己那份空表里，
    /// 表现出来是"命令登记了但没人路由"这种查不动的静默故障。
    #[test]
    fn a_null_bridge_is_refused() {
        assert_eq!(unsafe { attach_host(std::ptr::null()) }, 1);
    }

    /// 扮一次"插件 dll"：把宿主自己给的上行通道接进来之后，插件视角下的自由函数
    /// （`quartz::create_daily` / `quartz::exists`）必须落进宿主真正在跑的那张任务表。
    /// 这条是"各持一份状态会安静失效"的唯一防线，改坏了插件就只剩静默失效。
    #[test]
    fn adopting_the_host_redirects_the_free_functions() {
        let bridge = crate::runtime::log::host_bridge();
        assert_eq!(
            unsafe { attach_host(bridge) },
            HOST_ACCEPTED,
            "宿主自己给的通道必须接得下"
        );
        let job: crate::quartz::JobFn = std::sync::Arc::new(|| {});
        crate::quartz::create_daily(4, "AbiAdoptProbe", "abi-adopt", job);
        assert!(
            crate::framework::Framework::global()
                .jobs()
                .exists("AbiAdoptProbe"),
            "自由函数登记的任务应落在宿主的任务表里"
        );
        assert!(crate::quartz::exists("AbiAdoptProbe"));
        assert!(crate::quartz::remove("AbiAdoptProbe"));
    }

    #[test]
    fn abi_roundtrip_keeps_layout_and_api() {
        let packed = pack_abi(&ApiVersion::new(1, 7, 3));
        assert_eq!(unpack_layout(packed), ABI_LAYOUT);
        // patch 不参与握手
        assert_eq!(unpack_api(packed), ApiVersion::new(1, 7, 0));
    }

    /// 交给宿主的是裸指针，只能按 C 字符串读：漏了结尾 NUL，宿主会读过指纹继续找 0，
    /// 报出来的原因变成看不懂的「工具链不一致」，运气差还会扫进未映射的内存。
    #[test]
    fn reported_fingerprint_is_a_c_string() {
        let bytes = TOOLCHAIN_FINGERPRINT_C.as_bytes();
        assert_eq!(bytes.last(), Some(&0), "导出的指纹必须带 NUL 结尾");
        let read_back = std::ffi::CStr::from_bytes_with_nul(bytes)
            .expect("指纹中间不该有 NUL")
            .to_str()
            .expect("指纹是 UTF-8");
        assert_eq!(read_back, TOOLCHAIN_FINGERPRINT);
    }

    #[test]
    fn fingerprint_carries_the_whole_toolchain() {
        // 指纹缺一环都会让"能编译但会随机崩"的插件混进来
        for token in [
            "rustc/",
            "host/",
            "target/",
            "profile/",
            "crt-static/",
            "features/",
        ] {
            assert!(
                TOOLCHAIN_FINGERPRINT.contains(token),
                "工具链指纹缺少 {token}: {TOOLCHAIN_FINGERPRINT}"
            );
        }
    }

    /// `gui` 是唯一被放行、不进指纹的 feature：它必须只门控框架自己的界面与启动辅助，
    /// 绝不能出现在插件契约面上——否则插件与宿主的类型布局就不一致了。
    #[test]
    fn gui_feature_stays_out_of_the_plugin_facing_types() {
        use std::path::Path;

        // 允许带这个 cfg 的文件：模块声明处与 GUI 自身
        const ALLOWED: &[&str] = &["src/lib.rs", "src/runtime/mod.rs", "src/gui"];
        // 拆开写，免得本文件把自己的源码匹配成命中项
        const NEEDLE: &str = concat!("feature = \"gu", "i\"");
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut stack = vec![manifest.join("src")];
        let mut offenders = Vec::new();
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(relative) = path.strip_prefix(manifest) else {
                    continue;
                };
                if ALLOWED.iter().any(|ok| relative.starts_with(ok)) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if text.contains(NEEDLE) {
                    offenders.push(relative.to_path_buf());
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "这些文件按 gui 门控了代码，插件与宿主的类型布局可能已不一致: {offenders:?}"
        );
    }

    #[test]
    fn check_abi_rejects_each_mismatch() {
        let mine = TOOLCHAIN_FINGERPRINT.to_string();
        assert!(check_abi(pack_abi(&FRAMEWORK_API_VERSION), &mine).is_ok());
        assert!(
            check_abi(pack_abi(&FRAMEWORK_API_VERSION), "arona/0.0.0|rustc/9.9.9")
                .is_err_and(|reason| reason.contains("工具链不一致"))
        );
        let newer = ApiVersion::new(
            FRAMEWORK_API_VERSION.major,
            FRAMEWORK_API_VERSION.minor + 1,
            0,
        );
        assert!(
            check_abi(pack_abi(&newer), &mine)
                .is_err_and(|reason| reason.contains("插件按框架契约"))
        );
        assert!(
            check_abi((ABI_LAYOUT as u64 + 7) << 32, &mine)
                .is_err_and(|reason| reason.contains("ABI 布局版本"))
        );
    }
}
