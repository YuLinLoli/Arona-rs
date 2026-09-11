//! 仅测试环境使用的图片渲染探针与像素级自检：
//! 用合成数据生成抽卡结果图/活动日历图并核对关键区域颜色，
//! 运行: cargo test render -- --nocapture

use crate::entity::{Activity, ActivityType, ServerLocale};
use image::{GenericImageView, RgbaImage};

/// 统计区域内接近指定颜色的像素数
fn count_color(img: &RgbaImage, r: u8, g: u8, b: u8, tol: u8) -> usize {
    let mut count = 0usize;
    for pixel in img.pixels() {
        let [pr, pg, pb, _] = pixel.0;
        if pr.abs_diff(r) <= tol && pg.abs_diff(g) <= tol && pb.abs_diff(b) <= tol {
            count += 1;
        }
    }
    count
}

fn sample_activities() -> (Vec<Activity>, Vec<Activity>) {
    let now = chrono::Utc::now().timestamp_millis();
    let mut active = Vec::new();
    let mut pending = Vec::new();
    let specs = [
        ("总力战: ビナー", ActivityType::DecisiveBattle, true),
        ("【复刻】沙漠の狐の対抗戦", ActivityType::Activity, true),
        ("学院交流会 3rd", ActivityType::CollegeExchangeDrop, true),
        ("特殊任务 经验值2倍", ActivityType::SpecialDrop, true),
        ("常规维护", ActivityType::Maintenance, false),
        ("生日: 砂狼シロコ", ActivityType::Birthday, true),
        ("PickUp: 星野(水着)", ActivityType::PickUp, false),
    ];
    for (index, (name, kind, is_active)) in specs.iter().enumerate() {
        let start = now - 86400_000 * (index as i64 % 3);
        let end = now + 86400_000 * (index as i64 + 2);
        let activity = Activity::new(name.to_string(), ServerLocale::JP, start, end, *kind);
        if *is_active {
            active.push(activity);
        } else {
            pending.push(activity);
        }
    }
    (active, pending)
}

fn save_preview(path: &std::path::Path, width: u32) -> std::path::PathBuf {
    let preview = path.with_extension("preview.png");
    let image = image::open(path).expect("读取图片失败");
    let scaled = image.resize(
        width,
        width * image.height() / image.width(),
        image::imageops::FilterType::Triangle,
    );
    scaled.save(&preview).expect("保存预览失败");
    preview
}

#[ignore = "渲染冒烟自检(需系统中文字体), 运行: cargo test render -- --ignored"]
#[test]
fn render_probe() {
    assert!(crate::image::text::available(), "本机未找到可用中文字体");
    let pair = sample_activities();
    let path = crate::image::activity::render(&pair, ServerLocale::JP).expect("活动图渲染失败");
    println!("activity image: {}", path.display());
    let preview = save_preview(&path, 900);
    println!("activity preview: {}", preview.display());
    let img = image::open(&path).expect("读取活动图失败").to_rgba8();
    // 背景为白色
    assert!(
        count_color(&img, 255, 255, 255, 0) > 40_000,
        "活动图背景应为白色"
    );
    // 分组色块: 蓝(决战 level3) / 紫(普通活动 level2) / 橙(掉落 level1) / 绿(生日 level5)
    assert!(
        count_color(&img, 16, 126, 247, 12) > 1_000,
        "缺少蓝色决战色块"
    );
    assert!(
        count_color(&img, 138, 43, 226, 12) > 1_000,
        "缺少紫色普通活动色块"
    );
    assert!(
        count_color(&img, 255, 140, 0, 12) > 1_000,
        "缺少橙色掉落色块"
    );
    assert!(
        count_color(&img, 103, 194, 58, 12) > 1_000,
        "缺少绿色生日色块"
    );
    // 红(维护)在即将开始分组
    assert!(
        count_color(&img, 245, 108, 108, 12) > 1_000,
        "缺少红色维护色块"
    );
    // 深色标题文字存在
    assert!(count_color(&img, 40, 44, 52, 20) > 200, "缺少标题文字");
}

#[ignore = "渲染冒烟自检(需系统中文字体), 运行: cargo test render -- --ignored"]
#[tokio::test]
async fn render_gacha_probe() {
    use crate::gacha::{DrawReport, DrawResult};
    let mut results = Vec::new();
    for index in 0..10i64 {
        let star = (index % 3 + 1) as i32;
        results.push(DrawResult {
            star,
            name: format!("学生{}", index + 1),
            desc_name: format!("学生{}", index + 1),
            dev_name: String::new(),
            id: 10000 + index,
            avatar: String::new(),
            custom: index == 0 && star == 3,
            is_pickup: star == 3 && index < 3,
        });
    }
    let report = DrawReport {
        server: ServerLocale::JP,
        times: 10,
        results,
        hit_pickup: true,
        star1: 7,
        star2: 2,
        star3: 1,
        points: 10,
        pity_count: 3,
    };
    let path = crate::image::gacha::render_result(&report)
        .await
        .expect("抽卡图渲染失败");
    println!("gacha image: {}", path.display());
    let preview = save_preview(&path, 880);
    println!("gacha preview: {}", preview.display());
    let img = image::open(&path).expect("读取抽卡图失败").to_rgba8();
    // 背景渐变: 顶部偏蓝(160,213,246), 底部偏白(250,241,241)
    let top = img.get_pixel(20, 10).0;
    assert!(
        top[0].abs_diff(160) < 8 && top[1].abs_diff(213) < 8 && top[2].abs_diff(246) < 8,
        "顶部背景色错误: {top:?}"
    );
    let bottom = img.get_pixel(20, 1060).0;
    assert!(
        bottom[0].abs_diff(250) < 8 && bottom[1].abs_diff(241) < 8 && bottom[2].abs_diff(241) < 8,
        "底部背景色错误: {bottom:?}"
    );
    // 无头像占位渐变(140,180,210)->(96,134,168)盖住头像区
    let cell = img.get_pixel(500, 200).0;
    assert!(
        cell[2] > cell[0] && (90..170).contains(&cell[0]),
        "占位渐变异常: {cell:?}"
    );
    // 星级底板为灰蓝色渐变 (112,128,148)->(76,92,112)
    let plate = img.get_pixel(520, 330).0;
    assert!(
        plate[2] > plate[0] && plate[0] <= 130 && plate[2] >= 100,
        "星级底板颜色异常: {plate:?}"
    );
    // 金色星星像素存在(第一张1星 => 一颗, 位于底板中央附近)
    let gold = count_color(&img, 255, 224, 120, 60);
    assert!(gold > 150, "金色星星像素过少: {gold}");
    // 3星粉紫边框(第三列第一行卡片 index2, x=FIRST_COL+2*282=1032)
    let border = img.get_pixel(1033, 170).0;
    assert!(
        border[0].abs_diff(206) < 30 && border[2].abs_diff(240) < 30,
        "3星边框颜色异常: {border:?}"
    );
    // 右下角保底块为青色渐变并含白色计数文字
    let block = img.get_pixel(1900, 920).0;
    assert!(block[1].abs_diff(190) < 45, "保底块颜色异常: {block:?}");
    let white = count_color(&img, 255, 255, 255, 6);
    assert!(white > 1_000, "白色文字像素过少: {white}");
}
