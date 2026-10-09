//! The pull request a checkout's status shows is read with the credentials the rest of the pull
//! request code uses, including a token saved in Settings; it used to be read as if no settings
//! existed. No network, no real `gh`.
mod source_control_support;

use serde_json::{Value, json};
use source_control_support::*;
use t3_server::{
    auth::AuthService,
    persistence::Store,
    server_secret_store::ServerSecretStore,
    server_settings::{SettingsOptions, SettingsService},
    source_control_host::Host,
    transport::ApiState,
};

#[test]
fn a_status_pull_request_lookup_uses_the_token_saved_in_settings() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(Box::pin(scenario()));
    // The process-wide status source keeps the settings service it was given, which keeps the
    // service's own thread alive; the runtime is let go of rather than waited for.
    runtime.shutdown_background();
}

async fn scenario() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let node = json!({"number":7,"title":"Fix it","url":"https://github.com/acme/web/pull/7","baseRefName":"main","headRefName":"feature","headRefOid":"abc1234","state":"OPEN","isDraft":false,
        "mergedAt":null,"closedAt":null,"updatedAt":"2026-07-01T00:00:00Z","isCrossRepository":false,"headRepository":{"name":"web","nameWithOwner":"acme/web"},"headRepositoryOwner":{"login":"acme"}});
    let github = ScriptedServer::start(vec![Step::json(200, json!({"data":{"repository":{"h0":{"nodes":[node]}}}}))]).await;
    let cli = FakeCliDir::new();
    // No token in the environment and no `gh`: Settings is the only place one can come from.
    let test = test_host(&[], Some(cli.path()), Some(github.github_resolver()), &base);
    assert!(Host::install(test.host.clone()), "this binary installs the process host once");

    // Pushes and fetches go to a local bare repository; GitHub is only the name of the other remote.
    let remote = base.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "--bare", "--initial-branch=main"]);
    let (_repo, path) = repo_with_remotes(&[("origin", remote.to_str().unwrap()), ("upstream", "https://github.com/acme/web.git")]);
    git(&path, &["checkout", "-b", "feature"]);
    git(&path, &["push", "-u", "origin", "feature"]);

    let mut options = SettingsOptions::file(base.join("settings.json"), ServerSecretStore::open(base.join("secrets")).unwrap());
    options.watch = false;
    let settings = SettingsService::start(options).await.unwrap();
    settings.update(serde_json::from_value(json!({"github":{"tokens":{"github.com":"settings-token"}}})).unwrap()).await.unwrap();
    let store = Store::open(base.join("state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [9; 32], "t3_session_spt".into(), "loopback-browser".into()).unwrap();
    let state = ApiState {
        store,
        auth,
        environment: json!({}),
        config: None, settings: Some(settings), cors_origins: None, assets: None, providers: None, execution: None, workspace: None, terminals: None,
        discovery: None, resource_telemetry: None, host_resources: None, background: None, device_hosts: None, devices: None, provider_auth: None,
    };
    let status: Value = t3_server::vcs_rpc::command(&state, "vcs.refreshStatus", json!({"cwd":path.to_str().unwrap()})).await.unwrap();
    assert_eq!(status["pr"]["number"], 7, "{status}");
    let requests = github.recorded();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| request.authorization.as_deref() == Some("Bearer settings-token")), "{requests:?}");
}
