//! The configuration file: discovery, layering and first-run creation.
//!
//! The CLI reads exactly two files, both named [`CONFIG_FILE_NAME`]:
//!
//! * the **system** file, in the directory this platform has a convention for
//!   (`$XDG_CONFIG_HOME/srun-portal`, `~/Library/Application Support/srun-portal`,
//!   `%APPDATA%\srun-portal\config`), and
//! * the **project** file, next to the executable — the portable deployment.
//!
//! The project file is the project-level configuration: when it is present it
//! overrides the system file key by key, so a deployment can pin a `callback`
//! or a timeout without copying the rest of the user's settings. Both are
//! optional; when neither exists the CLI asks where to put one (portable when
//! there is no terminal to ask; see [`ask_storage`]) and seeds the new file from
//! the legacy `~/.srun_portal.json` if that file is still around.
//!
//! The legacy file is only ever read ([`crate::config::read_legacy`]): it is not
//! written any more, which is the deliberate deviation from SPEC §3.1/§12 item
//! 12 — the recorded portal URL now lives in the `portal_url` key of whichever
//! configuration file was read first. This module stores no password: the
//! unattended one lives in the system credential facility (see
//! [`crate::keyring`] and [`crate::credentials`]), never in these files.
//!
//! Unknown keys are kept in the parsed table and written back unchanged, so a
//! future key survives an older binary rewriting the file.

use std::path::{Path, PathBuf};

use toml::Value;

use crate::config::{self, ConfigError};
use crate::sys;
use crate::transport::Timeouts;

/// The configuration file name, in both locations.
pub const CONFIG_FILE_NAME: &str = "srun-portal.toml";

/// The environment variable holding an explicit configuration path. `--config`
/// wins over it.
pub const ENV_CONFIG_PATH: &str = "SRUN_PORTAL_CONFIG";

/// The comment block every file this module writes starts with.
const HEADER: &str = "\
# srun-portal configuration.
# Every key is optional; unknown keys are preserved across rewrites.
#   portal_url          full portal URL, or empty when `network` selects one
#   network             office or dorm; missing asks in interactive mode
#   username            account name used as the default for the login prompt
#   domain              domain suffix used when the account carries none (with or
#                       without '@'; empty or unset means no suffix, which is what
#                       the portal's own capture does)
#   callback            JSONP callback name (default \"jsonp\")
#   connect_timeout_ms  HTTP connect timeout in milliseconds (default 5000)
#   read_timeout_ms     HTTP read timeout in milliseconds (default 10000)
#   reconnect_interval_secs   background reconnect interval in seconds (default 300; 0: no interval)
#   reconnect_boot_delay_secs delay after startup before the first check (default 30; 0: not at startup)
";

const DEFAULT_CONNECT_MS: u64 = 5_000;
const DEFAULT_READ_MS: u64 = 10_000;
/// How often the installed background task checks the connection. A task that
/// does not run is not worth installing, so this is on by default; setting the
/// key to `0` switches the interval off.
const DEFAULT_INTERVAL_SECS: u64 = 300;
/// How long the installed task waits after startup before its first check, so a
/// boot-time race with the network coming up is not a failed login. `0` switches
/// the startup run off.
const DEFAULT_BOOT_DELAY_SECS: u64 = 30;

/// Where a first-run configuration file goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Storage {
    /// The platform's configuration directory.
    #[default]
    System,
    /// Next to the executable.
    Portable,
}

/// The keys one configuration file may carry. Every one is optional, so
/// "unset" stays distinguishable from "set to the default value" when files are
/// layered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub portal_url: Option<String>,
    pub network: Option<String>,
    pub username: Option<String>,
    pub domain: Option<String>,
    pub callback: Option<String>,
    pub connect_timeout_ms: Option<u64>,
    pub read_timeout_ms: Option<u64>,
    pub reconnect_interval_secs: Option<u64>,
    pub reconnect_boot_delay_secs: Option<u64>,
}

impl Settings {
    /// The values a **newly created** file is seeded with.
    ///
    /// They are the documented defaults written out, so the file shows every
    /// key and what it means instead of being empty: `domain` with no suffix
    /// (what the reference's own capture sends), the two timeouts, and both
    /// background cadences off. `Settings::default()` stays the "nothing is
    /// set" value the layering needs; this is the template for a fresh file.
    fn documented_defaults() -> Settings {
        Settings {
            portal_url: None,
            network: None,
            username: None,
            domain: Some(String::new()),
            callback: None,
            connect_timeout_ms: Some(DEFAULT_CONNECT_MS),
            read_timeout_ms: Some(DEFAULT_READ_MS),
            reconnect_interval_secs: Some(DEFAULT_INTERVAL_SECS),
            reconnect_boot_delay_secs: Some(DEFAULT_BOOT_DELAY_SECS),
        }
    }

    /// The keys that are set, as TOML values.
    fn pairs(&self) -> Vec<(&'static str, Value)> {
        let mut pairs = Vec::new();
        if let Some(value) = &self.portal_url {
            pairs.push(("portal_url", Value::String(value.clone())));
        }
        if let Some(value) = &self.network {
            pairs.push(("network", Value::String(value.clone())));
        }
        if let Some(value) = &self.username {
            pairs.push(("username", Value::String(value.clone())));
        }
        if let Some(value) = &self.domain {
            pairs.push(("domain", Value::String(value.clone())));
        }
        if let Some(value) = &self.callback {
            pairs.push(("callback", Value::String(value.clone())));
        }
        if let Some(value) = self.connect_timeout_ms {
            pairs.push(("connect_timeout_ms", Value::Integer(value as i64)));
        }
        if let Some(value) = self.read_timeout_ms {
            pairs.push(("read_timeout_ms", Value::Integer(value as i64)));
        }
        if let Some(value) = self.reconnect_interval_secs {
            pairs.push(("reconnect_interval_secs", Value::Integer(value as i64)));
        }
        if let Some(value) = self.reconnect_boot_delay_secs {
            pairs.push(("reconnect_boot_delay_secs", Value::Integer(value as i64)));
        }
        pairs
    }

    /// Read this version's keys out of a parsed table, reporting the first key
    /// whose type is wrong.
    fn from_table(table: &toml::Table, path: &Path) -> Result<Settings, ConfigError> {
        Ok(Settings {
            portal_url: optional_string(table, "portal_url", path)?,
            network: optional_string(table, "network", path)?,
            username: optional_string(table, "username", path)?,
            domain: optional_string(table, "domain", path)?,
            callback: optional_string(table, "callback", path)?,
            connect_timeout_ms: optional_millis(table, "connect_timeout_ms", path)?,
            read_timeout_ms: optional_millis(table, "read_timeout_ms", path)?,
            reconnect_interval_secs: optional_millis(table, "reconnect_interval_secs", path)?,
            reconnect_boot_delay_secs: optional_millis(table, "reconnect_boot_delay_secs", path)?,
        })
    }
}

fn optional_string(
    table: &toml::Table,
    key: &str,
    path: &Path,
) -> Result<Option<String>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format_error(path, key, "a string")),
    }
}

fn optional_millis(
    table: &toml::Table,
    key: &str,
    path: &Path,
) -> Result<Option<u64>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(value)) if *value >= 0 => Ok(Some(*value as u64)),
        Some(_) => Err(format_error(path, key, "a non-negative integer")),
    }
}

fn format_error(path: &Path, key: &str, expected: &str) -> ConfigError {
    ConfigError::Format {
        path: path.to_path_buf(),
        detail: format!("key \"{key}\" must be {expected}"),
    }
}

/// One configuration file: its parsed settings, and the whole table it was read
/// from so unknown keys survive a rewrite.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    pub path: PathBuf,
    pub settings: Settings,
    raw: toml::Table,
    text: String,
}

impl ConfigFile {
    /// Read and parse `path`; a missing file is `Ok(None)`.
    pub fn read(path: &Path) -> Result<Option<ConfigFile>, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(ConfigError::Io(format!("{}: {err}", path.display()))),
        };
        let raw: toml::Table = toml::from_str(&text).map_err(|err| ConfigError::Format {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })?;
        let settings = Settings::from_table(&raw, path)?;
        Ok(Some(ConfigFile {
            path: path.to_path_buf(),
            settings,
            raw,
            text,
        }))
    }

    /// Create `path` holding `seed`, creating parent directories as needed. An
    /// existing file is overwritten: callers only reach this with a path they
    /// have already read as missing.
    pub fn create(path: &Path, seed: &Settings) -> Result<ConfigFile, ConfigError> {
        let mut raw = toml::Table::new();
        for (key, value) in seed.pairs() {
            raw.insert(key.to_string(), value);
        }
        let mut file = ConfigFile {
            path: path.to_path_buf(),
            settings: seed.clone(),
            raw,
            text: String::new(),
        };
        file.flush()?;
        Ok(file)
    }

    /// Merge `changes` into the file and write it back when the result differs
    /// from what is already on disk. Returns whether it wrote.
    pub fn update(&mut self, changes: &[(&str, Value)]) -> Result<bool, ConfigError> {
        for (key, value) in changes {
            self.raw.insert((*key).to_string(), value.clone());
        }
        self.settings = Settings::from_table(&self.raw, &self.path)?;
        self.flush()
    }

    /// Remove `keys` from the file and write it back when something changed.
    /// Unknown keys are left alone, so removing a key this version does not know
    /// is a no-op rather than a deletion.
    pub fn remove(&mut self, keys: &[&str]) -> Result<bool, ConfigError> {
        let mut changed = false;
        for key in keys {
            if KEY_CATALOGUE.iter().any(|entry| entry.name == *key) {
                changed |= self.raw.remove(*key).is_some();
            }
        }
        if !changed {
            return Ok(false);
        }
        self.settings = Settings::from_table(&self.raw, &self.path)?;
        self.flush()
    }

    /// Every key the file currently sets, in the catalogue's order, with the
    /// TOML value as text — what `config list` prints.
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        KEY_CATALOGUE
            .iter()
            .filter_map(|entry| {
                self.raw
                    .get(entry.name)
                    .map(|value| (entry.name, render_value(value)))
            })
            .collect()
    }
}

/// What every configuration key is for. This is the single list the CLI uses to
/// validate `config set`, to document `config show`, and to decide what is safe
/// to delete — the same names `Settings::from_table` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySpec {
    pub name: &'static str,
    /// The value's type, for the error message and for `config show`.
    pub kind: KeyKind,
    /// One line for `config show`, and the `--help` text.
    pub about: &'static str,
}

/// The types a configuration value can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// Any string, empty allowed.
    Str,
    /// A whole number that must not be negative.
    Count,
}

impl KeyKind {
    /// How the type is described in an error message.
    pub fn describe(self) -> &'static str {
        match self {
            KeyKind::Str => "a string",
            KeyKind::Count => "a non-negative integer",
        }
    }
}

/// Every key this version understands, in the order the header lists them.
pub const KEY_CATALOGUE: &[KeySpec] = &[
    KeySpec {
        name: "portal_url",
        kind: KeyKind::Str,
        about: "full portal URL used when no argument is given",
    },
    KeySpec {
        name: "network",
        kind: KeyKind::Str,
        about: "network mode: office or dorm",
    },
    KeySpec {
        name: "username",
        kind: KeyKind::Str,
        about: "account used as the default for the login prompt",
    },
    KeySpec {
        name: "domain",
        kind: KeyKind::Str,
        about: "suffix appended to an account that carries none (with or without '@')",
    },
    KeySpec {
        name: "callback",
        kind: KeyKind::Str,
        about: "JSONP callback name",
    },
    KeySpec {
        name: "connect_timeout_ms",
        kind: KeyKind::Count,
        about: "HTTP connect timeout in milliseconds",
    },
    KeySpec {
        name: "read_timeout_ms",
        kind: KeyKind::Count,
        about: "HTTP read timeout in milliseconds",
    },
    KeySpec {
        name: "reconnect_interval_secs",
        kind: KeyKind::Count,
        about: "background reconnect interval in seconds (0: no interval)",
    },
    KeySpec {
        name: "reconnect_boot_delay_secs",
        kind: KeyKind::Count,
        about: "delay after startup before the first check (0: not at startup)",
    },
];

/// The catalogue entry for `name`, if this version has one.
pub fn key_spec(name: &str) -> Option<&'static KeySpec> {
    KEY_CATALOGUE.iter().find(|entry| entry.name == name)
}

/// Parse `text` as the kind of value `key` holds, or explain what was wrong.
///
/// Deliberately strict: the file must keep holding the type
/// `Settings::from_table` expects, so `config set connect_timeout_ms abc` is
/// refused here rather than turning into a broken file that the next run
/// reports as corrupt.
pub fn parse_key_value(key: &str, text: &str) -> Result<Value, String> {
    let Some(spec) = key_spec(key) else {
        return Err(format!("Unknown configuration key: {key}"));
    };
    if key == "network" && !matches!(text, "office" | "dorm") {
        return Err("key \"network\" must be one of: office, dorm".to_string());
    }
    match spec.kind {
        KeyKind::Str => Ok(Value::String(text.to_string())),
        KeyKind::Count => match text.parse::<u64>() {
            Ok(number) => Ok(Value::Integer(number as i64)),
            Err(_) => Err(format!(
                "key \"{key}\" must be {}",
                KeyKind::Count.describe()
            )),
        },
    }
}

/// A TOML value as the single line `config list` shows.
fn render_value(value: &Value) -> String {
    match value {
        Value::String(text) if text.is_empty() => "\"\"".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

impl ConfigFile {
    /// Serialize the table and write it, unless the text is unchanged.
    fn flush(&mut self) -> Result<bool, ConfigError> {
        let body = toml::to_string_pretty(&self.raw).map_err(|err| ConfigError::Format {
            path: self.path.clone(),
            detail: err.to_string(),
        })?;
        let mut text = String::from(HEADER);
        let body = body.trim_end_matches('\n');
        if !body.is_empty() {
            text.push('\n');
            text.push_str(body);
        }
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if text == self.text {
            return Ok(false);
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| ConfigError::Io(format!("{}: {err}", parent.display())))?;
            }
        }
        std::fs::write(&self.path, text.as_bytes())
            .map_err(|err| ConfigError::Io(format!("{}: {err}", self.path.display())))?;
        self.text = text;
        Ok(true)
    }
}

/// The settings after every layer has been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub portal_url: Option<String>,
    pub network: Option<String>,
    pub username: Option<String>,
    pub domain: Option<String>,
    /// Never empty: [`crate::api::DEFAULT_CALLBACK`] when no file sets it.
    pub callback: String,
    pub timeouts: Timeouts,
    /// Seconds between two background reconnect checks; `None` means the
    /// installed task does not run on an interval (`reconnect_interval_secs`
    /// unset or `0`, which is the default).
    pub reconnect_interval_secs: Option<u64>,
    /// Seconds after startup before the first check; `None` means it does not
    /// run at startup (`reconnect_boot_delay_secs` unset or `0`, which is the
    /// default). Both unset leaves a task that only runs when started by hand.
    pub reconnect_boot_delay_secs: Option<u64>,
}

impl Effective {
    /// The defaults, overlaid by each layer in turn: a later layer's key wins.
    fn layered(layers: &[&Settings]) -> Effective {
        let mut effective = Effective {
            portal_url: None,
            network: None,
            username: None,
            domain: None,
            callback: crate::api::DEFAULT_CALLBACK.to_string(),
            timeouts: Timeouts {
                connect_ms: DEFAULT_CONNECT_MS,
                read_ms: DEFAULT_READ_MS,
            },
            reconnect_interval_secs: Some(DEFAULT_INTERVAL_SECS),
            reconnect_boot_delay_secs: Some(DEFAULT_BOOT_DELAY_SECS),
        };
        for layer in layers {
            overlay(&mut effective.portal_url, &layer.portal_url);
            overlay(&mut effective.network, &layer.network);
            overlay(&mut effective.username, &layer.username);
            overlay(&mut effective.domain, &layer.domain);
            if let Some(callback) = &layer.callback {
                effective.callback = callback.clone();
            }
            if let Some(ms) = layer.connect_timeout_ms {
                effective.timeouts.connect_ms = ms;
            }
            if let Some(ms) = layer.read_timeout_ms {
                effective.timeouts.read_ms = ms;
            }
            // `0` is the way to switch one off in a later layer, so it must
            // overwrite rather than be skipped like an unset key.
            if let Some(secs) = layer.reconnect_interval_secs {
                effective.reconnect_interval_secs = (secs > 0).then_some(secs);
            }
            if let Some(secs) = layer.reconnect_boot_delay_secs {
                effective.reconnect_boot_delay_secs = (secs > 0).then_some(secs);
            }
        }
        effective
    }
}

impl Default for Effective {
    fn default() -> Self {
        Effective::layered(&[])
    }
}

fn overlay<T: Clone>(slot: &mut Option<T>, layer: &Option<T>) {
    if let Some(value) = layer {
        *slot = Some(value.clone());
    }
}

/// The paths this module works with.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The platform-convention file.
    pub system: PathBuf,
    /// The file next to the executable — the project-level one.
    pub portable: PathBuf,
    /// `~/.srun_portal.json`, when a home directory could be determined.
    pub legacy: Option<PathBuf>,
}

impl Paths {
    pub fn discover() -> Result<Paths, ConfigError> {
        Ok(Paths {
            system: system_config_path()?,
            portable: portable_config_path()?,
            legacy: config::legacy_env_file_path().ok(),
        })
    }
}

/// `directories::ProjectDirs::from("", "", "srun-portal").config_dir()`
/// joined with [`CONFIG_FILE_NAME`]: `$XDG_CONFIG_HOME/srun-portal/srun-portal.toml`
/// on Linux, `~/Library/Application Support/srun-portal/srun-portal.toml` on
/// macOS, `%APPDATA%\srun-portal\config\srun-portal.toml` on Windows. The
/// directories are derived, never hard-coded here.
pub fn system_config_path() -> Result<PathBuf, ConfigError> {
    match directories::ProjectDirs::from("", "", "srun-portal") {
        Some(dirs) => Ok(dirs.config_dir().join(CONFIG_FILE_NAME)),
        // No directory could be derived (no home directory at all): fall back
        // to the passwd home's `.config`, which is what the XDG default is.
        None => {
            let home = sys::home_dir().map_err(ConfigError::Io)?;
            Ok(home
                .join(".config")
                .join("srun-portal")
                .join(CONFIG_FILE_NAME))
        }
    }
}

/// [`CONFIG_FILE_NAME`] next to the executable.
pub fn portable_config_path() -> Result<PathBuf, ConfigError> {
    Ok(sys::exe_dir()
        .map_err(ConfigError::Io)?
        .join(CONFIG_FILE_NAME))
}

/// What [`resolve`] settled on.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The merged settings the CLI runs with.
    pub effective: Effective,
    /// The file a newly resolved portal URL is recorded in: the project file
    /// when it exists, else the system file. `None` never happens — resolution
    /// always ends with a file.
    pub base: Option<ConfigFile>,
    /// [`Resolution::base`]'s path, for display.
    pub base_path: Option<PathBuf>,
    /// Whether this run created the file.
    pub created: bool,
    /// The legacy file that seeded a newly created configuration.
    pub imported_legacy: Option<PathBuf>,
}

/// Load the configuration, creating it when there is none.
///
/// `explicit` is `--config`/`SRUN_PORTAL_CONFIG`: that file is then the whole
/// configuration and neither the system nor the project file is read.
/// `forced` is `--portable`, which skips the first-run question. `ask` is only
/// called when no configuration file exists at all.
pub fn resolve(
    paths: &Paths,
    explicit: Option<&Path>,
    forced: Option<Storage>,
    ask: &mut dyn FnMut(&Paths) -> Storage,
) -> Result<Resolution, ConfigError> {
    if let Some(path) = explicit {
        let (file, created) = match ConfigFile::read(path)? {
            Some(file) => (file, false),
            None => (
                ConfigFile::create(path, &Settings::documented_defaults())?,
                true,
            ),
        };
        let effective = Effective::layered(&[&file.settings]);
        return Ok(Resolution {
            effective,
            base_path: Some(file.path.clone()),
            base: Some(file),
            created,
            imported_legacy: None,
        });
    }

    let mut system = ConfigFile::read(&paths.system)?;
    let mut portable = ConfigFile::read(&paths.portable)?;

    // Read once: it seeds a first-run file and is the last-resort URL.
    let legacy_url = read_legacy_url(paths);
    let mut created = false;
    let mut imported_legacy = None;

    if system.is_none() && portable.is_none() {
        let mut seed = Settings::documented_defaults();
        if let Some(url) = &legacy_url {
            seed.portal_url = Some(url.clone());
            imported_legacy = paths.legacy.clone();
        }
        let storage = forced.unwrap_or_else(|| ask(paths));
        let target = match storage {
            Storage::Portable => paths.portable.clone(),
            Storage::System => paths.system.clone(),
        };
        let file = match ConfigFile::create(&target, &seed) {
            Ok(file) => file,
            // A portable deployment can be read-only (a shared install); the
            // system directory is then the only writable option.
            Err(err) if storage == Storage::Portable => {
                eprintln!("{} is not writable: {err}", target.display());
                ConfigFile::create(&paths.system, &seed)?
            }
            Err(err) => return Err(err),
        };
        eprintln!("Created {}", file.path.display());
        created = true;
        if file.path == paths.system {
            system = Some(file);
        } else {
            portable = Some(file);
        }
    }

    let mut layers: Vec<&Settings> = Vec::new();
    if let Some(file) = system.as_ref() {
        layers.push(&file.settings);
    }
    if let Some(file) = portable.as_ref() {
        layers.push(&file.settings);
    }
    let mut effective = Effective::layered(&layers);
    if effective.portal_url.is_none() {
        effective.portal_url = legacy_url;
    }

    // The project file wins, so that is where the next run looks first.
    let base = portable.or(system);
    Ok(Resolution {
        effective,
        base_path: base.as_ref().map(|file| file.path.clone()),
        base,
        created,
        imported_legacy,
    })
}

/// The effective settings of the files that already exist. Never creates,
/// asks, or writes: the read-only caller (`examples/live_check.rs`) must not
/// touch the user's home.
pub fn load_existing(paths: &Paths, explicit: Option<&Path>) -> Result<Effective, ConfigError> {
    if let Some(path) = explicit {
        return Ok(match ConfigFile::read(path)? {
            Some(file) => Effective::layered(&[&file.settings]),
            None => Effective::default(),
        });
    }
    let system = ConfigFile::read(&paths.system)?;
    let portable = ConfigFile::read(&paths.portable)?;
    let mut layers: Vec<&Settings> = Vec::new();
    if let Some(file) = system.as_ref() {
        layers.push(&file.settings);
    }
    if let Some(file) = portable.as_ref() {
        layers.push(&file.settings);
    }
    let mut effective = Effective::layered(&layers);
    if effective.portal_url.is_none() {
        effective.portal_url = read_legacy_url(paths);
    }
    Ok(effective)
}

/// The legacy file's portal URL, or `None` when there is no legacy file, it is
/// incomplete, or it cannot be read. A broken legacy file must not stop the CLI
/// from starting, so the failure is reported and then ignored.
fn read_legacy_url(paths: &Paths) -> Option<String> {
    let legacy = paths.legacy.as_deref()?;
    match config::read_legacy_from(legacy) {
        Ok(Some(env)) => config::url_from_env(&env),
        Ok(None) => None,
        Err(err) => {
            eprintln!("Ignoring {}: {err}", legacy.display());
            None
        }
    }
}

/// Ask where a first-run configuration file should go.
///
/// Without a terminal there is nobody to ask, so the system location is chosen
/// silently — a scripted first run stays non-interactive.
pub fn ask_storage(paths: &Paths) -> Storage {
    if !sys::stdin_is_tty() {
        return Storage::System;
    }
    eprintln!("No configuration file found.");
    eprintln!(
        "  1) Portable - next to the executable: {}",
        paths.portable.display()
    );
    eprintln!(
        "  2) System   - per this system's conventions: {}",
        paths.system.display()
    );
    loop {
        match sys::prompt_line("Store the configuration where? [2]: ") {
            Ok(Some(answer)) => match answer.trim().to_ascii_lowercase().as_str() {
                "" | "2" | "s" | "system" => return Storage::System,
                "1" | "p" | "portable" => return Storage::Portable,
                _ => eprintln!("Please answer 1 or 2."),
            },
            Ok(None) => return Storage::System,
            Err(err) => {
                eprintln!("{err}");
                return Storage::System;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A uniquely named scratch directory, removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = format!(
                "srun-portal-settings-{}-{}-{}-{}",
                tag,
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// The three locations, all inside one scratch directory. No file exists
    /// until a test writes one.
    fn scratch_paths(dir: &TempDir) -> Paths {
        Paths {
            system: dir.path().join("etc").join(CONFIG_FILE_NAME),
            portable: dir.path().join("bin").join(CONFIG_FILE_NAME),
            legacy: Some(dir.path().join(".srun_portal.json")),
        }
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn never_asked(_: &Paths) -> Storage {
        panic!("no question expected: a configuration file exists")
    }

    fn url(host: &str) -> String {
        format!("https://{host}/srun_portal_pc?ac_id=1")
    }

    #[test]
    fn unknown_keys_survive_rewrites() {
        let dir = TempDir::new("unknown");
        let path = dir.path().join(CONFIG_FILE_NAME);
        write(
            &path,
            "portal_url = \"https://a/srun_portal_pc?ac_id=1\"\nextra = 1\n",
        );

        let mut file = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(file.settings.portal_url.as_deref(), Some(&url("a")[..]));

        let written = file
            .update(&[("portal_url", Value::String(url("b")))])
            .unwrap();
        assert!(written, "a changed value is written");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("extra = 1"), "{text}");
        let reread = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(reread.settings.portal_url.as_deref(), Some(&url("b")[..]));
        assert_eq!(reread.raw.get("extra"), Some(&Value::Integer(1)));
    }

    #[test]
    fn update_writes_only_on_change() {
        let dir = TempDir::new("nochange");
        let path = dir.path().join(CONFIG_FILE_NAME);
        let mut file = ConfigFile::create(&path, &Settings::default()).unwrap();

        assert!(file
            .update(&[("portal_url", Value::String(url("a")))])
            .unwrap());
        let after_first = std::fs::read_to_string(&path).unwrap();
        assert!(!file
            .update(&[("portal_url", Value::String(url("a")))])
            .unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
    }

    #[test]
    fn portable_overrides_system_per_key() {
        let dir = TempDir::new("layers");
        let paths = scratch_paths(&dir);
        write(
            &paths.system,
            "callback = \"b\"\nconnect_timeout_ms = 100\nreconnect_interval_secs = 600\n",
        );
        write(&paths.portable, "portal_url = \"https://p/srun_portal_pc?ac_id=1\"\ncallback = \"p\"\nreconnect_interval_secs = 60\n");

        let resolution = resolve(&paths, None, None, &mut never_asked).unwrap();

        assert_eq!(resolution.effective.callback, "p");
        assert_eq!(
            resolution.effective.portal_url.as_deref(),
            Some(&url("p")[..])
        );
        // The project file still overrides the system one, key by key.
        assert_eq!(resolution.effective.reconnect_interval_secs, Some(60));
        // The system file still supplies what the project file does not set.
        assert_eq!(resolution.effective.timeouts.connect_ms, 100);
        assert_eq!(resolution.effective.timeouts.read_ms, DEFAULT_READ_MS);
        // Neither file sets this one, so the documented default applies.
        assert_eq!(resolution.effective.reconnect_boot_delay_secs, Some(30));
    }

    #[test]
    fn explicit_config_ignores_the_layering() {
        let dir = TempDir::new("explicit");
        let paths = scratch_paths(&dir);
        write(&paths.system, "callback = \"b\"\n");
        write(&paths.portable, "callback = \"p\"\n");
        let explicit = dir.path().join("chosen.toml");
        write(&explicit, "username = \"explicit\"\n");

        let resolution = resolve(&paths, Some(&explicit), None, &mut never_asked).unwrap();

        assert_eq!(resolution.base_path.as_deref(), Some(explicit.as_path()));
        assert_eq!(resolution.effective.username.as_deref(), Some("explicit"));
        assert_eq!(resolution.effective.callback, crate::api::DEFAULT_CALLBACK);
        assert!(!resolution.created);

        // A missing explicit path is created, seeded with nothing.
        let missing = dir.path().join("nested/new.toml");
        let resolution = resolve(&paths, Some(&missing), None, &mut never_asked).unwrap();
        assert!(resolution.created);
        assert_eq!(resolution.base_path.as_deref(), Some(missing.as_path()));
        assert!(missing.exists());
    }

    #[test]
    fn portable_wins_over_system() {
        let dir = TempDir::new("portable-wins");
        let paths = scratch_paths(&dir);
        write(
            &paths.system,
            "portal_url = \"https://system/srun_portal_pc?ac_id=1\"\n",
        );
        write(
            &paths.portable,
            "portal_url = \"https://portable/srun_portal_pc?ac_id=1\"\n",
        );

        let resolution = resolve(&paths, None, None, &mut never_asked).unwrap();

        assert_eq!(
            resolution.base_path.as_deref(),
            Some(paths.portable.as_path())
        );
        assert_eq!(
            resolution.effective.portal_url.as_deref(),
            Some(&url("portable")[..])
        );
        assert!(!resolution.created);
    }

    #[test]
    fn first_run_creates_the_chosen_location() {
        let dir = TempDir::new("first-run");
        let paths = scratch_paths(&dir);

        let mut choose_portable = |_: &Paths| Storage::Portable;
        let resolution = resolve(&paths, None, None, &mut choose_portable).unwrap();
        assert!(resolution.created);
        assert_eq!(
            resolution.base_path.as_deref(),
            Some(paths.portable.as_path())
        );
        assert!(paths.portable.exists());
        assert!(!paths.system.exists());

        let dir = TempDir::new("first-run-system");
        let paths = scratch_paths(&dir);
        let mut choose_system = |_: &Paths| Storage::System;
        let resolution = resolve(&paths, None, None, &mut choose_system).unwrap();
        assert!(resolution.created);
        assert_eq!(
            resolution.base_path.as_deref(),
            Some(paths.system.as_path())
        );
        assert!(paths.system.exists());
        assert!(!paths.portable.exists());

        // `--portable` answers the question without asking.
        let dir = TempDir::new("first-run-forced");
        let paths = scratch_paths(&dir);
        let resolution = resolve(&paths, None, Some(Storage::Portable), &mut never_asked).unwrap();
        assert!(resolution.created);
        assert_eq!(
            resolution.base_path.as_deref(),
            Some(paths.portable.as_path())
        );
    }

    #[test]
    fn non_tty_first_run_defaults_to_system() {
        if sys::stdin_is_tty() {
            return; // the question would block on a terminal
        }
        let dir = TempDir::new("non-tty");
        assert_eq!(ask_storage(&scratch_paths(&dir)), Storage::System);
    }

    #[test]
    fn legacy_seeds_created_config() {
        let dir = TempDir::new("legacy-seed");
        let paths = scratch_paths(&dir);
        write(
            paths.legacy.as_ref().unwrap(),
            r#"{"authURL":"https://net.szu.edu.cn","acid":"1"}"#,
        );

        let mut choose_system = |_: &Paths| Storage::System;
        let resolution = resolve(&paths, None, None, &mut choose_system).unwrap();

        assert_eq!(
            resolution.imported_legacy.as_deref(),
            paths.legacy.as_deref()
        );
        assert_eq!(
            resolution.effective.portal_url.as_deref(),
            Some("https://net.szu.edu.cn/srun_portal_pc?ac_id=1")
        );
        let created = std::fs::read_to_string(&paths.system).unwrap();
        assert!(
            created.contains("portal_url = \"https://net.szu.edu.cn/srun_portal_pc?ac_id=1\""),
            "{created}"
        );
        // The legacy file itself is left alone.
        assert!(std::fs::read_to_string(paths.legacy.as_ref().unwrap())
            .unwrap()
            .contains("\"acid\":\"1\""));
    }

    #[test]
    fn legacy_is_only_a_fallback() {
        let dir = TempDir::new("legacy-fallback");
        let paths = scratch_paths(&dir);
        write(
            &paths.system,
            &format!("portal_url = \"{}\"\n", url("configured")),
        );
        write(
            paths.legacy.as_ref().unwrap(),
            r#"{"authURL":"https://legacy","acid":"9"}"#,
        );
        let before = std::fs::read_to_string(&paths.system).unwrap();

        let resolution = resolve(&paths, None, None, &mut never_asked).unwrap();

        assert_eq!(
            resolution.effective.portal_url.as_deref(),
            Some(&url("configured")[..])
        );
        assert_eq!(resolution.imported_legacy, None);
        assert_eq!(std::fs::read_to_string(&paths.system).unwrap(), before);

        // A configuration without a URL still falls back to the legacy one.
        write(&paths.system, "username = \"someone\"\n");
        let resolution = resolve(&paths, None, None, &mut never_asked).unwrap();
        assert_eq!(
            resolution.effective.portal_url.as_deref(),
            Some("https://legacy/srun_portal_pc?ac_id=9")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_portable_falls_back_to_system() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("read-only");
        let paths = scratch_paths(&dir);
        let portable_dir = paths.portable.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&portable_dir).unwrap();
        std::fs::set_permissions(&portable_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        if unsafe { libc::geteuid() } == 0 {
            return; // root writes anywhere, so there is nothing to test
        }

        let resolution = resolve(&paths, None, Some(Storage::Portable), &mut never_asked).unwrap();

        assert_eq!(
            resolution.base_path.as_deref(),
            Some(paths.system.as_path())
        );
        assert!(paths.system.exists());
        assert!(!paths.portable.exists());

        std::fs::set_permissions(&portable_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn defaults_match_the_documentation() {
        let dir = TempDir::new("defaults");
        let paths = scratch_paths(&dir);
        write(&paths.system, "# nothing but a comment\nextra = \"kept\"\n");

        let effective = load_existing(&paths, None).unwrap();

        assert_eq!(effective, Effective::default());
        assert_eq!(effective.callback, "jsonp");
        assert_eq!(
            effective.timeouts,
            Timeouts {
                connect_ms: DEFAULT_CONNECT_MS,
                read_ms: DEFAULT_READ_MS,
            }
        );
        assert_eq!(effective.portal_url, None);
        // The background task is on by default: installing one that never runs
        // would be pointless. `0` is how a file switches either half off.
        assert_eq!(effective.reconnect_interval_secs, Some(300));
        assert_eq!(effective.reconnect_boot_delay_secs, Some(30));
    }

    #[test]
    fn the_catalogue_covers_every_key_the_loader_reads() {
        // The catalogue is what `config set`/`unset` validate against, so a key
        // the loader understands but the catalogue misses would be unsettable.
        let dir = TempDir::new("catalogue");
        let path = dir.path().join(CONFIG_FILE_NAME);
        let mut raw = toml::Table::new();
        for spec in KEY_CATALOGUE {
            raw.insert(
                spec.name.to_string(),
                match spec.kind {
                    KeyKind::Str => Value::String(String::new()),
                    KeyKind::Count => Value::Integer(1),
                },
            );
        }
        let text = toml::to_string_pretty(&raw).unwrap();
        write(&path, &text);

        // Parsing succeeds, which means every catalogue key is a real key, and
        // `entries` returns exactly all of them.
        let file = ConfigFile::read(&path).unwrap().unwrap();
        let names: Vec<&str> = file.entries().into_iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            KEY_CATALOGUE
                .iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn config_set_and_unset_write_through_the_file() {
        let dir = TempDir::new("set-unset");
        let path = dir.path().join(CONFIG_FILE_NAME);
        let mut file = ConfigFile::create(&path, &Settings::documented_defaults()).unwrap();

        // `set` accepts what the key's type allows, and refuses the rest.
        let value = parse_key_value("username", "testuser").unwrap();
        assert_eq!(value, Value::String("testuser".to_string()));
        assert_eq!(
            parse_key_value("read_timeout_ms", "20000").unwrap(),
            Value::Integer(20_000)
        );
        assert_eq!(
            parse_key_value("read_timeout_ms", "soon").unwrap_err(),
            "key \"read_timeout_ms\" must be a non-negative integer"
        );
        assert_eq!(
            parse_key_value("nope", "x").unwrap_err(),
            "Unknown configuration key: nope"
        );

        file.update(&[("username", value)]).unwrap();
        file.update(&[("reconnect_interval_secs", Value::Integer(600))])
            .unwrap();
        let reread = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(reread.settings.username.as_deref(), Some("testuser"));
        assert_eq!(reread.settings.reconnect_interval_secs, Some(600));

        // Removing one key leaves the others, and a second remove is a no-op.
        assert!(file.remove(&["reconnect_interval_secs"]).unwrap());
        assert!(!file.remove(&["reconnect_interval_secs"]).unwrap());
        let reread = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(reread.settings.reconnect_interval_secs, None);
        assert_eq!(reread.settings.username.as_deref(), Some("testuser"));

        // A key this version does not know is not deleted by name.
        file.update(&[("future_key", Value::Integer(1))]).unwrap();
        assert!(!file.remove(&["future_key"]).unwrap());
        assert_eq!(
            ConfigFile::read(&path)
                .unwrap()
                .unwrap()
                .raw
                .get("future_key"),
            Some(&Value::Integer(1))
        );
    }

    #[test]
    fn unsetting_returns_to_the_documented_defaults() {
        let dir = TempDir::new("unset-defaults");
        let path = dir.path().join(CONFIG_FILE_NAME);
        let mut file = ConfigFile::create(&path, &Settings::documented_defaults()).unwrap();
        file.update(&[("reconnect_interval_secs", Value::Integer(600))])
            .unwrap();

        file.remove(&["reconnect_interval_secs"]).unwrap();

        // Gone from the file, so the documented default applies again.
        let reread = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(reread.settings.reconnect_interval_secs, None);
        assert_eq!(
            Effective::layered(&[&reread.settings]).reconnect_interval_secs,
            Some(300)
        );
    }

    #[test]
    fn a_created_file_shows_the_documented_defaults() {
        let dir = TempDir::new("template");
        let paths = scratch_paths(&dir);
        ConfigFile::create(&paths.system, &Settings::documented_defaults()).unwrap();

        let text = std::fs::read_to_string(&paths.system).unwrap();
        // Every documented key is present with its default, including `domain`
        // and both cadences switched off (`0`).
        for line in [
            "domain = \"\"",
            "connect_timeout_ms = 5000",
            "read_timeout_ms = 10000",
            "reconnect_interval_secs = 300",
            "reconnect_boot_delay_secs = 30",
        ] {
            assert!(text.contains(line), "{line} missing from:\n{text}");
        }
        // Nothing else was invented.
        assert!(!text.contains("username ="), "{text}");
        assert!(!text.contains("portal_url ="), "{text}");

        // The template means what it says: read back, it is the same effective
        // settings as a configuration that says nothing at all.
        let effective = load_existing(&paths, None).unwrap();
        assert_eq!(effective.reconnect_interval_secs, Some(300));
        assert_eq!(effective.reconnect_boot_delay_secs, Some(30));
        assert_eq!(effective.timeouts, Effective::default().timeouts);
        assert_eq!(effective.domain.as_deref(), Some(""));
    }

    #[test]
    fn setting_zero_switches_a_background_off() {
        let dir = TempDir::new("zero");
        let paths = scratch_paths(&dir);
        write(
            &paths.system,
            "reconnect_interval_secs = 600\nreconnect_boot_delay_secs = 60\n",
        );

        let resolution = resolve(&paths, None, None, &mut never_asked).unwrap();
        assert_eq!(resolution.effective.reconnect_interval_secs, Some(600));
        assert_eq!(resolution.effective.reconnect_boot_delay_secs, Some(60));

        // A later layer switches one off without unsaying the other.
        write(&paths.portable, "reconnect_interval_secs = 0\n");
        let resolution = resolve(&paths, None, None, &mut never_asked).unwrap();
        assert_eq!(resolution.effective.reconnect_interval_secs, None);
        assert_eq!(resolution.effective.reconnect_boot_delay_secs, Some(60));
    }

    #[test]
    fn bad_value_reports_path_and_key() {
        let dir = TempDir::new("bad-value");
        let paths = scratch_paths(&dir);
        write(&paths.system, "connect_timeout_ms = \"x\"\n");

        let err = resolve(&paths, None, None, &mut never_asked).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("connect_timeout_ms"), "{message}");
        assert!(
            message.contains(&paths.system.display().to_string()),
            "{message}"
        );
        assert!(matches!(err, ConfigError::Format { .. }));

        // A negative timeout is not a duration.
        write(&paths.system, "read_timeout_ms = -1\n");
        assert!(matches!(
            load_existing(&paths, None),
            Err(ConfigError::Format { .. })
        ));

        // The reconnect keys are seconds, and reject the same way.
        write(&paths.system, "reconnect_interval_secs = \"often\"\n");
        let message = resolve(&paths, None, None, &mut never_asked)
            .unwrap_err()
            .to_string();
        assert!(message.contains("reconnect_interval_secs"), "{message}");
        assert!(
            message.contains("must be a non-negative integer"),
            "{message}"
        );
        write(&paths.system, "reconnect_boot_delay_secs = -5\n");
        assert!(matches!(
            load_existing(&paths, None),
            Err(ConfigError::Format { .. })
        ));
    }

    #[test]
    fn system_path_uses_the_convention_dir() {
        let path = system_config_path().unwrap();
        assert!(
            path.ends_with(Path::new("srun-portal").join(CONFIG_FILE_NAME)),
            "{path:?}"
        );
        assert_eq!(
            portable_config_path().unwrap(),
            sys::exe_dir().unwrap().join(CONFIG_FILE_NAME)
        );
    }
}
