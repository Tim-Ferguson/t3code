//! The hook the server installs to count and time git commands (`gitCommandsTotal` and `gitCommandDuration` of the original): every
//! command the core and the driver run is reported once, when it ends, with how it ended. One test, so the process-wide observer
//! sees one case at a time.
mod common;

use std::sync::{Arc, Mutex};

use common::{HANG_BOUND, LiveRunner, Repo, RunResult, live_core, live_git};
use futures_util::future::BoxFuture;
use t3_vcs::{
    GitCommand, GitCommandObservation, GitCommandOutcome,
    core::{ExecuteGitInput, GitCore},
    process::{ProcessRunInput, ProcessRunner},
    set_git_command_observer,
};

/// A runner whose commands start and never end, so one can be dropped in flight.
struct Hangs {
    started: Arc<tokio::sync::Notify>,
}

impl ProcessRunner for Hangs {
    fn run(&self, _input: ProcessRunInput) -> BoxFuture<'_, RunResult> {
        let started = self.started.clone();
        Box::pin(async move {
            started.notify_one();
            std::future::pending().await
        })
    }
}

fn git(operation: &str, repo: &Repo, args: &[&str], allow_non_zero_exit: bool) -> ExecuteGitInput {
    ExecuteGitInput { operation: operation.into(), cwd: repo.path().to_path_buf(), args: args.iter().map(|arg| (*arg).to_owned()).collect(), allow_non_zero_exit, ..Default::default() }
}

#[tokio::test]
async fn every_command_is_reported_once_with_how_it_ended() {
    let seen: Arc<Mutex<Vec<GitCommandObservation>>> = Arc::default();
    let sink = seen.clone();
    set_git_command_observer(Some(Arc::new(move |observation: &GitCommandObservation| sink.lock().unwrap().push(observation.clone()))));
    let repo = Repo::with_commit();
    let take = || std::mem::take(&mut *seen.lock().unwrap());

    // The core: a command that answers, one that answers with a non-zero exit the caller allowed, and one that fails.
    let core = live_core();
    core.execute(git("Core.answers", &repo, &["rev-parse", "HEAD"], false)).await.unwrap();
    core.execute(git("Core.allowed", &repo, &["rev-parse", "--verify", "no-such-ref"], true)).await.unwrap();
    core.execute(git("Core.fails", &repo, &["rev-parse", "--verify", "no-such-ref"], false)).await.unwrap_err();
    let reported = take();
    assert_eq!(reported.iter().map(|observation| (observation.operation.as_str(), observation.outcome)).collect::<Vec<_>>(), [("Core.answers", GitCommandOutcome::Success), ("Core.allowed", GitCommandOutcome::Success), ("Core.fails", GitCommandOutcome::Failure)]);

    // The driver has its own path to the process layer, and reports as the core does.
    let driver = live_git();
    assert!(driver.is_inside_work_tree(repo.path()).await.unwrap());
    let refused = driver.execute(GitCommand::new("Driver.fails", repo.path(), ["rev-parse", "--verify", "no-such-ref"])).await;
    assert!(refused.is_err());
    let reported = take();
    assert_eq!(reported.iter().map(|observation| (observation.operation.as_str(), observation.outcome)).collect::<Vec<_>>(), [("GitVcsDriver.isInsideWorkTree", GitCommandOutcome::Success), ("Driver.fails", GitCommandOutcome::Failure)]);

    // A command whose future is dropped before it ends is an interrupt, reported at once and only once.
    let started = Arc::new(tokio::sync::Notify::new());
    let hanging = GitCore::new(Arc::new(Hangs { started: started.clone() }));
    let input = git("Core.interrupted", &repo, &["status"], false);
    let task = tokio::spawn(async move { hanging.execute(input).await });
    tokio::time::timeout(HANG_BOUND, started.notified()).await.expect("the command started");
    assert!(take().is_empty(), "nothing is reported while it runs");
    task.abort();
    let _ = task.await;
    let reported = take();
    assert_eq!(reported.len(), 1);
    assert_eq!((reported[0].operation.as_str(), reported[0].outcome), ("Core.interrupted", GitCommandOutcome::Interrupt));

    // No observer, no report; the commands still run.
    set_git_command_observer(None);
    GitCore::new(Arc::new(LiveRunner)).execute(git("Core.unobserved", &repo, &["rev-parse", "HEAD"], false)).await.unwrap();
    assert!(take().is_empty());
}
