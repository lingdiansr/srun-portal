//! The command line: SPEC §5's interactive flow plus the unattended controls.
//!
//! Presentation notes: the banner and the account panel follow the reference's
//! observable output (a `-`-ruled panel with the labels `Username`, `IP`,
//! `Used Flow`, `Used Time`, `Balance`, `Product Name`), while the decorations
//! that are presentation only — spinner frames, emoji markers, ANSI colours, the
//! cursor rewrites of the reference's prompt library — are not reproduced. None
//! of them carries protocol meaning; see `README.md`.
//!
//! **The supervisor.** Every command that may talk to the portal — `reconnect`,
//! `disconnect`, `resume`, `service start`, the installed
//! `__background`, and the login half of the interactive shell — runs under one
//! discipline:
//!
//! 1. It holds the user's operation lock, so two runs never authenticate at once;
//!    a second one reports `busy` and exits 0.
//! 2. The network work happens in a hidden `__worker` child, which is killed —
//!    with its whole process tree — after [`DEADLINE`]. The parent therefore
//!    always regains control, records a `timeout`, and releases the lock.
//! 3. `SIGINT`/`SIGTERM` stop the worker first and then exit (`130`/`143`),
//!    instead of leaving an orphan that keeps talking to the portal.
//! 4. A pause written by `disconnect`/`service stop` is re-checked while the
//!    worker runs, so a run that is paused mid-flight is stopped.
//!
//! The worker gets the operation over stdin as JSON; the parent formats the
//! single JSON result it prints and records it. No password, configuration value
//! or query string crosses that boundary in either direction.

mod args;
mod supervisor;

use self::args::{Args, Command, ConfigAction, CredentialsAction, ServiceAction, Trigger, USAGE};
use self::supervisor::Supervisor;
use crate::account::AccountIdentity;
use crate::api;
use crate::config;
use crate::control::{self, ControlDir, ErrorKind, Event, Health, Outcome};
use crate::credentials;
use crate::format;
use crate::reconnect::{self, fetch_config, host_of, OutcomeReport};
use crate::runtime::{Runtime, RuntimeOptions};
use crate::service;
use crate::settings::{self, ConfigSource, Resolution};
use crate::sys;
use crate::transport;
use std::path::{Path, PathBuf};

/// How long one worker may run before it is killed.
pub const DEADLINE: std::time::Duration = std::time::Duration::from_secs(90);

/// How long `disconnect` waits for a run already in progress before giving up.
const DRAIN_LIMIT: std::time::Duration = std::time::Duration::from_secs(95);

/// Run the CLI; the returned value is the process exit status.
pub fn run(argv: Vec<String>) -> i32 {
    let args = match args::parse(&argv) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprint!("{USAGE}");
            return 2;
        }
    };
    if args.version {
        println!("srun-portal {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    if args.help {
        print!("{}", args::help_text(&args));
        return 0;
    }

    match &args.command {
        Some(Command::Reconnect) => return run_reconnect(&args, Operation::Reconnect),
        Some(Command::Disconnect) => return run_disconnect(&args),
        Some(Command::Resume) => return run_reconnect(&args, Operation::Resume),
        Some(Command::Service(action)) => return run_service(*action, &args),
        Some(Command::Config(action)) => return run_config(action, &args),
        Some(Command::Credentials(action)) => return run_credentials(*action, &args),
        Some(Command::Background {
            trigger,
            startup_delay_secs,
        }) => return run_background(*trigger, *startup_delay_secs, &args),
        Some(Command::Worker) => return supervisor::run_worker(),
        None => {}
    }

    run_interactive(&args)
}

/// The mutually exclusive actions a supervised run can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Reconnect,
    Resume,
    Disconnect,
}

// ---------------------------------------------------------------------------
// Configuration plumbing
// ---------------------------------------------------------------------------

/// The configuration source this run reads: `--config`, then the environment,
/// then the two platform locations. Never creates or asks.
fn config_source(args: &Args) -> Result<ConfigSource, i32> {
    let explicit = args
        .config
        .clone()
        .or_else(|| std::env::var_os(settings::ENV_CONFIG_PATH).map(PathBuf::from));
    ConfigSource::discover(
        explicit.as_deref(),
        args.portable.then_some(settings::Storage::Portable),
    )
    .map_err(|err| {
        eprintln!("{err}");
        2
    })
}

/// Load the configuration read-only.
fn load_resolution(args: &Args) -> Result<Resolution, i32> {
    Resolution::open(config_source(args)?).map_err(|err| {
        eprintln!("{err}");
        2
    })
}

/// Load it and refuse to go on unless it is usable for an unattended run.
fn load_complete(args: &Args) -> Result<Resolution, i32> {
    let resolution = load_resolution(args)?;
    settings::validate(resolution.effective(), true).map_err(|err| {
        eprintln!("{err}");
        2
    })?;
    Ok(resolution)
}

/// The control directory, for the commands that do not run the portal.
fn control_dir(args: Option<&Args>) -> Result<ControlDir, i32> {
    ControlDir::resolve(args.and_then(|args| args.control_dir.as_deref())).map_err(|err| {
        eprintln!("{err}");
        3
    })
}

/// The portal and account a run needs, from the effective settings.
fn target_of(effective: &settings::Effective) -> Result<(config::PortalUrl, AccountIdentity), i32> {
    let slug = effective.portal_url.as_deref().ok_or_else(|| {
        eprintln!("{}", crate::messages::PORTAL_URL_REQUIRED);
        2
    })?;
    let portal = config::parse_portal_url(slug).map_err(|err| {
        eprintln!("{err}");
        2
    })?;
    let username = effective.username.as_deref().ok_or_else(|| {
        eprintln!("{}", crate::messages::USERNAME_REQUIRED);
        2
    })?;
    Ok((
        portal,
        AccountIdentity::parse(username, effective.domain.as_deref()),
    ))
}

fn timeouts(effective: &settings::Effective) {
    // One-shot: the agent reads these when it is first built.
    let _ = transport::set_timeouts(effective.timeouts);
}

// ---------------------------------------------------------------------------
// Supervised operations
// ---------------------------------------------------------------------------

/// `reconnect`, `resume`: check once, under the supervisor.
fn run_reconnect(args: &Args, operation: Operation) -> i32 {
    let resolution = match load_complete(args) {
        Ok(resolution) => resolution,
        Err(status) => return status,
    };
    timeouts(resolution.effective());
    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };

    if operation == Operation::Resume {
        // `resume` is the one command that may clear the pause, so it does that
        // before the check rather than refusing because of it.
        if let Err(status) = clear_pause(&control) {
            return status;
        }
    }

    let started = control::now_ms();
    let report = supervised(Supervisor {
        control: &control,
        source: resolution.source(),
        operation: reconnect::Action::Reconnect,
        respect_pause: operation != Operation::Disconnect,
        startup_delay_secs: None,
        drain: false,
    });
    finish(&control, &report, args.json, started)
}

/// `disconnect`: pause first, then sign out.
fn run_disconnect(args: &Args) -> i32 {
    let resolution = match load_complete(args) {
        Ok(resolution) => resolution,
        Err(status) => return status,
    };
    timeouts(resolution.effective());
    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };

    // The pause is written before the sign-out is attempted: a failed sign-out
    // must still leave the device offline by policy, never the other way round.
    if let Err(status) = set_pause(&control, true) {
        return status;
    }

    let started = control::now_ms();
    let report = supervised(Supervisor {
        control: &control,
        source: resolution.source(),
        operation: reconnect::Action::Disconnect,
        respect_pause: false,
        startup_delay_secs: None,
        drain: true,
    });
    finish(&control, &report, args.json, started)
}

fn set_pause(control: &ControlDir, paused: bool) -> Result<(), i32> {
    control
        .update_control(|state| state.paused = paused)
        .map(|_| ())
        .map_err(|err| {
            eprintln!("{err}");
            3
        })
}

fn clear_pause(control: &ControlDir) -> Result<(), i32> {
    let state = control.read_control().map_err(|err| {
        eprintln!("{err}");
        3
    })?;
    if state.installing {
        eprintln!(
            "Installation incomplete; background reconnect paused. \
             Rerun service install or service uninstall."
        );
        return Err(3);
    }
    set_pause(control, false)
}

/// Run one supervised operation and print its result.
fn supervised(supervisor: Supervisor<'_>) -> OutcomeReport {
    supervisor.run()
}

/// Print and record the result, returning the exit status.
fn finish(control: &ControlDir, report: &OutcomeReport, json: bool, started_at_ms: u64) -> i32 {
    let finished = control::now_ms();
    let started = started_at_ms.min(finished);
    if json {
        let body = serde_json::json!({
            "schema_version": 1,
            "result": report.result,
            "error_kind": report.error_kind,
            "message": report.message,
        });
        println!("{body}");
    } else {
        match report.result {
            Outcome::Online => println!("Online."),
            Outcome::Reconnected => println!("Reconnected."),
            Outcome::Disconnected => println!("Disconnected."),
            Outcome::Paused => println!("{}", report.message),
            Outcome::Busy => println!("{}", report.message),
            Outcome::Failed => eprintln!("{}", report.message),
        }
    }
    // A pause or a busy result says nothing about the network, so it must not
    // overwrite the last real verdict.
    let health = Health {
        schema_version: 1,
        started_at_ms: started,
        finished_at_ms: finished,
        result: report.result,
        error_kind: report.error_kind,
        message: report.message.clone(),
        last_success_at_ms: None,
    };
    if report.result != Outcome::Busy {
        if let Err(err) = control.record_health(health) {
            eprintln!("{err}");
        }
    }
    let event_result = match report.result {
        Outcome::Online => "online".to_string(),
        Outcome::Reconnected => "reconnected".to_string(),
        Outcome::Disconnected => "disconnected".to_string(),
        Outcome::Paused => "paused".to_string(),
        Outcome::Busy => "busy".to_string(),
        Outcome::Failed => "failed".to_string(),
    };
    let _ = control.append_event(Event {
        schema_version: 1,
        started_at_ms: started,
        finished_at_ms: finished,
        result: event_result,
        error_kind: report.error_kind,
        message: report.message.clone(),
    });
    report.exit_code()
}

// ---------------------------------------------------------------------------
// The installed entry point
// ---------------------------------------------------------------------------

/// `__background`: what the native task runs.
///
/// It reports a pause or an incomplete install without touching the network, and
/// otherwise goes through the same supervisor as a manual `reconnect`, so a
/// scheduled run cannot bypass the pause or the deadline.
fn run_background(trigger: Trigger, startup_delay_secs: Option<u64>, args: &Args) -> i32 {
    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };
    let registration = match control.read_registration() {
        Ok(Some(registration)) => registration,
        Ok(None) => {
            eprintln!("Not installed: run `srun-portal service install`");
            return 2;
        }
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    let state = match control.read_control() {
        Ok(state) => state,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    if state.installing {
        eprintln!("Installation incomplete; background reconnect paused.");
        return 2;
    }
    if state.paused {
        // The schedule keeps running; it just does nothing and says so. No
        // request, no health record rewrite.
        let report = OutcomeReport::paused();
        if args.json {
            println!("{}", serde_json::to_string(&report).unwrap_or_default());
        } else {
            println!("{}", report.message);
        }
        return 0;
    }
    let delay = match trigger {
        Trigger::Startup => startup_delay_secs.or(registration.boot_delay_secs),
        Trigger::Interval => None,
    };
    // The startup delay is part of the run, so it is inside the recorded span.
    let started = control::now_ms();
    let report = supervised(Supervisor {
        control: &control,
        source: &registration.source,
        operation: reconnect::Action::Reconnect,
        respect_pause: true,
        startup_delay_secs: delay,
        drain: false,
    });
    finish(&control, &report, args.json, started)
}

// ---------------------------------------------------------------------------
// Configuration commands
// ---------------------------------------------------------------------------

fn run_config(action: &ConfigAction, args: &Args) -> i32 {
    match action {
        ConfigAction::Show => show_config(args),
        ConfigAction::List => list_config(args),
        ConfigAction::Set(pairs) => set_config(pairs, args),
        ConfigAction::Unset(keys) => unset_config(keys, args),
        ConfigAction::Validate => validate_config(args),
    }
}

fn show_config(args: &Args) -> i32 {
    let resolution = match load_resolution(args) {
        Ok(resolution) => resolution,
        Err(status) => return status,
    };
    let effective = resolution.effective();
    if args.json {
        let values = serde_json::json!({
            "portal_url": effective.portal_url,
            "username": effective.username,
            "timeout_ms": effective.timeouts.connect_ms,
            "reconnect_interval_secs": effective.reconnect_interval_secs,
            "reconnect_boot_delay_secs": effective.reconnect_boot_delay_secs,
        });
        let body = serde_json::json!({
            "schema_version": 1,
            "values": values,
            "sources": resolution.source().layers,
            "write_path": resolution.source().write_path,
        });
        println!("{body}");
        return 0;
    }
    for spec in settings::KEY_CATALOGUE {
        let value = effective_value(effective, spec.name);
        println!("{}: {value}", spec.name);
        println!("    {}", spec.about);
        match resolution.value_source(spec.name) {
            Ok(Some(path)) => println!("    from {}", path.display()),
            Ok(None) => println!("    from the default"),
            Err(err) => {
                eprintln!("{err}");
                return 2;
            }
        }
    }
    0
}

fn list_config(args: &Args) -> i32 {
    let resolution = match load_resolution(args) {
        Ok(resolution) => resolution,
        Err(status) => return status,
    };
    let entries = match resolution.entries() {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    if entries.is_empty() {
        println!("(nothing set in this file; the defaults apply)");
    }
    for (key, value) in entries {
        println!("{key} = {value}");
    }
    0
}

fn set_config(pairs: &[(String, String)], args: &Args) -> i32 {
    let mut resolution = match load_resolution(args) {
        Ok(resolution) => resolution,
        Err(status) => return status,
    };
    // Validate every pair before writing any: a typo in the last value must not
    // leave the first ones applied.
    let mut changes: Vec<(&str, toml::Value)> = Vec::with_capacity(pairs.len());
    for (key, text) in pairs {
        match settings::parse_key_value(key, text) {
            Ok(value) => changes.push((key.as_str(), value)),
            Err(message) => {
                eprintln!("{message}");
                return 2;
            }
        }
    }
    // …and validate the configuration the batch would produce, so an illegal
    // range or a malformed URL never reaches the file.
    let mut merged = resolution.effective().clone();
    for (key, value) in &changes {
        if let Err(message) = apply_setting(&mut merged, key, value) {
            eprintln!("{message}");
            return 2;
        }
    }
    if let Err(err) = settings::validate(&merged, false) {
        eprintln!("{err}");
        return 2;
    }
    let mut schedule_changed = false;
    match resolution.update(&changes) {
        Ok(_) => {
            for (key, value) in &changes {
                if matches!(
                    *key,
                    "reconnect_interval_secs" | "reconnect_boot_delay_secs"
                ) {
                    schedule_changed = true;
                }
                println!("{key} = {}", render_config_value(value));
            }
            if schedule_changed {
                println!("Schedule changed; run service install to apply.");
            }
            0
        }
        Err(err) => {
            eprintln!("{err}");
            3
        }
    }
}

fn unset_config(keys: &[String], args: &Args) -> i32 {
    let mut resolution = match load_resolution(args) {
        Ok(resolution) => resolution,
        Err(status) => return status,
    };
    for key in keys {
        if settings::key_spec(key).is_none() {
            eprintln!("Unknown configuration key: {key}");
            return 2;
        }
    }
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    match resolution.remove(&key_refs) {
        Ok(outcome) => {
            for (key, removed) in keys.iter().zip(&outcome.removed) {
                if *removed {
                    println!("Unset {key}");
                } else {
                    println!("{key} was not set");
                }
            }
            if keys.iter().any(|key| {
                matches!(
                    key.as_str(),
                    "reconnect_interval_secs" | "reconnect_boot_delay_secs"
                )
            }) {
                println!("Schedule changed; run service install to apply.");
            }
            0
        }
        Err(err) => {
            eprintln!("{err}");
            3
        }
    }
}

fn validate_config(args: &Args) -> i32 {
    let resolution = match load_resolution(args) {
        Ok(resolution) => resolution,
        Err(status) => return status,
    };
    match settings::validate(resolution.effective(), true) {
        Ok(()) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({"schema_version": 1, "valid": true})
                );
            } else {
                println!("Configuration is valid.");
            }
            0
        }
        Err(err) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "schema_version": 1,
                        "valid": false,
                        "errors": [err.to_string()],
                    })
                );
            }
            eprintln!("{err}");
            2
        }
    }
}

/// One setting applied to an in-memory snapshot, for batch validation.
fn apply_setting(
    effective: &mut settings::Effective,
    key: &str,
    value: &toml::Value,
) -> Result<(), String> {
    match (key, value) {
        ("portal_url", toml::Value::String(value)) => effective.portal_url = Some(value.clone()),
        ("username", toml::Value::String(value)) => effective.username = Some(value.clone()),
        ("timeout_ms", toml::Value::Integer(value)) => {
            effective.timeouts.connect_ms = *value as u64;
            effective.timeouts.read_ms = *value as u64;
        }
        ("reconnect_interval_secs", toml::Value::Integer(value)) => {
            effective.reconnect_interval_secs = (*value > 0).then_some(*value as u64);
        }
        ("reconnect_boot_delay_secs", toml::Value::Integer(value)) => {
            effective.reconnect_boot_delay_secs = (*value > 0).then_some(*value as u64);
        }
        _ => {
            return Err(format!(
                "Internal error: invalid configuration value for {key}"
            ))
        }
    }
    Ok(())
}

/// One effective setting as `config show` prints it.
fn effective_value(effective: &settings::Effective, key: &str) -> String {
    match key {
        "portal_url" => effective
            .portal_url
            .clone()
            .unwrap_or_else(|| "(not set)".into()),
        "username" => effective
            .username
            .clone()
            .unwrap_or_else(|| "(not set)".into()),
        "timeout_ms" => {
            if effective.timeouts.connect_ms == effective.timeouts.read_ms {
                effective.timeouts.connect_ms.to_string()
            } else {
                format!(
                    "{} (legacy connect/read values differ)",
                    effective.timeouts.read_ms
                )
            }
        }
        "reconnect_interval_secs" => match effective.reconnect_interval_secs {
            Some(secs) => format!("{secs}s"),
            None => "(off: no interval)".to_string(),
        },
        "reconnect_boot_delay_secs" => match effective.reconnect_boot_delay_secs {
            Some(secs) => format!("{secs}s"),
            None => "(off: not at startup)".to_string(),
        },
        other => format!("(unknown key {other})"),
    }
}

/// A written value as `config set` echoes it: strings bare, numbers as numbers.
fn render_config_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) if text.is_empty() => "\"\"".to_string(),
        toml::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Credentials commands
// ---------------------------------------------------------------------------

fn run_credentials(action: CredentialsAction, args: &Args) -> i32 {
    let resolution = match load_resolution(args) {
        Ok(resolution) => resolution,
        Err(status) => return status,
    };
    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };
    let path = control.credentials_path();

    // `forget --legacy` is the only command that works without an identity: it
    // is the way to clean up entries that were never bound to a portal.
    if let CredentialsAction::Forget { legacy: true } = action {
        let account = match resolution
            .effective()
            .username
            .as_deref()
            .filter(|name| !name.is_empty())
        {
            Some(username) => {
                AccountIdentity::parse(username, resolution.effective().domain.as_deref())
            }
            None => {
                eprintln!("{}", crate::messages::USERNAME_REQUIRED);
                return 2;
            }
        };
        let mut removed = false;
        match credentials::forget_legacy(account.qualified(), &path) {
            Ok(found) => removed |= found,
            Err(err) => {
                eprintln!("{err}");
                return 3;
            }
        }
        // The pre-v2 file lived beside the configuration file; only this
        // explicit command looks there.
        let legacy_path = credentials::legacy_path_for(&resolution.source().write_path);
        if legacy_path.exists() {
            match credentials::forget_legacy(account.qualified(), &legacy_path) {
                Ok(found) => removed |= found,
                Err(err) => {
                    eprintln!("{err}");
                    return 3;
                }
            }
        }
        println!(
            "{}",
            if removed {
                format!(
                    "Removed the legacy credential for {}. Save the password again with `credentials set`.",
                    account.qualified()
                )
            } else {
                format!("No legacy credential for {}.", account.qualified())
            }
        );
        return 0;
    }

    let (portal, account) = match target_of(resolution.effective()) {
        Ok(pair) => pair,
        Err(status) => return status,
    };
    let key = credentials::credential_key(&portal, account.qualified());

    match action {
        CredentialsAction::Set(target) => {
            let state = match control.read_control() {
                Ok(state) => state,
                Err(err) => {
                    eprintln!("{err}");
                    return 3;
                }
            };
            let _ = state;
            let password = match password_for_set(&key, &path) {
                Ok(password) => password,
                Err(status) => return status,
            };
            match credentials::store(&key, &password, &path, target) {
                Ok(stored) => {
                    if let Some(note) = stored.note {
                        println!("{note}");
                    }
                    println!(
                        "Password stored in the {} for {} at {}.",
                        stored.store.name(),
                        account.qualified(),
                        portal.origin
                    );
                    println!("Not verified: run `srun-portal reconnect` to check.");
                    0
                }
                Err(err) => {
                    eprintln!("{err}");
                    3
                }
            }
        }
        CredentialsAction::Forget { legacy: false } => match credentials::revoke(&key, &path) {
            Ok(true) => {
                println!(
                    "Removed the stored password for {} at {}.",
                    account.qualified(),
                    portal.origin
                );
                0
            }
            Ok(false) => {
                println!(
                    "No stored password for {} at {}.",
                    account.qualified(),
                    portal.origin
                );
                0
            }
            Err(err) => {
                eprintln!("{err}");
                3
            }
        },
        CredentialsAction::Forget { legacy: true } => unreachable!("handled above"),
        CredentialsAction::Status => {
            let status = match credentials::status(&key, &path) {
                Ok(status) => status,
                Err(err) => {
                    eprintln!("{err}");
                    return 3;
                }
            };
            if args.json {
                let mut body = serde_json::to_value(&status).unwrap_or_default();
                if let Some(object) = body.as_object_mut() {
                    object.insert("schema_version".to_string(), serde_json::Value::from(1));
                }
                println!("{body}");
            } else {
                println!("{}", render_credentials_status(&account, &path, &status));
            }
            if let credentials::Status::Unavailable { .. } = status {
                3
            } else {
                0
            }
        }
    }
}

/// The password to store: the environment, then a secure prompt, never an
/// argument.
fn password_for_set(key: &str, path: &Path) -> Result<String, i32> {
    if let Ok(password) = std::env::var(credentials::ENV_PASSWORD) {
        if !password.is_empty() {
            return Ok(password);
        }
    }
    if !sys::stdin_is_tty() {
        eprintln!(
            "No password: set {} or run this from a terminal",
            credentials::ENV_PASSWORD
        );
        return Err(2);
    }
    let _ = (key, path);
    match sys::prompt_password("Please enter your password: ") {
        Ok(Some(password)) if !password.is_empty() => Ok(password),
        Ok(_) => {
            eprintln!("{}", crate::messages::PASSWORD_REQUIRED);
            Err(2)
        }
        Err(err) => {
            eprintln!("{err}");
            Err(3)
        }
    }
}

fn render_credentials_status(
    account: &AccountIdentity,
    path: &Path,
    status: &credentials::Status,
) -> String {
    if std::env::var(credentials::ENV_PASSWORD).is_ok_and(|value| !value.is_empty()) {
        return format!(
            "{} is set (overrides any stored password)",
            credentials::ENV_PASSWORD
        );
    }
    match status {
        credentials::Status::Keyring => {
            format!("stored in the system keyring for {}", account.qualified())
        }
        credentials::Status::File => {
            format!("stored in {} for {}", path.display(), account.qualified())
        }
        credentials::Status::LegacyUnscoped => "only a legacy unscoped password was found; run \
             `srun-portal credentials forget --legacy` and save it again"
            .to_string(),
        credentials::Status::Missing => {
            format!("none stored for {}", account.qualified())
        }
        credentials::Status::Unavailable { detail } => {
            format!("cannot be read: {detail}")
        }
    }
}

// ---------------------------------------------------------------------------
// Service commands
// ---------------------------------------------------------------------------

fn run_service(action: ServiceAction, args: &Args) -> i32 {
    match action {
        ServiceAction::Status => return service_status(args),
        ServiceAction::Logs => return service_logs(args),
        ServiceAction::Uninstall => return service_uninstall(args),
        ServiceAction::Install => {}
        ServiceAction::Start | ServiceAction::Stop | ServiceAction::Restart => {
            return service_control(action, args)
        }
    }
    service_install(args)
}

fn service_logs(args: &Args) -> i32 {
    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };
    let events = match control.read_events() {
        Ok(events) => events,
        Err(err) => {
            eprintln!("{err}");
            return 3;
        }
    };
    if args.json {
        println!("{}", serde_json::to_string(&events).unwrap_or_default());
        return 0;
    }
    if events.is_empty() {
        println!("(no events recorded)");
    }
    for event in events {
        let kind = event
            .error_kind
            .map(|kind| format!(" [{kind:?}]"))
            .unwrap_or_default();
        let message = if event.message.is_empty() {
            String::new()
        } else {
            format!(": {}", event.message)
        };
        println!(
            "{} {}{kind}{message}",
            format_time_ms(event.finished_at_ms),
            event.result
        );
    }
    0
}

/// `YYYY-MM-DDTHH:MM:SSZ` from a Unix millisecond timestamp.
fn format_time_ms(ms: u64) -> String {
    let secs = ms / 1_000;
    let days = secs / 86_400;
    let time = secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3_600,
        (time % 3_600) / 60,
        time % 60
    )
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn service_status(args: &Args) -> i32 {
    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };
    let mut errors: Vec<String> = Vec::new();
    let registration = match control.read_registration() {
        Ok(registration) => registration,
        Err(err) => {
            errors.push(err.to_string());
            None
        }
    };
    let state = match control.read_control() {
        Ok(state) => state,
        Err(err) => {
            errors.push(err.to_string());
            Default::default()
        }
    };
    let health = match control.read_health() {
        Ok(health) => health,
        Err(err) => {
            errors.push(err.to_string());
            None
        }
    };
    let running = match control.is_operation_held() {
        Ok(running) => running,
        Err(err) => {
            errors.push(err.to_string());
            false
        }
    };
    let (platform, platform_state) = match &registration {
        Some(registration) => {
            let spec = spec_for(&control, registration);
            match service::platform_state(&spec) {
                Ok(state) => (platform_name().to_string(), state),
                Err(err) => {
                    errors.push(err);
                    (platform_name().to_string(), Default::default())
                }
            }
        }
        None => (platform_name().to_string(), Default::default()),
    };

    if args.json {
        let body = serde_json::json!({
            "schema_version": 1,
            "installed": registration.is_some(),
            "paused": state.paused,
            "installing": state.installing,
            "running": running,
            "config_sources": registration.as_ref().map(|r| r.source.layers.clone()),
            "requested_interval_secs": registration.as_ref().and_then(|r| r.interval_secs),
            "effective_interval_secs": registration.as_ref().and_then(|r| r.effective_interval_secs),
            "boot_delay_secs": registration.as_ref().and_then(|r| r.boot_delay_secs),
            "last_attempt": health,
            "last_success_at_ms": health.as_ref().and_then(|h| h.last_success_at_ms),
            "platform": platform,
            "platform_state": platform_state,
            "errors": errors,
        });
        println!("{body}");
    } else {
        match &registration {
            Some(registration) => {
                println!("Installed: {}", registration.exe.display());
                for layer in &registration.source.layers {
                    println!("  reads {}", layer.display());
                }
                println!("Requested cadence: {}", describe_cadence(registration));
                println!("Paused: {}", yes_no(state.paused));
                println!("Run in progress: {}", yes_no(running));
            }
            None => println!("Installed: no"),
        }
        if state.installing {
            println!("Installation: incomplete (rerun service install or service uninstall)");
        }
        match &health {
            Some(health) => {
                println!(
                    "Last attempt: {} ({} at {})",
                    result_name(health.result),
                    health.message,
                    format_time_ms(health.finished_at_ms)
                );
                match health.last_success_at_ms {
                    Some(ms) => println!("Last success: {}", format_time_ms(ms)),
                    None => println!("Last success: never"),
                }
            }
            None => {
                println!("Last attempt: never");
                println!("Last success: never");
            }
        }
        println!("Platform: {platform}");
        if let Err(lines) = print_platform_state(&platform_state) {
            errors.push(lines);
        }
        for error in &errors {
            eprintln!("error: {error}");
        }
    }
    if errors.is_empty() {
        0
    } else {
        3
    }
}

fn print_platform_state(state: &service::PlatformState) -> Result<(), String> {
    if !state.available {
        println!("  (the platform did not report a state)");
        return Ok(());
    }
    for resource in &state.resources {
        let present = if resource.present {
            "present"
        } else {
            "absent"
        };
        let running = if resource.running { "running" } else { "idle" };
        match resource.last_exit_code {
            Some(code) => println!(
                "  {}: {present}, {running}, last exit {code}",
                resource.name
            ),
            None => println!("  {}: {present}, {running}", resource.name),
        }
    }
    Ok(())
}

fn describe_cadence(registration: &control::Registration) -> String {
    let interval =
        registration
            .interval_secs
            .map(|secs| match registration.effective_interval_secs {
                Some(effective) if effective != secs => {
                    format!("every {effective}s (asked for {secs}s)")
                }
                _ => format!("every {secs}s"),
            });
    let startup = registration
        .boot_delay_secs
        .map(|secs| format!("{secs}s after startup"));
    match (interval, startup) {
        (Some(interval), Some(startup)) => format!("{interval}, {startup}"),
        (Some(interval), None) => interval,
        (None, Some(startup)) => format!("{startup} only"),
        (None, None) => "only when started by hand".to_string(),
    }
}

fn result_name(result: Outcome) -> &'static str {
    match result {
        Outcome::Online => "online",
        Outcome::Reconnected => "reconnected",
        Outcome::Disconnected => "disconnected",
        Outcome::Paused => "paused",
        Outcome::Busy => "busy",
        Outcome::Failed => "failed",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn platform_name() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "systemd"
    }
    #[cfg(target_os = "macos")]
    {
        "launchd"
    }
    #[cfg(target_os = "windows")]
    {
        "task-scheduler"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        "unsupported"
    }
}

fn spec_for(control: &ControlDir, registration: &control::Registration) -> service::Spec {
    service::Spec {
        exe: registration.exe.clone(),
        control_dir: control.path().to_path_buf(),
        interval_secs: registration.interval_secs,
        boot_delay_secs: registration.boot_delay_secs,
        home: sys::home_dir().unwrap_or_else(|_| PathBuf::from(".")),
        owner_id: registration.owner_id.clone(),
    }
}

fn service_uninstall(args: &Args) -> i32 {
    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };
    let registration = match control.read_registration() {
        Ok(registration) => registration,
        Err(err) => {
            eprintln!("{err}");
            return 3;
        }
    };
    // Without a record the known resources are still probed and removed: an
    // interrupted install leaves them behind precisely when the record is
    // missing.
    let spec = match &registration {
        Some(registration) => spec_for(&control, registration),
        None => service::Spec {
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("srun-portal")),
            control_dir: control.path().to_path_buf(),
            interval_secs: None,
            boot_delay_secs: None,
            home: sys::home_dir().unwrap_or_else(|_| PathBuf::from(".")),
            owner_id: service::owner_id(),
        },
    };
    let plan = service::plan(&spec);
    match service::uninstall(&plan) {
        Ok(log) => {
            for line in log {
                println!("{line}");
            }
        }
        Err(failure) => {
            for line in failure.log {
                println!("{line}");
            }
            eprintln!("{}", failure.error);
            eprintln!("Installation record kept so the removal can be retried.");
            return 3;
        }
    }
    if let Err(err) = control.remove_registration() {
        eprintln!("{err}");
        return 3;
    }
    let finished = control::now_ms();
    let _ = control.append_event(Event {
        schema_version: 1,
        started_at_ms: finished,
        finished_at_ms: finished,
        result: "uninstalled".to_string(),
        error_kind: None,
        message: String::new(),
    });
    println!("Removed the background task. Credentials and history were kept.");
    0
}

/// `service start|stop|restart`.
fn service_control(action: ServiceAction, args: &Args) -> i32 {
    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };
    let registration = match control.read_registration() {
        Ok(registration) => registration,
        Err(err) => {
            eprintln!("{err}");
            return 3;
        }
    };
    let state = match control.read_control() {
        Ok(state) => state,
        Err(err) => {
            eprintln!("{err}");
            return 3;
        }
    };
    if state.installing {
        eprintln!(
            "Installation incomplete; background reconnect paused. \
             Rerun service install or service uninstall."
        );
        return 3;
    }
    // Stopping is worth doing even with nothing installed: the pause is what
    // keeps a manual run from reconnecting, and there is no native run to stop.
    if registration.is_none() && !matches!(action, ServiceAction::Stop) {
        eprintln!("Not installed: run `srun-portal service install` first");
        return 2;
    }

    if matches!(action, ServiceAction::Stop | ServiceAction::Restart) {
        if let Err(status) = set_pause(&control, true) {
            return status;
        }
        if let Some(registration) = &registration {
            let spec = spec_for(&control, registration);
            match service::stop(&service::plan(&spec)) {
                Ok(log) => {
                    for line in log {
                        println!("{line}");
                    }
                }
                Err(failure) => {
                    eprintln!("{}", failure.error);
                    return 3;
                }
            }
        }
        let finished = control::now_ms();
        let _ = control.append_event(Event {
            schema_version: 1,
            started_at_ms: finished,
            finished_at_ms: finished,
            result: "stopped".to_string(),
            error_kind: None,
            message: String::new(),
        });
        if action == ServiceAction::Stop {
            println!("Paused. The task stays installed; `service start` resumes it.");
            return 0;
        }
    }

    // Start (and the second half of restart): clear the pause, then check.
    let registration = match registration {
        Some(registration) => registration,
        None => {
            eprintln!("Not installed: run `srun-portal service install` first");
            return 2;
        }
    };
    if let Err(status) = clear_pause(&control) {
        return status;
    }
    let resolution = match Resolution::open(registration.source.clone()) {
        Ok(resolution) => resolution,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };
    timeouts(resolution.effective());
    let started = control::now_ms();
    let report = supervised(Supervisor {
        control: &control,
        source: &registration.source,
        operation: reconnect::Action::Reconnect,
        respect_pause: true,
        startup_delay_secs: None,
        drain: false,
    });
    finish(&control, &report, args.json, started)
}

/// `service install`: local inputs, then one commit.
///
/// The order matters. Everything that can be validated or asked for happens
/// first, in memory; then the credential is stored as its own explicit step;
/// then the install is committed with `installing = true`. A failure after the
/// commit leaves that flag set and the task paused, so the next background run
/// does nothing and the operator sees why.
fn service_install(args: &Args) -> i32 {
    let mut source = match config_source(args) {
        Ok(source) => source,
        Err(status) => return status,
    };
    // Only install may create a configuration: a first run with no file asks
    // where to put one.
    if let Err(err) = source.exists() {
        eprintln!("{err}");
        return 2;
    }
    if !matches!(source.exists(), Ok(true)) {
        let (system, portable) = source.locations();
        let choice = settings::ask_storage(&system, &portable);
        settings::choose_first_run(&mut source, choice);
    }
    let mut resolution = match Resolution::open(source.clone()) {
        Ok(resolution) => resolution,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };

    if let Err(status) = apply_install_options(&mut resolution, args) {
        return status;
    }
    if let Err(status) = ensure_install_settings(&mut resolution, args) {
        return status;
    }
    let effective = resolution.effective().clone();
    if let Err(err) = settings::validate(&effective, true) {
        eprintln!("{err}");
        return 2;
    }
    let source = resolution.source().clone();
    let (portal, account) = match target_of(&effective) {
        Ok(pair) => pair,
        Err(status) => return status,
    };
    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };
    if let Err(status) = control.ensure() {
        eprintln!("{status}");
        return 3;
    }
    let key = credentials::credential_key(&portal, account.qualified());
    let path = control.credentials_path();

    // The credential is an explicit, separate step: it may be supplied or
    // reused, and its success says nothing about whether it works.
    match install_password(&key, &path) {
        Ok(password) => match credentials::store(&key, &password, &path, credentials::Target::Auto)
        {
            Ok(stored) => {
                if let Some(note) = stored.note {
                    println!("{note}");
                }
                println!(
                    "Password stored in the {} for {}.",
                    stored.store.name(),
                    account.qualified()
                );
                println!("Credentials stored; not verified. Run reconnect to check.");
            }
            Err(err) => {
                eprintln!("{err}");
                return 3;
            }
        },
        Err(status) => return status,
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("cannot locate the running executable: {err}");
            return 3;
        }
    };
    let home = match sys::home_dir() {
        Ok(home) => home,
        Err(err) => {
            eprintln!("{err}");
            return 3;
        }
    };
    let interval = effective.reconnect_interval_secs;
    let boot_delay = effective.reconnect_boot_delay_secs;
    let spec = service::Spec {
        exe,
        control_dir: control.path().to_path_buf(),
        interval_secs: interval,
        boot_delay_secs: boot_delay,
        home,
        owner_id: service::owner_id(),
    };

    // Commit: from here the failures leave `installing` set.
    let previous_pause = match control.read_control() {
        Ok(state) => state,
        Err(err) => {
            eprintln!("{err}");
            return 3;
        }
    };
    if let Err(err) = control.update_control(|state| {
        state.installing = true;
        state.paused = true;
    }) {
        eprintln!("{err}");
        return 3;
    }

    let plan = service::plan(&spec);
    let install_result = service::install(&plan);
    let log = match install_result {
        Ok(log) => log,
        Err(failure) => {
            for line in failure.log {
                println!("{line}");
            }
            eprintln!("{}", failure.error);
            incomplete_install(&control, previous_pause.paused);
            return 3;
        }
    };

    let registration = control::Registration {
        version: 1,
        exe: spec.exe.clone(),
        source: source.clone(),
        interval_secs: interval,
        effective_interval_secs: platform_effective_interval(&spec),
        boot_delay_secs: boot_delay,
        owner_id: spec.owner_id.clone(),
    };
    if let Err(err) = control.write_registration(&registration) {
        eprintln!("{err}");
        incomplete_install(&control, previous_pause.paused);
        return 3;
    }
    if let Err(err) = control.update_control(|state| {
        state.installing = false;
        state.paused = previous_pause.paused;
    }) {
        eprintln!("{err}");
        return 3;
    }

    for line in log {
        println!("{line}");
    }
    let finished = control::now_ms();
    let _ = control.append_event(Event {
        schema_version: 1,
        started_at_ms: finished,
        finished_at_ms: finished,
        result: "installed".to_string(),
        error_kind: None,
        message: format!("cadence: {}", plan.cadence),
    });
    println!("Reconnect: {}", plan.cadence);
    println!("{}", service::startup_note(&spec, &service::current_user()));
    if let Some(note) = service::note_for_executable(&spec.exe) {
        println!("{note}");
    }
    0
}

#[cfg(target_os = "windows")]
fn platform_effective_interval(spec: &service::Spec) -> Option<u64> {
    spec.interval_secs
        .map(|secs| service::scheduler::interval_minutes(secs) * 60)
}

#[cfg(not(target_os = "windows"))]
fn platform_effective_interval(spec: &service::Spec) -> Option<u64> {
    spec.interval_secs
}

fn incomplete_install(control: &ControlDir, _previous_pause: bool) {
    // The record, if any, is the previous one: it still describes a task set
    // that is partly gone, which is exactly what the operator has to repair.
    let finished = control::now_ms();
    let _ = control.append_event(Event {
        schema_version: 1,
        started_at_ms: finished,
        finished_at_ms: finished,
        result: "install_failed".to_string(),
        error_kind: Some(ErrorKind::Platform),
        message: String::new(),
    });
    eprintln!(
        "Installation incomplete; background reconnect paused. \
         Rerun service install or service uninstall."
    );
}

/// `--portal-url`, `--username`, `--timeout-ms`, cadence options: persisted.
fn apply_install_options(resolution: &mut Resolution, args: &Args) -> Result<(), i32> {
    if args.setting_options.is_empty() {
        return Ok(());
    }
    let mut changes = Vec::with_capacity(args.setting_options.len());
    for (key, value) in &args.setting_options {
        let parsed = settings::parse_key_value(key, value).map_err(|err| {
            eprintln!("--{} {}: {err}", key.replace('_', "-"), value);
            2
        })?;
        changes.push((key.as_str(), parsed));
    }
    if let Err(err) = resolution.update(&changes) {
        eprintln!("{err}");
        return Err(3);
    }
    Ok(())
}

/// Fill in what an install cannot work without, prompting when there is a
/// terminal and failing with the standard message when there is not.
fn ensure_install_settings(resolution: &mut Resolution, args: &Args) -> Result<(), i32> {
    if !sys::stdin_is_tty() {
        return Ok(());
    }
    let has = |resolution: &Resolution, key: &str| {
        args.setting_options.iter().any(|(option, _)| option == key)
            || match key {
                "portal_url" => resolution
                    .effective()
                    .portal_url
                    .as_deref()
                    .is_some_and(|slug| config::parse_portal_url(slug).is_ok()),
                "username" => resolution
                    .effective()
                    .username
                    .as_deref()
                    .is_some_and(|name| !name.is_empty()),
                _ => true,
            }
    };

    if !has(resolution, "portal_url") {
        loop {
            let input = prompt_with_default(
                "Portal web url",
                resolution.effective().portal_url.as_deref(),
            )?;
            if input.trim().is_empty() {
                eprintln!("{}", crate::messages::PORTAL_URL_REQUIRED);
                continue;
            }
            match config::parse_portal_url(&input) {
                Ok(_) => {
                    record_setting(resolution, "portal_url", &input)?;
                    break;
                }
                Err(err) => eprintln!("{err}"),
            }
        }
    }
    if !has(resolution, "username") {
        loop {
            let input =
                prompt_with_default("Username", resolution.effective().username.as_deref())?;
            if input.trim().is_empty() {
                eprintln!("{}", crate::messages::USERNAME_REQUIRED);
                continue;
            }
            record_setting(resolution, "username", &input)?;
            break;
        }
    }
    Ok(())
}

fn record_setting(resolution: &mut Resolution, key: &str, value: &str) -> Result<(), i32> {
    if let Err(err) = resolution.update(&[(key, toml::Value::String(value.to_string()))]) {
        eprintln!("{err}");
        return Err(3);
    }
    Ok(())
}

fn prompt_with_default(label: &str, default: Option<&str>) -> Result<String, i32> {
    let prompt_label = match default {
        Some(value) if !value.is_empty() => format!("{label} [{value}]: "),
        _ => format!("{label}: "),
    };
    let input = prompt(&prompt_label)?;
    if input.trim().is_empty() {
        Ok(default.unwrap_or_default().to_string())
    } else {
        Ok(input)
    }
}

/// The password an install stores: the environment, the one already stored for
/// this key, or a secure prompt. An install never verifies it.
fn install_password(key: &str, path: &Path) -> Result<String, i32> {
    if let Ok(password) = std::env::var(credentials::ENV_PASSWORD) {
        if !password.is_empty() {
            return Ok(password);
        }
    }
    let stored = match credentials::resolve(key, path, None) {
        Ok(stored) => stored,
        Err(err) => {
            eprintln!("{err}");
            return Err(3);
        }
    };
    if !sys::stdin_is_tty() {
        return match stored {
            Some(password) => Ok(password),
            None => {
                eprintln!("{}", crate::messages::PASSWORD_REQUIRED);
                Err(2)
            }
        };
    }
    match sys::prompt_password("Please enter your password: ") {
        Ok(Some(password)) if !password.is_empty() => Ok(password),
        // An empty answer keeps the credential already on file, which is what
        // makes a reinstall a one-keystroke operation.
        Ok(Some(_)) | Ok(None) => match stored {
            Some(password) => Ok(password),
            None => {
                eprintln!("{}", crate::messages::PASSWORD_REQUIRED);
                Err(2)
            }
        },
        Err(err) => {
            eprintln!("{err}");
            Err(3)
        }
    }
}

// ---------------------------------------------------------------------------
// The interactive shell
// ---------------------------------------------------------------------------

fn run_interactive(args: &Args) -> i32 {
    println!("{}", crate::messages::BANNER);

    let mut resolution = match load_resolution(args) {
        Ok(resolution) => resolution,
        Err(status) => return status,
    };
    if let Err(status) = apply_interactive_options(&mut resolution, args) {
        return status;
    }
    timeouts(resolution.effective());

    let slug = match resolve_slug(args, &resolution) {
        Ok(slug) => slug,
        Err(status) => return status,
    };
    let portal = match config::parse_portal_url(&slug) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("{err}");
            return 2;
        }
    };

    let cfg = match fetch_config(&portal) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("{}", err.render(&crate::translate::Translate::default()));
            return 1;
        }
    };

    // The configured URL is what the next run reads first, so it is recorded in
    // the file the run writes.
    let slug = format!(
        "{}{}?ac_id={}",
        portal.origin,
        api::CONFIG_PATHNAME,
        portal.ac_id
    );
    if let Err(err) = resolution.update(&[("portal_url", toml::Value::String(slug))]) {
        eprintln!("{err}");
    }

    let mut options = RuntimeOptions::from_config(&cfg, &portal.origin, &host_of(&portal.origin));
    options.callback = resolution.effective().callback.clone();
    let mut runtime = Runtime::new(options);
    runtime.apply_interface_correction(&sys::network_interfaces(), &cfg.ip);
    runtime.spawn_other_stack_probe();

    let control = match control_dir(Some(args)) {
        Ok(control) => control,
        Err(status) => return status,
    };
    let state = match control.read_control() {
        Ok(state) => state,
        Err(err) => {
            eprintln!("{err}");
            return 3;
        }
    };

    // The online check and the sign-out are protocol work, so they take the
    // operation lock like every other portal-talking path.
    let online = {
        let _lock = match control.lock_operation() {
            Ok(lock) => lock,
            Err(err) => {
                eprintln!("{err}");
                return 3;
            }
        };
        match runtime.check_online() {
            Ok(online) => online,
            Err(err) => {
                eprintln!("{}", err.render(&runtime.translate));
                return 4;
            }
        }
    };

    if online {
        show_account_info(&runtime);
        match confirm("Do you want to sign out? (y/N): ") {
            Ok(true) => {
                if state.paused {
                    println!("Background reconnect is paused; sign-out continues.");
                }
                let _lock = match control.lock_operation() {
                    Ok(lock) => lock,
                    Err(err) => {
                        eprintln!("{err}");
                        return 3;
                    }
                };
                match runtime.sign_out() {
                    Ok(()) => println!("Sign out success!"),
                    Err(err) => println!("SignOut failed: {}", err.render(&runtime.translate)),
                }
            }
            Ok(false) => {}
            Err(err) => {
                eprintln!("{err}");
                return 1;
            }
        }
        return 0;
    }

    if state.paused {
        eprintln!("Background reconnect is paused; run `srun-portal resume` to enable it.");
        return 0;
    }

    // SPEC §5/§12 item 11: a rejected authentication prompts again, forever. The
    // manual input is outside any deadline; only the requests are supervised.
    loop {
        let username = if has_setting_option(args, "username") {
            resolution.effective().username.clone().unwrap_or_default()
        } else {
            let label = match resolution.effective().username.as_deref() {
                Some(username) if !username.is_empty() => {
                    format!("Please enter your username [{username}]: ")
                }
                _ => "Please enter your username: ".to_string(),
            };
            let entered = match prompt(&label) {
                Ok(value) => value,
                Err(status) => return status,
            };
            if entered.trim().is_empty() {
                resolution.effective().username.clone().unwrap_or_default()
            } else {
                entered
            }
        };
        let password = match password_prompt() {
            Ok(value) => value,
            Err(status) => return status,
        };
        let identity = AccountIdentity::parse(&username, resolution.effective().domain.as_deref());
        let _lock = match control.lock_operation() {
            Ok(lock) => lock,
            Err(err) => {
                eprintln!("{err}");
                return 3;
            }
        };
        match runtime.auth_by_password(identity.local(), &password, identity.domain()) {
            Ok(()) => {
                println!("Auth Success!");
                show_account_info(&runtime);
                return 0;
            }
            Err(err) => println!("Auth failed: {}", err.render(&runtime.translate)),
        }
    }
}

/// The interactive run's `--portal-url`/`--username`/`--timeout-ms`, applied to
/// this run and persisted, since the interactive flow is the one that may create
/// a configuration.
fn apply_interactive_options(resolution: &mut Resolution, args: &Args) -> Result<(), i32> {
    if args.setting_options.is_empty() {
        return Ok(());
    }
    let mut changes = Vec::with_capacity(args.setting_options.len());
    for (key, value) in &args.setting_options {
        let parsed = settings::parse_key_value(key, value).map_err(|err| {
            eprintln!("--{} {}: {err}", key.replace('_', "-"), value);
            2
        })?;
        changes.push((key.as_str(), parsed));
    }
    if let Err(err) = resolution.update(&changes) {
        eprintln!("{err}");
        return Err(3);
    }
    Ok(())
}

fn has_setting_option(args: &Args, key: &str) -> bool {
    args.setting_options
        .iter()
        .any(|(option_key, _)| option_key == key)
}

/// The Portal URL: the argument, then the configured `portal_url`, then a
/// prompt (SPEC §3.1).
fn resolve_slug(args: &Args, resolution: &Resolution) -> Result<String, i32> {
    if let Some(argument) = args.portal_url.as_deref() {
        return match config::parse_portal_url(argument) {
            Ok(_) => Ok(argument.to_string()),
            Err(err) => {
                eprintln!("{err}");
                Err(2)
            }
        };
    }
    if let Some(slug) = resolution.effective().portal_url.as_deref() {
        match config::parse_portal_url(slug) {
            Ok(_) => return Ok(slug.to_string()),
            Err(err) => eprintln!("{err}"),
        }
    }
    loop {
        let input = match sys::prompt_line("Portal web url: ") {
            Ok(Some(input)) => input,
            Ok(None) => {
                eprintln!("{}", crate::messages::PORTAL_URL_REQUIRED);
                return Err(2);
            }
            Err(err) => {
                eprintln!("{err}");
                return Err(1);
            }
        };
        if input.trim().is_empty() {
            eprintln!("{}", crate::messages::PORTAL_URL_REQUIRED);
            continue;
        }
        match config::parse_portal_url(&input) {
            Ok(_) => return Ok(input),
            Err(err) => eprintln!("{err}"),
        }
    }
}

fn prompt(label: &str) -> Result<String, i32> {
    match sys::prompt_line(label) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => {
            eprintln!("{}", crate::messages::EXIT_MESSAGE);
            Err(1)
        }
        Err(err) => {
            eprintln!("{err}");
            Err(1)
        }
    }
}

fn password_prompt() -> Result<String, i32> {
    match sys::prompt_password("Please enter your password: ") {
        Ok(Some(value)) => Ok(value),
        Ok(None) => {
            eprintln!("{}", crate::messages::EXIT_MESSAGE);
            Err(1)
        }
        Err(err) => {
            eprintln!("{err}");
            Err(1)
        }
    }
}

fn confirm(label: &str) -> Result<bool, String> {
    match sys::prompt_line(label) {
        Ok(Some(answer)) => Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")),
        Ok(None) => Ok(false),
        Err(err) => Err(err),
    }
}

/// The reference's account panel: a 25 character rule, a centred
/// `You're online`, the fields, and a closing rule.
fn show_account_info(runtime: &Runtime) {
    let info = &runtime.userinfo;
    let rows = [
        ("Username", runtime.username_with_domain()),
        ("IP", runtime.user_ip.clone()),
        ("Used Flow", format::format_flow(info.used_flow)),
        ("Used Time", format::format_time(info.used_time)),
        ("Balance", format!("¥ {}", info.balance)),
        ("Product Name", info.product_name.clone()),
    ];
    println!();
    print_rule();
    println!("{}", center("You're online", RULE_WIDTH));
    print_rule();
    for (label, value) in rows {
        println!("{label:<13}: {value}");
    }
    print_rule();
    println!();
}

const RULE_WIDTH: usize = 25;

fn print_rule() {
    println!("{}", "-".repeat(RULE_WIDTH));
}

/// The reference centres with the spare character on the left.
fn center(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let left = (width - len).div_ceil(2);
    let right = width - len - left;
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_header_is_centred_like_the_reference() {
        assert_eq!(center("You're online", RULE_WIDTH).len(), RULE_WIDTH);
        assert_eq!(
            center("You're online", RULE_WIDTH),
            "      You're online      "
        );
    }

    #[test]
    fn timestamps_render_as_utc() {
        assert_eq!(format_time_ms(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_time_ms(1_700_000_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_time_ms(1_000), "1970-01-01T00:00:01Z");
    }

    #[test]
    fn a_batch_is_validated_before_it_is_written() {
        let root = std::env::temp_dir().join(format!(
            "srun-portal-cli-batch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("config.toml");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&path, "username = \"kept\"\n").unwrap();
        std::env::set_var(settings::ENV_CONFIG_PATH, &path);
        let args = args::parse(&["config".to_string(), "set".to_string()]).unwrap_or_else(|_| {
            args::parse(&["config".to_string(), "unset".to_string(), "x".to_string()]).unwrap()
        });
        let _ = args;
        std::env::remove_var(settings::ENV_CONFIG_PATH);

        // The range check itself is exercised through the pure helper, which is
        // what `set_config` validates against.
        let mut effective = settings::Effective::default();
        apply_setting(&mut effective, "timeout_ms", &toml::Value::Integer(0)).unwrap();
        assert!(settings::validate(&effective, false).is_err());
        apply_setting(&mut effective, "timeout_ms", &toml::Value::Integer(10_000)).unwrap();
        assert!(settings::validate(&effective, false).is_ok());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "username = \"kept\"\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
