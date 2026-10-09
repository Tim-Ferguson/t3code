//! Bitbucket: pull request decoding against `fixtures/source-control-bitbucket.jsonl` (run from
//! the original by `rust/tools/generate_source_control_bitbucket_fixtures.mjs`), and the API
//! operations (`BitbucketApi.test.ts`) against a scripted local server.
mod source_control_support;

use serde_json::{Value, json};
use source_control_support::*;
use t3_server::{source_control_bitbucket::*, source_control_host::Host, source_control_provider::*};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/source-control-bitbucket.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == kind)
        .collect()
}

#[test]
fn decodes_pull_requests_like_the_original() {
    for row in rows("pr") {
        let actual = decode_pull_request(&row["raw"]).map_or(json!({"error":true}), |request| json!({"ok":request}));
        assert_eq!(actual, row["result"], "{row}");
    }
    for row in rows("page") {
        let actual = decode_pull_request_page(&row["raw"]).map_or(json!({"error":true}), |list| json!({"ok":list}));
        assert_eq!(actual, row["result"], "{row}");
    }
}

#[test]
fn reads_repository_selectors_and_branch_names() {
    let locator = |text: &str| parse_repository_slug(text).map(|l| (l.workspace, l.repo_slug));
    assert_eq!(locator("ws/repo"), Some(("ws".into(), "repo".into())));
    assert_eq!(locator(" a/b/ws/repo.git "), Some(("ws".into(), "repo".into())));
    assert_eq!(locator("repo"), None);
    let remote = |text: &str| parse_remote_url(text).map(|l| (l.workspace, l.repo_slug));
    assert_eq!(remote("git@bitbucket.org:ws/repo.git"), Some(("ws".into(), "repo".into())));
    assert_eq!(remote("https://bitbucket.org/ws/repo.git"), Some(("ws".into(), "repo".into())));
    assert_eq!(sanitize_branch_fragment("Feature/Add  Thing!!"), "feature/add-thing");
    assert_eq!(sanitize_branch_fragment("  ../  "), "update");
    assert_eq!(sanitize_branch_fragment(&"a".repeat(80)), "a".repeat(64));
}

fn pull_request(id: u64, source: &str, source_repo: &str, state: &str) -> Value {
    json!({
        "id":id,"title":"Add Bitbucket provider","state":state,"updated_on":"2026-01-02T00:00:00.000Z",
        "links":{"html":{"href":format!("https://bitbucket.org/pingdotgg/t3code/pull-requests/{id}")}},
        "source":{"branch":{"name":source},"repository":{"full_name":source_repo,"workspace":{"slug":source_repo.split('/').next().unwrap()}}},
        "destination":{"branch":{"name":"main"},"repository":{"full_name":"pingdotgg/t3code","workspace":{"slug":"pingdotgg"}}},
    })
}

fn repository_json() -> Value {
    json!({
        "full_name":"pingdotgg/t3code",
        "links":{"html":{"href":"https://bitbucket.org/pingdotgg/t3code"},"clone":[
            {"name":"https","href":"https://bitbucket.org/pingdotgg/t3code.git"},{"name":"ssh","href":"git@bitbucket.org:pingdotgg/t3code.git"}]},
        "mainbranch":{"name":"main"},
    })
}

struct Fixture {
    server: ScriptedServer,
    test: TestHost,
    _repo: tempfile::TempDir,
    cwd: String,
}

async fn fixture(script: Vec<Step>, env: &[(&str, &str)]) -> Fixture {
    let server = ScriptedServer::start(script).await;
    let (repo, path) = repo_with_remotes(&[("origin", "git@bitbucket.org:pingdotgg/t3code.git")]);
    let base = format!("{}/2.0", server.origin);
    let mut vars = vec![("T3CODE_BITBUCKET_API_BASE_URL", base.as_str())];
    vars.extend_from_slice(env);
    let test = test_host(&vars, None, None, &path);
    Fixture { server, test, cwd: path.to_string_lossy().into_owned(), _repo: repo }
}

const EMAIL_TOKEN: [(&str, &str); 2] = [("T3CODE_BITBUCKET_EMAIL", "user@example.com"), ("T3CODE_BITBUCKET_API_TOKEN", "token")];

fn query(request: &Recorded) -> Vec<(String, String)> {
    url::form_urlencoded::parse(request.query.as_deref().unwrap_or("").as_bytes()).into_owned().collect()
}

#[tokio::test]
async fn parses_pull_requests_and_sends_the_credential() {
    let f = fixture(vec![Step::json(200, pull_request(42, "feature/source-control", "octocat/t3code", "OPEN"))], &EMAIL_TOKEN).await;
    let provider = BitbucketProvider::new(f.test.host.clone());
    let got = provider.get_change_request(GetChangeRequestInput { cwd: f.cwd.clone(), context: None, reference: "#42".into() }).await.unwrap();
    assert_eq!(
        json!(got),
        json!({
            "provider":"bitbucket","number":42,"title":"Add Bitbucket provider","url":"https://bitbucket.org/pingdotgg/t3code/pull-requests/42",
            "baseRefName":"main","headRefName":"feature/source-control","state":"open",
            "updatedAt":{"_tag":"Some","value":"2026-01-02T00:00:00.000Z"},"isCrossRepository":true,
            "headRepositoryNameWithOwner":"octocat/t3code","headRepositoryOwnerLogin":"octocat",
        })
    );
    let request = &f.server.recorded()[0];
    assert_eq!(request.path, "/2.0/repositories/pingdotgg/t3code/pullrequests/42");
    assert_eq!(request.accept.as_deref(), Some("application/json"));
    // user@example.com:token
    assert_eq!(request.authorization.as_deref(), Some("Basic dXNlckBleGFtcGxlLmNvbTp0b2tlbg=="));
}

#[tokio::test]
async fn lists_with_state_and_source_branch_params() {
    for (state, head, expected) in [
        (ListState::Merged, "origin:feature/merged", vec![("q", "source.branch.name = \"feature/merged\" AND state = \"MERGED\""), ("state", "MERGED")]),
        (
            ListState::Closed,
            "feature/closed",
            vec![("q", "source.branch.name = \"feature/closed\" AND (state = \"DECLINED\" OR state = \"SUPERSEDED\")"), ("state", "DECLINED"), ("state", "SUPERSEDED")],
        ),
        (
            ListState::All,
            "feature/all",
            vec![
                ("q", "source.branch.name = \"feature/all\" AND (state = \"OPEN\" OR state = \"MERGED\" OR state = \"DECLINED\" OR state = \"SUPERSEDED\")"),
                ("state", "OPEN"), ("state", "MERGED"), ("state", "DECLINED"), ("state", "SUPERSEDED"),
            ],
        ),
    ] {
        let f = fixture(vec![Step::json(200, json!({"values":[pull_request(7, "feature/merged", "pingdotgg/t3code", "MERGED")]}))], &EMAIL_TOKEN).await;
        let provider = BitbucketProvider::new(f.test.host.clone());
        let listed = provider
            .list_change_requests(ListChangeRequestsInput { cwd: f.cwd.clone(), context: None, source: None, head_selector: head.into(), state, limit: Some(10) })
            .await
            .unwrap();
        assert_eq!(listed[0].number, 7);
        let request = &f.server.recorded()[0];
        assert_eq!(request.path, "/2.0/repositories/pingdotgg/t3code/pullrequests");
        let mut want = vec![("pagelen".to_string(), "10".to_string()), ("sort".into(), "-updated_on".into())];
        want.extend(expected.into_iter().map(|(k, v)| (k.to_string(), v.to_string())));
        assert_eq!(query(request), want);
    }
}

#[tokio::test]
async fn reads_clone_urls_the_default_branch_and_creates_things() {
    let model = json!({"development":{"branch":{"name":"develop"},"name":"develop","use_mainbranch":false}});
    let f = fixture(vec![Step::json(200, repository_json()), Step::json(200, repository_json()), Step::json(200, model)], &EMAIL_TOKEN).await;
    let provider = BitbucketProvider::new(f.test.host.clone());
    let urls = provider
        .get_repository_clone_urls(RepositoryUrlsInput { cwd: f.cwd.clone(), context: None, repository: "pingdotgg/t3code".into() })
        .await
        .unwrap();
    assert_eq!(
        json!(urls),
        json!({"nameWithOwner":"pingdotgg/t3code","url":"https://bitbucket.org/pingdotgg/t3code.git","sshUrl":"git@bitbucket.org:pingdotgg/t3code.git"})
    );
    // The branching model's development branch wins over the main branch.
    f.server.set_script(vec![Step::json(200, repository_json()), Step::json(200, json!({"development":{"branch":{"name":"develop"},"use_mainbranch":false}}))]);
    let branch = provider.get_default_branch(DefaultBranchInput { cwd: f.cwd.clone(), context: None }).await.unwrap();
    assert_eq!(branch.as_deref(), Some("develop"));
    // A branching model that cannot be read falls back to the main branch.
    f.server.set_script(vec![Step::json(200, repository_json()), Step::json(404, json!({}))]);
    let branch = provider.get_default_branch(DefaultBranchInput { cwd: f.cwd.clone(), context: None }).await.unwrap();
    assert_eq!(branch.as_deref(), Some("main"));

    f.server.set_script(vec![Step::json(200, repository_json())]);
    provider
        .create_repository(CreateRepositoryInput { cwd: f.cwd.clone(), repository: "pingdotgg/new".into(), visibility: serde_json::from_value(json!("private")).unwrap() })
        .await
        .unwrap();
    let request = &f.server.recorded()[0];
    assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/2.0/repositories/pingdotgg/new"));
    assert_eq!(request.body, Some(json!({"scm":"git","is_private":true})));

    let body = tempfile::tempdir().unwrap();
    let body_file = body.path().join("body.md");
    std::fs::write(&body_file, "Body text").unwrap();
    f.server.set_script(vec![Step::json(201, pull_request(9, "fork-branch", "octocat/t3code", "OPEN"))]);
    provider
        .create_change_request(CreateChangeRequestInput {
            cwd: f.cwd.clone(),
            context: None,
            source: None,
            target: None,
            base_ref_name: "main".into(),
            head_selector: "octocat:fork-branch".into(),
            title: "Add it".into(),
            body_file: body_file.to_string_lossy().into_owned(),
        })
        .await
        .unwrap();
    let request = &f.server.recorded()[0];
    assert_eq!(request.path, "/2.0/repositories/pingdotgg/t3code/pullrequests");
    assert_eq!(
        request.body,
        Some(json!({
            "title":"Add it","description":"Body text",
            "source":{"branch":{"name":"fork-branch"},"repository":{"full_name":"octocat/t3code"}},
            "destination":{"branch":{"name":"main"}},
        }))
    );
}

#[tokio::test]
async fn reports_failures_with_the_original_messages() {
    let f = fixture(vec![Step::json(404, json!({"error":"nope"}))], &EMAIL_TOKEN).await;
    let provider = BitbucketProvider::new(f.test.host.clone());
    let error = provider.get_change_request(GetChangeRequestInput { cwd: f.cwd.clone(), context: None, reference: "1".into() }).await.unwrap_err();
    let wire = error.wire();
    assert_eq!(wire["detail"], "Failed to get change request.");
    assert_eq!(wire["cause"], json!({"name":"BitbucketResponseError","message":"Bitbucket API failed in getPullRequest: Bitbucket returned HTTP 404."}));
    assert!(wire.get("command").is_none());

    f.server.set_script(vec![Step { status: 200, headers: vec![], body: Some("not json".into()) }]);
    let error = provider
        .get_repository_clone_urls(RepositoryUrlsInput { cwd: f.cwd.clone(), context: None, repository: "pingdotgg/t3code".into() })
        .await
        .unwrap_err();
    assert_eq!(error.cause.unwrap().message, "Bitbucket API failed in getRepository: Bitbucket returned invalid JSON for the requested resource.");

    let error = provider
        .get_repository_clone_urls(RepositoryUrlsInput { cwd: f.cwd.clone(), context: None, repository: "justone".into() })
        .await
        .unwrap_err();
    assert_eq!(error.cause.unwrap().name, "BitbucketRepositoryLocatorError");

    // A checkout with no Bitbucket remote has nothing to read the repository from.
    let (_plain, plain_path) = repo_with_remotes(&[("origin", "git@github.com:a/b.git")]);
    let error = provider
        .get_change_request(GetChangeRequestInput { cwd: plain_path.to_string_lossy().into_owned(), context: None, reference: "1".into() })
        .await
        .unwrap_err();
    assert!(error.cause.unwrap().message.starts_with("Bitbucket API failed in resolveRepository: No Bitbucket repository remote was detected for"));
}

#[tokio::test]
async fn probes_authentication_without_a_cli() {
    use t3_server::source_control_discovery::ApiProbe;
    let f = fixture(vec![Step::json(200, json!({"username":"bitbucket-user"}))], &EMAIL_TOKEN).await;
    let probe = BitbucketProbe::new(f.test.host.clone());
    let auth = serde_json::to_value(probe.probe_auth().await).unwrap();
    assert_eq!(auth["status"], "authenticated");
    assert_eq!(auth["account"], json!({"_tag":"Some","value":"bitbucket-user"}));
    assert_eq!(auth["host"], json!({"_tag":"Some","value":"bitbucket.org"}));

    // A refused request reads as the configured credential's kind, or as signed out.
    f.server.set_script(vec![Step::json(401, json!({}))]);
    let auth = serde_json::to_value(probe.probe_auth().await).unwrap();
    assert_eq!((auth["status"].as_str(), auth["detail"].clone()), (Some("unknown"), json!({"_tag":"Some","value":"An API token is configured."})));
    assert_eq!(auth["account"], json!({"_tag":"Some","value":"user@example.com"}));
    let anonymous = fixture(vec![Step::json(401, json!({}))], &[]).await;
    let auth = serde_json::to_value(BitbucketProbe::new(anonymous.test.host.clone()).probe_auth().await).unwrap();
    assert_eq!(auth["status"], "unauthenticated");
    assert_eq!(anonymous.server.recorded()[0].authorization, None);

    // An access token wins over an email and API token, and a header-unsafe value is unset.
    let both = fixture(vec![Step::json(200, json!({}))], &[("T3CODE_BITBUCKET_ACCESS_TOKEN", "bb-token"), EMAIL_TOKEN[0], EMAIL_TOKEN[1]]).await;
    BitbucketProbe::new(both.test.host.clone()).probe_auth().await;
    assert_eq!(both.server.recorded()[0].authorization.as_deref(), Some("Bearer bb-token"));
    let unsafe_token = fixture(vec![Step::json(200, json!({}))], &[("T3CODE_BITBUCKET_ACCESS_TOKEN", "bad token\n"), EMAIL_TOKEN[0], EMAIL_TOKEN[1]]).await;
    BitbucketProbe::new(unsafe_token.test.host.clone()).probe_auth().await;
    assert!(unsafe_token.server.recorded()[0].authorization.as_deref().unwrap().starts_with("Basic "));
    let _ = Host::process_default;
}

#[tokio::test]
async fn checks_a_same_repository_pull_request_out() {
    let server = ScriptedServer::start(vec![Step::json(200, pull_request(5, "feature", "pingdotgg/t3code", "OPEN"))]).await;
    // The remote holding the branch is a local repository whose name only the pull request knows.
    let bare = tempfile::tempdir().unwrap();
    let bare_path = std::fs::canonicalize(bare.path()).unwrap();
    git(&bare_path, &["init", "--bare", "-q"]);
    let (_repo, path) = repo_with_remotes(&[("origin", bare_path.to_str().unwrap())]);
    git(&path, &["push", "-q", "origin", "HEAD:refs/heads/feature"]);
    let base = format!("{}/2.0", server.origin);
    let test = test_host(&[("T3CODE_BITBUCKET_API_BASE_URL", base.as_str())], None, None, &path);
    let context = ProviderContext {
        provider: serde_json::from_value(json!({"kind":"bitbucket","name":"Bitbucket","baseUrl":"https://bitbucket.org"})).unwrap(),
        remote_name: "origin".into(),
        remote_url: "git@bitbucket.org:pingdotgg/t3code.git".into(),
        requested_host: None,
    };
    let provider = BitbucketProvider::new(test.host.clone());
    provider
        .checkout_change_request(CheckoutChangeRequestInput { cwd: path.to_string_lossy().into_owned(), context: Some(context), reference: "5".into(), force: false })
        .await
        .unwrap();
    assert_eq!(git(&path, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature");
    assert_eq!(git(&path, &["rev-parse", "--abbrev-ref", "feature@{upstream}"]), "origin/feature");
}
