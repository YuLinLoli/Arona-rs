//! 服务容器（对应 mirai 的 `DiContainer`）
//!
//! 插件之间不靠全局 `static` 互相摸，而是各自把能力 `declare` 进来，
//! 由别人按类型取用：
//!
//! ```ignore
//! // 提供方（configure 阶段）
//! ctx.declare_service::<MyDice>(std::sync::Arc::new(MyDice));
//!
//! // 使用方（自己插件的 meta 里声明 soft_depends = ["dice"]）
//! if let Some(dice) = arona::container::instance::<MyDice>() {
//!     dice.roll(6);
//! } else {
//!     // 提供方没装或被停用：走降级路径
//! }
//! ```
//!
//! 服务按 `TypeId` 寻址，键类型必须 `Sized`。要把「接口」而不是具体结构暴露出去，
//! 用 `Arc<dyn Trait>` 当键类型（它本身是 `Sized` 的）：
//!
//! ```ignore
//! pub type Dice = std::sync::Arc<dyn DiceRoller>;
//! ctx.declare_service::<Dice>(std::sync::Arc::new(std::sync::Arc::new(MyDice) as Dice));
//! let dice: Option<Dice> = ctx.service::<Dice>();
//! ```
//!
//! 每个服务都记住登记它的插件 id：插件被停用时框架撤销它名下的全部服务，
//! 所以取到的一定来自还在工作的插件。表本身住在 [`ServiceContainer`] 实例里，
//! 由 [`crate::framework::Framework`] 持有；本模块的自由函数走进程默认实例。
use crate::framework::Framework;
use crate::runtime::config::Gating;
use std::any::{Any, TypeId};
use std::sync::{Arc, RwLock};

/// 一条已登记服务（诊断用）
pub struct ServiceInfo {
    pub plugin: String,
    pub type_name: &'static str,
}

struct Slot {
    id: TypeId,
    owner: String,
    type_name: &'static str,
    value: Arc<dyn Any + Send + Sync>,
}

/// 服务表：类型数量个位数，直接线性查找
pub struct ServiceContainer {
    /// 「提供方被停用则服务不可见」这条规则要看门控，故持有同一套门控状态
    gating: Arc<Gating>,
    items: RwLock<Vec<Slot>>,
}

impl ServiceContainer {
    pub(crate) fn new(gating: Arc<Gating>) -> ServiceContainer {
        ServiceContainer {
            gating,
            items: RwLock::new(Vec::new()),
        }
    }

    /// 登记一个服务（同类型重复登记时替换）。返回被顶掉的原归属插件 id。
    pub fn declare<T: Send + Sync + 'static>(
        &self,
        plugin: &str,
        service: Arc<T>,
    ) -> Option<String> {
        let id = TypeId::of::<T>();
        let mut items = self.items.write().unwrap();
        let previous = items.iter().position(|slot| slot.id == id).map(|at| {
            let owner = items[at].owner.clone();
            items.remove(at);
            owner
        });
        items.push(Slot {
            id,
            owner: plugin.to_string(),
            type_name: std::any::type_name::<T>(),
            value: service,
        });
        previous
    }

    /// 按类型取服务；提供方未登记或已被停用时返回 None
    pub fn instance<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        let id = TypeId::of::<T>();
        let guard = self.items.read().unwrap();
        let slot = guard.iter().find(|slot| slot.id == id)?;
        if !self.gating.plugin_enabled(&slot.owner) {
            return None;
        }
        slot.value.clone().downcast::<T>().ok()
    }

    /// 取服务提供方（软依赖是否就绪、诊断提示用）
    pub fn provider_of<T: Send + Sync + 'static>(&self) -> Option<String> {
        let id = TypeId::of::<T>();
        self.items
            .read()
            .unwrap()
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| slot.owner.clone())
    }

    /// 撤销某个插件登记的全部服务（插件停用时由框架调用），返回撤销条数
    pub fn revoke_plugin(&self, plugin: &str) -> usize {
        let mut items = self.items.write().unwrap();
        let before = items.len();
        items.retain(|slot| slot.owner != plugin);
        before - items.len()
    }

    /// 已登记服务概览（按类型名排序）
    pub fn list(&self) -> Vec<ServiceInfo> {
        let mut infos: Vec<ServiceInfo> = self
            .items
            .read()
            .unwrap()
            .iter()
            .map(|slot| ServiceInfo {
                plugin: slot.owner.clone(),
                type_name: slot.type_name,
            })
            .collect();
        infos.sort_by(|a, b| a.type_name.cmp(b.type_name));
        infos
    }
}

fn global() -> &'static ServiceContainer {
    Framework::global().container()
}

/// 登记一个服务（同类型重复登记时替换）。返回被顶掉的原归属插件 id。
pub fn declare<T: Send + Sync + 'static>(plugin: &str, service: Arc<T>) -> Option<String> {
    global().declare(plugin, service)
}

/// 按类型取服务；提供方未登记或已被停用时返回 None
pub fn instance<T: Send + Sync + 'static>() -> Option<Arc<T>> {
    global().instance::<T>()
}

/// 取服务提供方（软依赖是否就绪、诊断提示用）
pub fn provider_of<T: Send + Sync + 'static>() -> Option<String> {
    global().provider_of::<T>()
}

/// 撤销某个插件登记的全部服务（插件停用时由框架调用），返回撤销条数
pub fn revoke_plugin(plugin: &str) -> usize {
    global().revoke_plugin(plugin)
}

/// 已登记服务概览（按类型名排序）
pub fn list() -> Vec<ServiceInfo> {
    global().list()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::Framework;

    struct DummyService {
        answer: i32,
    }

    /// 整套注册表按实例持有，所以用例各拿一套、并行跑不会互相看见
    #[test]
    fn declares_provides_and_revokes_by_plugin() {
        let framework = Framework::new();
        let plugin = "ContainerTestPlugin";
        assert!(
            framework
                .container()
                .declare(plugin, Arc::new(DummyService { answer: 41 }))
                .is_none(),
            "首次登记不该有被替换者"
        );
        assert_eq!(
            framework
                .container()
                .instance::<DummyService>()
                .map(|service| service.answer),
            Some(41)
        );
        assert_eq!(
            framework
                .container()
                .provider_of::<DummyService>()
                .as_deref(),
            Some(plugin)
        );

        // 同类型再登记一次：新的胜出，旧的归属作为替换信息返回
        assert_eq!(
            framework
                .container()
                .declare(plugin, Arc::new(DummyService { answer: 42 }))
                .as_deref(),
            Some(plugin)
        );
        assert_eq!(
            framework
                .container()
                .instance::<DummyService>()
                .map(|service| service.answer),
            Some(42)
        );

        // 提供方被整体停用后，服务对所有人消失
        framework
            .gating()
            .set_disabled_plugins(vec![plugin.to_string()]);
        assert!(
            framework.container().instance::<DummyService>().is_none(),
            "停用插件的服务不该可用"
        );
        framework.gating().set_disabled_plugins(Vec::new());
        assert!(
            framework.container().instance::<DummyService>().is_some(),
            "重新启用后应恢复"
        );

        assert_eq!(framework.container().revoke_plugin(plugin), 1);
        assert!(framework.container().instance::<DummyService>().is_none());
        assert!(
            framework
                .container()
                .list()
                .iter()
                .all(|info| info.plugin != plugin)
        );
        // 别家实例完全看不见这份登记
        assert!(
            Framework::new()
                .container()
                .instance::<DummyService>()
                .is_none()
        );
    }

    trait DiceRoller: Send + Sync {
        fn roll(&self, sides: u64) -> u64;
    }
    struct MyDice;
    impl DiceRoller for MyDice {
        fn roll(&self, sides: u64) -> u64 {
            sides
        }
    }

    /// 接口型服务：`Arc::downcast` 要求键类型 `Sized`，所以暴露 trait 时用 `Arc<dyn Trait>`
    /// 当键类型（它本身是 Sized 的），而不是 `dyn Trait`。
    #[test]
    fn interface_service_roundtrips_as_arc_of_dyn() {
        type Dice = Arc<dyn DiceRoller>;
        let container = ServiceContainer::new(Arc::new(Gating::default()));
        container.declare("InterfaceDice", Arc::new(Arc::new(MyDice) as Dice));
        let dice = container
            .instance::<Dice>()
            .expect("接口服务应按 Arc<dyn Trait> 取回");
        assert_eq!(dice.roll(6), 6);
        assert_eq!(
            container.provider_of::<Dice>().as_deref(),
            Some("InterfaceDice")
        );
        assert_eq!(container.revoke_plugin("InterfaceDice"), 1);
        assert!(container.instance::<Dice>().is_none());
    }
}
