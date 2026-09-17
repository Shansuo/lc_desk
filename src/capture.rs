//! 被控端屏幕捕获：抓帧线程 → 编码线程 → JPEG 交给发送通道。
//! 抓帧只保留最新帧，编码独立进行，互不阻塞。

use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageBuffer};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// 待编码的原始帧
struct RawFrame {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

/// 编码完成的 JPEG 帧接收端：(宽, 高, JPEG 数据)
pub type FrameTx = SyncSender<(u32, u32, Vec<u8>)>;

pub fn spawn_capture(
    live: LiveCfg,
    tx: FrameTx,
    stop: Arc<AtomicBool>,
    on_error: Box<dyn Fn(String) + Send>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || run_capture(live, tx, stop, on_error))
        .expect("spawn capture thread")
}

fn run_capture(live: LiveCfg, tx: FrameTx, stop: Arc<AtomicBool>, on_error: Box<dyn Fn(String) + Send>) {
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
        try_recorder_loop(&monitor, &live, &raw_tx, &stop)
    } else {
        Err(anyhow::anyhow!("macOS 固定使用无光标的轮询捕获"))
    };
    if let Err(e) = result {
        if use_recorder {
            log::warn!("推模式捕获不可用（{e}），退回轮询截图模式");
        }
        poll_capture_loop(&monitor, &live, &raw_tx, stop, on_error);
    }
    drop(raw_tx);
    let _ = encoder.join();
}

/// xcap VideoRecorder：推送帧，取最新帧并按 fps 节流投递给编码线程。
fn try_recorder_loop(
    monitor: &xcap::Monitor,
    live: &LiveCfg,
    raw_tx: &SyncSender<RawFrame>,
    stop: &Arc<AtomicBool>,
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
        let frame_interval = Duration::from_secs_f64(1.0 / cfg.fps.max(1) as f64);
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(mut frame) => {
                // 只保留最新帧
                while let Ok(newer) = rx.try_recv() {
                    frame = newer;
                }
                if last_sent.elapsed() < frame_interval {
                    continue;
                }
                let expected = frame.width as usize * frame.height as usize * 4;
                if frame.raw.len() != expected {
                    continue; // 行对齐等异常帧直接丢弃
                }
                if raw_tx
                    .try_send(RawFrame { rgba: frame.raw, width: frame.width, height: frame.height })
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
    on_error: Box<dyn Fn(String) + Send>,
) {
    let mut error_reported = false;
    while !stop.load(Ordering::Relaxed) {
        let frame_interval = {
            let c = read_cfg(live);
            Duration::from_secs_f64(1.0 / c.fps.max(1) as f64)
        };
        let started = Instant::now();
        match monitor.capture_image() {
            Ok(img) => {
                error_reported = false;
                let (w, h) = (img.width(), img.height());
                let _ = raw_tx.try_send(RawFrame { rgba: img.into_raw(), width: w, height: h });
            }
            Err(e) => {
                if !error_reported {
                    error_reported = true;
                    on_error(format!("屏幕捕获失败：{e}。请检查屏幕录制权限。"));
                }
            }
        }
        let elapsed = started.elapsed();
        if elapsed < frame_interval {
            spin_sleep(frame_interval - elapsed, &stop);
        }
    }
}

/// 编码循环：降采样 → RGB → JPEG → 发送通道。
fn encode_loop(live: LiveCfg, rx: std::sync::mpsc::Receiver<RawFrame>, tx: FrameTx, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(frame) => {
                let cfg = read_cfg(&live);
                encode_and_send(&frame.rgba, frame.width, frame.height, cfg, &tx);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn encode_and_send(rgba: &[u8], width: u32, height: u32, cfg: CaptureConfig, tx: &FrameTx) -> bool {
    // 需要缩放时先降采样。fir 用 SIMD 加速，Retina→1920 缩放约 11ms，
    // 纯 Rust 的 imageops::resize 要 48ms，是帧率的主要瓶颈。
    let (rgb, w, h) = if width > cfg.max_width && cfg.max_width > 0 {
        use fast_image_resize::images::Image as FirImage;
        use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
        let nw = cfg.max_width;
        let nh = ((height as f64 * nw as f64 / width as f64).round() as u32).max(1);
        match FirImage::from_vec_u8(width, height, rgba.to_vec(), PixelType::U8x4) {
            Ok(src) => {
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
            Err(e) => {
                log::warn!("fir 构建源图失败，退回 imageops: {e}");
                imageops_fallback(rgba, width, height, nw, nh)
            }
        }
    } else {
        (rgba_to_rgb(rgba), width, height)
    };

    let mut jpeg = Vec::with_capacity(rgb.len() / 4);
    let mut enc = JpegEncoder::new_with_quality(&mut jpeg, cfg.jpeg_quality.clamp(10, 95));
    if let Err(e) = enc.encode(&rgb, w, h, ExtendedColorType::Rgb8) {
        log::warn!("JPEG 编码失败: {e}");
        return false;
    }
    tx.try_send((w, h, jpeg)).is_ok()
}

/// fir 不可用时的兜底缩放（纯 Rust，较慢但正确）。
fn imageops_fallback(rgba: &[u8], width: u32, height: u32, nw: u32, nh: u32) -> (Vec<u8>, u32, u32) {
    let img: ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, rgba.to_vec()).expect("frame buffer");
    let resized = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    (rgba_to_rgb(resized.as_raw()), nw, nh)
}

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for px in rgba.chunks_exact(4) {
        rgb.push(px[0]);
        rgb.push(px[1]);
        rgb.push(px[2]);
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
}
