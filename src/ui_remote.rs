//! 远控视口：显示远端画面，捕获并转发键鼠输入。

use crate::client::{OutMsg, RemoteSession};
use crate::keys;
use egui::{
    Align2, Color32, Event, FontId, Key, Modifiers, PointerButton, Pos2, Rect, Sense, TextureOptions,
    Vec2, ViewportCommand,
};
use std::sync::Arc;
use std::time::Instant;

pub fn show(ui: &mut egui::Ui, session: &Arc<RemoteSession>) {
    // ---- 工具栏 ----
    egui::Panel::top("remote_toolbar").show(ui, |ui| {
        ui.horizontal(|ui| {
            if ui.button("断开").clicked() {
                session.disconnect("用户断开");
            }
            ui.separator();
            let fs = session.fullscreen.load(std::sync::atomic::Ordering::Relaxed);
            if ui.button(if fs { "退出全屏" } else { "全屏" }).clicked() {
                let new_fs = !fs;
                session.fullscreen.store(new_fs, std::sync::atomic::Ordering::Relaxed);
                ui.ctx().send_viewport_cmd(ViewportCommand::Fullscreen(new_fs));
            }
            ui.separator();
            let mut vo = session.view_only.load(std::sync::atomic::Ordering::Relaxed);
            if ui.checkbox(&mut vo, "仅观看").changed() {
                session.view_only.store(vo, std::sync::atomic::Ordering::Relaxed);
            }
            if ui.button("发送剪贴板").clicked() {
                if !session.send_clipboard() {
                    set_toast(session, "本地剪贴板没有文本内容");
                } else {
                    set_toast(session, "已发送本地剪贴板");
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let stats = session.stats.lock().unwrap().clone();
                ui.colored_label(
                    Color32::from_gray(150),
                    format!(
                        "{:.0} fps · {:.0} ms · {}x{}",
                        stats.fps, stats.rtt_ms, stats.frame_w, stats.frame_h
                    ),
                );
                ui.label(&session.peer_name);
            });
        });
    });

    // ---- 画面区域 ----
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(Color32::BLACK))
        .show(ui, |ui| {
            let frame = session.frame.lock().unwrap().clone();
            let Some(img) = frame else {
                ui.centered_and_justified(|ui| {
                    ui.colored_label(Color32::from_gray(140), "正在等待远端画面…");
                });
                return;
            };

            let avail = ui.available_rect_before_wrap();
            let (iw, ih) = (img.width() as f32, img.height() as f32);
            let scale = (avail.width() / iw).min(avail.height() / ih).max(0.0001);
            let size = Vec2::new(iw * scale, ih * scale);
            let rect = Rect::from_center_size(avail.center(), size);

            // 纹理更新
            let (tex_id, tex_size) = {
                let mut st = session.ui_state.lock().unwrap();
                let tex = st.texture.get_or_insert_with(|| {
                    ui.ctx().load_texture("remote", (*img).clone(), TextureOptions::LINEAR)
                });
                if session.frame_dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    tex.set((*img).clone(), TextureOptions::LINEAR);
                }
                (tex.id(), tex.size_vec2())
            };

            ui.painter().image(
                tex_id,
                rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );

            let response = ui.allocate_rect(rect, Sense::click_and_drag());
            if response.hovered() {
                // 用系统箭头光标代表远端鼠标：本地 OS 渲染，零延迟、无重影
                ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
            }

            handle_input(ui, session, rect, tex_size);
        });

    // ---- 提示浮层 ----
    let toast = session.ui_state.lock().unwrap().toast.clone();
    if let Some((at, text)) = toast {
        if at.elapsed() < std::time::Duration::from_secs(3) {
            let painter = ui.painter();
            let pos = Pos2::new(ui.clip_rect().center().x, ui.clip_rect().bottom() - 36.0);
            painter.rect_filled(
                Rect::from_center_size(pos, Vec2::new(260.0, 30.0)),
                6.0,
                Color32::from_rgba_unmultiplied(30, 30, 30, 220),
            );
            painter.text(
                pos,
                Align2::CENTER_CENTER,
                text,
                FontId::proportional(13.0),
                Color32::WHITE,
            );
        }
    }
}

fn set_toast(session: &RemoteSession, text: &str) {
    session.ui_state.lock().unwrap().toast = Some((Instant::now(), text.to_string()));
}

fn handle_input(ui: &mut egui::Ui, session: &Arc<RemoteSession>, rect: Rect, _tex_size: Vec2) {
    let view_only = session.view_only.load(std::sync::atomic::Ordering::Relaxed);
    let closed = session.closed.load(std::sync::atomic::Ordering::Relaxed);
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
                    let _ = session.input_tx.send(OutMsg::Wheel(crate::protocol::WheelMsg { dx, dy }));
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
                    let s = ch.to_string();
                    send_key(session, &s, true);
                    send_key(session, &s, false);
                }
            }
            Event::Key { key, pressed, modifiers, .. } => {
                // 复制/粘贴/剪切被 egui 拦截，这里还原为快捷键
                match key {
                    Key::Copy | Key::Cut | Key::Paste => {
                        let ch = match key {
                            Key::Copy => 'c',
                            Key::Cut => 'x',
                            _ => 'v',
                        };
                        send_mods(session, &modifiers, true);
                        send_key(session, &ch.to_string(), true);
                        send_key(session, &ch.to_string(), false);
                        send_mods(session, &modifiers, false);
                        continue;
                    }
                    _ => {}
                }
                if let Some(name) = keys::key_to_name(key) {
                    if keys::is_modifier_name(name) {
                        continue; // 修饰键统一走 ModifiersChanged
                    }
                    send_key(session, name, pressed);
                }
                let _ = modifiers;
            }
            Event::ModifiersChanged(m) => {
                send_mods(session, &m, true);
            }
            _ => {}
        }
    }
    let _ = mods;
}

/// 按当前修饰键状态发送按下/释放（内部去重）。
fn send_mods(session: &Arc<RemoteSession>, m: &Modifiers, down: bool) {
    let want = [
        m.ctrl,
        m.alt,
        m.shift,
        m.mac_cmd || (m.command && !m.ctrl && !cfg!(target_os = "macos")),
    ];
    let names = ["ctrl", "alt", "shift", "super"];
    let mut st = session.ui_state.lock().unwrap();
    for i in 0..4 {
        if want[i] != st.last_mods[i] {
            st.last_mods[i] = want[i];
            drop(st);
            let _ = session
                .input_tx
                .send(OutMsg::Key(crate::protocol::KeyMsg { key: names[i].into(), down }));
            st = session.ui_state.lock().unwrap();
        }
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
