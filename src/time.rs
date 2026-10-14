//! Parsing and printing of the times and durations the command line takes.

use std::fmt::Write;

/// Reads an RFC 3339 timestamp (`2026-09-15T00:00:00Z`) or unix seconds.
pub fn parse_time(s: &str) -> Result<i64, String> {
    if let Ok(seconds) = s.parse::<i64>() {
        return Ok(seconds);
    }
    s.parse::<jiff::Timestamp>()
        .map(|t| t.as_second())
        .map_err(|_| format!("{s:?} is neither RFC 3339 (2026-09-15T00:00:00Z) nor unix seconds"))
}

/// Prints unix seconds as RFC 3339 in UTC.
pub fn format_time(seconds: i64) -> String {
    match jiff::Timestamp::from_second(seconds) {
        Ok(t) => t.to_string(),
        Err(_) => seconds.to_string(),
    }
}

const UNITS: [(char, i64); 5] = [
    ('w', 7 * 86_400),
    ('d', 86_400),
    ('h', 3_600),
    ('m', 60),
    ('s', 1),
];

/// Reads a positive duration such as `14d`, `36h`, `1d12h` or `90s`; a bare number is seconds.
pub fn parse_duration(s: &str) -> Result<i64, String> {
    let invalid = || format!("{s:?} is not a duration such as 14d, 36h, 1d12h or 90s");
    if s.is_empty() {
        return Err(invalid());
    }
    let mut total: i64 = 0;
    let mut digits = String::new();
    let mut last_unit = usize::MAX;
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = UNITS
            .iter()
            .position(|&(u, _)| u == c)
            .ok_or_else(invalid)?;
        // Each unit at most once, largest first.
        if digits.is_empty() || (last_unit != usize::MAX && unit <= last_unit) {
            return Err(invalid());
        }
        last_unit = unit;
        let n: i64 = digits.parse().map_err(|_| invalid())?;
        total = n
            .checked_mul(UNITS[unit].1)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(invalid)?;
        digits.clear();
    }
    if !digits.is_empty() {
        if last_unit != usize::MAX {
            return Err(invalid());
        }
        total = digits.parse().map_err(|_| invalid())?;
    }
    if total <= 0 {
        return Err(format!("{s:?} is not a positive duration"));
    }
    Ok(total)
}

/// Prints seconds the way [`parse_duration`] reads them, in days and smaller units.
pub fn format_duration(mut seconds: i64) -> String {
    if seconds <= 0 {
        return format!("{seconds}s");
    }
    let mut out = String::new();
    for &(unit, size) in &UNITS[1..] {
        if seconds >= size {
            let _ = write!(out, "{}{unit}", seconds / size);
            seconds %= size;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("2026-09-15T00:00:00Z"), Ok(1_789_430_400));
        assert_eq!(parse_time("2026-09-15T02:00:00+02:00"), Ok(1_789_430_400));
        assert_eq!(parse_time("1789430400"), Ok(1_789_430_400));
        assert_eq!(parse_time("-5"), Ok(-5));
        assert!(parse_time("2026-09-15").is_err());
        assert!(parse_time("yesterday").is_err());
        assert_eq!(format_time(1_789_430_400), "2026-09-15T00:00:00Z");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("14d"), Ok(14 * 86_400));
        assert_eq!(parse_duration("2w"), Ok(14 * 86_400));
        assert_eq!(parse_duration("1d12h"), Ok(36 * 3_600));
        assert_eq!(parse_duration("90s"), Ok(90));
        assert_eq!(parse_duration("3600"), Ok(3_600));
        for bad in [
            "", "d", "14", "0d", "1h1d", "1d1d", "1.5d", "14x", "1d5", "-1d",
        ] {
            if bad == "14" {
                continue;
            }
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
        assert_eq!(format_duration(14 * 86_400), "14d");
        assert_eq!(format_duration(90_061), "1d1h1m1s");
        assert_eq!(format_duration(90), "1m30s");
    }
}
