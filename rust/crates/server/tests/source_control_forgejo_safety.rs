//! What Forgejo support must never do, and how it is found: the token `fj` stores goes only to the
//! server it belongs to, is not repeated in an error, and is not sent where a redirect points;
//! discovery lists Forgejo and Gitea as the fifth host; a remote nobody recognises is told apart by
//! the logins; the identity hook's web address strips a server's mount path. Loopback only, HOME is
//! a temporary directory, and `fj` is a fake.
mod pull_request_support;
mod source_control_support;

use pull_request_support::*;
use serde_json::json;
use source_control_support::*;
use t3_contracts::*;
use t3_server::{
    source_control_forgejo::{ApiInput, ForgejoCli, RepositoryInput, host_platform, keys_paths, web_url},
    source_control_provider::ProviderContext,
    source_control_registry::SourceControlRegistry,
};

/// A temporary HOME holding fj's key for `host`, and a fake `fj` that confirms every login.
fn fj_home(base: &std::path::Path, host: &str, token: &str) -> (String, FakeCliDir) {
    let home = base.join("home");
    let key_file = keys_paths(host_platform(), &home.to_string_lossy(), None, None).remove(0);
    std::fs::create_dir_all(std::path::Path::new(&key_file).parent().unwrap()).unwrap();
    std::fs::write(&key_file, json!({"hosts": {host: {"type": "Application", "token": token}}}).to_string()).unwrap();
    let cli = FakeCliDir::new();
    install_routed(&cli, "fj", &[json!({"match":"whoami","stdout":""}), json!({"match":"version","stdout":"fj 0.6.0\n"})], ["unauthorized", "HTTP 429", "404"]);
    (home.to_string_lossy().into_owned(), cli)
}

#[tokio::test]
async fn the_token_goes_only_where_it_belongs() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let elsewhere = RouteServer::start_at(&[json!({"method":"GET","match":"/","body":"{}"})], "").await;
    let location = format!("{}/stolen", elsewhere.base);
    let server = RouteServer::start_at(
        &[
            json!({"method":"GET","match":"/api/v1/repos/acme/web/pulls/302","status":302,"body":"","location":location}),
            json!({"method":"GET","match":"/api/v1/repos/acme/web/pulls/400","status":400,"body":"you sent token test-token"}),
            json!({"method":"GET","match":"/api/v1/repos/acme/web","body":"{\"full_name\":\"acme/web\"}"}),
        ],
        "",
    )
    .await;
    let host = server.base.trim_start_matches("http://").to_string();
    let (home, cli) = fj_home(&base, &host, "test-token");
    let (_repo, repo_path) = repo_with_remotes(&[("origin", &format!("{}/acme/web.git", server.base))]);
    let test = test_host(&[("HOME", &home)], Some(cli.path()), None, &repo_path);
    let forgejo = ForgejoCli::new(test.host.clone());
    // the checkout's own remote says the server is plain HTTP, which is how fj's login is read as one
    let context = ProviderContext {
        provider: SourceControlProviderInfo { kind: SourceControlProviderKind::Forgejo, name: "Forgejo / Gitea".into(), base_url: server.base.clone() },
        remote_name: "origin".into(),
        remote_url: format!("{}/acme/web.git", server.base),
        requested_host: None,
    };
    let target = || RepositoryInput { cwd: repo_path.to_string_lossy().into_owned(), repository: Some("acme/web".into()), context: Some(context.clone()), ..Default::default() };
    let api = |path: &str| ApiInput { target: target(), path: path.into(), method: None, body: None };

    // a redirect is not followed, so the token cannot be carried to the place it points at
    let failure = forgejo.api(api("repos/acme/web/pulls/302")).await.err().unwrap();
    assert_eq!(failure.http_status, Some(302), "{}", failure.detail);
    assert!(elsewhere.take().is_empty() && elsewhere.authorizations().is_empty(), "nothing reached the redirect's target");

    // a server that repeats what it was sent does not put the token in an error
    let failure = forgejo.api(api("repos/acme/web/pulls/400")).await.err().unwrap();
    assert_eq!(failure.http_status, Some(400));
    assert!(failure.detail.contains("[redacted]") && !failure.detail.contains("test-token"), "{}", failure.detail);

    // a path that would leave the API, or the server, is refused before anything is sent
    server.take();
    for path in ["../../user", "../x", "http://127.0.0.1:1/api/v1/user", "//example.invalid/api/v1/x", "https://user:pass@example.invalid/"] {
        let failure = forgejo.api(api(path)).await.err().unwrap_or_else(|| panic!("{path} was sent"));
        assert!(failure.detail.contains("Invalid Forgejo API path") || failure.detail.contains("not found") || failure.detail.contains("request failed"), "{path}: {}", failure.detail);
    }
    assert!(server.take().iter().all(|request| request["url"].as_str().unwrap().starts_with("/api/v1/")), "only the API of that server was asked");

    // every request carried the credential fj stores, and only the servers' own
    assert!(server.authorizations().iter().all(|(_, header)| header.as_deref() == Some("token test-token")));
    assert!(elsewhere.authorizations().is_empty());
}

#[tokio::test]
async fn discovery_lists_forgejo_as_the_fifth_host_and_reads_its_account() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let server = RouteServer::start_at(&[json!({"method":"GET","match":"/api/v1/user","body":"{\"login\":\"carol\"}"})], "").await;
    let host = server.base.trim_start_matches("http://").to_string();
    let (home, cli) = fj_home(&base, &host, "test-token");
    let (_repo, repo_path) = repo_with_remotes(&[("origin", &format!("{}/acme/web.git", server.base))]);
    let test = test_host(&[("HOME", &home)], Some(cli.path()), None, &repo_path);
    let registry = SourceControlRegistry::new(test.host.clone());
    let found = registry.discover().await;
    let kinds: Vec<SourceControlProviderKind> = found.source_control_providers.iter().map(|item| item.kind).collect();
    assert_eq!(kinds, vec![SourceControlProviderKind::Github, SourceControlProviderKind::Gitlab, SourceControlProviderKind::AzureDevops, SourceControlProviderKind::Bitbucket, SourceControlProviderKind::Forgejo]);
    let forgejo = found.source_control_providers.iter().find(|item| item.kind == SourceControlProviderKind::Forgejo).unwrap();
    assert_eq!((forgejo.label.as_str(), forgejo.executable.as_deref(), forgejo.status), ("Forgejo / Gitea", Some("fj"), SourceControlDiscoveryStatus::Available));
    assert_eq!(forgejo.auth.status, SourceControlProviderAuthStatus::Authenticated);
    assert_eq!((forgejo.auth.account.as_deref(), forgejo.auth.host.as_deref()), (Some("carol"), Some(host.as_str())));
    assert!(forgejo.install_hint.contains("fj") && forgejo.install_hint.contains("tea"));
    assert!(server.authorizations().iter().all(|(_, header)| header.as_deref() == Some("token test-token")));

    // with neither tool installed, it is reported missing rather than failing the listing
    let bare = FakeCliDir::new();
    let test = test_host(&[("HOME", &home)], Some(bare.path()), None, &repo_path);
    let found = SourceControlRegistry::new(test.host.clone()).discover().await;
    let forgejo = found.source_control_providers.iter().find(|item| item.kind == SourceControlProviderKind::Forgejo).unwrap();
    assert_eq!(forgejo.status, SourceControlDiscoveryStatus::Missing);
}

#[tokio::test]
async fn a_remote_nobody_recognises_is_told_apart_by_the_logins() {
    let work = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(work.path()).unwrap();
    let (home, cli) = fj_home(&base, "git.company.test", "test-token");
    // fj holds a login for the server, so the remote on it is Forgejo; one on another server is not
    let (_repo, repo_path) = repo_with_remotes(&[]);
    let test = test_host(&[("HOME", &home)], Some(cli.path()), None, &repo_path);
    let registry = SourceControlRegistry::new(test.host.clone());
    let context = |url: &str| ProviderContext {
        provider: SourceControlProviderInfo { kind: SourceControlProviderKind::Unknown, name: "Unknown".into(), base_url: String::new() },
        remote_name: "origin".into(),
        remote_url: url.into(),
        requested_host: None,
    };
    let cwd = repo_path.to_string_lossy().into_owned();
    let known = registry.resolve_handle(&cwd, Some(context("https://git.company.test/acme/web.git"))).await.unwrap();
    let provider = known.context.unwrap().provider;
    assert_eq!((provider.kind, provider.name.as_str(), provider.base_url.as_str()), (SourceControlProviderKind::Forgejo, "Forgejo / Gitea", "https://git.company.test"));
    let ssh = registry.resolve_handle(&cwd, Some(context("git@git.company.test:acme/web.git"))).await.unwrap();
    assert_eq!(ssh.context.unwrap().provider.kind, SourceControlProviderKind::Forgejo, "an scp-style remote is matched by its host");
    let stranger = registry.resolve_handle(&cwd, Some(context("https://git.other.test/acme/web.git"))).await.unwrap();
    assert_eq!(stranger.context.unwrap().provider.kind, SourceControlProviderKind::Unknown);
}

#[test]
fn the_web_address_of_a_repository_leaves_a_servers_mount_path_out() {
    for (remote, base, expected) in [
        ("https://git.test/acme/web.git", "https://git.test", Some("https://git.test/acme/web")),
        ("https://git.test/git/acme/web.git", "https://git.test/git", Some("https://git.test/git/acme/web")),
        ("https://git.test/git/acme/web", "https://git.test/git/", Some("https://git.test/git/acme/web")),
        ("https://git.test/gitx/acme/web", "https://git.test/git", Some("https://git.test/git/gitx/acme/web")),
        ("ssh://git@git.test/git/acme/web.git", "https://git.test/git", Some("https://git.test/git/git/acme/web")),
        ("git@git.test:acme/web.git", "https://git.test", Some("https://git.test/acme/web")),
        ("nonsense", "https://git.test", None),
    ] {
        assert_eq!(web_url(remote, base).as_deref(), expected, "{remote} on {base}");
    }
}
