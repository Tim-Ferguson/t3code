//! One-shot text generation through Pi (`PiTextGeneration.ts`): an ephemeral `pi --mode rpc --no-session` with extensions and tools
//! off, the model set when one is named, the last assistant text read back and decoded. The executable is the fake `pi`
//! (`fixtures/pi-fake-cli.py`) behind a wrapper at an absolute path in the temp home; no real `pi` runs.
#![cfg(unix)]
mod provider_manage_support;
use provider_manage_support::*;
use serde_json::{Value, json};
use std::path::Path;
use t3_server::{
    text_generation::{Operation, Request, Runner, Shape},
    text_generation_run::CliRunner,
};

const CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pi-fake-cli.py");

fn pi(base: &Path, id: &str, reply: &str, launch_args: &str) -> Value {
    let wrapper = base.join("bin/pi");
    if !wrapper.exists() {
        write_executable(&wrapper, &format!("#!/bin/sh\nexec python3 {CLI} \"$@\"\n"));
    }
    json!({"driver":"pi","enabled":true,"displayName":"Pi","config":{"enabled":true,"binaryPath":wrapper,"launchArgs":launch_args},
        "environment":[{"name":"HOME","value":base.join("home")},{"name":"PI_FIXTURE_LOG","value":base.join(format!("{id}.log"))},{"name":"PI_FIXTURE_REPLY","value":reply}]})
}
fn log(base: &Path, id: &str) -> Vec<Value> {
    std::fs::read_to_string(base.join(format!("{id}.log"))).unwrap_or_default().lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}
fn request(instance: &str, model: &str) -> Request {
    Request { operation: Operation::ThreadTitle, cwd: std::env::temp_dir().to_string_lossy().into_owned(), prompt: "Name this thread".into(), shape: Shape::ThreadTitle, model_selection: json!({"instanceId":instance,"model":model}), attachments: vec![] }
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_writes_text_through_an_ephemeral_session_with_extensions_and_tools_off() {
    let harness = start(
        |base| {
            json!({
                "piGood": pi(base, "piGood", "Sure. ```json\n{\"title\":\"A tidy title\"}\n```", "--tools read,bash --extension /mine/ext.ts --thinking low"),
                "piBad": pi(base, "piBad", "no object here", ""),
                "piArgs": pi(base, "piArgs", "{\"title\":\"x\"}", "--provider only"),
            })
        },
        |_, _| {},
    )
    .await;
    let runner = CliRunner::for_registry(harness.registry.clone(), None);

    // The default model: no model is set; the reply's object is found inside its prose and fence.
    let answer = runner.run(request("piGood", "default")).await.unwrap();
    assert_eq!(answer["title"], "A tidy title");
    let entries = log(&harness.base, "piGood");
    // The last launch: the instance's own start-up discovery came before it.
    let launch: Vec<String> = entries.iter().rev().find_map(|entry| entry["launch"].as_array()).unwrap().iter().map(|arg| arg.as_str().unwrap().to_owned()).collect();
    assert_eq!(&launch[..3], ["--mode", "rpc", "--no-session"]);
    assert!(launch.contains(&"--no-extensions".to_owned()) && launch.contains(&"--no-tools".to_owned()));
    assert!(!launch.iter().any(|arg| arg == "--tools" || arg == "--extension" || arg == "/mine/ext.ts"), "{launch:?}");
    assert_eq!(launch.iter().skip_while(|arg| *arg != "--thinking").take(2).collect::<Vec<_>>(), ["--thinking", "low"], "other launch arguments are kept");
    let last_launch = entries.iter().rposition(|entry| entry.get("launch").is_some()).unwrap();
    let received: Vec<&str> = entries[last_launch..].iter().filter_map(|entry| entry["received"]["type"].as_str()).collect();
    assert_eq!(received, ["prompt", "get_last_assistant_text"]);

    // A named model is set first; a slug without a provider is refused rather than skipped.
    runner.run(request("piGood", "fake/plain")).await.unwrap();
    let received: Vec<Value> = log(&harness.base, "piGood").into_iter().filter_map(|entry| entry.get("received").cloned()).collect();
    assert!(received.iter().any(|record| record["type"] == "set_model" && record["provider"] == "fake" && record["modelId"] == "plain"), "{received:?}");
    let refused = runner.run(request("piGood", "plain")).await.unwrap_err();
    assert_eq!(refused.to_string().contains("Pi model 'plain' must use provider/model format."), true, "{refused}");

    // A reply with no object, and launch arguments the original refuses.
    let invalid = runner.run(request("piBad", "default")).await.unwrap_err();
    assert!(invalid.to_string().contains("Pi returned invalid structured output."), "{invalid}");
    let args = runner.run(request("piArgs", "default")).await.unwrap_err();
    assert!(args.to_string().contains("Pi launch argument '--provider' requires '--model'."), "{args}");
    harness.finish().await;
}

#[test]
fn a_json_object_is_found_inside_a_reply_as_the_original_finds_it() {
    use t3_server::text_generation_run::extract_json_object;
    for (raw, expected) in [
        ("  {\"a\":1}  ", "{\"a\":1}"),
        ("Here it is: {\"a\":{\"b\":\"}\"}} trailing", "{\"a\":{\"b\":\"}\"}}"),
        ("```json\n{\"a\":\"quote \\\" brace }\"}\n```", "{\"a\":\"quote \\\" brace }\"}"),
        ("no braces", "no braces"),
        ("   ", ""),
        ("prefix {\"open\": 1", "{\"open\": 1"),
        ("{\"a\":1}{\"b\":2}", "{\"a\":1}"),
    ] {
        assert_eq!(extract_json_object(raw), expected, "{raw}");
    }
}
