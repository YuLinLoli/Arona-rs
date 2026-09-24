//! 带有效期的读缓存原语（mirai 的 `subMemberReadService` 那一类"按需读、读一次留一会"）。
//!
//! ## 为什么要有这么个东西
//!
//! 框架里有一批"事实并不在本机、要问实现端才知道"的读操作：某人在某个群是什么身份、
//! 某人的资料长什么样。mirai 把这些做成了带缓存的读服务——`group[123]` 取十次只问一次。
//! 我们原先是每次现问（一次 `get_group_member_info` 往返），而 `group_role()` 那份私有的
//! 60 秒表又是**进程级 `static`**：它绕过了 [`Framework`](crate::framework::Framework) 实例，
//! 隔离实例跑测试时看到的是别家的数据，插件 dll 里更是读到自己那份空表。
//!
//! 所以这里给一个通用容器，实例上挂一份（见 [`crate::runtime::peers::PeerCache`]），
//! 谁要缓存远端读结果就用它，不再各自手写 `OnceLock<Mutex<HashMap<..>>>`。
//!
//! ## 为什么过期只做到"读的时候才发现"
//!
//! 起一个清理任务要占一条定时任务、还得处理框架关闭时的取消；这类表满打满算几千条，
//! 读时判过期就地删、写时超上限整体收敛一次，代价低到不值得为它排任何任务。

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 键值都带有效期的表：读时判过期，写时按上限收敛
pub struct TtlCache<K, V> {
    ttl: Duration,
    max: usize,
    entries: Mutex<HashMap<K, (V, Instant)>>,
}

impl<K: Eq + Hash + Clone, V: Clone> TtlCache<K, V> {
    /// `max` 是条数上限：写满时先丢掉已过期的，仍然超上限就整表重来。
    /// 机器人待的群只会越来越多，不设上限就等于让这张表只涨不落。
    pub fn new(ttl: Duration, max: usize) -> Self {
        TtlCache {
            ttl,
            max: max.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 有效期（构造时给的那个值，读缓存的调用方要知道它）
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// 取值：没存过、或存过但已经过期时给 `None`（过期的那条会被顺手删掉）
    pub fn get(&self, key: &K) -> Option<V> {
        let Ok(mut entries) = self.entries.lock() else {
            return None;
        };
        let Some((value, at)) = entries.get(key).cloned() else {
            return None;
        };
        if at.elapsed() < self.ttl {
            return Some(value);
        }
        entries.remove(key);
        None
    }

    /// 存值：写满时先清过期项，清完还满就整表丢掉
    pub fn put(&self, key: K, value: V) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if entries.len() >= self.max {
            entries.retain(|_, (_, at)| at.elapsed() < self.ttl);
            if entries.len() >= self.max {
                entries.clear();
            }
        }
        entries.insert(key, (value, Instant::now()));
    }

    /// 作废一个键：远端事实变了（人退群了、名片改了）就要主动删，不能等过期
    pub fn remove(&self, key: &K) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.remove(key);
    }

    /// 按键批量作废，返回删掉的条数。"某人不在群里了"要跨多个键一起清时用它。
    pub fn remove_where(&self, predicate: impl Fn(&K) -> bool) -> usize {
        let Ok(mut entries) = self.entries.lock() else {
            return 0;
        };
        let before = entries.len();
        entries.retain(|key, _| !predicate(key));
        before - entries.len()
    }

    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }

    /// 现存条数（含还没被读到的过期项）：只给日志与断言用
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K, V> Default for TtlCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn default() -> Self {
        // 60 秒 / 4096 条沿用 group_role 原来的那份口径
        Self::new(Duration::from_secs(60), 4096)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 过期必须是"读的时候才发现"，而不是靠一个后台任务去扫
    #[test]
    fn an_expired_entry_reads_as_a_miss_and_disappears() {
        let cache = TtlCache::new(Duration::from_millis(20), 8);
        cache.put("a", 1);
        assert_eq!(cache.get(&"a"), Some(1));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(cache.get(&"a"), None, "过期后不该再给值");
        assert!(cache.is_empty(), "读到过期项要就地删掉，不能留在表里");
    }

    /// 写满时先清过期项；全是新鲜的才整表丢掉——总之不能无上限地涨
    #[test]
    fn a_full_cache_shrinks_instead_of_growing() {
        let cache = TtlCache::new(Duration::from_secs(60), 3);
        for i in 0..3 {
            cache.put(i, i);
        }
        assert_eq!(cache.len(), 3);
        cache.put(9, 9);
        assert_eq!(cache.len(), 1, "都还新鲜时整表重来，只留刚写的这条");
    }

    #[test]
    fn remove_where_only_touches_the_matching_keys() {
        let cache = TtlCache::new(Duration::from_secs(60), 8);
        for (group, user) in [(1, 10), (1, 11), (2, 12)] {
            cache.put((group, user), user);
        }
        assert_eq!(cache.remove_where(|key| key.0 == 1), 2);
        assert_eq!(cache.get(&(1, 10)), None);
        assert_eq!(cache.get(&(2, 12)), Some(12), "别的群不该被牵连");
    }
}
