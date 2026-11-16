#![cfg(windows)]
//! P3.1 数据采集：Passport 首按 IME 观测，20 次为一组。
//!
//! 只观测、不恢复。每次按住说话后由 `ime_probe` 在 800ms / 1500ms 采样，
//! 结果写入诊断日志；本脚本额外打开 `SAYALL_PASSPORT_IME_TRACE` 让同样的行
//! 打印到 stderr，便于直接统计。
//!
//! 运行：
//! ```text
//! cargo test -p sayall-windows --test passport_ime_observation -- --ignored --nocapture
//! ```
//!
//! 前 10 次：保持当前输入法直接按；后 10 次：先把输入法切到别的工具再按。

use std::time::{Duration, Instant};

use sayall_core::settings::VoiceInputTool;
use sayall_core::DeviceMode;
use sayall_windows::send_input::{KeyChord, KeyCode};
use sayall_windows::WindowsPlatform;

const NORMAL_RUNS: u32 = 10;

/// 读取应用实际保存的按住说话快捷键（`voice-hold-hotkey.json`）；
/// 读不到时回退右 Shift 并明确打印，避免静默用错配置。
fn configured_hotkey() -> (KeyChord, String) {
    let path = std::env::var_os("APPDATA").map(|dir| {
        std::path::PathBuf::from(dir)
            .join("app.getsayall.remote-mic.windows")
            .join("voice-hold-hotkey.json")
    });
    if let Some(path) = path {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(chord) = serde_json::from_str::<KeyChord>(&text) {
                if !chord.keys.is_empty() {
                    return (chord, format!("{}", path.display()));
                }
            }
        }
    }
    (
        KeyChord {
            keys: vec![KeyCode::RightShift],
        },
        "回退右 Shift（未读到配置）".to_owned(),
    )
}
const SWITCHED_RUNS: u32 = 10;

#[test]
#[ignore = "真实 AI Passport + 真实输入法；手动运行 --ignored"]
fn passport_ime_observation_20x() {
    std::env::set_var("SAYALL_PASSPORT_IME_TRACE", "1");
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
    // 微信输入法靠"按住说话快捷键"进入听写（本机配置为 Ctrl+Win）。
    // 硬编码右 Shift 会让每次观测都落在 not_observed —— 这是采集脚本的配置问题。
    let (chord, chord_source) = configured_hotkey();
    eprintln!("[obs] 按住说话快捷键={:?}（{chord_source}）", chord.keys);
    platform.set_voice_hold_hotkey(Some(chord));
    // 观测必须有"当前输入工具"才能拿到 marker；默认按本机配置（微信输入法）。
    let tool = match std::env::var("SAYALL_IME_TOOL")
        .unwrap_or_else(|_| "wechat".to_owned())
        .as_str()
    {
        "wechat" => Some(VoiceInputTool::Wechat),
        "doubao" => Some(VoiceInputTool::Doubao),
        "vokie" => Some(VoiceInputTool::Vokie),
        "other" => Some(VoiceInputTool::Other),
        _ => None,
    };
    platform.set_voice_input_tool(tool);
    eprintln!("[obs] 输入法工具={tool:?}");
    platform
        .switch_device_mode(DeviceMode::Passport)
        .expect("切换到 Passport 模式失败");

    let deadline = Instant::now() + Duration::from_secs(90);
    while platform.passport_snapshot().device.is_none() {
        assert!(Instant::now() < deadline, "90 秒内未发现设备");
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("[obs] 设备已发现，开始 {NORMAL_RUNS}+{SWITCHED_RUNS} 次观测");

    let total = NORMAL_RUNS + SWITCHED_RUNS;
    for run in 1..=total {
        let phase = if run <= NORMAL_RUNS {
            "normal"
        } else {
            "switched"
        };
        if run == NORMAL_RUNS + 1 {
            eprintln!(
                "[obs] === 后 {SWITCHED_RUNS} 次：请每次按之前，先把输入法切到别的工具（例如英文/另一个输入法） ==="
            );
            std::thread::sleep(Duration::from_secs(8));
        }
        eprintln!("[RUN {run}/{total} {phase}] 请按住语音键说 1 秒后松开……");

        let before = platform.passport_snapshot();
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let now = platform.passport_snapshot();
            if now.sessions > before.sessions && now.blocks > before.blocks {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "[RUN {run}] 120 秒内没有完成一次会话"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
        // 等 1500ms 的第二次观测落地再进入下一次，避免日志交错。
        std::thread::sleep(Duration::from_millis(2200));
        eprintln!("[RUN {run}/{total}] 已记录");
    }

    platform
        .switch_device_mode(DeviceMode::Ble)
        .expect("切回 BLE 失败");
    eprintln!("[obs] 20 次采集结束");
}
