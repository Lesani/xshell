use std::time::{Duration, SystemTime};

// Cursor records timestamps as unix-epoch milliseconds (createdAtMs / updatedAtMs); reuse
// the SystemTime formatter so its dates render identically to the other agents'.
pub fn unix_ms_to_iso(ms: u64) -> String {
    system_time_to_iso(SystemTime::UNIX_EPOCH + Duration::from_millis(ms))
}

pub fn system_time_to_iso(time: SystemTime) -> String {
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    let days = secs / 86400;
    let remaining = secs % 86400;
    let hours = remaining / 3600;
    let minutes = (remaining % 3600) / 60;
    let seconds = remaining % 60;

    // Simple epoch-to-date calculation
    let mut y = 1970i64;
    let mut d = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if d < days_in_year {
            break;
        }
        d -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    for (i, &md) in month_days.iter().enumerate() {
        if d < md as i64 {
            m = i;
            break;
        }
        d -= md as i64;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m + 1,
        d + 1,
        hours,
        minutes,
        seconds
    )
}

/// An RFC 3339 timestamp, `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±hh:mm)`, as Unix milliseconds
/// (the fraction cut to milliseconds). `None` for anything else and for times before 1970.
pub fn iso_to_unix_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let d = b.get(r)?;
        d.iter()
            .all(u8::is_ascii_digit)
            .then(|| d.iter().fold(0i64, |n, c| n * 10 + i64::from(c - b'0')))
    };
    let sep = |i: usize, c: u8| b.get(i) == Some(&c);
    if !(sep(4, b'-') && sep(7, b'-') && (sep(10, b'T') || sep(10, b't')))
        || !(sep(13, b':') && sep(16, b':'))
    {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_len = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if d < 1 || d > month_len || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut ms = 0i64;
    if sep(i, b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            if i - start < 3 {
                ms = ms * 10 + i64::from(b[i] - b'0');
            }
            i += 1;
        }
        if i == start {
            return None;
        }
        for _ in (i - start)..3 {
            ms *= 10;
        }
    }
    let offset_min = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(&c @ (b'+' | b'-')) if i + 6 == b.len() && sep(i + 3, b':') => {
            let (oh, om) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let m = oh * 60 + om;
            if c == b'+' {
                m
            } else {
                -m
            }
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let (yy, mm) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    u64::try_from(secs * 1000 + ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_to_unix_ms_parses() {
        assert_eq!(iso_to_unix_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            iso_to_unix_ms("2024-02-29T12:34:56Z"),
            Some(1_709_210_096_000)
        );
        assert_eq!(
            iso_to_unix_ms("2024-02-29T12:34:56.999Z"),
            Some(1_709_210_096_999)
        );
        // Fractions: shorter ones scale, longer ones are cut to milliseconds.
        assert_eq!(
            iso_to_unix_ms("2024-02-29T12:34:56.5Z"),
            Some(1_709_210_096_500)
        );
        assert_eq!(
            iso_to_unix_ms("2026-10-09T14:24:22.213456Z"),
            iso_to_unix_ms("2026-10-09T14:24:22.213Z")
        );
        // Offsets: the same instant.
        assert_eq!(
            iso_to_unix_ms("2024-02-29T14:34:56+02:00"),
            Some(1_709_210_096_000)
        );
        assert_eq!(
            iso_to_unix_ms("2024-02-29T07:04:56.250-05:30"),
            Some(1_709_210_096_250)
        );
        // Round trip with the formatter.
        assert_eq!(
            unix_ms_to_iso(iso_to_unix_ms("2000-03-01T00:00:00Z").unwrap()),
            "2000-03-01T00:00:00Z"
        );
        for bad in [
            "",
            "garbage",
            "2024-02-30T00:00:00Z",
            "2023-02-29T00:00:00Z",
            "2024-13-01T00:00:00Z",
            "2024-02-29 12:34:56Z",
            "2024-02-29T24:00:00Z",
            "2024-02-29T12:34:56",
            "2024-02-29T12:34:56.Z",
            "2024-02-29T12:34:56Zx",
            "2024-02-29T12:34:56+0200",
            "1969-12-31T23:59:59Z",
            "２０２４-02-29T12:34:56Z",
        ] {
            assert_eq!(iso_to_unix_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn system_time_to_iso_formats_epoch_and_leap_day() {
        let at = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(
            system_time_to_iso(SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00Z"
        );
        assert_eq!(
            system_time_to_iso(at(1_709_210_096)),
            "2024-02-29T12:34:56Z"
        );
        // 2000 is a leap year (divisible by 400): Feb 29 exists, so 2000-03-01 is day 60.
        assert_eq!(system_time_to_iso(at(951_868_800)), "2000-03-01T00:00:00Z");
    }

    #[test]
    fn unix_ms_to_iso_drops_milliseconds() {
        assert_eq!(unix_ms_to_iso(1_709_210_096_999), "2024-02-29T12:34:56Z");
    }
}
