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
    pub(super) setting_options: Vec<(String, String)>,
    pub(super) command: Option<Command>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpTopic {
    Global,
    Reconnect,
    Service,
    ServiceInstall,
    ServiceUninstall,
    ServiceStatus,
    ServiceForget,
    Config,
    ConfigShow,
    ConfigList,
    ConfigSet,
    ConfigUnset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Command {
    Reconnect,
    Service(ServiceAction),
    Config(ConfigAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServiceAction {
    Install,
    Uninstall,
    Status,
    Forget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ConfigAction {
    Show,
    List,
    Set(Vec<(String, String)>),
    Unset(Vec<String>),
}

pub(super) const USAGE: &str = r#"Usage: srun-portal [options] [portal-url]
       srun-portal [options] reconnect
       srun-portal [options] service install|uninstall|status|forget
       srun-portal [options] config show|list|set|unset

Options:
  --config <path>   use exactly this configuration file (created when missing)
  --portable        keep the configuration next to the executable
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
  service install        install the background reconnect task for this system
  service uninstall      remove it
  service status         show what is installed and what the system says
  service forget         delete the stored password (keyring and file)
  config show            print every effective setting
  config list             print the keys the configuration file itself sets
  config set KEY VALUE   change one setting (repeatable)
  config unset KEY       remove one setting (repeatable)

Environment:
  SRUN_PORTAL_CONFIG     configuration file path (same as --config)
  SRUN_PORTAL_PASSWORD   password for `reconnect` (overrides the stored one)
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

        if args.command.is_none() && arg == "reconnect" {
            args.command = Some(Command::Reconnect);
            continue;
        }
        if args.command.is_none() && arg == "service" {
            let action = rest.next().ok_or_else(|| {
                "service needs an action: install, uninstall, status or forget".to_string()
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
                "forget" => ServiceAction::Forget,
                other => return Err(format!("Unknown service action: {other}")),
            };
            args.command = Some(Command::Service(action));
            continue;
        }
        if args.command.is_none() && arg == "config" {
            let action = rest
                .next()
                .ok_or_else(|| "config needs an action: show, list, set or unset".to_string())?;
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
        match arg {
            "--help" | "-h" => args.help = true,
            "--portable" => args.portable = true,
            "--config" => {
                let value = rest
                    .next()
                    .ok_or_else(|| "--config needs a path".to_string())?;
                args.config = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--config=") => {
                let value = &arg["--config=".len()..];
                if value.is_empty() {
                    return Err("--config needs a path".to_string());
                }
                args.config = Some(PathBuf::from(value));
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
    validate_setting_options(&args)?;
    Ok(args)
}

fn validate_setting_options(args: &Args) -> Result<(), String> {
    if args.setting_options.is_empty() {
        return Ok(());
    }
    if matches!(
        args.command.as_ref(),
        Some(Command::Service(
            ServiceAction::Uninstall | ServiceAction::Status | ServiceAction::Forget
        )) | Some(Command::Config(_))
    ) {
        return Err("setting options are not valid for this command".to_string());
    }
    for (key, _) in &args.setting_options {
        let allowed = match args.command.as_ref() {
            None | Some(Command::Reconnect) => {
                matches!(key.as_str(), "portal_url" | "username" | "timeout_ms")
            }
            Some(Command::Service(ServiceAction::Install)) => true,
            Some(Command::Service(_)) | Some(Command::Config(_)) => unreachable!(),
        };
        if !allowed {
            let mode = if args.command.is_none() {
                "interactive mode"
            } else {
                "reconnect"
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
        Some(Command::Service(ServiceAction::Install)) => HelpTopic::ServiceInstall,
        Some(Command::Service(ServiceAction::Uninstall)) => HelpTopic::ServiceUninstall,
        Some(Command::Service(ServiceAction::Status)) => HelpTopic::ServiceStatus,
        Some(Command::Service(ServiceAction::Forget)) => HelpTopic::ServiceForget,
        Some(Command::Config(ConfigAction::Show)) => HelpTopic::ConfigShow,
        Some(Command::Config(ConfigAction::List)) => HelpTopic::ConfigList,
        Some(Command::Config(ConfigAction::Set(_))) => HelpTopic::ConfigSet,
        Some(Command::Config(ConfigAction::Unset(_))) => HelpTopic::ConfigUnset,
        None => HelpTopic::Global,
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

Configuration overrides for this run:
  --portal-url URL              --username USERNAME[@DOMAIN]
  --timeout-ms MS

Options:
  --config <path>   use exactly this configuration file
  --portable        use the portable configuration location
  --help, -h        print this help and exit
"#
        }
        HelpTopic::Service => {
            r#"Usage: srun-portal [options] service <action>

Actions:
  install            validate credentials and install the task
  uninstall          remove the task; keep stored credentials
  status             show task and credential status
  forget             delete stored credentials

Use `srun-portal service <action> --help` for action-specific help.
Global options may precede `service`.
"#
        }
        HelpTopic::ServiceInstall => {
            r#"Usage: srun-portal [options] service install

Validate credentials and install the background reconnect task.
When run interactively, each missing or existing setting is prompted with its
current value as the default; press Enter to keep it.
The following action options are persisted in the selected configuration:

  --portal-url URL
  --username USERNAME[@DOMAIN]
  --timeout-ms MS
  --reconnect-interval-secs SECONDS
  --reconnect-boot-delay-secs SECONDS

For the timeout and reconnect options, values are milliseconds or seconds:
  --timeout-ms 10000
  --reconnect-interval-secs 300
  --reconnect-boot-delay-secs 30

Missing portal URL and username are prompted interactively. Password input is
never accepted as a command-line option; use SRUN_PORTAL_PASSWORD, an existing
stored password, or the secure prompt.

The command performs a login/check before writing the credential or
installing the task.

Equivalent configuration-file keys use underscores, for example:
  srun-portal config set reconnect_interval_secs 300 \
      reconnect_boot_delay_secs 30

Global options:
  --config <path>   use exactly this configuration file
  --portable        use the portable configuration location
  --help, -h        print this help and exit
"#
        }
        HelpTopic::ServiceUninstall => {
            r#"Usage: srun-portal [options] service uninstall

Remove the background reconnect task. Stored credentials are kept;
use `service forget` to delete them.
"#
        }
        HelpTopic::ServiceStatus => {
            r#"Usage: srun-portal [options] service status

Show the effective configuration, credential source, and task state.
"#
        }
        HelpTopic::ServiceForget => {
            r#"Usage: srun-portal [options] service forget

Delete the stored password from the system keyring and fallback file.
"#
        }
        HelpTopic::Config => {
            r#"Usage: srun-portal [options] config <action>

Actions: show, list, set KEY VALUE [...], unset KEY [...]
Use `srun-portal config <action> --help` for action-specific help.
"#
        }
        HelpTopic::ConfigShow => {
            r#"Usage: srun-portal [options] config show

Show every effective configuration value.
"#
        }
        HelpTopic::ConfigList => {
            r#"Usage: srun-portal [options] config list

Show only keys stored in the selected configuration file.
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
    fn parse_args_reads_the_config_command() {
        let args = parse(&strings(&["config", "set", "username", "alice"])).unwrap();
        assert_eq!(
            args.command,
            Some(Command::Config(ConfigAction::Set(vec![(
                "username".to_string(),
                "alice".to_string()
            )])))
        );
        let args = parse(&strings(&["config", "unset", "--timeout-ms"])).unwrap();
        assert_eq!(
            args.command,
            Some(Command::Config(ConfigAction::Unset(vec![
                "timeout_ms".to_string()
            ])))
        );
    }

    #[test]
    fn parse_args_reads_the_commands() {
        assert_eq!(
            parse(&strings(&["reconnect"])).unwrap().command,
            Some(Command::Reconnect)
        );
        assert_eq!(
            parse(&strings(&["service", "install"])).unwrap().command,
            Some(Command::Service(ServiceAction::Install))
        );
        assert_eq!(
            parse(&strings(&["reconnect", "x"])).unwrap_err(),
            "Unexpected argument: x"
        );
        assert_eq!(
            parse(&strings(&["service", "status", "extra"])).unwrap_err(),
            "Unexpected argument: extra"
        );
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
    fn cadence_options_are_rejected_in_interactive_mode_in_any_position() {
        for argv in [
            strings(&["--reconnect-interval-secs", "60"]),
            strings(&[
                "https://h/srun_portal_pc?ac_id=1",
                "--reconnect-interval-secs",
                "60",
            ]),
            strings(&[
                "--reconnect-boot-delay-secs",
                "30",
                "https://h/srun_portal_pc?ac_id=1",
            ]),
            strings(&[
                "https://h/srun_portal_pc?ac_id=1",
                "--reconnect-boot-delay-secs",
                "30",
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
}
