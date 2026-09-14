//! 被控端输入执行：把协议消息映射为 enigo 键鼠注入。

use crate::keys;
use crate::protocol::{KeyMsg, MouseMsg, WheelMsg};
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings, Axis};
use std::collections::HashSet;

pub struct InputExecutor {
    enigo: Enigo,
    /// 已按下未释放的修饰键（协议名）
    pressed_mods: HashSet<&'static str>,
    /// 显示范围（与 move_mouse Abs 坐标同空间）
    extent: (i32, i32),
    /// 被控端为 macOS 时，把 Ctrl 映射为 Command（Windows 主控的习惯）
    ctrl_as_cmd: bool,
}

impl InputExecutor {
    pub fn new(ctrl_as_cmd: bool) -> Result<Self, String> {
        let enigo = Enigo::new(&Settings {
            release_keys_when_dropped: true,
            ..Settings::default()
        })
        .map_err(|e| match e {
            enigo::NewConError::NoPermission => {
                "本机缺少「辅助功能」权限，无法注入键鼠。请到 系统设置 > 隐私与安全性 > 辅助功能 中授权。".to_string()
            }
            other => format!("初始化输入组件失败: {other}"),
        })?;
        let extent = enigo
            .main_display()
            .map_err(|e| format!("获取屏幕尺寸失败: {e}"))?;
        log::info!("输入执行器就绪，屏幕范围 {}x{}", extent.0, extent.1);
        Ok(Self { enigo, pressed_mods: HashSet::new(), extent, ctrl_as_cmd })
    }

    pub fn handle_mouse(&mut self, m: &MouseMsg) -> Result<(), String> {
        match m.action {
            0 => {
                // 移动：归一化坐标 → 显示范围
                let x = (m.x.clamp(0.0, 1.0) * self.extent.0 as f32).round() as i32;
                let y = (m.y.clamp(0.0, 1.0) * self.extent.1 as f32).round() as i32;
                self.enigo.move_mouse(x, y, Coordinate::Abs).map_err(es)?;
            }
            1 | 2 => {
                let button = match m.button {
                    1 => Button::Right,
                    2 => Button::Middle,
                    _ => Button::Left,
                };
                let dir = if m.action == 1 { Direction::Press } else { Direction::Release };
                self.enigo.button(button, dir).map_err(es)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn handle_wheel(&mut self, w: &WheelMsg) -> Result<(), String> {
        if w.dy != 0 {
            self.enigo.scroll(w.dy, Axis::Vertical).map_err(es)?;
        }
        if w.dx != 0 {
            self.enigo.scroll(w.dx, Axis::Horizontal).map_err(es)?;
        }
        Ok(())
    }

    pub fn handle_key(&mut self, k: &KeyMsg) -> Result<(), String> {
        let name = k.key.as_str();
        if keys::is_modifier_name(name) {
            let name = self.map_mod(name);
            if k.down {
                if self.pressed_mods.insert(name) {
                    self.enigo.key(mod_key(name), Direction::Press).map_err(es)?;
                }
            } else if self.pressed_mods.remove(name) {
                self.enigo.key(mod_key(name), Direction::Release).map_err(es)?;
            }
            return Ok(());
        }

        let key = if let Some(c) = keys::name_to_char(name) {
            Key::Unicode(c)
        } else {
            match name_to_enigo_key(name) {
                Some(k) => k,
                None => {
                    log::debug!("忽略未知按键名: {name}");
                    return Ok(());
                }
            }
        };
        let dir = if k.down { Direction::Press } else { Direction::Release };
        self.enigo.key(key, dir).map_err(es)
    }

    fn map_mod(&self, name: &str) -> &'static str {
        if self.ctrl_as_cmd && name == keys::MOD_CTRL {
            keys::MOD_SUPER
        } else {
            match name {
                keys::MOD_CTRL => keys::MOD_CTRL,
                keys::MOD_ALT => keys::MOD_ALT,
                keys::MOD_SHIFT => keys::MOD_SHIFT,
                _ => keys::MOD_SUPER,
            }
        }
    }

    pub fn release_all(&mut self) {
        for name in self.pressed_mods.drain() {
            let _ = self.enigo.key(mod_key(name), Direction::Release);
        }
    }
}

fn mod_key(name: &str) -> Key {
    match name {
        keys::MOD_ALT => Key::Alt,
        keys::MOD_SHIFT => Key::Shift,
        keys::MOD_SUPER => Key::Meta,
        _ => Key::Control,
    }
}

fn name_to_enigo_key(name: &str) -> Option<Key> {
    let k = match name {
        "enter" => Key::Return,
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "escape" => Key::Escape,
        "space" => Key::Space,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "delete" => Key::Delete,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        "f13" => Key::F13,
        "f14" => Key::F14,
        "f15" => Key::F15,
        "f16" => Key::F16,
        "f17" => Key::F17,
        "f18" => Key::F18,
        "f19" => Key::F19,
        "f20" => Key::F20,
        _ => return None,
    };
    Some(k)
}

fn es(e: enigo::InputError) -> String {
    format!("注入输入失败: {e:?}")
}
