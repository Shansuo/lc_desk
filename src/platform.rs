//! 平台相关：设备名 / 设备 ID / 权限提示入口 / CJK 字体加载。

pub fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Linux"
    }
}

pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 计算机显示名称（局域网内展示用）。
pub fn device_name() -> String {
    if let Ok(name) = hostname_via_cmd() {
        if !name.trim().is_empty() {
            return name.trim().to_string();
        }
    }
    "未知设备".to_string()
}

fn hostname_via_cmd() -> std::io::Result<String> {
    let out = if cfg!(target_os = "windows") {
        std::process::Command::new("hostname").output()?
    } else {
        std::process::Command::new("hostname").output()?
    };
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 稳定设备 ID：优先取系统硬件 UUID，否则用随机并持久化（由 config 兜底）。
pub fn device_id() -> String {
    if let Some(uuid) = machine_uuid() {
        return short_id(&uuid);
    }
    let mut rng = rand::rng();
    use rand::RngExt;
    short_id(&format!("random-{:016x}", rng.random::<u128>()))
}

fn short_id(seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(seed.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{}-{}", &hex[..4].to_uppercase(), &hex[4..8].to_uppercase())
}

fn machine_uuid() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if line.contains("IOPlatformUUID") {
                let v = line.split('"').nth(3)?;
                return Some(v.to_string());
            }
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        let out = std::process::Command::new("reg")
            .args(["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if line.contains("MachineGuid") {
                let v = line.rsplit(' ').next()?;
                return Some(v.trim().to_string());
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::fs::read_to_string("/etc/machine-id").ok().map(|s| s.trim().to_string())
    }
}

/// 打开 macOS 屏幕录制权限设置面板。
#[cfg(target_os = "macos")]
pub fn open_screen_permission_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn();
}

/// 打开 macOS 辅助功能权限设置面板。
#[cfg(target_os = "macos")]
pub fn open_accessibility_permission_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
}

/// 为 egui 安装 CJK 字体（macOS/Windows/Linux 的系统字体），保证中文正常渲染。
pub fn install_cjk_fonts(ctx: &egui::Context) {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/Hiragino Sans GB.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/Supplemental/Songti.ttc",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\simhei.ttf",
            "C:\\Windows\\Fonts\\simsun.ttc",
        ]
    } else {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        ]
    };

    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "lc_deck_cjk".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("lc_deck_cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("lc_deck_cjk".to_owned());
            ctx.set_fonts(fonts);
            log::info!("已加载 CJK 字体: {path}");
            return;
        }
    }
    log::warn!("未找到系统 CJK 字体，中文可能显示为方块");
}
