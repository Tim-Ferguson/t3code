//! Shared by the integration tests, each of which includes it with `mod support;`.

use std::time::Duration;

/// How long a test waits for something that is certain to happen before it calls it a hang. It bounds a wait for a real
/// event, reply or request outcome so a defect fails the test instead of hanging the run; it is never part of what a test
/// asserts, so a test must pass whatever its value, and it is long enough to outlast the heaviest machine load the suite
/// is run under.
pub const HANG_BOUND: Duration = Duration::from_secs(120);
