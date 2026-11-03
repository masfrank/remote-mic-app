#![cfg(windows)]
//! 真机验收：真实 AI Passport → UDP → 解码 → 现有 `AudioRuntime`。
//!
//! 需要：设备与电脑同一局域网、已安装虚拟声卡。测试通过正式的
//! `switch_device_mode(Passport)` 启动通道（P2 起不再使用开发环境变量）。
//! 运行（必须带 `--ignored`，否则不会执行）：
//!
//! ```text
//! cargo test -p sayall-windows --test passport_hardware -- --ignored --nocapture
//! ```
//!
//! 测试会选中虚拟声卡候选端点、把按住说话键临时设为右 Shift，然后等待操作者
//! 按住设备语音键说话。它验证的是平台接线后的完整链路，不是模拟。

use std::time::{Duration, Instant};

use sayall_windows::send_input::{KeyChord, KeyCode};
use sayall_windows::WindowsPlatform;

const DISCOVERY_WINDOW: Duration = Duration::from_secs(60);
const STREAM_WINDOW: Duration = Duration::from_secs(90);

#[test]
#[ignore = "需要真实 AI Passport 与虚拟声卡；手动运行 --ignored"]
fn real_device_streams_through_audio_runtime() {
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
    // 固件要求 hotkeyReady 才允许按住说话；这里用右 Shift 充当按住说话键。
    platform.set_voice_hold_hotkey(Some(KeyChord {
        keys: vec![KeyCode::RightShift],
    }));
    // P2 起走正式模式切换（开发环境变量已移除）。
    platform
        .switch_device_mode(sayall_core::DeviceMode::Passport)
        .expect("切换到 Passport 模式失败");
    assert_eq!(platform.device_mode(), sayall_core::DeviceMode::Passport);
    eprintln!("== 启动后快照: {:?}", platform.passport_snapshot());

    let discovery_deadline = Instant::now() + DISCOVERY_WINDOW;
    let mut linked = false;
    let mut last_report = Instant::now();
    while Instant::now() < discovery_deadline {
        let snapshot = platform.passport_snapshot();
        if last_report.elapsed() > Duration::from_secs(5) {
            last_report = Instant::now();
            eprintln!("== 等待设备: {snapshot:?}");
        }
        if snapshot.device.is_some() {
            linked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("== 设备发现: {}", if linked { "passed" } else { "failed" });
    eprintln!("== 发现阶段快照: {:?}", platform.passport_snapshot());
    eprintln!("== 请现在按住 AI Passport 的语音键说一句话（1~2 秒）……");

    let stream_deadline = Instant::now() + STREAM_WINDOW;
    let mut streamed = false;
    while Instant::now() < stream_deadline {
        let snapshot = platform.passport_snapshot();
        if snapshot.sessions > 0 && snapshot.blocks > 0 {
            streamed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    let final_snapshot = platform.passport_snapshot();
    platform.stop_passport().expect("停止 Passport 运行时失败");
    eprintln!("== 最终快照: {final_snapshot:?}");

    assert!(
        linked,
        "未发现设备：检查同网段 / AP 隔离 / 防火墙 UDP 33333"
    );
    assert!(streamed, "未收到音频：确认按住了语音键，且虚拟声卡端点可用");
}
