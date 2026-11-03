//! FoloToy AI Passport（Wi-Fi/UDP）通道的纯协议与解码。
//!
//! 与 BLE/ATVV 完全平行：这里不解析 ATVV 帧，也不依赖任何平台 API。
//! 协议契约见 FoloToy `windows/udp_transport.py`、`device/main/apps/voice/udp_audio.h`
//! 与 `app_state.c`（MIT，来源记录见仓库根 `ATTRIBUTION.md`）。

mod block;
mod protocol;

pub use block::{
    parse_audio, AudioBlock, BlockDecoder, BlockError, SequenceTracker, BLOCK_BYTES, BLOCK_SAMPLES,
    BLOCK_SAMPLES_PER_SECOND,
};
pub use protocol::{
    beacon_packet, parse_event, status_packet, KeyAction, PassportEvent, ProtocolError, T_AUDIO,
    T_BEACON, T_EVENT, T_PING, UDP_PORT,
};

use serde::{Deserialize, Serialize};

/// 通道阶段。放在 core 是为了让 `PlatformSnapshot` 在非 Windows 主机上也能携带该状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassportPhase {
    #[default]
    Stopped,
    Listening,
    DeviceLinked,
    Streaming,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PassportSnapshot {
    pub phase: PassportPhase,
    pub device: Option<String>,
    pub port: u16,
    pub blocks: u64,
    pub samples: u64,
    pub lost_blocks: u64,
    pub out_of_order: u64,
    pub bad_packets: u64,
    pub sessions: u64,
    pub audio_queue_drops: u64,
    pub generation: u64,
    pub last_error: Option<String>,
}
