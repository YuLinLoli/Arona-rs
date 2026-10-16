//! 框架随包资源：目前只有一件中文字体。
//!
//! 为什么这件事归框架：三个出图插件加 GUI 过去各自去系统目录猜一份中文字体，结果是
//! 「这台机器装了就有、没装就画不出字」，而且每台机器的字都不一样，观感没法验收。
//! 安装包把思源宋体简中（SIL OFL 1.1，允许随作品再分发）释到运行目录 `fonts/`，
//! 框架把这一份的位置交给所有要画字的地方——插件不再自带候选表。
//!
//! 插件侧不需要走 [`crate::plugin::abi::HostBridge`]：[`crate::runtime::paths::root`] 就是
//! 工作目录，宿主那份框架代码与每个 dll 里那份算出来的是同一个路径，
//! 所以这是一个纯新增的普通函数，不动符号布局版本。

use std::path::{Path, PathBuf};

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
