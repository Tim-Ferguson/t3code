//! Ports of the cases of `VcsStatusBroadcaster.test.ts` that hold without pull request lookups,
//! against a scripted status source. Time is tokio's paused clock, so intervals are crossed by
//! advancing it; every wait is for an event or a milestone, never a sleep.
use super::*;
use crate::test_support::HANG_BOUND;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use t3_vcs::status::{BranchChanges, WorkingTree};
use tokio::sync::Notify;

fn local(ref_name: &str) -> VcsStatusLocalResult {
    VcsStatusLocalResult {
        is_repo: true,
        source_control_provider: None,
        has_primary_remote: true,
        is_default_ref: false,
        ref_name: Some(ref_name.into()),
        has_working_tree_changes: false,
        working_tree: WorkingTree::default(),
        branch_changes: None,
    }
}
fn remote(ahead: u64, behind: u64, ahead_of_default: Option<u64>) -> VcsStatusRemoteResult {
    VcsStatusRemoteResult {
        has_upstream: true,
        ahead_count: ahead,
        behind_count: behind,
        ahead_of_default_count: ahead_of_default,
        pr: None,
    }
}
fn base_local() -> VcsStatusLocalResult {
    local("feature/status-broadcast")
}
fn base_remote() -> VcsStatusRemoteResult {
    remote(0, 0, None)
}

/// Holds a remote read open until released.
#[derive(Default)]
struct Gate {
    started: Notify,
    release: Notify,
}

struct SetOnDrop(Arc<AtomicBool>);
impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, SeqCst);
    }
}

#[derive(Default)]
struct State {
    local: Mutex<Option<VcsStatusLocalResult>>,
    remote: Mutex<Option<VcsStatusRemoteResult>>,
    local_calls: AtomicUsize,
    remote_calls: AtomicUsize,
    local_invalidations: AtomicUsize,
    remote_invalidations: AtomicUsize,
    pulls: AtomicUsize,
    seen_cwds: Mutex<Vec<String>>,
    refresh_values: Mutex<Vec<Option<bool>>>,
    fail_remote: AtomicBool,
    fail_first_remote: AtomicBool,
    /// Per remote read, the result to give instead of `remote` (consumed in order).
    scripted_remote: Mutex<Vec<Option<VcsStatusRemoteResult>>>,
    gate: Mutex<Option<Arc<Gate>>>,
    hang_remote: AtomicBool,
    remote_dropped: Arc<AtomicBool>,
}

#[derive(Clone)]
struct Mock(Arc<State>);

impl StatusSource for Mock {
    async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, VcsRpcError> {
        self.0.seen_cwds.lock().unwrap().push(cwd.into());
        self.0.local_calls.fetch_add(1, SeqCst);
        Ok(self.0.local.lock().unwrap().clone().unwrap())
    }
    async fn remote_status(
        &self,
        cwd: &str,
        options: RemoteOptions,
    ) -> Result<Option<VcsStatusRemoteResult>, VcsRpcError> {
        self.0.seen_cwds.lock().unwrap().push(cwd.into());
        let gate = self.0.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.started.notify_one();
            gate.release.notified().await;
        }
        if self.0.hang_remote.load(SeqCst) {
            let _flag = SetOnDrop(self.0.remote_dropped.clone());
            self.0.remote_calls.fetch_add(1, SeqCst);
            std::future::pending::<()>().await;
        }
        let call = self.0.remote_calls.fetch_add(1, SeqCst) + 1;
        self.0
            .refresh_values
            .lock()
            .unwrap()
            .push(options.refresh_upstream);
        if self.0.fail_remote.load(SeqCst) || (call == 1 && self.0.fail_first_remote.load(SeqCst)) {
            return Err(VcsRpcError::manager(
                "VcsStatusBroadcaster.test",
                cwd,
                "remote status failed",
                None,
            ));
        }
        let scripted = {
            let mut scripted = self.0.scripted_remote.lock().unwrap();
            (!scripted.is_empty()).then(|| scripted.remove(0))
        };
        Ok(scripted.unwrap_or_else(|| self.0.remote.lock().unwrap().clone()))
    }
    async fn invalidate_local(&self, _: &str) {
        self.0.local_invalidations.fetch_add(1, SeqCst);
    }
    async fn invalidate_remote(&self, _: &str) {
        self.0.remote_invalidations.fetch_add(1, SeqCst);
    }
    async fn invalidate_status(&self, _: &str) {
        self.0.local_invalidations.fetch_add(1, SeqCst);
        self.0.remote_invalidations.fetch_add(1, SeqCst);
    }
    async fn pull_current_branch(&self, _: &str) -> Result<VcsPullResult, VcsRpcError> {
        self.0.pulls.fetch_add(1, SeqCst);
        *self.0.remote.lock().unwrap() = Some(remote(0, 0, None));
        Ok(VcsPullResult {
            status: t3_vcs::wire::PullStatus::Pulled,
            ref_name: "main".into(),
            upstream_ref: Some("origin/main".into()),
        })
    }
}

struct Fixture {
    state: Arc<State>,
    broadcaster: Broadcaster<Mock>,
    hooks: Hooks,
    allowed: Arc<AtomicBool>,
    auto_pull_for: Arc<Mutex<String>>,
}

fn fixture(interval: Duration) -> Fixture {
    let state = Arc::new(State::default());
    *state.local.lock().unwrap() = Some(base_local());
    *state.remote.lock().unwrap() = Some(base_remote());
    let allowed = Arc::new(AtomicBool::new(true));
    let auto_pull_for = Arc::new(Mutex::new(String::new()));
    let (scope, pull) = (allowed.clone(), auto_pull_for.clone());
    let hooks = Hooks {
        fetch_interval: Arc::new(move || Box::pin(async move { interval })),
        scope_allowed: Arc::new(move |_| {
            let allowed = scope.clone();
            Box::pin(async move { allowed.load(SeqCst) })
        }),
        auto_pull: Arc::new(move |cwd| {
            let configured = pull.clone();
            Box::pin(async move { *configured.lock().unwrap() == cwd })
        }),
    };
    Fixture {
        broadcaster: Broadcaster::new(Mock(state.clone())),
        state,
        hooks,
        allowed,
        auto_pull_for,
    }
}

impl Fixture {
    fn calls(&self) -> (usize, usize, usize, usize) {
        (
            self.state.local_calls.load(SeqCst),
            self.state.remote_calls.load(SeqCst),
            self.state.local_invalidations.load(SeqCst),
            self.state.remote_invalidations.load(SeqCst),
        )
    }
    async fn stream(&self, cwd: &str, include_remote: bool) -> StatusStream<Mock> {
        self.broadcaster
            .open_stream(cwd, include_remote, &self.hooks)
            .await
            .unwrap()
    }
}

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn snapshot(local: VcsStatusLocalResult, remote: Option<VcsStatusRemoteResult>) -> StatusEvent {
    StatusEvent::Snapshot { local, remote }
}

#[tokio::test]
async fn reuses_the_cached_status_across_repeated_reads() {
    let f = fixture(HANG_BOUND);
    let expected = merge_status(&base_local(), Some(&base_remote()));
    assert_eq!(f.broadcaster.get_status("/repo").await.unwrap(), expected);
    assert_eq!(f.broadcaster.get_status("/repo").await.unwrap(), expected);
    assert_eq!(f.calls(), (1, 1, 0, 0));
}

#[tokio::test]
async fn refreshes_the_cached_snapshot_after_explicit_invalidation() {
    let f = fixture(HANG_BOUND);
    f.broadcaster.get_status("/repo").await.unwrap();
    *f.state.local.lock().unwrap() = Some(local("feature/updated-status"));
    *f.state.remote.lock().unwrap() = Some(remote(2, 0, None));
    let refreshed = f
        .broadcaster
        .refresh_status("/repo", &f.hooks)
        .await
        .unwrap();
    let cached = f.broadcaster.get_status("/repo").await.unwrap();
    let expected = merge_status(&local("feature/updated-status"), Some(&remote(2, 0, None)));
    assert_eq!((refreshed, cached), (expected.clone(), expected));
    assert_eq!(f.calls(), (2, 2, 1, 1));
}

#[tokio::test]
async fn keeps_the_cached_snapshot_unchanged_when_a_refresh_branch_fails() {
    let f = fixture(HANG_BOUND);
    f.broadcaster.get_status("/repo").await.unwrap();
    *f.state.local.lock().unwrap() = Some(local("feature/partial-refresh"));
    *f.state.remote.lock().unwrap() = Some(remote(3, 0, None));
    f.state.fail_remote.store(true, SeqCst);
    assert!(
        f.broadcaster
            .refresh_status("/repo", &f.hooks)
            .await
            .is_err()
    );
    assert_eq!(
        f.broadcaster.get_status("/repo").await.unwrap(),
        merge_status(&base_local(), Some(&base_remote()))
    );
}

#[tokio::test]
async fn refreshes_only_the_cached_local_snapshot_when_requested() {
    let f = fixture(HANG_BOUND);
    f.broadcaster.get_status("/repo").await.unwrap();
    let mut changed = local("feature/local-only-refresh");
    changed.has_working_tree_changes = true;
    *f.state.local.lock().unwrap() = Some(changed.clone());
    assert_eq!(
        f.broadcaster.refresh_local_status("/repo").await.unwrap(),
        changed
    );
    assert_eq!(
        f.broadcaster.get_status("/repo").await.unwrap(),
        merge_status(&changed, Some(&base_remote()))
    );
    assert_eq!(f.calls(), (2, 1, 1, 0));
}

#[cfg(unix)]
#[tokio::test]
async fn normalizes_symlinked_cwds_before_cache_lookup_and_source_calls() {
    let f = fixture(HANG_BOUND);
    let (real, links) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let link = links.path().join("repo-link");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();
    let real_path = std::fs::canonicalize(real.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    f.broadcaster
        .get_status(link.to_str().unwrap())
        .await
        .unwrap();
    f.broadcaster
        .get_status(real.path().to_str().unwrap())
        .await
        .unwrap();
    assert_eq!(
        *f.state.seen_cwds.lock().unwrap(),
        vec![real_path.clone(), real_path]
    );
    assert_eq!((f.calls().0, f.calls().1), (1, 1));
}

#[tokio::test]
async fn refreshing_a_loaded_cwd_rereads_remote_status_without_fetching() {
    let f = fixture(HANG_BOUND);
    // Nobody loaded this cwd yet: no host request is spent.
    assert_eq!(
        f.broadcaster
            .refresh_pull_request_status("/repo", &f.hooks)
            .await
            .unwrap(),
        None
    );
    assert_eq!(f.calls().1, 0);
    f.broadcaster.get_status("/repo").await.unwrap();
    *f.state.remote.lock().unwrap() = Some(remote(1, 0, None));
    let refreshed = f
        .broadcaster
        .refresh_pull_request_status("/repo", &f.hooks)
        .await
        .unwrap();
    assert_eq!(refreshed, Some(remote(1, 0, None)));
    assert_eq!(f.calls().1, 2);
    assert_eq!(f.calls().3, 0);
    assert_eq!(
        f.state.refresh_values.lock().unwrap().last(),
        Some(&Some(false))
    );
}

#[tokio::test]
async fn refreshing_a_loaded_cwd_is_skipped_when_background_policy_pauses_it() {
    let f = fixture(HANG_BOUND);
    f.allowed.store(false, SeqCst);
    f.broadcaster.get_status("/repo").await.unwrap();
    assert_eq!(
        f.broadcaster
            .refresh_pull_request_status("/repo", &f.hooks)
            .await
            .unwrap(),
        None
    );
    assert_eq!(f.calls().1, 1);
    assert_eq!(f.calls().3, 0);
}

#[tokio::test]
async fn an_initial_status_read_cannot_overwrite_an_explicit_refresh() {
    let f = fixture(HANG_BOUND);
    let gate = Arc::new(Gate::default());
    *f.state.gate.lock().unwrap() = Some(gate.clone());
    f.state
        .scripted_remote
        .lock()
        .unwrap()
        .extend([Some(base_remote()), Some(remote(5, 0, None))]);
    let initial = tokio::spawn({
        let b = f.broadcaster.clone();
        async move { b.get_status("/repo").await.unwrap() }
    });
    gate.started.notified().await;
    let refresh = tokio::spawn({
        let (b, hooks) = (f.broadcaster.clone(), f.hooks.clone());
        async move { b.refresh_status("/repo", &hooks).await.unwrap() }
    });
    settle().await;
    // The refresh queues behind the initial read: only that read has reached the source.
    assert_eq!(f.state.seen_cwds.lock().unwrap().len(), 2);
    gate.release.notify_one();
    initial.await.unwrap();
    gate.started.notified().await;
    gate.release.notify_one();
    refresh.await.unwrap();
    *f.state.gate.lock().unwrap() = None;
    assert_eq!(
        f.broadcaster.get_status("/repo").await.unwrap().ahead_count,
        5
    );
}

#[tokio::test]
async fn an_explicit_refresh_reads_local_totals_after_the_fetch() {
    let f = fixture(HANG_BOUND);
    let gate = Arc::new(Gate::default());
    *f.state.gate.lock().unwrap() = Some(gate.clone());
    let refresh = tokio::spawn({
        let (b, hooks) = (f.broadcaster.clone(), f.hooks.clone());
        async move { b.refresh_status("/repo", &hooks).await.unwrap() }
    });
    gate.started.notified().await;
    // The fetch moves the base while it runs, so totals read before it ends would be stale.
    let mut fetched = base_local();
    fetched.branch_changes = Some(BranchChanges {
        base_ref: Some("origin/main".into()),
        insertions: 0,
        deletions: 0,
    });
    *f.state.local.lock().unwrap() = Some(fetched.clone());
    gate.release.notify_one();
    assert_eq!(
        refresh.await.unwrap().branch_changes,
        fetched.branch_changes
    );
}

#[tokio::test]
async fn automatically_pulls_a_clean_default_branch_that_is_behind() {
    let f = fixture(HANG_BOUND);
    let mut main = local("main");
    main.is_default_ref = true;
    *f.state.local.lock().unwrap() = Some(main);
    *f.state.remote.lock().unwrap() = Some(remote(0, 2, None));
    *f.auto_pull_for.lock().unwrap() = "/repo".into();
    let status = f
        .broadcaster
        .refresh_status("/repo", &f.hooks)
        .await
        .unwrap();
    assert_eq!(f.state.pulls.load(SeqCst), 1);
    assert_eq!(status.behind_count, 0);
}

#[tokio::test]
async fn auto_pull_leaves_dirty_ahead_or_disabled_checkouts_alone() {
    for (dirty, ahead, enabled) in [(true, 0, true), (false, 1, true), (false, 0, false)] {
        let f = fixture(HANG_BOUND);
        let mut main = local("main");
        main.is_default_ref = true;
        main.has_working_tree_changes = dirty;
        *f.state.local.lock().unwrap() = Some(main);
        *f.state.remote.lock().unwrap() = Some(remote(ahead, 2, None));
        if enabled {
            *f.auto_pull_for.lock().unwrap() = "/repo".into();
        }
        let status = f
            .broadcaster
            .refresh_status("/repo", &f.hooks)
            .await
            .unwrap();
        assert_eq!(f.state.pulls.load(SeqCst), 0);
        assert_eq!(status.behind_count, 2);
    }
}

#[tokio::test(start_paused = true)]
async fn passive_streams_retain_cached_remote_status() {
    let f = fixture(Duration::from_secs(30));
    let mut passive = f.stream("/repo", false).await;
    assert_eq!(passive.next().await, Some(snapshot(base_local(), None)));
    tokio::time::advance(Duration::from_secs(60)).await;
    settle().await;
    assert_eq!(f.state.remote_calls.load(SeqCst), 0);
    assert_eq!(f.broadcaster.poller_subscribers("/repo"), None);
    f.broadcaster.get_status("/repo").await.unwrap();
    *f.state.remote.lock().unwrap() = Some(remote(1, 0, None));
    f.broadcaster
        .refresh_pull_request_status("/repo", &f.hooks)
        .await
        .unwrap();
    assert_eq!(
        passive.next().await,
        Some(StatusEvent::RemoteUpdated(Some(remote(1, 0, None))))
    );
    let mut later = f.stream("/repo", false).await;
    assert_eq!(
        later.next().await,
        Some(snapshot(base_local(), Some(remote(1, 0, None))))
    );
}

#[tokio::test(start_paused = true)]
async fn loads_remote_status_once_when_periodic_refreshes_are_disabled() {
    let f = fixture(Duration::ZERO);
    let mut stream = f.stream("/repo", true).await;
    assert_eq!(stream.next().await, Some(snapshot(base_local(), None)));
    // The first read has no earlier remote to compare, so local status is re-read as well.
    let mut events = Vec::new();
    while !events.contains(&StatusEvent::RemoteUpdated(Some(base_remote()))) {
        events.push(stream.next().await.unwrap());
    }
    assert_eq!(f.state.remote_calls.load(SeqCst), 1);
    assert_eq!(f.state.remote_invalidations.load(SeqCst), 0);
    assert_eq!(*f.state.refresh_values.lock().unwrap(), vec![Some(false)]);
    tokio::time::advance(Duration::from_secs(120)).await;
    settle().await;
    assert_eq!(f.state.remote_calls.load(SeqCst), 1);
    assert_eq!(f.state.remote_invalidations.load(SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn retries_the_initial_remote_load_when_periodic_refreshes_are_disabled() {
    let f = fixture(Duration::ZERO);
    f.state.fail_first_remote.store(true, SeqCst);
    let mut stream = f.stream("/repo", true).await;
    stream.next().await;
    settle().await;
    assert_eq!(f.state.remote_calls.load(SeqCst), 1);
    // The failure backs off 30 seconds, and nothing happens before then.
    tokio::time::advance(Duration::from_secs(29)).await;
    settle().await;
    assert_eq!(f.state.remote_calls.load(SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    let mut seen = None;
    while seen.is_none() {
        if let StatusEvent::RemoteUpdated(remote) = stream.next().await.unwrap() {
            seen = Some(remote);
        }
    }
    assert_eq!(seen, Some(Some(base_remote())));
    assert_eq!(f.state.remote_calls.load(SeqCst), 2);
    assert_eq!(f.state.remote_invalidations.load(SeqCst), 0);
    assert_eq!(
        *f.state.refresh_values.lock().unwrap(),
        vec![Some(false), Some(false)]
    );
}

#[tokio::test(start_paused = true)]
async fn delays_automatic_refresh_when_a_cached_remote_snapshot_is_available() {
    let f = fixture(Duration::from_secs(60));
    f.broadcaster.get_status("/repo").await.unwrap();
    let mut stream = f.stream("/repo", true).await;
    stream.next().await;
    settle().await;
    assert_eq!((f.calls().1, f.calls().3), (1, 0));
    tokio::time::advance(Duration::from_secs(59)).await;
    settle().await;
    assert_eq!(f.calls().1, 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!((f.calls().1, f.calls().3), (2, 1));
    assert_eq!(
        *f.state.refresh_values.lock().unwrap(),
        vec![None, Some(true)]
    );
}

#[tokio::test(start_paused = true)]
async fn rereads_local_status_when_a_fetch_reflects_a_push_or_a_merged_pull_request() {
    for initial in [remote(1, 0, Some(0)), remote(0, 0, Some(2))] {
        let f = fixture(Duration::from_secs(60));
        *f.state.remote.lock().unwrap() = Some(initial);
        f.broadcaster.get_status("/repo").await.unwrap();
        let mut stream = f.stream("/repo", true).await;
        stream.next().await;
        settle().await;
        // The next fetch moves the base, so the Changes totals shrink.
        let mut pushed = base_local();
        pushed.branch_changes = Some(BranchChanges {
            base_ref: Some("origin/main".into()),
            insertions: 0,
            deletions: 0,
        });
        *f.state.local.lock().unwrap() = Some(pushed.clone());
        *f.state.remote.lock().unwrap() = Some(remote(0, 0, Some(0)));
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(stream.next().await, Some(StatusEvent::LocalUpdated(pushed)));
    }
}

#[tokio::test(start_paused = true)]
async fn does_not_start_automatic_refreshes_without_foreground_client_demand() {
    let f = fixture(Duration::from_secs(1));
    f.allowed.store(false, SeqCst);
    f.broadcaster.get_status("/repo").await.unwrap();
    let mut stream = f.stream("/repo", true).await;
    assert!(stream.next().await.is_some());
    tokio::time::advance(Duration::from_secs(10)).await;
    settle().await;
    assert_eq!((f.calls().1, f.calls().3), (1, 0));
}

#[tokio::test(start_paused = true)]
async fn releases_remote_demand_while_passive_observers_remain() {
    let f = fixture(Duration::from_secs(30));
    f.state.hang_remote.store(true, SeqCst);
    let mut passive = f.stream("/repo", false).await;
    passive.next().await;
    assert_eq!(f.state.remote_calls.load(SeqCst), 0);
    let (mut first, mut second) = (f.stream("/repo", true).await, f.stream("/repo", true).await);
    first.next().await;
    second.next().await;
    settle().await;
    assert_eq!(f.state.remote_calls.load(SeqCst), 1);
    assert_eq!(f.broadcaster.poller_subscribers("/repo"), Some(2));
    drop(first);
    settle().await;
    assert!(!f.state.remote_dropped.load(SeqCst));
    assert_eq!(f.broadcaster.poller_subscribers("/repo"), Some(1));
    drop(second);
    settle().await;
    assert!(f.state.remote_dropped.load(SeqCst));
    assert_eq!(f.broadcaster.poller_subscribers("/repo"), None);
    drop(passive);
}

#[tokio::test]
async fn a_subscriber_that_falls_behind_gets_the_current_state() {
    let f = fixture(Duration::ZERO);
    let mut stream = f.stream("/repo", false).await;
    stream.next().await;
    for index in 0..400 {
        *f.state.local.lock().unwrap() = Some(local(&format!("branch-{index}")));
        f.broadcaster.refresh_local_status("/repo").await.unwrap();
    }
    assert_eq!(
        stream.next().await,
        Some(snapshot(local("branch-399"), None))
    );
}

#[tokio::test]
async fn publishes_only_when_a_part_changed() {
    let f = fixture(Duration::ZERO);
    let mut stream = f.stream("/repo", false).await;
    stream.next().await;
    f.broadcaster.refresh_local_status("/repo").await.unwrap();
    *f.state.local.lock().unwrap() = Some(local("moved"));
    f.broadcaster.refresh_local_status("/repo").await.unwrap();
    assert_eq!(
        stream.next().await,
        Some(StatusEvent::LocalUpdated(local("moved")))
    );
}

#[tokio::test]
async fn a_poll_tick_reports_the_back_off_after_a_failure() {
    let f = fixture(Duration::from_secs(1));
    f.state.fail_remote.store(true, SeqCst);
    let delay = f
        .broadcaster
        .poll_tick_for_test("/repo", &["/repo"], &f.hooks, true)
        .await;
    assert_eq!(delay, Duration::from_secs(30));
    f.state.fail_remote.store(false, SeqCst);
    let delay = f
        .broadcaster
        .poll_tick_for_test("/repo", &["/repo"], &f.hooks, true)
        .await;
    assert_eq!(delay, Duration::from_secs(1));
}
