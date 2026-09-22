//! 被控端屏幕捕获：抓帧线程 → 编码线程 → JPEG 交给发送通道。
//! 抓帧只保留最新帧，编码独立进行，互不阻塞。
//!
//! **延迟设计要点**
//!
//! 1. 用户正在操作时解除帧率节流（见 [`frame_interval`]），空闲后回落。
//! 2. **增量更新**：远控时屏幕上真正变化的区域很小（鼠标、高亮、输入框），
//!    整帧 JPEG 等于把没变的像素也编一遍。这里按图块与上一帧比对，只编码
//!    变化的块 —— 实测 1080p 整帧编码 15ms，而 8 个 128px 图块只要 1ms，
//!    数据量从 245KB 降到 16KB。脏块太多时才退回整帧（那时整帧更划算）。
//! 3. 每帧带上抓帧时刻，供发送端实测「捕获 → 写出」耗时并回填给主控端。

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

/// 图块边长（**缩放后画面**的像素）。网格定在缩放后空间上，
/// 保证所有图块无缝铺满整幅画面，主控端按坐标局部更新即可。
const TILE: u32 = 128;
/// 脏块占比超过该比例就改发整帧：图块越多，每块的 JPEG 头部开销与
/// 「块间无法复用冗余」的损失越大，整帧反而更划算（实测铺满 1080p：
/// 整帧 15.0ms vs 128px 块合计 17.1ms）。
const FULL_FRAME_DIRTY_RATIO: f32 = 0.35;
/// 变化检测的采样步长（像素）。越小越灵敏也越费；4 表示每 4 行取 1 行、
/// 每行每 4 像素取 1 个，足以捕捉光标这类 ~20px 的细微变化。
const DIFF_STRIDE: u32 = 4;

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

/// 一帧的交付形式。
#[derive(Clone, Debug)]
pub enum FrameKind {
    /// 整幅画面（会话首帧、分辨率变化、或对端不支持增量时）
    Full { jpeg: Vec<u8> },
    /// 只有下列图块发生变化
    Rects { rects: Vec<crate::protocol::RectBlock> },
}

/// 编码完成的帧。
pub struct Frame {
    /// 缩放后画面尺寸（主控端据此确定纹理大小）
    pub width: u32,
    pub height: u32,
    pub kind: FrameKind,
    /// 抓帧时刻（epoch 毫秒），发送端据此实测被控端侧整段处理耗时
    pub captured_ms: u64,
}

/// 编码完成的帧接收端
pub type FrameTx = SyncSender<Frame>;

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

/// 缩放后的画面尺寸。
fn scaled_size(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    if max_width == 0 || width <= max_width {
        return (width, height);
    }
    let w = max_width;
    let h = ((height as f64 * w as f64 / width as f64).round() as u32).max(1);
    (w, h)
}

/// 编码任务：整帧，或若干脏图块。
#[derive(Debug)]
enum EncodeJob {
    Full { rgba: Vec<u8>, width: u32, height: u32, captured_ms: u64 },
    Tiles { tiles: Vec<Tile>, width: u32, height: u32, captured_ms: u64 },
}

/// 一个待编码的图块：原始帧上的源区域 + 它应贴在缩放后画面的哪个位置。
#[derive(Debug)]
struct Tile {
    src: (u32, u32, u32, u32), // x, y, w, h（原始分辨率）
    dst: (u32, u32, u32, u32), // x, y, w, h（缩放后）
    rgba: Vec<u8>,
}

/// 抓帧线程持有的上一帧（用于差分）。它代表「主控端已经看到的画面」，
/// 所以每发出一个图块就要把它同步进这里，否则同一块会被反复重发。
struct Shadow {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

pub fn spawn_capture(
    live: LiveCfg,
    tx: FrameTx,
    stop: Arc<AtomicBool>,
    activity: Arc<AtomicU64>,
    // rects_enabled：对端是否支持增量帧（协议版本 ≥ 2）。不支持时一律发整帧。
    rects_enabled: bool,
    on_error: Box<dyn Fn(String) + Send>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || run_capture(live, tx, stop, activity, rects_enabled, on_error))
        .expect("spawn capture thread")
}

fn run_capture(
    live: LiveCfg,
    tx: FrameTx,
    stop: Arc<AtomicBool>,
    activity: Arc<AtomicU64>,
    rects_enabled: bool,
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

    // 编码线程：抓帧线程 → job_tx → 编码 → tx
    let (job_tx, job_rx) = sync_channel::<EncodeJob>(1);
    let enc_stop = stop.clone();
    let enc_live = live.clone();
    let encoder = std::thread::Builder::new()
        .name("capture-encoder".into())
        .spawn(move || encode_loop(enc_live, job_rx, tx, enc_stop))
        .expect("spawn capture-encoder");

    // 首选推模式（DXGI/WGC 桌面复制）。注意：macOS 的 VideoRecorder 会把远端
    // 光标烧进画面（xcap 硬编码 setCapturesCursor(true)），导致控制端出现
    // 「画面内慢速光标 + 本地快速光标」双光标，因此 macOS 直接用轮询截图
    // （CGWindowListCreateImage，不含光标），由控制端本地光标唯一代表位置。
    let use_recorder = !cfg!(target_os = "macos");
    let result = if use_recorder {
        try_recorder_loop(&monitor, &live, &job_tx, &stop, &activity, rects_enabled)
    } else {
        Err(anyhow::anyhow!("macOS 固定使用无光标的轮询捕获"))
    };
    if let Err(e) = result {
        if use_recorder {
            log::warn!("推模式捕获不可用（{e}），退回轮询截图模式");
        }
        poll_capture_loop(&monitor, &live, &job_tx, stop, activity, rects_enabled, on_error);
    }
    drop(job_tx);
    let _ = encoder.join();
}

/// 抓帧 → 差分 → 投递编码任务。两条抓帧路径共用这段逻辑。
struct Differ {
    shadow: Option<Shadow>,
    rects_enabled: bool,
}

impl Differ {
    fn new(rects_enabled: bool) -> Self {
        Self { shadow: None, rects_enabled }
    }

    /// 处理一帧原始画面，产出编码任务（无变化时不产出）。
    fn emit(&mut self, rgba: Vec<u8>, width: u32, height: u32, cfg: &CaptureConfig, captured_ms: u64) -> Option<EncodeJob> {
        let (dw, dh) = scaled_size(width, height, cfg.max_width);
        let need_full = !self.rects_enabled
            || match &self.shadow {
                None => true,                                              // 首帧
                Some(s) => s.width != width || s.height != height,          // 分辨率变了
            };
        if need_full {
            self.shadow = Some(Shadow { rgba: rgba.clone(), width, height });
            return Some(EncodeJob::Full { rgba, width, height, captured_ms });
        }

        let shadow = self.shadow.as_ref().unwrap();
        let Some(tiles) = diff_tiles(&rgba, width, height, shadow, dw, dh) else {
            return None; // 画面无变化
        };
        let total = tiles_x(dw) * tiles_y(dh);
        if tiles.len() as f32 / total.max(1) as f32 > FULL_FRAME_DIRTY_RATIO {
            self.shadow = Some(Shadow { rgba: rgba.clone(), width, height });
            return Some(EncodeJob::Full { rgba, width, height, captured_ms });
        }

        // 把已发出的块同步进影子帧，保证下次比对的是「主控端已看到的画面」
        {
            let s = self.shadow.as_mut().unwrap();
            for t in &tiles {
                blit(&rgba, width, t.src, &mut s.rgba, t.src);
            }
        }
        Some(EncodeJob::Tiles { tiles, width: dw, height: dh, captured_ms })
    }
}

fn tiles_x(w: u32) -> u32 {
    (w + TILE - 1) / TILE
}
fn tiles_y(h: u32) -> u32 {
    (h + TILE - 1) / TILE
}

/// 逐块比对，返回发生变化的图块（已把源像素拷出）。
///
/// 网格定在缩放后画面上（保证无缝覆盖），再按比例映射回原始帧取样比对。
fn diff_tiles(
    cur: &[u8],
    cw: u32,
    ch: u32,
    shadow: &Shadow,
    dw: u32,
    dh: u32,
) -> Option<Vec<Tile>> {
    let sx = cw as f64 / dw as f64;
    let sy = ch as f64 / dh as f64;
    let mut tiles = Vec::new();

    for ty in 0..tiles_y(dh) {
        for tx in 0..tiles_x(dw) {
            let dst = (
                tx * TILE,
                ty * TILE,
                (TILE).min(dw - tx * TILE),
                (TILE).min(dh - ty * TILE),
            );
            // 映射回原始帧：向外取整，宁可多取一点也不要漏掉变化
            let src_x = (dst.0 as f64 * sx).floor() as u32;
            let src_y = (dst.1 as f64 * sy).floor() as u32;
            let src_w = ((dst.0 + dst.2) as f64 * sx).ceil() as u32 - src_x;
            let src_h = ((dst.1 + dst.3) as f64 * sy).ceil() as u32 - src_y;
            let src = (
                src_x.min(cw.saturating_sub(1)),
                src_y.min(ch.saturating_sub(1)),
                src_w.clamp(1, cw - src_x.min(cw - 1)),
                src_h.clamp(1, ch - src_y.min(ch - 1)),
            );
            if !tile_changed(cur, cw, &shadow.rgba, src) {
                continue;
            }
            tiles.push(Tile { src, dst, rgba: extract(cur, cw, src) });
        }
    }
    if tiles.is_empty() {
        None
    } else {
        Some(tiles)
    }
}

/// 抽样比对一块是否变化（忽略 alpha）。
fn tile_changed(cur: &[u8], cw: u32, prev: &[u8], src: (u32, u32, u32, u32)) -> bool {
    let (x0, y0, w, h) = src;
    let row = cw as usize * 4;
    let mut y = y0;
    while y < y0 + h {
        let base = y as usize * row;
        let mut x = x0;
        while x < x0 + w {
            let i = base + x as usize * 4;
            if cur[i] != prev[i] || cur[i + 1] != prev[i + 1] || cur[i + 2] != prev[i + 2] {
                return true;
            }
            x += DIFF_STRIDE;
        }
        y += DIFF_STRIDE;
    }
    false
}

/// 从整帧里拷出一个矩形区域。
fn extract(src: &[u8], sw: u32, rect: (u32, u32, u32, u32)) -> Vec<u8> {
    let (x, y, w, h) = rect;
    let bpp = 4usize;
    let stride = sw as usize * bpp;
    let mut out = Vec::with_capacity(w as usize * h as usize * bpp);
    for row in y..y + h {
        let start = row as usize * stride + x as usize * bpp;
        out.extend_from_slice(&src[start..start + w as usize * bpp]);
    }
    out
}

/// 把一块像素写回影子帧（同步「已发出的内容」）。
fn blit(src: &[u8], sw: u32, rect: (u32, u32, u32, u32), dst: &mut [u8], _dst_rect: (u32, u32, u32, u32)) {
    let (x, y, w, h) = rect;
    let bpp = 4usize;
    let stride = sw as usize * bpp;
    for row in y..y + h {
        let start = row as usize * stride + x as usize * bpp;
        dst[start..start + w as usize * bpp]
            .copy_from_slice(&src[start..start + w as usize * bpp]);
    }
}

/// xcap VideoRecorder：推送帧，取最新帧并按当前节流间隔投递给编码线程。
fn try_recorder_loop(
    monitor: &xcap::Monitor,
    live: &LiveCfg,
    job_tx: &SyncSender<EncodeJob>,
    stop: &Arc<AtomicBool>,
    activity: &AtomicU64,
    rects_enabled: bool,
) -> anyhow::Result<()> {
    let (recorder, rx) = monitor.video_recorder()?;
    recorder.start()?;
    let mut differ = Differ::new(rects_enabled);
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
                let captured_ms = crate::platform::now_millis();
                if let Some(job) = differ.emit(frame.raw, frame.width, frame.height, &cfg, captured_ms) {
                    // 阻塞式投递而不用 try_send：增量帧一旦被丢弃，主控端就
                    // 永远少了那块内容（整帧可以丢旧的取新的，图块不行）。
                    // 这里做成背压：下游忙就等，绝不丢数据。
                    if job_tx.send(job).is_ok() {
                        last_sent = Instant::now();
                    } else {
                        return Ok(()); // 编码线程已退出
                    }
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
    job_tx: &SyncSender<EncodeJob>,
    stop: Arc<AtomicBool>,
    activity: Arc<AtomicU64>,
    rects_enabled: bool,
    on_error: Box<dyn Fn(String) + Send>,
) {
    let mut error_reported = false;
    let mut differ = Differ::new(rects_enabled);
    while !stop.load(Ordering::Relaxed) {
        let (interval, cfg) = {
            let c = read_cfg(live);
            (frame_interval(&c, &activity), c)
        };
        let started = Instant::now();
        // 以「开始抓」为时间原点：抓屏本身就要 10~30ms，
        // 这段耗时同样属于用户感知到的延迟，必须算进去。
        let captured_ms = crate::platform::now_millis();
        match monitor.capture_image() {
            Ok(img) => {
                error_reported = false;
                let (w, h) = (img.width(), img.height());
                if let Some(job) = differ.emit(img.into_raw(), w, h, &cfg, captured_ms) {
                    // 同上：增量帧必须完整送达，用阻塞投递做背压
                    if job_tx.send(job).is_err() {
                        return;
                    }
                }
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

/// 编码循环：整帧或图块 → JPEG → 发送通道。
fn encode_loop(
    live: LiveCfg,
    rx: std::sync::mpsc::Receiver<EncodeJob>,
    tx: FrameTx,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(job) => {
                let cfg = read_cfg(&live);
                if let Some(frame) = encode_job(job, cfg) {
                    // 阻塞投递：宁可让整条流水线慢下来，也不能丢图块
                    if tx.send(frame).is_err() {
                        break;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn encode_job(job: EncodeJob, cfg: CaptureConfig) -> Option<Frame> {
    // 排队太久才轮到的任务没有编码价值：画面早已更新，
    // 编完发出去只会占带宽，还会挤掉后面更新的帧。
    let captured_ms = match &job {
        EncodeJob::Full { captured_ms, .. } | EncodeJob::Tiles { captured_ms, .. } => *captured_ms,
    };
    if crate::platform::now_millis().saturating_sub(captured_ms) > STALE_DROP_MS {
        return None;
    }
    match job {
        EncodeJob::Full { rgba, width, height, captured_ms } => {
            let (dw, dh) = scaled_size(width, height, cfg.max_width);
            let (rgb, w, h) = resize_rgba_to_rgb(&rgba, width, height, dw, dh);
            let jpeg = jpeg_encode(&rgb, w, h, cfg.jpeg_quality)?;
            Some(Frame { width: w, height: h, kind: FrameKind::Full { jpeg }, captured_ms })
        }
        EncodeJob::Tiles { tiles, width, height, captured_ms } => {
            let mut rects = Vec::with_capacity(tiles.len());
            for t in tiles {
                let (dst_w, dst_h) = (t.dst.2, t.dst.3);
                let (rgb, w, h) = resize_rgba_to_rgb(&t.rgba, t.src.2, t.src.3, dst_w, dst_h);
                let jpeg = jpeg_encode(&rgb, w, h, cfg.jpeg_quality)?;
                rects.push(crate::protocol::RectBlock {
                    x: t.dst.0,
                    y: t.dst.1,
                    w,
                    h,
                    jpeg,
                });
            }
            Some(Frame { width, height, kind: FrameKind::Rects { rects }, captured_ms })
        }
    }
}

fn jpeg_encode(rgb: &[u8], w: u32, h: u32, quality: u8) -> Option<Vec<u8>> {
    let mut jpeg = Vec::with_capacity(rgb.len() / 3);
    let mut enc = JpegEncoder::new_with_quality(&mut jpeg, quality.clamp(10, 95));
    match enc.encode(rgb, w, h, ExtendedColorType::Rgb8) {
        Ok(()) => Some(jpeg),
        Err(e) => {
            log::warn!("JPEG 编码失败: {e}");
            None
        }
    }
}

/// 缩放到目标尺寸并去掉 alpha 通道。
///
/// fir 用 SIMD 加速，Retina→1920 整帧缩放约 13ms，纯 Rust 的
/// imageops::resize 要 48ms；但增量更新下只有小图块需要缩放，成本可忽略。
fn resize_rgba_to_rgb(rgba: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> (Vec<u8>, u32, u32) {
    if (sw, sh) == (dw, dh) {
        return (rgba_to_rgb(rgba), dw, dh);
    }
    use fast_image_resize::images::Image as FirImage;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    let src = match FirImage::from_vec_u8(sw, sh, rgba.to_vec(), PixelType::U8x4) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("fir 构建源图失败，退回 imageops: {e}");
            return imageops_fallback(rgba, sw, sh, dw, dh);
        }
    };
    let mut dst = FirImage::new(dw, dh, PixelType::U8x4);
    let mut resizer = Resizer::new();
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear));
    match resizer.resize(&src, &mut dst, Some(&opts)) {
        Ok(()) => (rgba_to_rgb(&dst.into_vec()), dw, dh),
        Err(e) => {
            log::warn!("fir 缩放失败，退回 imageops: {e}");
            imageops_fallback(rgba, sw, sh, dw, dh)
        }
    }
}

/// fir 不可用时的兜底缩放（纯 Rust，较慢但正确）。
fn imageops_fallback(rgba: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> (Vec<u8>, u32, u32) {
    let img: ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(sw, sh, rgba.to_vec()).expect("frame buffer");
    let resized = image::imageops::resize(&img, dw, dh, image::imageops::FilterType::Triangle);
    (rgba_to_rgb(resized.as_raw()), dw, dh)
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

    fn rgba_of(width: u32, height: u32, fill: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            v.extend_from_slice(&[fill, fill, fill, 255]);
        }
        v
    }

    #[test]
    fn test_scaled_size() {
        assert_eq!(scaled_size(1920, 1080, 1920), (1920, 1080));
        assert_eq!(scaled_size(3840, 2160, 1920), (1920, 1080));
        assert_eq!(scaled_size(2560, 1600, 1920), (1920, 1200));
        assert_eq!(scaled_size(100, 100, 0), (100, 100), "max_width=0 表示不限制");
    }

    /// 首帧必须是整帧：主控端要靠它建立纹理基准。
    #[test]
    fn test_first_frame_is_full() {
        let mut d = Differ::new(true);
        let cfg = CaptureConfig { fps: 24, jpeg_quality: 70, max_width: 1920 };
        let job = d.emit(rgba_of(256, 256, 10), 256, 256, &cfg, 0).expect("首帧应产出任务");
        assert!(matches!(job, EncodeJob::Full { .. }), "首帧必须是整帧");
    }

    /// 对端不支持增量时永远发整帧（否则旧版主控端读不出画面）。
    #[test]
    fn test_rects_disabled_always_full() {
        let mut d = Differ::new(false);
        let cfg = CaptureConfig { fps: 24, jpeg_quality: 70, max_width: 1920 };
        d.emit(rgba_of(256, 256, 10), 256, 256, &cfg, 0);
        for fill in [10, 20, 30] {
            let job = d.emit(rgba_of(256, 256, fill), 256, 256, &cfg, 0).expect("应有任务");
            assert!(matches!(job, EncodeJob::Full { .. }), "不支持增量时应始终整帧");
        }
    }

    /// 画面完全没变时不产出任何帧（省掉整条编码与发送链路）。
    #[test]
    fn test_unchanged_frame_emits_nothing() {
        let mut d = Differ::new(true);
        let cfg = CaptureConfig { fps: 24, jpeg_quality: 70, max_width: 1920 };
        d.emit(rgba_of(256, 256, 10), 256, 256, &cfg, 0);
        assert!(d.emit(rgba_of(256, 256, 10), 256, 256, &cfg, 1).is_none());
    }

    /// 只变一小块时只发这一块，且坐标落在缩放后画面的正确位置。
    #[test]
    fn test_small_change_emits_single_tile() {
        let mut d = Differ::new(true);
        let cfg = CaptureConfig { fps: 24, jpeg_quality: 70, max_width: 1920 };
        let (w, h) = (512u32, 512u32);
        d.emit(rgba_of(w, h, 10), w, h, &cfg, 0);

        // 只改第二块（缩放后坐标 128..256 那一格）里的几个像素
        let mut cur = rgba_of(w, h, 10);
        for y in 140..160u32 {
            for x in 140..160u32 {
                let i = ((y * w + x) * 4) as usize;
                cur[i..i + 3].copy_from_slice(&[200, 200, 200]);
            }
        }
        let job = d.emit(cur, w, h, &cfg, 1).expect("应产出任务");
        match job {
            EncodeJob::Tiles { tiles, width, height, .. } => {
                assert_eq!((width, height), (512, 512));
                assert_eq!(tiles.len(), 1, "只有一块变化，不该带上其它块");
                assert_eq!(tiles[0].dst, (128, 128, 128, 128));
            }
            other => panic!("应为图块任务，实际 {other:?}"),
        }
    }

    /// 同一块变化被发出后不应重复发送：影子帧必须同步更新。
    #[test]
    fn test_same_change_is_not_resent() {
        let mut d = Differ::new(true);
        let cfg = CaptureConfig { fps: 24, jpeg_quality: 70, max_width: 1920 };
        let (w, h) = (512u32, 512u32);
        let mut cur = rgba_of(w, h, 10);
        d.emit(cur.clone(), w, h, &cfg, 0);
        for y in 140..160u32 {
            for x in 140..160u32 {
                let i = ((y * w + x) * 4) as usize;
                cur[i..i + 3].copy_from_slice(&[200, 200, 200]);
            }
        }
        assert!(d.emit(cur.clone(), w, h, &cfg, 1).is_some(), "首次变化应发出");
        assert!(d.emit(cur.clone(), w, h, &cfg, 2).is_none(), "同样的画面不该重发");
    }

    /// 大范围变化（超过阈值）应退回整帧：那时整帧比逐块编码更划算。
    #[test]
    fn test_large_change_falls_back_to_full() {
        let mut d = Differ::new(true);
        let cfg = CaptureConfig { fps: 24, jpeg_quality: 70, max_width: 1920 };
        let (w, h) = (512u32, 512u32);
        d.emit(rgba_of(w, h, 10), w, h, &cfg, 0);
        let job = d.emit(rgba_of(w, h, 200), w, h, &cfg, 1).expect("应产出任务");
        assert!(matches!(job, EncodeJob::Full { .. }), "全屏变化应退回整帧");
    }

    /// 分辨率变化必须重发整帧，否则主控端纹理尺寸与图块坐标对不上。
    #[test]
    fn test_resolution_change_forces_full() {
        let mut d = Differ::new(true);
        let cfg = CaptureConfig { fps: 24, jpeg_quality: 70, max_width: 1920 };
        d.emit(rgba_of(256, 256, 10), 256, 256, &cfg, 0);
        let job = d.emit(rgba_of(512, 512, 10), 512, 512, &cfg, 1).expect("应产出任务");
        assert!(matches!(job, EncodeJob::Full { .. }));
    }

    /// 抽样比对必须能抓到光标级别（约 20px）的细微变化，否则会漏帧。
    #[test]
    fn test_diff_catches_cursor_sized_change() {
        let (w, h) = (256u32, 256u32);
        let prev = rgba_of(w, h, 10);
        let mut cur = prev.clone();
        // 只改 20×20 的一小片
        for y in 100..120u32 {
            for x in 100..120u32 {
                let i = ((y * w + x) * 4) as usize;
                cur[i] = 99;
            }
        }
        assert!(
            tile_changed(&cur, w, &prev, (96, 96, 32, 32)),
            "步长 {DIFF_STRIDE} 的抽样必须能发现 20px 级变化"
        );
        assert!(!tile_changed(&prev, w, &prev, (96, 96, 32, 32)), "未变化应返回 false");
    }

    #[test]
    fn test_extract_and_blit_roundtrip() {
        let (w, h) = (64u32, 64u32);
        let mut src = rgba_of(w, h, 7);
        for y in 8..16u32 {
            for x in 8..16u32 {
                let i = ((y * w + x) * 4) as usize;
                src[i] = 77;
            }
        }
        let piece = extract(&src, w, (8, 8, 8, 8));
        assert_eq!(piece.len(), 8 * 8 * 4);
        assert_eq!(piece[0..4], [77, 7, 7, 255], "extract 应拷出该块的首个像素");

        // blit 只负责把指定区域同步过去，其余部分不动
        let mut dst = rgba_of(w, h, 0);
        blit(&src, w, (8, 8, 8, 8), &mut dst, (8, 8, 8, 8));
        for y in 8..16u32 {
            for x in 8..16u32 {
                let i = ((y * w + x) * 4) as usize;
                assert_eq!(dst[i..i + 4], src[i..i + 4], "({x},{y}) 应与源一致");
            }
        }
        // 区域外保持原值
        let outside = (0usize) * 4;
        assert_eq!(dst[outside..outside + 4], [0, 0, 0, 255]);
    }

    /// 图块网格必须无缝铺满缩放后画面（主控端按坐标局部更新，留缝就是花屏）。
    #[test]
    fn test_tile_grid_covers_whole_frame() {
        for (dw, dh) in [(1920u32, 1080u32), (800, 600), (1921, 1081)] {
            let mut covered = vec![0u32; (dw * dh) as usize];
            for ty in 0..tiles_y(dh) {
                for tx in 0..tiles_x(dw) {
                    let (x, y, w, h) = (
                        tx * TILE,
                        ty * TILE,
                        TILE.min(dw - tx * TILE),
                        TILE.min(dh - ty * TILE),
                    );
                    for yy in y..y + h {
                        for xx in x..x + w {
                            covered[(yy * dw + xx) as usize] += 1;
                        }
                    }
                }
            }
            assert!(
                covered.iter().all(|&c| c == 1),
                "{dw}×{dh} 的网格有重叠或缝隙"
            );
        }
    }

    /// 端到端（不联网）：差分 → 编码 → 解码，验证图块**坐标与内容**都能对上。
    ///
    /// 增量更新最怕的就是坐标算错：贴错位置的画面比花屏更难发现。
    #[test]
    fn test_tiles_survive_encode_decode() {
        let (w, h) = (1024u32, 768u32);
        let cfg = CaptureConfig { fps: 24, jpeg_quality: 70, max_width: 1024 };

        // 造一张带渐变的“桌面”，避免全同色导致检测不到变化
        let mut base = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                base.extend_from_slice(&[(x % 200) as u8, (y % 200) as u8, 40, 255]);
            }
        }
        let mut d = Differ::new(true);
        // captured_ms 必须是真实时刻：编码前会丢弃滞留超过 STALE_DROP_MS 的帧
        let now = crate::platform::now_millis();
        d.emit(base.clone(), w, h, &cfg, now).expect("首帧应产出");

        // 在 (150,150) 附近画一块纯红
        let (px, py) = (150u32, 150u32);
        let mut cur = base;
        for y in py..py + 40 {
            for x in px..px + 40 {
                let i = ((y * w + x) * 4) as usize;
                cur[i..i + 3].copy_from_slice(&[255, 0, 0]);
            }
        }
        let job = d
            .emit(cur, w, h, &cfg, crate::platform::now_millis())
            .expect("变化应产出任务");
        assert!(matches!(job, EncodeJob::Tiles { .. }), "局部变化应走图块");

        let frame = encode_job(job, cfg).expect("编码应成功");
        let FrameKind::Rects { rects } = frame.kind else {
            panic!("应为增量帧");
        };
        assert_eq!(rects.len(), 1, "只有一块变化");
        let r = &rects[0];
        assert!(
            r.x <= px && px < r.x + r.w && r.y <= py && py < r.y + r.h,
            "变化点 ({px},{py}) 必须落在图块 ({},{},{},{}) 内",
            r.x, r.y, r.w, r.h
        );

        // 解码后该点应仍是红色（JPEG 有损，用宽松阈值）
        let decoded = crate::client::decode_jpeg(&r.jpeg, r.w, r.h).expect("图块应能解码");
        let idx = ((py - r.y) * r.w + (px - r.x)) as usize;
        let p = decoded.pixels[idx];
        assert!(p.r() > 190, "红色分量应保留，实际 {}", p.r());
        assert!(p.g() < 70 && p.b() < 70, "绿蓝分量应很小，实际 {}/{}", p.g(), p.b());
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

            let (dw, dh) = scaled_size(sw, sh, mw);
            // 预热一次，避免首帧的分配开销污染结果
            let _ = resize_rgba_to_rgb(&rgba, sw, sh, dw, dh);

            let t0 = Instant::now();
            let (rgb, w, h) = resize_rgba_to_rgb(&rgba, sw, sh, dw, dh);
            let resize_ms = t0.elapsed().as_secs_f64() * 1000.0;

            let t1 = Instant::now();
            let jpeg = jpeg_encode(&rgb, w, h, cfg.jpeg_quality).unwrap();
            let encode_ms = t1.elapsed().as_secs_f64() * 1000.0;

            println!(
                "{sw}×{sh} q{q} → {w}×{h}: 降采样+去alpha {resize_ms:.1}ms + JPEG编码 {encode_ms:.1}ms \
                 = {:.1}ms  ({:.0} KB, {:.1} Mbps@24fps)",
                resize_ms + encode_ms,
                jpeg.len() as f64 / 1024.0,
                jpeg.len() as f64 * 8.0 * 24.0 / 1_000_000.0
            );
        }

        // 增量更新（只编码变化区域）时的单图块成本：
        // 块越小则每次要发的块越少，但每块都有 JPEG 头部开销、且块间无法复用冗余。
        // 变化检测本身的成本：整幅扫一遍（增量更新新增的开销）
        {
            let (fw, fh) = (1920u32, 1080u32);
            let a = synth(fw, fh);
            let b = synth(fw, fh);
            let shadow = Shadow { rgba: b, width: fw, height: fh };
            let _ = diff_tiles(&a, fw, fh, &shadow, fw, fh); // 预热
            let t = Instant::now();
            let _ = diff_tiles(&a, fw, fh, &shadow, fw, fh);
            println!(
                "变化检测（{fw}×{fh} 全扫，步长 {DIFF_STRIDE}）: {:.2}ms",
                t.elapsed().as_secs_f64() * 1000.0
            );
        }

        for tw in [64u32, 128, 256] {
            let rgb = synth(tw, tw);
            let _ = resize_rgba_to_rgb(&rgb, tw, tw, tw, tw); // 预热
            let t0 = Instant::now();
            let (out, _, _) = resize_rgba_to_rgb(&rgb, tw, tw, tw, tw);
            let jpeg = jpeg_encode(&out, tw, tw, 70).unwrap();
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            // 该边长下铺满 1080p 需要的块数
            let need = ((1920 + tw - 1) / tw) * ((1080 + tw - 1) / tw);
            println!(
                "图块 {tw}×{tw}: 单块 {ms:.2}ms（{:.1} KB）；铺满 1080p 需 {need} 块共 {:.1}ms；\
                 只变 8 块则 {:.1}ms",
                jpeg.len() as f64 / 1024.0,
                ms * need as f64,
                ms * 8.0
            );
        }
    }
}
