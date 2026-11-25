//! The configuration file: discovery, layering and first-run creation.
//!
//! Discovery settles on a [`ConfigSource`]: an ordered list of files read low
//! to high, the single file writes go to, and the legacy `~/.srun_portal.json`
//! that may still supply a portal URL. By default the list is the **system**
//! file (`$XDG_CONFIG_HOME/srun-portal`, `~/Library/Application
//! Support/srun-portal`, `%APPDATA%\srun-portal\config`) followed by the
//! **portable** file next to the executable; `--config`/`SRUN_PORTAL_CONFIG`
//! replaces the list with that one file.
//!
//! Reading and initialising are separate operations. [`load_source`] only
//! reads — a missing layer is simply absent, and a run whose layers are all
//! missing has default values, not a new file. Only `config set`, the
//! interactive login and `service install` create the write target, and they do
//! it through [`Resolution::update`], which holds the file's cross-process lock
//! while it re-reads, re-migrates and merges. Two concurrent writers therefore
//! both land: the second one merges into what the first wrote instead of
//! overwriting it with a stale snapshot.
//!
//! Writes are atomic ([`crate::persistence`]): a reader sees the old file or the
//! new one, never a truncated mixture. Unknown keys are kept verbatim, except
//! for the four legacy login/timeout keys, which are migrated away on the next
//! write. No password is ever stored here.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml::Value;

use crate::account::AccountIdentity;
use crate::config::{self, ConfigError};
use crate::persistence::FileLock;
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
#   portal_url          full portal URL, e.g. \"https://net.szu.edu.cn/srun_portal_pc?ac_id=1\"
#   username            account name used as the default for the login prompt
#   timeout_ms          HTTP connection and read timeout in milliseconds (default 10000)
#   reconnect_interval_secs   background reconnect interval in seconds (default 300; 0: no interval)
#   reconnect_boot_delay_secs delay after startup before the first check (default 30; 0: not at startup)
";

fn migrate_legacy_keys(raw: &mut toml::Table, snapshot: &Effective) {
    let has_legacy_timeout =
        raw.contains_key("connect_timeout_ms") || raw.contains_key("read_timeout_ms");
    if has_legacy_timeout && !raw.contains_key("timeout_ms") {
        let timeout_ms = snapshot.timeouts.connect_ms.max(snapshot.timeouts.read_ms);
        raw.insert("timeout_ms".to_string(), Value::Integer(timeout_ms as i64));
    }
    raw.remove("connect_timeout_ms");
    raw.remove("read_timeout_ms");

    if let Some(domain) = raw.get("domain").and_then(Value::as_str) {
        if domain.is_empty() {
            raw.remove("domain");
        } else if let Some(username) = snapshot.username.as_deref().filter(|name| !name.is_empty())
        {
            let identity = AccountIdentity::parse(username, Some(domain));
            raw.insert(
                "username".to_string(),
                Value::String(identity.qualified().to_string()),
            );
            raw.remove("domain");
        }
    }
    raw.remove("callback");
}
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// How often the installed background task checks the connection. A task that
/// does not run is not worth installing, so this is on by default; setting the
/// key to `0` switches the interval off.
const DEFAULT_INTERVAL_SECS: u64 = 300;
/// How long the installed task waits after startup before its first check, so a
/// boot-time race with the network coming up is not a failed login. `0` switches
/// the startup run off.
const DEFAULT_BOOT_DELAY_SECS: u64 = 30;

/// The accepted range of every bounded key. A value outside it is rejected
/// before it is written, so a hand-edited or scripted file cannot produce a
/// timeout that never expires or a task that runs in a tight loop.
pub const MAX_TIMEOUT_MS: u64 = 60_000;
pub const MAX_INTERVAL_SECS: u64 = 86_340;
pub const MAX_BOOT_DELAY_SECS: u64 = 86_400;

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
    pub username: Option<String>,
    pub timeout_ms: Option<u64>,
    pub reconnect_interval_secs: Option<u64>,
    pub reconnect_boot_delay_secs: Option<u64>,
    legacy_domain: Option<String>,
    legacy_callback: Option<String>,
    legacy_connect_timeout_ms: Option<u64>,
    legacy_read_timeout_ms: Option<u64>,
}

impl Settings {
    /// The values a **newly created** file is seeded with.
    ///
    /// They are the documented defaults written out, so the file shows every
    /// current key and what it means instead of being empty. `Settings::default()`
    /// stays the "nothing is set" value the layering needs; this is the template
    /// for a fresh file.
    fn documented_defaults() -> Settings {
        Settings {
            portal_url: None,
            username: None,
            timeout_ms: Some(DEFAULT_TIMEOUT_MS),
            reconnect_interval_secs: Some(DEFAULT_INTERVAL_SECS),
            reconnect_boot_delay_secs: Some(DEFAULT_BOOT_DELAY_SECS),
            ..Settings::default()
        }
    }

    /// The keys that are set, as TOML values.
    fn pairs(&self) -> Vec<(&'static str, Value)> {
        let mut pairs = Vec::new();
        if let Some(value) = &self.portal_url {
            pairs.push(("portal_url", Value::String(value.clone())));
        }
        if let Some(value) = &self.username {
            pairs.push(("username", Value::String(value.clone())));
        }
        if let Some(value) = self.timeout_ms {
            pairs.push(("timeout_ms", Value::Integer(value as i64)));
        }
        if let Some(value) = self.reconnect_interval_secs {
            pairs.push(("reconnect_interval_secs", Value::Integer(value as i64)));
        }
        if let Some(value) = self.reconnect_boot_delay_secs {
            pairs.push(("reconnect_boot_delay_secs", Value::Integer(value as i64)));
        }
        pairs
    }

    /// Read current and legacy keys out of a parsed table, reporting the first
    /// key whose type is wrong. Legacy values are kept only long enough to
    /// preserve behavior until the next configuration write.
    fn from_table(table: &toml::Table, path: &Path) -> Result<Settings, ConfigError> {
        Ok(Settings {
            portal_url: optional_string(table, "portal_url", path)?,
            username: optional_string(table, "username", path)?,
            timeout_ms: optional_millis(table, "timeout_ms", path)?,
            reconnect_interval_secs: optional_millis(table, "reconnect_interval_secs", path)?,
            reconnect_boot_delay_secs: optional_millis(table, "reconnect_boot_delay_secs", path)?,
            legacy_domain: optional_string(table, "domain", path)?,
            legacy_callback: optional_string(table, "callback", path)?,
            legacy_connect_timeout_ms: optional_millis(table, "connect_timeout_ms", path)?,
            legacy_read_timeout_ms: optional_millis(table, "read_timeout_ms", path)?,
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

/// Result of removing requested keys from one configuration file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// Whether each input key was actually present and removed, in input order.
    pub removed: Vec<bool>,
    /// Whether migration or removal changed the serialized file.
    pub written: bool,
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
        Self::parse(path, text).map(Some)
    }

    fn parse(path: &Path, text: String) -> Result<ConfigFile, ConfigError> {
        let raw: toml::Table = toml::from_str(&text).map_err(|err| ConfigError::Format {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })?;
        let settings = Settings::from_table(&raw, path)?;
        Ok(ConfigFile {
            path: path.to_path_buf(),
            settings,
            raw,
            text,
        })
    }

    /// A fresh file holding `seed`, not yet on disk. The caller holds the file's
    /// lock, so no other process can create it in between.
    fn fresh(path: &Path, seed: &Settings) -> ConfigFile {
        let mut raw = toml::Table::new();
        for (key, value) in seed.pairs() {
            raw.insert(key.to_string(), value);
        }
        ConfigFile {
            path: path.to_path_buf(),
            settings: seed.clone(),
            raw,
            text: String::new(),
        }
    }

    /// Merge `changes` into the table after migrating legacy keys, and write it
    /// back when the result differs from what is already on disk.
    fn update(
        &mut self,
        changes: &[(&str, Value)],
        snapshot: &Effective,
    ) -> Result<bool, ConfigError> {
        migrate_legacy_keys(&mut self.raw, snapshot);
        for (key, value) in changes {
            self.raw.insert((*key).to_string(), value.clone());
        }
        self.settings = Settings::from_table(&self.raw, &self.path)?;
        self.flush()
    }

    /// Remove current keys after migrating legacy keys. Unknown keys are left
    /// alone. User removals happen after migration, so an explicit unset wins.
    fn remove(
        &mut self,
        keys: &[&str],
        snapshot: &Effective,
    ) -> Result<RemoveOutcome, ConfigError> {
        migrate_legacy_keys(&mut self.raw, snapshot);
        let removed = keys
            .iter()
            .map(|key| {
                KEY_CATALOGUE.iter().any(|entry| entry.name == *key)
                    && self.raw.remove(*key).is_some()
            })
            .collect();
        self.settings = Settings::from_table(&self.raw, &self.path)?;
        let written = self.flush()?;
        Ok(RemoveOutcome { removed, written })
    }

    /// Every key the file currently sets, in the current-catalogue order, with
    /// legacy compatibility keys appended when present.
    pub fn entries(&self) -> Vec<(&'static str, String)> {
        KEY_CATALOGUE
            .iter()
            .chain(LEGACY_KEY_CATALOGUE.iter())
            .filter_map(|entry| {
                self.raw
                    .get(entry.name)
                    .map(|value| (entry.name, render_value(value)))
            })
            .collect()
    }

    /// Serialize the table and replace the file atomically, unless the text is
    /// unchanged. The caller holds the file's lock.
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
        crate::persistence::atomic_write(&self.path, text.as_bytes(), false)
            .map_err(|err| ConfigError::Io(format!("{}: {err}", self.path.display())))?;
        self.text = text;
        Ok(true)
    }
}

/// The current writable configuration keys. Legacy keys are intentionally
/// absent so `config set` and `config unset` cannot create or manage them.
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

/// Current writable keys, in the order used by the generated file header.
pub const KEY_CATALOGUE: &[KeySpec] = &[
    KeySpec {
        name: "portal_url",
        kind: KeyKind::Str,
        about: "full portal URL used when no argument is given",
    },
    KeySpec {
        name: "username",
        kind: KeyKind::Str,
        about: "account used as the default for the login prompt",
    },
    KeySpec {
        name: "timeout_ms",
        kind: KeyKind::Count,
        about: "HTTP connection and read timeout in milliseconds",
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

/// Legacy keys accepted while reading old files and shown read-only until the
/// next configuration write migrates them.
pub const LEGACY_KEY_CATALOGUE: &[KeySpec] = &[
    KeySpec {
        name: "domain",
        kind: KeyKind::Str,
        about: "legacy account suffix; use username = \"user@domain\"",
    },
    KeySpec {
        name: "callback",
        kind: KeyKind::Str,
        about: "legacy JSONP callback; new files use the protocol default",
    },
    KeySpec {
        name: "connect_timeout_ms",
        kind: KeyKind::Count,
        about: "legacy HTTP connect timeout; migrated to timeout_ms on write",
    },
    KeySpec {
        name: "read_timeout_ms",
        kind: KeyKind::Count,
        about: "legacy HTTP read timeout; migrated to timeout_ms on write",
    },
];

/// The catalogue entry for `name`, if this version has one.
pub fn key_spec(name: &str) -> Option<&'static KeySpec> {
    KEY_CATALOGUE.iter().find(|entry| entry.name == name)
}

/// Parse `text` as the kind of value `key` holds, or explain what was wrong.
///
/// Deliberately strict: the file must keep holding the type
/// `Settings::from_table` expects. Legacy keys are accepted for reading but are
/// not accepted by `config set`.
pub fn parse_key_value(key: &str, text: &str) -> Result<Value, String> {
    let Some(spec) = key_spec(key) else {
        return Err(format!("Unknown configuration key: {key}"));
    };
    match spec.kind {
        KeyKind::Str => Ok(Value::String(text.to_string())),
        KeyKind::Count => match text.parse::<u64>() {
            Ok(number) if number <= i64::MAX as u64 => Ok(Value::Integer(number as i64)),
            Ok(_) => Err(format!(
                "key \"{key}\" must be no greater than {}",
                i64::MAX
            )),
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

/// The settings after every layer has been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub portal_url: Option<String>,
    pub username: Option<String>,
    pub domain: Option<String>,
    /// Never empty: [`crate::api::DEFAULT_CALLBACK`] when no file sets it.
    pub callback: String,
    pub timeouts: Timeouts,
    /// Seconds between two background reconnect checks; `None` means the
    /// installed task does not run on an interval (`reconnect_interval_secs`
    /// unset or `0`, which is the default).
    pub reconnect_interval_secs: Option<u64>,
    /// Seconds after startup before the first background check; `None` means it
    /// does not run at startup (`reconnect_boot_delay_secs` unset or `0`, which
    /// is the default). Both unset leaves a task that only runs when started by
    /// hand.
    pub reconnect_boot_delay_secs: Option<u64>,
}

impl Effective {
    /// The defaults, overlaid by each layer in turn: a later layer's key wins.
    fn layered(layers: &[&Settings]) -> Effective {
        let mut effective = Effective {
            portal_url: None,
            username: None,
            domain: None,
            callback: crate::api::DEFAULT_CALLBACK.to_string(),
            timeouts: Timeouts {
                connect_ms: DEFAULT_TIMEOUT_MS,
                read_ms: DEFAULT_TIMEOUT_MS,
            },
            reconnect_interval_secs: Some(DEFAULT_INTERVAL_SECS),
            reconnect_boot_delay_secs: Some(DEFAULT_BOOT_DELAY_SECS),
        };
        for layer in layers {
            overlay(&mut effective.portal_url, &layer.portal_url);
            overlay(&mut effective.username, &layer.username);
            overlay(&mut effective.domain, &layer.legacy_domain);
            if let Some(callback) = &layer.legacy_callback {
                effective.callback = callback.clone();
            }
            if let Some(ms) = layer.timeout_ms {
                effective.timeouts.connect_ms = ms;
                effective.timeouts.read_ms = ms;
            } else {
                if let Some(ms) = layer.legacy_connect_timeout_ms {
                    effective.timeouts.connect_ms = ms;
                }
                if let Some(ms) = layer.legacy_read_timeout_ms {
                    effective.timeouts.read_ms = ms;
                }
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

fn layered_with_legacy(layers: &[&Settings], legacy_url: Option<&str>) -> Effective {
    let mut effective = Effective::layered(layers);
    if effective.portal_url.is_none() {
        effective.portal_url = legacy_url.map(str::to_string);
    }
    effective
}

fn overlay<T: Clone>(slot: &mut Option<T>, layer: &Option<T>) {
    if let Some(value) = layer {
        *slot = Some(value.clone());
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

/// Every file one run reads, in increasing precedence, and the single file it
/// writes.
///
/// It is recorded in `installed.toml` so the background task reads exactly what
/// the install read, whichever directory it is started from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSource {
    /// Read low to high: a later file's key wins.
    pub layers: Vec<PathBuf>,
    /// The only file writes go to.
    pub write_path: PathBuf,
    /// `~/.srun_portal.json`, consulted only for a portal URL when the layers
    /// do not supply one. `None` for an explicit single-file source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_path: Option<PathBuf>,
}

impl ConfigSource {
    /// The source a run uses: the explicit path when one was given, else the
    /// system and portable files.
    ///
    /// Never creates or asks. `forced` only decides where a *new* file would go
    /// when neither exists, so `--portable` still selects the portable location
    /// without touching the disk.
    pub fn discover(
        explicit: Option<&Path>,
        forced: Option<Storage>,
    ) -> Result<ConfigSource, ConfigError> {
        if let Some(path) = explicit {
            return Ok(ConfigSource {
                layers: vec![path.to_path_buf()],
                write_path: path.to_path_buf(),
                legacy_path: None,
            });
        }
        let system = system_config_path()?;
        let portable = portable_config_path()?;
        let mut layers = vec![system.clone()];
        if !same_physical(&system, &portable)? {
            layers.push(portable.clone());
        }
        let write_path = if portable.exists() {
            portable.clone()
        } else if system.exists() {
            system.clone()
        } else {
            match forced {
                Some(Storage::Portable) => portable.clone(),
                _ => system.clone(),
            }
        };
        Ok(ConfigSource {
            layers,
            write_path,
            legacy_path: config::legacy_env_file_path().ok(),
        })
    }

    /// A source with one file, for tests and for a caller that already knows
    /// the path it wants.
    pub fn single(path: impl Into<PathBuf>) -> ConfigSource {
        let path = path.into();
        ConfigSource {
            layers: vec![path.clone()],
            write_path: path,
            legacy_path: None,
        }
    }

    /// Whether any layer exists. A source with nothing on disk has default
    /// values only.
    pub fn exists(&self) -> Result<bool, ConfigError> {
        for layer in &self.layers {
            if layer.is_file() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The system and portable locations this platform uses, for the first-run
    /// question.
    pub fn locations(&self) -> (PathBuf, PathBuf) {
        match self.layers.as_slice() {
            [system] => (system.clone(), system.clone()),
            [system, portable, ..] => (system.clone(), portable.clone()),
            [] => (self.write_path.clone(), self.write_path.clone()),
        }
    }
}

fn same_physical(left: &Path, right: &Path) -> Result<bool, ConfigError> {
    if left == right {
        return Ok(true);
    }
    if !left.exists() || !right.exists() {
        return Ok(false);
    }
    same_file::is_same_file(left, right).map_err(|err| {
        ConfigError::Io(format!(
            "cannot compare configuration files {} and {}: {err}",
            left.display(),
            right.display()
        ))
    })
}

/// What [`load_source`] read.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The merged settings.
    pub effective: Effective,
    /// The layers that exist, low to high.
    pub files: Vec<ConfigFile>,
    /// The legacy file that supplied the portal URL, if any.
    pub imported_legacy: Option<PathBuf>,
}

/// Read every layer of `source` and merge them, low to high.
///
/// Read-only: a missing layer is skipped, and a source whose layers are all
/// missing yields the documented defaults. Deciding whether those defaults are
/// enough is [`validate`]'s job, not this one's.
pub fn load_source(source: &ConfigSource) -> Result<Loaded, ConfigError> {
    let mut files: Vec<ConfigFile> = Vec::new();
    for layer in &source.layers {
        let Some(file) = ConfigFile::read(layer)? else {
            continue;
        };
        if files
            .last()
            .is_some_and(|seen| same_physical(&seen.path, &file.path).unwrap_or(false))
        {
            continue;
        }
        files.push(file);
    }
    let legacy_url = read_legacy_url(source.legacy_path.as_deref());
    let layers: Vec<&Settings> = files.iter().map(|file| &file.settings).collect();
    let effective = layered_with_legacy(&layers, legacy_url.as_deref());
    // The legacy file only counted when no layer supplied a URL itself.
    let imported_legacy = legacy_url
        .filter(|url| effective.portal_url.as_deref() == Some(url.as_str()))
        .and_then(|_| source.legacy_path.clone());
    Ok(Loaded {
        effective,
        files,
        imported_legacy,
    })
}

/// Parse a portal URL slug and require an http(s) origin.
///
/// The one rule `validate` and the read-only commands share: a portal URL that
/// does not name a web origin cannot be contacted.
pub(crate) fn parse_http_portal_url(slug: &str) -> Result<config::PortalUrl, ConfigError> {
    let portal = config::parse_portal_url(slug)
        .map_err(|err| ConfigError::Invalid(format!("portal_url: {err}")))?;
    if !(portal.origin.starts_with("http://") || portal.origin.starts_with("https://")) {
        return Err(ConfigError::Invalid(format!(
            "portal_url: {} is not an http or https URL",
            portal.origin
        )));
    }
    Ok(portal)
}

/// The bounded timeout rule, kept as one function so every caller reports the
/// same message for the same value.
pub(crate) fn validate_timeouts(timeouts: Timeouts) -> Result<(), ConfigError> {
    for (name, value) in [
        ("timeout_ms", timeouts.connect_ms),
        ("timeout_ms", timeouts.read_ms),
    ] {
        if !(1..=MAX_TIMEOUT_MS).contains(&value) {
            return Err(ConfigError::Invalid(format!(
                "{name} must be between 1 and {MAX_TIMEOUT_MS} (got {value})"
            )));
        }
    }
    Ok(())
}

/// Check that the effective settings can actually be used.
///
/// `require_identity` is what the unattended paths ask for: without a portal URL
/// and an account there is nothing to authenticate as, and failing here is
/// better than a task that reports an anonymous error every interval. Values
/// whose range is bounded are checked even when the identity is not required, so
/// `config validate` catches a timeout of zero before a run does.
pub fn validate(effective: &Effective, require_identity: bool) -> Result<(), ConfigError> {
    if let Some(slug) = effective.portal_url.as_deref() {
        parse_http_portal_url(slug)?;
    }
    if let Some(username) = effective.username.as_deref() {
        if username.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "username must not be empty".to_string(),
            ));
        }
    }
    validate_timeouts(effective.timeouts)?;
    if let Some(secs) = effective.reconnect_interval_secs {
        if secs > MAX_INTERVAL_SECS {
            return Err(ConfigError::Invalid(format!(
                "reconnect_interval_secs must be at most {MAX_INTERVAL_SECS} (got {secs})"
            )));
        }
    }
    if let Some(secs) = effective.reconnect_boot_delay_secs {
        if secs > MAX_BOOT_DELAY_SECS {
            return Err(ConfigError::Invalid(format!(
                "reconnect_boot_delay_secs must be at most {MAX_BOOT_DELAY_SECS} (got {secs})"
            )));
        }
    }
    if require_identity {
        if effective.portal_url.is_none() {
            return Err(ConfigError::Invalid(
                crate::messages::PORTAL_URL_REQUIRED.to_string(),
            ));
        }
        if effective.username.is_none() {
            return Err(ConfigError::Invalid(
                "username is required for an unattended reconnect".to_string(),
            ));
        }
    }
    Ok(())
}

/// The effective settings plus the ability to change the write target.
///
/// Every mutation re-reads the layers **inside** the file's cross-process lock,
/// re-migrates legacy keys against the effective values, merges the requested
/// change and only then replaces the file. A failed write leaves both the file
/// and this value as they were.
#[derive(Debug, Clone)]
pub struct Resolution {
    source: ConfigSource,
    effective: Effective,
}

impl Resolution {
    /// Load `source` without creating anything.
    pub fn open(source: ConfigSource) -> Result<Resolution, ConfigError> {
        let loaded = load_source(&source)?;
        Ok(Resolution {
            source,
            effective: loaded.effective,
        })
    }

    pub fn source(&self) -> &ConfigSource {
        &self.source
    }

    pub fn effective(&self) -> &Effective {
        &self.effective
    }

    /// The write target's contents as `config list` shows them; empty when the
    /// file does not exist yet.
    pub fn entries(&self) -> Result<Vec<(&'static str, String)>, ConfigError> {
        Ok(ConfigFile::read(&self.source.write_path)?
            .map(|file| file.entries())
            .unwrap_or_default())
    }

    /// The file that supplies `key` in the effective settings: the highest layer
    /// that sets it, or the legacy file for a portal URL it alone provides.
    pub fn value_source(&self, key: &str) -> Result<Option<PathBuf>, ConfigError> {
        let loaded = load_source(&self.source)?;
        let mut found = None;
        for file in &loaded.files {
            if file.entries().iter().any(|(name, _)| *name == key) {
                found = Some(file.path.clone());
            }
        }
        if found.is_none() && key == "portal_url" && loaded.imported_legacy.is_some() {
            found = loaded.imported_legacy;
        }
        Ok(found)
    }

    /// Merge `changes` into the write target, creating it when it is missing.
    pub fn update(&mut self, changes: &[(&str, Value)]) -> Result<bool, ConfigError> {
        let _lock = self.lock()?;
        let snapshot = load_source(&self.source)?.effective;
        let mut file = match ConfigFile::read(&self.source.write_path)? {
            Some(file) => file,
            None => ConfigFile::fresh(&self.source.write_path, &Settings::documented_defaults()),
        };
        let written = file.update(changes, &snapshot)?;
        self.refresh()?;
        Ok(written)
    }

    /// Remove `keys` from the write target, revealing lower layers or defaults.
    pub fn remove(&mut self, keys: &[&str]) -> Result<RemoveOutcome, ConfigError> {
        let _lock = self.lock()?;
        let snapshot = load_source(&self.source)?.effective;
        let Some(mut file) = ConfigFile::read(&self.source.write_path)? else {
            // Nothing on disk: nothing was set and nothing is written.
            let removed = keys
                .iter()
                .map(|key| {
                    KEY_CATALOGUE.iter().any(|entry| entry.name == *key)
                        && snapshot_has(&snapshot, key)
                })
                .collect();
            return Ok(RemoveOutcome {
                removed,
                written: false,
            });
        };
        let outcome = file.remove(keys, &snapshot)?;
        self.refresh()?;
        Ok(outcome)
    }

    fn lock(&self) -> Result<FileLock, ConfigError> {
        FileLock::acquire(&self.source.write_path)
            .map_err(|err| ConfigError::Io(format!("{}: {err}", self.source.write_path.display())))
    }

    fn refresh(&mut self) -> Result<(), ConfigError> {
        self.effective = load_source(&self.source)?.effective;
        Ok(())
    }
}

/// Whether the effective settings carry a value for `key`, used when the write
/// target does not exist and only the answer matters.
fn snapshot_has(effective: &Effective, key: &str) -> bool {
    match key {
        "portal_url" => effective.portal_url.is_some(),
        "username" => effective.username.is_some(),
        "reconnect_interval_secs" => effective.reconnect_interval_secs.is_some(),
        "reconnect_boot_delay_secs" => effective.reconnect_boot_delay_secs.is_some(),
        // The timeout keys always have a value (the default applies).
        _ => true,
    }
}

/// The legacy file's portal URL, or `None` when there is no legacy file, it is
/// incomplete, or it cannot be read. A broken legacy file must not stop the CLI
/// from starting, so the failure is reported and then ignored.
fn read_legacy_url(legacy: Option<&Path>) -> Option<String> {
    let legacy = legacy?;
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
/// silently — a scripted first run stays non-interactive. Only the three
/// initialising commands call this, and only when no layer exists.
pub fn ask_storage(system: &Path, portable: &Path) -> Storage {
    if !sys::stdin_is_tty() {
        return Storage::System;
    }
    eprintln!("No configuration file found.");
    eprintln!(
        "  1) Portable - next to the executable: {}",
        portable.display()
    );
    eprintln!(
        "  2) System   - per this system's conventions: {}",
        system.display()
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

/// Point a source that has no file anywhere at the location the user chose.
pub fn choose_first_run(source: &mut ConfigSource, storage: Storage) {
    let (system, portable) = source.locations();
    source.write_path = match storage {
        Storage::Portable => portable,
        Storage::System => system,
    };
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

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn url(host: &str) -> String {
        format!("https://{host}/srun_portal_pc?ac_id=1")
    }

    /// A two-layer source: a system file and a portable file.
    fn layered_source(dir: &TempDir) -> ConfigSource {
        ConfigSource {
            layers: vec![
                dir.path().join("etc").join(CONFIG_FILE_NAME),
                dir.path().join("bin").join(CONFIG_FILE_NAME),
            ],
            write_path: dir.path().join("bin").join(CONFIG_FILE_NAME),
            legacy_path: None,
        }
    }

    #[test]
    fn unknown_keys_survive_rewrites() {
        let dir = TempDir::new("unknown");
        let path = dir.path().join(CONFIG_FILE_NAME);
        write(
            &path,
            "portal_url = \"https://a/srun_portal_pc?ac_id=1\"\nextra = 1\n",
        );

        let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();
        assert_eq!(
            resolution.effective().portal_url.as_deref(),
            Some(&url("a")[..])
        );

        let written = resolution
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
    fn legacy_keys_work_until_the_next_write_then_migrate() {
        let dir = TempDir::new("legacy-migration");
        let path = dir.path().join(CONFIG_FILE_NAME);
        write(
            &path,
            "username = \"testuser\"\n\
             domain = \"szu\"\n\
             callback = \"cb\"\n\
             connect_timeout_ms = 5000\n\
             read_timeout_ms = 12000\n",
        );

        let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();
        assert_eq!(resolution.effective().domain.as_deref(), Some("szu"));
        assert_eq!(resolution.effective().callback, "cb");
        assert_eq!(resolution.effective().timeouts.connect_ms, 5000);
        assert_eq!(resolution.effective().timeouts.read_ms, 12000);
        let names: Vec<&str> = resolution
            .entries()
            .unwrap()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(
            names,
            vec![
                "username",
                "domain",
                "callback",
                "connect_timeout_ms",
                "read_timeout_ms",
            ]
        );

        resolution
            .update(&[("username", Value::String("next".to_string()))])
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("domain ="));
        assert!(!text.contains("callback ="));
        assert!(!text.contains("connect_timeout_ms ="));
        assert!(!text.contains("read_timeout_ms ="));
        assert!(text.contains("timeout_ms = 12000"));

        let reread = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(reread.settings.username.as_deref(), Some("next"));
        assert_eq!(reread.settings.timeout_ms, Some(12000));
    }

    #[test]
    fn legacy_timeout_migration_uses_the_prewrite_effective_maximum() {
        for (tag, body, expected) in [
            ("connect-only", "connect_timeout_ms = 5000\n", 10_000),
            ("read-only", "read_timeout_ms = 12000\n", 12_000),
        ] {
            let dir = TempDir::new(tag);
            let path = dir.path().join(CONFIG_FILE_NAME);
            write(&path, body);
            let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();

            resolution
                .update(&[("username", Value::String("alice".to_string()))])
                .unwrap();

            let file = ConfigFile::read(&path).unwrap().unwrap();
            assert_eq!(file.settings.timeout_ms, Some(expected));
            assert_eq!(resolution.effective().timeouts.connect_ms, expected);
            assert_eq!(resolution.effective().timeouts.read_ms, expected);
            let text = std::fs::read_to_string(path).unwrap();
            assert!(!text.contains("connect_timeout_ms"));
            assert!(!text.contains("read_timeout_ms"));
        }
    }

    #[test]
    fn layered_timeout_migration_preserves_the_larger_effective_value() {
        for (tag, system_body, expected) in [
            ("lower-legacy", "read_timeout_ms = 12000\n", 12_000),
            ("lower-current", "timeout_ms = 15000\n", 15_000),
        ] {
            let dir = TempDir::new(tag);
            let source = layered_source(&dir);
            write(&source.layers[0], system_body);
            write(&source.layers[1], "connect_timeout_ms = 5000\n");
            let system_before = std::fs::read_to_string(&source.layers[0]).unwrap();
            let mut resolution = Resolution::open(source.clone()).unwrap();

            resolution
                .update(&[("username", Value::String("alice".to_string()))])
                .unwrap();

            assert_eq!(
                std::fs::read_to_string(&source.layers[0]).unwrap(),
                system_before
            );
            let portable = ConfigFile::read(&source.write_path).unwrap().unwrap();
            assert_eq!(portable.settings.timeout_ms, Some(expected));
            assert_eq!(resolution.effective().timeouts.connect_ms, expected);
            assert_eq!(resolution.effective().timeouts.read_ms, expected);
        }
    }

    #[test]
    fn explicit_timeout_set_and_unset_win_after_migration() {
        let dir = TempDir::new("timeout-set-unset");
        let path = dir.path().join(CONFIG_FILE_NAME);
        write(
            &path,
            "connect_timeout_ms = 5000\nread_timeout_ms = 12000\n",
        );
        let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();
        resolution
            .update(&[("timeout_ms", Value::Integer(7000))])
            .unwrap();
        let file = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(file.settings.timeout_ms, Some(7000));
        assert_eq!(file.settings.legacy_connect_timeout_ms, None);
        assert_eq!(file.settings.legacy_read_timeout_ms, None);

        write(
            &path,
            "timeout_ms = 9000\nconnect_timeout_ms = 5000\nread_timeout_ms = 12000\n",
        );
        let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();
        let outcome = resolution.remove(&["timeout_ms"]).unwrap();
        assert_eq!(outcome.removed, [true]);
        assert!(outcome.written);
        let file = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(file.settings.timeout_ms, None);
        assert_eq!(file.settings.legacy_connect_timeout_ms, None);
        assert_eq!(file.settings.legacy_read_timeout_ms, None);
        assert_eq!(
            resolution.effective().timeouts,
            Effective::default().timeouts
        );
    }

    #[test]
    fn layered_timeout_unset_reveals_lower_current_value() {
        let dir = TempDir::new("layered-timeout-unset");
        let source = layered_source(&dir);
        write(&source.layers[0], "timeout_ms = 15000\n");
        write(
            &source.layers[1],
            "timeout_ms = 9000\nconnect_timeout_ms = 5000\n",
        );
        let mut resolution = Resolution::open(source.clone()).unwrap();

        resolution.remove(&["timeout_ms"]).unwrap();

        let portable = ConfigFile::read(&source.write_path).unwrap().unwrap();
        assert_eq!(portable.settings.timeout_ms, None);
        assert_eq!(portable.settings.legacy_connect_timeout_ms, None);
        assert_eq!(resolution.effective().timeouts.connect_ms, 15_000);
        assert_eq!(resolution.effective().timeouts.read_ms, 15_000);
    }

    #[test]
    fn domain_migration_materializes_the_effective_qualified_username() {
        for (tag, username, domain, expected) in [
            ("plain-domain", "alice", "szu", "alice@szu"),
            ("at-domain", "alice", "@szu", "alice@szu"),
            ("embedded-domain", "alice@staff", "student", "alice@staff"),
            ("trailing-at", "alice@", "szu", "alice@@szu"),
        ] {
            let dir = TempDir::new(tag);
            let path = dir.path().join(CONFIG_FILE_NAME);
            write(
                &path,
                &format!("username = \"{username}\"\ndomain = \"{domain}\"\n"),
            );
            let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();

            resolution
                .update(&[("portal_url", Value::String(url("portal")))])
                .unwrap();

            let file = ConfigFile::read(&path).unwrap().unwrap();
            assert_eq!(file.settings.username.as_deref(), Some(expected));
            assert_eq!(file.settings.legacy_domain, None);
            assert_eq!(resolution.effective().username.as_deref(), Some(expected));
            assert_eq!(resolution.effective().domain, None);
        }
    }

    #[test]
    fn domain_migration_uses_lower_username_or_waits_without_one() {
        let dir = TempDir::new("layered-domain");
        let source = layered_source(&dir);
        write(&source.layers[0], "username = \"alice\"\n");
        write(&source.layers[1], "domain = \"szu\"\n");
        let mut resolution = Resolution::open(source.clone()).unwrap();

        resolution
            .update(&[("portal_url", Value::String(url("portal")))])
            .unwrap();

        let portable = ConfigFile::read(&source.write_path).unwrap().unwrap();
        assert_eq!(portable.settings.username.as_deref(), Some("alice@szu"));
        assert_eq!(portable.settings.legacy_domain, None);
        assert_eq!(
            resolution.effective().username.as_deref(),
            Some("alice@szu")
        );
    }

    #[test]
    fn update_writes_only_on_change() {
        let dir = TempDir::new("nochange");
        let path = dir.path().join(CONFIG_FILE_NAME);
        let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();

        assert!(resolution
            .update(&[("portal_url", Value::String(url("a")))])
            .unwrap());
        let after_first = std::fs::read_to_string(&path).unwrap();
        assert!(!resolution
            .update(&[("portal_url", Value::String(url("a")))])
            .unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
    }

    #[test]
    fn portable_overrides_system_per_key() {
        let dir = TempDir::new("layers");
        let source = layered_source(&dir);
        write(
            &source.layers[0],
            "callback = \"b\"\nconnect_timeout_ms = 100\nreconnect_interval_secs = 600\n",
        );
        write(
            &source.layers[1],
            "portal_url = \"https://p/srun_portal_pc?ac_id=1\"\n",
        );

        let effective = load_source(&source).unwrap().effective;

        // `callback` is a legacy key, so the lower layer still supplies it.
        assert_eq!(effective.callback, "b");
        assert_eq!(effective.portal_url.as_deref(), Some(&url("p")[..]));
        // The system file still supplies what the project file does not set.
        assert_eq!(effective.timeouts.connect_ms, 100);
        assert_eq!(effective.timeouts.read_ms, DEFAULT_TIMEOUT_MS);
        // The project file does not set this one, so the lower layer wins.
        assert_eq!(effective.reconnect_interval_secs, Some(600));
        assert_eq!(effective.reconnect_boot_delay_secs, Some(30));
    }

    #[test]
    fn identical_and_hard_linked_layers_are_read_once() {
        let dir = TempDir::new("identical-paths");
        let path = dir.path().join(CONFIG_FILE_NAME);
        write(&path, &format!("portal_url = {:?}\n", url("shared")));
        let source = ConfigSource {
            layers: vec![path.clone(), path.clone()],
            write_path: path.clone(),
            legacy_path: None,
        };
        assert_eq!(load_source(&source).unwrap().files.len(), 1);

        let dir = TempDir::new("hard-linked-paths");
        let source = layered_source(&dir);
        write(
            &source.layers[0],
            &format!("portal_url = {:?}\n", url("linked")),
        );
        std::fs::create_dir_all(source.layers[1].parent().unwrap()).unwrap();
        std::fs::hard_link(&source.layers[0], &source.layers[1]).unwrap();
        let loaded = load_source(&source).unwrap();
        assert_eq!(loaded.files.len(), 1);
        assert_eq!(
            loaded.effective.portal_url.as_deref(),
            Some(&url("linked")[..])
        );
    }

    #[test]
    fn a_missing_configuration_is_defaults_not_a_new_file() {
        let dir = TempDir::new("missing");
        let source = layered_source(&dir);

        let loaded = load_source(&source).unwrap();
        assert_eq!(loaded.effective, Effective::default());
        assert!(!source.layers[0].exists());
        assert!(!source.layers[1].exists());
        assert!(!source.exists().unwrap());

        // Reading is not initialising: nothing was created.
        let resolution = Resolution::open(source.clone()).unwrap();
        assert!(resolution.entries().unwrap().is_empty());
        assert!(!source.layers[0].exists());
        assert!(!source.layers[1].exists());
    }

    #[test]
    fn validate_rejects_unusable_values() {
        let base = Effective::default();
        assert!(validate(&base, false).is_ok());

        let mut bad = base.clone();
        bad.portal_url = Some("not-a-url".to_string());
        assert!(validate(&bad, false).is_err());

        let mut bad = base.clone();
        bad.portal_url = Some("ftp://host/srun_portal_pc?ac_id=1".to_string());
        assert!(validate(&bad, false).is_err(), "scheme must be http(s)");

        let mut bad = base.clone();
        bad.username = Some("   ".to_string());
        assert!(validate(&bad, false).is_err());

        let mut bad = base.clone();
        bad.timeouts.connect_ms = 0;
        assert!(validate(&bad, false).is_err());
        bad.timeouts.connect_ms = MAX_TIMEOUT_MS + 1;
        assert!(validate(&bad, false).is_err());

        let mut bad = base.clone();
        bad.reconnect_interval_secs = Some(MAX_INTERVAL_SECS + 1);
        assert!(validate(&bad, false).is_err());
        bad.reconnect_interval_secs = Some(MAX_INTERVAL_SECS);
        assert!(validate(&bad, false).is_ok());

        let mut bad = base.clone();
        bad.reconnect_boot_delay_secs = Some(MAX_BOOT_DELAY_SECS + 1);
        assert!(validate(&bad, false).is_err());

        // Identity is only required when asked for.
        assert!(validate(&base, true).is_err());
        let mut complete = base.clone();
        complete.portal_url = Some(url("portal"));
        complete.username = Some("alice".to_string());
        assert!(validate(&complete, true).is_ok());
    }

    #[test]
    fn a_bad_legacy_value_is_reported_with_path_and_key() {
        let dir = TempDir::new("bad-value");
        let source = layered_source(&dir);
        write(&source.layers[0], "connect_timeout_ms = \"x\"\n");

        let err = load_source(&source).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("connect_timeout_ms"), "{message}");
        assert!(
            message.contains(&source.layers[0].display().to_string()),
            "{message}"
        );

        write(&source.layers[0], "reconnect_interval_secs = \"often\"\n");
        let message = load_source(&source).unwrap_err().to_string();
        assert!(message.contains("reconnect_interval_secs"), "{message}");
    }

    #[test]
    fn legacy_seeds_the_effective_url_without_writing() {
        let dir = TempDir::new("legacy-seed");
        let mut source = layered_source(&dir);
        let legacy = dir.path().join(".srun_portal.json");
        write(
            &legacy,
            r#"{"authURL":"https://net.szu.edu.cn","acid":"1"}"#,
        );
        source.legacy_path = Some(legacy.clone());

        let loaded = load_source(&source).unwrap();
        assert_eq!(
            loaded.effective.portal_url.as_deref(),
            Some("https://net.szu.edu.cn/srun_portal_pc?ac_id=1")
        );
        assert_eq!(loaded.imported_legacy.as_deref(), Some(legacy.as_path()));
        assert!(!source.layers[0].exists());
        assert!(!source.layers[1].exists());

        // A layer that sets the URL wins over the legacy file.
        write(
            &source.layers[0],
            &format!("portal_url = \"{}\"\n", url("configured")),
        );
        let loaded = load_source(&source).unwrap();
        assert_eq!(
            loaded.effective.portal_url.as_deref(),
            Some(&url("configured")[..])
        );
        assert_eq!(loaded.imported_legacy, None);
    }

    #[test]
    fn concurrent_updates_to_different_keys_both_land() {
        let dir = TempDir::new("concurrent");
        let path = dir.path().join(CONFIG_FILE_NAME);
        write(&path, "username = \"alice\"\n");

        // Two resolutions read the same snapshot, as two processes would…
        let mut first = Resolution::open(ConfigSource::single(&path)).unwrap();
        let mut second = Resolution::open(ConfigSource::single(&path)).unwrap();
        assert_eq!(first.effective().username.as_deref(), Some("alice"));
        assert_eq!(second.effective().username.as_deref(), Some("alice"));

        // …and each writes its own key. The lock makes the second re-read, so
        // the first one's key is still there afterwards.
        first
            .update(&[("timeout_ms", Value::Integer(20_000))])
            .unwrap();
        second
            .update(&[("reconnect_interval_secs", Value::Integer(600))])
            .unwrap();

        let file = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(file.settings.timeout_ms, Some(20_000));
        assert_eq!(file.settings.reconnect_interval_secs, Some(600));
        assert_eq!(file.settings.username.as_deref(), Some("alice"));
        assert_eq!(second.effective().timeouts.read_ms, 20_000);
    }

    #[test]
    fn a_failed_write_leaves_the_old_file_parseable() {
        let dir = TempDir::new("write-failure");
        let path = dir.path().join(CONFIG_FILE_NAME);
        write(&path, &format!("portal_url = {:?}\n", url("kept")));

        // A negative integer is rejected while re-parsing the merged table, so
        // the write never starts and the old bytes stay as they were.
        let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();
        assert!(resolution
            .update(&[("timeout_ms", Value::Integer(-5))])
            .is_err());
        let reread = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(
            reread.settings.portal_url.as_deref(),
            Some(&url("kept")[..])
        );
        assert_eq!(reread.settings.timeout_ms, None);
    }

    #[test]
    fn unset_reports_each_key_and_prunes_only_known_keys() {
        let dir = TempDir::new("set-unset");
        let path = dir.path().join(CONFIG_FILE_NAME);
        let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();

        let value = parse_key_value("username", "testuser").unwrap();
        resolution.update(&[("username", value)]).unwrap();
        resolution
            .update(&[("reconnect_interval_secs", Value::Integer(600))])
            .unwrap();
        let reread = ConfigFile::read(&path).unwrap().unwrap();
        assert_eq!(reread.settings.username.as_deref(), Some("testuser"));
        assert_eq!(reread.settings.reconnect_interval_secs, Some(600));

        let outcome = resolution
            .remove(&[
                "reconnect_interval_secs",
                "portal_url",
                "reconnect_interval_secs",
            ])
            .unwrap();
        assert_eq!(outcome.removed, [true, false, false]);
        assert!(outcome.written);
        let outcome = resolution.remove(&["reconnect_interval_secs"]).unwrap();
        assert_eq!(outcome.removed, [false]);
        assert!(!outcome.written);

        // A key this version does not know is not deleted by name.
        resolution
            .update(&[("future_key", Value::Integer(1))])
            .unwrap();
        let outcome = resolution.remove(&["future_key"]).unwrap();
        assert_eq!(outcome.removed, [false]);
        assert!(!outcome.written);
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
    fn a_created_file_shows_the_documented_defaults() {
        let dir = TempDir::new("template");
        let path = dir.path().join("nested").join(CONFIG_FILE_NAME);

        let mut resolution = Resolution::open(ConfigSource::single(&path)).unwrap();
        resolution
            .update(&[("username", Value::String("alice".to_string()))])
            .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        for line in [
            "timeout_ms = 10000",
            "reconnect_interval_secs = 300",
            "reconnect_boot_delay_secs = 30",
        ] {
            assert!(text.contains(line), "{line} missing from:\n{text}");
        }
        assert!(!text.contains("domain ="), "{text}");
        assert!(!text.contains("callback ="), "{text}");
        assert_eq!(resolution.effective().username.as_deref(), Some("alice"));
    }

    #[test]
    fn setting_zero_switches_a_background_off() {
        let dir = TempDir::new("zero");
        let source = layered_source(&dir);
        write(
            &source.layers[0],
            "reconnect_interval_secs = 600\nreconnect_boot_delay_secs = 60\n",
        );

        let effective = load_source(&source).unwrap().effective;
        assert_eq!(effective.reconnect_interval_secs, Some(600));
        assert_eq!(effective.reconnect_boot_delay_secs, Some(60));

        // A later layer switches one off without unsaying the other.
        write(&source.layers[1], "reconnect_interval_secs = 0\n");
        let effective = load_source(&source).unwrap().effective;
        assert_eq!(effective.reconnect_interval_secs, None);
        assert_eq!(effective.reconnect_boot_delay_secs, Some(60));
    }

    #[test]
    fn the_catalogue_covers_every_current_writable_key() {
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
        write(&path, &toml::to_string_pretty(&raw).unwrap());

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
    fn the_defaults_match_the_documentation() {
        let dir = TempDir::new("defaults");
        let source = layered_source(&dir);
        write(
            &source.layers[0],
            "# nothing but a comment\nextra = \"kept\"\n",
        );

        let effective = load_source(&source).unwrap().effective;

        assert_eq!(effective, Effective::default());
        assert_eq!(effective.callback, "jsonp");
        assert_eq!(
            effective.timeouts,
            Timeouts {
                connect_ms: DEFAULT_TIMEOUT_MS,
                read_ms: DEFAULT_TIMEOUT_MS,
            }
        );
        assert_eq!(effective.portal_url, None);
        assert_eq!(effective.reconnect_interval_secs, Some(300));
        assert_eq!(effective.reconnect_boot_delay_secs, Some(30));
    }

    #[test]
    fn system_path_uses_the_convention_dir() {
        let path = system_config_path().unwrap();
        // `directories` puts config under `.../srun-portal` on Linux and macOS
        // and under `.../srun-portal/config` on Windows (see the doc comment on
        // `system_config_path`), so the directory itself is what is asserted.
        assert!(
            path.components()
                .any(|component| component.as_os_str() == "srun-portal"),
            "{path:?}"
        );
        assert!(path.ends_with(CONFIG_FILE_NAME), "{path:?}");
        assert_eq!(
            portable_config_path().unwrap(),
            sys::exe_dir().unwrap().join(CONFIG_FILE_NAME)
        );
    }

    /// Whether the default write target exists *now*, so a test can prove
    /// discovery changed nothing without assuming a clean machine.
    fn source_default_write_path() -> bool {
        system_config_path()
            .map(|path| path.exists())
            .unwrap_or(false)
            || portable_config_path()
                .map(|path| path.exists())
                .unwrap_or(false)
    }

    #[test]
    fn discover_never_creates_or_asks() {
        let dir = TempDir::new("discover");
        let explicit = dir.path().join("chosen.toml");
        let source = ConfigSource::discover(Some(&explicit), None).unwrap();
        assert_eq!(source.layers, vec![explicit.clone()]);
        assert_eq!(source.write_path, explicit);
        assert!(source.legacy_path.is_none());
        assert!(!explicit.exists());

        // The default source lists both platform locations, and the write
        // target is whichever exists, without any file being created.
        let before = source_default_write_path();
        let source = ConfigSource::discover(None, None).unwrap();
        assert!(!source.layers.is_empty());
        assert_eq!(
            source.write_path.exists(),
            before,
            "discovery reads, it never creates"
        );
        // A forced location only decides where a *new* file would go; an
        // existing configuration keeps being written where it already is.
        let portable = ConfigSource::discover(None, Some(Storage::Portable)).unwrap();
        let chosen = portable_config_path().unwrap();
        if !system_config_path().unwrap().exists() && !chosen.exists() {
            assert_eq!(portable.write_path, chosen);
        }
    }
}
