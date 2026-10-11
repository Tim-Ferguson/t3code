//! `server.refreshProviders` and `server.updateProvider` through the real WebSocket transport. Providers
//! are fake Codex CLIs inside the temp T3 home, update tools are fake `npm` scripts found only through a
//! temporary search path, and the npm registry is a loopback server.
#![cfg(unix)]
use t3_server::test_support::HANG_BOUND;
mod provider_manage_support;
use provider_manage_support::*;
use serde_json::{Value, json};
use std::path::Path;

fn codex_instance(binary: &Path, home: &Path) -> Value {
    json!({"driver":"codex","enabled":true,"config":{"binaryPath":binary,"homePath":home}})
}
/// Reads the config stream until a `providerStatuses` event satisfies `wanted`, acknowledging each chunk.
async fn provider_event(socket: &mut Socket, id: u64, wanted: impl Fn(&Value) -> bool) -> Value {
    loop {
        let message = next_while(socket, "waiting for the provider list stream to publish the expected providerStatuses").await;
        assert_ne!(message["_tag"], "Exit", "stream ended: {message}");
        if message["_tag"] != "Chunk" {
            continue;
        }
        send(socket, json!({"_tag":"Ack","requestId":id})).await;
        for value in message["values"].as_array().unwrap() {
            if value["type"] == "providerStatuses" && wanted(&value["payload"]["providers"]) {
                return value["payload"]["providers"].clone();
            }
        }
    }
}
fn fake_tool(directory: &Path, name: &str, script: &str) {
    write_executable(&directory.join(name), &format!("#!/bin/sh\n{script}\n"));
}
/// A fake `npm` that records its arguments and "installs" `version` into the fake Codex package.
fn fake_npm(directory: &Path, prefix: &Path, version: &str, extra: &str) {
    let log = directory.join("npm.log");
    let version_file = prefix.join("lib/node_modules/@openai/codex/bin/version");
    fake_tool(directory, "npm", &format!("printf '%s\\n' \"$*\" >> '{}'\n{extra}\nprintf '%s' '{version}' > '{}'", log.display(), version_file.display()));
}
fn npm_log(directory: &Path) -> Vec<String> {
    std::fs::read_to_string(directory.join("npm.log")).map(|text| text.lines().map(str::to_owned).collect()).unwrap_or_default()
}
struct Fixture {
    harness: Harness,
    prefix: std::path::PathBuf,
    tools: std::path::PathBuf,
    registry: Registry,
}
/// A Codex installed by npm under `<home>/npm` at `version`, a second one under a version manager, a
/// loopback npm registry answering `latest`, and an empty directory for fake update tools.
async fn fixture(version: &'static str, latest: Option<&str>) -> Fixture {
    let registry = Registry::start(latest).await;
    let url = registry.url.clone();
    let harness = start(
        |base| {
            let codex = npm_codex(&base.join("npm"), version);
            let pinned = npm_codex(&base.join("mise/installs/codex/1.0"), version);
            json!({"codex":codex_instance(&codex, &base.join("codex-home")),"pinned":codex_instance(&pinned, &base.join("pinned-home"))})
        },
        move |base, management| {
            std::fs::create_dir_all(base.join("tools-bin")).unwrap();
            let path = base.join("tools-bin").to_string_lossy().into_owned();
            management.configure(|options| {
                options.registry_url = url;
                options.path = Some(path);
            });
        },
    )
    .await;
    let (prefix, tools) = (harness.base.join("npm"), harness.base.join("tools-bin"));
    Fixture { harness, prefix, tools, registry }
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_reprobes_providers_and_republishes_them_to_the_provider_list_stream() {
    let fixture = fixture("0.156.1", Some("0.158.0")).await;
    let harness = &fixture.harness;
    let mut manager = harness.connect(&harness.manager).await;
    // The stream has a socket of its own: unary replies on a shared one would swallow chunks nobody acknowledges.
    let mut watcher = harness.connect(&harness.manager).await;

    // The list a client already holds: the startup probe, without advisories.
    send(&mut watcher, json!({"_tag":"Request","id":1,"tag":"subscribeServerConfig","payload":{},"headers":[]})).await;
    let snapshot = loop {
        let message = next_while(&mut watcher, "waiting for the server config snapshot").await;
        if message["_tag"] == "Chunk" && message["values"][0]["type"] == "snapshot" {
            send(&mut watcher, json!({"_tag":"Ack","requestId":1})).await;
            break message["values"][0]["config"]["providers"].clone();
        }
    };
    assert_eq!(provider(&snapshot, "codex")["version"], "0.156.1");
    assert!(provider(&snapshot, "codex").get("versionAdvisory").is_none());

    // The installed CLI changes behind the server's back; a refresh finds out and says so to everyone.
    set_codex_version(&fixture.prefix, "0.157.0");
    let refreshed = rpc(&mut manager, 2, "server.refreshProviders", json!({})).await;
    let providers = &success(&refreshed)["providers"];
    let codex = provider(providers, "codex");
    assert_eq!(codex["version"], "0.157.0");
    assert_eq!(codex["status"], "ready");
    assert_eq!(codex["versionAdvisory"]["status"], "behind_latest");
    assert_eq!(codex["versionAdvisory"]["latestVersion"], "0.158.0");
    assert_eq!(codex["versionAdvisory"]["canUpdate"], true);
    assert_eq!(codex["versionAdvisory"]["updateCommand"], format!("npm install -g --prefix {} --allow-scripts=@openai/codex @openai/codex@latest", fixture.prefix.display()));
    assert_eq!(codex["compatibilityAdvisory"]["status"], "supported");
    assert_eq!(provider(providers, "claudeAgent")["enabled"], false);
    let pushed = provider_event(&mut watcher, 1, |providers| provider(providers, "codex")["version"] == "0.157.0").await;
    assert_eq!(provider(&pushed, "codex")["versionAdvisory"]["latestVersion"], "0.158.0");
    assert_eq!(fixture.registry.count(), 1);

    // One instance, and an unknown one (which answers with the list it has, probing nothing).
    set_codex_version(&fixture.prefix, "0.159.0");
    let unknown = rpc(&mut manager, 3, "server.refreshProviders", json!({"instanceId":"nope"})).await;
    assert_eq!(provider(&success(&unknown)["providers"], "codex")["version"], "0.157.0");
    let one = rpc(&mut manager, 4, "server.refreshProviders", json!({"instanceId":"codex"})).await;
    assert_eq!(provider(&success(&one)["providers"], "codex")["version"], "0.159.0");
    // The latest version is cached for an hour, and an explicit models refresh drops that cache.
    assert_eq!(fixture.registry.count(), 1);
    let models = rpc(&mut manager, 5, "server.refreshProviders", json!({"instanceId":"codex","refreshModels":true})).await;
    assert_eq!(success(&models)["providers"][0]["instanceId"], "codex");
    assert_eq!(fixture.registry.count(), 2);

    // Malformed input is a decode defect, not a probe.
    assert!(defect(&rpc(&mut manager, 6, "server.refreshProviders", json!({"instanceId":5})).await).as_str().is_some());
    assert!(defect(&rpc(&mut manager, 7, "server.refreshProviders", json!(null)).await).as_str().is_some());

    // Refreshing needs only the read scope: it re-reads status, it does not change anything.
    let mut reader = harness.connect(&harness.reader).await;
    assert_eq!(success(&rpc(&mut reader, 1, "server.refreshProviders", json!({})).await)["providers"].as_array().unwrap().len(), 8);
    drop((reader, manager, watcher));
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_keeps_a_workspace_snapshot_per_cwd_and_a_fresh_scan_renews_it() {
    let fixture = fixture("0.156.1", None).await;
    let harness = &fixture.harness;
    let mut manager = harness.connect(&harness.manager).await;
    let cwd = harness.base.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let scanned = rpc(&mut manager, 1, "server.refreshProviders", json!({"instanceId":"codex","cwd":cwd})).await;
    let snapshots = &provider(&success(&scanned)["providers"], "codex")["workspaceSnapshots"];
    assert_eq!(snapshots.as_array().unwrap().len(), 1);
    assert_eq!(snapshots[0]["cwd"], json!(cwd));
    let checked_at = snapshots[0]["checkedAt"].clone();
    // A current snapshot is left alone; a fresh scan replaces it.
    let again = rpc(&mut manager, 2, "server.refreshProviders", json!({"instanceId":"codex","cwd":cwd})).await;
    assert_eq!(provider(&success(&again)["providers"], "codex")["workspaceSnapshots"][0]["checkedAt"], checked_at);
    let fresh = rpc(&mut manager, 3, "server.refreshProviders", json!({"instanceId":"codex","cwd":cwd,"fresh":true})).await;
    let renewed = &provider(&success(&fresh)["providers"], "codex")["workspaceSnapshots"];
    assert_eq!(renewed.as_array().unwrap().len(), 1);
    assert_ne!(renewed[0]["checkedAt"], checked_at);
    drop(manager);
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn update_runs_the_owning_package_manager_and_reports_progress_through_the_provider_list() {
    let fixture = fixture("0.156.1", Some("0.200.0")).await;
    let harness = &fixture.harness;
    fake_npm(&fixture.tools, &fixture.prefix, "0.200.0", "");
    let mut manager = harness.connect(&harness.manager).await;
    let mut watcher = harness.connect(&harness.manager).await;
    send(&mut watcher, json!({"_tag":"Request","id":1,"tag":"subscribeServerConfig","payload":{},"headers":[]})).await;
    let first = next_while(&mut watcher, "waiting for the server config snapshot").await;
    assert_eq!(first["values"][0]["type"], "snapshot");
    send(&mut watcher, json!({"_tag":"Ack","requestId":1})).await;

    // Scope: managing providers is not reading them, nor operating threads.
    let mut operator = harness.connect(&harness.operator).await;
    let denied = rpc(&mut operator, 1, "server.updateProvider", json!({"provider":"codex"})).await;
    assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError");
    assert_eq!(failure(&denied)["requiredPermission"], "providers:manage");
    assert!(npm_log(&fixture.tools).is_empty());

    let updated = rpc(&mut manager, 2, "server.updateProvider", json!({"provider":"codex"})).await;
    let codex = provider(&success(&updated)["providers"], "codex");
    assert_eq!(codex["updateState"]["status"], "succeeded", "{codex}");
    assert_eq!(codex["updateState"]["message"], "Provider updated.");
    assert_eq!(codex["version"], "0.200.0");
    assert_eq!(codex["versionAdvisory"]["status"], "current");
    assert_eq!(npm_log(&fixture.tools), [format!("install -g --prefix {} --allow-scripts=@openai/codex @openai/codex@latest", fixture.prefix.display())]);
    let pushed = provider_event(&mut watcher, 1, |providers| provider(providers, "codex")["updateState"]["status"] == "succeeded").await;
    assert_eq!(provider(&pushed, "codex")["version"], "0.200.0");

    // An instance with another driver, an unknown instance, and a provider with nothing to run are refused.
    let wrong = rpc(&mut manager, 3, "server.updateProvider", json!({"provider":"codex","instanceId":"claudeAgent"})).await;
    assert_eq!(failure(&wrong), &json!({"_tag":"ServerProviderUpdateError","provider":"codex","reason":"This provider does not support one-click updates."}));
    let missing = rpc(&mut manager, 4, "server.updateProvider", json!({"provider":"codex","instanceId":"ghost"})).await;
    assert_eq!(failure(&missing)["reason"], "This provider does not support one-click updates.");
    assert_eq!(npm_log(&fixture.tools).len(), 1);
    assert!(defect(&rpc(&mut manager, 5, "server.updateProvider", json!({"provider":"codex","targetVersion":""})).await).as_str().is_some());
    drop((manager, operator, watcher));
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn update_outcomes_failed_unchanged_and_refused_are_states_of_the_instance() {
    let fixture = fixture("0.156.1", Some("0.200.0")).await;
    let harness = &fixture.harness;
    let mut manager = harness.connect(&harness.manager).await;

    // The tool exits non-zero: failed, with its output (credentials masked).
    fake_tool(&fixture.tools, "npm", "echo 'npm ERR! //registry.npmjs.org/:_authToken=sekret123' >&2\necho 'EACCES: permission denied'\nexit 3");
    let failed = rpc(&mut manager, 1, "server.updateProvider", json!({"provider":"codex"})).await;
    let state = &provider(&success(&failed)["providers"], "codex")["updateState"];
    assert_eq!(state["status"], "failed");
    assert_eq!(state["message"], "Update command exited with code 3.");
    let output = state["output"].as_str().unwrap();
    assert!(output.contains("EACCES: permission denied") && !output.contains("sekret123"), "{output}");
    assert_eq!(provider(&success(&failed)["providers"], "codex")["version"], "0.156.1");

    // The tool succeeds but the installed version does not move: unchanged, and still outdated.
    fake_tool(&fixture.tools, "npm", "exit 0");
    let unchanged = rpc(&mut manager, 2, "server.updateProvider", json!({"provider":"codex"})).await;
    let state = &provider(&success(&unchanged)["providers"], "codex")["updateState"];
    assert_eq!(state["status"], "unchanged");
    assert_eq!(state["message"], "Update command completed, but T3 Code still detects an outdated provider version.");

    // A tool that cannot be started at all is a failed update too.
    std::fs::remove_file(fixture.tools.join("npm")).unwrap();
    let missing = rpc(&mut manager, 3, "server.updateProvider", json!({"provider":"codex"})).await;
    assert_eq!(provider(&success(&missing)["providers"], "codex")["updateState"]["status"], "failed");

    // A tool that outlives its budget is killed.
    fake_tool(&fixture.tools, "npm", "exec /bin/sleep 600");
    fixture.harness.registry.management().configure(|options| options.update_timeout = std::time::Duration::from_millis(300));
    let timed_out = rpc(&mut manager, 4, "server.updateProvider", json!({"provider":"codex"})).await;
    let state = &provider(&success(&timed_out)["providers"], "codex")["updateState"];
    assert_eq!((state["status"].as_str(), state["message"].as_str()), (Some("failed"), Some("Update timed out.")));
    fixture.harness.registry.management().configure(|options| options.update_timeout = HANG_BOUND);

    // A latest version this release cannot run is not installed.
    *fixture.registry.version.lock().unwrap() = Some("0.100.0".into());
    fixture.harness.registry.management().invalidate("codex", Some("@openai/codex"));
    fake_npm(&fixture.tools, &fixture.prefix, "0.100.0", "");
    let rejected = rpc(&mut manager, 5, "server.updateProvider", json!({"provider":"codex"})).await;
    let state = &provider(&success(&rejected)["providers"], "codex")["updateState"];
    assert_eq!(state["message"], "The latest provider version is incompatible with this T3 Code release. Review provider settings.");
    assert!(npm_log(&fixture.tools).is_empty());

    // A specific version is installed only when the release policy recommends exactly it, and the bundled
    // Codex policy recommends a range, never a version.
    let pinned = rpc(&mut manager, 6, "server.updateProvider", json!({"provider":"codex","targetVersion":"0.157.0"})).await;
    let state = &provider(&success(&pinned)["providers"], "codex")["updateState"];
    assert_eq!(state["message"], "This version is no longer recommended or this installer cannot install a specific version. Refresh provider settings.");
    assert!(npm_log(&fixture.tools).is_empty());
    drop(manager);
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_one_update_runs_per_instance_and_a_manual_install_cannot_be_updated() {
    let fixture = fixture("0.156.1", Some("0.200.0")).await;
    let harness = &fixture.harness;
    // The fake tool announces itself on one FIFO and waits on another, so the test needs no timing.
    let (started, go) = (harness.base.join("started.fifo"), harness.base.join("go.fifo"));
    for fifo in [&started, &go] {
        assert!(std::process::Command::new("mkfifo").arg(fifo).status().unwrap().success());
    }
    fake_npm(&fixture.tools, &fixture.prefix, "0.200.0", &format!("echo up > '{}'\nread line < '{}'", started.display(), go.display()));
    let mut first = harness.connect(&harness.manager).await;
    let mut second = harness.connect(&harness.manager).await;
    send(&mut first, json!({"_tag":"Request","id":1,"tag":"server.updateProvider","payload":{"provider":"codex"},"headers":[]})).await;
    let started_reader = started.clone();
    blocking_step("waiting for the fake npm to announce itself on the started FIFO", move || std::fs::read_to_string(started_reader).unwrap()).await;
    let busy = rpc(&mut second, 1, "server.updateProvider", json!({"provider":"codex"})).await;
    assert_eq!(failure(&busy), &json!({"_tag":"ServerProviderUpdateError","provider":"codex","reason":"An update is already running for this provider."}));
    // While it runs, the instance reads as running to everyone.
    assert_eq!(provider(&harness.registry.snapshots().into(), "codex")["updateState"]["status"], "running");
    let go_writer = go.clone();
    blocking_step("releasing the fake npm through the go FIFO", move || std::fs::write(go_writer, "go\n").unwrap()).await;
    let done = loop {
        let message = next_while(&mut first, "waiting for the first updateProvider to finish").await;
        if message["_tag"] == "Exit" {
            break message["exit"].clone();
        }
    };
    assert_eq!(provider(&success(&done)["providers"], "codex")["updateState"]["status"], "succeeded");

    // Under a version manager the install is pinned by its config, so there is nothing to run.
    let managed = rpc(&mut first, 2, "server.refreshProviders", json!({"instanceId":"pinned"})).await;
    assert_eq!(provider(&success(&managed)["providers"], "pinned")["versionAdvisory"]["canUpdate"], false);
    let refused = rpc(&mut first, 3, "server.updateProvider", json!({"provider":"codex","instanceId":"pinned"})).await;
    assert_eq!(failure(&refused)["reason"], "This provider does not support one-click updates.");
    drop((first, second));
    fixture.harness.finish().await;
}
