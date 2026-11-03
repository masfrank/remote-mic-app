#![cfg(windows)]
//! FoloToy AI Passport（Wi-Fi/UDP）通道的真实网络集成测试。
//!
//! 这里不 mock socket：测试进程用 std UDP socket 冒充设备，向 `PassportRuntime`
//! 实际绑定的临时端口发包，覆盖「收包 → 解码 → 音频引擎」和停止清理。
//! 全部用例都用 `port: 0`，并发跑也不会互相抢端口。

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sayall_core::passport::{BlockDecoder, BLOCK_BYTES, BLOCK_SAMPLES, T_AUDIO, T_EVENT};
use sayall_windows::passport::{HoldKey, PassportConfig, PassportRuntime, VoiceAudio};

/// 夹具块固定解出 1600 样本；20 块正好是 1 秒 16 kHz 音频。
const BLOCKS_PER_SESSION: usize = 20;
const BLOCK: &[u8] = include_bytes!("../../sayall-core/tests/fixtures/passport/block.bin");

/// 等待上限：UDP 在 loopback 上几乎不丢，但线程启动与解码调度需要余量。
/// 轮询而不是固定 sleep，避免快机器白等、慢机器偶发假失败。
const WAIT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct FakeAudio {
    log: Mutex<AudioLog>,
}

#[derive(Default)]
struct AudioLog {
    begins: Vec<u64>,
    finishes: Vec<u64>,
    interrupts: usize,
    enqueues: usize,
    samples: usize,
}

impl FakeAudio {
    /// 回调可能运行在运行时的收包线程上：取锁失败只丢一条统计，绝不 panic。
    fn record(&self, update: impl FnOnce(&mut AudioLog)) {
        if let Ok(mut log) = self.log.lock() {
            update(&mut log);
        }
    }

    fn snapshot(&self) -> AudioLog {
        self.log
            .lock()
            .map(|log| AudioLog {
                begins: log.begins.clone(),
                finishes: log.finishes.clone(),
                interrupts: log.interrupts,
                enqueues: log.enqueues,
                samples: log.samples,
            })
            .unwrap_or_default()
    }
}

impl VoiceAudio for FakeAudio {
    fn begin(&self, generation: u64) -> Result<(), String> {
        self.record(|log| log.begins.push(generation));
        Ok(())
    }

    fn enqueue(&self, _generation: u64, samples: Vec<i16>) -> Result<(), String> {
        self.record(|log| {
            log.enqueues += 1;
            log.samples += samples.len();
        });
        Ok(())
    }

    fn finish(&self, generation: u64) -> Result<(), String> {
        self.record(|log| log.finishes.push(generation));
        Ok(())
    }

    fn interrupt(&self) -> Result<(), String> {
        self.record(|log| log.interrupts += 1);
        Ok(())
    }

    fn ready(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct FakeHold {
    counts: Mutex<(usize, usize)>,
}

impl FakeHold {
    fn press(&self) {
        if let Ok(mut counts) = self.counts.lock() {
            counts.0 += 1;
        }
    }

    fn release(&self) {
        if let Ok(mut counts) = self.counts.lock() {
            counts.1 += 1;
        }
    }

    fn snapshot(&self) -> (usize, usize) {
        self.counts.lock().map(|counts| *counts).unwrap_or((0, 0))
    }
}

impl HoldKey for FakeHold {
    fn press(&self) -> Result<(), String> {
        FakeHold::press(self);
        Ok(())
    }

    fn release(&self) -> Result<(), String> {
        FakeHold::release(self);
        Ok(())
    }
}

/// 用例无论断言成功与否都要停掉后台线程，否则失败的用例会拖住测试进程。
struct StopGuard<'a> {
    runtime: &'a PassportRuntime,
    armed: bool,
}

impl<'a> StopGuard<'a> {
    fn new(runtime: &'a PassportRuntime) -> Self {
        Self {
            runtime,
            armed: true,
        }
    }

    /// 用例已经显式 stop 过时调用，避免第三次 stop 干扰「重复停止安全」的断言。
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for StopGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _stopped = self.runtime.stop();
        }
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + WAIT;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn event_packet(json: &str) -> Vec<u8> {
    let mut packet = Vec::with_capacity(json.len() + 2);
    packet.push(T_EVENT);
    packet.extend_from_slice(json.as_bytes());
    // 固件按 JSON 行发送；协议层会剥掉结尾换行，这里保持真实线上格式。
    packet.push(b'\n');
    packet
}

fn audio_packet(seq: u8) -> Vec<u8> {
    let mut packet = Vec::with_capacity(1 + 2 + BLOCK_BYTES);
    packet.push(T_AUDIO);
    packet.push(seq);
    packet.push(0x80);
    packet.extend_from_slice(BLOCK);
    packet
}

/// 冒充设备：connect 后源端口固定，运行时才能把后续包归到同一对端。
fn device_socket(runtime_addr: SocketAddr) -> UdpSocket {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fake device socket");
    socket
        .connect(runtime_addr)
        .expect("connect fake device socket");
    socket
}

/// 给运行时线程留出处理上一条报文的时间，是小包突发时最稳的节奏。
fn quiet() {
    thread::sleep(Duration::from_millis(2));
}

fn start_runtime(audio: Arc<FakeAudio>, hold: Arc<FakeHold>) -> (PassportRuntime, SocketAddr) {
    let runtime = PassportRuntime::new(PassportConfig {
        audio,
        hold,
        engine: None,
        port: 0,
        extra_peers: Vec::new(),
    });
    runtime.start().expect("PassportRuntime::start failed");
    let addr = runtime
        .local_addr()
        .expect("PassportRuntime::local_addr returned None after start");
    // 运行时绑 0.0.0.0:port；设备 socket 必须连具体地址，回环即可送达。
    let addr = if addr.ip().is_unspecified() {
        SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port()))
    } else {
        addr
    };
    (runtime, addr)
}

/// 协议层与夹具必须同步，否则「20 块 = 32000 样本」的断言会先失真。
fn assert_fixture_matches_protocol() {
    assert_eq!(BLOCK.len(), BLOCK_BYTES, "block fixture size drifted");
    let samples = BlockDecoder::new()
        .decode(BLOCK)
        .expect("block fixture failed to decode");
    assert_eq!(
        samples.len(),
        BLOCK_SAMPLES,
        "block fixture sample count drifted"
    );
}

#[test]
fn voice_session_end_to_end_decodes_all_blocks() {
    assert_fixture_matches_protocol();

    let audio = Arc::new(FakeAudio::default());
    let hold = Arc::new(FakeHold::default());
    let (runtime, addr) = start_runtime(audio.clone(), hold);
    let _guard = StopGuard::new(&runtime);
    let device = device_socket(addr);

    device
        .send(&event_packet(r#"{"event":"device.hello","proto":1}"#))
        .expect("send hello");
    quiet();
    device
        .send(&event_packet(r#"{"event":"voice.start","audio":"pcm"}"#))
        .expect("send voice.start");
    quiet();
    for seq in 0..BLOCKS_PER_SESSION {
        device
            .send(&audio_packet(seq as u8))
            .expect("send audio block");
        quiet();
    }
    device
        .send(&event_packet(r#"{"event":"voice.end"}"#))
        .expect("send voice.end");

    // 以「音频引擎收到 finish」为完成标志，再读快照，避免只等到半条流。
    assert!(
        wait_until(|| !audio.snapshot().finishes.is_empty()),
        "voice.end 未在 {:?} 内送达音频引擎",
        WAIT
    );

    let log = audio.snapshot();
    assert_eq!(
        log.begins,
        vec![1],
        "会话应以 generation 1 开始且只开始一次"
    );
    assert_eq!(log.finishes, vec![1], "会话应以 generation 1 结束");
    // UDP 可能丢块，断言下界即可；额外校验样本数，防止实现把块拆细后漏样本。
    assert!(
        log.enqueues >= BLOCKS_PER_SESSION,
        "只入队 {} 次，少于 {} 块",
        log.enqueues,
        BLOCKS_PER_SESSION
    );
    assert!(
        log.samples >= BLOCKS_PER_SESSION * BLOCK_SAMPLES,
        "只入队 {} 样本，少于 {}",
        log.samples,
        BLOCKS_PER_SESSION * BLOCK_SAMPLES
    );
    assert_eq!(log.interrupts, 0, "正常结束不应触发 interrupt");

    let snapshot = runtime.snapshot();
    assert!(
        snapshot.blocks >= 20,
        "运行时只统计到 {} 块",
        snapshot.blocks
    );
    assert_eq!(snapshot.lost_blocks, 0, "连续 seq 不应产生丢块");
    assert_eq!(snapshot.sessions, 1, "一次 voice.start/end 应只算一个会话");
}

#[test]
fn stopping_mid_session_releases_the_hold_key() {
    let audio = Arc::new(FakeAudio::default());
    let hold = Arc::new(FakeHold::default());
    let (runtime, addr) = start_runtime(audio.clone(), hold.clone());
    let _guard = StopGuard::new(&runtime);
    let device = device_socket(addr);

    device
        .send(&event_packet(r#"{"event":"device.hello","proto":1}"#))
        .expect("send hello");
    quiet();
    device
        .send(&event_packet(r#"{"event":"voice.start","audio":"pcm"}"#))
        .expect("send voice.start");

    assert!(
        wait_until(|| hold.snapshot().0 >= 1),
        "voice.start 后保持键没有按下"
    );

    // 模拟用户在录音中途关闭通道：stop 必须把残留的按下状态收回去。
    let _stopped = runtime.stop();

    let (presses, releases) = hold.snapshot();
    assert!(presses >= 1, "没有记录到按下");
    assert_eq!(
        presses, releases,
        "stop 后保持键未释放（press={presses}, release={releases}），会卡键"
    );
}

#[test]
fn double_start_and_stop_are_safe_and_release_the_port() {
    let audio = Arc::new(FakeAudio::default());
    let hold = Arc::new(FakeHold::default());
    let (runtime, addr) = start_runtime(audio, hold);
    let _guard = StopGuard::new(&runtime);

    // 第二次 start 允许返回 Ok（幂等）或明确错误，规范只要求不 panic。
    let _second_start = runtime.start();
    let _first_stop = runtime.stop();
    let _second_stop = runtime.stop();
    _guard.disarm();

    // 停止后同一端口必须能重新 bind；Windows 上 socket 释放是同步的，
    // 仍留少量重试余量，避免其他测试进程同时探测时被误判。
    let port = addr.port();
    let deadline = Instant::now() + WAIT;
    let mut rebound = None;
    while Instant::now() < deadline {
        match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
            Ok(socket) => {
                rebound = Some(socket);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
    assert!(
        rebound.is_some(),
        "stop 后端口 {port} 仍被占用，UDP socket 未释放"
    );
}

#[test]
fn malformed_packets_are_counted_and_do_not_break_the_session() {
    let audio = Arc::new(FakeAudio::default());
    let hold = Arc::new(FakeHold::default());
    let (runtime, addr) = start_runtime(audio.clone(), hold);
    let _guard = StopGuard::new(&runtime);
    let device = device_socket(addr);

    device
        .send(&event_packet(r#"{"event":"device.hello","proto":1}"#))
        .expect("send hello");
    quiet();

    let baseline = runtime.snapshot().bad_packets;
    // 长度错误的 AUDIO：parse_audio 要求 806B 载荷，这里故意只给 5B。
    device
        .send(&[T_AUDIO, 0x00, 0x80, 0x01, 0x02, 0x03, 0x04])
        .expect("send truncated audio");
    // 非法 JSON：事件解析失败也必须计入 bad_packets，而不是打断收包线程。
    device
        .send(&event_packet(r#"{"event":"#))
        .expect("send malformed event");

    assert!(
        wait_until(|| runtime.snapshot().bad_packets >= baseline + 2),
        "坏包未计数：baseline={baseline}, now={}",
        runtime.snapshot().bad_packets
    );

    // 坏包之后正常会话仍应可用，证明错误只累加诊断计数。
    device
        .send(&event_packet(r#"{"event":"voice.start","audio":"pcm"}"#))
        .expect("send voice.start");
    quiet();
    for seq in 0..2u8 {
        device.send(&audio_packet(seq)).expect("send audio block");
        quiet();
    }
    device
        .send(&event_packet(r#"{"event":"voice.end"}"#))
        .expect("send voice.end");

    assert!(
        wait_until(|| !audio.snapshot().finishes.is_empty()),
        "坏包之后的会话没有完成"
    );
    let log = audio.snapshot();
    assert_eq!(log.begins, vec![1], "坏包不应额外开启会话");
    assert!(
        log.samples >= 2 * BLOCK_SAMPLES,
        "坏包之后只收到 {} 样本",
        log.samples
    );

    let snapshot = runtime.snapshot();
    assert!(snapshot.blocks >= 2, "坏包之后块统计未恢复");
    assert!(
        snapshot.bad_packets >= baseline + 2,
        "bad_packets 没有保持增长"
    );
    assert_eq!(snapshot.sessions, 1, "坏包不应制造会话");
}

#[test]
fn consecutive_sessions_advance_generation_without_stale_audio() {
    let audio = Arc::new(FakeAudio::default());
    let hold = Arc::new(FakeHold::default());
    let (runtime, addr) = start_runtime(Arc::clone(&audio), Arc::clone(&hold));
    let _guard = StopGuard::new(&runtime);
    let device = device_socket(addr);

    assert_fixture_matches_protocol();
    for (session, seq) in [(0u8, 0u8), (1u8, 1u8)] {
        device
            .send(&event_packet(
                r#"{"event":"voice.start","audio":"ima_adpcm"}"#,
            ))
            .expect("send voice.start");
        quiet();
        device.send(&audio_packet(seq)).expect("send audio block");
        quiet();
        device
            .send(&event_packet(r#"{"event":"voice.end"}"#))
            .expect("send voice.end");
        quiet();
        assert!(
            wait_until(|| runtime.snapshot().sessions >= u64::from(session) + 1),
            "第 {} 次会话未开始",
            session + 1
        );
    }

    assert!(
        wait_until(|| !audio.snapshot().finishes.is_empty() && runtime.snapshot().sessions == 2),
        "第二次会话未完成"
    );
    let log = audio.snapshot();
    assert_eq!(log.begins, vec![1, 2], "generation 必须逐会话递增");
    assert_eq!(log.finishes, vec![1, 2], "每次会话都应按各自代际排空");
    assert_eq!(hold.snapshot(), (2, 2), "按住键必须成对");
    assert_eq!(runtime.snapshot().generation, 2);
    assert_eq!(runtime.snapshot().lost_blocks, 0);
}

#[test]
fn key_action_enter_maps_to_ok_edge_on_the_shared_engine() {
    use sayall_windows::button_mapping::EngineMessage;
    use sayall_windows::raw_input::RemoteButton;

    let (engine, edges) = std::sync::mpsc::channel();
    let runtime = PassportRuntime::new(PassportConfig {
        audio: Arc::new(FakeAudio::default()),
        hold: Arc::new(FakeHold::default()),
        engine: Some(engine),
        port: 0,
        extra_peers: Vec::new(),
    });
    runtime.start().expect("PassportRuntime::start failed");
    let _guard = StopGuard::new(&runtime);
    let raw = runtime.local_addr().expect("local addr");
    let addr = if raw.ip().is_unspecified() {
        SocketAddr::from((Ipv4Addr::LOCALHOST, raw.port()))
    } else {
        raw
    };
    let device = device_socket(addr);

    device
        .send(&event_packet(r#"{"event":"key.action","action":"enter"}"#))
        .expect("send key.action");

    let press = edges.recv_timeout(WAIT).expect("没有收到按下边沿");
    match press {
        EngineMessage::GateEdge(edge) => {
            assert_eq!(edge.button, RemoteButton::Ok);
            assert!(edge.is_pressed);
        }
        other => panic!("期望 GateEdge，实际 {other:?}"),
    }
    let release = edges.recv_timeout(WAIT).expect("没有收到释放边沿");
    match release {
        EngineMessage::GateEdge(edge) => {
            assert_eq!(edge.button, RemoteButton::Ok);
            assert!(!edge.is_pressed);
        }
        other => panic!("期望 GateEdge，实际 {other:?}"),
    }
}
