//! 平台体检：发行版信息与运行期动态库依赖。
//!
//! 只在 Linux 上做实事，其它平台是空实现。
//!
//! ## 为什么需要这一层
//!
//! Windows 上「缺依赖」的表现很统一：双击没反应，然后去装 VC++ 运行库。Linux 上同一个
//! 问题会碎成很多种样子——`GLIBC_2.34 not found`、`libgcc_s.so.1: cannot open shared
//! object file`、Alpine 上的 `not found`（其实是没有 glibc）——用户拿到的只有动态加载器
//! 丢出来的一行英文，而**那句英文根本来不及进入我们的日志**：程序还没跑到 `main`。
//!
//! 所以这里做两件事：
//! 1. 启动时把「我在什么系统上跑」记进日志（发行版、内核、架构）。同一份日志发给别人看时，
//!    对方不用再问「你什么系统」。
//! 2. 主动探测几个关键共享库能不能加载。缺哪个就点名哪个，并把对应发行版的安装命令
//!    一并打出来——Linux 上装依赖基本就是一条命令的事，难的是知道该装哪个包。
//!
//! 注意**这一层不试图修复任何东西**，也不去调用 `apt`：以什么身份、在哪个发行版上装包
//! 是运行环境的决定，框架只负责把话说清楚。

/// 一个缺失的运行时依赖，以及怎么把它装上
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingDep {
    /// 探测用的共享库名（如 `libgcc_s.so.1`）
    pub library: String,
    /// 各发行版上的包名候选（同一份逻辑包在不同发行版叫法不同）
    pub packages: &'static [&'static str],
}

/// 关键运行时依赖：(库名, 各发行版包名)。
///
/// 只放**真正影响功能**的那几个。glibc 本身（`libc.so.6`）不在此列——它要是缺，
/// 进程根本起不来，任何检查代码都没机会执行；能跑到这里就说明它在。
const WATCHED: &[(&str, &[&str])] = &[
    // 栈展开（panic 时打印回溯、以及 Rust 的 unwind 路径）要用它
    ("libgcc_s.so.1", &["libgcc", "libgcc1", "gcc-libs", "libgcc_s1"]),
    // 数学库：image/字体排版等路径会用到
    ("libm.so.6", &["libc6", "glibc", "libc"]),
    // dlopen 与被插件装载强相关（新版 glibc 已并入 libc，但老系统上是独立库）
    ("libdl.so.2", &["libc6", "glibc", "libc"]),
];

/// 运行环境摘要，直接进启动日志。取不到的部分就不写，绝不因此让启动失败。
pub fn describe() -> String {
    #[cfg(target_os = "linux")]
    {
        let distro = os_pretty_name().unwrap_or_else(|| "未知发行版".to_string());
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|text| text.trim().to_string())
            .unwrap_or_else(|_| "未知内核".to_string());
        let arch = std::env::consts::ARCH;
        format!("{distro} (内核 {kernel}, {arch})")
    }
    #[cfg(not(target_os = "linux"))]
    {
        format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH)
    }
}

/// `/etc/os-release` 里的 `PRETTY_NAME`（如 `Ubuntu 24.04.1 LTS`）。
///
/// 这个文件是 freedesktop 规范的一部分，主流发行版都有；没有就回 `None`，
/// 由调用方退到更粗的描述。
#[cfg(target_os = "linux")]
fn os_pretty_name() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .ok()?;
    let mut fallback = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "PRETTY_NAME" => return (!value.is_empty()).then(|| value.to_string()),
            // PRETTY_NAME 缺失时 NAME 也能用（有些精简镜像会省掉前者）
            "NAME" if !value.is_empty() => fallback = Some(value.to_string()),
            _ => {}
        }
    }
    fallback
}

/// 探测哪些关键依赖加载不了。
///
/// 判定口径是 `dlopen` 成功与否——这正是动态加载器真正在意的那件事，比比对文件是否存在
/// 更准（文件在但 ABI 对不上时，`dlopen` 同样会失败）。
pub fn missing_deps() -> Vec<MissingDep> {
    WATCHED
        .iter()
        .filter(|(library, _)| !can_load(library))
        .map(|(library, packages)| MissingDep {
            library: (*library).to_string(),
            packages,
        })
        .collect()
}

/// 试加载一个共享库。加载成功立刻释放引用计数，不留常驻句柄。
fn can_load(library: &str) -> bool {
    match unsafe { libloading::Library::new(library) } {
        Ok(handle) => {
            // SAFETY: 句柄刚由本函数创建，尚未被任何代码借用
            drop(unsafe { handle.close() });
            true
        }
        Err(_) => false,
    }
}

/// 把缺失依赖拼成给用户看的说明，含各发行版的安装命令。
///
/// 覆盖主流包管理器：Debian/Ubuntu 的 apt、Fedora/RHEL 的 dnf、Arch 的 pacman、
/// Alpine 的 apk。用户照着其中一条抄即可，不必再去查包名。
pub fn install_hint(missing: &[MissingDep]) -> String {
    let debian: Vec<&str> = missing
        .iter()
        .filter_map(|dep| dep.packages.first().copied())
        .collect();
    let fedora: Vec<&str> = missing
        .iter()
        .filter_map(|dep| {
            dep.packages
                .iter()
                .find(|name| matches!(**name, "libgcc" | "glibc" | "gcc-libs"))
                .copied()
                .or_else(|| dep.packages.first().copied())
        })
        .collect();
    let mut names: Vec<&str> = missing
        .iter()
        .flat_map(|dep| dep.packages.iter().copied())
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut text = String::from("缺少运行期依赖：");
    text.push_str(
        &missing
            .iter()
            .map(|dep| dep.library.as_str())
            .collect::<Vec<_>>()
            .join("、"),
    );
    text.push_str("\n  装上其中一个即可：");
    text.push_str(&format!("\n    Debian/Ubuntu: sudo apt install {}", debian.join(" ")));
    text.push_str(&format!("\n    Fedora/RHEL:   sudo dnf install {}", fedora.join(" ")));
    text.push_str(&format!(
        "\n    Arch:          sudo pacman -S {}",
        names.join(" ")
    ));
    text.push_str(&format!(
        "\n    Alpine:        sudo apk add {}",
        names.join(" ")
    ));
    text.push_str("\n  （这些库通常随基础系统就有；容器/精简镜像里才容易缺）");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_is_never_empty() {
        let text = describe();
        assert!(!text.trim().is_empty(), "平台描述不该是空的");
    }

    /// 本机跑测试的环境一定是能加载 libc 的，所以在 Linux 上 `libm` 这一条必须判为在位。
    /// 这条挡的是「探测逻辑写反了，把在的东西报成缺的」那种假警报。
    #[cfg(target_os = "linux")]
    #[test]
    fn a_library_that_is_definitely_there_is_not_reported_missing() {
        assert!(can_load("libm.so.6"), "libm 都加载不了，探测逻辑有问题");
        let missing = missing_deps();
        assert!(
            !missing.iter().any(|dep| dep.library == "libm.so.6"),
            "在位的库被报成缺失了: {missing:?}"
        );
    }

    /// 明确不存在的东西必须被判为加载失败（否则探测等于永远返回「都挺好」）
    #[test]
    fn a_library_that_cannot_exist_fails_to_load() {
        assert!(!can_load("libarona-does-not-exist-0.0.so"));
    }

    #[test]
    fn the_install_hint_names_every_missing_library_and_a_command() {
        let missing = vec![
            MissingDep {
                library: "libgcc_s.so.1".to_string(),
                packages: &["libgcc"],
            },
            MissingDep {
                library: "libm.so.6".to_string(),
                packages: &["libc6", "glibc"],
            },
        ];
        let hint = install_hint(&missing);
        assert!(hint.contains("libgcc_s.so.1") && hint.contains("libm.so.6"));
        for manager in ["apt", "dnf", "pacman", "apk"] {
            assert!(hint.contains(manager), "缺少 {manager} 的安装提示: {hint}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn os_release_is_parsed_into_a_pretty_name() {
        // 绝大多数 Linux 上都能读到；读不到也不该 panic，只是回 None
        if std::path::Path::new("/etc/os-release").is_file() {
            let name = os_pretty_name().expect("有 /etc/os-release 就该解析出名字");
            assert!(!name.trim().is_empty());
        }
    }
}
