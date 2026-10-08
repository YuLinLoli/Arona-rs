//! 框架内置的 HTTP 出口：插件不必自带客户端
//!
//! ## 为什么请求要过一道边界
//!
//! dll 把自己那份 reqwest/hyper/tokio 静态链接了进来。hyper 在需要时会**自己** `tokio::spawn`
//! 后台任务（补连接、空闲驱逐），而那些任务第二次醒来时，dll 那份 tokio 的 thread-local
//! 上下文是空的——框架只能给"自己投出去的任务"逐帧补装上下文（见 [`reactor`](super::reactor)），
//! 管不到第三方库内部派生的任务。于是插件自己开着连接池发请求就会炸
//! `there is no reactor running`，炸在池子的锁里还会连带把之后每个请求都拖死。
//!
//! 所以这里把 HTTP 做成宿主的预制方法：插件侧只把请求描述递过
//! [`HostBridge::http`](crate::plugin::abi::HostBridge::http)，真正发请求的是**宿主那份**
//! reqwest 客户端与宿主的 runtime，连接池、keepalive、超时全开也不会炸。插件从此不必依赖
//! reqwest，dll 也跟着小一圈。
//!
//! 等结果用的是一只 oneshot：它的 `Receiver` 不需要任何 reactor，插件就算在没有 tokio
//! 上下文的地方 `await` 也不会出事。
//!
//! 宿主自己（以及从没被装载过的单元测试）没有这道边界，[`send`] 直接走本地客户端。

use crate::plugin::abi::{HttpCall, HttpDone, HttpPair, HttpText};
use std::ffi::c_void;
use std::ptr::null;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;

/// 插件没指定超时时用这一档
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// 单段字节的长度上限：正常 URL 几十字节，这里只挡指针与长度写歪了的插件
const TEXT_LIMIT: usize = 8 * 1024 * 1024;
/// 一次请求最多带多少对请求头（或表单项）
const PAIR_LIMIT: usize = 128;
/// 一次响应正文最多收多少字节。请求侧那道 `TEXT_LIMIT` 只管得进不出的方向，
/// 方向反过来同样要有闸：跑飞的接口或错误页回几百 MB 时，不能先全额进内存再决定要不要。
const RESPONSE_LIMIT: usize = 64 * 1024 * 1024;
/// 同时在途的代发请求数。原来每个请求都 spawn 一条任务且没有闸，
/// 插件一轮抓几十张大图就是几十份完整响应同时驻留在堆上。
const MAX_INFLIGHT: usize = 16;

/// 跨边界的响应字节闸：先看对端声明的长度，超了连读都不读
fn guard_declared_len(url: &str, declared: Option<u64>) -> Result<(), String> {
    match declared {
        Some(len) if len > RESPONSE_LIMIT as u64 => Err(format!(
            "请求 {url} 的响应声明 {len} 字节，超出单次响应上限 {RESPONSE_LIMIT}"
        )),
        _ => Ok(()),
    }
}

/// 收完之后的第二道闸：对端可以谎报 `content-length`，所以实读到的长度也要判
fn guard_actual_len(url: &str, got: usize) -> Result<(), String> {
    if got > RESPONSE_LIMIT {
        return Err(format!(
            "请求 {url} 实际回了 {got} 字节，超出单次响应上限 {RESPONSE_LIMIT}"
        ));
    }
    Ok(())
}

/// [`HostBridge::http`](crate::plugin::abi::HostBridge::http) 的函数指针类型
type Submit = extern "C" fn(call: *const HttpCall, done: HttpDone, user: *mut c_void);

/// 插件侧等待结果的凭据
type Sender = oneshot::Sender<Result<(Vec<u8>, Headers), String>>;

/// dll 侧：宿主的代发入口，由 `plugin::abi::attach_host` 写入；宿主自己那份恒空
static SUBMIT: OnceLock<Submit> = OnceLock::new();

/// 宿主侧那份常驻客户端
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// 请求方法
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Method {
    #[default]
    Get,
    Post,
}

/// 请求正文
#[derive(Clone, Debug, Default)]
pub enum Body {
    #[default]
    None,
    /// `application/x-www-form-urlencoded`
    Form(Vec<(String, String)>),
    /// 原样发出去的正文（JSON 用它，`content_type` 给 `application/json`）
    Raw { content_type: String, text: String },
}

/// 一次请求：`url` 必填，其余留默认值即可。
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Body,
    /// 整个请求的超时；`None` 用宿主的 60 秒
    pub timeout: Option<Duration>,
    /// 要不要把**响应头**一并拿回来（含重定向链上每一跳的 `Set-Cookie`）。
    ///
    /// 默认 `false`：逐条拷头是白花的钱，而且绝大多数调用只关心正文。要拿登录票的那一类才开
    /// （B 站扫码那份票只出现在 `crossDomain` 跳转的 `Set-Cookie` 上，正文里没有）。
    pub want_headers: bool,
}

/// 响应头：`Vec<(名字, 值)>`，同名可以有多条——`Set-Cookie` 正是这种，收进 `HashMap` 就只剩最后一条。
pub type Headers = Vec<(String, String)>;

impl Request {
    /// 只给 URL 的 GET
    pub fn get(url: impl Into<String>) -> Request {
        Request {
            url: url.into(),
            ..Default::default()
        }
    }

    /// POST 一份表单
    pub fn form(url: impl Into<String>, form: Vec<(String, String)>) -> Request {
        Request {
            method: Method::Post,
            url: url.into(),
            body: Body::Form(form),
            ..Default::default()
        }
    }

    /// POST 一段 JSON
    pub fn json(url: impl Into<String>, json: impl Into<String>) -> Request {
        Request {
            method: Method::Post,
            url: url.into(),
            body: Body::Raw {
                content_type: "application/json".into(),
                text: json.into(),
            },
            ..Default::default()
        }
    }

    /// 追加一条请求头
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Request {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 覆盖超时
    pub fn with_timeout(mut self, timeout: Duration) -> Request {
        self.timeout = Some(timeout);
        self
    }

    /// 这一趟要把响应头一并拿回来（配合 [`send_with_headers`]）
    pub fn wanting_headers(mut self) -> Request {
        self.want_headers = true;
        self
    }
}

/// 发一次请求，**只要正文**（绝大多数调用用这个）。
///
/// 非 2xx 一律算失败，原因里带 URL 与状态码；连不上、超时这些传输失败同理。
pub async fn send(request: Request) -> Result<Vec<u8>, String> {
    send_with_headers(request).await.map(|(body, _)| body)
}

/// 发一次请求，正文与响应头一起给（头只有请求里 `want_headers` 开着才非空）
pub async fn send_with_headers(request: Request) -> Result<(Vec<u8>, Headers), String> {
    match SUBMIT.get() {
        Some(submit) => via_host(*submit, request).await,
        None => local(request).await,
    }
}

/// GET 一段文本响应
pub async fn get<K: AsRef<str>, V: AsRef<str>>(
    url: &str,
    headers: &[(K, V)],
) -> Result<String, String> {
    as_text(
        url,
        send(Request {
            headers: own(headers),
            ..Request::get(url)
        })
        .await,
    )
}

/// GET 一段二进制响应（学生头像、攻略图这类）
pub async fn get_bytes<K: AsRef<str>, V: AsRef<str>>(
    url: &str,
    headers: &[(K, V)],
) -> Result<Vec<u8>, String> {
    send(Request {
        headers: own(headers),
        ..Request::get(url)
    })
    .await
}

/// 表单 POST
pub async fn post_form<K: AsRef<str>, V: AsRef<str>, F: AsRef<str>, W: AsRef<str>>(
    url: &str,
    headers: &[(K, V)],
    form: &[(F, W)],
) -> Result<String, String> {
    let request = Request {
        headers: own(headers),
        ..Request::form(url, own(form))
    };
    as_text(url, send(request).await)
}

/// JSON POST
pub async fn post_json<K: AsRef<str>, V: AsRef<str>>(
    url: &str,
    headers: &[(K, V)],
    json: &str,
) -> Result<String, String> {
    let request = Request {
        headers: own(headers),
        ..Request::json(url, json)
    };
    as_text(url, send(request).await)
}

fn as_text(url: &str, body: Result<Vec<u8>, String>) -> Result<String, String> {
    body.and_then(|bytes| String::from_utf8(bytes).map_err(|_| format!("响应不是合法 UTF-8 {url}")))
}

fn own<K: AsRef<str>, V: AsRef<str>>(pairs: &[(K, V)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| (name.as_ref().to_string(), value.as_ref().to_string()))
        .collect()
}

// ==================== 插件侧：把请求递过边界 ====================

/// dll 侧：记下宿主的代发入口（由 `plugin::abi::attach_host` 调用）
pub(crate) fn attach_host(submit: Submit) {
    let _ = SUBMIT.set(submit);
}

/// 把一次请求交给宿主代发，然后等回调。
///
/// 请求描述里的指针直接指向 `request` 自己的值（宿主在这次同步调用内就拷走了），
/// 所以插件侧一次多余拷贝都没有。
async fn via_host(submit: Submit, request: Request) -> Result<(Vec<u8>, Headers), String> {
    let (sender, receiver) = oneshot::channel::<Result<(Vec<u8>, Headers), String>>();
    // 这次提交的唯一凭据：宿主保证回调一次，回调里再把它收回 Box 销毁
    let user = Box::into_raw(Box::new(sender)).cast::<c_void>();
    commit(submit, &request, user);
    receiver
        .await
        .unwrap_or_else(|_| Err(format!("宿主没有回传这次请求的结果 {}", request.url)))
}

/// 摆成 ABI 布局并交出去。
///
/// 单独一个**同步**函数不是洁癖：`HttpCall` 里全是裸指针，因此不是 `Send`。留在
/// `via_host` 里就会跨过一次 `.await`，插件那条命令的 future 跟着变成 `!Send`——
/// 宿主按帧补装上下文时就不能把它搬到别的 worker 线程上了。这里让它在本帧内构造、
/// 本帧内用完，指针随函数返回一起消失。
fn commit(submit: Submit, request: &Request, user: *mut c_void) {
    let headers: Vec<HttpPair> = request.headers.iter().map(pair).collect();
    let form: Vec<HttpPair> = match &request.body {
        Body::Form(items) => items.iter().map(pair).collect(),
        _ => Vec::new(),
    };
    let (body, content_type) = match &request.body {
        Body::Raw { content_type, text } => (bytes_of(text), bytes_of(content_type)),
        _ => (HttpText::default(), HttpText::default()),
    };
    let call = HttpCall {
        method: match request.method {
            Method::Get => 0,
            Method::Post => 1,
        },
        body_kind: match &request.body {
            Body::None => 0,
            Body::Form(_) => 1,
            Body::Raw { .. } => 2,
        },
        url: bytes_of(&request.url),
        headers: if headers.is_empty() {
            null()
        } else {
            headers.as_ptr()
        },
        header_count: headers.len(),
        form: if form.is_empty() {
            null()
        } else {
            form.as_ptr()
        },
        form_count: form.len(),
        body,
        content_type,
        timeout_ms: request.timeout.map_or(0, |span| {
            u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
        }),
        want_headers: u8::from(request.want_headers),
    };
    crate::runtime::log::debug(format!("[HTTP] 插件请求代发 {}", request.url));
    submit(&call, deliver, user);
}

/// 宿主的回调：收回自己那份 oneshot 发送端，把结果转交出去
///
/// # Safety
/// `user` 必须是 [`via_host`] 交出去的那个指针；`error`/`body` 两段字节按
/// [`HttpDone`] 的约定在本次调用期内有效。
extern "C" fn deliver(
    user: *mut c_void,
    error: *const u8,
    error_len: usize,
    body: *const u8,
    body_len: usize,
    headers: *const HttpPair,
    header_count: usize,
) {
    // SAFETY: 约定由宿主保证——同一个 user 只会被回调一次
    let sender = unsafe { Box::from_raw(user.cast::<Sender>()) };
    let result = if error_len > 0 {
        // SAFETY: 约定 error/error_len 指向本次调用期内有效的一段 UTF-8
        Err(unsafe { text_of(error, error_len) })
    } else {
        // SAFETY: 约定 body/body_len 与 headers/header_count 都指向本次调用期内可读的那几段
        let body = unsafe { copy_of(body, body_len) };
        // SAFETY: 同上
        let headers = unsafe { copy_pairs(headers, header_count) };
        Ok((body, headers))
    };
    // 接收端可能已经不在了（那次等待被超时、或插件被停用），静默收下即可
    let _ = sender.send(result);
}

/// 拷走宿主给回来的响应头：空指针或 0 项就是没有。
///
/// # Safety
/// `list`/`count` 要么都是 0，要么指向本次调用期内可读的那么多项 [`HttpPair`]。
unsafe fn copy_pairs(list: *const HttpPair, count: usize) -> Headers {
    if list.is_null() || count == 0 {
        return Vec::new();
    }
    // SAFETY: 上面刚验过指针非空、项数在约定内
    unsafe { std::slice::from_raw_parts(list, count) }
        .iter()
        .map(|item| {
            // SAFETY: 约定由调用方保证
            (
                unsafe { text_of(item.name.ptr, item.name.len) },
                // SAFETY: 约定由调用方保证
                unsafe { text_of(item.value.ptr, item.value.len) },
            )
        })
        .collect()
}

/// 读一段宿主给回来的字节；空指针或零长度都当空
///
/// # Safety
/// `ptr`/`len` 要么都是 0，要么指向本次调用期内可读的一段内存。
unsafe fn copy_of(ptr: *const u8, len: usize) -> Vec<u8> {
    if ptr.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: 约定由调用方保证
    unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
}

/// 读一段宿主给回来的文字：不是合法 UTF-8 就按替换字符处理，
/// 一句日志文案不该把整个请求判死
///
/// # Safety
/// 同 [`copy_of`]。
unsafe fn text_of(ptr: *const u8, len: usize) -> String {
    String::from_utf8_lossy(&unsafe { copy_of(ptr, len) }).into_owned()
}

fn bytes_of(value: &str) -> HttpText {
    HttpText {
        ptr: value.as_ptr(),
        len: value.len(),
    }
}

fn pair((name, value): &(String, String)) -> HttpPair {
    HttpPair {
        name: bytes_of(name),
        value: bytes_of(value),
    }
}

// ==================== 宿主侧：真正发这一趟请求 ====================

/// 插件给的那个 oneshot 凭据。它只是宿主原样带回去的一个指针，宿主这边不解引用，
/// 但任务要跨 await 搬到别的 worker 线程上，所以得给它补一份 `Send`。
#[derive(Clone, Copy)]
struct Ticket(*mut c_void);

// SAFETY: Ticket 指向插件内存里的一个 Box，宿主从不碰它的内容，只负责在回调里原样带回
unsafe impl Send for Ticket {}

/// 宿主侧：接住插件递来的请求描述。
///
/// 指针内容在本次同步调用里就拷成自己的值，之后插件随时可以释放。
/// 无论走到哪条分支，`done` 都被调用**恰好一次**——这是 [`via_host`] 敢把 `Box`
/// 交出去的前提。
pub(crate) extern "C" fn host_submit(call: *const HttpCall, done: HttpDone, user: *mut c_void) {
    let ticket = Ticket(user);
    let request = match unsafe { take_call(call) } {
        Ok(request) => request,
        Err(reason) => return report(done, ticket, Err(reason)),
    };
    let url = request.url.clone();
    let task = async move {
        // 闸：`_permit` 活到这条任务末尾，所以在途响应最多 `MAX_INFLIGHT` 份。
        // `None` 只可能是闸被关闭（runtime 拆解），那时照常把请求发完比卡住更合适。
        let _permit = inflight().acquire().await.ok();
        report(done, ticket, local(request).await)
    };
    if crate::runtime::reactor::spawn(task).is_none() {
        report(
            done,
            ticket,
            Err(format!("宿主没有可用的运行时，发不了 {url}")),
        );
    }
}

fn report(done: HttpDone, ticket: Ticket, result: Result<(Vec<u8>, Headers), String>) {
    match result {
        Ok((body, headers)) => {
            crate::runtime::log::debug(format!(
                "[HTTP] 代发成功，响应 {} 字节、{} 个头",
                body.len(),
                headers.len()
            ));
            // 两段都只在这次回调期间有效：`pairs` 借的是 `headers` 里那些 String，
            // 而 `headers`/`body` 活到 `done` 返回之后才一起析构。
            let pairs: Vec<HttpPair> = headers.iter().map(pair).collect();
            done(
                ticket.0,
                null(),
                0,
                body.as_ptr(),
                body.len(),
                if pairs.is_empty() {
                    null()
                } else {
                    pairs.as_ptr()
                },
                pairs.len(),
            );
        }
        Err(reason) => {
            crate::runtime::log::debug(format!("[HTTP] 代发失败 {reason}"));
            done(
                ticket.0,
                reason.as_ptr(),
                reason.len(),
                null(),
                0,
                null(),
                0,
            );
        }
    }
}

/// 把跨边界的请求描述读成宿主自己的值
///
/// # Safety
/// `call` 要么为空指针，要么指向一个 [`HttpCall`] 布局的内存，且其中各指针所指的字节
/// 在本次调用期内可读。
unsafe fn take_call(call: *const HttpCall) -> Result<Request, String> {
    if call.is_null() {
        return Err("HTTP 请求描述是空指针".to_string());
    }
    // SAFETY: 约定由调用方保证
    let raw = unsafe { &*call };
    if raw.header_count > PAIR_LIMIT || raw.form_count > PAIR_LIMIT {
        return Err(format!("一次请求最多 {PAIR_LIMIT} 对键值"));
    }
    let method = match raw.method {
        0 => Method::Get,
        1 => Method::Post,
        other => return Err(format!("不支持的 HTTP 方法码 {other}")),
    };
    let url = unsafe { take_text(raw.url, "URL")? };
    if url.is_empty() {
        return Err("HTTP 请求没有 URL".to_string());
    }
    let headers = unsafe { take_pairs(raw.headers, raw.header_count, "请求头")? };
    let body = match raw.body_kind {
        0 => Body::None,
        1 => Body::Form(unsafe { take_pairs(raw.form, raw.form_count, "表单项")? }),
        2 => Body::Raw {
            content_type: unsafe { take_text(raw.content_type, "content-type") }?,
            text: unsafe { take_text(raw.body, "请求正文") }?,
        },
        other => return Err(format!("不支持的正文类型码 {other}")),
    };
    Ok(Request {
        method,
        url,
        headers,
        body,
        timeout: (raw.timeout_ms > 0).then(|| Duration::from_millis(raw.timeout_ms)),
        want_headers: raw.want_headers != 0,
    })
}

/// # Safety
/// 同 [`take_call`]：`value` 的指针与长度要么都是 0，要么指向本次调用期内可读的一段字节。
unsafe fn take_text(value: HttpText, what: &str) -> Result<String, String> {
    if value.len == 0 {
        return Ok(String::new());
    }
    if value.ptr.is_null() {
        return Err(format!("{what} 报了 {} 字节却是空指针", value.len));
    }
    if value.len > TEXT_LIMIT {
        return Err(format!("{what} 报了 {} 字节，超出上限", value.len));
    }
    // SAFETY: 上面刚验过指针非空、长度在限内，且约定这段字节在本次调用期内可读
    let bytes = unsafe { std::slice::from_raw_parts(value.ptr, value.len) };
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| format!("{what} 不是合法 UTF-8"))
}

/// # Safety
/// 同 [`take_call`]。
unsafe fn take_pairs(
    list: *const HttpPair,
    count: usize,
    what: &str,
) -> Result<Vec<(String, String)>, String> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if list.is_null() {
        return Err(format!("{what} 报了 {count} 项却是空指针"));
    }
    // SAFETY: 上面刚验过指针非空，且约定这 count 项在本次调用期内可读
    let items = unsafe { std::slice::from_raw_parts(list, count) };
    items
        .iter()
        .map(|item| {
            // SAFETY: 约定由调用方保证
            let name = unsafe { take_text(item.name, "键名")? };
            // SAFETY: 约定由调用方保证
            let value = unsafe { take_text(item.value, "键值")? };
            Ok((name, value))
        })
        .collect()
}

/// 宿主侧那份常驻客户端：连接池与 keepalive 都开着——跑在宿主的 runtime 上，
/// hyper 派生的后台任务本来就看得见上下文，用不着像插件自己发请求那样把池子关掉。
fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// 同时在途的代发请求闸。许可在 [`host_submit`] 那条任务里拿，拿不到就在宿主这边排队——
/// 背压回到插件那边是"这一条还没发完"，而不是继续在堆上叠第 17 份完整响应。
fn inflight() -> &'static Semaphore {
    static INFLIGHT: OnceLock<Semaphore> = OnceLock::new();
    INFLIGHT.get_or_init(|| Semaphore::new(MAX_INFLIGHT))
}

/// 手动走跳转时最多跟几跳（与 reqwest 默认的 `Policy::limited(10)` 同一档）
const MAX_HOPS: usize = 10;

/// 真发一次请求：非 2xx 与传输失败都归成 `Err`，文案与插件自己写 reqwest 时一贯的写法一致。
///
/// 开了 `want_headers` 的那一路走 [`hopping`]：跳转自己一层层跟，中间那一跳的 `Set-Cookie`
/// 才收得着。绝大多数调用只关心正文，照旧用常驻客户端、由它按默认策略跟进跳转。
async fn local(request: Request) -> Result<(Vec<u8>, Headers), String> {
    if request.want_headers {
        let url = request.url.clone();
        let total = request.timeout.unwrap_or(DEFAULT_TIMEOUT);
        // 超时按**整趟**算：`Request::timeout` 这个字段的口径是"整个请求"，不是"每一跳"。
        return match tokio::time::timeout(total, hopping(request)).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "请求 {url} 超时（整趟限 {total:?}，跳转链上每一跳都算在内）"
            )),
        };
    }
    let url = request.url.clone();
    let response = build(client(), &request, &url)
        .send()
        .await
        .map_err(|err| format!("请求失败 {url}: {err}"))?;
    Ok((final_body(&url, response).await?, Vec::new()))
}

/// `want_headers` 那一路：一层层跟跳转，把每一跳的响应头都收着。
///
/// 为什么非要自己跟：登录那类调用要的票**只在中间那一跳的 `Set-Cookie` 上**（B 站
/// `crossDomain` 就是 302 给票、再跳到落地页），而 reqwest 一旦自己跟进跳转，上一跳的响应头
/// 就再也取不出来了——`redirect::Attempt` 只给状态码和下一跳 URL，不给头。
///
/// 跟法照 reqwest/tower-http 自己的默认策略来，免得同一份配置两套语义：301/302/303 把 POST
/// 降级成 GET 并丢掉正文，307/308 原样重试；跨主机那一跳不带走凭据（[`strip_sensitive`]）。
async fn hopping(mut request: Request) -> Result<(Vec<u8>, Headers), String> {
    let client = no_follow_client();
    let mut url = request.url.clone();
    let mut bag = Headers::new();
    for _ in 0..MAX_HOPS {
        let response = build(client, &request, &url)
            .send()
            .await
            .map_err(|err| format!("请求失败 {url}: {err}"))?;
        let status = response.status();
        if !status.is_redirection() {
            // 最终这一跳给全部响应头；中间那几跳只收 `Set-Cookie`，别把无关的头也带过去
            push_all(response.headers(), &mut bag);
            let body = final_body(&url, response).await?;
            return Ok((body, bag));
        }
        push_cookies(response.headers(), &mut bag);
        let next = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        drop(response);
        let Some(location) = next else {
            // 3xx 却没有可跟的 Location：这一路要的就是头，把收着的交回去、正文留空。
            // 走 Err 那一支的话，票就跟着一起丢了。
            crate::runtime::log::debug(format!(
                "[HTTP] 代发 {url} 停在 HTTP {status}（没有 Location），已收到 {} 个头",
                bag.len()
            ));
            return Ok((Vec::new(), bag));
        };
        let next = resolve(&url, &location)
            .ok_or_else(|| format!("请求 {url} 的重定向目标 {location} 解析不出来"))?;
        if crosses_host(&url, &next) {
            strip_sensitive(&mut request.headers);
        }
        if matches!(status.as_u16(), 301..=303) && request.method == Method::Post {
            request.method = Method::Get;
            request.body = Body::None;
        }
        url = next;
    }
    Err(format!("请求 {} 的重定向超过 {MAX_HOPS} 跳", request.url))
}

/// 摆一次请求出去。`url` 单独传是因为手动走跳转的那一路每跳的 URL 都不同，
/// `request.url` 要留着当"这次请求"的名字报错。
fn build(client: &reqwest::Client, request: &Request, url: &str) -> reqwest::RequestBuilder {
    let mut call = client
        .request(
            match request.method {
                Method::Get => reqwest::Method::GET,
                Method::Post => reqwest::Method::POST,
            },
            url,
        )
        .timeout(request.timeout.unwrap_or(DEFAULT_TIMEOUT));
    for (name, value) in &request.headers {
        call = call.header(name.as_str(), value.as_str());
    }
    match &request.body {
        Body::None => call,
        Body::Form(items) => call.form(items),
        Body::Raw { content_type, text } => call
            .header(reqwest::header::CONTENT_TYPE, content_type.as_str())
            .body(text.clone()),
    }
}

/// 收完之后的收口：先按对端声明的长度拦一道（跑飞的接口或错误页回几百 MB 时连读都不该开始），
/// 再在**读正文之前**判状态码（于是一个几百 MB 的错误页不会先占住几百 MB 再被原样丢掉），
/// 最后才收正文并判实收长度——谎报 `content-length` 的兜底。
async fn final_body(url: &str, response: reqwest::Response) -> Result<Vec<u8>, String> {
    let status = response.status();
    guard_declared_len(url, response.content_length())?;
    if !status.is_success() {
        return Err(format!("请求 {url} 返回 HTTP {status}"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("读取响应失败 {url}: {err}"))?;
    guard_actual_len(url, bytes.len())?;
    Ok(bytes.to_vec())
}

/// 一条响应里的 `Set-Cookie` 全收进袋：同名多条都要留，收进 `HashMap` 就只剩最后一条
fn push_cookies(headers: &reqwest::header::HeaderMap, bag: &mut Headers) {
    for value in headers.get_all(reqwest::header::SET_COOKIE).iter() {
        if let Ok(text) = value.to_str() {
            bag.push(("set-cookie".to_string(), text.to_string()));
        }
    }
}

/// 最终响应的全部头。名字统一小写（HTTP 头名不区分大小写，插件那边按名字取就不用自己再折一遍）；
/// 值不是可见 ASCII 的那几条直接丢掉——一个二进制头不该把整条请求判失败。
fn push_all(headers: &reqwest::header::HeaderMap, bag: &mut Headers) {
    for (name, value) in headers {
        if let Ok(text) = value.to_str() {
            bag.push((name.as_str().to_ascii_lowercase(), text.to_string()));
        }
    }
}

/// 按 RFC 3986 把 `Location` 解到当前 URL 上：可以是绝对 URL，也可以是相对路径
fn resolve(base: &str, location: &str) -> Option<String> {
    reqwest::Url::parse(base)
        .ok()?
        .join(location)
        .ok()
        .map(|next| next.into())
}

/// 下一跳有没有换主机（域名或端口）——reqwest 自己跟进跳转时用的就是这个判据
fn crosses_host(from: &str, to: &str) -> bool {
    let (Ok(from), Ok(to)) = (reqwest::Url::parse(from), reqwest::Url::parse(to)) else {
        return true; // 解析不出来就按最保守的办：当成跨站，凭据不带过去
    };
    from.host_str() != to.host_str() || from.port_or_known_default() != to.port_or_known_default()
}

/// 跨主机那一跳不带走凭据，与 reqwest 默认策略摘的同一批头
fn strip_sensitive(headers: &mut Vec<(String, String)>) {
    const SENSITIVE: [&str; 5] = [
        "cookie",
        "cookie2",
        "authorization",
        "proxy-authorization",
        "www-authenticate",
    ];
    headers.retain(|(name, _)| !SENSITIVE.iter().any(|key| name.eq_ignore_ascii_case(key)));
}

/// 第二份常驻客户端：不自动跟随重定向，[`hopping`] 那一轮用它。
///
/// 重定向策略是**客户端级**的（`RequestBuilder` 上没有开关），所以这一路只能另建一份。跳转由
/// 我们自己跟、攒的头是每次调用的局部变量，这份客户端本身不含任何每请求状态，因此并发共用没问题。
fn no_follow_client() -> &'static reqwest::Client {
    static NO_FOLLOW: OnceLock<reqwest::Client> = OnceLock::new();
    NO_FOLLOW.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 一次性的本地 HTTP 回应：接一条连接，把收到的请求原样当正文答出去，然后收摊。
    /// 只监听 127.0.0.1，离线可跑，不依赖任何外部站点。
    async fn echo_server(status: u16) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            // 读到空行为止，就是请求头收完了
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    return;
                }
                head.push(byte[0]);
            }
            let length = std::str::from_utf8(&head)
                .ok()
                .and_then(|text| {
                    text.lines().find_map(|line| match line.split_once(':') {
                        Some((name, value))
                            if name.trim().eq_ignore_ascii_case("content-length") =>
                        {
                            value.trim().parse::<usize>().ok()
                        }
                        _ => None,
                    })
                })
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            if length > 0 {
                let _ = socket.read_exact(&mut body).await;
            }
            let echoed = format!(
                "{}{}",
                String::from_utf8_lossy(&head),
                String::from_utf8_lossy(&body)
            );
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                echoed.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.write_all(echoed.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}/probe")
    }

    /// echo 回来的报文里有没有这一行请求头。
    ///
    /// 客户端会把名字规范化成 `X-Game` 这种写法（经系统代理转发时更是如此），
    /// 所以按整行、不区分大小写地找，别去赌发送时的那个大小写。
    fn has_header(echoed: &str, header: &str) -> bool {
        echoed
            .lines()
            .any(|line| line.trim_end().eq_ignore_ascii_case(header))
    }

    #[tokio::test]
    async fn a_get_request_returns_the_body() {
        let url = echo_server(200).await;
        let text = get(&url, &[("x-game", "ba")]).await.unwrap();
        assert!(text.starts_with("GET /probe HTTP/1.1"), "{text}");
        assert!(has_header(&text, "x-game: ba"), "请求头应原样送到: {text}");
    }

    #[tokio::test]
    async fn a_form_post_reaches_the_server() {
        let url = echo_server(200).await;
        let text = post_form(&url, &[("game-alias", "ba")], &[("id", "12")])
            .await
            .unwrap();
        assert!(text.starts_with("POST /probe HTTP/1.1"), "{text}");
        assert!(has_header(&text, "game-alias: ba"), "{text}");
        assert!(text.ends_with("id=12"), "表单正文没到: {text}");
    }

    #[tokio::test]
    async fn a_json_post_carries_its_content_type() {
        let url = echo_server(200).await;
        let text = post_json(&url, &[] as &[(&str, &str)], "{\"id\":1}")
            .await
            .unwrap();
        assert!(
            text.contains("application/json"),
            "content-type 该由框架补上: {text}"
        );
        assert!(text.ends_with("{\"id\":1}"), "{text}");
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_error() {
        let url = echo_server(503).await;
        let reason = get(&url, &[] as &[(&str, &str)]).await.unwrap_err();
        assert!(
            reason.contains("HTTP 503"),
            "失败原因里该带状态码: {reason}"
        );
        assert!(reason.contains("/probe"), "失败原因里该带 URL: {reason}");
    }

    /// 一台会跳一跳的服务器：`/start` 回 302 并把票下在**中间这一跳**的 `Set-Cookie` 上，
    /// 跳到 `/end` 才回 200 与另一条 `Set-Cookie`。这正是 B 站 `crossDomain` 那副形状。
    ///
    /// 每一跳都回 `connection: close`，所以服务端只要按顺序多接几次连接就行，不必管连接复用。
    async fn redirect_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // 手动跟的那一路要 2 条连接，客户端自己跟的那一路又要 2 条，这里多留几条余量
            for _ in 0..6 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                        break;
                    }
                    head.push(byte[0]);
                }
                let reply = if String::from_utf8_lossy(&head).starts_with("GET /start") {
                    "HTTP/1.1 302 Found\r\nlocation: /end\r\nset-cookie: ticket=abc; Path=/\r\n\
                     set-cookie: sid=1\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 200 OK\r\nset-cookie: last=2\r\ncontent-length: 4\r\n\
                     connection: close\r\n\r\npong"
                };
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}/start")
    }

    /// #143 的正例：票下在重定向链中间那一跳上，手动跟跳转的那一路必须把它收回来。
    ///
    /// 判据按**整串**比对，不写 `contains`——少收一条、收重了、或名字没折成小写，都当场看得见。
    #[tokio::test]
    async fn a_redirect_chain_hands_back_the_cookie_from_the_middle_hop() {
        let url = redirect_server().await;
        let (body, headers) = send_with_headers(Request::get(&url).wanting_headers())
            .await
            .unwrap();
        assert_eq!(body, b"pong".to_vec(), "跟到最终那一跳的正文");
        let cookies: Vec<&str> = headers
            .iter()
            .filter(|(name, _)| name == "set-cookie")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(
            cookies,
            vec!["ticket=abc; Path=/", "sid=1", "last=2"],
            "中间那一跳的两条票加最终那条，一条都不能少: {headers:?}"
        );
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "content-length" && value == "4"),
            "最终响应的其余头也该在，名字统一折成小写: {headers:?}"
        );

        // 没举旗的那一路照旧由常驻客户端自己跟进跳转：正文一样，头一概不给
        let (plain, none) = send_with_headers(Request::get(&url)).await.unwrap();
        assert_eq!(plain, b"pong".to_vec(), "没举旗也要跟到最终那一跳");
        assert!(none.is_empty(), "没举旗就不该花钱拷头");
    }

    /// 跳转链上的三个纯判据：相对 Location 的解析、跨主机的认定、凭据的剥离。
    /// 手动跟跳转就等于自己担下了 reqwest 原本替做的事，这一条钉住它没做偏。
    #[test]
    fn a_manual_redirect_walk_follows_the_same_rules_as_the_client() {
        assert_eq!(
            resolve("https://passport.bilibili.com/x/y", "/z?ticket=1").as_deref(),
            Some("https://passport.bilibili.com/z?ticket=1"),
            "相对 Location 要按 RFC 3986 解到当前 URL 上"
        );
        assert_eq!(
            resolve("https://x.invalid/a", "https://y.invalid/b").as_deref(),
            Some("https://y.invalid/b"),
            "绝对 Location 原样用"
        );
        assert_eq!(resolve("不是 URL", "/x"), None);

        // 与 reqwest 同一判据：看主机与端口，不看 scheme
        assert!(
            crosses_host(
                "https://passport.bilibili.com/a",
                "https://www.bilibili.com/b"
            ),
            "换了域名就是跨站"
        );
        assert!(crosses_host(
            "https://x.invalid:443/a",
            "https://x.invalid:8443/b"
        ));
        assert!(!crosses_host(
            "https://x.invalid:443/a",
            "https://x.invalid/b"
        ));
        assert!(
            crosses_host("https://x.invalid/a", "不是 URL"),
            "解析不出来按跨站办"
        );

        let mut headers = vec![
            ("cookie".to_string(), "SESSDATA=1".to_string()),
            ("Authorization".to_string(), "Bearer 1".to_string()),
            ("game-alias".to_string(), "ba".to_string()),
        ];
        strip_sensitive(&mut headers);
        assert_eq!(
            headers,
            vec![("game-alias".to_string(), "ba".to_string())],
            "跨站只留凭据之外的头"
        );
    }

    /// 对端**声明**的长度超上限时，连读都不该开始（`local()` 里这一道在读正文之前）。
    #[test]
    fn a_declared_response_over_the_cap_is_rejected_before_reading() {
        assert!(
            guard_declared_len("http://x/big", Some(RESPONSE_LIMIT as u64 + 1)).is_err(),
            "声明超出上限却没拦下来"
        );
        assert!(guard_declared_len("http://x/ok", Some(10)).is_ok());
        // 分块传输没有 content-length，这不能当成"要攻击我"
        assert!(guard_declared_len("http://x/chunked", None).is_ok());
    }

    /// 谎报长度的兜底：实收到的字节也要判一道，边界上不误伤。
    #[test]
    fn an_undeclared_response_is_measured_after_reading() {
        assert!(
            guard_actual_len("http://x", RESPONSE_LIMIT).is_ok(),
            "正好在上限上不该拒"
        );
        assert!(
            guard_actual_len("http://x", RESPONSE_LIMIT + 1).is_err(),
            "超一个字节就该拒"
        );
    }

    /// 走完整 `local()` 的那道闸：对端**声明**一个 64MB+ 的长度时，必须在读正文之前就报错——
    /// 只测上面那两个纯函数不足以证明 `local()` 真的调了它们（原来这里是先读满再判状态码）。
    #[tokio::test]
    async fn a_huge_declared_response_is_refused_without_reading_the_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    return;
                }
                head.push(byte[0]);
            }
            // 声明一个超限的长度就再不写了：正文根本不存在，能读它会挂住/吃内存
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 999000000\r\n\r\n")
                .await;
            let _ = socket.shutdown().await;
        });
        let reason = send(Request::get(format!("http://{addr}/huge")))
            .await
            .expect_err("声明超出上限的响应该被拒掉");
        assert!(
            reason.contains("超出单次响应上限"),
            "拒得不对，像是读完之后再报的: {reason}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_url_is_reported_not_panicked() {
        // 127.0.0.1 上没人监听的端口：要的是"报错返回"，不是崩。
        // 本机挂着系统代理时 reqwest 会跟着走，那条路回来的是代理给的 502 而不是连接拒绝，
        // 所以这里只认"Err 且原因里说清了是哪一种"。
        let reason = get("http://127.0.0.1:1/none", &[] as &[(&str, &str)])
            .await
            .unwrap_err();
        assert!(
            reason.contains("请求失败") || reason.contains("返回 HTTP"),
            "连不上该有明确原因: {reason}"
        );
        assert!(reason.contains("/none"), "原因里该带 URL: {reason}");
    }

    #[tokio::test]
    async fn images_come_back_as_bytes() {
        let url = echo_server(200).await;
        let bytes = get_bytes(&url, &[("referer", "https://example.invalid/")])
            .await
            .unwrap();
        assert!(
            bytes.starts_with(b"GET /probe"),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
    }

    /// 过一道边界走宿主：这正是插件 dll 里跑的那条路，只是这里宿主与插件同在一个镜像里。
    #[tokio::test]
    async fn the_bridge_path_delivers_the_same_body() {
        attach_host(host_submit);
        let url = echo_server(200).await;
        let text = get(&url, &[("x-via", "bridge")]).await.unwrap();
        assert!(has_header(&text, "x-via: bridge"), "{text}");
    }

    // ==================== 「done 恰好一次」的计数探针 ====================
    //
    // [`host_submit`] 的契约是每条分支都回调**恰好一次**：少一次插件那头永远等不到结果
    // （`via_host` 的 `await` 挂死），多一次就是同一个 `Box<oneshot::Sender>` 被
    // `Box::from_raw` 收走两遍（double free）。下面三个用例把各条分支走一遍并数回调次数。
    //
    // 记的是「凭据 → 收到的回调」这张表，而不是一个共享计数器：每个用例给自己一个独一无二
    // 的 `user` 指针，并发跑也就各查各的，不必再拿一把全局锁把自己串行化——那把锁在异步用例
    // 里还得跨 `await` 持着，正是 `await_holding_lock` 要拦的东西。

    /// 一次回调拷回来的内容。三段字节都只在那次调用期内有效，所以当场拷走。
    #[derive(Clone, Default, Debug)]
    struct Report {
        reason: Option<String>,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    }

    static CALLBACKS: Mutex<BTreeMap<usize, Vec<Report>>> = Mutex::new(BTreeMap::new());

    extern "C" fn record_report(
        user: *mut c_void,
        error: *const u8,
        error_len: usize,
        body: *const u8,
        body_len: usize,
        headers: *const HttpPair,
        header_count: usize,
    ) {
        // 这里刻意不 panic：`host_submit` 是 `extern "C"`，回调里的 panic 会 abort 整个测试
        // 二进制，把同批跑的其它用例一起带走。内容缺失就让用例里的断言去判。
        let mut report = Report::default();
        if error_len > 0 && !error.is_null() {
            // SAFETY: 上面刚验过非空，且约定这段字节在本次调用期内可读
            let reason = unsafe { std::slice::from_raw_parts(error, error_len) };
            // 不是 UTF-8 也照样拷出来（变成替换符），让断言去判"原因里有没有那句话"
            report.reason = Some(String::from_utf8_lossy(reason).to_string());
        }
        if body_len > 0 && !body.is_null() {
            // SAFETY: 同上，约定正文那段字节在本次调用期内可读
            report.body = unsafe { std::slice::from_raw_parts(body, body_len) }.to_vec();
        }
        // SAFETY: 同上，约定那 count 项 [`HttpPair`] 在本次调用期内可读
        report.headers = unsafe { copy_pairs(headers, header_count) };
        CALLBACKS
            .lock()
            .unwrap()
            .entry(user as usize)
            .or_default()
            .push(report);
    }

    /// 宿主只把 `user` 原样带回来，从不解引用，所以这里给的是纯哨兵值；每个用例一个。
    const NULL_CALL: usize = 0x5A5A_1001;
    const LYING_HEADERS: usize = 0x5A5A_1002;
    const FAILED_LOCALLY: usize = 0x5A5A_1003;
    const SUCCEEDED_LOCALLY: usize = 0x5A5A_1004;
    const DELIVERED: usize = 0x5A5A_1005;
    const UNREACHABLE: usize = 0x5A5A_1006;

    fn credential(ticket: usize) -> *mut c_void {
        ticket as *mut c_void
    }

    /// 某个凭据到现在为止收到的回调，按到达顺序
    fn callbacks_of(ticket: usize) -> Vec<Report> {
        CALLBACKS
            .lock()
            .unwrap()
            .get(&ticket)
            .cloned()
            .unwrap_or_default()
    }

    /// 回调由宿主的 worker 线程打来，而 `#[tokio::test]` 默认是单线程 runtime：这里必须用
    /// `tokio::time::sleep` 让出执行权，换成同步的 `thread::sleep` 会把待投递的代发任务
    /// 一起堵死，等到的只有超时。
    async fn await_callback(ticket: usize) {
        for _ in 0..200 {
            if !callbacks_of(ticket).is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// 请求描述坏掉：`host_submit` 连任务都不该派，直接一次回调说明原因。
    #[test]
    fn a_refused_request_still_reports_once() {
        let url = "https://example.invalid/probe".to_string();
        let good = HttpCall {
            method: 0,
            body_kind: 0,
            url: bytes_of(&url),
            headers: null(),
            header_count: 0,
            form: null(),
            form_count: 0,
            body: HttpText::default(),
            content_type: HttpText::default(),
            timeout_ms: 0,
            want_headers: 0,
        };

        // 空指针描述
        host_submit(null(), record_report, credential(NULL_CALL));
        let reports = callbacks_of(NULL_CALL);
        assert_eq!(reports.len(), 1, "空指针描述也该回一次");
        assert!(reports[0].reason.is_some(), "拒绝该说明原因");
        assert!(reports[0].body.is_empty(), "被拒时不该给正文");

        // 键值计数撒谎
        let lying = HttpCall {
            header_count: 3,
            ..good
        };
        host_submit(&lying, record_report, credential(LYING_HEADERS));
        let reports = callbacks_of(LYING_HEADERS);
        assert_eq!(reports.len(), 1, "坏描述也该回一次");
        assert!(
            reports[0]
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("请求头")),
            "原因里该说清坏在哪一项: {reports:?}"
        );
    }

    /// 就地报错的那一支（宿主侧压根没运行时可用）不走 `host_submit` 去测：进程级那份
    /// `reactor::RUNTIME` 会被同批跑的其它用例登记成不确定的状态，"投得出去吗"就不是用例能定的了。
    /// 这里直接收口测 [`report`] —— 它是所有分支共用的那一次回调，成功与失败都必须正好一次。
    #[test]
    fn every_reported_outcome_reaches_the_plugin_exactly_once() {
        report(
            record_report,
            Ticket(credential(FAILED_LOCALLY)),
            Err("没有可用的运行时".to_string()),
        );
        let reports = callbacks_of(FAILED_LOCALLY);
        assert_eq!(reports.len(), 1, "失败只该回调一次");
        assert_eq!(
            reports[0].reason.as_deref(),
            Some("没有可用的运行时"),
            "失败要把原因原话带到"
        );
        assert!(reports[0].body.is_empty(), "失败不该给正文");
        assert!(reports[0].headers.is_empty(), "失败不该给响应头");

        report(
            record_report,
            Ticket(credential(SUCCEEDED_LOCALLY)),
            Ok((
                b"pong".to_vec(),
                vec![("set-cookie".to_string(), "ticket=abc".to_string())],
            )),
        );
        let reports = callbacks_of(SUCCEEDED_LOCALLY);
        assert_eq!(reports.len(), 1, "成功也只该回调一次");
        assert!(reports[0].reason.is_none(), "成功不该给原因");
        assert_eq!(reports[0].body, b"pong", "正文指针要当场读得出来");
        assert_eq!(
            reports[0].headers,
            vec![("set-cookie".to_string(), "ticket=abc".to_string())],
            "响应头那一段指针也要当场读得出来"
        );
    }

    /// 真发出去的那条路：回调一次、正文到位、没有错误段；传输失败同样只回一次。
    #[tokio::test]
    async fn a_delivered_request_reports_once() {
        let url = echo_server(200).await;
        let request = Request::get(&url);
        let call = HttpCall {
            method: 0,
            body_kind: 0,
            url: bytes_of(&request.url),
            headers: null(),
            header_count: 0,
            form: null(),
            form_count: 0,
            body: HttpText::default(),
            content_type: HttpText::default(),
            timeout_ms: 0,
            want_headers: 0,
        };
        host_submit(&call, record_report, credential(DELIVERED));
        await_callback(DELIVERED).await;
        let reports = callbacks_of(DELIVERED);
        assert_eq!(reports.len(), 1, "成功只该回调一次");
        assert!(reports[0].reason.is_none(), "成功不该给原因");
        assert!(!reports[0].body.is_empty(), "正文要非空");

        // 换个没人听的端口：失败同样只回一次，且带原因
        let dead = "http://127.0.0.1:1/none".to_string();
        let failing = HttpCall {
            url: bytes_of(&dead),
            ..call
        };
        host_submit(&failing, record_report, credential(UNREACHABLE));
        await_callback(UNREACHABLE).await;
        let reports = callbacks_of(UNREACHABLE);
        assert_eq!(reports.len(), 1, "失败也只该回调一次");
        assert!(reports[0].reason.is_some(), "失败该带原因");
        assert!(reports[0].body.is_empty());
    }

    /// 请求描述是插件给的裸指针：空指针、越界长度、非 UTF-8、错位计数都得当场拒掉，
    /// 不能让宿主照着读别人的内存。
    #[test]
    fn a_broken_request_description_is_refused() {
        let url = "https://example.invalid/probe".to_string();
        let good = HttpCall {
            method: 0,
            body_kind: 0,
            url: bytes_of(&url),
            headers: null(),
            header_count: 0,
            form: null(),
            form_count: 0,
            body: HttpText::default(),
            content_type: HttpText::default(),
            timeout_ms: 0,
            want_headers: 0,
        };
        let request = unsafe { take_call(&good) }.unwrap();
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.url, url);
        assert_eq!(request.timeout, None);
        assert!(!request.want_headers, "没举旗就该当没要响应头");
        // 旗子必须真过得了边界：编错偏移的话插件那头只是拿不到头，比崩更难查
        let wanting = HttpCall {
            want_headers: 1,
            ..good
        };
        assert!(unsafe { take_call(&wanting) }.unwrap().want_headers);

        assert!(unsafe { take_call(null()) }.is_err_and(|reason| reason.contains("空指针")));
        let dangling = HttpCall {
            url: HttpText {
                ptr: null(),
                len: 8,
            },
            ..good
        };
        assert!(unsafe { take_call(&dangling) }.is_err_and(|reason| reason.contains("空指针")));
        let too_long = HttpCall {
            url: HttpText {
                ptr: url.as_ptr(),
                len: TEXT_LIMIT + 1,
            },
            ..good
        };
        assert!(unsafe { take_call(&too_long) }.is_err_and(|reason| reason.contains("超出上限")));
        let bad_utf8 = HttpCall {
            url: HttpText {
                ptr: [0xFFu8].as_ptr(),
                len: 1,
            },
            ..good
        };
        assert!(unsafe { take_call(&bad_utf8) }.is_err_and(|reason| reason.contains("合法 UTF-8")));
        let bogus_method = HttpCall { method: 7, ..good };
        assert!(unsafe { take_call(&bogus_method) }.is_err_and(|reason| reason.contains("方法码")));
        let lying_count = HttpCall {
            header_count: 3,
            ..good
        };
        assert!(unsafe { take_call(&lying_count) }.is_err_and(|reason| reason.contains("请求头")));
        let too_many = HttpCall {
            header_count: PAIR_LIMIT + 1,
            ..good
        };
        assert!(unsafe { take_call(&too_many) }.is_err_and(|reason| reason.contains("最多")));
        let no_url = HttpCall {
            url: HttpText::default(),
            ..good
        };
        assert!(unsafe { take_call(&no_url) }.is_err_and(|reason| reason.contains("没有 URL")));
    }

    /// 超时按毫秒过边界：0 一律表示"用宿主的默认值"
    #[test]
    fn request_builders_keep_what_they_are_given() {
        let request = Request::json("/upload", "{\"a\":1}")
            .with_header("k", "v")
            .with_timeout(Duration::from_millis(1500));
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "/upload");
        assert_eq!(request.headers, vec![("k".to_string(), "v".to_string())]);
        assert!(matches!(&request.body, Body::Raw { content_type, text }
                if content_type == "application/json" && text == "{\"a\":1}"));
        assert_eq!(request.timeout, Some(Duration::from_millis(1500)));

        let form = Request::form("/f", vec![("id".to_string(), "12".to_string())]);
        assert!(matches!(&form.body, Body::Form(items) if items.len() == 1));
        assert!(Request::get("/g").url == "/g" && matches!(Request::get("/g").body, Body::None));
    }
}
