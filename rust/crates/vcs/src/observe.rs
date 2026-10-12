//! A hook the embedding server installs to learn of every git command this crate runs, for its counter and timer
//! (`gitCommandsTotal` and `gitCommandDuration` of the original's `Metrics.ts`, recorded by `GitVcsDriverCore.execute`). The crate
//! cannot reach the server's exporters, so it reports each command to the observer and the server decides what to do with them.
//!
//! The observer is process-wide, like the exporters it feeds; installing one replaces the last. A command is reported once, when it
//! ends: `Success` when it ran to an answer (a non-zero exit the caller allowed counts), `Failure` when it ended in an error, and
//! `Interrupt` when the future running it was dropped first.
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// How a git command ended (`outcomeFromExit` of the original).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCommandOutcome {
    Success,
    Failure,
    Interrupt,
}

impl GitCommandOutcome {
    /// The attribute value the original records.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Interrupt => "interrupt",
        }
    }
}

/// One finished git command.
#[derive(Debug, Clone, PartialEq)]
pub struct GitCommandObservation {
    /// The operation name the caller gave the command (`GitVcsDriver.isInsideWorkTree`, `GitCore.status.branchList`...).
    pub operation: String,
    pub outcome: GitCommandOutcome,
    /// From the start of the process work (after any wait for a place in the queue) to the end, on the monotonic clock.
    pub elapsed: Duration,
}

pub type GitCommandObserver = Arc<dyn Fn(&GitCommandObservation) + Send + Sync>;

static OBSERVER: RwLock<Option<GitCommandObserver>> = RwLock::new(None);

/// Installs the observer every git command is reported to, replacing the last; `None` removes it.
pub fn set_git_command_observer(observer: Option<GitCommandObserver>) {
    *OBSERVER.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = observer;
}

/// A command in flight. Reports `Interrupt` if it is dropped before `finish`.
pub(crate) struct Observation {
    operation: String,
    started: Instant,
    reported: bool,
}

pub(crate) fn start(operation: &str) -> Observation {
    Observation { operation: operation.to_owned(), started: Instant::now(), reported: false }
}

impl Observation {
    fn report(&mut self, outcome: GitCommandOutcome) {
        if std::mem::replace(&mut self.reported, true) {
            return;
        }
        let observer = OBSERVER.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if let Some(observer) = observer {
            observer(&GitCommandObservation { operation: std::mem::take(&mut self.operation), outcome, elapsed: self.started.elapsed() });
        }
    }

    pub(crate) fn finish(mut self, succeeded: bool) {
        self.report(if succeeded { GitCommandOutcome::Success } else { GitCommandOutcome::Failure });
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        self.report(GitCommandOutcome::Interrupt);
    }
}
