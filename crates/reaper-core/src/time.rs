//! Time as plain data. The core never reads a clock: `Timestamp` values come
//! from the snapshot, and durations from configuration.

use std::fmt;
use std::time::Duration;

/// Whole seconds since the Unix epoch, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

impl Timestamp {
    #[must_use]
    pub fn seconds_since(self, earlier: Timestamp) -> i64 {
        self.0 - earlier.0
    }

    /// True when at least `d` has passed between `earlier` and `self`.
    #[must_use]
    pub fn elapsed_at_least(self, earlier: Timestamp, d: Duration) -> bool {
        let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
        self.seconds_since(earlier) >= secs
    }

    /// Parses RFC 3339 in UTC (`2026-10-10T08:00:00Z`, fractional seconds
    /// allowed and truncated, `+00:00` accepted).
    pub fn parse_rfc3339(input: &str) -> Result<Self, TimeError> {
        let err = || TimeError(input.to_owned());
        let (date, rest) = input.split_once('T').ok_or_else(err)?;
        let rest = rest
            .strip_suffix('Z')
            .or_else(|| rest.strip_suffix("+00:00"))
            .ok_or_else(err)?;
        let time = rest.split_once('.').map_or(rest, |(t, frac)| {
            if frac.chars().all(|ch| ch.is_ascii_digit()) && !frac.is_empty() {
                t
            } else {
                ""
            }
        });
        let ymd: Vec<&str> = date.split('-').collect();
        let hms: Vec<&str> = time.split(':').collect();
        if ymd.len() != 3 || hms.len() != 3 {
            return Err(err());
        }
        let num = |p: &str, len: usize| -> Result<i64, TimeError> {
            if p.len() == len && p.chars().all(|ch| ch.is_ascii_digit()) {
                p.parse().map_err(|_| err())
            } else {
                Err(err())
            }
        };
        let (y, mo, da) = (num(ymd[0], 4)?, num(ymd[1], 2)?, num(ymd[2], 2)?);
        let (h, mi, se) = (num(hms[0], 2)?, num(hms[1], 2)?, num(hms[2], 2)?);
        if !(1..=12).contains(&mo)
            || da < 1
            || da > days_in_month(y, mo)
            || h > 23
            || mi > 59
            || se > 60
        {
            return Err(err());
        }
        Ok(Timestamp(
            days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + se,
        ))
    }
}

impl fmt::Display for Timestamp {
    /// RFC 3339 UTC with second precision.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self.0.div_euclid(86_400);
        let secs = self.0.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        write!(
            f,
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid RFC 3339 UTC timestamp {0:?}")]
pub struct TimeError(pub String);

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's civil calendar algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

/// Parses a Go/Flux style duration: one or more `<int><unit>` parts with unit
/// `h`, `m` or `s` (`90s`, `30m`, `1h30m`).
pub fn parse_duration(s: &str) -> Result<Duration, DurationError> {
    let err = || DurationError(s.to_owned());
    if s.is_empty() {
        return Err(err());
    }
    let mut total: u64 = 0;
    let mut digits = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return Err(err()),
        };
        let n: u64 = digits.parse().map_err(|_| err())?;
        digits.clear();
        total = n
            .checked_mul(unit)
            .and_then(|v| total.checked_add(v))
            .ok_or_else(err)?;
    }
    if !digits.is_empty() {
        return Err(err());
    }
    Ok(Duration::from_secs(total))
}

/// Formats a duration in the same form `parse_duration` reads (`1h30m`).
#[must_use]
pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s == 0 {
        return "0s".into();
    }
    let mut out = String::new();
    for (unit, suffix) in [(3600, 'h'), (60, 'm'), (1, 's')] {
        let n = s / unit % if unit == 3600 { u64::MAX } else { 60 };
        if n > 0 {
            out.push_str(&n.to_string());
            out.push(suffix);
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid duration {0:?}: expected parts like 90s, 30m or 1h30m")]
pub struct DurationError(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trips() {
        for s in [
            "1970-01-01T00:00:00Z",
            "2026-10-10T08:00:00Z",
            "2024-02-29T23:59:59Z",
            "2000-03-01T00:00:00Z",
        ] {
            assert_eq!(Timestamp::parse_rfc3339(s).unwrap().to_string(), s);
        }
        assert_eq!(
            Timestamp::parse_rfc3339("1970-01-01T00:01:00Z").unwrap(),
            Timestamp(60)
        );
    }

    #[test]
    fn rfc3339_accepts_fraction_and_offset_zero() {
        let t = Timestamp::parse_rfc3339("2026-10-10T08:00:00Z").unwrap();
        assert_eq!(
            Timestamp::parse_rfc3339("2026-10-10T08:00:00.123456Z").unwrap(),
            t
        );
        assert_eq!(
            Timestamp::parse_rfc3339("2026-10-10T08:00:00+00:00").unwrap(),
            t
        );
    }

    #[test]
    fn rfc3339_rejects_garbage() {
        for s in [
            "",
            "yesterday",
            "2026-10-10",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-10-10T08:00:00+02:00",
            "2026-10-10T08:00:00.Z",
            "2026-1-10T08:00:00Z",
        ] {
            assert!(Timestamp::parse_rfc3339(s).is_err(), "{s}");
        }
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_mins(30));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_mins(90));
        assert_eq!(parse_duration("168h").unwrap(), Duration::from_hours(168));
        for bad in [
            "",
            "30",
            "m",
            "1.5h",
            "-5m",
            "5d",
            "1h 30m",
            "99999999999999999999h",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations_format() {
        assert_eq!(format_duration(Duration::from_mins(90)), "1h30m");
        assert_eq!(format_duration(Duration::from_mins(1)), "1m");
        assert_eq!(format_duration(Duration::from_secs(90_061)), "25h1m1s");
        assert_eq!(format_duration(Duration::ZERO), "0s");
    }
}
