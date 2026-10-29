//! 插件升级（方案 A：换文件 + 重启进程）。
//!
//! 为什么是"换文件 + 重启"而不是"运行期卸载重装"：dll 加载后句柄被
//! [`std::mem::forget`](crate::plugin::dynamic) 掉，模块常驻不卸载（虚表指向模块代码段，
//! 卸载后任何虚调用都是野指针）。所以不可能"卸掉旧的再装新的"——能做的只有：
//!
//! 1. 停用目标插件（收干净命令/订阅/任务）；
//! 2. 把旧 dll `rename` 成 `.bak`（Windows 上已加载的 dll 不能删/覆盖，但**能改名**）；
//! 3. 把新 dll 拷进原路径；
//! 4. 请求重启：框架正常收尾退出，然后自己拉起一个新进程（`relaunch::spawn_self`），
//!    新进程启动时加载的已经是新文件，顺手清掉 `.bak`。
//!
//! 流程对用户表现为"选个文件点一下，面板关掉几秒后自己回来，插件已换新"。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::plugin::manager::PluginManager;

/// 「收尾后重启」标记。
///
/// 升级只做「换文件」，真正拉起新进程得等 GUI 主循环正常收尾之后（机器人、连接、
/// 调试口都先关干净，新进程才不会撞端口）。标记放在这里而不是 GUI 的重启模块：
/// `runtime::relaunch` 只在带窗口的构建里编译，而升级功能本身（换文件、校验）
/// 在无窗口的构建里也要能编——之前把标记挂在那边，Linux 的 `--no-default-features`
/// 构建就直接编不过。
static RESTART: AtomicBool = AtomicBool::new(false);

/// 请求「收尾后重启」（升级插件用）
pub fn request_restart() {
    RESTART.store(true, Ordering::SeqCst);
}

/// 是否挂着重启请求（GUI 收尾时查这个）
pub fn restart_requested() -> bool {
    RESTART.load(Ordering::SeqCst)
}

/// 把 `new_dll` 替换成 `plugin_id` 的新版本，并请求重启框架。
///
/// 这一步只做「校验 + 换文件 + 挂重启标记」，不负责真正拉起新进程——
/// 重启发生在 [`crate::gui`] 主循环正常收尾之后（见 `relaunch::request_restart`），
/// 保证旧进程的机器人、连接、调试口都先关干净，新进程不会撞端口。
pub fn upgrade_plugin(plugin_id: &str, new_dll: &Path) -> Result<(), String> {
    let manager = PluginManager::global();
    let plugin = manager
        .find(plugin_id)
        .ok_or_else(|| format!("没找到插件「{plugin_id}」"))?;
    let target = plugin
        .library_path()
        .ok_or_else(|| format!("「{plugin_id}」是内置插件，不通过文件升级"))?
        .to_path_buf();

    // —— 校验选中的文件 ——
    if !new_dll.is_file() {
        return Err("选中的文件不存在".to_string());
    }
    let is_dll = new_dll
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"));
    if !is_dll {
        return Err("只能选 .dll 文件".to_string());
    }

    // 替换前体检：能过握手才换，否则重启后才发现是个坏文件，白折腾一轮
    // SAFETY: new_dll 是用户选出来的本地文件，与本插件同为宿主进程内的可信代码
    unsafe { crate::plugin::dynamic::validate(new_dll) }
        .map_err(|reason| format!("新文件不是本框架能用的插件：{reason}"))?;

    // 停用目标插件：虽然马上要重启，但先停一次能保证它的 stop() 在旧文件上跑完，
    // 且停用顺序走的是框架那套依赖/回收逻辑，不会留个半拉子状态到退出
    if plugin.state().is_running() {
        manager.disable(&plugin);
    }

    // —— 换文件 ——
    let bak = {
        let mut name = target.as_os_str().to_owned();
        name.push(".bak");
        std::path::PathBuf::from(name)
    };
    // 上次升级的残留（那次进程早已退出，句柄已放，现在能删）
    let _ = std::fs::remove_file(&bak);
    std::fs::rename(&target, &bak).map_err(|err| format!("改名旧插件失败：{err}"))?;
    if let Err(err) = std::fs::copy(new_dll, &target) {
        // 拷失败了就把旧的改回来，别让插件消失
        let _ = std::fs::rename(&bak, &target);
        return Err(format!("写入新插件失败：{err}"));
    }

    crate::runtime::log::info_green(format!(
        "插件「{}」的新文件已就位（{}），正在重启框架",
        plugin.meta().name,
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    // 挂重启标记：GUI 主循环收尾后据此拉起新进程
    request_restart();
    Ok(())
}

#[cfg(test)]
mod tests {
    /// `.bak` 的命名必须稳定：`foo.dll` -> `foo.dll.bak`。
    /// 这个约定被 cleanup_stale 与 upgrade 两处依赖，改一边另一边就对不上。
    #[test]
    fn backup_suffix_is_predictable() {
        let target = std::path::PathBuf::from("plugins/foo.dll");
        let mut name = target.as_os_str().to_owned();
        name.push(".bak");
        let bak = std::path::PathBuf::from(name);
        assert_eq!(bak, std::path::PathBuf::from("plugins/foo.dll.bak"));
    }
}
