//! One system-wide monotonic clock that separate processes of this user can compare: an
//! ephemeral same-host deadline (`xshelld pair` → the Desktop owner), never a network
//! value. Unix: `CLOCK_MONOTONIC`. Windows: the performance counter, which Windows keeps
//! consistent across processors and processes.
//!
//! [`now_ns_floor`] never reads later than the true time and [`now_ns_ceil`] never earlier:
//! a capture uses the floor and a receipt the ceiling, so conversion can only shorten a
//! transmitted budget.

/// The current reading, rounded down to whole nanoseconds.
pub fn now_ns_floor() -> Result<u64, String> {
    sys::now_ns(false)
}

/// The current reading, rounded up to whole nanoseconds.
pub fn now_ns_ceil() -> Result<u64, String> {
    sys::now_ns(true)
}

/// `ticks` of a `frequency` Hz counter in nanoseconds, rounded as asked, overflow-checked.
pub fn ticks_to_ns(ticks: u64, frequency: u64, ceil: bool) -> Result<u64, String> {
    if frequency == 0 {
        return Err("invalid monotonic counter frequency".into());
    }
    let scaled = u128::from(ticks)
        .checked_mul(1_000_000_000)
        .ok_or("monotonic clock overflow")?;
    let frequency = u128::from(frequency);
    let ns = if ceil {
        scaled.div_ceil(frequency)
    } else {
        scaled / frequency
    };
    u64::try_from(ns).map_err(|_| "monotonic clock overflow".into())
}

#[cfg(unix)]
mod sys {
    pub(super) fn now_ns(_ceil: bool) -> Result<u64, String> {
        let mut value: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } != 0 {
            return Err("Unix monotonic clock unavailable".into());
        }
        let seconds = u64::try_from(value.tv_sec).map_err(|_| "invalid monotonic seconds")?;
        let nanos = u64::try_from(value.tv_nsec).map_err(|_| "invalid monotonic nanoseconds")?;
        if nanos >= 1_000_000_000 {
            return Err("invalid monotonic nanoseconds".into());
        }
        seconds
            .checked_mul(1_000_000_000)
            .and_then(|s| s.checked_add(nanos))
            .ok_or_else(|| "monotonic clock overflow".into())
    }
}

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };
    pub(super) fn now_ns(ceil: bool) -> Result<u64, String> {
        let (mut frequency, mut counter) = (0i64, 0i64);
        if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 || frequency <= 0 {
            return Err("Windows performance counter unavailable".into());
        }
        if unsafe { QueryPerformanceCounter(&mut counter) } == 0 || counter < 0 {
            return Err("Windows performance counter unavailable".into());
        }
        super::ticks_to_ns(counter as u64, frequency as u64, ceil)
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    pub(super) fn now_ns(_ceil: bool) -> Result<u64, String> {
        Err("monotonic clock adapter unavailable".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversion_rounds_as_asked_and_checks_overflow() {
        assert_eq!(ticks_to_ns(1, 3, false).unwrap(), 333_333_333);
        assert_eq!(ticks_to_ns(1, 3, true).unwrap(), 333_333_334);
        assert_eq!(
            ticks_to_ns(10_000_000, 10_000_000, true).unwrap(),
            1_000_000_000
        );
        assert!(ticks_to_ns(1, 0, false).is_err());
        assert!(ticks_to_ns(u64::MAX, 1, false).is_err());
    }
    #[test]
    fn readings_are_monotonic_and_floor_never_exceeds_ceil() {
        let a = now_ns_floor().unwrap();
        let b = now_ns_ceil().unwrap();
        let c = now_ns_floor().unwrap();
        assert!(a <= b && a <= c, "{a} {b} {c}");
    }
}
