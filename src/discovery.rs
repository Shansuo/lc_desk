//! 局域网设备发现：UDP 广播通告 + 对端登记簿。

use crate::protocol::UDP_DISCOVERY_PORT;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Socket, Type};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);
pub const PEER_EXPIRE: Duration = Duration::from_secs(7);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Announce {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub version: String,
    pub tcp_port: u16,
    pub accepting: bool,
}

#[derive(Clone, Debug)]
pub struct Peer {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    #[allow(dead_code)]
    pub version: String,
    /// 可用于 TCP 直连的对端地址（含端口）
    pub addr: SocketAddr,
    pub accepting: bool,
    pub last_seen: Instant,
}

pub type PeerBook = Arc<Mutex<HashMap<String, Peer>>>;

pub fn new_peer_book() -> PeerBook {
    Arc::new(Mutex::new(HashMap::new()))
}

/// 广播专用发送套接字：绑到随机端口，用 `send_to` 逐个投递到各广播目标。
/// 接收走另一个套接字（见 `open_listen_socket`），两者分离以免
/// 发送端的过滤掉非广播来源的报文。
///
/// 注意这里**不能** connect：一旦 connect 到某个广播地址，后续就只能往
/// 这一个目标发，无法覆盖多网卡。
fn open_socket() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(socket2::Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    let _ = socket.set_reuse_port(true);
    socket.set_broadcast(true)?;
    let bind_addr: SocketAddr = "0.0.0.0:0".parse().expect("valid bind addr");
    socket.bind(&bind_addr.into())?;
    Ok(socket.into())
}

/// 本次要投递的广播目标。
///
/// 只发 `255.255.255.255` 是不够的：该地址走**默认路由**选定的单张网卡，
/// 而 Windows 机器常带 VMware / Hyper-V / VPN 虚拟网卡，默认路由很可能落在
/// 虚拟网络上，真实局域网里的对端就永远收不到通告（表现为 mac 发现不了 win）。
/// 因此这里枚举所有 IPv4 网卡，额外投递每张网卡的**子网定向广播地址**
/// （如 192.168.1.255），内核会按直连路由从对应网卡发出。
fn broadcast_targets() -> Vec<SocketAddr> {
    let mut targets: Vec<SocketAddr> =
        vec![SocketAddr::from((Ipv4Addr::BROADCAST, UDP_DISCOVERY_PORT))];

    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            let if_addrs::IfAddr::V4(v4) = iface.addr else {
                continue;
            };
            if v4.ip.is_loopback() || v4.ip.is_unspecified() {
                continue;
            }
            // 接口自带 broadcast 最可靠；缺失时按掩码推导
            let bcast = v4.broadcast.unwrap_or_else(|| {
                let ip = u32::from(v4.ip);
                let mask = u32::from(v4.netmask);
                Ipv4Addr::from(ip | !mask)
            });
            if bcast == v4.ip {
                continue; // /32 之类的点到点地址，广播无意义
            }
            let addr = SocketAddr::from((bcast, UDP_DISCOVERY_PORT));
            if !targets.contains(&addr) {
                targets.push(addr);
            }
        }
    } else {
        log::warn!("枚举网卡失败，仅使用全局广播地址");
    }
    targets
}

fn format_targets(targets: &[SocketAddr]) -> String {
    targets
        .iter()
        .map(|t| t.ip().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn make_announce(self_id: &str, cfg: &crate::config::Config, accepting: bool) -> Announce {
    Announce {
        device_id: self_id.to_string(),
        name: cfg.device_name.clone(),
        platform: crate::platform::platform_name().to_string(),
        version: crate::platform::app_version().to_string(),
        tcp_port: cfg.tcp_port,
        accepting,
    }
}

/// 启动发现线程：周期广播自己 + 收集他人。返回停止开关。
pub fn start_discovery(
    self_id: String,
    peers: PeerBook,
    accepting: Arc<AtomicBool>,
    server_ready: Arc<AtomicBool>,
    config: Arc<Mutex<crate::config::Config>>,
    ctx: egui::Context,
) -> std::io::Result<Arc<AtomicBool>> {
    let socket = Arc::new(open_socket()?);
    let targets = broadcast_targets();
    log::info!("发现广播目标: {}", format_targets(&targets));

    let stop = Arc::new(AtomicBool::new(false));

    // 广播线程
    {
        let socket = socket.clone();
        let stop = stop.clone();
        let self_id = self_id.clone();
        let accepting = accepting.clone();
        let config = config.clone();
        let server_ready = server_ready.clone();
        std::thread::Builder::new()
            .name("discovery-tx".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let ann = {
                        let cfg = config.lock().unwrap();
                        // 只有真正监听成功才对外宣称可被控制
                        let online = accepting.load(Ordering::Relaxed)
                            && server_ready.load(Ordering::Relaxed);
                        make_announce(&self_id, &cfg, online)
                    };
                    if let Ok(json) = serde_json::to_vec(&ann) {
                        for target in &targets {
                            // 忽略单个目标的失败：某个网卡（如虚拟网卡）不支持
                            // 广播不应影响其他网卡
                            let _ = socket.send_to(&json, target);
                        }
                    }
                    // 分片睡眠，快速响应停止
                    for _ in 0..20 {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(ANNOUNCE_INTERVAL / 20);
                    }
                }
            })
            .expect("spawn discovery-tx");
    }

    // 接收线程
    {
        let stop = stop.clone();
        let self_id = self_id.clone();
        std::thread::Builder::new()
            .name("discovery-rx".into())
            .spawn(move || {
                let listen = match open_listen_socket() {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        log::error!("发现端口 {UDP_DISCOVERY_PORT} 绑定失败: {e}");
                        return;
                    }
                };
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    match listen.recv_from(&mut buf) {
                        Ok((n, src)) => {
                            let ann: Announce = match serde_json::from_slice(&buf[..n]) {
                                Ok(a) => a,
                                Err(_) => continue,
                            };
                            if ann.device_id == self_id {
                                continue;
                            }
                            let peer = Peer {
                                device_id: ann.device_id.clone(),
                                name: ann.name,
                                platform: ann.platform,
                                version: ann.version,
                                addr: SocketAddr::new(src.ip(), ann.tcp_port),
                                accepting: ann.accepting,
                                last_seen: Instant::now(),
                            };
                            let changed = {
                                let mut book = peers.lock().unwrap();
                                let old = book.get(&ann.device_id);
                                let differs = match old {
                                    Some(o) => {
                                        o.name != peer.name
                                            || o.addr != peer.addr
                                            || o.accepting != peer.accepting
                                    }
                                    None => true,
                                };
                                book.insert(ann.device_id.clone(), peer);
                                differs
                            };
                            if changed {
                                ctx.request_repaint();
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(500)),
                    }
                }
            })
            .expect("spawn discovery-rx");
    }

    Ok(stop)
}

fn open_listen_socket() -> std::io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(socket2::Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    let _ = socket.set_reuse_port(true);
    socket.set_nonblocking(true)?;
    let bind_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, UDP_DISCOVERY_PORT));
    socket.bind(&bind_addr.into())?;
    Ok(socket.into())
}

/// 清理过期对端。
pub fn prune_expired(peers: &PeerBook) {
    let mut book = peers.lock().unwrap();
    book.retain(|_, p| p.last_seen.elapsed() < PEER_EXPIRE);
}
