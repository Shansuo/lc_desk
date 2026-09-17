//! 剪贴板文本读写（arboard 封装，失败静默）。

use std::sync::Mutex;
use std::sync::OnceLock;

/// 剪贴板句柄。初始化可能失败（无显示服务 / 无剪贴板权限），
/// 因此存 Option 而不是 unwrap，访问失败时整体降级为空操作。
static CLIPBOARD: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();

fn with_board<R>(f: impl FnOnce(&mut arboard::Clipboard) -> R) -> Option<R> {
    let holder = CLIPBOARD.get_or_init(|| Mutex::new(arboard::Clipboard::new().ok()));
    let mut guard = holder.lock().ok()?;
    if guard.is_none() {
        // 首次失败后允许重试：剪贴板服务可能在运行期间才可用
        *guard = arboard::Clipboard::new().ok();
    }
    Some(f(guard.as_mut()?))
}

pub fn get_text() -> Option<String> {
    with_board(|b| b.get_text().ok())?
}

pub fn set_text(text: &str) -> bool {
    with_board(|b| b.set_text(text.to_string()).is_ok()).unwrap_or(false)
}

/// 应用远端推来的文本（内容相同则跳过，避免回环）。
pub fn apply_incoming(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    if let Some(local) = get_text() {
        if local == text {
            return false;
        }
    }
    set_text(text)
}
