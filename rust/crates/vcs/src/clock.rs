//! The time source of the throttles and caches. Production uses `SystemClock`; tests inject
//! `ManualClock` so a refresh interval or cooldown is crossed by advancing time, not waiting.
use std::{
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};

pub trait Clock: Send + Sync + 'static {
    /// Monotonic time since an arbitrary origin; only differences are meaningful.
    fn monotonic(&self) -> Duration;
    /// Wall-clock time, for timestamps that appear in results.
    fn utc_now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn monotonic(&self) -> Duration {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        ORIGIN.get_or_init(Instant::now).elapsed()
    }

    fn utc_now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to. Starts at 2020-01-01T00:00:00Z.
#[derive(Debug, Default)]
pub struct ManualClock {
    elapsed: Mutex<Duration>,
}

impl ManualClock {
    pub fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
    }
}

impl Clock for ManualClock {
    fn monotonic(&self) -> Duration {
        *self.elapsed.lock().unwrap()
    }

    fn utc_now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(1_577_836_800, 0).unwrap() + self.monotonic()
    }
}
