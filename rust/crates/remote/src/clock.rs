//! Time for the runtime. Wall-clock reads (token expiry, JWT claims, marker ages) go through
//! [`Clock`]; waiting goes through `tokio::time`, so a test that pauses tokio's clock
//! (`start_paused`) drives both: [`VirtualClock`] advances with it.

use std::time::{SystemTime, UNIX_EPOCH};

pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;

    fn now_seconds(&self) -> i64 {
        self.now_ms().div_euclid(1_000)
    }
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// Epoch time that moves with tokio's (possibly paused) clock.
pub struct VirtualClock {
    base_ms: i64,
    start: tokio::time::Instant,
}

impl VirtualClock {
    pub fn new(base_ms: i64) -> Self {
        Self {
            base_ms,
            start: tokio::time::Instant::now(),
        }
    }
}

impl Clock for VirtualClock {
    fn now_ms(&self) -> i64 {
        self.base_ms + self.start.elapsed().as_millis() as i64
    }
}

/// `DateTime.formatIso`: `2026-01-02T03:04:05.678Z`.
pub fn format_iso(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}
