//! The background machinery, end to end, through the real binary.
//!
//! Every test here drives `srun-portal` as a child process against a stub
//! portal: the same argv the installed task uses, the same control directory, a
//! private `SRUN_PORTAL_STATE_DIR`, a scratch `SRUN_PORTAL_CONFIG`, and a
//! password in the environment or in that state directory. Nothing touches the
//! developer's own installation, credentials, or terminal.
//!
//! No native task is registered: the platform backends are exercised by
//! `service_native.rs` and by the plan tests in the library.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{Behaviour, Stub, TempDir};

const BIN: &str = env!("CARGO_BIN_EXE_srun-portal");

/// One isolated run: its own state directory, configuration, and stub.
struct Harness {
    dir: TempDir,
    stub: Stub,
    config: PathBuf,
}

impl Harness {
    fn new(tag: &str, behaviour: Behaviour) -> Harness {
        let dir = TempDir::new(tag);
        let stub = Stub::start(behaviour);
        let config = dir.join("srun-portal.toml");
        std::fs::write(
            &config,
            format!(
                "portal_url = \"{}\"\nusername = \"testuser\"\n",
                stub.slug()
            ),
        )
        .expect("write configuration");
        Harness { dir, stub, config }
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .env("SRUN_PORTAL_STATE_DIR", self.state())
            .env("SRUN_PORTAL_CONFIG", &self.config)
            .env_remove("SRUN_PORTAL_PASSWORD");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("the binary runs")
    }

    fn run_with_password(&self, args: &[&str], password: &str) -> Output {
        self.command()
            .args(args)
            .env("SRUN_PORTAL_PASSWORD", password)
            .output()
            .expect("the binary runs")
    }

    /// Write the record an install would leave, so the hidden entry point and
    /// `service start` have something to act on without registering a real task.
    fn record_installation(&self) {
        std::fs::create_dir_all(self.state()).expect("state directory");
        // Serialise through the same type the program writes, so a Windows path
        // gets its backslashes escaped instead of producing invalid TOML.
        let registration = srun_portal::control::Registration {
            version: 1,
            exe: PathBuf::from(BIN),
            source: srun_portal::settings::ConfigSource {
                layers: vec![self.config.clone()],
                write_path: self.config.clone(),
                legacy_path: None,
            },
            interval_secs: Some(300),
            effective_interval_secs: Some(300),
            boot_delay_secs: Some(30),
            owner_id: "1000".to_string(),
        };
        let text = toml::to_string_pretty(&registration).expect("registration serialises");
        std::fs::write(self.state().join("installed.toml"), text).expect("write installed.toml");
    }

    fn health(&self) -> Value {
        let path = self.state().join("health.json");
        let text = std::fs::read_to_string(&path).expect("health was recorded");
        serde_json::from_str(&text).expect("health is JSON")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// The worker request the supervisor writes, built the same way.
fn worker_request(operation: &str, config: &Path, state: &Path) -> String {
    serde_json::json!({
        "operation": operation,
        "source": {
            "layers": [config],
            "write_path": config,
        },
        "control_dir": state,
    })
    .to_string()
}

#[test]
fn a_layered_install_reads_every_layer_and_an_explicit_config_reads_one() {
    // A system layer with the identity, a portable layer with only a timeout.
    let dir = TempDir::new("layers");
    let stub = Stub::start(Behaviour::offline());
    let system = dir.join("system.toml");
    let portable = dir.join("portable.toml");
    std::fs::write(
        &system,
        format!(
            "portal_url = \"{}\"\nusername = \"testuser\"\n",
            stub.slug()
        ),
    )
    .unwrap();
    std::fs::write(&portable, "timeout_ms = 15000\n").unwrap();

    let state = dir.join("state");
    let source = serde_json::json!({
        "layers": [system, portable],
        "write_path": portable,
    });
    let request =
        serde_json::json!({"operation": "reconnect", "source": source, "control_dir": state})
            .to_string();
    let output = Command::new(BIN)
        .arg("__worker")
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env_remove("SRUN_PORTAL_CONFIG")
        .env("SRUN_PORTAL_PASSWORD", "testpass")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(request.as_bytes())?;
            child.wait_with_output()
        })
        .expect("worker runs");

    assert!(output.status.success(), "{}", stderr(&output));
    let login = stub
        .last_query_for("/cgi-bin/srun_portal")
        .expect("the worker logged in");
    assert!(
        login.contains("username=testuser"),
        "the lower layer supplied the account: {login}"
    );

    // An explicit configuration ignores the layered pair entirely.
    let explicit = dir.join("explicit.toml");
    std::fs::write(
        &explicit,
        format!(
            "portal_url = \"{}\"\nusername = \"testuser\"\n",
            stub.slug()
        ),
    )
    .unwrap();
    let before = stub.request_count();
    let output = Command::new(BIN)
        .args(["reconnect", "--json"])
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env("SRUN_PORTAL_CONFIG", &explicit)
        .env("SRUN_PORTAL_PASSWORD", "testpass")
        .output()
        .expect("reconnect runs");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stub.request_count() > before,
        "the explicit configuration authenticated"
    );
}

#[test]
fn a_missing_configuration_is_reported_and_nothing_is_created() {
    let dir = TempDir::new("missing");
    let state = dir.join("state");
    let missing = dir.join("missing.toml");

    let show = Command::new(BIN)
        .args(["config", "show", "--json"])
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env("SRUN_PORTAL_CONFIG", &missing)
        .output()
        .expect("show runs");
    assert!(show.status.success(), "{}", stderr(&show));
    let body: Value = serde_json::from_str(stdout(&show).trim()).expect("JSON output");
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["values"]["portal_url"], Value::Null);
    assert_eq!(body["values"]["timeout_ms"], 10_000);
    assert!(!missing.exists(), "show never creates the configuration");
    assert!(!state.exists(), "show never creates the state directory");

    let list = Command::new(BIN)
        .args(["config", "list"])
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env("SRUN_PORTAL_CONFIG", &missing)
        .output()
        .expect("list runs");
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(!missing.exists());

    let status = Command::new(BIN)
        .args(["service", "status", "--json"])
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env("SRUN_PORTAL_CONFIG", &missing)
        .output()
        .expect("status runs");
    assert!(status.status.success(), "{}", stderr(&status));
    let body: Value = serde_json::from_str(stdout(&status).trim()).expect("JSON output");
    assert_eq!(body["installed"], false);
    assert_eq!(body["last_attempt"], Value::Null);
    assert_eq!(body["last_success_at_ms"], Value::Null);
    assert!(!missing.exists());
    assert!(!state.join("control.toml").exists());
}

#[test]
fn an_illegal_batch_is_rejected_and_the_file_is_unchanged() {
    let dir = TempDir::new("batch");
    let config = dir.join("srun-portal.toml");
    let before = "username = \"kept\"\ntimeout_ms = 12000\n";
    std::fs::write(&config, before).unwrap();
    let state = dir.join("state");

    for args in [
        vec!["config", "set", "portal_url", "not-a-url"],
        vec!["config", "set", "timeout_ms", "0"],
        vec!["config", "set", "timeout_ms", "90000"],
        vec!["config", "set", "reconnect_interval_secs", "90000"],
        vec!["config", "set", "reconnect_boot_delay_secs", "90000"],
        vec!["config", "set", "username", "   "],
        // A valid first pair must not land when the second one is illegal.
        vec!["config", "set", "username", "alice", "timeout_ms", "0"],
    ] {
        let output = Command::new(BIN)
            .args(&args)
            .env("SRUN_PORTAL_STATE_DIR", &state)
            .env("SRUN_PORTAL_CONFIG", &config)
            .output()
            .expect("config set runs");
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            before,
            "{args:?} must not write"
        );
    }

    // The legal ones do land, and unknown keys survive.
    std::fs::write(&config, format!("{before}future_key = 1\n")).unwrap();
    let output = Command::new(BIN)
        .args(["config", "set", "reconnect_interval_secs", "600"])
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env("SRUN_PORTAL_CONFIG", &config)
        .output()
        .expect("config set runs");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Schedule changed; run service install to apply."));
    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("future_key = 1"), "{text}");
    assert!(text.contains("reconnect_interval_secs = 600"), "{text}");
}

#[test]
fn concurrent_writers_to_different_keys_both_land() {
    let dir = TempDir::new("concurrent");
    let config = dir.join("srun-portal.toml");
    std::fs::write(&config, "username = \"alice\"\n").unwrap();
    let state = dir.join("state");

    let mut children = Vec::new();
    for args in [
        vec!["config", "set", "timeout_ms", "20000"],
        vec!["config", "set", "reconnect_interval_secs", "600"],
    ] {
        children.push(
            Command::new(BIN)
                .args(&args)
                .env("SRUN_PORTAL_STATE_DIR", &state)
                .env("SRUN_PORTAL_CONFIG", &config)
                .spawn()
                .expect("spawn"),
        );
    }
    for mut child in children {
        assert!(child.wait().expect("wait").success());
    }

    let text = std::fs::read_to_string(&config).unwrap();
    assert!(text.contains("username = \"alice\""), "{text}");
    assert!(text.contains("timeout_ms = 20000"), "{text}");
    assert!(text.contains("reconnect_interval_secs = 600"), "{text}");
}

#[test]
fn a_failed_write_leaves_the_previous_file_intact() {
    let dir = TempDir::new("write-failure");
    let config = dir.join("srun-portal.toml");
    let before = "portal_url = \"https://kept.example/srun_portal_pc?ac_id=1\"\n";
    std::fs::write(&config, before).unwrap();
    let state = dir.join("state");

    // A read-only directory: the atomic replace needs to create a temporary file
    // beside the target, so the write fails while the file itself stays readable.
    let readonly = dir.join("readonly");
    std::fs::create_dir_all(&readonly).unwrap();
    let target = readonly.join("srun-portal.toml");
    std::fs::write(&target, before).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();
        let output = Command::new(BIN)
            .args(["config", "set", "username", "alice"])
            .env("SRUN_PORTAL_STATE_DIR", &state)
            .env("SRUN_PORTAL_CONFIG", &target)
            .output()
            .expect("config set runs");
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), before);
        }
    }
    let _ = config;
}

#[test]
fn a_reconnect_logs_in_once_and_records_health() {
    let harness = Harness::new("reconnect", Behaviour::offline());

    let output = harness.run_with_password(&["reconnect", "--json"], "testpass");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON result");
    assert_eq!(body["result"], "reconnected");
    assert_eq!(body["error_kind"], Value::Null);

    assert_eq!(
        harness.stub.count_for("/cgi-bin/srun_portal"),
        1,
        "exactly one login"
    );
    let health = harness.health();
    assert_eq!(health["schema_version"], 1);
    assert_eq!(health["result"], "reconnected");
    assert!(health["last_success_at_ms"].as_u64().is_some());
    assert!(health["finished_at_ms"].as_u64().is_some());

    // The recorded health carries no secret of any kind.
    let text = std::fs::read_to_string(harness.state().join("health.json")).unwrap();
    assert!(!text.contains("testpass"), "{text}");
    assert!(!text.contains("%7BMD5%7D"), "{text}");
}

#[test]
fn an_online_device_is_not_authenticated_again() {
    let harness = Harness::new("already-online", Behaviour::already_online());

    let output = harness.run_with_password(&["reconnect", "--json"], "testpass");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON result");
    assert_eq!(body["result"], "online");
    assert_eq!(
        harness.stub.count_for("/cgi-bin/srun_portal"),
        0,
        "an online device sends no login"
    );
    // Two online probes: the fire-and-forget other-stack one and the check.
    assert!(harness.stub.count_for("/cgi-bin/rad_user_info") >= 1);
}

#[test]
fn a_paused_schedule_sends_nothing_and_keeps_the_last_verdict() {
    let harness = Harness::new("paused", Behaviour::offline());
    let output = harness.run_with_password(&["reconnect"], "testpass");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let requests = harness.stub.request_count();

    harness.record_installation();
    let output = harness.run_with_password(&["disconnect", "--json"], "testpass");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let after_disconnect = harness.stub.request_count();
    assert!(
        after_disconnect > requests,
        "the sign-out did reach the portal"
    );
    // A sign-out is itself a network conclusion, so it is the new last success.
    let success = harness.health()["last_success_at_ms"].clone();

    for _ in 0..2 {
        let output = harness.run_with_password(
            &[
                "__background",
                "--control-dir",
                harness.state().to_str().unwrap(),
                "--trigger",
                "interval",
            ],
            "testpass",
        );
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        assert!(stdout(&output).contains("paused"), "{}", stdout(&output));
    }
    assert_eq!(
        harness.stub.request_count(),
        after_disconnect,
        "a paused background run sends nothing"
    );
    let health = harness.health();
    assert_eq!(
        health["last_success_at_ms"], success,
        "a paused run does not overwrite the last success"
    );
    assert_ne!(health["result"], "failed");
}

#[test]
fn resume_clears_the_pause_and_checks_immediately() {
    let harness = Harness::new("resume", Behaviour::offline());
    harness.record_installation();
    let output = harness.run_with_password(&["disconnect"], "testpass");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));

    // A fresh process sees the pause.
    let paused = harness.run_with_password(&["reconnect", "--json"], "testpass");
    let body: Value = serde_json::from_str(stdout(&paused).trim()).expect("JSON result");
    assert_eq!(body["result"], "paused");
    let requests = harness.stub.request_count();

    let output = harness.run_with_password(&["resume", "--json"], "testpass");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON result");
    assert_eq!(body["result"], "reconnected");
    assert!(harness.stub.request_count() > requests, "resume checked");

    // And a later background run is no longer paused.
    let output = harness.run_with_password(
        &[
            "__background",
            "--control-dir",
            harness.state().to_str().unwrap(),
            "--trigger",
            "interval",
        ],
        "testpass",
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(!stdout(&output).contains("paused"), "{}", stdout(&output));
}

#[test]
fn a_failed_sign_out_still_leaves_the_pause_in_place() {
    let dir = TempDir::new("failed-signout");
    // A portal that is online but whose DM endpoint answers an error.
    let stub = Stub::start(Behaviour::already_online());
    let config = dir.join("srun-portal.toml");
    std::fs::write(
        &config,
        "portal_url = \"https://127.0.0.1:1/srun_portal_pc?ac_id=1\"\nusername = \"testuser\"\n",
    )
    .unwrap();
    let state = dir.join("state");
    // Unreachable portal: the sign-out cannot happen at all.
    let output = Command::new(BIN)
        .args(["disconnect", "--json"])
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env("SRUN_PORTAL_CONFIG", &config)
        .env("SRUN_PORTAL_PASSWORD", "testpass")
        .env("SRUN_TIMEOUT_MS", "2000")
        .output()
        .expect("disconnect runs");
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));

    let control = std::fs::read_to_string(state.join("control.toml")).unwrap();
    assert!(control.contains("paused = true"), "{control}");
    assert_eq!(stub.request_count(), 0, "the stub was never involved");
}

#[test]
fn two_concurrent_reconnects_do_one_login_and_one_busy() {
    let harness = Harness::new("concurrent-reconnect", Behaviour::offline());
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            harness
                .command()
                .args(["reconnect", "--json"])
                .env("SRUN_PORTAL_PASSWORD", "testpass")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn"),
        );
    }
    let mut results = Vec::new();
    for child in children {
        let output = child.wait_with_output().expect("wait");
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
        let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON result");
        results.push(body["result"].as_str().unwrap().to_string());
    }
    results.sort();
    assert_eq!(results, vec!["busy", "reconnected"]);
    assert_eq!(
        harness.stub.count_for("/cgi-bin/srun_portal"),
        1,
        "the busy one sent nothing"
    );
}

#[test]
fn a_permanent_conflict_fails_after_three_logins() {
    let harness = Harness::new("conflict", Behaviour::conflicting());
    let output = harness.run_with_password(&["reconnect", "--json"], "testpass");
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON result");
    assert_eq!(body["result"], "failed");
    assert_eq!(body["error_kind"], "authentication");

    assert_eq!(
        harness.stub.count_for("/cgi-bin/srun_portal"),
        3,
        "the conflict is retried exactly three times"
    );
    let health = harness.health();
    assert_eq!(health["result"], "failed");
    assert_eq!(health["error_kind"], "authentication");
}

#[test]
fn a_slow_portal_hits_the_deadline_and_releases_the_lock() {
    // The portal page arrives one byte at a time, so every individual read is
    // fast enough for the transport timeout while the whole answer outlasts the
    // supervisor's deadline. Only the deadline can stop this run.
    let mut harness = Harness::new("deadline", Behaviour::offline());
    harness.stub = Stub::start(Behaviour {
        dribble: Some(Duration::from_millis(150)),
        ..Behaviour::offline()
    });
    std::fs::write(
        &harness.config,
        format!(
            "portal_url = \"{}\"\nusername = \"testuser\"\n",
            harness.stub.slug()
        ),
    )
    .unwrap();

    let started = Instant::now();
    let output = harness.run_with_password(&["reconnect", "--json"], "testpass");
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "the supervisor bounded the run"
    );
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON result");
    assert_eq!(body["result"], "failed");
    assert_eq!(body["error_kind"], "timeout");
    let health = harness.health();
    assert_eq!(health["error_kind"], "timeout");

    // The lock was released, so the next command can run.
    let output = harness.run(&["service", "status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON output");
    assert_eq!(body["running"], false);
}

#[test]
fn credentials_are_keyed_by_portal_and_account() {
    let dir = TempDir::new("credentials");
    let state = dir.join("state");
    let config = dir.join("srun-portal.toml");
    std::fs::write(
        &config,
        "portal_url = \"https://one.example/srun_portal_pc?ac_id=1\"\nusername = \"testuser\"\n",
    )
    .unwrap();

    let output = Command::new(BIN)
        .args(["credentials", "set", "--store", "file"])
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env("SRUN_PORTAL_CONFIG", &config)
        .env("SRUN_PORTAL_PASSWORD", "secret-one")
        .output()
        .expect("credentials set runs");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("Not verified"),
        "{}",
        stdout(&output)
    );

    let status = Command::new(BIN)
        .args(["credentials", "status", "--json"])
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env("SRUN_PORTAL_CONFIG", &config)
        .env_remove("SRUN_PORTAL_PASSWORD")
        .output()
        .expect("credentials status runs");
    assert!(status.status.success(), "{}", stderr(&status));
    let body: Value = serde_json::from_str(stdout(&status).trim()).expect("JSON output");
    assert_eq!(body["state"], "file");
    assert!(!stdout(&status).contains("secret-one"));

    // The same account at another portal is a different key, so nothing is
    // found there.
    std::fs::write(
        &config,
        "portal_url = \"https://two.example/srun_portal_pc?ac_id=1\"\nusername = \"testuser\"\n",
    )
    .unwrap();
    let status = Command::new(BIN)
        .args(["credentials", "status", "--json"])
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env("SRUN_PORTAL_CONFIG", &config)
        .env_remove("SRUN_PORTAL_PASSWORD")
        .output()
        .expect("credentials status runs");
    let body: Value = serde_json::from_str(stdout(&status).trim()).expect("JSON output");
    assert_eq!(body["state"], "missing", "{body}");

    // Forget removes it, and forgetting again is a success.
    std::fs::write(
        &config,
        "portal_url = \"https://one.example/srun_portal_pc?ac_id=1\"\nusername = \"testuser\"\n",
    )
    .unwrap();
    for expected in ["Removed", "No stored"] {
        let output = Command::new(BIN)
            .args(["credentials", "forget"])
            .env("SRUN_PORTAL_STATE_DIR", &state)
            .env("SRUN_PORTAL_CONFIG", &config)
            .output()
            .expect("credentials forget runs");
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(stdout(&output).contains(expected), "{}", stdout(&output));
    }
}

#[test]
fn the_environment_password_survives_a_broken_credential_file() {
    let harness = Harness::new("broken-credentials", Behaviour::offline());
    std::fs::create_dir_all(harness.state()).unwrap();
    std::fs::write(
        harness.state().join("credentials.toml"),
        "passwords = \"not a table\"\n",
    )
    .unwrap();

    // Without the environment the store is unreadable and the run fails as a
    // credential problem, not as a missing password.
    let output = harness.run(&["reconnect", "--json"]);
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON result");
    assert_eq!(body["error_kind"], "credentials");

    // With it, the broken file does not matter.
    let output = harness.run_with_password(&["reconnect", "--json"], "testpass");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
}

#[test]
fn service_logs_show_the_recorded_events_in_order() {
    let harness = Harness::new("logs", Behaviour::offline());
    harness.run_with_password(&["reconnect"], "testpass");
    harness.run_with_password(&["disconnect"], "testpass");

    let output = harness.run(&["service", "logs", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let events: Vec<Value> = serde_json::from_str(stdout(&output).trim()).expect("JSON array");
    assert!(events.len() >= 2, "{events:?}");
    assert_eq!(events[0]["result"], "reconnected");
    let last = events.last().unwrap();
    assert_eq!(last["result"], "disconnected", "{events:?}");
    for event in &events {
        assert_eq!(event["schema_version"], 1);
        assert!(event["finished_at_ms"].as_u64().unwrap() > 0);
    }

    // Text mode is ascending and readable.
    let output = harness.run(&["service", "logs"]);
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].contains("reconnected"), "{text}");
    assert!(lines.last().unwrap().contains("disconnected"), "{text}");
    assert!(!text.contains("testpass"));
}

#[test]
fn no_output_carries_the_password_or_a_login_query() {
    let harness = Harness::new("no-secrets", Behaviour::offline());
    let output = harness.run_with_password(&["reconnect", "--json"], "hunter2-secret");
    let all = format!("{}{}", stdout(&output), stderr(&output));
    for secret in ["hunter2-secret", "%7BMD5%7D", "chksum=", "info="] {
        assert!(!all.contains(secret), "{secret} leaked:\n{all}");
    }
    let events = std::fs::read_to_string(harness.state().join("events.json")).unwrap();
    for secret in ["hunter2-secret", "%7BMD5%7D", "chksum="] {
        assert!(
            !events.contains(secret),
            "{secret} leaked into events:\n{events}"
        );
    }
}

#[test]
fn the_worker_reports_config_errors_as_config() {
    let dir = TempDir::new("worker-config");
    let state = dir.join("state");
    let config = dir.join("srun-portal.toml");
    std::fs::write(&config, "username = \"testuser\"\n").unwrap();

    let output = Command::new(BIN)
        .arg("__worker")
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env_remove("SRUN_PORTAL_CONFIG")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(worker_request("reconnect", &config, &state).as_bytes())?;
            child.wait_with_output()
        })
        .expect("worker runs");
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON result");
    assert_eq!(body["result"], "failed");
    assert_eq!(body["error_kind"], "config");
}

#[cfg(unix)]
#[test]
fn a_signal_stops_the_worker_and_exits_conventionally() {
    let mut harness = Harness::new("signal", Behaviour::offline());
    harness.stub = Stub::start(Behaviour {
        dribble: Some(Duration::from_millis(150)),
        ..Behaviour::offline()
    });
    std::fs::write(
        &harness.config,
        format!(
            "portal_url = \"{}\"\nusername = \"testuser\"\n",
            harness.stub.slug()
        ),
    )
    .unwrap();

    let mut child = harness
        .command()
        .args(["reconnect"])
        .env("SRUN_PORTAL_PASSWORD", "testpass")
        .spawn()
        .expect("spawn");
    // Let it get as far as the worker, then terminate it.
    std::thread::sleep(Duration::from_secs(2));
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let started = Instant::now();
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(143), "SIGTERM reports 143");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the worker did not outlive its parent"
    );

    // The lock is free again, so nothing is left running.
    let output = harness.run(&["service", "status", "--json"]);
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON output");
    assert_eq!(body["running"], false, "{body}");
}

#[test]
fn an_incomplete_install_refuses_resume_and_sends_nothing() {
    let harness = Harness::new("incomplete", Behaviour::offline());
    harness.record_installation();
    std::fs::write(
        harness.state().join("control.toml"),
        "version = 1\npaused = true\ninstalling = true\n",
    )
    .unwrap();
    let requests = harness.stub.request_count();

    for args in [
        vec!["resume"],
        vec!["service", "start"],
        vec!["service", "restart"],
    ] {
        let output = harness.run_with_password(&args, "testpass");
        assert_eq!(
            output.status.code(),
            Some(3),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("Installation incomplete"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    let output = harness.run_with_password(
        &[
            "__background",
            "--control-dir",
            harness.state().to_str().unwrap(),
            "--trigger",
            "interval",
        ],
        "testpass",
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(
        harness.stub.request_count(),
        requests,
        "an incomplete install never reaches the portal"
    );
    // The record the operator has to repair is still there.
    let control = std::fs::read_to_string(harness.state().join("control.toml")).unwrap();
    assert!(control.contains("installing = true"), "{control}");
}

#[test]
fn an_install_with_an_unusable_configuration_registers_nothing() {
    let harness = Harness::new("install-config", Behaviour::offline());
    std::fs::write(&harness.config, "username = \"testuser\"\n").unwrap();

    let output = harness.run(&["service", "install"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert_eq!(
        harness.stub.request_count(),
        0,
        "an install makes no portal request"
    );
    assert!(
        !harness.state().join("installed.toml").exists(),
        "nothing was registered"
    );
}

#[test]
fn stop_pauses_even_with_nothing_installed() {
    let harness = Harness::new("stop-uninstalled", Behaviour::offline());

    let output = harness.run(&["service", "stop"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let control = std::fs::read_to_string(harness.state().join("control.toml")).unwrap();
    assert!(control.contains("paused = true"), "{control}");

    // And a manual reconnect respects it.
    let requests = harness.stub.request_count();
    let output = harness.run_with_password(&["reconnect", "--json"], "testpass");
    let body: Value = serde_json::from_str(stdout(&output).trim()).expect("JSON result");
    assert_eq!(body["result"], "paused");
    assert_eq!(harness.stub.request_count(), requests);
}

#[test]
fn concurrent_credential_writes_for_two_accounts_both_land() {
    let dir = TempDir::new("credentials-concurrent");
    let state = dir.join("state");
    let mut children = Vec::new();
    for (portal, account) in [
        ("https://one.example", "alice"),
        ("https://two.example", "bob"),
    ] {
        let config = dir.join(&format!("{account}.toml"));
        std::fs::write(
            &config,
            format!("portal_url = \"{portal}/srun_portal_pc?ac_id=1\"\nusername = \"{account}\"\n"),
        )
        .unwrap();
        children.push(
            Command::new(BIN)
                .args(["credentials", "set", "--store", "file"])
                .env("SRUN_PORTAL_STATE_DIR", &state)
                .env("SRUN_PORTAL_CONFIG", &config)
                .env("SRUN_PORTAL_PASSWORD", format!("secret-{account}"))
                .spawn()
                .expect("spawn"),
        );
    }
    for mut child in children {
        assert!(child.wait().expect("wait").success());
    }

    let text = std::fs::read_to_string(state.join("credentials.toml")).unwrap();
    assert!(text.contains("alice"), "{text}");
    assert!(text.contains("bob"), "{text}");
    assert!(text.contains("secret-alice"), "{text}");
    assert!(text.contains("secret-bob"), "{text}");
}

#[test]
fn version_matches_the_crate() {
    let output = Command::new(BIN).arg("--version").output().expect("runs");
    assert!(output.status.success());
    assert_eq!(
        stdout(&output).trim(),
        format!("srun-portal {}", env!("CARGO_PKG_VERSION"))
    );
}
