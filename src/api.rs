//! Request construction for every portal endpoint (SPEC §4, §7, §8).
//!
//! Two shapes live here:
//!
//! * **pure builders** (`build_login`, `logout_pairs`, `dm_sign`, `dm_pairs`,
//!   `notice_url`) which only assemble parameters; and
//! * **senders** (`get_challenge`, `rad_user_info`, `send_login`, …) which
//!   append the query to the endpoint URL and hand the result to
//!   [`transport`].
//!
//! Every `/cgi-bin/*` endpoint goes out as a JSONP GET — `endpoint + "?" +
//! query_string(pairs)` with `callback` **last** — while `/v1` and `/v2` are
//! plain axios-style JSON GETs (SPEC §4). Because the differential test
//! compares raw query strings byte for byte, the parameter *order* below is
//! part of the contract and follows the tables in SPEC §7.1–§7.4 exactly.
//!
//! The login field order (§7.1) is:
//!
//! | # | field | value |
//! |---|---|---|
//! | 1 | `action` | `login` |
//! | 2 | `username` | `usernameWithDomain` |
//! | 3 | `password` | `{MD5}` + HMAC-MD5(password, token) — `{OTP}` + plain password when `is_otp` |
//! | 4 | `os` | `os.type()` |
//! | 5 | `name` | `os.platform()` |
//! | 6 | `double_stack` | `enableDoubleStack && otherStack` |
//! | 7 | `chksum` | SHA-1 over the §7.1 concatenation |
//! | 8 | `info` | `{SRBX1}…` (§6.4) |
//! | 9 | `ac_id` | HTML `#acid` |
//! | 10 | `ip` | this stack's IP |
//! | 11 | `n` | `200` |
//! | 12 | `type` | `0` |
//! | 13 | `callback` | `jsonp` |

use std::fmt;

use serde_json::Value;

use crate::crypto;
use crate::transport::{self, Endpoints, TransportError};

/// Path of the portal configuration page (SPEC §3.2, §4). It is the only
/// endpoint that returns HTML rather than JSONP.
pub const CONFIG_PATHNAME: &str = "/srun_portal_pc";

/// Fixed login parameter `n` (SPEC §2).
pub const LOGIN_N: u32 = 200;

/// Login parameter `type` — the CLI is a desktop client, so `clientType = 0`
/// (SPEC §2, §3.3).
pub const CLIENT_TYPE: u32 = 0;

/// Callback name the official CLI uses (SPEC §4); the server echoes whatever
/// name the caller sends, so this is only a default.
pub const DEFAULT_CALLBACK: &str = "jsonp";

/// `checkOnlineMaxNum` — retries before `Login failed` / `Logout failed`
/// (SPEC §2, §8).
pub const CHECK_ONLINE_MAX_NUM: u32 = 3;

/// `checkOnlineInterval` — delay between those retries (SPEC §2, §8).
pub const CHECK_ONLINE_INTERVAL_MS: u64 = 1000;

/// `promiseAny` timeout used by the double-stack race (SPEC §2, §8).
pub const PROMISE_ANY_TIMEOUT_MS: u64 = 3000;

const GET_CHALLENGE_PATH: &str = "/cgi-bin/get_challenge";
const RAD_USER_INFO_PATH: &str = "/cgi-bin/rad_user_info";
const SRUN_PORTAL_PATH: &str = "/cgi-bin/srun_portal";
const RAD_USER_DM_PATH: &str = "/cgi-bin/rad_user_dm";
const SRUNMOBILE_PORTAL_PATH: &str = "/cgi-bin/srunmobile_portal";
const PORTAL_SIGN_PATH: &str = "/v1/srun_portal_sign";
const PORTAL_MESSAGE_PATH: &str = "/v2/srun_portal_message";
const PORTAL_AGREE_PATH: &str = "/v1/srun_portal_agree_new";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure of an API call.
#[derive(Debug)]
pub enum ApiError {
    /// The request could not be made or the body was not JSON.
    Transport(TransportError),
    /// The server answered, but with a protocol-level failure. All four
    /// diagnostic fields are copied verbatim from the response because the
    /// runtime hands this variant to `Translate::translate_error` (SPEC §10),
    /// whose precedence is `ploy_msg` → `ecode == "E2901"` → `ecode` →
    /// `error_msg` → `error`.
    Api {
        error: String,
        ecode: Option<String>,
        error_msg: Option<String>,
        ploy_msg: Option<String>,
    },
    /// A field the protocol requires was absent or had the wrong type.
    MissingField {
        endpoint: &'static str,
        field: &'static str,
    },
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Transport(err) => write!(f, "{err}"),
            ApiError::Api { error, .. } => write!(f, "{error}"),
            ApiError::MissingField { endpoint, field } => {
                write!(f, "Missing field `{field}` in {endpoint} response")
            }
        }
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ApiError::Transport(err) => Some(err),
            _ => None,
        }
    }
}

impl From<TransportError> for ApiError {
    fn from(err: TransportError) -> Self {
        ApiError::Transport(err)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Serialise `entries` into owned pairs, preserving order.
fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

/// Read a response field as text. A JSON string is used as is; a number or
/// boolean is rendered in its JSON form; `null` and absent fields are `None`.
fn field_string(value: &Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        None | Some(Value::Null) => None,
        Some(other) => Some(other.to_string()),
    }
}

/// `error === "ok"` — the success criterion of every `/cgi-bin` endpoint
/// (SPEC §4).
fn is_ok(value: &Value) -> bool {
    value.get("error").and_then(Value::as_str) == Some("ok")
}

/// Build the `Api` variant from a response body, keeping the diagnostic fields
/// verbatim. The `/v1` endpoints have no `error` field, so their `code`/`Code`
/// stands in for it (SPEC §4, §10).
fn api_error(value: &Value) -> ApiError {
    let error = field_string(value, "error")
        .or_else(|| field_string(value, "code"))
        .or_else(|| field_string(value, "Code"))
        .unwrap_or_default();
    let error_msg = field_string(value, "error_msg")
        .or_else(|| field_string(value, "message"))
        .or_else(|| field_string(value, "msg"));
    ApiError::Api {
        error,
        ecode: field_string(value, "ecode"),
        error_msg,
        ploy_msg: field_string(value, "ploy_msg"),
    }
}

/// `endpoint?query` (SPEC §4: `url.search` is replaced wholesale by the
/// `URLSearchParams` serialisation).
fn endpoint_url(ep: &Endpoints, pathname: &str, other_stack: bool, query: &str) -> String {
    format!("{}?{}", ep.url(pathname, other_stack), query)
}

/// JSONP GET of a `/cgi-bin` endpoint with `pairs` in the given order.
fn jsonp_get(
    ep: &Endpoints,
    pathname: &str,
    other_stack: bool,
    ordered: &[(String, String)],
    callback: &str,
) -> Result<Value, ApiError> {
    let url = endpoint_url(ep, pathname, other_stack, &transport::query_string(ordered));
    Ok(transport::jsonp(&url, callback)?)
}

// ---------------------------------------------------------------------------
// /cgi-bin/get_challenge (SPEC §4)
// ---------------------------------------------------------------------------

/// A freshly issued challenge (salt) plus everything else the server sent.
#[derive(Debug, Clone)]
pub struct Challenge {
    /// The `challenge` field — the key of the HMAC and the XXTEA key.
    pub token: String,
    /// `expire`, in seconds. The official client ignores it (SPEC §12 item 8);
    /// it is kept so callers can re-issue a stale challenge themselves.
    pub expire_secs: Option<u64>,
    /// The untouched response body.
    pub raw: Value,
}

/// `username, ip, callback` (SPEC §4).
fn get_challenge_pairs(username: &str, ip: &str, callback: &str) -> Vec<(String, String)> {
    pairs(&[
        ("username", username),
        ("ip", ip),
        ("callback", callback),
    ])
}

/// Read `expire` when it is a number; a non-integral or negative value is
/// ignored rather than truncated.
fn expire_secs(value: &Value) -> Option<u64> {
    let field = value.get("expire")?;
    field.as_u64().or_else(|| {
        field
            .as_f64()
            .filter(|secs| secs.is_finite() && *secs >= 0.0 && secs.fract() == 0.0)
            .map(|secs| secs as u64)
    })
}

/// GET `/cgi-bin/get_challenge` and return the challenge (SPEC §4).
pub fn get_challenge(
    ep: &Endpoints,
    username: &str,
    ip: &str,
    other_stack: bool,
    callback: &str,
) -> Result<Challenge, ApiError> {
    let value = jsonp_get(
        ep,
        GET_CHALLENGE_PATH,
        other_stack,
        &get_challenge_pairs(username, ip, callback),
        callback,
    )?;
    if !is_ok(&value) {
        return Err(api_error(&value));
    }
    let token = match value.get("challenge").and_then(Value::as_str) {
        Some(token) => token.to_string(),
        None => {
            return Err(ApiError::MissingField {
                endpoint: GET_CHALLENGE_PATH,
                field: "challenge",
            })
        }
    };
    Ok(Challenge {
        token,
        expire_secs: expire_secs(&value),
        raw: value,
    })
}

// ---------------------------------------------------------------------------
// /cgi-bin/rad_user_info (SPEC §4, §5, §9)
// ---------------------------------------------------------------------------

/// `[ip], callback` — the `ip` parameter is **omitted entirely** when absent.
/// That omission is load-bearing: it is how the start-up probe asks the server
/// which IP of the other stack is online (SPEC §5, §9).
fn rad_user_info_pairs(ip: Option<&str>, callback: &str) -> Vec<(String, String)> {
    let mut ordered = Vec::with_capacity(2);
    if let Some(ip) = ip {
        ordered.push(("ip".to_string(), ip.to_string()));
    }
    ordered.push(("callback".to_string(), callback.to_string()));
    ordered
}

/// GET `/cgi-bin/rad_user_info` and return the response body untouched.
///
/// Online-ness is *not* decided here: `checkOnline` inspects `error` itself
/// (SPEC §5), and the start-up probe reads `online_ip` even from a body whose
/// `error` is not `ok`.
pub fn rad_user_info(
    ep: &Endpoints,
    ip: Option<&str>,
    other_stack: bool,
    callback: &str,
) -> Result<Value, ApiError> {
    jsonp_get(
        ep,
        RAD_USER_INFO_PATH,
        other_stack,
        &rad_user_info_pairs(ip, callback),
        callback,
    )
}

// ---------------------------------------------------------------------------
// /cgi-bin/srun_portal — login (SPEC §7.1)
// ---------------------------------------------------------------------------

/// Inputs of a login request (SPEC §7.1). Borrowed so the builder stays pure
/// and allocation-light.
#[derive(Debug, Clone)]
pub struct LoginParams<'a> {
    /// `username` + domain; the value of the `username` field and of `info`.
    pub username_with_domain: &'a str,
    pub password: &'a str,
    pub ip: &'a str,
    /// `ac_id` from the portal page.
    pub acid: &'a str,
    /// The challenge from [`get_challenge`].
    pub token: &'a str,
    pub n: u32,
    pub client_type: u32,
    /// Whether this request targets the other stack (SPEC §9).
    pub other_stack: bool,
    pub enable_double_stack: bool,
    pub os: &'a str,
    pub name: &'a str,
    pub is_otp: bool,
    pub callback: &'a str,
}

/// An assembled login request.
#[derive(Debug, Clone)]
pub struct LoginRequest {
    /// The §7.1 parameter list, in wire order with `callback` last.
    pub pairs: Vec<(String, String)>,
    /// `query_string(&pairs)` — the ready-to-append query string.
    pub query: String,
    /// The intermediate `hmac_md5_hex(password, token)` (SPEC §7.1).
    pub hmac_password: String,
    /// The `{SRBX1}…` payload (SPEC §6.4).
    pub info: String,
    /// The `chksum` (SPEC §7.1).
    pub chksum: String,
}

/// Assemble a login request without any I/O (SPEC §7.1, §6.3, §6.4).
///
/// `chksum` covers the HMAC password even in OTP mode, where the wire
/// `password` carries the plain text instead (SPEC §7.4).
pub fn build_login(params: &LoginParams<'_>) -> LoginRequest {
    let hmac_password = crypto::hmac_md5_hex(params.password, params.token);
    let info = crypto::encode_user_info(
        params.username_with_domain,
        params.password,
        params.ip,
        params.acid,
        params.token,
    );
    let n = params.n.to_string();
    let client_type = params.client_type.to_string();
    let chksum_input = format!(
        "{token}{username}{token}{hmac}{token}{acid}{token}{ip}{token}{n}{token}{client_type}{token}{info}",
        token = params.token,
        username = params.username_with_domain,
        hmac = hmac_password,
        acid = params.acid,
        ip = params.ip,
        n = n,
        client_type = client_type,
        info = info,
    );
    let chksum = crypto::sha1_hex(&chksum_input);
    let password_field = if params.is_otp {
        format!("{{OTP}}{}", params.password)
    } else {
        format!("{{MD5}}{hmac_password}")
    };
    let double_stack = if params.enable_double_stack && params.other_stack {
        "1"
    } else {
        "0"
    };
    let ordered = pairs(&[
        ("action", "login"),
        ("username", params.username_with_domain),
        ("password", password_field.as_str()),
        ("os", params.os),
        ("name", params.name),
        ("double_stack", double_stack),
        ("chksum", chksum.as_str()),
        ("info", info.as_str()),
        ("ac_id", params.acid),
        ("ip", params.ip),
        ("n", n.as_str()),
        ("type", client_type.as_str()),
        ("callback", params.callback),
    ]);
    let query = transport::query_string(&ordered);
    LoginRequest {
        pairs: ordered,
        query,
        hmac_password,
        info,
        chksum,
    }
}

/// GET the login endpoint and unwrap the JSONP body.
///
/// The body is returned as-is: the caller decides between the
/// `suc_msg == "ip_already_online_error"` recovery path and the error path
/// (SPEC §7.1).
pub fn send_login(
    ep: &Endpoints,
    req: &LoginRequest,
    other_stack: bool,
    callback: &str,
) -> Result<Value, ApiError> {
    let url = endpoint_url(ep, SRUN_PORTAL_PATH, other_stack, &req.query);
    Ok(transport::jsonp(&url, callback)?)
}

// ---------------------------------------------------------------------------
// /cgi-bin/srun_portal — logout (SPEC §7.2)
// ---------------------------------------------------------------------------

/// The `coreSignOutNormal` parameter list — `action=logout`, `username`, `ip`,
/// `ac_id`, `callback` (SPEC §7.2). The CLI itself signs out through
/// [`send_dm`] instead; this exists because the original API surface does.
pub fn logout_pairs(
    username_with_domain: &str,
    ip: &str,
    acid: &str,
    callback: &str,
) -> Vec<(String, String)> {
    pairs(&[
        ("action", "logout"),
        ("username", username_with_domain),
        ("ip", ip),
        ("ac_id", acid),
        ("callback", callback),
    ])
}

// ---------------------------------------------------------------------------
// /cgi-bin/rad_user_dm — unbind sign-out (SPEC §7.3)
// ---------------------------------------------------------------------------

/// `sha1(time + username + ip + "1" + time)`, with `time` rendered as the same
/// integer string that goes into the query — no fractional seconds
/// (SPEC §7.3).
pub fn dm_sign(username: &str, ip: &str, time: i64) -> String {
    dm_sign_str(username, ip, &time.to_string())
}

/// The same signature over the already-rendered integer string, so the query
/// and the digest cannot drift apart.
fn dm_sign_str(username: &str, ip: &str, time: &str) -> String {
    crypto::sha1_hex(&format!("{time}{username}{ip}1{time}"))
}

/// `ip, username, time, unbind=1, sign, callback` (SPEC §7.3).
pub fn dm_pairs(username: &str, ip: &str, time: i64, callback: &str) -> Vec<(String, String)> {
    let time = time.to_string();
    let sign = dm_sign_str(username, ip, &time);
    pairs(&[
        ("ip", ip),
        ("username", username),
        ("time", time.as_str()),
        ("unbind", "1"),
        ("sign", sign.as_str()),
        ("callback", callback),
    ])
}

/// GET `/cgi-bin/rad_user_dm` and return the JSONP body (SPEC §7.3).
pub fn send_dm(
    ep: &Endpoints,
    username: &str,
    ip: &str,
    time: i64,
    other_stack: bool,
    callback: &str,
) -> Result<Value, ApiError> {
    jsonp_get(
        ep,
        RAD_USER_DM_PATH,
        other_stack,
        &dm_pairs(username, ip, time, callback),
        callback,
    )
}

// ---------------------------------------------------------------------------
// axios endpoints (SPEC §4)
// ---------------------------------------------------------------------------

/// URL of the notice endpoint.
///
/// The parameter name really is `per-page=` **including the equals sign** — the
/// original source writes `{'per-page=': 100}`, so the query reads
/// `per-page==100`. This is a known upstream defect (SPEC §4, §12 item 8) and is
/// reproduced rather than "fixed".
pub fn notice_url(ep: &Endpoints) -> String {
    format!("{}?per-page==100", ep.url(PORTAL_MESSAGE_PATH, false))
}

/// GET the notice endpoint (axios-style JSON, no JSONP wrapper).
pub fn get_notice(ep: &Endpoints) -> Result<Value, ApiError> {
    Ok(transport::get_json(&notice_url(ep))?)
}

/// The user agreement, as returned by `/v1/srun_portal_agree_new` (SPEC §4).
#[derive(Debug, Clone, Default)]
pub struct Protocol {
    pub id: Value,
    pub title: String,
    pub content: String,
}

/// GET `/v1/srun_portal_agree_new?agree_type=1` and read
/// `data.data.{id,title,content}`; absent fields fall back to `null` / `""`
/// (SPEC §4).
pub fn get_protocol(ep: &Endpoints) -> Result<Protocol, ApiError> {
    let url = format!("{}?agree_type=1", ep.url(PORTAL_AGREE_PATH, false));
    let value = transport::get_json(&url)?;
    let data = value.get("data").and_then(|data| data.get("data"));
    Ok(Protocol {
        id: data
            .and_then(|data| data.get("id"))
            .cloned()
            .unwrap_or(Value::Null),
        title: data
            .and_then(|data| data.get("title"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content: data
            .and_then(|data| data.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

// ---------------------------------------------------------------------------
// /v1/srun_portal_sign (SPEC §4, §7.4)
// ---------------------------------------------------------------------------

/// `{token, sign}` for the visitor SMS flow.
#[derive(Debug, Clone)]
pub struct PhoneSign {
    pub token: String,
    pub sign: String,
}

/// `code === 0`, tolerating the `"0"` string form.
fn is_zero(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Number(number)) => number.as_f64() == Some(0.0),
        Some(Value::String(text)) => text == "0",
        _ => false,
    }
}

/// GET `/v1/srun_portal_sign` with the caller's parameters and return the
/// signature (SPEC §4). The response fields are accepted in both the lowercase
/// and the capitalised spelling (`token`/`Token`, `sign`/`Sign`).
pub fn get_phone_sign(ep: &Endpoints, params: &[(&str, &str)]) -> Result<PhoneSign, ApiError> {
    let url = endpoint_url(
        ep,
        PORTAL_SIGN_PATH,
        false,
        &transport::query_string(&pairs(params)),
    );
    let value = transport::get_json(&url)?;
    if !is_zero(value.get("code").or_else(|| value.get("Code"))) {
        return Err(api_error(&value));
    }
    let token = value
        .get("token")
        .or_else(|| value.get("Token"))
        .and_then(Value::as_str);
    let sign = value
        .get("sign")
        .or_else(|| value.get("Sign"))
        .and_then(Value::as_str);
    match (token, sign) {
        (Some(token), Some(sign)) => Ok(PhoneSign {
            token: token.to_string(),
            sign: sign.to_string(),
        }),
        (None, _) => Err(ApiError::MissingField {
            endpoint: PORTAL_SIGN_PATH,
            field: "token",
        }),
        (_, None) => Err(ApiError::MissingField {
            endpoint: PORTAL_SIGN_PATH,
            field: "sign",
        }),
    }
}

// ---------------------------------------------------------------------------
// Visitor SMS (SPEC §7.4)
// ---------------------------------------------------------------------------

/// GET `/cgi-bin/srunmobile_portal` to send a visitor verification code.
///
/// SPEC §7.4 documents only that `sendVisitorVcode(phone)` hits this endpoint;
/// it records neither the parameter set nor their order, so this
/// implementation sends the one argument it is given, followed by `callback`,
/// and leaves the `error` check to the caller.
pub fn send_visitor_vcode(
    ep: &Endpoints,
    phone: &str,
    callback: &str,
) -> Result<Value, ApiError> {
    jsonp_get(
        ep,
        SRUNMOBILE_PORTAL_PATH,
        false,
        &pairs(&[("phone", phone), ("callback", callback)]),
        callback,
    )
}

/// Visitor SMS authentication (SPEC §7.4).
///
/// First fetch `{token, sign}` from `/v1/srun_portal_sign` with
/// `type=auth, ac_id, phone, vcode, t, ip`, then merge them into one JSONP GET
/// of `/cgi-bin/srunmobile_portal`. SPEC §7.4 lists the merged field *names*
/// but neither their order nor this function's endpoint arguments, so the order
/// chosen here follows the order in which the spec lists them
/// (`token, sign, ac_id, phone, vcode, t, ip, mac, type=1, os, name, callback`);
/// the sign request uses the same order as the spec's `getPhoneSign` call.
#[allow(clippy::too_many_arguments)]
pub fn auth_by_sms_visitor(
    ep: &Endpoints,
    phone: &str,
    vcode: &str,
    t: &str,
    ac_id: &str,
    ip: &str,
    mac: &str,
    device: &str,
    platform: &str,
    callback: &str,
) -> Result<Value, ApiError> {
    let signature = get_phone_sign(
        ep,
        &[
            ("type", "auth"),
            ("ac_id", ac_id),
            ("phone", phone),
            ("vcode", vcode),
            ("t", t),
            ("ip", ip),
        ],
    )?;
    let ordered = pairs(&[
        ("token", signature.token.as_str()),
        ("sign", signature.sign.as_str()),
        ("ac_id", ac_id),
        ("phone", phone),
        ("vcode", vcode),
        ("t", t),
        ("ip", ip),
        ("mac", mac),
        ("type", "1"),
        ("os", device),
        ("name", platform),
        ("callback", callback),
    ]);
    let value = jsonp_get(ep, SRUNMOBILE_PORTAL_PATH, false, &ordered, callback)?;
    if !is_ok(&value) {
        return Err(api_error(&value));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const HMAC: &str = "94401f94e5f4eecf2f52ef15389fc664";
    const INFO: &str = "{SRBX1}1IPGZziAV7Q51Ak77DPQy+3T4n3fHXviay76y9/gGgyBNqV0INj7BUrtgOboiAMJpLywBA//0FJkVyPJIKGZf5HZe5M7TmAEvgrt3110qKZ2oo/TMdrVXahgu8+XAUQjxegK9v==";
    const CHKSUM: &str = "5eaddabca3a1435c9c45e18fa255b3a61a1397e8";
    const LOGIN_NAMES: [&str; 13] = [
        "action",
        "username",
        "password",
        "os",
        "name",
        "double_stack",
        "chksum",
        "info",
        "ac_id",
        "ip",
        "n",
        "type",
        "callback",
    ];

    fn names(ordered: &[(String, String)]) -> Vec<&str> {
        ordered.iter().map(|(key, _)| key.as_str()).collect()
    }

    fn value_of<'r>(req: &'r LoginRequest, name: &str) -> &'r str {
        req.pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .expect("pair present")
    }

    fn params(is_otp: bool, enable_double_stack: bool, other_stack: bool) -> LoginParams<'static> {
        LoginParams {
            username_with_domain: "testuser",
            password: "testpass",
            ip: "10.9.9.9",
            acid: "1",
            token: TOKEN,
            n: LOGIN_N,
            client_type: CLIENT_TYPE,
            other_stack,
            enable_double_stack,
            os: "Linux",
            name: "linux",
            is_otp,
            callback: DEFAULT_CALLBACK,
        }
    }

    fn endpoints(origin: &str) -> Endpoints {
        Endpoints {
            origin: origin.to_string(),
            v4_host: String::new(),
            v6_host: String::new(),
        }
    }

    // -- 1. the captured login vector (SPEC §13.1) --------------------------

    #[test]
    fn build_login_matches_original_vector() {
        let req = build_login(&params(false, false, false));
        assert_eq!(req.hmac_password, HMAC);
        assert_eq!(req.info, INFO);
        assert_eq!(req.chksum, CHKSUM);
        assert_eq!(names(&req.pairs), LOGIN_NAMES);
        assert_eq!(value_of(&req, "action"), "login");
        assert_eq!(value_of(&req, "username"), "testuser");
        assert_eq!(value_of(&req, "password"), "{MD5}94401f94e5f4eecf2f52ef15389fc664");
        assert_eq!(value_of(&req, "os"), "Linux");
        assert_eq!(value_of(&req, "name"), "linux");
        assert_eq!(value_of(&req, "ac_id"), "1");
        assert_eq!(value_of(&req, "ip"), "10.9.9.9");
        assert_eq!(value_of(&req, "n"), "200");
        assert_eq!(value_of(&req, "type"), "0");
        assert_eq!(value_of(&req, "callback"), "jsonp");
        assert_eq!(req.query, transport::query_string(&req.pairs));
        assert!(req.query.starts_with(
            "action=login&username=testuser&password=%7BMD5%7D94401f94e5f4eecf2f52ef15389fc664&os=Linux&name=linux&double_stack=0&chksum=5eaddabca3a1435c9c45e18fa255b3a61a1397e8&info=%7BSRBX1%7D"
        ));
        assert!(req
            .query
            .ends_with("&ac_id=1&ip=10.9.9.9&n=200&type=0&callback=jsonp"));
    }

    // -- 2. the captured sign-out vector (SPEC §7.3, §13.1) -----------------

    #[test]
    fn dm_sign_and_pairs_match_original_vector() {
        assert_eq!(
            dm_sign("testuser", "10.9.9.9", 1789819586),
            "061d82a8c262f51ecc1a07903027fdce0d8d11f6"
        );
        let ordered = dm_pairs("testuser", "10.9.9.9", 1789819586, DEFAULT_CALLBACK);
        assert_eq!(
            names(&ordered),
            ["ip", "username", "time", "unbind", "sign", "callback"]
        );
        assert_eq!(ordered[0].1, "10.9.9.9");
        assert_eq!(ordered[1].1, "testuser");
        assert_eq!(ordered[2].1, "1789819586");
        assert_eq!(ordered[3].1, "1");
        assert_eq!(ordered[4].1, "061d82a8c262f51ecc1a07903027fdce0d8d11f6");
        assert_eq!(ordered[5].1, "jsonp");
    }

    // -- 3. double_stack (SPEC §9) ------------------------------------------

    #[test]
    fn double_stack_follows_enable_and_other_stack() {
        assert_eq!(value_of(&build_login(&params(false, true, false)), "double_stack"), "0");
        assert_eq!(value_of(&build_login(&params(false, true, true)), "double_stack"), "1");
        assert_eq!(value_of(&build_login(&params(false, false, true)), "double_stack"), "0");
    }

    // -- 4. OTP mode (SPEC §7.4) --------------------------------------------

    #[test]
    fn otp_password_is_plain_but_chksum_covers_hmac() {
        let md5 = build_login(&params(false, false, false));
        let otp = build_login(&params(true, false, false));
        assert_eq!(value_of(&otp, "password"), "{OTP}testpass");
        assert_eq!(otp.chksum, md5.chksum);
        assert_eq!(otp.hmac_password, md5.hmac_password);
    }

    // -- 5. logout and notice ----------------------------------------------

    #[test]
    fn logout_pairs_and_notice_url() {
        let ordered = logout_pairs("testuser@szu.edu.cn", "10.9.9.9", "12", DEFAULT_CALLBACK);
        assert_eq!(names(&ordered), ["action", "username", "ip", "ac_id", "callback"]);
        assert_eq!(ordered[0].1, "logout");
        assert_eq!(ordered[1].1, "testuser@szu.edu.cn");
        assert_eq!(ordered[2].1, "10.9.9.9");
        assert_eq!(ordered[3].1, "12");
        assert_eq!(ordered[4].1, "jsonp");

        let url = notice_url(&endpoints("https://net.szu.edu.cn"));
        assert!(
            url.contains("per-page==100"),
            "upstream parameter name keeps its equals sign: {url}"
        );
        assert_eq!(
            url,
            "https://net.szu.edu.cn/v2/srun_portal_message?per-page==100"
        );
    }

    // -- 6. rad_user_info omits ip when absent (SPEC §5) --------------------

    /// Minimal single-shot HTTP/1.1 server; reports the request line of every
    /// request it answers, in order.
    struct Stub {
        base: String,
        requests: mpsc::Receiver<String>,
    }

    impl Stub {
        fn endpoints(&self) -> Endpoints {
            endpoints(&self.base)
        }

        fn next_request(&self) -> String {
            self.requests.recv().expect("request line")
        }
    }

    fn serve(responses: &[&'static str]) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (sender, requests) = mpsc::channel();
        let bodies: Vec<String> = responses.iter().map(|body| (*body).to_string()).collect();
        thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = vec![0u8; 8192];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let request_line = head.lines().next().unwrap_or_default().to_string();
                let _ = sender.send(request_line);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Stub {
            base: format!("http://{addr}"),
            requests,
        }
    }

    #[test]
    fn rad_user_info_omits_ip_when_absent() {
        let stub = serve(&[
            "jsonp({\"error\":\"ok\",\"online_ip\":\"1.2.3.4\"})",
            "jsonp({\"error\":\"ok\",\"online_ip\":\"10.9.9.9\"})",
        ]);
        let ep = stub.endpoints();

        let other = rad_user_info(&ep, None, false, DEFAULT_CALLBACK).expect("probe parses");
        assert_eq!(other["online_ip"], "1.2.3.4");
        assert_eq!(
            stub.next_request(),
            "GET /cgi-bin/rad_user_info?callback=jsonp HTTP/1.1"
        );

        let own = rad_user_info(&ep, Some("10.9.9.9"), false, DEFAULT_CALLBACK).expect("check parses");
        assert_eq!(own["online_ip"], "10.9.9.9");
        assert_eq!(
            stub.next_request(),
            "GET /cgi-bin/rad_user_info?ip=10.9.9.9&callback=jsonp HTTP/1.1"
        );
    }

    // -- challenge, protocol, SMS: the JSONP/axios shapes over loopback -----

    #[test]
    fn get_challenge_builds_url_reads_token_and_rejects_bad_responses() {
        let stub = serve(&[
            "jsonp({\"error\":\"ok\",\"challenge\":\"abc\",\"expire\":60})",
            "jsonp({\"error\":\"not_ok\",\"ecode\":\"E2901\",\"error_msg\":\"bad\",\"ploy_msg\":\"ploy\"})",
            "jsonp({\"error\":\"ok\"})",
        ]);
        let ep = stub.endpoints();

        let challenge = get_challenge(&ep, "testuser", "10.9.9.9", false, DEFAULT_CALLBACK)
            .expect("challenge parses");
        assert_eq!(challenge.token, "abc");
        assert_eq!(challenge.expire_secs, Some(60));
        assert_eq!(challenge.raw["error"], "ok");
        assert_eq!(
            stub.next_request(),
            "GET /cgi-bin/get_challenge?username=testuser&ip=10.9.9.9&callback=jsonp HTTP/1.1"
        );

        match get_challenge(&ep, "testuser", "10.9.9.9", false, DEFAULT_CALLBACK) {
            Err(ApiError::Api {
                error,
                ecode,
                error_msg,
                ploy_msg,
            }) => {
                assert_eq!(error, "not_ok");
                assert_eq!(ecode.as_deref(), Some("E2901"));
                assert_eq!(error_msg.as_deref(), Some("bad"));
                assert_eq!(ploy_msg.as_deref(), Some("ploy"));
            }
            other => panic!("expected Api error, got {other:?}"),
        }

        match get_challenge(&ep, "testuser", "10.9.9.9", false, DEFAULT_CALLBACK) {
            Err(ApiError::MissingField { endpoint, field }) => {
                assert_eq!(endpoint, "/cgi-bin/get_challenge");
                assert_eq!(field, "challenge");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
    }

    #[test]
    fn get_protocol_reads_nested_fields_and_defaults() {
        let stub = serve(&[
            "{\"code\":0,\"data\":{\"data\":{\"id\":7,\"title\":\"t\",\"content\":\"c\"}}}",
            "{\"code\":0,\"data\":{}}",
        ]);
        let ep = stub.endpoints();

        let protocol = get_protocol(&ep).expect("protocol parses");
        assert_eq!(protocol.id, serde_json::json!(7));
        assert_eq!(protocol.title, "t");
        assert_eq!(protocol.content, "c");
        assert_eq!(
            stub.next_request(),
            "GET /v1/srun_portal_agree_new?agree_type=1 HTTP/1.1"
        );

        let empty = get_protocol(&ep).expect("missing nested fields default");
        assert_eq!(empty.id, Value::Null);
        assert_eq!(empty.title, "");
        assert_eq!(empty.content, "");
    }

    #[test]
    fn sms_visitor_signs_then_authenticates() {
        let stub = serve(&["{\"code\":0,\"data\":{\"token\":\"tk\",\"sign\":\"sg\"}}"]);
        let ep = stub.endpoints();

        // `token`/`sign` at the top level and a non-zero code is a failure.
        let signed = auth_by_sms_visitor(
            &ep,
            "13800000000",
            "1234",
            "99",
            "12",
            "10.9.9.9",
            "aa:bb:cc",
            "Linux",
            "linux",
            DEFAULT_CALLBACK,
        );
        // The stub answers the sign endpoint with a nested payload, so this
        // first call must fail on the missing top-level fields — proving the
        // `token`/`sign` extraction, not just the JSON parse.
        match signed {
            Err(ApiError::MissingField { endpoint, field }) => {
                assert_eq!(endpoint, "/v1/srun_portal_sign");
                assert_eq!(field, "token");
            }
            other => panic!("expected MissingField, got {other:?}"),
        }
        assert_eq!(
            stub.next_request(),
            "GET /v1/srun_portal_sign?type=auth&ac_id=12&phone=13800000000&vcode=1234&t=99&ip=10.9.9.9 HTTP/1.1"
        );

        // Same request, now with the flat body the endpoint really returns.
        let stub = serve(&[
            "{\"code\":0,\"token\":\"tk\",\"sign\":\"sg\"}",
            "jsonp({\"error\":\"ok\"})",
        ]);
        let ep = stub.endpoints();
        let value = auth_by_sms_visitor(
            &ep,
            "13800000000",
            "1234",
            "99",
            "12",
            "10.9.9.9",
            "aa:bb:cc",
            "Linux",
            "linux",
            DEFAULT_CALLBACK,
        )
        .expect("sms auth succeeds");
        assert_eq!(value["error"], "ok");
        assert_eq!(
            stub.next_request(),
            "GET /v1/srun_portal_sign?type=auth&ac_id=12&phone=13800000000&vcode=1234&t=99&ip=10.9.9.9 HTTP/1.1"
        );
        assert_eq!(
            stub.next_request(),
            "GET /cgi-bin/srunmobile_portal?token=tk&sign=sg&ac_id=12&phone=13800000000&vcode=1234&t=99&ip=10.9.9.9&mac=aa%3Abb%3Acc&type=1&os=Linux&name=linux&callback=jsonp HTTP/1.1"
        );

        // The vcode sender: SPEC §7.4 fixes neither parameter set nor order.
        let stub = serve(&["jsonp({\"error\":\"ok\"})"]);
        let ep = stub.endpoints();
        send_visitor_vcode(&ep, "13800000000", DEFAULT_CALLBACK).expect("vcode accepted");
        assert_eq!(
            stub.next_request(),
            "GET /cgi-bin/srunmobile_portal?phone=13800000000&callback=jsonp HTTP/1.1"
        );
    }
}
