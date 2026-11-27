// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

#[test]
fn prints_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_coqo"))
        .arg("--version")
        .output()
        .expect("binary runs");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
    assert_eq!(stdout.trim(), format!("coqo {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn rejects_a_missing_folder() {
    let output = Command::new(env!("CARGO_BIN_EXE_coqo"))
        .arg("/nonexistent/coqo-test-folder")
        .output()
        .expect("binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf-8 output");
    assert!(stderr.contains("cannot open"), "{stderr}");
}

#[test]
fn rejects_a_file_instead_of_a_folder() {
    let output = Command::new(env!("CARGO_BIN_EXE_coqo"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .output()
        .expect("binary runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf-8 output");
    assert!(stderr.contains("is not a folder"), "{stderr}");
}
