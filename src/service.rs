//! What "reconnect in the background" means on each system.
//!
//! Every platform gets its own native mechanism, and each one is expressed as
//! **data** by a pure constructor in a submodule ([`systemd`], [`launchd`],
//! [`scheduler`]) so the exact artifacts can be asserted on any host:
//!
//! * Linux — a systemd **user** service plus a timer
//!   (`~/.config/systemd/user/srun-portal.{service,timer}`),
//! * macOS — a launchd **LaunchAgent** plist
//!   (`~/Library/LaunchAgents/com.srun-portal.reconnect.plist`),
//! * Windows — two Task Scheduler entries driven with `schtasks.exe` (no file
//!   artifact at all).
//!
//! [`install`], [`uninstall`] and [`status`] are the portable executor: write
//! the files, run the commands, report what happened. The only platform
//! branches left are [`plan`] (which constructor) and [`startup_note`] (it also
//! enables linger on Linux), so the platform surface stays in one place.
//!
//! Scope is always the **user**: the portal session, the configuration file and
//! the credentials file all belong to one account, and running as root would
//! read `/root/…` instead. "Before login" therefore needs a platform-specific
//! privilege step, which is reported rather than taken ([`startup_note`]).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Base name of the systemd unit and of the launchd label.
pub const LABEL: &str = "srun-portal";

/// The Windows task that runs at logon.
pub const WINDOWS_BOOT_TASK: &str = "srun-portal-reconnect-boot";

/// The Windows task that runs on a repeating schedule.
pub const WINDOWS_TIMER_TASK: &str = "srun-portal-reconnect";

/// What to install: the executable to run, the configuration it needs, and the
/// cadence the user configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    /// Usually `std::env::current_exe()`: the unit refers to this path, so a
    /// binary that moves needs a reinstall.
    pub exe: PathBuf,
    /// The configuration file the task reads (`--config`).
    pub config: PathBuf,
    /// Seconds between two checks; `None` installs no interval.
    pub interval_secs: Option<u64>,
    /// Seconds after startup before the first check; `None` leaves the task out
    /// of the startup sequence entirely.
    pub boot_delay_secs: Option<u64>,
    /// The home directory the per-user artifacts live under. Passed in rather
    /// than looked up here, so a plan is pure and the "no home directory" case
    /// surfaces once, at the CLI boundary.
    pub home: PathBuf,
}

impl Spec {
    /// One line describing when the installed task runs, for the user.
    ///
    /// With neither cadence set the task exists but only runs when something
    /// starts it, which is a legitimate way to install it ahead of time.
    pub fn cadence(&self) -> String {
        match (self.interval_secs, self.boot_delay_secs) {
            (Some(interval), Some(boot)) => format!("every {interval}s, {boot}s after startup"),
            (Some(interval), None) => format!("every {interval}s"),
            (None, Some(boot)) => format!("{boot}s after startup only"),
            (None, None) => "only when started by hand".to_string(),
        }
    }
}

/// One file the plan owns: the path and the exact bytes to put there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    pub path: PathBuf,
    pub body: String,
}

/// A complete description of the background task for one platform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Files to create, in order. Empty on Windows, where the task lives in the
    /// Task Scheduler store instead.
    pub files: Vec<FileSpec>,
    /// Commands that install/activate the task, run in order.
    pub enable: Vec<Vec<String>>,
    /// Commands that deactivate/remove the task, run in order. A failure is
    /// reported but does not stop the removal.
    pub disable: Vec<Vec<String>>,
    /// Read-only commands whose output [`status`] reports.
    pub query: Vec<Vec<String>>,
    /// Files [`status`] compares against the freshly planned bodies.
    pub owned: Vec<PathBuf>,
    /// One line describing when the task runs, for the user.
    pub cadence: String,
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
        cadence: "unsupported on this system".to_string(),
        ..Plan::default()
    }
}

/// What an install/uninstall attempt did before it stopped.
///
/// A failure part-way through still has to report the steps that ran: an
/// install that wrote the units and then could not enable the timer is a very
/// different situation from one that failed on the first command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The lines for the steps that completed.
    pub log: Vec<String>,
    /// The command or file that failed, and why.
    pub error: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

/// Create the plan's files and run its install commands.
///
/// Returns the lines to print, one per action taken.
pub fn install(plan: &Plan) -> Result<Vec<String>, Failure> {
    let mut log = Vec::new();
    for file in &plan.files {
        if let Some(parent) = file.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|err| Failure {
                    log: log.clone(),
                    error: format!("{}: {err}", parent.display()),
                })?;
            }
        }
        std::fs::write(&file.path, &file.body).map_err(|err| Failure {
            log: log.clone(),
            error: format!("{}: {err}", file.path.display()),
        })?;
        log.push(format!("Wrote {}", file.path.display()));
    }
    let mut cleanup_failed = false;
    for argv in &plan.enable {
        match run(argv) {
            Ok(line) => log.push(line),
            // The sequence always opens by clearing a previous install's timer.
            // "There is nothing to clear" is what stop/disable report as a
            // failure (`not loaded` / `does not exist`), and its wording is
            // localised, so it is not parsed — the command itself is what says
            // it is a cleanup step. Those failures are summarised once below
            // instead of being shown as errors; every other command must
            // succeed.
            Err(_) if is_cleanup(argv) => cleanup_failed = true,
            Err(error) => return Err(Failure { log, error }),
        }
    }
    if cleanup_failed {
        log.push("No previous timer to clear".to_string());
    }
    Ok(log)
}

/// Whether this argv is a `systemctl … stop`/`disable` — the two commands whose
/// failure means "there was nothing to clean up", not "the install broke".
fn is_cleanup(argv: &[String]) -> bool {
    argv.first()
        .is_some_and(|program| program.ends_with("systemctl"))
        && argv.iter().any(|word| word == "stop" || word == "disable")
}

/// Run the plan's disable commands, then remove the files it owns.
///
/// A disable command that fails is stepped over with a one-line summary: the
/// files still have to go, and "the unit was not loaded" is the ordinary case
/// for an install that never enabled a timer. Removing a file that is already
/// absent counts as success.
pub fn uninstall(plan: &Plan) -> Result<Vec<String>, Failure> {
    let mut log = Vec::new();
    let mut cleanup_failed = false;
    for argv in &plan.disable {
        match run(argv) {
            Ok(line) => log.push(line),
            Err(_) if is_cleanup(argv) => cleanup_failed = true,
            Err(err) => log.push(format!("Ignored: {err}")),
        }
    }
    if cleanup_failed {
        log.push("Nothing was enabled".to_string());
    }
    for file in &plan.files {
        match std::fs::remove_file(&file.path) {
            Ok(()) => log.push(format!("Removed {}", file.path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                log.push(format!("Already gone: {}", file.path.display()));
            }
            Err(err) => {
                return Err(Failure {
                    log,
                    error: format!("{}: {err}", file.path.display()),
                })
            }
        }
    }
    // Only the directory the plan's files live in, and only when nothing else
    // is left in it; a failure here is the normal "not empty" case. The walk
    // stops there on purpose — ascending further would start deleting
    // directories this crate has no business removing.
    let mut parents: Vec<&Path> = plan
        .files
        .iter()
        .filter_map(|file| file.path.parent())
        .filter(|parent| !parent.as_os_str().is_empty())
        .collect();
    parents.dedup();
    for parent in parents {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(log)
}

/// Report what is installed: each owned file against what a fresh plan would
/// write, then the platform's own view of the task.
///
/// The queries are read-only and their failures are part of the answer ("not
/// installed"), so they are reported rather than returned as errors.
pub fn status(plan: &Plan) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for owned in &plan.owned {
        let expected = plan
            .files
            .iter()
            .find(|file| file.path == *owned)
            .map(|file| file.body.as_str());
        let state = match (std::fs::read_to_string(owned), expected) {
            // A file the plan does not write at all (the timer with no
            // schedule) is not "missing" — it is deliberately absent.
            (Err(err), None) if err.kind() == std::io::ErrorKind::NotFound => {
                "not installed (no schedule)".to_string()
            }
            (Err(err), _) if err.kind() == std::io::ErrorKind::NotFound => "missing".to_string(),
            (Err(err), _) => format!("unreadable: {err}"),
            (Ok(_), None) => "present (not part of this schedule)".to_string(),
            (Ok(text), Some(expected)) if text == expected => "up to date".to_string(),
            (Ok(_), Some(_)) => "differs".to_string(),
        };
        lines.push(format!("{}: {state}", owned.display()));
    }
    for argv in &plan.query {
        match Command::new(&argv[0]).args(&argv[1..]).output() {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_string();
                let err = String::from_utf8_lossy(&output.stderr);
                let err = err.trim_end();
                if !err.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(err);
                }
                lines.push(format!("$ {}", argv.join(" ")));
                for line in text.lines() {
                    lines.push(format!("  {line}"));
                }
            }
            Err(err) => lines.push(format!("$ {}: {err}", argv.join(" "))),
        }
    }
    Ok(lines)
}

/// Run one argv, reporting it the way [`install`] returns it.
fn run(argv: &[String]) -> Result<String, String> {
    let line = argv.join(" ");
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|err| format!("{line} failed: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr.to_string()
        };
        return Err(format!("{line} failed: {detail}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim_end();
    Ok(if stdout.is_empty() {
        format!("Ran {line}")
    } else {
        format!("Ran {line}: {stdout}")
    })
}

/// How the task can start before a login, and the one step that needs
/// privileges — reported, never taken.
///
/// On Linux this also asks the user manager to keep running without a session
/// (`loginctl enable-linger`), which is unprivileged when polkit allows it for
/// the active session and otherwise needs an explicit `pkexec`. Linger is only
/// touched when something installed actually runs on its own: with both cadences
/// off there is no automatic start to enable it for, and changing a login-wide
/// setting to serve nothing would be an unasked-for side effect.
pub fn startup_note(spec: &Spec, user: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        if spec.interval_secs.is_none() && spec.boot_delay_secs.is_none() {
            return format!(
                "Linger: not changed - nothing runs automatically; \
                 `systemctl --user start {LABEL}.service` runs it by hand"
            );
        }
        if user.is_empty() {
            return "Linger: unknown user - run: loginctl enable-linger <user>".to_string();
        }
        let argv = ["loginctl", "enable-linger", user];
        match run(&argv.map(String::from)) {
            Ok(_) => "Linger: enabled (the reconnect task runs without a login)".to_string(),
            Err(_) => format!(
                "Linger: not enabled - run: pkexec loginctl enable-linger {user} \
                 (otherwise the task starts at your next login)"
            ),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (spec, user);
        "Boot before login needs a root LaunchDaemon; the agent above runs at login.".to_string()
    }
    #[cfg(target_os = "windows")]
    {
        let _ = (spec, user);
        "The logon task runs at login; /SC ONSTART needs an administrator.".to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (spec, user);
        "No startup hook is known for this system.".to_string()
    }
}

/// Escape one argument for a systemd `ExecStart=` line: systemd splits on
/// whitespace, so an argument carrying any needs quoting.
pub fn quote_unit_arg(value: &str) -> String {
    if value.is_empty() || value.chars().any(|c| matches!(c, ' ' | '\t' | '"' | '\\')) {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

/// Escape text for an XML text node (the launchd plist).
pub fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
    use super::{argv, user_config_dir, FileSpec, Plan, Spec, LABEL};

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
    ///   counts from the previous run has nothing to count from, so
    ///   `list-timers` reports an empty `NEXT` after `enable --now`.
    /// * A timer with **no trigger at all is not merely idle** — systemd rejects
    ///   the unit outright (`Timer unit lacks value setting. Refusing.`), it
    ///   lands in `LoadState=bad-setting`, and even `systemctl start` fails. So
    ///   "run only by hand" writes **no timer at all**, just the service, which
    ///   starts fine on its own. A placeholder trigger was tried and rejected:
    ///   a far-future `OnCalendar` does not parse (year 9999 → `Failed to parse
    ///   calendar specification`), and any date systemd accepts would eventually
    ///   fire.
    /// * `OnActiveSec=` **does** arm on its own, from the moment the timer is
    ///   activated — the interval-only case uses it.
    pub fn plan(spec: &Spec) -> Plan {
        let dir = user_config_dir(spec).join("systemd").join("user");
        let service = dir.join(format!("{LABEL}.service"));
        let timer = dir.join(format!("{LABEL}.timer"));
        let exe = super::quote_unit_arg(&spec.exe.to_string_lossy());
        let config = super::quote_unit_arg(&spec.config.to_string_lossy());

        let service_body = format!(
            "[Unit]\nDescription=Srun portal reconnect check\n\n\
             [Service]\nType=oneshot\nExecStart={exe} --config {config} reconnect\n"
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

        let unit = format!("{LABEL}.timer");
        // With no schedule at all there is no timer to write: systemd refuses a
        // trigger-less timer outright (`Timer unit lacks value setting`), and a
        // placeholder trigger would be a lie — it would either fire or need a
        // far-future date systemd will not even parse. Only the service is
        // installed, and it is started by hand.
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

        // A previous install may have written and enabled the timer (its
        // `timers.target.wants` symlink survives rewriting the units), so the
        // first steps always take it back out. `disable` without `--now` is
        // idempotent; `stop` is not, and install tolerates its failure.
        let mut enable = vec![
            argv(&["systemctl", "--user", "stop", &unit]),
            argv(&["systemctl", "--user", "disable", &unit]),
        ];
        if !manual_only {
            enable.push(argv(&["systemctl", "--user", "daemon-reload"]));
            enable.push(argv(&["systemctl", "--user", "enable", "--now", &unit]));
        }
        // `disable` is idempotent; `stop` is not (it reports "not loaded" for a
        // unit systemd never loaded), so both run and uninstall tolerates the
        // failure — the files still have to go.
        let disable = vec![
            argv(&["systemctl", "--user", "stop", &unit]),
            argv(&["systemctl", "--user", "disable", &unit]),
        ];
        Plan {
            files,
            enable,
            disable,
            query: if manual_only {
                // Nothing to ask about a timer that was not written.
                vec![argv(&[
                    "systemctl",
                    "--user",
                    "cat",
                    &format!("{LABEL}.service"),
                ])]
            } else {
                vec![
                    argv(&["systemctl", "--user", "is-enabled", &unit]),
                    argv(&["systemctl", "--user", "is-active", &unit]),
                    argv(&["systemctl", "--user", "list-timers", &unit, "--no-pager"]),
                ]
            },
            owned: vec![service, timer],
            cadence: spec.cadence(),
        }
    }
}

/// The launchd LaunchAgent.
pub mod launchd {
    use super::{argv, FileSpec, Plan, Spec};

    /// `~/Library/LaunchAgents/com.srun-portal.reconnect.plist`.
    ///
    /// The two schedule keys are independent in launchd, so each maps to the
    /// matching configuration key: `RunAtLoad` is the "at login" half
    /// (`reconnect_boot_delay_secs` gates it) and `StartInterval` the repeating
    /// one (`reconnect_interval_secs`). launchd has no per-user equivalent of a
    /// boot trigger, so anything before login would have to be a root
    /// LaunchDaemon (reported by [`super::startup_note`]).
    pub fn plan(spec: &Spec) -> Plan {
        let dir = spec.home.join("Library").join("LaunchAgents");
        let plist = dir.join("com.srun-portal.reconnect.plist");
        let log = spec
            .home
            .join("Library")
            .join("Logs")
            .join("srun-portal.log");
        let label = "com.srun-portal.reconnect";
        let (exe, config) = (
            super::xml_escape(&spec.exe.to_string_lossy()),
            super::xml_escape(&spec.config.to_string_lossy()),
        );
        let log = super::xml_escape(&log.to_string_lossy());

        // `RunAtLoad` with a delay, when one was asked for.
        let startup = match spec.boot_delay_secs {
            Some(_) => "\x20   <key>RunAtLoad</key>\n\x20   <true/>\n".to_string(),
            None => String::new(),
        };
        let interval = match spec.interval_secs {
            Some(interval) => {
                format!("\x20   <key>StartInterval</key>\n\x20   <integer>{interval}</integer>\n")
            }
            None => String::new(),
        };

        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \x20   <key>Label</key>\n\
             \x20   <string>{label}</string>\n\
             \x20   <key>ProgramArguments</key>\n\
             \x20   <array>\n\
             \x20       <string>{exe}</string>\n\
             \x20       <string>--config</string>\n\
             \x20       <string>{config}</string>\n\
             \x20       <string>reconnect</string>\n\
             \x20   </array>\n\
             {startup}{interval}\
             \x20   <key>ProcessType</key>\n\
             \x20   <string>Background</string>\n\
             \x20   <key>StandardOutPath</key>\n\
             \x20   <string>{log}</string>\n\
             \x20   <key>StandardErrorPath</key>\n\
             \x20   <string>{log}</string>\n\
             </dict>\n\
             </plist>\n"
        );
        let uid = uid();
        Plan {
            files: vec![FileSpec {
                path: plist.clone(),
                body,
            }],
            enable: vec![argv(&[
                "launchctl",
                "bootstrap",
                &format!("gui/{uid}"),
                &plist.to_string_lossy(),
            ])],
            disable: vec![argv(&[
                "launchctl",
                "bootout",
                &format!("gui/{uid}/{label}"),
            ])],
            query: vec![argv(&["launchctl", "list", label])],
            owned: vec![plist],
            cadence: spec.cadence(),
        }
    }

    /// The uid `gui/<uid>` domains are named after. The CLI is a per-user
    /// process, so the real uid is the session's owner.
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
}

/// The Windows Task Scheduler entries, driven with `schtasks.exe`.
pub mod scheduler {
    use super::{Plan, Spec, WINDOWS_BOOT_TASK, WINDOWS_TIMER_TASK};

    /// One task per enabled schedule, because `schtasks` cannot express "at
    /// logon *and* every N minutes" as a single trigger: `/SC ONLOGON` covers
    /// the session start (`reconnect_boot_delay_secs`) and `/SC MINUTE` the
    /// repetition (`reconnect_interval_secs`). Each configuration key switches
    /// its own task, and `/RL LIMITED` keeps both unprivileged.
    ///
    /// `schtasks` counts minutes, so a sub-minute interval is rounded up to one
    /// minute, which is also its minimum.
    ///
    /// An empty schedule leaves no task at all, which `schtasks /Delete` on a
    /// missing name would report as a failure; `uninstall` ignores that.
    pub fn plan(spec: &Spec) -> Plan {
        let exe = windows_arg(&spec.exe.to_string_lossy());
        let config = windows_arg(&spec.config.to_string_lossy());
        let action = format!("{exe} --config {config} reconnect");
        let create = |task: &str, extra: &[&str]| {
            let mut argv = vec!["schtasks", "/Create", "/F", "/RL", "LIMITED", "/TN", task];
            argv.extend_from_slice(extra);
            argv.extend_from_slice(&["/TR", &action]);
            super::argv(&argv)
        };

        let mut enable = Vec::new();
        let mut disable = Vec::new();
        let mut query = Vec::new();
        if let Some(interval) = spec.interval_secs {
            let minutes = interval.div_ceil(60).max(1).to_string();
            enable.push(create(
                WINDOWS_TIMER_TASK,
                &["/SC", "MINUTE", "/MO", &minutes],
            ));
            disable.push(super::argv(&[
                "schtasks",
                "/Delete",
                "/F",
                "/TN",
                WINDOWS_TIMER_TASK,
            ]));
            query.push(super::argv(&[
                "schtasks",
                "/Query",
                "/TN",
                WINDOWS_TIMER_TASK,
                "/V",
                "/FO",
                "LIST",
            ]));
        }
        if spec.boot_delay_secs.is_some() {
            enable.push(create(WINDOWS_BOOT_TASK, &["/SC", "ONLOGON"]));
            disable.push(super::argv(&[
                "schtasks",
                "/Delete",
                "/F",
                "/TN",
                WINDOWS_BOOT_TASK,
            ]));
            query.push(super::argv(&[
                "schtasks",
                "/Query",
                "/TN",
                WINDOWS_BOOT_TASK,
                "/V",
                "/FO",
                "LIST",
            ]));
        }
        Plan {
            files: Vec::new(),
            enable,
            disable,
            query,
            owned: Vec::new(),
            cadence: spec.cadence(),
        }
    }

    /// A `/TR` value is one command line, so a path with a space has to be
    /// quoted the way the Windows command line parser expects.
    fn windows_arg(value: &str) -> String {
        if value.contains([' ', '\t']) {
            format!("\"{value}\"")
        } else {
            value.to_string()
        }
    }
}

/// The account the task runs as, for [`startup_note`].
pub fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

/// Whether `exe` looks like a build artefact, which the task would then depend
/// on: a `target/` directory is the cargo one, and it is transient.
///
/// The path is absolute either way — `current_exe()` guarantees that — so this
/// only decides whether to say so out loud.
pub fn note_for_executable(exe: &Path) -> Option<String> {
    let sep = std::path::MAIN_SEPARATOR;
    let is_build_dir = exe
        .components()
        .any(|component| component.as_os_str() == "target")
        && exe.to_string_lossy().contains(&format!("{sep}target{sep}"));
    if !is_build_dir {
        return None;
    }
    Some(format!(
        "Note: {} is a build artefact; move the binary somewhere stable and \
         reinstall before relying on this task",
        exe.display()
    ))
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            exe: PathBuf::from("/opt/a/srun-portal"),
            config: PathBuf::from("/home/u/.config/srun-portal/srun-portal.toml"),
            interval_secs: Some(300),
            boot_delay_secs: Some(30),
            home: PathBuf::from("/home/u"),
        }
    }

    #[test]
    fn systemd_units_are_exact() {
        let plan = systemd::plan(&spec());

        let service = PathBuf::from("/home/u/.config/systemd/user/srun-portal.service");
        let timer = PathBuf::from("/home/u/.config/systemd/user/srun-portal.timer");
        assert_eq!(
            plan.files,
            vec![
                FileSpec {
                    path: service.clone(),
                    body: "[Unit]\nDescription=Srun portal reconnect check\n\n\
                           [Service]\nType=oneshot\n\
                           ExecStart=/opt/a/srun-portal --config \
                           /home/u/.config/srun-portal/srun-portal.toml reconnect\n"
                        .to_string(),
                },
                FileSpec {
                    path: timer.clone(),
                    body: "[Unit]\nDescription=Check the Srun portal connection every 300s\n\n\
                           [Timer]\nOnStartupSec=30s\nOnUnitActiveSec=300s\nAccuracySec=10s\n\
                           Unit=srun-portal.service\n\n[Install]\nWantedBy=timers.target\n"
                        .to_string(),
                },
            ]
        );
        assert_eq!(plan.owned, vec![service, timer]);
        assert_eq!(plan.cadence, "every 300s, 30s after startup");
        assert_eq!(
            plan.enable,
            vec![
                vec!["systemctl", "--user", "stop", "srun-portal.timer"],
                vec!["systemctl", "--user", "disable", "srun-portal.timer"],
                vec!["systemctl", "--user", "daemon-reload"],
                vec![
                    "systemctl",
                    "--user",
                    "enable",
                    "--now",
                    "srun-portal.timer"
                ],
            ]
        );
        assert_eq!(
            plan.disable,
            vec![
                vec!["systemctl", "--user", "stop", "srun-portal.timer"],
                vec!["systemctl", "--user", "disable", "srun-portal.timer"],
            ]
        );
        assert_eq!(
            plan.query,
            vec![
                vec!["systemctl", "--user", "is-enabled", "srun-portal.timer"],
                vec!["systemctl", "--user", "is-active", "srun-portal.timer"],
                vec![
                    "systemctl",
                    "--user",
                    "list-timers",
                    "srun-portal.timer",
                    "--no-pager"
                ],
            ]
        );
    }

    #[test]
    fn systemd_quotes_paths_with_spaces() {
        let mut spec = spec();
        spec.exe = PathBuf::from("/opt/a b/srun-portal");
        spec.config = PathBuf::from("/home/u/a\"b\\c.toml");
        let plan = systemd::plan(&spec);

        let body = &plan.files[0].body;
        assert!(
            body.contains(
                "ExecStart=\"/opt/a b/srun-portal\" --config \
                 \"/home/u/a\\\"b\\\\c.toml\" reconnect\n"
            ),
            "{body}"
        );
    }

    #[test]
    fn systemd_timer_without_startup_trigger_needs_onactivesec() {
        // `OnUnitActiveSec` only counts from a previous run, so an
        // interval-only timer would never arm at all. `OnActiveSec` is what
        // makes the first run happen. (Measured: `systemctl --user enable --now`
        // on an `OnUnitActiveSec`-only timer leaves `NEXT` empty.)
        let mut spec = spec();
        spec.boot_delay_secs = None;
        let plan = systemd::plan(&spec);

        let timer = &plan.files[1].body;
        assert!(!timer.contains("OnStartupSec"), "{timer}");
        assert!(
            timer.contains("OnActiveSec=1s\nOnUnitActiveSec=300s\n"),
            "{timer}"
        );
        assert_eq!(plan.cadence, "every 300s");
    }

    #[test]
    fn both_cadences_off_installs_a_manual_only_task() {
        let mut manual = spec();
        manual.interval_secs = None;
        manual.boot_delay_secs = None;
        assert_eq!(manual.cadence(), "only when started by hand");

        // systemd: only the service is written. A timer with no trigger is
        // rejected by systemd (`Timer unit lacks value setting`), and a
        // placeholder trigger would be a lie, so there is simply no timer.
        let plan = systemd::plan(&manual);
        assert_eq!(plan.files.len(), 1, "{:?}", plan.files);
        assert!(
            plan.files[0].path.ends_with("srun-portal.service"),
            "{:?}",
            plan.files
        );
        assert!(plan.files[0].body.contains("reconnect\n"));
        // A previous install may have left an enabled timer behind, so it is
        // stopped and disabled; nothing is enabled in its place.
        assert!(
            !plan
                .enable
                .iter()
                .any(|a| a.contains(&"enable".to_string())),
            "{:?}",
            plan.enable
        );
        assert!(
            plan.enable.iter().any(|a| a.contains(&"stop".to_string())),
            "{:?}",
            plan.enable
        );
        // `status` has no timer to ask about.
        assert!(
            !plan
                .query
                .iter()
                .any(|a| a.contains(&"list-timers".to_string())),
            "{:?}",
            plan.query
        );
        // The timer path is still owned, so a leftover file is cleaned up.
        assert!(
            plan.owned.iter().any(|p| p.ends_with("srun-portal.timer")),
            "{:?}",
            plan.owned
        );

        // launchd: no schedule keys at all, so bootstrap loads a job with no
        // triggers.
        let plan = launchd::plan(&manual);
        let plist = &plan.files[0].body;
        assert!(!plist.contains("RunAtLoad"), "{plist}");
        assert!(!plist.contains("StartInterval"), "{plist}");
        assert!(
            plist.contains("ProgramArguments"),
            "the job is still loaded"
        );

        // Windows: no tasks are created or deleted.
        let plan = scheduler::plan(&manual);
        assert!(plan.enable.is_empty());
        assert!(plan.disable.is_empty());
        assert!(plan.query.is_empty());
    }

    #[test]
    fn one_cadence_at_a_time_is_expressible() {
        // Interval only.
        let mut one = spec();
        one.boot_delay_secs = None;
        assert_eq!(one.cadence(), "every 300s");
        let plan = launchd::plan(&one);
        assert!(!plan.files[0].body.contains("RunAtLoad"));
        assert!(plan.files[0].body.contains("<key>StartInterval</key>"));
        let plan = scheduler::plan(&one);
        assert_eq!(plan.enable.len(), 1, "only the interval task: {plan:?}");
        assert!(plan.enable[0].contains(&"srun-portal-reconnect".to_string()));
        assert!(!plan.enable[0].contains(&"srun-portal-reconnect-boot".to_string()));

        // Startup only.
        let mut one = spec();
        one.interval_secs = None;
        assert_eq!(one.cadence(), "30s after startup only");
        let plan = systemd::plan(&one);
        let timer = &plan.files[1].body;
        assert!(timer.contains("OnStartupSec=30s"));
        assert!(!timer.contains("OnUnitActiveSec"), "{timer}");
        let plan = launchd::plan(&one);
        assert!(plan.files[0].body.contains("RunAtLoad"));
        assert!(!plan.files[0].body.contains("StartInterval"));
        let plan = scheduler::plan(&one);
        assert_eq!(plan.enable.len(), 1, "only the logon task: {plan:?}");
        assert!(plan.enable[0].contains(&"srun-portal-reconnect-boot".to_string()));
    }

    #[test]
    fn launchd_plist_is_exact() {
        let plan = launchd::plan(&spec());

        assert_eq!(plan.files.len(), 1);
        assert_eq!(
            plan.files[0].path,
            PathBuf::from("/home/u/Library/LaunchAgents/com.srun-portal.reconnect.plist")
        );
        assert_eq!(
            plan.files[0].body,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
             \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n\
             <dict>\n\
             \x20   <key>Label</key>\n\
             \x20   <string>com.srun-portal.reconnect</string>\n\
             \x20   <key>ProgramArguments</key>\n\
             \x20   <array>\n\
             \x20       <string>/opt/a/srun-portal</string>\n\
             \x20       <string>--config</string>\n\
             \x20       <string>/home/u/.config/srun-portal/srun-portal.toml</string>\n\
             \x20       <string>reconnect</string>\n\
             \x20   </array>\n\
             \x20   <key>RunAtLoad</key>\n\
             \x20   <true/>\n\
             \x20   <key>StartInterval</key>\n\
             \x20   <integer>300</integer>\n\
             \x20   <key>ProcessType</key>\n\
             \x20   <string>Background</string>\n\
             \x20   <key>StandardOutPath</key>\n\
             \x20   <string>/home/u/Library/Logs/srun-portal.log</string>\n\
             \x20   <key>StandardErrorPath</key>\n\
             \x20   <string>/home/u/Library/Logs/srun-portal.log</string>\n\
             </dict>\n\
             </plist>\n"
        );
        assert_eq!(plan.cadence, "every 300s, 30s after startup");
        assert_eq!(
            plan.query,
            vec![vec!["launchctl", "list", "com.srun-portal.reconnect"]]
        );
    }

    #[test]
    fn launchd_escapes_xml() {
        let mut spec = spec();
        spec.config = PathBuf::from("/home/u/a&b<c>.toml");
        let plan = launchd::plan(&spec);

        assert!(plan.files[0]
            .body
            .contains("<string>/home/u/a&amp;b&lt;c&gt;.toml</string>"));
        assert!(!plan.files[0].body.contains("a&b<c>"));
    }

    #[test]
    fn windows_tasks_are_exact() {
        let plan = scheduler::plan(&spec());
        let action =
            "/opt/a/srun-portal --config /home/u/.config/srun-portal/srun-portal.toml reconnect";

        assert!(plan.files.is_empty());
        assert!(plan.owned.is_empty());
        assert_eq!(
            plan.enable,
            vec![
                vec![
                    "schtasks",
                    "/Create",
                    "/F",
                    "/RL",
                    "LIMITED",
                    "/TN",
                    "srun-portal-reconnect",
                    "/SC",
                    "MINUTE",
                    "/MO",
                    "5",
                    "/TR",
                    action,
                ],
                vec![
                    "schtasks",
                    "/Create",
                    "/F",
                    "/RL",
                    "LIMITED",
                    "/TN",
                    "srun-portal-reconnect-boot",
                    "/SC",
                    "ONLOGON",
                    "/TR",
                    action,
                ],
            ]
        );
        assert_eq!(
            plan.disable,
            vec![
                vec!["schtasks", "/Delete", "/F", "/TN", "srun-portal-reconnect"],
                vec![
                    "schtasks",
                    "/Delete",
                    "/F",
                    "/TN",
                    "srun-portal-reconnect-boot"
                ],
            ]
        );
        assert_eq!(plan.cadence, "every 300s, 30s after startup");

        // A sub-minute interval rounds up to `schtasks`' own minimum.
        let mut spec = spec();
        spec.interval_secs = Some(100);
        assert_eq!(
            scheduler::plan(&spec).cadence,
            "every 100s, 30s after startup"
        );
        spec.interval_secs = Some(1);
        let plan = scheduler::plan(&spec);
        let argv = &plan.enable[0];
        let minutes = argv[argv.iter().position(|a| a == "/MO").unwrap() + 1].clone();
        assert_eq!(minutes, "1", "1s rounds up to /MO 1: {plan:?}");
    }

    #[test]
    fn windows_quotes_paths_with_spaces() {
        let mut spec = spec();
        spec.exe = PathBuf::from("C:\\Program Files\\srun\\srun-portal.exe");
        let plan = scheduler::plan(&spec);

        let create = &plan.enable[1];
        let action = create.last().unwrap();
        assert_eq!(
            action,
            "\"C:\\Program Files\\srun\\srun-portal.exe\" --config \
             /home/u/.config/srun-portal/srun-portal.toml reconnect"
        );
    }

    #[test]
    fn plan_dispatches_to_this_platform() {
        let plan = plan(&spec());
        #[cfg(target_os = "linux")]
        assert_eq!(plan, systemd::plan(&spec()));
        #[cfg(target_os = "macos")]
        assert_eq!(plan, launchd::plan(&spec()));
        #[cfg(target_os = "windows")]
        assert_eq!(plan, scheduler::plan(&spec()));
    }

    #[test]
    fn quote_unit_arg_quotes_only_what_needs_it() {
        assert_eq!(quote_unit_arg("/opt/a"), "/opt/a");
        assert_eq!(quote_unit_arg("a b"), "\"a b\"");
        assert_eq!(quote_unit_arg("a\tb"), "\"a\tb\"");
        assert_eq!(quote_unit_arg(""), "\"\"");
        assert_eq!(quote_unit_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_unit_arg("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn xml_escape_covers_the_three_entities() {
        assert_eq!(xml_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        assert_eq!(xml_escape("plain/path"), "plain/path");
    }

    /// A scratch directory with the files a plan owns, removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let unique = format!(
                "srun-portal-service-{}-{}-{}",
                tag,
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn install_writes_files_and_reports() {
        let dir = TempDir::new("install");
        let nested = dir.path.join("a").join("b").join("unit.conf");
        let plan = Plan {
            files: vec![FileSpec {
                path: nested.clone(),
                body: "hello\n".to_string(),
            }],
            enable: vec![vec!["/bin/true".to_string()]],
            query: Vec::new(),
            owned: vec![nested.clone()],
            cadence: "soon".to_string(),
            ..Plan::default()
        };

        let log = install(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "hello\n");
        assert_eq!(
            log,
            vec![
                format!("Wrote {}", nested.display()),
                "Ran /bin/true".to_string(),
            ]
        );

        // Status compares the file with what the plan would write.
        assert_eq!(
            status(&plan).unwrap(),
            vec![format!("{}: up to date", nested.display())]
        );
        std::fs::write(&nested, "stale\n").unwrap();
        assert_eq!(
            status(&plan).unwrap(),
            vec![format!("{}: differs", nested.display())]
        );
        std::fs::remove_file(&nested).unwrap();
        assert_eq!(
            status(&plan).unwrap(),
            vec![format!("{}: missing", nested.display())]
        );

        // Uninstall removes the file and prunes the directory it lived in.
        assert_eq!(install(&plan).unwrap().len(), 2);
        let log = uninstall(&plan).unwrap();
        assert_eq!(log, vec![format!("Removed {}", nested.display())]);
        assert!(!nested.exists());
        assert!(
            !dir.path.join("a").join("b").exists(),
            "the emptied parent is pruned"
        );
        assert!(
            dir.path.join("a").exists(),
            "and nothing above it is touched"
        );

        // Doing it again is not an error: nothing is left.
        let log = uninstall(&plan).unwrap();
        assert_eq!(log, vec![format!("Already gone: {}", nested.display())]);
    }

    #[test]
    fn install_reports_a_failing_command() {
        let dir = TempDir::new("install-fail");
        let file = dir.path.join("unit.conf");
        let plan = Plan {
            files: vec![FileSpec {
                path: file.clone(),
                body: "x".to_string(),
            }],
            enable: vec![vec!["/bin/false".to_string()]],
            ..Plan::default()
        };
        let failure = install(&plan).unwrap_err();
        // The file was written before the command failed, and that is reported.
        assert_eq!(failure.log, vec![format!("Wrote {}", file.display())]);
        assert!(
            failure.error.starts_with("/bin/false failed:"),
            "{}",
            failure.error
        );
        assert!(file.exists(), "the written file is not rolled back");
    }

    #[test]
    fn install_summarises_cleanup_instead_of_erroring() {
        let dir = TempDir::new("cleanup");
        // A stand-in for systemd's client: the predicate keys off the program
        // name, so the name has to be real even though the binary is not.
        let fake = dir.path.join("systemctl");
        std::fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let fake = fake.to_string_lossy().into_owned();

        // A failing command that is *not* the systemd cleanup pair is fatal…
        let plan = Plan {
            enable: vec![vec!["/bin/false".to_string()]],
            ..Plan::default()
        };
        assert!(install(&plan).is_err());

        // …while the cleanup pair is summarised and the install carries on.
        let file = dir.path.join("unit.conf");
        let plan = Plan {
            files: vec![FileSpec {
                path: file.clone(),
                body: "x".to_string(),
            }],
            enable: vec![
                vec![
                    fake.clone(),
                    "--user".to_string(),
                    "stop".to_string(),
                    "srun-portal.timer".to_string(),
                ],
                vec!["/bin/true".to_string()],
            ],
            ..Plan::default()
        };
        let log = install(&plan).unwrap();
        assert_eq!(
            log,
            vec![
                format!("Wrote {}", file.display()),
                "Ran /bin/true".to_string(),
                "No previous timer to clear".to_string(),
            ]
        );
    }

    #[test]
    fn uninstall_survives_a_failing_disable() {
        let dir = TempDir::new("uninstall");
        let file = dir.path.join("unit.conf");
        std::fs::write(&file, "x").unwrap();
        let plan = Plan {
            files: vec![FileSpec {
                path: file.clone(),
                body: "x".to_string(),
            }],
            disable: vec![
                vec!["/bin/false".to_string()],
                vec!["/bin/echo".to_string(), "bye".to_string()],
            ],
            ..Plan::default()
        };

        let log = uninstall(&plan).unwrap();
        assert!(log[0].starts_with("Ignored: /bin/false failed:"), "{log:?}");
        assert_eq!(log[1], "Ran /bin/echo bye: bye");
        assert_eq!(log[2], format!("Removed {}", file.display()));
        assert!(!file.exists());
    }

    #[test]
    fn status_reports_query_output() {
        let plan = Plan {
            query: vec![vec!["/bin/echo".to_string(), "state".to_string()]],
            ..Plan::default()
        };
        assert_eq!(
            status(&plan).unwrap(),
            vec!["$ /bin/echo state".to_string(), "  state".to_string()]
        );
    }
}
