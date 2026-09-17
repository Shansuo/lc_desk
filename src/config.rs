//! 配置持久化（JSON，位于系统配置目录）。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub const DEFAULT_FPS: u32 = 24;
pub const DEFAULT_QUALITY: u8 = 70;
pub const DEFAULT_MAX_WIDTH: u32 = 1920;
/// 帧率下限/上限（与设置面板滑条保持一致，避免多处硬编码漂移）
pub const FPS_RANGE: std::ops::RangeInclusive<u32> = 5..=30;
pub const QUALITY_RANGE: std::ops::RangeInclusive<u8> = 30..=95;
pub const MAX_WIDTH_RANGE: std::ops::RangeInclusive<u32> = 1280..=3840;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub device_name: String,
    /// 随机生成的稳定设备 ID（短码，用于展示）
    pub device_id: String,
    /// 控制密码的 sha256(salt||password) 十六进制；空 salt 表示未设密码
    pub password_salt: String,
    pub password_hash: String,
    /// 设置了密码时可自动接受连接（免确认弹窗）
    pub auto_accept: bool,
    pub tcp_port: u16,
    pub fps: u32,
    pub jpeg_quality: u8,
    pub max_width: u32,
    /// 被控时把主控端发来的 Ctrl 映射为 macOS Command
    pub ctrl_as_cmd: bool,
    /// 会话中双向同步剪贴板文本
    pub sync_clipboard: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: crate::platform::device_name(),
            device_id: String::new(),
            password_salt: String::new(),
            password_hash: String::new(),
            auto_accept: false,
            tcp_port: crate::protocol::TCP_DEFAULT_PORT,
            fps: DEFAULT_FPS,
            jpeg_quality: DEFAULT_QUALITY,
            max_width: DEFAULT_MAX_WIDTH,
            ctrl_as_cmd: cfg!(target_os = "macos"),
            sync_clipboard: true,
        }
    }
}

impl Config {
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("lc_deck").join("config.json"))
    }

    pub fn load() -> Self {
        let mut cfg = Self::default();
        if let Some(path) = Self::config_path() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                match serde_json::from_str::<Config>(&text) {
                    Ok(loaded) => cfg = loaded,
                    Err(e) => log::warn!("配置文件解析失败，使用默认配置: {e}"),
                }
            }
        }
        if cfg.device_id.is_empty() {
            cfg.device_id = crate::platform::device_id();
            cfg.save();
        }
        cfg.sanitize();
        cfg
    }

    /// 把越界的手改配置夹回合法区间（配置文件是明文可编辑的）。
    pub fn sanitize(&mut self) {
        self.fps = self.fps.clamp(*FPS_RANGE.start(), *FPS_RANGE.end());
        self.jpeg_quality = self
            .jpeg_quality
            .clamp(*QUALITY_RANGE.start(), *QUALITY_RANGE.end());
        self.max_width = self
            .max_width
            .clamp(*MAX_WIDTH_RANGE.start(), *MAX_WIDTH_RANGE.end());
        if self.device_name.trim().is_empty() {
            self.device_name = crate::platform::device_name();
        }
    }

    pub fn save(&self) {
        if let Some(path) = Self::config_path() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match serde_json::to_string_pretty(self) {
                Ok(text) => {
                    if let Err(e) = std::fs::write(&path, text) {
                        log::warn!("配置保存失败: {e}");
                    }
                }
                Err(e) => log::warn!("配置序列化失败: {e}"),
            }
        }
    }

    pub fn has_password(&self) -> bool {
        !self.password_salt.is_empty() && !self.password_hash.is_empty()
    }

    pub fn set_password(&mut self, password: &str) {
        if password.is_empty() {
            self.password_salt.clear();
            self.password_hash.clear();
            self.auto_accept = false;
        } else {
            let salt: String = {
                use rand::RngExt;
                let mut rng = rand::rng();
                (0..16).map(|_| format!("{:02x}", rng.random::<u8>())).collect()
            };
            self.password_salt = salt;
            self.password_hash = hash_password(&self.password_salt, password);
        }
    }

    pub fn verify_password(&self, password: &str) -> bool {
        self.has_password() && hash_password(&self.password_salt, password) == self.password_hash
    }
}

pub fn hash_password(salt: &str, password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(b"|lc_deck|");
    hasher.update(password.as_bytes());
    let out = hasher.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_password_hash_roundtrip() {
        let mut cfg = Config::default();
        cfg.set_password("hello123");
        assert!(cfg.has_password());
        assert!(cfg.verify_password("hello123"));
        assert!(!cfg.verify_password("hello124"));
        cfg.set_password("");
        assert!(!cfg.has_password());
    }

    #[test]
    fn test_sanitize_clamps_out_of_range() {
        let mut cfg = Config::default();
        cfg.fps = 0;
        cfg.jpeg_quality = 200;
        cfg.max_width = 999999;
        cfg.device_name = "   ".into();
        cfg.sanitize();
        assert_eq!(cfg.fps, *FPS_RANGE.start());
        assert_eq!(cfg.jpeg_quality, *QUALITY_RANGE.end());
        assert_eq!(cfg.max_width, *MAX_WIDTH_RANGE.end());
        assert!(!cfg.device_name.trim().is_empty());
    }
}
