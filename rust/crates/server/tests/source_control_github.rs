//! GitHub against `fixtures/source-control-github.jsonl`, recorded from the original by
//! `rust/tools/generate_source_control_github_fixtures.mjs`: the pure readers, every API
//! answer the original classifies, and the provider operations with the requests they send.
//! A scripted local server stands in for GitHub and a throwaway repository for the checkout.
mod source_control_support;

use serde_json::{Value, json};
use source_control_support::*;
use t3_contracts::{GitHubSettings, SourceControlProviderKind};
use t3_server::{
    source_control_discovery::AuthProbeInput,
    source_control_github_api::{self as api, GraphQlInput, Method, RestInput},
    source_control_github_auth::*,
    source_control_github_cli::*,
    source_control_github_credentials::{GitHubCredentialError, environment_token},
    source_control_github_prs::decode_pull_requests,
    source_control_host::HostEnv,
    source_control_provider::*,
};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/source-control-github.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == kind)
        .collect()
}

fn settings_of(value: &Value) -> GitHubSettings {
    serde_json::from_value(value.clone()).unwrap()
}

fn account_json(account: &GitHubAuthStatusAccount) -> Value {
    json!({
        "host":account.host,"account":account.account,"authenticated":account.authenticated,
        "active":account.active,"error":account.error,"environmentVariable":account.environment_variable,
    })
}

#[test]
fn reads_auth_status_and_chooses_accounts_like_the_original() {
    for row in rows("authStatus") {
        let status = parse_github_auth_status(row["text"].as_str().unwrap());
        let actual = json!({"parsed":status.parsed,"accounts":status.accounts.iter().map(account_json).collect::<Vec<_>>()});
        assert_eq!(actual, row["result"], "{row}");
    }
    let parse_rows = rows("parseAuth");
    assert!(parse_rows.len() > 100);
    for row in &parse_rows {
        let input = AuthProbeInput {
            stdout: row["stdout"].as_str().unwrap().into(),
            stderr: row["stderr"].as_str().unwrap().into(),
            exit_code: row["exitCode"].as_i64().unwrap() as i32,
        };
        let actual = serde_json::to_value(parse_github_auth(&input, &settings_of(&row["settings"]))).unwrap();
        assert_eq!(actual, row["result"], "{row}");
    }
    for row in rows("effective") {
        let status = parse_github_auth_status(row["text"].as_str().unwrap());
        let chosen = effective_account(row["host"].as_str().unwrap(), &status.accounts, &settings_of(&row["settings"]));
        assert_eq!(json!(chosen.map(account_json)), row["result"], "{row}");
    }
}

#[test]
fn reads_environment_tokens_and_api_urls_like_the_original() {
    for row in rows("envToken") {
        let env: Vec<(String, String)> = row["env"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().into())).collect();
        let env = HostEnv::from_pairs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let actual = environment_token(row["host"].as_str().unwrap(), &env);
        assert_eq!(json!(actual), row["result"], "{row}");
    }
    for row in rows("apiUrls") {
        let (rest, graphql) = api::github_api_urls(row["host"].as_str().unwrap());
        assert_eq!(json!({"rest":rest,"graphql":graphql}), row["result"], "{row}");
    }
}

#[test]
fn selects_repositories_and_names_branches_like_the_original() {
    for row in rows("baseRepository") {
        let input = &row["input"];
        let actual = select_base_repository(
            input["remotes"].as_str().unwrap(),
            input["resolved"].as_str().unwrap(),
            input["host"].as_str().unwrap(),
        )
        .map(|(owner, name)| json!({"owner":owner,"name":name}));
        assert_eq!(json!(actual), row["result"], "{row}");
    }
    for row in rows("apiHostForRemote") {
        assert_eq!(json!(github_api_host_for_remote(row["url"].as_str().unwrap())), row["result"], "{row}");
    }
    for row in rows("checkoutBranch") {
        let input = &row["input"];
        let actual = checkout_branch_name(
            input["headRefName"].as_str().unwrap(),
            input["headOwner"].as_str(),
            input["isCrossRepository"].as_bool().unwrap(),
            input["defaultBranch"].as_str(),
        );
        assert_eq!(json!(actual), row["result"], "{row}");
    }
}

#[test]
fn decodes_pull_request_rows_like_the_original() {
    let all = rows("pullRequests");
    assert!(all.len() > 20);
    for row in all {
        let actual = decode_pull_requests(row["entries"].as_array().unwrap());
        assert_eq!(serde_json::to_value(actual).unwrap(), row["result"], "{row}");
    }
}

fn api_input(call: &Value) -> (String, Value) {
    (call["kind"].as_str().unwrap().to_string(), call["input"].clone())
}

fn method_of(name: Option<&str>) -> Method {
    match name {
        Some("POST") => Method::Post,
        Some("PUT") => Method::Put,
        Some("PATCH") => Method::Patch,
        Some("DELETE") => Method::Delete,
        _ => Method::Get,
    }
}

/// A fake `gh auth token` that hands out `first`, then `second` once the server has refused
/// a token (it appends a line to `refusals` for every 401 it serves).
const ROTATING_TOKEN: &str = r#"
if [ "$1 $2" = "auth token" ]; then
  d="${0%/*}"; n=0
  if [ -f "$d/refusals" ]; then while read -r _; do n=$((n+1)); done < "$d/refusals"; fi
  if [ $n -eq 0 ]; then printf first; else printf second; fi
  exit 0
fi
exit 1
"#;

#[tokio::test]
async fn classifies_every_api_answer_like_the_original() {
    let scenarios = rows("api");
    assert!(scenarios.len() > 40);
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let credential_error = scenario["credentialError"].as_str();
        // A host turned off in Settings and a gh that fails are covered by the credential tests.
        if matches!(credential_error, Some("GitHubHostDisabledError" | "GitHubCliFailedError")) {
            continue;
        }
        let server = ScriptedServer::start(scenario["script"].as_array().unwrap().iter().map(Step::from_fixture).collect()).await;
        let cli = FakeCliDir::new();
        server.log_refusals_to(cli.dir.path().join("refusals"));
        match credential_error {
            Some("GitHubCliMissingError") => {}
            Some(_) => cli.install("gh", "echo 'no oauth token found' >&2; exit 1"),
            None => cli.install("gh", ROTATING_TOKEN),
        }
        let work = tempfile::tempdir().unwrap();
        let test = test_host(&[], Some(cli.path()), Some(server.github_resolver()), work.path());
        let mut results = Vec::new();
        for call in scenario["calls"].as_array().unwrap() {
            let (kind, input) = api_input(call);
            let interactive = call["interactive"].as_bool().unwrap_or(false);
            let host = test.host.clone();
            let run = async move {
                if kind == "graphql" {
                    api::graphql(
                        &host,
                        GraphQlInput {
                            host: input["host"].as_str().unwrap().into(),
                            operation: input["operation"].as_str().unwrap().into(),
                            query: input["query"].as_str().unwrap().into(),
                            max_response_bytes: input["maxResponseBytes"].as_u64().map(|n| n as usize),
                            ..GraphQlInput::default()
                        },
                    )
                    .await
                    .map(|body| json!({"body":body}))
                } else {
                    api::rest(
                        &host,
                        RestInput {
                            host: input["host"].as_str().unwrap().into(),
                            operation: input["operation"].as_str().unwrap().into(),
                            method: method_of(input["method"].as_str()),
                            path: input["path"].as_str().unwrap().into(),
                            body: input.get("body").cloned(),
                            accept: input["accept"].as_str().map(str::to_string),
                            if_none_match: input["ifNoneMatch"].as_str().map(str::to_string),
                            ..RestInput::default()
                        },
                    )
                    .await
                    .map(|response| json!({"status":response.status,"body":response.body,"truncated":response.truncated}))
                }
            };
            let outcome = if interactive { api::with_allow_reserve(run).await } else { run.await };
            results.push(match outcome {
                Ok(ok) => json!({"ok":ok}),
                Err(error) => {
                    let mut value = json!({"_tag":error.tag(),"message":error.message()});
                    match &error {
                        api::GitHubApiError::RateLimit { retry_at: Some(at), .. } => value["retryAt"] = json!(at),
                        api::GitHubApiError::Paused(paused) => value["retryAt"] = json!(paused.retry_at),
                        api::GitHubApiError::Response { status, .. } => value["status"] = json!(status),
                        _ => {}
                    }
                    json!({"error":value})
                }
            });
        }
        assert_eq!(json!(results), scenario["results"], "{name}");
        let requests: Vec<Value> = server
            .recorded()
            .iter()
            .map(|request| {
                json!({
                    "method":request.method,"url":ScriptedServer::original_url(request),
                    "authorization":request.authorization,"accept":request.accept,
                    "ifNoneMatch":request.if_none_match,"body":request.body,
                })
            })
            .collect();
        assert_eq!(json!(requests), scenario["requests"], "{name}");
        // The token is asked for once, and again only for a call that follows a refusal.
        let refusals = scenario["invalidations"].as_u64().unwrap() as usize;
        let calls = scenario["calls"].as_array().unwrap().len();
        let expected_asks = if credential_error == Some("GitHubCliMissingError") { 0 } else { 1 + refusals.min(calls - 1) };
        // (One token per host: the call that names three hosts asks for three.)
        if name != "rest ghe host urls" {
            assert_eq!(cli.calls("gh").len(), expected_asks, "{name}: gh asked again only after a refusal");
        }
    }
}

fn remotes_of(text: &str) -> Vec<(String, String)> {
    let mut remotes: Vec<(String, String)> = Vec::new();
    for line in text.lines().filter(|line| line.ends_with("(fetch)")) {
        let (name, rest) = line.split_once('\t').unwrap();
        remotes.push((name.into(), rest.trim_end_matches("(fetch)").trim().into()));
    }
    remotes
}

fn refs(value: &Value) -> Option<RefSelector> {
    value.as_object().map(|object| RefSelector {
        ref_name: object["refName"].as_str().unwrap().into(),
        owner: object.get("owner").and_then(Value::as_str).map(str::to_string),
        repository: object.get("repository").and_then(Value::as_str).map(str::to_string),
    })
}

fn context_of(value: &Value) -> Option<ProviderContext> {
    value.as_object().map(|_| ProviderContext {
        provider: serde_json::from_value(value["provider"].clone()).unwrap(),
        remote_name: value["remoteName"].as_str().unwrap().into(),
        remote_url: value["remoteUrl"].as_str().unwrap().into(),
        requested_host: None,
    })
}

/// The comparable part of a provider error: everything that reaches the wire except causes
/// deeper than the first, whose schema-issue contents the original serializes wholesale.
fn error_summary(error: &Value, cwd: &str) -> Value {
    let text = |name: &str| error[name].as_str().map(|text| text.replace(cwd, "/repo"));
    json!({
        "provider":error["provider"],"operation":error["operation"],"cwd":text("cwd"),"command":error["command"],
        "reference":error["reference"],"repository":error["repository"],"detail":text("detail"),
        "cause":{"name":error["cause"]["name"],"message":error["cause"]["message"].as_str().map(|text| text.replace(cwd, "/repo"))},
    })
}

#[tokio::test]
async fn runs_provider_operations_like_the_original() {
    let scenarios = rows("provider");
    assert!(scenarios.len() > 35);
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let server = ScriptedServer::start(scenario["script"].as_array().unwrap().iter().map(Step::from_fixture).collect()).await;
        let cli = FakeCliDir::new();
        server.log_refusals_to(cli.dir.path().join("refusals"));
        cli.install("gh", ROTATING_TOKEN);
        let remotes = remotes_of(scenario["remotes"].as_str().unwrap());
        let (_repo, repo_path) = repo_with_remotes(&remotes.iter().map(|(n, u)| (n.as_str(), u.as_str())).collect::<Vec<_>>());
        if let Some(resolved) = scenario["resolved"].as_str().filter(|text| !text.is_empty()) {
            for line in resolved.lines() {
                let (key, value) = line.split_once(' ').unwrap();
                git(&repo_path, &["config", key, value]);
            }
        }
        let env: Vec<(String, String)> = scenario["env"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().into())).collect();
        let test = test_host(
            &env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>(),
            Some(cli.path()),
            Some(server.github_resolver()),
            &repo_path,
        );
        let body_dir = tempfile::tempdir().unwrap();
        let body_file = match &scenario["bodyFile"] {
            Value::String(text) if text != "none" => {
                let path = body_dir.path().join("body.md");
                std::fs::write(&path, text).unwrap();
                path.to_string_lossy().into_owned()
            }
            Value::Null => body_dir.path().join("missing.md").to_string_lossy().into_owned(),
            _ => String::new(),
        };
        let provider = t3_server::source_control_github_provider::GitHubProvider::new(test.host.clone());
        let cwd = repo_path.to_string_lossy().into_owned();
        let mut results = Vec::new();
        for call in scenario["calls"].as_array().unwrap() {
            let input = &call["input"];
            let context = context_of(&call["context"]);
            let state = |text: &str| match text {
                "open" => ListState::Open,
                "closed" => ListState::Closed,
                "merged" => ListState::Merged,
                _ => ListState::All,
            };
            let outcome: Result<Value, t3_contracts::SourceControlProviderError> = match call["op"].as_str().unwrap() {
                "list" => provider
                    .list_change_requests(ListChangeRequestsInput {
                        cwd: cwd.clone(),
                        context,
                        source: refs(&input["source"]),
                        head_selector: input["headSelector"].as_str().unwrap().into(),
                        state: state(input["state"].as_str().unwrap()),
                        limit: input["limit"].as_f64().map(|limit| limit.trunc() as u32),
                    })
                    .await
                    .map(|items| serde_json::to_value(items).unwrap()),
                "get" => provider
                    .get_change_request(GetChangeRequestInput { cwd: cwd.clone(), context, reference: input["reference"].as_str().unwrap().into() })
                    .await
                    .map(|item| serde_json::to_value(item).unwrap()),
                "cloneUrls" => provider
                    .get_repository_clone_urls(RepositoryUrlsInput { cwd: cwd.clone(), context, repository: input["repository"].as_str().unwrap().into() })
                    .await
                    .map(|urls| serde_json::to_value(urls).unwrap()),
                "createRepo" => provider
                    .create_repository(CreateRepositoryInput {
                        cwd: cwd.clone(),
                        repository: input["repository"].as_str().unwrap().into(),
                        visibility: serde_json::from_value(input["visibility"].clone()).unwrap(),
                    })
                    .await
                    .map(|urls| serde_json::to_value(urls).unwrap()),
                "createPr" => provider
                    .create_change_request(CreateChangeRequestInput {
                        cwd: cwd.clone(),
                        context,
                        source: None,
                        target: None,
                        base_ref_name: input["baseRefName"].as_str().unwrap().into(),
                        head_selector: input["headSelector"].as_str().unwrap().into(),
                        title: input["title"].as_str().unwrap().into(),
                        body_file: body_file.clone(),
                    })
                    .await
                    .map(|()| Value::Null),
                _ => provider
                    .get_default_branch(DefaultBranchInput { cwd: cwd.clone(), context })
                    .await
                    .map(|branch| json!(branch)),
            };
            results.push(outcome);
        }
        let expected = scenario["results"].as_array().unwrap();
        assert_eq!(results.len(), expected.len(), "{name}");
        for (index, (actual, expected)) in results.iter().zip(expected).enumerate() {
            let context = format!("{name} #{index}");
            match (actual, expected.get("ok"), expected.get("error")) {
                (Ok(actual), Some(ok), None) => assert_eq!(actual, ok, "{context}"),
                (Err(actual), None, Some(error)) => {
                    let wire = actual.wire();
                    let mut got = error_summary(&wire, &cwd);
                    let mut want = error_summary(error, "/never/present");
                    // The text of an operating system failure is the system's own.
                    if name == "create pull request missing body file" {
                        for summary in [&mut got, &mut want] {
                            summary["detail"] = json!("os");
                            summary["cause"]["message"] = json!("os");
                        }
                    }
                    assert_eq!(got, want, "{context}");
                    assert_eq!(wire["_tag"], error["_tag"], "{context}");
                }
                other => panic!("{context}: expected {expected}, got {other:?}"),
            }
        }
        let requests: Vec<Value> = server
            .recorded()
            .iter()
            .map(|request| {
                let mut value = json!({
                    "method":request.method,"url":ScriptedServer::original_url(request),
                    "authorization":request.authorization,"accept":request.accept,
                    "ifNoneMatch":request.if_none_match,"body":request.body,
                });
                if let Some(text) = value["body"]["body"].as_str() {
                    value["body"]["body"] = json!(text);
                }
                value
            })
            .collect();
        assert_eq!(json!(requests), scenario["requests"], "{name}");
    }
}

#[test]
fn messages_credential_failures_like_the_original() {
    let cases = [
        (GitHubCredentialError::CliMissing { host: "github.com".into() }, "No GitHub credential for github.com: set GH_TOKEN, or install the GitHub CLI and run `gh auth login`."),
        (GitHubCredentialError::NotSignedIn { host: "github.com".into(), account: None }, "No GitHub credential for github.com: run `gh auth login --hostname github.com`."),
        (GitHubCredentialError::HostDisabled { host: "github.com".into() }, "GitHub host github.com is turned off in Settings → Source Control."),
        (GitHubCredentialError::CliFailed { host: "github.com".into(), cause: "boom".into() }, "The GitHub CLI could not hand over a credential for github.com. Check `gh auth status` on the server."),
    ];
    for (error, message) in cases {
        assert_eq!(error.message(), message);
    }
    let _ = SourceControlProviderKind::Github;
}
