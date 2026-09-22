//! 被控端屏幕捕获：抓帧线程 → 编码线程 → JPEG 交给发送通道。
//! 抓帧只保留最新帧，编码独立进行，互不阻塞。
//!
//! 延迟设计要点：
//! - 用户正在操作时解除帧率节流（见 [`frame_interval`]），让画面尽快反映操作；
//!   空闲后再回落到用户设定的帧率以节省带宽与 CPU。
//! - 每帧带上抓帧时刻，供发送端实测「捕获 → 写出」耗时并回填给主控端，
//!   这样界面上显示的延迟是可验证的真实数字，而不是估算。

use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageBuffer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct CaptureConfig {
    pub fps: u32,
    pub jpeg_quality: u8,
    /// 宽度超过该值则等比缩小（降低编码与网络压力）
    pub max_width: u32,
}

/// 会话中的抓帧/编码循环每帧实时读取配置，用户在设置里调整
/// 帧率/画质/宽度立即生效，无需重开会话。
pub type LiveCfg = std::sync::Arc<std::sync::Mutex<crate::config::Config>>;

/// 近 `ACTIVE_WINDOW_MS` 内收到过键鼠输入，即视为「用户正在操作」。
const ACTIVE_WINDOW_MS: u64 = 600;
/// 操作期间的抓帧间隔上限。刻意压到低于任何合理帧率，
/// 让节奏由「捕获 + 编码 + 发送」的实际速度决定，而不是人为等待。
const ACTIVE_INTERVAL: Duration = Duration::from_millis(8);
/// 在流水线里滞留超过该时长的帧直接丢弃：编码一帧旧画面毫无意义，
/// 只会占住编码线程，让更新的帧更晚出发。
const STALE_DROP_MS: u64 = 250;

/// 抓帧完成、等待编码的原始帧。
struct RawFrame {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    /// 抓帧时刻（epoch 毫秒）
    captured_ms: u64,
}

/// 编码完成的帧，交给发送线程。
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
    /// 抓帧时刻（epoch 毫秒），发送端据此实测被控端侧整段处理耗时
    pub captured_ms: u64,
}

/// 编码完成的帧接收端
pub type FrameTx = SyncSender<Frame>;

pub fn read_cfg(live: &LiveCfg) -> CaptureConfig {
    live.lock()
        .map(|c| CaptureConfig {
            fps: c.fps,
            jpeg_quality: c.jpeg_quality,
            max_width: c.max_width,
        })
        .unwrap_or(CaptureConfig {
            fps: crate::config::DEFAULT_FPS,
            jpeg_quality: crate::config::DEFAULT_QUALITY,
            max_width: crate::config::DEFAULT_MAX_WIDTH,
        })
}

/// 抓帧节流间隔。
///
/// 旧实现无论用户是否在操作都按设定帧率（默认 24fps）固定等待，这让每帧
/// 天然带上最多一整个帧间隔（41ms）的陈旧度 —— 而这段时间恰恰是操作最
/// 需要即时反馈的时候。现在只要 `activity` 显示近期有键鼠输入，就把间隔
/// 压到 `ACTIVE_INTERVAL`，出帧节奏交给流水线自身速度决定：交互期间延迟
/// 优先，空闲时再省带宽与 CPU。
fn frame_interval(cfg: &CaptureConfig, activity: &AtomicU64) -> Duration {
    let configured = Duration::from_secs_f64(1.0 / cfg.fps.max(1) as f64);
    let last = activity.load(Ordering::Relaxed);
    if last == 0 {
        return configured;
    }
    let now = crate::platform::now_millis();
    if now.saturating_sub(last) < ACTIVE_WINDOW_MS {
        configured.min(ACTIVE_INTERVAL)
    } else {
        configured
    }
}

pub fn spawn_capture(
    live: LiveCfg,
    tx: FrameTx,
    stop: Arc<AtomicBool>,
    activity: Arc<AtomicU64>,
    on_error: Box<dyn Fn(String) + Send>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || run_capture(live, tx, stop, activity, on_error))
        .expect("spawn capture thread")
}

fn run_capture(
    live: LiveCfg,
    tx: FrameTx,
    stop: Arc<AtomicBool>,
    activity: Arc<AtomicU64>,
    on_error: Box<dyn Fn(String) + Send>,
) {
    let monitors = match xcap::Monitor::all() {
        Ok(m) if !m.is_empty() => m,
        _ => {
            on_error("找不到可用的显示器".into());
            return;
        }
    };
    // 优先主显示器
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .cloned()
        .unwrap_or_else(|| monitors[0].clone());

    // 编码线程：抓帧线程 → raw_tx → 编码 → tx
    let (raw_tx, raw_rx) = sync_channel::<RawFrame>(1);
    let enc_stop = stop.clone();
    let enc_live = live.clone();
    let encoder = std::thread::Builder::new()
        .name("capture-encoder".into())
        .spawn(move || encode_loop(enc_live, raw_rx, tx, enc_stop))
        .expect("spawn capture-encoder");

    // 首选推模式（DXGI/WGC 桌面复制）。注意：macOS 的 VideoRecorder 会把远端
    // 光标烧进画面（xcap 硬编码 setCapturesCursor(true)），导致控制端出现
    // 「画面内慢速光标 + 本地快速光标」双光标，因此 macOS 直接用轮询截图
    // （CGWindowListCreateImage，不含光标），由控制端本地光标唯一代表位置。
    let use_recorder = !cfg!(target_os = "macos");
    let result = if use_recorder {
        try_recorder_loop(&monitor, &live, &raw_tx, &stop, &activity)
    } else {
        Err(anyhow::anyhow!("macOS 固定使用无光标的轮询捕获"))
    };
    if let Err(e) = result {
        if use_recorder {
            log::warn!("推模式捕获不可用（{e}），退回轮询截图模式");
        }
        poll_capture_loop(&monitor, &live, &raw_tx, stop, activity, on_error);
    }
    drop(raw_tx);
    let _ = encoder.join();
}

/// xcap VideoRecorder：推送帧，取最新帧并按当前节流间隔投递给编码线程。
fn try_recorder_loop(
    monitor: &xcap::Monitor,
    live: &LiveCfg,
    raw_tx: &SyncSender<RawFrame>,
    stop: &Arc<AtomicBool>,
    activity: &AtomicU64,
) -> anyhow::Result<()> {
    let (recorder, rx) = monitor.video_recorder()?;
    recorder.start()?;
    let mut last_sent = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = recorder.stop();
            return Ok(());
        }
        let cfg = read_cfg(live);
        let interval = frame_interval(&cfg, activity);
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(mut frame) => {
                // 只保留最新帧
                while let Ok(newer) = rx.try_recv() {
                    frame = newer;
                }
                // 节流未到点就丢掉这一帧，下一轮再取更新的：
                // 绝不把「已经等了一会儿的旧帧」发出去。
                if last_sent.elapsed() < interval {
                    continue;
                }
                let expected = frame.width as usize * frame.height as usize * 4;
                if frame.raw.len() != expected {
                    continue; // 行对齐等异常帧直接丢弃
                }
                if raw_tx
                    .try_send(RawFrame {
                        rgba: frame.raw,
                        width: frame.width,
                        height: frame.height,
                        captured_ms: crate::platform::now_millis(),
                    })
                    .is_ok()
                {
                    last_sent = Instant::now();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("video recorder stream closed");
            }
        }
    }
}

/// 轮询截图兜底。
fn poll_capture_loop(
    monitor: &xcap::Monitor,
    live: &LiveCfg,
    raw_tx: &SyncSender<RawFrame>,
    stop: Arc<AtomicBool>,
    activity: Arc<AtomicU64>,
    on_error: Box<dyn Fn(String) + Send>,
) {
    let mut error_reported = false;
    while !stop.load(Ordering::Relaxed) {
        let interval = {
            let c = read_cfg(live);
            frame_interval(&c, &activity)
        };
        let started = Instant::now();
        // 以「开始抓」为时间原点：抓屏本身就要 10~30ms，
        // 这段耗时同样属于用户感知到的延迟，必须算进去。
        let captured_ms = crate::platform::now_millis();
        match monitor.capture_image() {
            Ok(img) => {
                error_reported = false;
                let (w, h) = (img.width(), img.height());
                let _ = raw_tx.try_send(RawFrame {
                    rgba: img.into_raw(),
                    width: w,
                    height: h,
                    captured_ms,
                });
            }
            Err(e) => {
                if !error_reported {
                    error_reported = true;
                    on_error(format!("屏幕捕获失败：{e}。请检查屏幕录制权限。"));
                }
            }
        }
        let elapsed = started.elapsed();
        if elapsed < interval {
            spin_sleep(interval - elapsed, &stop);
        }
    }
}

/// 编码循环：降采样 → 去 alpha → JPEG → 发送通道。
fn encode_loop(
    live: LiveCfg,
    rx: std::sync::mpsc::Receiver<RawFrame>,
    tx: FrameTx,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(frame) => {
                let cfg = read_cfg(&live);
                encode_and_send(frame, cfg, &tx);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn encode_and_send(frame: RawFrame, cfg: CaptureConfig, tx: &FrameTx) -> bool {
    // 排队太久才轮到的帧没有编码价值：被控端画面早已更新，编完发出去只会
    // 占带宽，还会挤掉后面更新的帧。
    if crate::platform::now_millis().saturating_sub(frame.captured_ms) > STALE_DROP_MS {
        return false;
    }
    let (rgb, w, h) = resize_for_encode(&frame.rgba, frame.width, frame.height, cfg);

    let mut jpeg = Vec::with_capacity(rgb.len() / 4);
    let mut enc = JpegEncoder::new_with_quality(&mut jpeg, cfg.jpeg_quality.clamp(10, 95));
    if let Err(e) = enc.encode(&rgb, w, h, ExtendedColorType::Rgb8) {
        log::warn!("JPEG 编码失败: {e}");
        return false;
    }
    tx.try_send(Frame { width: w, height: h, jpeg, captured_ms: frame.captured_ms })
        .is_ok()
}

/// 需要缩放时先降采样。fir 用 SIMD 加速，Retina→1920 缩放约 11ms，
/// 纯 Rust 的 imageops::resize 要 48ms，是帧率的主要瓶颈。
fn resize_for_encode(rgba: &[u8], width: u32, height: u32, cfg: CaptureConfig) -> (Vec<u8>, u32, u32) {
    if cfg.max_width == 0 || width <= cfg.max_width {
        return (rgba_to_rgb(rgba), width, height);
    }

    use fast_image_resize::images::Image as FirImage;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    let nw = cfg.max_width;
    let nh = ((height as f64 * nw as f64 / width as f64).round() as u32).max(1);
    let src = match FirImage::from_vec_u8(width, height, rgba.to_vec(), PixelType::U8x4) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("fir 构建源图失败，退回 imageops: {e}");
            return imageops_fallback(rgba, width, height, nw, nh);
        }
    };
    let mut dst = FirImage::new(nw, nh, PixelType::U8x4);
    let mut resizer = Resizer::new();
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    match resizer.resize(&src, &mut dst, Some(&opts)) {
        Ok(()) => (rgba_to_rgb(&dst.into_vec()), nw, nh),
        Err(e) => {
            log::warn!("fir 缩放失败，退回 imageops: {e}");
            imageops_fallback(rgba, width, height, nw, nh)
        }
    }
}

/// fir 不可用时的兜底缩放（纯 Rust，较慢但正确）。
fn imageops_fallback(rgba: &[u8], width: u32, height: u32, nw: u32, nh: u32) -> (Vec<u8>, u32, u32) {
    let img: ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, rgba.to_vec()).expect("frame buffer");
    let resized = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    (rgba_to_rgb(resized.as_raw()), nw, nh)
}

/// 去掉 alpha 通道。用 extend_from_slice 让编译器把每像素 3 字节的拷贝
/// 合成一次宽写，比逐字节 push（每次都要检查容量）快一截。
fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[..3]);
    }
    rgb
}

/// 可中断的睡眠（Windows 精度较差，用短睡循环）。
fn spin_sleep(d: Duration, stop: &Arc<AtomicBool>) {
    let deadline = Instant::now() + d;
    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// 会话中修改配置后，read_cfg 必须立刻反映新值（帧率/画质实时生效的核心契约）。
    #[test]
    fn test_read_cfg_is_live() {
        let mut base = crate::config::Config::default();
        base.fps = 10;
        base.jpeg_quality = 50;
        base.max_width = 1280;
        let live = Arc::new(Mutex::new(base));
        assert_eq!(read_cfg(&live).fps, 10);
        assert_eq!(read_cfg(&live).jpeg_quality, 50);
        assert_eq!(read_cfg(&live).max_width, 1280);

        // 模拟用户在设置里拖动滑条 → 写入 config → 编码线程下一帧读到新值
        {
            let mut c = live.lock().unwrap();
            c.fps = 30;
            c.jpeg_quality = 90;
            c.max_width = 3840;
        }
        let after = read_cfg(&live);
        assert_eq!(after.fps, 30);
        assert_eq!(after.jpeg_quality, 90);
        assert_eq!(after.max_width, 3840);
    }

    /// 空闲时严格按用户设定的帧率节流。
    #[test]
    fn test_interval_idle_follows_config() {
        let cfg = CaptureConfig { fps: 10, jpeg_quality: 70, max_width: 1920 };
        let idle = AtomicU64::new(0);
        assert_eq!(frame_interval(&cfg, &idle), Duration::from_millis(100));
    }

    /// 有输入时解除节流：这是「操作跟手」的关键，设定帧率不能再是下限。
    #[test]
    fn test_interval_active_overrides_low_fps() {
        let cfg = CaptureConfig { fps: 5, jpeg_quality: 70, max_width: 1920 };
        let active = AtomicU64::new(crate::platform::now_millis());
        assert_eq!(
            frame_interval(&cfg, &active),
            ACTIVE_INTERVAL,
            "正在操作时必须无视低帧率设定，否则每帧白带 200ms 陈旧度"
        );
    }

    /// 输入停止超过活动窗口后必须回落设定帧率（否则带宽/CPU 白白消耗）。
    #[test]
    fn test_interval_falls_back_after_idle() {
        let cfg = CaptureConfig { fps: 5, jpeg_quality: 70, max_width: 1920 };
        let now = crate::platform::now_millis();
        let stale = AtomicU64::new(now.saturating_sub(ACTIVE_WINDOW_MS + 1));
        assert_eq!(frame_interval(&cfg, &stale), Duration::from_millis(200));
    }

    /// 已高于活动间隔的帧率设定不应被反向拉长。
    #[test]
    fn test_interval_never_lengthens_when_active() {
        let cfg = CaptureConfig { fps: 60, jpeg_quality: 70, max_width: 1920 };
        let active = AtomicU64::new(crate::platform::now_millis());
        assert_eq!(frame_interval(&cfg, &active), ACTIVE_INTERVAL);
    }

    #[test]
    fn test_rgba_to_rgb_drops_alpha() {
        let rgba = vec![1, 2, 3, 255, 4, 5, 6, 0];
        assert_eq!(rgba_to_rgb(&rgba), vec![1, 2, 3, 4, 5, 6]);
    }

    /// 不缩放时也要正确去掉 alpha（像素格式必须与 JPEG 编码器约定一致）。
    #[test]
    fn test_resize_for_encode_without_scaling() {
        let cfg = CaptureConfig { fps: 24, jpeg_quality: 70, max_width: 1920 };
        let rgba = vec![9, 8, 7, 255, 6, 5, 4, 255];
        let (rgb, w, h) = resize_for_encode(&rgba, 2, 1, cfg);
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgb, vec![9, 8, 7, 6, 5, 4]);
    }

    /// 手工基准：被控端「降采样 → 去 alpha → JPEG 编码」的耗时构成。
    /// 运行：`cargo test bench_encode -- --ignored --nocapture`
    ///
    /// 这个数字就是主控端界面上「被控端 XXms」的来源，调整画质/分辨率
    /// 设置后可重跑，据此判断该往哪个方向调。
    #[test]
    #[ignore = "手工基准，按需运行"]
    fn bench_encode_pipeline() {
        fn synth(width: u32, height: u32) -> Vec<u8> {
            // 桌面画面多为文字与色块，这里用「渐变 + 棋盘」近似其熵特征
            let mut buf = vec![0u8; (width * height * 4) as usize];
            for y in 0..height {
                for x in 0..width {
                    let i = ((y * width + x) * 4) as usize;
                    let checker = if ((x / 8) + (y / 8)) % 2 == 0 { 20 } else { 0 };
                    buf[i] = (x % 251) as u8;
                    buf[i + 1] = (y % 241) as u8;
                    buf[i + 2] = (((x + y) % 233) as u8).saturating_add(checker);
                    buf[i + 3] = 255;
                }
            }
            buf
        }

        // (源宽, 源高, 画质, 最大宽) —— 覆盖 Retina 缩放与原生 1080p 两种情形
        let cases = [
            (2560u32, 1600u32, 70u8, 1920u32),
            (1920, 1080, 70, 1920),
            (1920, 1080, 45, 1920),
            (3840, 2160, 70, 1920),
        ];
        for (sw, sh, q, mw) in cases {
            let rgba = synth(sw, sh);
            let cfg = CaptureConfig { fps: 24, jpeg_quality: q, max_width: mw };

            // 预热一次，避免首帧的分配开销污染结果
            let _ = resize_for_encode(&rgba, sw, sh, cfg);

            let t0 = Instant::now();
            let (rgb, w, h) = resize_for_encode(&rgba, sw, sh, cfg);
            let resize_ms = t0.elapsed().as_secs_f64() * 1000.0;

            let t1 = Instant::now();
            let mut jpeg = Vec::new();
            let mut enc = JpegEncoder::new_with_quality(&mut jpeg, cfg.jpeg_quality);
            enc.encode(&rgb, w, h, ExtendedColorType::Rgb8).unwrap();
            let encode_ms = t1.elapsed().as_secs_f64() * 1000.0;

            println!(
                "{sw}×{sh} q{q} → {w}×{h}: 降采样+去alpha {resize_ms:.1}ms + JPEG编码 {encode_ms:.1}ms \
                 = {:.1}ms  ({:.0} KB, {:.1} Mbps@24fps)",
                resize_ms + encode_ms,
                jpeg.len() as f64 / 1024.0,
                jpeg.len() as f64 * 8.0 * 24.0 / 1_000_000.0
            );
        }
    }
}
