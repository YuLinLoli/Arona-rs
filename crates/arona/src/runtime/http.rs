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
use tokio::sync::oneshot;

/// 插件没指定超时时用这一档
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// 单段字节的长度上限：正常 URL 几十字节，这里只挡指针与长度写歪了的插件
const TEXT_LIMIT: usize = 8 * 1024 * 1024;
/// 一次请求最多带多少对请求头（或表单项）
const PAIR_LIMIT: usize = 128;

/// [`HostBridge::http`](crate::plugin::abi::HostBridge::http) 的函数指针类型
type Submit = extern "C" fn(call: *const HttpCall, done: HttpDone, user: *mut c_void);

/// 插件侧等待结果的凭据
type Sender = oneshot::Sender<Result<Vec<u8>, String>>;

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
}

impl Request {
    /// 只给 URL 的 GET
    pub fn get(url: impl Into<String>) -> Request {
        Request { url: url.into(), ..Default::default() }
    }

    /// POST 一份表单
    pub fn form(url: impl Into<String>, form: Vec<(String, String)>) -> Request {
        Request { method: Method::Post, url: url.into(), body: Body::Form(form), ..Default::default() }
    }

    /// POST 一段 JSON
    pub fn json(url: impl Into<String>, json: impl Into<String>) -> Request {
        Request {
            method: Method::Post,
            url: url.into(),
            body: Body::Raw { content_type: "application/json".into(), text: json.into() },
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
}

/// 发一次请求，拿回响应正文（图片这类二进制就用它）。
///
/// 非 2xx 一律算失败，原因里带 URL 与状态码；连不上、超时这些传输失败同理。
pub async fn send(request: Request) -> Result<Vec<u8>, String> {
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
    as_text(url, send(Request { headers: own(headers), ..Request::get(url) }).await)
}

/// GET 一段二进制响应（学生头像、攻略图这类）
pub async fn get_bytes<K: AsRef<str>, V: AsRef<str>>(
    url: &str,
    headers: &[(K, V)],
) -> Result<Vec<u8>, String> {
    send(Request { headers: own(headers), ..Request::get(url) }).await
}

/// 表单 POST
pub async fn post_form<K: AsRef<str>, V: AsRef<str>, F: AsRef<str>, W: AsRef<str>>(
    url: &str,
    headers: &[(K, V)],
    form: &[(F, W)],
) -> Result<String, String> {
    let request = Request { headers: own(headers), ..Request::form(url, own(form)) };
    as_text(url, send(request).await)
}

/// JSON POST
pub async fn post_json<K: AsRef<str>, V: AsRef<str>>(
    url: &str,
    headers: &[(K, V)],
    json: &str,
) -> Result<String, String> {
    let request = Request { headers: own(headers), ..Request::json(url, json) };
    as_text(url, send(request).await)
}

fn as_text(url: &str, body: Result<Vec<u8>, String>) -> Result<String, String> {
    body.and_then(|bytes| {
        String::from_utf8(bytes).map_err(|_| format!("响应不是合法 UTF-8 {url}"))
    })
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
async fn via_host(submit: Submit, request: Request) -> Result<Vec<u8>, String> {
    let (sender, receiver) = oneshot::channel::<Result<Vec<u8>, String>>();
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
        method: match request.method { Method::Get => 0, Method::Post => 1 },
        body_kind: match &request.body {
            Body::None => 0,
            Body::Form(_) => 1,
            Body::Raw { .. } => 2,
        },
        url: bytes_of(&request.url),
        headers: if headers.is_empty() { null() } else { headers.as_ptr() },
        header_count: headers.len(),
        form: if form.is_empty() { null() } else { form.as_ptr() },
        form_count: form.len(),
        body,
        content_type,
        timeout_ms: request
            .timeout
            .map_or(0, |span| u64::try_from(span.as_millis()).unwrap_or(u64::MAX)),
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
) {
    // SAFETY: 约定由宿主保证——同一个 user 只会被回调一次
    let sender = unsafe { Box::from_raw(user.cast::<Sender>()) };
    let result = if error_len > 0 {
        // SAFETY: 约定 error/error_len 指向本次调用期内有效的一段 UTF-8
        Err(unsafe { text_of(error, error_len) })
    } else {
        // SAFETY: 约定 body/body_len 指向本次调用期内有效的一段字节
        Ok(unsafe { copy_of(body, body_len) })
    };
    // 接收端可能已经不在了（那次等待被超时、或插件被停用），静默收下即可
    let _ = sender.send(result);
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
    HttpText { ptr: value.as_ptr(), len: value.len() }
}

fn pair((name, value): &(String, String)) -> HttpPair {
    HttpPair { name: bytes_of(name), value: bytes_of(value) }
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
    let task = async move { report(done, ticket, local(request).await) };
    if crate::runtime::reactor::spawn(task).is_none() {
        report(done, ticket, Err(format!("宿主的运行时还没起来，发不了 {url}")));
    }
}

fn report(done: HttpDone, ticket: Ticket, result: Result<Vec<u8>, String>) {
    match result {
        Ok(body) => {
            crate::runtime::log::debug(format!("[HTTP] 代发成功，响应 {} 字节", body.len()));
            done(ticket.0, null(), 0, body.as_ptr(), body.len())
        }
        Err(reason) => {
            crate::runtime::log::debug(format!("[HTTP] 代发失败 {reason}"));
            done(ticket.0, reason.as_ptr(), reason.len(), null(), 0)
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

/// 宿主自己那份常驻客户端：连接池与 keepalive 都开着——跑在宿主的 runtime 上，
/// hyper 派生的后台任务本来就看得见上下文，用不着像插件自己发请求那样把池子关掉。
fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// 真发一次请求：非 2xx 与传输失败都归成 `Err`，文案与插件自己写 reqwest 时一贯的写法一致
async fn local(request: Request) -> Result<Vec<u8>, String> {
    let url = request.url.clone();
    let method = match request.method {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
    };
    let mut call = client()
        .request(method, &request.url)
        .timeout(request.timeout.unwrap_or(DEFAULT_TIMEOUT));
    for (name, value) in &request.headers {
        call = call.header(name.as_str(), value.as_str());
    }
    call = match &request.body {
        Body::None => call,
        Body::Form(items) => call.form(items),
        Body::Raw { content_type, text } => {
            call.header(reqwest::header::CONTENT_TYPE, content_type.as_str()).body(text.clone())
        }
    };
    let response = call.send().await.map_err(|err| format!("请求失败 {url}: {err}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| format!("读取响应失败 {url}: {err}"))?;
    if !status.is_success() {
        return Err(format!("请求 {url} 返回 HTTP {status}"));
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// 一次性的本地 HTTP 回应：接一条连接，把收到的请求原样当正文答出去，然后收摊。
    /// 只监听 127.0.0.1，离线可跑，不依赖任何外部站点。
    async fn echo_server(status: u16) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else { return };
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
        echoed.lines().any(|line| line.trim_end().eq_ignore_ascii_case(header))
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
        let text = post_form(&url, &[("game-alias", "ba")], &[("id", "12")]).await.unwrap();
        assert!(text.starts_with("POST /probe HTTP/1.1"), "{text}");
        assert!(has_header(&text, "game-alias: ba"), "{text}");
        assert!(text.ends_with("id=12"), "表单正文没到: {text}");
    }

    #[tokio::test]
    async fn a_json_post_carries_its_content_type() {
        let url = echo_server(200).await;
        let text = post_json(&url, &[] as &[(&str, &str)], "{\"id\":1}").await.unwrap();
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
        assert!(reason.contains("HTTP 503"), "失败原因里该带状态码: {reason}");
        assert!(reason.contains("/probe"), "失败原因里该带 URL: {reason}");
    }

    #[tokio::test]
    async fn an_unreachable_url_is_reported_not_panicked() {
        // 127.0.0.1 上没人监听的端口：要的是"报错返回"，不是崩。
        // 本机挂着系统代理时 reqwest 会跟着走，那条路回来的是代理给的 502 而不是连接拒绝，
        // 所以这里只认"Err 且原因里说清了是哪一种"。
        let reason = get("http://127.0.0.1:1/none", &[] as &[(&str, &str)]).await.unwrap_err();
        assert!(
            reason.contains("请求失败") || reason.contains("返回 HTTP"),
            "连不上该有明确原因: {reason}"
        );
        assert!(reason.contains("/none"), "原因里该带 URL: {reason}");
    }

    #[tokio::test]
    async fn images_come_back_as_bytes() {
        let url = echo_server(200).await;
        let bytes = get_bytes(&url, &[("referer", "https://example.invalid/")]).await.unwrap();
        assert!(bytes.starts_with(b"GET /probe"), "{}", String::from_utf8_lossy(&bytes));
    }

    /// 过一道边界走宿主：这正是插件 dll 里跑的那条路，只是这里宿主与插件同在一个镜像里。
    #[tokio::test]
    async fn the_bridge_path_delivers_the_same_body() {
        attach_host(host_submit);
        let url = echo_server(200).await;
        let text = get(&url, &[("x-via", "bridge")]).await.unwrap();
        assert!(has_header(&text, "x-via: bridge"), "{text}");
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
        };
        let request = unsafe { take_call(&good) }.unwrap();
        assert_eq!(request.method, Method::Get);
        assert_eq!(request.url, url);
        assert_eq!(request.timeout, None);

        assert!(unsafe { take_call(null()) }.is_err_and(|reason| reason.contains("空指针")));
        let dangling = HttpCall { url: HttpText { ptr: null(), len: 8 }, ..good };
        assert!(unsafe { take_call(&dangling) }.is_err_and(|reason| reason.contains("空指针")));
        let too_long =
            HttpCall { url: HttpText { ptr: url.as_ptr(), len: TEXT_LIMIT + 1 }, ..good };
        assert!(unsafe { take_call(&too_long) }.is_err_and(|reason| reason.contains("超出上限")));
        let bad_utf8 = HttpCall { url: HttpText { ptr: [0xFFu8].as_ptr(), len: 1 }, ..good };
        assert!(
            unsafe { take_call(&bad_utf8) }.is_err_and(|reason| reason.contains("合法 UTF-8"))
        );
        let bogus_method = HttpCall { method: 7, ..good };
        assert!(unsafe { take_call(&bogus_method) }.is_err_and(|reason| reason.contains("方法码")));
        let lying_count = HttpCall { header_count: 3, ..good };
        assert!(unsafe { take_call(&lying_count) }.is_err_and(|reason| reason.contains("请求头")));
        let too_many = HttpCall { header_count: PAIR_LIMIT + 1, ..good };
        assert!(unsafe { take_call(&too_many) }.is_err_and(|reason| reason.contains("最多")));
        let no_url = HttpCall { url: HttpText::default(), ..good };
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
        assert!(
            matches!(&request.body, Body::Raw { content_type, text }
                if content_type == "application/json" && text == "{\"a\":1}")
        );
        assert_eq!(request.timeout, Some(Duration::from_millis(1500)));

        let form = Request::form("/f", vec![("id".to_string(), "12".to_string())]);
        assert!(matches!(&form.body, Body::Form(items) if items.len() == 1));
        assert!(Request::get("/g").url == "/g" && matches!(Request::get("/g").body, Body::None));
    }
}
