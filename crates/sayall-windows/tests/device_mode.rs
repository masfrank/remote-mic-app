#![cfg(windows)]
//! P2 设备模式协调层测试：互斥、失败反馈、幂等与资源释放。
//!
//! 生产端口固定为 33333，因此所有用例串行执行（同一文件内共享一把锁），
//! 且不能与正在运行的 Passport 程序或应用实例并发执行。

use std::net::UdpSocket;
use std::sync::{Mutex, MutexGuard};

use sayall_core::passport::PassportPhase;
use sayall_core::DeviceMode;
use sayall_windows::WindowsPlatform;

/// 同一测试进程内的用例并行跑会互相抢 33333：文件级串行锁。
fn port_guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn platform() -> WindowsPlatform {
    WindowsPlatform::default()
}

#[test]
fn defaults_to_ble_without_starting_passport() {
    let _guard = port_guard();
    let platform = platform();
    assert_eq!(platform.device_mode(), DeviceMode::Ble);
    let snapshot = platform.passport_snapshot();
    assert_eq!(snapshot.phase, PassportPhase::Stopped);
    assert_eq!(snapshot.port, 0, "BLE 模式不应绑定 Passport 端口");
    assert_eq!(platform.snapshot().device_mode, DeviceMode::Ble);
}

#[test]
fn port_conflict_keeps_ble_mode_and_reports_error() {
    let _guard = port_guard();
    // 占用生产端口，模拟"旧 Python 程序还开着"。
    let blocker = UdpSocket::bind(("0.0.0.0", sayall_core::passport::UDP_PORT))
        .expect("33333 被别的程序占用，先关闭再跑该测试");

    let platform = platform();
    let error = platform
        .switch_device_mode(DeviceMode::Passport)
        .expect_err("端口被占用时切换必须失败");
    assert!(
        error.to_string().contains("33333"),
        "错误信息应点名端口：{error}"
    );

    // 不静默回退成"已切到 Passport"。
    assert_eq!(platform.device_mode(), DeviceMode::Ble);
    assert_eq!(platform.snapshot().passport.phase, PassportPhase::Error);
    assert!(platform.device_mode_error().is_some());
    assert_eq!(platform.snapshot().device_mode, DeviceMode::Ble);

    drop(blocker);
    platform
        .switch_device_mode(DeviceMode::Passport)
        .expect("端口释放后应能切换成功");
    assert_eq!(platform.device_mode(), DeviceMode::Passport);
    // 没有设备时停在 Listening，不能伪装成已连接。
    assert_eq!(platform.snapshot().passport.phase, PassportPhase::Listening);
    assert!(platform.device_mode_error().is_none());

    platform
        .switch_device_mode(DeviceMode::Ble)
        .expect("切回 BLE 必须成功");
    assert_eq!(platform.passport_snapshot().phase, PassportPhase::Stopped);
}

#[test]
fn repeated_switches_are_idempotent_and_release_the_port() {
    let _guard = port_guard();
    let platform = platform();

    platform
        .switch_device_mode(DeviceMode::Passport)
        .expect("首次切换失败（33333 可能被占用）");
    let first = platform.passport_snapshot().port;
    assert_ne!(first, 0);

    // 同模式重复切换：必须幂等，不能重复绑定端口或起第二组线程。
    platform
        .switch_device_mode(DeviceMode::Passport)
        .expect("重复切换必须成功");
    assert_eq!(platform.passport_snapshot().port, first);

    platform
        .switch_device_mode(DeviceMode::Ble)
        .expect("切回 BLE 失败");
    platform
        .switch_device_mode(DeviceMode::Ble)
        .expect("重复切回 BLE 必须成功");
    assert_eq!(platform.passport_snapshot().phase, PassportPhase::Stopped);

    // 停掉之后端口必须可再次绑定（线程与 socket 都已释放）。
    let rebound = UdpSocket::bind(("0.0.0.0", first));
    assert!(rebound.is_ok(), "Passport 停止后端口未释放：{rebound:?}");
}

#[test]
fn passport_mode_rejects_ble_connections() {
    let _guard = port_guard();
    let platform = platform();
    platform
        .switch_device_mode(DeviceMode::Passport)
        .expect("切换到 Passport 失败（33333 可能被占用）");

    let error = platform
        .connect_remote("unused-device-id".to_owned())
        .expect_err("Passport 模式下不允许连接 BLE 遥控器");
    assert!(error.to_string().contains("Passport"), "{error}");

    platform
        .switch_device_mode(DeviceMode::Ble)
        .expect("切回 BLE 失败");
}
