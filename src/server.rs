//! 被控端：TCP 监听、握手鉴权、本机确认、屏幕流发送、输入执行、剪贴板同步。

use crate::capture::{self, CaptureConfig};
use crate::input_exec::InputExecutor;
use crate::protocol::{self, Msg};
use crate::state::{tune_stream, AppShared, ControlledSession, UiEvent};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

/// 启动被控端监听线程。
pub fn start_server(shared: Arc<AppShared>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("lc-server".into())
        .spawn(move || run_listener(shared))
        .expect("spawn lc-server")
}

fn run_listener(shared: Arc<AppShared>) {
    let port = shared.config.lock().unwrap().tcp_port;
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            shared.notify(format!("被控端口 {port} 监听失败：{e}。请在设置中更换端口。"));
            return;
        }
    };
    log::info!("被控端监听 0.0.0.0:{port}");
    shared.notify(format!("被控端已就绪（端口 {port}）"));

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name("lc-conn".into())
                    .spawn(move || handle_conn(shared, stream))
                    .ok();
            }
            Err(e) => {
                log::warn!("接受连接失败: {e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn handle_conn(shared: Arc<AppShared>, stream: TcpStream) {
    tune_stream(&stream);
    let peer_ip = stream
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "未知".into());
    log::info!("主控端接入: {peer_ip}");

    let mut reader = match stream.try_clone() {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut stream = stream;

    // ---- 握手 ----
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let hello = match read_expect_hello(&mut reader) {
        Some(h) => h,
        None => return,
    };
    let auth_required = shared.config.lock().unwrap().has_password();
    let our_hello = Msg::Hello(protocol::Hello {
        protocol_version: protocol::PROTOCOL_VERSION,
        device_id: shared.device_id.clone(),
        name: shared.device_name(),
        platform: crate::platform::platform_name().to_string(),
        app_version: crate::platform::app_version().to_string(),
        auth_required,
    });
    if protocol::write_msg(&mut stream, &our_hello).is_err() {
        return;
    }

    // ---- 密码鉴权 ----
    let mut password_ok = !auth_required;
    if auth_required {
        match protocol::read_msg(&mut reader) {            Ok(Msg::Auth(a)) => {
                password_ok = shared.config.lock().unwrap().verify_password(&a.password);
                let _ = protocol::write_msg(
                    &mut stream,
                    &Msg::AuthResult(protocol::AuthResult {
                        ok: password_ok,
                        message: if password_ok { "ok".into() } else { "密码错误".into() },
                    }),
                );
                if !password_ok {
                    return;
                }
            }
            _ => return,
        }
    }

    // ---- 控制请求 ----
    let request = match protocol::read_msg(&mut reader) {
        Ok(Msg::RequestControl(r)) => r,
        _ => return,
    };
    let view_only = request.view_only;

    // 人工确认可能等很久，确认阶段取消读超时
    let _ = stream.set_read_timeout(None);
    let accepted = decide_accept(&shared, &hello, password_ok, view_only, &mut stream);
    if !accepted {
        return;
    }

    // ---- 建立会话 ----
    let session_id = shared.next_req_id();
    match run_session(&shared, stream, reader, view_only, &hello, &peer_ip, session_id) {
        Ok(reason) => {
            log::info!("会话 {session_id} 结束: {reason}");
            let _ = shared.events_tx.send(UiEvent::SessionEnded { session_id, reason });
        }
        Err(e) => {
            log::warn!("会话 {session_id} 建立失败: {e}");
            let _ = shared.events_tx.send(UiEvent::Notice { text: e });
        }
    }
}

fn read_expect_hello(reader: &mut TcpStream) -> Option<protocol::Hello> {
    match protocol::read_msg(reader) {
        Ok(Msg::Hello(h)) => Some(h),
        Ok(_) => None,
        Err(e) => {
            log::debug!("读取 Hello 失败: {e}");
            None
        }
    }
}

/// 是否接受本次控制请求（弹窗确认 / 密码自动接受），并回发结果。
fn decide_accept(
    shared: &Arc<AppShared>,
    hello: &protocol::Hello,
    password_ok: bool,
    view_only: bool,
    stream: &mut TcpStream,
) -> bool {
    let (allow, auto) = {
        let cfg = shared.config.lock().unwrap();
        (shared.accepting.load(Ordering::Relaxed), cfg.auto_accept && cfg.has_password())
    };

    let mut accepted = false;
    if !allow {
        let _ = protocol::write_msg(
            stream,
            &Msg::ControlResult(protocol::ControlResult {
                ok: false,
                message: "对端已关闭「允许被控制」".into(),
            }),
        );
        return false;
    } else if auto && password_ok {
        accepted = true;
    } else {
        // 弹窗确认
        let (tx, rx) = mpsc::channel::<bool>();
        let _ = shared.events_tx.send(UiEvent::IncomingRequest {
            req_id: shared.next_req_id(),
            name: hello.name.clone(),
            device_id: hello.device_id.clone(),
            ip: stream
                .peer_addr()
                .map(|a| a.ip().to_string())
                .unwrap_or_default(),
            view_only,
            resp: tx,
        });
        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(true) => accepted = true,
            Ok(false) => {}
            Err(_) => {} // 超时视为拒绝
        }
    }

    let _ = protocol::write_msg(
        stream,
        &Msg::ControlResult(protocol::ControlResult {
            ok: accepted,
            message: if accepted { "ok".into() } else { "对端拒绝了本次连接".into() },
        }),
    );
    accepted
}

/// 会话主体：抓屏发送 + 输入执行 + 剪贴板同步。
fn run_session(
    shared: &Arc<AppShared>,
    stream: TcpStream,
    mut reader: TcpStream,
    view_only: bool,
    hello: &protocol::Hello,
    peer_ip: &str,
    session_id: u64,
) -> Result<String, String> {
    let mut executor = InputExecutor::new(shared.config.lock().unwrap().ctrl_as_cmd)
        .map_err(|e| e)?;
    log::info!("会话 {session_id} 开始：{}({}) view_only={view_only}", hello.name, peer_ip);

    let stop = Arc::new(AtomicBool::new(false));
    shared.controlled_count.fetch_add(1, Ordering::Relaxed);

    // 控制消息通道：reader 线程 → writer 线程（Pong / Clipboard / Bye）
    let (ctl_tx, ctl_rx) = mpsc::channel::<Msg>();
    let session = Arc::new(ControlledSession {
        view_only: AtomicBool::new(view_only),
    });

    // 帧通道（有界，最新帧优先：容量 1，编码完成即替换，避免排队积压延迟）
    let (frame_tx, frame_rx) = mpsc::sync_channel::<(u32, u32, Vec<u8>)>(1);
    let cap_cfg = {
        let cfg = shared.config.lock().unwrap();
        CaptureConfig { fps: cfg.fps, jpeg_quality: cfg.jpeg_quality, max_width: cfg.max_width }
    };
    let cap_stop = stop.clone();
    let notify = shared.clone();
    let capture_handle = capture::spawn_capture(
        cap_cfg,
        frame_tx,
        cap_stop,
        Box::new(move |text| notify.notify(text)),
    );

    // writer 线程：帧 + 控制消息
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            stop.store(true, Ordering::Relaxed);
            return Err(format!("clone stream failed: {e}"));
        }
    };
    let writer_stop = stop.clone();
    let writer_handle = std::thread::Builder::new()
        .name("lc-conn-writer".into())
        .spawn(move || {
            while !writer_stop.load(Ordering::Relaxed) {
                // 先发帧（带 50ms 等待）。收到后清空通道取最新帧，避免发送排队旧帧。
                match frame_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(mut latest) => {
                        while let Ok(newer) = frame_rx.try_recv() {
                            latest = newer;
                        }
                        let (w, h, jpeg) = latest;
                        if protocol::write_msg(
                            &mut writer,
                            &Msg::VideoFrame { width: w, height: h, jpeg },
                        )
                        .is_err()
                        {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                let mut alive = true;
                while let Ok(msg) = ctl_rx.try_recv() {
                    if protocol::write_msg(&mut writer, &msg).is_err() {
                        alive = false;
                        break;
                    }
                }
                if !alive {
                    break;
                }
            }
            let _ = writer.flush();
        })
        .expect("spawn lc-conn-writer");

    // 剪贴板同步线程
    let clip_stop = stop.clone();
    let clip_shared = shared.clone();
    let clip_ctl = ctl_tx.clone();
    let clip_handle = std::thread::Builder::new()
        .name("lc-conn-clip".into())
        .spawn(move || clipboard_loop(clip_shared, clip_ctl, clip_stop))
        .expect("spawn lc-conn-clip");

    // reader 主循环：输入执行 + 控制应答
    let mut reason = "对端已断开".to_string();
    let mut input_err: Option<String> = None;
    let _ = reader.set_read_timeout(Some(Duration::from_secs(30)));    loop {
        if stop.load(Ordering::Relaxed) {
            reason = "对端已断开".to_string();
            break;
        }
        match protocol::read_msg(&mut reader) {
            Ok(msg) => match msg {
                Msg::Mouse(m) => {
                    if !session.view_only.load(Ordering::Relaxed) {
                        if let Err(e) = executor.handle_mouse(&m) {
                            input_err = Some(e);
                            break;
                        }
                    }
                }
                Msg::Wheel(w) => {
                    if !session.view_only.load(Ordering::Relaxed) {
                        if let Err(e) = executor.handle_wheel(&w) {
                            input_err = Some(e);
                            break;
                        }
                    }
                }
                Msg::Key(k) => {
                    if !session.view_only.load(Ordering::Relaxed) {
                        if let Err(e) = executor.handle_key(&k) {
                            input_err = Some(e);
                            break;
                        }
                    }
                }
                Msg::Ping(p) => {
                    let _ = ctl_tx.send(Msg::Pong(p));
                }
                Msg::Clipboard(c) => {
                    crate::clipboard_sync::apply_incoming(&c.text);
                }
                Msg::Bye(b) => {
                    reason = format!("对端主动断开: {}", b.reason);
                    break;
                }
                _ => {}
            },
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => {
                reason = "连接已断开".to_string();
                break;
            }
        }    }

    stop.store(true, Ordering::Relaxed);
    let _ = ctl_tx.send(Msg::Bye(protocol::ByeMsg { reason: "会话结束".into() }));
    let _ = capture_handle.join();
    let _ = writer_handle.join();
    let _ = clip_handle.join();
    executor.release_all();
    shared.controlled_count.fetch_sub(1, Ordering::Relaxed);

    if let Some(e) = input_err {
        return Err(e);
    }
    Ok(reason)
}

/// 双向剪贴板同步（被控端）：本地变更 → 推送；远端文本 → 应用。
fn clipboard_loop(shared: Arc<AppShared>, ctl_tx: Sender<Msg>, stop: Arc<AtomicBool>) {
    if !shared.config.lock().unwrap().sync_clipboard {
        return;
    }
    let mut last_local: Option<String> = crate::clipboard_sync::get_text();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(1500));
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Some(text) = crate::clipboard_sync::get_text() {
            let changed = last_local.as_deref() != Some(text.as_str());
            if changed && !text.is_empty() {
                last_local = Some(text.clone());
                let _ = ctl_tx.send(Msg::Clipboard(protocol::ClipboardMsg { text }));
            } else if changed {
                last_local = Some(text);
            }
        }
    }
}
