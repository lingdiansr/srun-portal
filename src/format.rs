//! `formatFlow` / `formatTime` (SPEC §1, `dist/lib/index.js`).
//!
//! The spec names these helpers without pinning their output, so the rendering
//! is taken from the reference binary's observable output (black-box probe of
//! the account panel): traffic in binary units with two decimals, `0 B` for
//! nothing, and a duration built from `小时` / `分` / `秒` parts — hours are not
//! folded into days (`86400` renders as `24小时`), empty parts are dropped, and a
//! zero duration renders as the reference's literal `0 秒`.

/// Human readable traffic (`formatFlow`): bytes in, `B`/`KB`/`MB`/`GB`/`TB` out.
pub fn format_flow(bytes: i64) -> String {
    const UNITS: [(&str, f64); 5] = [
        ("B", 1.0),
        ("KB", 1024.0),
        ("MB", 1_048_576.0),
        ("GB", 1_073_741_824.0),
        ("TB", 1_099_511_627_776.0),
    ];
    if bytes <= 0 {
        return "0 B".to_string();
    }
    let value = bytes as f64;
    let mut chosen = UNITS[0];
    for unit in UNITS {
        if value >= unit.1 {
            chosen = unit;
        }
    }
    format!("{:.2} {}", value / chosen.1, chosen.0)
}

/// Human readable duration (`formatTime`).
pub fn format_time(seconds: i64) -> String {
    let total = seconds.max(0);
    let hours = total / 3_600;
    let minutes = (total % 3_600) / 60;
    let secs = total % 60;
    let mut out = String::new();
    if hours > 0 {
        out.push_str(&format!("{hours}小时"));
    }
    if minutes > 0 {
        out.push_str(&format!("{minutes}分"));
    }
    if secs > 0 {
        out.push_str(&format!("{secs}秒"));
    }
    if out.is_empty() {
        out.push_str("0 秒");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_matches_the_reference_rendering() {
        for (bytes, want) in [
            (0_i64, "0 B"),
            (1, "1.00 B"),
            (512, "512.00 B"),
            (1023, "1023.00 B"),
            (1024, "1.00 KB"),
            (1536, "1.50 KB"),
            (1_048_575, "1024.00 KB"),
            (1_048_576, "1.00 MB"),
            (1_073_741_824, "1.00 GB"),
            (31_666_632_837, "29.49 GB"),
            (-1, "0 B"),
        ] {
            assert_eq!(format_flow(bytes), want, "bytes {bytes}");
        }
    }

    #[test]
    fn time_matches_the_reference_rendering() {
        for (seconds, want) in [
            (0_i64, "0 秒"),
            (1, "1秒"),
            (59, "59秒"),
            (60, "1分"),
            (61, "1分1秒"),
            (3_599, "59分59秒"),
            (3_600, "1小时"),
            (3_661, "1小时1分1秒"),
            (7_200, "2小时"),
            (86_399, "23小时59分59秒"),
            (86_400, "24小时"),
            (90_061, "25小时1分1秒"),
            (-5, "0 秒"),
        ] {
            assert_eq!(format_time(seconds), want, "seconds {seconds}");
        }
    }
}
