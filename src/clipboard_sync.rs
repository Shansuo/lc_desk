//! 剪贴板文本读写（arboard 封装，失败静默）。

use std::sync::Mutex;
use std::sync::OnceLock;

static CLIPBOARD: OnceLock<Mutex<arboard::Clipboard>> = OnceLock::new();

fn board() -> Option<&'static Mutex<arboard::Clipboard>> {
    Some(CLIPBOARD.get_or_init(|| {
        Mutex::new(arboard::Clipboard::new().expect("clipboard init"))
    }))
}

pub fn get_text() -> Option<String> {
    let board = board()?;
    let mut guard = board.lock().ok()?;
    guard.get_text().ok()
}

pub fn set_text(text: &str) -> bool {
    let Some(board) = board() else { return false };
    let Ok(mut guard) = board.lock() else { return false };
    guard.set_text(text.to_string()).is_ok()
}

/// 应用远端推来的文本（内容相同则跳过，避免回环）。
pub fn apply_incoming(text: &str) {
    if let Some(local) = get_text() {
        if local == text {
            return;
        }
    }
    set_text(text);
}
