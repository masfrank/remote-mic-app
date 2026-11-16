#![cfg(windows)]
//! VB-CABLE 客观验收：Passport 播放的 PCM 是否真的到达 `CABLE Output`。
//!
//! 之前所有测试只证明样本进入 `AudioRuntime` 队列；这里直接对虚拟声卡的**采集端**
//! 做电平测量（只统计峰值/帧数，不保存任何音频）。
//!
//! 运行：
//! ```text
//! cargo test -p sayall-windows --test cable_output_level -- --ignored --nocapture
//! ```
//! 需要：CABLE Output 端点存在、Passport 在「AI 语音」界面、测试期间按住语音键说话。

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sayall_core::passport::PassportPhase;
use sayall_core::DeviceMode;
use sayall_windows::send_input::{KeyChord, KeyCode};
use sayall_windows::WindowsPlatform;

const CAPTURE_NAME_HINT: &str = "CABLE Output";
/// -60 dBFS：低于此值视为静音（i16 满幅 32767）。
const SILENCE_PEAK: u32 = 33;

struct CableMeter {
    peak: Arc<AtomicU32>,
    frames: Arc<AtomicU64>,
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// 所有 WASAPI 对象都在采集线程内创建：COM 接口不实现 `Send`，
/// 且 `initialize_mta` 是线程级的。
fn run_capture(
    running: &AtomicBool,
    peak: &AtomicU32,
    frames: &AtomicU64,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    wasapi::initialize_mta();
    let enumerator = wasapi::DeviceEnumerator::new().map_err(|e| format!("枚举设备失败：{e}"))?;
    let collection = enumerator
        .get_device_collection(&wasapi::Direction::Capture)
        .map_err(|e| format!("读取采集端点失败：{e}"))?;
    let device = collection
        .into_iter()
        .filter_map(|item| item.ok())
        .find(|device| {
            device
                .get_friendlyname()
                .map(|name| name.contains(CAPTURE_NAME_HINT))
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("找不到包含「{CAPTURE_NAME_HINT}」的采集端点"))?;
    let name = device
        .get_friendlyname()
        .map_err(|e| format!("读取端点名失败：{e}"))?;
    let mut client = device
        .get_iaudioclient()
        .map_err(|e| format!("创建采集客户端失败：{e}"))?;
    let _ = client.get_mixformat();
    let (default_period, _) = client
        .get_device_period()
        .map_err(|e| format!("读取设备周期失败：{e}"))?;
    let format = wasapi::WaveFormat::new(16, 16, &wasapi::SampleType::Int, 16_000, 1, None);
    client
        .initialize_client(
            &format,
            &wasapi::Direction::Capture,
            &wasapi::StreamMode::PollingShared {
                autoconvert: true,
                buffer_duration_hns: default_period,
            },
        )
        .map_err(|e| format!("初始化采集客户端失败：{e}"))?;
    let capture = client
        .get_audiocaptureclient()
        .map_err(|e| format!("获取采集接口失败：{e}"))?;
    let frames_per_buffer = client
        .get_buffer_size()
        .map_err(|e| format!("读取缓冲区大小失败：{e}"))? as usize;
    client
        .start_stream()
        .map_err(|e| format!("启动采集失败：{e}"))?;
    // 采集已真正开始，才通知调用方；启动失败由外层补发 Err。
    ready.send(Ok(())).ok();
    eprintln!("[meter] 开始监听: {name}");

    let mut buffer = vec![0u8; frames_per_buffer.max(1) * 2];
    while running.load(Ordering::Relaxed) {
        match capture.read_from_device(&mut buffer) {
            Ok((count, _)) => {
                if count == 0 {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                frames.fetch_add(u64::from(count), Ordering::Relaxed);
                let mut local_peak = 0u32;
                for chunk in buffer[..count as usize * 2].chunks_exact(2) {
                    let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                    local_peak = local_peak.max(sample.unsigned_abs() as u32);
                }
                peak.fetch_max(local_peak, Ordering::Relaxed);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

impl CableMeter {
    fn start() -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let peak = Arc::new(AtomicU32::new(0));
        let frames = Arc::new(AtomicU64::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let thread = {
            let peak = Arc::clone(&peak);
            let frames = Arc::clone(&frames);
            let running = Arc::clone(&running);
            std::thread::spawn(move || {
                if let Err(error) = run_capture(&running, &peak, &frames, &ready_tx) {
                    // 启动阶段失败：Ok 还没发出，这里补 Err；运行期错误则已被消费。
                    let _ = ready_tx.send(Err(error));
                }
            })
        };

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                peak,
                frames,
                running,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                running.store(false, Ordering::Relaxed);
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                running.store(false, Ordering::Relaxed);
                let _ = thread.join();
                Err("采集启动超时".to_owned())
            }
        }
    }

    fn stop(mut self) -> (u32, u64) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        (
            self.peak.load(Ordering::Relaxed),
            self.frames.load(Ordering::Relaxed),
        )
    }
}

fn dbfs(peak: u32) -> f64 {
    if peak == 0 {
        return f64::NEG_INFINITY;
    }
    20.0 * (peak as f64 / 32_768.0).log10()
}

#[test]
#[ignore = "需要 VB-CABLE + 真实 AI Passport；手动运行 --ignored"]
fn cable_output_receives_passport_audio() {
    let platform = WindowsPlatform::default();
    let endpoints = platform.list_audio_endpoints().expect("枚举音频端点失败");
    let endpoint = endpoints
        .iter()
        .find(|endpoint| endpoint.is_virtual_cable_candidate)
        .or_else(|| endpoints.first())
        .expect("没有任何音频输出端点");
    platform
        .select_audio_endpoint(endpoint.id.clone())
        .expect("选择音频端点失败");
    eprintln!("[meter] 播放端点: {}", endpoint.name);
    platform.set_voice_hold_hotkey(Some(KeyChord {
        keys: vec![KeyCode::RightShift],
    }));
    platform
        .switch_device_mode(DeviceMode::Passport)
        .expect("切换到 Passport 模式失败");

    // 先等设备，再开采集：避免把"等待设备"的静音算进峰值窗口。
    let deadline = Instant::now() + Duration::from_secs(90);
    while platform.passport_snapshot().device.is_none() {
        assert!(Instant::now() < deadline, "90 秒内未发现设备");
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("[meter] 设备已发现，开始测量 CABLE Output……");

    let meter = CableMeter::start().expect("无法监听 CABLE Output");
    eprintln!("[meter] 请按住语音键说 1~2 秒……");

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut blocks = 0;
    while blocks == 0 {
        blocks = platform.passport_snapshot().blocks;
        assert!(Instant::now() < deadline, "120 秒内未收到 Passport 音频");
        std::thread::sleep(Duration::from_millis(250));
    }
    // 会话结束后再量 1 秒，覆盖 drain 尾巴。
    std::thread::sleep(Duration::from_secs(1));
    let (peak, frames) = meter.stop();
    let snapshot = platform.passport_snapshot();
    platform
        .switch_device_mode(DeviceMode::Ble)
        .expect("切回 BLE 失败");

    eprintln!(
        "[meter] Passport blocks={} samples={} phase={:?}；CABLE Output frames={} peak={} ({:.1} dBFS)",
        snapshot.blocks,
        snapshot.samples,
        snapshot.phase,
        frames,
        peak,
        dbfs(peak)
    );
    assert_eq!(
        platform.passport_snapshot().phase,
        PassportPhase::Stopped,
        "切回 BLE 后 Passport 未停止"
    );
    assert!(frames > 0, "CABLE Output 没有产生任何采集帧");
    assert!(
        peak > SILENCE_PEAK,
        "CABLE Output 全程静音（peak={peak}），PCM 没有真正到达虚拟声卡"
    );
}
