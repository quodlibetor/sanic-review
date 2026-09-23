//! End-to-end checks against the built binary.

use std::process::Command;

fn sanic_review() -> Command {
    Command::new(env!("CARGO_BIN_EXE_sanic-review"))
}

#[test]
fn version_prints_package_version() {
    let output = sanic_review().arg("--version").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
}

#[test]
fn unknown_ui_is_rejected() {
    let output = sanic_review()
        .args(["serve", "--ui", "bogus"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
