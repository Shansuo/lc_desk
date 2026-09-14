//! 按键名映射：主控端 egui 按键 → 协议字符串 → 被控端 enigo。

use egui::Key;

/// 协议按键名（不含单字符键，单字符直接用字符本身）：
/// 修饰键: "ctrl" "alt" "shift" "super"
/// 特殊键: enter backspace tab escape space up down left right home end
///         pageup pagedown delete insert f1..f20
/// 符号/数字: colon comma backslash slash pipe questionmark exclamationmark
///         openbracket closebracket opencurlybracket closecurlybracket
///         backtick minus period plus equals semicolon quote num0..num9
pub const MOD_CTRL: &str = "ctrl";
pub const MOD_ALT: &str = "alt";
pub const MOD_SHIFT: &str = "shift";
pub const MOD_SUPER: &str = "super";

pub fn is_modifier_name(name: &str) -> bool {
    matches!(name, MOD_CTRL | MOD_ALT | MOD_SHIFT | MOD_SUPER)
}

/// egui 按键 → 协议名。None 表示该键不转发。
pub fn key_to_name(key: Key) -> Option<&'static str> {
    let s = match key {
        Key::Enter => "enter",
        Key::Backspace => "backspace",
        Key::Tab => "tab",
        Key::Escape => "escape",
        Key::Space => "space",
        Key::ArrowUp => "up",
        Key::ArrowDown => "down",
        Key::ArrowLeft => "left",
        Key::ArrowRight => "right",
        Key::Home => "home",
        Key::End => "end",
        Key::PageUp => "pageup",
        Key::PageDown => "pagedown",
        Key::Delete => "delete",
        Key::Insert => "insert",
        Key::F1 => "f1",
        Key::F2 => "f2",
        Key::F3 => "f3",
        Key::F4 => "f4",
        Key::F5 => "f5",
        Key::F6 => "f6",
        Key::F7 => "f7",
        Key::F8 => "f8",
        Key::F9 => "f9",
        Key::F10 => "f10",
        Key::F11 => "f11",
        Key::F12 => "f12",
        Key::F13 => "f13",
        Key::F14 => "f14",
        Key::F15 => "f15",
        Key::F16 => "f16",
        Key::F17 => "f17",
        Key::F18 => "f18",
        Key::F19 => "f19",
        Key::F20 => "f20",
        Key::Colon => "colon",
        Key::Comma => "comma",
        Key::Backslash => "backslash",
        Key::Slash => "slash",
        Key::Pipe => "pipe",
        Key::Questionmark => "questionmark",
        Key::Exclamationmark => "exclamationmark",
        Key::OpenBracket => "openbracket",
        Key::CloseBracket => "closebracket",
        Key::OpenCurlyBracket => "opencurlybracket",
        Key::CloseCurlyBracket => "closecurlybracket",
        Key::Backtick => "backtick",
        Key::Minus => "minus",
        Key::Period => "period",
        Key::Plus => "plus",
        Key::Equals => "equals",
        Key::Semicolon => "semicolon",
        Key::Quote => "quote",
        Key::Num0 => "num0",
        Key::Num1 => "num1",
        Key::Num2 => "num2",
        Key::Num3 => "num3",
        Key::Num4 => "num4",
        Key::Num5 => "num5",
        Key::Num6 => "num6",
        Key::Num7 => "num7",
        Key::Num8 => "num8",
        Key::Num9 => "num9",
        Key::ShiftLeft | Key::ShiftRight => MOD_SHIFT,
        Key::ControlLeft | Key::ControlRight => MOD_CTRL,
        Key::AltLeft | Key::AltRight => MOD_ALT,
        Key::SuperLeft | Key::SuperRight => MOD_SUPER,
        _ => return None,
    };
    Some(s)
}

/// 符号/数字协议名 → 字符（服务端用 Key::Unicode 注入）。
pub fn name_to_char(name: &str) -> Option<char> {
    let c = match name {
        "colon" => ':',
        "comma" => ',',
        "backslash" => '\\',
        "slash" => '/',
        "pipe" => '|',
        "questionmark" => '?',
        "exclamationmark" => '!',
        "openbracket" => '[',
        "closebracket" => ']',
        "opencurlybracket" => '{',
        "closecurlybracket" => '}',
        "backtick" => '`',
        "minus" => '-',
        "period" => '.',
        "plus" => '+',
        "equals" => '=',
        "semicolon" => ';',
        "quote" => '\'',
        "num0" => '0',
        "num1" => '1',
        "num2" => '2',
        "num3" => '3',
        "num4" => '4',
        "num5" => '5',
        "num6" => '6',
        "num7" => '7',
        "num8" => '8',
        "num9" => '9',
        _ => {
            // 单字符键直接透传
            let mut it = name.chars();
            if let (Some(c), None) = (it.next(), it.next()) {
                c
            } else {
                return None;
            }
        }
    };
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_names() {
        assert_eq!(key_to_name(Key::Enter), Some("enter"));
        assert_eq!(key_to_name(Key::ArrowUp), Some("up"));
        assert_eq!(key_to_name(Key::F12), Some("f12"));
        assert_eq!(key_to_name(Key::Num7), Some("num7"));
        assert!(key_to_name(Key::Copy).is_none());
    }

    #[test]
    fn test_name_to_char() {
        assert_eq!(name_to_char("colon"), Some(':'));
        assert_eq!(name_to_char("num0"), Some('0'));
        assert_eq!(name_to_char("a"), Some('a'));
        assert_eq!(name_to_char("界"), Some('界'));
        assert_eq!(name_to_char("enter"), None);
        assert_eq!(name_to_char("ab"), None);
    }

    #[test]
    fn test_modifiers() {
        assert!(is_modifier_name("ctrl"));
        assert!(!is_modifier_name("enter"));
    }
}
