//! 会话驱动：把 Passport 的 `voice.start/voice.end` 映射到现有音频与按键。
//!
//! 这里是唯一持有 generation 的地方（`VoiceSession`），网络线程只送样本与命令，
//! 因此"旧会话音频进入新会话"在结构上不可能发生：代际由本驱动在会话开始时推进，
//! 且 `AudioRuntime` 自身还会按 generation 再校验一次。

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sayall_core::passport::KeyAction;
use sayall_core::{process_pcm, VoiceSession, VoiceSessionEvent, VoiceSessionState};

use crate::button_mapping::EngineMessage;
use crate::raw_input::{ButtonEdge, RemoteButton};

use super::Shared;

/// 会话内超过该时长没有音频即判定异常终止（参考实现同口径 2.5s）。
const AUDIO_STALL: Duration = Duration::from_millis(2500);

/// 短按 `key.action` 的按下持续时间：与参考实现 60ms 一致。
const KEY_TAP_HOLD: Duration = Duration::from_millis(60);

/// 音频出口抽象。生产实现转发到 `AudioRuntime`，测试用记录型假实现。
pub trait VoiceAudio: Send + Sync {
    fn begin(&self, generation: u64) -> Result<(), String>;
    /// 队列满时实现方返回 [`QUEUE_OVERFLOW`]；其它错误按会话错误记录。
    fn enqueue(&self, generation: u64, samples: Vec<i16>) -> Result<(), String>;
    fn finish(&self, generation: u64) -> Result<(), String>;
    fn interrupt(&self) -> Result<(), String>;
    /// `pc.status.audioReady`：端点已选且可用。
    fn ready(&self) -> bool;
}

/// `enqueue` 返回该哨兵表示有界队列满，驱动方只计数不判故障。
pub const QUEUE_OVERFLOW: &str = "audio_queue_overflow";

/// 按住说话热键抽象。生产实现把 `ensure_session_ime` + `SendInput` 包在一起。
pub trait HoldKey: Send + Sync {
    fn press(&self) -> Result<(), String>;
    fn release(&self) -> Result<(), String>;
    /// `pc.status.hotkeyReady`：用户已配置按住说话快捷键。
    fn ready(&self) -> bool {
        true
    }
}

pub(crate) enum SessionCommand {
    VoiceStart { audio: Option<String> },
    VoiceEnd,
    Block(Vec<i16>),
    KeyTap(KeyAction),
    LinkLost,
    Stop,
}

pub(crate) struct SessionDriver {
    audio: Arc<dyn VoiceAudio>,
    hold: Arc<dyn HoldKey>,
    engine: Option<std::sync::mpsc::Sender<EngineMessage>>,
    gain_db: Arc<Mutex<f32>>,
    shared: Arc<Shared>,
    session: VoiceSession,
    next_session_id: u8,
    held: bool,
    last_audio: Instant,
}

impl SessionDriver {
    pub(crate) fn new(
        audio: Arc<dyn VoiceAudio>,
        hold: Arc<dyn HoldKey>,
        engine: Option<std::sync::mpsc::Sender<EngineMessage>>,
        gain_db: Arc<Mutex<f32>>,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            audio,
            hold,
            engine,
            gain_db,
            shared,
            session: VoiceSession::default(),
            next_session_id: 0,
            held: false,
            last_audio: Instant::now(),
        }
    }

    pub(crate) fn run(&mut self, commands: Receiver<SessionCommand>) {
        loop {
            match commands.recv_timeout(Duration::from_millis(500)) {
                Ok(SessionCommand::Stop) => {
                    self.abort("stop");
                    break;
                }
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {
                    if self.session.state() == VoiceSessionState::Streaming
                        && self.last_audio.elapsed() > AUDIO_STALL
                    {
                        self.abort("audio_stalled");
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.abort("channel_closed");
                    break;
                }
            }
        }
    }

    fn handle(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::VoiceStart { audio } => self.start(audio.as_deref()),
            SessionCommand::VoiceEnd => self.end(),
            SessionCommand::Block(samples) => self.block(samples),
            SessionCommand::KeyTap(action) => self.tap(action),
            SessionCommand::LinkLost => self.abort("link_lost"),
            SessionCommand::Stop => self.abort("stop"),
        }
    }

    fn start(&mut self, audio: Option<&str>) {
        if self.session.state() != VoiceSessionState::Idle {
            // 重复 start：设备重发或 UPD 乱序都会出现，静默忽略比重建会话安全。
            crate::gatt_note(
                "passport session action=start result=ignored reason=already_active".to_owned(),
            );
            return;
        }
        let session_id = self.next_session_id;
        self.next_session_id = self.next_session_id.wrapping_add(1);
        if let Err(error) = self
            .session
            .apply(VoiceSessionEvent::StreamStarted { session_id })
        {
            crate::gatt_note(format!(
                "passport session action=start result=failed reason={error}"
            ));
            return;
        }
        let generation = self.session.generation();
        self.shared.set_generation(generation);

        if let Err(error) = self.audio.begin(generation) {
            self.shared.set_error(format!("音频会话开始失败：{error}"));
            crate::gatt_note(format!(
                "passport session action=start generation={generation} result=failed error_domain=audio reason=begin_rejected"
            ));
            let _ = self.session.apply(VoiceSessionEvent::Interrupted);
            return;
        }
        if let Err(error) = self.hold.press() {
            // 热键失败不阻断音频：先把会话收掉，避免半开状态。
            crate::gatt_note(format!(
                "passport session action=press result=failed reason={error}"
            ));
            let _ = self.audio.interrupt();
            let _ = self.session.apply(VoiceSessionEvent::Interrupted);
            return;
        }
        self.held = true;
        self.last_audio = Instant::now();
        self.shared.set_streaming(true);
        self.shared.count_session();
        crate::gatt_note(format!(
            "passport session action=start generation={generation} codec={} result=passed",
            audio.unwrap_or("unknown")
        ));
    }

    fn block(&mut self, samples: Vec<i16>) {
        if self.session.state() != VoiceSessionState::Streaming {
            return;
        }
        self.last_audio = Instant::now();
        let generation = self.session.generation();
        let gain_db = *self
            .gain_db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let pcm = process_pcm(&samples, gain_db);
        let accepted = pcm.len();
        match self.audio.enqueue(generation, pcm) {
            Ok(()) => {
                let _ = self.session.apply(VoiceSessionEvent::AudioAccepted {
                    sample_count: accepted,
                });
                self.shared.add_samples(accepted as u64);
            }
            Err(error) if error == QUEUE_OVERFLOW => {
                self.shared.count_queue_drop();
            }
            Err(error) => {
                self.shared.set_error(format!("音频入队失败：{error}"));
                self.shared.count_queue_drop();
            }
        }
    }

    fn end(&mut self) {
        if self.session.state() != VoiceSessionState::Streaming {
            // 重复 end / 已因异常收尾：保持幂等。
            return;
        }
        let generation = self.session.generation();
        let Some(session_id) = self.session.session_id() else {
            self.abort("missing_session_id");
            return;
        };
        if let Err(error) = self
            .session
            .apply(VoiceSessionEvent::StreamStopped { session_id })
        {
            self.shared
                .set_error(format!("结束会话状态不一致：{error}"));
            self.abort("stale_stop");
            return;
        }
        let drain = self.audio.finish(generation);
        self.release_hold();
        self.shared.set_streaming(false);

        if let Err(error) = drain {
            self.shared.set_error(format!("音频排空失败：{error}"));
            crate::gatt_note(format!(
                "passport session action=finish generation={generation} result=failed reason=drain"
            ));
            let _ = self.session.apply(VoiceSessionEvent::Interrupted);
            return;
        }
        let _ = self
            .session
            .apply(VoiceSessionEvent::DrainCompleted { generation });
        crate::gatt_note(format!(
            "passport session action=finish generation={generation} result=passed"
        ));
    }

    /// 异常收尾：断链、静默、停止都走这里，必须幂等且一定释放按住键。
    fn abort(&mut self, reason: &str) {
        let was_active = self.session.state() != VoiceSessionState::Idle;
        if was_active {
            let _ = self.audio.interrupt();
            let _ = self.session.apply(VoiceSessionEvent::Interrupted);
        }
        self.release_hold();
        self.shared.set_streaming(false);
        if was_active {
            if let Some(engine) = &self.engine {
                // 等同于"设备被移除"：引擎会释放所有按住状态，杜绝卡键。
                let _ = engine.send(EngineMessage::DeviceRemoved);
            }
            crate::gatt_note(format!(
                "passport session action=abort reason={reason} result=passed"
            ));
        }
    }

    fn release_hold(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        if let Err(error) = self.hold.release() {
            self.shared
                .set_error(format!("释放按住说话键失败：{error}"));
        }
    }

    /// `key.action`：当前固件只会发 `enter`（短按提交），映射到语义键 `Ok`，
    /// 由现有按键映射引擎按用户配置执行动作，不再另建一套快捷键表。
    fn tap(&mut self, action: KeyAction) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        match action {
            KeyAction::Enter => {
                std::thread::spawn(move || {
                    let _ = engine.send(EngineMessage::GateEdge(ButtonEdge {
                        button: RemoteButton::Ok,
                        is_pressed: true,
                    }));
                    std::thread::sleep(KEY_TAP_HOLD);
                    let _ = engine.send(EngineMessage::GateEdge(ButtonEdge {
                        button: RemoteButton::Ok,
                        is_pressed: false,
                    }));
                });
            }
            KeyAction::Clear => {
                crate::gatt_note(
                    "passport key action=clear result=ignored reason=unsupported".to_owned(),
                );
            }
            KeyAction::Other(name) => {
                crate::gatt_note(format!(
                    "passport key action=unknown name={name} result=ignored"
                ));
            }
        }
    }
}
