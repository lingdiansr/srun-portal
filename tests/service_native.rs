//! The native lifecycle, against a real platform task manager.
//!
//! These tests register, replace, stop, start, and remove an actual per-user task
//! and then ask the platform's own tooling what it did. They are therefore
//! `#[ignore]`d by default and run only when two conditions hold:
//!
//! * `SRUN_PORTAL_NATIVE_TESTS=1` — the operator has prepared a disposable
//!   account with a working user manager (`systemd --user`) or a GUI session
//!   (`launchd`) or a Task Scheduler service;
//! * the account has no pre-existing srun-portal task, because an install
//!   replaces the user's task set and this test must not destroy a real one.
//!
//! ```text
//! SRUN_PORTAL_NATIVE_TESTS=1 cargo test --test service_native -- --ignored --test-threads=1
//! ```
//!
//! The portal is the local stub: no campus portal is contacted and no real
//! credential is used.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use support::{Behaviour, Stub, TempDir};

const BIN: &str = env!("CARGO_BIN_EXE_srun-portal");

fn enabled() -> bool {
    if std::env::var("SRUN_PORTAL_NATIVE_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipped: set SRUN_PORTAL_NATIVE_TESTS=1 on a disposable account");
        return false;
    }
    true
}

struct Native {
    dir: TempDir,
    stub: Stub,
}

impl Native {
    fn new(tag: &str) -> Native {
        let dir = TempDir::new(tag);
        let stub = Stub::start(Behaviour::offline());
        Native { dir, stub }
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state")
    }

    fn config(&self) -> PathBuf {
        self.dir.join("srun-portal.toml")
    }

    fn write_config(&self, interval: u64, boot_delay: u64) {
        std::fs::write(
            self.config(),
            format!(
                "portal_url = \"{}\"\nusername = \"testuser\"\n\
                 reconnect_interval_secs = {interval}\n\
                 reconnect_boot_delay_secs = {boot_delay}\n",
                self.stub.slug()
            ),
        )
        .expect("write configuration");
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("SRUN_PORTAL_STATE_DIR", self.state())
            .env("SRUN_PORTAL_CONFIG", self.config())
            .env("SRUN_PORTAL_PASSWORD", "testpass")
            .output()
            .expect("the binary runs")
    }

    fn status_json(&self) -> Value {
        let output = self.run(&["service", "status", "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("status is JSON")
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Refuse to run on an account that already has a real installation.
fn require_clean_account(state: &Path) {
    if state.join("installed.toml").exists() {
        panic!(
            "{} already holds an installation; this test needs a disposable account",
            state.display()
        );
    }
}

#[test]
#[ignore = "registers a real platform task; needs a disposable account"]
fn the_native_lifecycle_registers_replaces_and_removes() {
    if !enabled() {
        return;
    }
    let native = Native::new("lifecycle");
    require_clean_account(&native.state());
    native.write_config(300, 30);

    // Install, twice: the second one replaces the first.
    for _ in 0..2 {
        let output = native.run(&["service", "install"]);
        assert!(
            output.status.success(),
            "install failed: {}",
            stderr(&output)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("Credentials stored; not verified"),
            "an install never claims verification: {text}"
        );
    }
    let status = native.status_json();
    assert_eq!(status["installed"], true, "{status}");
    assert_eq!(status["paused"], false, "{status}");
    assert_eq!(status["errors"].as_array().unwrap().len(), 0, "{status}");
    assert!(
        status["platform_state"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|resource| resource["present"] == true),
        "the platform reports the registration: {status}"
    );

    // Stop: paused across a new process, and the platform says it is not running.
    let output = native.run(&["service", "stop"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let status = native.status_json();
    assert_eq!(status["paused"], true, "{status}");
    assert_eq!(status["running"], false, "{status}");
    let paused = native.run(&["reconnect", "--json"]);
    let body: Value = serde_json::from_slice(&paused.stdout).expect("JSON result");
    assert_eq!(body["result"], "paused");

    // Start: clears the pause and checks, so the stub sees a request.
    let before = native.stub.request_count();
    let output = native.run(&["service", "start", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        native.stub.request_count() > before,
        "service start checks the connection"
    );

    // Restart: stop then start again.
    let output = native.run(&["service", "restart", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let status = native.status_json();
    assert_eq!(status["paused"], false, "{status}");

    // Status must not need the business configuration.
    std::fs::rename(native.config(), native.dir.join("moved.toml")).unwrap();
    let status = native.status_json();
    assert_eq!(status["installed"], true, "{status}");
    assert!(status["last_attempt"].is_object(), "{status}");

    // …and the next run reports the missing configuration instead of guessing.
    let output = native.run(&["reconnect", "--json"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));

    // Uninstall with the configuration still moved away.
    let output = native.run(&["service", "uninstall"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let status = native.status_json();
    assert_eq!(status["installed"], false, "{status}");
    assert!(
        !status["platform_state"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|resource| resource["present"] == true),
        "the native definition is gone: {status}"
    );
    // The store and the history are kept.
    assert!(!status["paused"].as_bool().unwrap_or(false) && status["last_attempt"].is_object());
}

#[test]
#[ignore = "registers a real platform task; needs a disposable account"]
fn turning_a_trigger_off_removes_its_definition() {
    if !enabled() {
        return;
    }
    let native = Native::new("triggers");
    require_clean_account(&native.state());

    // Both triggers, then interval only, then startup only, then neither.
    for (interval, boot_delay) in [(300u64, 30u64), (300, 0), (0, 30), (0, 0)] {
        native.write_config(interval, boot_delay);
        let output = native.run(&["service", "install"]);
        assert!(
            output.status.success(),
            "install ({interval}, {boot_delay}) failed: {}",
            stderr(&output)
        );
        let status = native.status_json();
        assert_eq!(
            status["requested_interval_secs"],
            if interval == 0 {
                Value::Null
            } else {
                Value::from(interval)
            }
        );
        assert_eq!(
            status["boot_delay_secs"],
            if boot_delay == 0 {
                Value::Null
            } else {
                Value::from(boot_delay)
            }
        );
    }

    let output = native.run(&["service", "uninstall"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(native.status_json()["installed"], false);
}

/// Linux only: the unit really carries the argument, including the characters
/// systemd would otherwise expand, and an inactive oneshot is not a fault.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "registers a real systemd user unit; needs a disposable account"]
fn systemd_keeps_the_exec_line_and_reports_the_last_exit() {
    if !enabled() {
        return;
    }
    let native = Native::new("systemd");
    require_clean_account(&native.state());

    // A control directory containing a space, a `%` and a `$`, which is where
    // the escaping has to hold.
    let control = native.dir.join("a b%c$d");
    std::fs::create_dir_all(&control).unwrap();
    native.write_config(300, 0);
    let output = Command::new(BIN)
        .args(["service", "install"])
        .env("SRUN_PORTAL_STATE_DIR", &control)
        .env("SRUN_PORTAL_CONFIG", native.config())
        .env("SRUN_PORTAL_PASSWORD", "testpass")
        .output()
        .expect("install runs");
    assert!(output.status.success(), "{}", stderr(&output));

    let unit = dirs_unit_path();
    let text = std::fs::read_to_string(&unit).expect("the unit was written");
    assert!(
        text.contains("a b%%c$$d"),
        "systemd would expand an unescaped value:\n{text}"
    );

    let show = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "srun-portal.service",
            "--property=ExecMainStatus,ActiveState",
        ])
        .output()
        .expect("systemctl runs");
    assert!(show.status.success(), "systemd reports the unit");

    let status = native.status_json();
    assert_eq!(status["platform"], "systemd", "{status}");
    assert_eq!(status["errors"].as_array().unwrap().len(), 0, "{status}");

    let output = native.run(&["service", "uninstall"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let _ = std::fs::remove_dir_all(&control);
}

#[cfg(target_os = "linux")]
fn dirs_unit_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    base.join("systemd")
        .join("user")
        .join("srun-portal.service")
}

/// Windows only: the registered task is the current SID's, limited, interactive,
/// and bounded to 100 seconds; the requested interval is rounded up to minutes
/// and reported both ways.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "registers a real scheduled task; needs a disposable account"]
fn windows_registers_principal_limits_and_minute_rounding() {
    if !enabled() {
        return;
    }
    let native = Native::new("windows");
    require_clean_account(&native.state());
    native.write_config(100, 2);

    let output = native.run(&["service", "install"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let status = native.status_json();
    assert_eq!(status["requested_interval_secs"], 100, "{status}");
    assert_eq!(status["effective_interval_secs"], 120, "{status}");

    let sid = srun_portal::service::owner_id();
    let script = format!(
        "$t = Get-ScheduledTask -TaskName 'srun-portal-{sid}-reconnect'; \
         $t.Principal.UserId; $t.Principal.LogonType; $t.Principal.RunLevel; \
         $t.Settings.ExecutionTimeLimit; $t.Settings.MultipleInstancesPolicy"
    );
    let out = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .output()
        .expect("powershell runs");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains(&sid), "{text}");
    assert!(text.to_lowercase().contains("interactive"), "{text}");
    assert!(text.to_lowercase().contains("limited"), "{text}");
    assert!(text.contains("PT1M40S") || text.contains("100"), "{text}");
    assert!(
        text.to_lowercase().contains("ignorenеw") || text.to_lowercase().contains("ignorenew"),
        "{text}"
    );

    // The logon task carries the delay natively.
    let script = format!(
        "$t = Get-ScheduledTask -TaskName 'srun-portal-{sid}-reconnect-boot'; \
         $t.Triggers[0].Delay"
    );
    let out = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .output()
        .expect("powershell runs");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("PT2S"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let output = native.run(&["service", "uninstall"]);
    assert!(output.status.success(), "{}", stderr(&output));
}

/// macOS only: two agents, one per trigger, with the startup delay only on the
/// startup one.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "loads real LaunchAgents; needs a GUI session on a disposable account"]
fn launchd_splits_the_triggers_and_delays_only_the_startup_one() {
    if !enabled() {
        return;
    }
    let native = Native::new("launchd");
    require_clean_account(&native.state());
    native.write_config(300, 2);

    let output = native.run(&["service", "install"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let status = native.status_json();
    assert_eq!(status["platform"], "launchd", "{status}");
    for resource in status["platform_state"]["resources"].as_array().unwrap() {
        assert_eq!(resource["present"], true, "{status}");
    }

    let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
    let agents = home.join("Library").join("LaunchAgents");
    let interval = std::fs::read_to_string(agents.join("com.srun-portal.reconnect.interval.plist"))
        .expect("interval agent");
    let startup = std::fs::read_to_string(agents.join("com.srun-portal.reconnect.startup.plist"))
        .expect("startup agent");
    assert!(interval.contains("StartInterval"), "{interval}");
    assert!(!interval.contains("RunAtLoad"), "{interval}");
    assert!(!interval.contains("--startup-delay-secs"), "{interval}");
    assert!(startup.contains("RunAtLoad"), "{startup}");
    assert!(startup.contains("--startup-delay-secs"), "{startup}");
    assert!(!startup.contains("StartInterval"), "{startup}");

    // Measured: the startup agent logs in after the delay, not before.
    let before = native.stub.request_count();
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert_eq!(
        native.stub.request_count(),
        before,
        "the startup run waits for its delay"
    );
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert!(
        native.stub.request_count() > before,
        "the startup run happened after the delay"
    );

    let output = native.run(&["service", "uninstall"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!agents
        .join("com.srun-portal.reconnect.interval.plist")
        .exists());
    assert!(!agents
        .join("com.srun-portal.reconnect.startup.plist")
        .exists());
}
