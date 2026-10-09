//! The non-interactive half of the SPEC §5 flow: one "check, log in when
//! needed" pass, with no banner and no prompts.
//!
//! This is what the installed background task runs (see [`crate::service`]): the
//! process starts, resolves the portal configuration, asks the portal whether
//! this device is already online, and authenticates only when it is not. Every
//! request it emits is the one the interactive flow emits at the same point —
//! the two share [`Runtime`], so the wire contract is the same by construction.
//!
//! The pieces the interactive shell also needs at the same points
//! ([`fetch_config`], [`host_of`]) live here and are used from
//! [`crate::cli`].

use crate::api;
use crate::config;
use crate::portal_config::{self, ConfigError};
use crate::runtime::{PortalError, Runtime, RuntimeOptions};
use crate::sys;
use crate::transport;

/// Everything one unattended pass needs: which portal to talk to and which
/// account to use. Plain data, so the pass itself is testable without a
/// configuration file.
#[derive(Debug, Clone)]
pub struct Target {
    pub portal: config::PortalUrl,
    pub username: String,
    /// As `authByPassword` wants it: empty, or `@domain`.
    pub domain: String,
    /// Legacy configuration compatibility; new files always use `jsonp`.
    pub callback: String,
}

/// What one pass changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The portal already reported this device online; nothing was sent beyond
    /// the probes the check itself makes.
    AlreadyOnline,
    /// The device was offline and the login succeeded.
    Reconnected,
}

/// Check the connection once and log in when the portal says this device is
/// offline.
///
/// `Err` carries `err.render(&runtime.translate)` — the text the CLI prints —
/// so the caller never formats a [`PortalError`] itself and no error message is
/// invented here.
pub fn reconnect_once(target: &Target, password: &str) -> Result<Outcome, String> {
    let cfg = fetch_config(&target.portal)?;
    let mut options =
        RuntimeOptions::from_config(&cfg, &target.portal.origin, &host_of(&target.portal.origin));
    options.callback = target.callback.clone();
    let mut runtime = Runtime::new(options);
    runtime.apply_interface_correction(&sys::network_interfaces(), &cfg.ip);
    runtime.spawn_other_stack_probe();

    if runtime
        .check_online()
        .map_err(|err| err.render(&runtime.translate))?
    {
        return Ok(Outcome::AlreadyOnline);
    }
    runtime
        .auth_by_password(&target.username, password, &target.domain)
        .map_err(|err| err.render(&runtime.translate))?;
    Ok(Outcome::Reconnected)
}

/// SPEC §3.2/§5: `GET {origin}/srun_portal_pc?ac_id=<url ac_id>&theme=app`, then
/// read the embedded configuration out of the page.
pub fn fetch_config(
    portal: &config::PortalUrl,
) -> Result<portal_config::PortalConfig, PortalError> {
    let url = format!(
        "{}{}?ac_id={}&theme=app",
        portal.origin,
        api::CONFIG_PATHNAME,
        transport::urlencode(&portal.ac_id)
    );
    let html =
        transport::get_text(&url).map_err(|err| PortalError::ConfigFetch(err.to_string()))?;
    portal_config::parse(&html).map_err(|err| match err {
        ConfigError::CliVersionTooLow => PortalError::CliVersionTooLow,
        other => PortalError::ConfigFetch(other.to_string()),
    })
}

/// The `host` of an origin string, port included (`url.host` in the reference).
pub fn host_of(origin: &str) -> String {
    origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin)
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_includes_port_but_not_path() {
        assert_eq!(
            host_of("https://example.test:8443/path"),
            "example.test:8443"
        );
        assert_eq!(host_of("example.test"), "example.test");
    }
}
