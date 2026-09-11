//! OneBot 事件/动作/响应模型（对应原版 OneBotModel）
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct OneBotEvent {
    pub time: i64,
    pub self_id: i64,
    pub post_type: String,
    pub notice_type: Option<String>,
    pub message_type: Option<String>,
    pub sub_type: Option<String>,
    pub message_id: Option<i64>,
    pub user_id: Option<i64>,
    pub operator_id: Option<i64>,
    pub group_id: Option<i64>,
    pub raw_message: Option<String>,
    pub message: Option<Value>,
    pub sender: Option<Value>,
    pub raw: Value,
}

#[derive(Clone, Debug)]
pub struct OneBotAction {
    pub action: String,
    pub params: Value,
    pub echo: String,
}

#[derive(Clone, Debug)]
pub struct OneBotActionResponse {
    pub status: String,
    pub retcode: i64,
    pub data: Option<Value>,
    pub message: Option<String>,
    pub wording: Option<String>,
    pub echo: Option<String>,
    pub raw: Value,
}

impl OneBotActionResponse {
    pub fn success(&self) -> bool {
        self.status == "ok" && self.retcode == 0
    }
}

#[derive(Clone, Debug)]
pub enum ParsedPayload {
    Event(OneBotEvent),
    Action(OneBotAction),
    Response(OneBotActionResponse),
}
