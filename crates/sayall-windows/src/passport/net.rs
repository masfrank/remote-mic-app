//! UDP 收包线程：绑定端口、维持 beacon 与 `pc.status`、解码音频块并把命令交给会话驱动。
//!
//! 这里刻意不做任何阻塞的音频设备操作（`begin/finish` 都在会话线程），
//! 因此网络抖动或 WASAPI 重开不会互相拖死。

use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sayall_core::passport::{
    beacon_packet, parse_audio, parse_event, status_packet, BlockDecoder, PassportEvent,
    SequenceTracker, T_AUDIO, T_BEACON, T_EVENT, T_PING,
};

use super::session::{HoldKey, SessionCommand, VoiceAudio};
use super::Shared;

const BEACON_INTERVAL: Duration = Duration::from_secs(1);
/// 设备侧 3.5s 收不到 `pc.status` 就会强制结束会话，这里按 1s 续命。
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// 多网卡/VPN 环境下广播目标可能变化（参考实现同为 30s）。
const TARGET_REFRESH: Duration = Duration::from_secs(30);
/// 设备静默多久判定链路断开；比固件 3s 的对端超时更宽松，容忍 2.4G 抖动。
const LINK_TIMEOUT: Duration = Duration::from_secs(12);

pub(crate) fn run(
    socket: UdpSocket,
    port: u16,
    shared: Arc<Shared>,
    audio: Arc<dyn VoiceAudio>,
    hold: Arc<dyn HoldKey>,
    commands: Sender<SessionCommand>,
    extra_peers: Vec<Ipv4Addr>,
) {
    let mut decoder = BlockDecoder::new();
    let mut sequence = SequenceTracker::default();
    let mut last_seen: Option<Instant> = None;
    let mut next_beacon = Instant::now();
    let mut next_status = Instant::now();
    let mut next_targets = Instant::now();
    let mut targets: Vec<SocketAddr> = Vec::new();
    let beacon = beacon_packet();
    let mut buffer = [0u8; 4096];

    while shared.is_running() {
        let now = Instant::now();

        if now >= next_targets {
            targets = broadcast_targets(port, &extra_peers);
            next_targets = now + TARGET_REFRESH;
        }
        if now >= next_beacon {
            for target in &targets {
                let _ = socket.send_to(&beacon, target);
            }
            next_beacon = now + BEACON_INTERVAL;
        }
        if now >= next_status {
            if let Some(device) = shared.device() {
                let packet = status_packet(audio.ready(), hold.ready());
                let _ = socket.send_to(&packet, device);
            }
            next_status = now + STATUS_INTERVAL;
        }

        match socket.recv_from(&mut buffer) {
            Ok((len, source)) => {
                if len == 0 {
                    continue;
                }
                let kind = buffer[0];
                let payload = &buffer[1..len];
                if kind != T_BEACON {
                    // 任何非 beacon 报文都算"设备活着"，地址以来源为准。
                    if shared.device() != Some(source) {
                        shared.set_device(source);
                        crate::gatt_note(format!("passport link action=learned addr={source}"));
                    }
                    last_seen = Some(Instant::now());
                }
                match kind {
                    T_AUDIO => {
                        handle_audio(payload, &mut decoder, &mut sequence, &shared, &commands)
                    }
                    T_EVENT => handle_event(payload, &shared, &commands),
                    T_PING => {}
                    T_BEACON => {}
                    _ => shared.count_bad_packet(),
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                shared.set_error(format!("UDP 接收失败：{error}"));
                crate::gatt_note(format!(
                    "passport link action=recv result=failed reason={error}"
                ));
                break;
            }
        }

        if let Some(seen) = last_seen {
            if seen.elapsed() > LINK_TIMEOUT && shared.device().is_some() {
                shared.clear_device();
                sequence.reset();
                last_seen = None;
                crate::gatt_note("passport link action=lost reason=timeout".to_owned());
                let _ = commands.send(SessionCommand::LinkLost);
            }
        }
    }
    // 网络线程退出即视为通道不可用：不要让快照继续显示 Listening。
    shared.set_running(false);
    crate::gatt_note("passport link action=stopped".to_owned());
}

fn handle_audio(
    payload: &[u8],
    decoder: &mut BlockDecoder,
    sequence: &mut SequenceTracker,
    shared: &Shared,
    commands: &Sender<SessionCommand>,
) {
    let Some(block) = parse_audio(payload) else {
        shared.count_bad_packet();
        return;
    };
    if !sequence.accept(block.seq) {
        shared.set_out_of_order(sequence.out_of_order());
        return;
    }
    shared.set_lost(sequence.lost());
    match decoder.decode(block.block) {
        Ok(samples) => {
            shared.count_block();
            let _ = commands.send(SessionCommand::Block(samples));
        }
        Err(_) => shared.count_bad_packet(),
    }
}

fn handle_event(payload: &[u8], shared: &Shared, commands: &Sender<SessionCommand>) {
    let event = match parse_event(payload) {
        Ok(event) => event,
        Err(_) => {
            shared.count_bad_packet();
            return;
        }
    };
    match event {
        PassportEvent::Hello { proto } => crate::gatt_note(format!(
            "passport link action=hello proto={}",
            proto.unwrap_or(0)
        )),
        PassportEvent::VoiceStart { audio } => {
            let _ = commands.send(SessionCommand::VoiceStart { audio });
        }
        PassportEvent::VoiceEnd => {
            let _ = commands.send(SessionCommand::VoiceEnd);
        }
        PassportEvent::KeyAction { action } => {
            let _ = commands.send(SessionCommand::KeyTap(action));
        }
        PassportEvent::Status { dropped } => crate::gatt_note(format!(
            "passport device action=status dropped={}",
            dropped.unwrap_or(0)
        )),
        PassportEvent::Unknown { name } => {
            crate::gatt_note(format!("passport device action=unknown_event name={name}"))
        }
    }
}

/// 受限广播 + 每个私网网段的定向广播。
///
/// 多网卡时 `255.255.255.255` 只从路由选中的网口出去，可能打不到设备所在网段；
/// 枚举本机私网 IPv4 后逐个发 `x.y.z.255`（与参考实现同一策略）。
fn broadcast_targets(port: u16, extra_peers: &[Ipv4Addr]) -> Vec<SocketAddr> {
    let mut targets: BTreeSet<Ipv4Addr> = BTreeSet::new();
    targets.insert(Ipv4Addr::BROADCAST);
    for ip in local_private_ipv4() {
        let [a, b, c, _] = ip.octets();
        targets.insert(Ipv4Addr::new(a, b, c, 255));
    }
    targets.extend(extra_peers.iter().copied());
    targets
        .into_iter()
        .map(|ip| SocketAddr::from((ip, port)))
        .collect()
}

fn local_private_ipv4() -> Vec<Ipv4Addr> {
    let mut found = Vec::new();

    // UDP connect 不发包，只借内核路由表选出默认路由口的本地地址。
    if let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        if socket
            .connect(SocketAddr::from((Ipv4Addr::new(223, 5, 5, 5), 53)))
            .is_ok()
        {
            if let Ok(SocketAddr::V4(addr)) = socket.local_addr() {
                found.push(*addr.ip());
            }
        }
    }

    if let Ok(name) = std::env::var("COMPUTERNAME") {
        if let Ok(addrs) = (name.as_str(), 0).to_socket_addrs() {
            found.extend(addrs.into_iter().filter_map(|addr| match addr {
                SocketAddr::V4(v4) => Some(*v4.ip()),
                SocketAddr::V6(_) => None,
            }));
        }
    }

    // Clash/Mihomo 的 TUN 会污染域名解析，再用 ipconfig 兜底；
    // 只扫描 ASCII 点分四段，不涉及控制台编码。
    if let Ok(output) = Command::new("ipconfig").output() {
        found.extend(scan_ipv4(&output.stdout));
    }

    found.retain(is_private);
    found.sort();
    found.dedup();
    found
}

fn is_private(ip: &Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
}

fn scan_ipv4(bytes: &[u8]) -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut token = String::new();
    for byte in bytes.iter().copied().chain(std::iter::once(b' ')) {
        if byte.is_ascii_digit() || byte == b'.' {
            token.push(byte as char);
        } else if !token.is_empty() {
            if token.matches('.').count() == 3 {
                if let Ok(ip) = token.parse::<Ipv4Addr>() {
                    out.push(ip);
                }
            }
            token.clear();
        }
    }
    out
}
