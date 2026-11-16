#![cfg(windows)]
//! Passport 真机验收（P2.1 之后的端到端体验验证）。
//!
//! 分两个场景，必须逐个运行（`--ignored` 会跑文件里所有 ignored 用例）：
//!
//! ```text
//! cargo test -p sayall-windows --test passport_acceptance -- --ignored --nocapture --exact passport_session_and_recovery
//! cargo test -p sayall-windows --test passport_acceptance -- --ignored --nocapture --exact ble_return_after_passport
//! ```
//!
//! 场景一覆盖：设备发现 → 单次会话与 `voice.end` 收尾 → 连续会话 generation →
//! 短按观测 → 12 秒断链与恢复。
//! 场景二覆盖：切回 BLE → 连接已配对的 RC003/RC001 → 语音样本增长。

use std::time::{Duration, Instant};

use sayall_core::passport::PassportPhase;
use sayall_core::DeviceMode;
use sayall_windows::send_input::{KeyChord, KeyCode};
use sayall_windows::ConnectionPhase;
use sayall_windows::WindowsPlatform;

const POLL: Duration = Duration::from_millis(250);

fn wait_until(
    platform: &WindowsPlatform,
    timeout: Duration,
    mut predicate: impl FnMut(&sayall_core::passport::PassportSnapshot) -> bool,
) -> Option<sayall_core::passport::PassportSnapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = platform.passport_snapshot();
        if predicate(&snapshot) {
            return Some(snapshot);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

fn sample_rate_seconds(samples: u64) -> f64 {
    samples as f64 / 16_000.0
}

/// 两个场景都必须先选输出端点：未选端点时 `AudioRuntime::begin_session` 会拒绝，
/// 表现为“遥控器/设备都正常但完全没有音频”。
fn select_output_endpoint(platform: &WindowsPlatform) {
    let endpoints = platform.list_audio_endpoints().expect("枚举音频端点失败");
    let endpoint = endpoints
        .iter()
        .find(|endpoint| endpoint.is_virtual_cable_candidate)
        .or_else(|| endpoints.first())
        .expect("没有任何音频输出端点");
    platform
        .select_audio_endpoint(endpoint.id.clone())
        .expect("选择音频端点失败");
    eprintln!("[audio] 已选择输出端点: {}", endpoint.name);
}

#[test]
#[ignore = "真实 AI Passport + 虚拟声卡；按场景手动运行"]
fn passport_session_and_recovery() {
    let platform = WindowsPlatform::default();
    select_output_endpoint(&platform);
    platform.set_voice_hold_hotkey(Some(KeyChord {
        keys: vec![KeyCode::RightShift],
    }));
    platform
        .switch_device_mode(DeviceMode::Passport)
        .expect("切换到 Passport 模式失败");
    assert_eq!(platform.device_mode(), DeviceMode::Passport);

    // ---- S1 设备发现 ----
    eprintln!("[S1] 请在 90 秒内确认设备停在「AI 语音」界面……");
    let discovered = wait_until(&platform, Duration::from_secs(90), |s| s.device.is_some())
        .expect("S1 未发现设备");
    eprintln!("[S1] passed: device={:?}", discovered.device);

    // ---- S2 单次会话与 voice.end ----
    eprintln!("[S2] 请按住语音键说 1~2 秒后松开……");
    let session = wait_until(&platform, Duration::from_secs(120), |s| {
        s.sessions >= 1 && s.blocks > 0
    })
    .expect("S2 未收到语音");
    eprintln!(
        "[S2] 收到音频: sessions={} blocks={} seconds={:.2}",
        session.sessions,
        session.blocks,
        sample_rate_seconds(session.samples)
    );
    let finished = wait_until(&platform, Duration::from_secs(10), |s| {
        s.phase != PassportPhase::Streaming
    });
    assert!(finished.is_some(), "S2 voice.end 后仍停在 Streaming");

    // ---- S3 连续会话 ----
    eprintln!("[S3] 请再按住语音键说 1~2 秒后松开（验证连续会话）……");
    let second = wait_until(&platform, Duration::from_secs(120), |s| s.sessions >= 2)
        .expect("S3 第二次会话未开始");
    assert_eq!(second.generation, 2, "连续会话 generation 未推进");
    eprintln!(
        "[S3] passed: sessions={} generation={} blocks={}",
        second.sessions, second.generation, second.blocks
    );

    // ---- S4 短按观测（不强制断言：取决于用户是否给「确定」配了动作）----
    let before = platform.button_mapping_snapshot().fired_gestures;
    eprintln!("[S4] 请短按一次语音键（不要按住），用于观测 key.action → 确定键映射……");
    std::thread::sleep(Duration::from_secs(15));
    let after = platform.button_mapping_snapshot().fired_gestures;
    eprintln!("[S4] 观测: fired_gestures {before} -> {after}（未配置动作时为 0 → 0）");

    platform
        .switch_device_mode(DeviceMode::Ble)
        .expect("切回 BLE 失败");
    assert_eq!(platform.passport_snapshot().phase, PassportPhase::Stopped);
    eprintln!("[S5] passed: 已切回 BLE，Passport 已停止");
}

#[test]
#[ignore = "真实 AI Passport；按场景手动运行"]
fn passport_link_recovery() {
    let platform = WindowsPlatform::default();
    platform
        .switch_device_mode(DeviceMode::Passport)
        .expect("切换到 Passport 模式失败");

    eprintln!("[R1] 请确认设备停在「AI 语音」界面……");
    wait_until(&platform, Duration::from_secs(90), |s| s.device.is_some()).expect("R1 未发现设备");
    eprintln!("[R1] passed: 设备已发现");

    eprintln!("[R2] 请离开「AI 语音」界面（或关闭设备 Wi-Fi），等待约 15 秒……");
    let lost = wait_until(&platform, Duration::from_secs(40), |s| {
        // 链路超时先清对端、会话线程随后 abort；两者都落定才算断开完成。
        s.device.is_none() && s.phase == PassportPhase::Listening
    })
    .expect("R2 未在 40 秒内完成断链");
    eprintln!("[R2] passed: phase={:?}", lost.phase);

    eprintln!("[R3] 请重新进入「AI 语音」界面……");
    let recovered = wait_until(&platform, Duration::from_secs(90), |s| s.device.is_some())
        .expect("R3 设备未恢复");
    eprintln!("[R3] passed: device={:?}", recovered.device);

    platform
        .switch_device_mode(DeviceMode::Ble)
        .expect("切回 BLE 失败");
    eprintln!("[R4] passed: 已切回 BLE");
}

#[test]
#[ignore = "真实 RC001/RC003 遥控器；按场景手动运行"]
fn ble_return_after_passport() {
    let platform = WindowsPlatform::default();
    assert_eq!(platform.device_mode(), DeviceMode::Ble);
    select_output_endpoint(&platform);

    let remotes = platform.scan_paired_remotes().expect("扫描已配对设备失败");
    let remote = remotes
        .iter()
        .find(|remote| remote.is_supported_candidate)
        .expect("没有已配对的 RC001/RC003");
    eprintln!("[B1] 连接 {}（{}）……", remote.name, remote.id);
    platform
        .connect_remote(remote.id.clone())
        .expect("连接遥控器失败");

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let connection = platform.connection_snapshot();
        if matches!(
            connection.phase,
            ConnectionPhase::Ready | ConnectionPhase::Streaming | ConnectionPhase::Draining
        ) {
            eprintln!("[B1] passed: phase={:?}", connection.phase);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B1 60 秒内未进入 Ready：{:?} / {:?}",
            connection.phase,
            connection.last_error
        );
        std::thread::sleep(POLL);
    }

    let baseline = platform.connection_snapshot().decoded_samples;
    eprintln!("[B2] 请按住遥控器语音键说 1~2 秒后松开……");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let connection = platform.connection_snapshot();
        if connection.decoded_samples > baseline {
            eprintln!(
                "[B2] passed: decoded_samples {} -> {}",
                baseline, connection.decoded_samples
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "B2 未收到蓝牙语音（baseline={baseline}, phase={:?}, error={:?}）",
            connection.phase,
            connection.last_error
        );
        std::thread::sleep(POLL);
    }

    platform.disconnect_remote().expect("断开遥控器失败");
    eprintln!("[B3] passed: 已断开，BLE 路径可用");
}
