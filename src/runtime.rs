//! The authentication state machine: SPEC §5 (flow), §8 (online checks and
//! retries), §9 (double stack), §10 (error dispatch).
//!
//! Fidelity notes that shape this module:
//!
//! * `getUserOtherStackIP()` is a fire-and-forget request issued at construction
//!   time, so the first double-stack decision can observe an empty
//!   other-stack address. The race is kept (SPEC §12 item 10): the address is
//!   shared behind a mutex that the probe writes, not pre-computed.
//! * Double-stack authentication is **serial** in the reference, because the
//!   `yield` inside its array literal resolves before `promiseAny` ever sees the
//!   promise (SPEC §5, §12 item 6). [`Runtime::auth_by_password`] therefore runs
//!   the local stack first and then the other stack, and returns the local
//!   stack's outcome, matching what the reference awaits.
//! * `apiVersion` tolerates a response that carries neither `sysver` nor
//!   `srun_ver` instead of crashing (SPEC §12 item 1).
//! * The `expire` window of `get_challenge` is validated (SPEC §4 recommends it
//!   for a port; the reference relies on being fast enough).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::api::{self, ApiError, Challenge, LoginParams};
use crate::portal_config::PortalConfig;
use crate::sys::NetIfAddr;
use crate::translate::Translate;
use crate::transport::Endpoints;
use crate::util::{self, Task};

/// SPEC §11 `PortalError`.
#[derive(Debug, Clone, PartialEq)]
pub enum PortalError {
    /// Displayed as `Get portal config error: …` (SPEC §10).
    ConfigFetch(String),
    CliVersionTooLow,
    Api {
        error: String,
        ecode: Option<String>,
        error_msg: Option<String>,
        ploy_msg: Option<String>,
    },
    DomainNeedsAt,
    LoginFailed,
    LogoutFailed,
    Timeout,
    Transport(String),
    MissingField { endpoint: &'static str, field: &'static str },
}

impl PortalError {
    /// Render through `Translate` so API errors follow the SPEC §10 precedence.
    pub fn render(&self, translate: &Translate) -> String {
        match self {
            PortalError::Api { error, ecode, error_msg, ploy_msg } => {
                let mut res = serde_json::Map::new();
                res.insert("error".into(), Value::String(error.clone()));
                if let Some(v) = ecode {
                    res.insert("ecode".into(), Value::String(v.clone()));
                }
                if let Some(v) = error_msg {
                    res.insert("error_msg".into(), Value::String(v.clone()));
                }
                if let Some(v) = ploy_msg {
                    res.insert("ploy_msg".into(), Value::String(v.clone()));
                }
                translate.translate_error(&Value::Object(res))
            }
            other => other.to_string(),
        }
    }
}

impl std::fmt::Display for PortalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortalError::ConfigFetch(err) => write!(f, "Get portal config error: {err}"),
            PortalError::CliVersionTooLow => f.write_str(crate::messages::CLI_VERSION_TOO_LOW),
            PortalError::Api { error, ecode, error_msg, .. } => {
                f.write_str(ecode.as_deref().or(error_msg.as_deref()).unwrap_or(error))
            }
            PortalError::DomainNeedsAt => f.write_str(crate::messages::DOMAIN_NEEDS_AT),
            PortalError::LoginFailed => f.write_str(crate::messages::LOGIN_FAILED),
            PortalError::LogoutFailed => f.write_str(crate::messages::LOGOUT_FAILED),
            PortalError::Timeout => f.write_str(util::TIMEOUT),
            PortalError::Transport(err) => write!(f, "{err}"),
            PortalError::MissingField { endpoint, field } => {
                write!(f, "{endpoint} did not return `{field}`")
            }
        }
    }
}

impl std::error::Error for PortalError {}

impl From<crate::transport::TransportError> for PortalError {
    fn from(err: crate::transport::TransportError) -> Self {
        PortalError::Transport(err.to_string())
    }
}

impl From<ApiError> for PortalError {
    fn from(err: ApiError) -> Self {
        match err {
            ApiError::Transport(err) => PortalError::Transport(err.to_string()),
            ApiError::Api { error, ecode, error_msg, ploy_msg } => {
                PortalError::Api { error, ecode, error_msg, ploy_msg }
            }
            ApiError::MissingField { endpoint, field } => {
                PortalError::MissingField { endpoint, field }
            }
        }
    }
}

impl From<PortalError> for String {
    fn from(err: PortalError) -> Self {
        err.to_string()
    }
}

/// Account information as `getUserInfo` maps it (SPEC §5): every field is
/// `?? default`, so a missing field is `""` / `0` rather than an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInfo {
    pub real_name: String,
    pub username: String,
    pub domain: String,
    pub username_with_domain: String,
    pub product_name: String,
    pub billing_name: String,
    pub balance: i64,
    pub used_flow: i64,
    pub used_time: i64,
    pub remain_flow: i64,
    pub remain_time: i64,
    pub online_device_num: i64,
    pub online_ip: String,
    pub user_mac: String,
    /// The response as received: `#showInfoList` on the portal page may name
    /// fields this client has no typed accessor for (SPEC §3.2).
    pub raw: Value,
}

impl UserInfo {
    /// SPEC §5 field mapping. `user_name@domain` composes the qualified name;
    /// an empty `domain` leaves the bare user name (the reference's sign-out
    /// vector is built from a bare name when the portal reports no domain —
    /// SPEC §13.1).
    pub fn from_response(res: &Value) -> Self {
        let username = str_field(res, "user_name");
        let domain = str_field(res, "domain");
        let username_with_domain = if domain.is_empty() {
            username.clone()
        } else {
            format!("{username}@{domain}")
        };
        UserInfo {
            real_name: str_field(res, "real_name"),
            username,
            domain,
            username_with_domain,
            product_name: str_field(res, "products_name"),
            billing_name: str_field(res, "billing_name"),
            balance: int_field(res, "user_balance"),
            used_flow: int_field(res, "sum_bytes"),
            used_time: int_field(res, "sum_seconds"),
            remain_flow: int_field(res, "remain_bytes"),
            remain_time: int_field(res, "remain_seconds"),
            online_device_num: int_field(res, "online_device_total"),
            online_ip: str_field(res, "online_ip"),
            user_mac: str_field(res, "user_mac"),
            raw: res.clone(),
        }
    }
}

fn str_field(res: &Value, key: &str) -> String {
    match res.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn int_field(res: &Value, key: &str) -> i64 {
    match res.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// `os.platform()` / `os.type()` as the runtime carries them (SPEC §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDevice {
    pub platform: String,
    pub device: String,
}

impl Default for UserDevice {
    fn default() -> Self {
        UserDevice {
            platform: crate::sys::platform().to_string(),
            device: crate::sys::os_type().to_string(),
        }
    }
}

/// The options `PortalCLI`'s constructor derives (SPEC §3.3).
#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    /// `new URL(defaultAuthURL).origin`
    pub auth_url: String,
    /// `Number(config.acid)` is only ever observed through its string form on
    /// the wire, so the page value is carried verbatim.
    pub acid: String,
    pub v4_auth_hostname: String,
    pub v6_auth_hostname: String,
    pub double_stack_pc: bool,
    pub double_stack_mobile: bool,
    pub mac_auth: bool,
    pub user_ip: String,
    pub user_mac: String,
    pub account_filter: String,
    /// The CLI hardcodes `'en-US'` (SPEC §3.3).
    pub language: String,
    pub is_desktop: bool,
    /// `clientType`, `0` for the CLI (SPEC §2).
    pub client_type: u32,
    pub user_device: UserDevice,
    /// JSONP callback name; `jsonp` by default and customisable (SPEC §4).
    pub callback: String,
    /// The fixed `n` parameter, `200` (SPEC §2).
    pub n: u32,
}

impl RuntimeOptions {
    /// SPEC §3.3, with the CLI's fixed choices (`isDesktop = true`,
    /// `clientType = 0`, `language = 'en-US'`). `portal_host` is the portal
    /// URL's `host`, port included, used when the page carries no `AuthIP`.
    pub fn from_config(cfg: &PortalConfig, origin: &str, portal_host: &str) -> Self {
        RuntimeOptions {
            auth_url: origin.to_string(),
            acid: cfg.acid.clone(),
            v4_auth_hostname: cfg.portal.str_or("AuthIP", portal_host),
            v6_auth_hostname: cfg.portal.str_or("AuthIP6", portal_host),
            double_stack_pc: cfg.portal.double_stack_pc(),
            double_stack_mobile: cfg.portal.double_stack_mobile(),
            mac_auth: cfg.portal.mac_auth(),
            user_ip: cfg.ip.clone(),
            user_mac: cfg.mac.clone(),
            account_filter: cfg.portal.account_filter(),
            language: "en-US".to_string(),
            is_desktop: true,
            client_type: api::CLIENT_TYPE,
            user_device: UserDevice::default(),
            callback: api::DEFAULT_CALLBACK.to_string(),
            n: api::LOGIN_N,
        }
    }
}

/// `checkOnlineMaxNum` attempts, `checkOnlineInterval` apart (SPEC §2, §8).
pub const CHECK_ONLINE_MAX_NUM: u32 = api::CHECK_ONLINE_MAX_NUM;
pub const CHECK_ONLINE_INTERVAL_MS: u64 = api::CHECK_ONLINE_INTERVAL_MS;

/// SPEC §11 `Runtime`, plus the state SPEC §5 threads through the flow.
pub struct Runtime {
    pub options: RuntimeOptions,
    pub ep: Endpoints,
    /// `Number(config.acid)` is not observable on the wire beyond this string.
    pub acid: String,
    pub user_ip: String,
    pub user_mac: String,
    pub is_online: bool,
    pub userinfo: UserInfo,
    pub api_version: Option<String>,
    pub translate: Translate,
    pub is_desktop: bool,
    pub client_type: u32,
    pub user_device: UserDevice,
    pub account_filter: String,
    pub callback: String,
    pub n: u32,
    /// Other-stack address, written by the fire-and-forget probe (SPEC §5).
    other_stack_ip: Arc<Mutex<String>>,
}

impl Runtime {
    pub fn new(options: RuntimeOptions) -> Self {
        let ep = Endpoints {
            origin: options.auth_url.clone(),
            v4_host: options.v4_auth_hostname.clone(),
            v6_host: options.v6_auth_hostname.clone(),
        };
        let translate = Translate::new(&options.language);
        Runtime {
            acid: options.acid.clone(),
            user_ip: options.user_ip.clone(),
            user_mac: options.user_mac.clone(),
            is_online: false,
            userinfo: UserInfo::default(),
            api_version: None,
            translate,
            is_desktop: options.is_desktop,
            client_type: options.client_type,
            user_device: options.user_device.clone(),
            account_filter: options.account_filter.clone(),
            callback: options.callback.clone(),
            n: options.n,
            ep,
            other_stack_ip: Arc::new(Mutex::new(String::new())),
            options,
        }
    }

    /// `userIPOtherStack` (SPEC §3.3, §9).
    pub fn user_ip_other_stack(&self) -> String {
        self.other_stack_ip
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set_user_ip_other_stack(&self, ip: &str) {
        let mut slot = self
            .other_stack_ip
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = ip.to_string();
    }

    /// `constructorChecker`: let the interface owning `config_ip` overwrite the
    /// stack addresses (SPEC §3.3). Runs after the probe is started, so
    /// `userIPOtherStack` is normally still empty here — as in the reference.
    pub fn apply_interface_correction(&mut self, interfaces: &[NetIfAddr], config_ip: &str) {
        let mut other = self.user_ip_other_stack();
        crate::sys::correct_user_ips(interfaces, config_ip, &mut self.user_ip, &mut other);
        self.set_user_ip_other_stack(&other);
    }

    /// `getUserOtherStackIP()`: fire-and-forget `rad_user_info` without an `ip`
    /// parameter, sent to the other stack's endpoint; `online_ip` of `'::'` or a
    /// missing field means "no other stack" (SPEC §5).
    pub fn spawn_other_stack_probe(&self) {
        let ep = self.ep.clone();
        let callback = self.callback.clone();
        let slot = Arc::clone(&self.other_stack_ip);
        std::thread::spawn(move || {
            let res = match api::rad_user_info(&ep, None, true, &callback) {
                Ok(res) => res,
                Err(_) => return,
            };
            let ip = match res.get("online_ip") {
                Some(Value::String(ip)) if ip != "::" => ip.clone(),
                _ => String::new(),
            };
            let mut slot = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = ip;
        });
    }

    /// `enableDoubleStack` (SPEC §9).
    pub fn enable_double_stack(&self) -> bool {
        let configured = if self.is_desktop {
            self.options.double_stack_pc
        } else {
            self.options.double_stack_mobile
        };
        configured
            && !self.user_ip.is_empty()
            && !self.user_ip_other_stack().is_empty()
            && !self.ep.v4_host.is_empty()
            && !self.ep.v6_host.is_empty()
    }

    fn stack_ip(&self, other_stack: bool) -> String {
        if other_stack {
            self.user_ip_other_stack()
        } else {
            self.user_ip.clone()
        }
    }

    /// `getUserInfo`: whole-object replacement of `userinfo`, `user_mac`
    /// write-back and the `apiVersion` derivation (SPEC §5, §8).
    pub fn get_user_info(&mut self, ip: Option<&str>) -> Result<&UserInfo, PortalError> {
        let ip = ip.map(|s| s.to_string()).unwrap_or_else(|| self.user_ip.clone());
        let res = api::rad_user_info(&self.ep, Some(&ip), false, &self.callback)?;
        self.userinfo = UserInfo::from_response(&res);
        self.user_mac = self.userinfo.user_mac.clone();
        self.is_online = res.get("error").and_then(Value::as_str) == Some("ok");
        self.api_version = if self.is_online { api_version_of(&res) } else { None };
        Ok(&self.userinfo)
    }

    /// `checkOnline(ip = userIP)` (SPEC §8).
    pub fn check_online(&mut self) -> Result<bool, PortalError> {
        self.get_user_info(None)?;
        Ok(self.is_online)
    }

    /// `checkSignSuccess(ip, n)`: up to `checkOnlineMaxNum` checks, a second
    /// apart, then `Login failed` (SPEC §8).
    pub fn check_sign_success(&mut self, ip: Option<&str>) -> Result<(), PortalError> {
        let ip = ip.map(|s| s.to_string());
        for attempt in 0..CHECK_ONLINE_MAX_NUM {
            if self.check_online_with(ip.as_deref())? {
                return Ok(());
            }
            if attempt + 1 == CHECK_ONLINE_MAX_NUM {
                break;
            }
            util::wait_ms(CHECK_ONLINE_INTERVAL_MS);
        }
        Err(PortalError::LoginFailed)
    }

    /// `checkSignOutSuccess(ip, n)` (SPEC §8).
    pub fn check_sign_out_success(&mut self, ip: Option<&str>) -> Result<(), PortalError> {
        let ip = ip.map(|s| s.to_string());
        for attempt in 0..CHECK_ONLINE_MAX_NUM {
            if !self.check_online_with(ip.as_deref())? {
                return Ok(());
            }
            if attempt + 1 == CHECK_ONLINE_MAX_NUM {
                break;
            }
            util::wait_ms(CHECK_ONLINE_INTERVAL_MS);
        }
        Err(PortalError::LogoutFailed)
    }

    fn check_online_with(&mut self, ip: Option<&str>) -> Result<bool, PortalError> {
        self.get_user_info(ip)?;
        Ok(self.is_online)
    }

    /// `get_challenge` (SPEC §4).
    pub fn get_challenge(
        &self,
        username: &str,
        ip: &str,
        other_stack: bool,
    ) -> Result<Challenge, PortalError> {
        api::get_challenge(&self.ep, username, ip, other_stack, &self.callback).map_err(Into::into)
    }

    /// `coreAuth(username, password, otherStack, isOTP?)` (SPEC §7.1).
    pub fn core_auth(
        &mut self,
        username_with_domain: &str,
        password: &str,
        other_stack: bool,
        is_otp: bool,
    ) -> Result<(), PortalError> {
        let ip = self.stack_ip(other_stack);
        let res = self.send_login(username_with_domain, password, &ip, other_stack, is_otp)?;

        if res.get("error").and_then(Value::as_str) == Some("ok")
            && res.get("suc_msg").and_then(Value::as_str) == Some("ip_already_online_error")
        {
            // SPEC §7.1: drop the existing binding, confirm, then retry as-is.
            let time = now_secs();
            let username = username_with_domain.to_string();
            api::send_dm(&self.ep, &username, &ip, time, other_stack, &self.callback)?;
            self.check_sign_out_success(Some(&ip))?;
            return self.core_auth(username_with_domain, password, other_stack, is_otp);
        }

        if res.get("error").and_then(Value::as_str) != Some("ok") {
            return Err(api_error(&res));
        }
        self.check_sign_success(Some(&ip))
    }

    fn send_login(
        &mut self,
        username_with_domain: &str,
        password: &str,
        ip: &str,
        other_stack: bool,
        is_otp: bool,
    ) -> Result<Value, PortalError> {
        let challenge = self.get_challenge(username_with_domain, ip, other_stack)?;
        let started = Instant::now();
        let params = LoginParams {
            username_with_domain,
            password,
            ip,
            acid: &self.acid,
            token: &challenge.token,
            n: self.n,
            client_type: self.client_type,
            other_stack,
            enable_double_stack: self.enable_double_stack(),
            os: crate::sys::os_type(),
            name: crate::sys::platform(),
            is_otp,
            callback: &self.callback,
        };
        let mut request = api::build_login(&params);
        // SPEC §4: the salt is short lived (60 s on the live portal) and the
        // reference simply assumes the flow is fast enough; re-fetch it when
        // that assumption does not hold.
        if salt_expired(challenge.expire_secs, started.elapsed()) {
            let fresh = self.get_challenge(username_with_domain, ip, other_stack)?;
            let params = LoginParams { token: &fresh.token, ..params };
            request = api::build_login(&params);
        }
        api::send_login(&self.ep, &request, other_stack, &self.callback).map_err(Into::into)
    }

    /// `authByPassword(username, password, domain)` (SPEC §5).
    pub fn auth_by_password(
        &mut self,
        username: &str,
        password: &str,
        domain: &str,
    ) -> Result<(), PortalError> {
        self.auth(username, password, domain, false)
    }

    /// The `isOTP` branch of the same call: `password` travels as `{OTP}` plus
    /// the plain secret (SPEC §7.4).
    pub fn auth_by_otp(
        &mut self,
        username: &str,
        password: &str,
        domain: &str,
    ) -> Result<(), PortalError> {
        self.auth(username, password, domain, true)
    }

    fn auth(
        &mut self,
        username: &str,
        password: &str,
        domain: &str,
        is_otp: bool,
    ) -> Result<(), PortalError> {
        let username = match self.account_filter.as_str() {
            "tolower" => username.to_lowercase(),
            "toupper" => username.to_uppercase(),
            _ => username.to_string(),
        };
        if !domain.is_empty() && !domain.starts_with('@') {
            return Err(PortalError::DomainNeedsAt);
        }
        let username_with_domain = format!("{username}{domain}");

        if self.enable_double_stack() {
            // SPEC §5/§12 item 6: the reference's `promiseAny` array literal
            // resolves the other-stack call first, so this is serial: the local
            // stack authenticates, then the other stack, and the local result is
            // what the caller sees.
            self.core_auth(&username_with_domain, password, false, is_otp)?;
            self.core_auth(&username_with_domain, password, true, is_otp)?;
        } else {
            self.core_auth(&username_with_domain, password, false, is_otp)?;
        }
        Ok(())
    }

    /// `signOut()`: DM unbind, single stack directly and double stack through
    /// `promiseAny` (SPEC §7.3, §12 item 14).
    pub fn sign_out(&mut self) -> Result<(), PortalError> {
        let username = self.username_with_domain();
        let time = now_secs();
        if self.enable_double_stack() {
            let (ep, callback) = (self.ep.clone(), self.callback.clone());
            let dm_task = |username: String, ip: String, other_stack: bool, callback: String| {
                let ep = ep.clone();
                let task: Task<()> = Box::new(move || {
                    let res = api::send_dm(&ep, &username, &ip, time, other_stack, &callback)
                        .map_err(|err| err.to_string())?;
                    if res.get("error").and_then(Value::as_str) == Some("ok") {
                        Ok(())
                    } else {
                        Err(format!("rad_user_dm: {res}"))
                    }
                });
                task
            };
            // Both stacks race here (SPEC §7.3 passes two already-started
            // promises to `promiseAny`); every failure or the timeout is
            // reported as `Timeout`, which is what `promiseAny` rejects with.
            let tasks = vec![
                dm_task(username.clone(), self.user_ip.clone(), false, callback.clone()),
                dm_task(username, self.user_ip_other_stack(), true, callback),
            ];
            util::promise_any(tasks, api::PROMISE_ANY_TIMEOUT_MS)
                .map_err(|_| PortalError::Timeout)?;
            self.check_sign_out_success(None)?;
        } else {
            let res = api::send_dm(&self.ep, &username, &self.user_ip, time, false, &self.callback)?;
            if res.get("error").and_then(Value::as_str) != Some("ok") {
                return Err(api_error(&res));
            }
            self.check_sign_out_success(None)?;
        }
        self.is_online = false;
        Ok(())
    }

    /// `coreSignOutNormal` (SPEC §7.2): part of the API surface, unused by the
    /// CLI (SPEC §12 item 14).
    pub fn sign_out_normal(&mut self) -> Result<(), PortalError> {
        let username = self.username_with_domain();
        let pairs = api::logout_pairs(&username, &self.user_ip, &self.acid, &self.callback);
        let url = format!(
            "{}?{}",
            self.ep.url("/cgi-bin/srun_portal", false),
            crate::transport::query_string(&pairs)
        );
        let res = crate::transport::jsonp(&url, &self.callback).map_err(PortalError::from)?;
        if res.get("error").and_then(Value::as_str) != Some("ok") {
            return Err(api_error(&res));
        }
        self.is_online = false;
        Ok(())
    }

    /// `usernameWithDomain` of the last `getUserInfo` (SPEC §5).
    pub fn username_with_domain(&self) -> String {
        self.userinfo.username_with_domain.clone()
    }

    /// `apiVersion` (SPEC §5): `sysver.split('.')[2]`, else `srun_ver` with its
    /// build suffix stripped. `None` when neither field is usable — the
    /// reference crashes here (SPEC §12 item 1).
    pub fn api_version(&self) -> Option<&str> {
        self.api_version.as_deref()
    }
}

fn api_version_of(res: &Value) -> Option<String> {
    if let Some(sysver) = res.get("sysver").and_then(Value::as_str) {
        if let Some(third) = sysver.split('.').nth(2) {
            return Some(third.to_string());
        }
    }
    let srun_ver = res.get("srun_ver").and_then(Value::as_str)?;
    srun_ver.split(' ').nth(2).map(|part| part.replace('B', ""))
}

fn api_error(res: &Value) -> PortalError {
    PortalError::Api {
        error: res
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        ecode: str_of(res, "ecode"),
        error_msg: str_of(res, "error_msg"),
        ploy_msg: str_of(res, "ploy_msg"),
    }
}

fn str_of(res: &Value, key: &str) -> Option<String> {
    match res.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

fn salt_expired(expire_secs: Option<u64>, elapsed: Duration) -> bool {
    match expire_secs {
        Some(expire) if expire > 0 => elapsed.as_secs() >= expire,
        _ => false,
    }
}

/// Whole seconds since the epoch — the sign-out timestamp is an integer because
/// `String(new Date())` has no milliseconds (SPEC §7.3, §12 item 7).
pub fn now_secs() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(delta) => delta.as_secs() as i64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_info_mapping_uses_defaults_for_missing_fields() {
        let res = json!({
            "error": "ok",
            "user_name": "testuser",
            "domain": "szu",
            "real_name": "Test User",
            "products_name": "p",
            "billing_name": "b",
            "user_balance": 7,
            "sum_bytes": 1,
            "sum_seconds": 2,
            "remain_bytes": 3,
            "remain_seconds": 4,
            "online_device_total": 5,
            "user_mac": "aa:bb:cc:dd:ee:ff"
        });
        let info = UserInfo::from_response(&res);
        assert_eq!(info.username, "testuser");
        assert_eq!(info.username_with_domain, "testuser@szu");
        assert_eq!(info.balance, 7);
        assert_eq!(info.online_device_num, 5);

        let bare = UserInfo::from_response(&json!({"user_name": "testuser", "domain": ""}));
        assert_eq!(bare.username_with_domain, "testuser");
        assert_eq!(bare.real_name, "");
        assert_eq!(bare.balance, 0);
    }

    #[test]
    fn api_version_reads_the_third_component_and_tolerates_missing_fields() {
        assert_eq!(
            api_version_of(&json!({"sysver": "1.01.20260320"})),
            Some("20260320".to_string())
        );
        assert_eq!(
            api_version_of(&json!({"srun_ver": "Srun 3 2.0.0B"})),
            Some("2.0.0".to_string())
        );
        assert_eq!(api_version_of(&json!({"sysver": "1.01"})), None);
        assert_eq!(api_version_of(&json!({})), None);
    }

    #[test]
    fn salt_expiry_only_triggers_on_a_positive_window() {
        assert!(!salt_expired(None, Duration::from_secs(3_600)));
        assert!(!salt_expired(Some(0), Duration::from_secs(3_600)));
        assert!(!salt_expired(Some(60), Duration::from_secs(59)));
        assert!(salt_expired(Some(60), Duration::from_secs(60)));
    }

    #[test]
    fn now_secs_is_whole_seconds() {
        assert!(now_secs() > 1_700_000_000);
    }

    #[test]
    fn error_rendering_follows_the_translate_precedence() {
        let translate = Translate::new("en-US");
        let err = PortalError::Api {
            error: "auth".into(),
            ecode: Some("E2901".into()),
            error_msg: Some("wrong password".into()),
            ploy_msg: None,
        };
        assert_eq!(err.render(&translate), "wrong password");
        assert_eq!(PortalError::LoginFailed.to_string(), "Login failed");
        assert_eq!(
            PortalError::ConfigFetch("boom".into()).to_string(),
            "Get portal config error: boom"
        );
        assert_eq!(
            PortalError::CliVersionTooLow.to_string(),
            "Portal CLI version is too low !!!"
        );
    }
}
