//! Forgejo and Gitea source control against `fixtures/source-control-forgejo.jsonl`, recorded from
//! the original by `rust/tools/generate_source_control_forgejo_fixtures.mjs`: the parts of
//! `ForgejoCli.ts` that read text, where `fj` keeps its keys, the pull request schema, and the
//! provider over a fake `fj`, a fake `tea` and a loopback server standing in for the Forgejo the
//! scenario names `{origin}`. Nothing leaves loopback; HOME is a temporary directory.
mod pull_request_support;
mod source_control_support;

use pull_request_support::*;
use serde_json::{Value, json};
use source_control_support::*;
use t3_server::{
    source_control_forgejo::{ForgejoProvider, Login, host_platform, keys_paths, match_login, parse_logins, parse_remote, decode_change_request},
};


fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/source-control-forgejo.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == kind)
        .collect()
}

fn login_json(login: &Login) -> Value {
    let mut value = json!({"name": login.name, "url": login.url, "user": login.user, "default": login.default});
    if let Some(ssh_host) = &login.ssh_host {
        value["ssh_host"] = json!(ssh_host);
    }
    if let Some(valid) = &login.valid {
        value["valid"] = json!(valid);
    }
    value
}

#[test]
fn reads_logins_and_remotes_like_the_original() {
    for row in rows("logins") {
        let actual: Vec<Value> = parse_logins(row["raw"].as_str().unwrap()).iter().map(login_json).collect();
        assert_eq!(json!(actual), row["result"], "{}", row["raw"]);
    }
    let all = rows("remote");
    assert!(all.len() > 25);
    for row in all {
        let actual = parse_remote(row["value"].as_str().unwrap()).map_or(Value::Null, |remote| json!({"host": remote.host, "hostname": remote.hostname, "ssh": remote.ssh, "path": remote.path}));
        assert_eq!(actual, row["result"], "{}", row["value"]);
    }
}

#[test]
fn matches_a_login_to_a_remote_like_the_original() {
    let all = rows("match");
    assert!(all.len() > 1000, "{}", all.len());
    for row in all {
        let logins = parse_logins(&row["logins"].to_string());
        let remote = parse_remote(row["remote"].as_str().unwrap()).unwrap();
        let actual = match_login(&logins, &remote, row["requestedHost"].as_str(), row["hostOnly"] == true).map_or(Value::Null, |login| login_json(&login));
        assert_eq!(actual, row["result"], "{} {} {} {}", row["name"], row["remote"], row["requestedHost"], row["hostOnly"]);
    }
}

#[test]
fn finds_fjs_keys_like_the_original() {
    let all = rows("keys");
    assert!(all.len() >= 10);
    for row in all {
        let actual = keys_paths(row["platform"].as_str().unwrap(), row["home"].as_str().unwrap(), row["dataHome"].as_str(), row["appData"].as_str());
        assert_eq!(json!(actual), row["result"], "{} {} {} {}", row["platform"], row["home"], row["dataHome"], row["appData"]);
    }
}

#[test]
fn reads_a_pull_request_like_the_original() {
    let all = rows("changeRequest");
    assert!(all.len() > 25);
    for row in all {
        let value: Value = serde_json::from_str(row["value"].as_str().unwrap()).unwrap();
        let actual = match decode_change_request(&value) {
            Some(request) => json!({"ok": request}),
            None => json!({"error": true}),
        };
        assert_eq!(actual, row["result"], "{}", row["value"]);
    }
}

// ---- the provider -------------------------------------------------------------------------------------

#[tokio::test]
async fn runs_provider_operations_like_the_original() {
    let scenarios = rows("provider");
    assert!(scenarios.len() > 20, "{}", scenarios.len());
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap();
        let http: Vec<Value> = scenario["http"].as_array().unwrap().clone();
        let server = RouteServer::start_at(&http, "").await;
        let (origin, host) = (server.base.clone(), server.base.trim_start_matches("http://").to_string());
        let sub = |text: &str| text.replace("{origin}", &origin).replace("{host}", &host);

        let home = tempfile::tempdir().unwrap();
        let home_path = std::fs::canonicalize(home.path()).unwrap().to_string_lossy().into_owned();
        // fj's keys, where this platform keeps them
        let keys = match &scenario["keys"] {
            Value::Null => None,
            Value::String(text) => Some(text.clone()),
            keys => Some(sub(&keys.to_string())),
        };
        if let Some(keys) = keys {
            let path = keys_paths(host_platform(), &home_path, None, None).remove(0);
            std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
            std::fs::write(path, keys).unwrap();
        }

        // the checkout: its origin is whatever the scenario's git answers for `remote get-url origin`
        let remote = scenario["processes"].as_array().unwrap().iter().find(|route| route["command"] == "git" && route["match"] == "get-url origin").map(|route| sub(route["stdout"].as_str().unwrap().trim()));
        let (_repo, repo_path) = repo_with_remotes(&remote.iter().map(|url| ("origin", url.as_str())).collect::<Vec<_>>());
        let cwd = repo_path.to_string_lossy().into_owned();
        // The commit the scenario's pull request is at: one the checkout really has.
        let head = git(&repo_path, &["rev-parse", "HEAD"]);
        let sub = |text: &str| text.replace("{origin}", &origin).replace("{host}", &host).replace("{head}", &head);
        let back = |text: String| text.replace(&origin, "{origin}").replace(&host, "{host}").replace(&head, "{head}");

        let dir = FakeCliDir::new();
        let mut tools: Vec<&str> = vec![];
        for tool in ["fj", "tea"] {
            let routes: Vec<Value> = scenario["processes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|route| route["command"] == tool)
                .map(|route| json!({"match": route["match"].as_str().map_or(String::new(), sub), "stdout": route["stdout"].as_str().map(sub), "stderr": route["stderr"].as_str().map(sub), "code": route["code"], "spawn": route["spawn"]}))
                .collect();
            if !routes.is_empty() {
                install_routed(&dir, tool, &routes, ["unauthorized", "HTTP 429", "404"]);
                tools.push(tool);
            }
        }
        let test = test_host(&[("HOME", home_path.as_str())], Some(dir.path()), None, &repo_path);
        let provider = ForgejoProvider::new(test.host.clone());
        let body_dir = tempfile::tempdir().unwrap();
        let body_file = body_dir.path().join("body.md");
        std::fs::write(&body_file, "Body from file\n").unwrap();
        let body_file = body_file.to_string_lossy().into_owned();

        let mut authorized = 0;
        for (index, call) in scenario["calls"].as_array().unwrap().iter().enumerate() {
            let input: Value = serde_json::from_str(&sub(&call["input"].to_string())).unwrap();
            let context: Value = if call["context"].is_object() { serde_json::from_str(&sub(&call["context"].to_string())).unwrap() } else { Value::Null };
            let outcome = run_call(&provider, &json!({"op": call["op"], "input": input, "context": context}), &cwd, &body_file).await;
            let outcome = outcome.map(|value| serde_json::from_str::<Value>(&back(value.to_string())).unwrap());
            check(name, index, &outcome, &scenario["results"][index], &cwd);

            // what the tools were asked, in order; git is the real one and is not recorded
            let seen: Vec<Value> = take_calls(&dir).into_iter().map(|call| serde_json::from_str::<Value>(&back(call.to_string())).unwrap()).collect();
            let expected: Vec<Value> = scenario["log"][index]["processes"].as_array().unwrap().iter().filter(|process| process["command"] != "git").map(|process| json!({"args": process["args"], "stdin": process["stdin"]})).collect();
            assert_eq!(seen, expected, "{name} #{index}: tool calls");

            // what the server was asked, and with which credential
            let requests = server.take();
            let wanted = scenario["log"][index]["requests"].as_array().unwrap();
            let got: Vec<Value> = requests.iter().map(|request| json!({"method": request["method"], "path": request["url"], "body": request["body"]})).collect();
            let expected: Vec<Value> = wanted.iter().map(|request| json!({"method": request["method"], "path": request["path"], "body": request["body"]})).collect();
            assert_eq!(got, expected, "{name} #{index}: requests");
            let credentials = server.authorizations();
            for (request, (_, header)) in wanted.iter().zip(&credentials[authorized..]) {
                assert_eq!(header.as_deref(), request["authorization"].as_str(), "{name} #{index}: credential");
            }
            authorized = credentials.len();
        }
        drop(tools);
    }
}
