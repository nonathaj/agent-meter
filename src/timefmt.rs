//! Compact human-readable durations ("2h13m", "4d6h").

use jiff::Timestamp;

/// Formats the time from `now` until `at`, e.g. `2h13m`. Past instants read `now`.
pub fn until(now: Timestamp, at: Timestamp) -> String {
    let secs = at.as_second() - now.as_second();
    if secs <= 0 {
        "now".to_string()
    } else {
        duration(secs.unsigned_abs())
    }
}

/// Formats the time elapsed from `at` to `now`, e.g. `5m`.
pub fn since(now: Timestamp, at: Timestamp) -> String {
    duration((now.as_second() - at.as_second()).max(0).unsigned_abs())
}

/// Formats a number of seconds using the two most significant units.
pub fn duration(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s >= DAY => pair(s / DAY, "d", (s % DAY) / HOUR, "h"),
        s if s >= HOUR => pair(s / HOUR, "h", (s % HOUR) / MIN, "m"),
        s if s >= MIN => format!("{}m", s / MIN),
        s => format!("{s}s"),
    }
}

fn pair(major: u64, major_unit: &str, minor: u64, minor_unit: &str) -> String {
    if minor == 0 {
        format!("{major}{major_unit}")
    } else {
        format!("{major}{major_unit}{minor}{minor_unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(60 * 5), "5m");
        assert_eq!(duration(3600 * 2 + 60 * 13), "2h13m");
        assert_eq!(duration(86400 * 4), "4d");
        assert_eq!(duration(86400 * 4 + 3600 * 6 + 59), "4d6h");
    }

    #[test]
    fn until_past_is_now() {
        let now = Timestamp::from_second(1000).unwrap();
        assert_eq!(until(now, Timestamp::from_second(10).unwrap()), "now");
    }
}
