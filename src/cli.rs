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

mod args;

use self::args::{Args, Command, ConfigAction, ServiceAction, USAGE};
use crate::account::AccountIdentity;
use crate::api;
use crate::config;
use crate::credentials;
use crate::format;
use crate::reconnect::{self, fetch_config, host_of};
use crate::runtime::{Runtime, RuntimeOptions};
use crate::service;
use crate::settings;
use crate::sys;
use crate::transport;
use std::path::PathBuf;

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
    if args.help {
        print!("{}", args::help_text(&args));
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
    if let Err(status) = apply_setting_options(&mut resolution, &args, true) {
        return status;
    }

    if let Some(path) = resolution.imported_legacy.as_deref() {
        eprintln!("Imported {}", path.display());
    }
    // One-shot: the agent reads these when it is first built. Failing means the
    // timeouts were already fixed, which is not an error here.
    let _ = transport::set_timeouts(resolution.effective.timeouts);

    let slug = match resolve_slug(&args, &resolution) {
        Ok(slug) => slug,
        Err(status) => return status,
    };
    let portal = match config::parse_portal_url(&slug) {
        Ok(url) => url,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
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
    // Keep legacy callback settings effective until the next config write.
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
        let username = if has_setting_option(&args, "username") {
            resolution.effective.username.clone().unwrap_or_default()
        } else {
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
            if entered.trim().is_empty() {
                resolution.effective.username.clone().unwrap_or_default()
            } else {
                entered
            }
        };
        let password = match password_prompt() {
            Ok(value) => value,
            Err(status) => return status,
        };
        let identity = AccountIdentity::parse(&username, resolution.effective.domain.as_deref());
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
            for spec in settings::KEY_CATALOGUE
                .iter()
                .chain(settings::LEGACY_KEY_CATALOGUE.iter())
            {
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
            match resolution.update(&changes) {
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
            match resolution.remove(&keys.iter().map(String::as_str).collect::<Vec<_>>()) {
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
    let mut resolution = match load_settings(args) {
        Ok(loaded) => loaded,
        Err(status) => return status,
    };
    if let Err(status) = apply_setting_options(&mut resolution, args, false) {
        return status;
    }
    let _ = transport::set_timeouts(resolution.effective.timeouts);

    let portal = match configured_portal(&resolution.effective) {
        Ok(portal) => portal,
        Err(status) => return status,
    };
    let account = match configured_account(&resolution.effective) {
        Ok(account) => account,
        Err(status) => return status,
    };
    let password = match configured_password(&account, resolution.base_path.as_deref()) {
        Ok(password) => password,
        Err(status) => return status,
    };

    let target = reconnect::Target {
        portal,
        username: account.local().to_string(),
        domain: account.domain().to_string(),
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
fn configured_portal(effective: &settings::Effective) -> Result<config::PortalUrl, i32> {
    let slug = match effective.portal_url.as_deref() {
        Some(slug) if !slug.is_empty() => slug,
        _ => {
            eprintln!("{}", crate::messages::PORTAL_URL_REQUIRED);
            return Err(1);
        }
    };
    config::parse_portal_url(slug).map_err(|err| {
        eprintln!("{err}");
        1
    })
}

/// The configured account's protocol and credential identities. Legacy config
/// files may still carry `domain`; it is honored until their next write.
fn configured_account(effective: &settings::Effective) -> Result<AccountIdentity, i32> {
    match effective.username.as_deref() {
        Some(username) if !username.is_empty() => Ok(AccountIdentity::parse(
            username,
            effective.domain.as_deref(),
        )),
        _ => {
            eprintln!("{}", crate::messages::USERNAME_REQUIRED);
            Err(1)
        }
    }
}

/// The password to authenticate with: `SRUN_PORTAL_PASSWORD`, then the stored
/// password for the complete effective account key.
/// `Err` is the exit status.
fn configured_password(
    account: &AccountIdentity,
    config: Option<&std::path::Path>,
) -> Result<String, i32> {
    let path = match config.map(credentials::path_for) {
        Some(path) => path,
        None => {
            eprintln!("{}", crate::messages::PASSWORD_REQUIRED);
            return Err(1);
        }
    };
    let stored = match credentials::read(account.qualified(), &path) {
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

fn apply_effective_setting(
    effective: &mut settings::Effective,
    key: &str,
    value: &toml::Value,
) -> Result<(), i32> {
    match (key, value) {
        ("portal_url", toml::Value::String(value)) => {
            effective.portal_url = Some(value.clone());
        }
        ("username", toml::Value::String(value)) => {
            effective.username = Some(value.clone());
        }
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
            eprintln!("Internal error: invalid configuration value for {key}");
            return Err(1);
        }
    }
    Ok(())
}

/// Apply command-line configuration options to this run, optionally persisting
/// them in the selected configuration file.
fn apply_setting_options(
    resolution: &mut settings::Resolution,
    args: &Args,
    persist: bool,
) -> Result<(), i32> {
    if args.setting_options.is_empty() {
        return Ok(());
    }
    let mut changes = Vec::with_capacity(args.setting_options.len());
    for (key, value) in &args.setting_options {
        let parsed = settings::parse_key_value(key, value).map_err(|err| {
            eprintln!("--{} {}: {err}", key.replace('_', "-"), value);
            1
        })?;
        changes.push((key.as_str(), parsed));
    }
    if persist {
        if let Err(err) = resolution.update(&changes) {
            eprintln!("{err}");
            return Err(1);
        }
        return Ok(());
    }
    for (key, value) in &changes {
        apply_effective_setting(&mut resolution.effective, key, value)?;
    }
    Ok(())
}

/// `srun-portal service install|uninstall|status`.
fn run_service(action: ServiceAction, args: &Args) -> i32 {
    let mut resolution = match load_settings(args) {
        Ok(loaded) => loaded,
        Err(status) => return status,
    };
    if action == ServiceAction::Install {
        if let Err(status) = apply_setting_options(&mut resolution, args, true) {
            return status;
        }
    }
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
    let mut spec = service::Spec {
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
            if let Err(status) = ensure_service_install_settings(&mut resolution, args) {
                return status;
            }
            let _ = transport::set_timeouts(resolution.effective.timeouts);
            spec.interval_secs = resolution.effective.reconnect_interval_secs;
            spec.boot_delay_secs = resolution.effective.reconnect_boot_delay_secs;
            let account = match configured_account(&resolution.effective) {
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
                username: account.local().to_string(),
                domain: account.domain().to_string(),
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
            match credentials::store(account.qualified(), &password, &credentials_path) {
                Ok((store, note)) => {
                    if let Some(note) = note {
                        println!("{note}");
                    }
                    println!(
                        "Password: stored in the {} ({})",
                        store.name(),
                        account.qualified()
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
            if let Ok(account) = configured_account(&resolution.effective) {
                if credentials::read(account.qualified(), &credentials_path)
                    .ok()
                    .flatten()
                    .is_some()
                {
                    println!(
                        "Kept the stored password for {} (run `srun-portal service forget`)",
                        account.qualified()
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
            let account = match configured_account(&resolution.effective) {
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
            match credentials::revoke(account.qualified(), &credentials_path) {
                Ok(true) => {
                    println!("Removed the stored password for {}", account.qualified());
                    0
                }
                Ok(false) => {
                    println!("No stored password for {}", account.qualified());
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
    let account = match configured_account(&resolution.effective) {
        Ok(account) => account,
        Err(_) => return "no account configured".to_string(),
    };
    let path = credentials::path_for(config);
    let status = credentials::status(account.qualified(), &path);
    render_credentials_status(account.qualified(), &path, status)
}

fn render_credentials_status(
    account_key: &str,
    path: &std::path::Path,
    status: credentials::Status,
) -> String {
    match status {
        credentials::Status::Keyring => {
            format!("stored in the system keyring ({account_key})")
        }
        credentials::Status::File => {
            format!("stored in {} ({account_key})", path.display())
        }
        credentials::Status::Missing {
            keyring_unavailable,
        } => {
            let reason = keyring_unavailable
                .map(|reason| format!(" - keyring unavailable: {reason}"))
                .unwrap_or_default();
            format!("none stored for {account_key}{reason}")
        }
    }
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
        .is_some_and(|slug| config::parse_portal_url(slug).is_ok());
    if !has_portal {
        if !sys::stdin_is_tty() {
            eprintln!("{}", crate::messages::PORTAL_URL_REQUIRED);
            return Err(1);
        }
        let stored = loop {
            match prompt("Portal web url: ") {
                Ok(input) if !input.trim().is_empty() => match config::parse_portal_url(&input) {
                    Ok(_) => break input,
                    Err(err) => eprintln!("{err}"),
                },
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

fn has_setting_option(args: &Args, key: &str) -> bool {
    args.setting_options
        .iter()
        .any(|(option_key, _)| option_key == key)
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

fn prompt_u64_setting(
    resolution: &mut settings::Resolution,
    args: &Args,
    key: &str,
    label: &str,
    default: u64,
) -> Result<(), i32> {
    if has_setting_option(args, key) {
        return Ok(());
    }
    loop {
        let input = prompt_with_default(label, Some(&default.to_string()))?;
        match input.trim().parse::<u64>() {
            Ok(value) if value <= i64::MAX as u64 => {
                return record_setting_value(resolution, key, toml::Value::Integer(value as i64));
            }
            _ => eprintln!("{label} must be a non-negative integer"),
        }
    }
}

/// Interactive `service install` configuration. Existing values are shown as
/// defaults; an empty answer keeps them. Non-interactive callers retain the
/// strict two-required-values behavior.
fn ensure_service_install_settings(
    resolution: &mut settings::Resolution,
    args: &Args,
) -> Result<(), i32> {
    if !sys::stdin_is_tty() {
        return ensure_unattended_settings(resolution);
    }

    if !has_setting_option(args, "portal_url") {
        let default = resolution
            .effective
            .portal_url
            .clone()
            .filter(|value| config::parse_portal_url(value).is_ok());
        loop {
            let input = prompt_with_default("Portal web url", default.as_deref())?;
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

    if !has_setting_option(args, "username") {
        let default = resolution
            .effective
            .username
            .clone()
            .filter(|value| !value.is_empty());
        loop {
            let input = prompt_with_default("Username", default.as_deref())?;
            if input.trim().is_empty() {
                eprintln!("{}", crate::messages::USERNAME_REQUIRED);
                continue;
            }
            record_setting(resolution, "username", &input)?;
            break;
        }
    }

    prompt_u64_setting(
        resolution,
        args,
        "timeout_ms",
        "Timeout (ms)",
        resolution
            .effective
            .timeouts
            .connect_ms
            .max(resolution.effective.timeouts.read_ms),
    )?;
    prompt_u64_setting(
        resolution,
        args,
        "reconnect_interval_secs",
        "Reconnect interval (seconds)",
        resolution.effective.reconnect_interval_secs.unwrap_or(0),
    )?;
    prompt_u64_setting(
        resolution,
        args,
        "reconnect_boot_delay_secs",
        "Reconnect startup delay (seconds)",
        resolution.effective.reconnect_boot_delay_secs.unwrap_or(0),
    )?;
    Ok(())
}

/// Write one key into the configuration file the run is anchored on; Resolution
/// refreshes the effective settings from all layers.
fn record_setting(
    resolution: &mut settings::Resolution,
    key: &str,
    value: &str,
) -> Result<(), i32> {
    record_setting_value(resolution, key, toml::Value::String(value.to_string()))
}

fn record_setting_value(
    resolution: &mut settings::Resolution,
    key: &str,
    value: toml::Value,
) -> Result<(), i32> {
    if let Err(err) = resolution.update(&[(key, value)]) {
        eprintln!("{err}");
        return Err(1);
    }
    Ok(())
}

/// The password to store for the task, in order of precedence: the environment
/// (`SRUN_PORTAL_PASSWORD`), the password already stored for this complete
/// account key, then a prompt.
///
/// Reusing the stored password matters because `install` is rerun whenever the
/// units change: asking again — or worse, failing because nobody can type one —
/// would make a reinstall impossible on a machine that already has a working
/// credential. An empty answer at the prompt also accepts the stored one.
fn install_password(
    account: &AccountIdentity,
    config: Option<&std::path::Path>,
) -> Result<String, i32> {
    let stored = match config.map(credentials::path_for) {
        Some(path) => match credentials::read(account.qualified(), &path) {
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
    if has_setting_option(args, "portal_url") {
        if let Some(slug) = resolution.effective.portal_url.as_deref() {
            return config::parse_portal_url(slug)
                .map(|_| slug.to_string())
                .map_err(|err| {
                    eprintln!("{err}");
                    1
                });
        }
    }

    if let Some(argument) = args.portal_url.as_deref() {
        return match config::parse_portal_url(argument) {
            Ok(_) => Ok(argument.to_string()),
            Err(err) => {
                eprintln!("{err}");
                Err(1)
            }
        };
    }
    if let Some(slug) = resolution.effective.portal_url.as_deref() {
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
    fn service_install_options_are_persisted_and_applied() {
        let root = std::env::temp_dir().join(format!(
            "srun-portal-cli-options-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("config.toml");
        let paths = settings::Paths {
            system: root.join("system.toml"),
            portable: root.join("portable.toml"),
            legacy: None,
        };
        let mut resolution = settings::resolve(&paths, Some(&path), None, &mut |_| {
            settings::Storage::System
        })
        .unwrap();
        let args = args::parse(&[
            "service".to_string(),
            "install".to_string(),
            "--timeout-ms".to_string(),
            "12000".to_string(),
            "--reconnect-interval-secs".to_string(),
            "0".to_string(),
            "--reconnect-boot-delay-secs=45".to_string(),
        ])
        .unwrap();

        apply_setting_options(&mut resolution, &args, true).unwrap();

        assert_eq!(resolution.effective.reconnect_interval_secs, None);
        assert_eq!(resolution.effective.reconnect_boot_delay_secs, Some(45));
        assert_eq!(resolution.effective.timeouts.connect_ms, 12000);
        assert_eq!(resolution.effective.timeouts.read_ms, 12000);
        let file = settings::ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(file.settings.reconnect_interval_secs, Some(0));
        assert_eq!(file.settings.reconnect_boot_delay_secs, Some(45));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn credential_status_renders_the_complete_account_key() {
        let path = std::path::Path::new("/tmp/srun-portal.credentials.toml");
        let cases = [
            (
                credentials::Status::Keyring,
                "stored in the system keyring (alice@staff)".to_string(),
            ),
            (
                credentials::Status::File,
                format!("stored in {} (alice@staff)", path.display()),
            ),
            (
                credentials::Status::Missing {
                    keyring_unavailable: Some("no wallet".to_string()),
                },
                "none stored for alice@staff - keyring unavailable: no wallet".to_string(),
            ),
        ];
        for (status, expected) in cases {
            let rendered = render_credentials_status("alice@staff", path, status);
            assert_eq!(rendered, expected);
            assert!(!rendered.contains("(alice)"));
        }
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
