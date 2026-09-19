//! 基础绘图原语（对应原版 java.awt.Graphics2D 的矩形/圆角/渐变/星形等操作）
//! 全部为纯 Rust 逐像素绘制，不依赖系统图形库。

use image::RgbaImage;

/// RGBA 颜色（4 通道 0-255）
pub type Color = [u8; 4];

#[inline]
pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    [r, g, b, 255]
}

#[inline]
pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    [r, g, b, a]
}

/// 颜色通道线性插值
#[inline]
fn lerp_channel(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)).round() as u8
}

/// 两个颜色按 t 线性插值
pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    [
        lerp_channel(a[0], b[0], t),
        lerp_channel(a[1], b[1], t),
        lerp_channel(a[2], b[2], t),
        255,
    ]
}

/// 将颜色 color 以覆盖率 alpha(0-255) 混合到目标像素上（画布始终不透明）
#[inline]
pub fn blend_pixel(img: &mut RgbaImage, x: i32, y: i32, color: Color, alpha: u8) {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 || alpha == 0 {
        return;
    }
    let dst = &mut img.get_pixel_mut(x as u32, y as u32).0;
    let a = alpha as u32;
    let sa = (color[3] as u32 * a) / 255;
    let inv_sa = 255 - sa;
    dst[0] = ((color[0] as u32 * sa + dst[0] as u32 * inv_sa) / 255) as u8;
    dst[1] = ((color[1] as u32 * sa + dst[1] as u32 * inv_sa) / 255) as u8;
    dst[2] = ((color[2] as u32 * sa + dst[2] as u32 * inv_sa) / 255) as u8;
    dst[3] = 255;
}

/// 实心矩形
pub fn fill_rect(img: &mut RgbaImage, x: i32, y: i32, w: i32, h: i32, color: Color) {
    if w <= 0 || h <= 0 {
        return;
    }
    for py in y..y + h {
        for px in x..x + w {
            blend_pixel(img, px, py, color, 255);
        }
    }
}

/// 垂直渐变实心矩形
pub fn fill_rect_vgrad(
    img: &mut RgbaImage,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    top: Color,
    bottom: Color,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    for row in 0..h {
        let color = lerp(
            top,
            bottom,
            if h <= 1 {
                0.0
            } else {
                row as f32 / (h - 1) as f32
            },
        );
        let y0 = y + row;
        for px in x..x + w {
            blend_pixel(img, px, y0, color, 255);
        }
    }
}

/// 点是否落在圆角矩形内部（矩形坐标 x0..x0+w, y0..y0+h）
pub(crate) fn inside_round(px: f64, py: f64, x0: f64, y0: f64, w: f64, h: f64, r: f64) -> bool {
    if px < x0 || px >= x0 + w || py < y0 || py >= y0 + h {
        return false;
    }
    let r = r.min(w / 2.0).min(h / 2.0);
    let cx = px.clamp(x0 + r, x0 + w - r);
    let cy = py.clamp(y0 + r, y0 + h - r);
    let dx = px - cx;
    let dy = py - cy;
    dx * dx + dy * dy <= r * r
}

/// 2x2 亚像素采样覆盖率：像素中心落在图形内部的比例
fn coverage(x: i32, y: i32, test: impl Fn(f64, f64) -> bool) -> u8 {
    let mut hits = 0u32;
    for (ox, oy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
        if test(x as f64 + ox, y as f64 + oy) {
            hits += 1;
        }
    }
    ((hits as f32 / 4.0) * 255.0).round() as u8
}

/// 圆角矩形（带抗锯齿边缘）
pub fn fill_rounded(img: &mut RgbaImage, x: i32, y: i32, w: i32, h: i32, r: i32, color: Color) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (x0, y0, wf, hf, rf) = (x as f64, y as f64, w as f64, h as f64, r as f64);
    for py in y..y + h {
        for px in x..x + w {
            let alpha = coverage(px, py, |a, b| inside_round(a, b, x0, y0, wf, hf, rf));
            if alpha > 0 {
                blend_pixel(img, px, py, color, alpha);
            }
        }
    }
}

/// 垂直渐变圆角矩形
pub fn fill_rounded_vgrad(
    img: &mut RgbaImage,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    top: Color,
    bottom: Color,
) {
    if w <= 0 || h <= 0 {
        return;
    }
    let (x0, y0, wf, hf, rf) = (x as f64, y as f64, w as f64, h as f64, r as f64);
    for row in 0..h {
        let color = lerp(
            top,
            bottom,
            if h <= 1 {
                0.0
            } else {
                row as f32 / (h - 1) as f32
            },
        );
        let py = y + row;
        for px in x..x + w {
            let alpha = coverage(px, py, |a, b| inside_round(a, b, x0, y0, wf, hf, rf));
            if alpha > 0 {
                blend_pixel(img, px, py, color, alpha);
            }
        }
    }
}

/// 圆角矩形描边（内缩 thickness 形成圆环）
pub fn stroke_rounded(
    img: &mut RgbaImage,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
    thickness: i32,
    color: Color,
) {
    if w <= 0 || h <= 0 || thickness <= 0 {
        return;
    }
    let (x0, y0, wf, hf, rf) = (x as f64, y as f64, w as f64, h as f64, r as f64);
    let t = thickness as f64;
    let inner_r = (rf - t).max(0.0);
    let inner_x = x0 + t;
    let inner_y = y0 + t;
    let inner_w = wf - 2.0 * t;
    let inner_h = hf - 2.0 * t;
    for py in y..y + h {
        for px in x..x + w {
            let cx = px as f64 + 0.5;
            let cy = py as f64 + 0.5;
            let in_outer = inside_round(cx, cy, x0, y0, wf, hf, rf);
            let in_inner = inside_round(cx, cy, inner_x, inner_y, inner_w, inner_h, inner_r);
            if in_outer && !in_inner {
                blend_pixel(img, px, py, color, 255);
            }
        }
    }
}

/// 由 5 角星外内半径生成 10 个顶点（角度起点朝上）
fn star_points(cx: f64, cy: f64, outer: f64, inner: f64) -> Vec<[f64; 2]> {
    let mut pts = Vec::with_capacity(10);
    for i in 0..10 {
        let radius = if i % 2 == 0 { outer } else { inner };
        let angle = (-90.0f64 + i as f64 * 36.0).to_radians();
        pts.push([cx + radius * angle.cos(), cy + radius * angle.sin()]);
    }
    pts
}

/// 射线法判断点是否在多边形内
fn inside_polygon(pts: &[[f64; 2]], px: f64, py: f64) -> bool {
    let mut inside = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (xi, yi) = (pts[i][0], pts[i][1]);
        let (xj, yj) = (pts[j][0], pts[j][1]);
        if (yi > py) != (yj > py) && px < (xj - xi) * (py - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// 点到线段的最短距离
fn dist_to_segment(px: f64, py: f64, ax: f64, ay: f64, bx: f64, by: f64) -> f64 {
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq <= f64::EPSILON {
        0.0
    } else {
        (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0)
    };
    let qx = ax + t * dx;
    let qy = ay + t * dy;
    let ex = px - qx;
    let ey = py - qy;
    (ex * ex + ey * ey).sqrt()
}

/// 五角星：先填充渐变，再沿边缘画一圈描边
pub fn fill_star(
    img: &mut RgbaImage,
    cx: f64,
    cy: f64,
    outer: f64,
    inner: f64,
    top: Color,
    bottom: Color,
    outline: Color,
) {
    let pts = star_points(cx, cy, outer, inner);
    let min_x = pts
        .iter()
        .map(|p| p[0])
        .fold(f64::INFINITY, f64::min)
        .floor() as i32;
    let max_x = pts
        .iter()
        .map(|p| p[0])
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil() as i32;
    let min_y = pts
        .iter()
        .map(|p| p[1])
        .fold(f64::INFINITY, f64::min)
        .floor() as i32;
    let max_y = pts
        .iter()
        .map(|p| p[1])
        .fold(f64::NEG_INFINITY, f64::max)
        .ceil() as i32;
    let span = (max_y - min_y).max(1) as f32;
    for py in min_y..=max_y {
        let color = lerp(bottom, top, (py as f32 - min_y as f32) / span);
        for px in min_x..=max_x {
            let alpha = coverage(px, py, |a, b| inside_polygon(&pts, a, b));
            if alpha > 0 {
                blend_pixel(img, px, py, color, alpha);
            }
        }
    }
    // 描边: 距任一边缘小于半宽即上色（覆盖内外两侧, 与原版 fill 后 draw 一致）
    let half = 1.75;
    for py in min_y..=max_y {
        for px in min_x..=max_x {
            let x = px as f64 + 0.5;
            let y = py as f64 + 0.5;
            let mut min_d = f64::INFINITY;
            for i in 0..pts.len() {
                let j = (i + 1) % pts.len();
                let d = dist_to_segment(x, y, pts[i][0], pts[i][1], pts[j][0], pts[j][1]);
                if d < min_d {
                    min_d = d;
                }
            }
            if min_d <= half {
                let a = ((half - min_d).clamp(0.0, 1.0) * 255.0).round() as u8;
                blend_pixel(img, px, py, outline, a.max(1));
            }
        }
    }
}

/// 图片等比放大铺满目标区域并居中裁剪绘制（fill-cover）
/// clip: 对目标坐标(x,y)判断是否可绘制的裁剪函数
pub fn draw_cover(
    img: &mut RgbaImage,
    src: &RgbaImage,
    area_x: i32,
    area_y: i32,
    area_w: i32,
    area_h: i32,
    clip: impl Fn(i32, i32) -> bool,
) {
    if area_w <= 0 || area_h <= 0 || src.width() == 0 || src.height() == 0 {
        return;
    }
    let scale = f64::max(
        area_w as f64 / src.width() as f64,
        area_h as f64 / src.height() as f64,
    );
    let draw_w = ((src.width() as f64 * scale).round() as i32).max(1);
    let draw_h = ((src.height() as f64 * scale).round() as i32).max(1);
    let scaled = image::imageops::resize(
        src,
        draw_w as u32,
        draw_h as u32,
        image::imageops::FilterType::Triangle,
    );
    let offset_x = area_x + (area_w - draw_w) / 2;
    let offset_y = area_y + (area_h - draw_h) / 2;
    for py in 0..draw_h {
        for px in 0..draw_w {
            let dx = offset_x + px;
            let dy = offset_y + py;
            if !clip(dx, dy) {
                continue;
            }
            let src_px = scaled.get_pixel(px as u32, py as u32).0;
            blend_pixel(img, dx, dy, src_px, 255);
        }
    }
}
