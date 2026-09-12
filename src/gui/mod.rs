//! Arona 管理 GUI（egui/eframe 原生窗口）
//! 功能：分群功能开关、群成员黑名单、OneBot 连接配置（支持多实例）与热重载、实时日志。
//! 构建：cargo build --release（GUI 默认启用，--no-default-features 可关闭）；
//! 运行：arona-rs 默认打开本面板，arona-rs --nogui 只启动命令行(黑窗口)
use crate::admin;
use crate::config::onebot::{ConnectionType, OneBotConfig};
use crate::runtime::config as runtime_config;
use crate::runtime::paths;
use eframe::egui;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

/// 后台任务回传消息
enum Msg {
    Groups(Result<Vec<admin::GroupInfo>, String>),
    Members(i64, Result<Vec<admin::MemberInfo>, String>),
    Note(Result<String, String>),
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Groups,
    Connections,
    Logs,
    About,
}

/// 界面外观偏好（持久化到 arona-standalone/gui.txt）
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ThemePref {
    /// 跟随系统（首次启动的默认值）
    System,
    /// 白天模式：浅色背景 + 黑色文字
    Light,
    /// 黑夜模式：深色背景 + 白色文字
    Dark,
}

impl ThemePref {
    fn label(self) -> &'static str {
        match self {
            ThemePref::System => "跟随系统",
            ThemePref::Light => "白天模式",
            ThemePref::Dark => "黑夜模式",
        }
    }

    /// 写进 gui.txt 的值
    fn key(self) -> &'static str {
        match self {
            ThemePref::System => "system",
            ThemePref::Light => "light",
            ThemePref::Dark => "dark",
        }
    }

    fn parse(text: &str) -> ThemePref {
        match text.trim().to_ascii_lowercase().as_str() {
            "light" | "day" => ThemePref::Light,
            "dark" | "night" => ThemePref::Dark,
            _ => ThemePref::System,
        }
    }

    fn to_egui(self) -> egui::ThemePreference {
        match self {
            ThemePref::System => egui::ThemePreference::System,
            ThemePref::Light => egui::ThemePreference::Light,
            ThemePref::Dark => egui::ThemePreference::Dark,
        }
    }
}

/// 外观偏好的存放位置（arona-standalone/gui.txt，只有一行）
fn theme_file() -> std::path::PathBuf {
    paths::prepare_standalone_root().join("gui.txt")
}

fn load_theme() -> ThemePref {
    std::fs::read_to_string(theme_file())
        .map(|text| ThemePref::parse(&text))
        .unwrap_or(ThemePref::System)
}

fn save_theme(pref: ThemePref) {
    let _ = std::fs::write(theme_file(), format!("{}\n", pref.key()));
}

struct Toast {
    text: String,
    error: bool,
    at: f64,
}

/// 提示文案：配置写入成功时返回给用户看的文字
trait NoteText {
    fn note_text(self) -> String;
}

impl NoteText for String {
    fn note_text(self) -> String {
        self
    }
}

impl NoteText for () {
    fn note_text(self) -> String {
        "已保存并热生效".to_string()
    }
}

/// 管理面板状态
struct AronaGui {
    rt: tokio::runtime::Handle,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    tab: Tab,
    // 外观（黑夜/白天）
    theme: ThemePref,
    theme_applied: Option<ThemePref>,
    started: bool,
    toast: Option<Toast>,
    // 群管理
    groups: Vec<admin::GroupInfo>,
    groups_error: Option<String>,
    loading_groups: bool,
    group_filter: String,
    selected: Option<i64>,
    // 群成员
    members: Vec<admin::MemberInfo>,
    members_for: Option<i64>,
    members_error: Option<String>,
    loading_members: bool,
    member_filter: String,
    only_blacklisted: bool,
    // OneBot 连接
    onebot: Option<OneBotConfig>,
    onebot_error: Option<String>,
    new_type: ConnectionType,
    expanded: Option<String>,
    // 实时日志
    log_lines: Vec<crate::runtime::log::LiveLine>,
    log_version: u64,
    log_auto_scroll: bool,
    log_filter: String,
    log_errors_only: bool,
    log_max: usize,
}

impl AronaGui {
    fn new(rt: tokio::runtime::Handle, tx: Sender<Msg>, rx: Receiver<Msg>) -> AronaGui {
        AronaGui {
            rt,
            tx,
            rx,
            tab: Tab::Groups,
            theme: load_theme(),
            theme_applied: None,
            started: false,
            toast: None,
            groups: Vec::new(),
            groups_error: None,
            loading_groups: false,
            group_filter: String::new(),
            selected: None,
            members: Vec::new(),
            members_for: None,
            members_error: None,
            loading_members: false,
            member_filter: String::new(),
            only_blacklisted: false,
            onebot: None,
            onebot_error: None,
            new_type: ConnectionType::WebSocket,
            expanded: None,
            log_lines: Vec::new(),
            log_version: 0,
            log_auto_scroll: true,
            log_filter: String::new(),
            log_errors_only: false,
            log_max: 500,
        }
    }

    fn spawn<F>(&self, ctx: &egui::Context, future: F)
    where
        F: std::future::Future<Output = Msg> + Send + 'static,
    {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let message = future.await;
            let _ = tx.send(message);
            ctx.request_repaint();
        });
    }

    fn note<T: NoteText>(&mut self, result: Result<T, String>) {
        let (text, error) = match result {
            Ok(value) => (value.note_text(), false),
            Err(err) => (err, true),
        };
        self.toast = Some(Toast {
            text,
            error,
            at: now_secs(),
        });
    }

    fn fetch_groups(&mut self, ctx: &egui::Context) {
        self.loading_groups = true;
        self.groups_error = None;
        self.spawn(ctx, async { Msg::Groups(admin::group_list().await) });
    }

    fn fetch_members(&mut self, group_id: i64, ctx: &egui::Context) {
        self.loading_members = true;
        self.members_error = None;
        self.members_for = Some(group_id);
        self.members.clear();
        self.spawn(ctx, async move {
            Msg::Members(group_id, admin::group_members(group_id).await)
        });
    }

    fn load_onebot(&mut self) {
        match admin::load_onebot_config() {
            Ok(config) => {
                self.onebot = Some(config);
                self.onebot_error = None;
            }
            Err(err) => self.onebot_error = Some(err),
        }
    }

    fn save_onebot(&mut self, ctx: &egui::Context) {
        let Some(config) = self.onebot.clone() else {
            return;
        };
        self.spawn(ctx, async move { Msg::Note(admin::save_and_reload(&config)) });
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Msg::Groups(result) => {
                    self.loading_groups = false;
                    match result {
                        Ok(groups) => {
                            self.groups = groups;
                            self.groups_error = None;
                            if let Some(selected) = self.selected {
                                if !self.groups.iter().any(|info| info.group_id == selected) {
                                    self.selected = None;
                                }
                            }
                        }
                        Err(err) => self.groups_error = Some(err),
                    }
                }
                Msg::Members(group_id, result) => {
                    self.loading_members = false;
                    match result {
                        Ok(members) => {
                            self.members = members;
                            self.members_for = Some(group_id);
                            self.members_error = None;
                        }
                        Err(err) => self.members_error = Some(err),
                    }
                }
                Msg::Note(result) => {
                    self.note(result);
                    if self.tab == Tab::Connections {
                        // 热重载后连接状态变化，无需重载配置
                    }
                }
            }
        }
        if let Some(toast) = &self.toast {
            if now_secs() - toast.at > 10.0 {
                self.toast = None;
            }
        }
        // 需要重绘以刷新提示超时
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }

    // ==================== 顶部 ====================

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Arona 管理面板");
            ui.separator();
            ui.selectable_value(&mut self.tab, Tab::Groups, "群管理");
            ui.selectable_value(&mut self.tab, Tab::Connections, "OneBot 连接");
            ui.selectable_value(&mut self.tab, Tab::Logs, "实时日志");
            ui.selectable_value(&mut self.tab, Tab::About, "关于");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(admin::status_text());
                ui.separator();
                let mut theme = self.theme;
                egui::ComboBox::from_id_salt("theme")
                    .selected_text(theme.label())
                    .width(104.0)
                    .show_ui(ui, |ui| {
                        for option in [ThemePref::System, ThemePref::Light, ThemePref::Dark] {
                            ui.selectable_value(&mut theme, option, option.label());
                        }
                    });
                if theme != self.theme {
                    self.theme = theme;
                    save_theme(theme);
                }
                ui.label("外观：");
            });
        });
    }

    // ==================== 群列表 ====================

    fn group_list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if ui.button("刷新群列表").clicked() {
                self.fetch_groups(ctx);
            }
            if ui.button("刷新成员").clicked() {
                if let Some(group_id) = self.selected {
                    self.fetch_members(group_id, ctx);
                }
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.group_filter)
                .hint_text("搜索群名或群号")
                .desired_width(f32::INFINITY),
        );
        if self.loading_groups {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在获取群列表...");
            });
        }
        if let Some(err) = self.groups_error.clone() {
            ui.colored_label(error_color(ui.visuals().dark_mode), err);
        }
        ui.separator();
        let filter = self.group_filter.trim().to_lowercase();
        let groups = self.groups.clone();
        let mut clicked: Option<i64> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if groups.is_empty() {
                    ui.weak("暂无群（连接 OneBot 后点「刷新群列表」）");
                }
                for info in &groups {
                    if !filter.is_empty()
                        && !info.name.to_lowercase().contains(&filter)
                        && !info.group_id.to_string().contains(&filter)
                    {
                        continue;
                    }
                    let mark = if info.enabled { "●" } else { "○" };
                    let tag = if info.disabled_count > 0 || info.blacklist_count > 0 {
                        format!("　关{}项/黑{}人", info.disabled_count, info.blacklist_count)
                    } else {
                        String::new()
                    };
                    let label =
                        format!("{mark} {}\n　({}) {}人{tag}", info.name, info.group_id, info.member_count);
                    if ui
                        .selectable_label(self.selected == Some(info.group_id), label)
                        .clicked()
                    {
                        clicked = Some(info.group_id);
                    }
                }
            });
        if let Some(group_id) = clicked {
            self.selected = Some(group_id);
            self.members.clear();
            self.members_for = None;
            self.fetch_members(group_id, ctx);
        }
    }

    // ==================== 群详情 ====================

    fn group_detail(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(group_id) = self.selected else {
            ui.weak("← 在左侧选择一个群，可配置功能开关与成员黑名单");
            return;
        };
        let info = self.groups.iter().find(|item| item.group_id == group_id).cloned();
        let name = info
            .as_ref()
            .map(|item| item.name.clone())
            .unwrap_or_else(|| group_id.to_string());
        ui.heading(format!("{} ({})", name, group_id));
        ui.horizontal(|ui| {
            let mut enabled = info.as_ref().map(|item| item.enabled).unwrap_or(true);
            if ui.checkbox(&mut enabled, "机器人在此群启用").changed() {
                self.note(admin::set_group_enabled(group_id, enabled));
                self.fetch_groups(ctx);
            }
            if info.as_ref().map(|item| item.all_groups).unwrap_or(false) {
                ui.weak("(arona.yml 的 groups 为空: 默认响应所有群)");
            }
            if let Some(item) = &info {
                if item.local_only {
                    ui.colored_label(warn_color(ui.visuals().dark_mode), "(不在 OneBot 群列表中)");
                }
            }
        });
        ui.separator();
        ui.strong("功能开关（取消勾选即在该群关闭）");
        let features: Vec<(String, String, String, bool)> = admin::features()
            .iter()
            .map(|feature| {
                (
                    feature.key.to_string(),
                    feature.name.to_string(),
                    feature.description.to_string(),
                    runtime_config::feature_enabled(Some(group_id), feature.key),
                )
            })
            .collect();
        egui::Grid::new("feature_grid")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                for (key, name, description, mut enabled) in features {
                    if ui.checkbox(&mut enabled, &name).changed() {
                        self.note(admin::set_group_feature(group_id, &key, enabled));
                    }
                    ui.weak(description);
                    ui.end_row();
                }
            });
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("群成员黑名单");
            if ui.button("获取群成员").clicked() {
                self.fetch_members(group_id, ctx);
            }
            ui.checkbox(&mut self.only_blacklisted, "只看黑名单");
            if self.loading_members {
                ui.spinner();
            }
            if let Some(count) = self.members_for.filter(|id| *id == group_id).map(|_| self.members.len())
            {
                ui.weak(format!("共 {count} 人"));
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.member_filter)
                .hint_text("搜索 QQ / 昵称")
                .desired_width(320.0),
        );
        if let Some(err) = self.members_error.clone() {
            ui.colored_label(error_color(ui.visuals().dark_mode), err);
        }
        ui.horizontal(|ui| {
            ui.weak(format!("配置文件: {}", admin::arona_file()));
            if ui.button("清空该群的设置").clicked() {
                self.note(admin::clear_group_setting(group_id));
                self.fetch_groups(ctx);
            }
        });
        let filter = self.member_filter.trim().to_lowercase();
        let only_blacklisted = self.only_blacklisted;
        let indices: Vec<usize> = self
            .members
            .iter()
            .enumerate()
            .filter(|(_, member)| {
                if only_blacklisted && !member.blacklisted && !member.global_blacklisted {
                    return false;
                }
                if filter.is_empty() {
                    return true;
                }
                member.user_id.to_string().contains(&filter)
                    || member.nickname.to_lowercase().contains(&filter)
                    || member.card.to_lowercase().contains(&filter)
            })
            .map(|(index, _)| index)
            .collect();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("member_grid")
                    .num_columns(5)
                    .striped(true)
                    .spacing([14.0, 4.0])
                    .show(ui, |ui| {
                        ui.strong("QQ");
                        ui.strong("昵称 / 群名片");
                        ui.strong("角色");
                        ui.strong("本群拉黑");
                        ui.strong("全局拉黑");
                        ui.end_row();
                        for index in indices {
                            let (user_id, nickname, card, role, level, mut group_black, mut global_black, is_manager) = {
                                let member = &self.members[index];
                                (
                                    member.user_id,
                                    member.nickname.clone(),
                                    member.card.clone(),
                                    member.role.clone(),
                                    member.level.clone(),
                                    member.blacklisted,
                                    member.global_blacklisted,
                                    member.is_manager,
                                )
                            };
                            ui.label(user_id.to_string());
                            let display = if card.is_empty() {
                                nickname
                            } else {
                                format!("{nickname}（{card}）")
                            };
                            ui.label(display);
                            let role_text = format!(
                                "{}{}{}",
                                admin::role_name(&role),
                                if level.is_empty() {
                                    String::new()
                                } else {
                                    format!(" Lv{level}")
                                },
                                if is_manager { " / 机器人管理员" } else { "" }
                            );
                            ui.label(role_text);
                            if ui.checkbox(&mut group_black, "").changed() {
                                let result = admin::set_group_blacklist(group_id, user_id, group_black);
                                self.note(result);
                                self.members[index].blacklisted = group_black;
                            }
                            if ui.checkbox(&mut global_black, "").changed() {
                                let result = admin::set_global_blacklist(user_id, global_black);
                                self.note(result);
                                for member in self.members.iter_mut() {
                                    if member.user_id == user_id {
                                        member.global_blacklisted = global_black;
                                    }
                                }
                            }
                            ui.end_row();
                        }
                    });
            });
    }

    // ==================== OneBot 连接 ====================

    fn connections(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if ui.button("读取 onebot.yml").clicked() {
                self.load_onebot();
            }
            if ui.button("保存并热重载").clicked() {
                self.save_onebot(ctx);
            }
            ui.weak("保存后立即重启全部 WebSocket / HTTP 连接（热更新，无需重启程序）");
        });
        if let Some(err) = self.onebot_error.clone() {
            ui.colored_label(error_color(ui.visuals().dark_mode), err);
        }
        let Some(config) = self.onebot.clone() else {
            ui.weak("尚未读取到配置，点「读取 onebot.yml」。");
            return;
        };
        ui.separator();
        ui.strong("连接列表（同一类型可添加多个）");
        let list = admin::connection_list(&config);
        let mut toggle: Option<(String, bool)> = None;
        let mut remove: Option<String> = None;
        let mut focus: Option<String> = None;
        egui::Grid::new("connection_grid")
            .num_columns(6)
            .striped(true)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                ui.strong("键名");
                ui.strong("类型");
                ui.strong("地址");
                ui.strong("启用");
                ui.strong("编辑");
                ui.strong("删除");
                ui.end_row();
                for info in &list {
                    ui.label(&info.key);
                    ui.label(format!("{} ({})", info.type_name, info.type_key));
                    ui.label(&info.address);
                    let mut enable = info.enable;
                    if ui.checkbox(&mut enable, "").changed() {
                        toggle = Some((info.key.clone(), enable));
                    }
                    if ui.button("编辑").clicked() {
                        focus = Some(info.key.clone());
                    }
                    if ui.button("删除").clicked() {
                        remove = Some(info.key.clone());
                    }
                    ui.end_row();
                }
            });
        if let Some((key, enable)) = toggle {
            if let Some(config) = self.onebot.as_mut() {
                if let Some(conn) = config.connections.get_mut(&key) {
                    conn.enable = enable;
                }
            }
        }
        if let Some(key) = remove {
            if let Some(config) = self.onebot.as_mut() {
                admin::remove_connection(config, &key);
            }
            if self.expanded.as_deref() == Some(key.as_str()) {
                self.expanded = None;
            }
            self.note(Ok(format!("已删除连接 {key}，点「保存并热重载」生效")));
        }
        if let Some(key) = focus {
            self.expanded = Some(key);
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("添加连接:");
            egui::ComboBox::from_id_salt("new_connection_type")
                .selected_text(self.new_type.display_name())
                .show_ui(ui, |ui| {
                    for conn_type in ConnectionType::all() {
                        ui.selectable_value(&mut self.new_type, conn_type, conn_type.display_name());
                    }
                });
            if ui.button("添加").clicked() {
                if let Some(config) = self.onebot.as_mut() {
                    let key = admin::add_connection(config, self.new_type);
                    self.expanded = Some(key.clone());
                    self.note(Ok(format!("已添加连接 {key}，点「保存并热重载」生效")));
                }
            }
        });
        ui.separator();
        // 图片发送方式存在 onebot.yml（与连接配置同一个文件），勾选后立即写盘并热生效
        ui.strong("发送设置（勾选立即生效并写入 onebot.yml，无需「保存并热重载」）");
        // 显示值取当前读到的 onebot.yml（与页面其它控件一致，点「读取 onebot.yml」后同步刷新）
        let mut direct_file = config.send_image_as_file;
        if ui
            .checkbox(
                &mut direct_file,
                "本地图片以 file:// 路径直传（大图发送更快，原图上传不压缩）",
            )
            .changed()
        {
            // 同步到内存副本，否则之后点「保存并热重载」会用旧值把这次改动覆盖回去
            if let Some(config) = self.onebot.as_mut() {
                config.send_image_as_file = direct_file;
            }
            self.note(admin::set_send_image_as_file(direct_file));
        }
        ui.weak("仅当 OneBot 实现(如 NapCat)与机器人同机部署、能访问相同磁盘时开启；跨机器/容器部署开启会导致图片发不出去");
        ui.separator();
        ui.strong("编辑连接");
        let keys: Vec<String> = config.connections.keys().cloned().collect();
        for key in keys {
            let header = format!("{key} · {}", type_name(&config, &key));
            let is_open = self.expanded.as_deref() == Some(key.as_str());
            egui::CollapsingHeader::new(header)
                .default_open(false)
                .open(if is_open { Some(true) } else { None })
                .show(ui, |ui| {
                    if let Some(config) = self.onebot.as_mut() {
                        if let Some(conn) = config.connections.get_mut(&key) {
                            egui::Grid::new(format!("conn_edit_{key}"))
                                .num_columns(2)
                                .spacing([12.0, 6.0])
                                .show(ui, |ui| {
                                    ui.label("连接类型");
                                    let mut conn_type = conn
                                        .resolve_type(&key)
                                        .unwrap_or(ConnectionType::WebSocket);
                                    egui::ComboBox::from_id_salt(format!("conn_type_{key}"))
                                        .selected_text(conn_type.display_name())
                                        .show_ui(ui, |ui| {
                                            for candidate in ConnectionType::all() {
                                                ui.selectable_value(
                                                    &mut conn_type,
                                                    candidate,
                                                    candidate.display_name(),
                                                );
                                            }
                                        });
                                    conn.connection_type = conn_type.key().to_string();
                                    ui.end_row();

                                    ui.label("启用");
                                    ui.checkbox(&mut conn.enable, "");
                                    ui.end_row();

                                    ui.label("host（反向监听地址）");
                                    ui.text_edit_singleline(&mut conn.host);
                                    ui.end_row();

                                    ui.label("port（反向监听端口）");
                                    int_edit(
                                        ui,
                                        egui::Id::new(("conn_port", key.as_str())),
                                        &mut conn.port,
                                        "1-65535",
                                        140.0,
                                        i64::from(u16::MAX),
                                    );
                                    ui.end_row();

                                    ui.label("url（正向连接地址）");
                                    ui.text_edit_singleline(&mut conn.url);
                                    ui.end_row();

                                    ui.label("path（反向路径）");
                                    ui.text_edit_singleline(&mut conn.path);
                                    ui.end_row();

                                    ui.label("token");
                                    ui.text_edit_singleline(&mut conn.token);
                                    ui.end_row();

                                    ui.label("心跳间隔(ms)");
                                    int_edit(
                                        ui,
                                        egui::Id::new(("conn_heartbeat", key.as_str())),
                                        &mut conn.heartbeat_interval,
                                        "例如 30000",
                                        140.0,
                                        i64::MAX,
                                    );
                                    ui.end_row();

                                    ui.label("重连间隔(ms)");
                                    int_edit(
                                        ui,
                                        egui::Id::new(("conn_reconnect", key.as_str())),
                                        &mut conn.reconnect_interval,
                                        "例如 5000",
                                        140.0,
                                        i64::MAX,
                                    );
                                    ui.end_row();
                                });
                        }
                    }
                });
        }
        ui.separator();
        ui.strong("机器人信息");
        if let Some(config) = self.onebot.as_mut() {
            egui::Grid::new("bot_info")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("机器人 QQ (self_id)");
                    int_edit(
                        ui,
                        egui::Id::new("bot_self_id"),
                        &mut config.self_id,
                        "纯数字，例如 123456789",
                        220.0,
                        i64::MAX,
                    );
                    ui.end_row();
                    ui.label("昵称");
                    ui.text_edit_singleline(&mut config.nickname);
                    ui.end_row();
                });
        }
        ui.weak(format!("配置文件: {}", paths::onebot_file().display()));
    }
}

fn type_name(config: &OneBotConfig, key: &str) -> String {
    config
        .connections
        .get(key)
        .and_then(|conn| conn.resolve_type(key).ok())
        .map(|conn_type| conn_type.display_name().to_string())
        .unwrap_or_else(|| "未知类型".to_string())
}

/// 数值输入框支持的整数类型（端口 u16、心跳/重连毫秒 u64、QQ 号 i64）
trait IntField: Copy {
    fn to_i64(self) -> i64;
    fn from_i64(value: i64) -> Self;
}

impl IntField for u16 {
    fn to_i64(self) -> i64 {
        i64::from(self)
    }
    fn from_i64(value: i64) -> Self {
        value as u16
    }
}

impl IntField for u64 {
    fn to_i64(self) -> i64 {
        self as i64
    }
    fn from_i64(value: i64) -> Self {
        value as u64
    }
}

impl IntField for i64 {
    fn to_i64(self) -> i64 {
        self
    }
    fn from_i64(value: i64) -> Self {
        value
    }
}

/// 整数输入框：只能手打数字。
///
/// 特意不用 `egui::DragValue`：鼠标扫过、按住轻轻一拖就会改数值（QQ 号这种长数字更是一拖
/// 就跳一大截），配置项被误改很难发现。这里用普通文本框，只保留数字；输入过程中还不合法
/// （空串、超长）就保持原值不动，没在编辑时则跟随配置显示（切换连接 / 重新读配置后同步）。
fn int_edit<T: IntField>(
    ui: &mut egui::Ui,
    id: egui::Id,
    value: &mut T,
    hint: &str,
    width: f32,
    max: i64,
) {
    let text_id = id.with("text");
    let mut text = ui
        .memory(|memory| memory.data.get_temp::<String>(text_id))
        .unwrap_or_else(|| value.to_i64().to_string());
    let response = ui.add(
        egui::TextEdit::singleline(&mut text)
            .id(text_id)
            .hint_text(hint)
            .desired_width(width),
    );
    if response.changed() {
        text.retain(|ch| ch.is_ascii_digit());
        if let Ok(parsed) = text.parse::<i64>() {
            *value = T::from_i64(parsed.clamp(0, max));
        }
    } else if !response.has_focus() {
        let current = value.to_i64().to_string();
        if text != current {
            text = current;
        }
    }
    ui.memory_mut(|memory| memory.data.insert_temp(text_id, text));
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

impl eframe::App for AronaGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 外观：只在偏好变化时下发，避免每帧改 options 触发额外重绘
        if self.theme_applied != Some(self.theme) {
            self.theme_applied = Some(self.theme);
            ctx.set_theme(self.theme.to_egui());
        }
        self.drain(ctx);
        if self.onebot.is_none() {
            self.load_onebot();
        }
        egui::TopBottomPanel::top("header").show(ctx, |ui| self.header(ui));
        // 底栏必须排在 CentralPanel 之前：egui 的中央面板会吃掉剩余空间，
        // 之后再补底栏就会盖在内容上面（实时日志的最后几行会被底栏切掉）
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(toast) = &self.toast {
                    let dark = ui.visuals().dark_mode;
                    let color = if toast.error {
                        error_color(dark)
                    } else {
                        ok_color(dark)
                    };
                    ui.colored_label(color, &toast.text);
                } else {
                    ui.weak("提示：修改功能开关/黑名单立即写入 arona.yml 并热生效；连接改动需点「保存并热重载」");
                }
            });
        });
        match self.tab {
            Tab::Groups => {
                egui::SidePanel::left("group_panel")
                    .resizable(true)
                    .default_width(300.0)
                    .width_range(220.0..=460.0)
                    .show(ctx, |ui| self.group_list(ui, ctx));
                egui::CentralPanel::default().show(ctx, |ui| self.group_detail(ui, ctx));
            }
            Tab::Connections => {
                egui::CentralPanel::default().show(ctx, |ui| self.connections(ui, ctx));
            }
            Tab::Logs => {
                egui::CentralPanel::default().show(ctx, |ui| self.logs(ui, ctx));
            }
            Tab::About => {
                egui::CentralPanel::default().show(ctx, |ui| self.about(ui));
            }
        }
        if !self.started {
            self.started = true;
            self.fetch_groups(ctx);
        }
    }
}

impl AronaGui {
    // ==================== 实时日志 ====================

    fn logs(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) {
        // 只在日志缓冲有新内容时重新取快照，避免每帧克隆
        let version = crate::runtime::log::live_version();
        if version != self.log_version {
            self.log_version = version;
            self.log_lines = crate::runtime::log::live_lines();
        }

        ui.horizontal(|ui| {
            ui.heading(format!("实时日志（{} 行）", self.log_lines.len()));
            ui.separator();
            if ui.button("刷新").clicked() {
                self.log_version = crate::runtime::log::live_version();
                self.log_lines = crate::runtime::log::live_lines();
            }
            if ui.button("清空").clicked() {
                crate::runtime::log::clear_live();
                self.log_version = crate::runtime::log::live_version();
                self.log_lines.clear();
            }
            ui.checkbox(&mut self.log_auto_scroll, "自动滚动");
            ui.checkbox(&mut self.log_errors_only, "只看告警/错误");
            egui::ComboBox::from_id_salt("log_max")
                .selected_text(format!("最近 {} 行", self.log_max))
                .show_ui(ui, |ui| {
                    for n in [200usize, 500, 1000, 3000] {
                        ui.selectable_value(&mut self.log_max, n, format!("最近 {n} 行"));
                    }
                });
            if ui.button("打开日志目录").clicked() {
                open_logs_dir();
            }
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.log_filter)
                .hint_text("过滤关键字（匹配行内容，不区分大小写）")
                .desired_width(f32::INFINITY),
        );
        ui.separator();

        let filter = self.log_filter.trim().to_lowercase();
        let errors_only = self.log_errors_only;
        let auto_scroll = self.log_auto_scroll;
        let dark = ui.visuals().dark_mode;
        let start = self.log_lines.len().saturating_sub(self.log_max);
        let raw_empty = self.log_lines[start..].is_empty();
        let lines: Vec<&crate::runtime::log::LiveLine> = self.log_lines[start..]
            .iter()
            .filter(|line| !errors_only || is_alert_line(line))
            .filter(|line| filter.is_empty() || line.text.to_lowercase().contains(&filter))
            .collect();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(auto_scroll)
            .show(ui, |ui| draw_log_lines(ui, &lines, dark, raw_empty));
    }
}

/// 画日志列表。
///
/// 每行合成一个 `LayoutJob`（`[时间]` 用弱色 + 正文按染色规则着色），折行交给 egui：
/// 续行与首行左对齐，时间戳永远和正文待在同一行。
/// 之前用 `horizontal_wrapped` 时，超长行（QQ 图片 URL）会被整条挤到下一行、
/// 续行直接顶到面板最左边，看起来就像日志左侧被切掉；再叠上贴边的滚动条与
/// 面板底边，就成了「左侧与下侧缺失」。
fn draw_log_lines(
    ui: &mut egui::Ui,
    lines: &[&crate::runtime::log::LiveLine],
    dark: bool,
    raw_empty: bool,
) {
    ui.spacing_mut().item_spacing.y = 2.0;
    // 左右留白：文字紧贴面板边缘 / 滚动条会被裁掉
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            if lines.is_empty() {
                ui.weak(if raw_empty {
                    "暂无日志（机器人启动后这里会实时刷新）"
                } else {
                    "没有匹配的日志行"
                });
                return;
            }
            let body = ui
                .style()
                .text_styles
                .get(&egui::TextStyle::Body)
                .cloned()
                .unwrap_or_else(|| egui::FontId::proportional(14.0));
            let time_font = body.clone();
            let text_font = egui::FontId::monospace(body.size);
            let time_color = ui.visuals().weak_text_color();
            let max_width = ui.available_width().max(48.0);
            for line in lines {
                let mut job = egui::text::LayoutJob::default();
                if is_ascii_art_line(&line.text) {
                    // 启动横幅等等宽艺术字：禁止折行，否则列会被拆散
                    job.wrap.max_width = f32::INFINITY;
                } else {
                    job.wrap.max_width = max_width;
                    // 长 URL 里没有空格，必须允许任意位置断行，否则整行会溢出到面板外被裁掉
                    job.wrap.break_anywhere = true;
                }
                job.append(
                    &format!("[{}] ", line.time),
                    0.0,
                    egui::TextFormat {
                        font_id: time_font.clone(),
                        color: time_color,
                        ..Default::default()
                    },
                );
                job.append(
                    &line.text,
                    0.0,
                    egui::TextFormat {
                        font_id: text_font.clone(),
                        color: log_line_color(&line.text, line.stderr, dark),
                        ..Default::default()
                    },
                );
                ui.add(egui::Label::new(job));
            }
            // 底部留一点空隙：滚到底时最后一行不会贴着面板底边被切掉一半
            ui.add_space(6.0);
        });
}

impl AronaGui {
    // ==================== 关于 ====================

    fn about(&self, ui: &mut egui::Ui) {
        ui.heading("关于 Arona-rs");
        ui.add_space(4.0);
        ui.label(format!("版本 v{}", env!("CARGO_PKG_VERSION")));
        ui.label("碧蓝档案 QQ 机器人 arona 的 Rust 移植版：OneBot 独立模式，完全剥离 Mirai / JVM。");
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(10.0);

        ui.strong("项目地址");
        ui.hyperlink_to(
            "https://github.com/YuLinLoli/Arona-rs",
            "https://github.com/YuLinLoli/Arona-rs",
        );
        ui.add_space(10.0);

        ui.strong("鸣谢");
        ui.label("本项目的玩法、数据与美术素材来自原版 arona 项目，感谢原作者的付出：");
        ui.hyperlink_to("原版 arona（diyigemt）", "https://github.com/diyigemt");
        ui.add_space(10.0);

        ui.separator();
        ui.add_space(10.0);
        ui.strong("开源协议");
        ui.label("GNU Affero General Public License v3.0（AGPL-3.0-only）。");
        ui.label("本程序按「现状」提供，不附带任何担保；移植部分版权归 Arona-rs 作者所有。");
        ui.add_space(10.0);

        ui.separator();
        ui.add_space(10.0);
        ui.strong("数据目录");
        ui.label(paths::standalone_root().display().to_string());
        ui.horizontal(|ui| {
            if ui.button("打开数据目录").clicked() {
                open_data_dir();
            }
            if ui.button("打开日志目录").clicked() {
                open_logs_dir();
            }
        });
        ui.add_space(10.0);
        ui.weak("提示：右上角「外观」可切换白天 / 黑夜模式，选择会保存到数据目录的 gui.txt。");
    }
}

/// 加载窗口（标题栏左上角/任务栏）图标：与 exe 图标同源，来自 assets/arona.png
fn load_window_icon() -> Option<egui::IconData> {
    const PNG: &[u8] = include_bytes!("../../assets/arona.png");
    let rgba = image::load_from_memory(PNG).ok()?.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

/// 是否为等宽艺术字行（启动横幅用的方框绘制/方块字符）：这类行在 GUI 里不能折行
fn is_ascii_art_line(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{2500}'..='\u{259F}'))
}

/// 常规文本色：黑夜模式纯白、白天模式纯黑（面板里没被特殊染色的文字都用它）
fn default_text_color(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::WHITE
    } else {
        egui::Color32::BLACK
    }
}

/// 错误/告警提示色：黑夜亮色系，白天换成深色系（浅色背景上亮色几乎看不清）
fn error_color(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::LIGHT_RED
    } else {
        egui::Color32::from_rgb(170, 0, 0)
    }
}

fn ok_color(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::LIGHT_GREEN
    } else {
        egui::Color32::from_rgb(0, 120, 0)
    }
}

fn warn_color(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::YELLOW
    } else {
        egui::Color32::from_rgb(150, 100, 0)
    }
}

/// 日志行颜色：普通行跟随外观（黑夜白色 / 白天黑色），
/// [Arona]/[OneBot] 仍是绿色、WARNING/ERROR 仍是黄/红——这些特殊色只是按背景深浅调暗，
/// 保证在白色背景上同样看得清。
fn log_line_color(text: &str, stderr: bool, dark: bool) -> egui::Color32 {
    use crate::runtime::console::Color;
    match crate::runtime::console::color_for_line(text) {
        Some(Color::BrightGreen) => {
            if dark {
                egui::Color32::from_rgb(110, 220, 130)
            } else {
                egui::Color32::from_rgb(0, 120, 0)
            }
        }
        Some(Color::BrightYellow) | Some(Color::Yellow) => warn_color(dark),
        None if stderr => error_color(dark),
        None => default_text_color(dark),
    }
}

/// 是否属于告警/错误（「只看告警/错误」过滤用）
fn is_alert_line(line: &crate::runtime::log::LiveLine) -> bool {
    use crate::runtime::console::Color;
    if line.stderr || line.text.contains("ERROR") || line.text.contains("WARNING") {
        return true;
    }
    matches!(
        crate::runtime::console::color_for_line(&line.text),
        Some(Color::BrightYellow) | Some(Color::Yellow)
    )
}

/// 用系统文件管理器打开目录
fn open_dir(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

/// 用系统文件管理器打开日志目录
fn open_logs_dir() {
    open_dir(&paths::logs_dir());
}

/// 用系统文件管理器打开数据目录（arona-standalone）
fn open_data_dir() {
    open_dir(&paths::standalone_root());
}

/// 安装中文字体（优先系统黑体/雅黑）
fn install_fonts(ctx: &egui::Context) {
    let Some(bytes) = load_cjk_font() else {
        crate::runtime::console::eprint_safe("[Arona] 未找到中文字体，GUI 中文可能显示为方块");
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "cjk".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("cjk".to_owned());
    ctx.set_fonts(fonts);
}

fn load_cjk_font() -> Option<Vec<u8>> {
    const WINDOWS: [&str; 8] = [
        "simhei.ttf",
        "msyh.ttc",
        "msyhbd.ttc",
        "msyhl.ttc",
        "simsun.ttc",
        "Deng.ttf",
        "Dengb.ttf",
        "simsunb.ttf",
    ];
    for name in WINDOWS {
        let path = std::path::PathBuf::from("C:/Windows/Fonts").join(name);
        if path.is_file() {
            if let Ok(bytes) = std::fs::read(&path) {
                return Some(bytes);
            }
        }
    }
    const OTHERS: [&str; 5] = [
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
    ];
    for path in OTHERS {
        let path = std::path::PathBuf::from(path);
        if path.is_file() {
            if let Ok(bytes) = std::fs::read(&path) {
                return Some(bytes);
            }
        }
    }
    None
}

/// wgpu 尝试变体：正常情况下第一个就够，后面几个是给环境刁钻的机器兜底的
#[derive(Clone, Copy, PartialEq, Eq)]
enum WgpuFlavor {
    /// DX12/Vulkan + 静态 DXC（默认，最优；着色器编译器直接编在 exe 里）
    Primary,
    /// 所有后端（含 GL）+ 静态 DXC
    AllBackends,
    /// DX12 + 系统自带的 FXC（万一静态 DXC 容器创建失败时还能救一下）
    Fxc,
}

/// 一次渲染后端的尝试
struct RendererAttempt {
    name: &'static str,
    renderer: eframe::Renderer,
    flavor: WgpuFlavor,
    /// 是否要先启用随程序附带的软件 OpenGL(Mesa llvmpipe)，再交给 glow 渲染
    softgl: bool,
}

impl RendererAttempt {
    /// 硬件 OpenGL（显卡驱动自带的 ICD）
    fn glow() -> RendererAttempt {
        RendererAttempt {
            name: "glow(OpenGL)",
            renderer: eframe::Renderer::Glow,
            flavor: WgpuFlavor::Primary,
            softgl: false,
        }
    }

    /// 软件 OpenGL：Mesa llvmpipe 用 CPU 把界面画出来（服务器/无显卡机器的保命方案）
    fn softgl() -> RendererAttempt {
        RendererAttempt {
            name: "glow(软件 OpenGL llvmpipe)",
            renderer: eframe::Renderer::Glow,
            flavor: WgpuFlavor::Primary,
            softgl: true,
        }
    }

    fn wgpu(name: &'static str, flavor: WgpuFlavor) -> RendererAttempt {
        RendererAttempt {
            name,
            renderer: eframe::Renderer::Wgpu,
            flavor,
            softgl: false,
        }
    }
}

/// 系统是否装了真实的 OpenGL 驱动（ICD）。
///
/// Windows 的 OpenGL 分两层：`opengl32.dll` 只是转发层，真正的实现由注册表
/// `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\OpenGLDrivers` 下登记的 ICD 提供。
/// 该键下没有子键时，`wgl` 只能拿到 GDI 自带的 OpenGL 1.1，egui 必然起不来；
/// 更麻烦的是实测某些环境下 eframe/glutin 不是返回错误，而是**直接把进程干掉**，
/// 那样整个渲染兜底链就断了。所以这里先探测一下，能跳过 glow 就跳过。
fn has_hardware_opengl() -> bool {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;

        const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as isize;
        const KEY_READ: u32 = 0x2_0019;
        const ERROR_SUCCESS: i32 = 0;

        #[link(name = "advapi32")]
        unsafe extern "system" {
            fn RegOpenKeyExW(
                hkey: isize,
                sub_key: *const u16,
                options: u32,
                sam: u32,
                result: *mut isize,
            ) -> i32;
            fn RegEnumKeyExW(
                hkey: isize,
                index: u32,
                name: *mut u16,
                name_len: *mut u32,
                reserved: *mut u32,
                class: *mut u16,
                class_len: *mut u32,
                last_write: *mut u64,
            ) -> i32;
            fn RegCloseKey(hkey: isize) -> i32;
        }

        let sub_key: Vec<u16> = OsStr::new(
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\OpenGLDrivers",
        )
        .encode_wide()
        .chain(Some(0))
        .collect();

        let mut key: isize = 0;
        let status = unsafe {
            RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub_key.as_ptr(), 0, KEY_READ, &mut key)
        };
        if status != ERROR_SUCCESS {
            // 读不到注册表就别拦着，交给正常的尝试流程去判断
            return true;
        }
        let mut name = [0u16; 512];
        let mut name_len = name.len() as u32;
        let first = unsafe {
            RegEnumKeyExW(
                key,
                0,
                name.as_mut_ptr(),
                &mut name_len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        unsafe { RegCloseKey(key) };
        first == ERROR_SUCCESS
    }
    #[cfg(not(windows))]
    {
        true
    }
}
/// 渲染后端候选顺序：有硬件 OpenGL 就用硬件 `glow`，否则先试 `wgpu`（DX12/WARP），
/// 最后才是随程序附带的软件 OpenGL(llvmpipe)。
///
/// 为什么不把软渲染排到 wgpu 前面：没有 OpenGL ICD 的机器未必没有显卡（实测装了
/// RX 6700 XT 的机器注册表里同样查不到 ICD 子键，但 wgpu 用 DX12 一秒就能起来），
/// 一上来就走 CPU 软渲染等于白白丢掉显卡。
/// 而 wgpu 在部分 Windows Server 上**会静默卡死**（一个适配器都枚举不到、不报错、
/// 也没有日志，用户看到的就是「双击没反应」），所以每次尝试都挂看门狗，卡死就换后端，
/// 见 [`spawn_attempt_watchdog`] / [`failover_for`]。
///
/// 可用 `--renderer=glow|wgpu|softgl`（或环境变量 `ARONA_RENDERER`）只试其中一族。
fn renderer_candidates(args: &[String]) -> Vec<RendererAttempt> {
    let requested = crate::find_arg(args, "--renderer=")
        .or_else(|| std::env::var("ARONA_RENDERER").ok());
    candidates_for(
        requested.as_deref(),
        has_hardware_opengl(),
        crate::runtime::softgl::dir().is_some(),
        crate::runtime::softgl::requested(args),
    )
}

/// [`renderer_candidates`] 的纯函数版本：环境探测结果由参数传入，方便单测覆盖各种机器
fn candidates_for(
    requested: Option<&str>,
    has_hardware_gl: bool,
    softgl_available: bool,
    softgl_requested: bool,
) -> Vec<RendererAttempt> {
    match requested {
        Some("wgpu") => wgpu_candidates(),
        Some("glow") => vec![glow_candidate(has_hardware_gl, softgl_available, softgl_requested)],
        Some("softgl") => vec![RendererAttempt::softgl()],
        Some(other) => {
            crate::runtime::console::eprint_safe(&format!(
                "[Arona] 未知渲染后端 {other}（可用 glow / wgpu / softgl），改用自动选择"
            ));
            auto_candidates(has_hardware_gl, softgl_available, softgl_requested)
        }
        None => auto_candidates(has_hardware_gl, softgl_available, softgl_requested),
    }
}

/// wgpu 的几种配置：正常情况下第一个就够，后面两个是给环境刁钻的机器兜底的
fn wgpu_candidates() -> Vec<RendererAttempt> {
    vec![
        RendererAttempt::wgpu("wgpu(DX12/Vulkan)", WgpuFlavor::Primary),
        RendererAttempt::wgpu("wgpu(全部后端)", WgpuFlavor::AllBackends),
        RendererAttempt::wgpu("wgpu(DX12+FXC)", WgpuFlavor::Fxc),
    ]
}

/// 自动顺序：能用显卡就用显卡，之后才是 CPU 软渲染兜底。
///
/// 注意这里**不**把软件 OpenGL 排在 wgpu 前面：没有 OpenGL ICD 的机器未必没有显卡
/// （实测有 RX 6700 XT 的机器注册表里同样没有 ICD 子键，但 wgpu 用 DX12 只要 1 秒就能
/// 起来），一上来就用 CPU 软渲染等于白白丢掉显卡。
/// 真正卡死的场景交给看门狗：wgpu 超过 6 秒还没出窗口就换到软件 OpenGL（见
/// [`spawn_attempt_watchdog`]），既不影响有显卡的机器，也不会让服务器一直黑着。
fn auto_candidates(
    has_hardware_gl: bool,
    softgl_available: bool,
    softgl_requested: bool,
) -> Vec<RendererAttempt> {
    let mut attempts = Vec::new();
    if softgl_requested {
        attempts.push(RendererAttempt::softgl());
    } else if has_hardware_gl {
        attempts.push(RendererAttempt::glow());
    } else if softgl_available {
        crate::runtime::log::info(
            "系统未安装 OpenGL 驱动(ICD)，跳过 glow 硬件后端；先用 wgpu(DX12/WARP)，卡死或失败时自动转用随程序附带的软件 OpenGL(llvmpipe)",
        );
    } else {
        crate::runtime::log::warning(
            "系统未安装 OpenGL 驱动(ICD)，跳过 glow 硬件后端；无显卡驱动/无 DX12 的机器可运行 scripts/fetch-softgl.ps1 获取软件 OpenGL",
        );
    }
    attempts.extend(wgpu_candidates());
    // 软件 OpenGL 排在最后：wgpu 正常就用显卡，卡死/失败再由看门狗切到它
    if !has_hardware_gl && !softgl_requested && softgl_available {
        attempts.push(RendererAttempt::softgl());
    }
    attempts
}
/// 用户显式要求 glow 时的单条候选：没有 ICD 但带了软件 OpenGL 时同样自动改用软渲染，
/// 否则会像以前那样直接失败（还会浪费一次进程级的 GL 加载）
fn glow_candidate(
    has_hardware_gl: bool,
    softgl_available: bool,
    softgl_requested: bool,
) -> RendererAttempt {
    if softgl_requested || (!has_hardware_gl && softgl_available) {
        RendererAttempt::softgl()
    } else {
        RendererAttempt::glow()
    }
}
/// 每次尝试都要新建一份 App（AppCreator 是 FnOnce）。
///
/// eframe 只在**渲染器初始化成功、窗口已经拿在手里**之后才调用这个闭包
/// （CreationContext 里的 gl / wgpu_render_state 此时已经就绪），所以这里给看门狗
/// 发「后端起来了」的信号：闭包一直没被调用就说明这个后端卡住了。
fn app_creator(
    handle: tokio::runtime::Handle,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ready: Arc<AtomicBool>,
    attempt_name: &'static str,
) -> eframe::AppCreator<'static> {
    Box::new(move |cc| {
        crate::runtime::log::info(format!("管理面板已就绪（渲染后端: {attempt_name}）"));
        ready.store(true, Ordering::SeqCst);
        install_fonts(&cc.egui_ctx);
        Ok(Box::new(AronaGui::new(handle, tx, rx)))
    })
}

/// wgpu 后端配置：允许 DX12/Vulkan（含 WARP 软件适配器），用于无显卡驱动 / 服务器环境
fn wgpu_options(flavor: WgpuFlavor) -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::egui_wgpu::{WgpuConfiguration, WgpuSetup};

    let mut config = WgpuConfiguration::default();
    if let WgpuSetup::CreateNew(setup) = &mut config.wgpu_setup {
        setup.instance_descriptor.backends = match flavor {
            WgpuFlavor::AllBackends => eframe::wgpu::Backends::all(),
            _ => eframe::wgpu::Backends::PRIMARY,
        };
        // 着色器编译器：默认把 DXC 静态编进 exe（既不需要 dxcompiler.dll/dxil.dll，
        // 也不需要系统 d3dcompiler_47.dll）；万一静态 DXC 容器创建失败，用系统 FXC 兜底
        setup.instance_descriptor.backend_options.dx12.shader_compiler = match flavor {
            WgpuFlavor::Fxc => eframe::wgpu::Dx12Compiler::Fxc,
            _ => eframe::wgpu::Dx12Compiler::StaticDxc,
        };
        setup.native_adapter_selector = Some(Arc::new(select_adapter));
    }
    config
}

/// 适配器选择：硬件优先（独显 > 集显 > 虚拟 GPU），没有可用硬件时退到 CPU 软件适配器
/// （DX12 下的 WARP / "Microsoft Basic Render Driver"）。
///
/// 注意：这里刻意**不**把「不支持当前窗口」的适配器直接丢掉——在没有显卡的服务器上，
/// 它往往是系统里唯一能用的适配器，宁可试一把也别让面板打不开。
fn select_adapter(
    adapters: &[eframe::wgpu::Adapter],
    surface: Option<&eframe::wgpu::Surface<'_>>,
) -> Result<eframe::wgpu::Adapter, String> {
    use eframe::wgpu::DeviceType;

    fn rank(device_type: DeviceType) -> u8 {
        match device_type {
            DeviceType::DiscreteGpu => 4,
            DeviceType::IntegratedGpu => 3,
            DeviceType::VirtualGpu | DeviceType::Other => 2,
            DeviceType::Cpu => 1,
        }
    }

    // Windows 上同级别时优先 DX12：只有 DX12 才有 WARP 软件回退
    fn backend_rank(backend: eframe::wgpu::Backend) -> u8 {
        match backend {
            eframe::wgpu::Backend::Dx12 => 3,
            eframe::wgpu::Backend::Vulkan | eframe::wgpu::Backend::Metal => 2,
            _ => 1,
        }
    }

    if adapters.is_empty() {
        // 走到这里说明 DX12/Vulkan 的 wgpu 实例一个都没建起来，具体原因
        // （例如 d3d12.dll 加载失败）由 wgpu 自己的 debug 日志给出，见 runtime::log::install_logger
        return Err(
            "wgpu 没有枚举到任何适配器（DX12/Vulkan 后端创建失败，原因见上方 wgpu 日志）".to_string(),
        );
    }

    let mut best_supported: Option<(&eframe::wgpu::Adapter, u16)> = None;
    let mut best_any: Option<(&eframe::wgpu::Adapter, u16)> = None;
    for (index, adapter) in adapters.iter().enumerate() {
        let info = adapter.get_info();
        let surface_ok = match surface {
            Some(surface) => adapter.is_surface_supported(surface),
            None => true,
        };
        // 记下全部候选：服务器上可据此确认有没有 WARP（Microsoft Basic Render Driver）
        crate::runtime::log::info(format!(
            "wgpu 候选适配器[{index}]: {}（{:?}, {:?}）驱动 {} {} / 厂商 0x{:04X} 设备 0x{:04X} / 支持当前窗口: {}",
            info.name,
            info.device_type,
            info.backend,
            info.driver,
            info.driver_info,
            info.vendor,
            info.device,
            if surface_ok { "是" } else { "否" },
        ));
        let score = u16::from(rank(info.device_type)) * 10 + u16::from(backend_rank(info.backend));
        if best_any.map_or(true, |(_, best)| score > best) {
            best_any = Some((adapter, score));
        }
        if surface_ok && best_supported.map_or(true, |(_, best)| score > best) {
            best_supported = Some((adapter, score));
        }
    }

    let (adapter, fallback_warning) = match (best_supported, best_any) {
        (Some((adapter, _)), _) => (adapter, false),
        // 服务器/虚拟机常见：唯一可用的适配器报告不支持当前窗口，仍然用它试一把
        (None, Some((adapter, _))) => (adapter, true),
        (None, None) => {
            return Err("没有可用的 wgpu 适配器（已包含 WARP 软件回退）".to_string());
        }
    };
    if fallback_warning {
        crate::runtime::log::warning("所有 wgpu 适配器都报告不支持当前窗口，仍尝试使用其中最优的一个");
    }
    let info = adapter.get_info();
    // 记下实际用的适配器：服务器上能借此确认是否退到了 WARP 软件渲染
    crate::runtime::log::info(format!(
        "wgpu 适配器: {}（{:?}, {:?}）",
        info.name, info.device_type, info.backend
    ));
    Ok(adapter.clone())
}
/// 渲染后端就绪的等待上限；超过这个时间还没起来就认为该后端**卡死**了。
///
/// wgpu 的默认值更短：正常的 wgpu 初始化 1~2 秒就能完成，卡死的话等再久也没用，
/// 不如早点换到 CPU 软渲染。`ARONA_GUI_TIMEOUT=<秒>` 可统一覆盖（支持小数，便于试验），
/// 设为 0 表示关掉看门狗。
fn gui_ready_timeout(is_wgpu: bool) -> Option<std::time::Duration> {
    const GLOW_SECS: u64 = 20;
    const WGPU_SECS: u64 = 6;
    let configured = std::env::var("ARONA_GUI_TIMEOUT")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs >= 0.0);
    let secs = match configured {
        Some(value) if value <= 0.0 => return None,
        Some(value) => value,
        None => {
            if is_wgpu {
                WGPU_SECS as f64
            } else {
                GLOW_SECS as f64
            }
        }
    };
    Some(std::time::Duration::from_secs_f64(secs))
}

/// 日志里显示超时时间：整数秒不带小数点
fn format_timeout(timeout: std::time::Duration) -> String {
    if timeout.subsec_millis() == 0 {
        format!("{}", timeout.as_secs())
    } else {
        format!("{:.1}", timeout.as_secs_f64())
    }
}

/// 最多换几次后端（含软渲染）就放弃 GUI：防止两条路径互相踢皮球
const MAX_HANDOFFS: u32 = 3;

/// 一次尝试卡死之后换到哪去
enum Failover {
    /// 换到候选链里的第 N 个渲染后端（另起一个进程，跳过已经卡死的这一族）
    Attempt(usize),
    /// 用软件 OpenGL(llvmpipe) 重新启动（覆盖 `--renderer=` / `ARONA_RENDERER`）
    Softgl,
    /// 所有图形后端都不行：退回命令行模式，至少保证机器人继续跑
    NoGui,
}

/// 渲染后端族：wgpu 与 glow(含软渲染) 是两族，卡死时直接从一族跳到另一族，
/// 不用把 wgpu 的三种配置各等一遍
fn is_wgpu_renderer(renderer: eframe::Renderer) -> bool {
    matches!(renderer, eframe::Renderer::Wgpu)
}

/// 下一个「不是同一族」的候选下标
fn next_family_index(attempts: &[RendererAttempt], index: usize) -> Option<usize> {
    let current = attempts.get(index)?;
    let want_wgpu = !is_wgpu_renderer(current.renderer);
    (index + 1..attempts.len()).find(|&other| is_wgpu_renderer(attempts[other].renderer) == want_wgpu)
}

/// 第 `index` 个候选卡死之后怎么办。
///
/// `handoff` 是「已经换过几次后端」，到 [`MAX_HANDOFFS`] 就老老实实退回命令行模式：
/// 否则 `--softgl`（列表 [软渲染, wgpu x3]）与强制 `--renderer=wgpu` 的场景可能
/// 在软渲染和 wgpu 之间来回重启，永远停不下来。
fn failover_for(
    attempts: &[RendererAttempt],
    index: usize,
    softgl_available: bool,
    handoff: u32,
) -> Failover {
    if handoff >= MAX_HANDOFFS {
        return Failover::NoGui;
    }
    match next_family_index(attempts, index) {
        Some(next) => Failover::Attempt(next),
        None if softgl_available && !attempts[index].softgl => Failover::Softgl,
        None => Failover::NoGui,
    }
}

/// 单次渲染后端尝试的看门狗。
///
/// 为什么需要它：eframe/wgpu 在部分环境里**不是**返回 Err，而是卡死不返回
/// （服务器上实测 wgpu(DX12) 停在实例创建阶段：不出窗口、不报错、也没有任何 wgpu 日志），
/// 那样「失败就换下一个后端」的兜底链直接断掉，用户看到的就是「双击没反应」。
/// 这里给每次尝试加一个超时：超时仍没等到渲染器就绪，就按 `failover` 另起一个进程
/// （卡死在 wgpu 里的线程没法从进程内干掉），然后结束当前进程。
///
/// 不会和机器人抢资源：子进程继承当前令牌（不再弹 UAC），并且带 1.5 秒启动延迟，
/// 等当前进程彻底退出后再去占端口/数据库。
fn spawn_attempt_watchdog(
    args: &[String],
    attempt_name: &'static str,
    is_wgpu: bool,
    failover: Failover,
    handoff: u32,
    ready: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
) {
    let Some(timeout) = gui_ready_timeout(is_wgpu) else {
        return;
    };
    let args = args.to_vec();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if ready.load(Ordering::SeqCst) || finished.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if ready.load(Ordering::SeqCst) || finished.load(Ordering::SeqCst) {
            return;
        }
        crate::runtime::log::error(format!(
            "{attempt_name} 后端在 {} 秒内没有完成初始化，判定为卡死",
            format_timeout(timeout)
        ));
        // 别再弹一次 UAC；再等 1.5 秒，让当前进程先彻底退出（端口/数据库句柄）
        let start_delay = [("ARONA_START_DELAY_MS", "1500")];
        let progress = format!("--gui-handoff={}", handoff + 1);
        let (extra, envs, remove_envs, strip_prefixes, message) = match failover {
            Failover::Attempt(next) => (
                vec![
                    format!("--gui-attempt={next}"),
                    progress,
                    "--arona-elevated".to_string(),
                ],
                start_delay.to_vec(),
                Vec::new(),
                vec!["--gui-attempt=", "--gui-handoff="],
                "已用下一个渲染后端重新启动管理面板（当前进程退出）",
            ),
            Failover::Softgl => (
                vec![
                    "--softgl".to_string(),
                    progress,
                    "--arona-elevated".to_string(),
                ],
                vec![("ARONA_START_DELAY_MS", "1500"), ("ARONA_SOFTGL", "1")],
                vec!["ARONA_RENDERER"],
                vec!["--gui-attempt=", "--gui-handoff=", "--renderer="],
                "已改用随程序附带的软件 OpenGL(llvmpipe) 重新启动管理面板（当前进程退出）",
            ),
            Failover::NoGui => (
                vec!["--nogui".to_string(), "--arona-elevated".to_string()],
                start_delay.to_vec(),
                Vec::new(),
                vec!["--gui-attempt=", "--gui-handoff="],
                "所有图形后端都不可用，已改用命令行模式重新启动（机器人继续运行）",
            ),
        };
        match crate::runtime::relaunch::spawn_self(
            &args,
            &extra,
            &envs,
            &remove_envs,
            &[],
            &strip_prefixes,
        ) {
            Ok(()) => {
                crate::runtime::log::warning(message);
                std::process::exit(0);
            }
            Err(err) => {
                crate::runtime::crash::report(
                    "GUI 启动失败",
                    &format!("{attempt_name} 后端卡死，且无法重新启动自身: {err}"),
                );
                std::process::exit(2);
            }
        }
    });
}

/// 启动 GUI（内部会先启动机器人主流程）；返回 Err 表示所有渲染后端都没能创建窗口，
/// 由调用方决定是否回退到命令行模式（见 main.rs）。
pub fn run(args: Vec<String>) -> Result<(), String> {
    let candidates = renderer_candidates(&args);
    let total = candidates.len();
    // 看门狗换后端时会带上 --gui-attempt=N：跳过已经试过（并且卡死）的那几个
    let skip = crate::find_arg(&args, "--gui-attempt=")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0)
        .min(total);
    // 看门狗每换一次后端就 +1，防止一直在两个后端之间重启
    let handoff = crate::find_arg(&args, "--gui-handoff=")
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(0);

    // 软件 OpenGL 必须在任何 GL 调用（也必须在创建任何线程）之前把 DLL 搜索目录塞好；
    // 只要候选链里有一个要用软渲染，就在这里统一启用
    if candidates[skip..].iter().any(|attempt| attempt.softgl) {
        crate::runtime::softgl::activate();
    }

    let runtime = crate::runtime::runtime_builder()
        .build()
        .map_err(|err| format!("创建 tokio 运行时失败: {err}"))?;
    let handle = runtime.handle().clone();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let bot_args = args.clone();
    handle.spawn(async move {
        if let Err(err) = crate::run_bot(bot_args, Some(shutdown_rx)).await {
            crate::runtime::crash::report("机器人运行失败", &err);
        }
    });

    // 不等机器人就绪：先让窗口显示出来，连接状态由面板自行轮询/刷新
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1200.0, 780.0])
        .with_min_inner_size([960.0, 620.0])
        .with_title("Arona 管理面板");
    match load_window_icon() {
        Some(icon) => viewport = viewport.with_icon(icon),
        None => crate::runtime::console::eprint_safe("[Arona] 未能加载窗口图标 assets/arona.png"),
    }

    // 依次尝试各个渲染后端：能用显卡就用显卡，之后才是随程序附带的软件 OpenGL；
    // 每次尝试都挂看门狗，卡死（wgpu 在部分服务器上会静默卡住）时自动换一个
    let softgl_available = crate::runtime::softgl::dir().is_some();
    let failovers: Vec<Failover> = (0..total)
        .map(|index| failover_for(&candidates, index, softgl_available, handoff))
        .collect();
    let mut errors: Vec<String> = Vec::new();
    let mut outcome: Result<(), String> = Err("没有可用的渲染后端".to_string());
    for ((index, attempt), failover) in candidates
        .into_iter()
        .enumerate()
        .zip(failovers)
        .skip(skip)
    {
        let (tx, rx) = channel();
        let ready = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        spawn_attempt_watchdog(
            &args,
            attempt.name,
            is_wgpu_renderer(attempt.renderer),
            failover,
            handoff,
            ready.clone(),
            finished.clone(),
        );
        let creator = app_creator(handle.clone(), tx, rx, ready, attempt.name);
        let options = eframe::NativeOptions {
            viewport: viewport.clone(),
            renderer: attempt.renderer,
            wgpu_options: wgpu_options(attempt.flavor),
            ..Default::default()
        };
        crate::runtime::log::info(format!(
            "管理面板启动中（渲染后端: {}）{}",
            attempt.name,
            if index == skip { "" } else { " [备用方案]" }
        ));
        let result = eframe::run_native("Arona 管理面板", options, creator);
        finished.store(true, Ordering::SeqCst);
        match result {
            Ok(()) => {
                outcome = Ok(());
                break;
            }
            Err(err) => {
                let message = format!("{} 后端启动失败: {err}", attempt.name);
                crate::runtime::log::warning(&message);
                errors.push(message);
                // main.rs 靠 "后端启动失败" 判断是不是渲染环境的问题，从而决定要不要用
                // 软件 OpenGL(llvmpipe) 重启自己再试一次
                outcome = Err(format!("渲染后端启动失败: {}", errors.join("；")));
            }
        }
    }
    // 关闭窗口 = 关闭机器人
    let _ = shutdown_tx.send(());
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    outcome
}
#[cfg(test)]
mod tests {
    use super::*;

    fn names(attempts: &[RendererAttempt]) -> Vec<&'static str> {
        attempts.iter().map(|attempt| attempt.name).collect()
    }

    /// 桌面场景：有硬件 OpenGL 时先用硬件 glow（最快），不掺软渲染
    #[test]
    fn auto_uses_hardware_opengl_first_on_desktop() {
        let attempts = candidates_for(None, true, true, false);
        assert_eq!(names(&attempts)[0], "glow(OpenGL)");
        assert!(!attempts[0].softgl);
        assert_eq!(attempts.len(), 4, "硬件 glow + wgpu 的三种配置");
        assert!(attempts.iter().all(|attempt| !attempt.softgl));
    }

    /// 没有 ICD 但有显卡（很常见：注册表里没有 ICD 子键，DX12 却是好的）：
    /// 仍然先试 wgpu，软件 OpenGL 只作为最后的兜底，不能抢在显卡前面
    #[test]
    fn auto_tries_wgpu_before_bundled_softgl() {
        let attempts = candidates_for(None, false, true, false);
        let names = names(&attempts);
        assert_eq!(names[0], "wgpu(DX12/Vulkan)");
        assert_eq!(names[3], "glow(软件 OpenGL llvmpipe)");
        assert!(attempts[3].softgl);
    }

    /// 既没有 ICD 也没有 softgl：跳过 glow（会直接失败甚至拖垮进程），只留 wgpu
    #[test]
    fn auto_skips_glow_without_any_opengl() {
        let attempts = candidates_for(None, false, false, false);
        assert_eq!(attempts.len(), 3);
        assert!(attempts.iter().all(|attempt| !attempt.softgl));
        assert_eq!(names(&attempts)[0], "wgpu(DX12/Vulkan)");
    }

    /// 显式指定（命令行 --renderer= 或 ARONA_RENDERER）时的候选
    #[test]
    fn requested_renderer_is_respected() {
        assert_eq!(names(&candidates_for(Some("wgpu"), true, true, false)).len(), 3);

        let softgl = candidates_for(Some("softgl"), true, false, false);
        assert_eq!(softgl.len(), 1);
        assert!(softgl[0].softgl);

        // --renderer=glow：没有 ICD 但带 softgl 时自动改用软渲染（否则必然失败）
        let glow = candidates_for(Some("glow"), false, true, false);
        assert_eq!(glow.len(), 1);
        assert!(glow[0].softgl);

        // 有硬件 OpenGL 时 --renderer=glow 就是硬件 glow
        let hardware = candidates_for(Some("glow"), true, true, false);
        assert_eq!(hardware.len(), 1);
        assert!(!hardware[0].softgl);
    }

    /// --softgl / ARONA_SOFTGL=1：无论有没有硬件 OpenGL，软渲染都排第一
    #[test]
    fn softgl_request_always_goes_first() {
        let attempts = candidates_for(None, true, true, true);
        assert_eq!(names(&attempts)[0], "glow(软件 OpenGL llvmpipe)");
        assert!(attempts[0].softgl);
        assert_eq!(attempts.len(), 4);
    }

    /// 卡死换后端：wgpu 卡住时直接跳到另一族（软渲染），不用把三种 wgpu 配置各等一遍
    #[test]
    fn failover_jumps_across_renderer_families() {
        let no_icd = candidates_for(None, false, true, false);
        // [wgpu, wgpu, wgpu, softgl]：第一个 wgpu 卡死 -> 直接跳到软渲染
        assert_eq!(next_family_index(&no_icd, 0), Some(3));
        assert_eq!(next_family_index(&no_icd, 1), Some(3));
        // 软渲染卡死 -> 后面没有别的族了
        assert_eq!(next_family_index(&no_icd, 3), None);

        // [glow(硬件), wgpu x3]：硬件 glow 卡死 -> 跳到第一个 wgpu
        let desktop = candidates_for(None, true, true, false);
        assert_eq!(next_family_index(&desktop, 0), Some(1));
    }

    /// 卡死换后端只换有限次：软渲染与 wgpu 互相踢皮球会在两个进程间无限重启
    #[test]
    fn failover_stops_after_max_handoffs() {
        let no_icd = candidates_for(None, false, true, false);
        assert!(matches!(failover_for(&no_icd, 0, true, 0), Failover::Attempt(3)));
        assert!(matches!(failover_for(&no_icd, 0, true, MAX_HANDOFFS), Failover::NoGui));
        // 软渲染自己卡死、后面又没有别的族 -> 命令行模式（不会回头再试 wgpu）
        assert!(matches!(failover_for(&no_icd, 3, true, 0), Failover::NoGui));

        // 强制 --renderer=wgpu（候选里没有软渲染）卡死时：转到软件 OpenGL 再试一次
        let forced = candidates_for(Some("wgpu"), true, true, false);
        assert!(matches!(failover_for(&forced, 0, true, 0), Failover::Softgl));
        assert!(matches!(failover_for(&forced, 0, false, 0), Failover::NoGui));
    }

    /// 看门狗超时时间：wgpu 默认 6 秒（卡死就早点换），glow/软渲染 20 秒；
    /// ARONA_GUI_TIMEOUT 可统一覆盖，0 表示关闭
    #[test]
    fn gui_ready_timeout_is_configurable() {
        // SAFETY: 只在本用例里读写这个变量，键名与其它用例不冲突
        unsafe { std::env::set_var("ARONA_GUI_TIMEOUT", "0") };
        assert!(gui_ready_timeout(true).is_none(), "0 表示关闭看门狗");
        unsafe { std::env::set_var("ARONA_GUI_TIMEOUT", "5") };
        assert_eq!(gui_ready_timeout(true), Some(std::time::Duration::from_secs(5)));
        assert_eq!(gui_ready_timeout(false), Some(std::time::Duration::from_secs(5)));
        unsafe { std::env::set_var("ARONA_GUI_TIMEOUT", "0.5") };
        assert_eq!(
            gui_ready_timeout(true),
            Some(std::time::Duration::from_millis(500)),
            "支持小数秒（便于试验/快速排障）"
        );
        unsafe { std::env::remove_var("ARONA_GUI_TIMEOUT") };
        assert_eq!(
            gui_ready_timeout(true),
            Some(std::time::Duration::from_secs(6)),
            "wgpu 默认 6 秒"
        );
        assert_eq!(
            gui_ready_timeout(false),
            Some(std::time::Duration::from_secs(20)),
            "glow/软渲染默认 20 秒"
        );
    }
    /// 日志配色：普通行跟随外观（黑夜白 / 白天黑），特殊色不退化并且按背景深浅分档
    #[test]
    fn log_line_color_follows_theme() {
        assert_eq!(
            log_line_color("普通日志", false, true),
            egui::Color32::WHITE,
            "黑夜模式下普通日志行必须是白色"
        );
        assert_eq!(
            log_line_color("普通日志", false, false),
            egui::Color32::BLACK,
            "白天模式下普通日志行必须是黑色"
        );

        // stderr 的普通行按错误色处理，不能变成默认黑/白
        assert_ne!(log_line_color("普通错误", true, true), egui::Color32::WHITE);
        assert_ne!(log_line_color("普通错误", true, false), egui::Color32::BLACK);

        // 特殊色：[Arona]/[OneBot] 绿色，不受“默认色”影响
        let green_dark = log_line_color("[Arona] 启动", false, true);
        let green_light = log_line_color("[Arona] 启动", false, false);
        assert_ne!(green_dark, egui::Color32::WHITE);
        assert_ne!(green_light, egui::Color32::BLACK);

        // WARNING 走黄色，两种外观下也不同
        let warn_dark = log_line_color("WARNING: 磁盘已满", false, true);
        let warn_light = log_line_color("WARNING: 磁盘已满", false, false);
        assert_ne!(warn_dark, warn_light);

        // 深色背景上的特殊色更亮，浅色背景上的更深（保证白底也看得清）
        let sum = |c: egui::Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        assert!(sum(green_dark) > sum(green_light));
        assert!(sum(warn_dark) > sum(warn_light));
    }

    /// 外观偏好的读写：gui.txt 里的值必须能原样读回来
    #[test]
    fn theme_pref_roundtrip() {
        for pref in [ThemePref::System, ThemePref::Light, ThemePref::Dark] {
            assert_eq!(ThemePref::parse(pref.key()), pref);
        }
        assert_eq!(ThemePref::parse("light\n"), ThemePref::Light);
        assert_eq!(ThemePref::parse("  Dark  "), ThemePref::Dark);
        assert_eq!(ThemePref::parse(""), ThemePref::System);
        assert_eq!(ThemePref::parse("乱写的"), ThemePref::System);
    }

    /// 底栏必须排在中央面板之前：egui 里中央面板会吃满剩余高度，之后再补底栏，
    /// 底栏就会反过来盖在内容上（实时日志列表最底下几行会被切掉一半）。
    #[test]
    fn bottom_panel_does_not_overlap_central_content() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 480.0));
        let mut footer_top = f32::NAN;
        let mut content_bottom = f32::NAN;
        ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| {
                egui::TopBottomPanel::top("header").show(ctx, |_ui| {});
                egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
                    footer_top = ui.max_rect().top();
                });
                egui::CentralPanel::default().show(ctx, |ui| {
                    content_bottom = ui.max_rect().bottom();
                });
            },
        );
        assert!(footer_top.is_finite() && content_bottom.is_finite());
        assert!(
            content_bottom <= footer_top,
            "中央面板底部({content_bottom}) 压到了底栏({footer_top})，日志底部会被底栏切掉"
        );
    }

    /// 实时日志的渲染：超长行（QQ 图片 URL 里没有空格）必须整行折行、不丢字，
    /// 也不能溢出面板。之前用 `horizontal_wrapped` 时，超长行会被整条挤到下一行、
    /// 续行直接顶到面板最左边，看起来就像日志左侧被切掉了一块。
    #[test]
    fn log_lines_wrap_inside_panel_without_losing_text() {
        let text = format!(
            "V/Bot.1493074321: [测试群(993871966)] 岚凛凛(739549669) -> &image,url=https://multimedia.nt.qq.com.cn/download?{}",
            "A".repeat(400)
        );
        let line = crate::runtime::log::LiveLine {
            time: "11:30:03".to_string(),
            text: text.clone(),
            stderr: false,
        };
        let lines = [&line];

        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 320.0));
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .show(ctx, |ui| draw_log_lines(ui, &lines, true, false));
            },
        );

        let mut galleys = Vec::new();
        for clipped in &output.shapes {
            if let egui::epaint::Shape::Text(shape) = &clipped.shape {
                galleys.push(shape.galley.clone());
            }
        }
        // 时间戳和正文必须在同一个文本块里（老实现里时间戳会被单独挤成一行）
        assert_eq!(galleys.len(), 1, "一行日志只该产生一个文本块");
        let galley = &galleys[0];
        assert!(galley.size().y > 8.0, "字体没加载，这条测试就没有意义");
        assert!(galley.rows.len() > 1, "超长 URL 必须折行");
        assert!(!galley.elided, "折行不能截断内容");
        assert_eq!(
            galley.text(),
            format!("[11:30:03] {text}"),
            "折行后不能丢字"
        );
        // 整块文字必须留在面板里
        assert!(
            galley.rect.min.x >= 0.0,
            "文字不能溢到面板左边: {:?}",
            galley.rect
        );
        assert!(
            galley.rect.max.x <= screen.width() - 8.0,
            "文字不能溢出面板右边: {:?}",
            galley.rect
        );
        // 第一行必须是「[时间] 正文……」开头：时间戳不能被挤到单独一行去
        let first_row: String = galley
            .text()
            .chars()
            .take(galley.rows[0].glyphs.len())
            .collect();
        assert!(
            first_row.starts_with("[11:30:03] V/Bot.1493074321: "),
            "首行内容不对: {first_row}"
        );
        // 折行续行不许缩进/外凸：每一行都从同一列开始
        let left = galley.rows[0].rect.min.x;
        for row in &galley.rows {
            assert!(
                (row.rect.min.x - left).abs() < 0.5,
                "折行续行没有和首行左对齐: {:?}",
                row.rect
            );
        }
    }
}
