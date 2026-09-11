//! 活动日历图渲染（对应原版 util/ActivityUtil.createActivityImage + ImageUtil）
//! 白底排版：标题 + 日期、正在进行/即将开始 两个分组；每个活动一行圆角色块，
//! 底色按 activity_type.level 映射（橙/紫/蓝/红/绿 五档），白字左对齐，
//! 右侧显示“今天/明天/后天 X点 开始/结束”的易读时间，底部标注数据来源。
//! 字体不可用时渲染返回 Err，由命令层回退为文本日历。
//! 产物为固定文件名 activity-<服务>.png, 属于「本地资源图片」: 由每日 0 点与活动到期后
//! 5 分钟的定时任务刷新(见 standalone::commands::activity), /活动 命中本地图片时直接发送。

use crate::data::activity::type_level;
use crate::entity::{Activity, ServerLocale};
use crate::image::draw;
use crate::image::text;
use crate::runtime::paths;
use crate::util::time::translate_readable_time;
use image::RgbaImage;
use std::path::PathBuf;

const TITLE_SIZE: f32 = 46.0;
const DATE_SIZE: f32 = 26.0;
const SECTION_SIZE: f32 = 34.0;
const CONTENT_SIZE: f32 = 30.0;
const TIME_SIZE: f32 = 23.0;
const FOOTER_SIZE: f32 = 22.0;
const PAD_X: i32 = 30;
const PAD_Y: i32 = 34;
const MIN_WIDTH: i32 = 620;
const MAX_WIDTH: i32 = 1800;

/// 渲染某服务器活动日历并保存 PNG，返回文件路径
pub fn render(
    pair: &(Vec<Activity>, Vec<Activity>),
    server: ServerLocale,
) -> Result<PathBuf, String> {
    if !text::available() {
        return Err("未找到可用中文字体, 无法生成活动图片".to_string());
    }
    let (active, pending) = pair;
    let mut active = active.clone();
    let mut pending = pending.clone();
    active.sort_by_key(|a| a.end_time);
    pending.sort_by_key(|a| a.start_time);
    let active_rows = collect_rows(&active, false);
    let pending_rows = collect_rows(&pending, true);

    let title = format!("{}活动日历", server.server_name());
    let date = chrono::Local::now().format("%Y/%m/%d").to_string();
    let footer = "数据来源: https://ba.gamekee.com/";

    // 画布宽度: 取标题与最宽“内容+时间”行的自然宽度, 限制在 MIN..MAX
    let mut widest = text::measure(&title, TITLE_SIZE) as i32;
    widest = widest.max(text::measure(&date, DATE_SIZE) as i32);
    for row in active_rows.iter().chain(pending_rows.iter()) {
        widest = widest.max(row.content_width());
    }
    widest = widest.max(text::measure(footer, FOOTER_SIZE) as i32);
    let width = (widest + PAD_X * 2).clamp(MIN_WIDTH, MAX_WIDTH);

    // 画布高度 = 按绘制顺序累加各元素盒高
    let mut height = PAD_Y;
    height += line_box(TITLE_SIZE);
    height += line_box(DATE_SIZE) + 10;
    height += draw_sections_preview(&active_rows, &pending_rows, width);
    height += line_box(FOOTER_SIZE) + PAD_Y;

    let mut img = RgbaImage::new(width as u32, height.max(120) as u32);
    draw::fill_rect(&mut img, 0, 0, width, height, draw::rgb(255, 255, 255));

    let mut y = PAD_Y as f32;
    // 标题（居中）
    let title_h = line_box(TITLE_SIZE);
    draw_line_centered(&mut img, &title, y, title_h, TITLE_SIZE, INK);
    y += title_h as f32;
    // 日期（右对齐）
    let date_h = line_box(DATE_SIZE);
    draw_line_right(&mut img, &date, width, y, date_h, DATE_SIZE, GRAY);
    y += date_h as f32 + 10.0;
    // 正在进行
    y = draw_section_rows(&mut img, y, width, "正在进行", &active_rows);
    // 即将开始
    y = draw_section_rows(&mut img, y, width, "即将开始", &pending_rows);
    // 数据来源
    let footer_h = line_box(FOOTER_SIZE);
    draw_line_left(&mut img, footer, y, footer_h, FOOTER_SIZE, GRAY);

    let file = image_path(server);
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    img.save(&file)
        .map_err(|err| format!("保存活动图片失败: {err}"))?;
    Ok(file)
}

/// 本地活动日历图路径: arona-standalone/images/activity/activity-<服务>.png
pub fn image_path(server: ServerLocale) -> PathBuf {
    paths::images_root()
        .join("activity")
        .join(format!("activity-{}.png", server.command_name()))
}

/// 本地已刷新好的活动日历图: 存在即直接复用(命令层不再重新渲染), 不存在返回 None
pub fn cached_image(server: ServerLocale) -> Option<PathBuf> {
    let file = image_path(server);
    file.exists().then_some(file)
}

const INK: draw::Color = [40, 44, 52, 255];
const GRAY: draw::Color = [120, 124, 132, 255];
const WHITE: draw::Color = [255, 255, 255, 255];

/// 单个活动展示行
struct Row {
    content: String,
    time_text: String,
    level: i32,
}

impl Row {
    fn content_width(&self) -> i32 {
        let content_w = text::measure(&self.content, CONTENT_SIZE) as i32;
        let time_w = if self.time_text.is_empty() {
            0
        } else {
            text::measure(&self.time_text, TIME_SIZE) as i32 + 30
        };
        content_w + time_w + PAD_X
    }

    fn box_height(&self) -> i32 {
        let line = line_box(CONTENT_SIZE).max(line_box(TIME_SIZE));
        line + 16
    }
}

fn collect_rows(activities: &[Activity], future: bool) -> Vec<Row> {
    if activities.is_empty() {
        return vec![Row {
            content: "无".to_string(),
            time_text: String::new(),
            level: 1,
        }];
    }
    activities
        .iter()
        .map(|activity| {
            let ts = if future {
                activity.start_time
            } else {
                activity.end_time
            };
            Row {
                content: activity.content.clone(),
                time_text: translate_readable_time(ts, future),
                level: type_level(activity.activity_type),
            }
        })
        .collect()
}

/// 单行文本的盒高（行距 + 上下留白）
fn line_box(size: f32) -> i32 {
    let line = text::line_metrics(size)
        .map(|(_, _, new_line)| new_line)
        .unwrap_or(size * 1.3);
    line.ceil() as i32
}

/// 预览高度: 两个分组标题 + 所有活动行盒高之和
fn draw_sections_preview(active_rows: &[Row], pending_rows: &[Row], width: i32) -> i32 {
    let section_h = line_box(SECTION_SIZE) + 12;
    let mut total = section_h * 2;
    for rows in [active_rows, pending_rows] {
        for row in rows {
            let _ = width;
            total += row.box_height() + 8;
        }
    }
    total
}

/// 绘制一个分组（标题 + 色块行），返回下一元素起点 y
fn draw_section_rows(img: &mut RgbaImage, y: f32, width: i32, label: &str, rows: &[Row]) -> f32 {
    let mut cursor = y;
    // 分组标题: 左侧色条 + 深色文字
    let section_h = line_box(SECTION_SIZE);
    draw::fill_rect(
        img,
        PAD_X - 6,
        cursor as i32 + 4,
        8,
        (section_h - 8) as i32,
        draw::rgb(90, 150, 255),
    );
    draw_line_left(img, label, cursor, section_h, SECTION_SIZE, INK);
    cursor += section_h as f32 + 12.0;
    for row in rows {
        let block_h = row.box_height();
        draw_activity_block(img, row, width, cursor, block_h);
        cursor += block_h as f32 + 8.0;
    }
    cursor
}

fn draw_activity_block(img: &mut RgbaImage, row: &Row, width: i32, y: f32, block_h: i32) {
    if row.content == "无" {
        let line_h = line_box(CONTENT_SIZE).max(line_box(TIME_SIZE));
        let top = y + (block_h as f32 - line_h as f32) / 2.0;
        draw_line_left_at(
            img,
            "无",
            PAD_X as f32,
            top,
            line_h as f32,
            CONTENT_SIZE,
            GRAY,
        );
        return;
    }
    // 内容超出可用宽度时从尾部裁剪并加省略号（保留右侧时间）
    let mut content = row.content.clone();
    let time_w = if row.time_text.is_empty() {
        0
    } else {
        text::measure(&row.time_text, TIME_SIZE) as i32 + 34
    };
    let available = width - PAD_X * 2 - time_w;
    if text::measure(&content, CONTENT_SIZE) as i32 > available {
        while !content.is_empty() && text::measure(&content, CONTENT_SIZE) as i32 > available {
            content.pop();
        }
        content.push('…');
    }
    // 整行圆角色块
    draw::fill_rounded(
        img,
        PAD_X - 10,
        y as i32,
        width - (PAD_X - 10) * 2,
        block_h,
        22,
        level_color(row.level),
    );
    let box_top =
        y + (block_h as f32 - line_box(CONTENT_SIZE).max(line_box(TIME_SIZE)) as f32) / 2.0;
    let line_h = line_box(CONTENT_SIZE).max(line_box(TIME_SIZE));
    draw_line_left_at(
        img,
        &content,
        PAD_X as f32,
        box_top,
        line_h as f32,
        CONTENT_SIZE,
        WHITE,
    );
    if !row.time_text.is_empty() {
        let time_w = text::measure(&row.time_text, TIME_SIZE);
        draw_line_at(
            img,
            &row.time_text,
            width as f32 - PAD_X as f32 - time_w,
            box_top,
            line_h as f32,
            TIME_SIZE,
            WHITE,
        );
    }
}

/// level -> 色块颜色（对应 ActivityColorMap: 1橙 2紫 3蓝 4红 5绿）
fn level_color(level: i32) -> draw::Color {
    match level {
        2 => draw::rgb(138, 43, 226),
        3 => draw::rgb(16, 126, 247),
        4 => draw::rgb(245, 108, 108),
        5 => draw::rgb(103, 194, 58),
        _ => draw::rgb(255, 140, 0),
    }
}

// ---- 文字排版（top 为文本盒顶, 盒内按行高垂直居中） ----

fn draw_line_left(
    img: &mut RgbaImage,
    s: &str,
    top: f32,
    box_h: i32,
    size: f32,
    color: draw::Color,
) {
    draw_line_left_at(img, s, PAD_X as f32, top, box_h as f32, size, color);
}

fn draw_line_left_at(
    img: &mut RgbaImage,
    s: &str,
    x: f32,
    top: f32,
    box_h: f32,
    size: f32,
    color: draw::Color,
) {
    draw_line_at(img, s, x, top, box_h, size, color);
}

fn draw_line_right(
    img: &mut RgbaImage,
    s: &str,
    width: i32,
    top: f32,
    box_h: i32,
    size: f32,
    color: draw::Color,
) {
    let text_w = text::measure(s, size);
    draw_line_at(
        img,
        s,
        width as f32 - PAD_X as f32 - text_w,
        top,
        box_h as f32,
        size,
        color,
    );
}

fn draw_line_centered(
    img: &mut RgbaImage,
    s: &str,
    top: f32,
    box_h: i32,
    size: f32,
    color: draw::Color,
) {
    let text_w = text::measure(s, size);
    draw_line_at(
        img,
        s,
        (img.width() as f32 - text_w) / 2.0,
        top,
        box_h as f32,
        size,
        color,
    );
}

fn draw_line_at(
    img: &mut RgbaImage,
    s: &str,
    x: f32,
    top: f32,
    box_h: f32,
    size: f32,
    color: draw::Color,
) {
    let baseline = top + box_h / 2.0 + size * 0.42;
    text::draw_line(img, s, x, baseline, size, color);
}
