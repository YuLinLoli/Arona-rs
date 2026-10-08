//! 动态插件的 ABI 约定（对应 mirai 的 `plugin.yml` + `@FilePluginMain` 入口）。
//!
//! ## 为什么"直接交换 Rust trait 对象"是可行的
//!
//! 一个 `cdylib` 插件会把自己那份 `arona` 静态链接进去，于是宿主与插件里各有一套
//! 全局静态量（`Framework::global()`、日志 sink、任务表）。若插件用裸 `tokio::spawn`
//! 或自行 `arona::plugin::manager()`，写的就是 dll 自己那份表，宿主看不见——静默失效。
//! 因此本框架的契约是：装载 dll 后，宿主第一时间把自己的**进程默认实例、日志出口、
//! tokio 句柄、HTTP 代发入口**通过 [`HostBridge`] 交回插件（[`export_arona_plugin!`] 生成的
//! `arona_plugin_host` 符号），插件里那一份静态状态当场改指宿主。此后插件无论走
//! [`PluginContext`](super::PluginContext) 还是走 `arona::quartz::*` 这类自由函数，
//! 读写的都是宿主真正在跑的那套表——跨边界传递的 `Arc<dyn AronaPlugin>`
//! 因此只在"同一版 rustc + 同一 target + 同一 profile + 同一 CRT + 同一份 arona 源码"
//! 的前提下才成立。
//!
//! 这个前提编译器管不了，就由 [`abi`](self) 在装载每个 dll 时逐项核对工具链指纹，
//! 对不上直接拒载。请把它当作硬性发布要求：**发布插件时必须连带说明它编译所用的
//! rustc 版本，且必须用 `--release` 与宿主同档构建**（详见 `PLUGIN_DEVELOPMENT.md`）。
//!
//! ## dll 必须导出的四个符号
//!
//! | 符号 | 签名 | 作用 |
//! | --- | --- | --- |
//! | `arona_plugin_abi` | `extern "C" fn() -> u64` | 符号布局版本 + 编译期框架契约版本 |
//! | `arona_plugin_toolchain` | `extern "C" fn() -> *const u8` | 以 NUL 结尾的工具链指纹 |
//! | `arona_plugin_host` | `extern "C" fn(*const HostBridge) -> u8` | 接受宿主的上行通道，0 才算成功 |
//! | `arona_plugin_new` | `extern "C" fn() -> *const dyn AronaPlugin` | 造实例，交出唯一一份强引用 |
//!
//! 插件作者不需要手写它们，一行 [`export_arona_plugin!`] 就够。

use crate::plugin::ApiVersion;
use crate::plugin::description::FRAMEWORK_API_VERSION;
use std::ffi::c_void;
use std::os::raw::c_char;

/// 本模块约定的符号布局版本。签名有任何改动都要抬它，宿主只接受相等的 dll。
///
/// 2 = 上行通道 [`HostBridge`] 带日志/默认实例/tokio 句柄三项；
/// 3 = 再多一项：宿主代发 HTTP（[`HostBridge::http`]）。
/// 4 = [`crate::framework::Framework`] 换了字段布局（多出 `chatlog_api` 一项）。
/// 5 = 插件会读的那几个跨镜像类型换了布局：[`EventBody::Meta`](crate::onebot::hooks::EventBody)
///     多带一个 `MetaInfo`、[`BodyFilter`](crate::onebot::hooks::BodyFilter) 多两档、
///     [`CommandContext`](crate::runtime::dispatcher::CommandContext) 末尾多一个 `framework`、
///     [`OneBotApi`](crate::onebot::api::OneBotApi) 由零宽单元类型变成持有实例引用的结构体。
/// 6 = 排期改走宿主通道（[`HostBridge::jobs`]）。这一版**把定时任务的布局问题一次性了结**：
///     dll 不再编译进 `Scheduler`/`TaskEntry` 的读写代码，那两个结构以后随便改都不碰插件。
/// 7 = 代发 HTTP 能带响应头回来了：[`HttpCall::want_headers`] 与 [`HttpDone`] 末尾那两段
///     `headers`/`header_count`。B 站扫码那份票只在 `crossDomain` 跳转的 `Set-Cookie` 上，
///     插件拿不到响应头就等于拿不到登录态——这条通道是补给插件的一个真缺口。
///
/// **别以为改了 `HostBridge` 才算改 ABI**：[`HostBridge::framework`] 交出去的是擦成
/// `*const c_void` 的实例指针，插件 dll 里那份框架代码按**它自己编译期的字段偏移**去读这个
/// 结构体。往 `Framework` 中间插一个字段、或改动它任何字段的类型，旧 dll 就会读错位置——
/// 那不是"握手失败"而是未定义行为（实测表现为宿主带崩：dll 拿着宿主创建的 `sqlite3*`
/// 句柄去调自己那份 bundled sqlite）。所以 `Framework` 的字段序本身就是 ABI 的一部分，
/// 动它必须抬这个版本号。同理，插件直接读的任何 `pub` 结构体/枚举（新变体、加字段、
/// 改字段类型）也一样要抬，哪怕宿主侧的签名一个字都没动。
pub const ABI_LAYOUT: u32 = 7;

/// 装载器名（写进 `plugins/<id>/plugin.yml` 的 `loader:` 与 GUI 插件列表）
pub const DYNAMIC_LOADER: &str = "dynamic";

/// 握手符号名
pub const ABI_SYMBOL: &str = "arona_plugin_abi";
/// 工具链指纹符号名
pub const TOOLCHAIN_SYMBOL: &str = "arona_plugin_toolchain";
/// 上行通道交接符号名
pub const HOST_SYMBOL: &str = "arona_plugin_host";
/// 实例入口符号名
pub const ENTRY_SYMBOL: &str = "arona_plugin_new";

/// [`attach_host`] 的返回值：接管完成
pub const HOST_ACCEPTED: u8 = 0;

/// 编译本 crate 所用工具链的指纹，由 `crates/arona/build.rs` 注入。
/// 插件通过 [`export_arona_plugin!`] 原样上报，宿主按字符串全等比对。
pub const TOOLCHAIN_FINGERPRINT: &str = env!("ARONA_ABI_FINGERPRINT");

/// [`TOOLCHAIN_FINGERPRINT`] 的 C 字符串版本：末尾多一个 NUL。
///
/// dll 导出的 `arona_plugin_toolchain` 交出去的是裸指针，宿主只能按 C 字符串读到 NUL 为止。
/// Rust 的字符串字面量**不带**结尾 NUL，直接 `.as_ptr()` 会让宿主读过字符串末尾继续找 0——
/// 撞上 0 就算"指纹不同"，撞上非 UTF-8 字节就报编码错误，运气差则踩进未映射内存。
/// 所以对外一律用这一份。
pub const TOOLCHAIN_FINGERPRINT_C: &str = concat!(env!("ARONA_ABI_FINGERPRINT"), "\0");

/// 把布局版本与框架契约版本打包成一个 `u64`：高 32 位是 [`ABI_LAYOUT`]，
/// 低 32 位是 `major << 16 | minor`（patch 不参与握手，与 mirai 的 `ApiVersion` 一致）。
pub const fn pack_abi(api: &ApiVersion) -> u64 {
    ((ABI_LAYOUT as u64) << 32) | ((api.major as u64) << 16) | api.minor as u64
}

/// 解出 [`pack_abi`] 里的布局版本
pub const fn unpack_layout(packed: u64) -> u32 {
    (packed >> 32) as u32
}

/// 解出 [`pack_abi`] 里的框架契约版本（patch 恒为 0，握手不看它）
pub const fn unpack_api(packed: u64) -> ApiVersion {
    ApiVersion::new(
        ((packed >> 16) & 0xFFFF) as u32,
        (packed & 0xFFFF) as u32,
        0,
    )
}

/// 插件 dll 上报的握手信息是否被当前框架接受。
/// 返回 `Ok(())` 只代表"可以试着造实例"，业务级的依赖检查仍归 `PluginManager`。
pub fn check_abi(packed: u64, toolchain: &str) -> Result<(), String> {
    let layout = unpack_layout(packed);
    if layout != ABI_LAYOUT {
        return Err(format!(
            "插件 ABI 布局版本不匹配：插件为 {layout}，本框架为 {ABI_LAYOUT}。请用同版本框架重新编译插件"
        ));
    }
    let built = unpack_api(packed);
    if !FRAMEWORK_API_VERSION.satisfies(&built) {
        return Err(format!(
            "插件按框架契约 {built} 编译，当前框架只有 {FRAMEWORK_API_VERSION}"
        ));
    }
    if toolchain != TOOLCHAIN_FINGERPRINT {
        return Err(format!(
            "插件与框架的构建工具链不一致，无法安全装载。\n  框架: {TOOLCHAIN_FINGERPRINT}\n  插件: {toolchain}\n\
             插件必须与宿主使用同一个 rustc、同一个目标三元组、同一档 profile（release/debug）、\n  同一 CRT 链接方式以及同一版本的 arona 依赖编译。"
        ));
    }
    Ok(())
}

/// 一段字节：指针 + 长度，不靠 NUL 结尾。
///
/// 这样既没有"正文里不许出现 0 字节"这种约束，宿主也不必像读 C 字符串那样扫过末尾——
/// 长度由插件自己报，读越界最多是它自己的内存出事，牵不到宿主身上。
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HttpText {
    pub ptr: *const u8,
    pub len: usize,
}

/// 一对键值（请求头或表单项）
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HttpPair {
    pub name: HttpText,
    pub value: HttpText,
}

/// 插件请宿主代发的一次 HTTP 请求（面向插件的封装见 [`crate::runtime::http`]）。
///
/// 各段字节只指向插件自己内存里的那份值，**只需在本次调用期间有效**：宿主在返回之前
/// 就把它们拷成自己的值，插件随后即可释放。
/// 字段顺序与类型本身就是 ABI 的一部分，改动必须抬 [`ABI_LAYOUT`]。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HttpCall {
    /// 0 = GET，1 = POST
    pub method: u8,
    /// 0 = 不带正文，1 = 表单项（`form`），2 = 原始正文（`body` + `content_type`）
    pub body_kind: u8,
    pub url: HttpText,
    /// 空数组时给空指针，`header_count` 为 0
    pub headers: *const HttpPair,
    pub header_count: usize,
    pub form: *const HttpPair,
    pub form_count: usize,
    /// `body_kind = 2` 时的正文与其 content-type，其余情况给空片段
    pub body: HttpText,
    pub content_type: HttpText,
    /// 超时毫秒数；0 表示用宿主的默认值
    pub timeout_ms: u64,
    /// 非 0 时把**响应头**一并交回（含重定向链上每一跳的 `Set-Cookie`）。
    ///
    /// 默认不要：多数插件只关心正文，逐条拷头是白花的钱。要拿登录票的插件才需要它——
    /// B 站扫码成功那份回执里，票只在这条 `crossDomain` 跳转的 `Set-Cookie` 上。
    pub want_headers: u8,
}

/// 宿主代发完成后回调插件：`error_len` 非 0 即为失败原因，成功时响应字节在
/// `body`/`body_len` 里。`user` 原样带回插件提交时给的那个指针。
///
/// `headers`/`header_count` 是响应头（同名可多条，`Set-Cookie` 就是这种），只有请求里
/// `want_headers` 非 0 才非空；没要或失败时给空指针与 0。头名一律小写（HTTP 头名不区分大小写，
/// 宿主先折一次，插件按名字取就不用自己再折）。跟进去的每一跳只收 `Set-Cookie`，最终那一跳
/// 给全部头——票常常只落在中间那一跳的 `Set-Cookie` 上，跳转由宿主手动跟才收得到它。
///
/// 三段缓冲**只在这次调用期间有效**，插件要用得当场拷走。宿主保证**每次提交恰好回调一次**
/// （解析失败、运行时还没起来这类就地报错也一样补一次回调），插件因此可以把 `user`
/// 当作一次性凭据，在回调里连同自己那份上下文一起销毁。
pub type HttpDone = extern "C" fn(
    user: *mut c_void,
    error: *const u8,
    error_len: usize,
    body: *const u8,
    body_len: usize,
    headers: *const HttpPair,
    header_count: usize,
);

/// [`HostBridge::jobs`] 的操作号。一条通道带多个操作，是为了让这项能力以后加操作时
/// 不必再动 `HostBridge` 的字段数——那才是要抬布局的事。
pub const JOB_CREATE: u32 = 0;
/// 按 `name` 取消一条任务，返回 1 表示确实删掉了一条
pub const JOB_REMOVE: u32 = 1;
/// 按 `name` 问有没有，返回 1 表示在表里
pub const JOB_EXISTS: u32 = 2;
/// 按 `group` 整组取消，返回收掉的条数
pub const JOB_REMOVE_GROUP: u32 = 3;
/// 按 `name` 立刻触发一次，返回 0 成 / 1 没这个名字 / 2 别的失败原因
pub const JOB_TRIGGER: u32 = 4;
/// 暂停/恢复全部任务，返回受影响的条数（**进程级**动作：一个插件按下去会连别人的一起停，
/// 这是自由函数层一直的语义，GUI 的总暂停按钮用的就是它）
pub const JOB_PAUSE_ALL: u32 = 5;
/// 见 [`JOB_PAUSE_ALL`]
pub const JOB_RESUME_ALL: u32 = 6;
/// 逐条回调 `sink`，返回送出去的条数；`sink` 返回 false 就提前收尾
pub const JOB_LIST: u32 = 7;

/// [`JobSpec::kind`] 的三档，与 `quartz` 内部的 `TaskKind` 一一对应。
/// 每天固定钟点，`at` 是 0-23
pub const JOB_DAILY: u32 = 0;
/// 固定间隔，`at` 是秒（宿主侧下限 1 秒，传 0 会退化成空转刷屏的死循环）
pub const JOB_REPEAT: u32 = 1;
/// 单次，`at` 是绝对毫秒时间戳
pub const JOB_SINGLE_AT: u32 = 2;

/// 一次任务操作的输入。
///
/// 与 [`HttpCall`] 同一条约定：`name`/`group` 指向插件自己内存里的 NUL 结尾 UTF-8，
/// **只需在本次调用期间有效**，宿主在返回之前就把它们拷成自己的值。
/// `user` 是插件那侧一个 `Box<JobFn>` 的裸指针，宿主全程只当不透明句柄握着，只做两件事：
/// 到点调 `run(user)`，摘掉条目时调 `dispose(user)` 把那个 Box 还给插件自己释放——
/// 后者必须发生在 dll 还映射着的时候，否则这条就断了：宿主拿着已经消失的代码地址回调。
///
/// 字段顺序与类型本身就是 ABI 的一部分，改动必须抬 [`ABI_LAYOUT`]。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JobSpec {
    /// [`JOB_CREATE`] 到 [`JOB_LIST`] 之一
    pub op: u32,
    /// 任务形态，只有 [`JOB_CREATE`] 看它
    pub kind: u32,
    /// 形态参数：钟点 / 间隔秒 / 绝对毫秒，只有 [`JOB_CREATE`] 看它
    pub at: i64,
    pub name: *const c_char,
    pub group: *const c_char,
    /// 到点回调（[`JOB_CREATE`] 必填，其余档忽略）。返回 false 表示插件那一侧的处理器炸了——
    /// panic 不能穿出 `extern "C"`（那是整个进程 abort），所以插件侧必须自己拦住并报这一笔，
    /// 宿主这边照旧按"这条任务炸了一次"记账（连续炸到阈值就隔离停用）。
    pub run: Option<extern "C" fn(user: *mut c_void) -> bool>,
    /// 释放回调：宿主摘掉这条任务时调用一次（[`JOB_CREATE`] 必填）
    pub dispose: Option<extern "C" fn(user: *mut c_void)>,
    /// [`JOB_LIST`] 的收件口：`info` 只在本次回调期间有效，返回 false 提前收尾
    pub sink: Option<extern "C" fn(info: *const JobInfo, user: *mut c_void) -> bool>,
    /// 插件自己的句柄，宿主原样带回来
    pub user: *mut c_void,
}

/// [`JOB_LIST`] 回调给插件的一条任务概览。
///
/// 两个字符串只在这次回调期间有效（宿主在本次回调内就把它们拷走），`-1` 表示"没有下一次/
/// 还没跑过"——`i64` 里不会出现合法的 -1 时间戳，省掉一层 nullable。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JobInfo {
    pub name: *const c_char,
    pub group: *const c_char,
    pub next_ms: i64,
    pub last_ms: i64,
}

/// 宿主代管的排期通道。返回值见各操作号上的说明；负数留给"通道自己出错"。
pub type Jobs = extern "C" fn(spec: *const JobSpec) -> i64;

/// 装载时把宿主真正在跑的东西接过来，插件里的自由函数才不落空表。
///
/// 字段顺序与签名就是 ABI 的一部分，改动必须抬 [`ABI_LAYOUT`]。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostBridge {
    /// 把插件打的一条日志交给宿主落地：`level` 是 `INFO`/`DEBUG`/`WARNING`/`ERROR`，
    /// `message` 是不带来源的正文，`source` 是 dll 这一侧的线程局部来源（可为空指针）。
    /// 三者都是 NUL 结尾 UTF-8；来源以宿主为准，宿主没标注时才用它兜底。
    pub log: extern "C" fn(level: *const c_char, message: *const c_char, source: *const c_char),
    /// 宿主的进程默认实例（`*const Framework`，擦成 `c_void`）。挂在实例上的状态
    /// （注册表、插件配置文件、框架那份 arona.yml、主动发消息的出口）都跟着这一份走，
    /// 所以插件读它们不必新增通道；**但改 `Framework` 的字段布局必须抬 [`ABI_LAYOUT`]**——
    /// dll 是按自己编译期的偏移读这个结构体的，错位等于未定义行为。同理，插件会直接读的
    /// 任何 `pub` 类型（加变体、加字段、改字段类型）也一样要抬，宿主侧签名一个字没改也算。
    ///
    /// 而且"挂在实例上"只保证 dll **找得到**这份状态，不保证它能亲手操作：dll 里静态链接
    /// 了自己那套依赖库，所以只能经**宿主生成的 vtable**（`Arc<dyn ..>`，如
    /// [`Framework::chatlog_api`](crate::framework::Framework::chatlog_api)）或 `extern "C"`
    /// 函数去用，直接在 dll 里调带第三方句柄的固有方法（rusqlite、reqwest）会当场崩。
    pub framework: extern "C" fn() -> *const c_void,
    /// 宿主登记的 tokio 句柄（`*const Handle`），没有则返回空指针
    pub runtime: extern "C" fn() -> *const c_void,
    /// 请宿主的 HTTP 客户端代发一次请求（面向插件的封装见 [`crate::runtime::http`]）。
    /// 无论成功、失败还是当场就把请求描述判废了，`done` 都会被调用**恰好一次**。
    pub http: extern "C" fn(call: *const HttpCall, done: HttpDone, user: *mut c_void),
    /// 把定时任务交给宿主代管（面向插件的封装见 [`crate::quartz`]）。
    ///
    /// 有了这条通道，插件里就不会再编译出读写 `Scheduler`/`TaskEntry` 的代码——那两个结构
    /// 的字段布局从此不属于 ABI。这一条不是洁癖：`PluginContext::repeat_job` 这类固有方法
    /// 编在 dll 里，dll 按自己编译期的偏移读写宿主的表，错位过一次的实测结果是「握手照放行、
    /// 插件装载启动都正常，但它排的定时任务一条都不跑」，且不报任何错。
    pub jobs: Jobs,
}

/// [`export_arona_plugin!`] 生成的 `arona_plugin_host` 的实现：把插件里的
/// 进程默认实例、日志出口、tokio 句柄、HTTP 代发入口一次性改指宿主。必须在造实例之前调用。
///
/// 返回 0 表示接管完成；非 0 表示失败（宿主据此拒载），因为「各持一份状态」的插件
/// 只是安静地失效，比装载失败更难查。
///
/// # Safety
/// `bridge` 必须是宿主给出、且在进程结束前保持有效的 [`HostBridge`]；
/// 其中的 `framework` 指针指向的实例必须永不被释放。
pub unsafe fn attach_host(bridge: *const HostBridge) -> u8 {
    if bridge.is_null() {
        return 1;
    }
    let bridge = unsafe { *bridge };
    // SAFETY: 约定 `framework` 指向宿主永不被释放的默认实例，`runtime` 指向它登记的句柄
    // （或空指针表示宿主还没有句柄），插件这份代码只在此后借用它们。
    unsafe {
        if !crate::framework::Framework::adopt_host((bridge.framework)()) {
            return 2;
        }
        crate::runtime::reactor::adopt_host((bridge.runtime)());
    }
    crate::runtime::http::attach_host(bridge.http);
    crate::runtime::log::attach_host(bridge);
    crate::quartz::attach_host(bridge.jobs);
    HOST_ACCEPTED
}

/// 导出动态插件入口：在 `crate-type = ["cdylib"]` 的插件 crate 里写一行即可。
///
/// ```ignore
/// arona::export_arona_plugin!(MyPlugin::default());
/// ```
///
/// 参数是构造插件实例的表达式（每次装载只求值一次，由宿主接管其强引用）。
/// 结构体没有字段时可以直接写类型名——`MyPlugin::default()` 会被
/// clippy 的 `default_constructed_unit_structs` 念一句。
#[macro_export]
macro_rules! export_arona_plugin {
    ($instance:expr) => {
        $crate::export_arona_plugin!(@impl $crate::plugin::abi::private_arc($instance));
    };
    (@impl $ctor:expr) => {
        /// 符号布局版本 + 本 dll 编译时所链接的框架契约版本
        #[unsafe(no_mangle)]
        pub extern "C" fn arona_plugin_abi() -> u64 {
            $crate::plugin::abi::pack_abi(&$crate::plugin::FRAMEWORK_API_VERSION)
        }

        /// 本 dll 的构建工具链指纹（以 NUL 结尾，宿主按字符串全等校验）
        #[unsafe(no_mangle)]
        pub extern "C" fn arona_plugin_toolchain() -> *const u8 {
            $crate::plugin::abi::TOOLCHAIN_FINGERPRINT_C.as_ptr()
        }

        /// 接受宿主的上行通道：把本 dll 里的进程默认实例、日志出口、tokio 句柄改指宿主。
        /// 框架在造实例之前调用它，返回非 0 即拒载。
        ///
        /// # Safety
        /// 由框架装载器调用，`bridge` 指向一个进程内长期有效的
        /// [`HostBridge`](::arona::plugin::abi::HostBridge)。
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn arona_plugin_host(
            bridge: *const $crate::plugin::abi::HostBridge,
        ) -> u8 {
            // SAFETY: 框架只在握手通过后调用，并保证指针指向有效的 HostBridge
            unsafe { $crate::plugin::abi::attach_host(bridge) }
        }

        /// 造出插件实例，交出唯一一份 `Arc`（宿主用 `Arc::from_raw` 接管）
        #[unsafe(no_mangle)]
        pub extern "C" fn arona_plugin_new() -> *const dyn $crate::plugin::AronaPlugin {
            let instance: ::std::sync::Arc<dyn $crate::plugin::AronaPlugin> = $ctor;
            ::std::sync::Arc::into_raw(instance)
        }
    };
}

/// 宏内部用：把构造表达式先绑成 `Arc<dyn AronaPlugin>`，避免各插件漏写类型标注。
pub fn private_arc<P>(plugin: P) -> std::sync::Arc<dyn crate::plugin::AronaPlugin>
where
    P: crate::plugin::AronaPlugin,
{
    std::sync::Arc::new(plugin)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HostBridge` 要跨 dll 边界传给插件，字段数量与顺序本身就是 ABI 的一部分：
    /// 布局漂了（多一个字段、忘了 `repr(C)`）宿主就会按错位的方式取函数指针。
    #[test]
    fn host_bridge_is_exactly_five_function_pointers() {
        use std::mem::{align_of, size_of};
        assert_eq!(size_of::<HostBridge>(), 5 * size_of::<usize>());
        assert_eq!(align_of::<HostBridge>(), align_of::<usize>());
    }

    /// 任务描述也是 ABI：`op` 与 `kind` 两个 u32 凑满一个字，其余七个字段各占一个字。
    /// 少一个、多一个或者漂了 `repr(C)`，宿主读到的就是错位的回调指针——那比读错整数严重得多。
    #[test]
    fn job_spec_is_eight_words() {
        const WORD: usize = std::mem::size_of::<usize>();
        assert_eq!(std::mem::size_of::<JobSpec>(), 8 * WORD);
        assert_eq!(std::mem::size_of::<JobInfo>(), 4 * WORD);
    }

    /// 请求描述也是 ABI：两个 u8 凑满一个字，其余字段各占一个字，末尾的 `want_headers`
    /// 自己占一个字（它前面是 u64，对齐到不了隔壁）。
    /// 少一个、多一个或者漂了 `repr(C)`，宿主读到的就是错位的数据。
    #[test]
    fn http_call_is_thirteen_words() {
        use std::mem::size_of;
        assert_eq!(size_of::<HttpCall>(), 13 * size_of::<usize>());
        assert_eq!(size_of::<HttpPair>(), 4 * size_of::<usize>());
    }

    /// 空通道必须被拒收：接管失败还继续装配，插件就会把状态写进自己那份空表里，
    /// 表现出来是"命令登记了但没人路由"这种查不动的静默故障。
    #[test]
    fn a_null_bridge_is_refused() {
        assert_eq!(unsafe { attach_host(std::ptr::null()) }, 1);
    }

    /// 扮一次"插件 dll"：把宿主自己给的上行通道接进来之后，插件视角下的自由函数
    /// （`quartz::create_daily` / `quartz::exists`）必须落进宿主真正在跑的那张任务表。
    /// 这条是"各持一份状态会安静失效"的唯一防线，改坏了插件就只剩静默失效。
    #[test]
    fn adopting_the_host_redirects_the_free_functions() {
        let bridge = crate::runtime::log::host_bridge();
        assert_eq!(
            unsafe { attach_host(bridge) },
            HOST_ACCEPTED,
            "宿主自己给的通道必须接得下"
        );
        let job: crate::quartz::JobFn = std::sync::Arc::new(|| {});
        crate::quartz::create_daily(4, "AbiAdoptProbe", "abi-adopt", job);
        assert!(
            crate::framework::Framework::global()
                .jobs()
                .exists("AbiAdoptProbe"),
            "自由函数登记的任务应落在宿主的任务表里"
        );
        assert!(crate::quartz::exists("AbiAdoptProbe"));
        assert!(crate::quartz::remove("AbiAdoptProbe"));
    }

    #[test]
    fn abi_roundtrip_keeps_layout_and_api() {
        let packed = pack_abi(&ApiVersion::new(1, 7, 3));
        assert_eq!(unpack_layout(packed), ABI_LAYOUT);
        // patch 不参与握手
        assert_eq!(unpack_api(packed), ApiVersion::new(1, 7, 0));
    }

    /// 交给宿主的是裸指针，只能按 C 字符串读：漏了结尾 NUL，宿主会读过指纹继续找 0，
    /// 报出来的原因变成看不懂的「工具链不一致」，运气差还会扫进未映射的内存。
    #[test]
    fn reported_fingerprint_is_a_c_string() {
        let bytes = TOOLCHAIN_FINGERPRINT_C.as_bytes();
        assert_eq!(bytes.last(), Some(&0), "导出的指纹必须带 NUL 结尾");
        let read_back = std::ffi::CStr::from_bytes_with_nul(bytes)
            .expect("指纹中间不该有 NUL")
            .to_str()
            .expect("指纹是 UTF-8");
        assert_eq!(read_back, TOOLCHAIN_FINGERPRINT);
    }

    #[test]
    fn fingerprint_carries_the_whole_toolchain() {
        // 指纹缺一环都会让"能编译但会随机崩"的插件混进来
        for token in [
            "rustc/",
            "host/",
            "target/",
            "profile/",
            "crt-static/",
            "features/",
        ] {
            assert!(
                TOOLCHAIN_FINGERPRINT.contains(token),
                "工具链指纹缺少 {token}: {TOOLCHAIN_FINGERPRINT}"
            );
        }
    }

    /// `gui` 是唯一被放行、不进指纹的 feature：它必须只门控框架自己的界面与启动辅助，
    /// 绝不能出现在插件契约面上——否则插件与宿主的类型布局就不一致了。
    #[test]
    fn gui_feature_stays_out_of_the_plugin_facing_types() {
        use std::path::Path;

        // 允许带这个 cfg 的文件：模块声明处与 GUI 自身
        const ALLOWED: &[&str] = &["src/lib.rs", "src/runtime/mod.rs", "src/gui"];
        // 拆开写，免得本文件把自己的源码匹配成命中项
        const NEEDLE: &str = concat!("feature = \"gu", "i\"");
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut stack = vec![manifest.join("src")];
        let mut offenders = Vec::new();
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(relative) = path.strip_prefix(manifest) else {
                    continue;
                };
                if ALLOWED.iter().any(|ok| relative.starts_with(ok)) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                if text.contains(NEEDLE) {
                    offenders.push(relative.to_path_buf());
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "这些文件按 gui 门控了代码，插件与宿主的类型布局可能已不一致: {offenders:?}"
        );
    }

    #[test]
    fn check_abi_rejects_each_mismatch() {
        let mine = TOOLCHAIN_FINGERPRINT.to_string();
        assert!(check_abi(pack_abi(&FRAMEWORK_API_VERSION), &mine).is_ok());
        assert!(
            check_abi(pack_abi(&FRAMEWORK_API_VERSION), "arona/0.0.0|rustc/9.9.9")
                .is_err_and(|reason| reason.contains("工具链不一致"))
        );
        let newer = ApiVersion::new(
            FRAMEWORK_API_VERSION.major,
            FRAMEWORK_API_VERSION.minor + 1,
            0,
        );
        assert!(
            check_abi(pack_abi(&newer), &mine)
                .is_err_and(|reason| reason.contains("插件按框架契约"))
        );
        assert!(
            check_abi((ABI_LAYOUT as u64 + 7) << 32, &mine)
                .is_err_and(|reason| reason.contains("ABI 布局版本"))
        );
    }
}
