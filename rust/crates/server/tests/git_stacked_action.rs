//! The stacked git action against `fixtures/git-stacked-action.jsonl`, recorded from the original
//! `GitManager.runStackedAction` by `rust/tools/generate_git_stacked_action_fixtures.mjs`. Every
//! scenario is built from real temporary repositories with a local bare remote that the checkout's
//! GitHub URL is rewritten to; GitHub itself is a local server that answers by what is asked.
//! The workflow runs with the same scripted text generation the recording used, and the progress
//! events, the result or error, the requests made of GitHub and the state of both repositories
//! must come out as they did. No network, no real `gh`, no real credentials.
#![recursion_limit = "256"]
mod source_control_support;

use axum::{Router, body::Bytes, extract::State, http::{HeaderMap, Method, StatusCode, Uri}, response::Response};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use source_control_support::*;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use t3_server::{
    git_workflow::WorkflowResult,
    git_workflow_stacked::*,
    persistence::Store,
    pull_request_service::Env,
    source_control_host::Host,
};

type Log = Arc<Mutex<Vec<Value>>>;

fn rows() -> Vec<Value> {
    include_str!("fixtures/git-stacked-action.jsonl").lines().filter(|line| !line.is_empty()).map(|line| serde_json::from_str(line).unwrap()).collect()
}

// ---- GitHub, answering by what is asked --------------------------------------------------------

#[derive(Clone)]
struct GithubState {
    prs: Arc<Mutex<Vec<Value>>>,
    log: Log,
    recorded: Arc<Mutex<Vec<Recorded>>>,
}

fn node_of(pr: &Value) -> Value {
    let state = pr["state"].as_str().unwrap_or("open").to_uppercase();
    json!({"number":pr["number"],"title":pr["title"],"url":pr["url"],"baseRefName":pr["baseRefName"],"headRefName":pr["headRefName"],"headRefOid":"abc1234","state":state,"isDraft":false,
        "mergedAt":null,"closedAt":null,"updatedAt":"2026-07-01T00:00:00Z","isCrossRepository":false,"headRepository":{"name":"web","nameWithOwner":"acme/web"},"headRepositoryOwner":{"login":"acme"}})
}

async fn serve(State(state): State<GithubState>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_string);
    let raw_body = String::from_utf8_lossy(&body).into_owned();
    let parsed: Option<Value> = serde_json::from_str(&raw_body).ok();
    state.recorded.lock().unwrap().push(Recorded {
        method: method.to_string(),
        path: uri.path().to_string(),
        query: uri.query().map(str::to_string),
        authorization: header("authorization"),
        accept: header("accept"),
        if_none_match: header("if-none-match"),
        body: parsed.clone(),
        raw_body,
    });
    let reply = |value: Value| Response::builder().status(StatusCode::OK).header("content-type", "application/json").body(axum::body::Body::from(value.to_string())).unwrap();
    let path = uri.path();
    if path.starts_with("/graphql/") {
        let variables = parsed.as_ref().map(|body| body["variables"].clone()).unwrap_or(Value::Null);
        let head = variables["h0"].as_str().unwrap_or("").to_string();
        let states: Vec<String> = variables["s0"].as_array().map(|states| states.iter().filter_map(|s| s.as_str().map(str::to_lowercase)).collect()).unwrap_or_default();
        state.log.lock().unwrap().push(json!({"op":"list","headSelector":head,"state":if states.len() == 1 { states[0].clone() } else { "all".to_string() }}));
        let nodes: Vec<Value> = state
            .prs
            .lock()
            .unwrap()
            .iter()
            .filter(|pr| pr["headRefName"] == head.as_str() && states.contains(&pr["state"].as_str().unwrap_or("open").to_string()))
            .map(node_of)
            .collect();
        return reply(json!({"data":{"repository":{"h0":{"nodes":nodes}}}}));
    }
    if method == Method::POST && path.ends_with("/repos/acme/web/pulls") {
        let body = parsed.unwrap();
        state.log.lock().unwrap().push(json!({"op":"create","baseRefName":body["base"],"headSelector":body["head"],"title":body["title"],"body":body["body"]}));
        let head = body["head"].as_str().unwrap().rsplit(':').next().unwrap().to_string();
        state.prs.lock().unwrap().push(json!({"number":42,"title":body["title"],"url":"https://github.com/acme/web/pull/42","baseRefName":body["base"],"headRefName":head,"state":"open"}));
        return reply(json!({"number":42,"html_url":"https://github.com/acme/web/pull/42"}));
    }
    if method == Method::GET && path.ends_with("/repos/acme/web") {
        return reply(json!({"full_name":"acme/web","html_url":"https://github.com/acme/web","ssh_url":"git@github.com:acme/web.git","default_branch":"main"}));
    }
    Response::builder().status(StatusCode::NOT_FOUND).body(axum::body::Body::from(json!({"message":format!("unscripted {method} {path}")}).to_string())).unwrap()
}

struct Github {
    origin: String,
    state: GithubState,
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl Github {
    async fn start(log: Log) -> Github {
        let state = GithubState { prs: Arc::default(), log, recorded: Arc::default() };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let app = Router::new().fallback(serve).with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
        });
        Github { origin, state, _stop: stop }
    }
    fn resolver(&self) -> t3_server::source_control_host::GitHubApiResolver {
        let origin = self.origin.clone();
        Arc::new(move |host| (format!("{origin}/rest/{host}"), format!("{origin}/graphql/{host}")))
    }
}

// ---- the text the recording scripted ---------------------------------------------------------------

struct Scripted(Log);

impl TextGeneration for Scripted {
    fn commit_message<'a>(&'a self, context: CommitContext<'a>) -> BoxFuture<'a, WorkflowResult<GeneratedCommit>> {
        self.0.lock().unwrap().push(json!({"op":"generateCommitMessage","branch":context.branch,"includeBranch":context.include_branch}));
        Box::pin(async move {
            Ok(GeneratedCommit {
                subject: "Implement stacked git actions".into(),
                body: String::new(),
                branch: context.include_branch.then(|| "feature/implement-stacked-git-actions".to_string()),
            })
        })
    }
    fn pr_content<'a>(&'a self, context: PrContext<'a>) -> BoxFuture<'a, WorkflowResult<GeneratedPr>> {
        self.0.lock().unwrap().push(json!({"op":"generatePrContent","baseBranch":context.base_branch,"headBranch":context.head_branch}));
        Box::pin(async move { Ok(GeneratedPr { title: "Add stacked git actions".into(), body: "## Summary\n- Add stacked git workflow\n\n## Testing\n- Not run".into() }) })
    }
}

// ---- building the repositories --------------------------------------------------------------------

fn play(steps: &[Value], work: &Path, remote: &Path) {
    let substitute = |args: &Value| -> Vec<String> { args.as_array().unwrap().iter().map(|arg| arg.as_str().unwrap().replace("{remote}", &remote.to_string_lossy())).collect() };
    for step in steps {
        if let Some(args) = step.get("remote") {
            git(remote, &substitute(args).iter().map(String::as_str).collect::<Vec<_>>());
        } else if let Some(args) = step.get("work") {
            git(work, &substitute(args).iter().map(String::as_str).collect::<Vec<_>>());
        } else if let Some(write) = step.get("write") {
            let file = work.join(write["path"].as_str().unwrap());
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, write["content"].as_str().unwrap()).unwrap();
        } else if let Some(hook) = step.get("hook") {
            executable(&work.join(".git/hooks").join(hook["name"].as_str().unwrap()), hook["script"].as_str().unwrap());
        } else if let Some(hook) = step.get("remoteHook") {
            executable(&remote.join("hooks").join(hook["name"].as_str().unwrap()), hook["script"].as_str().unwrap());
        } else {
            panic!("unknown step {step}");
        }
    }
}

fn executable(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Commit ids, paths and timings differ from run to run, so both sides are compared with markers.
fn clean(value: &Value, head: &str, work: &str, remote: &str) -> Value {
    fn walk(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, member) in map.iter_mut() {
                    if key == "durationMs" && member.is_number() {
                        *member = json!("<ms>");
                    } else {
                        walk(member);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    let sha = regex::Regex::new(r"\b[0-9a-f]{40}\b").unwrap();
    let text = value.to_string().replace(head, "<sha>").replace(&head[..7], "<sha7>").replace(work, "<cwd>").replace(remote, "<remote>");
    let mut cleaned: Value = serde_json::from_str(&sha.replace_all(&text, "<sha>")).unwrap();
    walk(&mut cleaned);
    cleaned
}

async fn replay(row: &Value, host: &Host, store: &Store) {
    let name = row["name"].as_str().unwrap();
    let root = tempfile::tempdir().unwrap();
    let root_path = std::fs::canonicalize(root.path()).unwrap();
    let (work, remote) = (root_path.join("work"), root_path.join("remote.git"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    play(row["base"].as_array().unwrap(), &work, &remote);
    play(row["steps"].as_array().unwrap(), &work, &remote);

    let log: Log = Arc::default();
    let github = Github::start(log.clone()).await;
    *github.state.prs.lock().unwrap() = row["prs"].as_array().unwrap().to_vec();
    // The host must see this scenario's server; the process host is only a default.
    let cli = FakeCliDir::new();
    let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), Some(github.resolver()), &root_path);
    let env = Env { host: test.host.clone(), store: store.clone() };
    let _ = host;

    let mut input = row["input"].clone();
    input["actionId"] = json!("action-1");
    input["cwd"] = json!(work.to_string_lossy());
    let input: StackedInput = serde_json::from_value(input).unwrap();
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let outcome = Box::pin(run_stacked_action(&env, &Scripted(log.clone()), &input, events)).await;
    let mut seen = vec![];
    while let Ok(event) = received.try_recv() {
        seen.push(event);
    }

    // A hook's end is read from git's own trace of the commit, so how it falls between the hook's
    // output and the commit's own summary lines depends on how fast the hook was; the original
    // sometimes only learns of it when the commit is over. Its start, its output and its exit
    // status are what is compared, and that git reports the end at all is checked on its own.
    if row["input"]["action"] == "commit" && name == "commit hook that talks" {
        let finished = seen.iter().position(|event| event["kind"] == "hook_finished").expect("the hook's end is reported");
        assert!(finished > seen.iter().position(|event| event["kind"] == "hook_started").unwrap());
        assert_eq!((seen[finished]["hookName"].as_str(), seen[finished]["exitCode"].as_i64()), (Some("pre-commit"), Some(0)));
    }
    let seen: Vec<Value> = seen
        .into_iter()
        .filter(|event| event["kind"] != "hook_finished")
        .map(|mut event| {
            if event["kind"] == "hook_output" && event["stream"] == "stdout" {
                event["hookName"] = Value::Null;
            }
            event
        })
        .collect();
    let head = git(&work, &["rev-parse", "HEAD"]);
    let (work_text, remote_text) = (work.to_string_lossy().into_owned(), remote.to_string_lossy().into_owned());
    let normal = |value: &Value| clean(value, &head, &work_text, &remote_text);
    let recorded: Vec<Value> = row["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] != "hook_finished")
        .cloned()
        .map(|mut event| {
            if event["kind"] == "hook_output" && event["stream"] == "stdout" {
                event["hookName"] = Value::Null;
            }
            event
        })
        .collect();
    assert_eq!(normal(&Value::Array(seen)), Value::Array(recorded), "{name}: events");
    match (&outcome, row["ok"].as_bool().unwrap()) {
        (Ok(result), true) => assert_eq!(normal(result), row["result"], "{name}: result"),
        (Err(error), false) => {
            let wire = error.wire();
            assert_eq!(wire["_tag"], row["error"]["tag"], "{name}: error");
            assert_eq!(normal(&json!(error.message())), row["error"]["message"], "{name}: error message");
        }
        (other, expected) => panic!("{name}: expected ok={expected}, got {other:?}"),
    }
    assert_eq!(normal(&Value::Array(log.lock().unwrap().clone())), row["providerCalls"], "{name}: what GitHub and the writer were asked");
    let refs: Vec<String> = {
        let mut refs: Vec<String> = git(&remote, &["for-each-ref", "--format=%(refname) %(subject)", "refs/heads"]).lines().map(str::to_string).filter(|line| !line.is_empty()).collect();
        refs.sort();
        refs
    };
    let after = json!({"branch":git(&work, &["rev-parse", "--abbrev-ref", "HEAD"]),"subject":git(&work, &["log", "-1", "--format=%B"]).trim(),"remoteRefs":refs,"status":git(&work, &["status", "--porcelain"])});
    assert_eq!(normal(&after), row["after"], "{name}: what the repositories hold afterwards");
}

#[test]
fn every_scenario_matches_the_original() {
    let rows = rows();
    assert_eq!(rows.len(), 26);
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
        let work = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(work.path()).unwrap();
        // Installed first so the process-wide caches the workflow touches never see a real host.
        let cli = FakeCliDir::new();
        let test = test_host(&[("GH_TOKEN", "ghp_test_token_value")], Some(cli.path()), None, &base);
        assert!(Host::install(test.host.clone()), "this binary installs the process host once");
        let store = Store::open(base.join("state.sqlite")).unwrap();
        for row in &rows {
            Box::pin(replay(row, &test.host, &store)).await;
        }
    });
}
