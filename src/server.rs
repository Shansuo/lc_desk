//! 被控端：TCP 监听、握手鉴权、本机确认、屏幕流发送、输入执行、剪贴板同步。

use crate::capture::{self};
use crate::input_exec::InputExecutor;
use crate::protocol::{self, Msg};
use crate::state::{tune_stream, AppShared, ControlledSession, UiEvent};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

/// 密码校验失败后的冷却时长（毫秒）。局域网内无 TLS，靠退避防止高速枚举。
const AUTH_COOLDOWN_MS: u64 = 1_500;
/// 会话中读取对端消息的超时（仅用于让循环有机会检查停止标志）
const SESSION_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// 发送线程的等待步长。取得很短是为了让控制消息（Pong / 剪贴板 / Bye）
/// 能及时插空发出：它们都很小，却直接决定交互手感与 RTT 读数。
const WRITER_TICK: Duration = Duration::from_millis(5);

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
    shared.server_ready.store(true, Ordering::Relaxed);
    shared.notify(format!("被控端已就绪（端口 {port}）"));

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = shared.clone();
                if std::thread::Builder::new()
                    .name("lc-conn".into())
                    .spawn(move || handle_conn(shared, stream))
                    .is_err()
                {
                    log::warn!("无法为新的主控端连接创建线程");
                }
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
        // 先按上次失败时间退避，再校验：连续失败之间强制间隔
        shared.await_auth_cooldown();
        match protocol::read_msg(&mut reader) {
            Ok(Msg::Auth(a)) => {
                password_ok = shared.config.lock().unwrap().verify_password(&a.password);
                if !password_ok {
                    shared.note_auth_failure(AUTH_COOLDOWN_MS);
                    log::warn!("来自 {peer_ip} 的密码校验失败");
                }
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
    let peer_name = hello.name.clone();
    match run_session(&shared, stream, reader, view_only, &hello, &peer_ip, session_id) {
        Ok(reason) => {
            log::info!("会话 {session_id} 结束: {reason}");
            let _ = shared.events_tx.send(UiEvent::SessionEnded { peer_name, reason });
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
            message: if accepted {
                "ok".into()
            } else {
                "对端拒绝了本次连接".into()
            },
        }),
    );
    accepted
}

/// 会话主体：抓屏发送 + 输入执行 + 剪贴板同步。
///
/// 计数与清理放在外层，保证任何提前返回的路径都不会漏掉
/// `controlled_count` 的递减（否则主界面会永久显示「正在被 N 台设备控制」）。
fn run_session(
    shared: &Arc<AppShared>,
    stream: TcpStream,
    reader: TcpStream,
    view_only: bool,
    hello: &protocol::Hello,
    peer_ip: &str,
    session_id: u64,
) -> Result<String, String> {
    let executor = InputExecutor::new(shared.config.lock().unwrap().ctrl_as_cmd).map_err(|e| e)?;
    shared.controlled_count.fetch_add(1, Ordering::Relaxed);
    let result = session_loop(shared, executor, stream, reader, view_only, hello, peer_ip, session_id);
    shared.controlled_count.fetch_sub(1, Ordering::Relaxed);
    result
}

fn session_loop(
    shared: &Arc<AppShared>,
    mut executor: InputExecutor,
    stream: TcpStream,
    mut reader: TcpStream,
    view_only: bool,
    hello: &protocol::Hello,
    peer_ip: &str,
    session_id: u64,
) -> Result<String, String> {
    log::info!(
        "会话 {session_id} 开始：{}({}) view_only={view_only}",
        hello.name,
        peer_ip
    );

    let stop = Arc::new(AtomicBool::new(false));

    // 控制消息通道：reader 线程 → writer 线程（Pong / Clipboard / Bye）
    let (ctl_tx, ctl_rx) = mpsc::channel::<Msg>();
    let session = Arc::new(ControlledSession {
        view_only: AtomicBool::new(view_only),
    });

    // 最近一帧「抓帧 → 写出发送缓冲」的实测耗时，随 Pong 回填给主控端
    let pipeline_ms = Arc::new(AtomicU32::new(0));

    // 帧通道（有界，最新帧优先：容量 1，编码完成即替换，避免排队积压延迟）
    let (frame_tx, frame_rx) = mpsc::sync_channel::<capture::Frame>(1);
    // 传配置句柄而非快照：用户在设置里调整帧率/画质/宽度可实时生效
    let capture_stop = stop.clone();
    let notify = shared.clone();
    let capture_handle = capture::spawn_capture(
        shared.config.clone(),
        frame_tx,
        capture_stop,
        shared.last_input_ms.clone(),
        Box::new(move |text| notify.notify(text)),
    );

    // writer 线程：帧 + 控制消息
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            stop.store(true, Ordering::Relaxed);
            let _ = capture_handle.join();
            return Err(format!("clone stream failed: {e}"));
        }
    };
    let writer_stop = stop.clone();
    let writer_pipeline = pipeline_ms.clone();
    let writer_handle = std::thread::Builder::new()
        .name("lc-conn-writer".into())
        .spawn(move || {
            while !writer_stop.load(Ordering::Relaxed) {
                // 1) 控制消息优先。它们小且关乎交互：旧实现「先等帧、写完帧
                //    才处理控制消息」，导致 Pong 被一整帧的发送耗时挡在后面，
                //    界面 RTT 显示成真实链路的几十倍。
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

                // 2) 再发最新一帧（清空通道取最新，避免排队旧帧）
                match frame_rx.recv_timeout(WRITER_TICK) {
                    Ok(mut latest) => {
                        while let Ok(newer) = frame_rx.try_recv() {
                            latest = newer;
                        }
                        let capture::Frame { width, height, jpeg, captured_ms } = latest;
                        if protocol::write_msg(
                            &mut writer,
                            &Msg::VideoFrame { width, height, jpeg },
                        )
                        .is_err()
                        {
                            break;
                        }
                        // 被控端侧整段耗时：抓帧 → 写出发送缓冲。
                        // 主控端收到的延迟里属于被控端的那一段，就是这个值。
                        let cost = crate::platform::now_millis().saturating_sub(captured_ms);
                        writer_pipeline
                            .store(cost.min(u32::MAX as u64) as u32, Ordering::Relaxed);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            // 关键：无论是发送失败还是通道关闭退出，都必须点亮停止标志，
            // 否则抓屏线程与外层的 capture_handle.join() 会永久等待。
            writer_stop.store(true, Ordering::Relaxed);
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
    let _ = reader.set_read_timeout(Some(SESSION_READ_TIMEOUT));
    loop {
        if stop.load(Ordering::Relaxed) {
            reason = "连接已断开".to_string();
            break;
        }
        match protocol::read_msg(&mut reader) {
            Ok(msg) => match msg {
                Msg::Mouse(m) => {
                    shared
                        .last_input_ms
                        .store(crate::platform::now_millis(), Ordering::Relaxed);
                    if !session.view_only.load(Ordering::Relaxed) {
                        if let Err(e) = executor.handle_mouse(&m) {
                            input_err = Some(e);
                            break;
                        }
                    }
                }
                Msg::Wheel(w) => {
                    shared
                        .last_input_ms
                        .store(crate::platform::now_millis(), Ordering::Relaxed);
                    if !session.view_only.load(Ordering::Relaxed) {
                        if let Err(e) = executor.handle_wheel(&w) {
                            input_err = Some(e);
                            break;
                        }
                    }
                }
                Msg::Key(k) => {
                    shared
                        .last_input_ms
                        .store(crate::platform::now_millis(), Ordering::Relaxed);
                    if !session.view_only.load(Ordering::Relaxed) {
                        if let Err(e) = executor.handle_key(&k) {
                            input_err = Some(e);
                            break;
                        }
                    }
                }
                Msg::Ping(p) => {
                    let _ = ctl_tx.send(Msg::Pong(protocol::PingMsg {
                        ts: p.ts,
                        pipeline_ms: pipeline_ms.load(Ordering::Relaxed),
                    }));
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
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = ctl_tx.send(Msg::Bye(protocol::ByeMsg { reason: "会话结束".into() }));
    let _ = capture_handle.join();
    let _ = writer_handle.join();
    let _ = clip_handle.join();
    executor.release_all();

    match input_err {
        Some(e) => Err(e),
        None => Ok(reason),
    }
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
