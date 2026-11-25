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
use support::{Behaviour, Stub, TempDir, IP};

const BIN: &str = env!("CARGO_BIN_EXE_srun-portal");

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

// ---------------------------------------------------------------------------
// `status`: the read-only query, driven through the real binary
// ---------------------------------------------------------------------------
//
// Every case starts the binary the way a GUI would: an explicit configuration,
// a state directory that does not exist yet, no password in the environment and
// closed stdin. The assertions are about what a GUI consumes — the exit code,
// the one stdout line, an empty stderr — and about what the query did *not* do:
// it writes no file and sends only its two reads.

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// One isolated `status` run: its own configuration file, state directory and
/// stub. `config_body` is written verbatim, so a test can pin what it read.
struct StatusRun {
    dir: TempDir,
    stub: Stub,
    config: std::path::PathBuf,
}

impl StatusRun {
    fn new(tag: &str, behaviour: Behaviour, config_body: &str) -> StatusRun {
        let dir = TempDir::new(tag);
        let stub = Stub::start(behaviour);
        let config = dir.join("config.toml");
        std::fs::write(&config, config_body).expect("config file");
        StatusRun { dir, stub, config }
    }

    fn state(&self) -> std::path::PathBuf {
        self.dir.join("state")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new(BIN)
            .args(args)
            .env("SRUN_PORTAL_CONFIG", &self.config)
            .env("SRUN_PORTAL_STATE_DIR", self.state())
            .env_remove("SRUN_PORTAL_PASSWORD")
            .stdin(std::process::Stdio::null())
            .output()
            .expect("the binary runs")
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(&self.config).expect("config still readable")
    }

    /// Write a configuration that only names this stub's portal, so a case does
    /// not have to build the body it does not care about.
    fn point_at_stub(&self) {
        std::fs::write(
            &self.config,
            format!(
                "portal_url = \"{}/srun_portal_pc?ac_id=1\"\n",
                self.stub.origin
            ),
        )
        .expect("config file");
    }
}

fn online_answer() -> serde_json::Value {
    json!({
        "error": "ok",
        "user_name": "reader",
        "domain": "campus",
        "online_ip": "10.9.9.9",
        "sum_bytes": 1_610_612_736_i64,
        "sum_seconds": 3_661_i64,
        "user_balance": 7_i64,
        "products_name": "学生套餐",
        "real_name": "not exported"
    })
}

#[test]
fn cli_status_online_reports_the_six_fields_and_honours_its_overrides() {
    // The file points at a different `ac_id` and callback than the run uses, so a
    // request that ignored the overrides would be visible in the log.
    let run = StatusRun::new(
        "status-online",
        Behaviour {
            rad_user_info_override: Some(online_answer()),
            ..Behaviour::default()
        },
        "",
    );
    let body = format!(
        "portal_url = \"{}/srun_portal_pc?ac_id=99\"\ncallback = \"status_cb\"\ntimeout_ms = 2000\n",
        run.stub.origin
    );
    std::fs::write(&run.config, &body).expect("config file");
    let before = run.config_text();

    let override_url = format!(
        "{}/srun_portal_success?ac_id=12&theme=proyx",
        run.stub.origin
    );
    let output = run.run(&[
        "status",
        "--json",
        "--portal-url",
        &override_url,
        "--timeout-ms",
        "3000",
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    let text = stdout_of(&output);
    assert_eq!(text.lines().count(), 1, "one JSON line, got {text:?}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(text.trim_end()).expect("stdout is JSON"),
        json!({
            "schema_version": 1,
            "result": "online",
            "account": {
                "username": "reader@campus",
                "ip": "10.9.9.9",
                "used_flow_bytes": 1_610_612_736_i64,
                "used_time_seconds": 3_661_i64,
                "balance": 7,
                "product_name": "学生套餐"
            }
        })
    );
    assert_eq!(
        run.stub.queries(),
        vec![
            "/srun_portal_pc ac_id=12&theme=app".to_string(),
            "/cgi-bin/rad_user_info ip=10.9.9.9&callback=status_cb".to_string(),
        ]
    );
    assert_eq!(
        run.config_text(),
        before,
        "status must not write the config"
    );
    assert!(
        !run.state().exists(),
        "status must not create the state dir"
    );

    // The same query without `--json`: the panel, and no exit prompt.
    let output = run.run(&[
        "status",
        "--portal-url",
        &override_url,
        "--timeout-ms",
        "3000",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    let text = stdout_of(&output);
    for expected in ["reader@campus", "1.50 GB", "1小时1分1秒", "¥ 7", "学生套餐"] {
        assert!(text.contains(expected), "{expected} missing from {text:?}");
    }
    assert!(
        !text.to_lowercase().contains("sign out"),
        "status must not ask to sign out: {text:?}"
    );
    // Both runs produced the same two requests, and nothing else did.
    assert_eq!(
        run.stub.queries(),
        vec![
            "/srun_portal_pc ac_id=12&theme=app".to_string(),
            "/cgi-bin/rad_user_info ip=10.9.9.9&callback=status_cb".to_string(),
            "/srun_portal_pc ac_id=12&theme=app".to_string(),
            "/cgi-bin/rad_user_info ip=10.9.9.9&callback=status_cb".to_string(),
        ]
    );
}

#[test]
fn cli_status_offline_is_a_result_and_a_bad_answer_is_a_failure() {
    // Offline: the portal's own "not this device" answer, exit 0, no account.
    let run = StatusRun::new(
        "status-offline",
        Behaviour {
            rad_user_info_override: Some(json!({"error": "not_online_error"})),
            ..Behaviour::default()
        },
        "",
    );
    run.point_at_stub();
    let output = run.run(&["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr_of(&output));
    assert_eq!(stderr_of(&output), "");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(stdout_of(&output).trim_end())
            .expect("stdout is JSON"),
        json!({"schema_version": 1, "result": "offline", "account": null})
    );
    assert_eq!(
        run.stub.queries(),
        vec![
            "/srun_portal_pc ac_id=1&theme=app".to_string(),
            "/cgi-bin/rad_user_info ip=10.9.9.9&callback=jsonp".to_string(),
        ]
    );
    assert!(!run.state().exists());

    // Any other answer is a failed query, not an offline device: exit 4, no
    // stdout, a diagnostic on stderr.
    for answer in [json!({"error": "database_unavailable"}), json!({})] {
        let run = StatusRun::new(
            "status-failure",
            Behaviour {
                rad_user_info_override: Some(answer.clone()),
                ..Behaviour::default()
            },
            "",
        );
        run.point_at_stub();
        let output = run.run(&["status", "--json"]);
        assert_eq!(
            output.status.code(),
            Some(4),
            "{answer} should fail: {}",
            stderr_of(&output)
        );
        assert_eq!(stdout_of(&output), "", "{answer} must print nothing");
        assert!(
            !stderr_of(&output).is_empty(),
            "{answer} needs a diagnostic"
        );
        assert_eq!(
            run.stub.queries(),
            vec![
                "/srun_portal_pc ac_id=1&theme=app".to_string(),
                "/cgi-bin/rad_user_info ip=10.9.9.9&callback=jsonp".to_string(),
            ],
            "{answer}"
        );
        assert!(!run.state().exists(), "{answer}");
    }
}

#[test]
fn cli_status_rejects_bad_options_before_any_request() {
    // A configuration path that does not exist and no URL on the command line:
    // nothing to query, so exit 2 and no request.
    let dir = TempDir::new("status-noconfig");
    let missing = dir.join("absent.toml");
    let state = dir.join("state");
    let output = std::process::Command::new(BIN)
        .args(["status", "--json"])
        .env("SRUN_PORTAL_CONFIG", &missing)
        .env("SRUN_PORTAL_STATE_DIR", &state)
        .env_remove("SRUN_PORTAL_PASSWORD")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "");
    assert!(
        stderr_of(&output).contains(srun_portal::messages::PORTAL_URL_REQUIRED),
        "{}",
        stderr_of(&output)
    );
    assert!(
        !missing.exists(),
        "status must not create the configuration"
    );
    assert!(!state.exists(), "status must not create the state dir");

    // A URL that resolves, with a timeout the configuration rule rejects: the
    // run stops at validation, before the portal sees anything.
    let run = StatusRun::new("status-timeout", Behaviour::default(), "");
    run.point_at_stub();
    let output = run.run(&[
        "status",
        "--json",
        "--portal-url",
        &format!("{}/srun_portal_pc?ac_id=1", run.stub.origin),
        "--timeout-ms",
        "0",
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    assert_eq!(stdout_of(&output), "");
    assert!(
        stderr_of(&output).contains("timeout_ms"),
        "{}",
        stderr_of(&output)
    );
    assert_eq!(run.stub.request_count(), 0, "no request may be sent");
    assert!(!run.state().exists());
}
