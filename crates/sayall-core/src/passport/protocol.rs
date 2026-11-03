//! UDP 报文格式：1 字节类型 + 载荷。
//!
//! 上行（设备→电脑）：`AUDIO` 定长 807B、`EVENT` UTF-8 JSON 行、`PING` 空载荷。
//! 下行（电脑→设备）：`BEACON` JSON、`EVENT` JSON 行（`pc.status`）。

use serde_json::Value;

pub const UDP_PORT: u16 = 33_333;

pub const T_AUDIO: u8 = 0x01;
pub const T_EVENT: u8 = 0x02;
pub const T_BEACON: u8 = 0x03;
pub const T_PING: u8 = 0x04;

/// 固件 `APP_PROTO_RX_CAP`：单条下行 JSON 的上限，超出会被设备直接丢弃。
pub const MAX_EVENT_BYTES: usize = 513;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassportEvent {
    Hello {
        proto: Option<u64>,
    },
    VoiceStart {
        audio: Option<String>,
    },
    VoiceEnd,
    KeyAction {
        action: KeyAction,
    },
    /// 设备侧的丢帧统计，仅用于诊断。
    Status {
        dropped: Option<u64>,
    },
    /// 固件新增但本端尚未建模的事件：保留名字，避免解析失败。
    Unknown {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Enter,
    Clear,
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    Empty,
    TooLarge,
    NotJson,
    MissingEvent,
}

/// JSON 行可能带结尾 `\n`（协议要求）或填充 NUL；统一剥掉再解析。
fn trimmed(mut payload: &[u8]) -> &[u8] {
    while let Some((last, rest)) = payload.split_last() {
        if *last == b'\n' || *last == b'\r' || *last == 0 {
            payload = rest;
        } else {
            break;
        }
    }
    payload
}

pub fn parse_event(payload: &[u8]) -> Result<PassportEvent, ProtocolError> {
    if payload.is_empty() {
        return Err(ProtocolError::Empty);
    }
    if payload.len() > MAX_EVENT_BYTES {
        return Err(ProtocolError::TooLarge);
    }
    let body = trimmed(payload);
    if body.is_empty() {
        return Err(ProtocolError::Empty);
    }
    let value: Value = serde_json::from_slice(body).map_err(|_| ProtocolError::NotJson)?;
    let name = value
        .get("event")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::MissingEvent)?;

    Ok(match name {
        "device.hello" => PassportEvent::Hello {
            proto: value.get("proto").and_then(Value::as_u64),
        },
        "voice.start" => PassportEvent::VoiceStart {
            audio: value
                .get("audio")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "voice.end" => PassportEvent::VoiceEnd,
        "key.action" => PassportEvent::KeyAction {
            action: match value.get("action").and_then(Value::as_str) {
                Some("enter") => KeyAction::Enter,
                Some("clear") => KeyAction::Clear,
                Some(other) => KeyAction::Other(other.to_owned()),
                None => KeyAction::Other(String::new()),
            },
        },
        "status" => PassportEvent::Status {
            dropped: value
                .get("dropped")
                .or_else(|| value.get("drop"))
                .and_then(Value::as_u64),
        },
        other => PassportEvent::Unknown {
            name: other.to_owned(),
        },
    })
}

/// `[0x03]{"v":1,"role":"pc","port":33333}`
///
/// 固件只据源地址学对端、不校验载荷，但保持与参考实现一致，便于两边互操作。
pub fn beacon_packet() -> Vec<u8> {
    let mut packet = Vec::with_capacity(48);
    packet.push(T_BEACON);
    packet.extend_from_slice(format!(r#"{{"v":1,"role":"pc","port":{UDP_PORT}}}"#).as_bytes());
    packet
}

/// `[0x02]{"type":"pc.status","audioReady":…,"hotkeyReady":…}\n`
///
/// 设备侧 `pc_ready = audioReady && hotkeyReady`，且 3.5s 不刷新会强制结束会话，
/// 因此调用方必须按秒发送。
pub fn status_packet(audio_ready: bool, hotkey_ready: bool) -> Vec<u8> {
    let mut packet = Vec::with_capacity(96);
    packet.push(T_EVENT);
    let body = serde_json::json!({
        "type": "pc.status",
        "audioReady": audio_ready,
        "hotkeyReady": hotkey_ready,
    })
    .to_string();
    packet.extend_from_slice(body.as_bytes());
    packet.push(b'\n');
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_voice_start_line_with_trailing_newline() {
        let event = parse_event(b"{\"event\":\"voice.start\",\"audio\":\"ima_adpcm\"}\n").unwrap();
        assert_eq!(
            event,
            PassportEvent::VoiceStart {
                audio: Some("ima_adpcm".to_owned())
            }
        );
    }

    #[test]
    fn parses_key_action_and_unknown_events() {
        assert_eq!(
            parse_event(b"{\"event\":\"key.action\",\"action\":\"enter\"}").unwrap(),
            PassportEvent::KeyAction {
                action: KeyAction::Enter
            }
        );
        assert_eq!(
            parse_event(b"{\"event\":\"agent.status\"}").unwrap(),
            PassportEvent::Unknown {
                name: "agent.status".to_owned()
            }
        );
    }

    #[test]
    fn rejects_broken_or_oversized_events() {
        assert_eq!(parse_event(b""), Err(ProtocolError::Empty));
        assert_eq!(parse_event(b"not json"), Err(ProtocolError::NotJson));
        assert_eq!(parse_event(b"{}"), Err(ProtocolError::MissingEvent));
        let oversized = vec![b'a'; MAX_EVENT_BYTES + 1];
        assert_eq!(parse_event(&oversized), Err(ProtocolError::TooLarge));
    }

    #[test]
    fn status_packet_shape_matches_firmware_contract() {
        let packet = status_packet(true, false);
        assert_eq!(packet[0], T_EVENT);
        assert_eq!(packet.last(), Some(&b'\n'));
        let body: Value = serde_json::from_slice(&packet[1..packet.len() - 1]).unwrap();
        assert_eq!(body["type"], "pc.status");
        assert_eq!(body["audioReady"], true);
        assert_eq!(body["hotkeyReady"], false);
    }

    #[test]
    fn beacon_packet_shape_matches_reference() {
        let packet = beacon_packet();
        assert_eq!(packet[0], T_BEACON);
        assert_eq!(&packet[1..], br#"{"v":1,"role":"pc","port":33333}"#);
    }
}
