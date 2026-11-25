use crate::credentials::Target;
use crate::settings;
use std::path::PathBuf;

/// The options the CLI accepts, before any of them has been acted on.
#[derive(Debug)]
pub(super) struct Args {
    pub(super) portal_url: Option<String>,
    pub(super) config: Option<PathBuf>,
    pub(super) portable: bool,
    pub(super) help: bool,
    help_topic: Option<HelpTopic>,
    pub(super) version: bool,
    /// `--json` for the commands that have a machine-readable form.
    pub(super) json: bool,
    /// The hidden `--control-dir` of the installed entry point.
    pub(super) control_dir: Option<PathBuf>,
    pub(super) setting_options: Vec<(String, String)>,
    pub(super) command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpTopic {
    Global,
    Reconnect,
    Status,
    Disconnect,
    Resume,
    Service,
    ServiceInstall,
    ServiceUninstall,
    ServiceStatus,
    ServiceStart,
    ServiceStop,
    ServiceRestart,
    ServiceLogs,
    Config,
    ConfigShow,
    ConfigList,
    ConfigSet,
    ConfigUnset,
    ConfigValidate,
    Credentials,
    CredentialsSet,
    CredentialsForget,
    CredentialsStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Command {
    Reconnect,
    Status,
    Disconnect,
    Resume,
    Service(ServiceAction),
    Config(ConfigAction),
    Credentials(CredentialsAction),
    /// The entry point the installed task runs.
    Background {
        trigger: Trigger,
        startup_delay_secs: Option<u64>,
    },
    /// The child that performs one network pass, driven over stdin.
    Worker,
}

/// Which native trigger started a background run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Trigger {
    Interval,
    Startup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServiceAction {
    Install,
    Uninstall,
    Status,
    Start,
    Stop,
    Restart,
    Logs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ConfigAction {
    Show,
    List,
    Set(Vec<(String, String)>),
    Unset(Vec<String>),
    Validate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CredentialsAction {
    Set(Target),
    Forget { legacy: bool },
    Status,
}

pub(super) const USAGE: &str = r#"Usage: srun-portal [options] [portal-url]
       srun-portal [options] reconnect|status|disconnect|resume
       srun-portal [options] service install|uninstall|status|start|stop|restart|logs
       srun-portal [options] config show|list|set|unset|validate
       srun-portal [options] credentials set|forget|status

Options:
  --config <path>   use exactly this configuration file
  --portable        prefer the configuration next to the executable
  --json            machine-readable output (where a command has one)
  --version, -V     print the version and exit
  --help, -h        print this help and exit

Interactive/reconnect/service install settings:
  --portal-url URL
  --username USERNAME[@DOMAIN]
  --timeout-ms MS
Service install only:
  --reconnect-interval-secs SECONDS
  --reconnect-boot-delay-secs SECONDS

Commands:
  reconnect              check the connection once and log in when needed
  status                 report whether this device is online, and the account panel
  disconnect             pause the background task and sign out
  resume                 clear the pause and check the connection now
  service install        install the background reconnect task for this system
  service uninstall      remove it and keep credentials
  service status         show what is installed, paused, and what the system says
  service start          clear the pause and check now (requires an install)
  service stop           pause the task and stop a run in progress
  service restart        stop, then start
  service logs           show the recent events
  config show            print every effective setting and where it comes from
  config list            print the keys the written file sets
  config set KEY VALUE   change one setting (repeatable)
  config unset KEY       remove one setting (repeatable)
  config validate        check that the effective configuration is usable
  credentials set        store the password for this portal and account
  credentials forget     delete it (--legacy: also the old unscoped entries)
  credentials status     where the password comes from, if anywhere

Environment:
  SRUN_PORTAL_CONFIG     configuration file path (same as --config)
  SRUN_PORTAL_PASSWORD   password for reconnect/disconnect (overrides the stored one)
  SRUN_PORTAL_STATE_DIR  control directory (install records, pause, history)
"#;

fn setting_option(arg: &str) -> Option<(&'static str, Option<&str>)> {
    let (name, inline) = arg
        .split_once('=')
        .map_or((arg, None), |(name, value)| (name, Some(value)));
    let key = match name {
        "--portal-url" => "portal_url",
        "--username" => "username",
        "--timeout-ms" => "timeout_ms",
        "--reconnect-interval-secs" => "reconnect_interval_secs",
        "--reconnect-boot-delay-secs" => "reconnect_boot_delay_secs",
        _ => return None,
    };
    Some((key, inline))
}

pub(super) fn parse(argv: &[String]) -> Result<Args, String> {
    let mut args = Args {
        portal_url: None,
        config: None,
        portable: false,
        help: false,
        help_topic: None,
        version: false,
        json: false,
        control_dir: None,
        setting_options: Vec::new(),
        command: None,
    };
    let mut rest = argv.iter();
    while let Some(arg) = rest.next() {
        let arg = arg.as_str();
        if let Some((key, inline)) = setting_option(arg) {
            let value = match inline {
                Some(value) => value.to_string(),
                None => rest
                    .next()
                    .ok_or_else(|| format!("{arg} needs a value"))?
                    .to_string(),
            };
            settings::parse_key_value(key, &value)
                .map_err(|err| format!("{arg} {value}: {err}"))?;
            args.setting_options.push((key.to_string(), value));
            continue;
        }

        // `--json` is a global switch: it is accepted anywhere a command is, and
        // rejected afterwards for the commands that have no JSON form.
        if arg == "--json" {
            args.json = true;
            continue;
        }

        if args.command.is_none() && arg == "reconnect" {
            args.command = Some(Command::Reconnect);
            continue;
        }
        if args.command.is_none() && arg == "status" {
            args.command = Some(Command::Status);
            continue;
        }
        if args.command.is_none() && arg == "disconnect" {
            args.command = Some(Command::Disconnect);
            continue;
        }
        if args.command.is_none() && arg == "resume" {
            args.command = Some(Command::Resume);
            continue;
        }
        if args.command.is_none() && arg == "service" {
            let action = rest.next().ok_or_else(|| {
                "service needs an action: install, uninstall, status, start, stop, restart or logs"
                    .to_string()
            })?;
            if matches!(action.as_str(), "-h" | "--help") {
                args.help = true;
                args.help_topic = Some(HelpTopic::Service);
                continue;
            }
            let action = match action.as_str() {
                "install" => ServiceAction::Install,
                "uninstall" => ServiceAction::Uninstall,
                "status" => ServiceAction::Status,
                "start" => ServiceAction::Start,
                "stop" => ServiceAction::Stop,
                "restart" => ServiceAction::Restart,
                "logs" => ServiceAction::Logs,
                other => return Err(format!("Unknown service action: {other}")),
            };
            args.command = Some(Command::Service(action));
            continue;
        }
        if args.command.is_none() && arg == "config" {
            let action = rest.next().ok_or_else(|| {
                "config needs an action: show, list, set, unset or validate".to_string()
            })?;
            if matches!(action.as_str(), "-h" | "--help") {
                args.help = true;
                args.help_topic = Some(HelpTopic::Config);
                continue;
            }
            if matches!(action.as_str(), "set" | "unset")
                && rest
                    .clone()
                    .next()
                    .is_some_and(|next| matches!(next.as_str(), "-h" | "--help"))
            {
                let _ = rest.next();
                if rest.next().is_some() {
                    return Err("Unexpected argument after --help".to_string());
                }
                args.help = true;
                args.help_topic = Some(if action == "set" {
                    HelpTopic::ConfigSet
                } else {
                    HelpTopic::ConfigUnset
                });
                continue;
            }
            let action = match action.as_str() {
                "show" => ConfigAction::Show,
                "list" => ConfigAction::List,
                "validate" => ConfigAction::Validate,
                "set" => {
                    let words: Vec<String> = rest.by_ref().cloned().collect();
                    if words.is_empty() || !words.len().is_multiple_of(2) {
                        return Err(
                            "config set needs KEY VALUE pairs, e.g. `config set username testuser`"
                                .to_string(),
                        );
                    }
                    ConfigAction::Set(
                        words
                            .chunks(2)
                            .map(|pair| (pair[0].clone(), pair[1].clone()))
                            .collect(),
                    )
                }
                "unset" => {
                    let words: Vec<String> = rest.by_ref().cloned().collect();
                    if words.is_empty() {
                        return Err("config unset needs at least one KEY".to_string());
                    }
                    let keys = words
                        .into_iter()
                        .map(|word| {
                            if let Some((key, None)) = setting_option(&word) {
                                Ok(key.to_string())
                            } else if word.starts_with("--") {
                                Err(format!("Unknown configuration option: {word}"))
                            } else {
                                Ok(word)
                            }
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    ConfigAction::Unset(keys)
                }
                other => return Err(format!("Unknown config action: {other}")),
            };
            args.command = Some(Command::Config(action));
            continue;
        }
        if args.command.is_none() && arg == "credentials" {
            let action = rest
                .next()
                .ok_or_else(|| "credentials needs an action: set, forget or status".to_string())?;
            if matches!(action.as_str(), "-h" | "--help") {
                args.help = true;
                args.help_topic = Some(HelpTopic::Credentials);
                continue;
            }
            let action = match action.as_str() {
                "set" => {
                    let mut target = Target::Auto;
                    while let Some(next) = rest.clone().next() {
                        let next = next.as_str();
                        if next == "--store" {
                            let _ = rest.next();
                            let value = rest.next().ok_or("--store needs a value")?;
                            target = parse_store(value)?;
                        } else if let Some(value) = next.strip_prefix("--store=") {
                            let _ = rest.next();
                            target = parse_store(value)?;
                        } else if next == "--json" {
                            let _ = rest.next();
                            args.json = true;
                        } else if matches!(next, "-h" | "--help") {
                            args.help = true;
                            args.help_topic = Some(HelpTopic::CredentialsSet);
                            break;
                        } else {
                            return Err(format!("Unexpected argument: {next}"));
                        }
                    }
                    if args.help {
                        continue;
                    }
                    CredentialsAction::Set(target)
                }
                "forget" => {
                    let mut legacy = false;
                    if let Some(next) = rest.clone().next() {
                        if matches!(next.as_str(), "-h" | "--help") {
                            let _ = rest.next();
                            args.help = true;
                            args.help_topic = Some(HelpTopic::CredentialsForget);
                            continue;
                        }
                        if next == "--legacy" {
                            let _ = rest.next();
                            legacy = true;
                        } else {
                            return Err(format!("Unexpected argument: {next}"));
                        }
                    }
                    CredentialsAction::Forget { legacy }
                }
                "status" => CredentialsAction::Status,
                other => return Err(format!("Unknown credentials action: {other}")),
            };
            args.command = Some(Command::Credentials(action));
            continue;
        }
        // The installed entry point and its worker. Both are internal: they are
        // recognised so the native task can run this same binary, and are absent
        // from the help text.
        if args.command.is_none() && arg == "__worker" {
            args.command = Some(Command::Worker);
            continue;
        }
        if args.command.is_none() && arg == "__background" {
            let mut trigger = Trigger::Interval;
            let mut startup_delay = None;
            while let Some(next) = rest.next() {
                match next.as_str() {
                    "--trigger" => {
                        let value = rest.next().ok_or("--trigger needs a value")?;
                        trigger = match value.as_str() {
                            "interval" => Trigger::Interval,
                            "startup" => Trigger::Startup,
                            other => return Err(format!("Unknown trigger: {other}")),
                        };
                    }
                    "--startup-delay-secs" => {
                        let value = rest.next().ok_or("--startup-delay-secs needs a value")?;
                        startup_delay = Some(
                            value
                                .parse::<u64>()
                                .map_err(|_| "--startup-delay-secs needs seconds".to_string())?,
                        );
                    }
                    "--control-dir" => {
                        let value = rest.next().ok_or("--control-dir needs a path")?;
                        args.control_dir = Some(PathBuf::from(value));
                    }
                    other => return Err(format!("Unexpected argument: {other}")),
                }
            }
            args.command = Some(Command::Background {
                trigger,
                startup_delay_secs: startup_delay,
            });
            continue;
        }
        match arg {
            "--help" | "-h" => args.help = true,
            "--version" | "-V" => args.version = true,
            "--portable" => args.portable = true,
            "--config" => {
                let value = rest
                    .next()
                    .ok_or_else(|| "--config needs a path".to_string())?;
                args.config = Some(PathBuf::from(value));
            }
            "--control-dir" => {
                let value = rest
                    .next()
                    .ok_or_else(|| "--control-dir needs a path".to_string())?;
                args.control_dir = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--config=") => {
                let value = &arg["--config=".len()..];
                if value.is_empty() {
                    return Err("--config needs a path".to_string());
                }
                args.config = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--control-dir=") => {
                let value = &arg["--control-dir=".len()..];
                if value.is_empty() {
                    return Err("--control-dir needs a path".to_string());
                }
                args.control_dir = Some(PathBuf::from(value));
            }
            _ if arg.starts_with('-') && arg.len() > 1 => {
                return Err(format!("Unknown option: {arg}"));
            }
            _ => {
                if args.command.is_some() || args.portal_url.is_some() {
                    return Err(format!("Unexpected argument: {arg}"));
                }
                args.portal_url = Some(arg.to_string());
            }
        }
    }
    validate(&args)?;
    Ok(args)
}

fn parse_store(value: &str) -> Result<Target, String> {
    match value {
        "auto" => Ok(Target::Auto),
        "file" => Ok(Target::File),
        "keyring" => Ok(Target::Keyring),
        other => Err(format!(
            "Unknown --store value: {other} (expected auto, file or keyring)"
        )),
    }
}

fn validate(args: &Args) -> Result<(), String> {
    if args.portal_url.is_some()
        && args
            .setting_options
            .iter()
            .any(|(key, _)| key == "portal_url")
    {
        return Err(
            "Portal URL cannot be specified both as a positional argument and with --portal-url"
                .to_string(),
        );
    }
    // `status` reads the URL the way a GUI supplies it: one explicit option, so a
    // positional URL that a command already made ambiguous is refused instead of
    // silently ignored.
    if args.command == Some(Command::Status) {
        if args.portal_url.is_some() {
            return Err(
                "status does not take a positional portal URL: use --portal-url".to_string(),
            );
        }
        if args.control_dir.is_some() {
            return Err("--control-dir is not valid for status".to_string());
        }
    }
    let supports_json = matches!(
        args.command,
        Some(Command::Reconnect)
            | Some(Command::Status)
            | Some(Command::Disconnect)
            | Some(Command::Resume)
            | Some(Command::Service(
                ServiceAction::Status | ServiceAction::Logs
            ))
            | Some(Command::Config(ConfigAction::Show | ConfigAction::Validate))
            | Some(Command::Credentials(CredentialsAction::Status))
    );
    if args.json && !supports_json {
        return Err("--json is not supported by this command".to_string());
    }
    if args.setting_options.is_empty() {
        return Ok(());
    }
    if !matches!(
        args.command,
        None | Some(Command::Reconnect)
            | Some(Command::Status)
            | Some(Command::Service(ServiceAction::Install))
    ) {
        return Err("setting options are not valid for this command".to_string());
    }
    for (key, _) in &args.setting_options {
        let allowed = match args.command {
            None | Some(Command::Reconnect) => {
                matches!(key.as_str(), "portal_url" | "username" | "timeout_ms")
            }
            Some(Command::Status) => matches!(key.as_str(), "portal_url" | "timeout_ms"),
            Some(Command::Service(ServiceAction::Install)) => true,
            _ => unreachable!("guarded above"),
        };
        if !allowed {
            let mode = match args.command {
                None => "interactive mode",
                Some(Command::Status) => "status",
                _ => "reconnect",
            };
            return Err(format!(
                "--{} is not valid for {mode}",
                key.replace('_', "-")
            ));
        }
    }
    Ok(())
}

fn selected_help_topic(args: &Args) -> HelpTopic {
    if let Some(topic) = args.help_topic {
        return topic;
    }
    match &args.command {
        Some(Command::Reconnect) => HelpTopic::Reconnect,
        Some(Command::Status) => HelpTopic::Status,
        Some(Command::Disconnect) => HelpTopic::Disconnect,
        Some(Command::Resume) => HelpTopic::Resume,
        Some(Command::Service(ServiceAction::Install)) => HelpTopic::ServiceInstall,
        Some(Command::Service(ServiceAction::Uninstall)) => HelpTopic::ServiceUninstall,
        Some(Command::Service(ServiceAction::Status)) => HelpTopic::ServiceStatus,
        Some(Command::Service(ServiceAction::Start)) => HelpTopic::ServiceStart,
        Some(Command::Service(ServiceAction::Stop)) => HelpTopic::ServiceStop,
        Some(Command::Service(ServiceAction::Restart)) => HelpTopic::ServiceRestart,
        Some(Command::Service(ServiceAction::Logs)) => HelpTopic::ServiceLogs,
        Some(Command::Config(ConfigAction::Show)) => HelpTopic::ConfigShow,
        Some(Command::Config(ConfigAction::List)) => HelpTopic::ConfigList,
        Some(Command::Config(ConfigAction::Set(_))) => HelpTopic::ConfigSet,
        Some(Command::Config(ConfigAction::Unset(_))) => HelpTopic::ConfigUnset,
        Some(Command::Config(ConfigAction::Validate)) => HelpTopic::ConfigValidate,
        Some(Command::Credentials(CredentialsAction::Set(_))) => HelpTopic::CredentialsSet,
        Some(Command::Credentials(CredentialsAction::Forget { .. })) => {
            HelpTopic::CredentialsForget
        }
        Some(Command::Credentials(CredentialsAction::Status)) => HelpTopic::CredentialsStatus,
        Some(Command::Background { .. }) | Some(Command::Worker) | None => HelpTopic::Global,
    }
}

pub(super) fn help_text(args: &Args) -> &'static str {
    match selected_help_topic(args) {
        HelpTopic::Global => USAGE,
        HelpTopic::Reconnect => {
            r#"Usage: srun-portal [options] reconnect

Check the portal connection once and log in when needed.
This command never prompts. It requires portal_url, username, and a
password from SRUN_PORTAL_PASSWORD or the stored credentials.

A pause from `disconnect`/`service stop` is respected: reconnect reports
`paused` and sends nothing until `resume`.

Output: `--json` prints {result, error_kind, message}.

Configuration overrides for this run:
  --portal-url URL              --username USERNAME[@DOMAIN]
  --timeout-ms MS

Options:
  --config <path>   use exactly this configuration file
  --portable        prefer the portable configuration location
  --json            machine-readable output
  --help, -h        print this help and exit
"#
        }
        HelpTopic::Status => {
            r#"Usage: srun-portal [options] status [--portal-url URL] [--timeout-ms MS] [--json]

Report whether this device is online right now, and the account panel when it
is. This command is read-only: it fetches the portal configuration page, then
asks the portal about this device's address once. It never authenticates, never
signs out, never retries, and does not read the stored password, the control
directory, the pause state or the event history.

Because nothing is written, `status` neither takes nor waits for the operation
lock and is unaffected by a pause from `disconnect`/`service stop`.

Online/offline is the portal's own answer for this device: `ok` is online and
`not_online_error` is offline. Any other answer is a failure, not an offline
device. The account comes from `rad_user_info` for this machine's address, so it
reports the session the portal attributes to this device.

The portal URL is `--portal-url`, then the merged configuration; the account is
not needed and is not read. Either URL form the portal publishes works,
including a `/srun_portal_success?...` page: only its origin and `ac_id` are
used, and that page is never requested.

Fields: username (`user_name@domain`), ip, used_flow_bytes (`sum_bytes`),
used_time_seconds (`sum_seconds`), balance (`user_balance`, the panel's currency
number, not converted), product_name (`products_name`). Counts are integers in
their native unit: bytes and seconds.

Output: `--json` prints one line, {schema_version, result, account}, where
result is "online" or "offline" and account is null unless online. Without
`--json`, online prints the account panel and offline prints `Offline.`.

Exit status:
  0   online or offline; the answer was obtained
  2   bad options or unusable configuration; no request was sent
  4   the query failed; nothing was printed on stdout
Diagnostics go to stderr only.
"#
        }
        HelpTopic::Disconnect => {
            r#"Usage: srun-portal [options] disconnect

Pause the background reconnect permanently, then sign this device out of
the portal. The pause is written before the sign-out is attempted, so a
failed sign-out still leaves the device offline by policy.

Offline is success. Undo with `srun-portal resume`.
"#
        }
        HelpTopic::Resume => {
            r#"Usage: srun-portal [options] resume

Clear the pause written by `disconnect`/`service stop`, then check the
connection immediately. A check that fails does not pause again: the next
scheduled run reports its own result.
"#
        }
        HelpTopic::Service => {
            r#"Usage: srun-portal [options] service <action>

Actions:
  install            register the background task (offline; no portal request)
  uninstall          remove the task; keep stored credentials and history
  status             what is installed, paused, and what the system reports
  start              clear the pause and check now (requires an install)
  stop               pause the task and stop a run in progress
  restart            stop, then start
  logs               the recent events (up to 200)

Use `srun-portal service <action> --help` for action-specific help.
Global options may precede `service`.
"#
        }
        HelpTopic::ServiceInstall => {
            r#"Usage: srun-portal [options] service install

Register the background reconnect task for this system. This step makes no
portal request of its own: it validates the configuration, stores a
password if one was supplied, and registers the task. The first real check
is the task's own next run, so `service install` succeeding says nothing
about whether the password is correct — run `reconnect` to find out.

When run interactively, each missing setting is prompted with its current
value as the default; press Enter to keep it.
The following action options are persisted in the selected configuration:

  --portal-url URL
  --username USERNAME[@DOMAIN]
  --timeout-ms MS
  --reconnect-interval-secs SECONDS
  --reconnect-boot-delay-secs SECONDS

Missing portal URL and username are prompted interactively. Password input is
never accepted as a command-line option; use SRUN_PORTAL_PASSWORD, an existing
stored password, or the secure prompt.

Equivalent configuration-file keys use underscores, for example:
  srun-portal config set reconnect_interval_secs 300 \
      reconnect_boot_delay_secs 30
"#
        }
        HelpTopic::ServiceUninstall => {
            r#"Usage: srun-portal [options] service uninstall

Remove the background reconnect task, its definitions and the record of the
installation. Stored credentials, the pause state and the event history are
kept; use `credentials forget` to delete a password.
"#
        }
        HelpTopic::ServiceStatus => {
            r#"Usage: srun-portal [options] service status

Show whether the task is installed, whether it is paused, whether a run is in
progress, the configuration files it reads, and the platform's own view of the
task. Nothing here reads a password or contacts the portal.

`--json` prints schema_version, installed, paused, installing, running,
config_sources, requested_interval_secs, effective_interval_secs,
boot_delay_secs, last_attempt, last_success_at_ms, platform, platform_state
and errors.
"#
        }
        HelpTopic::ServiceStart => {
            r#"Usage: srun-portal [options] service start

Clear the pause and check the connection immediately. Requires an installed
task; use `resume` when nothing is installed.
"#
        }
        HelpTopic::ServiceStop => {
            r#"Usage: srun-portal [options] service stop

Pause the background task and stop a run that is in progress. The task
definition stays registered, so the schedule resumes where it left off when
`service start` or `resume` clears the pause.
"#
        }
        HelpTopic::ServiceRestart => {
            r#"Usage: srun-portal [options] service restart

Stop (pause and terminate a run in progress), then start (clear the pause and
check now).
"#
        }
        HelpTopic::ServiceLogs => {
            r#"Usage: srun-portal [options] service logs

Print the recorded events, oldest first, at most 200. `--json` prints them as
an array. There is no follow mode: the record is bounded on purpose.
"#
        }
        HelpTopic::Config => {
            r#"Usage: srun-portal [options] config <action>

Actions: show, list, set KEY VALUE [...], unset KEY [...], validate
Use `srun-portal config <action> --help` for action-specific help.
"#
        }
        HelpTopic::ConfigShow => {
            r#"Usage: srun-portal [options] config show

Show every effective configuration value and which file supplies it.
`--json` prints {schema_version, values, sources, write_path}; `values` holds
the raw numbers, not display strings.
"#
        }
        HelpTopic::ConfigList => {
            r#"Usage: srun-portal [options] config list

Show only the keys the written configuration file sets.
"#
        }
        HelpTopic::ConfigSet => {
            r#"Usage: srun-portal [options] config set KEY VALUE [KEY VALUE ...]

Set one or more configuration values:
  portal_url
  username
  timeout_ms
  reconnect_interval_secs
  reconnect_boot_delay_secs

The whole batch is validated before anything is written; a rejected batch
leaves the file untouched. Changing a schedule key needs `service install`
again to reach the native task.

Examples:
  srun-portal config set username testuser@szu timeout_ms 10000
  srun-portal config set reconnect_interval_secs 300 \
      reconnect_boot_delay_secs 30
"#
        }
        HelpTopic::ConfigUnset => {
            r#"Usage: srun-portal [options] config unset KEY [KEY ...]

Remove one or more configuration values. Keys may use TOML names or the
command-line form:
  srun-portal config unset timeout_ms reconnect_interval_secs
  srun-portal config unset --timeout-ms --reconnect-interval-secs
"#
        }
        HelpTopic::ConfigValidate => {
            r#"Usage: srun-portal [options] config validate

Check that the effective configuration can be used: an http(s) portal URL, a
non-empty account, a timeout between 1 and 60000 ms, an interval up to 86340 s
and a startup delay up to 86400 s. Exit status 2 means the configuration is
wrong.
"#
        }
        HelpTopic::Credentials => {
            r#"Usage: srun-portal [options] credentials <action>

Actions:
  set [--store auto|file|keyring]   store the password for this portal and account
  forget [--legacy]                 delete it, or also the old unscoped entries
  status                            where the password comes from

Credentials are keyed by portal origin and account. `set` reads the password
from SRUN_PORTAL_PASSWORD or a secure prompt; it never contacts the portal.
"#
        }
        HelpTopic::CredentialsSet => {
            r#"Usage: srun-portal [options] credentials set [--store auto|file|keyring]

Store the password for the configured portal and account.

  auto      the system keyring, falling back to the private file with a warning
  file      the private file only
  keyring   the system keyring only; a failure is an error

The password comes from SRUN_PORTAL_PASSWORD or a secure prompt. No request is
sent to the portal, so a stored password is not a verified one.
"#
        }
        HelpTopic::CredentialsForget => {
            r#"Usage: srun-portal [options] credentials forget [--legacy]

Delete the stored password for the configured portal and account from both
the keyring and the private file. Other accounts are kept.

  --legacy   also delete the old unscoped entries: the keyring entry keyed by
             the bare account, the top-level `password` of the credentials file
             beside the configuration, and that account's pre-v2 key. Nothing
             is migrated to a portal automatically; save the password again.
"#
        }
        HelpTopic::CredentialsStatus => {
            r#"Usage: srun-portal [options] credentials status

Report where the password for the configured portal and account comes from:
the environment, the private file, the keyring, or not at all. `--json` prints
{state, detail?}. No secret is printed.
"#
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_string()).collect()
    }

    #[test]
    fn parse_args_reads_options_and_one_url() {
        let args = parse(&strings(&[
            "--portable",
            "--config",
            "/tmp/settings.toml",
            "https://h/srun_portal_pc?ac_id=1",
        ]))
        .unwrap();
        assert!(args.portable);
        assert_eq!(args.config, Some(PathBuf::from("/tmp/settings.toml")));
        assert_eq!(
            args.portal_url.as_deref(),
            Some("https://h/srun_portal_pc?ac_id=1")
        );
    }

    #[test]
    fn parse_args_reads_the_commands() {
        assert_eq!(
            parse(&strings(&["reconnect"])).unwrap().command,
            Some(Command::Reconnect)
        );
        assert_eq!(
            parse(&strings(&["disconnect"])).unwrap().command,
            Some(Command::Disconnect)
        );
        assert_eq!(
            parse(&strings(&["resume"])).unwrap().command,
            Some(Command::Resume)
        );
        assert_eq!(
            parse(&strings(&["service", "install"])).unwrap().command,
            Some(Command::Service(ServiceAction::Install))
        );
        assert_eq!(
            parse(&strings(&["service", "logs"])).unwrap().command,
            Some(Command::Service(ServiceAction::Logs))
        );
        assert_eq!(
            parse(&strings(&["config", "validate"])).unwrap().command,
            Some(Command::Config(ConfigAction::Validate))
        );
        assert_eq!(
            parse(&strings(&["reconnect", "x"])).unwrap_err(),
            "Unexpected argument: x"
        );
        assert_eq!(
            parse(&strings(&["service", "status", "extra"])).unwrap_err(),
            "Unexpected argument: extra"
        );
        // The old `service forget` is gone, with no alias left behind.
        assert_eq!(
            parse(&strings(&["service", "forget"])).unwrap_err(),
            "Unknown service action: forget"
        );
    }

    #[test]
    fn credentials_actions_parse_their_options() {
        assert_eq!(
            parse(&strings(&["credentials", "set"])).unwrap().command,
            Some(Command::Credentials(CredentialsAction::Set(Target::Auto)))
        );
        assert_eq!(
            parse(&strings(&["credentials", "set", "--store", "file"]))
                .unwrap()
                .command,
            Some(Command::Credentials(CredentialsAction::Set(Target::File)))
        );
        assert_eq!(
            parse(&strings(&["credentials", "set", "--store=keyring"]))
                .unwrap()
                .command,
            Some(Command::Credentials(CredentialsAction::Set(
                Target::Keyring
            )))
        );
        assert_eq!(
            parse(&strings(&["credentials", "forget", "--legacy"]))
                .unwrap()
                .command,
            Some(Command::Credentials(CredentialsAction::Forget {
                legacy: true
            }))
        );
        assert!(parse(&strings(&["credentials", "set", "--store", "cloud"])).is_err());
        assert!(parse(&strings(&["credentials", "frobnicate"])).is_err());
    }

    #[test]
    fn internal_entry_points_parse() {
        let args = parse(&strings(&[
            "__background",
            "--control-dir",
            "/tmp/state",
            "--trigger",
            "startup",
            "--startup-delay-secs",
            "2",
        ]))
        .unwrap();
        assert_eq!(args.control_dir, Some(PathBuf::from("/tmp/state")));
        assert_eq!(
            args.command,
            Some(Command::Background {
                trigger: Trigger::Startup,
                startup_delay_secs: Some(2),
            })
        );
        assert_eq!(
            parse(&strings(&["__worker"])).unwrap().command,
            Some(Command::Worker)
        );
        assert!(parse(&strings(&["__background", "--trigger", "minutely"])).is_err());
    }

    #[test]
    fn json_is_only_accepted_where_it_means_something() {
        for argv in [
            strings(&["reconnect", "--json"]),
            strings(&["disconnect", "--json"]),
            strings(&["resume", "--json"]),
            strings(&["service", "status", "--json"]),
            strings(&["service", "logs", "--json"]),
            strings(&["config", "show", "--json"]),
            strings(&["config", "validate", "--json"]),
            strings(&["credentials", "status", "--json"]),
        ] {
            assert!(parse(&argv).unwrap().json, "{argv:?}");
        }
        for argv in [
            strings(&["service", "install", "--json"]),
            strings(&["config", "list", "--json"]),
            strings(&["--json", "config", "set", "username", "a"]),
            strings(&["credentials", "set", "--json"]),
            strings(&["--json"]),
        ] {
            assert_eq!(
                parse(&argv).unwrap_err(),
                "--json is not supported by this command",
                "{argv:?}"
            );
        }
    }

    #[test]
    fn version_and_help_are_flags() {
        assert!(parse(&strings(&["--version"])).unwrap().version);
        assert!(parse(&strings(&["-V"])).unwrap().version);
        assert!(parse(&strings(&["--help"])).unwrap().help);
    }

    #[test]
    fn a_url_is_still_a_url() {
        let args = parse(&strings(&["https://h/srun_portal_pc?ac_id=1"])).unwrap();
        assert_eq!(args.command, None);
        assert_eq!(
            args.portal_url.as_deref(),
            Some("https://h/srun_portal_pc?ac_id=1")
        );
    }

    #[test]
    fn positional_and_option_portal_urls_are_mutually_exclusive() {
        let expected =
            "Portal URL cannot be specified both as a positional argument and with --portal-url";
        assert_eq!(
            parse(&strings(&[
                "https://positional.example/srun_portal_pc?ac_id=1",
                "--portal-url",
                "https://option.example/srun_portal_pc?ac_id=2",
            ]))
            .unwrap_err(),
            expected
        );
    }

    #[test]
    fn cadence_options_are_rejected_in_interactive_mode_in_any_position() {
        for argv in [
            strings(&["--reconnect-interval-secs", "60"]),
            strings(&[
                "https://h/srun_portal_pc?ac_id=1",
                "--reconnect-interval-secs",
                "60",
            ]),
        ] {
            let error = parse(&argv).unwrap_err();
            assert!(
                error.contains("is not valid for interactive mode"),
                "{error}"
            );
        }
    }

    #[test]
    fn service_install_accepts_cadence_options_before_and_after_command() {
        assert!(parse(&strings(&[
            "--reconnect-interval-secs",
            "60",
            "service",
            "install"
        ]))
        .is_ok());
        assert!(parse(&strings(&[
            "service",
            "install",
            "--reconnect-boot-delay-secs",
            "30"
        ]))
        .is_ok());
    }

    #[test]
    fn setting_options_are_rejected_where_they_do_nothing() {
        assert_eq!(
            parse(&strings(&["service", "status", "--username", "alice"])).unwrap_err(),
            "setting options are not valid for this command"
        );
    }

    #[test]
    fn interactive_settings_remain_valid() {
        let args = parse(&strings(&[
            "--portal-url",
            "https://h/srun_portal_pc?ac_id=1",
            "--username",
            "alice@staff",
            "--timeout-ms",
            "12000",
        ]))
        .unwrap();
        assert_eq!(args.setting_options.len(), 3);
    }

    #[test]
    fn status_is_a_command_with_its_own_settings() {
        for argv in [
            strings(&["status"]),
            strings(&["status", "--json"]),
            strings(&["--json", "status"]),
            strings(&[
                "status",
                "--portal-url",
                "https://h/srun_portal_success?ac_id=12",
            ]),
            strings(&["--portal-url=https://h/srun_portal_pc?ac_id=1", "status"]),
            strings(&["status", "--timeout-ms=3000"]),
            strings(&["--timeout-ms", "3000", "status"]),
        ] {
            assert_eq!(
                parse(&argv).unwrap().command,
                Some(Command::Status),
                "{argv:?}"
            );
        }
        assert_eq!(
            parse(&strings(&["status", "--timeout-ms", "3000"]))
                .unwrap()
                .setting_options,
            vec![("timeout_ms".to_string(), "3000".to_string())]
        );
        assert_eq!(
            parse(&strings(&["status", "--json"])).unwrap().help_topic,
            None
        );
        assert!(parse(&strings(&["status", "--json"])).unwrap().json);
    }

    #[test]
    fn status_rejects_what_it_does_not_use() {
        let args = parse(&strings(&["status", "--username", "alice"])).unwrap_err();
        assert_eq!(args, "--username is not valid for status");
        // A URL given before the command becomes the positional one, which this
        // command never reads.
        assert_eq!(
            parse(&strings(&["https://h/srun_portal_pc?ac_id=1", "status"])).unwrap_err(),
            "status does not take a positional portal URL: use --portal-url"
        );
        assert_eq!(
            parse(&strings(&["status", "--reconnect-interval-secs", "60"])).unwrap_err(),
            "--reconnect-interval-secs is not valid for status"
        );
        assert_eq!(
            parse(&strings(&["status", "--control-dir", "/tmp/state"])).unwrap_err(),
            "--control-dir is not valid for status"
        );
        // The command still takes no positional value of its own.
        assert_eq!(
            parse(&strings(&["status", "extra"])).unwrap_err(),
            "Unexpected argument: extra"
        );
    }

    #[test]
    fn every_status_help_request_reaches_the_status_topic() {
        for argv in [
            strings(&["status", "--help"]),
            strings(&["--help", "status"]),
        ] {
            let args = parse(&argv).unwrap();
            assert!(args.help, "{argv:?}");
            assert!(
                help_text(&args).starts_with("Usage: srun-portal [options] status"),
                "{argv:?}"
            );
        }
    }
}
