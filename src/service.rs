//! What "reconnect in the background" means on each system.
//!
//! Every platform gets its own native mechanism, and each one is expressed as
//! **data** by a pure constructor in a submodule ([`systemd`], [`launchd`],
//! [`scheduler`]) so the exact artifacts can be asserted on any host:
//!
//! * Linux — a systemd **user** service plus a timer
//!   (`~/.config/systemd/user/srun-portal.{service,timer}`),
//! * macOS — two LaunchAgents, one per trigger
//!   (`~/Library/LaunchAgents/com.srun-portal.reconnect.{interval,startup}.plist`),
//! * Windows — two Task Scheduler tasks, registered through PowerShell's
//!   `ScheduledTasks` module so nothing depends on the system language.
//!
//! A [`Plan`] separates the resources it **owns** (the full set of artifacts a
//! previous install may have left behind) from what it **does now**:
//!
//! * `prepare` takes the old trigger out and clears anything obsolete,
//! * `activate` enables the new definitions,
//! * `stop` stops a run in progress without touching the definitions,
//! * `remove` takes everything owned out, whatever the current cadence is,
//! * `query` reports the platform's own view, for a human.
//!
//! Removing a resource that is already gone is normal, so those commands carry
//! [`CommandSpec::when_present`] and are skipped when a [`Probe`] says the
//! resource does not exist. A probe that fails for any other reason is an
//! error: permission problems and an unreachable user manager are not "nothing
//! to clean up".
//!
//! The task itself runs this same executable as
//! `srun-portal __background --control-dir <dir> --trigger interval|startup`,
//! which re-reads the configuration every time and can be paused without
//! touching the native definitions.

use std::path::{Path, PathBuf};

use crate::process::{self, COMMAND_LIMIT};

/// Base name of the systemd unit.
pub const LABEL: &str = "srun-portal";

/// The macOS label of the interval agent.
pub const LAUNCHD_INTERVAL_LABEL: &str = "com.srun-portal.reconnect.interval";

/// The macOS label of the startup agent.
pub const LAUNCHD_STARTUP_LABEL: &str = "com.srun-portal.reconnect.startup";

/// What to install: the executable to run, the control directory it must act
/// on, the cadence the user configured, and who owns the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub exe: PathBuf,
    pub control_dir: PathBuf,
    pub interval_secs: Option<u64>,
    pub boot_delay_secs: Option<u64>,
    pub home: PathBuf,
    /// The real identity of this user: the uid on Unix, the token SID on
    /// Windows. Never taken from `$USER`.
    pub owner_id: String,
}

impl Spec {
    /// The arguments the native task passes to this executable.
    pub fn background_args(
        &self,
        trigger: Trigger,
        startup_delay_secs: Option<u64>,
    ) -> Vec<String> {
        let mut args = vec![
            "__background".to_string(),
            "--control-dir".to_string(),
            self.control_dir.to_string_lossy().into_owned(),
            "--trigger".to_string(),
            trigger.name().to_string(),
        ];
        if let Some(secs) = startup_delay_secs {
            args.push("--startup-delay-secs".to_string());
            args.push(secs.to_string());
        }
        args
    }

    /// The cadence as one line, for the installer to print.
    pub fn cadence(&self) -> String {
        let interval = self.interval_secs.map(|secs| format!("every {secs}s"));
        let startup = self
            .boot_delay_secs
            .map(|secs| format!("{secs}s after startup"));
        match (interval, startup) {
            (Some(interval), Some(startup)) => format!("{interval}, {startup}"),
            (Some(interval), None) => interval,
            (None, Some(startup)) => format!("{startup} only"),
            (None, None) => "only when started by hand".to_string(),
        }
    }

    /// What the platform can express: Task Scheduler counts whole minutes.
    pub fn effective_interval_secs(&self) -> Option<u64> {
        self.interval_secs
    }
}

/// Which native trigger a background run came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Interval,
    Startup,
}

impl Trigger {
    pub fn name(self) -> &'static str {
        match self {
            Trigger::Interval => "interval",
            Trigger::Startup => "startup",
        }
    }
}

/// One file the plan owns: the path and the exact bytes to put there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    pub path: PathBuf,
    pub body: String,
}

/// One command the plan runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub argv: Vec<String>,
    /// Standard input, for the platforms whose helper reads a request there.
    pub input: Option<String>,
    /// Skip this command when the named probe reports the resource absent.
    ///
    /// This is the only tolerated "already gone": a probe that fails for any
    /// other reason is an error, never a silent skip.
    pub when_present: Option<&'static str>,
}

impl CommandSpec {
    pub fn new(argv: Vec<String>) -> CommandSpec {
        CommandSpec {
            argv,
            input: None,
            when_present: None,
        }
    }

    fn only_when(mut self, resource: &'static str) -> CommandSpec {
        self.when_present = Some(resource);
        self
    }
}

/// A read-only native query that answers "is this resource there?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The name commands refer to with [`CommandSpec::when_present`].
    pub name: &'static str,
    pub argv: Vec<String>,
    /// How an absence shows up. Both variants are machine-readable: an exit
    /// code, or a value the platform prints as a stable token — never localized
    /// prose.
    pub absent: Absent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Absent {
    /// This exit status means "no such resource".
    ExitCode(i32),
    /// A line of stdout equal to this token means "no such resource".
    OutputLine(&'static str),
}

/// A complete description of the background task for one platform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub files: Vec<FileSpec>,
    pub prepare: Vec<CommandSpec>,
    pub activate: Vec<CommandSpec>,
    pub stop: Vec<CommandSpec>,
    pub remove: Vec<CommandSpec>,
    pub query: Vec<CommandSpec>,
    /// What to run after removing files, so the platform forgets them.
    pub reload: Vec<CommandSpec>,
    pub probes: Vec<Probe>,
    /// Every file a previous install of this plan family may have written.
    pub owned: Vec<PathBuf>,
    /// Every native task name a previous install may have registered.
    pub owned_tasks: Vec<String>,
    pub cadence: String,
}

impl Plan {
    /// The probe named `name`, if this plan has one.
    pub fn probe(&self, name: &str) -> Option<&Probe> {
        self.probes.iter().find(|probe| probe.name == name)
    }
}

/// What an install/uninstall attempt did before it stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub log: Vec<String>,
    pub error: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

/// The state of one native resource, as reported by the platform.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ResourceState {
    pub name: String,
    pub present: bool,
    pub running: bool,
    pub last_exit_code: Option<i32>,
}

/// The platform's own view of the task, for `service status --json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlatformState {
    pub available: bool,
    pub resources: Vec<ResourceState>,
    pub next_run_at_ms: Option<u64>,
}

/// The platform's plan. See the module docs for the three backends.
#[cfg(target_os = "linux")]
pub fn plan(spec: &Spec) -> Plan {
    systemd::plan(spec)
}

/// The platform's plan. See the module docs for the three backends.
#[cfg(target_os = "macos")]
pub fn plan(spec: &Spec) -> Plan {
    launchd::plan(spec)
}

/// The platform's plan. See the module docs for the three backends.
#[cfg(target_os = "windows")]
pub fn plan(spec: &Spec) -> Plan {
    scheduler::plan(spec)
}

/// The platform's plan. See the module docs for the three backends.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn plan(_spec: &Spec) -> Plan {
    Plan {
        files: Vec::new(),
        cadence: "unsupported on this system".to_string(),
        ..Plan::default()
    }
}

/// The probes' answers, keyed by resource name.
#[derive(Debug, Default)]
struct Presence {
    absent: Vec<&'static str>,
}

impl Presence {
    fn is_absent(&self, name: &str) -> bool {
        self.absent.contains(&name)
    }
}

fn probe_all(plan: &Plan, log: &mut Vec<String>) -> Result<Presence, Failure> {
    let mut presence = Presence::default();
    for probe in &plan.probes {
        let outcome = process::run(&probe.argv, None, COMMAND_LIMIT).map_err(|err| Failure {
            log: log.clone(),
            error: format!("cannot query {}: {err}", probe.name),
        })?;
        let absent = match (&probe.absent, &outcome.stop) {
            (Absent::ExitCode(code), process::Stop::Exited(status)) => status.code() == Some(*code),
            (Absent::ExitCode(_), _) => false,
            (Absent::OutputLine(token), _) => outcome
                .stdout_text()
                .lines()
                .any(|line| line.trim() == *token),
        };
        if absent {
            presence.absent.push(probe.name);
        } else if !outcome.succeeded() {
            // Present or not, the platform refused the query: report it rather
            // than treating an access problem as "nothing installed".
            return Err(Failure {
                log: log.clone(),
                error: format!(
                    "cannot query {}: {}: {}",
                    probe.name,
                    outcome.describe_stop(),
                    outcome.stderr_text()
                ),
            });
        } else {
            log.push(format!("Found {}", probe.name));
        }
    }
    Ok(presence)
}

fn run_commands(
    commands: &[CommandSpec],
    presence: &Presence,
    log: &mut Vec<String>,
) -> Result<(), Failure> {
    for command in commands {
        if let Some(resource) = command.when_present {
            if presence.is_absent(resource) {
                log.push(format!("Skipped (not present): {}", display(command)));
                continue;
            }
        }
        let input = command.input.as_deref().map(str::as_bytes);
        let outcome = process::run(&command.argv, input, COMMAND_LIMIT).map_err(|err| Failure {
            log: log.clone(),
            error: err.to_string(),
        })?;
        if !outcome.succeeded() {
            return Err(Failure {
                log: log.clone(),
                error: format!("{} failed: {}", display(command), outcome.describe_stop()),
            });
        }
        let stdout = outcome.stdout_text();
        log.push(if stdout.is_empty() {
            format!("Ran {}", display(command))
        } else {
            format!("Ran {}: {stdout}", display(command))
        });
    }
    Ok(())
}

/// A command as a log line, with the fixed PowerShell script replaced by its
/// action so the line stays readable and short.
fn display(command: &CommandSpec) -> String {
    command.argv.join(" ")
}

fn write_files(files: &[FileSpec], log: &mut Vec<String>) -> Result<(), Failure> {
    for file in files {
        crate::persistence::atomic_write(&file.path, file.body.as_bytes(), false).map_err(
            |err| Failure {
                log: log.clone(),
                error: format!("cannot write {}: {err}", file.path.display()),
            },
        )?;
        log.push(format!("Wrote {}", file.path.display()));
    }
    Ok(())
}

/// Create the plan's files and bring the native definitions up.
///
/// A file the plan owns but does not write is a leftover from an install with a
/// different cadence — a timer whose interval was switched off, say. It is
/// removed before the new definition is activated, because leaving it would keep
/// the old trigger firing behind the user's back.
pub fn install(plan: &Plan) -> Result<Vec<String>, Failure> {
    let mut log = Vec::new();
    let presence = probe_all(plan, &mut log)?;
    run_commands(&plan.prepare, &presence, &mut log)?;
    remove_stale_files(plan, &mut log)?;
    run_commands(&plan.reload, &presence, &mut log)?;
    write_files(&plan.files, &mut log)?;
    run_commands(&plan.activate, &presence, &mut log)?;
    Ok(log)
}

/// Delete every owned file this plan does not write.
fn remove_stale_files(plan: &Plan, log: &mut Vec<String>) -> Result<(), Failure> {
    for path in &plan.owned {
        if plan.files.iter().any(|file| file.path == *path) {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {
                log.push(format!("Removed obsolete {}", path.display()));
                prune_parent(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(Failure {
                    log: log.clone(),
                    error: format!("cannot remove {}: {err}", path.display()),
                })
            }
        }
    }
    Ok(())
}

/// Stop the definitions, remove the files the plan owns, and delete the native
/// tasks — all of them, whatever the current cadence is.
pub fn uninstall(plan: &Plan) -> Result<Vec<String>, Failure> {
    let mut log = Vec::new();
    let presence = probe_all(plan, &mut log)?;
    run_commands(&plan.stop, &presence, &mut log)?;
    run_commands(&plan.remove, &presence, &mut log)?;
    for path in &plan.owned {
        match std::fs::remove_file(path) {
            Ok(()) => log.push(format!("Removed {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                log.push(format!("Already gone: {}", path.display()))
            }
            Err(err) => {
                return Err(Failure {
                    log,
                    error: format!("cannot remove {}: {err}", path.display()),
                })
            }
        }
        prune_parent(path);
    }
    // Deleting unit files is not enough: the manager keeps them loaded until it
    // is told to look again. Other platforms have nothing to reload.
    run_commands(&plan.reload, &presence, &mut log)?;
    Ok(log)
}

/// Stop a run in progress without removing any definition.
pub fn stop(plan: &Plan) -> Result<Vec<String>, Failure> {
    let mut log = Vec::new();
    let presence = probe_all(plan, &mut log)?;
    run_commands(&plan.stop, &presence, &mut log)?;
    Ok(log)
}

/// The platform's human-readable status lines.
pub fn status(plan: &Plan) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for probe in &plan.probes {
        let outcome = process::run(&probe.argv, None, COMMAND_LIMIT)
            .map_err(|err| format!("cannot query {}: {err}", probe.name))?;
        lines.push(format!("$ {}", probe.argv.join(" ")));
        let text = outcome.stdout_text();
        for line in text.lines().take(20) {
            lines.push(format!("  {line}"));
        }
        if !outcome.succeeded() {
            lines.push(format!("  ({})", outcome.describe_stop()));
        }
    }
    Ok(lines)
}

/// Delete the parent directory of `path` when it is now empty and it is not a
/// directory this program must keep.
fn prune_parent(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    // Only a directory this program created for one of its own files is
    // removable; a shared one (`~/.config/systemd/user`, launchd's
    // `LaunchAgents`, a `*.wants` directory) belongs to the platform.
    let shared = ["user", "LaunchAgents", "wants"].iter().any(|name| {
        parent
            .file_name()
            .map(|part| part == *name)
            .unwrap_or(false)
    }) || parent
        .file_name()
        .map(|name| name.to_string_lossy().ends_with(".wants"))
        .unwrap_or(false);
    if shared {
        return;
    }
    let _ = std::fs::remove_dir(parent);
}

/// How the task can start before a login, and the one step that needs
/// privileges — reported, never taken.
pub fn startup_note(spec: &Spec, user: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        if spec.interval_secs.is_none() {
            return format!(
                "Linger: not changed - no interval is configured; \
                 `systemctl --user start {LABEL}.service` runs it by hand"
            );
        }
        if user.is_empty() {
            return "Linger: unknown user - run: loginctl enable-linger <user>".to_string();
        }
        let argv = ["loginctl", "enable-linger", user];
        let argv: Vec<String> = argv.iter().map(|part| (*part).to_string()).collect();
        match process::run(&argv, None, COMMAND_LIMIT) {
            Ok(outcome) if outcome.succeeded() => {
                "Linger: enabled (the reconnect task runs without a login)".to_string()
            }
            _ => format!(
                "Linger: not enabled - run: pkexec loginctl enable-linger {user} \
                 (otherwise the task starts at your next login)"
            ),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (spec, user);
        "Boot before login needs a root LaunchDaemon; the agents above run at login.".to_string()
    }
    #[cfg(target_os = "windows")]
    {
        let _ = (spec, user);
        "The logon task runs at login; boot-time execution needs an administrator.".to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (spec, user);
        "No startup hook is known for this system.".to_string()
    }
}

/// Escape one argument for a systemd `ExecStart=` line.
///
/// systemd splits on whitespace, so an argument carrying any needs quoting; and
/// because `%` and `$` are expanded by systemd inside unit files, they are
/// doubled. Without both halves an executable path containing `$` or `%` would
/// silently run something else.
pub fn quote_unit_arg(value: &str) -> String {
    let escaped = value.replace('%', "%%").replace('$', "$$");
    if escaped.is_empty()
        || escaped
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '"' | '\\' | '\''))
    {
        format!("\"{}\"", escaped.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        escaped
    }
}

/// Escape text for an XML text node (the launchd plist).
pub fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape one argument for a Windows command line, the way `CommandLineToArgvW`
/// parses it: every argument is quoted, backslashes before a quote (including
/// the closing one) are doubled, and embedded quotes are escaped.
pub fn windows_arg(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in value.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push('\\');
                out.push('"');
                backslashes = 0;
            }
            other => {
                backslashes = 0;
                out.push(other);
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// The per-user configuration directory: `$XDG_CONFIG_HOME` when set to an
/// absolute path, else `~/.config` (the XDG default systemd itself uses).
fn user_config_dir(spec: &Spec) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return dir;
        }
    }
    spec.home.join(".config")
}

/// The systemd **user** service and timer.
pub mod systemd {
    use super::{Absent, CommandSpec, FileSpec, Plan, Probe, Spec, LABEL};

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    fn service_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(format!("{LABEL}.service"))
    }

    fn timer_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(format!("{LABEL}.timer"))
    }

    /// `~/.config/systemd/user/srun-portal.service` and `.timer`.
    ///
    /// The startup trigger is `OnStartupSec=` rather than `OnBootSec=`: the
    /// manual documents it as "relative to when the service manager was
    /// started" and calls it "particularly useful for user managers", which is
    /// this case — with linger enabled, "startup" is boot, with nobody logged in.
    ///
    /// Three details here were **measured**, not read:
    ///
    /// * `OnUnitActiveSec=` alone **never arms**: a timer whose only trigger
    ///   counts from the previous run has nothing to count from.
    /// * A timer with **no trigger at all is not merely idle** — systemd rejects
    ///   the unit outright (`Timer unit lacks value setting. Refusing.`), it
    ///   lands in `LoadState=bad-setting`, and even `systemctl start` fails. So
    ///   "run only by hand" writes **no timer at all**, just the service.
    /// * `OnActiveSec=` **does** arm on its own, so the interval-only case uses
    ///   it for the first run.
    pub fn plan(spec: &Spec) -> Plan {
        let dir = super::user_config_dir(spec).join("systemd").join("user");
        let service = service_path(&dir);
        let timer = timer_path(&dir);
        let exe = super::quote_unit_arg(&spec.exe.to_string_lossy());
        let control = super::quote_unit_arg(&spec.control_dir.to_string_lossy());

        let service_body = format!(
            "[Unit]\nDescription=Srun portal reconnect check\n\n\
             [Service]\nType=oneshot\n\
             ExecStart={exe} __background --control-dir {control} --trigger interval\n\
             TimeoutStartSec=100s\nKillMode=control-group\n"
        );

        let manual_only = spec.interval_secs.is_none() && spec.boot_delay_secs.is_none();
        let mut triggers = String::new();
        if let Some(boot) = spec.boot_delay_secs {
            triggers.push_str(&format!("OnStartupSec={boot}s\n"));
        }
        if let Some(interval) = spec.interval_secs {
            if spec.boot_delay_secs.is_none() {
                triggers.push_str("OnActiveSec=1s\n");
            }
            triggers.push_str(&format!("OnUnitActiveSec={interval}s\n"));
        }
        let description = match (spec.interval_secs, spec.boot_delay_secs) {
            (Some(interval), _) => {
                format!("Check the Srun portal connection every {interval}s")
            }
            (None, Some(boot)) => {
                format!("Check the Srun portal connection {boot}s after startup")
            }
            (None, None) => "Srun portal reconnect check (manual start only)".to_string(),
        };
        let timer_body = format!(
            "[Unit]\nDescription={description}\n\n\
             [Timer]\n{triggers}AccuracySec=10s\nUnit={LABEL}.service\n\n\
             [Install]\nWantedBy=timers.target\n"
        );

        // Both triggers drive the same oneshot service; the startup delay is the
        // parent's wait, so a periodic run does not sleep it a second time
        // (which is what a separate `--startup-delay-secs` unit would do).
        let mut files = vec![FileSpec {
            path: service.clone(),
            body: service_body,
        }];
        if !manual_only {
            files.push(FileSpec {
                path: timer.clone(),
                body: timer_body,
            });
        }

        let unit = format!("{LABEL}.timer");
        let service_unit = format!("{LABEL}.service");
        // A previous install may have written and enabled the timer (its
        // `timers.target.wants` symlink survives rewriting the units), so the
        // old trigger always comes out first.
        let prepare = vec![
            CommandSpec::new(argv(&["systemctl", "--user", "stop", &unit])).only_when("timer"),
            CommandSpec::new(argv(&["systemctl", "--user", "disable", &unit])).only_when("timer"),
        ];
        let mut activate = vec![CommandSpec::new(argv(&[
            "systemctl",
            "--user",
            "daemon-reload",
        ]))];
        if !manual_only {
            activate.push(CommandSpec::new(argv(&[
                "systemctl",
                "--user",
                "enable",
                "--now",
                &unit,
            ])));
        } else {
            // Nothing schedules it, so the service is started by hand.
            activate.push(CommandSpec::new(argv(&[
                "systemctl",
                "--user",
                "start",
                &service_unit,
            ])));
        }

        Plan {
            files,
            prepare,
            activate,
            stop: vec![
                CommandSpec::new(argv(&["systemctl", "--user", "stop", &unit])).only_when("timer"),
                CommandSpec::new(argv(&["systemctl", "--user", "stop", &service_unit]))
                    .only_when("service"),
            ],
            remove: vec![
                CommandSpec::new(argv(&["systemctl", "--user", "disable", &unit]))
                    .only_when("timer"),
            ],
            query: vec![
                CommandSpec::new(argv(&[
                    "systemctl",
                    "--user",
                    "show",
                    &service_unit,
                    "--property=LoadState,ActiveState,Result,ExecMainStatus",
                ])),
                CommandSpec::new(argv(&[
                    "systemctl",
                    "--user",
                    "list-timers",
                    &unit,
                    "--no-pager",
                ])),
            ],
            reload: vec![CommandSpec::new(argv(&[
                "systemctl",
                "--user",
                "daemon-reload",
            ]))],
            probes: vec![
                Probe {
                    name: "service",
                    argv: argv(&[
                        "systemctl",
                        "--user",
                        "show",
                        &service_unit,
                        "--property=LoadState",
                        "--value",
                    ]),
                    absent: Absent::OutputLine("not-found"),
                },
                Probe {
                    name: "timer",
                    argv: argv(&[
                        "systemctl",
                        "--user",
                        "show",
                        &unit,
                        "--property=LoadState",
                        "--value",
                    ]),
                    absent: Absent::OutputLine("not-found"),
                },
            ],
            owned: vec![
                service,
                timer,
                dir.join(format!("{LABEL}-startup.service")),
                dir.join(format!("{LABEL}-startup.timer")),
            ],
            owned_tasks: Vec::new(),
            cadence: spec.cadence(),
        }
    }

    /// The platform's own view, from machine-readable properties only.
    pub fn platform_state(spec: &Spec) -> Result<super::PlatformState, String> {
        let _ = spec;
        let mut state = super::PlatformState {
            available: true,
            resources: Vec::new(),
            next_run_at_ms: None,
        };
        for (name, properties, is_timer) in [
            (
                format!("{LABEL}.service"),
                "LoadState,ActiveState,ExecMainStatus",
                false,
            ),
            (
                format!("{LABEL}.timer"),
                "LoadState,ActiveState,NextElapseUSecRealtime",
                true,
            ),
        ] {
            let mut args: Vec<String> = vec![
                "systemctl".into(),
                "--user".into(),
                "show".into(),
                name.clone(),
            ];
            args.push(format!("--property={properties}"));
            let outcome = super::process::run(&args, None, super::COMMAND_LIMIT)
                .map_err(|err| err.to_string())?;
            if !outcome.succeeded() {
                return Err(format!(
                    "systemctl show {name} failed: {} {}",
                    outcome.describe_stop(),
                    outcome.stderr_text()
                ));
            }
            let values = parse_properties(&outcome.stdout_text());
            let load = values.get("LoadState").cloned().unwrap_or_default();
            let active = values.get("ActiveState").cloned().unwrap_or_default();
            if is_timer {
                state.next_run_at_ms = values
                    .get("NextElapseUSecRealtime")
                    .and_then(|value| parse_systemd_timestamp(value));
            }
            state.resources.push(super::ResourceState {
                name,
                present: load == "loaded",
                running: active == "activating" || active == "active",
                last_exit_code: values
                    .get("ExecMainStatus")
                    .and_then(|value| value.parse::<i32>().ok()),
            });
        }
        Ok(state)
    }

    fn parse_properties(text: &str) -> std::collections::HashMap<String, String> {
        text.lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .collect()
    }

    /// `NextElapseUSecRealtime` is a microsecond epoch, or a human timestamp on
    /// older versions; either way an unparseable value is `None`, not zero.
    fn parse_systemd_timestamp(value: &str) -> Option<u64> {
        if let Ok(micros) = value.parse::<u64>() {
            return Some(micros / 1_000);
        }
        None
    }
}

/// The launchd LaunchAgents.
pub mod launchd {
    use super::{
        Absent, CommandSpec, FileSpec, Plan, Probe, Spec, LAUNCHD_INTERVAL_LABEL,
        LAUNCHD_STARTUP_LABEL,
    };

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    fn uid() -> u32 {
        #[cfg(unix)]
        {
            unsafe { libc::getuid() }
        }
        #[cfg(not(unix))]
        {
            0
        }
    }

    fn plist_path(spec: &Spec, label: &str) -> std::path::PathBuf {
        spec.home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist"))
    }

    /// One agent per trigger, because launchd keys them independently: the
    /// interval agent carries only `StartInterval` plus the hidden
    /// `--trigger interval`, and the startup agent only `RunAtLoad` plus
    /// `--startup-delay-secs`, so a periodic run never sleeps the startup delay
    /// a second time.
    pub fn plan(spec: &Spec) -> Plan {
        let exe = spec.exe.to_string_lossy().into_owned();
        let mut files = Vec::new();
        if let Some(interval) = spec.interval_secs {
            let args = spec.background_args(super::Trigger::Interval, None);
            files.push(FileSpec {
                path: plist_path(spec, LAUNCHD_INTERVAL_LABEL),
                body: plist(LAUNCHD_INTERVAL_LABEL, &exe, &args, Some(interval), false),
            });
        }
        if let Some(delay) = spec.boot_delay_secs {
            let args = spec.background_args(super::Trigger::Startup, Some(delay));
            files.push(FileSpec {
                path: plist_path(spec, LAUNCHD_STARTUP_LABEL),
                body: plist(LAUNCHD_STARTUP_LABEL, &exe, &args, None, true),
            });
        }

        // Every label a previous install may have loaded, including the single
        // agent of the older layout.
        let labels: [(&'static str, &'static str); 3] = [
            ("interval", LAUNCHD_INTERVAL_LABEL),
            ("startup", LAUNCHD_STARTUP_LABEL),
            ("legacy", "com.srun-portal.reconnect"),
        ];
        let mut prepare = Vec::new();
        let mut remove = Vec::new();
        let mut probes = Vec::new();
        let mut owned = Vec::new();
        for (name, label) in labels {
            let resource: &'static str = match name {
                "interval" => "interval",
                "startup" => "startup",
                _ => "legacy",
            };
            let bootout = CommandSpec::new(argv(&[
                "launchctl",
                "bootout",
                &format!("gui/{}/{}", uid(), label),
            ]))
            .only_when(resource);
            prepare.push(bootout.clone());
            remove.push(bootout);
            probes.push(Probe {
                name: resource,
                argv: argv(&["launchctl", "print", &format!("gui/{}/{}", uid(), label)]),
                absent: Absent::ExitCode(113),
            });
            owned.push(plist_path(spec, label));
        }

        let mut activate = Vec::new();
        for file in &files {
            activate.push(CommandSpec::new(argv(&[
                "launchctl",
                "bootstrap",
                &format!("gui/{}", uid()),
                &file.path.to_string_lossy(),
            ])));
        }

        let mut stop = Vec::new();
        for (name, label) in labels[..2].iter().copied() {
            let resource: &'static str = if name == "interval" {
                "interval"
            } else {
                "startup"
            };
            stop.push(
                CommandSpec::new(argv(&[
                    "launchctl",
                    "kill",
                    "SIGTERM",
                    &format!("gui/{}/{}", uid(), label),
                ]))
                .only_when(resource),
            );
        }

        Plan {
            files,
            prepare,
            activate,
            stop,
            remove,
            query: vec![
                CommandSpec::new(argv(&[
                    "launchctl",
                    "print",
                    &format!("gui/{}/{}", uid(), LAUNCHD_INTERVAL_LABEL),
                ])),
                CommandSpec::new(argv(&[
                    "launchctl",
                    "print",
                    &format!("gui/{}/{}", uid(), LAUNCHD_STARTUP_LABEL),
                ])),
            ],
            reload: Vec::new(),
            probes,
            owned,
            owned_tasks: Vec::new(),
            cadence: spec.cadence(),
        }
    }

    fn plist(
        label: &str,
        exe: &str,
        args: &[String],
        interval: Option<u64>,
        startup: bool,
    ) -> String {
        let mut arguments = String::new();
        for arg in args {
            arguments.push_str(&format!(
                "\x20       <string>{}</string>\n",
                super::xml_escape(arg)
            ));
        }
        let mut schedule = String::new();
        if startup {
            schedule.push_str("\x20   <key>RunAtLoad</key>\n\x20   <true/>\n");
        }
        if let Some(interval) = interval {
            schedule.push_str(&format!(
                "\x20   <key>StartInterval</key>\n\x20   <integer>{interval}</integer>\n"
            ));
        }
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \x20   <key>Label</key>\n\
             \x20   <string>{label}</string>\n\
             \x20   <key>ProgramArguments</key>\n\
             \x20   <array>\n\
             \x20       <string>{}</string>\n\
             {arguments}\
             \x20   </array>\n\
             {schedule}\
             \x20   <key>AbandonProcessGroup</key>\n\
             \x20   <false/>\n\
             \x20   <key>ProcessType</key>\n\
             \x20   <string>Background</string>\n\
             </dict>\n\
             </plist>\n",
            super::xml_escape(exe)
        )
    }

    /// `launchctl print` answers 113 for a label that is not loaded; anything
    /// else is reported rather than flattened into "not installed".
    pub fn platform_state(spec: &Spec) -> Result<super::PlatformState, String> {
        let mut state = super::PlatformState {
            available: true,
            resources: Vec::new(),
            next_run_at_ms: None,
        };
        for (name, label) in [
            (LAUNCHD_INTERVAL_LABEL, LAUNCHD_INTERVAL_LABEL),
            (LAUNCHD_STARTUP_LABEL, LAUNCHD_STARTUP_LABEL),
        ] {
            let outcome = super::process::run(
                &argv(&["launchctl", "print", &format!("gui/{}/{}", uid(), label)]),
                None,
                super::COMMAND_LIMIT,
            )
            .map_err(|err| err.to_string())?;
            let missing = matches!(
                &outcome.stop,
                super::process::Stop::Exited(status) if status.code() == Some(113)
            );
            if !outcome.succeeded() && !missing {
                return Err(format!(
                    "launchctl print {label} failed: {} {}",
                    outcome.describe_stop(),
                    outcome.stderr_text()
                ));
            }
            let text = outcome.stdout_text();
            state.resources.push(super::ResourceState {
                name: name.to_string(),
                present: !missing,
                running: text.contains("state = running"),
                last_exit_code: None,
            });
        }
        let _ = spec;
        Ok(state)
    }
}

/// The Windows Task Scheduler tasks, driven through PowerShell's object API.
pub mod scheduler {
    use super::{windows_arg, CommandSpec, Plan, Probe, Spec};
    use crate::process;
    use serde_json::{json, Value};

    const POWERSHELL: &str = "powershell.exe";

    /// Names an older version used; they are removed only when their principal
    /// proves they belong to this user.
    pub const LEGACY_TASKS: [&str; 2] = ["srun-portal-reconnect", "srun-portal-reconnect-boot"];

    /// The per-user task names. Two users on one machine get two independent
    /// sets instead of overwriting each other.
    pub fn interval_task(spec: &Spec) -> String {
        format!("srun-portal-{}-reconnect", spec.owner_id)
    }

    pub fn boot_task(spec: &Spec) -> String {
        format!("srun-portal-{}-reconnect-boot", spec.owner_id)
    }

    /// Task Scheduler counts whole minutes: a sub-minute interval is one minute,
    /// and anything past a day is a day.
    pub fn interval_minutes(secs: u64) -> u64 {
        secs.div_ceil(60).clamp(1, 1439)
    }

    /// One fixed script. Every dynamic value — paths, arguments, task names, the
    /// SID — arrives as JSON on standard input, so nothing from the
    /// configuration is ever spliced into PowerShell source.
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$req = [Console]::In.ReadToEnd() | ConvertFrom-Json
function Out($o) { $o | ConvertTo-Json -Compress -Depth 6 }
try {
  switch ($req.action) {
    'register' {
      $action = New-ScheduledTaskAction -Execute $req.exe -Argument $req.arguments
      if ($null -ne $req.interval_minutes) {
        $trigger = New-ScheduledTaskTrigger -Once -At ((Get-Date).AddMinutes(1)) `
          -RepetitionInterval (New-TimeSpan -Minutes $req.interval_minutes)
      } else {
        $trigger = New-ScheduledTaskTrigger -AtLogOn -User $req.sid
        if ($null -ne $req.boot_delay_secs -and $req.boot_delay_secs -gt 0) {
          $trigger.Delay = "PT$($req.boot_delay_secs)S"
        }
      }
      $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds 100) `
        -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
      $principal = New-ScheduledTaskPrincipal -UserId $req.sid -LogonType Interactive -RunLevel Limited
      Register-ScheduledTask -TaskName $req.name -Action $action -Trigger $trigger `
        -Settings $settings -Principal $principal -Force | Out-Null
      Out @{ ok = $true }
    }
    'stop' {
      $task = Get-ScheduledTask -TaskName $req.name -ErrorAction SilentlyContinue
      if ($null -eq $task) { Out @{ ok = $true; stopped = $false }; break }
      if ([string]$task.Principal.UserId -ne $req.sid) {
        Out @{ ok = $false; error = 'not owned by this user' }
        break
      }
      Stop-ScheduledTask -TaskName $req.name
      Out @{ ok = $true; stopped = $true }
    }
    'unregister' {
      $task = Get-ScheduledTask -TaskName $req.name -ErrorAction SilentlyContinue
      if ($null -eq $task) { Out @{ ok = $true; removed = $false; present = $false }; break }
      if ($null -ne $req.sid -and [string]$task.Principal.UserId -ne $req.sid) {
        Out @{ ok = $false; error = 'not owned by this user'; present = $true }
        break
      }
      Stop-ScheduledTask -TaskName $req.name -ErrorAction SilentlyContinue
      Unregister-ScheduledTask -TaskName $req.name -Confirm:$false
      Out @{ ok = $true; removed = $true; present = $true }
    }
    'query' {
      $task = Get-ScheduledTask -TaskName $req.name -ErrorAction SilentlyContinue
      if ($null -eq $task) { Out @{ ok = $true; present = $false }; break }
      $info = Get-ScheduledTaskInfo -TaskName $req.name
      $next = $null
      if ($null -ne $info.NextRunTime) { $next = $info.NextRunTime.ToUniversalTime().ToString('o') }
      Out @{ ok = $true; present = $true; state = [string]$task.State;
            owner = [string]$task.Principal.UserId;
            last_result = $info.LastTaskResult; next_run = $next }
    }
    default { Out @{ ok = $false; error = 'unknown action' } }
  }
} catch {
  Out @{ ok = $false; error = $_.Exception.Message }
  exit 1
}
"#;

    fn command(request: &Value) -> CommandSpec {
        CommandSpec {
            argv: vec![
                POWERSHELL.to_string(),
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                SCRIPT.to_string(),
            ],
            input: Some(request.to_string()),
            when_present: None,
        }
    }

    /// The executable plus its arguments as one command line, every token quoted
    /// the way the Windows parser expects.
    pub fn action_arguments(spec: &Spec, trigger: super::Trigger) -> String {
        let mut parts = vec![windows_arg(&spec.exe.to_string_lossy())];
        for arg in spec.background_args(trigger, None) {
            parts.push(windows_arg(&arg));
        }
        parts.join(" ")
    }

    pub fn plan(spec: &Spec) -> Plan {
        let interval_name = interval_task(spec);
        let boot_name = boot_task(spec);
        let mut activate = Vec::new();
        let mut prepare = Vec::new();
        let mut stop = Vec::new();
        let mut remove = Vec::new();
        let mut probes = Vec::new();

        if let Some(secs) = spec.interval_secs {
            activate.push(command(&json!({
                "action": "register",
                "name": interval_name,
                "exe": spec.exe.to_string_lossy(),
                "arguments": action_arguments(spec, super::Trigger::Interval),
                "sid": spec.owner_id,
                "interval_minutes": interval_minutes(secs),
            })));
        }
        if let Some(delay) = spec.boot_delay_secs {
            activate.push(command(&json!({
                "action": "register",
                "name": boot_name,
                "exe": spec.exe.to_string_lossy(),
                "arguments": action_arguments(spec, super::Trigger::Startup),
                "sid": spec.owner_id,
                "boot_delay_secs": delay,
            })));
        }

        // Both names are probed and cleaned up whatever the current cadence is,
        // so turning a trigger off does not leave its old task behind.
        for (resource, name) in [("interval", &interval_name), ("boot", &boot_name)] {
            let request = json!({ "action": "stop", "name": name, "sid": spec.owner_id });
            prepare.push(command(&request).only_when(resource));
            stop.push(command(&request).only_when(resource));
            remove.push(
                command(&json!({ "action": "unregister", "name": name, "sid": spec.owner_id }))
                    .only_when(resource),
            );
            probes.push(query_probe(
                resource,
                &json!({
                    "action": "query",
                    "name": name,
                }),
            ));
        }

        // Pre-SID names: probe them, then delete only when the task's own
        // principal matches this user. A probe that cannot prove ownership makes
        // the unregister refuse, and that refusal is reported.
        for (index, legacy) in LEGACY_TASKS.iter().enumerate() {
            let resource: &'static str = if index == 0 { "legacy" } else { "legacy_boot" };
            probes.push(query_probe(
                resource,
                &json!({ "action": "query", "name": legacy }),
            ));
            remove.push(
                command(&json!({
                    "action": "unregister",
                    "name": legacy,
                    "sid": spec.owner_id,
                }))
                .only_when(resource),
            );
        }

        Plan {
            files: Vec::new(),
            prepare,
            activate,
            stop,
            remove,
            query: vec![command(&json!({
                "action": "query",
                "name": interval_name,
            }))],
            reload: Vec::new(),
            probes,
            owned: Vec::new(),
            owned_tasks: vec![interval_name, boot_name],
            cadence: spec.cadence(),
        }
    }

    /// A probe answers through the task's `present` field, which the fixed
    /// script prints as `"present":false` for a task that is not there. That is
    /// a stable token, not localized prose.
    fn query_probe(resource: &'static str, _request: &Value) -> Probe {
        Probe {
            name: resource,
            argv: vec![
                POWERSHELL.to_string(),
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                SCRIPT.to_string(),
            ],
            absent: super::Absent::OutputLine("\"present\":false"),
        }
    }

    /// What one task query reported.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TaskInfo {
        pub present: bool,
        pub owned: bool,
        pub running: bool,
        pub last_exit_code: Option<i32>,
        pub next_run_ms: Option<u64>,
    }

    pub fn inspect(spec: &Spec, name: &str) -> Result<TaskInfo, String> {
        let request = json!({ "action": "query", "name": name });
        let outcome = process::run(
            &command(&request).argv,
            Some(request.to_string().as_bytes()),
            process::COMMAND_LIMIT,
        )
        .map_err(|err| err.to_string())?;
        if !outcome.succeeded() {
            return Err(format!(
                "cannot query task {name}: {} {}",
                outcome.describe_stop(),
                outcome.stderr_text()
            ));
        }
        let value: Value = serde_json::from_str(&outcome.stdout_text())
            .map_err(|err| format!("cannot read the task query for {name}: {err}"))?;
        Ok(TaskInfo {
            present: value
                .get("present")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            owned: value
                .get("owner")
                .and_then(Value::as_str)
                .map(|owner| owner.eq_ignore_ascii_case(&spec.owner_id))
                .unwrap_or(false),
            running: value.get("state").and_then(Value::as_str) == Some("Running"),
            last_exit_code: value
                .get("last_result")
                .and_then(Value::as_i64)
                .map(|code| code as i32),
            next_run_ms: value
                .get("next_run")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_ms),
        })
    }

    fn parse_rfc3339_ms(text: &str) -> Option<u64> {
        // `2026-10-08T13:43:00.0000000Z`
        let date = &text[..text.len().min(10)];
        let time = text.get(11..19)?;
        let mut parts = date.split('-');
        let year: i64 = parts.next()?.parse().ok()?;
        let month: i64 = parts.next()?.parse().ok()?;
        let day: i64 = parts.next()?.parse().ok()?;
        let mut clock = time.split(':');
        let hour: i64 = clock.next()?.parse().ok()?;
        let minute: i64 = clock.next()?.parse().ok()?;
        let second: i64 = clock.next()?.parse().ok()?;
        let days = days_from_civil(year, month, day);
        let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
        u64::try_from(secs).ok().map(|secs| secs * 1_000)
    }

    /// Howard Hinnant's `days_from_civil`, so no date library is needed for one
    /// timestamp.
    fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    pub fn platform_state(spec: &Spec) -> Result<super::PlatformState, String> {
        let mut state = super::PlatformState {
            available: true,
            resources: Vec::new(),
            next_run_at_ms: None,
        };
        for name in [interval_task(spec), boot_task(spec)] {
            let info = inspect(spec, &name)?;
            if state.next_run_at_ms.is_none() {
                state.next_run_at_ms = info.next_run_ms;
            }
            state.resources.push(super::ResourceState {
                name,
                present: info.present,
                running: info.running,
                last_exit_code: info.last_exit_code,
            });
        }
        Ok(state)
    }
}

/// The platform's own view of the installed task.
#[cfg(target_os = "linux")]
pub fn platform_state(spec: &Spec) -> Result<PlatformState, String> {
    systemd::platform_state(spec)
}

#[cfg(target_os = "macos")]
pub fn platform_state(spec: &Spec) -> Result<PlatformState, String> {
    launchd::platform_state(spec)
}

#[cfg(target_os = "windows")]
pub fn platform_state(spec: &Spec) -> Result<PlatformState, String> {
    scheduler::platform_state(spec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn platform_state(_spec: &Spec) -> Result<PlatformState, String> {
    Ok(PlatformState::default())
}

/// This user's real identity: the uid on Unix, the token SID on Windows.
#[cfg(unix)]
pub fn owner_id() -> String {
    unsafe { libc::getuid() }.to_string()
}

#[cfg(windows)]
pub fn owner_id() -> String {
    windows_sid().unwrap_or_default()
}

#[cfg(windows)]
fn windows_sid() -> Option<String> {
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut needed = 0u32;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
        let mut buffer = vec![0u8; needed.max(1) as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return None;
        }
        let user = buffer.as_ptr().cast::<TOKEN_USER>().read_unaligned();
        let mut sid_text: *mut u16 = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid_text) == 0 {
            return None;
        }
        let mut len = 0usize;
        while *sid_text.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, len));
        LocalFree(sid_text.cast());
        Some(sid)
    }
}

#[cfg(not(any(unix, windows)))]
pub fn owner_id() -> String {
    "0".to_string()
}

/// A human-readable name for this user, for the linger hint.
pub fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

/// Whether `exe` looks like a build artefact, which the task would then depend
/// on: a `target/` directory is the cargo one, and it is transient.
pub fn note_for_executable(exe: &Path) -> Option<String> {
    // A path can arrive with either separator: a user pasting a Unix path on
    // Windows, or the reverse in a log. The component check is separator-aware
    // already; this makes the textual one agree with it.
    let text = exe.to_string_lossy();
    let is_build_dir = exe
        .components()
        .any(|component| component.as_os_str() == "target")
        && (text.contains("/target/") || text.contains("\\target\\"));
    if !is_build_dir {
        return None;
    }
    Some(format!(
        "Note: {} is a build artefact; move the binary somewhere stable and \
         reinstall before relying on this task",
        exe.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            exe: PathBuf::from("/opt/a/srun-portal"),
            control_dir: PathBuf::from("/home/u/.config/srun-portal/service"),
            interval_secs: Some(300),
            boot_delay_secs: Some(30),
            home: PathBuf::from("/home/u"),
            owner_id: "1000".to_string(),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_plan_runs_the_hidden_entry_point_with_the_control_directory() {
        let spec = spec();
        let plan = plan(&spec);
        let bodies = plan
            .files
            .iter()
            .map(|file| file.body.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            bodies.contains("__background --control-dir /home/u/.config/srun-portal/service"),
            "{bodies}"
        );
        assert!(plan
            .owned
            .iter()
            .any(|path| path.ends_with("srun-portal.service")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_trigger_removes_the_old_definition() {
        let mut manual = spec();
        manual.interval_secs = None;
        manual.boot_delay_secs = None;
        let plan = plan(&manual);
        // No new trigger is activated, but the old ones are still taken out.
        assert!(
            !plan
                .activate
                .iter()
                .any(|command| command.argv.iter().any(|arg| arg == "timer")),
            "{:?}",
            plan.activate
        );
        assert!(plan
            .prepare
            .iter()
            .any(|command| command.argv.iter().any(|arg| arg == "srun-portal.timer")));
    }

    #[test]
    fn every_command_that_cleans_up_is_guarded_by_a_probe() {
        for spec in [spec(), {
            let mut one = spec();
            one.boot_delay_secs = None;
            one
        }] {
            let plan = plan(&spec);
            for command in plan.prepare.iter().chain(&plan.stop).chain(&plan.remove) {
                if let Some(name) = command.when_present {
                    assert!(
                        plan.probes.iter().any(|probe| probe.name == name),
                        "no probe for {name}: {command:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn quote_unit_arg_escapes_expansions_and_quotes() {
        assert_eq!(quote_unit_arg("/opt/a"), "/opt/a");
        assert_eq!(quote_unit_arg("a b"), "\"a b\"");
        assert_eq!(quote_unit_arg(""), "\"\"");
        assert_eq!(quote_unit_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_unit_arg("a\\b"), "\"a\\\\b\"");
        // `%` and `$` are expanded inside a unit file, so both are doubled —
        // independently of whether the argument needs quoting at all.
        assert_eq!(quote_unit_arg("100%$HOME"), "100%%$$HOME");
        assert_eq!(quote_unit_arg("a$b"), "a$$b");
        assert_eq!(quote_unit_arg("a b%c"), "\"a b%%c\"");
    }

    #[test]
    fn xml_escape_covers_the_three_entities() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        assert_eq!(xml_escape("plain/path"), "plain/path");
    }

    #[test]
    fn windows_arg_quotes_the_way_the_crt_parses() {
        assert_eq!(windows_arg("plain"), "\"plain\"");
        assert_eq!(windows_arg("a b"), "\"a b\"");
        assert_eq!(windows_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(windows_arg("a\\b"), "\"a\\b\"");
        // Backslashes before a quote are doubled, and so is a trailing run.
        assert_eq!(windows_arg("a\\\\\"b"), "\"a\\\\\\\\\\\"b\"");
        assert_eq!(windows_arg("dir\\"), "\"dir\\\\\"");
    }

    #[test]
    fn cadence_text_distinguishes_the_four_cases() {
        assert_eq!(spec().cadence(), "every 300s, 30s after startup");
        let mut one = spec();
        one.boot_delay_secs = None;
        assert_eq!(one.cadence(), "every 300s");
        let mut one = spec();
        one.interval_secs = None;
        assert_eq!(one.cadence(), "30s after startup only");
        let mut none = spec();
        none.interval_secs = None;
        none.boot_delay_secs = None;
        assert_eq!(none.cadence(), "only when started by hand");
    }

    #[test]
    fn a_build_artefact_is_called_out() {
        assert!(note_for_executable(Path::new("/repo/target/debug/srun-portal")).is_some());
        assert!(note_for_executable(Path::new("/opt/srun-portal")).is_none());
    }

    /// The plan constructors are pure, so the exact artifacts are asserted here
    /// for the host platform only; the other two are covered by the native test
    /// suite on their own systems.
    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_units_are_exact() {
        let plan = plan(&spec());
        let service = plan
            .files
            .iter()
            .find(|file| file.path.ends_with("srun-portal.service"))
            .unwrap();
        assert_eq!(
            service.body,
            "[Unit]\nDescription=Srun portal reconnect check\n\n\
             [Service]\nType=oneshot\n\
             ExecStart=/opt/a/srun-portal __background --control-dir \
             /home/u/.config/srun-portal/service --trigger interval\n\
             TimeoutStartSec=100s\nKillMode=control-group\n"
        );
        let timer = plan
            .files
            .iter()
            .find(|file| file.path.ends_with("srun-portal.timer"))
            .unwrap();
        assert_eq!(
            timer.body,
            "[Unit]\nDescription=Check the Srun portal connection every 300s\n\n\
             [Timer]\nOnStartupSec=30s\nOnUnitActiveSec=300s\nAccuracySec=10s\n\
             Unit=srun-portal.service\n\n[Install]\nWantedBy=timers.target\n"
        );
        // One service carries both triggers; the startup delay is the parent's
        // wait, so a periodic run does not sleep it again.
        assert!(!plan
            .files
            .iter()
            .any(|file| file.path.ends_with("srun-portal-startup.service")));
        assert!(
            plan.owned
                .iter()
                .any(|path| path.ends_with("srun-portal-startup.service")),
            "the obsolete unit is still owned, so an upgrade removes it"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_interval_only_timer_arms_with_onactivesec() {
        let mut spec = spec();
        spec.boot_delay_secs = None;
        let plan = plan(&spec);
        let timer = plan
            .files
            .iter()
            .find(|file| file.path.ends_with("srun-portal.timer"))
            .unwrap();
        assert!(
            timer
                .body
                .contains("OnActiveSec=1s\nOnUnitActiveSec=300s\n"),
            "{}",
            timer.body
        );
        assert!(!timer.body.contains("OnStartupSec"), "{}", timer.body);
    }

    #[cfg(target_os = "linux")]
    #[cfg(target_os = "linux")]
    #[test]
    fn an_install_from_an_earlier_cadence_removes_the_trigger_it_no_longer_writes() {
        let dir = std::env::temp_dir().join(format!(
            "srun-portal-stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let timer = dir.join("srun-portal.timer");
        let service = dir.join("srun-portal.service");
        std::fs::write(&timer, "old timer\n").unwrap();

        // A manual-only plan owns the timer path but writes only the service.
        let plan = Plan {
            files: vec![FileSpec {
                path: service.clone(),
                body: "service\n".to_string(),
            }],
            owned: vec![service.clone(), timer.clone()],
            ..Plan::default()
        };
        let log = install(&plan).unwrap();

        assert!(!timer.exists(), "the switched-off trigger is gone");
        assert!(service.exists());
        assert!(
            log.iter().any(|line| line.contains("Removed obsolete")),
            "{log:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_removes_the_timer_symlink_that_enable_created() {
        let dir = std::env::temp_dir().join(format!(
            "srun-portal-symlink-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let wants = dir.join("timers.target.wants");
        std::fs::create_dir_all(&wants).unwrap();
        let target = dir.join("srun-portal.timer");
        std::fs::write(&target, "timer\n").unwrap();
        let link = wants.join("srun-portal.timer");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let plan = Plan {
            owned: vec![target.clone()],
            ..Plan::default()
        };
        let log = uninstall(&plan).unwrap();
        assert!(log[0].contains("Removed"), "{log:?}");
        assert!(!target.exists());
        // The wants directory is not this program's to delete.
        assert!(wants.exists(), "a shared directory is never pruned");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bad_command_fails_the_install_and_keeps_the_log() {
        // A program that does not exist, so the failure is the same on every
        // platform and does not depend on a shell being present.
        let missing = if cfg!(windows) {
            "srun-portal-test-no-such-command.exe"
        } else {
            "srun-portal-test-no-such-command"
        };
        let plan = Plan {
            activate: vec![CommandSpec::new(vec![missing.to_string()])],
            ..Plan::default()
        };
        let failure = install(&plan).unwrap_err();
        assert!(failure.error.contains(missing), "{}", failure.error);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_probe_that_says_absent_skips_the_cleanup_command() {
        let plan = Plan {
            prepare: vec![CommandSpec::new(vec!["/bin/false".to_string()]).only_when("thing")],
            probes: vec![Probe {
                name: "thing",
                argv: vec!["/bin/echo".to_string(), "not-found".to_string()],
                absent: Absent::OutputLine("not-found"),
            }],
            ..Plan::default()
        };
        let log = install(&plan).unwrap();
        assert!(
            log.iter()
                .any(|line| line.contains("Skipped (not present)")),
            "{log:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_probe_that_fails_for_another_reason_is_an_error() {
        let plan = Plan {
            probes: vec![Probe {
                name: "thing",
                argv: vec!["/bin/false".to_string()],
                absent: Absent::ExitCode(4),
            }],
            ..Plan::default()
        };
        let failure = install(&plan).unwrap_err();
        assert!(
            failure.error.contains("cannot query thing"),
            "{}",
            failure.error
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn uninstall_removes_owned_files_and_reports_what_was_gone() {
        let dir = std::env::temp_dir().join(format!(
            "srun-portal-service-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("unit.conf");
        std::fs::write(&file, "x").unwrap();
        let plan = Plan {
            owned: vec![file.clone()],
            ..Plan::default()
        };
        let log = uninstall(&plan).unwrap();
        assert!(log[0].contains("Removed"), "{log:?}");
        assert!(!file.exists());
        let log = uninstall(&plan).unwrap();
        assert!(log[0].contains("Already gone"), "{log:?}");
        // The emptied parent is pruned, so cleanup must tolerate that too.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_windows_interval_is_rounded_up_and_clamped() {
        assert_eq!(scheduler::interval_minutes(1), 1);
        assert_eq!(scheduler::interval_minutes(100), 2);
        assert_eq!(scheduler::interval_minutes(300), 5);
        assert_eq!(scheduler::interval_minutes(86_340), 1439);
    }
}
