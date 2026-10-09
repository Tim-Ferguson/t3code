//! `server.discoverSourceControl`, the three `sourceControl.*` methods and the host badge of
//! `vcs.refreshStatus` over a real socket. GitHub's API is a scripted local server and the host
//! CLIs are fake scripts on a private `PATH`; clone and publish move commits between local
//! repositories. This test binary installs one process-wide host before the first request, so
//! everything runs in one test.
mod source_control_support;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use source_control_support::*;
use std::path::{Path, PathBuf};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    source_control_host::Host,
    transport::{ApiState, router},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(address: std::net::SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/ws?orchestrationProtocol=2").into_client_request().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

fn rpc<'a>(socket: &'a mut Socket, id: u64, tag: &'a str, payload: Value) -> futures_util::future::BoxFuture<'a, Value> {
    Box::pin(async move {
        let frame = json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]});
        socket.send(Message::Text(frame.to_string().into())).await.unwrap();
        loop {
            let message = socket.next().await.unwrap().unwrap();
            let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
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

fn failure(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    assert_eq!(exit["cause"][0]["_tag"], "Fail", "{exit}");
    &exit["cause"][0]["error"]
}

fn path(dir: &Path) -> &str {
    dir.to_str().unwrap()
}

fn repository_body(full_name: &str, remote: &str) -> Step {
    Step::json(200, json!({"full_name":full_name,"html_url":format!("https://github.com/{full_name}"),"ssh_url":remote,"default_branch":"main"}))
}

/// Runs on the default test thread stack, with the scenario's own (large) future on the heap.
#[test]
fn discovers_hosts_and_looks_up_clones_and_publishes_repositories() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(Box::pin(scenario()));
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let api = ScriptedServer::start(vec![]).await;
    let cli = FakeCliDir::new();
    cli.install(
        "gh",
        r#"
case "$1 $2" in
  "auth token") printf 'fake-token';;
  "auth status") printf '{"hosts":{"github.com":[{"state":"success","active":true,"host":"github.com","login":"octo","tokenSource":"keyring"}]}}';;
  *) echo "gh version 2.83.0 (fake)";;
esac
"#,
    );
    cli.install("az", r#"if [ "$1" = "account" ]; then echo 'azure-user@example.com'; else echo 'azure-cli 2.60 (fake)'; fi"#);
    cli.install(
        "glab",
        r#"if [ "$1" = "auth" ]; then printf 'code.example.com\n  Logged in to code.example.com as corp-user (token)\n'; else echo 'glab 1.40 (fake)'; fi"#,
    );
    let host = test_host(&[], Some(cli.path()), Some(api.github_resolver()), &base);
    assert!(Host::install(host.host.clone()), "this binary installs the process host once");

    let store = Store::open(base.join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_sc".into(), "loopback-browser".into()).unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1))
            .unwrap()
            .1
    };
    let writer = issue(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::SourceControlWrite]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"sc-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
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
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state)).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
    });
    let mut socket = connect(address, &writer).await;
    let mut readonly = connect(address, &reader).await;

    // ---- discovery ---------------------------------------------------------------------------
    let discovered = success(&rpc(&mut readonly, 1, "server.discoverSourceControl", json!({})).await).clone();
    let vcs = discovered["versionControlSystems"].as_array().unwrap();
    assert_eq!((vcs[0]["kind"].as_str(), vcs[0]["status"].as_str(), vcs[0]["implemented"].as_bool()), (Some("git"), Some("available"), Some(true)));
    assert_eq!(vcs[0]["version"]["_tag"], "Some");
    assert!(vcs[0]["version"]["value"].as_str().unwrap().starts_with("git version"));
    assert_eq!((vcs[1]["kind"].as_str(), vcs[1]["status"].as_str(), vcs[1]["implemented"].as_bool()), (Some("jj"), Some("missing"), Some(false)));
    let providers = discovered["sourceControlProviders"].as_array().unwrap();
    let kinds: Vec<&str> = providers.iter().map(|item| item["kind"].as_str().unwrap()).collect();
    assert_eq!(&kinds[..3], ["github", "gitlab", "azure-devops"]);
    assert_eq!(providers[0]["status"], "available");
    assert_eq!(providers[0]["version"], json!({"_tag":"Some","value":"gh version 2.83.0 (fake)"}));
    assert_eq!(providers[0]["auth"]["status"], "authenticated");
    assert_eq!(providers[0]["auth"]["account"], json!({"_tag":"Some","value":"octo"}));
    assert_eq!(providers[0]["auth"]["accounts"][0]["host"], "github.com");
    assert_eq!(providers[1]["auth"]["account"], json!({"_tag":"Some","value":"corp-user"}));
    assert_eq!(providers[1]["auth"]["host"], json!({"_tag":"Some","value":"code.example.com"}));
    assert_eq!(providers[2]["auth"]["account"], json!({"_tag":"Some","value":"azure-user@example.com"}));
    assert_eq!(providers[2]["executable"], "az");
    let malformed = rpc(&mut readonly, 2, "server.discoverSourceControl", json!(null)).await;
    assert_eq!(malformed["_tag"], "Failure");

    // ---- lookup ------------------------------------------------------------------------------
    let bare = base.join("acme-web.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--bare", "-q"]);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    std::fs::write(seed.join("README.md"), "hello\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    git(&seed, &["push", path(&bare), "HEAD:refs/heads/main"]);

    api.set_script(vec![repository_body("acme/web", path(&bare))]);
    let looked_up = success(&rpc(&mut readonly, 3, "sourceControl.lookupRepository", json!({"provider":"github","repository":" acme/web "})).await).clone();
    assert_eq!(looked_up, json!({"provider":"github","nameWithOwner":"acme/web","url":"https://github.com/acme/web","sshUrl":path(&bare)}));
    let recorded = api.recorded();
    assert_eq!((recorded[0].method.as_str(), recorded[0].path.as_str()), ("GET", "/rest/github.com/repos/acme/web"));
    assert_eq!(recorded[0].authorization.as_deref(), Some("Bearer fake-token"));

    let unknown = rpc(&mut readonly, 4, "sourceControl.lookupRepository", json!({"provider":"unknown","repository":"acme/web"})).await;
    assert_eq!(
        failure(&unknown),
        &json!({"_tag":"SourceControlRepositoryError","provider":"unknown","operation":"lookupRepository","detail":"Choose a source control provider before continuing."})
    );
    api.set_script(vec![Step::json(404, json!({"message":"Not Found"}))]);
    let missing = rpc(&mut readonly, 5, "sourceControl.lookupRepository", json!({"provider":"github","repository":"acme/gone"})).await;
    let error = failure(&missing);
    assert_eq!(error["_tag"], "SourceControlRepositoryError");
    assert_eq!((error["operation"].as_str(), error["detail"].as_str()), (Some("lookupRepository"), Some("The source control operation could not be completed.")));
    assert_eq!(error["cause"]["name"], "SourceControlProviderError");
    assert_eq!(
        error["cause"]["message"],
        "Source control provider github failed in getRepositoryCloneUrls: Pull request not found. Check the PR number or URL and try again."
    );
    let invalid = rpc(&mut readonly, 6, "sourceControl.lookupRepository", json!({"provider":"nope","repository":"x"})).await;
    // A payload that does not decode is a defect, as for every other method.
    assert_eq!(invalid["cause"][0]["_tag"], "Die");

    // ---- clone -------------------------------------------------------------------------------
    let refused = rpc(&mut readonly, 7, "sourceControl.cloneRepository", json!({"provider":"github","repository":"acme/web","destinationPath":path(&base.join("nope"))})).await;
    assert_eq!(failure(&refused)["_tag"], "EnvironmentAuthorizationError");
    assert!(!base.join("nope").exists());

    api.set_script(vec![repository_body("acme/web", path(&bare))]);
    let destination = base.join("clones").join("web");
    let cloned = success(
        &rpc(&mut socket, 8, "sourceControl.cloneRepository", json!({"provider":"github","repository":"acme/web","destinationPath":path(&destination),"protocol":"ssh"})).await,
    )
    .clone();
    assert_eq!(cloned["cwd"], path(&destination));
    assert_eq!(cloned["remoteUrl"], path(&bare));
    assert_eq!(cloned["repository"]["nameWithOwner"], "acme/web");
    assert_eq!(std::fs::read_to_string(destination.join("README.md")).unwrap(), "hello\n");

    let occupied = rpc(&mut socket, 9, "sourceControl.cloneRepository", json!({"provider":"github","repository":"acme/web","destinationPath":path(&destination)})).await;
    assert_eq!(failure(&occupied)["detail"], "Destination path already exists and is not empty.");
    let nothing = rpc(&mut socket, 10, "sourceControl.cloneRepository", json!({"destinationPath":path(&base.join("other"))})).await;
    assert_eq!(failure(&nothing)["detail"], "Enter a repository path or clone URL before cloning.");
    let by_url = success(&rpc(&mut socket, 11, "sourceControl.cloneRepository", json!({"remoteUrl":path(&bare),"destinationPath":path(&base.join("by-url"))})).await).clone();
    assert_eq!(by_url["repository"], Value::Null);
    assert!(base.join("by-url").join(".git").exists());
    let failed = rpc(&mut socket, 12, "sourceControl.cloneRepository", json!({"remoteUrl":path(&base.join("no-such.git")),"destinationPath":path(&base.join("broken"))})).await;
    let error = failure(&failed);
    assert_eq!(error["_tag"], "SourceControlRepositoryError");
    assert!(error["detail"].as_str().unwrap().to_lowercase().contains("does not exist") || error["detail"].as_str().unwrap().contains("could not"), "{error}");

    // ---- publish -----------------------------------------------------------------------------
    let publish_bare = base.join("tim-new.git");
    std::fs::create_dir_all(&publish_bare).unwrap();
    git(&publish_bare, &["init", "--bare", "-q"]);
    let local = base.join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q"]);
    let created = Step::json(201, json!({"full_name":"tim/new","html_url":"https://github.com/tim/new","ssh_url":path(&publish_bare)}));
    api.set_script(vec![Step::json(200, json!({"login":"tim"})), created.clone()]);
    let empty = success(
        &rpc(&mut socket, 13, "sourceControl.publishRepository", json!({"cwd":path(&local),"provider":"github","repository":"tim/new","visibility":"private","protocol":"ssh"})).await,
    )
    .clone();
    assert_eq!((empty["status"].as_str(), empty["remoteName"].as_str(), empty["branch"].as_str()), (Some("remote_added"), Some("origin"), Some("main")));
    assert_eq!(empty["remoteUrl"], path(&publish_bare));
    let posted = api.recorded();
    assert_eq!((posted[1].method.as_str(), posted[1].path.as_str()), ("POST", "/rest/github.com/user/repos"));
    assert_eq!(posted[1].body, Some(json!({"name":"new","private":true})));

    std::fs::write(local.join("a.txt"), "a\n").unwrap();
    git(&local, &["add", "."]);
    git(&local, &["commit", "-q", "-m", "first"]);
    api.set_script(vec![Step::json(200, json!({"login":"tim"})), created]);
    let pushed = success(
        &rpc(&mut socket, 14, "sourceControl.publishRepository", json!({"cwd":path(&local),"provider":"github","repository":"tim/new","visibility":"public","protocol":"ssh"})).await,
    )
    .clone();
    assert_eq!((pushed["status"].as_str(), pushed["branch"].as_str(), pushed["upstreamBranch"].as_str()), (Some("pushed"), Some("main"), Some("origin/main")));
    assert!(git(&publish_bare, &["rev-parse", "--verify", "refs/heads/main"]).len() >= 40);
    let no_provider = rpc(&mut socket, 15, "sourceControl.publishRepository", json!({"cwd":path(&local),"provider":"unknown","repository":"tim/new","visibility":"public"})).await;
    assert_eq!(failure(&no_provider)["detail"], "Choose a source control provider before continuing.");

    // ---- the host badge of the status --------------------------------------------------------
    let badge = |dir: PathBuf| dir;
    let mut seen = Vec::new();
    for (index, (remote, branch_remote)) in [
        ("https://github.com/acme/web.git", None),
        ("git@github.example.com:team/app.git", None),
        ("https://gitlab.com/group/project.git", None),
        ("https://dev.azure.com/org/project/_git/repo", None),
        ("https://bitbucket.org/ws/repo.git", None),
        ("https://code.example.com/team/app.git", None),
        ("https://git.nowhere.example/team/app.git", None),
        ("https://github.com/acme/web.git", Some("tracked")),
    ]
    .into_iter()
    .enumerate()
    {
        let (_repo, dir) = repo_with_remotes(&[("origin", remote)]);
        if let Some(name) = branch_remote {
            git(&dir, &["remote", "add", name, "https://gitlab.com/group/other.git"]);
            git(&dir, &["config", "branch.main.remote", name]);
        }
        let status = success(&rpc(&mut socket, 20 + index as u64, "vcs.refreshStatus", json!({"cwd":path(&badge(dir))})).await).clone();
        seen.push(status.get("sourceControlProvider").cloned().unwrap_or(Value::Null));
    }
    let provider = |kind: &str, name: &str, base_url: &str| json!({"kind":kind,"name":name,"baseUrl":base_url});
    assert_eq!(seen[0], provider("github", "GitHub", "https://github.com"));
    assert_eq!(seen[1], provider("github", "GitHub Self-Hosted", "https://github.example.com"));
    assert_eq!(seen[2], provider("gitlab", "GitLab", "https://gitlab.com"));
    assert_eq!(seen[3], provider("azure-devops", "Azure DevOps", "https://dev.azure.com"));
    assert_eq!(seen[4], provider("bitbucket", "Bitbucket", "https://bitbucket.org"));
    // A host nobody recognises by name is recognised by a CLI that is signed in to it.
    assert_eq!(seen[5], provider("gitlab", "GitLab Self-Hosted", "https://code.example.com"));
    assert_eq!(seen[6], provider("unknown", "git.nowhere.example", "https://git.nowhere.example"));
    // The remote the checked-out branch tracks decides, not `origin`.
    assert_eq!(seen[7], provider("gitlab", "GitLab", "https://gitlab.com"));
    let (_plain, no_remote) = repo_with_remotes(&[]);
    let plain = success(&rpc(&mut socket, 40, "vcs.refreshStatus", json!({"cwd":path(&no_remote)})).await).clone();
    assert!(plain.get("sourceControlProvider").is_none());

    stop.send(()).unwrap();
    server.await.unwrap();
}
