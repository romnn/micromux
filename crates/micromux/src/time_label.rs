//! Times shown to people: how far from now first, then the local clock time with its UTC offset.

use std::time::{SystemTime, UNIX_EPOCH};

/// The current time in Unix milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |now| u64::try_from(now.as_millis()).unwrap_or(u64::MAX))
}

/// `unix_ms` as how far it is from now, then the local time with its UTC offset, such as
/// `in 59m (at 23:22 +01:00)` or `5m ago (at 22:20 +01:00)`.
#[must_use]
pub fn display_time(unix_ms: u64) -> String {
    let now = now_ms();
    let relative = if unix_ms >= now {
        format!("in {}", distance(unix_ms - now))
    } else {
        format!("{} ago", distance(now - unix_ms))
    };
    with_clock(relative, "at", unix_ms)
}

/// When a claim must renew, as an obligation: `must renew within 59m (by 23:22 +01:00)`, or
/// `was due to renew 5m ago (at 22:20 +01:00)` once it is overdue.
#[must_use]
pub fn renew_label(renew_by_unix_ms: u64) -> String {
    let now = now_ms();
    if renew_by_unix_ms >= now {
        with_clock(
            format!("must renew within {}", distance(renew_by_unix_ms - now)),
            "by",
            renew_by_unix_ms,
        )
    } else {
        with_clock(
            format!("was due to renew {} ago", distance(now - renew_by_unix_ms)),
            "at",
            renew_by_unix_ms,
        )
    }
}

/// `relative` followed by `preposition` and the local clock time of `unix_ms` with its UTC
/// offset; the date shows only for a time on another day.
fn with_clock(relative: String, preposition: &str, unix_ms: u64) -> String {
    let Some(time) = i64::try_from(unix_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| time.with_timezone(&chrono::Local))
    else {
        return relative;
    };
    let format = if time.date_naive() == chrono::Local::now().date_naive() {
        "%H:%M %:z"
    } else {
        "%Y-%m-%d %H:%M %:z"
    };
    format!("{relative} ({preposition} {})", time.format(format))
}

/// A distance in time rounded to the nearest minute: `1h`, `1h 5m`, `59m`, or `under a minute`.
fn distance(ms: u64) -> String {
    let minutes = (ms + 30_000) / 60_000;
    match (minutes / 60, minutes % 60) {
        (0, 0) => "under a minute".to_string(),
        (0, minutes) => format!("{minutes}m"),
        (hours, 0) => format!("{hours}h"),
        (hours, minutes) => format!("{hours}h {minutes}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;

    #[test]
    fn distances_round_to_the_nearest_minute() {
        assert_eq!(distance(3_600_000 - 400), "1h");
        assert_eq!(distance(3_725_000), "1h 2m");
        assert_eq!(distance(3_540_500), "59m");
        assert_eq!(distance(20_000), "under a minute");
    }

    #[test]
    fn obligations_read_as_such() {
        let now = now_ms();
        assert!(renew_label(now + 3_540_000).starts_with("must renew within 59m (by "));
        assert!(renew_label(now - 300_000).starts_with("was due to renew 5m ago (at "));
    }
}
