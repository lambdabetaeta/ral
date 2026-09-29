//! Time as exarch writes it for people.

use jiff::Timestamp;
use std::time::Duration;

/// A span in its two coarsest units: `3d 04h`, `2h 05m`, `41m 09s`, `12s`.
pub fn hms(secs: u64) -> String {
    if secs >= 86400 {
        format!("{}d {:02}h", secs / 86400, (secs % 86400) / 3600)
    } else if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// An instant in the system time zone, as `Mon 14:05`.
pub fn local(t: Timestamp) -> String {
    t.to_zoned(jiff::tz::TimeZone::system())
        .strftime("%a %H:%M")
        .to_string()
}

/// The wait from `now` until `t`; zero once `t` has passed.
pub fn until(t: Timestamp, now: Timestamp) -> Duration {
    Duration::try_from(t.duration_since(now)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_writes_the_two_coarsest_units() {
        assert_eq!(hms(3 * 86400 + 4 * 3600), "3d 04h");
        assert_eq!(hms(7 * 86400), "7d 00h");
        assert_eq!(hms(7500), "2h 05m");
        assert_eq!(hms(69), "1m 09s");
        assert_eq!(hms(12), "12s");
    }
}
