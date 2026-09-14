//! 主窗口 UI：本机信息、设备列表、设置、接入确认、远控视口管理。

use crate::client::RemoteSession;
use crate::protocol::TCP_DEFAULT_PORT;
use crate::state::{AppShared, UiEvent};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct App {
    shared: Arc<AppShared>,
    events_rx: Receiver<UiEvent>,
    sessions: Vec<Arc<RemoteSession>>,
    requests: Vec<PendingRequest>,
    password_dialogs: Vec<PasswordDialog>,
    manual_addr: String,
    new_password: String,
    port_text: String,
    toasts: VecDeque<(Instant, String)>,
    last_prune: Instant,
}

struct PendingRequest {
    req_id: u64,
    name: String,
    device_id: String,
    ip: String,
    view_only: bool,
    resp: Sender<bool>,
}

struct PasswordDialog {
    req_id: u64,
    peer: String,
    resp: Sender<Option<String>>,
    input: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        crate::platform::install_cjk_fonts(&ctx);
        ctx.all_styles_mut(|style| {
            style.visuals = egui::Visuals::dark();
            style.visuals.window_fill = egui::Color32::from_rgb(28, 30, 34);
            style.visuals.panel_fill = egui::Color32::from_rgb(24, 26, 30);
            if let Some(f) = style.text_styles.get_mut(&egui::TextStyle::Body) {
                *f = egui::FontId::proportional(14.0);
            }
            if let Some(f) = style.text_styles.get_mut(&egui::TextStyle::Button) {
                *f = egui::FontId::proportional(14.0);
            }
        });

        let config = Arc::new(std::sync::Mutex::new(crate::config::Config::load()));
        let peers = crate::discovery::new_peer_book();
        let (events_tx, events_rx) = std::sync::mpsc::channel::<UiEvent>();
        let port_text = config.lock().unwrap().tcp_port.to_string();

        let shared = Arc::new(AppShared::new(config, peers, events_tx, ctx.clone()));

        // 发现线程
        let _ = crate::discovery::start_discovery(
            shared.device_id.clone(),
            shared.peers.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            shared.config.clone(),
            ctx.clone(),
        );
        // 被控端服务
        crate::server::start_server(shared.clone());

        // 主界面保活重绘（设备列表刷新）
        ctx.request_repaint_after(Duration::from_secs(1));

        Self {
            shared,
            events_rx,
            sessions: Vec::new(),
            requests: Vec::new(),
            password_dialogs: Vec::new(),
            manual_addr: String::new(),
            new_password: String::new(),
            port_text,
            toasts: VecDeque::new(),
            last_prune: Instant::now(),
        }
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.events_rx.try_recv() {
            match ev {
                UiEvent::IncomingRequest { req_id, name, device_id, ip, view_only, resp } => {
                    self.requests.push(PendingRequest { req_id, name, device_id, ip, view_only, resp });
                }
                UiEvent::PasswordNeeded { req_id, peer, resp } => {
                    self.password_dialogs.push(PasswordDialog { req_id, peer, resp, input: String::new() });
                }
                UiEvent::SessionStarted { session } => {
                    self.shared.ctx.send_viewport_cmd_to(
                        egui::ViewportId(egui::Id::new(("remote", session.id))),
                        egui::ViewportCommand::Focus,
                    );
                    self.sessions.push(session);
                }
                UiEvent::SessionEnded { session_id, reason } => {
                    self.push_toast(format!("会话结束：{reason}"));
                    let _ = session_id;
                }
                UiEvent::Notice { text } => self.push_toast(text),
            }
        }
        self.shared.ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn push_toast(&mut self, text: String) {
        log::info!("{text}");
        self.toasts.push_back((Instant::now(), text));
        while self.toasts.len() > 4 {
            self.toasts.pop_front();
        }
    }

    fn save_config(&self) {
        self.shared.config.lock().unwrap().save();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events();

        if self.last_prune.elapsed() > Duration::from_secs(2) {
            crate::discovery::prune_expired(&self.shared.peers);
            self.last_prune = Instant::now();
        }

        self.show_main_panel(ui);
        self.show_request_dialogs(&ctx);
        self.show_password_dialogs(&ctx);
        self.show_toasts(&ctx);
        self.show_remote_viewports(&ctx);
    }
}

impl App {
    fn show_main_panel(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical(|ui| {
                // ---- 头部 ----
                ui.horizontal(|ui| {                    ui.heading("LC-Deck");
                    ui.colored_label(
                        egui::Color32::from_gray(130),
                        format!("v{} · 局域网远控", crate::platform::app_version()),
                    );
                });
                ui.add_space(6.0);

                // ---- 本机卡片 ----
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let mut name = self.shared.device_name();
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut name)
                                .desired_width(180.0)
                                .hint_text("设备名称"),
                        );
                        if resp.changed() && !name.trim().is_empty() {
                            self.shared.config.lock().unwrap().device_name = name.trim().to_string();
                            self.save_config();
                        }
                        ui.separator();
                        let id = format!("ID: {}", self.shared.device_id);
                        ui.monospace(&id);
                        if ui.small_button("复制").clicked() {
                            copy_text(ui, self.shared.device_id.clone());
                        }
                        ui.separator();
                        let mut accepting = self.shared.accepting.load(Ordering::Relaxed);
                        if ui.checkbox(&mut accepting, "允许被控制").changed() {
                            self.shared.accepting.store(accepting, Ordering::Relaxed);
                        }
                        let controlled = self.shared.controlled_count.load(Ordering::Relaxed);
                        if controlled > 0 {
                            ui.colored_label(egui::Color32::from_rgb(250, 200, 80), format!("正在被 {controlled} 台设备控制"));
                        }
                    });
                });

                ui.add_space(6.0);

                // ---- 设置 ----
                egui::CollapsingHeader::new(egui::RichText::new("⚙ 设置").size(14.0))
                    .default_open(false)
                    .show(ui, |ui| {
                        self.show_settings(ui);
                    });

                ui.add_space(4.0);

                // ---- 手动连接 ----
                ui.horizontal(|ui| {
                    ui.label("手动连接：");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.manual_addr)
                            .desired_width(180.0)
                            .hint_text("IP 或 IP:端口"),
                    );
                    if ui.button("连接").clicked() {
                        if let Some(addr) = parse_addr(&self.manual_addr) {
                            crate::client::connect(self.shared.clone(), addr, false, None);
                        } else {
                            self.push_toast("地址格式不正确".into());
                        }
                    }
                    if ui.button("仅观看").clicked() {
                        if let Some(addr) = parse_addr(&self.manual_addr) {
                            crate::client::connect(self.shared.clone(), addr, true, None);
                        } else {
                            self.push_toast("地址格式不正确".into());
                        }
                    }
                });

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(2.0);
                ui.label(egui::RichText::new("发现局域网设备（若列表为空，请确认对端已运行且允许被控制）").size(12.0).color(egui::Color32::from_gray(120)));

                // ---- 设备列表 ----
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    let peers: Vec<crate::discovery::Peer> = {
                        let book = self.shared.peers.lock().unwrap();
                        let mut v: Vec<_> = book.values().cloned().collect();
                        v.sort_by(|a, b| a.name.cmp(&b.name));
                        v
                    };
                    if peers.is_empty() {
                        ui.add_space(8.0);
                        ui.centered_and_justified(|ui| {
                            ui.colored_label(egui::Color32::from_gray(110), "正在搜索局域网设备…");
                        });
                    }
                    for peer in &peers {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let dot = if peer.accepting {
                                    egui::Color32::from_rgb(90, 220, 120)
                                } else {
                                    egui::Color32::from_gray(110)
                                };
                                let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                                ui.painter().circle_filled(rect.center(), 4.0, dot);
                                ui.vertical(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.strong(&peer.name);
                                        ui.colored_label(
                                            egui::Color32::from_gray(120),
                                            format!("{} · {} · {}", peer.platform, peer.addr.ip(), peer.device_id),
                                        );
                                    });
                                });
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.add_enabled(peer.accepting, egui::Button::new("控制")).clicked() {
                                        crate::client::connect(self.shared.clone(), peer.addr, false, None);
                                    }
                                    if ui.add_enabled(peer.accepting, egui::Button::new("仅观看")).clicked() {
                                        crate::client::connect(self.shared.clone(), peer.addr, true, None);
                                    }
                                    if !peer.accepting {
                                        ui.colored_label(egui::Color32::from_gray(110), "已关闭被控");
                                    }
                                });
                            });
                        });
                    }
                });

                // ---- macOS 权限提示 ----
                if cfg!(target_os = "macos") {
                    ui.add_space(4.0);
                    egui::CollapsingHeader::new(
                        egui::RichText::new("🔑 macOS 权限说明（首次使用必读）").size(12.0).color(egui::Color32::from_gray(140)),
                    )
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "被控制本机需要两项系统权限：\n\
                                 · 屏幕录制 —— 用于把画面发送给主控端（缺失时画面为黑屏或报错）\n\
                                 · 辅助功能 —— 用于接受远端键鼠控制（缺失时输入无法注入）\n\
                                 授权后如不生效，请重启本应用。",
                            )
                            .size(12.0),
                        );
                        ui.horizontal(|ui| {
                            if ui.small_button("打开屏幕录制设置").clicked() {
                                crate::platform::open_screen_permission_settings();
                            }
                            if ui.small_button("打开辅助功能设置").clicked() {
                                crate::platform::open_accessibility_permission_settings();
                            }
                        });
                    });
                }
            });
        });
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        let (has_password, auto_accept, fps, jpeg_quality, max_width, ctrl_as_cmd, sync_clipboard) = {
            let cfg = self.shared.config.lock().unwrap();
            (
                cfg.has_password(),
                cfg.auto_accept,
                cfg.fps,
                cfg.jpeg_quality,
                cfg.max_width,
                cfg.ctrl_as_cmd,
                cfg.sync_clipboard,
            )
        };
        let (mut auto_accept, mut fps, mut jpeg_quality, mut max_width, mut ctrl_as_cmd, mut sync_clipboard) =
            (auto_accept, fps, jpeg_quality, max_width, ctrl_as_cmd, sync_clipboard);

        ui.horizontal(|ui| {
            ui.label("控制密码：");
            if ui
                .add(
                    egui::TextEdit::singleline(&mut self.new_password)
                        .password(true)
                        .desired_width(140.0)
                        .hint_text(if has_password { "已设置（输入可修改）" } else { "未设置" }),
                )
                .lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
            {
                {
                    let mut cfg = self.shared.config.lock().unwrap();
                    cfg.set_password(&self.new_password);
                }
                self.new_password.clear();
                self.shared.notify("控制密码已更新");
                self.save_config();
            }
            if ui.button("保存密码").clicked() {
                {
                    let mut cfg = self.shared.config.lock().unwrap();
                    cfg.set_password(&self.new_password);
                }
                self.new_password.clear();
                self.save_config();
            }
            if ui.add_enabled(has_password, egui::Checkbox::new(&mut auto_accept, "凭密码自动接受连接")).changed() {
                self.shared.config.lock().unwrap().auto_accept = auto_accept;
                self.save_config();
            }
        });

        ui.horizontal(|ui| {
            ui.label("端口：");
            if ui
                .add(egui::TextEdit::singleline(&mut self.port_text).desired_width(70.0))
                .lost_focus()
            {
                if let Ok(p) = self.port_text.parse::<u16>() {
                    let mut cfg = self.shared.config.lock().unwrap();
                    if p != cfg.tcp_port {
                        cfg.tcp_port = p;
                        drop(cfg);
                        self.save_config();
                        self.shared.notify("端口已保存，重启应用后生效");
                    }
                }
            }
            ui.separator();
            ui.label("帧率：");
            let r_fps = ui.add(egui::Slider::new(&mut fps, 5..=30).suffix(" fps"));
            ui.label("画质：");
            let r_q = ui.add(egui::Slider::new(&mut jpeg_quality, 30..=95));
            ui.colored_label(egui::Color32::from_gray(110), "即时生效");
            if r_fps.changed() {
                self.shared.config.lock().unwrap().fps = fps;
            }
            if r_q.changed() {
                self.shared.config.lock().unwrap().jpeg_quality = jpeg_quality;
            }
            if r_fps.drag_stopped() || r_q.drag_stopped() {
                self.save_config();
            }
        });
        ui.horizontal(|ui| {
            ui.label("画面最大宽度：");
            let r_w = ui.add(egui::Slider::new(&mut max_width, 1280..=3840).suffix(" px"));
            if r_w.changed() {
                self.shared.config.lock().unwrap().max_width = max_width;
            }
            if r_w.drag_stopped() {
                self.save_config();
                self.shared.notify("画面参数已保存并实时生效");
            }
            ui.separator();
            if ui.checkbox(&mut ctrl_as_cmd, "被控(macOS)时 Ctrl 映射为 Command").changed() {
                self.shared.config.lock().unwrap().ctrl_as_cmd = ctrl_as_cmd;
                self.save_config();
            }
            if ui.checkbox(&mut sync_clipboard, "会话中同步剪贴板").changed() {
                self.shared.config.lock().unwrap().sync_clipboard = sync_clipboard;
                self.save_config();
            }
        });
    }

    fn show_request_dialogs(&mut self, ctx: &egui::Context) {
        let mut answered: Vec<usize> = Vec::new();
        for (i, req) in self.requests.iter().enumerate() {
            let mut open = true;
            egui::Window::new(format!("连接请求 #{}", req.req_id))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(egui::RichText::new("有设备请求控制这台电脑").size(15.0));
                    ui.add_space(4.0);
                    egui::Grid::new("req_grid").num_columns(2).show(ui, |ui| {
                        ui.label("设备名称"); ui.strong(&req.name); ui.end_row();
                        ui.label("设备 ID"); ui.monospace(&req.device_id); ui.end_row();
                        ui.label("来源 IP"); ui.monospace(&req.ip); ui.end_row();
                        ui.label("请求模式"); ui.label(if req.view_only { "仅观看" } else { "完全控制" }); ui.end_row();
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(egui::RichText::new("允许").color(egui::Color32::from_rgb(120, 230, 140))).clicked() {
                            let _ = req.resp.send(true);
                            answered.push(i);
                        }
                        if ui.button("拒绝").clicked() {
                            let _ = req.resp.send(false);
                            answered.push(i);
                        }
                        ui.colored_label(egui::Color32::from_gray(120), "90 秒无响应将自动拒绝");
                    });
                });
            if !open {
                let _ = req.resp.send(false);
                answered.push(i);
            }
        }
        for i in answered.into_iter().rev() {
            self.requests.remove(i);
        }
    }

    fn show_password_dialogs(&mut self, ctx: &egui::Context) {
        let mut done: Vec<usize> = Vec::new();
        for (i, dlg) in self.password_dialogs.iter_mut().enumerate() {
            let mut open = true;
            egui::Window::new(format!("需要控制密码 #{}", dlg.req_id))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!("对端 {} 要求输入控制密码", dlg.peer));
                    ui.add_space(4.0);
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut dlg.input)
                            .password(true)
                            .desired_width(220.0)
                            .hint_text("控制密码"),
                    );
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("连接").clicked() || enter {
                            let _ = dlg.resp.send(Some(dlg.input.clone()));
                            done.push(i);
                        }
                        if ui.button("取消").clicked() {
                            let _ = dlg.resp.send(None);
                            done.push(i);
                        }
                    });
                });
            if !open {
                let _ = dlg.resp.send(None);
                done.push(i);
            }
        }
        for i in done.into_iter().rev() {
            self.password_dialogs.remove(i);
        }
    }

    fn show_toasts(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|(t, _)| t.elapsed() < Duration::from_secs(6));
        if self.toasts.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0])
            .show(ctx, |ui| {
                for (_, text) in &self.toasts {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(text);
                    });
                }
            });
    }

    fn show_remote_viewports(&mut self, ctx: &egui::Context) {
        // 移除已关闭会话
        let closed: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.closed.load(Ordering::Relaxed))
            .map(|(i, _)| i)
            .collect();
        for i in closed.into_iter().rev() {
            let s = self.sessions.remove(i);
            self.push_toast(format!("与 {} 的远控会话已结束", s.peer_name));
        }

        for session in &self.sessions {
            let s = session.clone();
            let id = egui::ViewportId(egui::Id::new(("remote", s.id)));
            let title = format!("远控 · {}", s.peer_name);
            ctx.show_viewport_deferred(
                id,
                egui::ViewportBuilder::default()
                    .with_title(title)
                    .with_inner_size([1100.0, 700.0])
                    .with_active(true),
                move |ui, _class| {
                    crate::ui_remote::show(ui, &s);
                },
            );
        }
    }
}

fn parse_addr(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains(':') {
        s.parse().ok()
    } else {
        format!("{s}:{TCP_DEFAULT_PORT}").parse().ok()
    }
}

fn copy_text(ui: &egui::Ui, text: String) {
    ui.output_mut(|o| o.commands.push(egui::OutputCommand::CopyText(text)));
}
