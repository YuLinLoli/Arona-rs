//! 框架随包资源：目前只有一件中文字体。
//!
//! 为什么这件事归框架：三个出图插件加 GUI 过去各自去系统目录猜一份中文字体，结果是
//! 「这台机器装了就有、没装就画不出字」，而且每台机器的字都不一样，观感没法验收。
//! 安装包把思源宋体简中（SIL OFL 1.1，允许随作品再分发）释到运行目录 `fonts/`，
//! 框架把这一份交给所有要画字的地方——插件不再自带候选表。
//!
//! 两样东西，两条路：
//! - **路径**不用通道：[`crate::runtime::paths::root`] 就是工作目录，宿主那份框架代码与每个
//!   dll 里那份算出来的是同一个路径，所以 [`cjk_font`] 是个普通函数；
//! - **字节**必须走通道：每个 dll 静态链接自己那份 arona，各自 `fs::read` 一次就是各自一份
//!   11 MB（三个出图插件 + 面板 = 四份）。所以进程里由宿主读一次、常驻一份，
//!   经 [`crate::plugin::abi::HostBridge::fonts`] 把 `&'static [u8]` **借**给每个 dll，
//!   面板那侧直接用同一份（`egui::FontData::from_static`，也不拷）。
//!   借出去的是只读字节，所有权全在宿主——dll 那边 `FontArc::try_from_slice` 不复制、不接管，
//!   dll 卸载了这些字节也还在（它自己不会去 free）。抬一次符号布局换掉 N 份重复的大块常驻。

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::plugin::abi::FontBytes;
use crate::runtime::paths;

/// 随包资源目录：运行目录下的 `fonts/`
pub fn fonts_dir() -> PathBuf {
    paths::root().join("fonts")
}

/// 框架提供的那份中文字体：（常规, 可选粗体）。
///
/// 认的是文件名里的大小写无关的 `regular` / `bold`，所以想换成别的字体只要把两个文件
/// 放进 `fonts/` 就行；目录里只有一份且名字不带字重（如可变字体）时，那一份就当常规用。
/// 一份都没有就回 `None`，由调用方退回系统字体目录——**没装字体不该让功能挂掉**。
pub fn cjk_font() -> Option<(PathBuf, Option<PathBuf>)> {
    let dir = fonts_dir();
    let regular = face_in(&dir, "regular")?;
    Some((regular, face_in(&dir, "bold")))
}

/// 进程里常驻的那一份随包字体字节（`None` 表示这台机器上没有随包字体）
struct Borrowed {
    regular: &'static [u8],
    /// 路径也给出去：dll 那侧要能判断"我按三级解析出来的那档，就是宿主借的这份"，
    /// 用户用 `bili-image.font` 指到别的文件时**不该**用借来的字节
    regular_path: &'static CStr,
    bold: Option<&'static [u8]>,
    bold_path: Option<&'static CStr>,
}

static BORROWED: OnceLock<Option<Borrowed>> = OnceLock::new();

/// 读一次随包字体并**永久常驻**（`Box::leak` / `CString::into_raw`）。
///
/// 为什么必须 leak：借出去的 `&'static [u8]` 要活过每一个 dll 的生命周期，而 dll 什么时候被
/// 卸载不由这里决定。字体文件是随包的、进程内不会换，所以常驻一份正是我们要的语义——
/// 反过来若哪天支持"运行期换字体"，这里就不能再 leak，得改成引用计数。
fn borrowed() -> Option<&'static Borrowed> {
    BORROWED
        .get_or_init(|| {
            let (regular, bold) = cjk_font()?;
            let face = |path: &Path| -> Option<(&'static [u8], &'static CStr)> {
                let bytes: &'static [u8] = Box::leak(std::fs::read(path).ok()?.into_boxed_slice());
                let boxed: Box<CStr> = CString::new(path.as_os_str().as_encoded_bytes())
                    .ok()?
                    .into();
                Some((bytes, Box::leak(boxed)))
            };
            let (regular_bytes, regular_path) = face(&regular)?;
            let bold = bold.as_ref().and_then(|path| face(path));
            Some(Borrowed {
                regular: regular_bytes,
                regular_path,
                bold: bold.map(|(bytes, _)| bytes),
                bold_path: bold.map(|(_, path)| path),
            })
        })
        .as_ref()
}

/// 宿主侧：把随包字体的字节与路径填进 dll 给的槽。
///
/// 没有随包字体时 `regular` 留空指针，dll 那侧照旧自己读盘（系统候选那一级本来就是各读各的）。
///
/// # Safety
/// `out` 必须指向一个 dll 侧的 [`FontBytes`]，且在本调用返回前保持可写。
pub(crate) extern "C" fn host_font_bytes(out: *mut FontBytes) {
    if out.is_null() {
        return;
    }
    // SAFETY: 约定 `out` 是调用方给的有效指针；空指针在上面已经挡了
    let slot = unsafe { &mut *out };
    let Some(borrowed) = borrowed() else {
        return;
    };
    slot.regular = borrowed.regular.as_ptr();
    slot.regular_len = borrowed.regular.len();
    slot.regular_path = borrowed.regular_path.as_ptr();
    if let (Some(bytes), Some(path)) = (borrowed.bold, borrowed.bold_path) {
        slot.bold = bytes.as_ptr();
        slot.bold_len = bytes.len();
        slot.bold_path = path.as_ptr();
    }
}

/// 随包字体的字节（常规, 可选粗体），与借给各 dll 的是**同一份**。
///
/// 面板用它而不是 `fs::read`：`egui::FontData::from_static` 接受 `&'static [u8]` 且不复制，
/// 于是宿主里也只有这一份。没有随包字体回 `None`，调用方退回系统字体目录。
pub fn cjk_font_bytes() -> Option<(&'static [u8], Option<&'static [u8]>)> {
    let borrowed = borrowed()?;
    Some((borrowed.regular, borrowed.bold))
}

/// 在 `dir` 里找一份带 `weight` 字样的字形文件。
///
/// `read_dir` 不保证顺序，所以先收集再按路径排序取第一个：同一台机器上每次必须选到同一份，
/// 否则目录里放了两套字体时，这一轮和下一轮画出来的字会不一样。
fn face_in(dir: &Path, weight: &str) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut with_weight: Vec<PathBuf> = Vec::new();
    let mut any: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || !crate::runtime::text::is_font_file(&path) {
            continue;
        }
        let named = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase().contains(weight))
            .unwrap_or(false);
        if named {
            with_weight.push(path.clone());
        }
        any.push(path);
    }
    with_weight.sort();
    any.sort();
    match with_weight.first() {
        Some(found) => Some(found.clone()),
        // 目录里就一份、名字又不带字重：那一份就是用户要的那份
        None if weight == "regular" && any.len() == 1 => Some(any[0].clone()),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个临时 `fonts/` 目录：只放指定名字的几份文件
    fn sandbox(names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "arona-fonts-{}-{}",
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
    fn picks_the_weighted_pair_and_prefers_regular_over_bold() {
        let dir = sandbox(&["SourceHanSerifCN-Bold.otf", "SourceHanSerifCN-Regular.otf"]);
        assert_eq!(
            face_in(&dir, "regular").unwrap().file_name().unwrap(),
            "SourceHanSerifCN-Regular.otf"
        );
        assert_eq!(
            face_in(&dir, "bold").unwrap().file_name().unwrap(),
            "SourceHanSerifCN-Bold.otf"
        );
        assert!(
            face_in(&dir, "heavy").is_none(),
            "没有那一档就该回 None，不能拿别的字重顶"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只放一份（可变字体那种）时它也当常规用——否则用户换了字体却发现"没生效"
    #[test]
    fn a_single_unweighted_face_counts_as_regular() {
        let dir = sandbox(&["MySerif-VF.ttf"]);
        assert_eq!(
            face_in(&dir, "regular").unwrap().file_name().unwrap(),
            "MySerif-VF.ttf"
        );
        assert!(face_in(&dir, "bold").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 两份都不带字重时不猜：那种情况交给用户自己命名清楚
    #[test]
    fn two_unweighted_faces_are_not_guessed() {
        let dir = sandbox(&["Alpha.otf", "Beta.otf"]);
        assert!(face_in(&dir, "regular").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_font_files_are_ignored() {
        let dir = sandbox(&["README.md", "SourceHanSerifCN-Regular.txt"]);
        assert!(face_in(&dir, "regular").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
