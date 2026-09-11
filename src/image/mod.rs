//! 图片渲染模块（对应原版依赖 Java AWT 的图片输出）
//! 原版三类图片在本移植版中由纯 Rust 生成:
//! - gacha: 抽卡结果图（2340x1080 卡片网格, 头像本地缓存/下载）
//! - activity: 活动日历图（白底圆角色块排版）
//! - tarot: 塔罗牌图沿用原版方案, 从 CDN 下载后发送（见 standalone/commands/tarot.rs）
//! 字体优先使用系统自带的 simhei/微软雅黑等中文字体, 找不到字体时命令层回退为文本输出。

pub mod activity;
pub mod draw;
pub mod gacha;
#[cfg(test)]
mod render_tests;
mod text;
