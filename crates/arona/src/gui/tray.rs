//! 托盘图标：关窗最小化后的落脚点。
//!
//! 为什么用 `tray-icon` 而不是自己调 `Shell_NotifyIcon`：那套 API 要自己建隐藏窗口、
//! 手工分派 `WM_APP` 消息、还要管 `TaskbarCreated`（explorer 重启后图标会消失，得重加）。
//! `tray-icon` 在 Windows 上自己起一个消息线程并处理这些，宿主只要轮询它的事件通道。
//!
//! 事件通道是**进程级全局**的（`MenuEvent::receiver` / `TrayIconEvent::receiver`），
//! 所以托盘只做一个实例、长期存活：`AronaGui` 里那份 `Option<Tray>` 被 drop 时，
//! 图标跟着消失。这也意味着它不能每帧重建——重建等于反复摘挂图标，任务栏会闪。
//!
//! 只有 Windows 有托盘（内嵌的窗口图标也是 PNG 直出 RGBA）；Linux 版本来就不含 GUI。

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// 托盘上能发出的两件事
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    /// 左键单击，或菜单里的「打开管理面板」：把窗口叫回前台
    Open,
    /// 菜单里的「退出 Arona-rs」：真退出（连机器人一起停）
    Quit,
}

/// 托盘图标本体。字段里的 `icon` 只为了持有它——drop 即从任务栏消失。
pub struct Tray {
    _icon: TrayIcon,
    open_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
}

impl Tray {
    /// 用窗口图标那份 RGBA 建托盘。建不起来（系统不允许、图标尺寸不对）时返回 `None`，
    /// 界面照常能用，只是关窗时只能选「退出」——不该让一个托盘拖垮整个面板。
    pub fn new(rgba: Vec<u8>, width: u32, height: u32) -> Option<Tray> {
        let icon = Icon::from_rgba(rgba, width, height).ok()?;
        let menu = Menu::new();
        let open = MenuItem::new("打开管理面板", true, None);
        let quit = MenuItem::new("退出 Arona-rs", true, None);
        // 分隔线是 `muda` 的预定义项：不加它，两个菜单项贴在一起，误点的概率明显变高
        menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit])
            .ok()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            // 左键**不弹菜单**：只做「把面板叫回来」。tray-icon 默认是左键也弹菜单，
            // 结果是点一下既弹菜单又要唤回窗口，两个动作抢焦点，用起来像卡了一下。
            // 右键仍然弹菜单（打开面板 / 退出）。
            .with_menu_on_left_click(false)
            .with_tooltip("Arona 管理面板（左键打开，右键菜单）")
            .with_icon(icon)
            .build()
            .ok()?;
        Some(Tray {
            _icon: tray,
            open_id: open.id().clone(),
            quit_id: quit.id().clone(),
        })
    }

    /// 起一个收货线程，把托盘事件翻成 [`super::Msg::Tray`]。
    ///
    /// 为什么不是每帧 `try_recv`：**窗口收进托盘后就不再渲染**，`update` 也不会再被调，
    /// 那时候没人轮询就等于托盘彻底失灵。这个线程是唯一能把"用户点了图标"变成一次唤醒
    /// 的地方——它发出消息后调 `request_repaint()`，主线程才会跑一帧把窗口显示出来。
    ///
    /// 80 ms 的空转一次两个 `try_recv`，代价可以忽略；换成阻塞等又得为"关闭 GUI 后
    /// 怎么让它退出"再造一套信号，不划算——发送端断开（面板关掉）线程会自己结束。
    pub fn start_watching(
        &self,
        tx: std::sync::mpsc::Sender<super::Msg>,
        ctx: super::egui::Context,
    ) {
        let open_id = self.open_id.clone();
        let quit_id = self.quit_id.clone();
        std::thread::spawn(move || {
            loop {
                let mut action = None;
                if let Ok(event) = TrayIconEvent::receiver().try_recv() {
                    // 只认「左键抬起」：按下也认的话，点一下会闪两次（按下+抬起各一次）
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        action = Some(TrayAction::Open);
                    }
                }
                if let Ok(event) = MenuEvent::receiver().try_recv() {
                    if event.id == open_id {
                        action = Some(TrayAction::Open);
                    } else if event.id == quit_id {
                        action = Some(TrayAction::Quit);
                    }
                }
                if let Some(action) = action {
                    // 关键一步：先用 Win32 把窗口叫出来，别指望 egui 的 update 去处理。
                    // 窗口藏着的时候它根本不跑，消息发过去只会烂在队列里。
                    match action {
                        TrayAction::Open => {
                            let shown = panel::show();
                            crate::runtime::log::info(if shown {
                                "托盘：打开管理面板"
                            } else {
                                "托盘：收到打开请求，但没找到面板窗口（标题被人改过？）"
                            });
                        }
                        TrayAction::Quit => {
                            let closed = panel::request_quit();
                            crate::runtime::log::info(if closed {
                                "托盘：收到退出请求，正在停止机器人并退出"
                            } else {
                                "托盘：收到退出请求，但没找到面板窗口，正在走备用路径"
                            });
                        }
                    }
                    // 面板已经关了（接收端断了）就收工，别留一个空转线程
                    if tx.send(super::Msg::Tray(action)).is_err() {
                        break;
                    }
                    // 兜底：窗口没找着的情况下，至少让 egui 试着跑一帧
                    ctx.request_repaint();
                }
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        });
    }
}

/// 直接操作面板窗口（Windows）。
///
/// **为什么不能只靠 egui 的消息**：窗口收进托盘（`ViewportCommand::Visible(false)`）之后，
/// winit 不再派发重绘，`update` 就停了；托盘线程发的那条 `Msg` 没有消费者，
/// `request_repaint()` 也唤不醒一个隐藏窗口。结果就是"点了托盘图标没反应、菜单里点退出也没反应"
/// —— 两个动作一起失灵，正是这个原因。
///
/// 所以关键路径绕开 egui：用 Win32 直接把窗口叫出来（`ShowWindow`），
/// 退出则先置一个 `QUIT_REQUESTED` 标志、再 `PostMessage(WM_CLOSE)`。
/// 窗口一旦可见，`update` 自然恢复，那条 `Msg` 才有人处理。
#[cfg(windows)]
pub mod panel {
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

    #[link(name = "user32")]
    unsafe extern "system" {
        fn FindWindowW(class_name: *const u16, window_name: *const u16) -> isize;
        fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
        fn SetForegroundWindow(hwnd: isize) -> i32;
        fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
        fn EnumWindows(callback: extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        fn GetWindowTextLengthW(hwnd: isize) -> i32;
    }

    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;
    const WM_CLOSE: u32 = 0x0010;

    /// 面板窗口标题。**必须与 `gui::WINDOW_TITLE` 一字不差**（有单测钉着）。
    pub(crate) const PANEL_TITLE: &str = "Arona 管理面板";

    /// 托盘点了「退出」：让 `handle_close` 放行关闭请求，而不是转成"收进托盘"。
    /// 用原子量而不是消息队列，是因为窗口隐藏时消息队列没人消费，这个标志没这个问题。
    static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

    /// `EnumWindows` 的回调拿不到闭包环境，结果只能放全局。找到即写、回调返回 0 停止枚举。
    static FOUND: AtomicIsize = AtomicIsize::new(0);

    extern "system" fn find_own_window(hwnd: isize, _lparam: isize) -> i32 {
        let mut pid = 0u32;
        // SAFETY: hwnd 由 EnumWindows 给出，pid 是本地栈变量
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid == std::process::id() {
            // 只看有标题的：那是我们的主窗口，不是 winit 内部那些空的辅助窗口
            let len = unsafe { GetWindowTextLengthW(hwnd) };
            if len > 0 {
                FOUND.store(hwnd, Ordering::SeqCst);
                return 0;
            }
        }
        1
    }

    /// 找面板窗口。两条路：先按标题（快），找不到再按进程 id 扫自己的顶层窗口。
    ///
    /// 第二条不是多余：标题一旦被改（本地化、加版本号、"没有响应"后缀），
    /// 按标题就永远找不到，而表现是"托盘点了完全没反应"——没有任何报错，最难查。
    fn find() -> Option<isize> {
        let title: Vec<u16> = PANEL_TITLE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: 只传一个以 NUL 结尾的 UTF-16 串；找不到返回 0
        let by_title = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if by_title != 0 {
            return Some(by_title);
        }

        FOUND.store(0, Ordering::SeqCst);
        // SAFETY: 回调只读窗口信息并写全局原子量，不触碰其它状态
        unsafe { EnumWindows(find_own_window, 0) };
        let by_pid = FOUND.load(Ordering::SeqCst);
        (by_pid != 0).then_some(by_pid)
    }

    /// 把面板叫回前台。窗口隐藏时这一步是**必需的**——不先显示它，后面什么都做不了。
    pub fn show() -> bool {
        let Some(hwnd) = find() else {
            return false;
        };
        // SAFETY: hwnd 来自 FindWindow*/EnumWindows，调用期间窗口由面板持有
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
        }
        true
    }

    /// 请求退出：先置标志，再把窗口显示出来并投递 `WM_CLOSE`。
    ///
    /// 顺序不能反：窗口如果还藏着，`WM_CLOSE` 触发的重绘没人画，`handle_close` 就跑不到；
    /// 先显示再关，`update` 会在下一帧看到 `close_requested`，配合标志直接放行。
    pub fn request_quit() -> bool {
        QUIT_REQUESTED.store(true, Ordering::SeqCst);
        let Some(hwnd) = find() else {
            return false;
        };
        // SAFETY: 同上；PostMessageW 只是投递，不等待处理
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        true
    }

    /// 托盘是否已经点过「退出」
    pub fn quit_requested() -> bool {
        QUIT_REQUESTED.load(Ordering::SeqCst)
    }
}

/// 非 Windows 平台（Linux 精简构建不含 GUI）的占位，保持调用方不用写 `cfg`
#[cfg(not(windows))]
pub mod panel {
    pub fn show() -> bool {
        false
    }
    pub fn request_quit() -> bool {
        false
    }
    pub fn quit_requested() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 托盘要的 RGBA 必须是「宽高相乘正好这么多字节」——窗口图标那份数据满足，
    /// 但尺寸传错（比如把 64 传成 32）会让 `Icon::from_rgba` 直接失败，
    /// 表现为"别的都正常，就是没有托盘图标"，最难查。这里把不变式钉住。
    #[test]
    fn icon_bytes_must_match_the_square_size() {
        let side = 4u32;
        let rgba = vec![0u8; (side * side * 4) as usize];
        assert!(Icon::from_rgba(rgba, side, side).is_ok());

        let short = vec![0u8; 8];
        assert!(
            Icon::from_rgba(short, side, side).is_err(),
            "字节数与宽高对不上时必须报错，而不是画出一张花屏图标"
        );
    }

    /// 找窗口靠标题匹配，标题写错就等于托盘全废。这里把两处「同一个字符串」
    /// 的不变式钉在编译期可查的位置：`gui::run` 里的标题必须与 `panel::PANEL_TITLE` 相同。
    #[test]
    fn panel_title_matches_the_window_title() {
        // 这个常量在 gui::run 里传给 eframe::run_native；两边不一致托盘就找不到窗口
        assert_eq!(crate::gui::WINDOW_TITLE, "Arona 管理面板");
        #[cfg(windows)]
        assert_eq!(super::panel::PANEL_TITLE, crate::gui::WINDOW_TITLE);
    }
}
