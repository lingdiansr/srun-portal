//! The supervisor: one bounded, serialised portal operation.
//!
//! A [`Supervisor`] owns the whole life of one run:
//!
//! * it waits out the startup delay **before** taking the operation lock or
//!   starting the deadline, so a slow login does not eat the budget;
//! * it re-checks the pause before and after the delay and while the worker
//!   runs, so `disconnect` takes effect on a run that is already going;
//! * it holds the user's operation lock for the whole run, which is what makes a
//!   second concurrent run report `busy` instead of authenticating twice;
//! * it starts `this-executable __worker reconnect|disconnect`, hands it the
//!   configuration source on stdin, and kills the whole worker tree at
//!   [`super::DEADLINE`];
//! * it parses the worker's single JSON line and returns it, so the caller
//!   formats once and persists once.
//!
//! Signals do not exit the process behind the worker's back: `SIGINT`/`SIGTERM`
//! set a cancellation flag the supervisor polls, kill the worker, and then exit
//! with the conventional status.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use std::io::Read;

use super::{config, settings, DEADLINE};
use crate::control::{ControlDir, ErrorKind, Outcome};
use crate::credentials;
use crate::process;
use crate::reconnect::{self, OutcomeReport, ReconnectError, Target};

/// What the worker is asked to do.
///
/// The control directory travels with the request because the installed task may
/// run with an explicit `--control-dir` that is not the default one, and that is
/// where its credentials are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub operation: String,
    pub source: settings::ConfigSource,
    pub control_dir: std::path::PathBuf,
}

/// Set by the signal handler; polled by the supervisor.
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Which signal asked for the cancellation, so the exit status can say which
/// (`130` for `SIGINT`, `143` for `SIGTERM`).
static SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Make `SIGINT` and `SIGTERM` cancel the current operation.
///
/// Installed once per process. The handlers do nothing but record the signal —
/// the supervisor is what stops the worker and then exits — so no `exit` races
/// the cleanup, and a worker is never left talking to the portal.
pub fn install_signals() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = ctrlc::set_handler(|| record(signal_hook::SIGINT));
        #[cfg(unix)]
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle_term as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = 0;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        }
    });
}

fn record(signal: i32) {
    // The first signal wins: a second one does not rewrite the report.
    let _ = SIGNAL.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
    CANCELLED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" fn handle_term(_signal: libc::c_int) {
    // Only async-signal-safe work here: two atomics.
    record(libc::SIGTERM);
}

/// The exit status the conventional shell expects for a signal.
fn signal_exit_status() -> i32 {
    128 + SIGNAL.load(Ordering::SeqCst).max(1)
}

/// `SIGINT`/`SIGTERM` numbers, without pulling in a second signal crate.
mod signal_hook {
    pub const SIGINT: i32 = 2;
}

fn cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

/// One supervised run.
pub struct Supervisor<'a> {
    pub control: &'a ControlDir,
    pub source: &'a settings::ConfigSource,
    pub operation: reconnect::Action,
    /// Whether a pause stops this run. `disconnect` is exempt: it is the command
    /// that creates the pause.
    pub respect_pause: bool,
    /// Wait this long before starting, checking the pause on both sides.
    pub startup_delay_secs: Option<u64>,
    /// Wait for a run already in progress to finish before taking the lock.
    pub drain: bool,
}

impl Supervisor<'_> {
    pub fn run(&self) -> OutcomeReport {
        install_signals();
        if self.respect_pause {
            match self.paused() {
                Ok(true) => return OutcomeReport::paused(),
                Ok(false) => {}
                Err(err) => return OutcomeReport::failed(ErrorKind::Config, err),
            }
        }
        if let Some(secs) = self.startup_delay_secs {
            if secs > 0 && !self.sleep_cancellable(Duration::from_secs(secs)) {
                return OutcomeReport::failed(ErrorKind::Platform, "cancelled");
            }
            if self.respect_pause {
                match self.paused() {
                    Ok(true) => return OutcomeReport::paused(),
                    Ok(false) => {}
                    Err(err) => return OutcomeReport::failed(ErrorKind::Config, err),
                }
            }
        }

        let _lock = match self.acquire() {
            Ok(Some(lock)) => lock,
            Ok(None) => return OutcomeReport::busy(),
            Err(err) => return OutcomeReport::failed(ErrorKind::Config, err),
        };

        let request = WorkerRequest {
            operation: match self.operation {
                reconnect::Action::Reconnect => "reconnect".to_string(),
                reconnect::Action::Disconnect => "disconnect".to_string(),
            },
            source: self.source.clone(),
            control_dir: self.control.path().to_path_buf(),
        };
        let body = match serde_json::to_string(&request) {
            Ok(body) => body,
            Err(err) => {
                return OutcomeReport::failed(
                    ErrorKind::Config,
                    format!("cannot encode the request: {err}"),
                )
            }
        };
        let argv = match worker_argv() {
            Ok(argv) => argv,
            Err(err) => return OutcomeReport::failed(ErrorKind::Platform, err),
        };

        let pause_check = || -> bool {
            if !self.respect_pause {
                return false;
            }
            matches!(self.paused(), Ok(true))
        };
        let watched = || cancelled() || pause_check();
        let outcome = process::supervise(&argv, Some(body.as_bytes()), DEADLINE, &watched);
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(err) => return OutcomeReport::failed(ErrorKind::Platform, err.to_string()),
        };

        match outcome.stop {
            process::Stop::Cancelled => {
                if pause_check() {
                    OutcomeReport::paused()
                } else {
                    exit_with(signal_exit_status())
                }
            }
            process::Stop::TimedOut => OutcomeReport::failed(
                ErrorKind::Timeout,
                format!("the run did not finish within {}s", DEADLINE.as_secs()),
            ),
            process::Stop::Truncated => OutcomeReport::failed(
                ErrorKind::Platform,
                "the run produced too much output and was stopped".to_string(),
            ),
            process::Stop::Exited(_) => match parse_report(&outcome.stdout_text()) {
                Some(report) => report,
                None => {
                    let stderr = outcome.stderr_text();
                    OutcomeReport::failed(
                        ErrorKind::Platform,
                        if stderr.is_empty() {
                            "the worker did not report a result".to_string()
                        } else {
                            stderr
                        },
                    )
                }
            },
        }
    }

    /// Take the operation lock, waiting for an in-flight run when asked to.
    fn acquire(&self) -> Result<Option<ProcessGuard>, String> {
        if self.drain {
            let started = Instant::now();
            while started.elapsed() < super::DRAIN_LIMIT {
                if cancelled() {
                    exit_with(signal_exit_status());
                }
                match self.control.try_lock_operation() {
                    Ok(Some(guard)) => return Ok(Some(ProcessGuard(guard))),
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    Err(err) => return Err(err.to_string()),
                }
            }
            return Err(format!(
                "another run is still in progress after {}s",
                super::DRAIN_LIMIT.as_secs()
            ));
        }
        match self.control.try_lock_operation() {
            Ok(Some(guard)) => Ok(Some(ProcessGuard(guard))),
            Ok(None) => Ok(None),
            Err(err) => Err(err.to_string()),
        }
    }

    fn paused(&self) -> Result<bool, String> {
        self.control
            .read_control()
            .map(|state| state.paused)
            .map_err(|err| err.to_string())
    }

    /// Sleep, waking early when a signal arrives. `false` means it was cancelled.
    fn sleep_cancellable(&self, total: Duration) -> bool {
        let started = Instant::now();
        while started.elapsed() < total {
            if cancelled() {
                return false;
            }
            let remaining = total.saturating_sub(started.elapsed());
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
        !cancelled()
    }
}

/// The operation lock, held for the whole run. Dropping it releases the lock,
/// which is what lets the next command (or `service status`) see "not running".
struct ProcessGuard(#[allow(dead_code)] crate::persistence::FileLock);

fn exit_with(code: i32) -> ! {
    std::process::exit(code)
}

/// This executable plus `__worker`.
fn worker_argv() -> Result<Vec<String>, String> {
    let exe =
        std::env::current_exe().map_err(|err| format!("cannot locate the executable: {err}"))?;
    Ok(vec![
        exe.to_string_lossy().into_owned(),
        "__worker".to_string(),
    ])
}

/// Read the worker's single JSON result.
fn parse_report(stdout: &str) -> Option<OutcomeReport> {
    let line = stdout.lines().rev().find(|line| !line.trim().is_empty())?;
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let result = match value.get("result")?.as_str()? {
        "online" => Outcome::Online,
        "reconnected" => Outcome::Reconnected,
        "disconnected" => Outcome::Disconnected,
        "paused" => Outcome::Paused,
        "busy" => Outcome::Busy,
        "failed" => Outcome::Failed,
        _ => return None,
    };
    let error_kind = value
        .get("error_kind")
        .and_then(|kind| serde_json::from_value(kind.clone()).ok());
    Some(OutcomeReport {
        result,
        error_kind,
        message: value
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// `__worker`: the child the supervisor starts.
///
/// It reads one [`WorkerRequest`] from stdin, does the work, prints one JSON
/// line and exits. It never takes the operation lock — the parent holds it — and
/// it never prompts, so nothing here can wait on a terminal.
pub fn run_worker() -> i32 {
    let mut body = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut body) {
        eprintln!("cannot read the request: {err}");
        return 3;
    }
    let request: WorkerRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(err) => {
            eprintln!("cannot parse the request: {err}");
            return 3;
        }
    };
    let report = match request.operation.as_str() {
        "reconnect" => worker_reconnect(
            &request.source,
            &request.control_dir,
            reconnect::Action::Reconnect,
        ),
        "disconnect" => worker_reconnect(
            &request.source,
            &request.control_dir,
            reconnect::Action::Disconnect,
        ),
        other => OutcomeReport::failed(ErrorKind::Config, format!("unknown operation: {other}")),
    };
    println!(
        "{}",
        serde_json::to_string(&report).unwrap_or_else(|_| {
            "{\"result\":\"failed\",\"error_kind\":\"platform\",\"message\":\"cannot encode\"}"
                .to_string()
        })
    );
    report.exit_code()
}

fn worker_reconnect(
    source: &settings::ConfigSource,
    control_dir: &std::path::Path,
    action: reconnect::Action,
) -> OutcomeReport {
    let resolution = match settings::Resolution::open(source.clone()) {
        Ok(resolution) => resolution,
        Err(err) => return OutcomeReport::failed(ErrorKind::Config, err.to_string()),
    };
    if let Err(err) = settings::validate(resolution.effective(), true) {
        return OutcomeReport::failed(ErrorKind::Config, err.to_string());
    }
    let _ = crate::transport::set_timeouts(resolution.effective().timeouts);
    let (portal, account) = match super::target_of(resolution.effective()) {
        Ok(pair) => pair,
        Err(status) => {
            return OutcomeReport::failed(ErrorKind::Config, format!("exit status {status}"))
        }
    };
    match run_target(
        &portal,
        &account,
        resolution.effective(),
        control_dir,
        action,
    ) {
        Ok(outcome) => OutcomeReport::from(outcome),
        Err(err) => err.into_report(),
    }
}

fn run_target(
    portal: &config::PortalUrl,
    account: &crate::account::AccountIdentity,
    effective: &settings::Effective,
    control_dir: &std::path::Path,
    action: reconnect::Action,
) -> Result<Outcome, ReconnectError> {
    let key = credentials::credential_key(portal, account.qualified());
    let path = control_dir.join(crate::credentials::CREDENTIALS_FILE_NAME);
    let env = std::env::var(credentials::ENV_PASSWORD).ok();
    let password = match credentials::resolve(&key, &path, env.as_deref()) {
        Ok(Some(password)) => password,
        Ok(None) => {
            return Err(ReconnectError::new(
                ErrorKind::Credentials,
                crate::messages::PASSWORD_REQUIRED,
            ))
        }
        Err(err) => return Err(ReconnectError::new(ErrorKind::Credentials, err.to_string())),
    };
    let target = Target {
        portal: portal.clone(),
        username: account.local().to_string(),
        domain: account.domain().to_string(),
        callback: effective.callback.clone(),
    };
    match action {
        reconnect::Action::Reconnect => reconnect::reconnect_once(&target, &password),
        reconnect::Action::Disconnect => reconnect::disconnect_once(&target, &password),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_result_round_trips() {
        let report = OutcomeReport::failed(ErrorKind::Timeout, "too slow");
        let text = serde_json::to_string(&report).unwrap();
        assert_eq!(parse_report(&text), Some(report));

        let online = OutcomeReport::from(Outcome::Reconnected);
        assert_eq!(
            parse_report(&serde_json::to_string(&online).unwrap()),
            Some(online)
        );

        assert_eq!(parse_report(""), None);
        assert_eq!(parse_report("not json"), None);
        assert_eq!(parse_report("{\"result\":\"mystery\"}"), None);
    }

    #[test]
    fn a_request_carries_the_source_and_nothing_secret() {
        let request = WorkerRequest {
            operation: "reconnect".to_string(),
            source: settings::ConfigSource::single("/etc/srun-portal/srun-portal.toml"),
            control_dir: std::path::PathBuf::from("/tmp/state"),
        };
        let body = serde_json::to_string(&request).unwrap();
        let back: WorkerRequest = serde_json::from_str(&body).unwrap();
        assert_eq!(back.operation, "reconnect");
        assert_eq!(back.source.layers.len(), 1);
        assert!(!body.contains("password"));
    }
}
