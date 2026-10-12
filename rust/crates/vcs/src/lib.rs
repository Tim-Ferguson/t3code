//! Git/VCS foundation for the Rust server.
//!
//! - `process`: running the git binary (`VcsProcess`), with the original's limits, error
//!   classification, concurrency bound and checkpoint-capture retry.
//! - `checkpoint`: hidden-ref workspace checkpoints (`CheckpointStore`, `GitVcs` checkpoint
//!   methods) and turn-diff summaries. Ref names and git command sequences match the
//!   TypeScript server.
//! - `git`: the `VcsDriver` surface for git (`GitVcs`): repository detection, workspace files,
//!   remotes, ignore filtering.
//! - `core`, `status`: `GitCore`, the branch/worktree/status half of `GitVcsDriverCore.ts`,
//!   which fails with `GitCommandError` rather than `VcsError`.
//! - `refs`, `remote`, `review`, `status`, `state`: the rest of `GitCore` - ref listing and its
//!   snapshot caches, upstream status and the throttled background fetch, commit/push/pull/
//!   fetch, review diffs. `GitTimings` and an injectable `Clock` set every cache lifetime.
//! - `wire`: serde shapes of `packages/contracts` (`git.ts`, `vcs.ts`, `review.ts`) for the
//!   results and inputs of the above.
//! - `progress`: line and hook progress callbacks for long commands.
//! - `project_config`: `.t3code/vcs.json` kind selection.
mod cache;
pub mod checkpoint;
pub mod clock;
pub mod core;
pub mod error;
pub mod observe;
pub mod git;
pub mod process;
pub mod progress;
pub mod project_config;
pub mod refs;
pub mod remote;
pub mod remote_refs;
pub mod review;
mod state;
pub mod status;
mod temp;
pub mod wire;

pub use checkpoint::{
    CheckpointStore, DiffCheckpointsInput, DiffFormat, TurnDiffFileSummary,
    checkpoint_ref_for_thread_turn, parse_turn_diff_files_from_numstat,
};
pub use clock::{Clock, ManualClock, SystemClock};
pub use core::{GitCommandError, GitCore};
pub use error::{VcsDriverKind, VcsError};
pub use git::{GitCommand, GitVcs};
pub use observe::{GitCommandObservation, GitCommandObserver, GitCommandOutcome, set_git_command_observer};
pub use state::{GitTimings, status_upstream_refresh_failure_cooldown};
pub use process::{VcsProcess, VcsProcessInput, VcsProcessOutput};
