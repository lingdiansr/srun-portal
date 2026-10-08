//! Legacy `~/.srun_portal.json` *readers* and portal-URL resolution (SPEC §3.1).
//!
//! The reference client stores the portal origin and the `ac_id` in
//! `~/.srun_portal.json`, next to the *passwd* home directory — `os.userInfo()
//! .homedir` follows the passwd entry and not `$HOME`, so running under `sudo`
//! touches `/root/.srun_portal.json` (SPEC §3.1). [`legacy_env_file_path`]
//! therefore goes through [`crate::sys::home_dir`]; [`legacy_env_file_path_in`]
//! exists so tests can address a scratch directory instead.
//!
//! That file is no longer written: [`crate::settings`] owns configuration now
//! and imports this one as a seed the first time it creates a file. What is
//! left here is the reader plus the URL validation both modules share.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::messages;

/// File name inside the home directory (SPEC §3.1).
const ENV_FILE_NAME: &str = ".srun_portal.json";

/// The two fields the legacy file holds (SPEC §3.1). `Default` is the "nothing
/// stored yet" state, which makes [`url_from_env`] return `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliEnv {
    pub auth_url: String,
    pub acid: String,
}

/// Failures surfaced by this module.
#[derive(Debug)]
pub enum ConfigError {
    /// No URL was supplied at all (SPEC §3.1: `Portal web url is required!`).
    PortalUrlRequired,
    /// A URL was supplied but is not a usable portal URL (SPEC §3.1).
    PortalUrlInvalid,
    /// The env file could not be read or written.
    Io(String),
    /// The env file is not valid JSON.
    Json(String),
    /// A configuration file is not valid for its format; `detail` names the
    /// offending key where there is one.
    Format { path: PathBuf, detail: String },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::PortalUrlRequired => f.write_str(messages::PORTAL_URL_REQUIRED),
            ConfigError::PortalUrlInvalid => f.write_str(messages::PORTAL_URL_INVALID),
            ConfigError::Io(detail) | ConfigError::Json(detail) => f.write_str(detail),
            ConfigError::Format { path, detail } => write!(f, "{}: {detail}", path.display()),
        }
    }
}

impl std::error::Error for ConfigError {}

/// `{home_dir}/.srun_portal.json`, the legacy file (SPEC §3.1).
pub fn legacy_env_file_path() -> Result<PathBuf, ConfigError> {
    let home = crate::sys::home_dir().map_err(ConfigError::Io)?;
    Ok(legacy_env_file_path_in(&home))
}

/// The legacy file inside an arbitrary directory — the test seam for the real
/// [`legacy_env_file_path`].
pub fn legacy_env_file_path_in(dir: &Path) -> PathBuf {
    dir.join(ENV_FILE_NAME)
}

/// Read the legacy file. A missing file yields `Ok(None)`; a malformed one is
/// an error rather than an empty result, so a caller can tell "nothing to
/// import" from "something is wrong with that file".
pub fn read_legacy() -> Result<Option<CliEnv>, ConfigError> {
    read_legacy_from(&legacy_env_file_path()?)
}

/// [`read_legacy`] against an explicit path.
pub fn read_legacy_from(path: &Path) -> Result<Option<CliEnv>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(ConfigError::Io(err.to_string())),
    };
    let value: Value = serde_json::from_str(&text).map_err(|e| ConfigError::Json(e.to_string()))?;
    // Unknown or missing keys degrade to empty strings; the reference just reads
    // `v.authURL` / `v.acid` (SPEC §3.1).
    Ok(Some(CliEnv {
        auth_url: string_field(&value, "authURL"),
        acid: string_field(&value, "acid"),
    }))
}

/// The three parts of a portal URL the caller keeps (SPEC §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortalUrl {
    /// `new URL(raw).origin`: scheme, host and non-default port only.
    pub origin: String,
    /// `new URL(raw).pathname`.
    pub pathname: String,
    /// The `ac_id` query value (percent-decoded).
    pub ac_id: String,
}
/// A root URL for the Dr.COM 4.0 EPortal used by the dormitory network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrcomUrl {
    /// `new URL(raw).origin`, without the root path or query.
    pub origin: String,
}

/// A supported portal URL. The path selects the wire protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortalTarget {
    Srun(PortalUrl),
    Drcom(DrcomUrl),
}

/// Parse either the existing Srun URL shape or a Dr.COM root URL.
pub fn parse_portal_target(raw: &str) -> Result<PortalTarget, ConfigError> {
    let cleaned: String = raw
        .trim_matches(|c: char| c.is_ascii_control() || c == ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    if cleaned.is_empty() {
        return Err(ConfigError::PortalUrlRequired);
    }
    let parts = split_absolute_url(&cleaned).ok_or(ConfigError::PortalUrlInvalid)?;
    if parts.pathname.starts_with("/srun_portal") {
        return parse_portal_url(&cleaned).map(PortalTarget::Srun);
    }
    if parts.pathname == "/" && parts.query.is_empty() {
        return Ok(PortalTarget::Drcom(DrcomUrl {
            origin: parts.origin,
        }));
    }
    Err(ConfigError::PortalUrlInvalid)
}

/// Validate a portal URL the way `CLI.start()` does (SPEC §3.1): it must parse
/// as an absolute URL, its `pathname` must start with `/srun_portal`, and it
/// must carry an `ac_id` query key.
pub fn parse_portal_url(raw: &str) -> Result<PortalUrl, ConfigError> {
    // `new URL` strips leading/trailing C0 controls and spaces, and removes tab
    // and newline characters anywhere in the input.
    let cleaned: String = raw
        .trim_matches(|c: char| c.is_ascii_control() || c == ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    if cleaned.is_empty() {
        return Err(ConfigError::PortalUrlRequired);
    }
    let parts = split_absolute_url(&cleaned).ok_or(ConfigError::PortalUrlInvalid)?;
    let UrlParts {
        origin,
        pathname,
        query,
    } = parts;
    if !pathname.starts_with("/srun_portal") {
        return Err(ConfigError::PortalUrlInvalid);
    }
    let raw_ac_id = query_param(&query, "ac_id").ok_or(ConfigError::PortalUrlInvalid)?;
    Ok(PortalUrl {
        origin,
        pathname,
        ac_id: percent_decode(&raw_ac_id),
    })
}

/// The URL slug built from the stored fields, when both are non-empty
/// (SPEC §3.1). It is validated by the caller exactly as if it had been typed.
pub fn url_from_env(env: &CliEnv) -> Option<String> {
    if env.auth_url.is_empty() || env.acid.is_empty() {
        return None;
    }
    Some(format!(
        "{}/srun_portal_pc?ac_id={}",
        env.auth_url, env.acid
    ))
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

struct UrlParts {
    origin: String,
    pathname: String,
    query: String,
}

/// `new URL(raw)` restricted to what the portal check needs: an absolute URL
/// (`scheme://` + non-empty authority), the origin, the pathname and the query.
///
/// Scheme and host are lower-cased and a port equal to the scheme's default is
/// dropped, which is what `URL.origin` returns (SPEC §3.1).
fn split_absolute_url(raw: &str) -> Option<UrlParts> {
    let (scheme, rest) = split_scheme(raw)?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return None;
    }
    let (host, port) = parse_authority(authority)?;
    let origin = match port {
        Some(port) if !is_default_port(&scheme, &port) => format!("{}://{}:{}", scheme, host, port),
        _ => format!("{}://{}", scheme, host),
    };

    let after = &rest[authority_end..];
    let query_end = after.find(['?', '#']).unwrap_or(after.len());
    let path = &after[..query_end];
    let query = match after[query_end..].strip_prefix('?') {
        Some(tail) => {
            let end = tail.find('#').unwrap_or(tail.len());
            &tail[..end]
        }
        None => "",
    };
    Some(UrlParts {
        origin,
        pathname: if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        },
        query: query.to_string(),
    })
}

/// `[A-Za-z][A-Za-z0-9+.-]*` followed by `://`.
fn split_scheme(raw: &str) -> Option<(String, &str)> {
    let colon = raw.find(':')?;
    let scheme = &raw[..colon];
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) {
        return None;
    }
    let rest = raw[colon + 1..].strip_prefix("//")?;
    Some((scheme.to_ascii_lowercase(), rest))
}

/// `[userinfo@]host[:port]`, returning the lower-cased host and the normalized
/// port. Userinfo is dropped (`URL.origin` never contains it).
fn parse_authority(authority: &str) -> Option<(String, Option<String>)> {
    let host_port = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    if let Some(bracketed) = host_port.strip_prefix('[') {
        // IPv6 literal: the host is everything up to the closing bracket.
        let close = bracketed.find(']')?;
        let host = format!("[{}]", &bracketed[..close]);
        let port = parse_port_suffix(&bracketed[close + 1..])?;
        return Some((host, port));
    }
    let (host, port) = match host_port.rfind(':') {
        Some(colon) => (&host_port[..colon], parse_port_suffix(&host_port[colon..])?),
        None => (host_port, None),
    };
    // A second colon means the host was malformed, and `new URL` rejects it.
    if host.is_empty() || host.contains(':') {
        return None;
    }
    Some((host.to_ascii_lowercase(), port))
}

/// `tail` is the text after the host: either empty, `:` ,or `:<digits>`.
///
/// Outer `None` is a parse failure (a non-numeric or out-of-range port makes
/// `new URL` throw); `None` inside means "no port".
fn parse_port_suffix(tail: &str) -> Option<Option<String>> {
    let digits = match tail.strip_prefix(':') {
        Some(digits) => digits,
        None if tail.is_empty() => return Some(None),
        None => return None,
    };
    if digits.is_empty() {
        // `http://host:/x` has no port at all.
        return Some(None);
    }
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u32 = digits.parse().ok()?;
    if value > 65535 {
        return None;
    }
    // Ports are re-serialized numerically, so `:080` is `:80`.
    Some(Some(value.to_string()))
}

fn is_default_port(scheme: &str, port: &str) -> bool {
    matches!(
        (scheme, port),
        ("http" | "ws", "80") | ("https" | "wss", "443") | ("ftp", "21")
    )
}

/// The first value of `key` in a `URLSearchParams` query string. Missing keys
/// and empty pairs are skipped; the value is returned still percent-encoded.
fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_key, raw_value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if percent_decode(raw_key) == key {
            return Some(raw_value.to_string());
        }
    }
    None
}

/// `URLSearchParams` decoding: `+` is a space, `%XX` is a byte, invalid UTF-8
/// becomes U+FFFD, and a stray `%` stays literal.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
            {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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
                "srun-portal-{}-{}-{}-{}",
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
    fn legacy_env_file_lives_in_the_passwd_home() {
        let Ok(home) = crate::sys::home_dir() else {
            return; // no passwd entry in this environment
        };
        assert_eq!(
            legacy_env_file_path().unwrap(),
            home.join(".srun_portal.json")
        );
        assert_eq!(
            legacy_env_file_path_in(Path::new("/tmp/x")),
            Path::new("/tmp/x/.srun_portal.json")
        );
    }

    #[test]
    fn legacy_env_file_is_read_back() {
        let dir = TempDir::new("legacy");
        let path = legacy_env_file_path_in(dir.path());

        // A missing file is "nothing to import", not an empty record.
        assert_eq!(read_legacy_from(&path).unwrap(), None);

        std::fs::write(&path, r#"{"authURL":"https://net.szu.edu.cn","acid":"1"}"#).unwrap();
        assert_eq!(
            read_legacy_from(&path).unwrap(),
            Some(CliEnv {
                auth_url: "https://net.szu.edu.cn".to_string(),
                acid: "1".to_string(),
            })
        );

        // Unknown keys and non-string values degrade to empty strings.
        std::fs::write(&path, r#"{"other":1,"acid":2}"#).unwrap();
        assert_eq!(read_legacy_from(&path).unwrap(), Some(CliEnv::default()));
    }

    #[test]
    fn legacy_env_file_errors_are_reported() {
        let dir = TempDir::new("legacy-errors");
        let path = legacy_env_file_path_in(dir.path());

        std::fs::write(&path, "not json").unwrap();
        assert!(matches!(read_legacy_from(&path), Err(ConfigError::Json(_))));
        std::fs::write(&path, "").unwrap();
        assert!(matches!(read_legacy_from(&path), Err(ConfigError::Json(_))));

        // A directory cannot be read as a file.
        assert!(matches!(
            read_legacy_from(dir.path()),
            Err(ConfigError::Io(_))
        ));
    }

    #[test]
    fn error_display_matches_the_cli_wording() {
        assert_eq!(
            ConfigError::PortalUrlRequired.to_string(),
            crate::messages::PORTAL_URL_REQUIRED
        );
        assert_eq!(
            ConfigError::PortalUrlInvalid.to_string(),
            crate::messages::PORTAL_URL_INVALID
        );
        assert_eq!(ConfigError::Io("boom".to_string()).to_string(), "boom");
        assert_eq!(ConfigError::Json("bad".to_string()).to_string(), "bad");
        assert_eq!(
            ConfigError::Format {
                path: PathBuf::from("/tmp/srun-portal.toml"),
                detail: "bad".to_string(),
            }
            .to_string(),
            "/tmp/srun-portal.toml: bad"
        );
    }

    #[test]
    fn parse_portal_url_accepts_the_documented_shapes() {
        let url = parse_portal_url("https://net.szu.edu.cn/srun_portal_pc?ac_id=1").unwrap();
        assert_eq!(
            url,
            PortalUrl {
                origin: "https://net.szu.edu.cn".to_string(),
                pathname: "/srun_portal_pc".to_string(),
                ac_id: "1".to_string(),
            }
        );

        // A port is part of the origin because it is not the scheme default.
        let url = parse_portal_url("http://127.0.0.1:8899/srun_portal_pc?ac_id=1").unwrap();
        assert_eq!(url.origin, "http://127.0.0.1:8899");
        assert_eq!(url.pathname, "/srun_portal_pc");
        assert_eq!(url.ac_id, "1");

        // No path beyond the mount point: the bare prefix is enough.
        let url = parse_portal_url("https://net.szu.edu.cn/srun_portal?ac_id=1").unwrap();
        assert_eq!(url.pathname, "/srun_portal");
        assert_eq!(url.ac_id, "1");

        // Extra query parameters are ignored.
        let url =
            parse_portal_url("https://net.szu.edu.cn/srun_portal_pc?ac_id=12&theme=app").unwrap();
        assert_eq!(url.ac_id, "12");
        assert_eq!(url.origin, "https://net.szu.edu.cn");

        // `new URL` normalisation: lower-cased scheme/host, default port dropped.
        assert_eq!(
            parse_portal_url("HTTPS://Net.SZU.edu.CN:443/srun_portal_pc?ac_id=1")
                .unwrap()
                .origin,
            "https://net.szu.edu.cn"
        );
        assert_eq!(
            parse_portal_url("http://h:080/srun_portal_pc?ac_id=1")
                .unwrap()
                .origin,
            "http://h"
        );
        // Userinfo is not part of the origin.
        assert_eq!(
            parse_portal_url("http://u:p@h/srun_portal_pc?ac_id=1")
                .unwrap()
                .origin,
            "http://h"
        );
        // The fragment is not the query.
        assert_eq!(
            parse_portal_url("https://h/srun_portal_pc?ac_id=5#x")
                .unwrap()
                .ac_id,
            "5"
        );
        // `ac_id` present without a value is still a present key.
        assert_eq!(
            parse_portal_url("https://h/srun_portal_pc?ac_id")
                .unwrap()
                .ac_id,
            ""
        );

        // The value is decoded like `URLSearchParams`.
        assert_eq!(
            parse_portal_url("https://h/srun_portal_pc?ac_id=a%2Bb")
                .unwrap()
                .ac_id,
            "a+b"
        );
        assert_eq!(
            parse_portal_url("https://h/srun_portal_pc?ac_id=1+2")
                .unwrap()
                .ac_id,
            "1 2"
        );
    }

    #[test]
    fn parse_portal_url_rejects_required_and_invalid_inputs() {
        for blank in ["", " ", "\t\n"] {
            assert!(matches!(
                parse_portal_url(blank),
                Err(ConfigError::PortalUrlRequired)
            ));
        }
        for bad in [
            // no scheme
            "net.szu.edu.cn/srun_portal_pc?ac_id=1",
            // relative path
            "/srun_portal_pc?ac_id=1",
            // wrong path
            "https://h/other?ac_id=1",
            // no ac_id key
            "https://h/srun_portal_pc",
            "https://h/srun_portal_pc?theme=app",
            // empty authority
            "https:///srun_portal_pc?ac_id=1",
            "https://",
            // malformed / out-of-range ports
            "https://h:notaport/srun_portal_pc?ac_id=1",
            "https://h:70000/srun_portal_pc?ac_id=1",
            // scheme without `://`
            "https:net.szu.edu.cn/srun_portal_pc?ac_id=1",
        ] {
            assert!(
                matches!(parse_portal_url(bad), Err(ConfigError::PortalUrlInvalid)),
                "expected {bad:?} to be invalid"
            );
        }
    }

    #[test]
    fn url_from_env_needs_both_fields() {
        let env = CliEnv {
            auth_url: "https://net.szu.edu.cn".to_string(),
            acid: "1".to_string(),
        };
        assert_eq!(
            url_from_env(&env).as_deref(),
            Some("https://net.szu.edu.cn/srun_portal_pc?ac_id=1")
        );
        assert_eq!(url_from_env(&CliEnv::default()), None);
        assert_eq!(
            url_from_env(&CliEnv {
                auth_url: "https://net.szu.edu.cn".to_string(),
                acid: String::new(),
            }),
            None
        );
        assert_eq!(
            url_from_env(&CliEnv {
                auth_url: String::new(),
                acid: "1".to_string(),
            }),
            None
        );
    }
}
