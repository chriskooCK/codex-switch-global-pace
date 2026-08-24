use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const READY_MARKER: &str = "codex-switch-global-pace update lock ready";

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_codex-switch-global-pace")
}

fn temp_home(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "codex-switch-global-pace-{name}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test home");
    path
}

fn holder_command(home: &Path, target: &Path) -> Command {
    let mut command = Command::new(binary());
    command.arg("__hold-update-lock");
    command.env("CS_UPDATE_LOCK_TARGET", target);
    command.env("CODEX_SWITCH_HOME", home.join("blocked-config"));
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command
}

fn read_marker(stdout: ChildStdout) -> std::io::Result<String> {
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn wait_success(mut child: Child, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().expect("poll lock holder") {
            Some(status) => {
                assert!(status.success(), "{description} exited with {status}");
                return;
            }
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{description} did not exit after stdin EOF");
            }
        }
    }
}

#[test]
fn internal_update_lock_command_is_absent_from_help() {
    let output = Command::new(binary())
        .arg("--help")
        .output()
        .expect("run help");
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("__hold-update-lock"));
}

#[test]
fn two_lock_holders_serialize_on_the_normalized_destination() {
    let home = temp_home("update-lock-holder");
    let install_dir = home.join("install");
    fs::create_dir(&install_dir).expect("create install directory");
    fs::write(home.join("blocked-config"), b"not a directory")
        .expect("create configuration blocker");

    let destination = install_dir.join("codex-switch-global-pace.exe");
    let equivalent_destination = install_dir
        .join("..")
        .join("install")
        .join("codex-switch-global-pace.exe");

    let mut first = holder_command(&home, &equivalent_destination)
        .spawn()
        .expect("start first lock holder");
    let first_marker =
        read_marker(first.stdout.take().expect("first stdout")).expect("read first ready marker");
    assert_eq!(first_marker, READY_MARKER);

    let mut second = holder_command(&home, &destination)
        .spawn()
        .expect("start second lock holder");
    let second_stdout = second.stdout.take().expect("second stdout");
    let (marker_tx, marker_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let _ = marker_tx.send(read_marker(second_stdout));
    });

    assert!(
        marker_rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "second holder acquired the lease before the first released it"
    );

    drop(first.stdin.take());
    wait_success(first, "first lock holder");

    let second_marker = marker_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second holder did not acquire the released lease")
        .expect("read second ready marker");
    assert_eq!(second_marker, READY_MARKER);
    reader.join().expect("join second marker reader");

    drop(second.stdin.take());
    wait_success(second, "second lock holder");
    fs::remove_dir_all(home).expect("remove test home");
}
