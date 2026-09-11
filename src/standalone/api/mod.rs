//! 服务注册（对应原版 standalone/commands/StandaloneServices）
//! 先注册全部服务，dispatcher 再按名称取 Arc<ServiceInfo> 用于命令守卫。

use crate::services::{self, ServiceInfo, service_info};
use std::sync::Arc;

pub fn register_all() {
    let list = all_services();
    services::register_all(&list);
}

/// 与 StandaloneServices.all 保持一致的服务列表
pub fn all_services() -> Vec<Arc<ServiceInfo>> {
    vec![
        service_info(23, "配置管理", false, true),
        service_info(1, "抽卡配置", false, true),
        service_info(3, "活动查询", false, false),
        service_info(12, "活动推送", false, false),
        service_info(19, "数据同步服务", false, false),
        service_info(20, "地图与学生攻略", false, false),
        service_info(4, "抽卡单抽", true, false),
        service_info(5, "抽卡十连", true, false),
        service_info(6, "抽卡狗叫查询", true, false),
        service_info(7, "抽卡历史查询", true, false),
        service_info(26, "抽卡服务器设置", false, false),
        service_info(16, "塔罗牌", false, false),
        service_info(17, "紧急停止", false, false),
        service_info(18, "自定义昵称", true, false),
        service_info(21, "游戏名记录", false, false),
        service_info(22, "游戏名反查", false, false),
        service_info(24, "定时任务", false, true),
        service_info(25, "备份恢复", false, true),
    ]
}
