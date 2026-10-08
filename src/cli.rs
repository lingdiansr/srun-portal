//! The interactive shell: SPEC §5's flow with the SPEC §10 message set.
//!
//! Presentation notes: the banner and the account panel follow the reference's
//! observable output (a `-`-ruled panel with the labels `Username`, `IP`,
//! `Used Flow`, `Used Time`, `Balance`, `Product Name`), while the decorations
//! that are presentation only — spinner frames, emoji markers, ANSI colours, the
//! cursor rewrites of the reference's prompt library — are not reproduced. None
//! of them carries protocol meaning; see `README.md`.
//!
//! Notice and agreement requests (`/v2/srun_portal_message`,
//! `/v1/srun_portal_agree_new`) are available in [`crate::api`] but are **not**
//! issued by this flow: the reference does not request them for the login and
//! sign-out paths, and issuing extra requests would break request-set equality.

use crate::api;
use crate::config;
use crate::credentials;
use crate::drcom::DrcomClient;
use crate::format;
use crate::keyring;
use crate::network::{self, NetworkMode};
use crate::reconnect::{self, domain_arg, fetch_config, host_of};
use crate::runtime::{Runtime, RuntimeOptions};
use crate::service;
use crate::settings;
use crate::sys;
use crate::transport;
use std::path::PathBuf;

/// The options the CLI accepts, before any of them has been acted on.
#[derive(Debug)]
struct Args {
    portal_url: Option<String>,
    config: Option<PathBuf>,
    portable: bool,
    help: bool,
    command: Option<Command>,
}

/// The subcommand, when one was given. The interactive flow is what happens
/// without one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// One "check, log in when needed" pass; what the installed task runs.
    Reconnect,
    /// Install, remove, or report the background task.
    Service(ServiceAction),
    /// Inspect or edit the configuration file.
    Config(ConfigAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceAction {
    Install,
    Uninstall,
    Status,
    /// Remove the stored password from every store that keeps one.
    Forget,
}

/// `srun-portal config …`: the configuration file, read and written through the
/// same key catalogue the loader uses, so a value typed here is a value that
/// loads.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigAction {
    /// Every key with its value: what is set now, after layering.
    Show,
    /// Just the keys the file itself sets.
    List,
    /// Set one or more `key value` pairs.
    Set(Vec<(String, String)>),
    /// Remove the named keys from the file.
    Unset(Vec<String>),
}

const USAGE: &str = "Usage: srun-portal [options] [portal-url]\n       srun-portal [options] reconnect\n       srun-portal [options] service install|uninstall|status|forget\n       srun-portal [options] config show|list|set|unset\n\nOptions:\n  --config <path>   use exactly this configuration file (created when missing)\n  --portable        keep the configuration next to the executable\n  --help, -h        print this help and exit\n\nCommands:\n  reconnect              check the connection once and log in when needed\n  service install        install the background reconnect task for this system\n  service uninstall      remove it\n  service status         show what is installed and what the system says\n  service forget         delete the stored password (keyring and file)\n  config show            print every effective setting\n  config list            print the keys the configuration file itself sets\n  config set KEY VALUE   change one setting (repeatable)\n  config unset KEY       remove one setting (repeatable)\n\nEnvironment:\n  SRUN_PORTAL_CONFIG     configuration file path (same as --config)\n  SRUN_PORTAL_PASSWORD   password for `reconnect` (overrides the stored one)\n";

/// Parse `argv` (without the program name). Unknown options and a second
/// positional argument are usage errors, which the caller reports with
/// [`USAGE`] and a distinct exit status.
///
/// The first positional argument is compared against the command names first:
/// neither `reconnect` nor `service` can be a portal URL, so the interactive
/// case is unaffected. Everything after a command belongs to it, and only
/// `service` takes one more word — its action.
fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args {
        portal_url: None,
        config: None,
        portable: false,
        help: false,
        command: None,
    };
    let mut rest = argv.iter();
    while let Some(arg) = rest.next() {
        let arg = arg.as_str();
        if args.command.is_none() && arg == "reconnect" {
            args.command = Some(Command::Reconnect);
            continue;
        }
        if args.command.is_none() && arg == "service" {
            let action = rest.next().ok_or_else(|| {
                "service needs an action: install, uninstall, status or forget".to_string()
            })?;
            args.command = Some(Command::Service(match action.as_str() {
                "install" => ServiceAction::Install,
                "uninstall" => ServiceAction::Uninstall,
                "status" => ServiceAction::Status,
                "forget" => ServiceAction::Forget,
                other => return Err(format!("Unknown service action: {other}")),
            }));
            continue;
        }
        if args.command.is_none() && arg == "config" {
            let action = rest
                .next()
                .ok_or_else(|| "config needs an action: show, list, set or unset".to_string())?;
            let action = match action.as_str() {
                "show" => ConfigAction::Show,
                "list" => ConfigAction::List,
                // `set` and `unset` take the rest of the command line: a key
                // name is never an option, so nothing here can be mistaken for
                // one, and `set` needs pairs rather than a fixed arity.
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
                    let keys: Vec<String> = rest.by_ref().cloned().collect();
                    if keys.is_empty() {
                        return Err("config unset needs at least one KEY".to_string());
                    }
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
                // After a command there is no positional argument left to take:
                // the URL is the interactive form's argument only.
                if args.command.is_some() || args.portal_url.is_some() {
                    return Err(format!("Unexpected argument: {arg}"));
                }
                args.portal_url = Some(arg.to_string());
            }
        }
    }
    Ok(args)
}

/// Run the CLI; the returned value is the process exit status.
pub fn run(argv: Vec<String>) -> i32 {
    let args = match parse_args(&argv) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprint!("{USAGE}");
            return 2;
        }
    };
    if args.help {
        print!("{USAGE}");
        return 0;
    }

    // The unattended forms are the same process with a different entry point:
    // no banner, no prompts (stdout goes to a service manager's journal), and
    // one meaningful line per event.
    match &args.command {
        Some(Command::Reconnect) => return run_reconnect(&args),
        Some(Command::Service(action)) => return run_service(*action, &args),
        Some(Command::Config(action)) => return run_config(action, &args),
        None => {}
    }

    sys::install_sigint_handler();
    println!("{}", crate::messages::BANNER);

    let mut resolution = match load_settings(&args) {
        Ok(loaded) => loaded,
        Err(status) => return status,
    };
    if let Some(path) = resolution.imported_legacy.as_deref() {
        eprintln!("Imported {}", path.display());
    }
    // One-shot: the agent reads these when it is first built. Failing means the
    // timeouts were already fixed, which is not an error here.
    let _ = transport::set_timeouts(resolution.effective.timeouts);
    if let Err(status) = ensure_network_setting(&mut resolution) {
        return status;
    }
    let persist_portal = should_persist_portal(&args, &resolution);

    let slug = match resolve_slug(&args, &resolution) {
        Ok(slug) => slug,
        Err(status) => return status,
    };
    let target = match config::parse_portal_target(&slug) {
        Ok(target) => target,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    if let config::PortalTarget::Drcom(portal) = &target {
        if persist_portal {
            if let Some(base) = resolution.base.as_mut() {
                if let Err(err) = base.update(&[("portal_url", toml::Value::String(slug.clone()))])
                {
                    eprintln!("{err}");
                }
            }
        }
        return run_drcom_interactive(portal, &resolution);
    }
    let config::PortalTarget::Srun(portal) = target else {
        unreachable!("Dr.COM target returned above");
    };

    let cfg = match fetch_config(&portal) {
        Ok(cfg) => cfg,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };

    // The configured URL is what the next run reads first, so it is recorded in
    // the file that was read first: the project file, or the system one.
    if persist_portal {
        if let Some(base) = resolution.base.as_mut() {
            let slug = format!(
                "{}{}?ac_id={}",
                portal.origin,
                api::CONFIG_PATHNAME,
                portal.ac_id
            );
            if let Err(err) = base.update(&[("portal_url", toml::Value::String(slug))]) {
                eprintln!("{err}");
            }
        }
    }

    let mut options = RuntimeOptions::from_config(&cfg, &portal.origin, &host_of(&portal.origin));
    options.callback = resolution.effective.callback.clone();
    let mut runtime = Runtime::new(options);
    runtime.apply_interface_correction(&sys::network_interfaces(), &cfg.ip);
    runtime.spawn_other_stack_probe();

    let online = match runtime.check_online() {
        Ok(online) => online,
        Err(err) => {
            eprintln!("{}", err.render(&runtime.translate));
            return 1;
        }
    };

    if online {
        show_account_info(&runtime);
        match confirm("Do you want to sign out? (y/N): ") {
            Ok(true) => match runtime.sign_out() {
                Ok(()) => println!("Sign out success!"),
                Err(err) => println!("SignOut failed: {}", err.render(&runtime.translate)),
            },
            Ok(false) => {}
            Err(err) => {
                eprintln!("{err}");
                return 1;
            }
        }
        return 0;
    }

    // SPEC §5/§12 item 11: a rejected authentication prompts again, forever.
    loop {
        let label = match resolution.effective.username.as_deref() {
            Some(username) if !username.is_empty() => {
                format!("Please enter your username [{username}]: ")
            }
            _ => "Please enter your username: ".to_string(),
        };
        let entered = match prompt(&label) {
            Ok(value) => value,
            Err(status) => return status,
        };
        // An empty answer accepts the configured account.
        let username = if entered.trim().is_empty() {
            resolution.effective.username.clone().unwrap_or_default()
        } else {
            entered
        };
        let password = match password_prompt() {
            Ok(value) => value,
            Err(status) => return status,
        };
        let (account, typed_domain) = split_domain(&username);
        let domain = if typed_domain.is_empty() {
            domain_arg(resolution.effective.domain.as_deref())
        } else {
            typed_domain
        };
        match runtime.auth_by_password(&account, &password, &domain) {
            Ok(()) => {
                println!("Auth Success!");
                show_account_info(&runtime);
                return 0;
            }
            Err(err) => println!("Auth failed: {}", err.render(&runtime.translate)),
        }
    }
}

fn run_drcom_interactive(portal: &config::DrcomUrl, resolution: &settings::Resolution) -> i32 {
    let client = match DrcomClient::new(portal) {
        Ok(client) => client,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    let online = match client.check_online() {
        Ok(online) => online,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    if online {
        match confirm("Do you want to sign out? (y/N): ") {
            Ok(true) => match client.logout() {
                Ok(()) => println!("Sign out success!"),
                Err(err) => println!("SignOut failed: {err}"),
            },
            Ok(false) => {}
            Err(err) => {
                eprintln!("{err}");
                return 1;
            }
        }
        return 0;
    }

    loop {
        let label = match resolution.effective.username.as_deref() {
            Some(username) if !username.is_empty() => {
                format!("Please enter your username [{username}]: ")
            }
            _ => "Please enter your username: ".to_string(),
        };
        let entered = match prompt(&label) {
            Ok(value) => value,
            Err(status) => return status,
        };
        let username = if entered.trim().is_empty() {
            resolution.effective.username.clone().unwrap_or_default()
        } else {
            entered
        };
        let password = match password_prompt() {
            Ok(value) => value,
            Err(status) => return status,
        };
        let (account, typed_domain) = split_domain(&username);
        let domain = typed_domain;
        match client.login(&account, &password, &domain) {
            Ok(()) => {
                println!("Auth Success!");
                return 0;
            }
            Err(err) => println!("Auth failed: {err}"),
        }
    }
}

/// `srun-portal config …`: read and write the configuration file through the
/// key catalogue, so a value set here is a value the loader accepts.
///
/// The file that is written is the one the run is anchored on (the project file
/// when there is one, else the system file) — the same file the interactive flow
/// records the resolved portal URL in.
fn run_config(action: &ConfigAction, args: &Args) -> i32 {
    let mut resolution = match load_settings(args) {
        Ok(loaded) => loaded,
        Err(status) => return status,
    };

    match action {
        ConfigAction::Show => {
            let effective = &resolution.effective;
            for spec in settings::KEY_CATALOGUE {
                let value = effective_value(effective, spec.name);
                println!("{}: {value}", spec.name);
                println!("    {}", spec.about);
            }
            0
        }
        ConfigAction::List => {
            let entries = match resolution.base.as_ref() {
                Some(base) => base.entries(),
                None => Vec::new(),
            };
            if entries.is_empty() {
                println!("(nothing set in this file; the defaults apply)");
            }
            for (key, value) in entries {
                println!("{key} = {value}");
            }
            0
        }
        ConfigAction::Set(pairs) => {
            // Validate every pair before writing any: a typo in the last value
            // must not leave the first ones applied.
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
            let Some(base) = resolution.base.as_mut() else {
                eprintln!("No configuration file to write to");
                return 1;
            };
            match base.update(&changes) {
                Ok(_) => {
                    for (key, value) in &changes {
                        println!("{key} = {}", render_config_value(value));
                    }
                    0
                }
                Err(err) => {
                    eprintln!("{err}");
                    1
                }
            }
        }
        ConfigAction::Unset(keys) => {
            for key in keys {
                if settings::key_spec(key).is_none() {
                    eprintln!("Unknown configuration key: {key}");
                    return 2;
                }
            }
            let Some(base) = resolution.base.as_mut() else {
                eprintln!("No configuration file to write to");
                return 1;
            };
            match base.remove(&keys.iter().map(String::as_str).collect::<Vec<_>>()) {
                Ok(changed) => {
                    for key in keys {
                        if changed {
                            println!("Unset {key}");
                        } else {
                            println!("{key} was not set");
                        }
                    }
                    0
                }
                Err(err) => {
                    eprintln!("{err}");
                    1
                }
            }
        }
    }
}

/// One effective setting as `config show` prints it: the resolved value, with
/// the "not set" cases spelled out rather than shown as an empty line.
fn effective_value(effective: &settings::Effective, key: &str) -> String {
    match key {
        "portal_url" => effective
            .portal_url
            .clone()
            .unwrap_or_else(|| "(not set)".into()),
        "network" => effective
            .network
            .clone()
            .unwrap_or_else(|| "(not set: asks interactively)".into()),
        "username" => effective
            .username
            .clone()
            .unwrap_or_else(|| "(not set)".into()),
        "domain" => match effective.domain.as_deref() {
            Some("") => "(empty: no suffix)".to_string(),
            Some(domain) => domain.to_string(),
            None => "(not set: no suffix)".to_string(),
        },
        "callback" => effective.callback.clone(),
        "connect_timeout_ms" => effective.timeouts.connect_ms.to_string(),
        "read_timeout_ms" => effective.timeouts.read_ms.to_string(),
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

/// The configuration load every mode shares: discover, resolve (creating a file
/// on a first run), and report the path on stderr. `Err` is the exit status.
///
/// The `Config:` line is printed before anything else a mode does, so a service
/// manager's journal starts with the file the run actually used.
fn load_settings(args: &Args) -> Result<settings::Resolution, i32> {
    let paths = match settings::Paths::discover() {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("{err}");
            return Err(1);
        }
    };
    let explicit = args
        .config
        .clone()
        .or_else(|| std::env::var_os(settings::ENV_CONFIG_PATH).map(PathBuf::from));
    let resolution = match settings::resolve(
        &paths,
        explicit.as_deref(),
        args.portable.then_some(settings::Storage::Portable),
        &mut settings::ask_storage,
    ) {
        Ok(resolution) => resolution,
        Err(err) => {
            eprintln!("{err}");
            return Err(1);
        }
    };
    if let Some(path) = resolution.base_path.as_deref() {
        eprintln!("Config: {}", path.display());
    }
    Ok(resolution)
}

/// `srun-portal reconnect`: SPEC §5's check and, when needed, the login — once.
///
/// Nothing here may prompt: this is what the background task runs, and a prompt
/// with no terminal behind it would hang the timer instead of reporting why.
fn run_reconnect(args: &Args) -> i32 {
    let resolution = match load_settings(args) {
        Ok(loaded) => loaded,
        Err(status) => return status,
    };
    let _ = transport::set_timeouts(resolution.effective.timeouts);

    let portal = match configured_portal(&resolution.effective) {
        Ok(portal) => portal,
        Err(status) => return status,
    };
    let username = match configured_username(&resolution.effective) {
        Ok(username) => username,
        Err(status) => return status,
    };
    let password = match configured_password(&username, resolution.base_path.as_deref()) {
        Ok(password) => password,
        Err(status) => return status,
    };

    let target = reconnect::Target {
        portal,
        username,
        domain: reconnect::domain_arg(resolution.effective.domain.as_deref()),
        callback: resolution.effective.callback.clone(),
    };
    match reconnect::reconnect_once(&target, &password) {
        Ok(reconnect::Outcome::AlreadyOnline) => {
            println!("Online.");
            0
        }
        Ok(reconnect::Outcome::Reconnected) => {
            println!("Reconnected.");
            0
        }
        Err(message) => {
            eprintln!("{message}");
            1
        }
    }
}

/// The configured portal URL, validated. `Err` is the exit status.
fn configured_portal(effective: &settings::Effective) -> Result<config::PortalTarget, i32> {
    if let Some(slug) = effective
        .portal_url
        .as_deref()
        .filter(|slug| !slug.is_empty())
    {
        return config::parse_portal_target(slug).map_err(|err| {
            eprintln!("{err}");
            1
        });
    }
    network::select_target(network_mode(effective)?).map_err(|error| {
        eprintln!("{error}");
        1
    })
}

fn network_mode(effective: &settings::Effective) -> Result<NetworkMode, i32> {
    let Some(value) = effective.network.as_deref() else {
        eprintln!("{}", crate::messages::NETWORK_REQUIRED);
        return Err(1);
    };
    NetworkMode::parse(value).map_err(|error| {
        eprintln!("{error}");
        1
    })
}

/// The configured account name. `Err` is the exit status.
fn configured_username(effective: &settings::Effective) -> Result<String, i32> {
    match effective.username.as_deref() {
        Some(username) if !username.is_empty() => Ok(username.to_string()),
        _ => {
            eprintln!("{}", crate::messages::USERNAME_REQUIRED);
            Err(1)
        }
    }
}

/// The password to authenticate with: `SRUN_PORTAL_PASSWORD`, then whatever the
/// system keeps for `account` (the credential facility, then the fallback file).
/// `Err` is the exit status.
fn configured_password(account: &str, config: Option<&std::path::Path>) -> Result<String, i32> {
    let path = match config.map(credentials::path_for) {
        Some(path) => path,
        None => {
            eprintln!("{}", crate::messages::PASSWORD_REQUIRED);
            return Err(1);
        }
    };
    let stored = match credentials::read(account, &path) {
        Ok(stored) => stored,
        Err(err) => {
            eprintln!("{err}");
            return Err(1);
        }
    };
    let from_env = std::env::var(credentials::ENV_PASSWORD).ok();
    match credentials::resolve_password(from_env.as_deref(), stored) {
        Some(password) => Ok(password),
        None => {
            eprintln!("{}", crate::messages::PASSWORD_REQUIRED);
            Err(1)
        }
    }
}

/// `srun-portal service install|uninstall|status`.
fn run_service(action: ServiceAction, args: &Args) -> i32 {
    let mut resolution = match load_settings(args) {
        Ok(loaded) => loaded,
        Err(status) => return status,
    };
    let _ = transport::set_timeouts(resolution.effective.timeouts);

    let config = match resolution.base_path.clone() {
        Some(path) => path,
        None => {
            eprintln!("No configuration file to attach the task to");
            return 1;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("cannot locate the running executable: {err}");
            return 1;
        }
    };
    let home = match sys::home_dir() {
        Ok(home) => home,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    let spec = service::Spec {
        exe,
        config: config.clone(),
        interval_secs: resolution.effective.reconnect_interval_secs,
        boot_delay_secs: resolution.effective.reconnect_boot_delay_secs,
        home,
    };
    match action {
        ServiceAction::Install => {
            // A task needs a portal and an account: filling them in now is the
            // last moment a question can still be answered.
            if let Err(status) = ensure_unattended_settings(&mut resolution) {
                return status;
            }
            let account = match configured_username(&resolution.effective) {
                Ok(account) => account,
                Err(status) => return status,
            };
            let password = match install_password(&account, resolution.base_path.as_deref()) {
                Ok(password) => password,
                Err(status) => return status,
            };
            // Prove the credentials before writing anything: a task with a
            // wrong password would fail silently every interval.
            let target = reconnect::Target {
                portal: match configured_portal(&resolution.effective) {
                    Ok(portal) => portal,
                    Err(status) => return status,
                },
                username: account.clone(),
                domain: reconnect::domain_arg(resolution.effective.domain.as_deref()),
                callback: resolution.effective.callback.clone(),
            };
            match reconnect::reconnect_once(&target, &password) {
                Ok(reconnect::Outcome::AlreadyOnline) => println!("Online."),
                Ok(reconnect::Outcome::Reconnected) => println!("Reconnected."),
                Err(message) => {
                    eprintln!("{message}");
                    eprintln!("Task not installed: the credentials above did not work");
                    return 1;
                }
            }

            let credentials_path = credentials::path_for(&config);
            match credentials::store(&target.username, &password, &credentials_path) {
                Ok((store, note)) => {
                    if let Some(note) = note {
                        println!("{note}");
                    }
                    println!(
                        "Password: stored in the {} ({})",
                        store.name(),
                        target.username
                    );
                    if store == credentials::Store::File {
                        println!("Chmod 600 {}", credentials_path.display());
                    }
                }
                Err(err) => {
                    eprintln!("{err}");
                    return 1;
                }
            }

            let plan = service::plan(&spec);
            match service::install(&plan) {
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
                    return 1;
                }
            }
            println!("Reconnect: {}", plan.cadence);
            println!("{}", service::startup_note(&spec, &service::current_user()));
            if let Some(note) = service::note_for_executable(&spec.exe) {
                println!("{note}");
            }
            0
        }
        ServiceAction::Uninstall => {
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
                    return 1;
                }
            }
            let credentials_path = credentials::path_for(&config);
            if credentials_path.exists() {
                println!(
                    "Kept {} (run `srun-portal service forget` to delete it)",
                    credentials_path.display()
                );
            }
            if let Ok(account) = configured_username(&resolution.effective) {
                if credentials::read(&account, &credentials_path)
                    .ok()
                    .flatten()
                    .is_some()
                {
                    println!(
                        "Kept the stored password for {account} (run `srun-portal service forget`)"
                    );
                }
            }
            0
        }
        ServiceAction::Status => {
            let plan = service::plan(&spec);
            println!("Reconnect: {}", plan.cadence);
            println!("Credentials: {}", credentials_status(&resolution, &config));
            match service::status(&plan) {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                    0
                }
                Err(err) => {
                    eprintln!("{err}");
                    1
                }
            }
        }
        ServiceAction::Forget => {
            let credentials_path = credentials::path_for(&config);
            let account = match configured_username(&resolution.effective) {
                Ok(account) => account,
                // Without an account there is no entry to name; the file is
                // still worth reporting about, so fall through to the keyring
                // only when we have a key.
                Err(_) => {
                    let removed = match std::fs::remove_file(&credentials_path) {
                        Ok(()) => true,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
                        Err(err) => {
                            eprintln!("{}: {err}", credentials_path.display());
                            return 1;
                        }
                    };
                    println!(
                        "{}",
                        if removed {
                            format!("Removed {}", credentials_path.display())
                        } else {
                            "Nothing was stored".to_string()
                        }
                    );
                    return 0;
                }
            };
            match credentials::revoke(&account, &credentials_path) {
                Ok(true) => {
                    println!("Removed the stored password for {account}");
                    0
                }
                Ok(false) => {
                    println!("No stored password for {account}");
                    0
                }
                Err(err) => {
                    eprintln!("{err}");
                    1
                }
            }
        }
    }
}

/// One line describing where the password currently comes from, for `status`.
fn credentials_status(resolution: &settings::Resolution, config: &std::path::Path) -> String {
    if std::env::var(credentials::ENV_PASSWORD).is_ok_and(|value| !value.is_empty()) {
        return format!(
            "{} is set (overrides any stored password)",
            credentials::ENV_PASSWORD
        );
    }
    let account = match configured_username(&resolution.effective) {
        Ok(account) => account,
        Err(_) => return "no account configured".to_string(),
    };
    let path = credentials::path_for(config);
    if keyring::read(&account).ok().flatten().is_some() {
        return format!("stored in the system keyring ({account})");
    }
    if credentials::read_file(&path).ok().flatten().is_some() {
        return format!("stored in {} ({account})", path.display());
    }
    let reason = match keyring::availability() {
        keyring::Availability::Ready => String::new(),
        keyring::Availability::Unavailable(reason) => format!(" - keyring unavailable: {reason}"),
    };
    format!("none stored for {account}{reason}")
}

fn ensure_network_setting(resolution: &mut settings::Resolution) -> Result<(), i32> {
    if let Some(value) = resolution.effective.network.as_deref() {
        if NetworkMode::parse(value).is_ok() {
            return Ok(());
        }
        eprintln!("Invalid network setting {value:?}; choose office or dorm.");
    }
    loop {
        let input = prompt("Network [office/dorm]: ")?;
        if input.trim().is_empty() {
            eprintln!("{}", crate::messages::NETWORK_REQUIRED);
            continue;
        }
        match NetworkMode::parse(input.trim()) {
            Ok(mode) => {
                record_setting(resolution, "network", network_mode_name(mode))?;
                return Ok(());
            }
            Err(error) => eprintln!("{error}"),
        }
    }
}

fn network_mode_name(mode: NetworkMode) -> &'static str {
    match mode {
        NetworkMode::Office => "office",
        NetworkMode::Dorm => "dorm",
    }
}

fn should_persist_portal(args: &Args, resolution: &settings::Resolution) -> bool {
    args.portal_url.is_some() || resolution.effective.portal_url.is_some()
}

/// Fill in `portal_url` and `username` when the task could not work without
/// them, prompting exactly once each.
///
/// The check is silent here — the message belongs to the point where the
/// command actually gives up, not to the moment it decides to ask. Without a
/// terminal there is nobody to ask, so the standard "required" message is the
/// answer.
fn ensure_unattended_settings(resolution: &mut settings::Resolution) -> Result<(), i32> {
    let has_portal = resolution
        .effective
        .portal_url
        .as_deref()
        .is_some_and(|slug| config::parse_portal_target(slug).is_ok());
    let network_missing = match resolution.effective.network.as_deref() {
        Some(value) => NetworkMode::parse(value).is_err(),
        None => true,
    };
    if network_missing && !has_portal {
        if !sys::stdin_is_tty() {
            eprintln!("{}", crate::messages::NETWORK_REQUIRED);
            return Err(1);
        }
        ensure_network_setting(resolution)?;
    }

    if !has_portal {
        if !sys::stdin_is_tty() {
            eprintln!("{}", crate::messages::PORTAL_URL_INVALID);
            return Err(1);
        }
        let stored = loop {
            match prompt("Portal web url: ") {
                Ok(input) if !input.trim().is_empty() => {
                    match config::parse_portal_target(&input) {
                        Ok(_) => break input,
                        Err(err) => eprintln!("{err}"),
                    }
                }
                Ok(_) => eprintln!("{}", crate::messages::PORTAL_URL_REQUIRED),
                Err(status) => return Err(status),
            }
        };
        record_setting(resolution, "portal_url", &stored)?;
    }

    let has_username = resolution
        .effective
        .username
        .as_deref()
        .is_some_and(|username| !username.is_empty());
    if !has_username {
        if !sys::stdin_is_tty() {
            eprintln!("{}", crate::messages::USERNAME_REQUIRED);
            return Err(1);
        }
        let username = match prompt("Please enter your username: ") {
            Ok(username) if !username.trim().is_empty() => username,
            Ok(_) => {
                eprintln!("{}", crate::messages::USERNAME_REQUIRED);
                return Err(1);
            }
            Err(status) => return Err(status),
        };
        record_setting(resolution, "username", &username)?;
    }
    Ok(())
}

/// Write one key into the configuration file the run is anchored on, and into
/// the effective settings the same run keeps using.
fn record_setting(
    resolution: &mut settings::Resolution,
    key: &str,
    value: &str,
) -> Result<(), i32> {
    let Some(base) = resolution.base.as_mut() else {
        eprintln!("No configuration file to record {key} in");
        return Err(1);
    };
    if let Err(err) = base.update(&[(key, toml::Value::String(value.to_string()))]) {
        eprintln!("{err}");
        return Err(1);
    }
    match key {
        "portal_url" => resolution.effective.portal_url = Some(value.to_string()),
        "network" => resolution.effective.network = Some(value.to_string()),
        "username" => resolution.effective.username = Some(value.to_string()),
        other => {
            eprintln!("Internal error: {other} is not a promptable key");
            return Err(1);
        }
    }
    Ok(())
}

/// The password to store for the task, in order of precedence: the environment
/// (`SRUN_PORTAL_PASSWORD`), the one already stored for this account, then a
/// prompt.
///
/// Reusing the stored password matters because `install` is rerun whenever the
/// units change: asking again — or worse, failing because nobody can type one —
/// would make a reinstall impossible on a machine that already has a working
/// credential. An empty answer at the prompt also accepts the stored one.
fn install_password(account: &str, config: Option<&std::path::Path>) -> Result<String, i32> {
    let stored = match config.map(credentials::path_for) {
        Some(path) => match credentials::read(account, &path) {
            Ok(stored) => stored,
            Err(err) => {
                eprintln!("{err}");
                return Err(1);
            }
        },
        None => None,
    };
    if let Ok(password) = std::env::var(credentials::ENV_PASSWORD) {
        if !password.is_empty() {
            return Ok(password);
        }
    }
    if !sys::stdin_is_tty() {
        return match stored {
            Some(password) => Ok(password),
            None => {
                eprintln!("{}", crate::messages::PASSWORD_REQUIRED);
                Err(1)
            }
        };
    }
    match sys::prompt_password("Please enter your password: ") {
        Ok(Some(password)) if !password.is_empty() => Ok(password),
        // An empty answer keeps the credential already on file, which is what
        // makes a reinstall a one-keystroke operation.
        Ok(Some(_)) => match stored {
            Some(password) => Ok(password),
            None => {
                eprintln!("{}", crate::messages::PASSWORD_REQUIRED);
                Err(1)
            }
        },
        Ok(None) => match stored {
            Some(password) => Ok(password),
            None => {
                eprintln!("Portal exit!");
                Err(1)
            }
        },
        Err(err) => {
            eprintln!("{err}");
            Err(1)
        }
    }
}

/// The portal URL: the argument, then the configured `portal_url`, then a
/// prompt (SPEC §3.1, with a configuration file in place of the legacy env
/// file).
fn resolve_slug(args: &Args, resolution: &settings::Resolution) -> Result<String, i32> {
    if let Some(argument) = args.portal_url.as_deref() {
        return match config::parse_portal_target(argument) {
            Ok(_) => Ok(argument.to_string()),
            Err(err) => {
                eprintln!("{err}");
                Err(1)
            }
        };
    }
    if let Some(slug) = resolution.effective.portal_url.as_deref() {
        match config::parse_portal_target(slug) {
            Ok(_) => return Ok(slug.to_string()),
            Err(err) => eprintln!("{err}"),
        }
    } else {
        return Ok(network::select_slug(network_mode(&resolution.effective)?).to_string());
    }
    loop {
        let input = match sys::prompt_line("Portal web url: ") {
            Ok(Some(input)) => input,
            Ok(None) => {
                eprintln!("{}", crate::messages::PORTAL_URL_REQUIRED);
                return Err(1);
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
        match config::parse_portal_target(&input) {
            Ok(_) => return Ok(input),
            Err(err) => eprintln!("{err}"),
        }
    }
}

fn prompt(label: &str) -> Result<String, i32> {
    match sys::prompt_line(label) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => {
            eprintln!("Portal exit!");
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
            eprintln!("Portal exit!");
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

/// `user@domain` typed as one field: everything after the first `@` becomes the
/// domain argument again, with its `@`, which is the shape `authByPassword`
/// requires (SPEC §5, §10's `Domain must start with @`).
pub fn split_domain(username: &str) -> (String, String) {
    match username.split_once('@') {
        Some((account, domain)) if !domain.is_empty() => {
            (account.to_string(), format!("@{domain}"))
        }
        _ => (username.to_string(), String::new()),
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
    fn parse_args_reads_options_and_one_url() {
        let args = parse_args(&[]).unwrap();
        assert_eq!(args.portal_url, None);
        assert!(!args.portable);
        assert!(!args.help);

        let args = parse_args(&[
            "--portable".to_string(),
            "https://h/srun_portal_pc?ac_id=1".to_string(),
        ])
        .unwrap();
        assert!(args.portable);
        assert_eq!(
            args.portal_url.as_deref(),
            Some("https://h/srun_portal_pc?ac_id=1")
        );

        // Both spellings of `--config`, and `--help`.
        let args = parse_args(&["--config".to_string(), "/tmp/a.toml".to_string()]).unwrap();
        assert_eq!(args.config, Some(PathBuf::from("/tmp/a.toml")));
        let args = parse_args(&["--config=/tmp/b.toml".to_string()]).unwrap();
        assert_eq!(args.config, Some(PathBuf::from("/tmp/b.toml")));
        assert!(parse_args(&["-h".to_string()]).unwrap().help);

        // Usage errors: a missing or empty `--config` value, an unknown option,
        // and a second positional argument.
        assert_eq!(
            parse_args(&["--config".to_string()]).unwrap_err(),
            "--config needs a path"
        );
        assert_eq!(
            parse_args(&["--config=".to_string()]).unwrap_err(),
            "--config needs a path"
        );
        assert_eq!(
            parse_args(&["--nope".to_string()]).unwrap_err(),
            "Unknown option: --nope"
        );
        assert_eq!(
            parse_args(&[
                "https://a.example/srun_portal_pc?ac_id=1".to_string(),
                "extra".to_string()
            ])
            .unwrap_err(),
            "Unexpected argument: extra"
        );
    }

    #[test]
    fn parse_args_reads_the_config_command() {
        assert_eq!(
            parse_args(&["config".to_string(), "show".to_string()])
                .unwrap()
                .command,
            Some(Command::Config(ConfigAction::Show))
        );
        assert_eq!(
            parse_args(&["config".to_string(), "list".to_string()])
                .unwrap()
                .command,
            Some(Command::Config(ConfigAction::List))
        );
        // `set` takes pairs, so several settings go in one call.
        assert_eq!(
            parse_args(&[
                "config".to_string(),
                "set".to_string(),
                "username".to_string(),
                "testuser".to_string(),
                "domain".to_string(),
                "szu".to_string(),
            ])
            .unwrap()
            .command,
            Some(Command::Config(ConfigAction::Set(vec![
                ("username".to_string(), "testuser".to_string()),
                ("domain".to_string(), "szu".to_string()),
            ])))
        );
        assert_eq!(
            parse_args(&[
                "config".to_string(),
                "unset".to_string(),
                "domain".to_string(),
                "callback".to_string(),
            ])
            .unwrap()
            .command,
            Some(Command::Config(ConfigAction::Unset(vec![
                "domain".to_string(),
                "callback".to_string(),
            ])))
        );

        // Malformed forms are usage errors, not silently partial commands.
        assert_eq!(
            parse_args(&["config".to_string()]).unwrap_err(),
            "config needs an action: show, list, set or unset"
        );
        assert_eq!(
            parse_args(&["config".to_string(), "frobnicate".to_string()]).unwrap_err(),
            "Unknown config action: frobnicate"
        );
        assert_eq!(
            parse_args(&[
                "config".to_string(),
                "set".to_string(),
                "username".to_string()
            ])
            .unwrap_err(),
            "config set needs KEY VALUE pairs, e.g. `config set username testuser`"
        );
        assert_eq!(
            parse_args(&["config".to_string(), "unset".to_string()]).unwrap_err(),
            "config unset needs at least one KEY"
        );
        // Options may still precede the command.
        assert!(parse_args(&[
            "--config".to_string(),
            "/tmp/a.toml".to_string(),
            "config".to_string(),
            "show".to_string(),
        ])
        .unwrap()
        .config
        .is_some());
    }

    #[test]
    fn parse_args_reads_the_commands() {
        // The words cannot be portal URLs, so they are commands, not slugs.
        let args = parse_args(&["reconnect".to_string()]).unwrap();
        assert_eq!(args.command, Some(Command::Reconnect));
        assert_eq!(args.portal_url, None);

        let args = parse_args(&["service".to_string(), "install".to_string()]).unwrap();
        assert_eq!(args.command, Some(Command::Service(ServiceAction::Install)));
        let args = parse_args(&["service".to_string(), "status".to_string()]).unwrap();
        assert_eq!(args.command, Some(Command::Service(ServiceAction::Status)));

        // Options may come before or after the command.
        let args = parse_args(&[
            "--config".to_string(),
            "/tmp/a.toml".to_string(),
            "service".to_string(),
            "uninstall".to_string(),
        ])
        .unwrap();
        assert_eq!(
            args.command,
            Some(Command::Service(ServiceAction::Uninstall))
        );
        assert_eq!(args.config, Some(PathBuf::from("/tmp/a.toml")));
        assert!(
            parse_args(&["reconnect".to_string(), "--portable".to_string()])
                .unwrap()
                .portable
        );

        assert_eq!(
            parse_args(&["service".to_string()]).unwrap_err(),
            "service needs an action: install, uninstall, status or forget"
        );
        assert_eq!(
            parse_args(&["service".to_string(), "forget".to_string()])
                .unwrap()
                .command,
            Some(Command::Service(ServiceAction::Forget))
        );
        assert_eq!(
            parse_args(&["service".to_string(), "frobnicate".to_string()]).unwrap_err(),
            "Unknown service action: frobnicate"
        );
        // A command takes no positional argument and takes no second one.
        assert_eq!(
            parse_args(&["reconnect".to_string(), "x".to_string()]).unwrap_err(),
            "Unexpected argument: x"
        );
        assert_eq!(
            parse_args(&[
                "service".to_string(),
                "status".to_string(),
                "extra".to_string()
            ])
            .unwrap_err(),
            "Unexpected argument: extra"
        );
    }

    #[test]
    fn a_url_is_still_a_url() {
        let args = parse_args(&["https://h/srun_portal_pc?ac_id=1".to_string()]).unwrap();
        assert_eq!(args.command, None);
        assert_eq!(
            args.portal_url.as_deref(),
            Some("https://h/srun_portal_pc?ac_id=1")
        );
    }

    #[test]
    fn split_domain_accepts_an_embedded_domain() {
        assert_eq!(
            split_domain("testuser"),
            ("testuser".to_string(), String::new())
        );
        assert_eq!(
            split_domain("testuser@szu"),
            ("testuser".to_string(), "@szu".to_string())
        );
        assert_eq!(
            split_domain("testuser@"),
            ("testuser@".to_string(), String::new())
        );
    }

    #[test]
    fn the_panel_header_is_centred_like_the_reference() {
        assert_eq!(center("You're online", RULE_WIDTH).len(), RULE_WIDTH);
        assert_eq!(
            center("You're online", RULE_WIDTH),
            "      You're online      "
        );
        assert_eq!(RULE_WIDTH, "-------------------------".len());
    }
}
