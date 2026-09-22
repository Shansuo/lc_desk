//! 主控端：连接握手、接收解码视频帧、发送输入事件、双向剪贴板、会话状态。

use crate::protocol::{self, Msg};
use crate::state::{tune_stream, AppShared, UiEvent};
use egui::ColorImage;
use std::collections::VecDeque;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 发往主控端 writer 线程的出站消息
#[derive(Debug)]
pub enum OutMsg {
    Mouse(protocol::MouseMsg),
    Wheel(protocol::WheelMsg),
    Key(protocol::KeyMsg),
    Clipboard(String),
    Bye(String),
}

/// 心跳间隔。旧实现按「循环次数」计数发心跳，鼠标一动每秒就发出几十个
/// Ping/Pong，白白占用被控端发送线程；改为固定时间间隔。
const PING_INTERVAL: Duration = Duration::from_secs(1);
/// 本地剪贴板轮询间隔。旧实现每发一条输入事件就查一次系统剪贴板，
/// 鼠标一动就是上百次系统调用叠在交互路径上。
const CLIPBOARD_POLL: Duration = Duration::from_millis(600);
/// 移动事件可被裁剪的软上限，见 [`Outbox`]。
const MAX_PENDING: usize = 256;
/// 队列硬上限。正常打字或点击绝无可能堆到这么多，只可能是发送线程卡死；
/// 此时宁可丢掉最早的旧输入，也不让队列无限增长（内存 + 延迟双失控）。
const HARD_CAP: usize = 1024;

/// 出站输入队列。
///
/// 鼠标移动是高频且高度冗余的事件：被控端只关心最后落点。旧实现用无界
/// mpsc 逐条排队，一旦发送线程被别的工作拖慢（例如每条事件后都去查一次
/// 系统剪贴板），队列就会越积越长，操作延迟随累积无上限增长。这里在入队
/// 时把「队尾同样是移动事件」的情况合并成一条，延迟与队列长度解耦；
/// 按键、点击等有序事件不与任何东西合并，顺序严格保持。
#[derive(Default)]
pub struct Outbox {
    q: Mutex<VecDeque<OutMsg>>,
    cv: Condvar,
}

impl Outbox {
    pub fn push(&self, msg: OutMsg) {
        let is_move = matches!(&msg, OutMsg::Mouse(m) if m.action == 0);
        {
            let mut q = self.q.lock().unwrap();
            if is_move {
                if let OutMsg::Mouse(m) = msg {
                    let mut pending = Some(m);
                    if let Some(OutMsg::Mouse(last)) = q.back_mut() {
                        if last.action == 0 {
                            *last = pending.take().unwrap();
                        }
                    }
                    if let Some(m) = pending {
                        q.push_back(OutMsg::Mouse(m));
                    }
                }
            } else {
                q.push_back(msg);
            }
            // 兜底一：积压时优先丢弃最早的移动事件 —— 它一定是冗余的，
            // 丢掉不会让用户的操作丢失语义。
            while q.len() > MAX_PENDING {
                match q.iter().position(|m| matches!(m, OutMsg::Mouse(mm) if mm.action == 0)) {
                    Some(i) => {
                        q.remove(i);
                    }
                    None => break,
                }
            }
            // 兜底二：队列里全是按键/点击仍超硬上限（只可能是发送线程卡死），
            // 此时丢弃最早的旧输入，避免内存与延迟双双失控。
            while q.len() > HARD_CAP {
                q.pop_front();
            }
        }
        self.cv.notify_one();
    }

    fn try_pop(&self) -> Option<OutMsg> {
        self.q.lock().unwrap().pop_front()
    }

    fn pop_timeout(&self, timeout: Duration) -> Option<OutMsg> {
        let mut q = self.q.lock().unwrap();
        if q.is_empty() {
            let (guard, _) = self.cv.wait_timeout(q, timeout).unwrap();
            q = guard;
        }
        q.pop_front()
    }
}

#[derive(Default, Clone, Debug)]
pub struct SessionStats {
    pub fps: f32,
    pub rtt_ms: f32,
    pub frame_w: u32,
    pub frame_h: u32,
    /// 下行实测带宽（KB/s），按读到的 JPEG 字节统计
    pub kbps: f32,
    /// 本地 JPEG 解码耗时（毫秒，滑动平均）
    pub decode_ms: f32,
    /// 被控端回填的「抓帧 → 写出」耗时（毫秒）。旧版对端不填，为 0。
    pub server_pipeline_ms: u32,
}

impl SessionStats {
    /// 端到端延迟估算 = 被控端流水线 + 单程网络 + 本地解码。
    ///
    /// 三段都是各自一端实测出来的，不依赖两端时钟同步，因此这个数字是
    /// 可信的；旧版对端不回填流水线耗时时无法估算，返回 None。
    pub fn latency_estimate_ms(&self) -> Option<f32> {
        if self.server_pipeline_ms == 0 || self.rtt_ms <= 0.0 {
            return None;
        }
        Some(self.server_pipeline_ms as f32 + self.rtt_ms / 2.0 + self.decode_ms)
    }
}

/// 一个远控会话（主控端视角），供 UI 视口使用。
pub struct RemoteSession {
    pub id: u64,
    pub peer_name: String,
    /// 最新解码帧
    pub frame: Arc<Mutex<Option<Arc<ColorImage>>>>,
    pub frame_dirty: AtomicBool,
    pub stats: Arc<Mutex<SessionStats>>,
    outbox: Outbox,
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
    /// 滚轮小数累积（单位：行）
    pub wheel_acc: (f32, f32),
    /// 最近一次滚轮事件的时间，用于停止滚动后补发不足一行的残留
    pub wheel_last: Option<Instant>,
    /// 最近一条提示
    pub toast: Option<(Instant, String)>,
}

impl RemoteSession {
    pub fn viewport_id(&self) -> egui::ViewportId {
        egui::ViewportId(egui::Id::new(("remote", self.id)))
    }

    /// 投递一条出站输入。会话已关闭时丢弃，避免向死连接堆积。
    pub fn push(&self, msg: OutMsg) {
        if !self.closed.load(Ordering::Relaxed) {
            self.outbox.push(msg);
        }
    }

    pub fn disconnect(&self, reason: &str) {
        if !self.closed.swap(true, Ordering::Relaxed) {
            self.outbox.push(OutMsg::Bye(reason.to_string()));
        }
    }

    pub fn send_clipboard(&self) -> bool {
        match crate::clipboard_sync::get_text() {
            Some(text) if !text.is_empty() => {
                self.push(OutMsg::Clipboard(text));
                true
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
        .map_err(|e| format!("无法连接：{e}。请确认对端已运行、同一网段，且防火墙已放行其端口"))?;
    tune_stream(&stream);
    let mut stream = stream;

    // ---- 握手 ----
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
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
        // 密码由用户在本机弹窗里输入，等待时间要充裕
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
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

    // 对端会弹窗等人工确认，必须明确告诉用户去看对端，否则主控端
    // 看起来就是「点了没反应」
    shared.notify(format!(
        "已向「{}」发起{}请求，请在对方设备上点「允许」",
        peer_hello.name,
        if view_only { "观看" } else { "控制" }
    ));
    // 与对端 90 秒确认超时对齐，留一点余量
    let _ = stream.set_read_timeout(Some(Duration::from_secs(95)));
    let result = match protocol::read_msg(&mut stream) {
        Ok(Msg::ControlResult(r)) => r,
        _ => return Err("等待对方确认超时（90 秒）或响应异常".into()),
    };
    if !result.ok {
        return Err(result.message);
    }

    // ---- 会话 ----
    let session_id = shared.next_req_id();
    let session = Arc::new(RemoteSession {
        id: session_id,
        peer_name: peer_hello.name.clone(),
        frame: Arc::new(Mutex::new(None)),
        frame_dirty: AtomicBool::new(false),
        stats: Arc::new(Mutex::new(SessionStats::default())),
        outbox: Outbox::default(),
        view_only: AtomicBool::new(view_only),
        closed: AtomicBool::new(false),
        fullscreen: AtomicBool::new(false),
        ui_state: Arc::new(Mutex::new(RemoteUiState::default())),
    });

    // ---- 收帧：读线程只负责把内核缓冲抽干，解码交给独立线程 ----
    //
    // 旧实现读与解码在同一循环里：解码一帧 1080p JPEG 要 10~25ms，这期间
    // 内核接收缓冲里会堆着后续几帧。缓冲里的旧帧必须逐帧解码完才能轮到
    // 最新的那帧，客户端越慢延迟越高。拆开之后读循环始终把缓冲抽干，
    // 解码线程只保留最新帧，未解码的旧帧直接丢弃。
    let (raw_tx, raw_rx) = mpsc::sync_channel::<(u32, u32, Vec<u8>)>(1);
    {
        let session = session.clone();
        let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
        // 带超时才能周期回到循环顶部检查 session.closed，
        // 否则断开后线程会一直阻塞在读上（此前无超时，线程泄漏）
        let _ = reader.set_read_timeout(Some(Duration::from_secs(5)));
        std::thread::Builder::new()
            .name("lc-viewer-reader".into())
            .spawn(move || {
                // 带宽统计：按读到的 JPEG 字节算，反映链路真实占用
                let mut bw_bytes: usize = 0;
                let mut bw_start = Instant::now();
                loop {
                    if session.closed.load(Ordering::Relaxed) {
                        break;
                    }
                    match protocol::read_msg(&mut reader) {
                        Ok(Msg::VideoFrame { width, height, jpeg }) => {
                            bw_bytes += jpeg.len();
                            let elapsed = bw_start.elapsed();
                            if elapsed >= Duration::from_millis(1000) {
                                let kbps = bw_bytes as f32 / 1024.0 / elapsed.as_secs_f32();
                                session.stats.lock().unwrap().kbps = kbps;
                                bw_bytes = 0;
                                bw_start = Instant::now();
                            }
                            // 通道满说明解码还没跟上，直接丢掉这一帧：
                            // 留着的旧帧只会让画面更旧。
                            let _ = raw_tx.try_send((width, height, jpeg));
                        }
                        Ok(Msg::Pong(p)) => {
                            let rtt = now_millis().saturating_sub(p.ts) as f32;
                            let mut stats = session.stats.lock().unwrap();
                            stats.rtt_ms = rtt;
                            stats.server_pipeline_ms = p.pipeline_ms;
                        }
                        Ok(Msg::Clipboard(c)) => {
                            // 走 apply_incoming 做去重：内容一致时不回写本地剪贴板，
                            // 否则「本地→远端→本地」的回环会反复刷新剪贴板时间戳。
                            crate::clipboard_sync::apply_incoming(&c.text);
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

    // 解码线程
    {
        let session = session.clone();
        let ctx = shared.ctx.clone();
        std::thread::Builder::new()
            .name("lc-viewer-decode".into())
            .spawn(move || {
                let mut frame_times: Vec<Instant> = Vec::with_capacity(64);
                loop {
                    if session.closed.load(Ordering::Relaxed) {
                        break;
                    }
                    match raw_rx.recv_timeout(Duration::from_millis(300)) {
                        Ok((width, height, jpeg)) => {
                            let started = Instant::now();
                            let Some(img) = decode_jpeg(&jpeg, width, height) else {
                                continue;
                            };
                            let decode_ms = started.elapsed().as_secs_f32() * 1000.0;
                            *session.frame.lock().unwrap() = Some(Arc::new(img));
                            session.frame_dirty.store(true, Ordering::Relaxed);

                            let now = Instant::now();
                            frame_times.retain(|t| now.duration_since(*t).as_secs_f32() < 2.0);
                            frame_times.push(now);
                            let mut stats = session.stats.lock().unwrap();
                            stats.frame_w = width;
                            stats.frame_h = height;
                            stats.fps = frame_times.len() as f32 / 2.0;
                            stats.decode_ms = if stats.decode_ms == 0.0 {
                                decode_ms
                            } else {
                                stats.decode_ms * 0.7 + decode_ms * 0.3
                            };
                            drop(stats);

                            // 注意：request_repaint() 只重绘当前视口（后台线程为 ROOT），
                            // 远控画面在独立视口，必须定向请求重绘，
                            // 否则画面要等用户碰窗口才更新。
                            ctx.request_repaint_of(session.viewport_id());
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("spawn viewer decode");
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
                let mut next_ping = Instant::now() + PING_INTERVAL;
                let mut next_clip = Instant::now() + CLIPBOARD_POLL;
                while !session.closed.load(Ordering::Relaxed) {
                    // 1) 先清空积压（移动事件已在入队时合并，通常只有一两条）。
                    //    批量发完再去做心跳/剪贴板，避免周期工作插在交互事件中间。
                    let mut alive = true;
                    while let Some(out) = session.outbox.try_pop() {
                        if !send_out(&mut writer, &session, out) {
                            alive = false;
                            break;
                        }
                    }
                    if !alive {
                        break;
                    }

                    let now = Instant::now();
                    // 2) 心跳
                    if now >= next_ping {
                        let ping = Msg::Ping(protocol::PingMsg { ts: now_millis(), pipeline_ms: 0 });
                        if protocol::write_msg(&mut writer, &ping).is_err() {
                            session.disconnect("发送失败");
                            break;
                        }
                        next_ping = now + PING_INTERVAL;
                    }
                    // 3) 本地剪贴板 → 远端（内容变更才推送，且按间隔轮询）
                    if sync_clipboard && now >= next_clip {
                        next_clip = now + CLIPBOARD_POLL;
                        if let Some(text) = crate::clipboard_sync::get_text() {
                            let changed = last_local.as_deref() != Some(text.as_str());
                            if changed && !text.is_empty() {
                                last_local = Some(text.clone());
                                let msg = Msg::Clipboard(protocol::ClipboardMsg { text });
                                if protocol::write_msg(&mut writer, &msg).is_err() {
                                    session.disconnect("发送失败");
                                    break;
                                }
                            } else if changed {
                                last_local = Some(text);
                            }
                        }
                    }

                    // 4) 阻塞等待下一条事件，超时用于心跳与剪贴板轮询。
                    //    注意 next_clip 只在开启剪贴板同步时才推进，若一并参与
                    //    取最小值，关闭同步后会退化成 1ms 空转。
                    let mut wait = next_ping.saturating_duration_since(Instant::now());
                    if sync_clipboard {
                        wait = wait.min(next_clip.saturating_duration_since(Instant::now()));
                    }
                    let wait = wait.max(Duration::from_millis(1));
                    if let Some(out) = session.outbox.pop_timeout(wait) {
                        if !send_out(&mut writer, &session, out) {
                            break;
                        }
                    }
                }
            })
            .expect("spawn viewer writer");
    }

    Ok(session)
}

/// 写出一条出站消息。返回 false 表示发送线程应当结束（收到 Bye 或写入失败）。
fn send_out(writer: &mut TcpStream, session: &RemoteSession, out: OutMsg) -> bool {
    let msg = match out {
        OutMsg::Mouse(m) => Msg::Mouse(m),
        OutMsg::Wheel(w) => Msg::Wheel(w),
        OutMsg::Key(k) => Msg::Key(k),
        OutMsg::Clipboard(text) => Msg::Clipboard(protocol::ClipboardMsg { text }),
        OutMsg::Bye(reason) => {
            let _ = protocol::write_msg(writer, &Msg::Bye(protocol::ByeMsg { reason }));
            return false;
        }
    };
    if protocol::write_msg(writer, &msg).is_err() {
        session.disconnect("发送失败");
        return false;
    }
    true
}

fn decode_jpeg(jpeg: &[u8], w: u32, h: u32) -> Option<ColorImage> {
    let img = image::load_from_memory(jpeg).ok()?;
    // JPEG 解码结果本就是 RGB，into_rgb8 直接接管解码缓冲（不复制像素），
    // 再由 from_rgb 一次转成 Color32。旧写法先 to_rgba8 再
    // from_rgba_unmultiplied，全图要遍历两遍、多分配一份 8MB 缓冲。
    let rgb = img.into_rgb8();
    if rgb.width() != w || rgb.height() != h {
        log::debug!("帧尺寸标注 {w}x{h} 与实际 {}x{} 不符", rgb.width(), rgb.height());
    }
    Some(ColorImage::from_rgb(
        [rgb.width() as usize, rgb.height() as usize],
        rgb.as_raw(),
    ))
}

fn now_millis() -> u64 {
    crate::platform::now_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{KeyMsg, MouseMsg};

    fn mouse(x: f32, action: u8) -> OutMsg {
        OutMsg::Mouse(MouseMsg { x, y: 0.5, button: 0, action })
    }

    /// 连续移动必须合并成一条：这是「发送线程被拖慢时延迟不累积」的关键。
    #[test]
    fn test_outbox_merges_consecutive_moves() {
        let ob = Outbox::default();
        ob.push(mouse(0.1, 0));
        ob.push(mouse(0.2, 0));
        ob.push(mouse(0.3, 0));
        let only = ob.try_pop().expect("应有一条");
        match only {
            OutMsg::Mouse(m) => assert_eq!(m.x, 0.3, "必须保留最后落点"),
            other => panic!("类型错误: {other:?}"),
        }
        assert!(ob.try_pop().is_none(), "中间位置不应残留");
    }

    /// 按下/抬起不能被合并，否则点击位置会错乱。
    #[test]
    fn test_outbox_keeps_button_events() {
        let ob = Outbox::default();
        ob.push(mouse(0.1, 0));
        ob.push(mouse(0.1, 1)); // 按下
        ob.push(mouse(0.2, 0)); // 移动
        ob.push(mouse(0.2, 2)); // 抬起
        let actions: Vec<u8> = std::iter::from_fn(|| ob.try_pop())
            .map(|m| match m {
                OutMsg::Mouse(mm) => mm.action,
                _ => 9,
            })
            .collect();
        assert_eq!(actions, vec![0, 1, 0, 2], "按下/抬起必须原样保留且顺序不变");
    }

    /// 按键事件不能被移动事件挤掉，顺序也不能变。
    #[test]
    fn test_outbox_preserves_key_order() {
        let ob = Outbox::default();
        ob.push(OutMsg::Key(KeyMsg { key: "a".into(), down: true }));
        ob.push(mouse(0.5, 0));
        ob.push(mouse(0.6, 0));
        ob.push(OutMsg::Key(KeyMsg { key: "a".into(), down: false }));
        let mut kinds = Vec::new();
        for m in std::iter::from_fn(|| ob.try_pop()) {
            kinds.push(match m {
                OutMsg::Key(k) => format!("key:{}:{}", k.key, k.down),
                OutMsg::Mouse(mm) => format!("move:{}", mm.x),
                _ => "other".into(),
            });
        }
        assert_eq!(
            kinds,
            vec!["key:a:true", "move:0.6", "key:a:false"],
            "移动被合并，但按键与相对顺序必须完整保留"
        );
    }

    /// 积压时优先丢移动事件，按键/点击一个都不能少。
    #[test]
    fn test_outbox_drops_moves_before_keys() {
        let ob = Outbox::default();
        ob.push(OutMsg::Key(KeyMsg { key: "enter".into(), down: true }));
        for i in 0..MAX_PENDING {
            ob.push(OutMsg::Key(KeyMsg { key: format!("k{i}"), down: true }));
        }
        // 队列已满，此后每次推入「移动 + 按键」都会先裁掉那条移动
        for i in 0..20 {
            ob.push(mouse(0.5, 0));
            ob.push(OutMsg::Key(KeyMsg { key: format!("extra{i}"), down: true }));
        }

        let mut keys = 0;
        let mut moves = 0;
        let mut has_enter = false;
        while let Some(m) = ob.try_pop() {
            match m {
                OutMsg::Key(k) => {
                    keys += 1;
                    if k.key == "enter" {
                        has_enter = true;
                    }
                }
                OutMsg::Mouse(_) => moves += 1,
                _ => {}
            }
        }
        assert!(has_enter, "overflow 只应丢弃移动事件，按键必须保留");
        assert_eq!(moves, 0, "积压时应把移动事件全部裁掉（它们一定是冗余的）");
        assert_eq!(keys, 1 + MAX_PENDING + 20, "按键一个都不能少");
    }

    /// 全是按键却堆到硬上限（发送线程卡死），此时允许丢最早的旧输入兜底。
    #[test]
    fn test_outbox_hard_cap_bounds_queue() {
        let ob = Outbox::default();
        for i in 0..HARD_CAP + 100 {
            ob.push(OutMsg::Key(KeyMsg { key: format!("k{i}"), down: true }));
        }
        let mut n = 0;
        let mut first = String::new();
        while let Some(m) = ob.try_pop() {
            if let OutMsg::Key(k) = m {
                if n == 0 {
                    first = k.key;
                }
                n += 1;
            }
        }
        assert_eq!(n, HARD_CAP, "硬上限必须生效，否则队列与延迟都会无限增长");
        assert_eq!(first, format!("k{}", 100), "丢的必须是最早的旧输入");
    }

    /// 延迟估算必须把三段相加；旧版对端（无流水线耗时）应返回 None 而不是瞎猜。
    #[test]
    fn test_latency_estimate() {
        let mut s = SessionStats { rtt_ms: 4.0, decode_ms: 12.0, ..Default::default() };
        assert_eq!(s.latency_estimate_ms(), None, "旧版对端无流水线耗时不估算");

        s.server_pipeline_ms = 35;
        let est = s.latency_estimate_ms().unwrap();
        assert!((est - (35.0 + 2.0 + 12.0)).abs() < 0.01);
    }

    /// 手工基准：主控端单帧「JPEG 解码 → ColorImage」耗时。
    /// 运行：`cargo test --release bench_decode -- --ignored --nocapture`
    /// 这是主控端固定占用的延迟，与网络无关。
    #[test]
    #[ignore = "手工基准，按需运行"]
    fn bench_decode_jpeg() {
        use image::codecs::jpeg::JpegEncoder;
        // 与 capture.rs 的编码基准用同一套内容特征（渐变 + 棋盘），
        // 否则合成噪声会让 JPEG 体积虚高几倍，解码耗时不可比。
        fn synth(width: u32, height: u32) -> Vec<u8> {
            let mut buf = vec![0u8; (width * height * 3) as usize];
            for y in 0..height {
                for x in 0..width {
                    let i = ((y * width + x) * 3) as usize;
                    let checker = if ((x / 8) + (y / 8)) % 2 == 0 { 20 } else { 0 };
                    buf[i] = (x % 251) as u8;
                    buf[i + 1] = (y % 241) as u8;
                    buf[i + 2] = (((x + y) % 233) as u8).saturating_add(checker);
                }
            }
            buf
        }

        for (w, h, q) in [(1920u32, 1200u32, 70u8), (1920, 1080, 70), (2560, 1600, 70)] {
            let rgb = synth(w, h);
            let mut jpeg = Vec::new();
            JpegEncoder::new_with_quality(&mut jpeg, q)
                .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
                .unwrap();

            let _ = decode_jpeg(&jpeg, w, h); // 预热
            let t0 = Instant::now();
            let img = decode_jpeg(&jpeg, w, h).expect("解码失败");
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            println!(
                "{w}×{h} q{q}: 解码+转 ColorImage {ms:.1}ms（{}KB，纹理 {}MB）",
                jpeg.len() / 1024,
                img.pixels.len() * 4 / 1_048_576
            );
        }
    }
}
