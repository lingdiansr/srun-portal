//! The SPEC §13.1 vector table and the SPEC §13.2 byte-exact query comparison.
//!
//! Every literal in this file was captured from the original binary, so these
//! tests fail the moment the request construction drifts by one byte.

use srun_portal::api::{self, LoginParams};
use srun_portal::crypto::{
    atob, btoa, encode_user_info, hmac_md5_hex, sha1_hex, xdecode, xencode, SRUN_ALPHABET,
    STANDARD_ALPHABET,
};
use srun_portal::transport::{query_string, urlencode};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const USERNAME: &str = "testuser";
const PASSWORD: &str = "testpass";
const IP: &str = "10.9.9.9";
const ACID: &str = "1";

/// SPEC §13.1: `{MD5}` part of the `password` field.
const CAPTURED_PASSWORD: &str = "94401f94e5f4eecf2f52ef15389fc664";
/// SPEC §13.1: `info` field.
const CAPTURED_INFO: &str = "{SRBX1}1IPGZziAV7Q51Ak77DPQy+3T4n3fHXviay76y9/gGgyBNqV0INj7BUrtgOboiAMJpLywBA//0FJkVyPJIKGZf5HZe5M7TmAEvgrt3110qKZ2oo/TMdrVXahgu8+XAUQjxegK9v==";
/// SPEC §13.1: `chksum` field.
const CAPTURED_CHKSUM: &str = "5eaddabca3a1435c9c45e18fa255b3a61a1397e8";
/// SPEC §13.1: `rad_user_dm.sign` at `time=1789819586`; the differential capture
/// uses the same construction at `time=1789820230`.
const CAPTURED_SIGN: &str = "061d82a8c262f51ecc1a07903027fdce0d8d11f6";
const CAPTURED_DM_SIGN: &str = "2b8fd1aaad462e1760a2b227732003652d9c17a6";

/// SPEC §13.2: the raw query the original binary sent to the stub portal.
const CAPTURED_LOGIN_QUERY: &str = "action=login&username=testuser&password=%7BMD5%7D94401f94e5f4eecf2f52ef15389fc664&os=Linux&name=linux&double_stack=0&chksum=5eaddabca3a1435c9c45e18fa255b3a61a1397e8&info=%7BSRBX1%7D1IPGZziAV7Q51Ak77DPQy%2B3T4n3fHXviay76y9%2FgGgyBNqV0INj7BUrtgOboiAMJpLywBA%2F%2F0FJkVyPJIKGZf5HZe5M7TmAEvgrt3110qKZ2oo%2FTMdrVXahgu8%2BXAUQjxegK9v%3D%3D&ac_id=1&ip=10.9.9.9&n=200&type=0&callback=jsonp";
/// SPEC §13.2: the raw query of the captured DM sign-out.
const CAPTURED_DM_QUERY: &str = "ip=10.9.9.9&username=testuser&time=1789820230&unbind=1&sign=2b8fd1aaad462e1760a2b227732003652d9c17a6&callback=jsonp";
/// SPEC §13.2: the configuration page request the original sent (SPEC §3.2 order).
const CAPTURED_CONFIG_QUERY: &str = "ac_id=1&theme=app";

fn login_params<'a>(token: &'a str, acid: &'a str) -> LoginParams<'a> {
    LoginParams {
        username_with_domain: USERNAME,
        password: PASSWORD,
        ip: IP,
        acid,
        token,
        n: api::LOGIN_N,
        client_type: api::CLIENT_TYPE,
        other_stack: false,
        enable_double_stack: false,
        os: "Linux",
        name: "linux",
        is_otp: false,
        callback: api::DEFAULT_CALLBACK,
    }
}

#[test]
fn password_field_matches_capture() {
    assert_eq!(hmac_md5_hex(PASSWORD, TOKEN), CAPTURED_PASSWORD);
}

#[test]
fn info_field_matches_capture() {
    assert_eq!(
        encode_user_info(USERNAME, PASSWORD, IP, ACID, TOKEN),
        CAPTURED_INFO
    );
}

#[test]
fn chksum_matches_capture() {
    let request = api::build_login(&login_params(TOKEN, ACID));
    assert_eq!(request.hmac_password, CAPTURED_PASSWORD);
    assert_eq!(request.info, CAPTURED_INFO);
    assert_eq!(request.chksum, CAPTURED_CHKSUM);
}

#[test]
fn signout_sign_matches_capture() {
    assert_eq!(api::dm_sign(USERNAME, IP, 1_789_819_586), CAPTURED_SIGN);
}

#[test]
fn login_query_is_byte_identical() {
    let request = api::build_login(&login_params(TOKEN, ACID));
    assert_eq!(request.query, CAPTURED_LOGIN_QUERY);
    // The field order is part of the contract, not just the values.
    let names: Vec<&str> = request.pairs.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        vec![
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
        ]
    );
}

#[test]
fn signout_query_is_byte_identical() {
    let pairs = api::dm_pairs(USERNAME, IP, 1_789_820_230, api::DEFAULT_CALLBACK);
    assert_eq!(query_string(&pairs), CAPTURED_DM_QUERY);
    assert!(CAPTURED_DM_SIGN == api::dm_sign(USERNAME, IP, 1_789_820_230));
}

#[test]
fn config_page_query_is_byte_identical() {
    let pairs = vec![
        ("ac_id".to_string(), ACID.to_string()),
        ("theme".to_string(), "app".to_string()),
    ];
    assert_eq!(query_string(&pairs), CAPTURED_CONFIG_QUERY);
}

#[test]
fn jsonp_bodies_escape_the_way_urlsearchparams_does() {
    // The captured login query is the oracle for the escaping of the payload.
    let escaped_info = urlencode(CAPTURED_INFO);
    assert!(
        CAPTURED_LOGIN_QUERY.contains(&escaped_info),
        "{escaped_info}"
    );
    assert_eq!(
        urlencode(&format!("{{MD5}}{CAPTURED_PASSWORD}")),
        "%7BMD5%7D94401f94e5f4eecf2f52ef15389fc664"
    );
}

#[test]
fn variant_base64_matches_standard_base64() {
    for probe in [
        &b""[..],
        b"a",
        b"ab",
        b"abc",
        b"\x00\x01\x02\xfe\xffabc",
        b"the quick brown fox jumps over the lazy dog",
    ] {
        assert_eq!(
            btoa(probe, STANDARD_ALPHABET),
            reference_base64(probe),
            "probe {probe:?}"
        );
        assert_eq!(
            atob(&btoa(probe, STANDARD_ALPHABET), STANDARD_ALPHABET).unwrap(),
            probe
        );
        let srun = btoa(probe, SRUN_ALPHABET);
        assert_eq!(atob(&srun, SRUN_ALPHABET).unwrap(), probe);
        assert_eq!(srun.len(), std_base64_len(probe));
    }
}

#[test]
fn xencode_round_trips() {
    for text in ["", "a", "hello world", TOKEN] {
        let blob = xencode(text, TOKEN);
        assert_eq!(xdecode(&blob, TOKEN).unwrap(), text, "round trip {text:?}");
    }
    assert!(xencode("", TOKEN).is_empty());
}

#[test]
fn xencode_is_lossy_for_non_ascii_payloads() {
    // SPEC §6.2/§12 item 3: `s()` consumes UTF-16 code units and `l()` emits
    // their bytes, so only the ASCII range survives a round trip. This pins the
    // documented lossiness instead of pretending it does not exist.
    let decoded = xdecode(&xencode("hello 中", TOKEN), TOKEN).unwrap();
    assert_ne!(decoded, "hello 中");
    assert_eq!(decoded.chars().count(), "hello 中".encode_utf16().count());
}

#[test]
fn sha1_is_hex_lowercase() {
    assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
}

fn reference_base64(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn std_base64_len(input: &[u8]) -> usize {
    input.len().div_ceil(3) * 4
}
