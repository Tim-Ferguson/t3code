//! Shared by the unit tests (`crate::test_support`) and the integration tests (`t3_server::test_support`); compiled only for
//! tests, through `cfg(test)` or the `outbound-check` feature the dev-dependency switches on.

use std::time::Duration;

/// How long a test waits for something that is certain to happen before it calls it a hang. It bounds a wait for a real event
/// or reply so a defect fails the test instead of hanging the run; it is never part of what a test asserts, so a test must
/// pass whatever its value, and it is long enough to outlast the heaviest machine load the suite is run under.
pub const HANG_BOUND: Duration = Duration::from_secs(120);

/// The budget a test gives a command it expects to be killed for running too long, when the command must first start (an
/// interpreter's startup) and report in. The kill is the behaviour under test and the budget counts from the spawn, so it has
/// to outlast the start on a loaded machine; the test takes this long to finish, so it is shorter than [`HANG_BOUND`].
pub const KILL_AFTER: Duration = Duration::from_secs(20);
