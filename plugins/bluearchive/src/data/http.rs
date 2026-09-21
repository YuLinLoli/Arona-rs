//! 简单的 HTTP 请求封装（对应原版 NetworkUtil + Jsoup 请求头）
//! 全部接口使用 async reqwest，调用方需处于 tokio 运行时中。

use once_cell::sync::Lazy;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::time::Duration;

/// 浏览器 UA（对应原版 gameKeeHeaders 中的 user-agent）
pub const USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";
/// arona 云端接口使用的 UA（对应原版 NetworkUtil.request）
pub const BACKEND_USER_AGENT_VALUE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/102.0.0.0 Safari/537.36";

const ACCEPT_LANGUAGE_VALUE: &str = "zh-CN,zh;q=0.9,zh-Hans;q=0.8,und;q=0.7,zh-Hant;q=0.6,ja;q=0.5";
const JSON_ACCEPT: &str = "application/json, text/plain, */*";
const IMAGE_ACCEPT: &str = "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8";

/// 全局共享客户端：复用连接池，避免每个请求都重新 TCP+TLS 握手
static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

fn client() -> reqwest::Client {
    CLIENT.clone()
}

/// 把 (名, 值) 列表转成 HeaderMap，非法字段名/值直接忽略
fn build_headers(headers: &[(&str, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (key, value) in headers {
        if let (Ok(name), Ok(value)) = (key.parse::<HeaderName>(), HeaderValue::from_str(value)) {
            map.insert(name, value);
        }
    }
    map
}

/// GameKee 各 JSON 接口的完整浏览器请求头（对应原版 GameKeeUtil.gameKeeHeaders）。
///
/// 注意：host/connection 由 HTTP 栈自动处理，accept-encoding 由 reqwest 自动协商并解压，
/// 手动设置会与自动解压冲突，因此这里不重复设置这两类字段。
pub fn game_kee_headers(referer: &str) -> Vec<(&'static str, String)> {
    vec![
        ("accept", JSON_ACCEPT.to_string()),
        ("accept-language", ACCEPT_LANGUAGE_VALUE.to_string()),
        ("device-num", "1".to_string()),
        ("dnt", "1".to_string()),
        ("game-alias", "ba".to_string()),
        ("lang", "zh-cn".to_string()),
        ("referer", referer.to_string()),
        (
            "sec-ch-ua",
            "\"Not;A=Brand\";v=\"8\", \"Chromium\";v=\"150\", \"Google Chrome\";v=\"150\""
                .to_string(),
        ),
        ("sec-ch-ua-mobile", "?0".to_string()),
        ("sec-ch-ua-platform", "\"Windows\"".to_string()),
        ("sec-fetch-dest", "empty".to_string()),
        ("sec-fetch-mode", "cors".to_string()),
        ("sec-fetch-site", "same-origin".to_string()),
        ("user-agent", USER_AGENT_VALUE.to_string()),
        ("x-requested-with", "XMLHttpRequest".to_string()),
    ]
}

/// GameKee 图片 CDN 下载头（对应原版 downloadImages）
pub fn game_kee_image_headers(referer: &str) -> Vec<(&'static str, String)> {
    vec![
        ("accept", IMAGE_ACCEPT.to_string()),
        ("accept-language", ACCEPT_LANGUAGE_VALUE.to_string()),
        ("referer", referer.to_string()),
        ("user-agent", USER_AGENT_VALUE.to_string()),
    ]
}

/// api-cdn.gamekee.com 内容 JSON 请求头（对应原版 getScheduleNoteImages 中的内联请求头）
pub fn game_kee_content_headers(referer: &str) -> Vec<(&'static str, String)> {
    vec![
        ("accept", JSON_ACCEPT.to_string()),
        ("accept-language", ACCEPT_LANGUAGE_VALUE.to_string()),
        ("dnt", "1".to_string()),
        ("origin", "https://www.gamekee.com".to_string()),
        ("referer", referer.to_string()),
        (
            "sec-ch-ua",
            "\"Not;A=Brand\";v=\"8\", \"Chromium\";v=\"150\", \"Google Chrome\";v=\"150\""
                .to_string(),
        ),
        ("sec-ch-ua-mobile", "?0".to_string()),
        ("sec-ch-ua-platform", "\"Windows\"".to_string()),
        ("sec-fetch-dest", "empty".to_string()),
        ("sec-fetch-mode", "cors".to_string()),
        ("sec-fetch-site", "same-site".to_string()),
        ("user-agent", USER_AGENT_VALUE.to_string()),
    ]
}

/// arona 云端鉴权头（对应原版 NetworkUtil.requestWithAuth）
pub fn arona_backend_headers(uuid: &str, version: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", uuid.to_string()),
        ("version", version.to_string()),
        ("user-agent", BACKEND_USER_AGENT_VALUE.to_string()),
    ]
}

/// 带指定请求头的 GET，返回响应文本
pub async fn get_with(url: &str, headers: &[(&str, String)]) -> Result<String, String> {
    let response = client()
        .get(url)
        .headers(build_headers(headers))
        .send()
        .await
        .map_err(|err| format!("请求失败 {url}: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| format!("读取响应失败 {url}: {err}"))?;
    if !status.is_success() {
        return Err(format!("请求 {url} 返回 HTTP {status}"));
    }
    Ok(text)
}

/// 带指定请求头的 GET，返回原始字节
pub async fn get_bytes_with(url: &str, headers: &[(&str, String)]) -> Result<Vec<u8>, String> {
    let response = client()
        .get(url)
        .headers(build_headers(headers))
        .send()
        .await
        .map_err(|err| format!("请求失败 {url}: {err}"))?;
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

/// 带指定请求头的表单 POST，返回响应文本
pub async fn post_form_with(
    url: &str,
    headers: &[(&str, String)],
    form: &[(&str, String)],
) -> Result<String, String> {
    let response = client()
        .post(url)
        .headers(build_headers(headers))
        .form(form)
        .send()
        .await
        .map_err(|err| format!("请求失败 {url}: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| format!("读取响应失败 {url}: {err}"))?;
    if !status.is_success() {
        return Err(format!("请求 {url} 返回 HTTP {status}"));
    }
    Ok(text)
}

/// 通用浏览器 GET + referer + 少量额外头（kivo.wiki 等非 GameKee 接口使用）
pub async fn get(url: &str, referer: &str, extra: &[(&str, &str)]) -> Result<String, String> {
    let mut headers: Vec<(&str, String)> = vec![
        ("user-agent", USER_AGENT_VALUE.to_string()),
        ("accept", JSON_ACCEPT.to_string()),
        ("accept-language", ACCEPT_LANGUAGE_VALUE.to_string()),
        ("referer", referer.to_string()),
    ];
    for (key, value) in extra {
        headers.push((key, (*value).to_string()));
    }
    get_with(url, &headers).await
}

/// 通用浏览器 GET 图片字节（学生头像/塔罗图片下载）
pub async fn get_bytes(url: &str, referer: &str) -> Result<Vec<u8>, String> {
    let headers: Vec<(&str, String)> = vec![
        ("user-agent", USER_AGENT_VALUE.to_string()),
        ("accept", IMAGE_ACCEPT.to_string()),
        ("accept-language", "zh-CN,zh;q=0.9".to_string()),
        ("referer", referer.to_string()),
    ];
    get_bytes_with(url, &headers).await
}

/// 探一张网络图片此刻还取不取到：QQ 的图床直链会过期，还原引用前得先问一次。
/// 只索要第一个字节，5 秒内没有成功响应就当作已过期。
pub async fn url_alive(url: &str) -> bool {
    let request = client()
        .get(url)
        .timeout(Duration::from_secs(5))
        .header("range", "bytes=0-0")
        .header("user-agent", USER_AGENT_VALUE)
        .header("accept", IMAGE_ACCEPT);
    matches!(request.send().await, Ok(response) if response.status().is_success())
}

/// 通用表单 POST（保留给后续非 GameKee 接口使用）
pub async fn post_form(url: &str, form: &[(&str, &str)]) -> Result<String, String> {
    let headers: Vec<(&str, String)> = vec![
        ("user-agent", USER_AGENT_VALUE.to_string()),
        ("accept", JSON_ACCEPT.to_string()),
        ("accept-language", "zh-CN,zh;q=0.9".to_string()),
        ("referer", "https://www.gamekee.com/ba/".to_string()),
        ("game-alias", "ba".to_string()),
    ];
    let form: Vec<(&str, String)> = form
        .iter()
        .map(|(key, value)| (*key, (*value).to_string()))
        .collect();
    post_form_with(url, &headers, &form).await
}
