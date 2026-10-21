//! 插件描述元数据（对应 mirai 的 `PluginDescription` / jar 内的 `plugin.yml`）
//!
//! 这些值写在插件的 `meta()` 里，框架装载插件时把它们渲染成
//! `plugins/<id>/plugin.yml`，磁盘上看到的描述与代码里的完全一致。
//!
//! **这里曾经有一个 `ApiVersion` 契约号**（对齐 mirai 的 `ApiVerification`），已经去掉。
//! 原因是它与符号布局号在结构上冗余：契约号只在插件接口破坏性变更时才需要抬，而那种变更
//! 必然改变跨边界的类型布局或导出符号，也就必然抬 `abi::ABI_LAYOUT`。实测 v1.0.0 → v1.2.0
//! 三个版本里契约号一路都是 `1.0.0`，布局号却从 3 走到 10——它一次也没独立拦下过东西。
//! 两个数各自演进只会多出一种「一个对一个不对」的状态。现在插件能不能装进来，
//! 只看布局号与工具链指纹（见 [`crate::plugin::abi::check_abi`]）。

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
    /// 硬依赖的插件 id（mirai 的 depends）：任一缺失或被停用，本插件不装配
    pub depends: &'static [&'static str],
    /// 软依赖的插件 id（mirai 的 softDepends）：只决定装配先后，缺了照样跑
    pub soft_depends: &'static [&'static str],
    /// 本插件是否按群生效。**默认 true**，与加这个字段之前的行为一字不差。
    ///
    /// 声明成 `false` 的是纯私聊插件（只订 `BodyFilter::private(..)`，不接群命令）：
    /// 框架不再给它画「启用的群」那一格，`group_settings.<群>.disabled_plugins` 里
    /// 也写不进去，运行期的分群名单对它一律放行（见 [`crate::runtime::config::Gating::plugin_enabled_in_group`]）。
    /// 少一排永远勾不动的复选框，比让用户猜"这个群的开关为什么对它没用"便宜。
    ///
    /// 这是跨 dll 边界的字段：加上它时 `abi::ABI_LAYOUT` 跟着抬，旧插件按新布局读
    /// 会错位，宿主一律拒载（见 PLUGIN_DEVELOPMENT.md §22）。
    pub group_scoped: bool,
}

impl PluginMeta {
    /// 常规构造：其余字段取空，再用 `with_*` 按需补
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
            depends: &[],
            soft_depends: &[],
            group_scoped: true,
        }
    }

    pub const fn with_author(mut self, author: &'static str) -> PluginMeta {
        self.author = author;
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

    /// 声明本插件不按群生效（`false`）：框架收起「启用的群」那一格，分群停用名单对它放行。
    /// 纯私聊插件才该这么写——它一旦也接群消息或群命令，那个开关就是真能救急的东西。
    pub const fn with_group_scoped(mut self, group_scoped: bool) -> PluginMeta {
        self.group_scoped = group_scoped;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_builders_keep_defaults() {
        let meta = PluginMeta::new("demo", "Demo", "0.1.0", "演示").with_author("岚雨凛");
        assert_eq!(meta.id, "demo");
        assert_eq!(meta.author, "岚雨凛");
        assert!(meta.depends.is_empty() && meta.soft_depends.is_empty());
        assert!(
            meta.group_scoped,
            "默认按群生效：这个字段加进来不能改变任何既有插件的行为"
        );
    }

    #[test]
    fn group_scope_can_be_declared_off() {
        let meta = PluginMeta::new("demo", "Demo", "0.1.0", "演示").with_group_scoped(false);
        assert!(!meta.group_scoped);
        // 单位置写两遍要以最后一次为准：builder 是链式的，别留下"前一个值还在"的错觉
        let back = meta.with_group_scoped(true);
        assert!(back.group_scoped);
    }
}
