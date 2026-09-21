//! /紧急停止 投票命令（对应原版 StandaloneEmergencyStop）

use arona::runtime::dispatcher::CommandContext;
use arona::runtime::message::OutgoingMessage;
use arona::services;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub struct EmergencyStop {
    times: usize,
    duration_minutes: i64,
    start_ms: i64,
    votes: HashSet<i64>,
    /// 投票通过时要关停的服务表（本插件被装配进的那套注册表）
    board: Arc<services::ServiceManager>,
}

impl EmergencyStop {
    pub fn new(board: Arc<services::ServiceManager>) -> EmergencyStop {
        EmergencyStop {
            times: 5,
            duration_minutes: 5,
            start_ms: chrono::Utc::now().timestamp_millis(),
            votes: HashSet::new(),
            board,
        }
    }

    pub fn vote(&mut self, context: &Arc<CommandContext>) -> Option<OutgoingMessage> {
        let now = chrono::Utc::now().timestamp_millis();
        let user_id = context.user_id;
        let minutes = (now - self.start_ms) / 1000 / 60;
        if minutes >= self.duration_minutes {
            self.start_ms = now;
            self.votes.clear();
        }
        self.votes.insert(user_id);
        if self.votes.len() >= self.times {
            for service in self.board.all() {
                service.enable.store(false, Ordering::SeqCst);
            }
            return Some(OutgoingMessage::text("达到目标票数,紧急停止"));
        }
        Some(OutgoingMessage::text(format!(
            "当前:{}/{}票, 剩余时间: {}分钟",
            self.votes.len(),
            self.times,
            self.duration_minutes - minutes
        )))
    }
}
