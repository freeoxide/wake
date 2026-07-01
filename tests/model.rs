//! Unit tests for the locked domain model (`oxiwake::model`).
//! These exercise the pure helpers that backends and the CLI rely on, and do
//! not touch any platform code.

use oxiwake::model::{DoctorReport, WakeMode, WakeRequest, WakeTarget};

#[test]
fn logind_what_default_is_sleep_idle() {
    let req = WakeRequest::default_linux();
    assert_eq!(req.logind_what(), "sleep:idle");
}

#[test]
fn logind_what_aggressive_lid_appends_handle_lid_switch() {
    let mut req = WakeRequest::default_linux();
    req.aggressive_lid = true;
    assert_eq!(req.logind_what(), "sleep:idle:handle-lid-switch");
}

#[test]
fn logind_what_preserves_priority_order_and_dedups() {
    let req = WakeRequest {
        targets: vec![
            WakeTarget::Shutdown,
            WakeTarget::SystemSleep,
            WakeTarget::Idle,
        ],
        reason: "test".into(),
        display: false,
        aggressive_lid: false,
    };
    assert_eq!(req.logind_what(), "shutdown:sleep:idle");
}

#[test]
fn logind_what_dedups_repeated_handle_lid_switch() {
    let req = WakeRequest {
        targets: vec![WakeTarget::LidSwitch],
        reason: "test".into(),
        display: false,
        aggressive_lid: true, // would re-add if not deduped
    };
    assert_eq!(req.logind_what(), "handle-lid-switch");
}

#[test]
fn logind_what_display_target_contributes_no_token() {
    let req = WakeRequest {
        targets: vec![WakeTarget::Display, WakeTarget::Idle],
        reason: "test".into(),
        display: true,
        aggressive_lid: false,
    };
    assert_eq!(req.logind_what(), "idle");
}

#[test]
fn wake_mode_as_str_matches_systemd_inhibit() {
    assert_eq!(WakeMode::Block.as_str(), "block");
    assert_eq!(WakeMode::Delay.as_str(), "delay");
    assert_eq!(WakeMode::BlockWeak.as_str(), "block-weak");
}

#[test]
fn doctor_report_not_compiled_shape() {
    let r = DoctorReport::not_compiled("wayland-idle-inhibit");
    assert_eq!(r.backend, "wayland-idle-inhibit");
    assert!(!r.supported);
    assert!(!r.available);
    assert!(r.guarantees.iter().any(|g| g.contains("not compiled")));
}

#[test]
fn request_round_trips_through_serde_json() {
    let req = WakeRequest::default_linux();
    let json = serde_json::to_string(&req).unwrap();
    let back: WakeRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(back.targets, req.targets);
    assert_eq!(back.reason, req.reason);
}
