//! 消息协议：`[u32 LE 长度][u8 类型][载荷]`。
//! 控制类消息载荷为 JSON；VideoFrame 为二进制（w: u32, h: u32, jpeg 字节）。

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const TCP_DEFAULT_PORT: u16 = 48500;
pub const UDP_DISCOVERY_PORT: u16 = 48501;
/// 协议版本：写入 Hello 握手，不兼容时由对端拒绝连接。
pub const PROTOCOL_VERSION: u32 = 2;
/// 能接受的最低对端版本。1 = v0.1.12 及更早（只有整帧）。
pub const PROTOCOL_MIN_VERSION: u32 = 1;
/// 达到该版本才支持「脏矩形增量更新」，否则一律发整帧。
pub const PROTOCOL_RECTS_MIN: u32 = 2;
/// 单条消息上限。4K JPEG 帧实测约 1~2MB，16MB 已有 8 倍余量；
/// 上限同时是「未读内容就分配内存」的上界，过大会被异常/恶意对端打爆内存。
pub const MAX_MSG_LEN: u32 = 16 * 1024 * 1024;

const T_HELLO: u8 = 1;
const T_AUTH: u8 = 2;
const T_AUTH_RESULT: u8 = 3;
const T_REQUEST_CONTROL: u8 = 4;
const T_CONTROL_RESULT: u8 = 5;
const T_VIDEO_FRAME: u8 = 6;
const T_MOUSE: u8 = 7;
const T_WHEEL: u8 = 8;
const T_KEY: u8 = 9;
const T_CLIPBOARD: u8 = 10;
const T_PING: u8 = 11;
const T_PONG: u8 = 12;
const T_BYE: u8 = 13;
/// 增量帧：一帧里只带发生变化的图块（见 [`FrameRects`]）。
const T_FRAME_RECTS: u8 = 14;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Hello {
    pub protocol_version: u32,
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub app_version: String,
    pub auth_required: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Auth {
    pub password: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RequestControl {
    pub view_only: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ControlResult {
    pub ok: bool,
    pub message: String,
}

/// 鼠标事件。坐标为归一化 0..1。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MouseMsg {
    pub x: f32,
    pub y: f32,
    /// 0=左 1=右 2=中
    pub button: u8,
    /// 0=移动 1=按下 2=抬起
    pub action: u8,
}

/// 滚轮。正值 dy = 向下滚动（enigo 语义），正值 dx = 向右。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WheelMsg {
    pub dx: i32,
    pub dy: i32,
}

/// 键盘事件。key 为按键名（见 key.rs 风格映射）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KeyMsg {
    pub key: String,
    pub down: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ClipboardMsg {
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PingMsg {
    pub ts: u64,
    /// 被控端回填的「抓帧 → 写出发送缓冲」实测耗时（毫秒）。
    /// 旧版对端不认识该字段，缺失时按 0 处理，界面据此隐藏该项读数。
    #[serde(default)]
    pub pipeline_ms: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ByeMsg {
    pub reason: String,
}

/// 一个发生变化的图块。坐标与尺寸为**缩放后画面**的像素，
/// 主控端可直接按此坐标局部更新纹理。
///
/// 走二进制编码而不是 JSON：热路径上每帧可能有上百个块，
/// JSON 的解析与临时字符串分配都太贵。
#[derive(Clone, Debug)]
pub struct RectBlock {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub jpeg: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Hello(Hello),
    Auth(Auth),
    AuthResult(AuthResult),
    RequestControl(RequestControl),
    ControlResult(ControlResult),
    VideoFrame { width: u32, height: u32, jpeg: Vec<u8> },
    /// 增量帧：本帧只有 `rects` 里的这些图块发生变化。
    /// `width`/`height` 为整幅画面尺寸（主控端据此确定纹理大小）。
    FrameRects { width: u32, height: u32, rects: Vec<RectBlock> },
    Mouse(MouseMsg),
    Wheel(WheelMsg),
    Key(KeyMsg),
    Clipboard(ClipboardMsg),
    Ping(PingMsg),
    Pong(PingMsg),
    Bye(ByeMsg),
}

impl Msg {
    fn ty(&self) -> u8 {
        match self {
            Msg::Hello(_) => T_HELLO,
            Msg::Auth(_) => T_AUTH,
            Msg::AuthResult(_) => T_AUTH_RESULT,
            Msg::RequestControl(_) => T_REQUEST_CONTROL,
            Msg::ControlResult(_) => T_CONTROL_RESULT,
            Msg::VideoFrame { .. } => T_VIDEO_FRAME,
            Msg::FrameRects { .. } => T_FRAME_RECTS,
            Msg::Mouse(_) => T_MOUSE,
            Msg::Wheel(_) => T_WHEEL,
            Msg::Key(_) => T_KEY,
            Msg::Clipboard(_) => T_CLIPBOARD,
            Msg::Ping(_) => T_PING,
            Msg::Pong(_) => T_PONG,
            Msg::Bye(_) => T_BYE,
        }
    }

    fn payload(&self) -> io::Result<Vec<u8>> {
        Ok(match self {
            Msg::Hello(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::Auth(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::AuthResult(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::RequestControl(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::ControlResult(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::VideoFrame { width, height, jpeg } => {
                let mut buf = Vec::with_capacity(8 + jpeg.len());
                buf.extend_from_slice(&width.to_le_bytes());
                buf.extend_from_slice(&height.to_le_bytes());
                buf.extend_from_slice(jpeg);
                buf
            }
            Msg::FrameRects { width, height, rects } => {
                let bytes: usize = rects.iter().map(|r| 20 + r.jpeg.len()).sum();
                let mut buf = Vec::with_capacity(12 + bytes);
                buf.extend_from_slice(&width.to_le_bytes());
                buf.extend_from_slice(&height.to_le_bytes());
                buf.extend_from_slice(&(rects.len() as u32).to_le_bytes());
                for r in rects {
                    buf.extend_from_slice(&r.x.to_le_bytes());
                    buf.extend_from_slice(&r.y.to_le_bytes());
                    buf.extend_from_slice(&r.w.to_le_bytes());
                    buf.extend_from_slice(&r.h.to_le_bytes());
                    buf.extend_from_slice(&(r.jpeg.len() as u32).to_le_bytes());
                    buf.extend_from_slice(&r.jpeg);
                }
                buf
            }
            Msg::Mouse(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::Wheel(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::Key(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::Clipboard(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::Ping(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::Pong(m) => serde_json::to_vec(m).map_err(io_err)?,
            Msg::Bye(m) => serde_json::to_vec(m).map_err(io_err)?,
        })
    }

    fn from_parts(ty: u8, payload: Vec<u8>) -> io::Result<Msg> {
        let msg = match ty {
            T_HELLO => Msg::Hello(parse(&payload)?),
            T_AUTH => Msg::Auth(parse(&payload)?),
            T_AUTH_RESULT => Msg::AuthResult(parse(&payload)?),
            T_REQUEST_CONTROL => Msg::RequestControl(parse(&payload)?),
            T_CONTROL_RESULT => Msg::ControlResult(parse(&payload)?),
            T_VIDEO_FRAME => {
                if payload.len() < 8 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "video frame too short"));
                }
                let width = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                let height = u32::from_le_bytes(payload[4..8].try_into().unwrap());
                Msg::VideoFrame { width, height, jpeg: payload[8..].to_vec() }
            }
            T_FRAME_RECTS => {
                if payload.len() < 12 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "rect frame too short"));
                }
                let width = u32::from_le_bytes(payload[0..4].try_into().unwrap());
                let height = u32::from_le_bytes(payload[4..8].try_into().unwrap());
                let count = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
                let mut rects = Vec::with_capacity(count.min(1024));
                let mut pos = 12usize;
                for _ in 0..count {
                    if pos + 20 > payload.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "rect frame truncated",
                        ));
                    }
                    let x = u32::from_le_bytes(payload[pos..pos + 4].try_into().unwrap());
                    let y = u32::from_le_bytes(payload[pos + 4..pos + 8].try_into().unwrap());
                    let w = u32::from_le_bytes(payload[pos + 8..pos + 12].try_into().unwrap());
                    let h = u32::from_le_bytes(payload[pos + 12..pos + 16].try_into().unwrap());
                    let len = u32::from_le_bytes(payload[pos + 16..pos + 20].try_into().unwrap()) as usize;
                    pos += 20;
                    if pos + len > payload.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "rect jpeg truncated",
                        ));
                    }
                    rects.push(RectBlock { x, y, w, h, jpeg: payload[pos..pos + len].to_vec() });
                    pos += len;
                }
                Msg::FrameRects { width, height, rects }
            }
            T_MOUSE => Msg::Mouse(parse(&payload)?),
            T_WHEEL => Msg::Wheel(parse(&payload)?),
            T_KEY => Msg::Key(parse(&payload)?),
            T_CLIPBOARD => Msg::Clipboard(parse(&payload)?),
            T_PING => Msg::Ping(parse(&payload)?),
            T_PONG => Msg::Pong(parse(&payload)?),
            T_BYE => Msg::Bye(parse(&payload)?),
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown msg type {ty}"))),
        };
        Ok(msg)
    }
}

fn parse<T: serde::de::DeserializeOwned>(payload: &[u8]) -> io::Result<T> {
    serde_json::from_slice(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn io_err(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// 对端版本是否支持脏矩形增量更新。
///
/// 被控端据此决定发整帧还是增量帧：对方是 v0.1.12 及更早的版本时
/// 只能发整帧，否则对端会读不出画面。
pub fn supports_rects(peer_version: u32) -> bool {
    peer_version >= PROTOCOL_RECTS_MIN && peer_version <= PROTOCOL_VERSION
}

/// 对端版本能否互通（握手时校验）。
pub fn version_compatible(peer_version: u32) -> bool {
    peer_version >= PROTOCOL_MIN_VERSION && peer_version <= PROTOCOL_VERSION
}

pub fn write_msg<W: Write>(w: &mut W, msg: &Msg) -> io::Result<()> {
    let payload = msg.payload()?;
    if payload.len() as u64 + 1 > MAX_MSG_LEN as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let len = (payload.len() + 1) as u32;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&[msg.ty()])?;
    w.write_all(&payload)?;
    w.flush()
}

pub fn read_msg<R: Read>(r: &mut R) -> io::Result<Msg> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_MSG_LEN || len < 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad message length"));
    }
    let mut ty_buf = [0u8; 1];
    r.read_exact(&mut ty_buf)?;
    let mut payload = vec![0u8; (len - 1) as usize];
    r.read_exact(&mut payload)?;
    Msg::from_parts(ty_buf[0], payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Msg) -> Msg {
        let mut buf = Vec::new();
        write_msg(&mut buf, &msg).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        read_msg(&mut cursor).unwrap()
    }

    #[test]
    fn test_roundtrip_all_types() {
        let msgs = vec![
            Msg::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                device_id: "ABCD-1234".into(),
                name: "测试机".into(),
                platform: "macOS".into(),
                app_version: "0.1.0".into(),
                auth_required: true,
            }),
            Msg::Auth(Auth { password: "pw".into() }),
            Msg::AuthResult(AuthResult { ok: true, message: "ok".into() }),
            Msg::RequestControl(RequestControl { view_only: false }),
            Msg::ControlResult(ControlResult { ok: false, message: "拒绝".into() }),
            Msg::VideoFrame { width: 1920, height: 1080, jpeg: vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3] },
            Msg::FrameRects {
                width: 1920,
                height: 1080,
                rects: vec![
                    RectBlock { x: 0, y: 0, w: 128, h: 128, jpeg: vec![0xFF, 0xD8, 0xFF] },
                    RectBlock { x: 128, y: 256, w: 128, h: 128, jpeg: vec![1, 2, 3, 4, 5] },
                ],
            },
            Msg::Mouse(MouseMsg { x: 0.5, y: 0.25, button: 0, action: 1 }),
            Msg::Wheel(WheelMsg { dx: -2, dy: 3 }),
            Msg::Key(KeyMsg { key: "enter".into(), down: true }),
            Msg::Key(KeyMsg { key: "界".into(), down: false }),
            Msg::Clipboard(ClipboardMsg { text: "剪贴板内容\n第二行".into() }),
            Msg::Ping(PingMsg { ts: 1726000000123, pipeline_ms: 0 }),
            Msg::Pong(PingMsg { ts: 1726000000456, pipeline_ms: 37 }),
            Msg::Bye(ByeMsg { reason: "用户断开".into() }),
        ];
        for m in msgs {
            let rt = roundtrip(m.clone());
            let a = format!("{m:?}");
            let b = format!("{rt:?}");
            assert_eq!(a, b, "roundtrip mismatch for {a}");
        }
    }

    #[test]
    fn test_large_frame_roundtrip() {
        let jpeg = vec![7u8; 2 * 1024 * 1024];
        let rt = roundtrip(Msg::VideoFrame { width: 3840, height: 2160, jpeg: jpeg.clone() });
        match rt {
            Msg::VideoFrame { width, height, jpeg: j } => {
                assert_eq!((width, height), (3840, 2160));
                assert_eq!(j, jpeg);
            }
            other => panic!("wrong msg: {other:?}"),
        }
    }

    #[test]
    fn test_bad_length_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        buf.push(T_HELLO);
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_msg(&mut cursor).is_err());
    }

    /// 增量帧必须能原样往返：主控端靠它只更新变化区域，坐标错了画面就花了。
    #[test]
    fn test_frame_rects_roundtrip() {
        let rects = vec![
            RectBlock { x: 0, y: 0, w: 128, h: 128, jpeg: vec![0xFF, 0xD8, 0xFF] },
            RectBlock { x: 640, y: 512, w: 128, h: 96, jpeg: vec![9, 8, 7, 6] },
        ];
        let msg = Msg::FrameRects { width: 1920, height: 1080, rects: rects.clone() };
        match roundtrip(msg) {
            Msg::FrameRects { width, height, rects: got } => {
                assert_eq!((width, height), (1920, 1080));
                assert_eq!(got.len(), 2);
                for (a, b) in rects.iter().zip(got.iter()) {
                    assert_eq!((a.x, a.y, a.w, a.h), (b.x, b.y, b.w, b.h));
                    assert_eq!(a.jpeg, b.jpeg);
                }
            }
            other => panic!("类型错误: {other:?}"),
        }
    }

    /// 零图块也要能正常编码/解码（没有变化时的“空帧”）。
    #[test]
    fn test_frame_rects_empty() {
        let msg = Msg::FrameRects { width: 800, height: 600, rects: Vec::new() };
        match roundtrip(msg) {
            Msg::FrameRects { rects, .. } => assert!(rects.is_empty()),
            other => panic!("类型错误: {other:?}"),
        }
    }

    /// 截断的增量帧必须报错而不是 panic 或读到半张图。
    #[test]
    fn test_frame_rects_truncated_rejected() {
        let mut buf = Vec::new();
        let msg = Msg::FrameRects {
            width: 100,
            height: 100,
            rects: vec![RectBlock { x: 0, y: 0, w: 8, h: 8, jpeg: vec![1, 2, 3, 4] }],
        };
        write_msg(&mut buf, &msg).unwrap();
        // 砍掉末尾 2 字节，让最后一个 jpeg 不完整
        buf.truncate(buf.len() - 2);
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_msg(&mut cursor).is_err());
    }

    /// 版本协商：旧版对端能连上但只能发整帧。
    #[test]
    fn test_version_gating() {
        assert!(version_compatible(1), "v0.1.12 及更早必须还能连上");
        assert!(version_compatible(2));
        assert!(!version_compatible(0));
        assert!(!version_compatible(3), "比本机更新的版本不兼容");

        assert!(!supports_rects(1), "旧版只能收整帧");
        assert!(supports_rects(2));
    }

    /// 新增的 pipeline_ms 必须对旧版对端保持兼容：旧端发来的 JSON 里没有该字段，
    /// 反序列化要落到 0（界面据此隐藏读数），而不是报错导致整条消息读失败。
    #[test]
    fn test_ping_without_pipeline_ms_is_accepted() {
        let legacy = br#"{"ts":1726000000123}"#;
        let parsed: PingMsg = serde_json::from_slice(legacy).expect("旧版 Ping 必须仍可解析");
        assert_eq!(parsed.ts, 1726000000123);
        assert_eq!(parsed.pipeline_ms, 0);
    }
}
