//! A nonexistent inherited fd must be rejected before startup opens any fd
//! that could reuse its number. In particular no database/state is created.
#[cfg(unix)]
#[test]
fn invalid_inherited_descriptor_is_rejected_before_runtime_and_state_creation() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("not-created");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_t3-server"))
        .args(["serve", "--state-dir"])
        .arg(&state)
        .args([
            "--mode",
            "desktop",
            "--desktop-telemetry-fd",
            "99999999",
            "--port",
            "0",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Bad file descriptor"));
    assert!(
        !state.exists(),
        "startup opened state before validating inherited ownership"
    );
}
