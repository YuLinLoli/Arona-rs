//! 服务注册与管理（对应原版 service 包 AronaService/AronaServiceManager）
//!
//! 「服务」是插件对外暴露的**可单独开关的功能单元**（GUI 的「服务管理」页、`/紧急停止`
//! 与 `/服务` 指令操作的都是这张表）。每条服务都记着提供它的插件 id：
//! 插件被停用时框架按归属一次收干净，不留一行僵尸条目。
//!
//! 表本身住在 [`crate::framework::Framework`] 实例上（`framework.services()`）；
//! 本模块的自由函数走进程默认实例。

use crate::framework::Framework;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 独立模式服务描述（对应 StandaloneServiceInfo）
pub struct ServiceInfo {
    pub id: i32,
    pub name: &'static str,
    pub group_only: bool,
    pub admin_only: bool,
    pub enable: AtomicBool,
}

impl ServiceInfo {
    pub fn new(id: i32, name: &'static str) -> Arc<ServiceInfo> {
        service_info(id, name, false, false)
    }
}

/// 一条服务 + 提供它的插件（None = 框架自带，不随插件停用回收）
struct Entry {
    plugin: Option<String>,
    info: Arc<ServiceInfo>,
}

#[derive(Default)]
pub struct ServiceManager {
    entries: Mutex<Vec<Entry>>,
}

impl ServiceManager {
    /// 登记一条服务（同名以最后一次为准，插件重复 configure 不会产生两行）
    pub fn register(&self, plugin: &str, service: &Arc<ServiceInfo>) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.info.name == service.name)
        {
            existing.info = service.clone();
            existing.plugin = Some(plugin.to_string());
            return;
        }
        entries.push(Entry {
            plugin: Some(plugin.to_string()),
            info: service.clone(),
        });
    }

    pub fn register_all(&self, plugin: &str, services: &[Arc<ServiceInfo>]) {
        for service in services {
            self.register(plugin, service);
        }
    }

    /// 撤销某个插件的全部服务（框架停用/卸载插件时回收），返回收回的条数
    pub fn revoke(&self, plugin: &str) -> usize {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        entries.retain(|entry| entry.plugin.as_deref() != Some(plugin));
        before - entries.len()
    }

    pub fn find_by_name(&self, name: &str) -> Option<Arc<ServiceInfo>> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find(|entry| entry.info.name == name)
            .map(|entry| entry.info.clone())
    }

    /// 某条服务由哪个插件提供（GUI 展示用）
    pub fn owner_of(&self, name: &str) -> Option<String> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find(|entry| entry.info.name == name)
            .and_then(|entry| entry.plugin.clone())
    }

    /// 全部服务，按 id 升序（GUI 列表顺序稳定）
    pub fn all(&self) -> Vec<Arc<ServiceInfo>> {
        let mut infos: Vec<Arc<ServiceInfo>> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .map(|entry| entry.info.clone())
            .collect();
        infos.sort_by_key(|info| info.id);
        infos
    }

    pub fn enable(&self, name: &str) -> Option<Arc<ServiceInfo>> {
        let service = self.find_by_name(name)?;
        service.enable.store(true, Ordering::SeqCst);
        Some(service)
    }

    pub fn disable(&self, name: &str) -> Option<Arc<ServiceInfo>> {
        let service = self.find_by_name(name)?;
        service.enable.store(false, Ordering::SeqCst);
        Some(service)
    }
}

/// 进程默认实例上的服务表：插件在自己的装配代码之外（命令处理器、后台任务）
/// 按名字查/开关服务时用这个；装配期请用 [`crate::plugin::PluginContext::register_service`]。
pub fn global_board() -> Arc<ServiceManager> {
    Framework::global().services().clone()
}

/// 生成 groupOnly/adminOnly 的服务（对应原版 StandaloneServiceInfo 构造）
pub fn service_info(
    id: i32,
    name: &'static str,
    group_only: bool,
    admin_only: bool,
) -> Arc<ServiceInfo> {
    Arc::new(ServiceInfo {
        id,
        name,
        group_only,
        admin_only,
        enable: AtomicBool::new(true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 归属记账 + 停用回收：只收这家插件的条目，别家的照旧可查
    #[test]
    fn revokes_only_the_disabled_plugin() {
        let manager = ServiceManager::default();
        manager.register("Alpha", &service_info(2, "乙服务", false, false));
        manager.register("Alpha", &service_info(1, "甲服务", true, true));
        manager.register("Beta", &service_info(3, "丙服务", false, false));

        let ids: Vec<i32> = manager.all().iter().map(|info| info.id).collect();
        assert_eq!(ids, vec![1, 2, 3], "列表要按 id 稳定排序");
        assert_eq!(manager.owner_of("甲服务").as_deref(), Some("Alpha"));
        assert!(
            manager
                .all()
                .iter()
                .any(|info| info.group_only && info.admin_only),
            "group/admin 标记应原样保留"
        );

        assert_eq!(manager.revoke("Alpha"), 2);
        assert!(manager.find_by_name("甲服务").is_none());
        assert!(manager.find_by_name("乙服务").is_none());
        assert!(manager.find_by_name("丙服务").is_some());
        // 重复撤销不该把别家牵进来
        assert_eq!(manager.revoke("Alpha"), 0);
        assert_eq!(manager.revoke("Beta"), 1);
        assert!(manager.all().is_empty());
    }

    /// 重复登记（插件重新 configure）只更新那一行，不多出一行
    #[test]
    fn registering_the_same_name_twice_keeps_one_row() {
        let manager = ServiceManager::default();
        manager.register("Alpha", &service_info(7, "同一服务", false, false));
        manager.register("Alpha", &service_info(7, "同一服务", false, true));
        assert_eq!(manager.all().len(), 1);
        assert!(manager.all()[0].admin_only, "应以最后一次登记为准");
    }

    /// 隔离性：另一套框架实例的服务表看不见这套登记的条目
    #[test]
    fn boards_are_isolated_per_framework() {
        crate::framework::Framework::new()
            .services()
            .register("Isolated", &service_info(9, "隔离服务", false, false));
        assert!(
            ServiceManager::default().find_by_name("隔离服务").is_none(),
            "实例之间不该共用一张表"
        );
        assert!(
            global_board().find_by_name("隔离服务").is_none(),
            "默认实例也不该看见别的实例登记的东西"
        );
    }
}
