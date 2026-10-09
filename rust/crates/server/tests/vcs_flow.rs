//! The `vcs.*` methods and `subscribeVcsStatus` over real sockets against git repositories in
//! temporary directories. Remotes are local bare repositories; nothing touches a network.
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    transport::{ApiState, router},
    vcs_rpc::{status_poller_subscribers, wait_for_no_status_poller},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    /// Canonical, so paths compare with what the server reports.
    base: PathBuf,
    _directory: tempfile::TempDir,
    address: std::net::SocketAddr,
    writer: String,
    reader: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

async fn harness() -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(
        store.clone(),
        [7; 32],
        "t3_session_vcs".into(),
        "loopback-browser".into(),
    )
    .unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session(
            "fixture",
            "bearer-access-token",
            scopes,
            json!({"deviceType":"unknown"}),
            Utc::now(),
            chrono::Duration::hours(1),
        )
        .unwrap()
        .1
    };
    let writer = issue(vec![
        AuthEnvironmentScope::OrchestrationRead,
        AuthEnvironmentScope::SourceControlWrite,
    ]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"vcs-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: None,
        execution: None,
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    Harness {
        base,
        _directory: directory,
        address,
        writer,
        reader,
        stop: Some(stop),
        server: Some(server),
    }
}

impl Harness {
    async fn connect(&self, token: &str) -> Socket {
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
    }
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn commit_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-m", &format!("update {name}")]);
}

/// A repository on `main` with one commit.
fn repo(harness: &Harness, name: &str) -> PathBuf {
    let dir = harness.dir(name);
    git(&dir, &["init"]);
    commit_file(&dir, "file.txt", "one\n");
    dir
}

/// A repository whose `origin` is a local bare repository, pushed and tracking.
fn repo_with_origin(harness: &Harness, name: &str) -> (PathBuf, PathBuf) {
    let bare = harness.dir(&format!("{name}-origin.git"));
    git(&bare, &["init", "--bare"]);
    let dir = repo(harness, name);
    git(&dir, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&dir, &["push", "-u", "origin", "main"]);
    (dir, bare)
}

async fn send(socket: &mut Socket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}
async fn next(socket: &mut Socket) -> Value {
    let message = socket.next().await.unwrap().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

/// Boxed: the debug-build futures of a long test would otherwise overflow its thread stack.
fn rpc<'a>(
    socket: &'a mut Socket,
    id: u64,
    tag: &'a str,
    payload: Value,
) -> futures_util::future::BoxFuture<'a, Value> {
    Box::pin(async move {
        send(
            socket,
            json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]}),
        )
        .await;
        loop {
            let value = next(socket).await;
            if value["_tag"] == "Exit" && value["requestId"] == id {
                return value["exit"].clone();
            }
        }
    })
}
fn success(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Success", "{exit}");
    &exit["value"]
}
/// The wire error of a typed failure.
fn failure(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Fail", "{exit}");
    &exit["cause"][0]["error"]
}
fn path(dir: &Path) -> &str {
    dir.to_str().unwrap()
}

#[tokio::test]
async fn refresh_status_reports_the_branch_and_working_tree() {
    let h = harness().await;
    let mut socket = h.connect(&h.writer).await;
    let dir = repo(&h, "repo");

    let clean = success(
        &rpc(
            &mut socket,
            1,
            "vcs.refreshStatus",
            json!({"cwd":path(&dir)}),
        )
        .await,
    )
    .clone();
    assert_eq!(clean["isRepo"], true);
    assert_eq!(clean["refName"], "main");
    assert_eq!(clean["hasWorkingTreeChanges"], false);
    assert_eq!(clean["hasUpstream"], false);
    assert_eq!(clean["pr"], Value::Null);
    assert!(clean.get("sourceControlProvider").is_none());

    std::fs::write(dir.join("file.txt"), "one\ntwo\n").unwrap();
    let dirty = success(
        &rpc(
            &mut socket,
            2,
            "vcs.refreshStatus",
            json!({"cwd":format!("  {}  ", path(&dir))}),
        )
        .await,
    )
    .clone();
    assert_eq!(dirty["hasWorkingTreeChanges"], true);
    assert_eq!(dirty["workingTree"]["files"][0]["path"], "file.txt");
    assert_eq!(dirty["workingTree"]["insertions"], 1);

    // A plain directory and a missing one are not repositories, not errors.
    for (id, target) in [(3, h.dir("plain")), (4, h.base.join("missing"))] {
        let status = success(
            &rpc(
                &mut socket,
                id,
                "vcs.refreshStatus",
                json!({"cwd":path(&target)}),
            )
            .await,
        )
        .clone();
        assert_eq!(
            (
                status["isRepo"].clone(),
                status["refName"].clone(),
                status["hasUpstream"].clone()
            ),
            (json!(false), Value::Null, json!(false))
        );
    }
    h.finish().await;
}

#[tokio::test]
async fn list_refs_pages_filters_and_tolerates_non_repositories() {
    let h = harness().await;
    let mut socket = h.connect(&h.writer).await;
    let dir = repo(&h, "repo");
    for branch in ["feature/a", "feature/b", "other"] {
        git(&dir, &["branch", branch]);
    }

    let all =
        success(&rpc(&mut socket, 1, "vcs.listRefs", json!({"cwd":path(&dir)})).await).clone();
    assert_eq!(
        (all["isRepo"].clone(), all["totalCount"].clone()),
        (json!(true), json!(4))
    );
    assert_eq!(all["refs"][0]["name"], "main");
    assert_eq!(all["refs"][0]["current"], true);

    let page = success(
        &rpc(
            &mut socket,
            2,
            "vcs.listRefs",
            json!({"cwd":path(&dir),"limit":2}),
        )
        .await,
    )
    .clone();
    assert_eq!(page["refs"].as_array().unwrap().len(), 2);
    assert_eq!(page["nextCursor"], 2);
    let rest = success(
        &rpc(
            &mut socket,
            3,
            "vcs.listRefs",
            json!({"cwd":path(&dir),"limit":2,"cursor":2}),
        )
        .await,
    )
    .clone();
    assert_eq!(rest["nextCursor"], Value::Null);

    let filtered = success(
        &rpc(
            &mut socket,
            4,
            "vcs.listRefs",
            json!({"cwd":path(&dir),"query":"feature"}),
        )
        .await,
    )
    .clone();
    assert_eq!(filtered["totalCount"], 2);

    let plain = success(
        &rpc(
            &mut socket,
            5,
            "vcs.listRefs",
            json!({"cwd":path(&h.dir("plain"))}),
        )
        .await,
    )
    .clone();
    assert_eq!(
        plain,
        json!({"refs":[],"isRepo":false,"hasPrimaryRemote":false,"nextCursor":null,"totalCount":0})
    );

    // The schema limits belong to this layer.
    for (id, payload) in [
        (6, json!({"cwd":path(&dir),"limit":201})),
        (7, json!({"cwd":path(&dir),"query":"x".repeat(257)})),
        (8, json!({"cwd":"  "})),
    ] {
        let exit = rpc(&mut socket, id, "vcs.listRefs", payload).await;
        assert_eq!(exit["cause"][0]["_tag"], "Die", "{exit}");
    }
    h.finish().await;
}

#[tokio::test]
async fn refs_can_be_created_and_switched_and_a_dirty_tree_blocks_the_switch() {
    let h = harness().await;
    let mut socket = h.connect(&h.writer).await;
    let dir = repo(&h, "repo");

    let created = success(
        &rpc(
            &mut socket,
            1,
            "vcs.createRef",
            json!({"cwd":path(&dir),"refName":"feature/x","switchRef":true}),
        )
        .await,
    )
    .clone();
    assert_eq!(created, json!({"refName":"feature/x"}));
    assert_eq!(git(&dir, &["branch", "--show-current"]).trim(), "feature/x");
    commit_file(&dir, "file.txt", "feature\n");

    let back = success(
        &rpc(
            &mut socket,
            2,
            "vcs.switchRef",
            json!({"cwd":path(&dir),"refName":"main"}),
        )
        .await,
    )
    .clone();
    assert_eq!(back, json!({"refName":"main"}));
    let no_switch = success(
        &rpc(
            &mut socket,
            3,
            "vcs.createRef",
            json!({"cwd":path(&dir),"refName":"parked"}),
        )
        .await,
    )
    .clone();
    assert_eq!(no_switch, json!({"refName":"parked"}));
    assert_eq!(git(&dir, &["branch", "--show-current"]).trim(), "main");

    // Uncommitted edits that the other branch would overwrite stop the checkout.
    std::fs::write(dir.join("file.txt"), "local edit\n").unwrap();
    let blocked = rpc(
        &mut socket,
        4,
        "vcs.switchRef",
        json!({"cwd":path(&dir),"refName":"feature/x"}),
    )
    .await;
    let error = failure(&blocked);
    assert_eq!(error["_tag"], "GitCommandError");
    assert_eq!(error["operation"], "GitVcsDriver.switchRef.checkout");
    assert_eq!(git(&dir, &["branch", "--show-current"]).trim(), "main");

    let missing = rpc(
        &mut socket,
        5,
        "vcs.switchRef",
        json!({"cwd":path(&dir),"refName":"no-such-branch"}),
    )
    .await;
    assert_eq!(failure(&missing)["_tag"], "GitCommandError");
    let duplicate = rpc(
        &mut socket,
        6,
        "vcs.createRef",
        json!({"cwd":path(&dir),"refName":"parked"}),
    )
    .await;
    assert_eq!(failure(&duplicate)["reason"], "branch_already_exists");
    h.finish().await;
}

#[tokio::test]
async fn worktrees_are_created_under_the_home_or_a_given_path_and_removed() {
    let h = harness().await;
    let mut socket = h.connect(&h.writer).await;
    let dir = repo(&h, "repo");
    git(&dir, &["branch", "side"]);

    // No path and no setting: <home>/worktrees/<repo>/<branch with slashes flattened>.
    let created = success(
        &rpc(
            &mut socket,
            1,
            "vcs.createWorktree",
            json!({"cwd":path(&dir),"refName":"main","newRefName":"feat/wt","path":null}),
        )
        .await,
    )
    .clone();
    let expected = h.base.join("worktrees").join("repo").join("feat-wt");
    assert_eq!(
        created,
        json!({"worktree":{"path":path(&expected),"refName":"feat/wt"}})
    );
    assert!(expected.join("file.txt").exists());
    assert_eq!(
        git(&expected, &["branch", "--show-current"]).trim(),
        "feat/wt"
    );

    let explicit = h.base.join("elsewhere");
    let second = success(
        &rpc(
            &mut socket,
            2,
            "vcs.createWorktree",
            json!({"cwd":path(&dir),"refName":"side","path":path(&explicit)}),
        )
        .await,
    )
    .clone();
    assert_eq!(second["worktree"]["path"], path(&explicit));

    // The same branch cannot be checked out twice.
    let again = rpc(
        &mut socket,
        3,
        "vcs.createWorktree",
        json!({"cwd":path(&dir),"refName":"side","path":path(&h.base.join("third"))}),
    )
    .await;
    assert_eq!(failure(&again)["reason"], "branch_checked_out_in_worktree");
    let missing = rpc(
        &mut socket,
        4,
        "vcs.createWorktree",
        json!({"cwd":path(&dir),"refName":"nope","path":null}),
    )
    .await;
    assert_eq!(failure(&missing)["_tag"], "GitCommandError");

    for (id, target) in [(5, &expected), (6, &explicit)] {
        assert_eq!(
            success(
                &rpc(
                    &mut socket,
                    id,
                    "vcs.removeWorktree",
                    json!({"cwd":path(&dir),"path":path(target)})
                )
                .await
            ),
            &Value::Null
        );
        assert!(!target.exists());
    }
    h.finish().await;
}

#[tokio::test]
async fn pull_fast_forwards_from_the_upstream() {
    let h = harness().await;
    let mut socket = h.connect(&h.writer).await;
    let (dir, bare) = repo_with_origin(&h, "repo");
    let other = h.base.join("other");
    git(&h.base, &["clone", path(&bare), path(&other)]);
    commit_file(&other, "pushed.txt", "from elsewhere\n");
    git(&other, &["push"]);

    let pulled = success(&rpc(&mut socket, 1, "vcs.pull", json!({"cwd":path(&dir)})).await).clone();
    assert_eq!(
        pulled,
        json!({"status":"pulled","refName":"main","upstreamRef":"origin/main"})
    );
    assert!(dir.join("pushed.txt").exists());
    let current =
        success(&rpc(&mut socket, 2, "vcs.pull", json!({"cwd":path(&dir)})).await).clone();
    assert_eq!(current["status"], "skipped_up_to_date");

    // A branch with no upstream has nothing to pull.
    git(&dir, &["checkout", "-b", "lonely"]);
    let none = rpc(&mut socket, 3, "vcs.pull", json!({"cwd":path(&dir)})).await;
    assert_eq!(failure(&none)["_tag"], "GitCommandError");
    h.finish().await;
}

#[tokio::test]
async fn init_creates_a_repository_and_refuses_other_kinds() {
    let h = harness().await;
    let mut socket = h.connect(&h.writer).await;
    let dir = h.dir("fresh");
    assert_eq!(
        success(&rpc(&mut socket, 1, "vcs.init", json!({"cwd":path(&dir)})).await),
        &Value::Null
    );
    assert!(dir.join(".git").exists());
    let again = h.dir("again");
    assert_eq!(
        success(
            &rpc(
                &mut socket,
                2,
                "vcs.init",
                json!({"cwd":path(&again),"kind":"git"})
            )
            .await
        ),
        &Value::Null
    );

    let jj = rpc(
        &mut socket,
        3,
        "vcs.init",
        json!({"cwd":path(&h.dir("jj")),"kind":"jj"}),
    )
    .await;
    assert_eq!(failure(&jj)["_tag"], "VcsUnsupportedOperationError");
    assert_eq!(failure(&jj)["detail"], "No jj VCS driver is registered.");
    let unknown = rpc(
        &mut socket,
        4,
        "vcs.init",
        json!({"cwd":path(&h.dir("unknown")),"kind":"unknown"}),
    )
    .await;
    assert_eq!(
        failure(&unknown)["operation"],
        "VcsProvisioningService.resolveRequestedKind"
    );
    h.finish().await;
}

#[tokio::test]
async fn commands_outside_a_repository_fail_with_a_routed_command_error() {
    let h = harness().await;
    let mut socket = h.connect(&h.writer).await;
    let plain = h.dir("plain");
    let cwd = path(&plain);
    let calls = [
        (
            "vcs.switchRef",
            json!({"cwd":cwd,"refName":"main"}),
            "GitWorkflowService.switchRef",
        ),
        (
            "vcs.createRef",
            json!({"cwd":cwd,"refName":"x"}),
            "GitWorkflowService.createRef",
        ),
        (
            "vcs.createWorktree",
            json!({"cwd":cwd,"refName":"main","path":null}),
            "GitWorkflowService.createWorktree",
        ),
        (
            "vcs.removeWorktree",
            json!({"cwd":cwd,"path":"/nowhere"}),
            "GitWorkflowService.removeWorktree",
        ),
        (
            "vcs.pull",
            json!({"cwd":cwd}),
            "GitWorkflowService.pullCurrentBranch",
        ),
    ];
    for (index, (tag, payload, operation)) in calls.into_iter().enumerate() {
        let exit = rpc(&mut socket, index as u64 + 1, tag, payload).await;
        let error = failure(&exit);
        assert_eq!(error["_tag"], "GitCommandError", "{tag}");
        assert_eq!(error["command"], "vcs-route", "{tag}");
        assert_eq!(error["operation"], operation, "{tag}");
        assert_eq!(
            error["cause"]["name"], "VcsUnsupportedOperationError",
            "{tag}"
        );
        assert_eq!(
            error["cause"]["message"].as_str().unwrap(),
            format!(
                "VCS operation is unsupported for unknown in VcsDriverRegistry.resolve: No supported VCS repository was detected at {cwd}."
            ),
            "{tag}"
        );
    }
    h.finish().await;
}

#[tokio::test]
async fn a_read_only_connection_may_read_status_and_refs_but_not_change_them() {
    let h = harness().await;
    let mut reader = h.connect(&h.reader).await;
    let dir = repo(&h, "repo");
    let cwd = path(&dir);

    assert_eq!(
        success(&rpc(&mut reader, 1, "vcs.refreshStatus", json!({"cwd":cwd})).await)["refName"],
        "main"
    );
    assert_eq!(
        success(&rpc(&mut reader, 2, "vcs.listRefs", json!({"cwd":cwd})).await)["totalCount"],
        1
    );
    let writes = [
        ("vcs.pull", json!({"cwd":cwd})),
        (
            "vcs.createWorktree",
            json!({"cwd":cwd,"refName":"main","path":null}),
        ),
        ("vcs.removeWorktree", json!({"cwd":cwd,"path":"/x"})),
        ("vcs.createRef", json!({"cwd":cwd,"refName":"x"})),
        ("vcs.switchRef", json!({"cwd":cwd,"refName":"main"})),
        ("vcs.init", json!({"cwd":cwd})),
    ];
    for (index, (tag, payload)) in writes.into_iter().enumerate() {
        let exit = rpc(&mut reader, index as u64 + 10, tag, payload).await;
        let error = failure(&exit);
        assert_eq!(error["_tag"], "EnvironmentAuthorizationError", "{tag}");
        assert!(error["requiredScope"].is_string(), "{error}");
        assert!(error.get("operation").is_none(), "{error}");
    }
    // Nothing was created by the refused calls.
    assert_eq!(git(&dir, &["branch", "--list"]).lines().count(), 1);
    h.finish().await;
}

/// The next status event, acknowledged so the server can send another.
async fn next_event(socket: &mut Socket, id: u64) -> Value {
    loop {
        let message = next(socket).await;
        if message["_tag"] == "Chunk" && message["requestId"] == id {
            send(socket, json!({"_tag":"Ack","requestId":id})).await;
            return message["values"][0].clone();
        }
        assert_ne!(message["_tag"], "Exit", "the subscription ended: {message}");
    }
}

/// Events until one reports `ref_name`, whichever shape carries it.
async fn until_ref(socket: &mut Socket, id: u64, ref_name: &str) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let event = next_event(socket, id).await;
        let reached = event["local"]["refName"] == ref_name;
        seen.push(event);
        if reached {
            return seen;
        }
    }
}

#[tokio::test]
async fn the_status_subscription_delivers_changes_once_and_stops_after_disconnect() {
    let h = harness().await;
    let (dir, _bare) = repo_with_origin(&h, "repo");
    let mut watcher = h.connect(&h.reader).await;
    let mut actor = h.connect(&h.writer).await;

    send(&mut watcher, json!({"_tag":"Request","id":1,"tag":"subscribeVcsStatus","payload":{"cwd":path(&dir)},"headers":[]})).await;
    let snapshot = next_event(&mut watcher, 1).await;
    assert_eq!(snapshot["_tag"], "snapshot");
    assert_eq!(snapshot["local"]["refName"], "main");
    // The snapshot names no remote yet; the subscription now holds the directory's poller.
    assert_eq!(snapshot["remote"], Value::Null);
    assert_eq!(status_poller_subscribers(path(&dir)), Some(1));

    // A branch change made through the API reaches the subscriber.
    success(
        &rpc(
            &mut actor,
            1,
            "vcs.createRef",
            json!({"cwd":path(&dir),"refName":"feature/live","switchRef":true}),
        )
        .await,
    );
    let seen = until_ref(&mut watcher, 1, "feature/live").await;
    assert!(seen.iter().all(|event| {
        ["snapshot", "localUpdated", "remoteUpdated"].contains(&event["_tag"].as_str().unwrap())
    }));

    // Refreshing with nothing changed publishes nothing: the next event is the next change.
    success(
        &rpc(
            &mut actor,
            2,
            "vcs.refreshStatus",
            json!({"cwd":path(&dir)}),
        )
        .await,
    );
    success(
        &rpc(
            &mut actor,
            3,
            "vcs.createRef",
            json!({"cwd":path(&dir),"refName":"feature/after","switchRef":true}),
        )
        .await,
    );
    let seen = until_ref(&mut watcher, 1, "feature/after").await;
    assert!(
        seen.iter()
            .all(|event| event["local"].is_null() || event["local"]["refName"] != "feature/live"),
        "a duplicate of the unchanged status was published: {seen:?}"
    );

    // Working-tree edits are noticed by a refresh as well.
    std::fs::write(dir.join("file.txt"), "changed\n").unwrap();
    success(
        &rpc(
            &mut actor,
            4,
            "vcs.refreshStatus",
            json!({"cwd":path(&dir)}),
        )
        .await,
    );
    loop {
        let event = next_event(&mut watcher, 1).await;
        if event["local"]["hasWorkingTreeChanges"] == true {
            break;
        }
    }

    // Closing the connection releases the subscription's hold on the poller.
    watcher.close(None).await.unwrap();
    actor.close(None).await.unwrap();
    wait_for_no_status_poller(path(&dir)).await;
    assert_eq!(status_poller_subscribers(path(&dir)), None);
    h.finish().await;
}

#[tokio::test]
async fn a_passive_subscription_does_not_start_a_poller_and_an_interrupt_ends_it() {
    let h = harness().await;
    let dir = repo(&h, "repo");
    let mut socket = h.connect(&h.reader).await;
    send(&mut socket, json!({"_tag":"Request","id":1,"tag":"subscribeVcsStatus","payload":{"cwd":path(&dir),"includeRemote":false},"headers":[]})).await;
    let snapshot = next_event(&mut socket, 1).await;
    assert_eq!(snapshot["local"]["refName"], "main");
    assert_eq!(status_poller_subscribers(path(&dir)), None);

    // An invalid payload fails the request instead of opening a stream.
    let exit = rpc(&mut socket, 2, "subscribeVcsStatus", json!({"cwd":" "})).await;
    assert_eq!(exit["cause"][0]["_tag"], "Die");

    send(&mut socket, json!({"_tag":"Interrupt","requestId":1})).await;
    loop {
        let message = next(&mut socket).await;
        if message["_tag"] == "Exit" && message["requestId"] == 1 {
            assert_eq!(message["exit"]["cause"][0]["_tag"], "Interrupt");
            break;
        }
    }
    h.finish().await;
}
