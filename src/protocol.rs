//! 消息协议：`[u32 LE 长度][u8 类型][载荷]`。
//! 控制类消息载荷为 JSON；VideoFrame 为二进制（w: u32, h: u32, jpeg 字节）。

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const TCP_DEFAULT_PORT: u16 = 48500;
pub const UDP_DISCOVERY_PORT: u16 = 48501;
/// 协议版本：写入 Hello 握手，不兼容时由对端拒绝连接。
pub const PROTOCOL_VERSION: u32 = 1;
/// 单条消息上限（4K JPEG 帧约 1~2MB，留足余量）
const MAX_MSG_LEN: u32 = 64 * 1024 * 1024;

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
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ByeMsg {
    pub reason: String,
}

#[derive(Clone, Debug)]
pub enum Msg {
    Hello(Hello),
    Auth(Auth),
    AuthResult(AuthResult),
    RequestControl(RequestControl),
    ControlResult(ControlResult),
    VideoFrame { width: u32, height: u32, jpeg: Vec<u8> },
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
            Msg::Mouse(MouseMsg { x: 0.5, y: 0.25, button: 0, action: 1 }),
            Msg::Wheel(WheelMsg { dx: -2, dy: 3 }),
            Msg::Key(KeyMsg { key: "enter".into(), down: true }),
            Msg::Key(KeyMsg { key: "界".into(), down: false }),
            Msg::Clipboard(ClipboardMsg { text: "剪贴板内容\n第二行".into() }),
            Msg::Ping(PingMsg { ts: 1726000000123 }),
            Msg::Pong(PingMsg { ts: 1726000000456 }),
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
}
