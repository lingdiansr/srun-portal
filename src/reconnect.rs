//! The non-interactive half of the SPEC §5 flow: one "check, log in when
//! needed" pass, with no banner and no prompts.
//!
//! This is what the installed background task runs (see [`crate::service`]): the
//! process starts, resolves the portal configuration, asks the portal whether
//! this device is already online, and authenticates only when it is not. Every
//! request it emits is the one the interactive flow emits at the same point —
//! the two share [`Runtime`], so the wire contract is the same by construction.
//!
//! The pass reports a [`ReconnectError`] carrying an [`ErrorKind`], so the caller
//! can classify a failure without parsing a message. The classification is
//! lossless where it matters: a transport failure is the typed
//! [`crate::transport::TransportError`] rather than its rendering, and a session
//! rejected by the portal is `Authentication` rather than a string the caller
//! would have to match on.
//!
//! The pieces the interactive shell also needs at the same points
//! ([`fetch_config`], [`host_of`]) live here and are used from [`crate::cli`].

use serde::{Deserialize, Serialize};

use crate::api;
use crate::config;
use crate::control::{ErrorKind, Outcome};
use crate::portal_config;
use crate::runtime::{PortalError, Runtime, RuntimeOptions};
use crate::transport::{self, TransportError};

/// Everything one unattended pass needs: which portal to talk to and which
/// account to use. Plain data, so the pass itself is testable without a
/// configuration file.
#[derive(Debug, Clone)]
pub struct Target {
    pub portal: config::PortalUrl,
    pub username: String,
    pub domain: String,
    pub callback: String,
}

/// A failed pass, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectError {
    pub kind: ErrorKind,
    pub message: String,
}

impl ReconnectError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> ReconnectError {
        ReconnectError {
            kind,
            message: message.into(),
        }
    }

    /// Classify a runtime error. The typed transport variants are what make this
    /// more than a guess.
    pub fn from_portal(err: &PortalError) -> ReconnectError {
        let kind = match err {
            PortalError::Transport(TransportError::Http(_))
            | PortalError::ConfigFetch(TransportError::Http(_)) => ErrorKind::Network,
            PortalError::Transport(_) | PortalError::ConfigFetch(_) => ErrorKind::Protocol,
            PortalError::ConfigParse(_)
            | PortalError::MissingField { .. }
            | PortalError::CliVersionTooLow => ErrorKind::Protocol,
            PortalError::Api { .. } | PortalError::LoginFailed | PortalError::LogoutFailed => {
                ErrorKind::Authentication
            }
            PortalError::Timeout => ErrorKind::Timeout,
            PortalError::DomainNeedsAt => ErrorKind::Config,
        };
        ReconnectError {
            kind,
            message: err.to_string(),
        }
    }
}

impl std::fmt::Display for ReconnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<ReconnectError> for OutcomeReport {
    fn from(err: ReconnectError) -> Self {
        OutcomeReport::failed(err.kind, err.message)
    }
}

/// The result of one attempt, in the shape the health record and `--json` use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeReport {
    pub result: Outcome,
    pub error_kind: Option<ErrorKind>,
    pub message: String,
}

impl OutcomeReport {
    pub fn failed(kind: ErrorKind, message: impl Into<String>) -> OutcomeReport {
        OutcomeReport {
            result: Outcome::Failed,
            error_kind: Some(kind),
            message: message.into(),
        }
    }

    pub fn paused() -> OutcomeReport {
        OutcomeReport {
            result: Outcome::Paused,
            error_kind: None,
            message: "background reconnect is paused; run `srun-portal resume`".to_string(),
        }
    }

    pub fn busy() -> OutcomeReport {
        OutcomeReport {
            result: Outcome::Busy,
            error_kind: None,
            message: "another srun-portal process is running".to_string(),
        }
    }

    /// Whether this attempt reached the portal successfully.
    pub fn is_network_health(&self) -> bool {
        matches!(
            self.result,
            Outcome::Online | Outcome::Reconnected | Outcome::Disconnected
        )
    }

    /// The exit status this result maps to.
    pub fn exit_code(&self) -> i32 {
        match self.result {
            Outcome::Online
            | Outcome::Reconnected
            | Outcome::Disconnected
            | Outcome::Paused
            | Outcome::Busy => 0,
            Outcome::Failed => match self.error_kind {
                Some(ErrorKind::Config) => 2,
                Some(ErrorKind::Credentials)
                | Some(ErrorKind::Platform)
                | Some(ErrorKind::Protocol) => 3,
                _ => 4,
            },
        }
    }
}

impl From<Outcome> for OutcomeReport {
    fn from(outcome: Outcome) -> Self {
        OutcomeReport {
            result: outcome,
            error_kind: None,
            message: match outcome {
                Outcome::Online => "already online".to_string(),
                Outcome::Reconnected => "reconnected".to_string(),
                Outcome::Disconnected => "disconnected".to_string(),
                // A pass never produces a control result.
                Outcome::Paused | Outcome::Busy | Outcome::Failed => "failed".to_string(),
            },
        }
    }
}

impl ReconnectError {
    /// The report of a failed pass.
    pub fn into_report(self) -> OutcomeReport {
        OutcomeReport::failed(self.kind, self.message)
    }
}

/// Check the connection once and log in when the portal says this device is
/// offline.
pub fn reconnect_once(target: &Target, password: &str) -> Result<Outcome, ReconnectError> {
    run(target, password, Action::Reconnect)
}

/// Sign the device out of the portal.
pub fn disconnect_once(target: &Target, password: &str) -> Result<Outcome, ReconnectError> {
    run(target, password, Action::Disconnect)
}

/// Which half of the flow to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Reconnect,
    Disconnect,
}

fn run(target: &Target, password: &str, action: Action) -> Result<Outcome, ReconnectError> {
    let cfg = fetch_config(&target.portal).map_err(|err| {
        let mut classified = ReconnectError::from_portal(&err);
        classified.message = err.render(&crate::translate::Translate::default());
        classified
    })?;
    let mut options =
        RuntimeOptions::from_config(&cfg, &target.portal.origin, &host_of(&target.portal.origin));
    options.callback = target.callback.clone();
    let mut runtime = Runtime::new(options);
    // The probe is fire-and-forget in the reference; the background pass keeps
    // that behaviour (SPEC §12 item 10) but has no interface correction to make,
    // because the pass runs long after the interfaces settled.
    runtime.spawn_other_stack_probe();

    let online = runtime
        .check_online()
        .map_err(|err| portal_error(&runtime, err))?;
    match action {
        Action::Reconnect => {
            if online {
                return Ok(Outcome::Online);
            }
            runtime
                .auth_by_password(&target.username, password, &target.domain)
                .map_err(|err| portal_error(&runtime, err))?;
            Ok(Outcome::Reconnected)
        }
        Action::Disconnect => {
            if !online {
                // Asking the portal to unbind a session it does not have is not
                // an error: the device is already in the state asked for.
                return Ok(Outcome::Disconnected);
            }
            runtime
                .sign_out()
                .map_err(|err| portal_error(&runtime, err))?;
            Ok(Outcome::Disconnected)
        }
    }
}

fn portal_error(runtime: &Runtime, err: PortalError) -> ReconnectError {
    let mut classified = ReconnectError::from_portal(&err);
    classified.message = err.render(&runtime.translate);
    classified
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
        crate::transport::urlencode(&portal.ac_id)
    );
    let html = transport::get_text(&url).map_err(PortalError::ConfigFetch)?;
    match portal_config::parse(&html) {
        Ok(cfg) => Ok(cfg),
        Err(err) => match err {
            portal_config::ConfigError::CliVersionTooLow => Err(PortalError::CliVersionTooLow),
            other => Err(PortalError::ConfigParse(other.to_string())),
        },
    }
}

/// The `host` of an origin string, port included (`url.host` in the reference).
pub fn host_of(origin: &str) -> String {
    origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(origin)
        .split('/')
        .next()
        .unwrap_or(origin)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transport_failure_is_network_and_a_bad_payload_is_protocol() {
        let http = ReconnectError::from_portal(&PortalError::Transport(TransportError::Http(
            "connection refused".to_string(),
        )));
        assert_eq!(http.kind, ErrorKind::Network);

        let not_jsonp = ReconnectError::from_portal(&PortalError::Transport(
            TransportError::NotJsonp("<html>".to_string()),
        ));
        assert_eq!(not_jsonp.kind, ErrorKind::Protocol);

        let parse = ReconnectError::from_portal(&PortalError::ConfigParse("no acid".to_string()));
        assert_eq!(parse.kind, ErrorKind::Protocol);

        let rejected = ReconnectError::from_portal(&PortalError::LoginFailed);
        assert_eq!(rejected.kind, ErrorKind::Authentication);

        let timed_out = ReconnectError::from_portal(&PortalError::Timeout);
        assert_eq!(timed_out.kind, ErrorKind::Timeout);
    }

    #[test]
    fn exit_codes_follow_the_error_class() {
        assert_eq!(OutcomeReport::from(Outcome::Online).exit_code(), 0);
        assert_eq!(OutcomeReport::paused().exit_code(), 0);
        assert_eq!(OutcomeReport::busy().exit_code(), 0);
        assert_eq!(OutcomeReport::failed(ErrorKind::Config, "x").exit_code(), 2);
        assert_eq!(
            OutcomeReport::failed(ErrorKind::Credentials, "x").exit_code(),
            3
        );
        for kind in [
            ErrorKind::Network,
            ErrorKind::Authentication,
            ErrorKind::Timeout,
        ] {
            assert_eq!(OutcomeReport::failed(kind, "x").exit_code(), 4);
        }
    }

    #[test]
    fn the_report_serializes_its_class() {
        let report = OutcomeReport::failed(ErrorKind::Timeout, "too slow");
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"result\":\"failed\""), "{json}");
        assert!(json.contains("\"error_kind\":\"timeout\""), "{json}");
        let back: OutcomeReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
    }
}
