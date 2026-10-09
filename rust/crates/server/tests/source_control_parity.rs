//! Replays `fixtures/source-control-core.jsonl`: the original host detection, identifier
//! helpers, auth-output readers and `Retry-After` parsing, run by
//! `rust/tools/generate_source_control_fixtures.mjs`. Every row must come out identical.
use serde_json::{Value, json};
use t3_server::{
    source_control_detect::*,
    source_control_discovery::*,
    source_control_rate_limit::retry_at_from_header,
};

fn rows() -> Vec<Value> {
    include_str!("fixtures/source-control-core.jsonl")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap()
}

fn optional(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

#[test]
fn replays_the_original_core_helpers() {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for (index, row) in rows().iter().enumerate() {
        let kind = text(&row["type"]);
        *counts.entry(kind.into()).or_default() += 1;
        let context = format!("row {index}: {row}");
        match kind {
            "detect" => {
                let actual = detect_provider_from_remote_url(text(&row["remoteUrl"]));
                assert_eq!(serde_json::to_value(actual).unwrap(), row["result"], "{context}");
                assert_eq!(is_ssh_remote_url(text(&row["remoteUrl"])), row["ssh"], "{context}");
            }
            "selector" => {
                let identity = (!row["identity"].is_null()).then(|| {
                    let identity = &row["identity"];
                    RepositoryIdentityParts {
                        provider: serde_json::from_value(identity["provider"].clone()).ok(),
                        display_name: optional(&identity["displayName"]),
                        owner: optional(&identity["owner"]),
                        name: optional(&identity["name"]),
                    }
                });
                assert_eq!(json!(repository_selector(identity.as_ref())), row["result"], "{context}");
            }
            "canonical" => {
                assert_eq!(json!(canonical_repository_key(text(&row["key"]))), row["result"], "{context}");
            }
            "transportSafe" => {
                assert_eq!(json!(transport_safe_value(text(&row["value"]))), row["result"], "{context}");
            }
            "ownerRef" => {
                let actual = parse_owner_ref(text(&row["headSelector"]))
                    .map(|(owner, ref_name)| json!({"owner":owner,"refName":ref_name}));
                assert_eq!(json!(actual), row["result"], "{context}");
            }
            "sourceRef" => {
                let source = (!row["source"].is_null()).then(|| RefSelector {
                    ref_name: text(&row["source"]["refName"]).into(),
                    owner: optional(&row["source"]["owner"]),
                    repository: optional(&row["source"]["repository"]),
                });
                let head = text(&row["headSelector"]);
                assert_eq!(json!(source_branch(head, source.as_ref())), row["branch"], "{context}");
                let actual = source_ref_from_input(head, source.as_ref()).map(|selector| {
                    let mut value = json!({"refName":selector.ref_name});
                    if let Some(owner) = selector.owner {
                        value["owner"] = json!(owner);
                    }
                    if let Some(repository) = selector.repository {
                        value["repository"] = json!(repository);
                    }
                    value
                });
                assert_eq!(json!(actual), row["ref"], "{context}");
            }
            "readers" => {
                let input = text(&row["text"]);
                assert_eq!(json!(first_non_empty_line(input)), row["firstNonEmptyLine"], "{context}");
                assert_eq!(json!(first_safe_auth_line(input)), row["firstSafeAuthLine"], "{context}");
                assert_eq!(json!(parse_cli_host(input)), row["parseCliHost"], "{context}");
            }
            "combined" => {
                let input = AuthProbeInput {
                    stdout: text(&row["stdout"]).into(),
                    stderr: text(&row["stderr"]).into(),
                    exit_code: 0,
                };
                assert_eq!(json!(combined_auth_output(&input)), row["result"], "{context}");
            }
            "matchFirst" => {
                let first = regex::Regex::new(r"(?iu)Logged in to .* as\s+([^\s(]+)").unwrap();
                let second = regex::Regex::new(r"(?iu)account:\s*([^\s(]+)").unwrap();
                let actual = match_first(text(&row["text"]), &[&first, &second]);
                assert_eq!(json!(actual), row["result"], "{context}");
            }
            "providerAuth" => {
                let input = &row["input"];
                let status = serde_json::from_value(input["status"].clone()).unwrap();
                let actual = provider_auth(
                    status,
                    input["account"].as_str(),
                    input["host"].as_str(),
                    input["detail"].as_str(),
                );
                assert_eq!(serde_json::to_value(actual).unwrap(), row["result"], "{context}");
            }
            "retryAt" => {
                let actual = retry_at_from_header(row["value"].as_str(), row["now"].as_i64().unwrap());
                assert_eq!(json!(actual), row["result"], "{context}");
            }
            other => panic!("unknown row type {other}"),
        }
    }
    assert_eq!(counts["detect"], 68);
    assert!(counts.values().sum::<usize>() >= 231);
}
