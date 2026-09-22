//! Error-code to message dispatch (`Translate`, SPEC §10).
//!
//! The catalogue is *data*: the reference ships bundled `dist/lang/*.js`
//! tables, which SPEC Appendix B explicitly puts out of scope. A [`Translate`]
//! built with [`Translate::new`] therefore starts with an empty dictionary and
//! every lookup falls back to the raw string; callers that want wording load it
//! with [`Translate::insert`] or [`Translate::with_dictionary`]. The CLI-level
//! fixed strings (`Login failed`, `Get notice failed`, …) live in
//! [`crate::messages`], not here.
//!
//! Dispatch, exactly as the reference does it (SPEC §10):
//!
//! * [`Translate::t`]: JSON string → [`Translate::translate_message`]; object
//!   with an `error` key → [`Translate::translate_error`]; object with a `code`
//!   or `Code` key → [`Translate::translate_code`]; otherwise the *compact*
//!   re-serialisation of the value (the reference returns `undefined` there,
//!   which callers render as such).
//! * The lookup chain is `dictionary[lang][messageFormat(key)] ||
//!   dictionary[lang][key] || key`: an empty catalogue entry is falsy in
//!   JavaScript, so it falls through as well.
//!
//! Non-obvious textual rules are cited to SPEC §10 inline.

use std::collections::HashMap;

use serde_json::Value;

/// Message catalogue for one or more languages.
///
/// `lang` selects the table used by [`Translate::t`] and friends; the
/// dictionary is keyed by language so bundled tables can be merged in later
/// (SPEC §10, Appendix B).
#[derive(Debug, Clone, Default)]
pub struct Translate {
    lang: String,
    dictionary: HashMap<String, HashMap<String, String>>,
}

impl Translate {
    /// A translator for `lang` with an empty catalogue, so every message falls
    /// back to the raw string (SPEC Appendix B: the bundled tables are data and
    /// are not part of this crate).
    pub fn new(lang: &str) -> Self {
        Self {
            lang: lang.to_string(),
            dictionary: HashMap::new(),
        }
    }

    /// A translator for `lang` over a pre-built catalogue.
    pub fn with_dictionary(lang: &str, dictionary: HashMap<String, HashMap<String, String>>) -> Self {
        Self {
            lang: lang.to_string(),
            dictionary,
        }
    }

    /// Add one catalogue entry, creating the language table when needed.
    pub fn insert(&mut self, lang: &str, key: &str, value: &str) {
        self.dictionary
            .entry(lang.to_string())
            .or_default()
            .insert(key.to_string(), value.to_string());
    }

    /// `t(data)` (SPEC §10).
    pub fn t(&self, data: &Value) -> String {
        match data {
            Value::String(s) => self.translate_message(s),
            Value::Object(map) => {
                if map.contains_key("error") {
                    return self.translate_error(data);
                }
                if map.contains_key("code") || map.contains_key("Code") {
                    if let Some(msg) = self.translate_code(data) {
                        return msg;
                    }
                }
                // `undefined` coming out of `translateCode` / no dispatch branch
                // matched: the caller stringifies the response.
                compact(data)
            }
            _ => compact(data),
        }
    }

    /// `translateError(res)` (SPEC §10), in the reference's priority order.
    pub fn translate_error(&self, res: &Value) -> String {
        // 1. `ploy_msg`, unless it is empty or an `E0000…` placeholder.
        if let Some(msg) = string_field(res, "ploy_msg") {
            if !msg.is_empty() && !msg.starts_with("E0000") {
                return self.translate_message(msg);
            }
        }
        let ecode = string_field(res, "ecode");
        // 2. `ecode === 'E2901'` means "IP in use": the real text is in `error_msg`.
        if ecode == Some("E2901") {
            return self.translate_message(string_field(res, "error_msg").unwrap_or(""));
        }
        // 3. any other non-empty `ecode`.
        if let Some(code) = ecode.filter(|c| !c.is_empty()) {
            return self.translate_message(code);
        }
        // 4. `error_msg`.
        if let Some(msg) = string_field(res, "error_msg").filter(|m| !m.is_empty()) {
            return self.translate_message(msg);
        }
        // 5. `error` (`ok` reads as the success wording).
        self.translate_message(string_field(res, "error").unwrap_or(""))
    }

    /// `translateCode(res)` (SPEC §10); `None` models the reference returning
    /// `undefined` for `code === 1`.
    pub fn translate_code(&self, res: &Value) -> Option<String> {
        if let Some(msg) = string_field(res, "ploy_msg") {
            if !msg.is_empty() && !msg.starts_with("E0000") {
                return Some(self.translate_message(msg));
            }
        }
        if let Some(msg) = string_field(res, "message").filter(|m| !m.is_empty()) {
            return Some(self.translate_message(msg));
        }
        // The dispatch in `t` accepts `code` or `Code`, so both are read here.
        let code = res.get("code").or_else(|| res.get("Code"));
        if matches!(code, Some(Value::Number(n)) if n.as_f64() == Some(0.0)) {
            return Some(self.lookup_or("Success", "Success"));
        }
        if matches!(code, Some(Value::Number(n)) if n.as_f64() == Some(1.0)) {
            return None;
        }
        Some(self.lookup_or("Error", "Error"))
    }

    /// `translateMessage(str)` (SPEC §10): special `Exxxx` codes are truncated
    /// to their first five characters first, then the catalogue is consulted as
    /// `dictionary[lang][messageFormat(str)] || dictionary[lang][str] || str`.
    pub fn translate_message(&self, msg: &str) -> String {
        let key: String = if is_special_error(msg) {
            msg.chars().take(5).collect()
        } else {
            msg.to_string()
        };
        if let Some(hit) = self.lookup(&message_format(&key)) {
            return hit.to_string();
        }
        if let Some(hit) = self.lookup(&key) {
            return hit.to_string();
        }
        key
    }

    /// `dictionary[lang][key]`, treating an empty entry as a miss (`||` chain).
    ///
    /// A language with no table is simply a miss — the reference never indexes
    /// the catalogue in a way that can throw for an unknown language.
    fn lookup(&self, key: &str) -> Option<&str> {
        self.dictionary
            .get(&self.lang)
            .and_then(|table| table.get(key))
            .map(String::as_str)
            .filter(|hit| !hit.is_empty())
    }

    fn lookup_or<'a>(&'a self, key: &str, fallback: &'a str) -> String {
        self.lookup(key).unwrap_or(fallback).to_string()
    }
}

/// Compact (no whitespace) re-serialisation of a response value.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// `res[key]` when it is a string.
fn string_field<'a>(res: &'a Value, key: &str) -> Option<&'a str> {
    res.get(key).and_then(Value::as_str)
}

/// `isSpecialError(str)` (SPEC §10): an `E` followed by a *truthy* JavaScript
/// number in the next four characters, but never the `E2901` code itself.
pub fn is_special_error(s: &str) -> bool {
    if !s.starts_with('E') || s.starts_with("E2901") {
        return false;
    }
    // `str.substring(1, 5)`: at most four characters, fewer when the string is
    // shorter. `Number("") === 0`, so a bare `E` is not special.
    let digits: String = s.chars().skip(1).take(4).collect();
    js_number_is_truthy(&digits)
}

/// `messageFormat(str)` (SPEC §10): JavaScript
/// `str.replace(/(_|, | |^)\S/g, c => c.toUpperCase()).replace(/\./g, '')`.
///
/// Each match is a separator (`_`, `", "`, `" "`, or the zero-width start
/// anchor) followed by a non-whitespace character; the separator is dropped and
/// the character upper-cased. Afterwards every `.` is removed, which is how
/// `"Login success."` becomes `"LoginSuccess"`.
pub fn message_format(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        // The alternatives are tried left to right, as in the regex.
        let mut hit: Option<(usize, char)> = None;
        if chars[i] == '_' && next_is_non_whitespace(&chars, i + 1) {
            hit = Some((2, chars[i + 1]));
        }
        if hit.is_none()
            && chars[i] == ','
            && chars.get(i + 1) == Some(&' ')
            && next_is_non_whitespace(&chars, i + 2)
        {
            hit = Some((3, chars[i + 2]));
        }
        if hit.is_none() && chars[i] == ' ' && next_is_non_whitespace(&chars, i + 1) {
            hit = Some((2, chars[i + 1]));
        }
        // `^` is zero-width and matches only at the very start (no `m` flag).
        if hit.is_none() && i == 0 && !js_is_whitespace(chars[0]) {
            hit = Some((1, chars[0]));
        }
        match hit {
            Some((consumed, ch)) => {
                out.extend(ch.to_uppercase());
                i += consumed;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out.replace('.', "")
}

fn next_is_non_whitespace(chars: &[char], index: usize) -> bool {
    chars.get(index).is_some_and(|c| !js_is_whitespace(*c))
}

/// JavaScript `\s` (`[ \t\n\v\f\r\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000\ufeff]`).
///
/// Rust's `char::is_whitespace` differs at U+0085 (Rust yes, JS no) and
/// U+FEFF (JS yes, Rust no), so the class is spelled out.
fn js_is_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// Truthiness of `Number(s)` for a short (≤ 4 UTF-16 unit) string.
///
/// `0`, `-0`, `""`, whitespace-only and `NaN` are falsy; everything else —
/// including `0x`/`0b`/`0o` integer literals and exponents — is truthy.
fn js_number_is_truthy(s: &str) -> bool {
    let t = trim_js_whitespace(s);
    if t.is_empty() {
        return false; // Number("") === 0
    }
    let lower = t.to_ascii_lowercase();
    if let Some(digits) = lower.strip_prefix("0x") {
        return radix_nonzero(digits, 16);
    }
    if let Some(digits) = lower.strip_prefix("0b") {
        return radix_nonzero(digits, 2);
    }
    if let Some(digits) = lower.strip_prefix("0o") {
        return radix_nonzero(digits, 8);
    }
    // A sign is only allowed in front of a decimal literal or `Infinity`.
    let body = t
        .strip_prefix('+')
        .or_else(|| t.strip_prefix('-'))
        .unwrap_or(t);
    if body == "Infinity" {
        return true;
    }
    match js_decimal_literal(body) {
        Some(value) => value != 0.0,
        None => false, // NaN
    }
}

fn trim_js_whitespace(s: &str) -> &str {
    s.trim_matches(js_is_whitespace)
}

/// A radix-prefixed integer literal: every character must be a digit of that
/// radix and the value must be non-zero.
fn radix_nonzero(digits: &str, radix: u32) -> bool {
    if digits.is_empty() {
        return false; // Number("0x") is NaN
    }
    let mut non_zero = false;
    for c in digits.chars() {
        match c.to_digit(radix) {
            Some(0) => {}
            Some(_) => non_zero = true,
            None => return false, // trailing junk is NaN
        }
    }
    non_zero
}

/// `StrDecimalLiteral` (sign already stripped): `digits[.digits][(e|E)[+-]digits]`.
/// `None` is JavaScript `NaN`; overflow saturates to infinity, which is truthy
/// on both sides.
fn js_decimal_literal(body: &str) -> Option<f64> {
    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut int_digits = 0usize;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        int_digits += 1;
    }
    let mut frac_digits = 0usize;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            frac_digits += 1;
        }
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let mut exp_digits = 0usize;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            exp_digits += 1;
        }
        if exp_digits == 0 {
            return None;
        }
    }
    if i != bytes.len() {
        return None;
    }
    body.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalogue() -> HashMap<String, HashMap<String, String>> {
        let mut en = HashMap::new();
        en.insert("Success".to_string(), "OK!".to_string());
        en.insert("Error".to_string(), "bad".to_string());
        en.insert("LoginSuccess".to_string(), "logged in".to_string());
        en.insert("E2902".to_string(), "special error".to_string());
        let mut zh = HashMap::new();
        zh.insert("Success".to_string(), "成功".to_string());
        let mut dir = HashMap::new();
        dir.insert("en-US".to_string(), en);
        dir.insert("zh-CN".to_string(), zh);
        dir
    }

    fn translator() -> Translate {
        Translate::with_dictionary("en-US", catalogue())
    }

    #[test]
    fn message_format_applies_the_separator_and_dot_rules() {
        assert_eq!(message_format("Login success."), "LoginSuccess");
        assert_eq!(message_format("ip_already_online_error"), "IpAlreadyOnlineError");
        assert_eq!(message_format("a b"), "AB");
        assert_eq!(message_format("Please, try again"), "PleaseTryAgain");
        // Already-camel text keeps its first character upper-cased only.
        assert_eq!(message_format("alreadyOnline"), "AlreadyOnline");
        assert_eq!(message_format(""), "");
        assert_eq!(message_format("a.b_c"), "AbC");
    }

    #[test]
    fn is_special_error_requires_a_truthy_number_after_the_e() {
        assert!(!is_special_error("E2901")); // the IP-in-use code is never special
        assert!(is_special_error("E2902"));
        assert!(!is_special_error("E0000")); // Number("0000") === 0
        assert!(!is_special_error("ok"));
        assert!(!is_special_error("Eabcd")); // NaN
        assert!(!is_special_error("E29012")); // starts with E2901
        assert!(is_special_error("E12")); // short string: Number("12")
        assert!(is_special_error("E0x12")); // Number("0x12") === 18
        assert!(!is_special_error("E"));
        assert!(!is_special_error(""));
    }

    #[test]
    fn translate_error_follows_the_documented_precedence() {
        let t = translator();

        // 1. ploy_msg wins over everything, and takes the message-format path.
        assert_eq!(
            t.translate_error(&json!({
                "ploy_msg": "Login success.", "ecode": "E2901",
                "error_msg": "x", "error": "ok"
            })),
            "logged in"
        );
        // …unless it is an empty or E0000 placeholder.
        assert_eq!(
            t.translate_error(&json!({"ploy_msg": "", "ecode": "E2901", "error_msg": "Login success."})),
            "logged in"
        );
        assert_eq!(
            t.translate_error(&json!({"ploy_msg": "E0000", "ecode": "E2901", "error_msg": "Login success."})),
            "logged in"
        );

        // 2. E2901 reports error_msg.
        assert_eq!(
            t.translate_error(&json!({"ecode": "E2901", "error_msg": "Login success.", "error": "ok"})),
            "logged in"
        );

        // 3. any other ecode is used verbatim (truncated to five chars, then
        //    looked up in the injected dictionary — a real catalogue hit).
        assert_eq!(
            t.translate_error(&json!({"ecode": "E2902", "error_msg": "Login success."})),
            "special error"
        );
        assert_eq!(t.translate_error(&json!({"ecode": "E2903"})), "E2903");

        // 4. error_msg.
        assert_eq!(t.translate_error(&json!({"error_msg": "Login success."})), "logged in");

        // 5. error, with the identity fallback when nothing is registered.
        assert_eq!(t.translate_error(&json!({"error": "Login success."})), "logged in");
        assert_eq!(t.translate_error(&json!({"error": "ok"})), "ok");
        assert_eq!(t.translate_error(&json!({})), "");
    }

    #[test]
    fn translate_code_handles_ploy_message_success_error_and_silence() {
        let t = translator();

        // code === 0 → dictionary "Success", literal when absent.
        assert_eq!(t.translate_code(&json!({"code": 0})).as_deref(), Some("OK!"));
        assert_eq!(
            Translate::new("en-US").translate_code(&json!({"code": 0})).as_deref(),
            Some("Success")
        );
        // code === 1 → the reference returns undefined.
        assert_eq!(t.translate_code(&json!({"code": 1})), None);
        // anything else → dictionary "Error", literal when absent.
        assert_eq!(t.translate_code(&json!({"code": 2})).as_deref(), Some("bad"));
        assert_eq!(
            Translate::new("en-US").translate_code(&json!({"code": 2})).as_deref(),
            Some("Error")
        );

        // ploy_msg and message short-circuit the code switch.
        assert_eq!(
            t.translate_code(&json!({"code": 1, "ploy_msg": "Login success."})).as_deref(),
            Some("logged in")
        );
        assert_eq!(
            t.translate_code(&json!({"code": 1, "ploy_msg": "E0000", "message": "Login success."}))
                .as_deref(),
            Some("logged in")
        );
        assert_eq!(
            t.translate_code(&json!({"code": 0, "message": "Login success."})).as_deref(),
            Some("logged in")
        );

        // An unknown language is a miss, never a panic.
        assert_eq!(Translate::new("fr-FR").translate_code(&json!({"code": 0})).as_deref(), Some("Success"));
    }

    #[test]
    fn t_dispatches_strings_errors_codes_and_everything_else() {
        let t = translator();

        assert_eq!(t.t(&json!("Login success.")), "logged in");
        assert_eq!(t.t(&json!("unknown")), "unknown");
        assert_eq!(t.t(&json!({"error": "Login success."})), "logged in");
        assert_eq!(t.t(&json!({"error": "x", "code": 1})), "x"); // error wins
        assert_eq!(t.t(&json!({"code": 0})), "OK!");
        assert_eq!(t.t(&json!({"Code": 0})), "OK!");
        // `code === 1` gives no message: the response is stringified instead.
        assert_eq!(t.t(&json!({"code": 1})), r#"{"code":1}"#);
        assert_eq!(t.t(&json!({"foo": 1})), r#"{"foo":1}"#);
        assert_eq!(t.t(&json!(7)), "7");
        assert_eq!(t.t(&json!(null)), "null");
    }

    #[test]
    fn insert_adds_entries_for_any_language() {
        let mut t = Translate::new("zh-CN");
        assert_eq!(t.translate_message("Login success."), "Login success.");
        t.insert("zh-CN", "LoginSuccess", "登录成功");
        assert_eq!(t.translate_message("Login success."), "登录成功");
        // A different language table does not leak into this one.
        t.insert("en-US", "LoginSuccess", "logged in");
        assert_eq!(t.translate_message("Login success."), "登录成功");
    }
}
