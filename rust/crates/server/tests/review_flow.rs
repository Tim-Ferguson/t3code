//! The review diffs over a real socket: the "Uncommitted" and "Changes" sources of a checkout and
//! the unchanged lines of one file, and the boundaries around them. A directory the server does
//! not serve is refused before git is asked, a file whose path leads out of the checkout is
//! refused before it is read, and a connection without the filesystem scope gets nothing.
#![recursion_limit = "256"]
mod source_control_support;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use source_control_support::git;
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    transport::{ApiState, router},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::{Message, client::IntoClientRequest}};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

async fn rpc(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    let frame = json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]});
    socket.send(Message::Text(frame.to_string().into())).await.unwrap();
    loop {
        let message: Value = serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        if message["_tag"] == "Exit" && message["requestId"] == id {
            return message["exit"].clone();
        }
    }
}

fn success(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Success", "{exit}");
    &exit["value"]
}

fn error_of(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Fail", "{exit}");
    &exit["cause"][0]["error"]
}

#[test]
fn review_diffs_over_the_socket() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(Box::pin(scenario()));
}

async fn scenario() {
    let root = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(root.path()).unwrap();
    // The workspace the server serves, a repository inside it, and a place it does not serve.
    let workspace = base.join("workspace");
    let repo = workspace.join("repo");
    let outside = base.join("elsewhere");
    for directory in [&repo, &outside] {
        std::fs::create_dir_all(directory).unwrap();
    }
    git(&repo, &["init", "--initial-branch=main"]);
    std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(repo.join("gone.txt"), "x\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "Initial commit"]);
    git(&repo, &["checkout", "-b", "feature/x"]);
    std::fs::write(repo.join("b.txt"), "new\n").unwrap();
    git(&repo, &["add", "b.txt"]);
    git(&repo, &["commit", "-m", "Add b"]);
    std::fs::write(repo.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::write(outside.join("secret.txt"), "do not leak\n").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), repo.join("link.txt")).unwrap();
    git(&outside, &["init", "--initial-branch=main"]);

    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_rev".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let without = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let with = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::FilesystemRead]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"rev-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: Some(json!({"cwd":workspace.to_string_lossy()})),
        settings: None, cors_origins: None, assets: None, providers: None, execution: None, workspace: None, terminals: None,
        discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move { axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap() });
    let mut denied = connect(address, &without).await;
    let mut socket = connect(address, &with).await;
    let cwd = repo.to_string_lossy().into_owned();

    // The filesystem scope is what opens these.
    let exit = rpc(&mut denied, 1, "review.getDiffPreview", json!({"cwd":cwd})).await;
    assert_eq!(error_of(&exit)["_tag"], "EnvironmentAuthorizationError");
    assert_eq!(error_of(&exit)["requiredPermission"], "filesystem:read");
    let exit = rpc(&mut denied, 2, "review.getDiffFileContents", json!({"cwd":cwd,"sourceKind":"working-tree","changeType":"change","baseRef":null,"headRef":null,"oldPath":"a.txt","newPath":"a.txt"})).await;
    assert_eq!(error_of(&exit)["requiredPermission"], "filesystem:read");

    // Both diff scopes of a checkout: what is uncommitted, and what the branch changes.
    let preview = success(&rpc(&mut socket, 3, "review.getDiffPreview", json!({"cwd":cwd,"baseRef":"main"})).await).clone();
    assert_eq!(preview["cwd"], cwd.as_str());
    let sources = preview["sources"].as_array().unwrap();
    let kinds: Vec<&str> = sources.iter().map(|source| source["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["working-tree", "branch-range"], "{preview}");
    let uncommitted = &sources[0];
    assert!(uncommitted["diff"].as_str().unwrap().contains("+TWO"), "{uncommitted}");
    assert!(uncommitted["files"].as_array().unwrap().iter().any(|file| file["path"] == "a.txt" && file["additions"] == 1 && file["deletions"] == 1));
    let changes = &sources[1];
    assert!(changes["diff"].as_str().unwrap().contains("b.txt"));
    assert_eq!(changes["baseRef"], "main");
    // One file of one source, for the viewer that asks for a single patch.
    let single = success(&rpc(&mut socket, 4, "review.getDiffPreview", json!({"cwd":cwd,"baseRef":"main","file":{"path":"b.txt","previousPath":null,"sourceKind":"branch-range"}})).await).clone();
    let branch = single["sources"].as_array().unwrap().iter().find(|source| source["kind"] == "branch-range").unwrap();
    assert!(branch["diff"].as_str().unwrap().contains("+new"));
    let other = single["sources"].as_array().unwrap().iter().find(|source| source["kind"] == "working-tree").map(|source| source["diff"].as_str().unwrap().to_string());
    assert_eq!(other.as_deref().unwrap_or(""), "", "only the named source is read: {single}");

    // The unchanged lines of a file, old side from git and new side from disk.
    let contents = success(&rpc(&mut socket, 5, "review.getDiffFileContents", json!({"cwd":cwd,"sourceKind":"working-tree","changeType":"change","baseRef":null,"headRef":null,"oldPath":"a.txt","newPath":"a.txt"})).await).clone();
    assert_eq!(contents, json!({"oldContents":"one\ntwo\nthree\n","newContents":"one\nTWO\nthree\n"}));
    let contents = success(&rpc(&mut socket, 6, "review.getDiffFileContents", json!({"cwd":cwd,"sourceKind":"branch-range","changeType":"new","baseRef":"main","headRef":null,"oldPath":"b.txt","newPath":"b.txt"})).await).clone();
    assert_eq!(contents, json!({"oldContents":"","newContents":"new\n"}));

    // A directory the server does not serve is refused before git is asked anything.
    for (id, tag, payload) in [
        (7, "review.getDiffPreview", json!({"cwd":outside.to_string_lossy()})),
        (8, "review.getDiffFileContents", json!({"cwd":outside.to_string_lossy(),"sourceKind":"working-tree","changeType":"change","baseRef":null,"headRef":null,"oldPath":"secret.txt","newPath":"secret.txt"})),
        // A way around by walking back out of the workspace.
        (9, "review.getDiffPreview", json!({"cwd":format!("{}/../../elsewhere", repo.display())})),
        (10, "review.getDiffPreview", json!({"cwd":"/"})),
    ] {
        let exit = rpc(&mut socket, id, tag, payload).await;
        let error = error_of(&exit);
        assert_eq!(error["_tag"], "VcsRepositoryDetectionError", "{tag}: {exit}");
        assert!(error["detail"].as_str().unwrap().contains("must stay within the configured workspace root"), "{error}");
    }

    // A path inside the checkout that leads out of it is refused before it is read.
    for (id, new_path) in [(11, "../../elsewhere/secret.txt"), (12, "link.txt"), (13, "/etc/hosts")] {
        let exit = rpc(&mut socket, id, "review.getDiffFileContents", json!({"cwd":cwd,"sourceKind":"working-tree","changeType":"new","baseRef":null,"headRef":null,"oldPath":"a.txt","newPath":new_path})).await;
        let error = error_of(&exit);
        assert_eq!(error["_tag"], "GitCommandError", "{new_path}: {exit}");
        assert!(error["detail"].as_str().unwrap().contains("resolves outside the review workspace"), "{new_path}: {error}");
        assert!(!exit.to_string().contains("do not leak"));
    }

    // Inside the workspace but not a repository: nothing to review, and no error.
    std::fs::create_dir_all(workspace.join("plain")).unwrap();
    let plain = workspace.join("plain").to_string_lossy().into_owned();
    let empty = success(&rpc(&mut socket, 14, "review.getDiffPreview", json!({"cwd":plain})).await).clone();
    assert_eq!(empty["sources"], json!([]));
    let exit = rpc(&mut socket, 15, "review.getDiffFileContents", json!({"cwd":plain,"sourceKind":"working-tree","changeType":"change","baseRef":null,"headRef":null,"oldPath":"a.txt","newPath":"a.txt"})).await;
    assert_eq!(error_of(&exit)["_tag"], "VcsUnsupportedOperationError");
    assert_eq!(error_of(&exit)["detail"], "Unchanged diff expansion currently requires a Git repository.");

    // Malformed input is a decode failure.
    let exit = rpc(&mut socket, 16, "review.getDiffPreview", json!({"cwd":"  "})).await;
    assert_eq!(exit["cause"][0]["_tag"], "Die");

    drop(socket);
    drop(denied);
    stop.send(()).unwrap();
    server.await.unwrap();
}
