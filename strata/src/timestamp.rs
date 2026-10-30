//! TIMESTAMP values: microseconds since 1970-01-01T00:00:00Z, written as
//! ISO 8601 text.
//!
//! [`parse`] reads a date (`2026-10-01`, midnight UTC) or a date and time
//! (`2026-10-01T12:30`, `2026-10-01 12:30:05.25`), with an optional offset
//! (`Z`, `+02:00`, `-0530`, `+02`); a time without one is UTC. Seconds take
//! up to nine decimals, of which the first six are kept. Years run from
//! 0000 to 9999, in the proleptic Gregorian calendar. [`format`] writes
//! UTC, `2026-10-01T12:30:05.250Z`, with the fraction only when it is not
//! zero: three digits when whole milliseconds, else six.

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Microseconds since the Unix epoch, or `None` for text that is not an
/// ISO 8601 date or date and time.
pub fn parse(text: &str) -> Option<i64> {
    let s = text.trim().as_bytes();
    let mut p = Cursor { s, at: 0 };
    let year = p.digits(4)?;
    p.byte(b'-')?;
    let month = p.digits(2)?;
    p.byte(b'-')?;
    let day = p.digits(2)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut micros = days_from_civil(year, month, day) * MICROS_PER_DAY;
    if p.done() {
        return Some(micros);
    }
    if !matches!(p.next()?, b'T' | b't' | b' ') {
        return None;
    }
    let hour = p.digits(2)?;
    p.byte(b':')?;
    let minute = p.digits(2)?;
    let mut second = 0;
    let mut fraction = 0;
    if p.peek() == Some(b':') {
        p.at += 1;
        second = p.digits(2)?;
        if matches!(p.peek(), Some(b'.' | b',')) {
            p.at += 1;
            let start = p.at;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.at += 1;
            }
            let digits = &s[start..p.at];
            if digits.is_empty() || digits.len() > 9 {
                return None;
            }
            for i in 0..6 {
                fraction = fraction * 10 + digits.get(i).map_or(0, |c| (c - b'0') as i64);
            }
        }
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    micros += ((hour * 60 + minute) * 60 + second) * 1_000_000 + fraction;
    let offset = match p.next() {
        None => 0,
        Some(b'Z' | b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let h = p.digits(2)?;
            let m = match p.peek() {
                None => 0,
                Some(b':') => {
                    p.at += 1;
                    p.digits(2)?
                }
                Some(_) => p.digits(2)?,
            };
            if h > 23 || m > 59 {
                return None;
            }
            let o = (h * 60 + m) * 60_000_000;
            if sign == b'-' { -o } else { o }
        }
        Some(_) => return None,
    };
    if !p.done() {
        return None;
    }
    Some(micros - offset)
}

/// ISO 8601 text in UTC for microseconds since the Unix epoch. Years
/// outside 0000 to 9999 (which no parsed text reaches) are written with a
/// sign, as ISO 8601's expanded years are.
pub fn format(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let mut rest = micros.rem_euclid(MICROS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let fraction = rest % 1_000_000;
    rest /= 1_000_000;
    let (h, min, s) = (rest / 3600, rest / 60 % 60, rest % 60);
    let year = if (0..=9999).contains(&y) { format!("{y:04}") } else { format!("{y:+05}") };
    let frac = match fraction {
        0 => String::new(),
        f if f % 1000 == 0 => format!(".{:03}", f / 1000),
        f => format!(".{f:06}"),
    };
    format!("{year}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}{frac}Z")
}

/// Microseconds since the Unix epoch now.
pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as i64)
}

struct Cursor<'a> {
    s: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.at).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.at += 1;
        Some(c)
    }

    fn byte(&mut self, b: u8) -> Option<()> {
        (self.next()? == b).then_some(())
    }

    /// Exactly `n` ASCII digits.
    fn digits(&mut self, n: usize) -> Option<i64> {
        let ds = self.s.get(self.at..self.at + n)?;
        if !ds.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.at += n;
        Some(ds.iter().fold(0, |v, c| v * 10 + (c - b'0') as i64))
    }

    fn done(&self) -> bool {
        self.at == self.s.len()
    }
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01 (`civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = MICROS_PER_DAY;

    #[test]
    fn dates_and_times() {
        assert_eq!(parse("1970-01-01"), Some(0));
        assert_eq!(parse("1970-01-02"), Some(DAY));
        assert_eq!(parse("1969-12-31"), Some(-DAY));
        assert_eq!(parse("2000-03-01"), Some(11_017 * DAY));
        assert_eq!(parse("2026-10-01T00:00:00Z"), Some(20_727 * DAY));
        assert_eq!(parse(" 2026-10-01 12:30 "), Some(20_727 * DAY + 45_000_000_000));
        assert_eq!(parse("2026-10-01t12:30:05.25z"), Some(20_727 * DAY + 45_005_250_000));
        // Seven to nine decimals: the microseconds are kept.
        assert_eq!(parse("1970-01-01T00:00:00.123456789"), Some(123_456));
        assert_eq!(parse("1970-01-01T00:00:00,5"), Some(500_000));
        assert_eq!(parse("0000-01-01"), Some(-719_528 * DAY));
        assert_eq!(parse("9999-12-31T23:59:59.999999Z"), Some(253_402_300_799_999_999));
    }

    #[test]
    fn offsets() {
        let utc = parse("2026-10-01T12:00:00Z").unwrap();
        for (text, minutes) in [("+02:00", 120), ("-05:30", -330), ("+0200", 120), ("+02", 120), ("-00:00", 0)] {
            assert_eq!(parse(&format!("2026-10-01T12:00:00{text}")), Some(utc - minutes * 60_000_000), "{text}");
        }
    }

    #[test]
    fn refuses_what_is_not_a_date() {
        for bad in [
            "", "2026", "2026-10", "2026-1-01", "26-10-01", "2026-13-01", "2026-00-10", "2026-02-29", "1900-02-29", "2026-04-31",
            "2026-10-01T", "2026-10-01T24:00", "2026-10-01T12:60", "2026-10-01T12:00:60", "2026-10-01T12", "2026-10-01T12:00:00.",
            "2026-10-01T12:00:00.1234567890", "2026-10-01T12:00:00+24:00", "2026-10-01T12:00:00 Z", "2026-10-01T12:00:00Zx", "now",
            "+2026-10-01", "2026-10-01T12:00:00+2",
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
        assert_eq!(parse("2000-02-29"), Some(11_016 * DAY));
        assert_eq!(parse("2024-02-29").map(format).as_deref(), Some("2024-02-29T00:00:00Z"));
    }

    #[test]
    fn formats_utc_and_round_trips() {
        assert_eq!(format(0), "1970-01-01T00:00:00Z");
        assert_eq!(format(-1), "1969-12-31T23:59:59.999999Z");
        assert_eq!(format(1_500), "1970-01-01T00:00:00.001500Z");
        assert_eq!(format(250_000), "1970-01-01T00:00:00.250Z");
        assert_eq!(format(i64::MAX), "+294247-01-10T04:00:54.775807Z");
        assert_eq!(format(i64::MIN), "-290308-12-21T19:59:05.224192Z");
        // Days from 0000-01-01 to 9999-12-31, at odd times.
        for day in (-719_528..2_932_897).step_by(97) {
            let t = day * DAY + (day.rem_euclid(86_400)) * 1_000_003 % DAY;
            assert_eq!(parse(&format(t)), Some(t), "{}", format(t));
        }
    }
}
