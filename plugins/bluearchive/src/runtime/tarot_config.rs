//! 塔罗牌运行期配置（对应原版 RuntimeTarotConfig）
//! image=true 时在 /塔罗牌 消息后附带 CDN 塔罗牌图片（image/tarot/{n}-{up|down}.png），
//! 下载失败自动回退为纯文本；启动时会预下载全部 22x2 张牌到本地缓存。

use once_cell::sync::OnceCell;
use std::sync::RwLock;

pub struct TarotConfig {
    /// 是否每天只抽一次（记录到数据库）
    pub day_one: bool,
    /// 是否附带塔罗牌图片（默认开启，与原版一致；下载失败自动回退文本）
    pub image: bool,
}

static CONFIG: OnceCell<RwLock<TarotConfig>> = OnceCell::new();

pub fn instance() -> &'static RwLock<TarotConfig> {
    CONFIG.get_or_init(|| {
        RwLock::new(TarotConfig {
            day_one: false,
            image: true,
        })
    })
}

pub fn day_one() -> bool {
    instance().read().unwrap().day_one
}

pub fn image() -> bool {
    instance().read().unwrap().image
}

/// 供运行期切换是否附带塔罗牌图片
pub fn set_image(enabled: bool) {
    instance().write().unwrap().image = enabled;
}
