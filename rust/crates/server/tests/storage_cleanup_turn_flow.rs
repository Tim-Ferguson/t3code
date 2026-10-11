#![cfg(unix)]
//! The companion of storage cleanup: a thread whose worktree was removed gets it back from its branch when its
//! next turn starts, through the real effect loop and a recording Codex fixture. Everything lives in a temporary
//! directory; the "remote" is a local repository.
use chrono::Utc;
use serde_json::{Value, json};
use std::{path::Path, process::Command};
use t3_server::{
    execution::ExecutionService,
    persistence::{Store, StoredEvent},
    project::ProjectService,
    provider_registry::ProviderRegistry,
    test_support::HANG_BOUND,
    thread::ThreadService,
};

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
}

async fn until(store: &Store, events: &mut tokio::sync::broadcast::Receiver<Vec<StoredEvent>>, done: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(HANG_BOUND, async {
        loop {
            let view = store.projection("thread", "thread").unwrap().unwrap();
            if done(&view) {
                return view;
            }
            events.recv().await.unwrap();
        }
    })
    .await
    .expect("the thread reached the wanted state")
}

#[tokio::test]
async fn a_turn_recreates_the_threads_missing_worktree_from_its_branch_before_the_provider_starts() {
    let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-recreate-").tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let state_dir = base.join("home/userdata");
    std::fs::create_dir_all(&state_dir).unwrap();
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    git(&project, &["init", "-b", "main"]);
    std::fs::write(project.join("README.md"), "base\n").unwrap();
    git(&project, &["add", "."]);
    git(&project, &["commit", "-m", "base"]);
    let worktree = base.join("home/worktrees/thread");
    git(&project, &["worktree", "add", "-b", "thread-branch", worktree.to_str().unwrap()]);
    let worktree = std::fs::canonicalize(&worktree).unwrap();
    std::fs::write(worktree.join("committed.txt"), "kept\n").unwrap();
    git(&worktree, &["add", "committed.txt"]);
    git(&worktree, &["commit", "-m", "work"]);
    // What cleanup does: the checkout goes, the branch and the thread's path stay.
    git(&project, &["worktree", "remove", worktree.to_str().unwrap()]);
    assert!(!worktree.exists());

    let store = Store::open(state_dir.join("rust-state.sqlite")).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":project}), Utc::now()).unwrap();
    let receipt = ThreadService::new(store.clone())
        .dispatch(&json!({"type":"thread.create","commandId":"create","threadId":"thread","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","branch":"thread-branch","worktreePath":worktree}), Utc::now())
        .unwrap();
    assert_eq!(receipt.status, "accepted", "{receipt:?}");
    let record = base.join("codex-requests.jsonl");
    let binary = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":base.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":record}]}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &base).await.unwrap();
    let execution = ExecutionService::start(store.clone(), providers);
    let mut events = store.subscribe();
    execution.dispatch(&json!({"type":"message.dispatch","commandId":"send","threadId":"thread","messageId":"send","text":"hello","attachments":[],"dispatchMode":{"type":"start_immediately"}}), Utc::now()).unwrap();
    let view = until(&store, &mut events, |view| view["runs"].as_array().is_some_and(|runs| runs.iter().all(|run| run["status"] == "completed") && !runs.is_empty())).await;
    assert_eq!(view["runs"][0]["status"], "completed", "{}", view["runs"][0]);
    assert!(worktree.join(".git").is_file(), "the checkout is back at the thread's path");
    assert_eq!(std::fs::read_to_string(worktree.join("committed.txt")).unwrap(), "kept\n", "from the branch, with its commits");
    let started = std::fs::read_to_string(&record).unwrap().lines().any(|line| serde_json::from_str::<Value>(line).unwrap()["method"] == "turn/start");
    assert!(started, "the provider ran its turn in it");
    execution.shutdown().await;
}
