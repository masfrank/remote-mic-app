// P0 可行性探针: FoloToy AI Passport (Wi-Fi/UDP) -> 解码 -> WAV。
// Read-only 之外的副作用只有: 绑定 UDP 33333、广播 beacon、写临时媒体文件。
// 不触碰 BLE、WASAPI、SendInput、设置与任何产品运行时。
//
// 协议来源 (MIT): folo-ai-passport-voice/windows/udp_transport.py、windows/adpcm.py、
// device/main/apps/voice/udp_audio.h。仅复用公开协议常量，解码走 sayall-core 现有实现。
//
// 用法:
//   cargo run -p sayall-windows --example passport_probe -- --seconds 60
//   cargo run -p sayall-windows --example passport_probe -- --decode capture.pscap --wav out.wav
//
// 产物(默认 %TEMP%\sayall-passport-probe\):
//   passport-<unix>.wav    Rust 解码后的 16 kHz mono s16 PCM
//   passport-<unix>.pscap  原始 804B 块(seq + 长度 + 块)，供 Python 端逐块比对

#[cfg(windows)]
fn main() {
    if let Err(error) = probe::run() {
        eprintln!("passport_probe failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This probe requires Windows (uses ipconfig for broadcast targets).");
}

#[cfg(windows)]
mod probe {
    use sayall_core::{ImaAdpcmDecoder, NibbleOrder};
    use std::collections::BTreeSet;
    use std::fs::{self, File};
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const UDP_PORT: u16 = 33_333;

    const T_AUDIO: u8 = 0x01;
    const T_EVENT: u8 = 0x02;
    const T_BEACON: u8 = 0x03;
    const T_PING: u8 = 0x04;

    /// 块头 = [int16 LE predictor][uint8 step_index][uint8 reserved]
    const BLOCK_HEADER_BYTES: usize = 4;
    const BLOCK_SAMPLES: usize = 1600;
    const BLOCK_BYTES: usize = BLOCK_HEADER_BYTES + BLOCK_SAMPLES / 2; // 804
    /// AUDIO 载荷 = [seq][0x80] + 804B 块
    const AUDIO_PAYLOAD_BYTES: usize = 2 + BLOCK_BYTES; // 806

    const SAMPLE_RATE: u32 = 16_000;
    const BEACON_INTERVAL: Duration = Duration::from_secs(1);
    const STATUS_INTERVAL: Duration = Duration::from_secs(1);
    /// 会话中超过该时长没有音频: 只告警，不伪造数据
    const AUDIO_STALL: Duration = Duration::from_millis(2500);
    const RECV_TIMEOUT: Duration = Duration::from_millis(200);

    const CAPTURE_MAGIC: &[u8; 8] = b"PSCAP1\r\n";

    struct Args {
        seconds: u64,
        out_dir: PathBuf,
        peers: Vec<Ipv4Addr>,
        no_broadcast: bool,
        decode: Option<PathBuf>,
        wav: Option<PathBuf>,
        hotkey_ready: bool,
        verbose: bool,
    }

    impl Args {
        fn parse() -> Result<Self, String> {
            let mut args = Args {
                seconds: 60,
                out_dir: std::env::temp_dir().join("sayall-passport-probe"),
                peers: Vec::new(),
                no_broadcast: false,
                decode: None,
                wav: None,
                hotkey_ready: true,
                verbose: false,
            };
            let mut items = std::env::args().skip(1);
            while let Some(item) = items.next() {
                match item.as_str() {
                    "--seconds" | "--dur" => {
                        let value = items.next().ok_or("--seconds 缺少取值")?;
                        args.seconds = value.parse().map_err(|_| "--seconds 需要秒数")?;
                    }
                    "--out" => {
                        args.out_dir = PathBuf::from(items.next().ok_or("--out 缺少路径")?);
                    }
                    "--peer" => {
                        let value = items.next().ok_or("--peer 缺少地址")?;
                        args.peers.push(
                            value
                                .parse()
                                .map_err(|_| format!("--peer 不是 IPv4 地址: {value}"))?,
                        );
                    }
                    "--no-broadcast" => args.no_broadcast = true,
                    "--decode" => {
                        args.decode = Some(PathBuf::from(
                            items.next().ok_or("--decode 缺少 capture 路径")?,
                        ));
                    }
                    "--wav" => {
                        args.wav = Some(PathBuf::from(items.next().ok_or("--wav 缺少路径")?));
                    }
                    "--no-hotkey" => args.hotkey_ready = false,
                    "--verbose" => args.verbose = true,
                    "--help" | "-h" => {
                        print_usage();
                        std::process::exit(0);
                    }
                    other => return Err(format!("未知参数: {other}")),
                }
            }
            Ok(args)
        }
    }

    fn print_usage() {
        println!(
            "passport_probe —— FoloToy AI Passport UDP 链路探针\n\
             \n\
             --seconds <n>      采集时长，默认 60\n\
             --out <dir>        输出目录，默认 %TEMP%\\sayall-passport-probe\n\
             --peer <ipv4>      额外单播目标，可重复\n\
             --no-broadcast     不广播，仅用 --peer\n\
             --no-hotkey        pc.status 上报 hotkeyReady=false\n\
             --decode <file>    离线解码 .pscap -> WAV(不开 socket)\n\
             --wav <file>       --decode 的输出路径\n\
             --verbose          打印每个事件原文\n\
             \n\
             逐块比对: 用 folo-ai-passport-voice/windows/adpcm.py 的 decode_block\n\
             解码同一个 .pscap，再与 WAV 的 data 段逐字节比较"
        );
    }

    pub fn run() -> Result<(), String> {
        let args = Args::parse()?;
        if let Some(capture) = args.decode.clone() {
            return decode_capture(&capture, args.wav.clone());
        }
        run_live(&args)
    }

    // --- 协议 -------------------------------------------------------------

    struct BlockDecoder {
        inner: ImaAdpcmDecoder,
    }

    impl BlockDecoder {
        fn new() -> Self {
            // FoloToy 与 WAV IMA ADPCM 一致: 低半字节先
            Self {
                inner: ImaAdpcmDecoder::new(NibbleOrder::LowFirst),
            }
        }

        /// 804B 块 -> 1600 样本。
        ///
        /// 参考实现的块头 predictor 就是第 0 个输出样本(见 `adpcm.c`
        /// `adpcm_decode_block`: `pcm[0] = st.predictor`)，数据区只承载第 1..1599
        /// 个样本(1599 个 nibble)。而 `ImaAdpcmDecoder::decode` 每字节固定出 2 个
        /// nibble，因此这里取前 1599 个解码结果，再补上块头里的首样本。
        fn decode(&mut self, block: &[u8]) -> Option<Vec<i16>> {
            if block.len() != BLOCK_BYTES {
                return None;
            }
            let first = i16::from_le_bytes([block[0], block[1]]);
            let step_index = block[2];
            if step_index > 88 {
                return None;
            }
            self.inner.reset(i32::from(first), i32::from(step_index));
            let decoded = self.inner.decode(&block[BLOCK_HEADER_BYTES..]);
            if decoded.len() < BLOCK_SAMPLES - 1 {
                return None;
            }
            let mut samples = Vec::with_capacity(BLOCK_SAMPLES);
            samples.push(first);
            samples.extend_from_slice(&decoded[..BLOCK_SAMPLES - 1]);
            Some(samples)
        }
    }

    fn beacon_packet() -> Vec<u8> {
        let mut packet = Vec::with_capacity(64);
        packet.push(T_BEACON);
        packet.extend_from_slice(format!(r#"{{"v":1,"role":"pc","port":{UDP_PORT}}}"#).as_bytes());
        packet
    }

    fn status_packet(hotkey_ready: bool) -> Vec<u8> {
        let mut packet = Vec::with_capacity(96);
        packet.push(T_EVENT);
        packet.extend_from_slice(
            serde_json::json!({
                "type": "pc.status",
                "audioReady": true,
                "hotkeyReady": hotkey_ready,
            })
            .to_string()
            .as_bytes(),
        );
        packet.push(b'\n');
        packet
    }

    // --- 广播目标 ---------------------------------------------------------

    fn broadcast_targets() -> Vec<SocketAddr> {
        let mut targets: BTreeSet<Ipv4Addr> = BTreeSet::new();
        targets.insert(Ipv4Addr::BROADCAST);
        for ip in local_private_ipv4() {
            let octets = ip.octets();
            targets.insert(Ipv4Addr::new(octets[0], octets[1], octets[2], 255));
        }
        targets
            .into_iter()
            .map(|ip| SocketAddr::from((ip, UDP_PORT)))
            .collect()
    }

    /// 多网卡时 255.255.255.255 只从默认路由口出去，可能打不到设备所在的网段，
    /// 因此额外枚举本机私网 IPv4 并逐个发定向广播(与参考实现同一策略)。
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

        // Clash/Mihomo 的 TUN 会污染域名解析(参考实现 2026-09-13 实测)，
        // 所以再用 ipconfig 兜底。只扫 ASCII 点分四段，不涉及控制台编码。
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

    // --- 输出 -------------------------------------------------------------

    struct WavWriter {
        file: File,
        data_bytes: u32,
        path: PathBuf,
    }

    impl WavWriter {
        fn create(path: &Path) -> io::Result<Self> {
            let mut file = File::create(path)?;
            file.write_all(&Self::header(0))?;
            Ok(Self {
                file,
                data_bytes: 0,
                path: path.to_path_buf(),
            })
        }

        fn header(data_bytes: u32) -> [u8; 44] {
            let mut header = [0u8; 44];
            let byte_rate = SAMPLE_RATE * 2;
            header[0..4].copy_from_slice(b"RIFF");
            header[4..8].copy_from_slice(&(36 + data_bytes).to_le_bytes());
            header[8..12].copy_from_slice(b"WAVE");
            header[12..16].copy_from_slice(b"fmt ");
            header[16..20].copy_from_slice(&16u32.to_le_bytes());
            header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
            header[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
            header[24..28].copy_from_slice(&SAMPLE_RATE.to_le_bytes());
            header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
            header[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
            header[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits
            header[36..40].copy_from_slice(b"data");
            header[40..44].copy_from_slice(&data_bytes.to_le_bytes());
            header
        }

        fn write_samples(&mut self, samples: &[i16]) -> io::Result<()> {
            let mut pcm = Vec::with_capacity(samples.len() * 2);
            for sample in samples {
                pcm.extend_from_slice(&sample.to_le_bytes());
            }
            self.file.write_all(&pcm)?;
            self.data_bytes += pcm.len() as u32;
            Ok(())
        }

        fn finish(mut self) -> io::Result<PathBuf> {
            self.file.seek(SeekFrom::Start(0))?;
            self.file.write_all(&Self::header(self.data_bytes))?;
            self.file.flush()?;
            Ok(self.path)
        }
    }

    struct CaptureWriter {
        file: File,
        blocks: u64,
        path: PathBuf,
    }

    impl CaptureWriter {
        fn create(path: &Path) -> io::Result<Self> {
            let mut file = File::create(path)?;
            file.write_all(CAPTURE_MAGIC)?;
            Ok(Self {
                file,
                blocks: 0,
                path: path.to_path_buf(),
            })
        }

        fn record(&mut self, seq: u8, block: &[u8]) -> io::Result<()> {
            self.file.write_all(&[seq])?;
            self.file.write_all(&(block.len() as u16).to_le_bytes())?;
            self.file.write_all(block)?;
            self.blocks += 1;
            Ok(())
        }

        fn finish(mut self) -> io::Result<PathBuf> {
            self.file.flush()?;
            Ok(self.path)
        }
    }

    // --- 实时采集 ---------------------------------------------------------

    #[derive(Default)]
    struct Stats {
        audio_packets: u64,
        audio_bytes: u64,
        blocks: u64,
        samples: u64,
        lost_blocks: u64,
        out_of_order: u64,
        bad_packets: u64,
        bad_blocks: u64,
        events: u64,
        pings: u64,
        unknown: u64,
        beacons_sent: u64,
        statuses_sent: u64,
        session_blocks: u64,
        /// 设备漏发 voice.start 时只提示一次，避免每块都刷屏
        resume_warned: bool,
    }

    fn run_live(args: &Args) -> Result<(), String> {
        fs::create_dir_all(&args.out_dir).map_err(|error| format!("创建输出目录失败: {error}"))?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let wav_path = args.out_dir.join(format!("passport-{stamp}.wav"));
        let capture_path = args.out_dir.join(format!("passport-{stamp}.pscap"));

        let mut wav =
            WavWriter::create(&wav_path).map_err(|error| format!("创建 WAV 失败: {error}"))?;
        let mut capture = CaptureWriter::create(&capture_path)
            .map_err(|error| format!("创建 capture 失败: {error}"))?;

        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, UDP_PORT)).map_err(|error| {
            format!(
                "绑定 UDP {UDP_PORT} 失败: {error}\n\
                 端口可能被占用(旧 Passport 程序仍开着)，或需要放行入站 UDP"
            )
        })?;
        socket
            .set_broadcast(true)
            .map_err(|error| format!("开启广播失败: {error}"))?;
        socket
            .set_read_timeout(Some(RECV_TIMEOUT))
            .map_err(|error| format!("设置读超时失败: {error}"))?;

        let mut targets: Vec<SocketAddr> = if args.no_broadcast {
            Vec::new()
        } else {
            broadcast_targets()
        };
        targets.extend(
            args.peers
                .iter()
                .map(|ip| SocketAddr::from((*ip, UDP_PORT))),
        );

        log(serde_json::json!({
            "kind": "passport_probe_start",
            "port": UDP_PORT,
            "seconds": args.seconds,
            "wav": wav_path.display().to_string(),
            "capture": capture_path.display().to_string(),
            "broadcastTargets": targets.iter().map(|t| t.to_string()).collect::<Vec<_>>(),
            "hotkeyReady": args.hotkey_ready,
        }));

        let started = Instant::now();
        let deadline = started + Duration::from_secs(args.seconds.max(1));
        let mut decoder = BlockDecoder::new();
        let mut stats = Stats::default();
        let mut device: Option<SocketAddr> = None;
        let mut last_seq: Option<u8> = None;
        let mut in_session = false;
        let mut last_audio = Instant::now();

        let mut next_beacon = Instant::now();
        let mut next_status = Instant::now();
        let mut next_tick = Instant::now() + Duration::from_secs(1);
        let mut buffer = [0u8; 4096];

        while Instant::now() < deadline {
            let now = Instant::now();

            if now >= next_beacon {
                if !targets.is_empty() {
                    let packet = beacon_packet();
                    for target in &targets {
                        let _ = socket.send_to(&packet, target);
                    }
                    stats.beacons_sent += 1;
                }
                next_beacon = now + BEACON_INTERVAL;
            }

            if now >= next_status {
                if let Some(addr) = device {
                    let _ = socket.send_to(&status_packet(args.hotkey_ready), addr);
                    stats.statuses_sent += 1;
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

                    if kind != T_BEACON && kind != T_PING && device != Some(source) {
                        device = Some(source);
                        log(serde_json::json!({
                            "kind": "device_learned",
                            "addr": source.to_string(),
                        }));
                    }

                    match kind {
                        T_AUDIO => {
                            stats.audio_packets += 1;
                            stats.audio_bytes += len as u64;
                            handle_audio(
                                payload,
                                &mut decoder,
                                &mut wav,
                                &mut capture,
                                &mut stats,
                                &mut last_seq,
                                &mut in_session,
                                &mut last_audio,
                            )?;
                        }
                        T_EVENT => {
                            stats.events += 1;
                            handle_event(
                                payload,
                                &mut stats,
                                &mut in_session,
                                &mut last_audio,
                                args.verbose,
                            );
                        }
                        T_BEACON => {}
                        T_PING => stats.pings += 1,
                        _ => stats.unknown += 1,
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(format!("接收失败: {error}")),
            }

            let now = Instant::now();
            if in_session && now.duration_since(last_audio) > AUDIO_STALL {
                in_session = false;
                log(serde_json::json!({
                    "kind": "audio_stalled",
                    "silentMs": now.duration_since(last_audio).as_millis() as u64,
                    "sessionBlocks": stats.session_blocks,
                }));
            }

            if now >= next_tick {
                log(serde_json::json!({
                    "kind": "stats",
                    "elapsedS": started.elapsed().as_secs(),
                    "audioPackets": stats.audio_packets,
                    "blocks": stats.blocks,
                    "audioSeconds": stats.samples as f64 / f64::from(SAMPLE_RATE),
                    "lostBlocks": stats.lost_blocks,
                    "outOfOrder": stats.out_of_order,
                    "badPackets": stats.bad_packets,
                    "badBlocks": stats.bad_blocks,
                    "events": stats.events,
                    "pings": stats.pings,
                    "unknown": stats.unknown,
                    "device": device.map(|addr| addr.to_string()),
                }));
                if device.is_none() && started.elapsed().as_secs() == 10 {
                    log(serde_json::json!({
                        "kind": "hint",
                        "message": "10s 内未收到设备报文: 确认设备已进入「AI语音」、与电脑同一局域网(AP 隔离会挡 beacon)、防火墙已放行入站 UDP 33333",
                    }));
                }
                next_tick += Duration::from_secs(1);
            }
        }

        let wav_path = wav
            .finish()
            .map_err(|error| format!("收尾 WAV 失败: {error}"))?;
        let capture_path = capture
            .finish()
            .map_err(|error| format!("收尾 capture 失败: {error}"))?;
        log(serde_json::json!({
            "kind": "passport_probe_done",
            "wav": wav_path.display().to_string(),
            "capture": capture_path.display().to_string(),
            "blocks": stats.blocks,
            "samples": stats.samples,
            "audioSeconds": stats.samples as f64 / f64::from(SAMPLE_RATE),
            "lostBlocks": stats.lost_blocks,
            "outOfOrder": stats.out_of_order,
            "badPackets": stats.bad_packets,
            "badBlocks": stats.bad_blocks,
            "events": stats.events,
            "beaconRounds": stats.beacons_sent,
            "statusesSent": stats.statuses_sent,
        }));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_audio(
        payload: &[u8],
        decoder: &mut BlockDecoder,
        wav: &mut WavWriter,
        capture: &mut CaptureWriter,
        stats: &mut Stats,
        last_seq: &mut Option<u8>,
        in_session: &mut bool,
        last_audio: &mut Instant,
    ) -> Result<(), String> {
        if payload.len() != AUDIO_PAYLOAD_BYTES || payload[1] & 0x80 == 0 {
            stats.bad_packets += 1;
            return Ok(());
        }
        let seq = payload[0];

        // 与参考实现同口径: 0 或 >=128 视为重复/乱序，直接丢弃。
        if let Some(previous) = *last_seq {
            let gap = seq.wrapping_sub(previous);
            if gap == 0 || gap >= 128 {
                stats.out_of_order += 1;
                return Ok(());
            }
            if gap > 1 {
                stats.lost_blocks += u64::from(gap - 1);
            }
        }
        *last_seq = Some(seq);

        let block = &payload[2..];
        let Some(samples) = decoder.decode(block) else {
            stats.bad_blocks += 1;
            return Ok(());
        };

        if !*in_session {
            *in_session = true;
            stats.session_blocks = 0;
            // 会话起点也刷新: 否则静默判定会拿"进程启动时刻"当基线，误报 10s 停顿
            *last_audio = Instant::now();
            if !stats.resume_warned {
                stats.resume_warned = true;
                log(
                    serde_json::json!({ "kind": "session_resumed_without_voice_start", "seq": seq }),
                );
            }
        }

        wav.write_samples(&samples)
            .map_err(|error| format!("写入 WAV 失败: {error}"))?;
        capture
            .record(seq, block)
            .map_err(|error| format!("写入 capture 失败: {error}"))?;

        stats.blocks += 1;
        stats.samples += samples.len() as u64;
        stats.session_blocks += 1;
        *last_audio = Instant::now();
        Ok(())
    }

    fn handle_event(
        payload: &[u8],
        stats: &mut Stats,
        in_session: &mut bool,
        last_audio: &mut Instant,
        verbose: bool,
    ) {
        let text = String::from_utf8_lossy(payload);
        let trimmed = text.trim();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            log(serde_json::json!({ "kind": "event_unparsed", "raw": trimmed }));
            return;
        };
        let event = value
            .get("event")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        match event {
            "voice.start" => {
                *in_session = true;
                stats.session_blocks = 0;
                // 会话起点就是静默判定的基线；否则会拿进程启动时刻误报
                *last_audio = Instant::now();
                log(serde_json::json!({
                    "kind": "event",
                    "event": event,
                    "audio": value.get("audio").and_then(|v| v.as_str()),
                }));
            }
            "voice.end" => {
                log(serde_json::json!({
                    "kind": "event",
                    "event": event,
                    "sessionBlocks": stats.session_blocks,
                }));
                *in_session = false;
            }
            "key.action" => {
                log(serde_json::json!({
                    "kind": "event",
                    "event": event,
                    "action": value.get("action").and_then(|v| v.as_str()),
                }));
            }
            _ => {
                log(serde_json::json!({
                    "kind": "event",
                    "event": event,
                    "raw": if verbose { trimmed } else { "" },
                }));
            }
        }
    }

    // --- 离线解码 ---------------------------------------------------------

    fn decode_capture(capture: &Path, wav: Option<PathBuf>) -> Result<(), String> {
        let mut bytes = Vec::new();
        File::open(capture)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|error| format!("读取 capture 失败: {error}"))?;
        if bytes.len() < CAPTURE_MAGIC.len() || &bytes[..CAPTURE_MAGIC.len()] != CAPTURE_MAGIC {
            return Err("capture 文件头不符(期望 PSCAP1)".to_owned());
        }

        let wav_path = wav.unwrap_or_else(|| capture.with_extension("wav"));
        let mut writer =
            WavWriter::create(&wav_path).map_err(|error| format!("创建 WAV 失败: {error}"))?;
        let mut decoder = BlockDecoder::new();
        let mut offset = CAPTURE_MAGIC.len();
        let mut blocks = 0u64;
        let mut samples = 0u64;
        let mut bad = 0u64;

        while offset + 3 <= bytes.len() {
            let len = u16::from_le_bytes([bytes[offset + 1], bytes[offset + 2]]) as usize;
            offset += 3;
            let Some(block) = bytes.get(offset..offset + len) else {
                return Err(format!("capture 截断: 记录声明 {len}B，实际不足"));
            };
            offset += len;
            match decoder.decode(block) {
                Some(decoded) => {
                    writer
                        .write_samples(&decoded)
                        .map_err(|error| format!("写入 WAV 失败: {error}"))?;
                    blocks += 1;
                    samples += decoded.len() as u64;
                }
                None => bad += 1,
            }
        }

        let wav_path = writer
            .finish()
            .map_err(|error| format!("收尾 WAV 失败: {error}"))?;
        log(serde_json::json!({
            "kind": "passport_probe_decode",
            "capture": capture.display().to_string(),
            "wav": wav_path.display().to_string(),
            "blocks": blocks,
            "samples": samples,
            "audioSeconds": samples as f64 / f64::from(SAMPLE_RATE),
            "badBlocks": bad,
        }));
        Ok(())
    }

    fn log(value: serde_json::Value) {
        println!("{value}");
    }
}
