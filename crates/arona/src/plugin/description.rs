//! 插件描述元数据（对应 mirai 的 `PluginDescription` / jar 内的 `plugin.yml`）
//!
//! 这些值写在插件的 `meta()` 里，框架装载插件时把它们渲染成
//! `plugins/<id>/plugin.yml`，磁盘上看到的描述与代码里的完全一致。
use std::fmt;

/// 框架与插件之间的契约版本。
///
/// 握手规则与 mirai 的 `ApiVersion.isCompatibleWith` 一致：**主版本号必须相等，
/// 且框架的次版本号不低于插件要求的**（框架只会加能力不会删，所以要求低版本的插件
/// 在新框架上照常跑）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ApiVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ApiVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> ApiVersion {
        ApiVersion {
            major,
            minor,
            patch,
        }
    }

    /// 本框架版本是否满足插件的要求
    pub const fn satisfies(&self, required: &ApiVersion) -> bool {
        self.major == required.major && self.minor >= required.minor
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// 本框现在的插件契约版本。`arona` crate 版本号无关，只在破坏性改动插件接口时抬。
pub const FRAMEWORK_API_VERSION: ApiVersion = ApiVersion::new(1, 0, 0);

/// 插件元信息（规范要求：必须有 id、name 与 version）
#[derive(Clone, Debug)]
pub struct PluginMeta {
    /// 短横线/小写的稳定标识（如 `hello`）：目录名与配置里的键都用它，
    /// 即 `plugins/<id>/`、`config/<id>/arona.yml`、`data/<id>/`、`disabled_plugins: [<id>]`。
    /// 改名等于换一份用户数据，所以定下来就别动。
    pub id: &'static str,
    /// 展示名（GUI 列表、日志里用）
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    /// 作者（mirai 的 author）
    pub author: &'static str,
    /// 插件要求的框架契约版本
    pub api_version: ApiVersion,
    /// 硬依赖的插件 id（mirai 的 depends）：任一缺失或被停用，本插件不装配
    pub depends: &'static [&'static str],
    /// 软依赖的插件 id（mirai 的 softDepends）：只决定装配先后，缺了照样跑
    pub soft_depends: &'static [&'static str],
}

impl PluginMeta {
    /// 常规构造：其余字段取空/当前契约版本，再用 `with_*` 按需补
    pub const fn new(
        id: &'static str,
        name: &'static str,
        version: &'static str,
        description: &'static str,
    ) -> PluginMeta {
        PluginMeta {
            id,
            name,
            version,
            description,
            author: "",
            api_version: FRAMEWORK_API_VERSION,
            depends: &[],
            soft_depends: &[],
        }
    }

    pub const fn with_author(mut self, author: &'static str) -> PluginMeta {
        self.author = author;
        self
    }

    /// 声明要求的框架契约版本（只在用到新接口时才需要抬高）
    pub const fn requires_api(mut self, api_version: ApiVersion) -> PluginMeta {
        self.api_version = api_version;
        self
    }

    pub const fn depends_on(mut self, depends: &'static [&'static str]) -> PluginMeta {
        self.depends = depends;
        self
    }

    pub const fn soft_depends_on(mut self, soft: &'static [&'static str]) -> PluginMeta {
        self.soft_depends = soft;
        self
    }

    /// 与框架的契约是否兼容
    pub fn is_compatible(&self) -> bool {
        FRAMEWORK_API_VERSION.satisfies(&self.api_version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_handshake_follows_semver_minor_rule() {
        let framework = FRAMEWORK_API_VERSION;
        // 要求等于或低于框架次版本号的都能跑
        assert!(framework.satisfies(&ApiVersion::new(1, 0, 0)));
        assert!(framework.satisfies(&ApiVersion::new(1, 0, 9)));
        assert!(
            !framework.satisfies(&ApiVersion::new(1, 99, 0)),
            "要求比框架新的接口应判不兼容"
        );
        assert!(
            !framework.satisfies(&ApiVersion::new(2, 0, 0)),
            "跨主版本一律不兼容"
        );
        assert_eq!(ApiVersion::new(1, 2, 3).to_string(), "1.2.3");
    }

    #[test]
    fn meta_builders_keep_defaults() {
        let meta = PluginMeta::new("demo", "Demo", "0.1.0", "演示").with_author("岚雨凛");
        assert_eq!(meta.id, "demo");
        assert_eq!(meta.author, "岚雨凛");
        assert!(meta.is_compatible(), "默认要求的正是当前契约版本");
        assert!(meta.depends.is_empty() && meta.soft_depends.is_empty());
    }
}
