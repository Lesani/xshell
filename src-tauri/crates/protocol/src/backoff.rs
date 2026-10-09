//! The reconnect schedule shared by every supervisor that dials again after a failure: the
//! Desktop's Host links and the Ring's Relay connection. Pure; no clock, no threads.

use std::time::Duration;

/// The waits before retries, in units: 1, 2, 4 … 32, then 60 for good.
pub const BACKOFF: [u64; 7] = [1, 2, 4, 8, 16, 32, 60];

/// The wait before retry `n` (0-based) in units: 1, 2, 4 … 32, then 60 for good.
pub fn backoff(n: u32, unit: Duration) -> Duration {
    unit * BACKOFF[(n as usize).min(BACKOFF.len() - 1)] as u32
}

/// Consecutive failures, reset only by a connection that stayed up long enough.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub failures: u32,
}

impl Backoff {
    /// A connect attempt failed: count it and return the wait before the next one.
    pub fn failed(&mut self, unit: Duration) -> Duration {
        self.failures += 1;
        backoff(self.failures - 1, unit)
    }

    /// An established connection ended after `up_for`: a stable one resets the count and
    /// retries at once; one that dropped sooner counts as a failure, so a flapping Host
    /// backs off and goes offline like an unreachable one.
    pub fn ended(&mut self, up_for: Duration, stable_after: Duration, unit: Duration) -> Duration {
        if up_for >= stable_after {
            self.failures = 0;
            Duration::ZERO
        } else {
            self.failed(unit)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule() {
        let s: Vec<u64> = (0..8)
            .map(|n| backoff(n, Duration::from_secs(1)).as_secs())
            .collect();
        assert_eq!(s, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(
            backoff(u32::MAX, Duration::from_millis(10)),
            Duration::from_millis(600)
        );
    }

    #[test]
    fn reset_only_after_stable() {
        let unit = Duration::from_secs(1);
        let stable = Duration::from_secs(10);
        let mut b = Backoff::default();
        assert_eq!(b.failed(unit), Duration::from_secs(1));
        assert_eq!(b.failed(unit), Duration::from_secs(2));
        assert_eq!(b.failed(unit), Duration::from_secs(4));
        // Connected, dropped after 3 s: counts as a failure and escalates.
        assert_eq!(
            b.ended(Duration::from_secs(3), stable, unit),
            Duration::from_secs(8)
        );
        assert_eq!(b.failures, 4);
        assert_eq!(
            b.ended(Duration::from_secs(1), stable, unit),
            Duration::from_secs(16)
        );
        assert_eq!(b.failed(unit), Duration::from_secs(32));
        // Up for 10 s: reset, retry at once.
        assert_eq!(b.ended(stable, stable, unit), Duration::ZERO);
        assert_eq!(b.failures, 0);
        assert_eq!(b.failed(unit), Duration::from_secs(1));
    }
}
