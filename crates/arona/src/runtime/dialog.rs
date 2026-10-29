//! 原生「打开文件」对话框（Windows）。
//!
//! 项目不引 GUI 框架外的对话框库，这里按老规矩直接挂系统 API：`comdlg32` 的
//! `GetOpenFileNameW`。只在 GUI 线程里用——它是模态对话框，会挡住当前帧，
//! 但对"选个文件"这种交互是预期行为。

use std::path::PathBuf;

/// 弹出「选择插件」对话框，返回选中的 .dll 路径；取消/失败返回 `None`。
#[cfg(windows)]
pub fn pick_dll() -> Option<PathBuf> {
    // 过滤串：`插件 (*.dll)\0*.dll\0\0`——两段描述/通配，末尾再来一个空段收尾
    let filter: Vec<u16> = "插件 (*.dll)\0*.dll\0\0".encode_utf16().collect::<Vec<_>>();
    let title: Vec<u16> = "选择要升级的插件 dll\0".encode_utf16().collect();
    let mut buffer = vec![0u16; 1024];

    let mut ofn: OpenFileNameW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OpenFileNameW>() as u32;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = buffer.as_mut_ptr();
    ofn.nMaxFile = buffer.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    // 必须给「文件必须存在」+「不改进程工作目录」：后者尤其关键——
    // 不改的话选一次文件，config/ data/ 等相对路径就全跟着换了家
    ofn.flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

    // SAFETY: 结构体各指针都指向本次调用存活期的有效缓冲，尺寸用 size_of 填
    let ok = unsafe { GetOpenFileNameW(&mut ofn) };
    if ok == 0 {
        return None;
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..end]);
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

#[cfg(not(windows))]
pub fn pick_dll() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
#[cfg(windows)]
const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
#[cfg(windows)]
const OFN_NOCHANGEDIR: u32 = 0x0000_0008;

#[cfg(windows)]
#[link(name = "comdlg32")]
unsafe extern "system" {
    fn GetOpenFileNameW(param: *mut OpenFileNameW) -> i32;
}

/// `OPENFILENAMEW` 的镜像。只声明用得到的字段——`GetOpenFileNameW` 按 `lStructSize`
/// 决定读多少，所以漏掉尾部的字段没关系（`flagsEx` 之后那截不要，避免版本差异）。
#[cfg(windows)]
#[repr(C)]
#[allow(non_snake_case, dead_code)]
struct OpenFileNameW {
    lStructSize: u32,
    hwndOwner: isize,
    hInstance: isize,
    lpstrFilter: *const u16,
    lpstrCustomFilter: *mut u16,
    nMaxCustFilter: u32,
    nFilterIndex: u32,
    lpstrFile: *mut u16,
    nMaxFile: u32,
    lpstrFileTitle: *mut u16,
    nMaxFileTitle: u32,
    lpstrInitialDir: *const u16,
    lpstrTitle: *const u16,
    flags: u32,
    nFileOffset: u16,
    nFileExtension: u16,
    lpstrDefExt: *const u16,
    lCustData: isize,
    lpfnHook: isize,
    lpTemplateName: *const u16,
    pvReserved: *mut std::ffi::c_void,
    dwReserved: u32,
    flagsEx: u32,
}
