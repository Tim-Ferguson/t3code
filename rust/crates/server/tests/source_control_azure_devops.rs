//! Azure DevOps against `fixtures/source-control-azure-devops.jsonl`, recorded from the
//! original by `rust/tools/generate_source_control_azure_devops_fixtures.mjs`: pull request
//! decoding, web URL assembly, the `az account show` reader, and the provider over a scripted
//! fake `az`.
mod source_control_support;

use serde_json::{Value, json};
use source_control_support::*;
use t3_server::{source_control_azure_devops::*, source_control_discovery::AuthProbeInput};

fn rows(kind: &str) -> Vec<Value> {
    include_str!("fixtures/source-control-azure-devops.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["type"] == kind)
        .collect()
}

#[test]
fn decodes_pull_requests_like_the_original() {
    let single = rows("pr");
    assert!(single.len() > 35);
    for row in single {
        let actual = match decode_pull_request_json(row["raw"].as_str().unwrap()) {
            Ok(request) => json!({"ok":request}),
            Err(_) => json!({"error":true}),
        };
        assert_eq!(actual, row["result"], "{row}");
    }
    for row in rows("prList") {
        let actual = match decode_pull_request_list_json(row["raw"].as_str().unwrap()) {
            Ok(list) => json!({"ok":list}),
            Err(_) => json!({"error":true}),
        };
        assert_eq!(actual, row["result"], "{row}");
    }
}

#[test]
fn builds_web_urls_and_reads_the_account_like_the_original() {
    for row in rows("webUrl") {
        let input = &row["input"];
        let actual = pull_request_web_url(
            input["pullRequestId"].as_u64().unwrap(),
            input["webLink"].as_str(),
            input["repositoryWebUrl"].as_str(),
            input["restApiUrl"].as_str(),
            input["projectName"].as_str(),
            input["repositoryName"].as_str(),
        );
        assert_eq!(json!(actual), row["result"], "{row}");
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
}

#[tokio::test]
async fn runs_provider_operations_like_the_original() {
    let scenarios = rows("provider");
    assert!(scenarios.len() > 10);
    run_cli_scenarios(scenarios, "az", |host| Box::new(AzureDevOpsProvider::new(host))).await;
}
