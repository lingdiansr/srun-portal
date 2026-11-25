//! End-to-end flow against a stub portal: configuration page → online probe →
//! `get_challenge` → `login` → online re-check → DM sign-out, asserting the raw
//! query strings the wire actually carried (SPEC §13.2's differential, with the
//! stub reimplemented in std so the test needs no external artifacts).

mod support;

use serde_json::json;
use srun_portal::api;
use srun_portal::control::Outcome as PassOutcome;
use srun_portal::portal_config;
use srun_portal::runtime::{Runtime, RuntimeOptions};
use srun_portal::transport::{self, Endpoints};
use support::{Behaviour, Stub, IP};

/// The captured login query (SPEC §13.2) for the stub's fixed inputs.
///
/// `os`/`name` are the running platform's `os.type()`/`os.platform()` (see
/// `runtime::Runtime::new`), so the vector differs on each platform and the
/// rest of the query is what the capture pins down.
fn expected_login_query() -> String {
    format!(
        "action=login&username=testuser&password=%7BMD5%7D94401f94e5f4eecf2f52ef15389fc664\
         &os={}&name={}&double_stack=0\
         &chksum=5eaddabca3a1435c9c45e18fa255b3a61a1397e8\
         &info=%7BSRBX1%7D1IPGZziAV7Q51Ak77DPQy%2B3T4n3fHXviay76y9%2FgGgyBNqV0INj7BUrtgOboiAMJpLywBA%2F%2F0FJkVyPJIKGZf5HZe5M7TmAEvgrt3110qKZ2oo%2FTMdrVXahgu8%2BXAUQjxegK9v%3D%3D\
         &ac_id=1&ip=10.9.9.9&n=200&type=0&callback=jsonp",
        srun_portal::sys::os_type(),
        srun_portal::sys::platform(),
    )
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    support::parse_query(query)
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
    let stub = Stub::start(Behaviour::offline());
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
        vec![expected_login_query()]
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
    let stub = Stub::start(Behaviour::offline());
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
    let stub = Stub::start(Behaviour::offline());
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
    let stub = Stub::start(Behaviour {
        login_marks_online: false,
        ..Behaviour::offline()
    });
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
    let stub = Stub::start(Behaviour::offline());
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
    let stub = Stub::start(Behaviour::offline());
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
    let stub = Stub::start(Behaviour::offline());

    let outcome = srun_portal::reconnect::reconnect_once(&target(&stub), "testpass")
        .expect("the unattended pass succeeds");

    assert_eq!(outcome, PassOutcome::Reconnected);
    // Exactly the one login the interactive flow sends, byte for byte.
    assert_eq!(stub.queries_for("/cgi-bin/get_challenge").len(), 1);
    assert_eq!(
        stub.queries_for("/cgi-bin/srun_portal"),
        vec![expected_login_query()]
    );
    assert_eq!(
        stub.last_query_for("/srun_portal_pc").as_deref(),
        Some("ac_id=1&theme=app")
    );
}

#[test]
fn reconnect_once_leaves_an_online_session_alone() {
    let stub = Stub::start(Behaviour::offline());
    let (_, _) = authenticate(&stub);
    let logins = stub.queries_for("/cgi-bin/srun_portal").len();

    let outcome = srun_portal::reconnect::reconnect_once(&target(&stub), "testpass")
        .expect("the unattended pass succeeds");

    assert_eq!(outcome, PassOutcome::Online);
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
    let stub = Stub::start(Behaviour {
        login_marks_online: false,
        ..Behaviour::offline()
    });

    let err = srun_portal::reconnect::reconnect_once(&target(&stub), "testpass")
        .expect_err("the portal never reports the session online");

    assert_eq!(err.message, "Login failed");
    assert_eq!(err.kind, srun_portal::control::ErrorKind::Authentication);
}
