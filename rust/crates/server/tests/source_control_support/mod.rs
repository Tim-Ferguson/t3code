//! Shared fixtures of the source-control tests: a scripted HTTP server standing in for a
//! host's API, fake host CLIs on a private `PATH`, throwaway git repositories, and a clock that
//! only moves when told to. Nothing here touches the network, a real host CLI or credentials.
#![allow(dead_code)]
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::Response,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_server::source_control_host::{GitHubApiResolver, Host, HostEnv, HostOptions};
use t3_vcs::{Clock, VcsProcess};

/// Where a host's API is pointed when a test did not give it a fake: loopback, nothing listening.
pub const UNREACHABLE_ORIGIN: &str = "http://127.0.0.1:1";

/// 2026-10-05T12:00:00Z, the instant the fixtures were recorded at.
pub const NOW_MS: i64 = 1_791_201_600_000;

/// A clock fixed at `NOW_MS` plus whatever it was advanced by.
#[derive(Default)]
pub struct FixedClock {
    elapsed: Mutex<Duration>,
}

impl FixedClock {
    pub fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
    }
}

impl Clock for FixedClock {
    fn monotonic(&self) -> Duration {
        *self.elapsed.lock().unwrap()
    }
    fn utc_now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(NOW_MS).unwrap() + chrono::Duration::from_std(self.monotonic()).unwrap()
    }
}

#[derive(Debug, Clone)]
pub struct Step {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl Step {
    /// A step as the fixtures script it: `{status?, headers?, body? | raw?}`.
    pub fn from_fixture(step: &Value) -> Step {
        let body = match (step.get("raw"), step.get("body")) {
            (Some(Value::String(raw)), _) => Some(raw.clone()),
            (_, Some(body)) if !body.is_null() => Some(body.to_string()),
            _ => None,
        };
        Step {
            status: step["status"].as_u64().unwrap_or(200) as u16,
            headers: step["headers"]
                .as_object()
                .map(|headers| headers.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string())).collect())
                .unwrap_or_default(),
            body,
        }
    }

    pub fn json(status: u16, body: Value) -> Step {
        Step { status, headers: Vec::new(), body: Some(body.to_string()) }
    }
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub authorization: Option<String>,
    pub accept: Option<String>,
    pub if_none_match: Option<String>,
    pub body: Option<Value>,
    pub raw_body: String,
}

struct ServerState {
    script: Vec<Step>,
    recorded: Vec<Recorded>,
    /// A file that gains a line for every 401 served, which a fake CLI reads to hand out a
    /// new token once the old one was refused.
    refusals: Option<PathBuf>,
}

/// An HTTP server that answers from a script (the last step repeats) and records requests.
pub struct ScriptedServer {
    pub origin: String,
    state: Arc<Mutex<ServerState>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

async fn answer(State(state): State<Arc<Mutex<ServerState>>>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_string);
    let raw_body = String::from_utf8_lossy(&body).into_owned();
    let step = {
        let mut state = state.lock().unwrap();
        let recorded = Recorded {
            method: method.to_string(),
            path: uri.path().to_string(),
            query: uri.query().map(str::to_string),
            authorization: header("authorization"),
            accept: header("accept"),
            if_none_match: header("if-none-match"),
            body: serde_json::from_str(&raw_body).ok(),
            raw_body,
        };
        state.recorded.push(recorded);
        let index = (state.recorded.len() - 1).min(state.script.len().saturating_sub(1));
        let step = state.script.get(index).cloned().unwrap_or(Step { status: 200, headers: Vec::new(), body: Some("{}".into()) });
        if step.status == 401
            && let Some(file) = &state.refusals
        {
            use std::io::Write;
            let mut log = std::fs::OpenOptions::new().create(true).append(true).open(file).unwrap();
            writeln!(log, "refused").unwrap();
        }
        step
    };
    let mut response = Response::builder().status(StatusCode::from_u16(step.status).unwrap());
    response = response.header("content-type", "application/json");
    for (name, value) in &step.headers {
        response = response.header(name.as_str(), value.as_str());
    }
    response.body(axum::body::Body::from(step.body.unwrap_or_default())).unwrap()
}

impl ScriptedServer {
    pub async fn start(script: Vec<Step>) -> ScriptedServer {
        let state = Arc::new(Mutex::new(ServerState { script, recorded: Vec::new(), refusals: None }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let app = Router::new().fallback(answer).with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        ScriptedServer { origin, state, stop: Some(stop) }
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().recorded.clone()
    }

    pub fn log_refusals_to(&self, file: PathBuf) {
        self.state.lock().unwrap().refusals = Some(file);
    }

    pub fn set_script(&self, script: Vec<Step>) {
        let mut state = self.state.lock().unwrap();
        state.script = script;
        state.recorded.clear();
    }

    /// Redirects the GitHub API of every host to this server: REST under `/rest/<host>` and
    /// GraphQL under `/graphql/<host>`, so the host a request was meant for stays visible.
    pub fn github_resolver(&self) -> GitHubApiResolver {
        let origin = self.origin.clone();
        Arc::new(move |host| (format!("{origin}/rest/{host}"), format!("{origin}/graphql/{host}")))
    }

    /// A recorded request as the URL the real GitHub would have been sent: the real API base
    /// of its host plus the part of the path below the redirect.
    pub fn original_url(request: &Recorded) -> String {
        let mut parts = request.path.trim_start_matches('/').splitn(3, '/');
        let (kind, host, rest) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap_or(""));
        let (rest_base, graphql_base) = t3_server::source_control_github_api::github_api_urls(host);
        let mut url = if kind == "graphql" { graphql_base } else { format!("{rest_base}/{rest}") };
        if kind == "graphql" && !rest.is_empty() {
            url.push_str(&format!("/{rest}"));
        }
        if let Some(query) = &request.query {
            url.push_str(&format!("?{query}"));
        }
        url
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// A directory of fake host CLIs that becomes the whole `PATH` of a host. `git` is linked in
/// from the real one, so git still works and nothing else (no real `gh`) can be found.
pub struct FakeCliDir {
    pub dir: tempfile::TempDir,
}

impl FakeCliDir {
    pub fn new() -> FakeCliDir {
        let dir = tempfile::tempdir().unwrap();
        for tool in ["git", "cat"] {
            let found = String::from_utf8(Command::new("which").arg(tool).output().unwrap().stdout).unwrap();
            std::os::unix::fs::symlink(found.trim(), dir.path().join(tool)).unwrap();
        }
        FakeCliDir { dir }
    }

    pub fn path(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
    }

    /// Writes `name` as a shell script that appends its arguments to `<name>.log` (each
    /// followed by a unit separator, one line per call), then runs `body`.
    pub fn install(&self, name: &str, body: &str) {
        let log = self.dir.path().join(format!("{name}.log"));
        let script = format!(
            "#!/bin/sh\nprintf '%s\\037' \"$@\" >> '{log}'\nprintf '\\n' >> '{log}'\n{body}\n",
            log = log.display()
        );
        let path = self.dir.path().join(name);
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Installs `name` answering its Nth call with the Nth step (the last step repeats).
    pub fn install_scripted(&self, name: &str, steps: &[CliStep]) {
        for (index, step) in steps.iter().enumerate() {
            std::fs::write(self.dir.path().join(format!("{name}.{index}.out")), &step.stdout).unwrap();
            std::fs::write(self.dir.path().join(format!("{name}.{index}.err")), &step.stderr).unwrap();
        }
        let mut body = String::from("d=\"${0%/*}\"; n=0\nif [ -f \"$d/%NAME%.count\" ]; then read -r n < \"$d/%NAME%.count\"; fi\necho $((n+1)) > \"$d/%NAME%.count\"\ncase $n in\n");
        for (index, step) in steps.iter().enumerate() {
            let pattern = if index + 1 == steps.len() { "*".to_string() } else { index.to_string() };
            body.push_str(&format!(
                "{pattern}) cat \"$d/%NAME%.{index}.out\"; cat \"$d/%NAME%.{index}.err\" >&2; exit {};;\n",
                step.code
            ));
        }
        body.push_str("esac\n");
        self.install(name, &body.replace("%NAME%", name));
    }

    /// Every invocation of a fake CLI, as its argument list.
    pub fn calls(&self, name: &str) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.dir.path().join(format!("{name}.log")))
            .map(|log| {
                log.lines()
                    .map(|line| match line.strip_suffix('\u{1f}') {
                        Some(arguments) => arguments.split('\u{1f}').map(str::to_string).collect(),
                        None => Vec::new(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// What a scripted fake CLI prints and exits with.
#[derive(Debug, Clone, Default)]
pub struct CliStep {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl CliStep {
    pub fn from_fixture(step: &Value) -> CliStep {
        CliStep {
            stdout: step["stdout"].as_str().unwrap_or("").to_string(),
            stderr: step["stderr"].as_str().unwrap_or("").to_string(),
            code: step["code"].as_i64().unwrap_or(0) as i32,
        }
    }
}

pub struct TestHost {
    pub host: Host,
    pub clock: Arc<FixedClock>,
    pub cwd: PathBuf,
}

/// A host with an explicit environment, the given `PATH`, and (optionally) GitHub redirected.
pub fn test_host(env: &[(&str, &str)], path: Option<String>, github: Option<GitHubApiResolver>, cwd: &Path) -> TestHost {
    let clock = Arc::new(FixedClock::default());
    // A host nobody redirected must still never leave loopback: discovery probes Bitbucket without
    // any credential and GitHub with an environment token. Port 1 refuses at once.
    let mut pairs: Vec<(&str, &str)> = env.to_vec();
    if !pairs.iter().any(|(name, _)| *name == "T3CODE_BITBUCKET_API_BASE_URL") {
        pairs.push(("T3CODE_BITBUCKET_API_BASE_URL", UNREACHABLE_ORIGIN));
    }
    let github = github.or_else(|| {
        Some(Arc::new(|host: &str| (format!("{UNREACHABLE_ORIGIN}/rest/{host}"), format!("{UNREACHABLE_ORIGIN}/graphql/{host}"))) as GitHubApiResolver)
    });
    let host = Host::new(HostOptions {
        env: HostEnv::from_pairs(pairs),
        cwd: cwd.to_path_buf(),
        path,
        clock: clock.clone(),
        github_api: github,
        process: VcsProcess::new(),
    });
    TestHost { host, clock, cwd: cwd.to_path_buf() }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository with one commit and the given `(name, url)` remotes. Nothing is fetched.
pub fn repo_with_remotes(remotes: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap();
    git(&path, &["init", "-q"]);
    std::fs::write(path.join("README.md"), "hello\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-q", "-m", "init"]);
    for (name, url) in remotes {
        git(&path, &["remote", "add", name, url]);
    }
    (directory, path)
}


// ---- scenarios of CLI-backed hosts ------------------------------------------------------------

use t3_server::{source_control_detect::RefSelector, source_control_provider::*};
use serde_json::json;

pub fn refs(value: &Value) -> Option<RefSelector> {
    value.as_object().map(|object| RefSelector {
        ref_name: object["refName"].as_str().unwrap().into(),
        owner: object.get("owner").and_then(Value::as_str).map(str::to_string),
        repository: object.get("repository").and_then(Value::as_str).map(str::to_string),
    })
}

/// The wire envelope down to the error that caused it; deeper causes are the operating system's
/// or a schema's own text.
pub fn summary(error: &Value, cwd: &str) -> Value {
    let swap = |value: &Value| value.as_str().map(|text| text.replace(cwd, "/repo"));
    let nested = &error["cause"]["cause"];
    let mut value = json!({
        "tag":error["_tag"],"provider":error["provider"],"operation":error["operation"],"cwd":swap(&error["cwd"]),
        "command":error["command"],"reference":error["reference"],"repository":error["repository"],"detail":swap(&error["detail"]),
        "cause":{"name":error["cause"]["name"],"message":swap(&error["cause"]["message"])},
    });
    if matches!(nested["name"].as_str(), Some(name) if name.starts_with("Vcs")) {
        value["cause"]["cause"] = json!({"name":nested["name"],"message":swap(&nested["message"])});
    }
    value
}

/// One call of a provider operation against `provider`.
pub async fn run_call(
    provider: &dyn SourceControlProvider,
    call: &Value,
    cwd: &str,
    body_file: &str,
) -> Result<Value, t3_contracts::SourceControlProviderError> {
    let input = &call["input"];
    let cwd = cwd.to_string();
    let wire_context = if call["context"].is_object() { &call["context"] } else { &input["context"] };
    let context = wire_context.as_object().map(|_| ProviderContext {
        provider: serde_json::from_value(wire_context["provider"].clone()).unwrap(),
        remote_name: wire_context["remoteName"].as_str().unwrap().into(),
        remote_url: wire_context["remoteUrl"].as_str().unwrap().into(),
        requested_host: None,
    });
    match call["op"].as_str().unwrap() {
        "list" => provider
            .list_change_requests(ListChangeRequestsInput {
                cwd,
                context: context.clone(),
                source: refs(&input["source"]),
                head_selector: input["headSelector"].as_str().unwrap().into(),
                state: match input["state"].as_str().unwrap() {
                    "open" => ListState::Open,
                    "closed" => ListState::Closed,
                    "merged" => ListState::Merged,
                    _ => ListState::All,
                },
                limit: input["limit"].as_u64().map(|limit| limit as u32),
            })
            .await
            .map(|items| json!(items)),
        "get" => provider
            .get_change_request(GetChangeRequestInput { cwd, context: context.clone(), reference: input["reference"].as_str().unwrap().into() })
            .await
            .map(|item| json!(item)),
        "cloneUrls" => provider
            .get_repository_clone_urls(RepositoryUrlsInput { cwd, context: context.clone(), repository: input["repository"].as_str().unwrap().into() })
            .await
            .map(|urls| json!(urls)),
        "createRepo" => provider
            .create_repository(CreateRepositoryInput {
                cwd,
                repository: input["repository"].as_str().unwrap().into(),
                visibility: serde_json::from_value(input["visibility"].clone()).unwrap(),
            })
            .await
            .map(|urls| json!(urls)),
        "createMr" | "createPr" => provider
            .create_change_request(CreateChangeRequestInput {
                cwd,
                context: context.clone(),
                source: refs(&input["source"]),
                target: refs(&input["target"]),
                base_ref_name: input["baseRefName"].as_str().unwrap().into(),
                head_selector: input["headSelector"].as_str().unwrap().into(),
                title: input["title"].as_str().unwrap().into(),
                body_file: body_file.into(),
            })
            .await
            .map(|()| Value::Null),
        "checkout" => provider
            .checkout_change_request(CheckoutChangeRequestInput {
                cwd,
                context: context.clone(),
                reference: input["reference"].as_str().unwrap().into(),
                force: input["force"].as_bool().unwrap_or(false),
            })
            .await
            .map(|()| Value::Null),
        _ => provider.get_default_branch(DefaultBranchInput { cwd, context: context.clone() }).await.map(|branch| json!(branch)),
    }
}

pub fn check(name: &str, index: usize, actual: &Result<Value, t3_contracts::SourceControlProviderError>, expected: &Value, cwd: &str) {
    let context = format!("{name} #{index}");
    match (actual, expected.get("ok"), expected.get("error")) {
        (Ok(actual), Some(ok), None) => assert_eq!(actual, ok, "{context}"),
        (Err(actual), None, Some(error)) => assert_eq!(summary(&actual.wire(), cwd), summary(error, "/never"), "{context}"),
        other => panic!("{context}: expected {expected}, got {other:?}"),
    }
}


/// Runs the `provider` rows of a CLI-backed host's fixture against `make(host)` with a fake
/// `executable` on the host's whole `PATH`, comparing results, error envelopes and the
/// arguments the CLI was started with.
pub async fn run_cli_scenarios(scenarios: Vec<Value>, executable: &str, make: impl Fn(Host) -> Box<dyn SourceControlProvider>) {
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let steps = scenario["script"].as_array().unwrap();
        let (_repo, repo_path) = repo_with_remotes(&[]);
        let cwd = repo_path.to_string_lossy().into_owned();
        let body_dir = tempfile::tempdir().unwrap();
        let body_file = body_dir.path().join("body.md").to_string_lossy().into_owned();
        let calls = scenario["calls"].as_array().unwrap();
        let expected = scenario["results"].as_array().unwrap();
        let args_of = |calls: Vec<Vec<String>>| -> Vec<Vec<String>> {
            calls.into_iter().map(|args| args.into_iter().map(|a| a.replace(&body_file, "<BODY_FILE>")).collect()).collect()
        };
        let recorded: Vec<Vec<String>> = scenario["processes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|process| process["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect())
            .collect();
        let one_process_per_call = steps.iter().any(|step| step["spawn"] == true || step["timeout"] == true);
        if one_process_per_call {
            // Each call is one process, and the script says how that one ends: a CLI that is
            // not there, or one that answers.
            for (index, call) in calls.iter().enumerate() {
                let step = &steps[index.min(steps.len() - 1)];
                if step["timeout"] == true {
                    // A hung CLI is covered by the process layer's own timeout tests.
                    continue;
                }
                let fake = FakeCliDir::new();
                if step["spawn"] != true {
                    fake.install_scripted(executable, &[CliStep::from_fixture(step)]);
                }
                let test = test_host(&[], Some(fake.path()), None, &repo_path);
                let outcome = run_call(&*make(test.host.clone()), call, &cwd, &body_file).await;
                check(name, index, &outcome, &expected[index], &cwd);
                if step["spawn"] != true {
                    assert_eq!(args_of(fake.calls(executable)), vec![recorded[index].clone()], "{name} #{index}: {executable} arguments");
                }
            }
        } else {
            let fake = FakeCliDir::new();
            let cli_steps: Vec<CliStep> = steps.iter().map(CliStep::from_fixture).collect();
            fake.install_scripted(executable, &cli_steps);
            let test = test_host(&[], Some(fake.path()), None, &repo_path);
            let provider = make(test.host.clone());
            for (index, call) in calls.iter().enumerate() {
                let outcome = run_call(&*provider, call, &cwd, &body_file).await;
                check(name, index, &outcome, &expected[index], &cwd);
            }
            assert_eq!(args_of(fake.calls(executable)), recorded, "{name}: {executable} arguments");
        }
    }
}
