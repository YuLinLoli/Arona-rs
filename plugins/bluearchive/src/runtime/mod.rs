//! 插件本地运行期配置（保存在内存，随进程重启恢复默认）。
//! 抽卡/塔罗的运行期参数由功能插件自己维护，与框架的 runtime 无关。
pub mod gacha_config;
pub mod tarot_config;
