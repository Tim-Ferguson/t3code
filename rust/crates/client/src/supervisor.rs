//! What keeps an environment connected: when to try again after a failure, how a lost connection moves among the routes a saved
//! environment has, and how long a connection must hold before the failures before it are forgotten (port of the pure parts of
//! `packages/client-runtime/src/connection/supervisor.ts`). Nothing here reads a clock, a random source, a socket or a disk: the
//! caller supplies the time and the random number, which is what lets a test run an hour of retries in an instant.

/// `RETRY_BASE_DELAY_MS`.
pub const RETRY_BASE_DELAY_MS: u64 = 1_000;
/// `RETRY_MAX_DELAY_MS`: the longest a connection that keeps failing waits between attempts.
pub const RETRY_MAX_DELAY_MS: u64 = 300_000;
/// `BACKOFF_RESET_AFTER_MS`: a connection that held this long was a good one, and the failures before it are forgotten.
pub const BACKOFF_RESET_AFTER_MS: u64 = 30_000;

/// `retryDelayMs`: the delay before the next attempt after `failure_count` consecutive failures (0 for the first retry). The ceiling
/// doubles from 2 s up to five minutes, and the delay is a random point in its upper half, so that clients that lost the same server
/// do not all come back in the same second. `random` is in [0, 1).
pub fn retry_delay_ms(failure_count: u32, random: f64) -> u64 {
    let ceiling = RETRY_MAX_DELAY_MS.min(RETRY_BASE_DELAY_MS.saturating_mul(1u64.checked_shl(failure_count.saturating_add(1)).unwrap_or(u64::MAX)));
    let ceiling = ceiling as f64;
    (ceiling / 2.0 + (ceiling / 2.0) * random.clamp(0.0, 1.0)).round() as u64
}

/// How an environment's connection is doing, as its supervisor sees it (`SupervisorConnectionPhase`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Not wanted: switched off, or not started.
    Available,
    Connecting,
    Connected,
    /// Waiting to try again at `retry_at` (milliseconds since the Unix epoch), after `failures` attempts in a row failed.
    Backoff { retry_at: u64, failures: u32 },
    /// Refused for a reason retrying will not change (the token, a different server at the address, a protocol it cannot speak).
    Blocked(String),
}

/// The count of consecutive failures and when the connection last came up: the whole memory of a supervisor's retry policy.
#[derive(Debug, Clone, Default)]
pub struct Backoff {
    failures: u32,
    connected_at: Option<u64>,
}

impl Backoff {
    pub fn failures(&self) -> u32 {
        self.failures
    }
    /// A connection came up at `now` (milliseconds).
    pub fn established(&mut self, now: u64) {
        self.connected_at = Some(now);
    }
    /// A connection or an attempt failed at `now`; the delay before the next one. A connection that held for
    /// [`BACKOFF_RESET_AFTER_MS`] starts the ladder over, so one dropped by a restart of the server comes back in a second or two, not
    /// after the five minutes a long outage earned.
    pub fn failed(&mut self, now: u64, random: f64) -> u64 {
        if self.connected_at.take().is_some_and(|since| now.saturating_sub(since) >= BACKOFF_RESET_AFTER_MS) {
            self.failures = 0;
        }
        let delay = retry_delay_ms(self.failures, random);
        self.failures = self.failures.saturating_add(1);
        delay
    }
    /// An explicit retry, the app returning to the foreground, the network coming back: the next attempt starts the ladder over.
    pub fn reset(&mut self) {
        self.failures = 0;
        self.connected_at = None;
    }
}

/// Where the next attempt goes among the routes of an environment, preferred first (`routes.length` of them). A lost connection
/// tries the other routes at once, in preference order from the one after the route that failed, and waits (with backoff) only
/// when every route has failed in a row: `failed_in_a_row` counts the failures since the last time anything connected.
/// Returns the route to try and whether to wait before trying it.
pub fn next_route(routes: usize, current: usize, failed_in_a_row: usize) -> (usize, bool) {
    if routes == 0 {
        return (0, true);
    }
    let next = (current + 1) % routes;
    // A whole pass without a connection: wait before the next pass.
    (next, failed_in_a_row > 0 && failed_in_a_row % routes == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_doubles_from_two_seconds_in_the_upper_half_of_its_ceiling_and_stops_at_five_minutes() {
        // The ceiling is 2 s, 4 s, 8 s, ...; the delay is never under half of it and never over it.
        for (failures, ceiling) in [(0u32, 2_000u64), (1, 4_000), (2, 8_000), (5, 64_000), (7, 256_000), (8, 300_000), (30, 300_000), (u32::MAX, 300_000)] {
            assert_eq!(retry_delay_ms(failures, 0.0), ceiling / 2, "{failures} failures, quickest");
            assert_eq!(retry_delay_ms(failures, 0.999_999_999), ceiling, "{failures} failures, slowest");
            let middle = retry_delay_ms(failures, 0.5);
            assert!(middle > ceiling / 2 && middle < ceiling);
        }
    }

    #[test]
    fn a_connection_that_held_forgets_the_failures_before_it_and_one_that_did_not_does_not() {
        let mut backoff = Backoff::default();
        assert_eq!(backoff.failed(0, 0.0), 1_000);
        assert_eq!(backoff.failed(1_000, 0.0), 2_000);
        assert_eq!(backoff.failed(3_000, 0.0), 4_000);
        // It connects and drops again within the reset time: the ladder goes on.
        backoff.established(10_000);
        assert_eq!(backoff.failed(10_000 + BACKOFF_RESET_AFTER_MS - 1, 0.0), 8_000);
        // It connects and holds: the next failure is a first one.
        backoff.established(100_000);
        assert_eq!(backoff.failed(100_000 + BACKOFF_RESET_AFTER_MS, 0.0), 1_000);
        // An explicit retry starts over too.
        backoff.failed(0, 0.0);
        backoff.reset();
        assert_eq!((backoff.failures(), backoff.failed(0, 0.0)), (0, 1_000));
    }

    #[test]
    fn a_lost_connection_tries_every_route_before_it_waits() {
        // Three routes, the first (index 0) was connected and is lost: the others are tried at once, then a wait, then the first again.
        assert_eq!(next_route(3, 0, 1), (1, false));
        assert_eq!(next_route(3, 1, 2), (2, false));
        assert_eq!(next_route(3, 2, 3), (0, true));
        assert_eq!(next_route(3, 0, 4), (1, false));
        // One route has only the wait.
        assert_eq!(next_route(1, 0, 1), (0, true));
        assert_eq!(next_route(0, 0, 1), (0, true));
    }
}
