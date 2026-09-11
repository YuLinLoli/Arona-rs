//! 运行期全局配置（对应原版 RuntimeConfig）
use once_cell::sync::OnceCell;
use std::sync::RwLock;

pub struct RuntimeConfig {
    /// 允许响应的群号列表，留空表示响应所有群
    pub groups: RwLock<Vec<i64>>,
    /// 管理员 QQ 号列表
    pub managers: RwLock<Vec<i64>>,
    /// 机器人 QQ
    pub bot_id: RwLock<i64>,
    /// 名称后缀（独立模式固定为“老师”）
    pub end_with_sensei: RwLock<String>,
    /// arona 云端鉴权用 UUID（对应原版 RuntimeConfig.uuid，独立模式默认空字符串）
    pub uuid: RwLock<String>,
}

static CONFIG: OnceCell<RuntimeConfig> = OnceCell::new();

fn instance() -> &'static RuntimeConfig {
    CONFIG.get_or_init(|| RuntimeConfig {
        groups: RwLock::new(Vec::new()),
        managers: RwLock::new(Vec::new()),
        bot_id: RwLock::new(0),
        end_with_sensei: RwLock::new(String::from("老师")),
        uuid: RwLock::new(String::new()),
    })
}

pub fn set_groups(groups: Vec<i64>) {
    *instance().groups.write().unwrap() = groups;
}

pub fn set_managers(managers: Vec<i64>) {
    *instance().managers.write().unwrap() = managers;
}

pub fn groups() -> Vec<i64> {
    instance().groups.read().unwrap().clone()
}

pub fn managers() -> Vec<i64> {
    instance().managers.read().unwrap().clone()
}

pub fn is_manager(user_id: i64) -> bool {
    instance().managers.read().unwrap().contains(&user_id)
}

pub fn set_bot_id(id: i64) {
    *instance().bot_id.write().unwrap() = id;
}

pub fn bot_id() -> i64 {
    *instance().bot_id.read().unwrap()
}

pub fn end_with_sensei() -> String {
    instance().end_with_sensei.read().unwrap().clone()
}

pub fn set_end_with_sensei(value: String) {
    *instance().end_with_sensei.write().unwrap() = value;
}

pub fn uuid() -> String {
    instance().uuid.read().unwrap().clone()
}

pub fn set_uuid(value: String) {
    *instance().uuid.write().unwrap() = value;
}
