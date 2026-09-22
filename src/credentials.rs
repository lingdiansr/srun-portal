//! Where the unattended password is kept.
//!
//! Two stores, in this order of preference:
//!
//! 1. **the system credential facility** ([`crate::keyring`]) — the Secret
//!    Service, the Windows Credential Manager or the login keychain. This is the
//!    right home for a secret: the wallet owns it, it is unlocked the same way
//!    the user's other passwords are, and nothing of it is left in the
//!    configuration directory.
//! 2. **a `0600` file** next to the configuration,
//!    [`CREDENTIALS_FILE_NAME`] — the fallback for a system that has no usable
//!    facility (a headless machine with no session bus, say).
//!
//! [`store`] and [`read`] pick the facility when it is available and fall back
//! to the file otherwise, so the commands above this module never choose.
//! [`ENV_PASSWORD`] overrides both. The main configuration file itself stays
//! password-free by design (see [`crate::settings`]).
//!
//! The fallback file is TOML with one key, so the escaping rules are the same
//! ones the configuration file already uses:
//!
//! ```toml
//! # srun-portal credentials; keep this file private.
//! password = "…"
//! ```

use std::path::{Path, PathBuf};

use toml::Value;

use crate::config::ConfigError;
use crate::keyring;

/// The credentials file name, next to the configuration file.
pub const CREDENTIALS_FILE_NAME: &str = "srun-portal.credentials.toml";

/// The environment variable holding the password. It overrides every store.
pub const ENV_PASSWORD: &str = "SRUN_PORTAL_PASSWORD";

/// The comment line every file this module writes starts with.
const HEADER: &str = "# srun-portal credentials; keep this file private.\n";

/// Where a password ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    /// The system credential facility (see [`crate::keyring`]).
    Keyring,
    /// The `0600` file next to the configuration.
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

/// The credentials file that belongs to `config`: same directory, so
/// `--config /etc/srun-portal/srun-portal.toml` keeps its secret beside it.
pub fn path_for(config: &Path) -> PathBuf {
    let dir = match config.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    dir.join(CREDENTIALS_FILE_NAME)
}

/// Keep `password` for `account`, preferring the credential facility.
///
/// A facility failure is not fatal: the file takes over and the error is
/// returned alongside the store that was actually used. Returns `(store, note)`
/// where `note` explains the fallback, for the caller to print.
pub fn store(account: &str, password: &str, path: &Path) -> Result<(Store, Option<String>), String> {
    match keyring::availability() {
        keyring::Availability::Ready => match keyring::store(account, password) {
            Ok(()) => Ok((Store::Keyring, None)),
            Err(err) => {
                write_file(path, password)
                    .map_err(|file_err| format!("keyring: {err}; file: {file_err}"))?;
                Ok((
                    Store::File,
                    Some(format!("Keyring unavailable ({err}); stored in the file instead")),
                ))
            }
        },
        keyring::Availability::Unavailable(reason) => {
            write_file(path, password)
                .map_err(|file_err| format!("keyring unavailable ({reason}); file: {file_err}"))?;
            Ok((
                Store::File,
                Some(format!("Keyring unavailable ({reason}); stored in the file instead")),
            ))
        }
    }
}

/// The stored password for `account`: the facility first, then the fallback file
/// at `path`. `None` when neither holds one.
pub fn read(account: &str, path: &Path) -> Result<Option<String>, ConfigError> {
    match keyring::read(account) {
        Ok(Some(password)) => return Ok(Some(password)),
        Ok(None) => {}
        // A broken facility must not hide a perfectly good file, but it is worth
        // saying so rather than silently reading the other store.
        Err(err) => eprintln!("Cannot use the system keyring: {err}"),
    }
    read_file(path)
}

/// Forget `account` everywhere it is kept. Returns whether anything was.
pub fn revoke(account: &str, path: &Path) -> Result<bool, String> {
    let from_keyring = match keyring::delete(account) {
        Ok(found) => found,
        Err(err) => {
            eprintln!("Cannot use the system keyring: {err}");
            false
        }
    };
    let from_file = match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    Ok(from_keyring || from_file)
}

/// Write the fallback file, private from the start.
fn write_file(path: &Path, password: &str) -> Result<(), ConfigError> {
    write(path, password)?;
    // `mode` only applies at creation, so a file that already existed wider than
    // 0600 is tightened here too.
    ensure_private_mode(path)?;
    Ok(())
}

/// Read the fallback file. A missing file is `Ok(None)`; a malformed one is an
/// error, so a caller never mistakes a broken store for "no password".
pub fn read_file(path: &Path) -> Result<Option<String>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(ConfigError::Io(format!("{}: {err}", path.display()))),
    };
    let table: toml::Table = toml::from_str(&text).map_err(|err| ConfigError::Format {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    match table.get("password") {
        Some(Value::String(password)) => Ok(Some(password.clone())),
        Some(_) => Err(format_error(path, "key \"password\" must be a string")),
        None => Err(format_error(path, "key \"password\" is missing")),
    }
}

/// Write `password` to `path`, mode `0600`, replacing anything already there.
///
/// On Windows the file inherits the directory's ACL, which is the portable
/// equivalent of "only this user"; there is no `chmod` to apply.
pub fn write(path: &Path, password: &str) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|err| ConfigError::Io(format!("{}: {err}", parent.display())))?;
        }
    }
    let body = format!("{HEADER}password = {}\n", Value::String(password.to_string()));
    create_private(path, body.as_bytes())?;
    // `mode` only applies at creation, so a file that already existed wider
    // than 0600 is tightened here too.
    ensure_private_mode(path)?;
    Ok(())
}

#[cfg(unix)]
fn create_private(path: &Path, body: &[u8]) -> Result<(), ConfigError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| ConfigError::Io(format!("{}: {err}", path.display())))?;
    file.write_all(body)
        .map_err(|err| ConfigError::Io(format!("{}: {err}", path.display())))
}

#[cfg(not(unix))]
fn create_private(path: &Path, body: &[u8]) -> Result<(), ConfigError> {
    std::fs::write(path, body).map_err(|err| ConfigError::Io(format!("{}: {err}", path.display())))
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

/// Which password to authenticate with: a non-empty [`ENV_PASSWORD`] first,
/// then the stored one. Pure, so the priority is testable without an
/// environment.
pub fn resolve_password(explicit_env: Option<&str>, stored: Option<String>) -> Option<String> {
    match explicit_env {
        Some(value) if !value.is_empty() => Some(value.to_string()),
        _ => stored,
    }
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

    /// A uniquely named scratch directory, removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = format!(
                "srun-portal-credentials-{}-{}-{}-{}",
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

    #[test]
    fn credentials_live_beside_the_configuration() {
        assert_eq!(
            path_for(Path::new("/etc/srun-portal/srun-portal.toml")),
            PathBuf::from("/etc/srun-portal").join(CREDENTIALS_FILE_NAME)
        );
        // A bare file name has no parent to join, so it stays in the cwd.
        assert_eq!(
            path_for(Path::new("srun-portal.toml")),
            PathBuf::from(".").join(CREDENTIALS_FILE_NAME)
        );
    }

    #[test]
    fn a_missing_file_is_no_password() {
        let dir = TempDir::new("missing");
        assert_eq!(read_file(&dir.path().join(CREDENTIALS_FILE_NAME)).unwrap(), None);
    }

    /// The priority between the two stores, and that `revoke` clears both.
    ///
    /// Written to hold on either kind of host: the keyring either answers (and
    /// the assertions cover the keyring branch) or it does not (and the file
    /// branch is what runs).
    #[test]
    fn store_read_and_revoke_use_the_best_available_store() {
        let dir = TempDir::new("stores");
        let path = dir.path().join(CREDENTIALS_FILE_NAME);
        let account = format!("srun-portal-store-test-{}", std::process::id());
        // A leftover from an interrupted run must not decide the outcome.
        let _ = revoke(&account, &path);

        let (chosen, note) = store(&account, "testpass", &path).unwrap();
        match chosen {
            Store::Keyring => assert_eq!(note, None, "the keyring needed no fallback"),
            Store::File => {
                let note = note.expect("a fallback says why");
                assert!(note.contains("unavailable"), "{note}");
                assert!(path.exists(), "the fallback wrote the file");
            }
        }
        assert_eq!(read(&account, &path).unwrap().as_deref(), Some("testpass"));

        // Replacing in place, not appending a second entry.
        let _ = store(&account, "second", &path).unwrap();
        assert_eq!(read(&account, &path).unwrap().as_deref(), Some("second"));

        assert!(revoke(&account, &path).unwrap(), "something was stored");
        assert_eq!(read(&account, &path).unwrap(), None);
        assert!(!path.exists(), "the fallback file is gone too");
        assert!(!revoke(&account, &path).unwrap(), "and nothing is left");
    }

    /// A file left behind while the keyring holds the password is still cleared
    /// by `revoke`: forgetting means forgetting.
    #[test]
    fn revoke_clears_the_file_even_with_a_keyring_entry() {
        let dir = TempDir::new("revoke-both");
        let path = dir.path().join(CREDENTIALS_FILE_NAME);
        write(&path, "left-behind").unwrap();

        let removed = revoke("srun-portal-revoke-test-nobody", &path).unwrap();

        assert!(removed, "the file counted as something stored");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_is_private_and_read_round_trips() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("round-trip");
        let path = dir.path().join(CREDENTIALS_FILE_NAME);
        // A wider mode already on disk is what `ensure_private_mode` is for.
        std::fs::write(&path, "password = \"old\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_file(&path).unwrap().as_deref(), Some("old"));

        write(&path, "testpass").unwrap();
        assert_eq!(read_file(&path).unwrap().as_deref(), Some("testpass"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a fresh write is already private");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# srun-portal credentials; keep this file private.\npassword = \"testpass\"\n"
        );

        // A password needing escapes survives the round trip (the `toml`
        // renderer switches to a literal string instead of escaping).
        let tricky = "p@ss \"quoted\" \\ path";
        write(&path, tricky).unwrap();
        assert_eq!(read_file(&path).unwrap().as_deref(), Some(tricky));

        // A file someone opened up again is tightened, and reported as changed.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(ensure_private_mode(&path).unwrap());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0, "group and other bits are gone");
        assert!(!ensure_private_mode(&path).unwrap(), "0600 needs no change");
    }

    #[test]
    fn a_wrongly_typed_password_is_an_error() {
        let dir = TempDir::new("typed");
        let path = dir.path().join(CREDENTIALS_FILE_NAME);

        std::fs::write(&path, "password = 1\n").unwrap();
        let err = read_file(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("password"), "{message}");

        std::fs::write(&path, "# nothing here\n").unwrap();
        let err = read_file(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Format { .. }));
        assert!(err.to_string().contains("missing"), "{err}");
    }

    #[test]
    fn the_environment_overrides_the_stored_password() {
        assert_eq!(
            resolve_password(Some("from-env"), Some("stored".to_string())),
            Some("from-env".to_string())
        );
        // An explicitly empty variable means "unset", not "empty password".
        assert_eq!(
            resolve_password(Some(""), Some("stored".to_string())),
            Some("stored".to_string())
        );
        assert_eq!(resolve_password(None, Some("stored".to_string())), Some("stored".to_string()));
        assert_eq!(resolve_password(Some("from-env"), None), Some("from-env".to_string()));
        assert_eq!(resolve_password(None, None), None);
    }
}
