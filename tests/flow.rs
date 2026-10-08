//! End-to-end flow against a stub portal: configuration page → online probe →
//! `get_challenge` → `login` → online re-check → DM sign-out, asserting the raw
//! query strings the wire actually carried (SPEC §13.2's differential, with the
//! stub reimplemented in std so the test needs no external artifacts).

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::json;
use srun_portal::api;
use srun_portal::portal_config;
use srun_portal::runtime::{Runtime, RuntimeOptions};
use srun_portal::transport::{self, Endpoints};

const CHALLENGE: &str = "0123456789abcdef0123456789abcdef";
const IP: &str = "10.9.9.9";
const MAC: &str = "aa:bb:cc:dd:ee:ff";

/// The captured login query (SPEC §13.2) for the stub's fixed inputs.
const EXPECTED_LOGIN_QUERY: &str = "action=login&username=testuser&password=%7BMD5%7D94401f94e5f4eecf2f52ef15389fc664&os=Linux&name=linux&double_stack=0&chksum=5eaddabca3a1435c9c45e18fa255b3a61a1397e8&info=%7BSRBX1%7D1IPGZziAV7Q51Ak77DPQy%2B3T4n3fHXviay76y9%2FgGgyBNqV0INj7BUrtgOboiAMJpLywBA%2F%2F0FJkVyPJIKGZf5HZe5M7TmAEvgrt3110qKZ2oo%2FTMdrVXahgu8%2BXAUQjxegK9v%3D%3D&ac_id=1&ip=10.9.9.9&n=200&type=0&callback=jsonp";

struct Stub {
    origin: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn start() -> Stub {
        Stub::start_with(true)
    }

    /// `login_marks_online = false` models a portal that accepts the login
    /// request but never reports the session online, which is what the SPEC §8
    /// retry path has to survive.
    fn start_with(login_marks_online: bool) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let addr = listener.local_addr().expect("stub addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let online = Arc::new(AtomicBool::new(false));
        let (log, state) = (Arc::clone(&requests), Arc::clone(&online));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (log, state) = (Arc::clone(&log), Arc::clone(&state));
                thread::spawn(move || serve(stream, log, state, login_marks_online));
            }
        });
        Stub {
            origin: format!("http://{addr}"),
            requests,
        }
    }

    fn queries(&self) -> Vec<String> {
        self.requests.lock().expect("log").clone()
    }

    /// The raw query of the last request to `path`, or `None` when absent.
    fn last_query_for(&self, path: &str) -> Option<String> {
        self.queries()
            .into_iter()
            .rfind(|line| line.starts_with(&format!("{path} ")))
            .and_then(|line| line.split_once(' ').map(|(_, query)| query.to_string()))
    }

    fn queries_for(&self, path: &str) -> Vec<String> {
        self.queries()
            .into_iter()
            .filter(|line| line.starts_with(&format!("{path} ")))
            .filter_map(|line| line.split_once(' ').map(|(_, query)| query.to_string()))
            .collect()
    }
}

fn page() -> String {
    let portal = json!({
        "AuthIP": "127.0.0.1",
        "AuthIP6": "127.0.0.1",
        "ServiceIP": "",
        "DoubleStackPC": false,
        "DoubleStackMobile": false,
        "MacAuth": false,
        "TrafficCarry": 1024,
        "AccountFilter": ""
    });
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>stub</title></head><body>\n\
         <span id=\"cliVersion\">1.9.15</span>\n\
         <span id=\"acid\">\"1\"</span>\n\
         <span id=\"ip\">\"{IP}\"</span>\n\
         <span id=\"nas\">\"stub-nas\"</span>\n\
         <span id=\"mac\">\"{MAC}\"</span>\n\
         <span id=\"lang\">\"en-US\"</span>\n\
         <span id=\"isIPv6\">false</span>\n\
         <span id=\"domain\">{{}}</span>\n\
         <span id=\"portal\">{portal}</span>\n\
         <span id=\"project\">stub</span>\n\
         <span id=\"color\">#1976D2</span>\n\
         <span id=\"useLogo\">false</span>\n\
         <span id=\"showInfoList\">username,ip</span>\n\
         </body></html>\n"
    )
}

/// The JSONP wrapper the stub answers with: the callback name the request
/// carried, echoed the way the real portal echoes it.
fn jsonp(params: &std::collections::HashMap<String, String>, value: serde_json::Value) -> String {
    let callback = params
        .get("callback")
        .map(String::as_str)
        .unwrap_or("jsonp");
    format!("{callback}({value})")
}

fn serve(
    mut stream: TcpStream,
    log: Arc<Mutex<Vec<String>>>,
    online: Arc<AtomicBool>,
    login_marks_online: bool,
) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // Drain the rest of the request head.
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) if header == "\r\n" || header == "\n" => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    log.lock().expect("log").push(format!("{path} {query}"));
    let params = parse_query(query);

    let body = match path {
        "/srun_portal_pc" => page(),
        "/cgi-bin/get_challenge" => jsonp(&params, json!({"error": "ok", "challenge": CHALLENGE})),
        "/cgi-bin/rad_user_info" => {
            let mut info = json!({
                "error": if online.load(Ordering::SeqCst) { "ok" } else { "auth" },
                "online_ip": IP,
                "sysver": "1.9.15"
            });
            if online.load(Ordering::SeqCst) {
                info["user_name"] = json!("testuser");
                info["domain"] = json!("");
                info["real_name"] = json!("Test User");
                info["user_mac"] = json!(MAC);
            }
            jsonp(&params, info)
        }
        "/cgi-bin/srun_portal" => {
            if params.get("action").map(String::as_str) == Some("login") && login_marks_online {
                online.store(true, Ordering::SeqCst);
            }
            jsonp(&params, json!({"error": "ok"}))
        }
        "/cgi-bin/rad_user_dm" => {
            online.store(false, Ordering::SeqCst);
            jsonp(&params, json!({"error": "ok"}))
        }
        "/v2/srun_portal_message" => json!({"code": 0, "data": []}).to_string(),
        "/v1/srun_portal_agree_new" => {
            json!({"code": 0, "data": {"data": {"id": 1, "title": "t", "content": "c"}}})
                .to_string()
        }
        _ => "not found".to_string(),
    };

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&value[i + 1..i + 3], 16) {
                Ok(byte) => {
                    out.push(byte);
                    i += 3;
                }
                Err(_) => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Drive the runtime the way the CLI does, stopping before the interactive loop.
fn authenticate(stub: &Stub) -> (Runtime, Vec<String>) {
    let slug = format!("{}/srun_portal_pc?ac_id=1", stub.origin);
    let portal = srun_portal::config::parse_portal_url(&slug).expect("valid slug");
    let url = format!(
        "{}{}?ac_id={}&theme=app",
        portal.origin,
        api::CONFIG_PATHNAME,
        transport::urlencode(&portal.ac_id)
    );
    let html = transport::get_text(&url).expect("config page");
    let cfg = portal_config::parse(&html).expect("config parses");
    let host = stub.origin.split_once("://").expect("origin").1.to_string();
    let mut runtime = Runtime::new(RuntimeOptions::from_config(&cfg, &portal.origin, &host));
    runtime.spawn_other_stack_probe();
    assert!(!runtime.check_online().expect("online probe"));

    runtime
        .auth_by_password("testuser", "testpass", "")
        .expect("authentication succeeds against the stub");
    (runtime, stub.queries())
}

#[test]
fn login_flow_carries_the_captured_queries() {
    let stub = Stub::start();
    let (runtime, _) = authenticate(&stub);

    assert_eq!(
        stub.last_query_for("/srun_portal_pc").as_deref(),
        Some("ac_id=1&theme=app")
    );
    assert_eq!(
        stub.last_query_for("/cgi-bin/get_challenge").as_deref(),
        Some("username=testuser&ip=10.9.9.9&callback=jsonp")
    );
    assert_eq!(
        stub.queries_for("/cgi-bin/srun_portal"),
        vec![EXPECTED_LOGIN_QUERY.to_string()]
    );
    assert_eq!(runtime.username_with_domain(), "testuser");
    assert_eq!(runtime.userinfo.username, "testuser");
    assert_eq!(runtime.api_version(), Some("15"));
    assert!(runtime.is_online);

    // The other-stack probe goes out without an `ip` parameter (SPEC §5), and it
    // reaches the stub's non-default port (SPEC §4's `url.hostname` swap).
    let probe = stub.queries_for("/cgi-bin/rad_user_info");
    assert!(
        probe.iter().any(|query| query == "callback=jsonp"),
        "expected the parameter-less probe, saw {probe:?}"
    );
}

#[test]
fn configured_callback_is_used() {
    // A `callback` from the configuration file reaches every JSONP request, and
    // the stub echoes it, so the response only parses when the name matches.
    let stub = Stub::start();
    let slug = format!("{}/srun_portal_pc?ac_id=1", stub.origin);
    let portal = srun_portal::config::parse_portal_url(&slug).expect("valid slug");
    let html = transport::get_text(&format!(
        "{}{}?ac_id={}&theme=app",
        portal.origin,
        api::CONFIG_PATHNAME,
        transport::urlencode(&portal.ac_id)
    ))
    .expect("config page");
    let cfg = portal_config::parse(&html).expect("config parses");
    let host = stub.origin.split_once("://").expect("origin").1.to_string();

    let mut options = RuntimeOptions::from_config(&cfg, &portal.origin, &host);
    options.callback = "cb".to_string();
    let mut runtime = Runtime::new(options);

    runtime
        .core_auth("testuser", "testpass", false, false)
        .expect("the stub echoes the configured callback");
    assert!(runtime.is_online);
    assert_eq!(
        stub.last_query_for("/cgi-bin/get_challenge").as_deref(),
        Some("username=testuser&ip=10.9.9.9&callback=cb")
    );
    let login = stub
        .last_query_for("/cgi-bin/srun_portal")
        .expect("login recorded");
    assert!(login.ends_with("&callback=cb"), "{login}");
}

#[test]
fn sign_out_carries_the_dm_query_with_a_consistent_sign() {
    let stub = Stub::start();
    let (mut runtime, _) = authenticate(&stub);

    runtime.sign_out().expect("sign-out succeeds");
    let query = stub
        .last_query_for("/cgi-bin/rad_user_dm")
        .expect("dm request recorded");
    let params = parse_query(&query);
    assert_eq!(params["ip"], IP);
    assert_eq!(params["username"], "testuser");
    assert_eq!(params["unbind"], "1");
    assert_eq!(params["callback"], "jsonp");
    let time: i64 = params["time"].parse().expect("integer seconds");
    assert_eq!(params["sign"], api::dm_sign("testuser", IP, time));
    assert!(!runtime.is_online);
}

#[test]
fn rejected_credentials_are_reported_and_not_retried_by_the_library() {
    // A portal that answers the login request but never reports the session
    // online: the runtime must spend its three checks a second apart, then fail
    // with `Login failed` (SPEC §8).
    let stub = Stub::start_with(false);
    let slug = format!("{}/srun_portal_pc?ac_id=1", stub.origin);
    let portal = srun_portal::config::parse_portal_url(&slug).expect("valid slug");
    let html = transport::get_text(&format!(
        "{}{}?ac_id={}&theme=app",
        portal.origin,
        api::CONFIG_PATHNAME,
        transport::urlencode(&portal.ac_id)
    ))
    .expect("config page");
    let cfg = portal_config::parse(&html).expect("config parses");
    let mut runtime = Runtime::new(RuntimeOptions::from_config(
        &cfg,
        &portal.origin,
        stub.origin.split_once("://").expect("origin").1,
    ));

    // The stub answers every attempt, so three online checks, one second apart,
    // are what the runtime performs before giving up (SPEC §8).
    let started = std::time::Instant::now();
    let err = runtime
        .core_auth("testuser", "testpass", false, false)
        .expect_err("the portal never reports the session online");
    assert_eq!(err.to_string(), "Login failed");
    assert!(
        started.elapsed().as_millis() >= 2_000,
        "expected the 1 s retry gaps"
    );
}

#[test]
fn double_stack_authentication_runs_lv4_then_v6_serially() {
    let stub = Stub::start();
    let slug = format!("{}/srun_portal_pc?ac_id=1", stub.origin);
    let portal = srun_portal::config::parse_portal_url(&slug).expect("valid slug");
    let html = transport::get_text(&format!(
        "{}{}?ac_id={}&theme=app",
        portal.origin,
        api::CONFIG_PATHNAME,
        transport::urlencode(&portal.ac_id)
    ))
    .expect("config page");
    let mut cfg = portal_config::parse(&html).expect("config parses");
    cfg.portal = srun_portal::portal_config::PortalFlags::from_map(
        match json!({
            "AuthIP": "127.0.0.1",
            "AuthIP6": "127.0.0.1",
            "DoubleStackPC": true,
            "DoubleStackMobile": false,
            "MacAuth": false,
            "AccountFilter": ""
        }) {
            serde_json::Value::Object(map) => map,
            other => panic!("flags must be an object: {other}"),
        },
    );

    let host = stub.origin.split_once("://").expect("origin").1.to_string();
    let mut runtime = Runtime::new(RuntimeOptions::from_config(&cfg, &portal.origin, &host));
    runtime.spawn_other_stack_probe();
    // The probe is fire-and-forget (SPEC §12 item 10), so wait for its result
    // rather than racing it; the stub reports `online_ip` on both stacks.
    for _ in 0..200 {
        if !runtime.user_ip_other_stack().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(runtime.user_ip_other_stack(), IP);
    assert!(runtime.enable_double_stack());

    runtime
        .auth_by_password("testuser", "testpass", "")
        .expect("both stacks authenticate");
    let logins = stub.queries_for("/cgi-bin/srun_portal");
    assert_eq!(logins.len(), 2, "one login per stack: {logins:?}");
    assert!(logins[0].contains("double_stack=0"));
    assert!(logins[1].contains("double_stack=1"), "{:?}", logins[1]);
}

#[test]
fn double_stack_sign_out_unbinds_both_stacks() {
    let stub = Stub::start();
    let slug = format!("{}/srun_portal_pc?ac_id=1", stub.origin);
    let portal = srun_portal::config::parse_portal_url(&slug).expect("valid slug");
    let html = transport::get_text(&format!(
        "{}{}?ac_id={}&theme=app",
        portal.origin,
        api::CONFIG_PATHNAME,
        transport::urlencode(&portal.ac_id)
    ))
    .expect("config page");
    let mut cfg = portal_config::parse(&html).expect("config parses");
    cfg.portal = srun_portal::portal_config::PortalFlags::from_map(
        match json!({
            "AuthIP": "127.0.0.1",
            "AuthIP6": "127.0.0.1",
            "DoubleStackPC": true,
            "DoubleStackMobile": false,
            "MacAuth": false,
            "AccountFilter": ""
        }) {
            serde_json::Value::Object(map) => map,
            other => panic!("flags must be an object: {other}"),
        },
    );

    let host = stub.origin.split_once("://").expect("origin").1.to_string();
    let mut runtime = Runtime::new(RuntimeOptions::from_config(&cfg, &portal.origin, &host));
    runtime.spawn_other_stack_probe();
    for _ in 0..200 {
        if !runtime.user_ip_other_stack().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(runtime.enable_double_stack());
    runtime
        .auth_by_password("testuser", "testpass", "")
        .expect("authentication succeeds");
    assert!(runtime.is_online);

    // SPEC §7.3: both stacks are unbound through one `promiseAny` race; the
    // sign-out check then runs once (the stub ends the session on the DM call).
    runtime.sign_out().expect("sign-out succeeds");
    let dms = stub.queries_for("/cgi-bin/rad_user_dm");
    assert_eq!(dms.len(), 2, "one DM per stack: {dms:?}");
    for query in &dms {
        let params = parse_query(query);
        assert_eq!(params["unbind"], "1");
        assert_eq!(params["username"], "testuser");
        let time: i64 = params["time"].parse().expect("integer seconds");
        assert_eq!(
            params["sign"],
            api::dm_sign("testuser", &params["ip"], time)
        );
    }
    assert!(!runtime.is_online);
}

#[test]
fn endpoints_keep_the_stub_port_for_both_stacks() {
    let endpoints = Endpoints {
        origin: "http://127.0.0.1:8899".to_string(),
        v4_host: "127.0.0.1".to_string(),
        v6_host: "127.0.0.1".to_string(),
    };
    assert_eq!(
        endpoints.url("/cgi-bin/rad_user_info", true),
        "http://127.0.0.1:8899/cgi-bin/rad_user_info"
    );
}

/// The `Target` one unattended pass runs with, against the stub.
fn target(stub: &Stub) -> srun_portal::reconnect::Target {
    let slug = format!("{}/srun_portal_pc?ac_id=1", stub.origin);
    srun_portal::reconnect::Target {
        portal: srun_portal::config::parse_portal_url(&slug).expect("valid slug"),
        username: "testuser".to_string(),
        domain: String::new(),
        callback: api::DEFAULT_CALLBACK.to_string(),
    }
}

#[test]
fn reconnect_once_logs_in_when_the_stub_is_offline() {
    let stub = Stub::start();

    let outcome = srun_portal::reconnect::reconnect_once(&target(&stub), "testpass")
        .expect("the unattended pass succeeds");

    assert_eq!(outcome, srun_portal::reconnect::Outcome::Reconnected);
    // Exactly the one login the interactive flow sends, byte for byte.
    assert_eq!(stub.queries_for("/cgi-bin/get_challenge").len(), 1);
    assert_eq!(
        stub.queries_for("/cgi-bin/srun_portal"),
        vec![EXPECTED_LOGIN_QUERY.to_string()]
    );
    assert_eq!(
        stub.last_query_for("/srun_portal_pc").as_deref(),
        Some("ac_id=1&theme=app")
    );
}

#[test]
fn reconnect_once_leaves_an_online_session_alone() {
    let stub = Stub::start();
    let (_, _) = authenticate(&stub);
    let logins = stub.queries_for("/cgi-bin/srun_portal").len();

    let outcome = srun_portal::reconnect::reconnect_once(&target(&stub), "testpass")
        .expect("the unattended pass succeeds");

    assert_eq!(outcome, srun_portal::reconnect::Outcome::AlreadyOnline);
    assert_eq!(
        stub.queries_for("/cgi-bin/srun_portal").len(),
        logins,
        "an online device sends no login"
    );
}

#[test]
fn reconnect_once_reports_a_failed_login() {
    // The portal takes the login but never reports the session online, so the
    // SPEC §8 retry path ends in `Login failed` — rendered, not reworded.
    let stub = Stub::start_with(false);

    let err = srun_portal::reconnect::reconnect_once(&target(&stub), "testpass")
        .expect_err("the portal never reports the session online");

    assert_eq!(err, "Login failed");
}
