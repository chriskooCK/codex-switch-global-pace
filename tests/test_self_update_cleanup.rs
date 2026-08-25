#![cfg(windows)]

mod support;

use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;

fn copied_application() -> (support::TempDir, PathBuf, PathBuf) {
    let directory = support::tempdir();
    std::fs::create_dir(directory.path().join("state")).expect("create application state");
    std::fs::create_dir(directory.path().join("codex")).expect("create Codex state");
    let executable = directory.path().join("cleanup-startup-fixture.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_codex-switch-global-pace"), &executable)
        .expect("copy application executable");
    let executable = std::fs::canonicalize(executable).expect("resolve copied application");
    let journal = executable
        .parent()
        .expect("copied executable parent")
        .join(".cleanup-startup-fixture.exe.self-update-cleanup-journal");
    (directory, executable, journal)
}

fn run(executable: &Path, directory: &Path, arguments: &[&str]) -> Output {
    std::process::Command::new(executable)
        .args(arguments)
        .env("CODEX_SWITCH_HOME", directory.join("state"))
        .env("CODEX_HOME", directory.join("codex"))
        .env("CS_COLOR", "never")
        .env("RUST_LOG", "off")
        .output()
        .expect("run copied application")
}

fn assert_normal_json_command_continues_with_warning(output: &Output, expected: &str) {
    assert!(
        output.status.success(),
        "normal command was bricked: status={}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("normal command stdout remains JSON");
    assert!(
        stdout.is_object(),
        "normal command did not return its JSON payload: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("previous executable cleanup remains pending"));
    assert!(stderr.contains(expected), "{stderr}");
}

fn assert_self_update_fails_as_json_before_publication(output: &Output, expected: &str) {
    assert!(
        !output.status.success(),
        "self-update ignored its pending cleanup authority"
    );
    let stdout: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("self-update failure stdout remains JSON");
    assert_eq!(stdout["ok"], false);
    let error = stdout["error"].as_str().expect("typed JSON error");
    assert!(error.contains("previous Windows self-update cleanup"));
    assert!(error.contains(expected), "{error}");
}

#[test]
fn corrupt_or_locked_cleanup_journal_warns_normally_and_blocks_only_publication() {
    let (directory, executable, journal) = copied_application();
    std::fs::write(&journal, b"{not valid json").expect("write corrupt cleanup journal");

    let normal = run(
        &executable,
        directory.path(),
        &["--json", "daemon", "status"],
    );
    assert_normal_json_command_continues_with_warning(&normal, "parsing cleanup journal");

    let update = run(&executable, directory.path(), &["--json", "self-update"]);
    assert_self_update_fails_as_json_before_publication(&update, "parsing cleanup journal");

    let pin = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&journal)
        .expect("pin cleanup journal against every competing open");
    let normal = run(
        &executable,
        directory.path(),
        &["--json", "daemon", "status"],
    );
    assert_normal_json_command_continues_with_warning(&normal, "opening cleanup journal");

    let update = run(&executable, directory.path(), &["--json", "self-update"]);
    assert_self_update_fails_as_json_before_publication(&update, "opening cleanup journal");
    drop(pin);
}
