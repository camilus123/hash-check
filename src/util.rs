//! Small formatting helpers shared by the UI and reports.

use chrono::{Local, TimeZone};

pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Binary units, matching what Windows Explorer shows.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

/// 1234567 -> "1 234 567"
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

pub fn format_unix_secs(secs: i64) -> String {
    match Local.timestamp_opt(secs, 0).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => "-".to_string(),
    }
}

pub fn format_unix_nanos(ns: i64) -> String {
    format_unix_secs(ns.div_euclid(1_000_000_000))
}

pub fn format_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "-".to_string();
    }
    let s = secs.round() as u64;
    if s >= 3600 {
        format!("{} h {:02} min", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{} min {:02} s", s / 60, s % 60)
    } else {
        format!("{s} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_and_sizes() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1 000");
        assert_eq!(group_digits(1234567), "1 234 567");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.00 KB");
        assert_eq!(human_bytes(12 * 1024 * 1024 * 1024), "12.0 GB");
        assert_eq!(format_duration(87.0), "1 min 27 s");
    }
}
