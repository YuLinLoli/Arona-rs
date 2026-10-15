//! 会话展示板：插件把自己"在某个会话里订了些什么"交给宿主，框架 GUI 按通用形状列出来。
//!
//! ## 为什么框架需要一个这样的板子，又不该知道内容
//!
//! 框架的硬边界是**框架本体里不许出现任何插件信息**：它不读 `data/<插件>/store.yml`，也不认得
//! "UP 主""番剧"这类业务词。这里存的只有 `(会话号, 插件 id, 一行行的"标题 + 条目")`，
//! 标题与条目都是插件给的字符串——GUI 负责把它们排出来，框架自己一个字都不解释。
//!
//! ## 为什么写入走宿主通道，而不是把表交给插件读写
//!
//! 跟定时任务同一回事（见 [`crate::quartz`]）：插件 dll 把 `arona` 连同依赖静态链接成第二份，
//! dll 里的固有方法会按**它自己编译期的字段偏移**去读写宿主实例上的结构体。错位过的实测结果是
//! 「握手照放行、装载与启动都正常，但插件排的任务一条都不跑」且不报错。所以本表的**结构布局
//! 不出边界**：插件侧只经 [`crate::plugin::abi::HostBridge::views`] 递一个 [`ViewSpec`]
//! 过来，读写宿主的表由宿主这份代码做。
//!
//! ## 跨界只传一段 C 字符串
//!
//! 行数与条目数都是外来的，把它们摆成嵌套数组会让插件那边多背一层分配与释放的约定。所以
//! [`ViewSpec::rows`] 是一段文本，行之间用 `\n`、行内标题与条目之间用 `\t`：
//!
//! ```text
//! UP 主\t央视新闻\t哔哩哔哩会员购\n番剧\t碧蓝之海
//! ```
//!
//! 分隔符是制表符与换行——这两样都不会出现在给人看的名字里。真正会混进名字的别的控制字符（以及
//! NUL，那会让 C 字符串直接截断）由插件侧的 [`sanitize`] 在拼串之前统一换掉，宿主读的时候就不再
//! 需要猜。条目为空的那一行表示"这一档现在没东西"，仍然算一行。

use crate::plugin::abi::{VIEW_CLEAR, VIEW_CLEAR_PLUGIN, VIEW_PUBLISH, ViewSpec, Views};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::sync::OnceLock;

/// 展示板里的一行：给人看的标题，加上归在它名下的条目。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewRow {
    pub label: String,
    pub items: Vec<String>,
}

impl ViewRow {
    /// 造一行。条目里那些空字符串与只含空白的写法在这儿就丢掉——它们到了 GUI 里只会变成空行。
    pub fn new(
        label: impl Into<String>,
        items: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        ViewRow {
            label: sanitize(&label.into()),
            items: items
                .into_iter()
                .map(|item| sanitize(&item.into()))
                .filter(|item| !item.trim().is_empty())
                .collect(),
        }
    }
}

/// 一张表：会话号 → (插件 id → 行)。表本身住在 [`crate::framework::Framework`] 实例上。
#[derive(Default)]
pub struct ViewBoard {
    entries: Mutex<BTreeMap<i64, BTreeMap<String, Vec<ViewRow>>>>,
}

impl ViewBoard {
    /// 用 `rows` 覆盖该插件在该会话上的行。给空集合等于 [`clear`](Self::clear)。
    pub fn publish(&self, plugin: &str, peer: i64, rows: Vec<ViewRow>) {
        let mut table = self.lock();
        if rows.is_empty() {
            if let Some(by_plugin) = table.get_mut(&peer) {
                by_plugin.remove(plugin);
                if by_plugin.is_empty() {
                    table.remove(&peer);
                }
            }
            return;
        }
        table
            .entry(peer)
            .or_default()
            .insert(plugin.to_string(), rows);
    }

    /// 摘掉该插件在该会话上的行，返回是不是真摘掉过一行
    pub fn clear(&self, plugin: &str, peer: i64) -> bool {
        let mut table = self.lock();
        let Some(by_plugin) = table.get_mut(&peer) else {
            return false;
        };
        let gone = by_plugin.remove(plugin).is_some();
        if by_plugin.is_empty() {
            table.remove(&peer);
        }
        gone
    }

    /// 摘掉某插件名下所有会话的行（停用/卸载时由框架调用），返回收掉的会话数
    pub fn clear_plugin(&self, plugin: &str) -> usize {
        let mut table = self.lock();
        let before = table.len();
        for by_plugin in table.values_mut() {
            by_plugin.remove(plugin);
        }
        table.retain(|_, by_plugin| !by_plugin.is_empty());
        before - table.len()
    }

    /// 某个会话的展示行，按插件 id 排。GUI 只有在来路不止一个插件时才标插件名。
    pub fn rows_of(&self, peer: i64) -> Vec<(String, ViewRow)> {
        let mut out = Vec::new();
        for (plugin, rows) in &self.lock().get(&peer).cloned().unwrap_or_default() {
            out.extend(rows.iter().cloned().map(|row| (plugin.clone(), row)));
        }
        out
    }

    /// 某个插件在所有会话上的行，按会话号升序（GUI「插件管理」页详情用）。
    ///
    /// 与 [`rows_of`](Self::rows_of) 同一份表、只是换个方向索引：框架照旧只搬运不解释，
    /// `label` 与 `items` 里是什么字都由插件自己给。
    pub fn rows_of_plugin(&self, plugin: &str) -> Vec<(i64, ViewRow)> {
        let mut out = Vec::new();
        for (peer, by_plugin) in self.lock().iter() {
            if let Some(rows) = by_plugin.get(plugin) {
                out.extend(rows.iter().cloned().map(|row| (*peer, row)));
            }
        }
        out.sort_by_key(|(peer, _)| *peer);
        out
    }

    /// 该会话有没有东西（GUI 决定要不要在列表行里挂那个标记）
    pub fn has(&self, peer: i64) -> bool {
        self.lock().contains_key(&peer)
    }

    /// 诊断与单测用：整张表的快照
    pub fn snapshot(&self) -> Vec<(i64, String, Vec<ViewRow>)> {
        let mut out = Vec::new();
        for (peer, by_plugin) in self.lock().iter() {
            for (plugin, rows) in by_plugin {
                out.push((*peer, plugin.clone(), rows.clone()));
            }
        }
        out
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<i64, BTreeMap<String, Vec<ViewRow>>>> {
        // 锁中毒只可能是某次 publish 的参数在 clone 时崩了（外来字符串不会造成这种 panic），
        // 展示板是给人看一眼的东西，别把宿主整条命令判死
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

// ==================== 插件侧：拼串与递交 ====================

/// 把控制字符换成空格：`\t`/`\n` 会被当成字段边界，NUL 会让 C 字符串就地截断。
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\t' | '\n' | '\r' | '\u{0}' => ' ',
            other => other,
        })
        .collect()
}

/// 把若干行拼成 [`ViewSpec::rows`] 那段文本
fn encode(rows: &[ViewRow]) -> String {
    rows.iter()
        .map(|row| {
            let mut line = String::from(&row.label);
            for item in &row.items {
                line.push('\t');
                line.push_str(item);
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`encode`] 的逆。宿主与"没装载过插件"的那一路共用这一份解析，两边看到的形状才必然一致。
fn decode(text: &str) -> Vec<ViewRow> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut parts = line.split('\t');
            ViewRow {
                label: parts.next().unwrap_or_default().to_string(),
                items: parts.map(str::to_string).collect(),
            }
        })
        .collect()
}

/// 递交用的通道。恒空于宿主自己那份镜像——没有那道边界时直接操作实例上的表。
static VIEWS: OnceLock<Views> = OnceLock::new();

/// 装载时把宿主的展示板入口接过来（由 `plugin::abi::attach_host` 调用）
pub(crate) fn attach_host(views: Views) {
    let _ = VIEWS.set(views);
}

/// 该不该走通道：通道在 **且** 目标实例就是进程默认实例。少了第二条，插件回归测试里
/// `Framework::new()` 那套隔离实例上报的行会跑到全局表上去（跟 [`crate::quartz`] 同一个坑）。
fn bridge_for(target: &crate::framework::Framework) -> Option<Views> {
    VIEWS
        .get()
        .copied()
        .filter(|_| std::ptr::eq(target, crate::framework::Framework::global()))
}

/// 用 `rows` 覆盖某插件在某会话上的展示行。
///
/// 返回是不是真交出去了（`false` 只可能是通道内部异常——展示板不是关键路径，插件不必处理）。
pub(crate) fn submit_publish(
    target: &crate::framework::Framework,
    plugin: &str,
    peer: i64,
    rows: &[ViewRow],
) -> bool {
    let text = encode(rows);
    match bridge_for(target) {
        None => {
            target.views().publish(plugin, peer, decode(&text));
            true
        }
        Some(views) => {
            let (Ok(plugin), Ok(rows)) = (CString::new(plugin), CString::new(text)) else {
                return false;
            };
            let spec = ViewSpec {
                op: VIEW_PUBLISH,
                plugin: plugin.as_ptr(),
                peer,
                rows: rows.as_ptr(),
            };
            views(&spec) >= 0
        }
    }
}

/// 摘掉某插件在某会话上的展示行
pub(crate) fn submit_clear(target: &crate::framework::Framework, plugin: &str, peer: i64) {
    match bridge_for(target) {
        None => {
            target.views().clear(plugin, peer);
        }
        Some(views) => call_plugin(views, VIEW_CLEAR, plugin, peer),
    }
}

/// 摘掉某插件名下所有会话的展示行（插件被停用时）
pub(crate) fn submit_clear_plugin(target: &crate::framework::Framework, plugin: &str) {
    match bridge_for(target) {
        None => {
            target.views().clear_plugin(plugin);
        }
        Some(views) => call_plugin(views, VIEW_CLEAR_PLUGIN, plugin, 0),
    }
}

fn call_plugin(views: Views, op: u32, plugin: &str, peer: i64) {
    let Ok(plugin) = CString::new(plugin) else {
        return;
    };
    let spec = ViewSpec {
        op,
        plugin: plugin.as_ptr(),
        peer,
        rows: std::ptr::null(),
    };
    views(&spec);
}

// ==================== 宿主侧：读进来并落到表上 ====================

/// 宿主侧：接住插件递来的一次展示板操作。返回值：`>=0` 是这次收下的行数，负数是通道自己出错。
///
/// # Safety
/// `spec` 由插件给出，可以指向任意内存：判空、非 UTF-8、外来超长都在这里就地变成返回码。
/// panic 绝不穿出 `extern "C"`——那是整个进程 abort，插件手滑不该把宿主带崩。
pub(crate) extern "C" fn host_views(spec: *const ViewSpec) -> i64 {
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| match unsafe { spec.as_ref() } {
        None => -1,
        Some(spec) => host_view_op(spec),
    }));
    match outcome {
        Ok(count) => count,
        Err(_) => {
            crate::runtime::log::error("会话展示板通道内部异常，这次上报已丢弃");
            -2
        }
    }
}

/// 一段上报最多收多少字节、多少行、一行多少条：插件给的是外来数据，
/// 这里只挡"跑飞了把宿主堆撑爆"那种量级，正常一张板子远到不了这些线。
const TEXT_LIMIT: usize = 256 * 1024;
const ROW_LIMIT: usize = 128;
const ITEM_LIMIT: usize = 512;

fn host_view_op(spec: &ViewSpec) -> i64 {
    let plugin = crate::quartz::read_c(spec.plugin);
    if plugin.is_empty() {
        return -1;
    }
    let board = crate::framework::Framework::global().views();
    match spec.op {
        VIEW_PUBLISH => {
            let text = crate::quartz::read_c(spec.rows);
            if text.len() > TEXT_LIMIT {
                crate::runtime::log::warning(format!(
                    "插件 {plugin} 的会话展示上报超长（{} 字节），已丢弃",
                    text.len()
                ));
                return -1;
            }
            let mut rows = decode(&text);
            rows.truncate(ROW_LIMIT);
            for row in &mut rows {
                row.items.truncate(ITEM_LIMIT);
            }
            let count = rows.iter().map(|row| row.items.len()).sum::<usize>();
            board.publish(&plugin, spec.peer, rows);
            // 这一行是端到端唯一能指着说"插件的订阅表真报上来了"的凭据：GUI 那块板子看不见摸不着，
            // 而"偏移编错了、插件只是看不见条目"这类缺陷不会崩，只会安静地空着。
            crate::runtime::log::debug(format!(
                "[展示板] 插件 {plugin} 的会话 {} 收下 {count} 项",
                spec.peer
            ));
            count as i64
        }
        VIEW_CLEAR => {
            let gone = board.clear(&plugin, spec.peer);
            if gone {
                crate::runtime::log::debug(format!(
                    "[展示板] 摘掉插件 {plugin} 的会话 {}",
                    spec.peer
                ));
            }
            i64::from(gone)
        }
        VIEW_CLEAR_PLUGIN => {
            let sessions = board.clear_plugin(&plugin);
            crate::runtime::log::debug(format!(
                "已摘掉插件 {plugin} 的展示条目（{sessions} 个会话）"
            ));
            sessions as i64
        }
        other => {
            crate::runtime::log::warning(format!("插件 {plugin} 报了未知的展示板操作号 {other}"));
            -1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE: &str = "ViewsProbe";
    const PEER: i64 = 88001;

    /// 摆一份跨边界的描述并调一次通道。两段 C 字符串在 `host_views` 返回之前必须活着——
    /// 宿主就是在这一次调用期内把它们读成自己的值的。
    fn report(op: u32, plugin: &str, peer: i64, rows: &str) -> i64 {
        let (who, body) = (CString::new(plugin), CString::new(rows));
        match (who, body) {
            (Ok(who), Ok(body)) => host_views(&ViewSpec {
                op,
                plugin: who.as_ptr(),
                peer,
                rows: body.as_ptr(),
            }),
            // 测试自己造不出合法输入，这算用例失效而不是通道的问题
            _ => -999,
        }
    }

    #[test]
    fn rows_survive_the_channel_shape() {
        let rows = vec![
            ViewRow::new("UP 主", ["央视新闻", "哔哩哔哩会员购"]),
            ViewRow::new("番剧", ["碧蓝之海"]),
            // 空的那一档也要留得下：GUI 靠它区分"没登记"和"登记了但现在没东西"
            ViewRow::new("直播", Vec::<String>::new()),
        ];
        assert_eq!(decode(&encode(&rows))[..], rows[..]);
        assert_eq!(decode("").len(), 0);
        assert_eq!(decode("\n \n").len(), 0, "空白行不该变成一条空条目");
    }

    /// 名字里带控制字符时不能把行 boundary 撑开：那是外来数据唯一能伪造结构的方式。
    #[test]
    fn a_foreign_name_cannot_forge_a_row_boundary() {
        let row = ViewRow::new("UP\t主\n伪造", ["甲\x00乙", "丙\n丁"]);
        assert!(!row.label.contains('\t') && !row.label.contains('\n'));
        let encoded = encode(&[row]);
        let back = decode(&encoded);
        assert_eq!(back.len(), 1, "一个名字不该变成两行");
        assert_eq!(
            back[0].items.len(),
            2,
            "条目里不该多出伪造的第三条: {back:?}"
        );
        assert_eq!(back[0].items[1], "丙 丁");
    }

    #[test]
    fn the_board_covers_clears_and_revokes_by_plugin() {
        let board = ViewBoard::default();
        board.publish("p1", PEER, vec![ViewRow::new("订阅", ["A"])]);
        board.publish("p2", PEER, vec![ViewRow::new("收藏", ["B", "C"])]);
        let rows = board.rows_of(PEER);
        assert_eq!(rows.len(), 2, "两家插件的行都要看得见");
        // 排的是插件 id（GUI 再按它分组），不是 publish 的先后
        assert_eq!(rows[0].0, "p1");
        assert_eq!(rows[1].0, "p2");

        // 覆盖式：同一个插件再报一次是以这次为准，不去追加
        board.publish("p1", PEER, vec![ViewRow::new("订阅", ["A", "A2"])]);
        assert_eq!(board.rows_of(PEER).len(), 2);
        assert_eq!(board.rows_of(PEER)[0].1.items, ["A", "A2"]);

        assert!(board.clear("p2", PEER));
        assert!(!board.clear("p2", PEER), "没登记过不该报成摘掉了");
        assert_eq!(board.rows_of(PEER).len(), 1);

        // 空集合等于清掉，不留一行空壳
        board.publish("p1", PEER, vec![]);
        assert!(!board.has(PEER), "整张会话都该跟着消失");
        assert_eq!(board.clear_plugin("p1"), 0);

        board.publish("p1", PEER, vec![ViewRow::new("订阅", ["A"])]);
        board.publish("p1", PEER + 1, vec![ViewRow::new("订阅", ["B"])]);
        assert_eq!(
            board.clear_plugin("p1"),
            2,
            "按插件回收要一次收干净所有会话"
        );
        assert!(board.snapshot().is_empty());
    }

    /// 跨边界的那一手：坏描述只能变成返回码，不许把宿主带崩（panic 穿出 extern "C" 是整进程 abort）
    #[test]
    fn a_broken_report_becomes_a_return_code_not_a_crash() {
        assert_eq!(host_views(std::ptr::null()), -1);
        assert_eq!(
            report(VIEW_PUBLISH, "", PEER, "订阅\tA"),
            -1,
            "报了空插件 id 的不上板"
        );
        assert_eq!(
            report(700, PROBE, PEER, "订阅\tA"),
            -1,
            "未知操作号要当场拒"
        );
        assert_eq!(
            report(
                VIEW_PUBLISH,
                PROBE,
                PEER,
                &format!("订阅\t{}", "名".repeat(TEXT_LIMIT))
            ),
            -1,
            "超长上报该拒掉而不是照收"
        );
        // 88999 这一格别的用例不碰：清一个从没登记过的会话该给 0 而不是负数
        assert_eq!(report(VIEW_CLEAR, PROBE, 88999, ""), 0);
    }

    /// 正面那一路：一次合法上报真落到宿主的表上，GUI 侧的读口取得到，插件被停用时按 id 收干净。
    /// 这一条是"偏移编错了插件只是看不见条目"那种静默缺陷的唯一牙齿。
    #[test]
    fn a_published_report_lands_on_the_host_board_and_is_revoked_by_id() {
        let board = crate::framework::Framework::global().views();
        board.clear_plugin(PROBE);

        assert_eq!(
            report(
                VIEW_PUBLISH,
                PROBE,
                PEER,
                "UP 主\t央视新闻\t会员购\n番剧\t碧蓝之海"
            ),
            3,
            "返回的该是这次收下的人数"
        );
        let rows = board.rows_of(PEER);
        assert_eq!(rows.len(), 2, "两家一档：UP 主与番剧");
        assert_eq!(rows[0].0, PROBE);
        assert_eq!(rows[0].1.label, "UP 主");
        assert_eq!(rows[0].1.items, ["央视新闻", "会员购"]);
        assert_eq!(rows[1].1.label, "番剧");

        assert_eq!(report(VIEW_CLEAR, PROBE, PEER, ""), 1);
        assert!(board.rows_of(PEER).is_empty());

        assert_eq!(report(VIEW_PUBLISH, PROBE, PEER, "UP 主\t央视新闻"), 1);
        assert_eq!(
            report(VIEW_CLEAR_PLUGIN, PROBE, 0, ""),
            1,
            "停用回收要一次收掉它所有会话"
        );
        assert!(board.rows_of(PEER).is_empty());
    }
}
