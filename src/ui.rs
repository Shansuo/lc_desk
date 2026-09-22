//! 主窗口 UI：本机信息、设备列表、设置、接入确认、远控视口管理。

use crate::client::RemoteSession;
use crate::protocol::TCP_DEFAULT_PORT;
use crate::state::{AppShared, UiEvent};
use crate::theme;
use egui::{Id, RichText, Ui, Vec2};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 接入请求等待用户响应的时长
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

pub struct App {
    shared: Arc<AppShared>,
    events_rx: Receiver<UiEvent>,
    sessions: Vec<Arc<RemoteSession>>,
    requests: Vec<PendingRequest>,
    password_dialogs: Vec<PasswordDialog>,
    manual_addr: String,
    new_password: String,
    port_text: String,
    toasts: VecDeque<(Instant, ToastKind, String)>,
    last_prune: Instant,
    /// 发现线程的停止开关（退出时置位）
    discovery_stop: Option<Arc<AtomicBool>>,
    /// 新建会话的窗口需要主动聚焦，Viewport 创建于下一帧故延迟下发
    pending_focus: VecDeque<egui::ViewportId>,
    /// 本机 IP 缓存（每帧探测会频繁开 socket，网络切换又能改变结果）
    local_ip: Option<String>,
    last_ip_check: Instant,
}

struct PendingRequest {
    req_id: u64,
    name: String,
    device_id: String,
    ip: String,
    view_only: bool,
    created: Instant,
    resp: Sender<bool>,
}

struct PasswordDialog {
    req_id: u64,
    peer: String,
    resp: Sender<Option<String>>,
    input: String,
}

#[derive(Clone, Copy)]
enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

impl ToastKind {
    fn color(self) -> egui::Color32 {
        match self {
            ToastKind::Info => theme::TEXT_DIM,
            ToastKind::Success => theme::SUCCESS,
            ToastKind::Warn => theme::WARN,
            ToastKind::Error => theme::DANGER,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            ToastKind::Info => "ℹ",
            ToastKind::Success => "✓",
            ToastKind::Warn => "⚠",
            ToastKind::Error => "✕",
        }
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        crate::platform::install_cjk_fonts(&ctx);
        theme::apply(&ctx);

        let config = Arc::new(std::sync::Mutex::new(crate::config::Config::load()));
        let peers = crate::discovery::new_peer_book();
        let (events_tx, events_rx) = std::sync::mpsc::channel::<UiEvent>();
        let port_text = config.lock().unwrap().tcp_port.to_string();

        let shared = Arc::new(AppShared::new(config, peers, events_tx, ctx.clone()));

        // 发现线程。注意必须把 shared.accepting 交给它，
        // 否则广播里的 accepting 恒为 true，主界面开关形同虚设。
        // 发现线程失败必须让用户看见：此前用 .ok() 静默丢弃，
        // 一旦端口被占用或广播不可用，界面只会一直显示「找不到设备」而无从排查。
        let mut init_error: Option<String> = None;
        let discovery_stop = match crate::discovery::start_discovery(
            shared.device_id.clone(),
            shared.peers.clone(),
            shared.accepting.clone(),
            shared.server_ready.clone(),
            shared.config.clone(),
            ctx.clone(),
        ) {
            Ok(stop) => Some(stop),
            Err(e) => {
                log::error!("设备发现启动失败: {e}");
                init_error = Some(format!("设备发现启动失败，无法搜索局域网设备：{e}"));
                None
            }
        };
        // 被控端服务
        crate::server::start_server(shared.clone());

        // 主界面保活重绘（设备列表刷新）
        ctx.request_repaint_after(Duration::from_secs(1));

        let mut app = Self {
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
            discovery_stop,
            pending_focus: VecDeque::new(),
            local_ip: None,
            last_ip_check: Instant::now(),
        };
        if let Some(err) = init_error {
            app.push_toast(ToastKind::Error, err);
        }
        app
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.events_rx.try_recv() {
            match ev {
                UiEvent::IncomingRequest { req_id, name, device_id, ip, view_only, resp } => {
                    self.requests.push(PendingRequest {
                        req_id,
                        name,
                        device_id,
                        ip,
                        view_only,
                        created: Instant::now(),
                        resp,
                    });
                    // 把主窗口提到前台：确认弹窗若藏在后台窗口里没人点，
                    // 主控端就只能干等到超时，表现为「点了没反应」
                    self.shared.ctx.send_viewport_cmd_to(
                        egui::ViewportId::ROOT,
                        egui::ViewportCommand::Focus,
                    );
                }
                UiEvent::PasswordNeeded { req_id, peer, resp } => {
                    self.password_dialogs
                        .push(PasswordDialog { req_id, peer, resp, input: String::new() });
                }
                UiEvent::SessionStarted { session } => {
                    self.pending_focus.push_back(session.viewport_id());
                    self.sessions.push(session);
                }
                UiEvent::SessionEnded { peer_name, reason, .. } => {
                    self.push_toast(ToastKind::Info, format!("{peer_name} 的远控已结束：{reason}"));
                }
                UiEvent::Notice { text } => self.push_toast(ToastKind::Info, text),
            }
        }
        self.shared.ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn push_toast(&mut self, kind: ToastKind, text: String) {
        log::info!("{text}");
        self.toasts.push_back((Instant::now(), kind, text));
        while self.toasts.len() > 4 {
            self.toasts.pop_front();
        }
    }

    fn save_config(&self) {
        self.shared.config.lock().unwrap().save();
    }

    /// 每 5 秒探测一次本机 IP，兼顾实时性与开销。
    fn cached_local_ip(&mut self) -> Option<String> {
        if self.last_ip_check.elapsed() > Duration::from_secs(5) {
            self.local_ip = crate::platform::local_ip();
            self.last_ip_check = Instant::now();
        }
        self.local_ip.clone()
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events();

        if self.last_prune.elapsed() > Duration::from_secs(2) {
            crate::discovery::prune_expired(&self.shared.peers);
            self.last_prune = Instant::now();
        }

        // 面板布局有顺序要求：先顶/底，CentralPanel 最后吃掉剩余空间
        self.show_header(ui);
        self.show_footer(ui);
        self.show_body(ui);
        self.show_request_dialogs(&ctx);
        self.show_password_dialogs(&ctx);
        self.show_toasts(&ctx);
        self.show_remote_viewports(&ctx);
    }

    fn on_exit(&mut self) {
        if let Some(stop) = &self.discovery_stop {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

impl App {
    // ---------------- 头部 ----------------
    fn show_header(&mut self, ui: &mut Ui) {
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    draw_logo(ui);
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("LC-Deck").size(19.0).color(theme::TEXT).strong());
                        ui.label(
                            RichText::new(format!(
                                "局域网远控 · {} · v{}",
                                crate::platform::platform_name(),
                                crate::platform::app_version()
                            ))
                            .size(11.5)
                            .color(theme::TEXT_FAINT),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let accepting = self.shared.accepting.load(Ordering::Relaxed);
                        let ready = self.shared.server_ready.load(Ordering::Relaxed);
                        let (text, color) = match (accepting, ready) {
                            (false, _) => ("已关闭被控", theme::TEXT_DIM),
                            (true, false) => ("端口不可用", theme::DANGER),
                            (true, true) => ("可被控制", theme::SUCCESS),
                        };
                        theme::pill(ui, text, color);
                    });
                });
            });
    }

    // ---------------- 中部：本机卡片 + 设备列表 ----------------
    fn show_body(&mut self, ui: &mut Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin::symmetric(12, 10)))
            .show(ui, |ui| {
                self.show_self_card(ui);
                ui.add_space(10.0);
                self.show_peer_list(ui);
            });
    }

    fn show_self_card(&mut self, ui: &mut Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());

            // 设备名（可编辑）
            theme::section_title(ui, "本机名称");
            ui.add_space(4.0);
            let mut name = self.shared.device_name();
            let resp = ui.add_sized(
                Vec2::new(ui.available_width(), 30.0),
                egui::TextEdit::singleline(&mut name)
                    .hint_text("设备名称")
                    .margin(egui::Margin::symmetric(8, 6)),
            );
            if resp.changed() && !name.trim().is_empty() {
                self.shared.config.lock().unwrap().device_name = name.trim().to_string();
                self.save_config();
            }
            ui.add_space(10.0);

            // 设备 ID
            ui.horizontal(|ui| {
                ui.label(RichText::new("设备 ID").size(12.0).color(theme::TEXT_FAINT));
                ui.label(
                    RichText::new(&self.shared.device_id)
                        .monospace()
                        .size(13.0)
                        .color(theme::TEXT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add(theme::ghost_button("复制").min_size(Vec2::new(52.0, 24.0))).clicked() {
                        copy_text(ui, self.shared.device_id.clone());
                        self.push_toast(ToastKind::Success, "设备 ID 已复制".into());
                    }
                });
            });
            // 本机 IP（手动连接时需要告知对端）
            if let Some(ip) = self.cached_local_ip() {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("本机 IP").size(12.0).color(theme::TEXT_FAINT));
                    ui.label(RichText::new(&ip).monospace().size(13.0).color(theme::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(theme::ghost_button("复制").min_size(Vec2::new(52.0, 24.0)))
                            .clicked()
                        {
                            copy_text(ui, ip.clone());
                            self.push_toast(ToastKind::Success, format!("已复制 IP {ip}"));
                        }
                    });
                });
            }

            ui.add_space(8.0);
            theme::hairline(ui);

            // 允许被控制开关
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new("允许被控制").size(14.0).color(theme::TEXT).strong());
                    ui.label(
                        RichText::new("关闭后局域网内无法连接本机")
                            .size(11.5)
                            .color(theme::TEXT_FAINT),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut accepting = self.shared.accepting.load(Ordering::Relaxed);
                    if theme::toggle(ui, Id::new("accepting_toggle"), &mut accepting).clicked() {
                        self.shared.accepting.store(accepting, Ordering::Relaxed);
                        self.push_toast(
                            if accepting { ToastKind::Success } else { ToastKind::Warn },
                            if accepting {
                                "已开启「允许被控制」".into()
                            } else {
                                "已关闭「允许被控制」，其他设备将无法连接本机".into()
                            },
                        );
                    }
                });
            });

            let controlled = self.shared.controlled_count.load(Ordering::Relaxed);
            if controlled > 0 {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    theme::dot_glow(ui, theme::WARN, 4.0);
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("正在被 {controlled} 台设备控制"))
                            .size(12.5)
                            .color(theme::WARN),
                    );
                });
            }
        });
    }

    fn show_peer_list(&mut self, ui: &mut Ui) {
        let peers: Vec<crate::discovery::Peer> = {
            let book = self.shared.peers.lock().unwrap();
            let mut v: Vec<_> = book.values().cloned().collect();
            v.sort_by(|a, b| {
                b.accepting
                    .cmp(&a.accepting)
                    .then_with(|| a.name.cmp(&b.name))
            });
            v
        };

        ui.horizontal(|ui| {
            ui.label(RichText::new("局域网设备").size(14.0).color(theme::TEXT).strong());
            ui.add_space(4.0);
            if !peers.is_empty() {
                theme::pill(ui, peers.len().to_string(), theme::ACCENT);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new("每 2 秒自动发现")
                        .size(11.0)
                        .color(theme::TEXT_FAINT),
                );
            });
        });
        ui.add_space(6.0);

        if peers.is_empty() {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                theme::empty_state(
                    ui,
                    "◎",
                    "正在搜索局域网设备…",
                    "确认对端已运行且防火墙已放行；也可用上方「本机 IP」手动连接",
                );
            });
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                for peer in &peers {
                    self.peer_card(ui, peer);
                }
            });
    }

    fn peer_card(&mut self, ui: &mut Ui, peer: &crate::discovery::Peer) {
        match draw_peer_card(ui, peer) {
            PeerAction::Connect => self.connect_to(peer, false),
            PeerAction::ViewOnly => self.connect_to(peer, true),
            PeerAction::None => {}
        }
    }

    // ---------------- 底部：手动连接 + 设置 + 权限 ----------------
    fn show_footer(&mut self, ui: &mut Ui) {
        egui::Panel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .stroke(egui::Stroke::new(1.0, theme::BORDER)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        Vec2::new((ui.available_width() - 172.0).max(110.0), 30.0),
                        egui::TextEdit::singleline(&mut self.manual_addr)
                            .hint_text("手动连接：IP 或 IP:端口")
                            .margin(egui::Margin::symmetric(8, 6)),
                    );
                    if ui.add(theme::ghost_button("仅观看")).clicked() {
                        self.manual_connect(true);
                    }
                    if ui.add(theme::primary_button("连接")).clicked() {
                        self.manual_connect(false);
                    }
                });

                ui.add_space(2.0);

                egui::CollapsingHeader::new(RichText::new("⚙  设置").size(13.0).color(theme::TEXT_DIM))
                    .default_open(false)
                    .show(ui, |ui| {
                        // 限高：设置展开时不要把中部的设备列表挤没
                        egui::ScrollArea::vertical()
                            .max_height(320.0)
                            .auto_shrink(false)
                            .show(ui, |ui| self.show_settings(ui));
                    });

                if cfg!(target_os = "macos") {
                    egui::CollapsingHeader::new(
                        RichText::new("🔑  macOS 权限说明").size(13.0).color(theme::TEXT_DIM),
                    )
                    .default_open(false)
                    .show(ui, |ui| self.show_permissions(ui));
                }
            });
    }

    /// 发起连接。必须先给一个即时反馈：握手与对端人工确认可能耗时长，
    /// 没有任何提示的话用户会以为「点了没反应」。
    fn connect_to(&mut self, peer: &crate::discovery::Peer, view_only: bool) {
        self.push_toast(
            ToastKind::Info,
            format!(
                "正在连接 {}（{}:{}）…",
                peer.name,
                peer.addr.ip(),
                peer.addr.port()
            ),
        );
        crate::client::connect(self.shared.clone(), peer.addr, view_only, None);
    }

    fn manual_connect(&mut self, view_only: bool) {
        match parse_addr(&self.manual_addr) {
            Some(addr) => {
                self.push_toast(ToastKind::Info, format!("正在连接 {addr}…"));
                crate::client::connect(self.shared.clone(), addr, view_only, None);
                self.manual_addr.clear();
            }
            None => self.push_toast(ToastKind::Error, "地址格式不正确，示例 192.168.1.9".into()),
        }
    }

    fn show_settings(&mut self, ui: &mut Ui) {
        let (has_password, mut auto_accept, mut fps, mut quality, mut max_width, mut ctrl_as_cmd, mut sync_clipboard) = {
            let c = self.shared.config.lock().unwrap();
            (
                c.has_password(),
                c.auto_accept,
                c.fps,
                c.jpeg_quality,
                c.max_width,
                c.ctrl_as_cmd,
                c.sync_clipboard,
            )
        };

        // ---- 访问与安全 ----
        theme::section_title(ui, "访问与安全");
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let hint = if has_password { "已设置，输入新密码可修改" } else { "未设置" };
            let resp = ui.add_sized(
                Vec2::new(180.0, 28.0),
                egui::TextEdit::singleline(&mut self.new_password)
                    .password(true)
                    .hint_text(hint)
                    .margin(egui::Margin::symmetric(8, 5)),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.add(theme::ghost_button("保存密码")).clicked() || enter {
                self.apply_password();
            }
            if ui
                .add_enabled(has_password, theme::ghost_button("清除"))
                .clicked()
            {
                self.new_password.clear();
                self.apply_password();
            }
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(has_password, |ui| {
                if ui
                    .add(egui::Checkbox::new(
                        &mut auto_accept,
                        RichText::new("凭密码自动接受连接").size(12.5).color(theme::TEXT_DIM),
                    ))
                    .changed()
                {
                    self.shared.config.lock().unwrap().auto_accept = auto_accept;
                    self.save_config();
                }
            });
            if !has_password {
                ui.label(RichText::new("（需先设置密码）").size(11.0).color(theme::TEXT_FAINT));
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("端口").size(12.5).color(theme::TEXT_DIM));
            let resp = ui.add_sized(
                Vec2::new(84.0, 26.0),
                egui::TextEdit::singleline(&mut self.port_text)
                    .margin(egui::Margin::symmetric(8, 4)),
            );
            let enter = resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if resp.lost_focus() || enter {
                self.apply_port();
            }
            ui.label(RichText::new("重启后生效").size(11.0).color(theme::TEXT_FAINT));
        });

        ui.add_space(10.0);
        theme::hairline(ui);

        // ---- 画质与性能 ----
        theme::section_title(ui, "画质与性能");
        ui.add_space(2.0);
        ui.label(RichText::new("会话中调整即时生效").size(11.0).color(theme::TEXT_FAINT));
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.add_sized(Vec2::new(44.0, 20.0), egui::Label::new(RichText::new("帧率").size(12.5).color(theme::TEXT_DIM)));
            let r = ui.add_sized(
                Vec2::new(180.0, 20.0),
                egui::Slider::new(&mut fps, crate::config::FPS_RANGE).suffix(" fps").show_value(true),
            );
            changed |= r.changed();
            ui.label(RichText::new(format!("{fps}")).size(11.5).color(theme::TEXT_FAINT));
        });
        ui.horizontal(|ui| {
            ui.add_sized(Vec2::new(44.0, 20.0), egui::Label::new(RichText::new("画质").size(12.5).color(theme::TEXT_DIM)));
            let r = ui.add_sized(
                Vec2::new(180.0, 20.0),
                egui::Slider::new(&mut quality, crate::config::QUALITY_RANGE).show_value(true),
            );
            changed |= r.changed();
            ui.label(RichText::new(format!("{quality}")).size(11.5).color(theme::TEXT_FAINT));
        });
        ui.horizontal(|ui| {
            ui.add_sized(Vec2::new(44.0, 20.0), egui::Label::new(RichText::new("宽度").size(12.5).color(theme::TEXT_DIM)));
            let r = ui.add_sized(
                Vec2::new(180.0, 20.0),
                egui::Slider::new(&mut max_width, crate::config::MAX_WIDTH_RANGE)
                    .suffix(" px")
                    .show_value(true),
            );
            changed |= r.changed();
            ui.label(RichText::new(format!("{max_width}")).size(11.5).color(theme::TEXT_FAINT));
        });
        if changed {
            let mut c = self.shared.config.lock().unwrap();
            c.fps = fps;
            c.jpeg_quality = quality;
            c.max_width = max_width;
            drop(c);
            self.save_config();
        }

        ui.add_space(10.0);
        theme::hairline(ui);

        // ---- 输入与剪贴板 ----
        theme::section_title(ui, "输入与剪贴板");
        ui.add_space(2.0);
        if ui
            .add(egui::Checkbox::new(
                &mut ctrl_as_cmd,
                RichText::new("被控(macOS)时 Ctrl 映射为 Command").size(12.5).color(theme::TEXT_DIM),
            ))
            .changed()
        {
            self.shared.config.lock().unwrap().ctrl_as_cmd = ctrl_as_cmd;
            self.save_config();
        }
        if ui
            .add(egui::Checkbox::new(
                &mut sync_clipboard,
                RichText::new("会话中双向同步剪贴板").size(12.5).color(theme::TEXT_DIM),
            ))
            .changed()
        {
            self.shared.config.lock().unwrap().sync_clipboard = sync_clipboard;
            self.save_config();
        }
    }

    fn apply_password(&mut self) {
        {
            let mut cfg = self.shared.config.lock().unwrap();
            cfg.set_password(&self.new_password);
        }
        let had = self.shared.config.lock().unwrap().has_password();
        self.new_password.clear();
        self.save_config();
        self.push_toast(
            if had { ToastKind::Success } else { ToastKind::Warn },
            if had { "控制密码已更新".into() } else { "已清除控制密码".into() },
        );
    }

    fn apply_port(&mut self) {
        match self.port_text.trim().parse::<u16>() {
            Ok(p) if p > 0 => {
                let mut cfg = self.shared.config.lock().unwrap();
                if p != cfg.tcp_port {
                    cfg.tcp_port = p;
                    drop(cfg);
                    self.save_config();
                    self.push_toast(ToastKind::Warn, "端口已保存，重启应用后生效".into());
                }
            }
            _ => {
                self.port_text = self.shared.config.lock().unwrap().tcp_port.to_string();
                self.push_toast(ToastKind::Error, "端口需为 1-65535 的数字".into());
            }
        }
    }

    fn show_permissions(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new(
                "被控制本机需要两项系统权限：\n\
                 · 屏幕录制 —— 用于把画面发送给主控端（缺失时黑屏或报错）\n\
                 · 辅助功能 —— 用于接受远端键鼠（缺失时输入无法注入）\n\
                 授权后如不生效，请重启本应用。",
            )
            .size(12.0)
            .color(theme::TEXT_DIM),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.add(theme::ghost_button("打开屏幕录制设置").min_size(Vec2::new(0.0, 26.0))).clicked() {
                crate::platform::open_screen_permission_settings();
            }
            if ui.add(theme::ghost_button("打开辅助功能设置").min_size(Vec2::new(0.0, 26.0))).clicked() {
                crate::platform::open_accessibility_permission_settings();
            }
        });
    }

    // ---------------- 接入确认 ----------------
    fn show_request_dialogs(&mut self, ctx: &egui::Context) {
        let mut answered: Vec<usize> = Vec::new();
        for (i, req) in self.requests.iter().enumerate() {
            let mut decision: Option<bool> = None;
            let remaining = REQUEST_TIMEOUT
                .as_secs()
                .saturating_sub(req.created.elapsed().as_secs());

            egui::Modal::new(Id::new(("incoming", req.req_id)))
                .frame(
                    egui::Frame::new()
                        .fill(theme::PANEL)
                        .stroke(egui::Stroke::new(1.0, theme::BORDER))
                        .corner_radius(theme::R_LG)
                        .inner_margin(egui::Margin::same(18))
                        .shadow(egui::epaint::Shadow {
                            offset: [0, 12],
                            blur: 32,
                            spread: 0,
                            color: egui::Color32::from_black_alpha(140),
                        }),
                )
                .backdrop_color(egui::Color32::from_black_alpha(120))
                .show(ctx, |ui| {
                    ui.set_min_width(320.0);
                    ui.horizontal(|ui| {
                        theme::dot_glow(ui, theme::WARN, 5.0);
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new("连接请求")
                                .size(16.0)
                                .color(theme::TEXT)
                                .strong(),
                        );
                    });
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!("「{}」请求{}这台电脑", req.name, if req.view_only { "观看" } else { "控制" }))
                            .size(13.0)
                            .color(theme::TEXT_DIM),
                    );
                    ui.add_space(10.0);

                    theme::sunken().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::Grid::new(("req_grid", req.req_id))
                            .num_columns(2)
                            .spacing([14.0, 6.0])
                            .show(ui, |ui| {
                                ui.label(RichText::new("设备 ID").size(12.0).color(theme::TEXT_FAINT));
                                ui.label(RichText::new(&req.device_id).monospace().size(12.5).color(theme::TEXT));
                                ui.end_row();
                                ui.label(RichText::new("来源 IP").size(12.0).color(theme::TEXT_FAINT));
                                ui.label(RichText::new(&req.ip).monospace().size(12.5).color(theme::TEXT));
                                ui.end_row();
                                ui.label(RichText::new("模式").size(12.0).color(theme::TEXT_FAINT));
                                ui.label(
                                    RichText::new(if req.view_only { "仅观看" } else { "完全控制" })
                                        .size(12.5)
                                        .color(if req.view_only { theme::ACCENT } else { theme::WARN }),
                                );
                                ui.end_row();
                            });
                    });

                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        if ui.add(theme::primary_button("允许")).clicked() {
                            decision = Some(true);
                        }
                        if ui.add(theme::danger_button("拒绝")).clicked() {
                            decision = Some(false);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!("{remaining} 秒后自动拒绝"))
                                    .size(11.0)
                                    .color(theme::TEXT_FAINT),
                            );
                        });
                    });
                });

            if let Some(ok) = decision {
                let _ = req.resp.send(ok);
                if !answered.contains(&i) {
                    answered.push(i);
                }
            }
        }
        for i in answered.into_iter().rev() {
            self.requests.remove(i);
        }
    }

    fn show_password_dialogs(&mut self, ctx: &egui::Context) {
        let mut done: Vec<usize> = Vec::new();
        for (i, dlg) in self.password_dialogs.iter_mut().enumerate() {
            let mut submit: Option<Option<String>> = None;

            egui::Modal::new(Id::new(("password", dlg.req_id)))
                .frame(
                    egui::Frame::new()
                        .fill(theme::PANEL)
                        .stroke(egui::Stroke::new(1.0, theme::BORDER))
                        .corner_radius(theme::R_LG)
                        .inner_margin(egui::Margin::same(18))
                        .shadow(egui::epaint::Shadow {
                            offset: [0, 12],
                            blur: 32,
                            spread: 0,
                            color: egui::Color32::from_black_alpha(140),
                        }),
                )
                .backdrop_color(egui::Color32::from_black_alpha(120))
                .show(ctx, |ui| {
                    ui.set_min_width(300.0);
                    ui.label(RichText::new("需要控制密码").size(16.0).color(theme::TEXT).strong());
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("对端 {} 要求输入控制密码", dlg.peer))
                            .size(12.5)
                            .color(theme::TEXT_DIM),
                    );
                    ui.add_space(10.0);
                    let resp = ui.add_sized(
                        Vec2::new(ui.available_width(), 30.0),
                        egui::TextEdit::singleline(&mut dlg.input)
                            .password(true)
                            .hint_text("控制密码")
                            .margin(egui::Margin::symmetric(8, 6)),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = Some(Some(dlg.input.clone()));
                    }
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        if ui.add(theme::primary_button("连接")).clicked() {
                            submit = Some(Some(dlg.input.clone()));
                        }
                        if ui.add(theme::ghost_button("取消")).clicked() {
                            submit = Some(None);
                        }
                    });
                });

            if let Some(value) = submit {
                let _ = dlg.resp.send(value);
                if !done.contains(&i) {
                    done.push(i);
                }
            }
        }
        for i in done.into_iter().rev() {
            self.password_dialogs.remove(i);
        }
    }

    // ---------------- Toast ----------------
    fn show_toasts(&mut self, ctx: &egui::Context) {
        self.toasts
            .retain(|(t, _, _)| t.elapsed() < Duration::from_secs(6));
        if self.toasts.is_empty() {
            return;
        }
        // 锚在头部下方右侧：既不遮挡底部的连接/设置，也不压住设备列表主体
        egui::Area::new(Id::new("toasts"))
            .anchor(egui::Align2::RIGHT_TOP, [-14.0, 72.0])
            .show(ctx, |ui| {
                ui.vertical(|ui| {
                    for (_, kind, text) in &self.toasts {
                        let color = kind.color();
                        egui::Frame::new()
                            .fill(theme::CARD)
                            .stroke(egui::Stroke::new(1.0, theme::BORDER))
                            .corner_radius(theme::R_MD)
                            .inner_margin(egui::Margin::symmetric(12, 9))
                            .outer_margin(egui::Margin::symmetric(0, 4))
                            .shadow(egui::epaint::Shadow {
                                offset: [0, 6],
                                blur: 20,
                                spread: 0,
                                color: egui::Color32::from_black_alpha(120),
                            })
                            .show(ui, |ui| {
                                ui.set_max_width(300.0);
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(kind.icon()).size(13.0).color(color));
                                    ui.add_space(2.0);
                                    ui.label(
                                        RichText::new(text).size(12.5).color(theme::TEXT),
                                    );
                                });
                            });
                    }
                });
            });
    }

    // ---------------- 远控视口 ----------------
    fn show_remote_viewports(&mut self, ctx: &egui::Context) {
        // 移除已关闭会话，并显式关闭其窗口
        let closed: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.closed.load(Ordering::Relaxed))
            .map(|(i, _)| i)
            .collect();
        for i in closed.into_iter().rev() {
            let s = self.sessions.remove(i);
            ctx.send_viewport_cmd_to(s.viewport_id(), egui::ViewportCommand::Close);
            self.push_toast(ToastKind::Info, format!("与 {} 的远控会话已结束", s.peer_name));
        }

        let mut focus_now = std::mem::take(&mut self.pending_focus);
        for session in &self.sessions {
            let s = session.clone();
            let id = s.viewport_id();
            let title = format!("LC-Deck · 远控 {}", s.peer_name);
            ctx.show_viewport_deferred(
                id,
                egui::ViewportBuilder::default()
                    .with_title(title)
                    .with_inner_size([1180.0, 760.0])
                    .with_min_inner_size([640.0, 420.0]),
                move |ui, _class| {
                    crate::ui_remote::show(ui, &s);
                },
            );
            if let Some(pos) = focus_now.iter().position(|v| *v == id) {
                focus_now.remove(pos);
                ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Focus);
            }
        }
        self.pending_focus = focus_now;
    }
}

// ---------------- 工具 ----------------

/// 品牌标记：一个简单的显示器图形。
fn draw_logo(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(38.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, theme::R_MD, theme::tint(theme::ACCENT, 34));
    // 屏幕
    let screen = egui::Rect::from_min_size(
        rect.min + Vec2::new(8.0, 10.0),
        Vec2::new(rect.width() - 16.0, rect.height() - 20.0),
    );
    ui.painter().rect_stroke(
        screen,
        2.0,
        egui::Stroke::new(1.6, theme::ACCENT),
        egui::StrokeKind::Outside,
    );
    // 光标（对角线）
    ui.painter().line_segment(
        [
            egui::pos2(screen.center().x - 3.0, screen.center().y - 3.0),
            egui::pos2(screen.center().x + 4.0, screen.center().y + 4.0),
        ],
        egui::Stroke::new(1.6, theme::ACCENT),
    );
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

fn copy_text(ui: &Ui, text: String) {
    ui.ctx().copy_text(text);
}

// ---------------- 设备卡片 ----------------

/// 设备卡片的绘制结果：把「怎么画」和「点了做什么」分开，卡片布局即可单测。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PeerAction {
    None,
    Connect,
    ViewOnly,
}

/// 画一张设备卡片，返回用户点选的动作。
///
/// 布局上修掉了此前两个问题：
///
/// 1. **卡片被拉伸到整个列表视口高度。** 原实现是「两行 + 嵌套
///    `with_layout(right_to_left)`」，`with_layout` 的子 Ui 以剩余可用空间为
///    最大矩形，横向布局的十字轴居中会把这一整块高度算进自身尺寸。实测卡片
///    高 171px（列表视口约 181px），按钮下方留下 50px 空白，更要命的是第 2、3
///    台设备被压成 0 高度、内容总高又不超出视口所以连滚动条都不出现 ——
///    发现到 3 台设备却只看得到 1 台。现在整卡是**单行**，高度只由内容决定。
/// 2. **长设备名会把右侧操作区挤出卡片。** 现在先右后左分配：操作区先占位，
///    信息区再吃剩余宽度，并对名称/详情做截断而不是换行。
fn draw_peer_card(ui: &mut Ui, peer: &crate::discovery::Peer) -> PeerAction {
    let mut action = PeerAction::None;

    theme::card().show(ui, |ui| {
        // 右对齐必须用 `with_layout(..., Align::Min)`。
        //
        // 这里踩过一个坑：用 `Align::Center`（以及 `with_layout` 的其它居中写法）
        // 时，egui 会把「整块可用高度」算成该子布局的 frame 并 `expand_to_include_rect`，
        // 于是卡片被拉伸到列表视口的高度（实测 900 高视口里卡片高 903）。
        // 后果不只是卡片虚胖：第 2、3 台设备被压成 0 高度，内容总高又不超出视口，
        // 连滚动条都不出现 —— 发现到 3 台设备却只看得到 1 台。
        // `ui.horizontal()` / `Align::Min` 走的是有界路径，不会有这个问题。
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            // 右侧操作区先占位：否则长设备名会把按钮挤出卡片
            if peer.accepting {
                if ui.add(theme::primary_button("控制")).clicked() {
                    action = PeerAction::Connect;
                }
                if ui.add(theme::ghost_button("仅观看")).clicked() {
                    action = PeerAction::ViewOnly;
                }
            } else {
                theme::pill(ui, "已关闭被控", theme::TEXT_DIM);
            }

            // 左侧信息区：第一行「状态点 + 设备名」，第二行详情（与名称左对齐）
            let name_color = if peer.accepting { theme::TEXT } else { theme::TEXT_DIM };
            let dot_color = if peer.accepting { theme::SUCCESS } else { theme::TEXT_FAINT };
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    theme::dot_glow(ui, dot_color, 4.0);
                    ui.add_space(4.0);
                    // truncate：设备名可能很长，宁可截断也不要换行把卡片撑高
                    ui.add(
                        egui::Label::new(
                            RichText::new(peer.name.as_str()).size(14.5).color(name_color).strong(),
                        )
                        .truncate(),
                    );
                });
                ui.horizontal(|ui| {
                    // 与名称左对齐（= 状态点宽度 16 + 间距 4）
                    ui.add_space(20.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "{} · {} · {}",
                                peer.platform,
                                peer.addr.ip(),
                                peer.device_id
                            ))
                            .size(11.5)
                            .color(theme::TEXT_FAINT),
                        )
                        .truncate(),
                    );
                });
            });
        });
    });

    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Peer;

    fn peer(name: &str, accepting: bool) -> Peer {
        Peer {
            device_id: "1DE5-883A".into(),
            name: name.into(),
            platform: "Windows".into(),
            version: "0.1.11".into(),
            addr: "192.168.0.20:48500".parse().unwrap(),
            accepting,
            last_seen: Instant::now(),
        }
    }

    /// 在指定视口里渲染一张设备卡片，返回它实际占用的高度（逻辑像素）。
    ///
    /// 用 egui 的无头 Context 直接跑布局，不需要窗口，因此可以在单测里
    /// 精确量出高度 —— 截图只能看出「不对劲」，这个能给出数字。
    /// 跑一帧无头布局，把闭包的返回值带出来。
    ///
    /// 必须清理 `textures_delta`：epaint 在 drop 未处理的纹理增量时会 panic，
    /// 无头测试没有后端来消费它。
    fn run_frame<R>(viewport: egui::Vec2, f: impl FnOnce(&mut Ui) -> R) -> R {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, viewport)),
            ..Default::default()
        };
        let mut f = Some(f);
        let mut out = None;
        let mut output = ctx.run_ui(input, |ui| {
            if let Some(f) = f.take() {
                out = Some(f(ui));
            }
        });
        output.textures_delta.clear();
        out.expect("闭包必须执行一次")
    }

    /// 渲染一张设备卡片，返回它实际占用的高度（逻辑像素）。
    ///
    /// 用 egui 的无头 Context 直接跑布局，不需要窗口，因此可以在单测里
    /// 精确量出高度 —— 截图只能看出「不对劲」，这个能给出数字。
    fn card_height(viewport: egui::Vec2, name: &str, accepting: bool) -> f32 {
        run_frame(viewport, |ui| {
            let top = ui.cursor().top();
            let _ = draw_peer_card(ui, &peer(name, accepting));
            ui.cursor().top() - top
        })
    }

    /// 卡片高度必须只由内容决定，不能随列表视口高度变化。
    ///
    /// 回归用例：旧布局下卡片会被拉伸到视口高度（420×900 的视口里高度
    /// 接近 900），导致同一屏只能看到一台设备、其余被压成 0 高度。
    #[test]
    fn test_peer_card_height_is_content_driven() {
        let tight = card_height(egui::vec2(420.0, 120.0), "DESKTOP-2RRTIEP", true);
        let roomy = card_height(egui::vec2(420.0, 900.0), "DESKTOP-2RRTIEP", true);
        assert!(
            (tight - roomy).abs() < 1.0,
            "卡片高度不能随视口变化：紧凑视口 {tight:.1}，宽裕视口 {roomy:.1}"
        );
        assert!(roomy < 80.0, "单行设备卡片应低于 80px，实际 {roomy:.1}");
    }

    /// 超长设备名不能把卡片撑高（必须走截断，而不是换行）。
    #[test]
    fn test_peer_card_survives_long_name() {
        let long = "QIXUN-WORKSTATION-INTEL-XEON-一个特别特别长的设备名称测试用例";
        let short = card_height(egui::vec2(420.0, 400.0), "DESK", true);
        let tall = card_height(egui::vec2(420.0, 400.0), long, true);
        assert!(
            (tall - short).abs() < 1.0,
            "长名称不应改变卡片高度：短名 {short:.1}，长名 {tall:.1}"
        );
    }

    /// 关闭被控的卡片同样保持单行高度，并给出明确状态。
    #[test]
    fn test_peer_card_not_accepting_is_also_compact() {
        let on = card_height(egui::vec2(420.0, 400.0), "DESKTOP-2RRTIEP", true);
        let off = card_height(egui::vec2(420.0, 400.0), "DESKTOP-2RRTIEP", false);
        assert!(off < 80.0, "不可控状态也应是单行，实际 {off:.1}");
        assert!((on - off).abs() < 20.0, "两种状态高度不应差太多");
    }


    /// 窄窗口下也不能把内容挤到视口外（卡片宽度应受视口约束）。
    #[test]
    fn test_peer_card_fits_narrow_viewport() {
        let width = run_frame(egui::vec2(360.0, 400.0), |ui| {
            let _ = draw_peer_card(ui, &peer("DESKTOP-2RRTIEP", true));
            ui.min_rect().width()
        });
        assert!(width <= 360.0, "卡片不应超出窗口宽度，实际 {width:.1}");
    }
}
