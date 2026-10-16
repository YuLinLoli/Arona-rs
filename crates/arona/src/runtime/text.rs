//! 惰性字面：一份字体文件的字节 + 一层有界字形缓存，GUI 之外的出图插件共用这一份实现。
//!
//! 为什么这件事归框架：三个出图插件过去各自写了一份一模一样的东西（读文件 → 解析轮廓 →
//! 按 (字符, 字号) 缓存 → Weak 化），三份代码要同步改三遍。收在这里之后插件只保留自己的
//! 排版与贴图，字面这一层只有一个实现。
//!
//! **收的是代码，不是那份常驻字节**：每个插件 dll 各自静态链接一份 arona，所以三张图同时在画时
//! 进程里仍然有三份字体字节（思源宋体常规 11.1 MB × 3）。要真收成一份得让宿主代发栅格
//! （`HostBridge` 加一条通道，那是抬符号布局的事），这一档不做。
//!
//! 为什么是惰性的：fontdue 那种"把字体文件里每个码位的轮廓一次全解析完"的路子，实测同一份雅黑
//! 常驻 307.6 MB、思源宋体 253.5 MB，装载还要 0.3~0.6 s。这里只把文件字节收下来（0.01 s），
//! 画到哪个字才解析哪个字，栅格结果进一层有界 LRU。两档后端的几何逐字比过（TrueType 与 CFF
//! 两类轮廓都验），包围盒与 advance 最大偏差 0 格、0.0 px——换的不是观感。
//!
//! 落点口径沿用 fontdue 的 `Metrics`（[`Metrics::ymin`] 是相对基线的**下**边界、向上为正），
//! 而 ab_glyph 的 `px_bounds()` 是 y 向下的，取一次 `-(bottom)` 换回来，插件那一路不用动。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use ab_glyph::{Font as _, Glyph, ScaleFont};

/// 字形缓存的上限。一张卡实际用到的 (字符, 字号) 组合在几百到一两千这一档，4096 条按 1~2 KB
/// 算是几 MB——比全量预解析的 160~250 MB 差两个数量级，又足够让一整轮里反复出现的字都命中。
const GLYPH_CACHE_MAX: usize = 4096;

/// 认的字形文件后缀（思源宋体是 `.otf`，用户换成 `.ttf`/`.ttc` 也该能用）
const EXTENSIONS: [&str; 3] = ["otf", "ttf", "ttc"];

/// 一个字栅格后的位置与大小。`xmin`/`ymin` 是相对笔位置（水平笔位、垂直基线）的左边界与
/// **下**边界，y 向上为正。
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub xmin: i32,
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance: f32,
}

/// 单行文字的基线指标，口径与 fontdue 的 `horizontal_line_metrics` 一致：
/// `descent` 通常在基线以下为负，`new_line_size = ascent - descent + line_gap`。
#[derive(Clone, Copy, Debug)]
pub struct LineMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub line_gap: f32,
    pub new_line_size: f32,
}

/// 缓存里存的那一条：落点 + 覆盖率位图。取用只加一次引用
type Cached = Arc<(Metrics, Vec<u8>)>;

/// 一份字面：字体文件的原始字节 + 一层按 (字符, 字号) 计的字形缓存
pub struct Face {
    font: ab_glyph::FontArc,
    glyphs: Mutex<Glyphs>,
}

/// 有界的字形 LRU。顺序表记"最近用过"，到顶从头上摘最久没用的那一条；
/// 上限只有几千，线性挪位的开销比栅格化本身低两个数量级
struct Glyphs {
    map: HashMap<(char, u32), Cached>,
    order: VecDeque<(char, u32)>,
}

impl Glyphs {
    /// 命中就把这条挪到队尾
    fn touch(&mut self, key: (char, u32)) {
        if let Some(at) = self.order.iter().position(|have| *have == key) {
            self.order.remove(at);
        }
        self.order.push_back(key);
    }

    /// 装满了就先丢最久没被用的那条，再放新的
    fn put(&mut self, key: (char, u32), done: Cached) {
        while self.map.len() >= GLYPH_CACHE_MAX && self.map.len() > 1 {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.map.remove(&oldest);
                }
                None => break,
            }
        }
        self.map.insert(key, done);
        self.touch(key);
    }
}

impl Face {
    /// 读一个字体文件成一份字面。`.ttc` 是字体集合，ab_glyph 取的是第 0 个 face。
    ///
    /// 装载只是把文件字节收下来（思源宋体 11.1 MB，实测 0.01 s），字形要等真画到那个字才解析。
    pub fn open(path: &Path) -> Result<Face, String> {
        let bytes =
            std::fs::read(path).map_err(|err| format!("读字体 {} 失败：{err}", path.display()))?;
        match ab_glyph::FontArc::try_from_vec(bytes) {
            Ok(font) => Ok(Face {
                font,
                glyphs: Mutex::new(Glyphs {
                    map: HashMap::new(),
                    order: VecDeque::new(),
                }),
            }),
            Err(err) => Err(format!("字体 {} 解析失败：{err}", path.display())),
        }
    }

    /// 一个字占多宽（排版只用这一个数，不必解析轮廓）
    pub fn advance(&self, ch: char, px: f32) -> f32 {
        let scaled = self.font.as_scaled(self.scale(px));
        scaled.h_advance(scaled.glyph_id(ch))
    }

    /// 一串字占多宽（不含表情方块，那部分由调用方按图算）
    pub fn text_width(&self, text: &str, px: f32) -> f32 {
        text.chars().map(|ch| self.advance(ch, px)).sum()
    }

    /// 一个字的覆盖率位图（0~255），加它的落点
    pub fn rasterize(&self, ch: char, px: f32) -> (Metrics, Vec<u8>) {
        let done = self.raster(ch, px);
        (done.0, done.1.clone())
    }

    /// 单行基线指标。字库没给 hhea 那一档（`units_per_em` 读不出来）时回 `None`，
    /// 由调用方决定兜底行距——这里不替插件猜，猜错了整张图的行高都会跳。
    pub fn line_metrics(&self, px: f32) -> Option<LineMetrics> {
        let em = self.font.units_per_em()?;
        if em == 0.0 {
            return None;
        }
        let k = px / em;
        let (ascent, descent, line_gap) = (
            self.font.ascent_unscaled() * k,
            self.font.descent_unscaled() * k,
            self.font.line_gap_unscaled() * k,
        );
        Some(LineMetrics {
            ascent,
            descent,
            line_gap,
            new_line_size: ascent - descent + line_gap,
        })
    }

    /// 这个字在这份字面里有没有真字形（缺字形会落到 `.notdef`，描出来是空的）。
    /// 探测"这份字体能不能渲染中文"用它，别拿整篇文案去试。
    pub fn renders(&self, ch: char) -> bool {
        let (metrics, _) = self.rasterize(ch, 24.0);
        metrics.width > 0 && metrics.height > 0
    }

    /// 把"字号 = em 的像素数"换成 ab_glyph 要的 `PxScale`。
    ///
    /// ab_glyph 的 `PxScale` 基准不是 em，而是 `height_unscaled() = ascent - descent`
    /// （雅黑那一档是 1320 而 unitsPerEm 是 1000）——不换算所有文字会整体小 24%，
    /// 卡片版式全乱。换算式两边一约：`scale / height_unscaled == px / units_per_em`。
    fn scale(&self, px: f32) -> f32 {
        let em = self.font.units_per_em().unwrap_or(1000.0);
        if em == 0.0 {
            return px;
        }
        px * self.font.height_unscaled() / em
    }

    fn raster(&self, ch: char, px: f32) -> Cached {
        let key = (ch, px.to_bits());
        let mut cache = self
            .glyphs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(hit) = cache.map.get(&key) {
            let hit = Arc::clone(hit);
            cache.touch(key);
            return hit;
        }
        let done = Arc::new(uncached(&self.font, self.scale(px), ch));
        cache.put(key, Arc::clone(&done));
        done
    }
}

/// 真正解析一个字的轮廓并铺成覆盖率位图（缓存没命中时才走这里）
fn uncached(font: &ab_glyph::FontArc, scale: f32, ch: char) -> (Metrics, Vec<u8>) {
    let scaled = font.as_scaled(scale);
    let id = scaled.glyph_id(ch);
    let advance = scaled.h_advance(id);
    let glyph: Glyph = id.with_scale(scale);
    let Some(outline) = font.outline_glyph(glyph) else {
        return (
            Metrics {
                xmin: 0,
                ymin: 0,
                width: 0,
                height: 0,
                advance,
            },
            Vec::new(),
        );
    };
    // `px_bounds` 是 **y 向下**的像素包围盒（min.y 是顶边），而 [`Metrics`] 沿用 fontdue 的口径：
    // `ymin` 是相对基线的**下**边界、向上为正。换算就一次取负。
    let bounds = outline.px_bounds();
    let (left, right) = (bounds.min.x.floor(), bounds.max.x.ceil());
    let (top, bottom) = (bounds.min.y.floor(), bounds.max.y.ceil());
    let width = (right - left).max(0.0) as usize;
    let height = (bottom - top).max(0.0) as usize;
    let mut coverage = vec![0u8; width * height];
    if width > 0 && height > 0 {
        outline.draw(|x, y, value| {
            let (x, y) = (x as usize, y as usize);
            if x < width && y < height {
                coverage[y * width + x] = (value * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        });
    }
    (
        Metrics {
            xmin: left as i32,
            ymin: -(bottom as i32),
            width,
            height,
            advance,
        },
        coverage,
    )
}

/// 一次装载好的常规 + 可选粗体两份字面。
///
/// 粗体是**另一份文件**（随包那档是 `SourceHanSerifCN-Bold.otf`），不是把常规描粗：
/// 四亚像素偏移叠画那种假粗在衬线体上会糊掉笔画。没有粗体那一档时 [`Typeface::face`] 退回常规，
/// 由调用方决定要不要再自己合成。
pub struct Typeface {
    paths: FaceKey,
    regular: Face,
    bold: Option<Face>,
}

/// 缓存的键：解析出来的（常规, 可选粗体）两把路径
pub type FaceKey = (PathBuf, Option<PathBuf>);

/// 按解析出的字体文件组合缓存字面，**只存 Weak**：谁在画谁拿着 Arc，最后一个落下就整个还掉，
/// 下一轮真要画时再装载一次（0.01 s 量级）。惰性后端下这份常驻本来就只是字体文件的字节加一层
/// 有界字形缓存，不再是要长期按着的百 MB 轮廓。
/// 键用解析后的文件路径而不是配置原文：配置换了写法但指着同一组文件时，不该再存第二份。
static CACHE: OnceLock<Mutex<HashMap<FaceKey, Weak<Typeface>>>> = OnceLock::new();

impl Typeface {
    /// 拿这一组文件对应的字面：缓存里还有人拿着就复用，没有才读盘装载。
    ///
    /// 返回的一直是强引用：拿着它的那一段就保证字面不被换掉。同一档文件组合并发进来时只装载一次，
    /// 后到的排在锁上等同一份——字体文件那二十来 MB 字节不该在进程里存两份。
    pub fn acquire(paths: FaceKey) -> Result<Arc<Typeface>, String> {
        let cache = CACHE.get_or_init(Default::default);
        let mut map = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(alive) = map.get(&paths).and_then(Weak::upgrade) {
            return Ok(alive);
        }
        let regular = Face::open(&paths.0)?;
        let bold = paths
            .1
            .as_ref()
            .filter(|path| path.is_file())
            .map(|path| Face::open(path))
            .transpose()?;
        let face = Arc::new(Typeface {
            paths: paths.clone(),
            regular,
            bold,
        });
        // 已经没人用的高级键就地清掉：这张表只留「现在正被拿着的」那几档
        map.retain(|_, weak| weak.strong_count() > 0);
        map.insert(paths, Arc::downgrade(&face));
        Ok(face)
    }

    /// 按配置值取字面。`configured` 是「常规;粗体」两把路径或两个文件名（后缀可省），
    /// 留空走 [`resolve`] 那三级。`extra_folders` 是插件自己的字体目录（按文件名找），
    /// 比如 `config/<插件>/font/`。
    pub fn discover(configured: &str, extra_folders: &[PathBuf]) -> Result<Arc<Typeface>, String> {
        Self::acquire(resolve(configured, extra_folders)?)
    }

    /// 这一档字面实际用的两把路径（日志用）
    pub fn paths(&self) -> &FaceKey {
        &self.paths
    }

    pub fn regular(&self) -> &Face {
        &self.regular
    }

    /// 有没有真的粗体那一档（没有的话 [`Self::face(true)`] 会退回常规，
    /// 想要假粗的插件得自己叠）
    pub fn has_bold(&self) -> bool {
        self.bold.is_some()
    }

    pub fn face(&self, bold: bool) -> &Face {
        if bold {
            self.bold.as_ref().unwrap_or(&self.regular)
        } else {
            &self.regular
        }
    }
}

/// 系统里那几份中文字体（按优先级；粗体可空）
const CANDIDATES: &[(&str, Option<&str>)] = &[
    (
        r"C:\Windows\Fonts\msyh.ttc",
        Some(r"C:\Windows\Fonts\msyhbd.ttc"),
    ),
    (r"C:\Windows\Fonts\simhei.ttf", None),
    (
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        Some("/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc"),
    ),
    (
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        None,
    ),
    ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", None),
    ("/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc", None),
    ("/System/Library/Fonts/PingFang.ttc", None),
];

/// 字面取哪一份，三级：配置指定 > 框架随包那份（安装目录 `fonts/`，见
/// [`crate::runtime::assets::cjk_font`]）> 系统候选。
///
/// 中间那级让 GUI 与所有出图插件用上同一套字，观感因此不随"这台机器装了什么字体"漂。
/// 配置里两档用 `;` 或 `,` 分隔；每一档可以先当路径用，再到 `extra_folders` 里按文件名找。
pub fn resolve(configured: &str, extra_folders: &[PathBuf]) -> Result<FaceKey, String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let (main, bold) = match configured.split_once([';', ',']) {
            Some((main, bold)) => (main.trim().to_string(), Some(bold.trim().to_string())),
            None => (configured.to_string(), None),
        };
        let regular = locate(&main, extra_folders).ok_or_else(|| {
            format!(
                "配置里指定的字体不存在：{main}（写绝对路径，或把字体文件放进随包 fonts/ 与插件自己的 font 目录）"
            )
        })?;
        let bold = match bold {
            Some(name) => Some(
                locate(&name, extra_folders)
                    .ok_or_else(|| format!("配置里指定的粗体字体不存在：{name}"))?,
            ),
            None => None,
        };
        return Ok((regular, bold));
    }
    if let Some(bundled) = crate::runtime::assets::cjk_font() {
        return Ok(bundled);
    }
    CANDIDATES
        .iter()
        .filter_map(|(regular, bold)| {
            let path = PathBuf::from(regular);
            path.is_file().then(|| (path, bold.map(PathBuf::from)))
        })
        .next()
        .ok_or_else(|| {
            "找不到中文字体：安装目录的 fonts/ 里没有字体文件，系统里也没命中候选。 \
             可以在插件配置里指定字体文件路径"
                .to_string()
        })
}

/// 先当路径试，再当 `extra_folders` 里的文件名试（`.ttf/.otf/.ttc` 可省）
pub fn locate(name: &str, extra_folders: &[PathBuf]) -> Option<PathBuf> {
    let direct = PathBuf::from(name);
    if direct.is_file() {
        return Some(direct);
    }
    extra_folders
        .iter()
        .find_map(|folder| locate_in(name, folder))
}

/// 按配置里那一个词到 `folder` 里按文件名找，后缀 `.ttf/.otf/.ttc` 可省
/// （写了路径分隔符的只认最后那一段，不去翻别处）。
pub fn locate_in(name: &str, folder: &Path) -> Option<PathBuf> {
    let file = Path::new(name).file_name()?;
    for extension in ["", ".ttf", ".otf", ".ttc"] {
        let candidate = if extension.is_empty() {
            folder.join(file)
        } else {
            folder.join(format!("{}{extension}", file.to_string_lossy()))
        };
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 这一档后缀像不像字形文件（`assets` 那一级也用它）
pub fn is_font_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arona-text-{}-{}",
            std::process::id(),
            names.join("+").len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in names {
            std::fs::write(dir.join(name), b"placeholder").unwrap();
        }
        dir
    }

    #[test]
    fn a_bare_font_name_finds_its_file_in_the_extra_folders() {
        let dir = sandbox(&["MyFont.ttf"]);
        assert_eq!(
            locate("MyFont", &[dir.clone()]).as_deref(),
            Some(dir.join("MyFont.ttf").as_path())
        );
        assert_eq!(
            locate("MyFont.ttf", &[dir.clone()]).as_deref(),
            Some(dir.join("MyFont.ttf").as_path())
        );
        assert_eq!(
            locate("sub/MyFont", &[dir.clone()]).as_deref(),
            Some(dir.join("MyFont.ttf").as_path()),
            "配置里手滑带上目录时，只认最后那一段"
        );
        assert_eq!(locate("NoSuchFont", &[dir.clone()]), None);
        // 给了完整路径就当路径试，跟 folder 无关
        assert_eq!(
            locate(
                dir.join("MyFont.ttf").to_string_lossy().as_ref(),
                &[dir.clone()]
            )
            .as_deref(),
            Some(dir.join("MyFont.ttf").as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配置里那串「常规;粗体」两档都要能找到，缺哪一档就报哪一档
    #[test]
    fn the_configured_pair_splits_on_a_semicolon() {
        let dir = sandbox(&["A.ttf", "B.ttf"]);
        let configured = format!("A;{}", dir.join("B.ttf").display());
        let (regular, bold) = resolve(&configured, &[dir.clone()]).unwrap();
        assert_eq!(regular, dir.join("A.ttf"));
        assert_eq!(bold, Some(dir.join("B.ttf")));
        let missing = resolve("A;Nope", &[dir.clone()]).unwrap_err();
        assert!(missing.contains("粗体"), "报错要说清缺的是哪一档");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn font_extension_is_the_gate() {
        assert!(is_font_file(Path::new("a.otf")));
        assert!(is_font_file(Path::new("a.TTC")));
        assert!(!is_font_file(Path::new("LICENSE.txt")));
    }

    /// 留空时那三级的顺序：随包那份命中就不去看系统候选。
    /// 这一条要改工作目录，所以只在临时目录里当"没有随包字体"验一遍退回逻辑。
    #[test]
    fn without_a_bundled_face_it_falls_back_to_system_candidates() {
        let dir = std::env::temp_dir().join(format!("arona-text-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir.join("fonts"));
        std::fs::create_dir_all(dir.join("fonts")).unwrap();
        let previous = std::env::current_dir().ok();
        if std::env::set_current_dir(&dir).is_ok() {
            let back = resolve("", &[]);
            let _ = previous.map(|path| std::env::set_current_dir(path));
            // 这台机器上要么命中候选、要么明确报"找不到"，不能 panic
            assert!(back.is_ok() || back.unwrap_err().contains("找不到中文字体"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真字面这一档：随包那两份在仓库里（`installer/fonts/`），拿不到才退到系统候选。
    /// 一条"工作目录没有 fonts/ 就跳过"的测试等于没测，所以这里按路径自己找。
    fn any_real_face() -> Option<PathBuf> {
        let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../installer/fonts/SourceHanSerifCN-Regular.otf");
        if bundled.is_file() {
            return Some(bundled);
        }
        CANDIDATES
            .iter()
            .map(|(regular, _)| PathBuf::from(regular))
            .find(|path| path.is_file())
    }

    #[test]
    fn a_real_face_measures_and_rasterizes_hanzi() {
        let Some(path) = any_real_face() else {
            panic!("这台机器上既没有仓库里的随包字体，也没有系统候选，这一条没法验");
        };
        let face = Face::open(&path).expect("真字面该打得开");
        assert!(face.renders('活'), "中文字面必须能渲染「活」");
        assert!(face.advance('活', 32.0) > 30.0, "32 px 的汉字不该窄于 30");
        let metrics = face.line_metrics(32.0).expect("hhea 该读得出来");
        assert!(metrics.ascent > 0.0 && metrics.descent < 0.0);
        assert!(
            (metrics.new_line_size - (metrics.ascent - metrics.descent + metrics.line_gap)).abs()
                < 1e-4
        );
        // 一个字的栅格：覆盖率位图该按 width*height 铺满，且真有非零像素
        let (glyph, coverage) = face.rasterize('活', 32.0);
        assert_eq!(coverage.len(), glyph.width * glyph.height);
        assert!(coverage.iter().any(|value| *value > 0));
        // 没映射到的码位不该 panic：落到 `.notdef` 之后要么空要么一个豆腐框，
        // 这两种都是字体自己的事，这里只保证 `coverage.len()` 与落点自洽
        let (missing, coverage) = face.rasterize('\u{0530}', 32.0);
        assert_eq!(coverage.len(), missing.width * missing.height);
    }

    /// 字形缓存到顶就丢最久没被用的那条——这张表不该随文案长度一直涨
    #[test]
    fn the_glyph_cache_stays_bounded() {
        let Some(path) = any_real_face() else {
            panic!("这台机器上没有可打开的真字面，这一条验不了上限");
        };
        let face = Face::open(&path).unwrap();
        let pool: Vec<char> = (0..(GLYPH_CACHE_MAX + 40) as u32)
            .map(|offset| char::from_u32(0x4E00 + offset).unwrap_or('一'))
            .collect();
        for ch in &pool[..GLYPH_CACHE_MAX] {
            face.rasterize(*ch, 32.0);
        }
        assert_eq!(
            face.glyphs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .map
                .len(),
            GLYPH_CACHE_MAX
        );
        // 再挤十条进去：队头那批该被摘掉，队尾那条该留着
        for ch in &pool[GLYPH_CACHE_MAX..GLYPH_CACHE_MAX + 10] {
            face.rasterize(*ch, 32.0);
        }
        let cache = face
            .glyphs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            cache.map.len() <= GLYPH_CACHE_MAX,
            "缓存涨到 {} 条，上限是 {}",
            cache.map.len(),
            GLYPH_CACHE_MAX
        );
        assert!(
            cache
                .map
                .contains_key(&(pool[GLYPH_CACHE_MAX - 1], 32.0f32.to_bits())),
            "最久没用的该让位，刚用过的不该被丢"
        );
    }

    /// 缓存的是"这一组文件"那一份字面：同一路径两次 acquire 该拿到同一个 Arc，
    /// 全部放手之后该真的还掉（Weak 那条口径的判据）
    #[test]
    fn the_same_pair_of_files_shares_one_loaded_face() {
        let Some(path) = any_real_face() else {
            panic!("这台机器上没有可打开的真字面");
        };
        let key = (path.clone(), None);
        let first = Typeface::acquire(key.clone()).unwrap();
        let second = Typeface::acquire(key.clone()).unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "同一组文件该复用同一份字面，不该各读一遍盘"
        );
        assert_eq!(first.paths(), &key);
        assert!(!first.has_bold(), "没给粗体路径就不该有粗体那一档");
        drop(second);
        drop(first);
        // 全放手之后再来一次：允许是新装载的一份，只要不是同一份被按着不放就行
        let third = Typeface::acquire(key).unwrap();
        assert!(third.regular().renders('活'));
    }
}
