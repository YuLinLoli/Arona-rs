//! 插件自持有的一块配置（落到 `config/hello/arona.yml` 的 `hello:` 区）。
//!
//! 插件只写这个 serde 结构 + 字段注释：文件模板生成、加载、热重载、未知子键过滤、
//! 旧配置搬迁全部由框架完成（见 PLUGIN_DEVELOPMENT.md §12）。
//! 读用 `ctx.config::<HelloConfig>("hello").get()`，写用 `ConfigEntry::update(..)`。

use arona::config::arona::PluginConfig;
use serde::{Deserialize, Serialize};

/// 本插件的配置区键名
pub const SECTION: &str = "hello";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HelloConfig {
    /// 每天几点向下面的群问好（0-23）
    pub greet_hour: i64,
    /// 要主动问好的群号，留空表示不主动发
    pub greet_groups: Vec<i64>,
    /// 给机器人每条出站消息加的前缀，留空表示不加
    pub outbound_prefix: String,
}

impl Default for HelloConfig {
    fn default() -> Self {
        HelloConfig {
            greet_hour: 9,
            greet_groups: Vec::new(),
            outbound_prefix: String::new(),
        }
    }
}

impl PluginConfig for HelloConfig {
    const TITLE: &'static str = "示例插件";
    const DOC: &'static str = "HelloPlugin 的玩法参数。改完保存即热生效，不必重启机器人。";

    fn comment(path: &str) -> Option<&'static str> {
        Some(match path {
            "greet_hour" => "每天几点向下面的群问好(0-23)",
            "greet_groups" => "要主动问好的群号，留空表示不主动发",
            "outbound_prefix" => "给每条出站消息加的前缀，留空表示不加",
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_the_bot_quiet() {
        let config = HelloConfig::default();
        assert_eq!(config.greet_hour, 9);
        assert!(
            config.greet_groups.is_empty(),
            "默认不主动骚扰任何群：主动消息总得用户自己点名"
        );
        assert!(config.outbound_prefix.is_empty(), "默认不改写机器人的话");
    }

    #[test]
    fn partial_file_fills_the_rest_with_defaults() {
        // 用户只写了一项，其余靠 `#[serde(default)]` 补齐——升级新增字段不会让老配置读不出来
        let text = "greet_hour: 20\n";
        let loaded: HelloConfig = serde_yaml::from_str(text).expect("片段应能解析");
        assert_eq!(loaded.greet_hour, 20);
        assert_eq!(
            loaded,
            HelloConfig {
                greet_hour: 20,
                ..Default::default()
            }
        );
    }

    #[test]
    fn framework_renders_the_template_from_the_type() {
        // 插件只写结构体和 comment()，带注释模板整段由框架生成——首次启动写进
        // config/hello/arona.yml 的就是下面这份，用户看得懂、也敢直接改
        use arona::config::arona::typed_section;
        let section = typed_section::<HelloConfig>(SECTION);
        assert_eq!(
            section.render(&section.default_value()),
            "\
# ==================== 示例插件 ====================
# HelloPlugin 的玩法参数。改完保存即热生效，不必重启机器人。
hello:
  # 每天几点向下面的群问好(0-23)
  greet_hour: 9
  # 要主动问好的群号，留空表示不主动发
  greet_groups: []
  # 给每条出站消息加的前缀，留空表示不加
  outbound_prefix: ''
"
        );
    }
}
