#![cfg(unix)]
//! Storage cleanup and worktree recreation (`storageCleanup.ts` with the companion in
//! `ProviderTurnStartService.ts`) against real repositories. Every repository, worktree, log and artifact lives
//! under a temporary directory named `t3code-rust-port-test-…`; the remote is a local bare repository; the
//! pull request host is a fake. Time is the cleanup's injected clock; nothing sleeps or polls, and waits are
//! bounded by `HANG_BOUND`.
//!
//! Each exclusion has a test that makes the worktree otherwise eligible, adds the one thing that must keep it,
//! and shows it stays; the matching case shows the same worktree goes without it.
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use t3_contracts::ChangeRequestState;
use t3_server::{
    git_workflow_status::BranchPullRequest,
    persistence::{Store, write_projection},
    project::ProjectService,
    scheduled_tasks::ManualClock,
    storage_cleanup::{BranchPullRequests, LiveTerminals, Options, SettingsFeed, StorageCleanup, recreate_missing_worktree},
    test_support::HANG_BOUND,
    thread::ThreadService,
};
use t3_vcs::{GitVcs, VcsProcess};

const DAY_MS: i64 = 86_400_000;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false", "-c", "protocol.file.allow=always"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct FakeSettings {
    value: Mutex<Value>,
    sender: tokio::sync::mpsc::UnboundedSender<Value>,
    receiver: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Value>>>,
    reads: AtomicUsize,
    /// Called with the number of reads so far, before each read answers.
    on_read: Mutex<Option<Box<dyn FnMut(usize, &mut Value) + Send>>>,
}
impl FakeSettings {
    fn new(value: Value) -> Arc<Self> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(Self { value: Mutex::new(value), sender, receiver: Mutex::new(Some(receiver)), reads: AtomicUsize::new(0), on_read: Mutex::new(None) })
    }
    fn change(&self, value: Value) {
        *self.value.lock().unwrap() = value.clone();
        self.sender.send(value).unwrap();
    }
}
#[async_trait]
impl SettingsFeed for FakeSettings {
    async fn current(&self) -> Result<Value, String> {
        let reads = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        let mut value = self.value.lock().unwrap();
        if let Some(hook) = self.on_read.lock().unwrap().as_mut() {
            hook(reads, &mut value);
        }
        Ok(value.clone())
    }
    async fn changes(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        self.receiver.lock().unwrap().take()
    }
}

#[derive(Default)]
struct FakePullRequests {
    answer: Mutex<Option<BranchPullRequest>>,
    asked: AtomicUsize,
}
#[async_trait]
impl BranchPullRequests for FakePullRequests {
    async fn latest(&self, _cwd: &Path, _branch: &str) -> Result<Option<BranchPullRequest>, String> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Ok(self.answer.lock().unwrap().clone())
    }
}

struct World {
    _directory: tempfile::TempDir,
    base: PathBuf,
    state_dir: PathBuf,
    project: PathBuf,
    origin: PathBuf,
    store: Store,
    settings: Arc<FakeSettings>,
    prs: Arc<FakePullRequests>,
    terminals: Arc<LiveTerminals>,
    clock: Arc<ManualClock>,
    cleanup: StorageCleanup,
    git: t3_vcs::GitCore,
}

fn default_settings() -> Value {
    let mut settings = serde_json::to_value(serde_json::from_value::<t3_contracts::ServerSettings>(json!({})).unwrap()).unwrap();
    settings["storageCleanup"] = json!({"worktreeAfterDays": null, "worktreeOnMerge": false, "worktreeOnDelete": false, "worktreeUnchanged": false, "browserArtifactsAfterDays": null, "logsAfterDays": null});
    settings
}

fn world(rules: Value) -> World {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-cleanup-").tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    // `<home>/userdata` is the state directory, so the default worktrees directory is `<home>/worktrees`.
    let state_dir = base.join("home/userdata");
    std::fs::create_dir_all(&state_dir).unwrap();
    let (origin, project) = (base.join("origin.git"), base.join("project"));
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--bare", "-b", "main"]);
    std::fs::create_dir_all(&project).unwrap();
    git(&project, &["init", "-b", "main"]);
    std::fs::write(project.join(".gitignore"), "node_modules/\n.env\nbuild/\n").unwrap();
    std::fs::write(project.join("README.md"), "base\n").unwrap();
    git(&project, &["add", "."]);
    git(&project, &["commit", "-m", "base"]);
    git(&project, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&project, &["push", "-u", "origin", "main"]);
    git(&project, &["remote", "set-head", "origin", "main"]);
    let store = Store::open(state_dir.join("rust-state.sqlite")).unwrap();
    ProjectService::new(store.clone())
        .mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":project}), Utc::now())
        .unwrap();
    let mut settings = default_settings();
    for (key, value) in rules.as_object().unwrap() {
        settings["storageCleanup"][key] = value.clone();
    }
    let settings = FakeSettings::new(settings);
    let prs = Arc::new(FakePullRequests::default());
    let terminals = Arc::new(LiveTerminals::default());
    let clock = ManualClock::new(Utc::now().timestamp_millis());
    let git_core = GitVcs::new(VcsProcess::new()).core();
    let cleanup = StorageCleanup::new(Options {
        store: store.clone(),
        state_dir: state_dir.clone(),
        settings: settings.clone(),
        git: git_core.clone(),
        pull_requests: prs.clone(),
        clock: clock.clone(),
        terminals: terminals.clone(),
    });
    World { _directory: directory, base, state_dir, project, origin, store, settings, prs, terminals, clock, cleanup, git: git_core }
}

impl World {
    fn worktrees(&self) -> PathBuf {
        self.base.join("home/worktrees")
    }
    /// A linked worktree on a new branch, as a thread launch makes one, and the thread that owns it.
    fn thread(&self, id: &str) -> PathBuf {
        let path = self.worktrees().join(id);
        git(&self.project, &["worktree", "add", "-b", id, path.to_str().unwrap()]);
        let path = std::fs::canonicalize(path).unwrap();
        self.thread_at(id, id, Some(&path));
        path
    }
    fn thread_at(&self, id: &str, branch: &str, path: Option<&Path>) {
        let receipt = ThreadService::new(self.store.clone())
            .dispatch(&json!({"type":"thread.create","commandId":format!("create-{id}"),"threadId":id,"projectId":"project","title":id,"modelSelection":{"instanceId":"codex","model":"fixture"},"runtimeMode":"approval-required","interactionMode":"default","branch":branch,"worktreePath":path}), Utc::now())
            .unwrap();
        assert_eq!(receipt.status, "accepted", "{receipt:?}");
    }
    fn edit(&self, thread: &str, change: impl FnOnce(&mut Value)) {
        self.store
            .transaction(|transaction| {
                let mut projection = t3_server::persistence::read_projection(transaction, "thread", thread)?.unwrap();
                change(&mut projection);
                write_projection(transaction, "thread", thread, &projection)
            })
            .unwrap();
    }
    fn rules(&self, change: impl FnOnce(&mut Value)) {
        let mut value = self.settings.value.lock().unwrap();
        change(&mut value["storageCleanup"]);
    }
    /// The clock a number of days after the threads were made.
    fn days_later(&self, days: i64) {
        self.clock.set(Utc::now().timestamp_millis() + days * DAY_MS);
    }
    async fn sweep(&self) {
        tokio::time::timeout(HANG_BOUND, self.cleanup.sweep()).await.expect("a sweep ends");
    }
    fn branch_exists(&self, branch: &str) -> bool {
        Command::new("git").current_dir(&self.project).args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).status().unwrap().success()
    }
}

// ---- inactivity ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_worktree_idle_longer_than_the_retention_goes_and_its_branch_and_thread_stay_so_a_turn_can_recreate_it() {
    let world = world(json!({"worktreeAfterDays": 30}));
    let path = world.thread("old");
    std::fs::create_dir_all(path.join("node_modules/dep")).unwrap();
    std::fs::write(path.join("node_modules/dep/index.js"), "x").unwrap();

    world.days_later(10);
    world.sweep().await;
    assert!(path.exists(), "ten idle days is under the retention");

    world.days_later(40);
    world.sweep().await;
    assert!(!path.exists(), "a clean worktree whose only ignored files are dependency installs is removed");
    assert!(world.branch_exists("old"), "removal never deletes the branch");
    assert!(git(&world.project, &["worktree", "list", "--porcelain"]).lines().all(|line| !line.ends_with("/old")), "and no registration is left");

    // The turn that follows recreates the checkout from the branch, at the thread's path.
    recreate_missing_worktree(&world.store, || serde_json::from_value(json!({})).unwrap(), &world.git, "old").await;
    assert!(path.join(".git").is_file() && path.join("README.md").exists());
    assert_eq!(git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]), "old");
}

#[tokio::test]
async fn recreation_leaves_an_existing_checkout_alone_and_a_failure_does_not_stop_the_turn() {
    let world = world(json!({}));
    let path = world.thread("present");
    std::fs::write(path.join("work.txt"), "unsaved\n").unwrap();
    recreate_missing_worktree(&world.store, || panic!("settings are not needed while the checkout exists"), &world.git, "present").await;
    assert_eq!(std::fs::read_to_string(path.join("work.txt")).unwrap(), "unsaved\n");
    // A branch that no longer exists cannot be checked out: the failure is logged and nothing is made.
    let gone = world.worktrees().join("gone");
    world.thread_at("gone", "no-such-branch", Some(&gone));
    recreate_missing_worktree(&world.store, || serde_json::from_value(json!({})).unwrap(), &world.git, "gone").await;
    assert!(!gone.exists());
    // A thread with no worktree, or an unknown thread, is not touched.
    world.thread_at("plain", "main", None);
    recreate_missing_worktree(&world.store, || panic!("no worktree to recreate"), &world.git, "plain").await;
    recreate_missing_worktree(&world.store, || panic!("no thread"), &world.git, "missing").await;
}

#[tokio::test]
async fn no_rule_means_nothing_is_removed_and_a_project_policy_overrides_the_environment() {
    let world = world(json!({"worktreeAfterDays": null}));
    let path = world.thread("kept");
    world.days_later(200);
    world.sweep().await;
    assert!(path.exists(), "with every rule off nothing is eligible");

    world.rules(|rules| rules["worktreeAfterDays"] = json!(30));
    world.settings.value.lock().unwrap()["projectSettingsOverrides"] = json!({"project": {"worktreeCleanup": {"mode": "off"}}});
    world.sweep().await;
    assert!(path.exists(), "a project whose policy is off keeps its worktrees whatever the environment says");

    world.settings.value.lock().unwrap()["projectSettingsOverrides"] = json!({"project": {"worktreeCleanup": {"mode": "custom", "rules": {"worktreeAfterDays": 365, "worktreeOnMerge": false, "worktreeOnDelete": false, "worktreeUnchanged": false}}}});
    world.sweep().await;
    assert!(path.exists(), "the project's own retention is a year, longer than the environment's");
    world.days_later(500);
    world.sweep().await;
    assert!(!path.exists());
}

// ---- exclusions ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn uncommitted_work_keeps_the_worktree() {
    let world = world(json!({"worktreeAfterDays": 30}));
    let untracked = world.thread("untracked");
    let modified = world.thread("modified");
    let staged = world.thread("staged");
    let clean = world.thread("clean");
    std::fs::write(untracked.join("new.txt"), "x").unwrap();
    std::fs::write(modified.join("README.md"), "changed\n").unwrap();
    std::fs::write(staged.join("added.txt"), "x").unwrap();
    git(&staged, &["add", "added.txt"]);
    world.days_later(60);
    world.sweep().await;
    assert!(untracked.exists() && modified.exists() && staged.exists(), "dirty worktrees stay");
    assert!(!clean.exists(), "the clean one beside them goes");
}

#[tokio::test]
async fn ignored_files_other_than_dependency_installs_keep_the_worktree() {
    let world = world(json!({"worktreeAfterDays": 30}));
    let secrets = world.thread("secrets");
    let build = world.thread("build");
    let nested = world.thread("nested");
    std::fs::write(secrets.join(".env"), "TOKEN=1\n").unwrap();
    std::fs::create_dir_all(build.join("build")).unwrap();
    std::fs::write(build.join("build/out.bin"), "x").unwrap();
    std::fs::create_dir_all(nested.join("packages/web/node_modules/pkg")).unwrap();
    std::fs::write(nested.join("packages/web/node_modules/pkg/i.js"), "x").unwrap();
    world.days_later(60);
    world.sweep().await;
    assert!(secrets.exists(), "an ignored .env may hold secrets");
    assert!(build.exists(), "an ignored build directory may hold data the user wants");
    // Git lists the directories that hold only ignored files ("packages/", "packages/web/") beside the install itself, and
    // the original's pattern accepts only a `node_modules/` entry, so a nested install keeps the worktree. Kept as it is: it errs
    // toward keeping.
    assert!(nested.exists(), "a nested node_modules is listed with its parents, which are not dependency installs");
}

#[tokio::test]
async fn a_thread_that_is_working_or_waiting_keeps_its_worktree() {
    let world = world(json!({"worktreeAfterDays": 30}));
    let running = world.thread("running");
    let asking = world.thread("asking");
    let queued = world.thread("queued");
    let idle = world.thread("idle");
    let run = |status: &str| json!({"id": "run", "ordinal": 1, "status": status, "requestedAt": "2026-01-01T00:00:00.000Z"});
    world.edit("running", |projection| projection["runs"] = json!([run("running")]));
    world.edit("queued", |projection| projection["runs"] = json!([run("queued")]));
    world.edit("asking", |projection| {
        projection["runs"] = json!([run("waiting")]);
        projection["runtimeRequests"] = json!([{"id": "request", "kind": "command", "status": "pending", "createdAt": "2026-01-01T00:00:00.000Z"}]);
    });
    world.days_later(60);
    world.sweep().await;
    for kept in [&running, &asking, &queued] {
        assert!(kept.exists(), "{}", kept.display());
    }
    assert!(!idle.exists());
}

#[tokio::test]
async fn a_live_terminal_or_provider_session_inside_the_worktree_keeps_it() {
    let world = world(json!({"worktreeAfterDays": 30}));
    let terminal = world.thread("terminal");
    let deep = world.thread("deep");
    let session = world.thread("session");
    let stopped = world.thread("stopped");
    let finished = world.thread("finished");
    world.days_later(60);
    let summary = |path: &Path, status: &str, key: &str| json!({"threadId": key, "terminalId": key, "status": status, "cwd": path, "worktreePath": null});
    world.terminals.apply(&json!({"type": "snapshot", "terminals": [summary(&terminal, "running", "a"), summary(&deep.join("src"), "starting", "b"), summary(&finished, "exited", "c")]}));
    world.edit("session", |projection| projection["providerSessions"] = json!([{"id": "s", "status": "ready", "cwd": session.join("pkg")}]));
    world.edit("stopped", |projection| projection["providerSessions"] = json!([{"id": "s", "status": "stopped", "cwd": stopped}]));
    std::fs::create_dir_all(deep.join("src")).unwrap();
    std::fs::create_dir_all(session.join("pkg")).unwrap();
    world.sweep().await;
    assert!(terminal.exists() && deep.exists(), "a running or starting terminal in, or inside, the worktree");
    assert!(session.exists(), "a provider session whose cwd is inside it");
    assert!(!stopped.exists() && !finished.exists(), "a stopped session and an exited terminal hold nothing");

    // A terminal that closes releases the worktree on the next sweep.
    world.terminals.apply(&json!({"type": "remove", "threadId": "a", "terminalId": "a"}));
    world.sweep().await;
    assert!(!terminal.exists() && deep.exists());
}

#[tokio::test]
async fn a_path_shared_by_two_threads_is_never_removed() {
    let world = world(json!({"worktreeAfterDays": 30}));
    let shared = world.thread("shared");
    world.thread_at("sibling", "shared", Some(&shared));
    world.days_later(60);
    world.sweep().await;
    assert!(shared.exists());
}

#[tokio::test]
async fn only_linked_worktrees_under_a_managed_directory_that_are_not_symbolic_links_or_project_roots_are_removed() {
    let world = world(json!({"worktreeAfterDays": 30}));
    // A worktree outside every managed directory.
    let outside = world.base.join("elsewhere/outside");
    git(&world.project, &["worktree", "add", "-b", "outside", outside.to_str().unwrap()]);
    world.thread_at("outside", "outside", Some(&std::fs::canonicalize(&outside).unwrap()));
    // A thread whose worktree path is a symbolic link into a managed directory.
    let real = world.thread("real-target");
    std::fs::write(real.join("dirty.txt"), "x").unwrap();
    let link = world.worktrees().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    world.thread_at("link", "real-target", Some(&link));
    // A directory that is not a worktree at all (its .git is a directory).
    let copy = world.worktrees().join("copy");
    std::fs::create_dir_all(&copy).unwrap();
    git(&copy, &["init", "-b", "copy"]);
    git(&copy, &["commit", "--allow-empty", "-m", "x"]);
    world.thread_at("copy", "copy", Some(&copy));
    // A worktree that holds another project's root.
    let nested = world.thread("holds-project");
    let inner = nested.join("inner-project");
    std::fs::create_dir_all(&inner).unwrap();
    ProjectService::new(world.store.clone())
        .mutate(json!({"type":"project.create","commandId":"inner","projectId":"inner","title":"Inner","workspaceRoot":inner}), Utc::now())
        .unwrap();
    world.days_later(60);
    world.sweep().await;
    assert!(outside.exists(), "outside the managed worktrees directories");
    assert!(real.exists() && link.is_symlink(), "a symbolic link is not followed");
    assert!(copy.join(".git").is_dir(), "a repository of its own is not a linked worktree");
    assert!(nested.exists(), "a worktree that contains a project's workspace root");
}

#[tokio::test]
async fn the_main_checkout_is_never_removed_even_when_a_managed_directory_holds_it() {
    let world = world(json!({"worktreeAfterDays": 30}));
    // The managed directory is the whole test tree, so the project's checkout lies inside it.
    world.settings.value.lock().unwrap()["worktreesDirectory"] = json!(world.base);
    world.thread_at("main-checkout", "main", Some(&world.project));
    world.days_later(60);
    world.sweep().await;
    assert!(world.project.join(".git").is_dir() && world.project.join("README.md").exists());
    assert_eq!(git(&world.project, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
}

#[tokio::test]
async fn a_worktree_on_another_branch_than_the_threads_keeps_it() {
    let world = world(json!({"worktreeAfterDays": 30}));
    let path = world.thread("branchy");
    git(&path, &["switch", "-c", "other-branch"]);
    world.days_later(60);
    world.sweep().await;
    assert!(path.exists(), "the checkout is no longer the thread's branch");
}

// ---- merged work -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_branch_already_in_the_default_branch_goes_when_unchanged_removal_is_on_and_the_default_branch_is_fetched_first() {
    let world = world(json!({"worktreeUnchanged": true}));
    let merged = world.thread("merged");
    let ahead = world.thread("ahead");
    let fresh = world.thread("fresh");
    // `ahead` has a commit nobody merged; `merged` is published on main by someone else after the last fetch.
    std::fs::write(ahead.join("a.txt"), "a\n").unwrap();
    git(&ahead, &["add", "a.txt"]);
    git(&ahead, &["commit", "-m", "ahead"]);
    std::fs::write(merged.join("m.txt"), "m\n").unwrap();
    git(&merged, &["add", "m.txt"]);
    git(&merged, &["commit", "-m", "merged work"]);
    let other = world.base.join("other-clone");
    git(&world.base, &["clone", world.origin.to_str().unwrap(), other.to_str().unwrap()]);
    git(&other, &["fetch", world.project.to_str().unwrap(), "merged:merged-copy"]);
    git(&other, &["merge", "--ff-only", "merged-copy"]);
    git(&other, &["push", "origin", "main"]);
    assert_ne!(git(&world.project, &["rev-parse", "origin/main"]), git(&other, &["rev-parse", "HEAD"]), "this clone's remote-tracking ref is stale until the cleanup fetches");
    world.sweep().await;
    assert!(!merged.exists(), "its head is in the freshly fetched default branch");
    assert!(ahead.exists(), "unmerged commits keep the checkout");
    assert!(!fresh.exists(), "a branch at the default branch's head is unchanged");
    assert!(world.branch_exists("merged") && world.branch_exists("fresh"));
    assert_eq!(world.prs.asked.load(Ordering::SeqCst), 0, "without the merge rule the host is not asked");
}

#[tokio::test]
async fn a_merged_pull_request_removes_a_squash_merged_worktree_only_when_it_names_this_head() {
    let world = world(json!({"worktreeOnMerge": true}));
    let path = world.thread("squashed");
    std::fs::write(path.join("s.txt"), "s\n").unwrap();
    git(&path, &["add", "s.txt"]);
    git(&path, &["commit", "-m", "work"]);
    let head = git(&path, &["rev-parse", "HEAD"]);
    let pr = |state, base: &str, sha: Option<&str>| BranchPullRequest { state, head_ref: "squashed".into(), base_ref: base.into(), head_sha: sha.map(str::to_owned) };
    for (name, answer) in [
        ("no pull request", None),
        ("open", Some(pr(ChangeRequestState::Open, "main", Some(&head)))),
        ("closed", Some(pr(ChangeRequestState::Closed, "main", Some(&head)))),
        ("another head", Some(pr(ChangeRequestState::Merged, "main", Some(&"c".repeat(40))))),
        ("no reported head", Some(pr(ChangeRequestState::Merged, "main", None))),
        ("another base", Some(pr(ChangeRequestState::Merged, "release", Some(&head)))),
    ] {
        *world.prs.answer.lock().unwrap() = answer;
        world.sweep().await;
        assert!(path.exists(), "{name}");
    }
    *world.prs.answer.lock().unwrap() = Some(pr(ChangeRequestState::Merged, "main", Some(&head)));
    world.sweep().await;
    assert!(!path.exists());
}

// ---- deletion ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_deleted_threads_worktree_goes_once_its_effects_have_finished_and_nothing_else_uses_the_path() {
    let world = world(json!({"worktreeOnDelete": true}));
    let doomed = world.thread("doomed");
    let reused = world.thread("reused");
    let live = world.thread("live");
    ThreadService::new(world.store.clone()).dispatch(&json!({"type":"thread.delete","commandId":"delete-doomed","threadId":"doomed"}), Utc::now()).unwrap();
    ThreadService::new(world.store.clone()).dispatch(&json!({"type":"thread.delete","commandId":"delete-reused","threadId":"reused"}), Utc::now()).unwrap();
    world.thread_at("reuser", "reused", Some(&reused));
    world.days_later(1);

    let outstanding: i64 = world.store.read(|connection| Ok(connection.query_row("SELECT COUNT(*) FROM rust_effect_outbox WHERE thread_id='doomed' AND status NOT IN ('succeeded','cancelled')", [], |row| row.get(0))?)).unwrap();
    assert!(outstanding > 0, "deleting a thread queues its cleanup effects");
    world.sweep().await;
    assert!(doomed.exists(), "an effect of the deleted thread is still outstanding");

    world.store.transaction(|transaction| Ok(transaction.execute("UPDATE rust_effect_outbox SET status='succeeded'", [])?)).unwrap();
    world.sweep().await;
    assert!(!doomed.exists(), "every effect has finished");
    assert!(reused.exists(), "a live thread uses the path of the deleted one");
    assert!(live.exists(), "a thread that was not deleted is not touched by the delete rule");
    assert!(world.branch_exists("doomed"));

    // The rule is read again before removal: with it off nothing is removed.
    let kept = world.thread("kept");
    ThreadService::new(world.store.clone()).dispatch(&json!({"type":"thread.delete","commandId":"delete-kept","threadId":"kept"}), Utc::now()).unwrap();
    world.store.transaction(|transaction| Ok(transaction.execute("UPDATE rust_effect_outbox SET status='succeeded'", [])?)).unwrap();
    world.rules(|rules| rules["worktreeOnDelete"] = json!(false));
    world.sweep().await;
    assert!(kept.exists());
}

#[tokio::test]
async fn a_rule_that_changes_while_the_worktree_is_being_judged_cancels_the_removal() {
    let world = world(json!({"worktreeAfterDays": 30}));
    let path = world.thread("racing");
    world.days_later(60);
    // The sweep reads the settings once to start and once more just before removing: the second sees a longer retention.
    *world.settings.on_read.lock().unwrap() = Some(Box::new(|reads, settings| {
        if reads >= 2 {
            settings["storageCleanup"]["worktreeAfterDays"] = json!(365);
        }
    }));
    world.sweep().await;
    assert!(path.exists());
}

// ---- files ---------------------------------------------------------------------------------------------------

fn aged(path: &Path, days: i64, clock: &ManualClock) {
    use t3_server::scheduled_tasks::SchedulerClock;
    let modified = std::time::UNIX_EPOCH + std::time::Duration::from_millis((clock.now_ms() - days * DAY_MS) as u64);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(modified).unwrap();
}

#[tokio::test]
async fn old_browser_artifacts_and_rotated_logs_go_and_nothing_else_does() {
    let world = world(json!({"browserArtifactsAfterDays": 7, "logsAfterDays": 14}));
    let (artifacts, logs) = (world.state_dir.join("browser-artifacts"), world.state_dir.join("logs"));
    std::fs::create_dir_all(artifacts.join("nested")).unwrap();
    std::fs::create_dir_all(logs.join("terminals")).unwrap();
    let files = [
        (artifacts.join("old.png"), 30, true),
        (artifacts.join("recent.png"), 1, false),
        (artifacts.join("nested/old.png"), 30, false),
        (logs.join("server.log.1"), 30, true),
        (logs.join("server.log.2"), 3, false),
        (logs.join("terminals/t.log.3"), 30, true),
        (logs.join("provider.ndjson.4"), 30, true),
        (logs.join("server.log"), 30, false),
        (logs.join("server.trace.ndjson"), 30, false),
        (logs.join("terminals/notes.txt"), 30, false),
        (logs.join("old.png"), 30, false),
    ];
    for (path, days, _) in &files {
        std::fs::write(path, "x").unwrap();
        aged(path, *days, &world.clock);
    }
    // A link to an old file outside the directory is not followed or removed.
    let outside = world.base.join("outside.log.1");
    std::fs::write(&outside, "x").unwrap();
    aged(&outside, 90, &world.clock);
    std::os::unix::fs::symlink(&outside, logs.join("link.log.9")).unwrap();
    world.sweep().await;
    for (path, _, removed) in &files {
        assert_eq!(!path.exists(), *removed, "{}", path.display());
    }
    assert!(outside.exists() && logs.join("link.log.9").is_symlink());
}

#[tokio::test]
async fn retention_off_removes_no_files_and_a_setting_that_changes_mid_pass_ends_that_directory() {
    let world = world(json!({"browserArtifactsAfterDays": null, "logsAfterDays": null}));
    let artifacts = world.state_dir.join("browser-artifacts");
    std::fs::create_dir_all(&artifacts).unwrap();
    for name in ["a.png", "b.png"] {
        std::fs::write(artifacts.join(name), "x").unwrap();
        aged(&artifacts.join(name), 90, &world.clock);
    }
    world.sweep().await;
    assert!(artifacts.join("a.png").exists() && artifacts.join("b.png").exists());

    world.rules(|rules| rules["browserArtifactsAfterDays"] = json!(7));
    // Each deletion re-reads the setting; the first read of the pass is the sweep's own, the second precedes a deletion.
    *world.settings.on_read.lock().unwrap() = Some(Box::new(|reads, settings| {
        if reads >= 2 {
            settings["storageCleanup"]["browserArtifactsAfterDays"] = json!(365);
        }
    }));
    world.sweep().await;
    assert!(artifacts.join("a.png").exists() && artifacts.join("b.png").exists(), "the retention became longer before the first file was removed");
}

// ---- triggers --------------------------------------------------------------------------------------------------

#[tokio::test]
async fn sweeps_run_at_start_on_a_cleanup_setting_change_and_hourly_on_the_injected_clock() {
    let world = world(json!({"worktreeAfterDays": null}));
    let first = world.thread("first");
    let second = world.thread("second");
    // Older than any retention below, so the first sweep to see a rule removes them.
    world.days_later(60);
    let mut done = world.cleanup.completed();
    world.cleanup.start(None).await;
    tokio::time::timeout(HANG_BOUND, done.wait_for(|n| *n >= 1)).await.unwrap().unwrap();
    assert!(first.exists() && second.exists(), "the sweep at start found no rule");

    // A change to a cleanup setting sweeps with the new setting.
    let before = *done.borrow();
    world.settings.change({
        let mut changed = world.settings.value.lock().unwrap().clone();
        changed["storageCleanup"]["worktreeAfterDays"] = json!(30);
        changed
    });
    tokio::time::timeout(HANG_BOUND, done.wait_for(|n| *n > before)).await.unwrap().unwrap();
    assert!(!first.exists() && !second.exists(), "the change was acted on");

    // An hour on the clock sweeps again, whatever changed.
    let third = world.thread("third");
    let before = *done.borrow();
    world.clock.advance(60 * 60 * 1000);
    tokio::time::timeout(HANG_BOUND, done.wait_for(|n| *n > before)).await.unwrap().unwrap();
    assert!(!third.exists());
    world.cleanup.shutdown().await;
}

#[tokio::test]
async fn deleting_a_thread_sweeps_when_a_delete_rule_exists_and_the_worktree_goes_when_its_effects_are_done() {
    let world = world(json!({"worktreeOnDelete": true}));
    let path = world.thread("deleted");
    let mut done = world.cleanup.completed();
    world.cleanup.start(None).await;
    tokio::time::timeout(HANG_BOUND, done.wait_for(|n| *n >= 1)).await.unwrap().unwrap();
    assert!(path.exists());
    let before = *done.borrow();
    ThreadService::new(world.store.clone()).dispatch(&json!({"type":"thread.delete","commandId":"delete","threadId":"deleted"}), Utc::now()).unwrap();
    tokio::time::timeout(HANG_BOUND, done.wait_for(|n| *n > before)).await.unwrap().unwrap();
    assert!(path.exists(), "the deletion woke a sweep, which found the thread's cleanup effects outstanding");
    world.store.transaction(|transaction| Ok(transaction.execute("UPDATE rust_effect_outbox SET status='succeeded'", [])?)).unwrap();
    world.cleanup.request();
    tokio::time::timeout(HANG_BOUND, world.cleanup.drain()).await.unwrap();
    assert!(!path.exists());
    world.cleanup.shutdown().await;
}

// ---- cleanup rules an import carried --------------------------------------------------------------------------

/// The fake settings, read through the hold an import leaves (`cleanup_hold.rs`).
struct Shared(Arc<FakeSettings>);
#[async_trait]
impl SettingsFeed for Shared {
    async fn current(&self) -> Result<Value, String> {
        self.0.current().await
    }
    async fn changes(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        self.0.changes().await
    }
}

#[tokio::test]
async fn rules_an_import_carried_delete_nothing_until_the_user_changes_one() {
    let world = world(json!({"worktreeAfterDays": 30}));
    let path = world.thread("old");
    std::fs::write(world.state_dir.join(t3_server::cleanup_hold::HOLD_FILE), br#"{"rules":null}"#).unwrap();
    let held = StorageCleanup::new(Options {
        store: world.store.clone(),
        state_dir: world.state_dir.clone(),
        settings: Arc::new(t3_server::cleanup_hold::HeldSettings { inner: Shared(world.settings.clone()), state_dir: world.state_dir.clone() }),
        git: world.git.clone(),
        pull_requests: world.prs.clone(),
        clock: world.clock.clone(),
        terminals: world.terminals.clone(),
    });
    world.days_later(400);
    tokio::time::timeout(HANG_BOUND, held.sweep()).await.expect("a sweep ends");
    assert!(path.exists(), "a worktree idle for 400 days stays while the carried rules are held");
    // The user changes a rule: what they set is acted on as written, and the hold is over.
    world.rules(|rules| rules["worktreeAfterDays"] = json!(31));
    tokio::time::timeout(HANG_BOUND, held.sweep()).await.expect("a sweep ends");
    assert!(!path.exists(), "the changed rule removes it");
    assert!(!world.state_dir.join(t3_server::cleanup_hold::HOLD_FILE).exists());
    assert!(world.branch_exists("old"));
}
