//! 抽卡结果图渲染（对应原版 gacha/GachaV2ImageRenderer）
//! 画布 2340x1080：渐变背景 + 5x2 卡片网格 + 右下角保底计数块。
//! 头像优先读取本地缓存 images/gacha/avatar/<角色id>.*，未命中时按 avatar 地址下载；
//! 任何头像下载/解码失败都降级为渐变占位（彩蛋显示 A，普通显示 ?），不影响成图。

use crate::data::http;
use crate::gacha::{DrawReport, DrawResult};
use crate::image::draw;
use crate::image::text;
use image::RgbaImage;
use std::path::{Path, PathBuf};

const CANVAS_WIDTH: i32 = 2340;
const CANVAS_HEIGHT: i32 = 1080;

const CELL_WIDTH: i32 = 246;
const CELL_HEIGHT: i32 = 258;
const COL_STEP: i32 = 282;
const ROW_STEP: i32 = 342;
const FIRST_COL: i32 = 468;
const FIRST_ROW: i32 = 138;
const STAR_PLATE_HEIGHT: i32 = 66;

const COUNT_X: i32 = 1812;
const COUNT_Y: i32 = 870;
const COUNT_WIDTH: i32 = 368;
const COUNT_HEIGHT: i32 = 115;

const CARD_RADIUS: i32 = 24;
const STAR_OUTER: f64 = 30.0;
const STAR_INNER: f64 = 15.0;
const STAR_SPACING: f64 = 74.0;
/// 卡片圆角 24、图片结果均为 2-3MB 级别，头像扩展名候选
const AVATAR_EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];

fn avatar_dir() -> PathBuf {
    crate::image_dir().join("gacha").join("avatar")
}

fn result_dir() -> PathBuf {
    crate::image_dir().join("gacha").join("result")
}

/// 渲染抽卡结果图并保存为 PNG，返回文件路径。
///
/// 头像加载（未命中本地缓存时联网）留在异步侧；绘制 + PNG 编码是纯 CPU 的重活，
/// 交给阻塞线程池执行，避免占住 tokio 工作线程让日志/收消息卡住（见 `image::cpu_bound`）。
pub async fn render_result(report: &DrawReport) -> Result<PathBuf, String> {
    if report.results.is_empty() || report.results.len() > 10 {
        return Err(format!("抽卡结果数量异常: {}", report.results.len()));
    }
    let mut avatars = Vec::with_capacity(report.results.len());
    for result in &report.results {
        avatars.push(load_avatar(result).await);
    }
    let report = report.clone();
    crate::image::cpu_bound(move || draw_report(&report, &avatars)).await
}

/// 纯 CPU 部分：渐变背景 + 卡片网格 + 保底计数块 + PNG 落盘
fn draw_report(report: &DrawReport, avatars: &[Option<RgbaImage>]) -> Result<PathBuf, String> {
    let mut img = RgbaImage::new(CANVAS_WIDTH as u32, CANVAS_HEIGHT as u32);
    // 背景: 上(160,213,246) -> 下(250,241,241) 垂直渐变
    draw::fill_rect_vgrad(
        &mut img,
        0,
        0,
        CANVAS_WIDTH,
        CANVAS_HEIGHT,
        draw::rgb(160, 213, 246),
        draw::rgb(250, 241, 241),
    );
    for (index, result) in report.results.iter().enumerate() {
        draw_cell(
            &mut img,
            index,
            result,
            avatars.get(index).and_then(|it| it.as_ref()),
        );
    }
    draw_count_block(&mut img, report.pity_count);
    let file = new_result_file();
    img.save(&file)
        .map_err(|err| format!("保存抽卡结果图失败: {err}"))?;
    Ok(file)
}

/// 生成唯一结果文件路径 images/gacha/result/gacha-<时间戳>-<随机8位>.png
fn new_result_file() -> PathBuf {
    let dir = result_dir();
    let _ = std::fs::create_dir_all(&dir);
    let millis = chrono::Utc::now().timestamp_millis();
    let random: String = uuid::Uuid::new_v4().to_string().chars().take(8).collect();
    dir.join(format!("gacha-{millis}-{random}.png"))
}

/// 读取本地头像缓存或下载：返回可解码的 RGBA 图
async fn load_avatar(result: &DrawResult) -> Option<RgbaImage> {
    let dir = avatar_dir();
    if let Some(cached) = read_cached(&dir, result.id) {
        return Some(cached);
    }
    let url = result.avatar.trim();
    if url.is_empty() {
        return None;
    }
    let normalized = if url.starts_with("//") {
        format!("https:{url}")
    } else {
        url.to_string()
    };
    let referer = if normalized.contains("gamekee.com") {
        "https://www.gamekee.com/ba/"
    } else {
        "https://ba.kivo.wiki/"
    };
    let bytes = match http::get_bytes(&normalized, referer).await {
        Ok(bytes) if !bytes.is_empty() => bytes,
        _ => {
            arona::runtime::log::warning(format!("下载抽卡头像失败 {normalized}"));
            return None;
        }
    };
    let decoded = image::load_from_memory(&bytes)
        .map(|dyn_image| dyn_image.to_rgba8())
        .map_err(|err| format!("头像解码失败 {normalized}: {err}"))
        .ok()?;
    // 仅保存可解码的图片，避免下次反复下载坏文件
    let _ = std::fs::create_dir_all(&dir);
    let suffix = url_extension(&normalized);
    let file = dir.join(format!("{}.{suffix}", result.id));
    if std::fs::write(&file, &bytes).is_ok() {
        arona::runtime::log::info(format!("抽卡头像已缓存: {}", file.to_string_lossy()));
    }
    Some(decoded)
}

fn read_cached(dir: &Path, id: i64) -> Option<RgbaImage> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .collect();
    names.sort();
    for name in names {
        if !name.starts_with(&format!("{id}.")) {
            continue;
        }
        if !AVATAR_EXTS
            .iter()
            .any(|ext| name.ends_with(&format!(".{ext}")))
        {
            continue;
        }
        let path = dir.join(&name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if let Ok(dyn_image) = image::load_from_memory(&bytes) {
            return Some(dyn_image.to_rgba8());
        }
    }
    None
}

/// 从 URL 中提取图片扩展名（query 截断，非法时回退 png）
fn url_extension(url: &str) -> String {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let ext = clean.rsplit('.').next().unwrap_or("");
    if (2..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        ext.to_lowercase()
    } else {
        "png".to_string()
    }
}

/// 画单张卡片
fn draw_cell(img: &mut RgbaImage, index: usize, result: &DrawResult, avatar: Option<&RgbaImage>) {
    let col = (index % 5) as i32;
    let row = (index / 5) as i32;
    let x = FIRST_COL + col * COL_STEP;
    let y = FIRST_ROW + row * ROW_STEP;
    // 卡片背景: 白 -> 浅蓝(226,240,251)
    draw::fill_rounded_vgrad(
        img,
        x,
        y,
        CELL_WIDTH,
        CELL_HEIGHT,
        CARD_RADIUS,
        draw::rgb(255, 255, 255),
        draw::rgb(226, 240, 251),
    );
    // 头像区域: 卡片去掉底部星级底板
    let avatar_area_w = CELL_WIDTH;
    let avatar_area_h = CELL_HEIGHT - STAR_PLATE_HEIGHT;
    match avatar {
        Some(image) => {
            draw::draw_cover(img, image, x, y, avatar_area_w, avatar_area_h, |px, py| {
                draw::inside_round(
                    px as f64 + 0.5,
                    py as f64 + 0.5,
                    x as f64,
                    y as f64,
                    CELL_WIDTH as f64,
                    CELL_HEIGHT as f64,
                    CARD_RADIUS as f64,
                )
            });
        }
        None => draw_placeholder(img, result, x, y, avatar_area_w, avatar_area_h),
    }
    // 星级底板: 深灰蓝渐变, 位于头像下方
    let plate_y = y + CELL_HEIGHT - STAR_PLATE_HEIGHT;
    draw::fill_rounded_vgrad(
        img,
        x,
        plate_y,
        CELL_WIDTH,
        STAR_PLATE_HEIGHT,
        CARD_RADIUS,
        draw::rgb(112, 128, 148),
        draw::rgb(76, 92, 112),
    );
    draw::fill_rect(
        img,
        x + 12,
        plate_y + 3,
        CELL_WIDTH - 24,
        2,
        draw::rgb(150, 165, 185),
    );
    // 金色五角星
    let center_x = (x + CELL_WIDTH / 2) as f64;
    let center_y = (plate_y + STAR_PLATE_HEIGHT / 2 + 1) as f64;
    if result.star > 0 {
        let start_x = center_x - STAR_SPACING * (result.star - 1) as f64 / 2.0;
        for i in 0..result.star {
            let cx = start_x + i as f64 * STAR_SPACING;
            draw::fill_star(
                img,
                cx,
                center_y,
                STAR_OUTER,
                STAR_INNER,
                draw::rgb(255, 224, 120),
                draw::rgb(238, 168, 46),
                draw::rgb(180, 122, 28),
            );
        }
    }
    // 星级边框: 3星粉紫(206,96,240) / 2星金(242,178,46) / 1星白
    let border = match result.star {
        3 => draw::rgb(206, 96, 240),
        2 => draw::rgb(242, 178, 46),
        _ => draw::rgb(255, 255, 255),
    };
    draw::stroke_rounded(img, x, y, CELL_WIDTH, CELL_HEIGHT, CARD_RADIUS, 8, border);
}

/// 无头像占位: 圆角渐变 + 白色 A(彩蛋)/?(普通)
fn draw_placeholder(img: &mut RgbaImage, result: &DrawResult, x: i32, y: i32, w: i32, h: i32) {
    draw::fill_rounded_vgrad(
        img,
        x + 8,
        y + 8,
        w - 16,
        h - 16,
        20,
        draw::rgb(140, 180, 210),
        draw::rgb(96, 134, 168),
    );
    let marker = if result.custom { "A" } else { "?" };
    if text::available() {
        text::draw_centered(img, marker, x, y, w, h, 100.0, draw::rgb(255, 255, 255));
    }
}

/// 右下角保底计数块: 青色渐变圆角块 + "{pity}抽"
fn draw_count_block(img: &mut RgbaImage, pity_count: i64) {
    draw::fill_rounded_vgrad(
        img,
        COUNT_X,
        COUNT_Y,
        COUNT_WIDTH,
        COUNT_HEIGHT,
        30,
        draw::rgb(112, 216, 250),
        draw::rgb(56, 164, 226),
    );
    // 上半部半透明白高光
    draw::fill_rounded(
        img,
        COUNT_X,
        COUNT_Y,
        COUNT_WIDTH,
        COUNT_HEIGHT / 2,
        30,
        draw::rgba(255, 255, 255, 56),
    );
    if text::available() {
        text::draw_centered(
            img,
            &format!("{pity_count}抽"),
            COUNT_X,
            COUNT_Y,
            COUNT_WIDTH,
            COUNT_HEIGHT,
            58.0,
            draw::rgb(255, 255, 255),
        );
    }
}
