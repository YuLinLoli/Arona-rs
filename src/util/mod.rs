//! 通用工具

pub mod tarot;
pub mod time;

/// 简单随机工具（对应原版 GeneralUtils.randomInt/randomBoolean）
pub fn random_int(max: i64) -> i64 {
    use rand::Rng;
    rand::thread_rng().gen_range(0..max)
}

pub fn random_bool() -> bool {
    use rand::Rng;
    rand::thread_rng().gen_bool(0.5)
}
