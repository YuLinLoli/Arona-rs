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
                    // 面板已经关了（接收端断了）就收工，别留一个空转线程
                    if tx.send(super::Msg::Tray(action)).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        });
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
}
