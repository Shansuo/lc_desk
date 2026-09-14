//! 应用共享状态与 UI 事件通道。

use crate::config::Config;
use crate::discovery::PeerBook;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

/// 被控端会话共享状态
pub struct ControlledSession {
    /// 是否仅观看
    pub view_only: AtomicBool,
}

pub struct AppShared {
    pub device_id: String,
    pub config: Arc<Mutex<Config>>,
    pub peers: PeerBook,
    pub events_tx: Sender<UiEvent>,
    pub ctx: egui::Context,
    /// 允许被控制（总开关）
    pub accepting: AtomicBool,
    /// 正在被控的会话数
    pub controlled_count: AtomicU64,
    next_req_id: AtomicU64,
}

impl AppShared {
    pub fn new(
        config: Arc<Mutex<Config>>,
        peers: PeerBook,
        events_tx: Sender<UiEvent>,
        ctx: egui::Context,
    ) -> Self {
        let device_id = config.lock().unwrap().device_id.clone();
        let accepting = AtomicBool::new(true);
        Self {
            device_id,
            config,
            peers,
            events_tx,
            ctx,
            accepting,
            controlled_count: AtomicU64::new(0),
            next_req_id: AtomicU64::new(1),
        }
    }

    pub fn next_req_id(&self) -> u64 {
        self.next_req_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn notify(&self, text: impl Into<String>) {
        let _ = self.events_tx.send(UiEvent::Notice { text: text.into() });
    }

    pub fn device_name(&self) -> String {
        self.config.lock().unwrap().device_name.clone()
    }
}

pub enum UiEvent {
    /// 被控端：有人请求控制，等待本机确认
    IncomingRequest {
        req_id: u64,
        name: String,
        device_id: String,
        ip: String,
        view_only: bool,
        resp: std::sync::mpsc::Sender<bool>,
    },
    /// 主控端：对端要求密码
    PasswordNeeded {
        req_id: u64,
        peer: String,
        resp: std::sync::mpsc::Sender<Option<String>>,
    },
    /// 主控端：会话已建立，打开远控窗口
    SessionStarted { session: Arc<crate::client::RemoteSession> },
    SessionEnded { session_id: u64, reason: String },
    Notice { text: String },
}

/// 便捷：给 TCP 流设置 NODELAY。读写超时由各阶段自行设置
/// （握手阶段可能等待用户人工确认，不能用固定读超时）。
pub fn tune_stream(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
}
