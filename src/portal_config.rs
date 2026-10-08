//! The server configuration the portal page carries (SPEC §3.2).
//!
//! `getAuthConfig` fetches `GET {origin}/srun_portal_pc?ac_id=…&theme=app` and
//! reads one hidden element per setting with cheerio. Every "JSON" row of the
//! SPEC §3.2 table goes through `JSON.parse(html())`, so the element text must
//! be valid JSON — strings arrive quoted (`"1"`, `"en-US"`) — while the custom
//! rows are taken as raw text. The decoded values feed the runtime options of
//! SPEC §3.3.

use serde_json::{Map, Value};

use crate::html::element_html;

/// `SrunPortalConfig` (SPEC §3.2/§3.3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PortalConfig {
    pub acid: String,
    pub ip: String,
    pub mac: String,
    pub nas: String,
    pub lang: String,
    pub is_ipv6: bool,
    /// `#domain`, parsed but unused by the runtime: kept exactly as the portal
    /// sends it (any JSON shape).
    pub domain: Value,
    pub portal: PortalFlags,
    pub custom: CustomConfig,
}

/// The `custom` block of the page: page-decoration settings, only partly used
/// by the CLI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CustomConfig {
    pub project: Option<String>,
    pub color: Option<String>,
    pub use_logo: bool,
    pub show_info_list: Vec<String>,
}

/// `config.portal`: the `#portal` JSON object, kept as-is because field names
/// are case sensitive and the set is server-defined.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PortalFlags {
    fields: Map<String, Value>,
}

impl PortalFlags {
    /// Wrap an already-parsed `#portal` object.
    pub fn from_map(fields: Map<String, Value>) -> Self {
        PortalFlags { fields }
    }

    pub fn raw(&self) -> &Map<String, Value> {
        &self.fields
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.fields.get(name)
    }

    /// JS `portal.X || fallback`: a falsy value falls back (SPEC §3.3, where
    /// `AuthIP`/`AuthIP6` fall back to the portal URL's host).
    pub fn str_or(&self, name: &str, fallback: &str) -> String {
        match self.get(name) {
            Some(value) if truthy(value) => js_string(value),
            _ => fallback.to_string(),
        }
    }

    /// JS `portal.X` used as a string: a missing or falsy property is `""`.
    pub fn str(&self, name: &str) -> String {
        self.str_or(name, "")
    }

    /// JS truthiness: `false`, `null`, `0`, `""` and a missing property are
    /// falsy; every array and object is truthy.
    pub fn bool(&self, name: &str) -> bool {
        self.get(name).is_some_and(truthy)
    }

    /// `Number(portal.X)`: a JSON number or a numeric string; `None` when the
    /// property is missing or not numeric.
    pub fn num(&self, name: &str) -> Option<i64> {
        match self.get(name)? {
            Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            Value::String(s) => s.trim().parse::<i64>().ok(),
            _ => None,
        }
    }

    /// SPEC §3.3 `config.portal.AuthIP` (`|| url.host` is applied by the
    /// runtime, which knows the URL).
    pub fn auth_ip(&self) -> String {
        self.str("AuthIP")
    }

    /// SPEC §3.3 `config.portal.AuthIP6 || url.host`.
    pub fn auth_ip6(&self) -> String {
        self.str("AuthIP6")
    }

    /// `config.portal.DoubleStackPC` (SPEC §3.3).
    pub fn double_stack_pc(&self) -> bool {
        self.bool("DoubleStackPC")
    }

    /// `config.portal.DoubleStackMobile` (SPEC §3.3).
    pub fn double_stack_mobile(&self) -> bool {
        self.bool("DoubleStackMobile")
    }

    /// `config.portal.MacAuth` (SPEC §3.3).
    pub fn mac_auth(&self) -> bool {
        self.bool("MacAuth")
    }

    /// `config.portal.AccountFilter`: `'tolower'` / `'toupper'` or empty
    /// (SPEC §5).
    pub fn account_filter(&self) -> String {
        self.str("AccountFilter")
    }

    /// `config.portal.AuthMode` (optional flag).
    pub fn auth_mode(&self) -> String {
        self.str("AuthMode")
    }

    /// `config.portal.UserAgreeSwitch` (optional flag).
    pub fn user_agree_switch(&self) -> bool {
        self.bool("UserAgreeSwitch")
    }

    /// `config.portal.MsgApi` (optional flag).
    pub fn msg_api(&self) -> String {
        self.str("MsgApi")
    }

    /// `config.portal.CloseLogout` (optional flag).
    pub fn close_logout(&self) -> bool {
        self.bool("CloseLogout")
    }

    /// `config.portal.OtherPCStack` (optional flag).
    pub fn other_pc_stack(&self) -> bool {
        self.bool("OtherPCStack")
    }

    /// `config.portal.OtherMobileStack` (optional flag).
    pub fn other_mobile_stack(&self) -> bool {
        self.bool("OtherMobileStack")
    }

    /// `config.portal.ServiceIP` (optional flag).
    pub fn service_ip(&self) -> String {
        self.str("ServiceIP")
    }

    /// `config.portal.TrafficCarry` (optional flag).
    pub fn traffic_carry(&self) -> Option<i64> {
        self.num("TrafficCarry")
    }

    /// `config.portal.DialSwitch` (optional flag).
    pub fn dial_switch(&self) -> bool {
        self.bool("DialSwitch")
    }

    /// `config.portal.PublicSuccessPages` (optional flag).
    pub fn public_success_pages(&self) -> String {
        self.str("PublicSuccessPages")
    }

    /// `config.portal.RedirectUrl` (optional flag).
    pub fn redirect_url(&self) -> String {
        self.str("RedirectUrl")
    }
}

/// Everything that can go wrong while reading the portal page (SPEC §3.2).
#[derive(Debug)]
pub enum ConfigError {
    /// `#cliVersion` is empty or absent: `Portal CLI version is too low !!!`.
    CliVersionTooLow,
    /// The page has no element with that id, where the JSON rows require one.
    MissingId(String),
    /// `JSON.parse` of the element text threw.
    NotJson {
        id: String,
        raw: String,
        detail: String,
    },
    /// The element held valid JSON of the wrong type (a string was required).
    NotString { id: String, raw: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::CliVersionTooLow => f.write_str(crate::messages::CLI_VERSION_TOO_LOW),
            ConfigError::MissingId(id) => write!(f, "Missing element {id} in portal config"),
            ConfigError::NotJson { id, raw, detail } => {
                write!(f, "{id} is not valid JSON: {detail} (raw: {raw:?})")
            }
            ConfigError::NotString { id, raw } => {
                write!(f, "{id} is not a JSON string (raw: {raw:?})")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Decode `GET /srun_portal_pc?ac_id=…&theme=app` (SPEC §3.2).
///
/// Elements are read in the order of the SPEC §3.2 table, so the first error
/// reported is the first broken row of the page.
pub fn parse(html: &str) -> Result<PortalConfig, ConfigError> {
    // `#cliVersion` is the version gate: `.html()` non-empty passes, and a
    // missing element is `null`, which is just as empty.
    match element_html(html, "cliVersion") {
        Some(version) if !version.is_empty() => {}
        _ => return Err(ConfigError::CliVersionTooLow),
    }

    let acid = json_string(html, "acid")?;
    let ip = json_string(html, "ip")?;
    let nas = json_string(html, "nas")?;
    let mac = json_string(html, "mac")?;
    let lang = json_string(html, "lang")?;
    let is_ipv6 = json_bool(html, "isIPv6")?;
    let domain = json_value(html, "domain")?;
    let portal = json_portal(html)?;

    // `custom` is raw text. A missing element is cheerio's `null`, so
    // `project`/`color` stay `None`, `useLogo` stays false and `showInfoList`
    // stays empty; an element that exists but is empty is used verbatim
    // (`"".split(',')` is one empty entry, SPEC §3.2's `.split(',')`).
    let custom = CustomConfig {
        project: element_html(html, "project"),
        color: element_html(html, "color"),
        use_logo: element_html(html, "useLogo").is_some_and(|text| text == "true"),
        show_info_list: element_html(html, "showInfoList")
            .map(|text| text.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
    };

    Ok(PortalConfig {
        acid,
        ip,
        mac,
        nas,
        lang,
        is_ipv6,
        domain,
        portal,
        custom,
    })
}

/// `JSON.parse($('#{id}').html())`, kept as an arbitrary JSON value.
fn json_value(html: &str, id: &str) -> Result<Value, ConfigError> {
    let raw = raw_text(html, id)?;
    serde_json::from_str::<Value>(&raw).map_err(|e| ConfigError::NotJson {
        id: selector(id),
        raw,
        detail: e.to_string(),
    })
}

/// `JSON.parse` of a row that must be a JSON string (the page quotes them).
fn json_string(html: &str, id: &str) -> Result<String, ConfigError> {
    let raw = raw_text(html, id)?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::String(s)) => Ok(s),
        Ok(_) => Err(ConfigError::NotString {
            id: selector(id),
            raw,
        }),
        Err(e) => Err(ConfigError::NotJson {
            id: selector(id),
            raw,
            detail: e.to_string(),
        }),
    }
}

/// `JSON.parse` of a row that must be a JSON boolean.
fn json_bool(html: &str, id: &str) -> Result<bool, ConfigError> {
    let raw = raw_text(html, id)?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Bool(b)) => Ok(b),
        Ok(_) => Err(ConfigError::NotString {
            id: selector(id),
            raw,
        }),
        Err(e) => Err(ConfigError::NotJson {
            id: selector(id),
            raw,
            detail: e.to_string(),
        }),
    }
}

/// `JSON.parse` of `#portal`, which must be an object — a missing element
/// parses as nothing at all, hence `NotJson` rather than `MissingId`.
fn json_portal(html: &str) -> Result<PortalFlags, ConfigError> {
    let raw = element_html(html, "portal").unwrap_or_default();
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Ok(PortalFlags::from_map(map)),
        Ok(_) => Err(ConfigError::NotJson {
            id: selector("portal"),
            raw,
            detail: "expected a JSON object".to_string(),
        }),
        Err(e) => Err(ConfigError::NotJson {
            id: selector("portal"),
            raw,
            detail: e.to_string(),
        }),
    }
}

/// Raw element text; the caller's element must be present.
fn raw_text(html: &str, id: &str) -> Result<String, ConfigError> {
    element_html(html, id).ok_or_else(|| ConfigError::MissingId(selector(id)))
}

fn selector(id: &str) -> String {
    format!("#{id}")
}

/// JS truthiness (SPEC §3.2: the flags go through `||` and `if`).
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `String(value)` for the scalars the page carries; containers are
/// rendered as compact JSON, which no flag the CLI reads ever is.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, ConfigError, PortalConfig};
    use serde_json::json;

    /// The shapes a Srun portal serves: hidden spans carrying the config, one
    /// element per SPEC §3.2 selector. Deliberately mixes quoting, attribute
    /// order, a nested element and a preceding `data-id` decoy.
    const FULL_PAGE: &str = r##"<!DOCTYPE html>
<html>
  <head><meta charset="utf-8"><title>Srun Portal</title></head>
  <body>
    <span data-id="acid">not-json</span>
    <span id="cliVersion">1.9.15</span>
    <p class="hidden" id='acid'>"12"</p>
    <p class="hidden" id="ip">"10.9.9.9"</p>
    <div class="wrap"><span id="nas">"10.0.0.1"</span></div>
    <span id='mac'>"aa:bb:cc:dd:ee:ff"</span>
    <span id="lang">"zh-CN"</span>
    <span id="isIPv6">false</span>
    <span id="domain">{"domain":"default"}</span>
    <span id="portal">{"AuthIP":"1.1.1.1","AuthIP6":"::1","ServiceIP":"2.2.2.2","DoubleStackMobile":true,"DoubleStackPC":false,"MacAuth":true,"TrafficCarry":100,"AccountFilter":"tolower","UserAgreeSwitch":true,"OtherMobileStack":true,"OtherPCStack":false,"MsgApi":"msg","AuthMode":"auth","CloseLogout":true,"DialSwitch":true,"PublicSuccessPages":"pages","RedirectUrl":"https://net.szu.edu.cn/"}</span>
    <input id="voidFlag" value="ignored">
    <span id="project">proj</span>
    <span id="color">#fff</span>
    <span id="useLogo">true</span>
    <span id="showInfoList">a,b,c</span>
  </body>
</html>"##;

    /// Every element a valid page carries ahead of `#portal`, so the failure
    /// fixtures below are the real page with exactly one element broken.
    const VALID_PREFIX: &str = r##"<span id="cliVersion">1.9.15</span><span id="acid">"12"</span><span id="ip">"10.9.9.9"</span><span id="nas">"10.0.0.1"</span><span id="mac">"aa:bb:cc:dd:ee:ff"</span><span id="lang">"zh-CN"</span><span id="isIPv6">false</span><span id="domain">{"domain":"default"}</span>"##;

    fn page(tail: &str) -> String {
        format!("{VALID_PREFIX}{tail}")
    }

    #[test]
    fn full_page_decodes_every_field() {
        let cfg: PortalConfig = parse(FULL_PAGE).expect("valid page");
        assert_eq!(cfg.acid, "12");
        assert_eq!(cfg.ip, "10.9.9.9");
        assert_eq!(cfg.mac, "aa:bb:cc:dd:ee:ff");
        assert_eq!(cfg.nas, "10.0.0.1");
        assert_eq!(cfg.lang, "zh-CN");
        assert!(!cfg.is_ipv6);
        assert_eq!(cfg.domain, json!({"domain": "default"}));

        assert_eq!(cfg.custom.project.as_deref(), Some("proj"));
        assert_eq!(cfg.custom.color.as_deref(), Some("#fff"));
        assert!(cfg.custom.use_logo);
        assert_eq!(cfg.custom.show_info_list, vec!["a", "b", "c"]);
    }

    #[test]
    fn portal_flags_read_back_through_named_accessors() {
        let flags = parse(FULL_PAGE).unwrap().portal;
        assert_eq!(flags.auth_ip(), "1.1.1.1");
        assert_eq!(flags.auth_ip6(), "::1");
        assert_eq!(flags.service_ip(), "2.2.2.2");
        assert!(flags.double_stack_mobile());
        assert!(!flags.double_stack_pc());
        assert!(flags.mac_auth());
        assert_eq!(flags.account_filter(), "tolower");
        assert!(flags.user_agree_switch());
        assert!(flags.other_mobile_stack());
        assert!(!flags.other_pc_stack());
        assert_eq!(flags.msg_api(), "msg");
        assert_eq!(flags.auth_mode(), "auth");
        assert!(flags.close_logout());
        assert!(flags.dial_switch());
        assert_eq!(flags.public_success_pages(), "pages");
        assert_eq!(flags.redirect_url(), "https://net.szu.edu.cn/");
        assert_eq!(flags.traffic_carry(), Some(100));

        assert_eq!(flags.raw().len(), 17);
        assert_eq!(flags.get("AuthIP"), Some(&json!("1.1.1.1")));
        assert_eq!(flags.get("Missing"), None);
        assert_eq!(flags.str("Missing"), "");
        assert!(!flags.bool("Missing"));
        assert_eq!(flags.num("Missing"), None);
        assert_eq!(flags.num("AuthIP"), None);
        assert_eq!(flags.str_or("Missing", "fallback"), "fallback");
        assert_eq!(flags.str_or("AccountFilter", "fallback"), "tolower");
    }

    #[test]
    fn optional_flags_are_absent_without_the_property() {
        let flags = parse(&page(r##"<span id="portal">{}</span>"##))
            .unwrap()
            .portal;
        assert_eq!(flags.raw().len(), 0);
        assert_eq!(flags.auth_ip(), "");
        assert_eq!(flags.auth_ip6(), "");
        assert_eq!(flags.service_ip(), "");
        assert!(!flags.mac_auth());
        assert!(!flags.user_agree_switch());
        assert!(!flags.other_pc_stack());
        assert!(!flags.dial_switch());
        assert_eq!(flags.traffic_carry(), None);
        assert_eq!(flags.public_success_pages(), "");
        assert_eq!(flags.redirect_url(), "");
    }

    #[test]
    fn falsy_flags_fall_back_like_js() {
        let flags = parse(&page(
            r##"<span id="portal">{"AuthIP":"","TrafficCarry":"300","DoubleStackPC":0,"MacAuth":false,"ServiceIP":null,"AccountFilter":""}</span>"##,
        ))
        .unwrap()
        .portal;
        assert_eq!(flags.auth_ip(), "");
        assert_eq!(flags.str_or("AuthIP", "url.host"), "url.host");
        assert_eq!(flags.str_or("ServiceIP", "url.host"), "url.host");
        assert_eq!(flags.str_or("AccountFilter", "none"), "none");
        assert_eq!(flags.traffic_carry(), Some(300));
        assert!(!flags.double_stack_pc());
        assert!(!flags.mac_auth());
    }

    #[test]
    fn custom_defaults_when_elements_are_missing() {
        let cfg = parse(FULL_PAGE.split("<span id=\"project\">").next().unwrap())
            .expect("page without custom elements");
        assert_eq!(cfg.custom.project, None);
        assert_eq!(cfg.custom.color, None);
        assert!(!cfg.custom.use_logo);
        assert!(cfg.custom.show_info_list.is_empty());
    }

    #[test]
    fn use_logo_requires_the_exact_true_text() {
        let cfg = parse(&page(
            r##"<span id="portal">{}</span><span id="useLogo">TRUE</span><span id="showInfoList">only</span>"##,
        ))
        .unwrap();
        assert!(!cfg.custom.use_logo);
        assert_eq!(cfg.custom.show_info_list, vec!["only"]);
    }

    #[test]
    fn missing_cli_version_is_too_low() {
        let err = parse(r##"<span id="acid">"12"</span>"##).unwrap_err();
        assert!(matches!(err, ConfigError::CliVersionTooLow));
        assert_eq!(err.to_string(), "Portal CLI version is too low !!!");
        // A page without any element at all behaves the same way.
        assert!(matches!(parse(""), Err(ConfigError::CliVersionTooLow)));
    }

    #[test]
    fn empty_cli_version_is_too_low() {
        let err =
            parse(r##"<span id="cliVersion"></span><span id="acid">"12"</span>"##).unwrap_err();
        assert!(matches!(err, ConfigError::CliVersionTooLow));
    }

    #[test]
    fn missing_element_is_reported_by_selector() {
        let html = r##"<span id="cliVersion">1.9.15</span><span id="acid">"12"</span>"##;
        match parse(html).unwrap_err() {
            ConfigError::MissingId(id) => assert_eq!(id, "#ip"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unquoted_number_acid_is_not_a_string() {
        let html = r##"<span id="cliVersion">1.9.15</span><span id="acid">12</span>"##;
        match parse(html).unwrap_err() {
            ConfigError::NotString { id, raw } => {
                assert_eq!(id, "#acid");
                assert_eq!(raw, "12");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invalid_json_acid_is_not_json() {
        let html = r##"<span id="cliVersion">1.9.15</span><span id="acid">"1</span>"##;
        match parse(html).unwrap_err() {
            ConfigError::NotJson { id, raw, .. } => {
                assert_eq!(id, "#acid");
                assert_eq!(raw, "\"1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_portal_is_not_json() {
        match parse(&page("")).unwrap_err() {
            ConfigError::NotJson { id, raw, .. } => {
                assert_eq!(id, "#portal");
                assert_eq!(raw, "");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn array_portal_is_not_json() {
        match parse(&page(r##"<span id="portal">["AuthIP"]</span>"##)).unwrap_err() {
            ConfigError::NotJson { id, raw, .. } => {
                assert_eq!(id, "#portal");
                assert_eq!(raw, "[\"AuthIP\"]");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invalid_is_ipv6_is_rejected() {
        let prefix = r##"<span id="cliVersion">1.9.15</span><span id="acid">"12"</span><span id="ip">"10.9.9.9"</span><span id="nas">"10.0.0.1"</span><span id="mac">"aa"</span><span id="lang">"zh-CN"</span><span id="domain">{"domain":"default"}</span>"##;
        match parse(&format!("{prefix}<span id=\"isIPv6\">yes</span>")).unwrap_err() {
            ConfigError::NotJson { id, raw, .. } => {
                assert_eq!(id, "#isIPv6");
                assert_eq!(raw, "yes");
            }
            other => panic!("{other:?}"),
        }
        match parse(&format!("{prefix}<span id=\"isIPv6\">1</span>")).unwrap_err() {
            ConfigError::NotString { id, raw } => {
                assert_eq!(id, "#isIPv6");
                assert_eq!(raw, "1");
            }
            other => panic!("{other:?}"),
        }
        let ok = parse(&format!(
            "{prefix}<span id=\"isIPv6\">true</span><span id=\"portal\">{{}}</span>"
        ))
        .unwrap();
        assert!(ok.is_ipv6);
    }

    #[test]
    fn domain_keeps_whatever_json_the_portal_sends() {
        let html = r##"<span id="cliVersion">1.9.15</span><span id="acid">"12"</span><span id="ip">"1.2.3.4"</span><span id="nas">"10.0.0.1"</span><span id="mac">"aa"</span><span id="lang">"zh-CN"</span><span id="isIPv6">false</span><span id="domain">"default"</span><span id="portal">{}</span>"##;
        let cfg = parse(html).unwrap();
        assert_eq!(cfg.domain, json!("default"));
    }

    #[test]
    fn missing_domain_is_reported() {
        // `#domain` is a required row of the page, so removing it is an error.
        let html = r##"<span id="cliVersion">1.9.15</span><span id="acid">"12"</span><span id="ip">"1.2.3.4"</span><span id="nas">"10.0.0.1"</span><span id="mac">"aa"</span><span id="lang">"zh-CN"</span><span id="isIPv6">false</span>"##;
        match parse(html).unwrap_err() {
            ConfigError::MissingId(id) => assert_eq!(id, "#domain"),
            other => panic!("{other:?}"),
        }
    }
}
