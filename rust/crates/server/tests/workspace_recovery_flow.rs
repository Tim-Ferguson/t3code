#![cfg(unix)]
//! A server that dies while a launched thread's workspace is being prepared. The previous
//! process is simulated by what it left in the state directory: a launch that committed, an
//! outbox effect it had claimed, and the worktree it had created. A second `ExecutionService`
//! on the same store is the restart. Nothing outside the temporary directory is touched and the
//! provider is the recording Codex fixture.
use chrono::Utc;
use serde_json::{Value, json};
use t3_server::{
    execution::ExecutionService,
    launch::ThreadLaunchService,
    persistence::{Store, StoredEvent},
    project::ProjectService,
    provider_registry::ProviderRegistry,
    thread::ThreadService,
};

struct Fixture {
    _directory: tempfile::TempDir,
    base: std::path::PathBuf,
    workspace: std::path::PathBuf,
    store: Store,
    providers: ProviderRegistry,
    record: std::path::PathBuf,
}

fn git(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(["-c", "user.email=test@test.com", "-c", "user.name=Test", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    let workspace = base.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q", "-b", "main"]);
    std::fs::write(workspace.join("README.md"), "hello\n").unwrap();
    git(&workspace, &["add", "."]);
    git(&workspace, &["commit", "-q", "-m", "initial"]);
    ProjectService::new(store.clone())
        .mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now())
        .unwrap();
    let record = base.join("codex-requests.jsonl");
    let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":base.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":record}]}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &base).await.unwrap();
    Fixture { _directory: directory, base, workspace, store, providers, record }
}

impl Fixture {
    fn launch(&self, message: bool) {
        let mut input = json!({"commandId":"launch","threadId":"thread","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","workspaceStrategy":{"type":"worktree","baseRef":"main"}});
        if message {
            input["initialMessage"] = json!({"text":"Do the work","attachments":[]});
        }
        ThreadLaunchService::with_providers(self.store.clone(), self.providers.clone()).launch(input, Utc::now()).unwrap();
    }
    fn worktrees(&self) -> Vec<std::path::PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(self.base.join("worktrees").join("workspace"))
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default();
        found.sort();
        found
    }
    fn provider_requests(&self, method: &str) -> Vec<Value> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|record| record["method"] == method)
            .collect()
    }
    fn preparation_effect(&self) -> String {
        self.store
            .read(|connection| {
                Ok(connection.query_row("SELECT id FROM rust_effect_outbox WHERE json_extract(request_json,'$.type')='workspace.prepare'", [], |row| row.get::<_, String>(0))?)
            })
            .unwrap()
    }
    /// What the lost process had done: it claimed the preparation, created a worktree on a
    /// temporary branch, noted the directory on the effect and, when `recorded`, recorded it on
    /// the thread.
    fn lost_process_left_a_worktree(&self, recorded: bool) -> std::path::PathBuf {
        let effect = self.preparation_effect();
        let claimed = self.store.claim_effect("lost-process", Utc::now(), chrono::Duration::seconds(60)).unwrap().expect("the launch left a preparation");
        assert_eq!((claimed.id.as_str(), claimed.attempt_count), (effect.as_str(), 1));
        let path = self.base.join("worktrees").join("workspace").join("t3-deadbeef");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        git(&self.workspace, &["worktree", "add", "-q", "-b", "t3/deadbeef", path.to_str().unwrap(), "main"]);
        self.store
            .transaction(|transaction| {
                transaction.execute("UPDATE rust_effect_outbox SET request_json=json_set(request_json,'$.claimedWorktree',?1) WHERE id=?2", rusqlite::params![path.to_str().unwrap(), effect])?;
                Ok(())
            })
            .unwrap();
        if recorded {
            ThreadService::new(self.store.clone())
                .dispatch(&json!({"type":"thread.metadata.update","commandId":"launch:workspace","threadId":"thread","branch":"t3/deadbeef","worktreePath":path}), Utc::now())
                .unwrap();
        }
        path
    }
}

async fn until(store: &Store, events: &mut tokio::sync::broadcast::Receiver<Vec<StoredEvent>>, done: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if let Some(view) = store.projection("thread", "thread").unwrap() {
                last = json!({"thread":view["thread"],"runs":view["runs"]});
                if done(&view) {
                    return view;
                }
            }
            events.recv().await.unwrap();
        }
    })
    .await;
    outcome.unwrap_or_else(|_| panic!("the thread never reached the wanted state: {last}"))
}
fn run_status(view: &Value, status: &str) -> bool {
    view["runs"].as_array().is_some_and(|runs| runs.len() == 1 && runs[0]["status"] == status)
}
fn branches(workspace: &std::path::Path) -> Vec<String> {
    git(workspace, &["branch", "--format=%(refname:short)"]).lines().map(str::to_owned).collect()
}

#[tokio::test]
async fn a_restart_during_preparation_fails_the_run_removes_the_worktree_it_created_and_offers_a_retry() {
    let fixture = fixture().await;
    fixture.launch(true);
    let path = fixture.lost_process_left_a_worktree(true);
    assert!(path.exists());
    let view = fixture.store.projection("thread", "thread").unwrap().unwrap();
    assert!(run_status(&view, "preparing"), "{}", view["runs"]);
    assert_eq!(view["thread"]["worktreePath"], path.to_str().unwrap());

    let mut events = fixture.store.subscribe();
    let execution = ExecutionService::start(fixture.store.clone(), fixture.providers.clone());
    let view = until(&fixture.store, &mut events, |view| run_status(view, "failed")).await;
    // As an interrupted preparation in the original: the checkout is gone and the thread
    // forgets it, so a retry does not reuse a checkout that may be half written.
    assert!(!path.exists());
    assert!(fixture.worktrees().is_empty());
    assert!(!git(&fixture.workspace, &["worktree", "list", "--porcelain"]).contains("t3-deadbeef"));
    assert_eq!(view["thread"]["worktreePath"], Value::Null);
    assert_eq!(view["thread"]["branch"], Value::Null);
    let error = view["turnItems"].as_array().unwrap().iter().find(|item| item["type"] == "error").unwrap().clone();
    assert_eq!(error["status"], "failed");
    assert_eq!(error["failure"]["message"], "Workspace preparation failed: Worktree setup cancelled.");
    assert!(fixture.provider_requests("thread/start").is_empty());

    // The failure offers the same retry as any other failed preparation: a new worktree, the
    // same run, and the provider starts in it.
    let run_id = view["runs"][0]["id"].clone();
    let mut events = fixture.store.subscribe();
    execution.dispatch(&json!({"type":"prepared-run.retry","commandId":"retry","threadId":"thread","runId":run_id}), Utc::now()).unwrap();
    let view = until(&fixture.store, &mut events, |view| run_status(view, "completed")).await;
    assert_eq!(view["runs"][0]["id"], run_id);
    let recorded = view["thread"]["worktreePath"].as_str().expect("the retry created and recorded a worktree").to_owned();
    assert_ne!(std::path::Path::new(&recorded), path);
    assert!(std::path::Path::new(&recorded).exists());
    assert_eq!(fixture.provider_requests("thread/start")[0]["params"]["cwd"], recorded);
    execution.shutdown().await;
    assert_eq!(fixture.store.effect(&fixture.preparation_effect()).unwrap().unwrap().status, "succeeded");
}

#[tokio::test]
async fn a_worktree_the_lost_process_created_but_never_recorded_is_removed_too() {
    let fixture = fixture().await;
    fixture.launch(true);
    let path = fixture.lost_process_left_a_worktree(false);
    assert!(path.exists());
    let mut events = fixture.store.subscribe();
    let execution = ExecutionService::start(fixture.store.clone(), fixture.providers.clone());
    let view = until(&fixture.store, &mut events, |view| run_status(view, "failed")).await;
    assert!(!path.exists());
    assert!(fixture.worktrees().is_empty());
    assert_eq!(view["thread"]["worktreePath"], Value::Null);
    execution.shutdown().await;
    // The temporary branch stays, as it does after the original's cancelled setups.
    assert!(branches(&fixture.workspace).contains(&"t3/deadbeef".to_owned()));
}

#[tokio::test]
async fn a_preparation_the_lost_process_never_started_runs_after_the_restart() {
    let fixture = fixture().await;
    fixture.launch(true);
    let effect = fixture.store.effect(&fixture.preparation_effect()).unwrap().unwrap();
    assert_eq!((effect.status.as_str(), effect.attempt_count), ("pending", 0));
    let mut events = fixture.store.subscribe();
    let execution = ExecutionService::start(fixture.store.clone(), fixture.providers.clone());
    let view = until(&fixture.store, &mut events, |view| run_status(view, "completed")).await;
    let recorded = view["thread"]["worktreePath"].as_str().expect("the worktree was created after the restart").to_owned();
    assert!(std::path::Path::new(&recorded).exists());
    assert_eq!(fixture.provider_requests("thread/start")[0]["params"]["cwd"], recorded);
    execution.shutdown().await;
}

#[tokio::test]
async fn a_restart_during_a_launch_without_a_message_removes_the_worktree_and_clears_the_thread() {
    let fixture = fixture().await;
    fixture.launch(false);
    let path = fixture.lost_process_left_a_worktree(true);
    let mut events = fixture.store.subscribe();
    let execution = ExecutionService::start(fixture.store.clone(), fixture.providers.clone());
    let view = until(&fixture.store, &mut events, |view| view["thread"]["worktreePath"].is_null()).await;
    assert!(!path.exists());
    assert!(view["runs"].as_array().unwrap().is_empty());
    execution.shutdown().await;
}

#[tokio::test]
async fn a_preparing_run_without_a_preparation_effect_is_still_cancelled_by_recovery() {
    let fixture = fixture().await;
    fixture.launch(true);
    // A state directory from before preparations were durable: nothing will ever release the run.
    let effect = fixture.preparation_effect();
    fixture.store.transaction(|transaction| {
        transaction.execute("DELETE FROM rust_effect_outbox WHERE id=?1", [&effect])?;
        Ok(())
    }).unwrap();
    let mut events = fixture.store.subscribe();
    let execution = ExecutionService::start(fixture.store.clone(), fixture.providers.clone());
    until(&fixture.store, &mut events, |view| run_status(view, "cancelled")).await;
    execution.shutdown().await;
}
