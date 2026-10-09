//! GitLab against `fixtures/source-control-gitlab.jsonl`, recorded from the original by
//! `rust/tools/generate_source_control_gitlab_fixtures.mjs`: merge request decoding, the
//! `glab auth status` reader, and the provider over a scripted fake `glab`.
mod source_control_support;

use serde_json::{Value, json};
use source_control_support::*;
use t3_server::{
    source_control_discovery::AuthProbeInput,
    source_control_gitlab::*,
    source_control_provider::*,
};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/source-control-gitlab.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == kind)
        .collect()
}

#[test]
fn decodes_merge_requests_like_the_original() {
    let single = rows("mr");
    assert!(single.len() > 35);
    for row in single {
        let actual = match decode_merge_request_json(row["raw"].as_str().unwrap()) {
            Ok(request) => json!({"ok":request}),
            Err(_) => json!({"error":true}),
        };
        assert_eq!(actual, row["result"], "{row}");
    }
    for row in rows("mrList") {
        let actual = match decode_merge_request_list_json(row["raw"].as_str().unwrap()) {
            Ok(list) => json!({"ok":list}),
            Err(_) => json!({"error":true}),
        };
        assert_eq!(actual, row["result"], "{row}");
    }
}

#[test]
fn reads_glab_auth_status_like_the_original() {
    for row in rows("authHosts") {
        let hosts: Vec<Value> = parse_auth_status_hosts(row["text"].as_str().unwrap())
            .into_iter()
            .map(|host| json!({"host":host.host,"account":host.account}))
            .collect();
        assert_eq!(json!(hosts), row["result"], "{row}");
    }
    let spec = cli_spec();
    for row in rows("parseAuth") {
        let input = AuthProbeInput {
            stdout: row["stdout"].as_str().unwrap().into(),
            stderr: row["stderr"].as_str().unwrap().into(),
            exit_code: row["exitCode"].as_i64().unwrap() as i32,
        };
        assert_eq!(serde_json::to_value((spec.parse_auth)(&input)).unwrap(), row["result"], "{row}");
    }
    let refine = spec.refine_unknown_remote.as_ref().unwrap();
    for row in rows("refine") {
        let context = ProviderContext {
            provider: serde_json::from_value(row["context"]["provider"].clone()).unwrap(),
            remote_name: "origin".into(),
            remote_url: row["context"]["remoteUrl"].as_str().unwrap().into(),
            requested_host: None,
        };
        let input = AuthProbeInput { stdout: row["stdout"].as_str().unwrap().into(), stderr: String::new(), exit_code: 0 };
        assert_eq!(json!(refine(&context, &input)), row["result"], "{row}");
    }
}

#[tokio::test]
async fn runs_provider_operations_like_the_original() {
    let scenarios = rows("provider");
    assert!(scenarios.len() > 14);
    run_cli_scenarios(scenarios, "glab", |host| Box::new(GitLabProvider::new(host))).await;
}
