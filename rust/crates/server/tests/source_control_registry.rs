//! Host matching (`SourceControlProviderRegistry.test.ts`), discovery
//! (`SourceControlDiscovery.test.ts`), GitHub credentials (`GitHubCredentials.test.ts`) and the
//! GitHub pull request checkout (`GitHubCli.test.ts`), against throwaway repositories, fake host
//! CLIs on a private `PATH`, and a scripted local GitHub.
mod source_control_support;

use serde_json::{Value, json};
use source_control_support::*;
use t3_contracts::{ServerSettingsPatch, SourceControlProviderInfo, SourceControlProviderKind};
use t3_server::{
    server_secret_store::ServerSecretStore,
    server_settings::{SettingsOptions, SettingsService},
    source_control_github_credentials as credentials,
    source_control_provider::*,
    source_control_registry::*,
};

fn info(kind: &str, name: &str, base_url: &str) -> SourceControlProviderInfo {
    serde_json::from_value(json!({"kind":kind,"name":name,"baseUrl":base_url})).unwrap()
}

fn remotes(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(n, u)| (n.to_string(), u.to_string())).collect()
}

#[test]
fn prefers_origin_then_a_recognised_host_then_any_host() {
    let pick = |list: &[(&str, &str)]| select_provider_context(&remotes(list)).map(|c| (c.remote_name, c.provider.kind));
    use SourceControlProviderKind::*;
    assert_eq!(pick(&[("upstream", "git@gitlab.com:g/p.git"), ("origin", "git@github.com:a/b.git")]), Some(("origin".into(), Github)));
    // Without origin, the first remote whose host is recognised wins over an unknown one.
    assert_eq!(pick(&[("a", "https://code.example/x/y"), ("fork", "https://gitlab.com/g/p")]), Some(("fork".into(), Gitlab)));
    assert_eq!(pick(&[("a", "https://code.example/x/y")]), Some(("a".into(), Unknown)));
    assert_eq!(pick(&[("a", "/local/path/repo.git")]), None);
    assert_eq!(pick(&[]), None);
}

fn registry(test: &TestHost) -> SourceControlRegistry {
    SourceControlRegistry::new(test.host.clone())
}

#[tokio::test]
async fn routes_a_checkout_to_its_host_and_reports_unregistered_ones() {
    let work = tempfile::tempdir().unwrap();
    let test = test_host(&[], Some(FakeCliDir::new().path()), None, work.path());
    let registry = registry(&test);
    for (url, kind, name) in [
        ("git@github.com:a/b.git", "github", "GitHub"),
        ("https://gitlab.com/g/p.git", "gitlab", "GitLab"),
        ("https://dev.azure.com/o/p/_git/r", "azure-devops", "Azure DevOps"),
        ("https://bitbucket.org/w/r.git", "bitbucket", "Bitbucket"),
    ] {
        let (_repo, path) = repo_with_remotes(&[("origin", url)]);
        let handle = registry.resolve_handle(path.to_str().unwrap(), None).await.unwrap();
        let context = handle.context.unwrap();
        assert_eq!((context.provider.kind.as_str(), context.provider.name.as_str(), context.remote_name.as_str()), (kind, name, "origin"));
        assert_eq!(handle.provider.kind().as_str(), kind);
    }
    // A host with no registered provider fails each operation by name, with the working directory.
    let (_repo, path) = repo_with_remotes(&[("origin", "https://codeberg.org/o/r.git")]);
    let cwd = path.to_str().unwrap().to_string();
    let provider = registry.resolve(&cwd).await.unwrap();
    let error = provider.get_default_branch(DefaultBranchInput { cwd: cwd.clone(), context: None }).await.unwrap_err();
    assert_eq!(
        error.wire(),
        json!({"_tag":"SourceControlProviderError","provider":"forgejo","operation":"getDefaultBranch","cwd":cwd,"detail":"No forgejo source control provider is registered."})
    );
    // A checkout with no remote has no host, and the provider for "unknown" says so.
    let (_bare, plain) = repo_with_remotes(&[]);
    let handle = registry.resolve_handle(plain.to_str().unwrap(), None).await.unwrap();
    assert!(handle.context.is_none());
    assert_eq!(handle.provider.kind(), SourceControlProviderKind::Unknown);
    // A directory that is no repository is a detection failure with context.
    let missing = work.path().join("not-a-repo");
    std::fs::create_dir_all(&missing).unwrap();
    let error = registry.resolve(missing.to_str().unwrap()).await.err().unwrap();
    let wire = error.wire();
    assert_eq!((wire["operation"].as_str(), wire["detail"].as_str(), wire["provider"].as_str()), (Some("detectProvider"), Some("Failed to detect source control provider."), Some("unknown")));
    assert_eq!(wire["cause"]["name"], "VcsUnsupportedOperationError");
}

#[tokio::test]
async fn refines_an_unrecognised_remote_through_the_signed_in_cli() {
    let cli = FakeCliDir::new();
    cli.install("glab", r#"printf 'code.example.com:8443\n  Logged in to code.example.com:8443 as corp (token)\n'"#);
    let work = tempfile::tempdir().unwrap();
    let test = test_host(&[], Some(cli.path()), None, work.path());
    let registry = registry(&test);
    // The selected remote is refined, not another one that happens to be configured.
    let (_repo, path) = repo_with_remotes(&[("origin", "https://code.example.com:8443/team/app.git"), ("other", "https://other.example.org/x/y")]);
    let cwd = path.to_str().unwrap();
    let context = registry.resolve_handle(cwd, None).await.unwrap().context.unwrap();
    assert_eq!(context.provider, info("gitlab", "GitLab Self-Hosted", "https://code.example.com:8443"));
    assert_eq!(cli.calls("glab")[0], ["auth", "status"]);
    // An explicit context for a remote the CLI is not signed in to stays unknown.
    let explicit = ProviderContext {
        provider: info("unknown", "other.example.org", "https://other.example.org"),
        remote_name: "other".into(),
        remote_url: "https://other.example.org/x/y".into(),
        requested_host: None,
    };
    let handle = registry.resolve_handle(cwd, Some(explicit.clone())).await.unwrap();
    assert_eq!(handle.context.unwrap(), explicit);
    // The detected host is kept for a few seconds: a second lookup does not ask the CLI again.
    let before = cli.calls("glab").len();
    registry.resolve_handle(cwd, None).await.unwrap();
    assert_eq!(cli.calls("glab").len(), before);
    test.clock.advance(std::time::Duration::from_secs(6));
    registry.resolve_handle(cwd, None).await.unwrap();
    assert_eq!(cli.calls("glab").len(), before + 1);
}

#[tokio::test]
async fn resolves_linked_subjects_only_for_credential_free_https_urls() {
    let work = tempfile::tempdir().unwrap();
    let test = test_host(&[], Some(FakeCliDir::new().path()), None, work.path());
    let registry = registry(&test);
    for refused in [
        "https://example.test/team/project/issues/1",
        "https://github.attacker.test/team/project/issues/1",
        "https://gitlab.attacker.test/team/project/-/issues/1",
        "https://github.com/team/project",
        "https://codeberg.org/team/project/issues/1",
        "https://bitbucket.org/team/project/pull-requests/1",
        "http://github.com/team/project/issues/1",
        "https://user:secret@github.com/team/project/issues/1",
        "https://gitlab.com/team/project/-/wiki/1",
    ] {
        assert!(registry.resolve_link("/tmp", &url::Url::parse(refused).unwrap()).is_none(), "{refused}");
    }
    for accepted in ["https://github.com/team/project/pull/2/files", "https://github.com/team/project/issues/1", "https://gitlab.com/team/sub/project/-/merge_requests/2"] {
        assert!(registry.resolve_link("/tmp", &url::Url::parse(accepted).unwrap()).is_some(), "{accepted}");
    }
}

#[tokio::test]
async fn reads_a_linked_github_subject_from_the_linked_host() {
    let api = ScriptedServer::start(vec![Step::json(200, json!({"title":"Fix it","body":null}))]).await;
    let cli = FakeCliDir::new();
    cli.install("gh", "printf token");
    let work = tempfile::tempdir().unwrap();
    let test = test_host(&[], Some(cli.path()), Some(api.github_resolver()), work.path());
    let registry = registry(&test);
    let lookup = registry.resolve_link("/tmp", &url::Url::parse("https://github.com/acme/web/pull/7/files").unwrap()).unwrap();
    assert_eq!(lookup.await.unwrap(), LinkSubject { title: "Fix it".into(), body: None });
    assert_eq!(api.recorded()[0].path, "/rest/github.com/repos/acme/web/issues/7");
    api.set_script(vec![Step::json(500, json!({}))]);
    let failed = registry.resolve_link("/tmp", &url::Url::parse("https://github.com/acme/web/issues/8").unwrap()).unwrap().await.unwrap_err();
    assert_eq!(failed.wire()["detail"], "The linked subject could not be read.");
}

fn item<'a>(result: &'a Value, kind: &str) -> &'a Value {
    result["sourceControlProviders"].as_array().unwrap().iter().find(|item| item["kind"] == kind).unwrap()
}

#[tokio::test]
async fn discovery_separates_installed_tools_from_signed_in_accounts() {
    let work = tempfile::tempdir().unwrap();
    // Nothing installed but git.
    let bare = FakeCliDir::new();
    let none = serde_json::to_value(registry(&test_host(&[], Some(bare.path()), None, work.path())).discover().await).unwrap();
    assert_eq!(none["versionControlSystems"][0]["status"], "available");
    assert_eq!(none["versionControlSystems"][1]["status"], "missing");
    for kind in ["github", "gitlab", "azure-devops"] {
        let missing = item(&none, kind);
        assert_eq!(missing["status"], "missing", "{kind}");
        assert_eq!(missing["auth"]["status"], "unknown");
        assert_eq!(missing["auth"]["detail"], json!({"_tag":"Some","value":"Hosting integration command was not found on the server PATH."}));
        assert!(missing["detail"]["value"].as_str().unwrap().contains("failed to spawn"));
    }
    // Installed but signed out, too old, or signed in.
    let cli = FakeCliDir::new();
    cli.install("gh", r#"if [ "$1" = "auth" ]; then echo 'unknown flag: --json' >&2; exit 1; fi; echo 'gh version 2.40.0'"#);
    cli.install("glab", r#"if [ "$1" = "auth" ]; then echo 'gitlab.com'; echo '  x not logged in' ; exit 1; fi; echo 'glab 1.40'"#);
    cli.install("az", r#"if [ "$1" = "account" ]; then echo 'Please run az login' >&2; exit 1; fi; echo 'azure-cli 2.60'"#);
    let result = serde_json::to_value(registry(&test_host(&[], Some(cli.path()), None, work.path())).discover().await).unwrap();
    assert_eq!(item(&result, "github")["auth"]["status"], "unknown");
    assert!(item(&result, "github")["auth"]["detail"]["value"].as_str().unwrap().contains("Update `gh` to 2.81.0"));
    assert_eq!(item(&result, "gitlab")["auth"]["status"], "unauthenticated");
    assert_eq!(item(&result, "gitlab")["auth"]["host"], json!({"_tag":"Some","value":"gitlab.com"}));
    assert_eq!(item(&result, "azure-devops")["auth"]["status"], "unauthenticated");
    assert_eq!(item(&result, "azure-devops")["version"], json!({"_tag":"Some","value":"azure-cli 2.60"}));
    // Bitbucket has no executable and is always "available"; its auth comes from the API.
    let bitbucket = item(&result, "bitbucket");
    assert_eq!((bitbucket["status"].as_str(), bitbucket.get("executable")), (Some("available"), None));
}

/// The original's `probeAuth` asks Bitbucket for `/user` whether or not a credential is set, so
/// discovery contacts the API host; this proves the request lands on the redirected loopback
/// server and carries no credential.
#[tokio::test]
async fn bitbucket_discovery_probes_the_redirected_api_without_a_credential() {
    let work = tempfile::tempdir().unwrap();
    let api = ScriptedServer::start(vec![Step::json(401, json!({}))]).await;
    let base = format!("{}/2.0", api.origin);
    let cli = FakeCliDir::new();
    let test = test_host(&[("T3CODE_BITBUCKET_API_BASE_URL", base.as_str())], Some(cli.path()), None, work.path());
    let result = serde_json::to_value(registry(&test).discover().await).unwrap();
    assert_eq!(item(&result, "bitbucket")["auth"]["status"], "unauthenticated");
    let recorded = api.recorded();
    assert_eq!((recorded[0].path.as_str(), recorded[0].authorization.as_deref()), ("/2.0/user", None));
}

#[tokio::test]
async fn github_discovery_checks_an_environment_token_against_the_api() {
    let cli = FakeCliDir::new();
    cli.install("gh", r#"if [ "$1" = "auth" ]; then echo 'not logged in' >&2; exit 1; fi; echo 'gh version 2.83.0'"#);
    let work = tempfile::tempdir().unwrap();
    for (script, expected_status, expected_detail) in [
        (Step::json(200, json!({"login":"envuser"})), "authenticated", "Using GH_TOKEN from the server environment; it overrides the account chosen in Settings."),
        (Step::json(401, json!({})), "unauthenticated", "GitHub refused the token in GH_TOKEN. Replace it, or unset it to use `gh auth login`."),
        (Step::json(500, json!({})), "unknown", "Could not check the token in GH_TOKEN: GitHub returned HTTP 500."),
    ] {
        let api = ScriptedServer::start(vec![script]).await;
        let test = test_host(&[("GH_TOKEN", "env-token")], Some(cli.path()), Some(api.github_resolver()), work.path());
        let result = serde_json::to_value(registry(&test).discover().await).unwrap();
        let github = item(&result, "github");
        assert_eq!(github["status"], "available");
        assert_eq!(github["auth"]["status"], expected_status);
        assert_eq!(github["auth"]["detail"], json!({"_tag":"Some","value":expected_detail}));
        assert_eq!(api.recorded()[0].path, "/rest/github.com/user");
        assert_eq!(api.recorded()[0].authorization.as_deref(), Some("Bearer env-token"));
    }
}

async fn settings_service(dir: &std::path::Path) -> SettingsService {
    let mut options = SettingsOptions::file(dir.join("settings.json"), ServerSecretStore::open(dir.join("secrets")).unwrap());
    options.watch = false;
    SettingsService::start(options).await.unwrap()
}

fn patch(value: Value) -> ServerSettingsPatch {
    serde_json::from_value(value).unwrap()
}

/// A fake `gh auth token` that answers per account the way the original's mock does.
const ACCOUNT_TOKENS: &str = r#"
if [ "$1 $2" = "auth token" ]; then
  user=""; for arg in "$@"; do if [ "$prev" = "--user" ]; then user="$arg"; fi; prev="$arg"; done
  if [ -z "$user" ]; then echo 'active-token'; elif [ "$user" = "gone" ]; then :; else echo "token-for-$user"; fi
fi
"#;

#[tokio::test]
async fn github_credentials_follow_settings_the_environment_and_gh() {
    let dir = tempfile::tempdir().unwrap();
    let settings = settings_service(dir.path()).await;
    let cli = FakeCliDir::new();
    cli.install("gh", ACCOUNT_TOKENS);
    let work = tempfile::tempdir().unwrap();
    let test = test_host(&[], Some(cli.path()), None, work.path());
    let host = test.host.with_settings(Some(settings.clone()));
    let token = |c: &credentials::GitHubCredential| c.token.expose().to_string();

    // gh's active login when Settings pin nothing.
    let first = credentials::get(&host, "GitHub.com").await.unwrap();
    assert_eq!((token(&first), first.source), ("active-token".to_string(), credentials::GitHubCredentialSource::Gh));
    assert_eq!(cli.calls("gh"), vec![vec!["auth", "token", "--hostname", "github.com"]]);

    // A pinned account applies on the next request, a host turned off fails without asking gh,
    // and clearing the choice finds the still-cached unpinned token.
    settings.update(patch(json!({"github":{"hosts":{"github.com":{"account":"work","enabled":true}}}}))).await.unwrap();
    assert_eq!(token(&credentials::get(&host, "github.com").await.unwrap()), "token-for-work");
    assert_eq!(cli.calls("gh")[1], ["auth", "token", "--hostname", "github.com", "--user", "work"]);
    settings.update(patch(json!({"github":{"hosts":{"github.com":{"enabled":false}}}}))).await.unwrap();
    let off = credentials::get(&host, "github.com").await.unwrap_err();
    assert_eq!((off.tag(), off.message()), ("GitHubHostDisabledError", "GitHub host github.com is turned off in Settings → Source Control.".to_string()));
    settings.update(patch(json!({"github":{"hosts":{}}}))).await.unwrap();
    assert_eq!(token(&credentials::get(&host, "github.com").await.unwrap()), "active-token");
    assert_eq!(cli.calls("gh").len(), 2, "the unpinned token stayed cached");

    // A pinned login gh no longer holds falls back to the active one.
    settings.update(patch(json!({"github":{"hosts":{"github.com":{"account":"gone","enabled":true}}}}))).await.unwrap();
    assert_eq!(token(&credentials::get(&host, "github.com").await.unwrap()), "active-token");
    let calls = cli.calls("gh");
    assert_eq!(&calls[2][4..], ["--user", "gone"]);
    assert_eq!(calls[3], ["auth", "token", "--hostname", "github.com"]);

    // A token saved in Settings beats the environment and gh, and never asks gh; a host turned
    // off stays off even with one.
    let env_host = test_host(&[("GH_TOKEN", "env-token")], Some(cli.path()), None, work.path()).host.with_settings(Some(settings.clone()));
    settings.update(patch(json!({"github":{"hosts":{},"tokens":{"github.com":"saved-token"}}}))).await.unwrap();
    let before = cli.calls("gh").len();
    let saved = credentials::get(&env_host, "github.com").await.unwrap();
    assert_eq!((token(&saved), saved.source), ("saved-token".to_string(), credentials::GitHubCredentialSource::Settings));
    assert_eq!(cli.calls("gh").len(), before);
    settings.update(patch(json!({"github":{"hosts":{"github.com":{"enabled":false}}}}))).await.unwrap();
    assert_eq!(credentials::get(&env_host, "github.com").await.unwrap_err().tag(), "GitHubHostDisabledError");

    // An environment token beats a pinned account, as in gh.
    settings.update(patch(json!({"github":{"hosts":{"github.com":{"account":"work","enabled":true}},"tokens":{"github.com":""}}}))).await.unwrap();
    let from_env = credentials::get(&env_host, "github.com").await.unwrap();
    assert_eq!((token(&from_env), from_env.source), ("env-token".to_string(), credentials::GitHubCredentialSource::Env));
    assert_eq!(cli.calls("gh").len(), before);
    settings.shutdown().await;
}

#[tokio::test]
async fn github_credentials_report_a_missing_gh() {
    let work = tempfile::tempdir().unwrap();
    let test = test_host(&[], Some(FakeCliDir::new().path()), None, work.path());
    let missing = credentials::get(&test.host, "github.com").await.unwrap_err();
    assert_eq!(missing.tag(), "GitHubCliMissingError");
    let cli = FakeCliDir::new();
    cli.install("gh", "echo 'no oauth token found' >&2; exit 1");
    let signed_out = credentials::get(&test_host(&[], Some(cli.path()), None, work.path()).host, "ghe.acme.test").await.unwrap_err();
    assert_eq!(signed_out.message(), "No GitHub credential for ghe.acme.test: run `gh auth login --hostname ghe.acme.test`.");
}

fn pull_node(number: u64, head: &str, owner: &str, cross: bool) -> Value {
    json!({
        "number":number,"title":format!("PR {number}"),"url":format!("https://github.com/acme/web/pull/{number}"),"baseRefName":"main",
        "headRefName":head,"headRefOid":"abc","state":"OPEN","isCrossRepository":cross,"updatedAt":"2026-01-02T00:00:00Z",
        "headRepository":{"name":"web","nameWithOwner":format!("{owner}/web")},"headRepositoryOwner":{"login":owner},
    })
}

struct Checkout {
    server: ScriptedServer,
    provider: t3_server::source_control_github_provider::GitHubProvider,
    repo: std::path::PathBuf,
    _keep: (tempfile::TempDir, tempfile::TempDir, FakeCliDir),
}

/// A checkout whose `origin` is a local repository holding `feature`, read as `acme/web` through
/// GH_REPO, with a scripted GitHub.
async fn checkout_fixture(script: Vec<Step>) -> Checkout {
    let bare = tempfile::tempdir().unwrap();
    let bare_path = std::fs::canonicalize(bare.path()).unwrap();
    git(&bare_path, &["init", "--bare", "-q"]);
    let (repo_dir, repo) = repo_with_remotes(&[("origin", bare_path.to_str().unwrap())]);
    git(&repo, &["push", "-q", "origin", "HEAD:refs/heads/feature"]);
    std::fs::write(repo.join("main-only.txt"), "x").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "main moves"]);
    let server = ScriptedServer::start(script).await;
    let cli = FakeCliDir::new();
    cli.install("gh", "printf token");
    let test = test_host(&[("GH_REPO", "acme/web")], Some(cli.path()), Some(server.github_resolver()), &repo);
    let provider = t3_server::source_control_github_provider::GitHubProvider::new(test.host.clone());
    Checkout { server, provider, repo, _keep: (bare, repo_dir, cli) }
}

fn checkout_input(c: &Checkout, reference: &str, force: bool) -> CheckoutChangeRequestInput {
    CheckoutChangeRequestInput { cwd: c.repo.to_string_lossy().into_owned(), context: None, reference: reference.into(), force }
}

#[tokio::test]
async fn checks_a_same_repository_pull_request_out_from_its_head_branch() {
    let c = checkout_fixture(vec![Step::json(200, json!({"data":{"repository":{"pullRequest":pull_node(7, "feature", "acme", false)}}}))]).await;
    c.provider.checkout_change_request(checkout_input(&c, "7", false)).await.unwrap();
    assert_eq!(git(&c.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature");
    assert_eq!(git(&c.repo, &["rev-parse", "--abbrev-ref", "feature@{upstream}"]), "origin/feature");
    // Behind a moved branch, an existing checkout fast-forwards; one that diverged needs force.
    git(&c.repo, &["checkout", "-q", "feature"]);
    std::fs::write(c.repo.join("local.txt"), "local").unwrap();
    git(&c.repo, &["add", "."]);
    git(&c.repo, &["commit", "-q", "-m", "local work"]);
    git(&c.repo, &["push", "-q", "--force", "origin", "main:refs/heads/feature"]);
    git(&c.repo, &["checkout", "-q", "main"]);
    let diverged = c.provider.checkout_change_request(checkout_input(&c, "7", false)).await.unwrap_err();
    assert_eq!(diverged.wire()["operation"], "checkoutChangeRequest");
    c.provider.checkout_change_request(checkout_input(&c, "7", true)).await.unwrap();
    assert_eq!(git(&c.repo, &["rev-parse", "feature"]), git(&c.repo, &["rev-parse", "origin/feature"]));
}

#[tokio::test]
async fn refuses_a_fork_checkout_when_the_default_branch_cannot_be_read_and_names_default_heads_after_the_owner() {
    // The base's default branch is unreadable.
    let c = checkout_fixture(vec![
        Step::json(200, json!({"data":{"repository":{"pullRequest":pull_node(8, "feature", "fork", true)}}})),
        Step::json(500, json!({})),
    ])
    .await;
    let error = c.provider.checkout_change_request(checkout_input(&c, "8", false)).await.unwrap_err();
    assert_eq!(error.wire()["cause"]["name"], "GitHubCliCommandError");
    assert_eq!(git(&c.repo, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");

    // A fork whose head is called like the base's default branch is checked out as `owner/main`,
    // fetched from a remote added for the fork.
    let fork = tempfile::tempdir().unwrap();
    let fork_path = std::fs::canonicalize(fork.path()).unwrap();
    git(&fork_path, &["init", "--bare", "-q"]);
    git(&c.repo, &["push", "-q", fork_path.to_str().unwrap(), "HEAD:refs/heads/main"]);
    let repo_body = |full: &str, ssh: &str| json!({"full_name":full,"html_url":ssh,"ssh_url":ssh,"default_branch":"main"});
    c.server.set_script(vec![
        Step::json(200, json!({"data":{"repository":{"pullRequest":pull_node(9, "main", "fork", true)}}})),
        Step::json(200, repo_body("acme/web", "unused")),
        Step::json(200, repo_body("fork/web", fork_path.to_str().unwrap())),
    ]);
    // (The original runs the same `git branch --set-upstream-to=fork/main` for a local branch
    // that is itself called `fork/main`, which git refuses as ambiguous; the checkout and the
    // remote are what this pins.)
    let _ = c.provider.checkout_change_request(checkout_input(&c, "9", false)).await;
    assert_eq!(git(&c.repo, &["symbolic-ref", "HEAD"]), "refs/heads/fork/main");
    assert!(git(&c.repo, &["remote"]).lines().any(|name| name == "fork"));
}
