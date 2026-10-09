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

#[cfg(test)]
mod tests {
    use super::*;

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
