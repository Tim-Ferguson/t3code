#![cfg(unix)]
//! The native stack scope of pull request actions (`githubStackActions.ts`, `githubStackRebase.ts`): GitHub's stacks API and its
//! GraphQL are a local server that answers by what is asked, the repository is a local bare remote, and the scratch clone of the
//! rebase lives under the temporary directory. Nothing leaves loopback and the checkout of the environment is never involved.
use axum::{Router, body::Bytes, extract::State, http::{Method, StatusCode, Uri}, response::Response};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use t3_contracts::{PullRequestAction, PullRequestMergeMethod, PullRequestStackHead};
use t3_server::{
    pull_request_actions::RunAction,
    pull_request_github::{GitHub, Target},
    pull_request_stack::StackRun,
    source_control_host::{GitHubApiResolver, Host, HostEnv, HostOptions},
};

type Handler = Arc<dyn Fn(&str, &str, &Value) -> (u16, String) + Send + Sync>;

struct Fake {
    origin: String,
    seen: Arc<Mutex<Vec<(String, String, Value)>>>,
    _stop: tokio::sync::oneshot::Sender<()>,
}

#[derive(Clone)]
struct FakeState {
    seen: Arc<Mutex<Vec<(String, String, Value)>>>,
    handler: Handler,
}

async fn answer(State(state): State<FakeState>, method: Method, uri: Uri, body: Bytes) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let path = format!("{}{}", uri.path(), uri.query().map(|query| format!("?{query}")).unwrap_or_default());
    let (status, text) = (state.handler)(method.as_str(), &path, &parsed);
    state.seen.lock().unwrap().push((method.to_string(), path, parsed));
    Response::builder().status(StatusCode::from_u16(status).unwrap()).header("content-type", "application/json").body(axum::body::Body::from(text)).unwrap()
}

impl Fake {
    async fn start(handler: Handler) -> Fake {
        let seen = Arc::new(Mutex::new(vec![]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let app = Router::new().fallback(answer).with_state(FakeState { seen: seen.clone(), handler });
        tokio::spawn(async move {
            axum::serve(listener, app).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
        });
        Fake { origin, seen, _stop: stop }
    }
    fn resolver(&self) -> GitHubApiResolver {
        let origin = self.origin.clone();
        Arc::new(move |host| (format!("{origin}/rest/{host}"), format!("{origin}/graphql/{host}")))
    }
    fn writes(&self) -> Vec<String> {
        self.seen.lock().unwrap().iter().filter(|(method, ..)| method != "GET").map(|(method, path, _)| format!("{method} {path}")).collect()
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A bare remote with `main` and a stack of three branches, each one commit on the one below, and a work clone to move them.
struct World {
    _directory: tempfile::TempDir,
    remote: PathBuf,
    work: PathBuf,
}

fn commit(work: &Path, file: &str, text: &str) -> String {
    std::fs::write(work.join(file), text).unwrap();
    git(work, &["add", "."]);
    git(work, &["commit", "-q", "-m", &format!("{file}: {text}")]);
    git(work, &["rev-parse", "HEAD"])
}

impl World {
    fn new() -> (World, Vec<String>) {
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-stack-").tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let (remote, work) = (base.join("remote.git"), base.join("work"));
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "-q", "-b", "main"]);
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["remote", "add", "origin", remote.to_str().unwrap()]);
        commit(&work, "base.txt", "base\n");
        git(&work, &["push", "-q", "origin", "main"]);
        let mut heads = vec![];
        for layer in 1..=3 {
            git(&work, &["checkout", "-q", "-b", &format!("l{layer}")]);
            heads.push(commit(&work, &format!("layer{layer}.txt"), &format!("layer {layer}\n")));
            git(&work, &["push", "-q", "origin", &format!("l{layer}")]);
        }
        (World { _directory: directory, remote, work }, heads)
    }
    /// Moves the base on without touching any layer's files.
    fn advance_main(&self, file: &str, text: &str) -> String {
        git(&self.work, &["checkout", "-q", "main"]);
        let head = commit(&self.work, file, text);
        git(&self.work, &["push", "-q", "origin", "main"]);
        head
    }
    fn head(&self, branch: &str) -> String {
        git(&self.remote, &["rev-parse", &format!("refs/heads/{branch}")])
    }
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        Command::new("git").current_dir(&self.remote).args(["merge-base", "--is-ancestor", ancestor, descendant]).env("GIT_CONFIG_GLOBAL", "/dev/null").status().unwrap().success()
    }
    fn count_since(&self, base: &str, branch: &str) -> usize {
        git(&self.remote, &["rev-list", "--count", &format!("{base}..{branch}")]).parse().unwrap()
    }
}

fn stack_json(heads: &[String], states: [&str; 3]) -> String {
    let layers: Vec<Value> = (1..=3).map(|number| json!({"number":number,"title":format!("Layer {number}"),"draft":false,"head":{"sha":heads[number - 1],"ref":format!("l{number}")},"state":states[number - 1],"merged_at":if states[number - 1] == "merged" { json!("2026-01-01T00:00:00Z") } else { Value::Null }})).collect();
    json!([{"number":9,"id":"stack-9","url":"https://api.example/stacks/9","html_url":"https://github.com/acme/web/stacks/9","base":"main","pull_requests":layers}]).to_string()
}

fn host(fake: &Fake, remote: Option<&Path>) -> Host {
    let remote = remote.map(Path::to_path_buf);
    Host::new(HostOptions {
        env: HostEnv::from_pairs([("GH_TOKEN", "ghp_stack_token"), ("GIT_CONFIG_GLOBAL", "/dev/null"), ("GIT_CONFIG_SYSTEM", "/dev/null")]),
        github_api: Some(fake.resolver()),
        git_remote: remote.map(|remote| Arc::new(move |_host: &str, _repository: &str| remote.to_string_lossy().into_owned()) as t3_server::source_control_host::GitRemoteResolver),
        // Polling waits a millisecond where it waits a second.
        stack_poll_unit: Some(std::time::Duration::from_millis(1)),
        ..HostOptions::default()
    })
}

fn target(number: u64) -> Target {
    Target { host: "github.com".into(), repository: "acme/web".into(), number }
}

fn run(action: PullRequestAction) -> RunAction {
    RunAction { action, merge_method: Some(PullRequestMergeMethod::Squash), update_method: None, remove_agent_credits: false }
}

fn reviewed(heads: &[String]) -> Option<Vec<PullRequestStackHead>> {
    Some(heads.iter().enumerate().map(|(index, sha)| PullRequestStackHead { number: index as u64 + 1, head_sha: sha.clone() }).collect())
}

fn permissions(allowed: bool) -> String {
    let access = |number: u64| (format!("pr{number}"), if allowed { json!({"headRepository":{"viewerPermission":"WRITE"},"maintainerCanModify":false}) } else { json!({"headRepository":{"viewerPermission":"READ"},"maintainerCanModify":false}) });
    json!({"data":{"repository":(1..=3).map(access).collect::<serde_json::Map<_, _>>()}}).to_string()
}

#[tokio::test]
async fn a_rebase_moves_each_layers_own_commits_onto_the_new_base_and_pushes_with_a_lease() {
    let (world, heads) = World::new();
    world.advance_main("main2.txt", "later\n");
    let fresh = heads.clone();
    let stack = stack_json(&heads, ["open"; 3]);
    let fake = Fake::start(Arc::new(move |method, path, _| match (method, path) {
        ("GET", path) if path.contains("/stacks?pull_request=3") => (200, stack.clone()),
        ("POST", path) if path.contains("/graphql/") => (200, permissions(true)),
        _ => (404, "{}".into()),
    }))
    .await;
    let github = GitHub::new(host(&fake, Some(&world.remote)));
    let stack_run = StackRun { stack_number: 9, expected_heads: reviewed(&fresh) };
    github.run_stack_action(&target(3), &run(PullRequestAction::UpdateBranch), &stack_run).await.unwrap();
    // Every layer sits on the new base, in order, and carries only its own commit.
    let (main, l1, l2, l3) = (world.head("main"), world.head("l1"), world.head("l2"), world.head("l3"));
    assert!(world.is_ancestor(&main, &l1) && world.is_ancestor(&l1, &l2) && world.is_ancestor(&l2, &l3));
    assert_eq!((world.count_since("main", "l1"), world.count_since("l1", "l2"), world.count_since("l2", "l3")), (1, 1, 1));
    assert!(heads.iter().all(|old| *old != l1 && *old != l2 && *old != l3));
    // Nothing was sent to GitHub but reads and the permission query: the push is git's.
    assert!(fake.writes().iter().all(|request| request.contains("/graphql/")), "{:?}", fake.writes());
}

#[tokio::test]
async fn a_conflict_stops_at_that_layer_and_the_layers_below_it_stay_rebased() {
    let (world, heads) = World::new();
    // The base now edits the file layer 2 created, so layer 2 cannot sit on it.
    git(&world.work, &["checkout", "-q", "main"]);
    commit(&world.work, "layer2.txt", "from the base\n");
    git(&world.work, &["push", "-q", "origin", "main"]);
    let stack = stack_json(&heads, ["open"; 3]);
    let fake = Fake::start(Arc::new(move |method, path, _| match (method, path) {
        ("GET", path) if path.contains("/stacks?pull_request=3") => (200, stack.clone()),
        ("POST", path) if path.contains("/graphql/") => (200, permissions(true)),
        _ => (404, "{}".into()),
    }))
    .await;
    let github = GitHub::new(host(&fake, Some(&world.remote)));
    let error = github.run_stack_action(&target(3), &run(PullRequestAction::UpdateBranch), &StackRun { stack_number: 9, expected_heads: reviewed(&heads) }).await.unwrap_err();
    assert_eq!(error.detail, "PR #2 conflicts with the layer below it. 1 layers were rebased; resolve #2 with `gh stack rebase` and push.");
    assert!(world.is_ancestor(&world.head("main"), &world.head("l1")), "the layer below the conflict was rebased");
    assert_eq!(world.head("l2"), heads[1], "the layer that conflicted is as it was");
    assert_eq!(world.head("l3"), heads[2]);
}

#[tokio::test]
async fn a_stack_that_changed_or_cannot_be_written_is_refused_before_anything_moves() {
    let (world, heads) = World::new();
    world.advance_main("main2.txt", "later\n");
    let stack = stack_json(&heads, ["open"; 3]);
    let allowed = Arc::new(Mutex::new(true));
    let gate = allowed.clone();
    let fake = Fake::start(Arc::new(move |method, path, _| match (method, path) {
        ("GET", path) if path.contains("/stacks?pull_request=3") => (200, stack.clone()),
        ("GET", path) if path.contains("/stacks?pull_request=1") => (200, "[]".into()),
        ("POST", path) if path.contains("/graphql/") => (200, permissions(*gate.lock().unwrap())),
        _ => (404, "{}".into()),
    }))
    .await;
    let github = GitHub::new(host(&fake, Some(&world.remote)));
    let update = run(PullRequestAction::UpdateBranch);
    // The reader reviewed another head of layer 2.
    let mut stale = reviewed(&heads).unwrap();
    stale[1].head_sha = "0".repeat(40);
    let error = github.run_stack_action(&target(3), &update, &StackRun { stack_number: 9, expected_heads: Some(stale) }).await.unwrap_err();
    assert_eq!(error.detail, "The stack changed. Refresh it before trying again.");
    // No heads at all, the wrong stack, and a layer that is not the top (a rebase is of the whole stack from its top).
    for (stack_number, expected, number, text) in [
        (9, None, 3, "The stack changed. Refresh it before trying again."),
        (8, reviewed(&heads), 3, "The stack changed. Refresh it before trying again."),
        (9, reviewed(&heads), 1, "The stack changed. Refresh it before trying again."),
    ] {
        let error = github.run_stack_action(&target(number), &update, &StackRun { stack_number, expected_heads: expected }).await.unwrap_err();
        assert_eq!(error.detail, text);
    }
    // An action that has no stack scope.
    let error = github.run_stack_action(&target(3), &run(PullRequestAction::Close), &StackRun { stack_number: 9, expected_heads: reviewed(&heads) }).await.unwrap_err();
    assert_eq!(error.detail, "This operation is not supported for this stack.");
    // No write access to some layer's branch.
    *allowed.lock().unwrap() = false;
    let error = github.run_stack_action(&target(3), &update, &StackRun { stack_number: 9, expected_heads: reviewed(&heads) }).await.unwrap_err();
    assert_eq!(error.detail, "You cannot update every branch in this stack. Check write access and fork maintainer permissions before retrying.");
    assert_eq!((world.head("l1"), world.head("l2"), world.head("l3")), (heads[0].clone(), heads[1].clone(), heads[2].clone()), "nothing moved");
}

#[tokio::test]
async fn a_stack_merge_waits_for_github_to_finish_and_reports_how_it_ended() {
    let heads: Vec<String> = ["a", "b", "c"].iter().map(|text| text.repeat(40)).collect();
    let stack = stack_json(&heads, ["merged", "open", "open"]);
    let polls = Arc::new(Mutex::new(0));
    let outcome = Arc::new(Mutex::new("merged"));
    let (counter, final_status) = (polls.clone(), outcome.clone());
    let fake = Fake::start(Arc::new(move |method, path, body| match (method, path) {
        ("GET", path) if path.contains("/stacks?pull_request=3") => (200, stack.clone()),
        ("PUT", path) if path.ends_with("/pulls/3/merge-async") => {
            assert_eq!(body, &json!({"merge_method":"squash","merge_action":"default","sha":"c".repeat(40)}), "the merge names the head that was reviewed");
            *counter.lock().unwrap() = 0;
            (200, json!({"status":"pending","details":{"uuid":"job 1"}}).to_string())
        }
        ("GET", path) if path.ends_with("/pulls/3/merge-async/job%201") => {
            let mut polls = counter.lock().unwrap();
            *polls += 1;
            let status = if *polls < 3 { "pending" } else { *final_status.lock().unwrap() };
            (200, json!({"status":status,"details":{"uuid":"job 1","message":"done"}}).to_string())
        }
        _ => (404, "{}".into()),
    }))
    .await;
    let github = GitHub::new(host(&fake, None));
    // Layer 1 is merged already, so the reader reviewed the two open ones.
    let expected = Some(vec![PullRequestStackHead { number: 2, head_sha: heads[1].clone() }, PullRequestStackHead { number: 3, head_sha: heads[2].clone() }]);
    let merge = run(PullRequestAction::Merge);
    github.run_stack_action(&target(3), &merge, &StackRun { stack_number: 9, expected_heads: expected.clone() }).await.unwrap();
    assert_eq!(*polls.lock().unwrap(), 3, "polled until GitHub said it was done");
    *outcome.lock().unwrap() = "failed";
    let error = github.run_stack_action(&target(3), &merge, &StackRun { stack_number: 9, expected_heads: expected.clone() }).await.unwrap_err();
    assert_eq!(error.detail, "GitHub refused the stack merge. Check the stack's branch rules and merge requirements.");
    // A request that is still running when the five minutes end.
    *outcome.lock().unwrap() = "pending";
    let error = github.run_stack_action(&target(3), &merge, &StackRun { stack_number: 9, expected_heads: expected.clone() }).await.unwrap_err();
    assert_eq!(error.detail, "The merge is still running on GitHub. Check its status there before submitting another request.");
    // A draft layer cannot be merged.
    let drafts = stack_json(&heads, ["merged", "open", "open"]).replace("\"draft\":false", "\"draft\":true");
    let fake = Fake::start(Arc::new(move |_, path, _| if path.contains("/stacks?pull_request=3") { (200, drafts.clone()) } else { (404, "{}".into()) })).await;
    let error = GitHub::new(host(&fake, None)).run_stack_action(&target(3), &merge, &StackRun { stack_number: 9, expected_heads: expected }).await.unwrap_err();
    assert_eq!(error.detail, "This operation is not supported for this stack.");
}
