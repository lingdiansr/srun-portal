//! The system's own credential facility, so the password does not have to live
//! in a file.
//!
//! Each platform has a service that exists for exactly this, and the unattended
//! commands use it:
//!
//! * Linux — the **Secret Service** D-Bus API (`org.freedesktop.secrets`),
//!   reached through `secret-tool` from libsecret. KWallet, gnome-keyring and
//!   ksecretd all implement that interface, so the wallet the user already has
//!   is the one that gets used.
//! * Windows — the **Credential Manager** (`CredReadW` / `CredWriteW` /
//!   `CredDeleteW`), i.e. the entries shown under "Windows Credentials".
//! * macOS — the **login keychain**, through `security(1)`
//!   (`add-generic-password` / `find-generic-password` / `delete-generic-password`).
//!
//! A facility is not always present (a headless Linux box with no session bus,
//! for instance), so every operation can fail: [`store`] returning an error is
//! the caller's signal that the file fallback is needed, and [`read`] returning
//! `Ok(None)` is a plain "nothing stored". [`availability`] answers the question
//! up front, in the same vocabulary, so a caller can explain the situation
//! before trying.
//!
//! Nothing here is a secret in itself — the entry is keyed by the portal service
//! name and the account, and only the value is sensitive.

/// The service name every entry is filed under.
pub const SERVICE: &str = "srun-portal";

/// Whether this system's credential facility can be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// A facility is present and believed to be working.
    Ready,
    /// There is none, or it cannot be reached; the reason is for the user.
    Unavailable(String),
}

impl Availability {
    pub fn is_ready(&self) -> bool {
        matches!(self, Availability::Ready)
    }
}

/// Whether a usable facility exists here.
pub fn availability() -> Availability {
    imp::availability()
}

/// The stored password for `account`: `Ok(None)` when nothing is stored.
pub fn read(account: &str) -> Result<Option<String>, String> {
    imp::read(account)
}

/// Replace the stored password for `account`.
pub fn store(account: &str, password: &str) -> Result<(), String> {
    imp::store(account, password)
}

/// Delete the stored password for `account`; `Ok(false)` when there was none.
pub fn delete(account: &str) -> Result<bool, String> {
    imp::delete(account)
}

/// The Secret Service, through `secret-tool`.
///
/// `secret-tool` speaks the same D-Bus interface every Linux wallet implements,
/// which is why this asks for no wallet-specific code. Its exit status is the
/// only channel for "no such item" versus "no bus at all" — both are `1` with no
/// output — so [`availability`] probes the bus separately instead of guessing
/// from an operation's failure.
#[cfg(target_os = "linux")]
mod imp {
    use super::{Availability, SERVICE};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    /// The libsecret CLI.
    const TOOL: &str = "secret-tool";

    /// The Secret Service name on the session bus.
    const SECRET_SERVICE: &str = "org.freedesktop.secrets";

    pub fn availability() -> Availability {
        if tool_on_path(TOOL).is_none() {
            return Availability::Unavailable(format!("{TOOL} (libsecret) is not installed"));
        }
        match secret_service_owner() {
            Some(true) => Availability::Ready,
            Some(false) => Availability::Unavailable(format!(
                "nothing on the session bus offers {SECRET_SERVICE} \
                 (start gnome-keyring, KWallet or another wallet)"
            )),
            // No way to ask: let the operation itself report the truth.
            None => Availability::Ready,
        }
    }

    pub fn read(account: &str) -> Result<Option<String>, String> {
        let argv = lookup_argv(account);
        let output = run_captured(&argv, None)?;
        match output.status.code() {
            Some(0) => {
                let text = String::from_utf8_lossy(&output.stdout);
                // `secret-tool` terminates the secret with one newline.
                Ok(Some(text.strip_suffix('\n').unwrap_or(&text).to_string()))
            }
            Some(1) => Ok(None),
            _ => Err(format!("{} failed: {}", argv.join(" "), detail(&output))),
        }
    }

    pub fn store(account: &str, password: &str) -> Result<(), String> {
        let argv = store_argv(account);
        let output = run_captured(&argv, Some(password.as_bytes()))?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!("{} failed: {}", argv.join(" "), detail(&output)))
    }

    pub fn delete(account: &str) -> Result<bool, String> {
        let argv = delete_argv(account);
        let output = run_captured(&argv, None)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(format!("{} failed: {}", argv.join(" "), detail(&output))),
        }
    }

    /// `secret-tool lookup …`: one line of stdout is the secret.
    pub(super) fn lookup_argv(account: &str) -> Vec<String> {
        argv(&["lookup", "service", SERVICE, "account", account])
    }

    /// `secret-tool store … --label …`: the secret arrives on stdin.
    pub(super) fn store_argv(account: &str) -> Vec<String> {
        let label = format!("{SERVICE}: {account}");
        argv(&[
            "store", "--label", &label, "service", SERVICE, "account", account,
        ])
    }

    pub(super) fn delete_argv(account: &str) -> Vec<String> {
        argv(&["clear", "service", SERVICE, "account", account])
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        std::iter::once(TOOL.to_string())
            .chain(parts.iter().map(|part| (*part).to_string()))
            .collect()
    }

    /// Run the tool, optionally feeding it `stdin`.
    fn run_captured(argv: &[String], stdin: Option<&[u8]>) -> Result<std::process::Output, String> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("{} failed: {err}", argv.join(" ")))?;
        if let Some(bytes) = stdin {
            let mut handle = child.stdin.take().ok_or_else(|| {
                format!("{}: stdin was not available", argv.join(" "))
            })?;
            handle
                .write_all(bytes)
                .map_err(|err| format!("{} failed: {err}", argv.join(" ")))?;
            // Dropping the handle closes the pipe, which is what `secret-tool`
            // reads until.
        }
        child
            .wait_with_output()
            .map_err(|err| format!("{} failed: {err}", argv.join(" ")))
    }

    /// The stderr of a failed run, or the exit status when it said nothing.
    fn detail(output: &std::process::Output) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr.to_string()
        }
    }

    /// `secret-tool` on `PATH`, if it is there.
    fn tool_on_path(tool: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(tool))
            .find(|candidate| is_executable(candidate))
    }

    fn is_executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;

        match std::fs::metadata(path) {
            Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }

    /// Whether the session bus has an owner for the Secret Service.
    ///
    /// `None` means no tool was available to ask. `gdbus` and `dbus-send` are
    /// asked in turn; both are plain D-Bus clients with no keyring behind them,
    /// so neither can itself be the thing that is missing.
    fn secret_service_owner() -> Option<bool> {
        let probes: [Vec<String>; 2] = [
            argv_from(&[
                "gdbus",
                "call",
                "--session",
                "--dest",
                "org.freedesktop.DBus",
                "--object-path",
                "/org/freedesktop/DBus",
                "--method",
                "org.freedesktop.DBus.NameHasOwner",
                SECRET_SERVICE,
            ]),
            argv_from(&[
                "dbus-send",
                "--session",
                "--print-reply",
                "--dest=org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus.NameHasOwner",
                &format!("string:{SECRET_SERVICE}"),
            ]),
        ];
        for probe in probes {
            let Ok(output) = Command::new(&probe[0]).args(&probe[1..]).output() else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let text = String::from_utf8_lossy(&output.stdout);
            return Some(text.contains("true"));
        }
        None
    }

    fn argv_from(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }
}

/// The Credential Manager, through `advapi32`.
///
/// The password is written as UTF-16LE without a terminator, the shape Windows
/// itself uses for a generic credential's blob, so the entry is interoperable
/// with the credential UI and `cmdkey` rather than only with this crate.
#[cfg(windows)]
mod imp {
    use super::{Availability, SERVICE};
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    pub fn availability() -> Availability {
        // The Credential Manager ships with Windows and needs no session bus.
        Availability::Ready
    }

    pub fn read(account: &str) -> Result<Option<String>, String> {
        let target = encode(&target_name(account));
        let mut credential: *mut CREDENTIALW = std::ptr::null_mut();
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if ok == 0 {
            return match unsafe { GetLastError() } {
                ERROR_NOT_FOUND => Ok(None),
                code => Err(format!("CredReadW({account}) failed with error {code}")),
            };
        }
        let blob = unsafe {
            let credential = &*credential;
            std::slice::from_raw_parts(
                credential.CredentialBlob,
                credential.CredentialBlobSize as usize,
            )
            .to_vec()
        };
        unsafe { CredFree(credential as *const core::ffi::c_void) };
        let units: Vec<u16> = blob
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units)
            .map(Some)
            .map_err(|err| format!("the stored credential for {account} is not valid UTF-16: {err}"))
    }

    pub fn store(account: &str, password: &str) -> Result<(), String> {
        let mut target = encode(&target_name(account));
        let mut username = encode(account);
        let mut comment = encode(&format!("{SERVICE} unattended reconnect password"));
        // UTF-16LE bytes, no terminator: the blob Windows itself writes.
        let blob: Vec<u8> = password
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            Comment: comment.as_mut_ptr(),
            LastWritten: Default::default(),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_ptr() as *mut u8,
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: std::ptr::null_mut(),
            UserName: username.as_mut_ptr(),
        };
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            let code = unsafe { GetLastError() };
            return Err(format!("CredWriteW({account}) failed with error {code}"));
        }
        Ok(())
    }

    pub fn delete(account: &str) -> Result<bool, String> {
        let target = encode(&target_name(account));
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
            return Ok(true);
        }
        match unsafe { GetLastError() } {
            ERROR_NOT_FOUND => Ok(false),
            code => Err(format!("CredDeleteW({account}) failed with error {code}")),
        }
    }

    /// One credential per account, under a name only this service uses.
    pub(super) fn target_name(account: &str) -> String {
        format!("{SERVICE}/{account}")
    }

    /// A Rust string as the NUL-terminated wide string the Win32 API wants.
    fn encode(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

/// The login keychain, through `security(1)`.
#[cfg(target_os = "macos")]
mod imp {
    use super::{Availability, SERVICE};
    use std::process::{Command, Stdio};

    const TOOL: &str = "security";

    pub fn availability() -> Availability {
        // Login keychains are created by the system at first login, so the tool
        // existing is the whole question.
        Availability::Ready
    }

    pub fn read(account: &str) -> Result<Option<String>, String> {
        let argv = lookup_argv(account);
        let output = run(&argv)?;
        match output.status.code() {
            Some(0) => {
                let text = String::from_utf8_lossy(&output.stdout);
                Ok(Some(text.strip_suffix('\n').unwrap_or(&text).to_string()))
            }
            // `security` reports a missing item as a non-zero exit with a
            // "could not be found" message.
            _ if missing(&output) => Ok(None),
            _ => Err(format!("{} failed: {}", argv.join(" "), detail(&output))),
        }
    }

    pub fn store(account: &str, password: &str) -> Result<(), String> {
        // `-w` takes the password from the argument here, so it goes in the
        // process table for the moment the command runs; `security` has no
        // stdin form for a generic password.
        let argv = store_argv(account, password);
        let output = run(&argv)?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "security add-generic-password failed: {}",
            detail(&output)
        ))
    }

    pub fn delete(account: &str) -> Result<bool, String> {
        let argv = delete_argv(account);
        let output = run(&argv)?;
        if output.status.success() {
            return Ok(true);
        }
        if missing(&output) {
            return Ok(false);
        }
        Err(format!("{} failed: {}", argv.join(" "), detail(&output)))
    }

    fn lookup_argv(account: &str) -> Vec<String> {
        argv(&["find-generic-password", "-s", SERVICE, "-a", account, "-w"])
    }

    fn store_argv(account: &str, password: &str) -> Vec<String> {
        argv(&[
            "add-generic-password",
            "-U",
            "-s",
            SERVICE,
            "-a",
            account,
            "-w",
            password,
        ])
    }

    fn delete_argv(account: &str) -> Vec<String> {
        argv(&["delete-generic-password", "-s", SERVICE, "-a", account])
    }

    fn argv(parts: &[&str]) -> Vec<String> {
        std::iter::once(TOOL.to_string())
            .chain(parts.iter().map(|part| (*part).to_string()))
            .collect()
    }

    fn run(argv: &[String]) -> Result<std::process::Output, String> {
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("{} failed: {err}", argv[0]))?;
        child
            .wait_with_output()
            .map_err(|err| format!("{} failed: {err}", argv[0]))
    }

    /// `security` has no distinct exit code for "not found": it says so.
    fn missing(output: &std::process::Output) -> bool {
        let stderr = String::from_utf8_lossy(&output.stderr);
        stderr.contains("could not be found") || stderr.contains("SecKeychainSearchCopyNext")
    }

    fn detail(output: &std::process::Output) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr.to_string()
        }
    }
}

/// Targets with none of the three: there is nothing to fall back to but the
/// file, which the caller already has.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod imp {
    use super::Availability;

    pub fn availability() -> Availability {
        Availability::Unavailable("this system has no known credential facility".to_string())
    }

    pub fn read(_account: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    pub fn store(_account: &str, _password: &str) -> Result<(), String> {
        Err("this system has no known credential facility".to_string())
    }

    pub fn delete(_account: &str) -> Result<bool, String> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_name_is_the_crate_name() {
        assert_eq!(SERVICE, "srun-portal");
    }

    /// The argv each backend builds, checked where the tool is known.
    #[cfg(target_os = "linux")]
    #[test]
    fn secret_tool_argv_is_exact() {
        use imp::{delete_argv, lookup_argv, store_argv};

        assert_eq!(
            lookup_argv("testuser"),
            vec!["secret-tool", "lookup", "service", "srun-portal", "account", "testuser"]
        );
        assert_eq!(
            store_argv("testuser"),
            vec![
                "secret-tool",
                "store",
                "--label",
                "srun-portal: testuser",
                "service",
                "srun-portal",
                "account",
                "testuser",
            ]
        );
        assert_eq!(
            delete_argv("testuser"),
            vec!["secret-tool", "clear", "service", "srun-portal", "account", "testuser"]
        );
    }

    /// The real facility, when the machine running the tests has one. The entry
    /// is written under a name only this test uses and removed again.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_wallet_round_trips() {
        if !availability().is_ready() {
            return; // headless: nothing to exercise, the file fallback covers it
        }
        let account = format!("srun-portal-test-{}", std::process::id());
        // Clean any leftover from an interrupted earlier run.
        let _ = delete(&account);
        assert_eq!(read(&account).unwrap(), None, "nothing stored yet");

        store(&account, "p@ss word \"quoted\"").unwrap();
        assert_eq!(read(&account).unwrap().as_deref(), Some("p@ss word \"quoted\""));

        // A second store replaces rather than duplicating.
        store(&account, "second").unwrap();
        assert_eq!(read(&account).unwrap().as_deref(), Some("second"));

        assert!(delete(&account).unwrap(), "the entry was there");
        assert_eq!(read(&account).unwrap(), None);
        assert!(!delete(&account).unwrap(), "and is gone now");
    }

    #[test]
    fn availability_explains_itself() {
        match availability() {
            Availability::Ready => {}
            Availability::Unavailable(reason) => {
                assert!(!reason.is_empty(), "an unavailable facility has a reason");
            }
        }
    }
}
