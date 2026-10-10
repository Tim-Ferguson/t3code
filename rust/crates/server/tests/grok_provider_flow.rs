//! A Grok instance through the real WebSocket transport: discovery, version, health, authentication
//! state, models, slash commands, skills and usage, and what the snapshot never carries. The executable is
//! a wrapper at an absolute path in the temp home around `fixtures/grok-fake-cli.py`; HOME and GROK_HOME
//! stay in the temp home, and no real `grok` runs.
#![cfg(unix)]
mod provider_manage_support;
use provider_manage_support::*;
use serde_json::{Value, json};
use std::path::Path;

const CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/grok-fake-cli.py");

fn wrapper(base: &Path) -> std::path::PathBuf {
    let path = base.join("bin/grok");
    write_executable(&path, &format!("#!/bin/sh\nexec python3 {CLI} \"$@\"\n"));
    path
}
fn grok(base: &Path, environment: Value) -> Value {
    let mut variables = vec![json!({"name":"HOME","value":base.join("home")}), json!({"name":"GROK_HOME","value":base.join("home/.grok")}), json!({"name":"GROK_FIXTURE_ARGS_LOG","value":base.join("args.log")})];
    variables.extend(environment.as_array().unwrap().iter().cloned());
    json!({"driver":"grok","enabled":true,"displayName":"Grok","config":{"enabled":true,"binaryPath":wrapper(base)},"environment":variables})
}
async fn refreshed(harness: &Harness, id: u64) -> Value {
    let mut manager = harness.connect(&harness.manager).await;
    let reply = rpc(&mut manager, id, "server.refreshProviders", json!({"instanceId":"grok"})).await;
    provider(&success(&reply)["providers"], "grok").clone()
}
fn launches(base: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(base.join("args.log")).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_logged_in_grok_is_ready_with_its_models_commands_skills_and_an_honest_account() {
    let harness = start(|base| json!({"grok":grok(base, json!([]))}), |_, _| {}).await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!(snapshot["driver"], "grok");
    assert_eq!((snapshot["enabled"].clone(), snapshot["installed"].clone(), snapshot["status"].clone()), (json!(true), json!(true), json!("ready")), "{snapshot}");
    assert_eq!(snapshot["version"], "1.0.41");
    assert_eq!(snapshot["auth"], json!({"status":"authenticated","type":"cached_token","label":"Grok account"}));
    assert_eq!(snapshot["supportedRuntimeModes"], json!(["approval-required","auto","full-access"]));
    assert_eq!((snapshot["showInteractionModeToggle"].clone(), snapshot["supportsConversationRollback"].clone()), (json!(false), json!(false)));
    assert_eq!(snapshot["continuation"]["groupKey"], "grok:instance:grok");
    // The models are the ACP probe's, the agent's current one the default, with the reasoning options it advertises.
    let models = snapshot["models"].as_array().unwrap();
    assert_eq!(models.iter().map(|model| model["slug"].as_str().unwrap()).collect::<Vec<_>>(), ["grok-4.7", "grok-4.6"]);
    assert_eq!(models[0]["isDefault"], true);
    assert_eq!(models[0]["capabilities"]["optionDescriptors"][0]["id"], "reasoningEffort");
    assert_eq!(models[0]["capabilities"]["optionDescriptors"][0]["currentValue"], "high");
    assert_eq!(snapshot["slashCommands"].as_array().unwrap().iter().map(|command| command["name"].as_str().unwrap()).collect::<Vec<_>>(), ["compact", "init"]);
    assert_eq!(snapshot["slashCommands"][0]["input"], json!({"hint":"what to keep"}));
    assert_eq!(snapshot["skills"].as_array().unwrap().iter().map(|skill| skill["name"].as_str().unwrap()).collect::<Vec<_>>(), ["here", "review"]);
    // No login file means no account to read usage for: the snapshot says so rather than inventing a number.
    assert_eq!(snapshot["usageLimits"]["unavailable"]["reason"], "unsupported");
    assert!(snapshot["auth"].get("email").is_none());
    // The probe launched the CLI the way T3 launches it for metadata: no session flags.
    assert!(launches(&harness.base).contains(&vec!["agent".to_owned(), "stdio".to_owned()]));
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_logged_out_grok_reports_the_login_command_and_still_lists_its_models() {
    let harness = start(|base| json!({"grok":grok(base, json!([{"name":"GROK_FIXTURE_LOGGED_OUT","value":"1"}]))}), |_, _| {}).await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!(snapshot["status"], "error");
    assert_eq!(snapshot["auth"], json!({"status":"unauthenticated"}));
    assert_eq!(snapshot["message"], "Grok CLI is installed but not logged in. Run `grok login`.");
    assert_eq!(snapshot["installed"], true);
    assert_eq!(snapshot["models"][0]["slug"], "grok-4.7");
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_api_key_is_a_login_whose_value_never_reaches_the_snapshot() {
    let secret = "xai-do-not-leak-0123456789abcdef";
    let harness = start(|base| json!({"grok":grok(base, json!([{"name":"XAI_API_KEY","value":secret,"sensitive":true},{"name":"GROK_FIXTURE_LOGGED_OUT","value":"1"}]))}), |_, _| {}).await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!(snapshot["auth"], json!({"status":"authenticated","type":"api_key","label":"xAI API key"}));
    assert_eq!(snapshot["status"], "ready");
    assert!(!snapshot.to_string().contains(secret));
    let mut reader = harness.connect(&harness.reader).await;
    let listed = rpc(&mut reader, 2, "server.refreshProviders", json!({})).await;
    assert!(!success(&listed).to_string().contains(secret));
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_metadata_probe_degrades_the_model_picker_not_the_provider() {
    let harness = start(|base| json!({"grok":grok(base, json!([{"name":"GROK_FIXTURE_PROBE_FAILS","value":"1"}]))}), |_, _| {}).await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!(snapshot["status"], "warning");
    assert_eq!(snapshot["message"], "Grok CLI is installed but ACP initialize failed. Model options may be incomplete.");
    // The models fall back to what `grok models` lists, and the commands to /compact alone.
    assert_eq!(snapshot["models"].as_array().unwrap().iter().map(|model| model["slug"].as_str().unwrap()).collect::<Vec<_>>(), ["grok-4.7", "grok-4.6"]);
    assert_eq!(snapshot["slashCommands"], json!([{"name":"compact","description":"Summarize the conversation and reduce context usage"}]));
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_executable_is_not_installed_and_a_disabled_instance_launches_nothing() {
    let harness = start(
        |base| {
            let mut missing = grok(base, json!([]));
            missing["config"]["binaryPath"] = json!(base.join("bin/does-not-exist"));
            let mut disabled = grok(base, json!([]));
            disabled["enabled"] = json!(false);
            json!({"grok":missing,"grok-off":disabled})
        },
        |_, _| {},
    )
    .await;
    let missing = refreshed(&harness, 1).await;
    assert_eq!((missing["installed"].clone(), missing["status"].clone()), (json!(false), json!("error")));
    assert_eq!(missing["message"], "Grok CLI (`grok`) is not installed or not on PATH.");
    let mut manager = harness.connect(&harness.manager).await;
    let reply = rpc(&mut manager, 2, "server.refreshProviders", json!({"instanceId":"grok-off"})).await;
    let off = provider(&success(&reply)["providers"], "grok-off");
    assert_eq!((off["enabled"].clone(), off["status"].clone()), (json!(false), json!("disabled")));
    assert_eq!(off["message"], "Grok is disabled in T3 Code settings.");
    // The wrapper that would have logged a launch was never run.
    assert!(launches(&harness.base).is_empty());
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_in_account_reports_its_email_and_the_usage_the_billing_endpoint_returns() {
    // The billing endpoint is a loopback server; the credential file lives in the temp GROK_HOME.
    use axum::{Router, http::HeaderMap, routing::get};
    let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let record = seen.clone();
    let app = Router::new().route("/v1/billing", get(move |headers: HeaderMap| {
        let record = record.clone();
        async move {
            record.lock().unwrap().push(headers.get("authorization").and_then(|value| value.to_str().ok()).unwrap_or("").to_owned());
            json!({"config":{"creditUsagePercent":37.5,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-10-12T00:00:00Z"}}}).to_string()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/billing", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(directory.path()).unwrap().join("home/.grok");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("auth.json"), json!({"https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828":{"key":"token-123","auth_mode":"oauth","email":" me@example.test "},"https://other.example":{"key":"another","email":"other@example.test"}}).to_string()).unwrap();
    let instance = t3_server::grok_instance::GrokInstance { instance_id: "grok".into(), display_name: "Grok".into(), accent_color: None, enabled: true, config: serde_json::from_value(json!({"enabled":true,"binaryPath":"/nonexistent/grok"})).unwrap(), environment: [("GROK_HOME".to_owned(), home.to_string_lossy().into_owned()), ("HOME".to_owned(), home.parent().unwrap().to_string_lossy().into_owned())].into() };
    let (email, limits) = instance.read_account(&url).await;
    assert_eq!(email.as_deref(), Some("me@example.test"));
    assert_eq!(limits["windows"][0], json!({"id":"subscription","kind":"weekly","label":"Weekly","usedPercent":37.5,"resetsAt":"2026-10-12T00:00:00.000Z"}));
    assert_eq!(*seen.lock().unwrap(), vec!["Bearer token-123".to_owned()]);
    // Another auth deployment configured in the environment is left to the CLI: no account, no request.
    let mut custom = instance.clone();
    custom.environment.insert("GROK_OAUTH2_ISSUER".into(), "https://issuer.example".into());
    let (email, limits) = custom.read_account(&url).await;
    assert_eq!(email, None);
    assert_eq!(limits["unavailable"]["reason"], "unsupported");
    assert_eq!(seen.lock().unwrap().len(), 1);
    // A billing failure still knows who it asked about.
    let (email, limits) = instance.read_account("http://127.0.0.1:1/v1/billing").await;
    assert_eq!(email.as_deref(), Some("me@example.test"));
    assert_eq!(limits["unavailable"]["reason"], "probeFailed");
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn grok_is_its_own_updater_and_a_registry_release_ahead_of_it_is_an_advisory() {
    let registry = Registry::start(Some("1.0.50")).await;
    let url = registry.url.clone();
    let harness = start(
        |base| {
            let version_file = base.join("grok-version");
            std::fs::write(&version_file, "grok 1.0.41").unwrap();
            let mut instance = grok(base, json!([{"name":"GROK_FIXTURE_VERSION_FILE","value":version_file},{"name":"GROK_FIXTURE_UPDATE_TO","value":"1.0.50"}]));
            // A secret in the instance's environment is no business of an update.
            instance["environment"].as_array_mut().unwrap().push(json!({"name":"XAI_API_KEY","value":"xai-update-secret-0123456789","sensitive":true}));
            json!({"grok":instance})
        },
        move |_, management| management.configure(|options| options.registry_url = url),
    )
    .await;
    let behind = refreshed(&harness, 1).await;
    let advisory = &behind["versionAdvisory"];
    assert_eq!((advisory["status"].clone(), advisory["currentVersion"].clone(), advisory["latestVersion"].clone()), (json!("behind_latest"), json!("1.0.41"), json!("1.0.50")), "{advisory}");
    assert_eq!(advisory["canUpdate"], true);
    assert!(advisory["updateCommand"].as_str().unwrap().ends_with("bin/grok update"), "the resolved executable runs `update` itself: {advisory}");
    assert!(!behind.to_string().contains("xai-update-secret"));
    // The update is the CLI's own: it replaces the install and the version is read again.
    let mut manager = harness.connect(&harness.manager).await;
    let updated = rpc(&mut manager, 2, "server.updateProvider", json!({"provider":"grok"})).await;
    let grok = provider(&success(&updated)["providers"], "grok");
    assert_eq!(grok["updateState"]["status"], "succeeded", "{grok}");
    assert_eq!(grok["version"], "1.0.50");
    assert_eq!(grok["versionAdvisory"]["status"], "current");
    assert!(launches(&harness.base).contains(&vec!["update".to_owned()]));
    assert!(!updated.to_string().contains("xai-update-secret"));
    // A Grok that is not installed has nothing to update.
    drop(manager);
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_scan_lists_what_that_directory_has_without_rereading_the_account() {
    let harness = start(|base| json!({"grok":grok(base, json!([]))}), |_, _| {}).await;
    let machine = refreshed(&harness, 1).await;
    let project = harness.base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut manager = harness.connect(&harness.manager).await;
    let scanned = rpc(&mut manager, 2, "server.refreshProviders", json!({"instanceId":"grok","cwd":project})).await;
    let grok = provider(&success(&scanned)["providers"], "grok");
    let snapshots = grok["workspaceSnapshots"].as_array().unwrap();
    assert_eq!(snapshots.len(), 1, "{grok}");
    assert_eq!(snapshots[0]["cwd"], json!(project));
    let here = |skills: &Value| skills.as_array().unwrap().iter().find(|skill| skill["name"] == "here").unwrap()["description"].clone();
    assert_eq!(here(&snapshots[0]["skills"]), json!(std::fs::canonicalize(&project).unwrap()), "the directory scanned is the one asked about");
    assert_ne!(here(&machine["skills"]), here(&snapshots[0]["skills"]), "the machine snapshot is the server's own directory");
    // The account belongs to the machine: a workspace scan neither carries nor changes it.
    assert!(snapshots[0].get("usageLimits").is_none() && snapshots[0].get("auth").is_none());
    assert_eq!(grok["usageLimits"], machine["usageLimits"]);
    drop(manager);
    harness.finish().await;
}
