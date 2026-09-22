//! JSONP transport, `URLSearchParams` encoding and endpoint URLs (SPEC §4, §9).
//!
//! Every non-HTML request the original client makes goes out as a JSONP GET:
//! the query string is the [`URLSearchParams`] serialisation of an ordered
//! parameter list with `callback` **last**, and the response body is
//! `jsonp({...})`, unwrapped by dropping `callback.len() + 1` leading
//! characters and the final `)`.
//!
//! [`URLSearchParams`]: https://url.spec.whatwg.org/#urlsearchparams

use std::fmt;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

/// The HTTP timeouts, in milliseconds.
///
/// The specification fixes none; these are this client's own choice, exposed so
/// that a configuration file can override them before the first request goes
/// out. [`Timeouts::default`] is the 5 s connect / 10 s read pair this crate
/// shipped with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect_ms: u64,
    pub read_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect_ms: 5_000,
            read_ms: 10_000,
        }
    }
}

/// The timeouts the shared agent was built with.
static TIMEOUTS: OnceLock<Timeouts> = OnceLock::new();

/// Fix the HTTP timeouts for this process.
///
/// The value is read when [`agent`] builds the shared agent, so this is a
/// one-shot startup call: the first call wins and a later call returns the
/// value already in force as its `Err`. Call it before the first request.
pub fn set_timeouts(timeouts: Timeouts) -> Result<(), Timeouts> {
    TIMEOUTS.set(timeouts)
}

/// The timeouts in force: [`Timeouts::default`] until [`set_timeouts`] is
/// called.
pub fn timeouts() -> Timeouts {
    TIMEOUTS.get().copied().unwrap_or_default()
}

/// `URLSearchParams` serialisation (SPEC §4).
///
/// The application/x-www-form-urlencoded serialiser keeps `A-Za-z0-9*-._`,
/// turns a space into `+`, and percent-encodes every other byte of the UTF-8
/// encoding with **uppercase** hex digits (`{`→`%7B`, `}`→`%7D`, `=`→`%3D`,
/// `+`→`%2B`, `/`→`%2F`, `~`→`%7E`, `!`→`%21`).
pub fn urlencode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// Serialise ordered key/value pairs as `URLSearchParams(data)` does: each side
/// is encoded, pairs are joined with `&`, and the **caller's order is kept**
/// (the server echoes the request order back and the differential test compares
/// raw query strings byte for byte, SPEC §4/§13.2).
pub fn query_string(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&urlencode(k));
        out.push('=');
        out.push_str(&urlencode(v));
    }
    out
}

/// The portal endpoints of one access point (SPEC §4, §9).
///
/// `origin` is `new URL(defaultAuthURL).origin`; the two host strings are
/// `config.portal.AuthIP || url.host` and `config.portal.AuthIP6 || url.host`
/// (SPEC §3.3) and therefore **may carry a port** — `url.host` includes it —
/// and are used verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub origin: String,
    pub v4_host: String,
    pub v6_host: String,
}

impl Endpoints {
    /// `authURL(pathname, otherStack)` (SPEC §4): `url.search` is cleared and
    /// the pathname is appended to the origin. With `other_stack` the host is
    /// swapped between the two stacks — only when both are known (SPEC §9);
    /// otherwise the origin authority stands.
    pub fn url(&self, pathname: &str, other_stack: bool) -> String {
        let (scheme, authority) = split_origin(&self.origin);
        let swapped = self.swapped_authority(authority, other_stack);
        let authority = swapped.as_deref().unwrap_or(authority);
        let mut out = String::with_capacity(scheme.len() + 3 + authority.len() + pathname.len());
        if !scheme.is_empty() {
            out.push_str(scheme);
            out.push_str("://");
        }
        out.push_str(authority);
        out.push_str(pathname);
        out
    }

    /// New authority for `other_stack`, or `None` when the origin authority is
    /// to be kept (SPEC §4: swap only if both hosts are non-empty).
    ///
    /// The reference assigns to `url.hostname`, so the origin's **port survives**
    /// the swap; that is observable in the captured stub traffic, where the
    /// other-stack probe still lands on the portal's non-default port even
    /// though `AuthIP` carries no port. A host string that brings its own port
    /// (which is what `url.host` yields as the `AuthIP` fallback) is used
    /// verbatim; a bracket-less IPv6 literal is rejected by the URL host parser
    /// and therefore leaves the origin authority untouched.
    fn swapped_authority(&self, authority: &str, other_stack: bool) -> Option<String> {
        if !other_stack || self.v4_host.is_empty() || self.v6_host.is_empty() {
            return None;
        }
        // The original swaps whenever the origin host is not the v4 one; an
        // origin already on the "other" stack is also routed to v4.
        let (host, port) = split_host_port(authority);
        let other = if host == self.v4_host {
            &self.v6_host
        } else {
            &self.v4_host
        };
        with_origin_port(other, port)
    }
}

/// Swap `url.hostname` for `other` while keeping the origin's port: `Some` is
/// the replacement authority, `None` means the assignment is a no-op.
fn with_origin_port(other: &str, port: &str) -> Option<String> {
    if other.starts_with('[') {
        let close = other.rfind(']')?;
        return Some(if other[close + 1..].starts_with(':') {
            other.to_string()
        } else {
            format!("{other}{port}")
        });
    }
    match other.match_indices(':').count() {
        // a plain host name: adopt it, keeping the origin's port
        0 => Some(format!("{other}{port}")),
        // `host:port`: carried verbatim
        1 => {
            let (_, candidate) = other.split_once(':').expect("one colon");
            let numeric = !candidate.is_empty() && candidate.chars().all(|c| c.is_ascii_digit());
            numeric.then(|| other.to_string())
        }
        // several colons without brackets: an IPv6 literal the URL host parser
        // rejects, so the assignment is a no-op
        _ => None,
    }
}

/// Split an authority into its host and its `:port` suffix (empty when default).
fn split_host_port(authority: &str) -> (&str, &str) {
    if let Some(close) = authority.rfind(']') {
        return (&authority[..close + 1], &authority[close + 1..]);
    }
    match authority.rfind(':') {
        Some(i) => (&authority[..i], &authority[i..]),
        None => (authority, ""),
    }
}

/// Split `scheme://authority` back into its halves; an origin without `://`
/// is treated as a bare authority.
fn split_origin(origin: &str) -> (&str, &str) {
    match origin.find("://") {
        Some(i) => (&origin[..i], &origin[i + 3..]),
        None => ("", origin),
    }
}

/// Transport-level failure.
#[derive(Debug)]
pub enum TransportError {
    /// The request itself failed (connection, status, body read).
    Http(String),
    /// The (unwrapped) body is not valid JSON.
    Json(String),
    /// The body is not `callback({...})`; holds its first 64 bytes.
    NotJsonp(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Http(msg) => write!(f, "{msg}"),
            TransportError::Json(msg) => write!(f, "Invalid JSON response: {msg}"),
            TransportError::NotJsonp(head) => {
                write!(f, "Response is not a JSONP payload: {head:?}")
            }
        }
    }
}

impl std::error::Error for TransportError {}

/// The shared HTTP agent.
///
/// The specification fixes no HTTP timeouts; this implementation picks a 5 s
/// connect timeout and a 10 s read timeout so a dead portal cannot hang the
/// authentication state machine forever. [`set_timeouts`] replaces both before
/// the first request; later calls have no effect.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
        let timeouts = timeouts();
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(timeouts.connect_ms))
            .timeout_read(Duration::from_millis(timeouts.read_ms))
            .build()
    });
    &AGENT
}

/// GET `url` and return the body as text (the HTML configuration page, §3.2).
pub fn get_text(url: &str) -> Result<String, TransportError> {
    let response = agent()
        .get(url)
        .call()
        .map_err(|e| TransportError::Http(e.to_string()))?;
    response
        .into_string()
        .map_err(|e| TransportError::Http(e.to_string()))
}

/// GET `url` as JSONP and unwrap the callback (SPEC §4).
///
/// The body must be `callback({json})`; `callback.len() + 1` leading characters
/// and the trailing `)` are dropped, then the remainder is parsed as JSON. The
/// callback name is a parameter because callers may choose a different one and
/// the server echoes it back.
pub fn jsonp(url: &str, callback: &str) -> Result<serde_json::Value, TransportError> {
    let text = get_text(url)?;
    let opener_len = callback.len() + 1;
    let shaped = text.len() > opener_len
        && text.starts_with(callback)
        && text.as_bytes()[callback.len()] == b'('
        && text.ends_with(')');
    if !shaped {
        return Err(TransportError::NotJsonp(head64(&text)));
    }
    // Both ends trimmed here are ASCII, so the inner slice stays on char
    // boundaries. The trailing character is the `)` — `text.length - 1` in the
    // original `substring` call.
    let inner = &text[opener_len..text.len() - 1];
    serde_json::from_str(inner).map_err(|e| TransportError::Json(e.to_string()))
}

/// GET an axios-style endpoint (`/v1`, `/v2`, SPEC §4) and parse the whole body
/// as JSON — these endpoints are not JSONP and have no wrapper to strip.
pub fn get_json(url: &str) -> Result<serde_json::Value, TransportError> {
    let text = get_text(url)?;
    serde_json::from_str(&text).map_err(|e| TransportError::Json(e.to_string()))
}

/// First 64 bytes of `text`, cut back to a character boundary.
fn head64(text: &str) -> String {
    let end = text.len().min(64);
    let end = (0..=end)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn urlencode_matches_urlsearchparams() {
        assert_eq!(urlencode("a b"), "a+b");
        assert_eq!(urlencode("{}"), "%7B%7D");
        assert_eq!(urlencode("="), "%3D");
        assert_eq!(urlencode("+"), "%2B");
        assert_eq!(urlencode("/"), "%2F");
        assert_eq!(urlencode("*-._"), "*-._");
        assert_eq!(urlencode("AZaz09"), "AZaz09");
        assert_eq!(urlencode("中文"), "%E4%B8%AD%E6%96%87");
        assert_eq!(urlencode("~!"), "%7E%21");
        assert_eq!(urlencode("{SRBX1}"), "%7BSRBX1%7D");
        assert_eq!(urlencode(""), "");
    }

    #[test]
    fn query_string_keeps_order_and_encodes_both_sides() {
        assert_eq!(
            query_string(&[("info".to_string(), "{SRBX1}+".to_string())]),
            "info=%7BSRBX1%7D%2B"
        );
        let pairs = vec![
            ("action".to_string(), "login".to_string()),
            ("n".to_string(), "200".to_string()),
            ("callback".to_string(), "jsonp".to_string()),
        ];
        assert_eq!(query_string(&pairs), "action=login&n=200&callback=jsonp");
        assert_eq!(query_string(&[]), "");
    }

    #[test]
    fn endpoint_urls_and_host_swap() {
        let same = Endpoints {
            origin: "https://net.szu.edu.cn".to_string(),
            v4_host: "net.szu.edu.cn".to_string(),
            v6_host: "net.szu.edu.cn".to_string(),
        };
        assert_eq!(
            same.url("/cgi-bin/rad_user_info", false),
            "https://net.szu.edu.cn/cgi-bin/rad_user_info"
        );
        assert_eq!(
            same.url("/cgi-bin/rad_user_info", true),
            "https://net.szu.edu.cn/cgi-bin/rad_user_info"
        );

        // Distinct stacks, ports included verbatim.
        let dual = Endpoints {
            origin: "https://net.szu.edu.cn".to_string(),
            v4_host: "10.0.0.1:8080".to_string(),
            v6_host: "[2001:db8::1]:8080".to_string(),
        };
        assert_eq!(
            dual.url("/cgi-bin/get_challenge", false),
            "https://net.szu.edu.cn/cgi-bin/get_challenge"
        );
        assert_eq!(
            dual.url("/cgi-bin/get_challenge", true),
            "https://10.0.0.1:8080/cgi-bin/get_challenge"
        );

        // Origin already on the other stack swaps back to v4.
        let on_v6 = Endpoints {
            origin: "https://[2001:db8::1]:8080".to_string(),
            v4_host: "10.0.0.1:8080".to_string(),
            v6_host: "[2001:db8::1]:8080".to_string(),
        };
        assert_eq!(
            on_v6.url("/srun_portal_pc", true),
            "https://10.0.0.1:8080/srun_portal_pc"
        );
        assert_eq!(
            on_v6.url("/srun_portal_pc", false),
            "https://[2001:db8::1]:8080/srun_portal_pc"
        );

        // `url.hostname` keeps the origin's port: this is the stub scenario the
        // captured traffic shows — AuthIP has no port, the probe still reaches
        // the portal's non-default port.
        let portless = Endpoints {
            origin: "http://127.0.0.1:8899".to_string(),
            v4_host: "127.0.0.1".to_string(),
            v6_host: "127.0.0.1".to_string(),
        };
        assert_eq!(
            portless.url("/cgi-bin/rad_user_info", true),
            "http://127.0.0.1:8899/cgi-bin/rad_user_info"
        );

        let v6_portless = Endpoints {
            origin: "https://net.szu.edu.cn:8443".to_string(),
            v4_host: "net.szu.edu.cn".to_string(),
            v6_host: "[2001:db8::1]".to_string(),
        };
        assert_eq!(
            v6_portless.url("/cgi-bin/rad_user_info", true),
            "https://[2001:db8::1]:8443/cgi-bin/rad_user_info"
        );

        // A bracket-less IPv6 literal is rejected by the URL host parser, so the
        // assignment changes nothing.
        let bare_v6 = Endpoints {
            origin: "https://net.szu.edu.cn".to_string(),
            v4_host: "net.szu.edu.cn".to_string(),
            v6_host: "2001:db8::1".to_string(),
        };
        assert_eq!(
            bare_v6.url("/cgi-bin/rad_user_info", true),
            "https://net.szu.edu.cn/cgi-bin/rad_user_info"
        );

        // Either host empty -> origin authority is kept (SPEC §9).
        let half = Endpoints {
            origin: "https://net.szu.edu.cn".to_string(),
            v4_host: "10.0.0.1:8080".to_string(),
            v6_host: String::new(),
        };
        assert_eq!(
            half.url("/cgi-bin/rad_user_info", true),
            "https://net.szu.edu.cn/cgi-bin/rad_user_info"
        );
        let none = Endpoints {
            origin: "http://127.0.0.1:8899".to_string(),
            v4_host: String::new(),
            v6_host: String::new(),
        };
        assert_eq!(
            none.url("/srun_portal_pc", true),
            "http://127.0.0.1:8899/srun_portal_pc"
        );

        // The pathname is appended verbatim, query and all.
        assert_eq!(
            none.url("/srun_portal_pc?ac_id=1&theme=app", false),
            "http://127.0.0.1:8899/srun_portal_pc?ac_id=1&theme=app"
        );
    }

    /// A JSONP body that is far longer than the 64-byte error preview.
    const LONG_BODY: &str = "this is not a jsonp payload at all, not even close, and it is far longer than sixty-four bytes";

    /// Minimal HTTP/1.1 server for the transport tests.
    fn serve(limit: usize) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            for _ in 0..limit {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut request = Vec::new();
                let mut chunk = [0u8; 512];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            request.extend_from_slice(&chunk[..n]);
                            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body: &str = match path.as_str() {
                    "/jsonp" => "jsonp({\"error\":\"ok\",\"challenge\":\"abc\"})",
                    "/json" => "{\"code\":0,\"token\":\"tok\",\"sign\":\"sig\"}",
                    "/plain" => "hello, not jsonp",
                    "/badjson" => "jsonp(not json)",
                    "/long" => LONG_BODY,
                    _ => "not a jsonp body",
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn jsonp_and_json_over_loopback() {
        let base = serve(8);

        let value = jsonp(&format!("{base}/jsonp"), "jsonp").expect("jsonp parses");
        assert_eq!(value["error"], "ok");
        assert_eq!(value["challenge"], "abc");

        let value = get_json(&format!("{base}/json")).expect("json parses");
        assert_eq!(value["code"], 0);
        assert_eq!(value["sign"], "sig");

        let text = get_text(&format!("{base}/jsonp")).expect("text");
        assert_eq!(text, "jsonp({\"error\":\"ok\",\"challenge\":\"abc\"})");

        // The server echoes `jsonp`, so a different callback name never matches
        // the wrapper (SPEC §4: the unwrap length follows the sent callback).
        match jsonp(&format!("{base}/jsonp"), "other") {
            Err(TransportError::NotJsonp(head)) => {
                assert_eq!(head, "jsonp({\"error\":\"ok\",\"challenge\":\"abc\"})");
            }
            other => panic!("expected NotJsonp, got {other:?}"),
        }

        // A body that is not `callback(...)` at all.
        match jsonp(&format!("{base}/plain"), "jsonp") {
            Err(TransportError::NotJsonp(head)) => assert_eq!(head, "hello, not jsonp"),
            other => panic!("expected NotJsonp, got {other:?}"),
        }

        // Unwrapping succeeds but the payload is not JSON.
        match jsonp(&format!("{base}/badjson"), "jsonp") {
            Err(TransportError::Json(_)) => {}
            other => panic!("expected Json, got {other:?}"),
        }
        // A body wrapped in some other call is not this client's JSONP.
        match jsonp(&format!("{base}/plain"), "hello, not ") {
            Err(TransportError::NotJsonp(head)) => assert_eq!(head, "hello, not jsonp"),
            other => panic!("expected NotJsonp, got {other:?}"),
        }

        // `NotJsonp` keeps at most the first 64 bytes.
        let base = serve(2);
        match jsonp(&format!("{base}/long"), "jsonp") {
            Err(TransportError::NotJsonp(head)) => {
                assert_eq!(head.len(), 64);
                assert_eq!(head, LONG_BODY[..64]);
            }
            other => panic!("expected NotJsonp, got {other:?}"),
        }
    }
}
