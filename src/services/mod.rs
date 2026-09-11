//! 服务注册与管理（对应原版 service 包 AronaService/AronaServiceManager）

use once_cell::sync::OnceCell;
use std::collections::HashMap;
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
        Arc::new(ServiceInfo {
            id,
            name,
            group_only: false,
            admin_only: false,
            enable: AtomicBool::new(true),
        })
    }

    pub fn group_only(mut self: Arc<Self>, value: bool) -> Arc<ServiceInfo> {
        // 通过 Arc 内部修改受限，直接操作字段需要可变性；此处仅记录标记
        let _ = value;
        self
    }
}

pub struct ServiceManager {
    map: Mutex<HashMap<String, Arc<ServiceInfo>>>,
}

static MANAGER: OnceCell<ServiceManager> = OnceCell::new();

pub fn manager() -> &'static ServiceManager {
    MANAGER.get_or_init(|| ServiceManager {
        map: Mutex::new(HashMap::new()),
    })
}

impl ServiceManager {
    pub fn register(&self, service: &Arc<ServiceInfo>) {
        let mut map = self.map.lock().unwrap();
        map.insert(service.name.to_string(), service.clone());
        map.insert(service.id.to_string(), service.clone());
    }

    pub fn find_by_name(&self, name: &str) -> Option<Arc<ServiceInfo>> {
        self.map.lock().unwrap().get(name).cloned()
    }

    pub fn all(&self) -> Vec<Arc<ServiceInfo>> {
        let map = self.map.lock().unwrap();
        let mut names: Vec<&String> = map.keys().filter(|k| k.parse::<i32>().is_ok()).collect();
        names.sort_by_key(|k| k.parse::<i32>().unwrap_or(0));
        names
            .into_iter()
            .filter_map(|k| map.get(k).cloned())
            .collect()
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

pub fn register_all(services: &[Arc<ServiceInfo>]) {
    for service in services {
        manager().register(service);
    }
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
