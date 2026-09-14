//! 主控端：连接握手、接收解码视频帧、发送输入事件、双向剪贴板、会话状态。

use crate::protocol::{self, Msg};
use crate::state::{tune_stream, AppShared, UiEvent};
use egui::ColorImage;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 发往主控端 writer 线程的出站消息
pub enum OutMsg {
    Mouse(protocol::MouseMsg),
    Wheel(protocol::WheelMsg),
    Key(protocol::KeyMsg),
    Clipboard(String),
    Bye(String),
}

#[derive(Default, Clone, Debug)]
pub struct SessionStats {
    pub fps: f32,
    pub rtt_ms: f32,
    pub frame_w: u32,
    pub frame_h: u32,
}

/// 一个远控会话（主控端视角），供 UI 视口使用。
pub struct RemoteSession {
    pub id: u64,
    pub peer_name: String,
    /// 最新解码帧
    pub frame: Arc<Mutex<Option<Arc<ColorImage>>>>,
    pub frame_dirty: AtomicBool,
    pub stats: Arc<Mutex<SessionStats>>,
    pub input_tx: Sender<OutMsg>,
    pub view_only: AtomicBool,
    pub closed: AtomicBool,
    /// 视口状态（由 UI 维护，跨帧记忆）
    pub fullscreen: AtomicBool,
    /// 输入处理所需的帧内状态
    pub ui_state: Arc<Mutex<RemoteUiState>>,
}

/// 远控视口的可变局部状态（仅 UI 线程访问，但需内部可变性）。
#[derive(Default)]
pub struct RemoteUiState {
    pub texture: Option<egui::TextureHandle>,
    /// 上次发送的鼠标归一化位置（节流用）
    pub last_sent: Option<(f32, f32)>,
    /// 本地按下的鼠标键（拖拽出画面时仍转发移动）
    pub buttons_down: [bool; 3],
    /// 上次发送的修饰键状态
    pub last_mods: [bool; 4], // ctrl alt shift super
    /// 滚轮小数累积
    pub wheel_acc: (f32, f32),
    /// 最近一条提示
    pub toast: Option<(Instant, String)>,
}

impl RemoteSession {
    pub fn disconnect(&self, reason: &str) {
        if !self.closed.swap(true, Ordering::Relaxed) {
            let _ = self.input_tx.send(OutMsg::Bye(reason.to_string()));
        }
    }

    pub fn send_clipboard(&self) -> bool {
        match crate::clipboard_sync::get_text() {
            Some(text) if !text.is_empty() => {
                self.input_tx.send(OutMsg::Clipboard(text)).is_ok()
            }
            _ => false,
        }
    }
}

/// 发起连接（后台线程完成握手后通过事件交付会话）。
pub fn connect(shared: Arc<AppShared>, addr: SocketAddr, view_only: bool, password: Option<String>) {
    std::thread::Builder::new()
        .name("lc-viewer".into())
        .spawn(move || run_connect(shared, addr, view_only, password))
        .expect("spawn lc-viewer");
}

fn run_connect(shared: Arc<AppShared>, addr: SocketAddr, view_only: bool, password: Option<String>) {
    match try_connect(&shared, addr, view_only, password) {
        Ok(session) => {
            let _ = shared.events_tx.send(UiEvent::SessionStarted { session });
        }
        Err(e) => {
            shared.notify(format!("连接 {addr} 失败：{e}"));
        }
    }
}

fn try_connect(
    shared: &Arc<AppShared>,
    addr: SocketAddr,
    view_only: bool,
    password: Option<String>,
) -> Result<Arc<RemoteSession>, String> {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| format!("无法连接: {e}"))?;
    tune_stream(&stream);
    let mut stream = stream;

    // ---- 握手 ----
    let hello_out = Msg::Hello(protocol::Hello {
        protocol_version: protocol::PROTOCOL_VERSION,
        device_id: shared.device_id.clone(),
        name: shared.device_name(),
        platform: crate::platform::platform_name().to_string(),
        app_version: crate::platform::app_version().to_string(),
        auth_required: false,
    });
    protocol::write_msg(&mut stream, &hello_out).map_err(|e| e.to_string())?;
    let peer_hello = match protocol::read_msg(&mut stream) {
        Ok(Msg::Hello(h)) if h.protocol_version == protocol::PROTOCOL_VERSION => h,
        Ok(Msg::Hello(h)) => {
            return Err(format!(
                "协议版本不兼容（对端 v{}，本机 v{}）",
                h.protocol_version,
                protocol::PROTOCOL_VERSION
            ))
        }
        Ok(_) | Err(_) => return Err("对端握手异常".into()),
    };

    // ---- 密码 ----
    if peer_hello.auth_required {
        let password = match password {
            Some(p) => p,
            None => {
                // 向 UI 要密码
                let (tx, rx) = mpsc::channel::<Option<String>>();
                let _ = shared.events_tx.send(UiEvent::PasswordNeeded {
                    req_id: shared.next_req_id(),
                    peer: format!("{} ({addr})", peer_hello.name),
                    resp: tx,
                });
                match rx.recv_timeout(Duration::from_secs(90)) {
                    Ok(Some(p)) => p,
                    _ => return Err("已取消".into()),
                }
            }
        };
        protocol::write_msg(&mut stream, &Msg::Auth(protocol::Auth { password }))
            .map_err(|e| e.to_string())?;
        match protocol::read_msg(&mut stream) {
            Ok(Msg::AuthResult(r)) if r.ok => {}
            Ok(Msg::AuthResult(r)) => return Err(r.message),
            _ => return Err("鉴权响应异常".into()),
        }
    }

    // ---- 控制请求 ----
    protocol::write_msg(&mut stream, &Msg::RequestControl(protocol::RequestControl { view_only }))
        .map_err(|e| e.to_string())?;
    let result = match protocol::read_msg(&mut stream) {
        Ok(Msg::ControlResult(r)) => r,
        _ => return Err("对端响应异常".into()),
    };
    if !result.ok {
        return Err(result.message);
    }

    // ---- 会话 ----
    let session_id = shared.next_req_id();
    let (input_tx, input_rx) = mpsc::channel::<OutMsg>();
    let session = Arc::new(RemoteSession {
        id: session_id,
        peer_name: peer_hello.name.clone(),
        frame: Arc::new(Mutex::new(None)),
        frame_dirty: AtomicBool::new(false),
        stats: Arc::new(Mutex::new(SessionStats::default())),
        input_tx: input_tx.clone(),
        view_only: AtomicBool::new(view_only),
        closed: AtomicBool::new(false),
        fullscreen: AtomicBool::new(false),
        ui_state: Arc::new(Mutex::new(RemoteUiState::default())),
    });

    // reader 线程
    {
        let session = session.clone();
        let ctx = shared.ctx.clone();
        let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
        std::thread::Builder::new()
            .name("lc-viewer-reader".into())
            .spawn(move || {
                let mut frame_times: Vec<Instant> = Vec::with_capacity(64);
                loop {
                    if session.closed.load(Ordering::Relaxed) {
                        break;
                    }
                    match protocol::read_msg(&mut reader) {
                        Ok(Msg::VideoFrame { width, height, jpeg }) => {
                            if let Some(img) = decode_jpeg(&jpeg, width, height) {
                                let img = Arc::new(img);
                                *session.frame.lock().unwrap() = Some(img.clone());
                                session.frame_dirty.store(true, Ordering::Relaxed);
                                let mut stats = session.stats.lock().unwrap();
                                stats.frame_w = width;
                                stats.frame_h = height;
                                let now = Instant::now();
                                frame_times.retain(|t| now.duration_since(*t).as_secs_f32() < 2.0);
                                frame_times.push(now);
                                stats.fps = frame_times.len() as f32 / 2.0;
                                drop(stats);
                                ctx.request_repaint();
                            }
                        }
                        Ok(Msg::Pong(p)) => {
                            let rtt = now_millis().saturating_sub(p.ts) as f32;
                            session.stats.lock().unwrap().rtt_ms = rtt;
                        }
                        Ok(Msg::Clipboard(c)) => {
                            crate::clipboard_sync::set_text(&c.text);
                        }
                        Ok(Msg::Bye(b)) => {
                            session.disconnect(&b.reason);
                            break;
                        }
                        Ok(_) => {}
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            continue;
                        }
                        Err(_) => {
                            session.disconnect("连接断开");
                            break;
                        }
                    }
                }
            })
            .expect("spawn viewer reader");
    }

    // writer 线程：输入事件 + 心跳 + 本地剪贴板推送
    {
        let session = session.clone();
        let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
        let sync_clipboard = shared.config.lock().unwrap().sync_clipboard;
        std::thread::Builder::new()
            .name("lc-viewer-writer".into())
            .spawn(move || {
                let mut last_local: Option<String> = crate::clipboard_sync::get_text();
                let mut ping_ts = 0u64;
                loop {
                    if session.closed.load(Ordering::Relaxed) {
                        break;
                    }
                    match input_rx.recv_timeout(Duration::from_secs(2)) {
                        Ok(out) => {
                            let msg = match out {
                                OutMsg::Mouse(m) => Msg::Mouse(m),
                                OutMsg::Wheel(w) => Msg::Wheel(w),
                                OutMsg::Key(k) => Msg::Key(k),
                                OutMsg::Clipboard(text) => {
                                    last_local = Some(text.clone());
                                    Msg::Clipboard(protocol::ClipboardMsg { text })
                                }
                                OutMsg::Bye(reason) => {
                                    let _ = protocol::write_msg(
                                        &mut writer,
                                        &Msg::Bye(protocol::ByeMsg { reason }),
                                    );
                                    break;
                                }
                            };
                            if protocol::write_msg(&mut writer, &msg).is_err() {
                                session.disconnect("发送失败");
                                break;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                    // 心跳
                    ping_ts += 1;
                    if ping_ts % 2 == 0 {
                        let _ = protocol::write_msg(
                            &mut writer,
                            &Msg::Ping(protocol::PingMsg { ts: now_millis() }),
                        );
                    }
                    // 本地剪贴板 → 远端
                    if sync_clipboard {
                        if let Some(text) = crate::clipboard_sync::get_text() {
                            let changed = last_local.as_deref() != Some(text.as_str());
                            if changed && !text.is_empty() {
                                last_local = Some(text.clone());
                                let _ = protocol::write_msg(
                                    &mut writer,
                                    &Msg::Clipboard(protocol::ClipboardMsg { text }),
                                );
                            }
                        }
                    }
                }
            })
            .expect("spawn viewer writer");
    }

    Ok(session)
}

fn decode_jpeg(jpeg: &[u8], w: u32, h: u32) -> Option<ColorImage> {
    let img = image::load_from_memory(jpeg).ok()?;
    let rgba = img.to_rgba8();
    if rgba.width() != w || rgba.height() != h {
        log::debug!("帧尺寸标注 {w}x{h} 与实际 {}x{} 不符", rgba.width(), rgba.height());
    }
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    let pixels = rgba.into_raw();
    Some(ColorImage::from_rgba_unmultiplied([w, h], &pixels))
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
