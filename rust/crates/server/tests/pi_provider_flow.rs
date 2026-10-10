//! A Pi instance through the real WebSocket transport: discovery, version, health, authentication state, models and their
//! thinking options, slash commands, skills, launch arguments, the update, and what the snapshot never carries. The
//! executable is a wrapper at an absolute path in the temp home around `fixtures/pi-fake-cli.py`; HOME stays in the temp
//! home, and no real `pi` runs.
#![cfg(unix)]
mod provider_manage_support;
use provider_manage_support::*;
use serde_json::{Value, json};
use std::path::Path;

const CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pi-fake-cli.py");

/// The plain wrapper, unless a test already put its own there.
fn wrapper(base: &Path) -> std::path::PathBuf {
    let path = base.join("bin/pi");
    if !path.exists() {
        write_executable(&path, &format!("#!/bin/sh\nexec python3 {CLI} \"$@\"\n"));
    }
    path
}
fn pi(base: &Path, environment: Value) -> Value {
    wrapper(base);
    let mut variables = vec![json!({"name":"HOME","value":base.join("home")}), json!({"name":"PI_FIXTURE_LOG","value":base.join("pi.log")})];
    variables.extend(environment.as_array().unwrap().iter().cloned());
    json!({"driver":"pi","enabled":true,"displayName":"Pi","config":{"enabled":true,"binaryPath":wrapper(base)},"environment":variables})
}
async fn refreshed(harness: &Harness, id: u64) -> Value {
    let mut manager = harness.connect(&harness.manager).await;
    let reply = rpc(&mut manager, id, "server.refreshProviders", json!({"instanceId":"pi"})).await;
    provider(&success(&reply)["providers"], "pi").clone()
}
fn launches(base: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(base.join("pi.log")).unwrap_or_default().lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).filter_map(|entry| entry["launch"].as_array().map(|args| args.iter().map(|arg| arg.as_str().unwrap().to_owned()).collect())).collect()
}
fn received(base: &Path) -> Vec<String> {
    std::fs::read_to_string(base.join("pi.log")).unwrap_or_default().lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).filter_map(|entry| entry["received"]["type"].as_str().map(str::to_owned)).collect()
}
fn models() -> Value {
    json!([
        {"type":"chat","provider":"fake","id":"thinker","name":"Thinker","reasoning":true,"contextWindow":100000,"thinkingLevelMap":{"off":"none","minimal":null,"low":null,"xhigh":"x"}},
        {"type":"chat","provider":"fake","id":"plain","reasoning":false},
        {"type":"chat","provider":"fake","id":"thinker","name":"Repeated"}
    ])
}
fn commands() -> Value {
    json!([
        {"name":"review","description":"Review a pull request"},
        {"name":"compact","description":"theirs"},
        {"name":"skill:deploy","source":"skill","description":"Deploy","sourceInfo":{"path":"/skills/deploy/SKILL.md","scope":"global"}}
    ])
}
fn signed_in(base: &Path) -> Value {
    pi(base, json!([{"name":"PI_FIXTURE_MODELS","value":models().to_string()},{"name":"PI_FIXTURE_COMMANDS","value":commands().to_string()}]))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_with_models_is_ready_with_its_models_thinking_levels_commands_and_skills() {
    let harness = start(|base| {
        let mut instance = signed_in(base);
        instance["config"]["customModels"] = json!([{"slug":"mine/custom","name":"Mine"}]);
        json!({"pi":instance})
    }, |_, _| {}).await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!(snapshot["driver"], "pi");
    assert_eq!((snapshot["enabled"].clone(), snapshot["installed"].clone(), snapshot["status"].clone()), (json!(true), json!(true), json!("ready")), "{snapshot}");
    assert_eq!(snapshot["version"], "1.0.0");
    assert_eq!(snapshot["auth"], json!({"status":"authenticated","type":"pi"}));
    assert_eq!(snapshot["supportedRuntimeModes"], json!(["approval-required","auto-accept-edits","full-access"]));
    assert_eq!((snapshot["showInteractionModeToggle"].clone(), snapshot["reportsContextWindow"].clone(), snapshot["requiresNewThreadForModelChange"].clone()), (json!(false), json!(true), json!(false)));
    assert_eq!(snapshot["continuation"]["groupKey"], "pi:instance:pi");
    assert!(snapshot.get("message").is_none() && snapshot.get("usageLimits").is_none(), "{snapshot}");
    // "Pi default" first, each Pi model once under its provider, then the user's custom slugs.
    let listed: Vec<(&str, &str)> = snapshot["models"].as_array().unwrap().iter().map(|model| (model["slug"].as_str().unwrap(), model["name"].as_str().unwrap())).collect();
    assert_eq!(listed, [("default", "Pi default"), ("fake/thinker", "Thinker"), ("fake/plain", "fake/plain"), ("mine/custom", "Mine")]);
    assert_eq!(snapshot["models"][1]["subProvider"], "fake");
    // The thinking ladder is what the model supports: off, then medium and high (low and minimal are null), and xhigh because it is mapped.
    let ladder = |model: &Value| model["capabilities"]["optionDescriptors"][0]["options"].as_array().map(|options| options.iter().map(|option| option["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>());
    assert_eq!(ladder(&snapshot["models"][1]), Some(["off", "medium", "high", "xhigh"].map(String::from).to_vec()));
    assert_eq!(snapshot["models"][1]["capabilities"]["optionDescriptors"][0]["options"][2]["isDefault"], true);
    assert_eq!(ladder(&snapshot["models"][2]), None);
    // /compact leads, with the hint T3 maps to the RPC call; Pi's own commands follow, and the skill is a skill.
    let names: Vec<&str> = snapshot["slashCommands"].as_array().unwrap().iter().map(|command| command["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["compact", "review"]);
    assert_eq!(snapshot["slashCommands"][0]["input"], json!({"hint":"Optional instructions"}));
    assert_eq!(snapshot["skills"], json!([{"name":"deploy","path":"/skills/deploy/SKILL.md","enabled":true,"description":"Deploy","scope":"user"}]));
    // The probe is an ephemeral RPC session with no session file and no extension, and asked for nothing but the three listings.
    let probe: Vec<Vec<String>> = launches(&harness.base).into_iter().filter(|args| args.contains(&"--no-session".to_owned())).collect();
    assert!(!probe.is_empty() && probe.iter().all(|args| args[..2] == ["--mode", "rpc"] && !args.contains(&"--extension".to_owned())), "{probe:?}");
    let mut asked = received(&harness.base);
    asked.sort();
    asked.dedup();
    assert_eq!(asked, ["get_available_models", "get_commands", "get_state"]);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_without_usable_models_is_unauthenticated_and_says_how_to_log_in() {
    let harness = start(|base| json!({"pi":pi(base, json!([{"name":"PI_FIXTURE_MODELS","value":"[]"}]))}), |_, _| {}).await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!((snapshot["status"].clone(), snapshot["installed"].clone()), (json!("warning"), json!(true)), "{snapshot}");
    assert_eq!(snapshot["auth"], json!({"status":"unauthenticated","type":"pi"}));
    assert_eq!(snapshot["message"], "Pi has no usable models. Run `pi` in a terminal and use /login, or configure an API key in ~/.pi/agent.");
    assert_eq!(snapshot["models"].as_array().unwrap().len(), 1);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pi_that_is_too_old_unreadable_or_launched_with_arguments_t3_owns_is_an_error_that_says_so() {
    let harness = start(|base| json!({
        "old":pi(base, json!([{"name":"PI_FIXTURE_VERSION","value":"0.80.4"}])),
        "unreadable":pi(base, json!([{"name":"PI_FIXTURE_VERSION","value":"unknown build"}])),
        "failing":pi(base, json!([{"name":"PI_FIXTURE_VERSION_EXIT","value":"2"}])),
        "owned":{"driver":"pi","enabled":true,"config":{"enabled":true,"binaryPath":wrapper(base),"launchArgs":"--mode json"},"environment":[]},
        "provider-only":{"driver":"pi","enabled":true,"config":{"enabled":true,"binaryPath":wrapper(base),"launchArgs":"--provider openrouter"},"environment":[]},
    }), |_, _| {}).await;
    let mut manager = harness.connect(&harness.manager).await;
    let listed = rpc(&mut manager, 1, "server.refreshProviders", json!({})).await;
    let providers = &success(&listed)["providers"];
    let message = |id: &str| (provider(providers, id)["status"].clone(), provider(providers, id)["message"].as_str().unwrap().to_owned(), provider(providers, id)["version"].clone());
    assert_eq!(message("old"), (json!("error"), "Pi 0.80.4 is unsupported. Update to Pi 0.80.5 or newer.".into(), json!("0.80.4")));
    assert_eq!(message("unreadable"), (json!("error"), "T3 Code could not determine the Pi version. Pi 0.80.5 or newer is required.".into(), Value::Null));
    assert_eq!(message("failing").1, "Pi CLI is installed but failed to run.");
    assert_eq!(message("owned").1, "Pi launch argument '--mode' is controlled by T3 Code and cannot be overridden.");
    assert_eq!(message("provider-only").1, "Pi launch argument '--provider' requires '--model'.");
    // Nothing past the version check was launched for them.
    assert!(!launches(&harness.base).iter().any(|args| args.contains(&"--no-session".to_owned())));
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_executable_is_not_installed_and_pi_is_disabled_until_the_user_opts_in() {
    // No Pi configured at all: the default instance is there, off, and nothing runs.
    let harness = start(|base| json!({"gone":{"driver":"pi","enabled":true,"config":{"enabled":true,"binaryPath":base.join("bin/does-not-exist")}}}), |_, _| {}).await;
    let mut manager = harness.connect(&harness.manager).await;
    let listed = rpc(&mut manager, 1, "server.refreshProviders", json!({})).await;
    let providers = &success(&listed)["providers"];
    let gone = provider(providers, "gone");
    assert_eq!((gone["installed"].clone(), gone["status"].clone()), (json!(false), json!("error")));
    assert_eq!(gone["message"], "Pi CLI (`pi`) is not installed or not on PATH. Install with `npm install -g @earendil-works/pi-coding-agent`.");
    let default = provider(providers, "pi");
    assert_eq!((default["enabled"].clone(), default["status"].clone(), default["installed"].clone()), (json!(false), json!("disabled"), json!(false)), "{default}");
    assert_eq!(default["message"], "Pi is disabled in T3 Code settings.");
    assert_eq!(default["models"][0]["slug"], "default");
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discovery_that_fails_leaves_pi_ready_with_the_fallback_models_and_a_message() {
    let harness = start(|base| json!({"pi":pi(base, json!([{"name":"PI_FIXTURE_DISCOVERY_FAILS","value":"1"}]))}), |_, _| {}).await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!(snapshot["status"], "ready");
    assert_eq!(snapshot["message"], "Pi is available, but T3 Code could not refresh its models and commands. The live session will retry startup.");
    assert_eq!(snapshot["auth"], json!({"status":"unknown"}));
    assert_eq!(snapshot["models"].as_array().unwrap().iter().map(|model| model["slug"].as_str().unwrap()).collect::<Vec<_>>(), ["default"]);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_in_the_environment_never_reaches_a_snapshot() {
    let secret = "sk-pi-do-not-leak-0123456789abcdef";
    let harness = start(|base| json!({"pi":pi(base, json!([{"name":"OPENROUTER_API_KEY","value":secret,"sensitive":true},{"name":"PI_FIXTURE_MODELS","value":models().to_string()}]))}), |_, _| {}).await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!(snapshot["status"], "ready");
    assert!(!snapshot.to_string().contains(secret));
    let mut reader = harness.connect(&harness.reader).await;
    let listed = rpc(&mut reader, 2, "server.refreshProviders", json!({})).await;
    assert!(!success(&listed).to_string().contains(secret));
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_is_its_own_updater_and_a_registry_release_ahead_of_it_is_an_advisory() {
    let registry = Registry::start(Some("1.2.0")).await;
    let url = registry.url.clone();
    let harness = start(
        |base| {
            let version_file = base.join("pi-version");
            std::fs::write(&version_file, "1.0.0").unwrap();
            // The updater runs with the server's environment, not the instance's (as the original's does), so the fake finds its
            // version file through the wrapper.
            write_executable(&base.join("bin/pi"), &format!("#!/bin/sh\nPI_FIXTURE_VERSION_FILE='{}' PI_FIXTURE_UPDATE_TO=1.2.0 PI_FIXTURE_LOG='{}' exec python3 {CLI} \"$@\"\n", version_file.display(), base.join("pi.log").display()));
            let mut instance = pi(base, json!([{"name":"PI_FIXTURE_MODELS","value":models().to_string()}]));
            instance["config"]["binaryPath"] = json!(base.join("bin/pi"));
            json!({"pi":instance})
        },
        move |_, management| management.configure(|options| options.registry_url = url),
    )
    .await;
    let behind = refreshed(&harness, 1).await;
    let advisory = &behind["versionAdvisory"];
    assert_eq!((advisory["status"].clone(), advisory["currentVersion"].clone(), advisory["latestVersion"].clone()), (json!("behind_latest"), json!("1.0.0"), json!("1.2.0")), "{advisory}");
    assert_eq!(advisory["canUpdate"], true);
    assert!(advisory["updateCommand"].as_str().unwrap().ends_with("bin/pi update --self"), "{advisory}");
    let mut manager = harness.connect(&harness.manager).await;
    let updated = rpc(&mut manager, 2, "server.updateProvider", json!({"provider":"pi"})).await;
    let pi = provider(&success(&updated)["providers"], "pi");
    assert_eq!(pi["updateState"]["status"], "succeeded", "{pi}");
    assert_eq!(pi["version"], "1.2.0");
    assert_eq!(pi["versionAdvisory"]["status"], "current");
    assert!(std::fs::read_to_string(harness.base.join("pi.log")).unwrap().contains("\"update\": [\"update\", \"--self\"]"));
    drop(manager);
    harness.finish().await;
}
