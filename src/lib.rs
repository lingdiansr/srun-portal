//! Clean-room Rust implementation of the Srun portal client protocol.
//!
//! Behaviour contract: `portal-core@1.9.15` (the `portal` binary distributed by
//! the SZU information centre), as specified by a specification reverse
//! engineered from that binary (not distributed with this crate). The goal is
//! request equivalence: for the same inputs this client must emit byte-identical
//! query strings.
//!
//! Layer map (SPEC section in brackets):
//!
//! | module | responsibility |
//! |---|---|
//! | [`crypto`] | variant base64, XXTEA (`XEncode`), HMAC-MD5, SHA-1, `info` payload [§6] |
//! | [`transport`] | `URLSearchParams` encoding, JSONP transport, endpoint URLs [§4] |
//! | [`html`] | the `$('#id').html()` subset needed to read the portal page [§3.2] |
//! | [`portal_config`] | `PortalConfig` / `PortalFlags` decoded from that page [§3.2] |
//! | [`api`] | request construction for every endpoint [§7], [§8] |
//! | [`translate`] | error-code to message dispatch [§10] |
//! | [`config`] | `~/.srun_portal.json` (legacy, read-only) and portal-URL resolution [§3.1] |
//! | [`settings`] | configuration file discovery, layering and first-run creation [§3.1] |
//! | [`credentials`] | the `0600` password-file fallback |
//! | [`keyring`] | the system credential facility (Secret Service, Credential Manager, keychain) |
//! | [`sys`] | passwd entry, network interfaces, device identity, signal handlers |
//! | [`util`] | `wait`, `promiseAny` |
//! | [`format`] | `formatFlow` / `formatTime` |
//! | [`runtime`] | the authentication state machine [§5], [§8], [§9] |
//! | [`reconnect`] | one non-interactive "check, log in when needed" pass [§5], [§8] |
//! | [`service`] | the per-system background task definition (systemd, launchd, Task Scheduler) |
//! | [`cli`] | interactive shell: banner, prompts, retry loop [§5], [§10] |
//!
//! The crate also has an unattended half: [`reconnect`] performs SPEC §5's check
//! and login once, [`credentials`] keeps the password it needs in the system's
//! own credential facility ([`keyring`]: Secret Service, Credential Manager or
//! keychain) or, where none is usable, a `0600` file beside the configuration,
//! and [`service`] registers that one-shot command with systemd, launchd or the
//! Windows Task Scheduler. Nothing there changes the protocol behaviour above —
//! it drives the same [`runtime`] with the same requests.
//!
//! Fidelity policy: the HTTP contract is reproduced exactly. Where the spec
//! records a defect or asks for an explicit decision (SPEC §12), the choice is
//! documented in `README.md` and in the doc comment of the code that implements
//! it.

pub mod api;
pub mod cli;
pub mod config;
pub mod credentials;
pub mod crypto;
pub mod format;
pub mod html;
pub mod keyring;
pub mod portal_config;
pub mod reconnect;
pub mod runtime;
pub mod service;
pub mod settings;
pub mod sys;
pub mod translate;
pub mod transport;
pub mod util;

/// Error code strings the CLI layer itself raises (SPEC §10).
pub mod messages {
    pub const PORTAL_URL_REQUIRED: &str = "Portal web url is required!";
    pub const PORTAL_URL_INVALID: &str = "Portal web url is invalid!";
    pub const CLI_VERSION_TOO_LOW: &str = "Portal CLI version is too low !!!";
    pub const LOGIN_FAILED: &str = "Login failed";
    pub const LOGOUT_FAILED: &str = "Logout failed";
    pub const DOMAIN_NEEDS_AT: &str = "Domain must start with @";
    pub const GET_NOTICE_FAILED: &str = "Get notice failed";
    pub const GET_PROTOCOL_FAILED: &str = "Get protocol failed";
    pub const EXIT_MESSAGE: &str = "\n\nPortal exit!\n";
    pub const BANNER: &str = " Srun Network Auth ";
    /// Raised when a command that cannot ask for one finds no account.
    pub const USERNAME_REQUIRED: &str = "Username is required for an unattended reconnect!";
    /// Raised when the unattended path has no password from any source.
    pub const PASSWORD_REQUIRED: &str =
        "No stored password: run `srun-portal service install` or set SRUN_PORTAL_PASSWORD";
}
