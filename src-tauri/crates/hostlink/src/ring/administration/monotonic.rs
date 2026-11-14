//! Ephemeral same-host Unix deadline, never a network proposal or durable journal field.
use std::time::{Duration, Instant};
const MAX: Duration = Duration::from_secs(600);
#[cfg(unix)]
pub(super) fn now_ns() -> Result<u64, String> {
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
#[cfg(not(unix))]
pub(super) fn now_ns() -> Result<u64, String> {
    Err("Unix monotonic adapter unavailable".into())
}
fn capture_from_samples(tick: u64, remaining: Duration) -> Result<u64, String> {
    if remaining.is_zero() || remaining > MAX {
        return Err("invalid original Pair budget".into());
    }
    tick.checked_add(u64::try_from(remaining.as_nanos()).map_err(|_| "Pair deadline overflow")?)
        .ok_or_else(|| "Pair deadline overflow".into())
}
/// Sample the shared clock FIRST, then the remaining original Instant budget: elapsed
/// time between samples can only shorten the transmitted deadline, never extend it.
pub fn capture(until: Instant) -> Result<u64, String> {
    let tick = now_ns()?;
    let remaining = until
        .checked_duration_since(Instant::now())
        .ok_or("Pair deadline expired")?;
    capture_from_samples(tick, remaining)
}
fn remaining_from_samples(bound: u64, tick: u64) -> Result<Duration, String> {
    let left = bound
        .checked_sub(tick)
        .filter(|n| *n > 0)
        .ok_or("origin Pair deadline expired")?;
    let left = Duration::from_nanos(left);
    if left > MAX {
        return Err("origin Pair deadline exceeds ten minutes".into());
    }
    Ok(left)
}
/// The caller samples local Instant BEFORE this shared-clock read. The derived local
/// deadline is conservative even if dispatch was delayed or the origin stopped polling.
pub(super) fn receive_at(
    bound: u64,
    earlier_instant: Instant,
    later_tick: u64,
) -> Result<Instant, String> {
    earlier_instant
        .checked_add(remaining_from_samples(bound, later_tick)?)
        .ok_or_else(|| "Pair deadline overflow".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_dispatch_and_stalled_origin_cannot_renew_original_bound() {
        let bound = capture_from_samples(1_000, Duration::from_nanos(500)).unwrap();
        assert_eq!(
            remaining_from_samples(bound, 1_100).unwrap(),
            Duration::from_nanos(400)
        );
        assert_eq!(
            remaining_from_samples(bound, 1_400).unwrap(),
            Duration::from_nanos(100)
        );
        assert!(remaining_from_samples(bound, 1_500).is_err());
        assert!(remaining_from_samples(bound, 1_501).is_err());
    }
    #[test]
    fn malformed_overflow_expired_and_subsecond_bounds_are_conservative() {
        assert!(capture_from_samples(u64::MAX, Duration::from_nanos(1)).is_err());
        assert!(capture_from_samples(10, Duration::ZERO).is_err());
        assert!(remaining_from_samples(0, 0).is_err());
        assert!(remaining_from_samples(u64::MAX, 1).is_err());
        let bound = capture_from_samples(1_000, Duration::from_millis(200)).unwrap();
        assert_eq!(
            remaining_from_samples(bound, 1_000).unwrap(),
            Duration::from_millis(200)
        );
        // Conversion to the protocol's whole-second absolute expiry must not round up.
        assert_eq!(Duration::from_millis(200).as_secs(), 0);
    }
}
