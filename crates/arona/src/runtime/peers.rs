//! 远端资料的按需读缓存（mirai 的 `MemberRead` / `FriendlySource` 那一层）。
//!
//! ## 它解决的是"句柄背后没有缓存"
//!
//! mirai 里 `group[123]`、`bot.getUser(id)` 拿到的是对象，资料由读服务按需取、取一次留一会；
//! 我们原先每个 [`Group::member`](crate::bot::Group::member) / [`Member::info`](crate::bot::Member::info)
//! 都实打实问一次实现端，一句"先查身份再查昵称"的插件就是两次网络往返。
//! 更早的 [`crate::runtime::dispatcher::CommandContext::group_role`] 则自带一张进程级 `static`
//! 表——那是第一道关没过（状态没挂在 [`Framework`](crate::framework::Framework) 实例上）。
//!
//! 现在统一收成实例上这一份，键是"(群号, QQ 号)"与"QQ 号"，值是实现端原样回的资料结构。
//!
//! ## 跨镜像为什么可以直接用
//!
//! 里面只有 `Mutex<HashMap<String,...>>` 这类纯 Rust 状态，没有 C 语言句柄：dll 拿着宿主
//! 实例上的这张表调自己那份 `TtlCache` 代码，读写的是同一块系统堆内存，行为与宿主一致
//! （对比 [`crate::runtime::chatlog`] 那份 sqlite 句柄——那种才必须走 trait 出口）。
//!
//! ## 失效只有两条路
//!
//! 一是到期自动不作数（[`TtlCache`] 读时判过期），二是群/人相关的通知事件主动作废
//! （见 [`PeerCache::forget_member`] 等的调用点）。没有"后台清理任务"这一条：
//! 这种表撑死几千条，犯不着为它占一条定时任务。

use crate::onebot::api::{GroupMemberInfo, StrangerInfo};
use crate::runtime::cache::TtlCache;
use std::time::Duration;

/// 群成员资料留 60 秒：群名片、头衔、身份一天里改不了几次，而命令门控每条消息都要看它
const MEMBER_TTL: Duration = Duration::from_secs(60);

/// 陌生人资料留 10 分钟：`get_stranger_info` 在 NTQQ 系上是个慢动作，且它比群成员更少见
const STRANGER_TTL: Duration = Duration::from_secs(600);

/// 单张表的条数上限，与搬走前那张 `role_cache` 一个口径
const CACHE_MAX: usize = 4096;

/// 实例上的远端资料缓存。挂在 `Framework` 上，所以隔离实例各查各的、互不串台。
#[derive(Default)]
pub struct PeerCache {
    members: TtlCache<(i64, i64), GroupMemberInfo>,
    strangers: TtlCache<i64, StrangerInfo>,
}

impl PeerCache {
    pub fn new() -> Self {
        PeerCache {
            members: TtlCache::new(MEMBER_TTL, CACHE_MAX),
            strangers: TtlCache::new(STRANGER_TTL, CACHE_MAX),
        }
    }

    /// 群成员资料表（`(群号, QQ 号)` 为键）
    pub fn members(&self) -> &TtlCache<(i64, i64), GroupMemberInfo> {
        &self.members
    }

    /// 陌生人资料表（QQ 号为键）
    pub fn strangers(&self) -> &TtlCache<i64, StrangerInfo> {
        &self.strangers
    }

    /// 某个人的群内资料不再可信：退群、被踢、被设/撤管理员、改了群名片都算
    pub fn forget_member(&self, group_id: i64, user_id: i64) {
        self.members.remove(&(group_id, user_id));
    }

    /// 整个群的成员资料一起作废：机器人群成员列表变动、或实现端重连之后
    pub fn forget_group(&self, group_id: i64) {
        self.members.remove_where(|(group, _)| *group == group_id);
    }

    /// 一个人跨群的所有资料都作废（被全局封禁、注销这种没给出群号的情形用它兜底）
    pub fn forget_everywhere(&self, user_id: i64) {
        self.members.remove_where(|(_, member)| *member == user_id);
        self.strangers.remove(&user_id);
    }

    pub fn clear(&self) {
        self.members.clear();
        self.strangers.clear();
    }

    /// 现存条数（含还没被读到的过期项），只给日志与探针用
    pub fn len(&self) -> usize {
        self.members.len() + self.strangers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onebot::api::GroupMemberInfo;

    #[test]
    fn a_hit_is_reused_and_a_forget_forces_the_next_read() {
        let cache = PeerCache::new();
        cache.members().put(
            (20001, 100),
            GroupMemberInfo {
                group_id: 20001,
                user_id: 100,
                card: "老师".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(
            cache
                .members()
                .get(&(20001, 100))
                .map(|info| info.card)
                .as_deref(),
            Some("老师")
        );

        cache.forget_member(20001, 100);
        assert!(cache.members().get(&(20001, 100)).is_none());
    }

    /// 群成员是成批变的：一条"整群作废"要把这个群的键清干净，又不能碰别的群
    #[test]
    fn forgetting_a_group_leaves_the_others_alone() {
        let cache = PeerCache::new();
        for (group, user) in [(20001, 100), (20001, 101), (20002, 100)] {
            cache.members().put(
                (group, user),
                GroupMemberInfo {
                    group_id: group,
                    user_id: user,
                    ..Default::default()
                },
            );
        }
        cache.forget_group(20001);
        assert!(cache.members().get(&(20001, 100)).is_none());
        assert!(cache.members().get(&(20001, 101)).is_none());
        assert!(cache.members().get(&(20002, 100)).is_some());

        // 没带群号的变动只能跨群一起清
        cache.forget_everywhere(100);
        assert!(cache.members().get(&(20002, 100)).is_none());
    }
}
