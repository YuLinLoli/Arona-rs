//! arona 云端图片库与远端动作接口（对应原版 NetworkUtil + GeneralUtils + entity/ImageResult）
//!
//! - `/image?name=`：精确/模糊图片检索
//! - CDN `/image<path>`：图片文件下载
//! - `/action/one?id=`：远端动作（`/抽卡 update` 用）

use arona::runtime::config as runtime_config;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const BACKEND_ADDRESS: &str = "https://arona.diyigemt.com";
pub const BACKEND_API_ADDRESS: &str = "https://arona.diyigemt.com/api/v1";
pub const BACKEND_IMAGE_FOLDER: &str = "/image";
pub const CDN_ADDRESS: &str = "https://arona.cdn.diyigemt.com";

/// 构建版本号，作为 `version` 请求头发送（对应原版 BuildConfig.version）
pub const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 模糊搜索结果标记（对应原版 FuzzyImageResult）
pub const FUZZY_IMAGE_RESULT: i64 = 0;

/// 远端图片条目（对应原版 entity/ImageResult）
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ImageResult {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub r#type: i64,
}

/// 云端响应外壳（对应原版 entity/ServerResponse）
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ServerResponse<T> {
    #[serde(default)]
    pub status: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: T,
}

/// 精确/模糊检索结果（对应原版 entity/ImageRequestResult）
#[derive(Clone, Debug, Default)]
pub struct ImageRequestResult {
    pub list: Vec<ImageResult>,
    pub file: Option<PathBuf>,
}

/// 远端动作条目（对应原版 remote/action 的 RemoteActionItem）
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RemoteActionItem {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub time: String,
}

/// 图片库根目录（等价原版 dataRoot/image）
pub fn image_library_root() -> PathBuf {
    crate::image_dir()
}

/// 图片库本地文件路径（对应原版 GeneralUtils.localImageFile）
pub fn local_image_file(path: &str) -> PathBuf {
    image_library_root().join(path.trim_start_matches('/'))
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 向后端请求图片（对应原版 NetworkUtil.requestImage）
pub async fn request_image(name: &str) -> Result<ServerResponse<Vec<ImageResult>>, String> {
    let url = format!("{BACKEND_API_ADDRESS}/image?name={}", percent_encode(name));
    let headers = super::http::arona_backend_headers(&runtime_config::uuid(), BUILD_VERSION);
    let text = super::http::get_with(&url, &headers).await?;
    serde_json::from_str(&text).map_err(|err| format!("图片检索响应解析失败: {err}"))
}

/// 获取远端动作（对应原版 NetworkUtil.fetchDataFromServer("/action/one")）
pub async fn fetch_remote_action(id: i64) -> Result<ServerResponse<RemoteActionItem>, String> {
    let url = format!("{BACKEND_API_ADDRESS}/action/one?id={id}");
    let headers = super::http::arona_backend_headers(&runtime_config::uuid(), BUILD_VERSION);
    let text = super::http::get_with(&url, &headers).await?;
    serde_json::from_str(&text).map_err(|err| format!("远端动作响应解析失败: {err}"))
}

/// 从 CDN 下载文件并写入本地（对应原版 NetworkUtil.downloadCDNFile）
pub async fn download_cdn_file(path: &str, local_file: &Path) -> Result<(), String> {
    let url = format!("{CDN_ADDRESS}{path}");
    let headers = super::http::arona_backend_headers(&runtime_config::uuid(), BUILD_VERSION);
    let bytes = super::http::get_bytes_with(&url, &headers).await?;
    if let Some(parent) = local_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(local_file, bytes)
        .map_err(|err| format!("写入图片 {} 失败: {err}", local_file.display()))
}

/// 下载图片库文件（对应原版 NetworkUtil.downloadImageFile）
pub async fn download_image_file(path: &str, local_file: &Path) -> Result<(), String> {
    download_cdn_file(&format!("{BACKEND_IMAGE_FOLDER}{path}"), local_file).await
}
