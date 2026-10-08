//! Read-only live checks against a real portal (SPEC §13.3): fetch the page,
//! report the decoded configuration, and ask the portal who is online. No
//! credentials, no login, no sign-out.
//!
//! ```text
//! cargo run --example live_check -- [portal-url] [username]
//! ```

use srun_portal::{
    api, config, portal_config, runtime::Runtime, runtime::RuntimeOptions, settings, transport,
};

const DEFAULT_SLUG: &str = "https://net.szu.edu.cn/srun_portal_pc?ac_id=1";

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let slug = match argv.first() {
        Some(slug) => slug.clone(),
        None => configured_slug().unwrap_or_else(|| DEFAULT_SLUG.to_string()),
    };
    println!("slug         : {slug}");

    let portal = match config::parse_portal_url(&slug) {
        Ok(portal) => portal,
        Err(err) => die(&err.to_string()),
    };
    let url = format!(
        "{}{}?ac_id={}&theme=app",
        portal.origin,
        api::CONFIG_PATHNAME,
        transport::urlencode(&portal.ac_id)
    );
    let html = match transport::get_text(&url) {
        Ok(html) => html,
        Err(err) => die(&format!("Get portal config error: {err}")),
    };
    let cfg = match portal_config::parse(&html) {
        Ok(cfg) => cfg,
        Err(err) => die(&err.to_string()),
    };

    println!("origin       : {}", portal.origin);
    println!("acid         : {}", cfg.acid);
    println!("ip (server)  : {}", cfg.ip);
    println!("mac          : {}", cfg.mac);
    println!("lang         : {}", cfg.lang);
    println!("isIPv6       : {}", cfg.is_ipv6);
    println!("DoubleStackPC: {}", cfg.portal.double_stack_pc());
    println!("MacAuth      : {}", cfg.portal.mac_auth());
    println!(
        "AuthIP/AuthIP6: {} / {}",
        cfg.portal.auth_ip(),
        cfg.portal.auth_ip6()
    );
    println!("AccountFilter: {:?}", cfg.portal.account_filter());

    let host = portal
        .origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .to_string();
    let mut runtime = Runtime::new(RuntimeOptions::from_config(&cfg, &portal.origin, &host));

    if let Some(user) = argv.get(1) {
        match runtime.get_challenge(user, &cfg.ip, false) {
            Ok(challenge) => println!(
                "get_challenge: {} chars (expire={:?})",
                challenge.token.len(),
                challenge.expire_secs
            ),
            Err(err) => println!("get_challenge failed: {err}"),
        }
    }

    match runtime.get_user_info(Some(&cfg.ip)).cloned() {
        Ok(info) => {
            let sysver = match info.raw.get("sysver") {
                Some(value) => value.to_string(),
                None => "-".to_string(),
            };
            println!(
                "rad_user_info: error=\"{}\" user=\"{}\" online_ip=\"{}\" sysver={} sum_bytes={} apiVersion={}",
                if runtime.is_online { "ok" } else { "auth" },
                info.username,
                info.online_ip,
                sysver,
                info.used_flow,
                runtime.api_version().unwrap_or("-"),
            );
        }
        Err(err) => println!("rad_user_info failed: {err}"),
    }
}

/// The configured `portal_url`, without creating, writing or asking for
/// anything: this example is read-only (SPEC §13.3).
fn configured_slug() -> Option<String> {
    let paths = settings::Paths::discover().ok()?;
    let explicit = std::env::var_os(settings::ENV_CONFIG_PATH).map(std::path::PathBuf::from);
    settings::load_existing(&paths, explicit.as_deref())
        .ok()?
        .portal_url
}

fn die(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}
