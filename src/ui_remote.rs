//! 远控视口：显示远端画面，捕获并转发键鼠输入。

use crate::client::{OutMsg, RemoteSession};
use crate::keys;
use crate::theme;
use egui::{
    Align2, Color32, Event, FontId, Key, Modifiers, PointerButton, Pos2, Rect, Sense, TextureOptions,
    Vec2, ViewportCommand,
};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

pub fn show(ui: &mut egui::Ui, session: &Arc<RemoteSession>) {
    // ---- 工具栏 ----
    egui::Panel::top("remote_toolbar")
        .frame(
            egui::Frame::new()
                .fill(theme::PANEL)
                .inner_margin(egui::Margin::symmetric(10, 7))
                .stroke(egui::Stroke::new(1.0, theme::BORDER)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.add(theme::danger_button("断开")).clicked() {
                    session.disconnect("用户断开");
                }

                ui.add(egui::Separator::default().vertical());

                let fs = session.fullscreen.load(Ordering::Relaxed);
                if ui
                    .add(theme::tool_button(if fs { "退出全屏" } else { "全屏" }, true))
                    .clicked()
                {
                    let new_fs = !fs;
                    session.fullscreen.store(new_fs, Ordering::Relaxed);
                    ui.ctx().send_viewport_cmd(ViewportCommand::Fullscreen(new_fs));
                }

                let mut vo = session.view_only.load(Ordering::Relaxed);
                let vo_resp = ui.add(theme::tool_button(
                    if vo { "仅观看 ✓" } else { "仅观看" },
                    true,
                ));
                if vo_resp.clicked() {
                    vo = !vo;
                    session.view_only.store(vo, Ordering::Relaxed);
                    set_toast(
                        session,
                        if vo { "已切换到仅观看" } else { "已开启键鼠控制" },
                    );
                }

                ui.add(egui::Separator::default().vertical());

                if ui.add(theme::tool_button("发送剪贴板", true)).clicked() {
                    if session.send_clipboard() {
                        set_toast(session, "已发送本地剪贴板");
                    } else {
                        set_toast(session, "本地剪贴板没有文本内容");
                    }
                }

                // 右侧状态区
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let stats = session.stats.lock().unwrap().clone();
                    ui.add_space(4.0);
                    stat_chip(
                        ui,
                        &format!("{:.0} fps", stats.fps),
                        fps_color(stats.fps),
                    );
                    stat_chip(ui, &format!("{:.0} ms", stats.rtt_ms), rtt_color(stats.rtt_ms));
                    if stats.frame_w > 0 {
                        stat_chip(
                            ui,
                            &format!("{}×{}", stats.frame_w, stats.frame_h),
                            theme::TEXT_DIM,
                        );
                    }
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(&session.peer_name)
                            .size(13.5)
                            .color(theme::TEXT)
                            .strong(),
                    );
                    theme::dot_glow(ui, theme::ACCENT, 4.0);
                });
            });
        });

    // ---- 画面区域 ----
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(Color32::from_rgb(8, 9, 12)))
        .show(ui, |ui| {
            let frame = session.frame.lock().unwrap().clone();
            let Some(img) = frame else {
                theme::spinner_text(ui, "正在连接远端画面…");
                return;
            };

            let avail = ui.available_rect_before_wrap();
            let (iw, ih) = (img.width() as f32, img.height() as f32);
            let scale = (avail.width() / iw).min(avail.height() / ih).max(0.0001);
            let size = Vec2::new(iw * scale, ih * scale);
            let rect = Rect::from_center_size(avail.center(), size);

            // 纹理更新
            {
                let mut st = session.ui_state.lock().unwrap();
                let tex = st.texture.get_or_insert_with(|| {
                    ui.ctx().load_texture("remote", (*img).clone(), TextureOptions::LINEAR)
                });
                if session.frame_dirty.swap(false, Ordering::Relaxed) {
                    tex.set((*img).clone(), TextureOptions::LINEAR);
                }
                let tex_id = tex.id();
                ui.painter().image(
                    tex_id,
                    rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }

            // 画面外框，避免黑背景下边界不清晰
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(1.0, theme::BORDER_STRONG),
                egui::StrokeKind::Outside,
            );

            let response = ui.allocate_rect(rect, Sense::click_and_drag());
            if response.hovered() {
                // 用系统箭头光标代表远端鼠标：本地 OS 渲染，零延迟、无重影
                ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
            }

            handle_input(ui, session, rect);
        });

    // ---- 仅观看角标 ----
    if session.view_only.load(Ordering::Relaxed) && !session.closed.load(Ordering::Relaxed) {
        let painter = ui.painter();
        let pos = Pos2::new(ui.clip_rect().right() - 74.0, ui.clip_rect().top() + 50.0);
        painter.rect_filled(
            Rect::from_center_size(pos, Vec2::new(132.0, 26.0)),
            theme::R_PILL,
            Color32::from_rgba_unmultiplied(20, 23, 29, 235),
        );
        painter.text(
            pos,
            Align2::CENTER_CENTER,
            "仅观看 · 不转发键鼠",
            FontId::proportional(12.0),
            theme::ACCENT,
        );
    }

    // ---- 提示浮层 ----
    let toast = session.ui_state.lock().unwrap().toast.clone();
    if let Some((at, text)) = toast {
        if at.elapsed() < std::time::Duration::from_secs(3) {
            let painter = ui.painter();
            let pos = Pos2::new(ui.clip_rect().center().x, ui.clip_rect().bottom() - 40.0);
            painter.rect_filled(
                Rect::from_center_size(pos, Vec2::new(280.0, 32.0)),
                theme::R_MD,
                Color32::from_rgba_unmultiplied(27, 31, 39, 240),
            );
            painter.text(
                pos,
                Align2::CENTER_CENTER,
                text,
                FontId::proportional(13.0),
                theme::TEXT,
            );
        }
    }
}

/// 状态药丸（fps / RTT / 分辨率共用）。
fn stat_chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), FontId::proportional(11.5), color);
    let size = galley.size() + Vec2::new(14.0, 7.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter()
        .rect_filled(rect, theme::R_PILL, theme::tint(color, 44));
    ui.painter().galley(
        egui::pos2(rect.min.x + 7.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
}

fn fps_color(fps: f32) -> Color32 {
    if fps >= 20.0 {
        theme::SUCCESS
    } else if fps >= 10.0 {
        theme::WARN
    } else {
        theme::DANGER
    }
}

fn rtt_color(ms: f32) -> Color32 {
    if ms <= 30.0 {
        theme::SUCCESS
    } else if ms <= 100.0 {
        theme::WARN
    } else {
        theme::DANGER
    }
}

fn set_toast(session: &RemoteSession, text: &str) {
    session.ui_state.lock().unwrap().toast = Some((Instant::now(), text.to_string()));
}

fn handle_input(ui: &mut egui::Ui, session: &Arc<RemoteSession>, rect: Rect) {
    let view_only = session.view_only.load(Ordering::Relaxed);
    let closed = session.closed.load(Ordering::Relaxed);
    if view_only || closed {
        return;
    }

    let events = ui.ctx().input(|i| i.events.clone());
    let mods = ui.ctx().input(|i| i.modifiers);

    for ev in events {
        match ev {
            Event::PointerMoved(p) => {
                let mut st = session.ui_state.lock().unwrap();
                let dragging = st.buttons_down.iter().any(|&b| b);
                if rect.contains(p) || dragging {
                    let nx = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                    let ny = ((p.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                    // 节流：位置变化明显才发送
                    let should_send = match st.last_sent {
                        Some((lx, ly)) => (nx - lx).abs() > 0.0008 || (ny - ly).abs() > 0.0008,
                        None => true,
                    };
                    if should_send {
                        st.last_sent = Some((nx, ny));
                        drop(st);
                        send_mouse(session, nx, ny, 0, 0);
                    }
                }
            }
            Event::PointerButton { pos, button, pressed, .. } => {
                if !rect.contains(pos) {
                    continue;
                }
                let idx = match button {
                    PointerButton::Primary => 0,
                    PointerButton::Secondary => 1,
                    PointerButton::Middle => 2,
                    PointerButton::Extra1 => 3,
                    PointerButton::Extra2 => 4,
                };
                let action = if pressed { 1 } else { 2 };
                let nx = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                let ny = ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                {
                    let mut st = session.ui_state.lock().unwrap();
                    if idx < 3 {
                        st.buttons_down[idx] = pressed;
                    }
                    if !pressed {
                        st.buttons_down = [false, false, false];
                    }
                }
                send_mouse(session, nx, ny, idx.min(2) as u8, action);
            }
            Event::MouseWheel { delta, .. } => {
                let pos = ui.input(|i| i.pointer.latest_pos());
                let inside = pos.map(|p| rect.contains(p)).unwrap_or(false);
                if !inside {
                    continue;
                }
                let (dx, dy) = {
                    let mut st = session.ui_state.lock().unwrap();
                    st.wheel_acc.0 -= delta.x / 40.0;
                    st.wheel_acc.1 -= delta.y / 40.0; // egui 向上为正 → enigo 向下为正
                    let dx = if st.wheel_acc.0.abs() >= 1.0 {
                        let v = st.wheel_acc.0.trunc() as i32;
                        st.wheel_acc.0 -= v as f32;
                        v
                    } else {
                        0
                    };
                    let dy = if st.wheel_acc.1.abs() >= 1.0 {
                        let v = st.wheel_acc.1.trunc() as i32;
                        st.wheel_acc.1 -= v as f32;
                        v
                    } else {
                        0
                    };
                    (dx, dy)
                };
                if dx != 0 || dy != 0 {
                    let _ =
                        session.input_tx.send(OutMsg::Wheel(crate::protocol::WheelMsg { dx, dy }));
                }
            }
            Event::Text(t) => {
                for ch in t.chars() {
                    if ch.is_control() {
                        // Ctrl+字母（部分平台以控制字符出现）
                        let code = ch as u32;
                        if (1..=26).contains(&code) {
                            let letter = (b'a' + (code as u8 - 1)) as char;
                            send_key(session, &letter.to_string(), true);
                            send_key(session, &letter.to_string(), false);
                        }
                        continue;
                    }
                    if ch == ' ' {
                        continue; // 空格走 Event::Key{Space}，避免双发
                    }
                    // 大写锁定归一化：字符是大写但本地没按 Shift，说明来自
                    // Caps Lock。直接转发大写字母会让被控端（尤其 macOS 拼音
                    // 输入法不识别大写字母）无法组句。转小写发送；
                    // 要真正的大写请用 Shift+字母（此时 shift 已另行转发）。
                    let ch = if ch.is_alphabetic() && ch.is_uppercase() && !mods.shift {
                        ch.to_lowercase().next().unwrap_or(ch)
                    } else {
                        ch
                    };
                    let s = ch.to_string();
                    send_key(session, &s, true);
                    send_key(session, &s, false);
                }
            }
            Event::Key { key, pressed, .. } => {
                // 复制/粘贴/剪切被 egui 拦截成合成事件，还原为字母键
                //（修饰键状态已由 ModifiersChanged 同步到被控端）。
                match key {
                    Key::Copy | Key::Cut | Key::Paste => {
                        let ch = match key {
                            Key::Copy => 'c',
                            Key::Cut => 'x',
                            _ => 'v',
                        };
                        send_key(session, &ch.to_string(), true);
                        send_key(session, &ch.to_string(), false);
                        continue;
                    }
                    _ => {}
                }
                if let Some(name) = keys::key_to_name(key) {
                    if keys::is_modifier_name(name) {
                        continue; // 修饰键统一走 ModifiersChanged
                    }
                    // 数字/符号键会同时触发 Event::Text（真实字符），
                    // 由 Text 路径注入即可；这里跳过，防止一次按键重复注入。
                    if keys::name_to_char(name).is_some() {
                        continue;
                    }
                    send_key(session, name, pressed);
                }
            }
            Event::Ime(egui::ImeEvent::Commit(text)) => {
                // 本地输入法（中文/日文等）组合完成：逐字符经现有 KeyMsg
                // 通道注入被控端（被控端 v0.1.2+ 对单字符走 fast_text，
                // 线程安全且支持任意 Unicode）。不改协议，兼容旧版对端。
                for ch in text.chars() {
                    if ch == ' ' {
                        send_key(session, "space", true);
                        send_key(session, "space", false);
                    } else {
                        send_key(session, &ch.to_string(), true);
                        send_key(session, &ch.to_string(), false);
                    }
                }
            }
            Event::ModifiersChanged(m) => {
                sync_mods(session, &m);
            }
            _ => {}
        }
    }
    // 失焦兜底：本窗口失焦时不会收到修饰键抬起事件，主动释放，
    // 否则被控端会残留 Shift/Ctrl 卡键（大写卡死、组合键错乱）。
    let focused = ui.ctx().input(|i| i.viewport().focused.unwrap_or(true));
    if !focused {
        sync_mods(session, &Modifiers::NONE);
    }

    // 声明 IME 激活（锚定在画面区）。eframe 依据 output.ime 是否为 Some
    // 来调用窗口 set_ime_allowed：不声明则系统在「无文本框」的窗口里
    // 直接屏蔽输入法切换（Ctrl+空格/Win+空格失效），无法输入中文。
    ui.ctx().output_mut(|o| {
        o.ime = Some(egui::output::IMEOutput {
            purpose: egui::IMEPurpose::Normal,
            rect,
            cursor_rect: Rect::from_min_size(rect.center(), Vec2::new(1.0, 16.0)),
            should_interrupt_composition: false,
        })
    });
}

/// 按当前修饰键状态同步到被控端：按下发 down、松开发 up（内部去重）。
/// 此前版本有 bug：释放时误发了 down，导致被控端修饰键永久卡住。
fn sync_mods(session: &Arc<RemoteSession>, m: &Modifiers) {
    let want = [m.ctrl, m.alt, m.shift, m.mac_cmd || m.command];
    let names = ["ctrl", "alt", "shift", "super"];
    let mut st = session.ui_state.lock().unwrap();
    for i in 0..4 {
        if want[i] == st.last_mods[i] {
            continue;
        }
        st.last_mods[i] = want[i];
        let down = want[i];
        drop(st);
        let _ = session
            .input_tx
            .send(OutMsg::Key(crate::protocol::KeyMsg { key: names[i].into(), down }));
        st = session.ui_state.lock().unwrap();
    }
}

fn send_key(session: &Arc<RemoteSession>, name: &str, down: bool) {
    let _ = session
        .input_tx
        .send(OutMsg::Key(crate::protocol::KeyMsg { key: name.to_string(), down }));
}

fn send_mouse(session: &Arc<RemoteSession>, x: f32, y: f32, button: u8, action: u8) {
    let _ = session
        .input_tx
        .send(OutMsg::Mouse(crate::protocol::MouseMsg { x, y, button, action }));
}
