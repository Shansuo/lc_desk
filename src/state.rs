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
    /// 允许被控制（总开关）。用 Arc 持有，便于直接交给发现线程共享，
    /// 否则广播里的 accepting 永远是构造时的初始值。
    pub accepting: Arc<AtomicBool>,
    /// 被控端监听是否就绪。端口被占用时监听线程会退出，但广播仍在跑，
    /// 若不区分，对端会看到一台「可被控制」却永远连不上的机器。
    pub server_ready: Arc<AtomicBool>,
    /// 正在被控的会话数
    pub controlled_count: AtomicU64,
    /// 上次密码校验失败的时刻（epoch 毫秒），用于暴力破解冷却
    pub last_auth_fail_ms: AtomicU64,
    /// 最近一次收到键鼠输入的时刻（epoch 毫秒，0 = 本会话从未收到）。
    /// 抓帧线程据此判断「用户正在操作」，从而临时解除帧率节流。
    pub last_input_ms: Arc<AtomicU64>,
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
        Self {
            device_id,
            config,
            peers,
            events_tx,
            ctx,
            accepting: Arc::new(AtomicBool::new(true)),
            server_ready: Arc::new(AtomicBool::new(false)),
            controlled_count: AtomicU64::new(0),
            last_auth_fail_ms: AtomicU64::new(0),
            last_input_ms: Arc::new(AtomicU64::new(0)),
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

    /// 密码校验失败后的冷却：连续尝试之间至少间隔 `cooldown`，
    /// 避免局域网内对控制密码做高速暴力枚举。
    pub fn note_auth_failure(&self, cooldown_ms: u64) {
        let now = crate::platform::now_millis();
        let until = now + cooldown_ms;
        // 只在推进时间时才写入，避免并发下的回退
        self.last_auth_fail_ms
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.max(until))
            })
            .ok();
    }

    /// 按上次失败时间执行退避等待。
    pub fn await_auth_cooldown(&self) {
        let until = self.last_auth_fail_ms.load(Ordering::Relaxed);
        let now = crate::platform::now_millis();
        if until > now {
            std::thread::sleep(std::time::Duration::from_millis(until - now));
        }
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
    /// 被控端：一个远控会话结束了
    SessionEnded { peer_name: String, reason: String },
    Notice { text: String },
}

/// 便捷：给 TCP 流设置 NODELAY。读写超时由各阶段自行设置
/// （握手阶段可能等待用户人工确认，不能用固定读超时）。
pub fn tune_stream(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
}
