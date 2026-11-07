//! Values of the metadata filters `size:`, `dm:`/`dc:`/`da:` and `attrib:`
//! (ADR 0004, Part 2). Pure parsing: dates resolve against a supplied
//! local clock, so tests are deterministic.

/// 100-nanosecond ticks since the Unix epoch (the index's time unit).
const TICKS_PER_SECOND: i64 = 10_000_000;

/// Inclusive size bounds in bytes.
pub(crate) fn parse_size(text: &str) -> Result<(Option<u64>, Option<u64>), String> {
    if let Some((a, b)) = text.split_once("..") {
        let min = if a.is_empty() {
            None
        } else {
            Some(quantity(a)?)
        };
        let max = if b.is_empty() {
            None
        } else {
            Some(quantity(b)?)
        };
        if min.is_none() && max.is_none() {
            return Err("size range needs at least one bound".into());
        }
        if let (Some(lo), Some(hi)) = (min, max)
            && lo > hi
        {
            return Err("size range is reversed".into());
        }
        return Ok((min, max));
    }
    let (op, rest) = comparison(text);
    let v = quantity(rest)?;
    Ok(match op {
        Cmp::Gt => (Some(v.saturating_add(1)), None),
        Cmp::Ge => (Some(v), None),
        Cmp::Lt => {
            if v == 0 {
                return Err("nothing is smaller than 0 bytes".into());
            }
            (None, Some(v - 1))
        }
        Cmp::Le => (None, Some(v)),
        Cmp::Eq => (Some(v), Some(v)),
    })
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

fn comparison(text: &str) -> (Cmp, &str) {
    for (p, c) in [
        (">=", Cmp::Ge),
        ("<=", Cmp::Le),
        (">", Cmp::Gt),
        ("<", Cmp::Lt),
        ("=", Cmp::Eq),
    ] {
        if let Some(rest) = text.strip_prefix(p) {
            return (c, rest);
        }
    }
    (Cmp::Eq, text)
}

/// `10`, `10b`, `1.5mb`, `2g`: binary units (1 KB = 1024 bytes), like
/// Explorer and the app's Size column.
fn quantity(text: &str) -> Result<u64, String> {
    let lower = text.to_ascii_lowercase();
    let split = lower
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(lower.len());
    let (number, unit) = lower.split_at(split);
    let scale: u64 = match unit {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => {
            return Err(format!(
                "unknown size unit `{unit}` (use b, kb, mb, gb or tb)"
            ));
        }
    };
    if number.is_empty() || number.len() > 24 || number.matches('.').count() > 1 {
        return Err("expected a size such as 10mb".into());
    }
    let value: f64 = number
        .parse()
        .map_err(|_| "expected a size such as 10mb".to_string())?;
    let bytes = (value * scale as f64).round();
    if !bytes.is_finite() || bytes < 0.0 || bytes > u64::MAX as f64 {
        return Err("size is out of range".into());
    }
    Ok(bytes as u64)
}

/// The local clock a date filter resolves against: the current time in
/// ticks and the local UTC offset in seconds (east positive).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    pub now: i64,
    pub offset_seconds: i64,
}

impl Clock {
    pub(crate) fn local() -> Clock {
        use chrono::{Local, Offset};
        let now = Local::now();
        Clock {
            now: now.timestamp() * TICKS_PER_SECOND + i64::from(now.timestamp_subsec_nanos() / 100),
            offset_seconds: i64::from(now.offset().fix().local_minus_utc()),
        }
    }

    /// Ticks of local midnight starting the day `days` days from today.
    fn midnight(&self, days: i64) -> i64 {
        let offset = self.offset_seconds * TICKS_PER_SECOND;
        let day = 86_400 * TICKS_PER_SECOND;
        (self.now + offset).div_euclid(day) * day - offset + days * day
    }
}

/// Half-open tick range `[start, end)` for a date filter value.
pub(crate) fn parse_date(text: &str, clock: &Clock) -> Result<(Option<i64>, Option<i64>), String> {
    if let Some((a, b)) = text.split_once("..") {
        let start = if a.is_empty() {
            None
        } else {
            Some(span(a, clock)?.0)
        };
        let end = if b.is_empty() {
            None
        } else {
            Some(span(b, clock)?.1)
        };
        if start.is_none() && end.is_none() {
            return Err("date range needs at least one bound".into());
        }
        if let (Some(s), Some(e)) = (start, end)
            && s >= e
        {
            return Err("date range is reversed".into());
        }
        return Ok((start, end));
    }
    let (op, rest) = comparison(text);
    let (s, e) = span(rest, clock)?;
    Ok(match op {
        Cmp::Eq => (Some(s), Some(e)),
        Cmp::Gt => (Some(e), None),
        Cmp::Ge => (Some(s), None),
        Cmp::Lt => (None, Some(s)),
        Cmp::Le => (None, Some(e)),
    })
}

/// One date word or calendar date as `[start, end)`.
fn span(text: &str, clock: &Clock) -> Result<(i64, i64), String> {
    let lower = text.to_ascii_lowercase();
    let day = |d: i64| clock.midnight(d);
    let tomorrow = day(1);
    match lower.as_str() {
        "today" => return Ok((day(0), tomorrow)),
        "yesterday" => return Ok((day(-1), day(0))),
        "thisweek" => {
            // Weeks start on Monday (ISO 8601).
            let days_since_monday = ((day(0) + clock.offset_seconds * TICKS_PER_SECOND)
                .div_euclid(86_400 * TICKS_PER_SECOND)
                + 3)
            .rem_euclid(7);
            return Ok((day(-days_since_monday), tomorrow));
        }
        "thismonth" | "thisyear" => {
            use chrono::{Datelike, NaiveDate};
            let local = chrono::DateTime::from_timestamp(
                (clock.now / TICKS_PER_SECOND) + clock.offset_seconds,
                0,
            )
            .ok_or("clock out of range")?
            .naive_utc()
            .date();
            let first = if lower == "thismonth" {
                NaiveDate::from_ymd_opt(local.year(), local.month(), 1)
            } else {
                NaiveDate::from_ymd_opt(local.year(), 1, 1)
            }
            .ok_or("clock out of range")?;
            let days_back = (local - first).num_days();
            return Ok((day(-days_back), tomorrow));
        }
        _ => {}
    }
    // last<N><unit>: a rolling window ending now (last7days, last10minutes).
    if let Some(rest) = lower.strip_prefix("last") {
        let split = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let (n, unit) = rest.split_at(split);
        let n: i64 = if n.is_empty() {
            1
        } else {
            n.parse()
                .ok()
                .filter(|n| (1..=100_000).contains(n))
                .ok_or("count out of range")?
        };
        let seconds = match unit {
            "minute" | "minutes" | "min" | "mins" => 60,
            "hour" | "hours" => 3_600,
            "day" | "days" => 86_400,
            "week" | "weeks" => 7 * 86_400,
            _ => return Err(format!("unknown date word `{text}`")),
        };
        return Ok((clock.now - n * seconds * TICKS_PER_SECOND, i64::MAX));
    }
    // YYYY-MM-DD in local time.
    let date = chrono::NaiveDate::parse_from_str(&lower, "%Y-%m-%d").map_err(|_| {
        format!(
            "unknown date `{text}` (use today, yesterday, thisweek, thismonth, thisyear, last7days, last10minutes or YYYY-MM-DD)"
        )
    })?;
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch");
    let days = (date - epoch).num_days();
    let start = (days * 86_400 - clock.offset_seconds) * TICKS_PER_SECOND;
    Ok((start, start + 86_400 * TICKS_PER_SECOND))
}

/// `attrib:` letters, all required: R read-only, H hidden, S system,
/// D directory, A archive, T temporary, P sparse, L reparse point,
/// C compressed, O offline, I not content-indexed, E encrypted.
/// `;` separators are allowed (`attrib:H;S`).
pub(crate) fn parse_attributes(text: &str) -> Result<u32, String> {
    let mut bits = 0;
    for c in text.chars().filter(|c| *c != ';') {
        bits |= match c.to_ascii_uppercase() {
            'R' => 0x1,
            'H' => 0x2,
            'S' => 0x4,
            'D' => 0x10,
            'A' => 0x20,
            'T' => 0x100,
            'P' => 0x200,
            'L' => 0x400,
            'C' => 0x800,
            'O' => 0x1000,
            'I' => 0x2000,
            'E' => 0x4000,
            _ => {
                return Err(format!(
                    "unknown attribute `{c}` (use R H S D A T P L C O I E)"
                ));
            }
        };
    }
    if bits == 0 {
        return Err("expected attribute letters such as H or RS".into());
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    #[test]
    fn sizes() {
        assert_eq!(parse_size(">10mb").unwrap(), (Some(10 * MB + 1), None));
        assert_eq!(parse_size(">=10MB").unwrap(), (Some(10 * MB), None));
        assert_eq!(parse_size("<1kb").unwrap(), (None, Some(1023)));
        assert_eq!(parse_size("<=1k").unwrap(), (None, Some(1024)));
        assert_eq!(
            parse_size("1.5mb").unwrap(),
            (Some(MB * 3 / 2), Some(MB * 3 / 2))
        );
        assert_eq!(parse_size("1mb..5mb").unwrap(), (Some(MB), Some(5 * MB)));
        assert_eq!(parse_size("..5mb").unwrap(), (None, Some(5 * MB)));
        assert_eq!(parse_size("100").unwrap(), (Some(100), Some(100)));
        for bad in [
            "",
            ">",
            "10xb",
            "5mb..1mb",
            "..",
            "1.2.3mb",
            "<0",
            "-5",
            "99999999999999999999tb",
        ] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }

    // 2026-11-07 12:00:00 UTC, in a UTC+9 zone (21:00 local).
    const NOON_UTC: i64 = 1_794_052_800 * TICKS_PER_SECOND;
    const JST: Clock = Clock {
        now: NOON_UTC,
        offset_seconds: 9 * 3600,
    };
    const DAY: i64 = 86_400 * TICKS_PER_SECOND;

    #[test]
    fn date_words_follow_local_midnight() {
        // Local midnight 2026-11-07 00:00 JST = 2026-11-06 15:00 UTC.
        let midnight = NOON_UTC - 21 * 3600 * TICKS_PER_SECOND;
        assert_eq!(
            parse_date("today", &JST).unwrap(),
            (Some(midnight), Some(midnight + DAY))
        );
        assert_eq!(
            parse_date("yesterday", &JST).unwrap(),
            (Some(midnight - DAY), Some(midnight))
        );
        assert_eq!(
            parse_date("2026-11-07", &JST).unwrap(),
            (Some(midnight), Some(midnight + DAY))
        );
        assert_eq!(
            parse_date("2026-11-01..2026-11-07", &JST).unwrap(),
            (Some(midnight - 6 * DAY), Some(midnight + DAY))
        );
        assert_eq!(
            parse_date(">2026-11-06", &JST).unwrap(),
            (Some(midnight), None)
        );
        assert_eq!(parse_date("<today", &JST).unwrap(), (None, Some(midnight)));
        // 2026-11-07 is a Saturday: the week began Monday 2026-11-02.
        assert_eq!(
            parse_date("thisweek", &JST).unwrap().0,
            Some(midnight - 5 * DAY)
        );
        assert_eq!(
            parse_date("thismonth", &JST).unwrap().0,
            Some(midnight - 6 * DAY)
        );
        assert_eq!(
            parse_date("thisyear", &JST).unwrap().0,
            parse_date("2026-01-01", &JST).unwrap().0
        );
    }

    #[test]
    fn rolling_windows_end_now() {
        let (s, e) = parse_date("last10minutes", &JST).unwrap();
        assert_eq!(s, Some(NOON_UTC - 600 * TICKS_PER_SECOND));
        assert_eq!(e, Some(i64::MAX));
        assert_eq!(
            parse_date("last7days", &JST).unwrap().0,
            Some(NOON_UTC - 7 * DAY)
        );
        assert_eq!(
            parse_date("lasthour", &JST).unwrap().0,
            Some(NOON_UTC - 3600 * TICKS_PER_SECOND)
        );
        for bad in [
            "last0days",
            "lastfortnight",
            "2026-13-01",
            "tomorrowish",
            "",
            "2026-11-07..2026-11-01",
            "..",
        ] {
            assert!(parse_date(bad, &JST).is_err(), "{bad}");
        }
    }

    #[test]
    fn attributes() {
        assert_eq!(parse_attributes("H").unwrap(), 0x2);
        assert_eq!(parse_attributes("rs").unwrap(), 0x5);
        assert_eq!(parse_attributes("H;S").unwrap(), 0x6);
        assert!(parse_attributes("Q").is_err());
        assert!(parse_attributes(";").is_err());
    }
}
