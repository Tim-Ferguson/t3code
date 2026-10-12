//! Ownership and ordering of a server tab's control (apps/server/src/preview/SessionControl.test.ts), with events in place of timers.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::oneshot;

#[derive(Debug, PartialEq)]
enum Failure {
    Control(PreviewAutomationControlReason, String),
    Plain(&'static str),
}

impl From<BrowserControlInterrupted> for Failure {
    fn from(error: BrowserControlInterrupted) -> Self {
        Self::Control(error.reason, error.message)
    }
}

type Log = Arc<Mutex<Vec<&'static str>>>;

fn control(agent: &str) -> SessionControl {
    SessionControl::new(Some(agent.to_owned()), || {})
}

fn log() -> Log {
    Arc::default()
}

fn push(log: &Log, event: &'static str) {
    log.lock().unwrap().push(event);
}

fn events(log: &Log) -> Vec<&'static str> {
    log.lock().unwrap().clone()
}

fn interrupted<T: std::fmt::Debug>(result: Result<T, Failure>) -> PreviewAutomationControlReason {
    match result {
        Err(Failure::Control(reason, _)) => reason,
        other => panic!("expected a control failure, got {other:?}"),
    }
}

#[tokio::test]
async fn an_action_returns_before_its_tracked_navigation_but_drains_it_before_queued_actions() {
    let control = control("agent");
    let (commit, committed) = oneshot::channel::<()>();
    let log = log();
    let (navigation_log, next_log) = (log.clone(), log.clone());
    let tracked = control.clone();
    let navigation = control.agent::<_, Failure, _, _>("agent", move || async move {
        tracked.track(async move {
            let _ = committed.await;
            push(&navigation_log, "navigation committed");
        });
        Ok("started")
    });
    let next = control.agent::<_, Failure, _, _>("agent", move || async move {
        push(&next_log, "next action");
        Ok(())
    });
    assert_eq!(navigation.await, Ok("started"));
    assert!(events(&log).is_empty());
    commit.send(()).unwrap();
    next.await.unwrap();
    assert_eq!(events(&log), ["navigation committed", "next action"]);
}

#[tokio::test]
async fn a_failed_tracked_navigation_does_not_poison_the_next_action() {
    let control = control("agent");
    let (commit, committed) = oneshot::channel::<()>();
    let tracked = control.clone();
    control
        .agent::<_, Failure, _, _>("agent", move || async move {
            tracked.track(async move {
                // The sender is dropped, so this navigation ends without committing.
                let _ = committed.await;
            });
            Ok(())
        })
        .await
        .unwrap();
    drop(commit);
    assert_eq!(control.agent::<_, Failure, _, _>("agent", || async { Ok("resumed") }).await, Ok("resumed"));
}

#[tokio::test]
async fn only_the_assigned_agent_acts_and_it_is_restored_after_human_control() {
    let control = control("agent-a");
    use PreviewAutomationControlReason::*;
    assert_eq!(interrupted(control.agent::<_, Failure, _, _>("agent-b", || async { Ok("wrong agent") }).await), AgentMismatch);
    assert_eq!(control.agent::<_, Failure, _, _>("agent-a", || async { Ok("first") }).await, Ok("first"));
    control.take("viewer-a").await.unwrap();
    assert_eq!(interrupted(control.agent::<_, Failure, _, _>("agent-a", || async { Ok("racing") }).await), HumanControl);
    assert_eq!(control.human::<_, Failure, _, _>("viewer-a", || async { Ok("typed") }).await, Ok("typed"));
    control.release::<Failure, _, _>("viewer-a", || async { Ok(()) }).await.unwrap();
    assert_eq!(control.agent::<_, Failure, _, _>("agent-a", || async { Ok("resumed") }).await, Ok("resumed"));
    assert_eq!(interrupted(control.agent::<_, Failure, _, _>("agent-b", || async { Ok("wrong agent") }).await), AgentMismatch);
}

#[tokio::test]
async fn a_takeover_drains_running_work_and_cancels_actions_queued_before_it() {
    let control = control("agent");
    let (started, has_started) = oneshot::channel::<()>();
    let (finish, finished) = oneshot::channel::<()>();
    let log = log();
    let running_log = log.clone();
    let running = tokio::spawn(control.agent::<_, Failure, _, _>("agent", move || async move {
        push(&running_log, "agent started");
        started.send(()).unwrap();
        let _ = finished.await;
        push(&running_log, "agent finished");
        Ok(())
    }));
    has_started.await.unwrap();
    let stale_log = log.clone();
    let queued = control.agent::<_, Failure, _, _>("agent", move || async move {
        push(&stale_log, "stale agent action");
        Ok(())
    });
    let takeover = control.take("viewer");
    let typing_log = log.clone();
    let typing = control.human::<_, Failure, _, _>("viewer", move || async move {
        push(&typing_log, "human typed");
        Ok(())
    });
    assert_eq!(interrupted(control.agent::<_, Failure, _, _>("agent", || async { Ok("late action") }).await), PreviewAutomationControlReason::HumanControl);
    assert_eq!(events(&log), ["agent started"]);
    finish.send(()).unwrap();
    running.await.unwrap().unwrap();
    assert_eq!(interrupted(queued.await), PreviewAutomationControlReason::Interrupted);
    takeover.await.unwrap();
    typing.await.unwrap();
    assert_eq!(events(&log), ["agent started", "agent finished", "human typed"]);
}

#[tokio::test]
async fn a_second_viewer_cannot_steal_type_release_or_disconnect_the_owner() {
    let control = control("agent");
    let takeover = control.take("viewer-a");
    assert_eq!(control.take("viewer-b").await.unwrap_err().message, "Another viewer controls this browser tab.");
    takeover.await.unwrap();
    assert_eq!(interrupted(control.human::<_, Failure, _, _>("viewer-b", || async { Ok("wrong viewer") }).await), PreviewAutomationControlReason::Interrupted);
    assert_eq!(interrupted(control.release::<Failure, _, _>("viewer-b", || async { Ok(()) }).await.map(|()| 0)), PreviewAutomationControlReason::Interrupted);
    control.disconnect::<Failure, _, _>("viewer-b", || async { Ok(()) }).await.unwrap();
    assert_eq!(control.controller().as_deref(), Some("viewer-a"));
    assert_eq!(control.human::<_, Failure, _, _>("viewer-a", || async { Ok("still controlled") }).await, Ok("still controlled"));
}

#[tokio::test]
async fn a_disconnect_invalidates_queued_human_actions_and_refs() {
    let invalidations = Arc::new(AtomicUsize::new(0));
    let counted = invalidations.clone();
    let control = SessionControl::new(Some("agent".into()), move || {
        counted.fetch_add(1, Ordering::SeqCst);
    });
    control.take("viewer").await.unwrap();
    let (started, has_started) = oneshot::channel::<()>();
    let (finish, finished) = oneshot::channel::<()>();
    let log = log();
    let running_log = log.clone();
    let running = tokio::spawn(control.human::<_, Failure, _, _>("viewer", move || async move {
        started.send(()).unwrap();
        let _ = finished.await;
        push(&running_log, "human finished");
        Ok(())
    }));
    has_started.await.unwrap();
    let stale_log = log.clone();
    let queued = control.human::<_, Failure, _, _>("viewer", move || async move {
        push(&stale_log, "stale human action");
        Ok(())
    });
    let disconnected = control.disconnect::<Failure, _, _>("viewer", || async { Ok(()) });
    let resumed_log = log.clone();
    let resumed = control.agent::<_, Failure, _, _>("agent", move || async move {
        push(&resumed_log, "agent resumed");
        Ok(())
    });
    assert_eq!((control.generation(), invalidations.load(Ordering::SeqCst)), (2, 2));
    finish.send(()).unwrap();
    running.await.unwrap().unwrap();
    assert_eq!(interrupted(queued.await), PreviewAutomationControlReason::Interrupted);
    disconnected.await.unwrap();
    resumed.await.unwrap();
    assert_eq!(events(&log), ["human finished", "agent resumed"]);
}

#[tokio::test]
async fn a_takeover_released_before_running_work_finishes_revives_no_stale_action() {
    let control = control("agent");
    let (started, has_started) = oneshot::channel::<()>();
    let (finish, finished) = oneshot::channel::<()>();
    let running = tokio::spawn(control.agent::<_, Failure, _, _>("agent", move || async move {
        started.send(()).unwrap();
        let _ = finished.await;
        Ok(())
    }));
    has_started.await.unwrap();
    let stale_agent = control.agent::<_, Failure, _, _>("agent", || async { Ok("stale") });
    let takeover = control.take("viewer");
    let stale_human = control.human::<_, Failure, _, _>("viewer", || async { Ok("stale") });
    let release = control.release::<Failure, _, _>("viewer", || async { Ok(()) });
    let resumed = control.agent::<_, Failure, _, _>("agent", || async { Ok("fresh") });
    finish.send(()).unwrap();
    running.await.unwrap().unwrap();
    assert_eq!(interrupted(stale_agent.await), PreviewAutomationControlReason::Interrupted);
    assert_eq!(takeover.await.unwrap_err().reason, PreviewAutomationControlReason::Interrupted);
    assert_eq!(interrupted(stale_human.await), PreviewAutomationControlReason::Interrupted);
    release.await.unwrap();
    assert_eq!(resumed.await, Ok("fresh"));
}

#[tokio::test]
async fn closing_waits_for_running_work_and_nothing_queued_or_new_runs() {
    let control = control("agent");
    let (started, has_started) = oneshot::channel::<()>();
    let (finish, finished) = oneshot::channel::<()>();
    let running = tokio::spawn(control.agent::<_, Failure, _, _>("agent", move || async move {
        started.send(()).unwrap();
        let _ = finished.await;
        Ok("finished")
    }));
    has_started.await.unwrap();
    let queued = control.agent::<_, Failure, _, _>("agent", || async { Ok("stale") });
    let closed = control.close();
    assert_eq!(control.take("viewer").await.unwrap_err().reason, PreviewAutomationControlReason::Closed);
    assert_eq!(interrupted(control.agent::<_, Failure, _, _>("agent", || async { Ok("new") }).await), PreviewAutomationControlReason::Closed);
    finish.send(()).unwrap();
    assert_eq!(running.await.unwrap(), Ok("finished"));
    assert_eq!(interrupted(queued.await), PreviewAutomationControlReason::Closed);
    closed.await;
    control.disconnect::<Failure, _, _>("viewer", || async { Ok(()) }).await.unwrap();
    control.close().await;
    assert_eq!(control.generation(), 1);
}

#[tokio::test]
async fn a_failed_action_does_not_poison_the_serial_queue() {
    let control = control("agent");
    assert_eq!(control.agent::<(), Failure, _, _>("agent", || async { Err(Failure::Plain("navigation failed")) }).await, Err(Failure::Plain("navigation failed")));
    assert_eq!(control.agent::<_, Failure, _, _>("agent", || async { Ok("recovered") }).await, Ok("recovered"));
}
