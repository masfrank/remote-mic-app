//! FoloToy AI Passport 运行时（Wi-Fi/UDP 语音通道）。
//!
//! 与 BLE/ATVV 完全平行：自带 UDP 线程、beacon 与 `pc.status` 心跳、会话驱动，
//! 复用现有 `AudioRuntime`、按键映射引擎与按住说话热键，不依赖 `BleRuntime`。
//!
//! 典型用法（P1 由隐藏开发开关驱动，尚无正式 UI）：
//! ```ignore
//! let runtime = PassportRuntime::new(PassportConfig {
//!     audio, hold, engine: Some(button_mapping.sender()), port: 33333, extra_peers: vec![],
//! });
//! runtime.start()?;
//! // ...
//! runtime.stop()?;
//! ```

pub(crate) mod ime_probe;
mod net;
mod session;

pub use sayall_core::passport::{PassportPhase, PassportSnapshot};
pub use session::{HoldKey, VoiceAudio, QUEUE_OVERFLOW};

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::button_mapping::EngineMessage;
use crate::PlatformError;

/// 与固件约定的端口；`PassportConfig::port` 传 0 时由内核分配（测试用）。
pub const PORT: u16 = sayall_core::passport::UDP_PORT;

pub struct PassportConfig {
    pub audio: Arc<dyn VoiceAudio>,
    pub hold: Arc<dyn HoldKey>,
    /// 按键短按注入通道；`None` 时 `key.action` 只记日志。
    pub engine: Option<Sender<EngineMessage>>,
    pub port: u16,
    /// 额外的单播目标：跨网段/VPN 或测试时指定设备地址。
    pub extra_peers: Vec<Ipv4Addr>,
}

pub(crate) struct Shared {
    running: AtomicBool,
    failed: AtomicBool,
    streaming: AtomicBool,
    port: AtomicU16,
    local_addr: Mutex<Option<SocketAddr>>,
    device: Mutex<Option<SocketAddr>>,
    blocks: AtomicU64,
    samples: AtomicU64,
    lost: AtomicU64,
    out_of_order: AtomicU64,
    bad_packets: AtomicU64,
    sessions: AtomicU64,
    queue_drops: AtomicU64,
    generation: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            streaming: AtomicBool::new(false),
            port: AtomicU16::new(0),
            local_addr: Mutex::new(None),
            device: Mutex::new(None),
            blocks: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            out_of_order: AtomicU64::new(0),
            bad_packets: AtomicU64::new(0),
            sessions: AtomicU64::new(0),
            queue_drops: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            last_error: Mutex::new(None),
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub(crate) fn set_running(&self, running: bool) {
        self.running.store(running, Ordering::SeqCst);
    }

    pub(crate) fn set_streaming(&self, streaming: bool) {
        self.streaming.store(streaming, Ordering::SeqCst);
    }

    pub(crate) fn device(&self) -> Option<SocketAddr> {
        *lock(&self.device)
    }

    pub(crate) fn set_device(&self, addr: SocketAddr) {
        *lock(&self.device) = Some(addr);
    }

    pub(crate) fn clear_device(&self) {
        *lock(&self.device) = None;
    }

    pub(crate) fn set_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::SeqCst);
    }

    pub(crate) fn count_session(&self) {
        self.sessions.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_block(&self) {
        self.blocks.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_samples(&self, samples: u64) {
        self.samples.fetch_add(samples, Ordering::Relaxed);
    }

    pub(crate) fn set_lost(&self, lost: u64) {
        self.lost.store(lost, Ordering::Relaxed);
    }

    pub(crate) fn set_out_of_order(&self, count: u64) {
        self.out_of_order.store(count, Ordering::Relaxed);
    }

    pub(crate) fn count_bad_packet(&self) {
        self.bad_packets.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_queue_drop(&self) {
        self.queue_drops.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn set_error(&self, message: String) {
        crate::gatt_note(format!("passport error detail={message}"));
        *lock(&self.last_error) = Some(message);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct PassportRuntime {
    config: PassportConfig,
    gain_db: Arc<Mutex<f32>>,
    shared: Arc<Shared>,
    lifecycle: Mutex<()>,
    commands: Mutex<Option<Sender<session::SessionCommand>>>,
    net: Mutex<Option<JoinHandle<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl PassportRuntime {
    pub fn new(config: PassportConfig) -> Self {
        Self {
            config,
            gain_db: Arc::new(Mutex::new(0.0)),
            shared: Arc::new(Shared::new()),
            lifecycle: Mutex::new(()),
            commands: Mutex::new(None),
            net: Mutex::new(None),
            worker: Mutex::new(None),
        }
    }

    /// 复用设置里的语音增益（`AppSettings.gain_db`），与 BLE 路径同口径。
    pub fn with_gain(mut self, gain_db: Arc<Mutex<f32>>) -> Self {
        self.gain_db = gain_db;
        self
    }

    /// 幂等：已在运行时直接返回 Ok。
    pub fn start(&self) -> Result<(), PlatformError> {
        let _guard = lock(&self.lifecycle);
        if lock(&self.worker).is_some() {
            return Ok(());
        }

        let port = self.config.port;
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|error| {
            let message = format!(
                "绑定 UDP {port} 失败：{error}（端口可能被旧 Passport 程序占用，或需放行入站 UDP）"
            );
            self.shared.failed.store(true, Ordering::SeqCst);
            self.shared.set_error(message.clone());
            PlatformError::WindowsApi(message)
        })?;
        socket
            .set_broadcast(true)
            .map_err(|error| PlatformError::WindowsApi(format!("开启 UDP 广播失败：{error}")))?;
        socket
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .map_err(|error| PlatformError::WindowsApi(format!("设置 UDP 读超时失败：{error}")))?;

        let local = socket
            .local_addr()
            .map_err(|error| PlatformError::WindowsApi(format!("读取本地端口失败：{error}")))?;
        *lock(&self.shared.local_addr) = Some(local);
        self.shared.port.store(local.port(), Ordering::SeqCst);
        self.shared.failed.store(false, Ordering::SeqCst);
        self.shared.set_running(true);

        let (tx, rx) = mpsc::channel();
        *lock(&self.commands) = Some(tx.clone());

        let shared = Arc::clone(&self.shared);
        let audio = Arc::clone(&self.config.audio);
        let hold = Arc::clone(&self.config.hold);
        let gain = Arc::clone(&self.gain_db);
        let engine = self.config.engine.clone();
        let worker = std::thread::Builder::new()
            .name("sayall-passport-session".to_owned())
            .spawn(move || {
                let mut driver =
                    session::SessionDriver::new(audio, hold, engine, gain, Arc::clone(&shared));
                driver.run(rx);
            })
            .map_err(|error| {
                self.shared.set_running(false);
                PlatformError::WindowsApi(format!("启动 Passport 会话线程失败：{error}"))
            })?;

        let shared = Arc::clone(&self.shared);
        let audio = Arc::clone(&self.config.audio);
        let hold = Arc::clone(&self.config.hold);
        let peers = self.config.extra_peers.clone();
        let actual_port = local.port();
        let net = std::thread::Builder::new()
            .name("sayall-passport-net".to_owned())
            .spawn(move || {
                net::run(socket, actual_port, shared, audio, hold, tx, peers);
            })
            .map_err(|error| {
                self.shared.set_running(false);
                PlatformError::WindowsApi(format!("启动 Passport 网络线程失败：{error}"))
            })?;

        *lock(&self.worker) = Some(worker);
        *lock(&self.net) = Some(net);
        if !self.config.hold.ready() {
            // 固件要求 hotkeyReady 才允许按住说话：未配置时音频链路完全不会启动，
            // 这条日志是唯一能解释"设备显示通道未就绪"的线索。
            crate::gatt_note(
                "passport hotkey action=unconfigured hint=设备将不允许按住说话，请先在连接页配置按住说话快捷键"
                    .to_owned(),
            );
        }
        crate::gatt_note(format!(
            "passport runtime action=start port={} result=passed",
            local.port()
        ));
        Ok(())
    }

    /// 幂等：未运行时直接返回 Ok。停止顺序固定为「停网络 → 停会话 → 释放按键」。
    pub fn stop(&self) -> Result<(), PlatformError> {
        let _guard = lock(&self.lifecycle);
        let worker = lock(&self.worker).take();
        let net = lock(&self.net).take();
        if worker.is_none() && net.is_none() {
            return Ok(());
        }

        self.shared.set_running(false);
        if let Some(tx) = lock(&self.commands).take() {
            let _ = tx.send(session::SessionCommand::Stop);
        }
        // 网络线程最多阻塞 200ms（读超时），不会拖住停止。
        if let Some(handle) = net {
            let _ = handle.join();
        }
        if let Some(handle) = worker {
            let _ = handle.join();
        }
        *lock(&self.shared.device) = None;
        self.shared.set_streaming(false);
        crate::gatt_note("passport runtime action=stop result=passed".to_owned());
        Ok(())
    }

    pub fn snapshot(&self) -> PassportSnapshot {
        let shared = &self.shared;
        let device = shared.device();
        let phase = if shared.is_running() {
            if shared.streaming.load(Ordering::SeqCst) {
                PassportPhase::Streaming
            } else if device.is_some() {
                PassportPhase::DeviceLinked
            } else {
                PassportPhase::Listening
            }
        } else if shared.failed.load(Ordering::SeqCst) {
            PassportPhase::Error
        } else {
            PassportPhase::Stopped
        };
        PassportSnapshot {
            phase,
            device: device.map(|addr| addr.to_string()),
            port: shared.port.load(Ordering::SeqCst),
            blocks: shared.blocks.load(Ordering::Relaxed),
            samples: shared.samples.load(Ordering::Relaxed),
            lost_blocks: shared.lost.load(Ordering::Relaxed),
            out_of_order: shared.out_of_order.load(Ordering::Relaxed),
            bad_packets: shared.bad_packets.load(Ordering::Relaxed),
            sessions: shared.sessions.load(Ordering::Relaxed),
            audio_queue_drops: shared.queue_drops.load(Ordering::Relaxed),
            generation: shared.generation.load(Ordering::SeqCst),
            last_error: lock(&shared.last_error).clone(),
        }
    }

    /// 实际绑定地址（`port: 0` 时用于测试与诊断）。
    pub fn local_addr(&self) -> Option<SocketAddr> {
        *lock(&self.shared.local_addr)
    }
}

impl Drop for PassportRuntime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
