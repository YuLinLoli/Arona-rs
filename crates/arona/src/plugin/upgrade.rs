//! 插件升级（方案 A：换文件 + 重启进程）。
//!
//! 为什么是"换文件 + 重启"而不是"运行期卸载重装"：dll 加载后句柄被
//! [`std::mem::forget`](crate::plugin::dynamic) 掉，模块常驻不卸载（虚表指向模块代码段，
//! 卸载后任何虚调用都是野指针）。所以不可能"卸掉旧的再装新的"——能做的只有：
//!
//! 1. 停用目标插件（收干净命令/订阅/任务）；
//! 2. 把旧 dll `rename` 成 `.bak`（Windows 上已加载的 dll 不能删/覆盖，但**能改名**）；
//! 3. 把新 dll 拷进原路径；
//! 4. **清目录**：插件所在文件夹里除新 dll 外全删（老版本拷贝、.bak、旧资源——手动
//!    上传的插件，目录里常攒多份老版本，`discover()` 同 id 撞车后谁生效全凭运气）；
//! 5. 请求重启：框架正常收尾退出，然后自己拉起一个新进程（`relaunch::spawn_self`），
//!    新进程启动时加载的已经是新文件，`.bak` 若因句柄没放清不掉，启动早期再补一刀。
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
///
/// **文件名沿用新上传的**：用户手里的 dll 带版本号（`xxx-0.4.12.dll`），升级后
/// 目录里就该叫这个名，而不是把新内容塞回旧名字——旧名混着旧版本号只会误导。
/// 旧文件（仍被本进程加载着）rename 成 `.bak`，重启后由启动期的
/// [`crate::plugin::dynamic::cleanup_stale`] 删掉。
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

    // —— 换文件：新文件用**它自己的名字**落进插件目录 ——
    let plugin_dir = target
        .parent()
        .ok_or_else(|| format!("插件路径异常（没有父目录）：{}", target.display()))?
        .to_path_buf();
    let new_target = plugin_dir.join(
        new_dll
            .file_name()
            .ok_or_else(|| format!("选中的文件名异常：{}", new_dll.display()))?,
    );
    let bak = {
        let mut name = target.as_os_str().to_owned();
        name.push(".bak");
        std::path::PathBuf::from(name)
    };
    // 上次升级的残留（那次进程早已退出，句柄已放，现在能删）
    let _ = std::fs::remove_file(&bak);
    std::fs::rename(&target, &bak).map_err(|err| format!("改名旧插件失败：{err}"))?;
    if let Err(err) = std::fs::copy(new_dll, &new_target) {
        // 拷失败就把旧的改回来，别让插件消失（名字不同就删掉写了一半的新文件）
        let _ = std::fs::rename(&bak, &target);
        if new_target != target {
            let _ = std::fs::remove_file(&new_target);
        }
        return Err(format!("写入新插件失败：{err}"));
    }

    // —— 清目录：留下这次升级选中的文件即可（用户明确要求）——
    wipe_plugin_dir_except(&new_target, &crate::runtime::paths::plugins_dir());

    crate::runtime::log::info_green(format!(
        "插件「{}」的新文件已就位（{}），正在重启框架",
        plugin.meta().name,
        new_target.file_name().unwrap_or_default().to_string_lossy()
    ));
    // 挂重启标记：GUI 主循环收尾后据此拉起新进程
    request_restart();
    Ok(())
}

/// 安装一个**新**插件：校验 ABI → 落到 `plugins/<文件名去后缀>/` → 请求重启。
///
/// 与 [`upgrade_plugin`] 的差别：没有已登记的插件可查路径，目录名取 dll 文件名
/// 去后缀（推荐布局：每个插件独占一个目录，资源文件放得下）；若是往一个曾有
/// 插件的目录里重装，[`wipe_plugin_dir_except`] 会把旧的清干净。
pub fn install_plugin(new_dll: &Path) -> Result<(), String> {
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
    // SAFETY: new_dll 是用户选出来的本地文件，与本插件同为宿主进程内的可信代码
    unsafe { crate::plugin::dynamic::validate(new_dll) }
        .map_err(|reason| format!("不是本框架能用的插件：{reason}"))?;

    let file_name = new_dll
        .file_name()
        .ok_or_else(|| format!("选中的文件名异常：{}", new_dll.display()))?;
    let stem = new_dll
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| format!("选中的文件名异常：{}", new_dll.display()))?;
    let dir = crate::runtime::paths::plugins_dir().join(stem);
    std::fs::create_dir_all(&dir).map_err(|err| format!("建插件目录失败：{err}"))?;
    let target = dir.join(file_name);
    std::fs::copy(new_dll, &target).map_err(|err| format!("写入插件失败：{err}"))?;

    // 重装进旧目录时把残留清干净；全新目录则只有这一个文件
    wipe_plugin_dir_except(&target, &crate::runtime::paths::plugins_dir());

    crate::runtime::log::info_green(format!(
        "插件已安装（{}），正在重启框架",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    request_restart();
    Ok(())
}

/// 换完文件后清空插件所在目录、只留刚选中的这份。
///
/// 手动上传的插件，目录里常攒着多份老版本（`-0.1.6.dll`、`-0.1.7.dll`、`.bak`…），
/// [`crate::plugin::dynamic::discover`] 会挨个加载，同 id 撞车后谁生效全凭排序运气。
/// 升级正是清理的时机：反正是手动上传的，除了刚选中的这份**全删**（用户原话），
/// 新文件要不要、是不是更新，[`crate::plugin::dynamic::validate`] 的 ABI 体检已经把关。
///
/// 范围分两种：
/// - dll 在 `plugins/<子目录>/` 里 → 清整个子目录（插件带的资源文件也一起走，用户认可）；
/// - dll 直接躺在 `plugins/` 根上 → 不能殃及邻居插件，只删**自己的** `.bak`，
///   其余交给启动时的 [`crate::plugin::dynamic::cleanup_stale`]。
///
/// 已加载的旧 dll（刚 rename 成的 `.bak`）句柄还开着，Windows 上可能删不掉——
/// 删不掉不报错，留给下次启动的 `cleanup_stale` 兜底。
fn wipe_plugin_dir_except(keep: &Path, plugins_root: &Path) {
    let Some(dir) = keep.parent() else {
        return;
    };
    // 直接躺在 plugins/ 根上的插件：范围收到「自己的 .bak」，别的全是邻居的
    let flat = dir == plugins_root;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == *keep {
            continue;
        }
        if flat && path.extension().and_then(|ext| ext.to_str()) != Some("bak") {
            continue;
        }
        let result = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(err) = result {
            crate::runtime::log::info(format!(
                "清理旧文件失败（留给下次启动清理）：{} —— {err}",
                path.display()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 子目录布局：`plugins/<id>/` 里的老版本、.bak、资源文件全清，只留新 dll。
    #[test]
    fn subdir_wipe_keeps_only_the_chosen_file() {
        let root = std::env::temp_dir().join(format!("arona-wipe-sub-{}", std::process::id()));
        let dir = root.join("myplugin");
        std::fs::create_dir_all(&dir).unwrap();
        let keep = dir.join("myplugin.dll");
        std::fs::write(&keep, b"new").unwrap();
        std::fs::write(dir.join("myplugin-0.1.6.dll"), b"old").unwrap();
        std::fs::write(dir.join("myplugin.dll.bak"), b"old").unwrap();
        std::fs::write(dir.join("banner.png"), b"res").unwrap();
        std::fs::create_dir_all(dir.join("lang")).unwrap();

        wipe_plugin_dir_except(&keep, &root);

        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            left,
            vec!["myplugin.dll".to_string()],
            "只该留新 dll：{left:?}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 根目录布局：只删自己的 .bak，邻居插件与子目录一个字节都不能碰。
    #[test]
    fn flat_wipe_only_takes_the_bak() {
        let root = std::env::temp_dir().join(format!("arona-wipe-flat-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let keep = root.join("myplugin.dll");
        std::fs::write(&keep, b"new").unwrap();
        std::fs::write(root.join("myplugin.dll.bak"), b"old").unwrap();
        std::fs::write(root.join("neighbor.dll"), b"other").unwrap();
        std::fs::create_dir_all(root.join("otherplugin")).unwrap();

        wipe_plugin_dir_except(&keep, &root);

        assert!(keep.is_file());
        assert!(!root.join("myplugin.dll.bak").exists(), ".bak 应被删");
        assert!(root.join("neighbor.dll").is_file(), "邻居插件不能动");
        assert!(root.join("otherplugin").is_dir(), "邻居子目录不能动");
        std::fs::remove_dir_all(&root).unwrap();
    }

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
