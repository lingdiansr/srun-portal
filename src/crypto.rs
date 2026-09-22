//! The portal's cryptographic primitives (SPEC §6).
//!
//! Everything here exists to produce three wire values byte-identically to the original
//! client: the `password` field (§6.3 HMAC-MD5, see [`hmac_md5_hex`]), the `info` field
//! (§6.4, see [`encode_user_info`]) and the `chksum` / `rad_user_dm.sign` fields (§6.3
//! SHA-1, see [`sha1_hex`]).
//!
//! Fidelity notes carried by SPEC:
//!
//! * §6.2 — `XEncode` is a **variant of XXTEA** defined inside `encodeUserInfo`, with its
//!   own byte packing: the payload is built from **UTF-16 code units** (`charCodeAt`), not
//!   from UTF-8 bytes.
//! * §12 item 3 — because of that packing, payloads containing non-ASCII are lossy: see the
//!   [`xencode`] / [`xdecode`] docs.
//! * §12 item 13 — `encode('') === ''`; an empty encode input leaves the `{SRBX1}` prefix
//!   standing alone. See [`encode_user_info`].
//! * §6.1 — the variant base64 keeps js-base64's odd quarter-step loop; padding is an
//!   emergent property of it, so it is reproduced rather than special-cased.

use md5::{Digest, Md5};
use sha1::Sha1;

/// SPEC §2 `SRUN_B64_ALPHABET`: the 64-character alphabet used for the `info` field.
pub const SRUN_ALPHABET: &str = "LVoJPiCN2R8G90yg+hmFHuacZ1OWMnrsSTXkYpUq/3dlbfKwv6xztjI7DeBE45QA";

/// The standard base64 alphabet, padding character included (js-base64's own `b64chars`).
///
/// Not used on the wire by the portal; it is the alphabet the §6.1 comparison against
/// standard base64 is written against.
pub const STANDARD_ALPHABET: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";

/// SPEC §2 `DELTA`: the XXTEA round constant (source writes `0x86014019 | 0x183639A0`).
pub const DELTA: u32 = 0x9E37_79B9;

/// SPEC §6.4: the constant `enc_ver` value, always packed into the `info` payload.
pub const ENC_VER: &str = "srun_bx1";

/// Failures the §6 primitives can report.
///
/// `BadLength` mirrors the `len % 4 === 1` guard of §6.1 / the `Custom a2b error` throw;
/// `BadPadding` mirrors `l(a, b)` returning `null` when the trailing length word is out of
/// range (§6.2).
#[derive(Debug, PartialEq, Eq)]
pub enum CryptoError {
    BadLength,
    BadPadding,
}

/// SPEC §6.1 `customBase64.btoa(input, alphabet)`.
///
/// The loop is transcribed, not re-derived: the index advances by `3/4` per emitted
/// character, the bounds test happens **before** each step, a past-the-end read yields `0`,
/// and once the index is past the end the mapping table degrades to `=` — so the padding
/// characters are produced by the loop itself rather than appended afterwards. Block
/// arithmetic keeps u32 wrapping semantics (`block = (block << 8) | code`,
/// `map[(block >> (8 - (idx % 1) * 8)) & 63]`).
///
/// The running index is held as an integer count of quarter steps (`idx == quarter / 4`):
/// additions of `0.75` are exact in binary floating point, so this is the same sequence of
/// indices the JavaScript performs, without floating-point arithmetic.
///
/// With [`STANDARD_ALPHABET`] the output is byte-identical to standard base64 for every
/// input length, which the tests check against an independent reference encoder.
pub fn btoa(input: &[u8], alphabet: &str) -> String {
    let len = input.len();
    let mut out = String::with_capacity((len + 2) / 3 * 4 + 4);
    let mut quarter: u64 = 0;
    let mut block: u32 = 0;
    // Once the index is past the end the table is `=` from then on; the loop still has to
    // run until the index is integral (`idx % 1 == 0`) because that is how many padding
    // characters are emitted.
    let mut past_end = false;
    while (quarter >> 2) < len as u64 || quarter & 3 != 0 {
        if (quarter >> 2) >= len as u64 {
            past_end = true;
        }
        quarter += 3;
        let code = input.get((quarter >> 2) as usize).copied().unwrap_or(0) as u32;
        block = block.wrapping_shl(8) | code;
        let shift = 8 - (quarter & 3) as u32 * 2;
        if past_end {
            out.push('=');
        } else if let Some(c) = alphabet.chars().nth(((block >> shift) & 63) as usize) {
            // `map.charAt(i)` past the end of a short alphabet yields '' — nothing appended.
            out.push(c);
        }
    }
    out
}

/// SPEC §6.1 `customBase64.atob(input, alphabet)`, the inverse of [`btoa`].
///
/// Trailing `=` are stripped, `len % 4 == 1` is rejected (`BadLength`), and decoding stops
/// at the first symbol the alphabet does not contain (the source's `indexOf` returning `-1`
/// is what terminates its loop).
///
/// Only the tests use this: the runtime encodes the `info` payload and never decodes
/// anything.
pub fn atob(input: &str, alphabet: &str) -> Result<Vec<u8>, CryptoError> {
    let stripped = input.trim_end_matches('=');
    if stripped.chars().count() % 4 == 1 {
        return Err(CryptoError::BadLength);
    }
    let mut values: Vec<u8> = Vec::with_capacity(stripped.len());
    for ch in stripped.chars() {
        match alphabet.chars().position(|c| c == ch) {
            Some(i) => values.push(i as u8),
            None => break,
        }
    }
    let mut out = Vec::with_capacity(values.len() / 4 * 3);
    for group in values.chunks(4) {
        if group.len() < 2 {
            break;
        }
        let g0 = group[0] as u32;
        let g1 = group[1] as u32;
        out.push((((g0 << 2) | (g1 >> 4)) & 0xFF) as u8);
        if group.len() >= 3 {
            let g2 = group[2] as u32;
            out.push(((((g1 & 0x0F) << 4) | (g2 >> 2)) & 0xFF) as u8);
            if group.len() >= 4 {
                out.push(((((g2 & 0x03) << 6) | group[3] as u32) & 0xFF) as u8);
            }
        }
    }
    Ok(out)
}

/// SPEC §6.2 `s(a, b)`: pack a string into u32 words, four UTF-16 code units per word,
/// little-endian, slots past the end of the string reading as `0`.
///
/// `with_length` is `b`: `true` appends the code-unit count of the whole string as one extra
/// word (the payload path), `false` is the key path.
fn to_words(s: &str, with_length: bool) -> Vec<u32> {
    let mut v = Vec::with_capacity(s.len() / 4 + 2);
    let mut acc = [0u32; 4];
    let mut filled = 0usize;
    let mut units = 0u32;
    for unit in s.encode_utf16() {
        acc[filled] = unit as u32;
        filled += 1;
        units += 1;
        if filled == 4 {
            v.push(pack_word(&acc));
            acc = [0; 4];
            filled = 0;
        }
    }
    if filled != 0 {
        v.push(pack_word(&acc));
    }
    if with_length {
        v.push(units);
    }
    v
}

/// `a[i] | a[i+1] << 8 | a[i+2] << 16 | a[i+3] << 24` under JS shift semantics.
#[inline]
fn pack_word(units: &[u32; 4]) -> u32 {
    units[0]
        | units[1].wrapping_shl(8)
        | units[2].wrapping_shl(16)
        | units[3].wrapping_shl(24)
}

/// SPEC §6.2 `l(a, b)` with `b == false`: four little-endian bytes per word.
fn words_to_bytes(v: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for w in v {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

/// The round function of SPEC §6.2:
/// `m = (z >>> 5) ^ (y << 2); m += ((y >>> 3) ^ (z << 4)) ^ (d ^ y); m += k ^ z`.
///
/// `k` is the already-selected key word. Every operation wraps at 32 bits.
#[inline]
fn mx(z: u32, y: u32, d: u32, k: u32) -> u32 {
    let mut m = (z >> 5) ^ y.wrapping_shl(2);
    m = m.wrapping_add(((y >> 3) ^ z.wrapping_shl(4)) ^ (d ^ y));
    m.wrapping_add(k ^ z)
}

/// SPEC §6.2 `encodeUserInfo`'s `encode(str, key)`.
///
/// Returns the `l(v, false)` byte stream: the ciphertext words, each as four little-endian
/// bytes, **including** the trailing length word that `s(str, true)` appended. An empty
/// `text` returns an empty vector (§12 item 13).
///
/// §12 item 3: the payload is built from UTF-16 code units and expanded back to one byte per
/// code-unit byte, so it is *not* a UTF-8 encoding of `text`. A code unit above `0xFF`
/// occupies two (or more) of those bytes instead of its UTF-8 sequence, which is why a text
/// containing non-ASCII does not survive [`xdecode`] — the portal's own encoder has the same
/// defect and the server sees the same bytes.
pub fn xencode(text: &str, key: &str) -> Vec<u8> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut v = to_words(text, true);
    let mut k = to_words(key, false);
    while k.len() < 4 {
        // `k.length = 4`: missing slots are `undefined`, which XORs as 0.
        k.push(0);
    }
    let n = v.len() - 1;
    let mut z = v[n];
    let mut d: u32 = 0;
    let mut q = 6 + 52 / (n as u32 + 1);
    while q > 0 {
        q -= 1;
        d = d.wrapping_add(DELTA);
        let e = (d >> 2) & 3;
        for p in 0..n {
            // `y` is assigned before every use, so it carries no state between rounds.
            let y = v[p + 1];
            z = v[p].wrapping_add(mx(z, y, d, k[(((p as u32) & 3) ^ e) as usize]));
            v[p] = z;
        }
        // The round's closing block uses `p == n` (SPEC §6.2 note).
        let y = v[0];
        z = v[n].wrapping_add(mx(z, y, d, k[(((n as u32) & 3) ^ e) as usize]));
        v[n] = z;
    }
    words_to_bytes(&v)
}

/// The inverse of [`xencode`] — the same XXTEA variant run backwards, followed by SPEC §6.2
/// `l(a, b)` with `b == true`.
///
/// The rounds are undone in reverse (the closing `p == n` block first, then the descending
/// loop), each with the round constant `d` rewound by `DELTA`. The trailing word is then
/// read as the message length: it must satisfy `c - 3 <= m <= c` with `c = (len - 1) << 2`,
/// otherwise `BadPadding` (the source's `l()` returns `null` there).
///
/// The remaining bytes are read back one byte per UTF-16 code unit, mirroring `l()`'s
/// `String.fromCharCode`; for a payload that came from non-ASCII input this is the lossy step
/// described by §12 item 3.
pub fn xdecode(blob: &[u8], key: &str) -> Result<String, CryptoError> {
    if blob.is_empty() {
        return Ok(String::new());
    }
    if blob.len() % 4 != 0 {
        return Err(CryptoError::BadLength);
    }
    let mut v: Vec<u32> = blob
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if v.len() < 2 {
        // A message always contributes at least one payload word plus the length word, so a
        // single-word blob has no length word to validate.
        return Err(CryptoError::BadPadding);
    }
    let mut k = to_words(key, false);
    while k.len() < 4 {
        k.push(0);
    }
    let n = v.len() - 1;
    let mut q = 6 + 52 / (n as u32 + 1);
    // SPEC §6.2: `d` advanced by DELTA once per round, so undoing starts from `q * DELTA`.
    let mut d = q.wrapping_mul(DELTA);
    while q > 0 {
        q -= 1;
        let e = (d >> 2) & 3;
        // Undo the closing block of the round (it ran with `p == n`, `z == v[n-1]`).
        let m = mx(v[n - 1], v[0], d, k[(((n as u32) & 3) ^ e) as usize]);
        v[n] = v[n].wrapping_sub(m);
        for p in (0..n).rev() {
            let y = v[p + 1];
            let z = if p == 0 { v[n] } else { v[p - 1] };
            let m = mx(z, y, d, k[(((p as u32) & 3) ^ e) as usize]);
            v[p] = v[p].wrapping_sub(m);
        }
        d = d.wrapping_sub(DELTA);
    }
    // `l(v, true)`: `c = (len - 1) << 2`, and the trailing word must satisfy
    // `c - 3 <= m <= c`, otherwise the source returns `null`.
    let c = (n as u32).wrapping_shl(2);
    let m = v[n];
    if m < c.wrapping_sub(3) || m > c {
        return Err(CryptoError::BadPadding);
    }
    let mut out = words_to_bytes(&v);
    out.truncate(m as usize);
    Ok(out.into_iter().map(|b| b as char).collect())
}

/// SPEC §6.3 `hmac(str, key)`: **HMAC-MD5**, lowercase hex.
///
/// The §7.1 call site passes `msg = password` and `key = challenge` (the token). Note the
/// neighbourhood the spec flags: third-party clients compute `md5(password + token)`; this
/// client uses HMAC-MD5, which is what the §13.1 vectors pin down.
pub fn hmac_md5_hex(msg: &str, key: &str) -> String {
    const BLOCK: usize = 64;
    let key_bytes = key.as_bytes();
    let mut block = [0u8; BLOCK];
    if key_bytes.len() > BLOCK {
        block[..16].copy_from_slice(Md5::digest(key_bytes).as_slice());
    } else {
        block[..key_bytes.len()].copy_from_slice(key_bytes);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5Cu8; BLOCK];
    for i in 0..BLOCK {
        inner_pad[i] ^= block[i];
        outer_pad[i] ^= block[i];
    }
    let mut inner = Md5::new();
    inner.update(inner_pad);
    inner.update(msg.as_bytes());
    let inner = inner.finalize();
    let mut outer = Md5::new();
    outer.update(outer_pad);
    outer.update(inner);
    hex::encode(outer.finalize())
}

/// SPEC §6.3 `sha1(str)`: SHA-1 of the string's UTF-8 bytes, lowercase hex.
///
/// Used for `chksum` (§7.1) and `rad_user_dm.sign` (§7.3).
pub fn sha1_hex(input: &str) -> String {
    hex::encode(Sha1::digest(input.as_bytes()))
}

/// SPEC §6.4: `JSON.stringify({username, password, ip, acid, enc_ver: "srun_bx1"})`.
///
/// Compact, with the keys in exactly that order. `acid` is stringified, so the caller passes
/// its textual form. The escaping follows `JSON.stringify`: `"` and `\` are escaped, the
/// five C0 controls with short forms get them (`\b \t \n \f \r`), every other control
/// character below `0x20` becomes `\u00xx`, and anything at or above `0x20` — including
/// non-ASCII — is emitted raw.
pub fn user_info_json(username: &str, password: &str, ip: &str, acid: &str) -> String {
    let mut out = String::with_capacity(
        64 + username.len() + password.len() + ip.len() + acid.len() + ENC_VER.len(),
    );
    out.push_str("{\"username\":");
    push_json_string(&mut out, username);
    out.push_str(",\"password\":");
    push_json_string(&mut out, password);
    out.push_str(",\"ip\":");
    push_json_string(&mut out, ip);
    out.push_str(",\"acid\":");
    push_json_string(&mut out, acid);
    out.push_str(",\"enc_ver\":");
    push_json_string(&mut out, ENC_VER);
    out.push('}');
    out
}

/// Append `s` as a `JSON.stringify` string literal.
fn push_json_string(out: &mut String, s: &str) {
    use std::fmt::Write;
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// SPEC §6.4: `info = "{SRBX1}" + btoa(XEncode(json, token), SRUN_ALPHABET)`.
///
/// `username` is the caller's `usernameWithDomain`. Because the JSON envelope of an object
/// is never empty, the §12 item 13 short circuit (`encode('') === ''`) is not reachable
/// through this function with real arguments: it only makes the `{SRBX1}` prefix stand alone
/// when the *encode input* itself is empty, which is why the empty string case is asserted
/// at the [`xencode`]/[`btoa`] level rather than faked here.
pub fn encode_user_info(
    username: &str,
    password: &str,
    ip: &str,
    acid: &str,
    token: &str,
) -> String {
    let blob = xencode(&user_info_json(username, password, ip, acid), token);
    let encoded = btoa(&blob, SRUN_ALPHABET);
    let mut out = String::with_capacity("{SRBX1}".len() + encoded.len());
    out.push_str("{SRBX1}");
    out.push_str(&encoded);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §13.1 fixed inputs.
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    const USERNAME: &str = "testuser";
    const PASSWORD: &str = "testpass";
    const IP: &str = "10.9.9.9";
    const ACID: &str = "1";
    const TIME: &str = "1789819586";

    /// SPEC §13.1 `info` capture.
    const INFO_FIELD: &str = "{SRBX1}1IPGZziAV7Q51Ak77DPQy+3T4n3fHXviay76y9/gGgyBNqV0INj7BUrtgOboiAMJpLywBA//0FJkVyPJIKGZf5HZe5M7TmAEvgrt3110qKZ2oo/TMdrVXahgu8+XAUQjxegK9v==";

    /// An independent, textbook base64 encoder, used only as a reference for `btoa`.
    fn reference_b64(input: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in input.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
            let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
            let triple = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[(triple >> 18) as usize & 63] as char);
            out.push(TABLE[(triple >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(triple >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[triple as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn hmac_md5_matches_original() {
        // SPEC §13.1: password field = "{MD5}" + hmac(password, token), §6.3.
        assert_eq!(
            hmac_md5_hex(PASSWORD, TOKEN),
            "94401f94e5f4eecf2f52ef15389fc664"
        );
    }

    #[test]
    fn info_field_matches_original() {
        // SPEC §13.1 / §6.4.
        assert_eq!(
            encode_user_info(USERNAME, PASSWORD, IP, ACID, TOKEN),
            INFO_FIELD
        );
    }

    #[test]
    fn signout_sign_matches_original() {
        // SPEC §7.3: sign = sha1(time + username + ip + "1" + time); `unbind` is the number
        // 1, so it joins as the character "1", and `time` appears twice unchanged.
        let sign_input = format!("{TIME}{USERNAME}{IP}{}{TIME}", 1);
        assert_eq!(sign_input, "1789819586testuser10.9.9.911789819586");
        assert_eq!(
            sha1_hex(&sign_input),
            "061d82a8c262f51ecc1a07903027fdce0d8d11f6"
        );
    }

    #[test]
    fn custom_base64_equals_standard_base64() {
        const INPUTS: &[&[u8]] = &[
            b"",
            b"a",
            b"ab",
            b"abc",
            b"\x00\x01\x02\xfe\xffabc",
            b"the quick brown fox jumps",
        ];
        let long = [b'x'; 17];
        let all_bytes: Vec<u8> = (0..=255u8).collect();
        let cases: Vec<&[u8]> = INPUTS
            .iter()
            .copied()
            .chain([&long[..], &all_bytes[..]])
            .collect();

        for input in &cases {
            assert_eq!(
                btoa(input, STANDARD_ALPHABET),
                reference_b64(input),
                "btoa disagrees with standard base64 for {:?}",
                input
            );
        }
        for input in &cases {
            assert_eq!(
                atob(&btoa(input, STANDARD_ALPHABET), STANDARD_ALPHABET).unwrap(),
                *input
            );
            assert_eq!(
                atob(&btoa(input, SRUN_ALPHABET), SRUN_ALPHABET).unwrap(),
                *input
            );
        }
    }

    #[test]
    fn xencode_round_trips() {
        let long = "x".repeat(93);
        let cases: Vec<&str> = vec![
            "",
            "a",
            "hello world",
            "the quick brown fox jumps over the lazy dog",
            &long,
        ];
        for text in cases {
            assert_eq!(xdecode(&xencode(text, TOKEN), TOKEN).unwrap(), text);
        }
        assert!(xencode("", TOKEN).is_empty());
        // The key is padded to four words; a longer key must still work.
        assert_eq!(
            xdecode(&xencode("hello world", "a longer key than four characters"), "a longer key than four characters")
                .unwrap(),
            "hello world"
        );
    }

    #[test]
    fn xdecode_rejects_bad_blobs() {
        assert_eq!(xdecode(&[], TOKEN).unwrap(), "");
        assert_eq!(xdecode(&[0u8; 5], TOKEN), Err(CryptoError::BadLength));
        // A single word has no trailing length word.
        assert_eq!(xdecode(&[0u8; 4], TOKEN), Err(CryptoError::BadPadding));
        // Corrupt the length word: `l(v, true)` must reject the out-of-range value.
        let mut blob = xencode("hello world", TOKEN);
        let tail = blob.len() - 4;
        blob[tail..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(xdecode(&blob, TOKEN), Err(CryptoError::BadPadding));
        assert_eq!(atob("A", SRUN_ALPHABET), Err(CryptoError::BadLength));
    }

    #[test]
    fn non_ascii_is_lossy_per_spec_12_3() {
        // SPEC §6.2 / §12 item 3: `s()` reads UTF-16 code units, so "中" (U+4E2D) becomes a
        // single word rather than three UTF-8 bytes, and `l()` gives back one byte per code
        // unit byte — decoding a non-ASCII payload does not return the original text.
        assert_eq!(to_words("\u{4E2D}", false), vec![0x4E2D]);
        assert_eq!(
            xdecode(&xencode("hello \u{4E2D}", TOKEN), TOKEN).unwrap(),
            "hello -"
        );
    }

    #[test]
    fn user_info_json_matches_json_stringify() {
        assert_eq!(
            user_info_json(USERNAME, PASSWORD, IP, ACID),
            "{\"username\":\"testuser\",\"password\":\"testpass\",\"ip\":\"10.9.9.9\",\"acid\":\"1\",\"enc_ver\":\"srun_bx1\"}"
        );
        // Compact separators, key order fixed, string-escaping rules of JSON.stringify.
        assert_eq!(
            user_info_json("a\"b", "c\\d\ne\tf\u{1}", "1.2.3.4", "0"),
            "{\"username\":\"a\\\"b\",\"password\":\"c\\\\d\\ne\\tf\\u0001\",\"ip\":\"1.2.3.4\",\"acid\":\"0\",\"enc_ver\":\"srun_bx1\"}"
        );
        // Non-ASCII is not escaped.
        assert_eq!(
            user_info_json("\u{4E2D}\u{6587}", "", "", ""),
            "{\"username\":\"\u{4E2D}\u{6587}\",\"password\":\"\",\"ip\":\"\",\"acid\":\"\",\"enc_ver\":\"srun_bx1\"}"
        );
    }

    #[test]
    fn empty_inputs_follow_spec_6_4() {
        // SPEC §12 item 13: `encode('') === ''`, so an empty *encode input* leaves the
        // `{SRBX1}` prefix standing alone.
        assert!(xencode("", TOKEN).is_empty());
        assert_eq!(btoa(&xencode("", TOKEN), SRUN_ALPHABET), "");
        assert_eq!(
            format!("{{SRBX1}}{}", btoa(&xencode("", TOKEN), SRUN_ALPHABET)),
            "{SRBX1}"
        );
        // The §6.4 payload is `JSON.stringify({...})`, which is not empty even when every
        // field is empty, so the faithful composition of all-empty arguments still carries a
        // blob. Expected value computed from the §6.1/§6.2 primitives the §13.1 vector pins
        // down, not copied from an implementation.
        let empty_json = "{\"username\":\"\",\"password\":\"\",\"ip\":\"\",\"acid\":\"\",\"enc_ver\":\"srun_bx1\"}";
        assert_eq!(
            encode_user_info("", "", "", "", ""),
            format!("{{SRBX1}}{}", btoa(&xencode(empty_json, ""), SRUN_ALPHABET))
        );
        assert_eq!(
            encode_user_info("", "", "", "", ""),
            "{SRBX1}hFGfr2Wsk4wAB4z51C+hswq7qGXSzavXAYmJCIXhyNzewlceM5DU6SlvD2vENoRE1Pe8Rzsm2A/Ppv3i7wQJqQrhDivwL6dl"
        );
    }
}
