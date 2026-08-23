#![cfg(unix)]

mod mock;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use mock::scenarios;
use serde_json::{Value, json};

struct TestEnv {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    codex_home: PathBuf,
    usage_url: String,
    token_url: String,
}

impl TestEnv {
    fn app_home(&self) -> PathBuf {
        self.home.join(".codex-switch")
    }

    fn current_file(&self) -> PathBuf {
        self.app_home().join("current")
    }

    fn live_auth_path(&self) -> PathBuf {
        self.codex_home.join("auth.json")
    }

    fn pidfile(&self) -> PathBuf {
        self.app_home().join("daemon.pid")
    }

    fn cache_file(&self) -> PathBuf {
        self.app_home().join("cache.json")
    }
}

fn make_id_token(email: &str, plan_type: &str, account_id: &str) -> String {
    let claims = json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": plan_type,
            "chatgpt_account_id": account_id,
            "chatgpt_user_id": format!("user_{account_id}"),
            "organizations": [],
        }
    });
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    format!("header.{payload}.signature")
}

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
}

fn setup_env(
    entries: &[(String, Vec<Value>)],
    current_alias: &str,
    usage_url: String,
    token_url: String,
) -> TestEnv {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let codex_home = tmp.path().join("codex-home");
    let profiles_dir = home.join(".codex-switch").join("profiles");

    std::fs::create_dir_all(&profiles_dir).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();

    for (token, responses) in entries {
        let alias = token.strip_prefix("tok_").unwrap_or(token);
        let plan_type = responses[0]["plan_type"].as_str().unwrap_or("plus");
        let auth_json = json!({
            "tokens": {
                "access_token": token,
                "refresh_token": format!("refresh_{token}"),
                "id_token": make_id_token(
                    &format!("{alias}@mock.test"),
                    plan_type,
                    &format!("acct_{alias}")
                ),
                "account_id": format!("acct_{alias}"),
            }
        });
        write_json(&profiles_dir.join(alias).join("auth.json"), &auth_json);

        if alias == current_alias {
            write_json(&codex_home.join("auth.json"), &auth_json);
        }
    }

    std::fs::write(home.join(".codex-switch").join("current"), current_alias).unwrap();
    std::fs::write(
        home.join(".codex-switch").join("config.toml"),
        r#"[use]
safety_margin_7d = 20
team_priority = true

[daemon]
poll_interval_secs = 1
switch_threshold = 50
cache_refresh_interval_secs = 1
auto_warmup = false
token_check_interval_secs = 60
notify = false
# info so the startup line reaches the rotating log file (asserted below).
log_level = "info"
# The host running this test may have real Codex sessions open; switching
# must not be deferred by them.
defer_switch_while_codex_running = false
"#,
    )
    .unwrap();

    TestEnv {
        _tmp: tmp,
        home,
        codex_home,
        usage_url,
        token_url,
    }
}

fn run_cmd(env: &TestEnv, args: &[&str]) -> Output {
    let bin = std::env::var("CARGO_BIN_EXE_codex-switch-global-pace").unwrap();
    Command::new(bin)
        .args(args)
        .env("HOME", &env.home)
        .env("CODEX_HOME", &env.codex_home)
        .env("CS_USAGE_URL", &env.usage_url)
        .env("CS_TOKEN_URL", &env.token_url)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

fn wait_until<F>(timeout: Duration, label: &str, mut check: F)
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("condition '{}' not met within {:?}", label, timeout);
}

fn read_live_access_token(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_start_switch_status_and_stop() {
    let entries = scenarios::gradual_exhaustion();
    let server = mock::MockServer::start(entries.clone()).await;
    let env = setup_env(
        &entries,
        "gradual_a",
        server.usage_url(),
        server.token_url(),
    );

    std::fs::write(env.pidfile(), "99999999").unwrap();
    let stale_status = run_cmd(&env, &["--json", "daemon", "status"]);
    assert!(
        stale_status.status.success(),
        "stale status stderr: {}",
        String::from_utf8_lossy(&stale_status.stderr)
    );
    let stale_json = stdout_json(&stale_status);
    assert_eq!(stale_json["state"], "stale");
    assert_eq!(stale_json["running"], false);
    assert_eq!(stale_json["pid"], 99999999);
    assert_eq!(stale_json["stale_pid_cleaned"], true);
    assert_eq!(stale_json["config"]["poll_interval_secs"], 1);
    assert!(
        !env.pidfile().exists(),
        "status --json should clean stale pidfile"
    );

    let status_before = run_cmd(&env, &["daemon", "status"]);
    assert!(status_before.status.success());
    assert_eq!(stdout(&status_before), "Daemon is not running");

    let start = run_cmd(&env, &["daemon", "start"]);
    assert!(
        start.status.success(),
        "start stderr: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(stdout(&start).starts_with("Daemon started (PID "));

    wait_until(Duration::from_secs(10), "pidfile created", || {
        env.pidfile().exists()
    });

    wait_until(Duration::from_secs(10), "daemon status=running", || {
        let out = run_cmd(&env, &["daemon", "status"]);
        out.status.success() && stdout(&out).starts_with("Daemon is running (PID ")
    });

    let json_status = run_cmd(&env, &["--json", "daemon", "status"]);
    assert!(
        json_status.status.success(),
        "json status stderr: {}",
        String::from_utf8_lossy(&json_status.stderr)
    );
    let status_json = stdout_json(&json_status);
    assert_eq!(status_json["state"], "running");
    assert_eq!(status_json["running"], true);
    assert!(status_json["pid"].as_u64().is_some());
    assert!(
        status_json["pidfile"]
            .as_str()
            .is_some_and(|path| path.ends_with("daemon.pid"))
    );
    assert_eq!(status_json["platform"]["daemon_start_supported"], true);
    let os = status_json["platform"]["os"].as_str().unwrap_or("");
    assert_eq!(
        status_json["platform"]["service_install_supported"],
        os == "macos" || os == "linux"
    );
    assert!(
        status_json["platform"]["service_manager"]
            .as_str()
            .is_some()
    );
    assert_eq!(status_json["platform"]["service_installed"], false);
    assert_eq!(status_json["config"]["cache_refresh_interval_secs"], 1);
    assert_eq!(status_json["config"]["auto_warmup"], false);
    assert_eq!(status_json["config"]["switch_threshold"], 50.0);

    wait_until(
        Duration::from_secs(10),
        "daemon refreshed all profile usage into cache",
        || {
            let Ok(raw) = std::fs::read_to_string(env.cache_file()) else {
                return false;
            };
            let Ok(cache) = serde_json::from_str::<Value>(&raw) else {
                return false;
            };
            cache["entries"].get("gradual_a").is_some()
                && cache["entries"].get("gradual_b").is_some()
        },
    );

    wait_until(
        Duration::from_secs(15),
        "daemon switches to gradual_b",
        || {
            std::fs::read_to_string(env.current_file())
                .map(|s| s.trim() == "gradual_b")
                .unwrap_or(false)
                && read_live_access_token(&env.live_auth_path()).as_deref() == Some("tok_gradual_b")
        },
    );

    // The daemon runs with stdio discarded, so its loop must log to the
    // capped daily file under app_home/logs.
    wait_until(Duration::from_secs(10), "daemon writes a log file", || {
        std::fs::read_dir(env.app_home().join("logs"))
            .map(|entries| {
                entries.flatten().any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("codex-switch-global-pace.")
                        && e.metadata().is_ok_and(|m| m.len() > 0)
                })
            })
            .unwrap_or(false)
    });

    // The loop's state snapshot should record the switch and be exposed by status --json.
    wait_until(
        Duration::from_secs(10),
        "status snapshot records the switch",
        || {
            let out = run_cmd(&env, &["--json", "daemon", "status"]);
            if !out.status.success() {
                return false;
            }
            let json = stdout_json(&out);
            json["snapshot"]["last_switch"]["to"] == "gradual_b"
                && json["snapshot"]["pid"].as_u64().is_some()
                && json["snapshot"]["last_poll_at"].as_i64().is_some()
        },
    );
    assert!(
        env.app_home().join("daemon-state.json").exists(),
        "daemon should write its state snapshot"
    );

    let stop = run_cmd(&env, &["daemon", "stop"]);
    assert!(
        stop.status.success(),
        "stop stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(stdout(&stop).starts_with("Sent stop signal to daemon (PID "));

    wait_until(
        Duration::from_secs(15),
        "daemon stopped and status=not running",
        || {
            let out = run_cmd(&env, &["daemon", "status"]);
            out.status.success() && stdout(&out) == "Daemon is not running"
        },
    );

    assert!(
        !env.pidfile().exists(),
        "pidfile should be removed after stop"
    );
    assert_eq!(
        std::fs::read_to_string(env.current_file()).unwrap().trim(),
        "gradual_b"
    );
    assert_eq!(
        read_live_access_token(&env.live_auth_path()).as_deref(),
        Some("tok_gradual_b")
    );

    server.shutdown();
}
