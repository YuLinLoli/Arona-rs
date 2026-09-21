//! 调度优先级（框架内共用一份序：数值越小越先执行）。
//!
//! 语义取自 mirai 的 `EventPriority`：声明顺序即执行顺序
//! `Monitor -> Normal -> High -> Low -> Lowest`。
//! Monitor 排最前，用来做"先看一眼、必要时短路"的观察者；
//! Lowest 排最后，是兜底实现（如模糊匹配、默认回复）。

/// 事件监听 / 命令匹配的先后次序
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// 最先执行：监控、审计、必要时短路
    Monitor = 0,
    /// 默认档
    Normal = 100,
    /// 比默认更早
    High = 200,
    /// 比默认更晚
    Low = 300,
    /// 最后执行：兜底实现
    Lowest = 400,
}

impl Priority {
    /// 执行序号（越小越先）
    pub fn order(self) -> i32 {
        self as i32
    }

    /// 中文名（GUI/日志用）
    pub fn display_name(self) -> &'static str {
        match self {
            Priority::Monitor => "监控",
            Priority::Normal => "默认",
            Priority::High => "优先",
            Priority::Low => "延后",
            Priority::Lowest => "兜底",
        }
    }
}

impl Default for Priority {
    fn default() -> Self {
        Priority::Normal
    }
}

/// 事件监听优先级
pub type ListenerPriority = Priority;
/// 命令匹配优先级
pub type CommandPriority = Priority;
