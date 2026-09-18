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
    /// 滚动换算的余数（单位：行）。Windows 的滚动粒度是整「档」，
    /// 不足一档的部分必须留着，否则小幅滚动会被直接吞掉。
    wheel_rem: (f32, f32), // (水平, 垂直)
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
        Ok(Self {
            enigo,
            pressed_mods: HashSet::new(),
            extent,
            ctrl_as_cmd,
            wheel_rem: (0.0, 0.0),
        })
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

    /// 协议里 dx/dy 的单位统一是「行」（见 ui_remote 的滚轮归一化）。
    pub fn handle_wheel(&mut self, w: &WheelMsg) -> Result<(), String> {
        self.scroll_lines(w.dy as f32, Axis::Vertical)?;
        self.scroll_lines(w.dx as f32, Axis::Horizontal)?;
        Ok(())
    }

    fn scroll_lines(&mut self, lines: f32, axis: Axis) -> Result<(), String> {
        if lines == 0.0 {
            return Ok(());
        }
        let vertical = matches!(axis, Axis::Vertical);
        let rem = if vertical { &mut self.wheel_rem.1 } else { &mut self.wheel_rem.0 };

        // enigo 各平台 scroll() 的单位并不一致：
        //   · macOS   —— ScrollEventUnit::LINE，传入的就是「行」
        //   · Windows —— mouse_event 的 WHEEL_DELTA 倍数，单位是「档(notch)」，
        //                一档约 3 行，且最小粒度就是一档（滚不了半档）
        // 所以 Windows 上必须把行换算成档，否则滚动量会被放大约 3 倍；
        // 同时用余数保留不足一档的部分，避免轻扫一下完全没反应。
        let amount: i32 = if cfg!(target_os = "windows") {
            const LINES_PER_NOTCH: f32 = 3.0;
            let total = (*rem + lines) / LINES_PER_NOTCH;
            let notches = total.trunc();
            *rem = (total - notches) * LINES_PER_NOTCH;
            notches as i32
        } else {
            lines.round() as i32
        };

        if amount != 0 {
            self.enigo.scroll(amount, axis).map_err(es)?;
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

        if let Some(c) = keys::name_to_char(name) {
            // 文本字符注入。macOS 上 Key::Unicode 会调用 TIS/TSM 键盘布局接口
            // （强制主线程，在会话线程调用直接 SIGTRAP 崩溃），因此全部改走
            // 线程安全的 CGEvent 路径：
            //   · 无修饰键 → fast_text（SetUnicodeString，支持大小写/中文/任意布局）
            //   · 按住修饰键 → raw(US 物理键码)，enigo 的 event_flags 自动携带
            //     已按住的 Command/Shift 等，Command+C 等快捷键正常
            // fast_text 注入是原子的：只处理按下，忽略抬起。
            if cfg!(target_os = "macos") {
                if self.pressed_mods.is_empty() {
                    if k.down {
                        return match self.enigo.fast_text(&c.to_string()).map_err(es)? {
                            Some(()) => Ok(()),
                            None => Err("文本注入不可用".into()),
                        };
                    }
                    return Ok(());
                }
                return match char_to_mac_keycode(c) {
                    Some(code) => {
                        let dir = if k.down { Direction::Press } else { Direction::Release };
                        self.enigo.raw(code, dir).map_err(es)
                    }
                    None => {
                        if k.down {
                            let _ = self.enigo.fast_text(&c.to_string());
                        }
                        Ok(())
                    }
                };
            }
            let dir = if k.down { Direction::Press } else { Direction::Release };
            return self.enigo.key(Key::Unicode(c), dir).map_err(es);
        }

        let key = match name_to_enigo_key(name) {
            Some(k) => k,
            None => {
                log::debug!("忽略未知按键名: {name}");
                return Ok(());
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

/// macOS 虚拟键码（Apple US ANSI 布局）。仅在按住修饰键时用于快捷键注入：
/// 此路径不查询键盘布局（无 TIS 调用），符号依赖 US 布局物理位置。
#[cfg(target_os = "macos")]
fn char_to_mac_keycode(c: char) -> Option<u16> {
    let code: u16 = match c.to_ascii_lowercase() {
        'a' => 0x00, 's' => 0x01, 'd' => 0x02, 'f' => 0x03,
        'h' => 0x04, 'g' => 0x05, 'z' => 0x06, 'x' => 0x07,
        'c' => 0x08, 'v' => 0x09, 'b' => 0x0B, 'q' => 0x0C,
        'w' => 0x0D, 'e' => 0x0E, 'r' => 0x0F, 'y' => 0x10,
        't' => 0x11, '1' => 0x12, '2' => 0x13, '3' => 0x14,
        '4' => 0x15, '6' => 0x16, '5' => 0x17, '=' => 0x18,
        '9' => 0x19, '7' => 0x1A, '-' => 0x1B, '8' => 0x1C,
        '0' => 0x1D, ']' => 0x1E, 'o' => 0x1F, 'u' => 0x20,
        '[' => 0x21, 'i' => 0x22, 'p' => 0x23, 'l' => 0x25,
        'j' => 0x26, '\'' => 0x27, 'k' => 0x28, ';' => 0x29,
        '\\' => 0x2A, ',' => 0x2B, '/' => 0x2C, 'n' => 0x2D,
        'm' => 0x2E, '.' => 0x2F, '`' => 0x32,
        ' ' => 0x31, '\n' => 0x24, '\t' => 0x30,
        _ => return None,
    };
    Some(code)
}

#[cfg(not(target_os = "macos"))]
fn char_to_mac_keycode(_c: char) -> Option<u16> {
    None
}
