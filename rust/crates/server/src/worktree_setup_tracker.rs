//! Live stages of a thread's worktree setup, so clients can show a progress card while the first
//! turn is still being prepared (port of `project/WorktreeSetupTracker.ts`).
//!
//! State is memory only. It exists from `begin` until the turn starts or the setup fails, plus a
//! short retention window so a client that subscribes late still sees the outcome. The durable
//! record of a setup is the thread's worktree path and the preparation turn items.
use chrono::{SecondsFormat, Utc};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::{
    NonNegativeInt, ThreadId, TrimmedNonEmptyString, WORKTREE_SETUP_DETAIL_MAX_LENGTH,
    WORKTREE_SETUP_ERROR_MAX_LENGTH, WORKTREE_SETUP_STAGE_ORDER, WORKTREE_SETUP_TAIL_LINE_MAX_LENGTH,
    WorktreeSetupPhase, WorktreeSetupScript, WorktreeSetupSnapshot, WorktreeSetupStage,
    WorktreeSetupStageId, WorktreeSetupStageStatus,
};
use tokio::sync::watch;

const TAIL_LINE_LIMIT: usize = 4;
/// Finished snapshots stay visible this long so a late subscriber sees the outcome.
const FINISHED_RETENTION: Duration = Duration::from_secs(30);

/// Keeps free text inside the contract limit, ending in an ellipsis when cut. Lengths count
/// UTF-16 code units, as the contract's do.
pub(crate) fn clamp_text(text: &str, max: usize) -> String {
    if text.encode_utf16().count() <= max {
        return text.to_owned();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let width = character.len_utf16();
        if used + width > max - 1 {
            break;
        }
        used += width;
        result.push(character);
    }
    result.push('…');
    result
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn trimmed(value: Option<&str>) -> Option<TrimmedNonEmptyString> {
    value.and_then(|value| TrimmedNonEmptyString::new(value).ok())
}

fn empty_stage(id: WorktreeSetupStageId) -> WorktreeSetupStage {
    WorktreeSetupStage {
        id,
        status: WorktreeSetupStageStatus::Pending,
        started_at: None,
        ended_at: None,
        percent: None,
        detail: None,
        tail: vec![],
    }
}

/// What a running setup holds: the flag that cancels it and the signal that it has unwound.
#[derive(Clone)]
struct Cancel {
    flag: watch::Sender<bool>,
    settled: watch::Receiver<bool>,
}

/// The preparing task's side of a tracked setup.
pub(crate) struct Handle {
    /// Turns true when `cancel` was called.
    pub cancelled: watch::Receiver<bool>,
    /// The task sends true once it has unwound, so `cancel` can wait for it.
    pub settled: watch::Sender<bool>,
}

struct Tracked {
    snapshot: WorktreeSetupSnapshot,
    cancel: Option<Cancel>,
}

#[derive(Default)]
struct State {
    setups: HashMap<String, Tracked>,
    /// Sequences keep increasing across setups of one thread.
    last_sequence: HashMap<String, u64>,
    channels: HashMap<String, watch::Sender<Option<WorktreeSetupSnapshot>>>,
    retention: HashMap<String, tokio::task::JoinHandle<()>>,
}

struct Inner {
    state: Mutex<State>,
    retention: Duration,
}

#[derive(Clone)]
pub struct WorktreeSetupTracker(Arc<Inner>);

impl Default for WorktreeSetupTracker {
    fn default() -> Self {
        Self::with_retention(FINISHED_RETENTION)
    }
}

/// A partial change to one stage; `None` leaves a field alone.
#[derive(Default)]
pub(crate) struct StagePatch {
    pub status: Option<WorktreeSetupStageStatus>,
    pub percent: Option<Option<u8>>,
    pub detail: Option<Option<String>>,
}

impl WorktreeSetupTracker {
    pub fn with_retention(retention: Duration) -> Self {
        Self(Arc::new(Inner { state: Mutex::new(State::default()), retention }))
    }

    fn channel<'a>(
        state: &'a mut State,
        thread: &str,
    ) -> &'a watch::Sender<Option<WorktreeSetupSnapshot>> {
        state.channels.entry(thread.to_owned()).or_insert_with(|| watch::channel(None).0)
    }

    fn publish(state: &mut State, thread: &str, snapshot: Option<WorktreeSetupSnapshot>) {
        Self::channel(state, thread).send_replace(snapshot);
    }

    fn modify(&self, thread: &str, change: impl FnOnce(&mut Tracked)) -> Option<WorktreeSetupSnapshot> {
        let mut state = self.0.state.lock().unwrap();
        let tracked = state.setups.get_mut(thread)?;
        change(tracked);
        tracked.snapshot.sequence = NonNegativeInt(tracked.snapshot.sequence.0 + 1);
        let snapshot = tracked.snapshot.clone();
        state.last_sequence.insert(thread.to_owned(), snapshot.sequence.0);
        Self::publish(&mut state, thread, Some(snapshot.clone()));
        Some(snapshot)
    }

    /// Creates a fresh running snapshot for the thread, replacing any prior one. The returned
    /// handle is how the preparing task observes cancellation.
    pub(crate) fn begin(
        &self,
        thread: &str,
        branch: Option<&str>,
        base_ref: Option<&str>,
        stages: &[WorktreeSetupStageId],
    ) -> Handle {
        let mut state = self.0.state.lock().unwrap();
        if let Some(task) = state.retention.remove(thread) {
            task.abort();
        }
        let sequence = state.last_sequence.get(thread).map_or(0, |last| last + 1);
        let snapshot = WorktreeSetupSnapshot {
            thread_id: ThreadId::new(thread).expect("thread ids are non-blank"),
            phase: WorktreeSetupPhase::Running,
            started_at: now(),
            ended_at: None,
            branch: trimmed(branch),
            base_ref: trimmed(base_ref),
            worktree_path: None,
            setup_script: None,
            stages: WORKTREE_SETUP_STAGE_ORDER
                .iter()
                .filter(|id| stages.contains(id))
                .map(|id| empty_stage(*id))
                .collect(),
            error: None,
            sequence: NonNegativeInt(sequence),
        };
        let (flag, cancelled) = watch::channel(false);
        let (settled, settled_receiver) = watch::channel(false);
        state.last_sequence.insert(thread.to_owned(), sequence);
        state.setups.insert(
            thread.to_owned(),
            Tracked { snapshot: snapshot.clone(), cancel: Some(Cancel { flag, settled: settled_receiver }) },
        );
        Self::publish(&mut state, thread, Some(snapshot));
        Handle { cancelled, settled }
    }

    pub(crate) fn update(&self, thread: &str, change: impl FnOnce(&mut WorktreeSetupSnapshot)) {
        self.modify(thread, |tracked| change(&mut tracked.snapshot));
    }

    /// Records where the checkout landed and which branch it carries.
    pub(crate) fn set_worktree(&self, thread: &str, path: &str, branch: &str) {
        self.update(thread, |snapshot| {
            snapshot.worktree_path = trimmed(Some(path));
            snapshot.branch = trimmed(Some(branch));
        });
    }

    pub(crate) fn set_setup_script(&self, thread: &str, name: &str, command: &str, terminal_id: &str) {
        self.update(thread, |snapshot| {
            snapshot.setup_script = match (trimmed(Some(name)), trimmed(Some(command)), trimmed(Some(terminal_id))) {
                (Some(name), Some(command), Some(terminal_id)) => Some(WorktreeSetupScript { name, command, terminal_id }),
                _ => None,
            };
        });
    }

    pub(crate) fn stage(&self, thread: &str, id: WorktreeSetupStageId, patch: StagePatch) {
        self.update(thread, |snapshot| {
            if let Some(entry) = snapshot.stages.iter_mut().find(|entry| entry.id == id) {
                if let Some(status) = patch.status {
                    entry.status = status;
                }
                if let Some(percent) = patch.percent {
                    entry.percent = percent;
                }
                if let Some(detail) = patch.detail {
                    entry.detail = detail.map(|detail| clamp_text(&detail, WORKTREE_SETUP_DETAIL_MAX_LENGTH));
                }
            }
        });
    }

    pub(crate) fn stage_status(
        &self,
        thread: &str,
        id: WorktreeSetupStageId,
        status: WorktreeSetupStageStatus,
        detail: Option<Option<&str>>,
    ) {
        let at = now();
        self.update(thread, |snapshot| {
            if let Some(entry) = snapshot.stages.iter_mut().find(|entry| entry.id == id) {
                if entry.started_at.is_none() && status != WorktreeSetupStageStatus::Pending {
                    entry.started_at = Some(at.clone());
                }
                entry.ended_at = match status {
                    WorktreeSetupStageStatus::Running | WorktreeSetupStageStatus::Pending => None,
                    _ => entry.ended_at.take().or(Some(at)),
                };
                entry.status = status;
                if let Some(detail) = detail {
                    entry.detail = detail.map(|detail| clamp_text(detail, WORKTREE_SETUP_DETAIL_MAX_LENGTH));
                }
            }
        });
    }

    pub(crate) fn append_tail(&self, thread: &str, id: WorktreeSetupStageId, line: &str) {
        self.update(thread, |snapshot| {
            if let Some(entry) = snapshot.stages.iter_mut().find(|entry| entry.id == id) {
                entry.tail.push(clamp_text(line, WORKTREE_SETUP_TAIL_LINE_MAX_LENGTH));
                if entry.tail.len() > TAIL_LINE_LIMIT {
                    entry.tail.remove(0);
                }
            }
        });
    }

    /// Settles the snapshot, then drops it once the retention window has passed. Returns the
    /// settled snapshot, or `None` when nothing was tracked.
    pub(crate) fn finish(
        &self,
        thread: &str,
        phase: WorktreeSetupPhase,
        error: Option<&str>,
    ) -> Option<WorktreeSetupSnapshot> {
        let ended = now();
        let snapshot = self.modify(thread, |tracked| {
            tracked.cancel = None;
            let snapshot = &mut tracked.snapshot;
            snapshot.phase = phase;
            snapshot.ended_at = Some(ended.clone());
            snapshot.error = error.map(|error| clamp_text(error, WORKTREE_SETUP_ERROR_MAX_LENGTH));
            for entry in snapshot.stages.iter_mut().filter(|entry| entry.status == WorktreeSetupStageStatus::Running) {
                entry.status = match phase {
                    WorktreeSetupPhase::Done => WorktreeSetupStageStatus::Done,
                    WorktreeSetupPhase::Cancelled => WorktreeSetupStageStatus::Skipped,
                    _ => WorktreeSetupStageStatus::Failed,
                };
                entry.ended_at = Some(ended.clone());
            }
        })?;
        let mut state = self.0.state.lock().unwrap();
        if let Some(task) = state.retention.remove(thread) {
            task.abort();
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (tracker, owner, retention) = (self.clone(), thread.to_owned(), self.0.retention);
            state.retention.insert(
                thread.to_owned(),
                runtime.spawn(async move {
                    tokio::time::sleep(retention).await;
                    tracker.remove(&owner);
                }),
            );
        }
        Some(snapshot)
    }

    /// Drops a finished snapshot, unless a newer setup replaced it meanwhile.
    fn remove(&self, thread: &str) {
        let mut state = self.0.state.lock().unwrap();
        if state.setups.get(thread).is_none_or(|tracked| tracked.snapshot.phase == WorktreeSetupPhase::Running) {
            return;
        }
        state.setups.remove(thread);
        state.retention.remove(thread);
        // A subscriber that outlives retention sees null and accepts any sequence after it.
        state.last_sequence.remove(thread);
        Self::publish(&mut state, thread, None);
        if state.channels.get(thread).is_some_and(|channel| channel.receiver_count() == 0) {
            state.channels.remove(thread);
        }
    }

    /// Drops the cancel handle. Called right before the turn is released so a late cancel
    /// cannot roll back a thread whose agent has already started.
    pub(crate) fn mark_uncancellable(&self, thread: &str) {
        if let Some(tracked) = self.0.state.lock().unwrap().setups.get_mut(thread) {
            tracked.cancel = None;
        }
    }

    /// Signals the running setup to stop and waits for it to unwind, so the caller's launch has
    /// already failed and rolled back when this returns. `false` when nothing is running or
    /// the setup is past cancellation.
    pub async fn cancel(&self, thread: &str) -> bool {
        let cancel = {
            let state = self.0.state.lock().unwrap();
            match state.setups.get(thread) {
                Some(tracked) if tracked.snapshot.phase == WorktreeSetupPhase::Running => tracked.cancel.clone(),
                _ => None,
            }
        };
        let Some(cancel) = cancel else { return false };
        cancel.flag.send_replace(true);
        let mut settled = cancel.settled;
        // A task that dropped its sender has finished too.
        let _ = settled.wait_for(|settled| *settled).await;
        true
    }

    pub fn get(&self, thread: &str) -> Option<WorktreeSetupSnapshot> {
        self.0.state.lock().unwrap().setups.get(thread).map(|tracked| tracked.snapshot.clone())
    }

    /// The current snapshot (or null) first, then the newest after every change. A slow
    /// subscriber only ever holds the newest snapshot, which is a whole state.
    pub fn subscribe(&self, thread: &str) -> watch::Receiver<Option<WorktreeSetupSnapshot>> {
        let mut state = self.0.state.lock().unwrap();
        Self::channel(&mut state, thread).subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use WorktreeSetupStageId::*;

    fn tracker() -> WorktreeSetupTracker {
        WorktreeSetupTracker::with_retention(Duration::from_millis(50))
    }

    #[test]
    fn records_stage_transitions_checkout_progress_and_the_final_phase() {
        let tracker = tracker();
        tracker.begin("thread", Some("feature"), Some("main"), &[Agent, Checkout, SetupScript]);
        let started = tracker.get("thread").unwrap();
        // Stages follow the contract order whatever order they are asked for in.
        assert_eq!(started.stages.iter().map(|stage| stage.id).collect::<Vec<_>>(), [Checkout, SetupScript, Agent]);
        assert_eq!(started.phase, WorktreeSetupPhase::Running);
        assert_eq!(started.sequence.0, 0);
        tracker.stage_status("thread", Checkout, WorktreeSetupStageStatus::Running, None);
        tracker.stage("thread", Checkout, StagePatch { percent: Some(Some(40)), ..StagePatch::default() });
        tracker.set_worktree("thread", "/work/tree", "feature");
        tracker.stage_status("thread", Checkout, WorktreeSetupStageStatus::Done, None);
        let done = tracker.get("thread").unwrap();
        assert_eq!(done.stages[0].percent, Some(40));
        assert!(done.stages[0].started_at.is_some() && done.stages[0].ended_at.is_some());
        assert_eq!(done.worktree_path.as_ref().map(|path| path.as_str()), Some("/work/tree"));
        assert!(done.sequence.0 > started.sequence.0);
        tracker.stage_status("thread", SetupScript, WorktreeSetupStageStatus::Running, None);
        let finished = tracker.finish("thread", WorktreeSetupPhase::Failed, Some("boom")).unwrap();
        assert_eq!(finished.phase, WorktreeSetupPhase::Failed);
        assert_eq!(finished.error.as_deref(), Some("boom"));
        // A running stage fails with the setup; one that never started is untouched.
        assert_eq!(finished.stages[1].status, WorktreeSetupStageStatus::Failed);
        assert_eq!(finished.stages[2].status, WorktreeSetupStageStatus::Pending);
        assert!(tracker.finish("other", WorktreeSetupPhase::Done, None).is_none());
    }

    #[test]
    fn a_cancelled_setup_skips_its_running_stage_and_the_tail_keeps_the_newest_lines() {
        let tracker = tracker();
        tracker.begin("thread", None, None, &[SetupScript]);
        tracker.stage_status("thread", SetupScript, WorktreeSetupStageStatus::Running, None);
        for line in ["1", "2", "3", "4", "5"] {
            tracker.append_tail("thread", SetupScript, line);
        }
        assert_eq!(tracker.get("thread").unwrap().stages[0].tail, ["2", "3", "4", "5"]);
        let finished = tracker.finish("thread", WorktreeSetupPhase::Cancelled, None).unwrap();
        assert_eq!(finished.stages[0].status, WorktreeSetupStageStatus::Skipped);
    }

    #[tokio::test]
    async fn subscribers_see_the_current_state_first_and_sequences_keep_increasing_across_setups() {
        let tracker = tracker();
        let mut idle = tracker.subscribe("thread");
        assert!(idle.borrow_and_update().is_none());
        tracker.begin("thread", None, None, &[Agent]);
        idle.changed().await.unwrap();
        let first = idle.borrow_and_update().clone().unwrap();
        tracker.finish("thread", WorktreeSetupPhase::Done, None);
        let last = tracker.get("thread").unwrap().sequence.0;
        // A new setup before retention elapses continues the numbering.
        tracker.begin("thread", None, None, &[Agent]);
        assert_eq!(tracker.get("thread").unwrap().sequence.0, last + 1);
        assert!(last > first.sequence.0);
        let mut late = tracker.subscribe("thread");
        assert_eq!(late.borrow_and_update().as_ref().unwrap().sequence.0, last + 1);
    }

    #[tokio::test]
    async fn finished_setups_are_dropped_after_the_retention_window() {
        let tracker = tracker();
        let mut subscription = tracker.subscribe("thread");
        tracker.begin("thread", None, None, &[Agent]);
        tracker.finish("thread", WorktreeSetupPhase::Done, None);
        assert!(tracker.get("thread").is_some());
        loop {
            subscription.changed().await.unwrap();
            if subscription.borrow_and_update().is_none() {
                break;
            }
        }
        assert!(tracker.get("thread").is_none());
        // The numbering starts over once nothing is retained.
        tracker.begin("thread", None, None, &[Agent]);
        assert_eq!(tracker.get("thread").unwrap().sequence.0, 0);
    }

    #[tokio::test]
    async fn cancel_signals_the_task_waits_for_it_to_unwind_and_is_a_no_op_once_uncancellable() {
        let tracker = tracker();
        assert!(!tracker.cancel("thread").await);
        let handle = tracker.begin("thread", None, None, &[Agent]);
        let mut cancelled = handle.cancelled.clone();
        let task = tokio::spawn(async move {
            cancelled.wait_for(|cancelled| *cancelled).await.unwrap();
            // The task finishes its cleanup before it reports having settled.
            tokio::task::yield_now().await;
            handle.settled.send_replace(true);
            "unwound"
        });
        assert!(tracker.cancel("thread").await);
        assert_eq!(task.await.unwrap(), "unwound");
        let second = tracker.begin("thread", None, None, &[Agent]);
        tracker.mark_uncancellable("thread");
        assert!(!tracker.cancel("thread").await);
        assert!(!*second.cancelled.borrow());
        // A finished setup cannot be cancelled either.
        let third = tracker.begin("thread", None, None, &[Agent]);
        tracker.finish("thread", WorktreeSetupPhase::Done, None);
        assert!(!tracker.cancel("thread").await);
        drop(third);
    }

    #[test]
    fn free_text_is_clamped_to_the_contract_limits_with_an_ellipsis() {
        let tracker = tracker();
        tracker.begin("thread", None, None, &[SetupScript]);
        tracker.stage_status("thread", SetupScript, WorktreeSetupStageStatus::Running, Some(Some(&"d".repeat(500))));
        tracker.append_tail("thread", SetupScript, &"t".repeat(900));
        let finished = tracker.finish("thread", WorktreeSetupPhase::Failed, Some(&"e".repeat(3000))).unwrap();
        assert_eq!(finished.stages[0].detail.as_ref().unwrap().encode_utf16().count(), WORKTREE_SETUP_DETAIL_MAX_LENGTH);
        assert_eq!(finished.stages[0].tail[0].encode_utf16().count(), WORKTREE_SETUP_TAIL_LINE_MAX_LENGTH);
        assert_eq!(finished.error.as_ref().unwrap().encode_utf16().count(), WORKTREE_SETUP_ERROR_MAX_LENGTH);
        assert!(finished.error.unwrap().ends_with('…'));
    }
}
