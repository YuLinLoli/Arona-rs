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

/// 在阻塞线程池里执行纯 CPU 的绘图 + PNG 编码。
///
/// 抽卡结果图(2340×1080)与活动日历图的绘制、编码都是同步的 CPU 密集操作：直接放在
/// tokio 工作线程上跑会把它占住，单核/双核服务器（工作线程本来就少）上收消息、写日志
/// 这些任务会被整段卡死——表现就是「发完指令控制台不动了，过几秒才一次性刷出来」。
/// 丢进 spawn_blocking 后异步工作线程只负责 IO，绘图期间日志照常输出。
pub async fn cpu_bound<T, F>(task: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(task).await {
        Ok(result) => result,
        Err(err) => Err(format!("绘图任务执行失败: {err}")),
    }
}
