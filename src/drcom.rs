//! Dr.COM 4.0 EPortal client used by the dormitory network.
//!
//! The dormitory portal is not Srun-compatible: it exposes a root page with
//! terminal information and a JSONP EPortal API on port 801. This module keeps
//! that wire protocol separate from the Srun runtime.

use std::fmt;
use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::DrcomUrl;
use crate::{crypto, sys, transport};

const DEFAULT_EPORTAL_PORT: u16 = 801;
const JS_VERSION: &str = "4.1.3";
const LANGUAGE: &str = "zh";
const DEFAULT_MAC: &str = "000000000000";

/// A failure while talking to or decoding the Dr.COM portal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrcomError(String);

impl DrcomError {
    fn message(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for DrcomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DrcomError {}

impl From<transport::TransportError> for DrcomError {
    fn from(error: transport::TransportError) -> Self {
        Self::message(error.to_string())
    }
}

#[derive(Debug, Clone)]
struct Terminal {
    ipv4: String,
    ipv6: String,
    mac: String,
    vlan: String,
    ac_ip: String,
    ac_name: String,
    eportal_port: u16,
}

#[derive(Debug, Clone)]
struct PortalSettings {
    login_method: String,
    account_prefix: bool,
    account_suffix: String,
    check_online_method: u8,
    vlan: String,
    ac_logout: String,
    register_mode: String,
    eportal_port: u16,
}

/// A Dr.COM session context. Constructing it fetches the root page and the
/// EPortal page configuration, but does not authenticate or sign out.
#[derive(Debug, Clone)]
pub struct DrcomClient {
    origin: String,
    eportal_origin: String,
    terminal: Terminal,
    settings: PortalSettings,
}

impl DrcomClient {
    /// Load the terminal and portal settings from a Dr.COM root URL.
    pub fn new(portal: &DrcomUrl) -> Result<Self, DrcomError> {
        let origin = portal.origin.trim_end_matches('/').to_string();
        let html = transport::get_text(&format!("{origin}/"))?;
        let terminal = Terminal::from_page(&html)?;
        let initial_eportal_origin = endpoint_origin(&origin, terminal.eportal_port)?;
        let settings = load_settings(&initial_eportal_origin, &terminal)?;
        let eportal_origin = endpoint_origin(&origin, settings.eportal_port)?;
        Ok(Self {
            origin,
            eportal_origin,
            terminal,
            settings,
        })
    }

    /// Return whether the portal reports this terminal online.
    pub fn check_online(&self) -> Result<bool, DrcomError> {
        let value = if self.settings.check_online_method == 1 {
            let ip = ipv4_to_integer(&self.terminal.ipv4)?;
            jsonp_get(
                &format!("{}/eportal/portal/online_list", self.eportal_origin),
                vec![
                    ("user_account", String::new()),
                    ("user_password", String::new()),
                    ("wlan_user_mac", self.terminal.mac.to_uppercase()),
                    ("wlan_user_ip", ip.to_string()),
                    ("curr_user_ip", ip.to_string()),
                ],
            )?
        } else {
            jsonp_get(&format!("{}/drcom/chkstatus", self.origin), Vec::new())?
        };
        Ok(result_is_success(&value))
    }

    /// Authenticate with a Dr.COM account. `domain` is the optional suffix
    /// already normalized by the CLI (`""` or `"@name"`).
    pub fn login(&self, username: &str, password: &str, domain: &str) -> Result<(), DrcomError> {
        if self.settings.login_method != "1" && self.settings.login_method != "9" {
            return Err(DrcomError::message(format!(
                "unsupported Dr.COM login method: {}",
                self.settings.login_method
            )));
        }
        let account = format!("{username}{domain}{}", self.settings.account_suffix);
        let account = if self.settings.account_prefix {
            format!(",0,{account}")
        } else {
            account
        };
        let value = jsonp_get(
            &format!("{}/eportal/portal/login", self.eportal_origin),
            vec![
                ("login_method", self.settings.login_method.clone()),
                ("user_account", account),
                ("user_password", password.to_string()),
                ("wlan_user_ip", self.terminal.ipv4.clone()),
                ("wlan_user_ipv6", self.terminal.ipv6.clone()),
                ("wlan_user_mac", self.terminal.mac.clone()),
                ("wlan_ac_ip", self.terminal.ac_ip.clone()),
                ("wlan_ac_name", self.terminal.ac_name.clone()),
                ("terminal_type", "1".to_string()),
            ],
        )?;
        ensure_success("login", &value)
    }

    /// Sign out the current Dr.COM terminal.
    pub fn logout(&self) -> Result<(), DrcomError> {
        let value = jsonp_get(
            &format!("{}/eportal/portal/logout", self.eportal_origin),
            vec![
                ("login_method", self.settings.login_method.clone()),
                ("user_account", "drcom".to_string()),
                ("user_password", "123".to_string()),
                ("ac_logout", self.settings.ac_logout.clone()),
                ("register_mode", self.settings.register_mode.clone()),
                ("wlan_user_ip", self.terminal.ipv4.clone()),
                ("wlan_user_ipv6", self.terminal.ipv6.clone()),
                ("wlan_vlan_id", self.settings.vlan.clone()),
                ("wlan_user_mac", self.terminal.mac.clone()),
                ("wlan_ac_ip", self.terminal.ac_ip.clone()),
                ("wlan_ac_name", self.terminal.ac_name.clone()),
                ("terminal_type", "1".to_string()),
            ],
        )?;
        ensure_success("logout", &value)
    }
}

impl Terminal {
    fn from_page(html: &str) -> Result<Self, DrcomError> {
        let ipv4 = assignment(html, "v4ip").unwrap_or_default();
        let ipv4 = if ipv4.is_empty() || ipv4 == "000.000.000.000" {
            local_ipv4().ok_or_else(|| {
                DrcomError::message("Dr.COM did not report a terminal IPv4 address")
            })?
        } else {
            ipv4
        };
        let eportal_port = assignment(html, "authloginport")
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_EPORTAL_PORT);
        Ok(Self {
            ipv4,
            ipv6: assignment(html, "v6ip").unwrap_or_default(),
            mac: DEFAULT_MAC.to_string(),
            vlan: "1".to_string(),
            ac_ip: String::new(),
            ac_name: String::new(),
            eportal_port,
        })
    }
}

fn load_settings(eportal_origin: &str, terminal: &Terminal) -> Result<PortalSettings, DrcomError> {
    let value = jsonp_get(
        &format!("{eportal_origin}/eportal/portal/page/loadConfig"),
        vec![
            ("program_index", String::new()),
            ("wlan_vlan_id", terminal.vlan.clone()),
            (
                "wlan_user_ip",
                crypto::btoa(terminal.ipv4.as_bytes(), crypto::STANDARD_ALPHABET),
            ),
            (
                "wlan_user_ipv6",
                crypto::btoa(terminal.ipv6.as_bytes(), crypto::STANDARD_ALPHABET),
            ),
            ("wlan_user_ssid", String::new()),
            ("wlan_user_areaid", String::new()),
            (
                "wlan_ac_ip",
                crypto::btoa(terminal.ac_ip.as_bytes(), crypto::STANDARD_ALPHABET),
            ),
            ("wlan_ap_mac", DEFAULT_MAC.to_string()),
            ("gw_id", "000000000000".to_string()),
        ],
    )?;
    let data = value
        .get("data")
        .ok_or_else(|| DrcomError::message("Dr.COM config response has no data"))?;
    Ok(PortalSettings {
        login_method: string_field(data, "login_method"),
        account_prefix: string_field(data, "account_prefix") == "1",
        account_suffix: string_field(data, "account_suffix"),
        check_online_method: string_field(data, "check_online_method")
            .parse()
            .unwrap_or(0),
        vlan: non_empty_or(string_field(data, "cvlan_id"), "4095"),
        ac_logout: non_empty_or(string_field(data, "ac_logout"), "0"),
        register_mode: non_empty_or(string_field(data, "register_mode"), "1"),
        eportal_port: string_field(data, "ep_http_port")
            .parse()
            .unwrap_or(DEFAULT_EPORTAL_PORT),
    })
}

fn jsonp_get(url: &str, mut pairs: Vec<(&str, String)>) -> Result<Value, DrcomError> {
    let nonce = nonce();
    let callback = format!("dr{nonce}");
    let mut ordered = Vec::with_capacity(pairs.len() + 4);
    ordered.push(("callback".to_string(), callback.clone()));
    for (key, value) in pairs.drain(..) {
        ordered.push((key.to_string(), value));
    }
    ordered.push(("jsVersion".to_string(), JS_VERSION.to_string()));
    ordered.push(("v".to_string(), nonce.to_string()));
    ordered.push(("lang".to_string(), LANGUAGE.to_string()));
    let url = format!("{url}?{}", transport::query_string(&ordered));
    drcom_jsonp(&url, &callback)
}

fn drcom_jsonp(url: &str, callback: &str) -> Result<Value, DrcomError> {
    let text = transport::get_text(url)?;
    let text = text.trim();
    let prefix_len = callback.len() + 1;
    let suffix_len = if text.ends_with(';') { 2 } else { 1 };
    let shaped = text.len() > prefix_len + suffix_len
        && text.starts_with(callback)
        && text.as_bytes()[callback.len()] == b'('
        && (text.ends_with(')') || text.ends_with(");"));
    if !shaped {
        let head: String = text.chars().take(64).collect();
        return Err(DrcomError::message(format!(
            "Response is not a Dr.COM JSONP payload: {head:?}"
        )));
    }
    serde_json::from_str(&text[prefix_len..text.len() - suffix_len])
        .map_err(|error| DrcomError::message(format!("invalid Dr.COM JSONP: {error}")))
}

fn ensure_success(operation: &str, value: &Value) -> Result<(), DrcomError> {
    if result_is_success(value) {
        Ok(())
    } else {
        let detail = value
            .get("msg")
            .or_else(|| value.get("message"))
            .or_else(|| value.get("error"))
            .map(Value::to_string)
            .unwrap_or_else(|| value.to_string());
        Err(DrcomError::message(format!(
            "Dr.COM {operation} failed: {detail}"
        )))
    }
}

fn result_is_success(value: &Value) -> bool {
    match value.get("result") {
        Some(Value::Number(number)) => number.as_i64() == Some(1),
        Some(Value::String(result)) => result == "1" || result == "ok",
        _ => false,
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn non_empty_or(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn assignment(text: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=");
    let start = text.find(&marker)? + marker.len();
    let rest = text[start..].trim_start();
    if let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let end = rest[quote.len_utf8()..].find(quote)? + quote.len_utf8();
        return Some(rest[quote.len_utf8()..end].to_string());
    }
    Some(
        rest.split(|c: char| c == ';' || c.is_whitespace())
            .next()
            .unwrap_or_default()
            .to_string(),
    )
}

fn local_ipv4() -> Option<String> {
    sys::network_interfaces()
        .into_iter()
        .filter_map(|interface| match interface.addr {
            IpAddr::V4(address) if !address.is_loopback() && !address.is_unspecified() => {
                Some(address.to_string())
            }
            _ => None,
        })
        .next()
}

fn ipv4_to_integer(ip: &str) -> Result<u32, DrcomError> {
    ip.parse::<std::net::Ipv4Addr>()
        .map(u32::from)
        .map_err(|_| DrcomError::message(format!("invalid Dr.COM IPv4 address: {ip}")))
}

fn endpoint_origin(origin: &str, port: u16) -> Result<String, DrcomError> {
    let (scheme, authority) = origin
        .split_once("://")
        .ok_or_else(|| DrcomError::message("invalid Dr.COM portal origin"))?;
    let host = if authority.starts_with('[') {
        let end = authority
            .find(']')
            .ok_or_else(|| DrcomError::message("invalid Dr.COM portal host"))?;
        &authority[..=end]
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    Ok(format!("{scheme}://{host}:{port}"))
}

fn nonce() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    nanos % 9_500 + 500
}

#[cfg(test)]
mod tests {
    use super::{assignment, result_is_success};
    use serde_json::json;

    #[test]
    fn reads_drcom_script_assignments() {
        assert_eq!(
            assignment("v4ip='172.29.5.68';", "v4ip"),
            Some("172.29.5.68".to_string())
        );
        assert_eq!(
            assignment("authloginport=801;", "authloginport"),
            Some("801".to_string())
        );
    }

    #[test]
    fn accepts_numeric_and_string_success_results() {
        assert!(result_is_success(&json!({"result": 1})));
        assert!(result_is_success(&json!({"result": "ok"})));
        assert!(!result_is_success(&json!({"result": 0})));
    }
}
