// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

#[test]
fn prints_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_coqod"))
        .arg("--version")
        .output()
        .expect("binary runs");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
    assert_eq!(
        stdout.trim(),
        format!("coqod {}", env!("CARGO_PKG_VERSION"))
    );
}
