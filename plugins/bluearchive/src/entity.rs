//! 业务实体（对应原版 entity 包中独立模式用到的模型）

use serde::{Deserialize, Serialize};

/// 服务器（对应原版 `ServerLocale`）
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub enum ServerLocale {
    JP,
    GLOBAL,
    CN,
}

impl ServerLocale {
    pub const ALL: [ServerLocale; 3] = [ServerLocale::JP, ServerLocale::GLOBAL, ServerLocale::CN];

    pub fn server_name(self) -> &'static str {
        match self {
            ServerLocale::JP => "日服",
            ServerLocale::GLOBAL => "国际服",
            ServerLocale::CN => "国服",
        }
    }

    pub fn db_name(self) -> &'static str {
        match self {
            ServerLocale::JP => "JPN",
            ServerLocale::GLOBAL => "GLB",
            ServerLocale::CN => "CN",
        }
    }

    pub fn command_name(self) -> &'static str {
        match self {
            ServerLocale::JP => "jp",
            ServerLocale::GLOBAL => "en",
            ServerLocale::CN => "cn",
        }
    }

    pub fn by_command_or_name(raw: &str) -> Option<ServerLocale> {
        Self::ALL
            .into_iter()
            .find(|s| s.command_name() == raw || s.server_name() == raw)
    }
}

/// 活动类型（对应原版 `ActivityType`）
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ActivityType {
    Null,
    N2_3,
    H2_3,
    SpecialDrop,
    WantedDrop,
    CollegeExchangeDrop,
    Schedule,
    DecisiveBattle,
    JointExercises,
    PickUp,
    Kabala,
    Maintenance,
    Activity,
    Birthday,
}

impl ActivityType {
    pub fn name(self) -> &'static str {
        match self {
            ActivityType::Null => "NULL",
            ActivityType::N2_3 => "N2_3",
            ActivityType::H2_3 => "H2_3",
            ActivityType::SpecialDrop => "SPECIAL_DROP",
            ActivityType::WantedDrop => "WANTED_DROP",
            ActivityType::CollegeExchangeDrop => "COLLEGE_EXCHANGE_DROP",
            ActivityType::Schedule => "SCHEDULE",
            ActivityType::DecisiveBattle => "DECISIVE_BATTLE",
            ActivityType::JointExercises => "JOINT_EXERCISES",
            ActivityType::PickUp => "PICK_UP",
            ActivityType::Kabala => "KABALA",
            ActivityType::Maintenance => "MAINTENANCE",
            ActivityType::Activity => "ACTIVITY",
            ActivityType::Birthday => "BIRTHDAY",
        }
    }

    pub fn from_name(name: &str) -> ActivityType {
        match name {
            "N2_3" => ActivityType::N2_3,
            "H2_3" => ActivityType::H2_3,
            "SPECIAL_DROP" => ActivityType::SpecialDrop,
            "WANTED_DROP" => ActivityType::WantedDrop,
            "COLLEGE_EXCHANGE_DROP" => ActivityType::CollegeExchangeDrop,
            "SCHEDULE" => ActivityType::Schedule,
            "DECISIVE_BATTLE" => ActivityType::DecisiveBattle,
            "JOINT_EXERCISES" => ActivityType::JointExercises,
            "PICK_UP" => ActivityType::PickUp,
            "KABALA" => ActivityType::Kabala,
            "MAINTENANCE" => ActivityType::Maintenance,
            "ACTIVITY" => ActivityType::Activity,
            "BIRTHDAY" => ActivityType::Birthday,
            _ => ActivityType::Null,
        }
    }
}

/// 活动（对应原版 `Activity`）
#[derive(Clone, Debug)]
pub struct Activity {
    pub content: String,
    /// 展示用时间文本（开始/结束）
    pub time: String,
    pub activity_type: ActivityType,
    pub server: ServerLocale,
    /// 开始时间戳（毫秒）
    pub start_time: i64,
    /// 结束时间戳（毫秒）
    pub end_time: i64,
}

impl Activity {
    pub fn new(
        content: String,
        server: ServerLocale,
        start_ms: i64,
        end_ms: i64,
        activity_type: ActivityType,
    ) -> Activity {
        Activity {
            content,
            time: String::new(),
            activity_type,
            server,
            start_time: start_ms,
            end_time: end_ms,
        }
    }
}
