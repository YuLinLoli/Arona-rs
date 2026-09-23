//! 动态插件装载器：启动时扫描 `plugins/` 目录，把 dll 里的插件实例接管进框架。
//!
//! 对齐 mirai 的 `JvmPluginManager`：用户在**启动前**把插件包丢进 `plugins/`，
//! 启动时框架自动发现、握手、登记，GUI「插件管理」页随之可见可开关——不需要重新编译宿主。
//!
//! 与 mirai 的差别在加载方式：mirai 用独立 ClassLoader 隔离 JVM 类，这里用
//! `LoadLibrary` 把 dll 映射进宿主进程，插件那份 `arona` 是**静态链接进 dll 的第二份**。
//! 于是握手比 mirai 严格得多——除了契约版本，还要逐项比对
//! [`abi::TOOLCHAIN_FINGERPRINT`](super::abi::TOOLCHAIN_FINGERPRINT)
//! （rustc / target / profile / CRT / arona 版本与 feature），任何一项不一致都拒载。
//! 握手通过后立刻把宿主的 [`HostBridge`](super::abi::HostBridge) 交进 dll：
//! 插件里的进程默认实例、日志出口与 tokio 句柄当场改指宿主，
//! 它写的每一张表才是宿主真正在跑的那一张（详见 [`abi`](super::abi) 模块头）。
//!
//! 装载失败只影响单个文件：一个 dll 崩在握手阶段，其余照常装配。
//!
//! dll 句柄在握手成功后被 [`std::mem::forget`] 掉——插件实例的虚表指向该模块的代码段，
//! 一旦 `Library` 析构就会 `FreeLibrary` 卸载模块，之后任何一次虚调用都是访问野指针。
//! 框架不提供"运行期卸载插件"，停用只做资源回收，所以常驻不释放是安全的。

use crate::plugin::AronaPlugin;
use crate::plugin::abi::{
    ABI_SYMBOL, DYNAMIC_LOADER, ENTRY_SYMBOL, HOST_ACCEPTED, HOST_SYMBOL, HostBridge,
    TOOLCHAIN_SYMBOL, check_abi, unpack_api, unpack_layout,
};
use crate::plugin::manager::PluginLoader;
use crate::runtime::log;
use crate::runtime::paths;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 本装载器的名字（`plugins/<id>/plugin.yml` 的 `loader:` 字段与 GUI 列表用）
pub const LOADER_NAME: &str = DYNAMIC_LOADER;

/// 平台动态库扩展名
pub fn library_extension() -> &'static str {
    if cfg!(windows) {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

/// 扫描 `plugins/` 得到的候选 dll：目录直属的那批，加上每个一级子目录里的那批。
/// 推荐布局是每个插件独占一个以 id 命名的子目录（可以同时放资源文件），
/// 但直接把 `xxx.dll` 丢在 `plugins/` 下也能加载。
pub fn discover(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_libraries(dir, &mut found);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_libraries(&entry.path(), &mut found);
        }
    }
    found.sort();
    found
}

fn collect_libraries(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && is_plugin_library(&path) {
            out.push(path);
        }
    }
}

fn is_plugin_library(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    // `__.dll` 之类是插件依赖的第三方运行库，不该被当成插件本身
    !name.starts_with('_')
        && path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(library_extension()))
}

/// 扫描 `plugins/` 目录的装载器
pub struct DynamicPluginLoader;

impl PluginLoader for DynamicPluginLoader {
    fn name(&self) -> &'static str {
        DYNAMIC_LOADER
    }

    fn load(&self) -> Vec<Arc<dyn AronaPlugin>> {
        let dir = paths::plugins_dir();
        let candidates = discover(&dir);
        if candidates.is_empty() {
            log::info(format!(
                "plugins 目录下没有功能插件，本次启动只提供框架本体（想加功能就把插件 dll 放进 {}）",
                dir.display()
            ));
            return Vec::new();
        }
        let mut loaded = Vec::new();
        for path in candidates {
            match unsafe { instantiate(&path) } {
                Ok(plugin) => {
                    let meta = plugin.meta();
                    log::info(format!(
                        "已装载动态插件: {} {} ({})",
                        meta.name,
                        meta.version,
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    loaded.push(plugin);
                }
                Err(reason) => log::error(format!(
                    "跳过插件 {} —— {reason}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }
        loaded
    }
}

/// 加载一个 dll 并取出插件实例。失败原因写成给用户看的话。
///
/// # Safety
/// 调用方必须保证 `path` 指向可信的本地文件：dll 里的代码以宿主同等权限运行，
/// 且握手只校验工具链一致性、不校验恶意行为。
pub unsafe fn instantiate(path: &Path) -> Result<Arc<dyn AronaPlugin>, String> {
    let library =
        unsafe { libloading::Library::new(path) }.map_err(|err| format!("无法加载模块: {err}"))?;
    unsafe { handshake(&library)? };
    unsafe { install_host_bridge(&library)? };
    let entry: libloading::Symbol<'_, unsafe extern "C" fn() -> *const dyn AronaPlugin> =
        unsafe { symbol(&library, ENTRY_SYMBOL)? };
    let instance = unsafe { entry() };
    if instance.is_null() {
        return Err("插件入口返回了空实例".to_string());
    }
    // 虚表与代码段随模块常驻，句柄不能析构
    std::mem::forget(library);
    // SAFETY: 握手已通过，`arona_plugin_new` 约定交出引用计数为 1 的 `Arc`，
    // 且与宿主同工具链编译，胖指针布局一致。
    Ok(unsafe { Arc::from_raw(instance) })
}

/// 取符号并转成目标函数指针类型
unsafe fn symbol<'lib, F: 'lib>(
    library: &'lib libloading::Library,
    name: &'static str,
) -> Result<libloading::Symbol<'lib, F>, String> {
    unsafe { library.get::<F>(name.as_bytes()) }
        .map_err(|_| format!("缺少导出符号 `{name}`，不是 Arona 插件或版本过旧"))
}

/// 把宿主的上行通道交进 dll：插件里那份框架状态（进程默认实例、日志出口、tokio 句柄）
/// 当场改指宿主。必须在取实例之前完成——插件构造函数里就可能碰这些表。
///
/// # Safety
/// 同 [`instantiate`]。
pub unsafe fn install_host_bridge(library: &libloading::Library) -> Result<(), String> {
    let host: libloading::Symbol<'_, unsafe extern "C" fn(*const HostBridge) -> u8> =
        unsafe { symbol(library, HOST_SYMBOL)? };
    let bridge = std::ptr::from_ref(log::host_bridge());
    let code = unsafe { host(bridge) };
    if code == HOST_ACCEPTED {
        return Ok(());
    }
    Err(format!(
        "插件没能接管宿主的上行通道（代码 {code}）：1=通道为空，2=它已经先建了自己的默认实例。\
         请用同版本框架重新编译插件"
    ))
}

/// 只跑握手不取实例：给自检与测试用
///
/// # Safety
/// 同 [`instantiate`]。
pub unsafe fn handshake(library: &libloading::Library) -> Result<(), String> {
    let abi: libloading::Symbol<'_, unsafe extern "C" fn() -> u64> =
        unsafe { symbol(library, ABI_SYMBOL)? };
    let toolchain: libloading::Symbol<'_, unsafe extern "C" fn() -> *const u8> =
        unsafe { symbol(library, TOOLCHAIN_SYMBOL)? };
    let packed = unsafe { abi() };
    let pointer = unsafe { toolchain() };
    if pointer.is_null() {
        return Err("插件未上报构建工具链".to_string());
    }
    // SAFETY: 上面已验过指针非空，且它来自刚映射进本进程的模块
    let reported = unsafe { read_c_string(pointer) }?;
    check_abi(packed, &reported)?;
    log::debug(format!(
        "插件 ABI 握手通过: 布局 {}，契约 {}",
        unpack_layout(packed),
        unpack_api(packed)
    ));
    Ok(())
}

/// 插件上报的指纹允许的最长字节数：正常一两百字节，留足余量又能挡住没有结尾的指针
const FINGERPRINT_LIMIT: usize = 4096;

/// 逐字节读 dll 交上来的 C 字符串。
///
/// 不直接用 `CStr::from_ptr`：那会一路扫到内存里下一个 0 才算完。插件漏写结尾 NUL 时，
/// 轻则把邻居数据当成指纹（报成看不懂的"工具链不一致"），重则扫进未映射的页把宿主一起带崩。
/// 这里限定最多 [`FINGERPRINT_LIMIT`] 字节，读不到结尾就当场拒载。
///
/// # Safety
/// `pointer` 指向已成功映射进本进程的模块里的一段字节。
unsafe fn read_c_string(pointer: *const u8) -> Result<String, String> {
    let mut bytes = Vec::new();
    for offset in 0..FINGERPRINT_LIMIT {
        // SAFETY: 模块常驻期间这段地址可读，偏移被上面的上限夹住
        let byte = unsafe { *pointer.add(offset) };
        if byte != 0 {
            bytes.push(byte);
            continue;
        }
        return String::from_utf8(bytes).map_err(|_| "工具链指纹不是合法 UTF-8".to_string());
    }
    Err(format!(
        "插件上报的工具链指纹满 {FINGERPRINT_LIMIT} 字节还没有结尾，拒载"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工探针：把某个插件 dll 上报的两项握手数据原样打出来。
    /// 只在排查「工具链指纹不是合法 UTF-8」这类拒载时用。
    #[test]
    #[ignore = "需要 ARONA_PROBE_DLL 指向一个真实 dll"]
    fn dump_a_dlls_handshake() {
        let path = std::env::var("ARONA_PROBE_DLL").expect("设 ARONA_PROBE_DLL=<dll 路径>");
        let library = unsafe { libloading::Library::new(&path) }.unwrap();
        let abi: libloading::Symbol<'_, unsafe extern "C" fn() -> u64> =
            unsafe { symbol(&library, ABI_SYMBOL) }.unwrap();
        let toolchain: libloading::Symbol<'_, unsafe extern "C" fn() -> *const u8> =
            unsafe { symbol(&library, TOOLCHAIN_SYMBOL) }.unwrap();
        let packed = unsafe { abi() };
        let pointer = unsafe { toolchain() };
        println!("宿主指纹: {}", crate::plugin::abi::TOOLCHAIN_FINGERPRINT);
        println!(
            "packed = {packed:#018x} 布局 {} 契约 {}",
            unpack_layout(packed),
            unpack_api(packed)
        );
        let mut bytes = Vec::new();
        for offset in 0..512usize {
            // SAFETY: 探针，只在这台机器上手工跑；模块常驻期间这段内存可读
            let byte = unsafe { *pointer.add(offset) };
            if byte == 0 {
                break;
            }
            bytes.push(byte);
        }
        println!("前 {} 字节: {bytes:?}", bytes.len());
        println!("按 UTF-8 解读: {:?}", std::str::from_utf8(&bytes));
    }

    #[test]
    fn discovery_only_picks_up_plugin_libraries() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|span| span.subsec_nanos())
            .unwrap_or_default();
        let root =
            std::env::temp_dir().join(format!("arona-discover-{}-{stamp}", std::process::id()));
        let nested = root.join("nested-plugin");
        std::fs::create_dir_all(&nested).unwrap();
        let ext = library_extension();
        for name in [
            format!("top.{ext}"),
            format!("_dep.{ext}"),
            "readme.txt".to_string(),
            "not-a-plugin.exe".to_string(),
        ] {
            std::fs::write(root.join(&name), []).unwrap();
        }
        std::fs::write(nested.join(format!("inside.{ext}")), []).unwrap();

        let found = discover(&root);
        assert!(
            found.contains(&root.join(format!("top.{ext}"))),
            "目录直属的 dll 该被发现: {found:?}"
        );
        assert!(
            found.contains(&nested.join(format!("inside.{ext}"))),
            "一级子目录里的 dll 该被发现: {found:?}"
        );
        assert_eq!(
            found.len(),
            2,
            "下划线前缀的依赖库、txt 与 exe 都不该算插件: {found:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// 拿一个确实存在、但肯定没有插件导出的系统模块来验握手：必须报"缺少导出符号"，
    /// 而不是崩溃或误判成插件。kernel32 早已映射进进程，这里只是多加一次引用计数。
    #[test]
    fn a_library_without_plugin_exports_is_rejected() {
        let Some(system_dir) = std::env::var_os("SystemRoot").or_else(|| {
            if cfg!(windows) {
                None
            } else {
                Some(std::ffi::OsString::from("/bin"))
            }
        }) else {
            return;
        };
        let name = if cfg!(windows) { "kernel32.dll" } else { "sh" };
        let path = Path::new(&system_dir).join(name);
        if !path.is_file() {
            return;
        }
        let reason = match unsafe { instantiate(&path) } {
            Ok(_) => panic!("普通模块不该被当成插件装载"),
            Err(reason) => reason,
        };
        assert!(
            reason.contains("缺少导出符号") || reason.contains("无法加载模块"),
            "握手失败要写明原因: {reason}"
        );
    }
}
