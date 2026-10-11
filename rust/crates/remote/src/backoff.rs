//! The retry schedules the original builds from Effect's `Schedule` (`Schedule.exponential("1
//! second")` capped at 30 s, optionally jittered or bounded by a total time).

use std::time::Duration;

const BASE: Duration = Duration::from_secs(1);
const CAP: Duration = Duration::from_secs(30);

/// Source of randomness for jitter: a value in `[0, 1)`. Injected so tests are deterministic.
pub trait Random: Send + Sync {
    fn next_f64(&self) -> f64;
}

pub struct ThreadRandom;

impl Random for ThreadRandom {
    fn next_f64(&self) -> f64 {
        rand::random::<f64>()
    }
}

/// Always the same value, for tests.
pub struct FixedRandom(pub f64);

impl Random for FixedRandom {
    fn next_f64(&self) -> f64 {
        self.0
    }
}

/// `exponential(1 s)` with each delay capped at 30 s: 1, 2, 4, 8, 16, 30, 30, ...
pub fn capped_exponential_delay(attempt: u32) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    BASE.saturating_mul(u32::try_from(factor).unwrap_or(u32::MAX))
        .min(CAP)
}

/// `Schedule.jittered`: the delay scaled by a random factor between 0.8 and 1.2. The cap applies
/// before jitter, as in the original, so the longest wait is 36 s.
pub fn jittered(delay: Duration, random: &dyn Random) -> Duration {
    delay.mul_f64(0.8 + 0.4 * random.next_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_and_cap_at_thirty_seconds() {
        let delays: Vec<u64> = (0..8)
            .map(|n| capped_exponential_delay(n).as_secs())
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(capped_exponential_delay(200).as_secs(), 30);
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        let low = jittered(Duration::from_secs(10), &FixedRandom(0.0));
        let high = jittered(Duration::from_secs(10), &FixedRandom(0.999_999));
        assert_eq!(low, Duration::from_secs(8));
        assert!(high > Duration::from_millis(11_990) && high <= Duration::from_secs(12));
    }
}
