//! Where the unattended password is kept, and which one is used.
//!
//! A credential is addressed by an **opaque key** produced from the Portal and
//! the account: `v2:<urlencoded origin>:<urlencoded qualified account>`, see
//! [`credential_key`]. The Portal is part of the key because the same account
//! name at two portals is two different secrets; the `ac_id` is not, because one
//! origin is one authentication domain.
//!
//! Priority is fixed and observable:
//!
//! 1. a non-empty [`ENV_PASSWORD`] — checked before any store is touched, so it
//!    still works when the file is corrupt and the wallet is locked;
//! 2. **this key's entry in the fallback file**, when it has one;
//! 3. the system credential facility ([`crate::keyring`]).
//!
//! File before keyring is what makes the documented fallback honest: a password
//! stored in the file because the wallet failed stays the one in use, instead of
//! being shadowed by whatever older value the wallet still holds.
//!
//! Storage is a locked transaction. Every read and write of the file happens
//! under its sidecar lock ([`crate::persistence::FileLock`]), and the keyring
//! calls happen inside the same transaction, so two concurrent migrations cannot
//! interleave their keyring and file halves. The file is private from creation
//! (`0600`, or a DACL admitting only this user and `SYSTEM` on Windows), is
//! written atomically, and refuses to be a symlink.
//!
//! The file itself uses [`CREDENTIALS_FILE_NAME`] in the control directory and
//! holds one table of v2 keys:
//!
//! ```toml
//! # srun-portal credentials; keep this file private.
//! [passwords]
//! "v2:https%3A%2F%2Fnet.szu.edu.cn:alice%40staff" = "…"
//! ```
//!
//! Older files used a single top-level `password`, and an even older version
//! keyed the table by the bare account. Neither is ever bound to a portal
//! automatically; [`Status::LegacyUnscoped`] reports them and
//! [`forget_legacy`] is the one command that removes them.

use std::path::{Path, PathBuf};

use serde::Serialize;
use toml::{Table, Value};

use crate::config::{self, ConfigError};
use crate::keyring;
use crate::persistence::{self, FileLock};

/// The credentials file name inside the control directory.
pub const CREDENTIALS_FILE_NAME: &str = "credentials.toml";

/// The environment variable holding the password. It overrides every store.
pub const ENV_PASSWORD: &str = "SRUN_PORTAL_PASSWORD";

/// The comment line every file this module writes starts with.
const HEADER: &str = "# srun-portal credentials; keep this file private.\n";

/// The prefix of every key this version writes.
const KEY_PREFIX: &str = "v2:";

/// Where a password ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    /// The system credential facility (see [`crate::keyring`]).
    Keyring,
    /// The fallback file in the control directory.
    File,
}

impl Store {
    /// One word for a status line.
    pub fn name(self) -> &'static str {
        match self {
            Store::Keyring => "keyring",
            Store::File => "file",
        }
    }
}

/// Which backend a `credentials set` may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The facility, falling back to the file with a warning.
    Auto,
    /// The file only.
    File,
    /// The facility only; a failure is an error, never a silent fallback.
    Keyring,
}

/// What [`status`] found for one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// Stored in the system credential facility.
    Keyring,
    /// Stored in the fallback file under this key.
    File,
    /// The file has an old, unscoped entry (top-level `password` or a bare
    /// account key). It is not used for authentication.
    LegacyUnscoped,
    /// Nothing is stored for this key.
    Missing,
    /// A store could not be read at all: the answer is unknown, not "missing".
    Unavailable {
        /// Which store failed, and why, without any secret.
        detail: String,
    },
}

/// The opaque credential key for `account` at `portal`.
///
/// `v2:` marks the format; both components are URL-encoded so the `:` separators
/// stay unambiguous and a case-insensitive filesystem or a wallet that folds
/// case cannot merge two keys.
pub fn credential_key(portal: &config::PortalUrl, account: &str) -> String {
    format!(
        "{KEY_PREFIX}{}:{}",
        crate::transport::urlencode(&portal.origin),
        crate::transport::urlencode(account)
    )
}

/// The system credential facility, behind one object so a test can substitute a
/// failing or slow backend and assert what the user observes.
trait Backend {
    fn store(&self, key: &str, password: &str) -> Result<(), String>;
    fn read(&self, key: &str) -> Result<Option<String>, String>;
    fn delete(&self, key: &str) -> Result<bool, String>;
}

/// The real facility ([`crate::keyring`]).
struct SystemBackend;

impl Backend for SystemBackend {
    fn store(&self, key: &str, password: &str) -> Result<(), String> {
        keyring::store(key, password)
    }

    fn read(&self, key: &str) -> Result<Option<String>, String> {
        keyring::read(key)
    }

    fn delete(&self, key: &str) -> Result<bool, String> {
        keyring::delete(key)
    }
}

/// The record of one credential operation, for the caller to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub store: Store,
    /// Why the file was used instead of the facility, when that happened.
    pub note: Option<String>,
}

/// Keep `password` for `key`.
///
/// `Auto` tries the facility first and writes the file on failure (reporting
/// why). After a successful keyring write the same key's file entry is removed
/// **inside the same transaction**; if that cleanup fails the operation reports
/// an error instead of claiming a migration it did not complete.
pub fn store(key: &str, password: &str, path: &Path, target: Target) -> Result<Stored, String> {
    store_with(&SystemBackend, key, password, path, target)
}

fn store_with(
    backend: &dyn Backend,
    key: &str,
    password: &str,
    path: &Path,
    target: Target,
) -> Result<Stored, String> {
    let fail = |err: ConfigError| err.to_string();
    let _guard = lock(path).map_err(|err| err.to_string())?;
    match target {
        Target::File => {
            write_file(key, password, path).map_err(fail)?;
            Ok(Stored {
                store: Store::File,
                note: None,
            })
        }
        Target::Keyring => {
            backend.store(key, password)?;
            remove_file_entry(key, path).map_err(fail)?;
            Ok(Stored {
                store: Store::Keyring,
                note: None,
            })
        }
        Target::Auto => match backend.store(key, password) {
            Ok(()) => {
                remove_file_entry(key, path).map_err(fail)?;
                Ok(Stored {
                    store: Store::Keyring,
                    note: None,
                })
            }
            Err(err) => {
                write_file(key, password, path).map_err(fail)?;
                Ok(Stored {
                    store: Store::File,
                    note: Some(format!(
                        "Keyring unavailable ({err}); the password is stored in {} instead",
                        path.display()
                    )),
                })
            }
        },
    }
}

/// The password to use for `key`, from `explicit_env` or the stores.
///
/// The environment wins before any store is opened. Otherwise this key's file
/// entry wins over the facility; a file that exists but cannot be read is an
/// error rather than "no password", so a broken store is never mistaken for an
/// account without one.
pub fn resolve(
    key: &str,
    path: &Path,
    explicit_env: Option<&str>,
) -> Result<Option<String>, ConfigError> {
    resolve_with(&SystemBackend, key, path, explicit_env)
}

fn resolve_with(
    backend: &dyn Backend,
    key: &str,
    path: &Path,
    explicit_env: Option<&str>,
) -> Result<Option<String>, ConfigError> {
    if let Some(password) = explicit_env.filter(|value| !value.is_empty()) {
        return Ok(Some(password.to_string()));
    }
    let _guard = match FileLock::acquire(path) {
        Ok(guard) => guard,
        Err(err) => return Err(ConfigError::Io(format!("{}: {err}", path.display()))),
    };
    match inspect_file(key, path)? {
        FileCredential::Password(password) => return Ok(Some(password)),
        FileCredential::Legacy | FileCredential::Missing => {}
    }
    match backend.read(key) {
        Ok(Some(password)) => Ok(Some(password)),
        Ok(None) => Ok(None),
        // The file has no entry and the facility cannot answer: the password
        // may well exist, so this is not "missing".
        Err(err) => Err(ConfigError::Invalid(format!(
            "cannot read the system keyring: {err}"
        ))),
    }
}

/// Forget `key` in both backends.
///
/// Both are attempted even when the first fails; a failure is reported for the
/// whole operation, because a caller that hears "forgotten" must be able to rely
/// on it. "Not found" in both is `Ok(false)`, which is success.
pub fn revoke(key: &str, path: &Path) -> Result<bool, String> {
    revoke_with(&SystemBackend, key, path)
}

fn revoke_with(backend: &dyn Backend, key: &str, path: &Path) -> Result<bool, String> {
    let _guard = lock(path).map_err(|err| err.to_string())?;
    let mut removed = false;
    let mut failures = Vec::new();
    match backend.delete(key) {
        Ok(found) => removed |= found,
        Err(err) => failures.push(format!("keyring: {err}")),
    }
    match remove_file_entry(key, path) {
        Ok(found) => removed |= found,
        Err(err) => failures.push(format!("file: {err}")),
    }
    if failures.is_empty() {
        Ok(removed)
    } else {
        Err(failures.join("; "))
    }
}

/// Inspect both backends for `key` without returning any secret.
///
/// `account` is the bare qualified account, which is what the pre-v2 code filed
/// the wallet entry under; it is probed only after `key` misses, so an entry
/// this version can actually use always wins.
pub fn status(key: &str, account: &str, path: &Path) -> Result<Status, ConfigError> {
    status_with(&SystemBackend, key, account, path)
}

fn status_with(
    backend: &dyn Backend,
    key: &str,
    account: &str,
    path: &Path,
) -> Result<Status, ConfigError> {
    let guard = FileLock::acquire(path)
        .map_err(|err| ConfigError::Io(format!("{}: {err}", path.display())))?;
    let file = inspect_file(key, path);
    drop(guard);
    match file {
        Ok(FileCredential::Password(_)) => return Ok(Status::File),
        Ok(FileCredential::Legacy) => return Ok(Status::LegacyUnscoped),
        Ok(FileCredential::Missing) => {}
        Err(err) => {
            return Ok(Status::Unavailable {
                detail: err.to_string(),
            })
        }
    }
    match backend.read(key) {
        Ok(Some(_)) => return Ok(Status::Keyring),
        Ok(None) => {}
        Err(err) => {
            return Ok(Status::Unavailable {
                detail: format!("keyring: {err}"),
            })
        }
    }
    // The wallet is reachable and has no entry this version can use. An entry
    // under the bare account is what an install before v2 keys wrote, and it is
    // reported rather than hidden as "missing": the password exists, it is only
    // unbound, and `credentials forget --legacy` is the documented way out.
    match backend.read(account) {
        Ok(Some(_)) => Ok(Status::LegacyUnscoped),
        Ok(None) => Ok(Status::Missing),
        Err(err) => Ok(Status::Unavailable {
            detail: format!("keyring: {err}"),
        }),
    }
}

/// Remove the legacy, unscoped entry that belongs to `account` from `path`.
///
/// Only what can belong to this account is touched: a top-level `password`, and
/// a `[passwords]` entry keyed by the bare qualified account (the pre-v2 format).
/// v2 keys, other accounts' keys and unknown data are left alone; a file with
/// several legacy account keys loses only the requested one. The keyring's bare
/// key is deleted too, since that is where the pre-v2 code kept it.
pub fn forget_legacy(account: &str, path: &Path) -> Result<bool, String> {
    forget_legacy_with(&SystemBackend, account, path)
}

fn forget_legacy_with(backend: &dyn Backend, account: &str, path: &Path) -> Result<bool, String> {
    let _guard = lock(path).map_err(|err| err.to_string())?;
    let mut removed = false;
    match backend.delete(account) {
        Ok(found) => removed |= found,
        Err(err) => return Err(format!("keyring: {err}")),
    }

    let Some(table) = read_table(path).map_err(|err| err.to_string())? else {
        return Ok(removed);
    };
    let mut table = table;
    if table.remove("password").is_some() {
        removed = true;
    }
    let mut table_changed = removed;
    if let Some(Value::Table(passwords)) = table.get_mut("passwords") {
        if passwords.remove(account).is_some() {
            removed = true;
            table_changed = true;
        }
        if passwords.is_empty() {
            table.remove("passwords");
            table_changed = true;
        }
    }
    if !table_changed {
        return Ok(removed);
    }
    if table.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("{}: {err}", path.display())),
        }
        return Ok(removed);
    }
    write_table(&table, path).map_err(|err| err.to_string())?;
    Ok(removed)
}

/// The legacy credentials file of an older version: beside the configuration
/// file it was created for. Only `credentials forget --legacy` looks for it.
pub fn legacy_path_for(config: &Path) -> PathBuf {
    let dir = match config.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    dir.join("srun-portal.credentials.toml")
}

fn lock(path: &Path) -> std::io::Result<FileLock> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    FileLock::acquire(path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileCredential {
    Missing,
    Legacy,
    Password(String),
}

fn read_table(path: &Path) -> Result<Option<Table>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(ConfigError::Io(format!("{}: {err}", path.display()))),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|err| ConfigError::Format {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })
}

/// The `[passwords]` table, validating every entry.
///
/// `Ok(None)` means the file is a valid old file with a single top-level
/// `password`; the two shapes cannot be combined, because the old reader refused
/// that and a half-migrated file would be ambiguous.
fn passwords_table(table: &Table, path: &Path) -> Result<Option<Table>, ConfigError> {
    let legacy = table.get("password");
    let passwords = table.get("passwords");
    if legacy.is_some() && passwords.is_some() {
        return Err(format_error(
            path,
            "keys \"password\" and \"passwords\" cannot be used together",
        ));
    }
    if let Some(value) = legacy {
        return match value {
            Value::String(_) => Ok(None),
            _ => Err(format_error(path, "key \"password\" must be a string")),
        };
    }
    let Some(value) = passwords else {
        // A file with neither key holds nothing; anything else is unknown data
        // this version must not reinterpret.
        return Ok(Some(Table::new()));
    };
    let Value::Table(passwords) = value else {
        return Err(format_error(path, "key \"passwords\" must be a table"));
    };
    for (key, value) in passwords {
        if !value.is_str() {
            return Err(format_error(
                path,
                &format!("password for {key:?} must be a string"),
            ));
        }
    }
    Ok(Some(passwords.clone()))
}

fn inspect_file(key: &str, path: &Path) -> Result<FileCredential, ConfigError> {
    let Some(table) = read_table(path)? else {
        return Ok(FileCredential::Missing);
    };
    let Some(passwords) = passwords_table(&table, path)? else {
        return Ok(FileCredential::Legacy);
    };
    if let Some(Value::String(password)) = passwords.get(key) {
        return Ok(FileCredential::Password(password.clone()));
    }
    let has_legacy_key = passwords.contains_key("password")
        || passwords
            .keys()
            .any(|stored| !stored.starts_with(KEY_PREFIX));
    Ok(if has_legacy_key {
        FileCredential::Legacy
    } else {
        FileCredential::Missing
    })
}

fn write_file(key: &str, password: &str, path: &Path) -> Result<(), ConfigError> {
    let mut root = match read_table(path)? {
        Some(table) => table,
        None => Table::new(),
    };
    if root.contains_key("password") {
        return Err(format_error(
            path,
            "legacy unscoped credential file; run `srun-portal credentials forget --legacy` first",
        ));
    }
    let mut passwords = match root.remove("passwords") {
        Some(Value::Table(passwords)) => passwords,
        Some(_) => return Err(format_error(path, "key \"passwords\" must be a table")),
        None => Table::new(),
    };
    passwords.insert(key.to_string(), Value::String(password.to_string()));
    root.insert("passwords".to_string(), Value::Table(passwords));
    write_table(&root, path)
}

/// Replace the file with `table`, private and atomically. The caller holds the
/// lock.
fn write_table(table: &Table, path: &Path) -> Result<(), ConfigError> {
    let serialized = toml::to_string_pretty(table).map_err(|err| ConfigError::Format {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    let body = format!(
        "{HEADER}{}",
        serialized.trim_end_matches('\n').to_string() + "\n"
    );
    persistence::atomic_write(path, body.as_bytes(), true)
        .map_err(|err| ConfigError::Io(format!("{}: {err}", path.display())))
}

fn remove_file_entry(key: &str, path: &Path) -> Result<bool, ConfigError> {
    let Some(table) = read_table(path)? else {
        return Ok(false);
    };
    let Some(passwords) = passwords_table(&table, path)? else {
        // A legacy single-password file has no entry for this key; it is not
        // deleted here because it cannot be attributed to one account.
        return Ok(false);
    };
    if !passwords.contains_key(key) {
        return Ok(false);
    }
    let mut root = table;
    let mut passwords = passwords;
    passwords.remove(key);
    if passwords.is_empty() {
        root.remove("passwords");
    } else {
        root.insert("passwords".to_string(), Value::Table(passwords));
    }
    if root.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(err) => return Err(ConfigError::Io(format!("{}: {err}", path.display()))),
        }
    }
    write_table(&root, path)?;
    Ok(true)
}

/// Tighten `path` to `0600` when it is wider than that. Returns whether it
/// changed the mode; platforms without POSIX permissions always return `false`.
#[cfg(unix)]
pub fn ensure_private_mode(path: &Path) -> Result<bool, ConfigError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path)
        .map_err(|err| ConfigError::Io(format!("{}: {err}", path.display())))?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        return Ok(false);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|err| ConfigError::Io(format!("{}: {err}", path.display())))?;
    Ok(true)
}

/// Tighten `path` to `0600` when it is wider than that. Returns whether it
/// changed the mode; platforms without POSIX permissions always return `false`.
#[cfg(not(unix))]
pub fn ensure_private_mode(_path: &Path) -> Result<bool, ConfigError> {
    Ok(false)
}

fn format_error(path: &Path, detail: &str) -> ConfigError {
    ConfigError::Format {
        path: path.to_path_buf(),
        detail: detail.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "srun-portal-credentials-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn file(&self) -> PathBuf {
            self.path.join(CREDENTIALS_FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn portal(origin: &str) -> config::PortalUrl {
        config::parse_portal_url(&format!("{origin}/srun_portal_pc?ac_id=1")).unwrap()
    }

    #[test]
    fn the_key_separates_portals_and_accounts() {
        let staff = credential_key(&portal("https://net.szu.edu.cn"), "alice@staff");
        let student = credential_key(&portal("https://net.szu.edu.cn"), "alice@student");
        let other = credential_key(&portal("https://other.example"), "alice@staff");

        assert_ne!(staff, student);
        assert_ne!(
            staff, other,
            "the same account at another portal is a new key"
        );
        assert_eq!(
            staff, "v2:https%3A%2F%2Fnet.szu.edu.cn:alice%40staff",
            "the encoding is part of the stored format"
        );
        // The access point does not split the authentication domain.
        let second_ac =
            config::parse_portal_url("https://net.szu.edu.cn/srun_portal_pc?ac_id=7").unwrap();
        assert_eq!(staff, credential_key(&second_ac, "alice@staff"));
        assert!(staff.is_ascii(), "the key survives a byte-oriented store");
    }

    #[test]
    fn a_missing_file_is_no_password() {
        let dir = TempDir::new("missing");
        assert_eq!(
            resolve("v2:https%3A%2F%2Fh:alice", &dir.file(), None).unwrap(),
            None
        );
    }

    #[test]
    fn file_entries_are_isolated_and_updates_preserve_siblings() {
        let dir = TempDir::new("accounts");
        let path = dir.file();
        let staff = "v2:https%3A%2F%2Fh:alice%40staff";
        let student = "v2:https%3A%2F%2Fh:alice%40student";

        store(staff, "staff-secret", &path, Target::File).unwrap();
        store(student, "student-secret", &path, Target::File).unwrap();
        assert_eq!(
            resolve(staff, &path, None).unwrap().as_deref(),
            Some("staff-secret")
        );
        assert_eq!(
            resolve(student, &path, None).unwrap().as_deref(),
            Some("student-secret")
        );

        store(staff, "new-staff", &path, Target::File).unwrap();
        assert_eq!(
            resolve(staff, &path, None).unwrap().as_deref(),
            Some("new-staff")
        );
        assert_eq!(
            resolve(student, &path, None).unwrap().as_deref(),
            Some("student-secret")
        );
    }

    #[test]
    fn the_environment_wins_before_any_store_is_opened() {
        let dir = TempDir::new("env");
        let path = dir.file();
        let key = "v2:https%3A%2F%2Fh:alice";
        store(key, "stored", &path, Target::File).unwrap();
        assert_eq!(
            resolve(key, &path, Some("from-env")).unwrap().as_deref(),
            Some("from-env")
        );
        // An empty variable means "unset", not "an empty password".
        assert_eq!(
            resolve(key, &path, Some("")).unwrap().as_deref(),
            Some("stored")
        );

        // A corrupt file does not stop the environment password.
        std::fs::write(&path, "passwords = 3\n").unwrap();
        assert_eq!(
            resolve(key, &path, Some("from-env")).unwrap().as_deref(),
            Some("from-env")
        );
        assert!(
            resolve(key, &path, None).is_err(),
            "a broken store is reported"
        );
    }

    #[test]
    fn revoking_removes_only_the_requested_key_then_the_file() {
        let dir = TempDir::new("revoke");
        let path = dir.file();
        let staff = "v2:https%3A%2F%2Fh:alice%40staff";
        let student = "v2:https%3A%2F%2Fh:alice%40student";
        store(staff, "a", &path, Target::File).unwrap();
        store(student, "b", &path, Target::File).unwrap();

        remove_file_entry(staff, &path).unwrap();
        assert!(path.exists());
        assert_eq!(resolve(staff, &path, None).unwrap(), None);
        assert_eq!(resolve(student, &path, None).unwrap().as_deref(), Some("b"));
        assert!(remove_file_entry(student, &path).unwrap());
        assert!(!path.exists(), "the last entry takes the file with it");
        assert!(!remove_file_entry(student, &path).unwrap());
    }

    #[test]
    fn an_explicit_file_store_never_touches_the_keyring() {
        let dir = TempDir::new("file-only");
        let path = dir.file();
        let stored = store("v2:https%3A%2F%2Fh:alice", "secret", &path, Target::File).unwrap();
        assert_eq!(stored.store, Store::File);
        assert!(stored.note.is_none());
        assert!(path.exists());
    }

    #[test]
    fn a_legacy_file_is_reported_and_never_bound() {
        let dir = TempDir::new("legacy");
        let path = dir.file();
        std::fs::write(&path, "password = \"unowned-secret\"\n").unwrap();
        let key = "v2:https%3A%2F%2Fh:alice";

        assert_eq!(resolve(key, &path, None).unwrap(), None);
        assert_eq!(status(key, "alice", &path).unwrap(), Status::LegacyUnscoped);

        // Writing a v2 entry into that file is refused rather than producing a
        // shape the reader rejects.
        let err = store(key, "new", &path, Target::File).unwrap_err();
        assert!(err.contains("forget --legacy"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "password = \"unowned-secret\"\n"
        );
    }

    #[test]
    fn legacy_cleanup_removes_only_the_current_account() {
        let dir = TempDir::new("legacy-clean");
        let path = dir.file();
        std::fs::write(
            &path,
            "[passwords]\n\"alice@staff\" = \"old\"\n\"bob@staff\" = \"other\"\n\
             \"v2:https%3A%2F%2Fh:alice%40staff\" = \"current\"\n",
        )
        .unwrap();

        assert!(forget_legacy("alice@staff", &path).unwrap());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"alice@staff\""), "{text}");
        assert!(
            text.contains("\"bob@staff\""),
            "another account is kept: {text}"
        );
        assert!(
            text.contains("v2:https%3A%2F%2Fh:alice%40staff"),
            "a v2 key is not legacy: {text}"
        );
    }

    #[test]
    fn legacy_cleanup_removes_a_top_level_password() {
        let dir = TempDir::new("legacy-top");
        let path = dir.file();
        std::fs::write(&path, "password = \"unowned\"\nfoo = \"kept\"\n").unwrap();

        assert!(forget_legacy("alice@staff", &path).unwrap());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("unowned"), "{text}");
        assert!(
            text.contains("foo = \"kept\""),
            "unknown data is kept: {text}"
        );
    }

    #[test]
    fn legacy_cleanup_reports_nothing_when_there_is_nothing() {
        let dir = TempDir::new("legacy-none");
        let path = dir.file();
        store("v2:https%3A%2F%2Fh:alice", "current", &path, Target::File).unwrap();
        assert!(!forget_legacy("alice@staff", &path).unwrap());
        assert_eq!(
            resolve("v2:https%3A%2F%2Fh:alice", &path, None)
                .unwrap()
                .as_deref(),
            Some("current")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private_from_creation_and_escapes_its_strings() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("round-trip");
        let path = dir.file();
        let key = "v2:https%3A%2F%2Fh:alice%40staff";
        let tricky = "p@ss \"quoted\" \\\\ path";

        store(key, tricky, &path, Target::File).unwrap();

        assert_eq!(resolve(key, &path, None).unwrap().as_deref(), Some(tricky));
        let parsed: Table = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["passwords"][key].as_str(), Some(tricky));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a fresh write is already private");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        store(key, "replacement", &path, Target::File).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an overwritten file never stays wider");
        assert!(!ensure_private_mode(&path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_credentials_file_is_refused() {
        let dir = TempDir::new("symlink");
        let real = dir.path().join("elsewhere");
        std::fs::write(&real, "").unwrap();
        let path = dir.file();
        std::os::unix::fs::symlink(&real, &path).unwrap();

        let err = store("v2:https%3A%2F%2Fh:alice", "secret", &path, Target::File).unwrap_err();
        assert!(err.contains("symbolic link"), "{err}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "");
    }

    #[test]
    fn malformed_files_are_errors_with_the_path() {
        let dir = TempDir::new("malformed");
        let path = dir.file();
        for body in [
            "passwords = \"not a table\"\n",
            "[passwords]\n\"v2:x\" = 1\n",
            "password = \"legacy\"\n[passwords]\n\"v2:x\" = \"new\"\n",
        ] {
            std::fs::write(&path, body).unwrap();
            let err = resolve("v2:x", &path, None).unwrap_err();
            assert!(
                err.to_string().contains(&path.display().to_string()),
                "{err}"
            );
            assert!(matches!(
                status("v2:x", "alice", &path).unwrap(),
                Status::Unavailable { .. }
            ));
        }
    }

    /// A substitute facility, so a failure or a stale value can be asserted
    /// without a wallet.
    #[derive(Default)]
    struct Fake {
        stored: std::sync::Mutex<Option<String>>,
        fail_read: bool,
        fail_store: bool,
        fail_delete: bool,
    }

    impl Backend for Fake {
        fn store(&self, _key: &str, password: &str) -> Result<(), String> {
            if self.fail_store {
                return Err("wallet locked".to_string());
            }
            *self.stored.lock().unwrap() = Some(password.to_string());
            Ok(())
        }

        fn read(&self, _key: &str) -> Result<Option<String>, String> {
            if self.fail_read {
                return Err("no session bus".to_string());
            }
            Ok(self.stored.lock().unwrap().clone())
        }

        fn delete(&self, _key: &str) -> Result<bool, String> {
            if self.fail_delete {
                return Err("access denied".to_string());
            }
            Ok(self.stored.lock().unwrap().take().is_some())
        }
    }

    #[test]
    fn a_stale_keyring_value_never_shadows_the_file_entry() {
        let dir = TempDir::new("file-first");
        let path = dir.file();
        let key = "v2:https%3A%2F%2Fh:alice";
        let backend = Fake::default();
        *backend.stored.lock().unwrap() = Some("old-keyring".to_string());
        store(key, "new-file", &path, Target::File).unwrap();

        assert_eq!(
            resolve_with(&backend, key, &path, None).unwrap().as_deref(),
            Some("new-file"),
            "the file entry is authoritative once it exists"
        );

        // Without a file entry the facility is used again.
        remove_file_entry(key, &path).unwrap();
        assert_eq!(
            resolve_with(&backend, key, &path, None).unwrap().as_deref(),
            Some("old-keyring")
        );
    }

    #[test]
    fn a_failing_facility_falls_back_to_the_file_with_a_warning() {
        let dir = TempDir::new("fallback");
        let path = dir.file();
        let key = "v2:https%3A%2F%2Fh:alice";
        let backend = Fake {
            fail_store: true,
            ..Fake::default()
        };

        let stored = store_with(&backend, key, "secret", &path, Target::Auto).unwrap();

        assert_eq!(stored.store, Store::File);
        assert!(stored.note.unwrap().contains("wallet locked"));
        assert!(path.exists());
        assert_eq!(
            resolve_with(&backend, key, &path, None).unwrap().as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn an_explicit_keyring_store_does_not_fall_back() {
        let dir = TempDir::new("keyring-only");
        let path = dir.file();
        let backend = Fake {
            fail_store: true,
            ..Fake::default()
        };
        let err = store_with(&backend, "v2:x", "secret", &path, Target::Keyring).unwrap_err();
        assert!(err.contains("wallet locked"), "{err}");
        assert!(!path.exists(), "the file is not a silent fallback");

        // The same target succeeds and leaves no file entry behind.
        let backend = Fake::default();
        let stored = store_with(&backend, "v2:x", "secret", &path, Target::Keyring).unwrap();
        assert_eq!(stored.store, Store::Keyring);
        assert!(!path.exists());
    }

    #[test]
    fn a_partial_delete_is_an_error_even_when_the_other_side_worked() {
        let dir = TempDir::new("partial-revoke");
        let path = dir.file();
        let key = "v2:https%3A%2F%2Fh:alice";
        store(key, "secret", &path, Target::File).unwrap();
        let backend = Fake {
            fail_delete: true,
            ..Fake::default()
        };

        let err = revoke_with(&backend, key, &path).unwrap_err();

        assert!(err.contains("keyring"), "{err}");
        assert!(err.contains("access denied"), "{err}");
        assert!(!path.exists(), "the file half still ran");
        // Another account's entry is never collateral damage.
        store("v2:https%3A%2F%2Fh:bob", "other", &path, Target::File).unwrap();
        revoke_with(&Fake::default(), "v2:https%3A%2F%2Fh:nobody", &path).unwrap();
        assert_eq!(
            resolve_with(&Fake::default(), "v2:https%3A%2F%2Fh:bob", &path, None)
                .unwrap()
                .as_deref(),
            Some("other")
        );
    }

    #[test]
    fn a_migration_that_cannot_clean_up_reports_an_error() {
        let dir = TempDir::new("migration-cleanup");
        let path = dir.file();
        let key = "v2:https%3A%2F%2Fh:alice";
        // A file entry that the keyring write is supposed to replace.
        store(key, "from-file", &path, Target::File).unwrap();
        // Make the cleanup fail by turning the file into something unparseable
        // after the keyring write, which is what an unreadable file looks like.
        let backend = Fake::default();
        std::fs::write(&path, "passwords = 3\n").unwrap();
        let err = store_with(&backend, key, "new", &path, Target::Keyring).unwrap_err();
        assert!(err.contains(&path.display().to_string()), "{err}");
        // The keyring write did land; the caller is told the migration is
        // incomplete rather than being told it succeeded.
        assert_eq!(backend.read(key).unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn an_unreadable_facility_is_unavailable_not_missing() {
        let dir = TempDir::new("unavailable");
        let path = dir.file();
        let backend = Fake {
            fail_read: true,
            ..Fake::default()
        };
        assert_eq!(
            status_with(&backend, "v2:https%3A%2F%2Fh:alice", "alice", &path).unwrap(),
            Status::Unavailable {
                detail: "keyring: no session bus".to_string()
            }
        );
        // A file entry still answers even when the facility cannot be read.
        store("v2:https%3A%2F%2Fh:alice", "on-disk", &path, Target::File).unwrap();
        assert_eq!(
            status_with(&backend, "v2:https%3A%2F%2Fh:alice", "alice", &path).unwrap(),
            Status::File
        );
        assert_eq!(
            resolve_with(&backend, "v2:https%3A%2F%2Fh:alice", &path, None)
                .unwrap()
                .as_deref(),
            Some("on-disk")
        );
    }

    #[test]
    fn the_status_distinguishes_file_keyring_and_missing() {
        let dir = TempDir::new("status");
        let path = dir.file();
        let key = "v2:https%3A%2F%2Fh:alice";
        assert_eq!(status(key, "alice", &path).unwrap(), Status::Missing);
        store(key, "secret", &path, Target::File).unwrap();
        assert_eq!(status(key, "alice", &path).unwrap(), Status::File);
    }

    #[test]
    fn a_bare_keyring_entry_from_before_v2_is_reported_not_hidden() {
        use std::collections::HashMap;

        /// A facility that files each entry under its own key, like the real
        /// one does; `Fake` holds a single value and cannot tell two keys apart.
        struct Wallet(HashMap<String, String>);

        impl Backend for Wallet {
            fn store(&self, _key: &str, _password: &str) -> Result<(), String> {
                unreachable!("status never stores")
            }
            fn read(&self, key: &str) -> Result<Option<String>, String> {
                Ok(self.0.get(key).cloned())
            }
            fn delete(&self, _key: &str) -> Result<bool, String> {
                unreachable!("status never deletes")
            }
        }

        let dir = TempDir::new("legacy-keyring");
        let path = dir.file();
        let key = "v2:https%3A%2F%2Fh:alice%40staff";

        // What an install before v2 keys left in the wallet: the bare account,
        // holding a real password the current key never looks at.
        let backend = Wallet(HashMap::from([(
            "alice@staff".to_string(),
            "real-password".to_string(),
        )]));

        assert_eq!(
            status_with(&backend, key, "alice@staff", &path).unwrap(),
            Status::LegacyUnscoped,
            "the password exists, so the answer must not be Missing"
        );
        // It is not usable for authentication either.
        assert_eq!(
            resolve_with(&backend, key, &path, None).unwrap(),
            None,
            "an unbound entry is never bound automatically"
        );

        // A v2 entry under the same account wins over the bare one.
        store(key, "current", &path, Target::File).unwrap();
        assert_eq!(
            status_with(&backend, key, "alice@staff", &path).unwrap(),
            Status::File
        );
    }

    #[test]
    fn the_legacy_file_location_is_beside_the_configuration() {
        assert_eq!(
            legacy_path_for(Path::new("/etc/srun-portal/srun-portal.toml")),
            PathBuf::from("/etc/srun-portal/srun-portal.credentials.toml")
        );
    }
}
