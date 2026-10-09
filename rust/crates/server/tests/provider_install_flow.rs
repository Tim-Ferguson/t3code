//! Managed Codex installation through the real WebSocket transport: downloads come from a loopback host
//! serving scripted archives, the machine's own PATH is never consulted (a temp directory stands in for
//! it), and no real provider CLI is ever run.
#![cfg(unix)]
mod provider_manage_support;
use provider_manage_support::*;
use serde_json::{Value, json};
use std::{path::{Path, PathBuf}, sync::Arc};
use t3_server::provider_install::ReleaseAsset;

const VERSION: &str = "0.156.1";
const TARGET: &str = "test-target";

fn managed_instance(base: &Path) -> Value {
    json!({"driver":"codex","enabled":true,"config":{"setupMode":"managed","homePath":base.join("codex-home")}})
}
struct Fixture {
    harness: Harness,
    releases: Releases,
    managed: PathBuf,
    path_dir: PathBuf,
}
/// `asset` is built from the archive served at `/package.tgz` unless `adjust` changes it.
async fn fixture(archive_bytes: Option<Vec<u8>>, adjust: impl FnOnce(&mut ReleaseAsset) + Send + 'static) -> Fixture {
    fixture_with(archive_bytes, adjust, false).await
}
/// With `custom`, a second instance is configured to run a binary inside the managed runtime.
async fn fixture_with(archive_bytes: Option<Vec<u8>>, adjust: impl FnOnce(&mut ReleaseAsset) + Send + 'static, custom: bool) -> Fixture {
    let releases = Releases::start().await;
    let url = match &archive_bytes {
        Some(bytes) => releases.serve("/package.tgz", Route::Body(bytes.clone())),
        None => format!("{}/package.tgz", releases.url),
    };
    let mut asset = ReleaseAsset { version: VERSION.into(), target: TARGET.into(), url, sha256: archive_bytes.as_ref().map(|bytes| sha256_hex(bytes)).unwrap_or_else(|| "0".repeat(64)), archive_bytes: archive_bytes.as_ref().map(|bytes| bytes.len() as u64).unwrap_or(0) };
    adjust(&mut asset);
    let harness = start(
        |base| {
            let custom_instance = json!({"driver":"codex","enabled":false,"config":{"binaryPath":base.join("tools/codex/0.156.1/bin/codex"),"homePath":base.join("custom-home")}});
            json!({"codex":managed_instance(base),
                   "custom":if custom { custom_instance } else { json!({"driver":"codex","enabled":false,"config":{"binaryPath":base.join("elsewhere/custom")}}) },
                   "existing":{"driver":"codex","enabled":false,"config":{"setupMode":"existing","binaryPath":base.join("elsewhere/codex")}},
                   "agy":{"driver":"antigravity","enabled":true}})
        },
        move |base, management| {
            let path_dir = base.join("path");
            std::fs::create_dir_all(&path_dir).unwrap();
            let path = path_dir.to_string_lossy().into_owned();
            management.configure(|options| {
                options.codex_release = Some(Some(asset));
                options.codex_path = Some(path);
            });
        },
    )
    .await;
    let managed = harness.base.join("tools/codex");
    let path_dir = harness.base.join("path");
    Fixture { harness, releases, managed, path_dir }
}
/// Reads an install subscription until a state satisfies `wanted`, returning every state seen.
async fn states_until(socket: &mut Socket, id: u64, wanted: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut seen = vec![];
    loop {
        let message = next_while(socket, "waiting for the install subscription to publish the expected state").await;
        assert_ne!(message["_tag"], "Exit", "stream ended: {message}");
        if message["_tag"] != "Chunk" {
            continue;
        }
        send(socket, json!({"_tag":"Ack","requestId":id})).await;
        for value in message["values"].as_array().unwrap() {
            seen.push(value.clone());
            if wanted(value) {
                return seen;
            }
        }
    }
}
async fn subscribe(socket: &mut Socket, id: u64, instance: &str) {
    send(socket, json!({"_tag":"Request","id":id,"tag":"provider.install.subscribe","payload":{"instanceId":instance},"headers":[]})).await;
}
fn leftovers(managed: &Path) -> Vec<String> {
    std::fs::read_dir(managed).map(|entries| entries.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).filter(|name| name.starts_with(".install-") || name.starts_with(".active-")).collect()).unwrap_or_default()
}
async fn failed_with(fixture: &Fixture, socket: &mut Socket, message: &str) -> Value {
    // Watching has a socket of its own: a stream's next chunk waits for its acknowledgement.
    let mut events = fixture.harness.connect(&fixture.harness.manager).await;
    subscribe(&mut events, 90, "codex").await;
    let started = rpc(socket, 91, "provider.install.start", json!({"instanceId":"codex"})).await;
    assert_eq!(success(&started)["phase"], "downloading");
    let states = states_until(&mut events, 90, |state| state["phase"] == "failed").await;
    let last = states.last().unwrap().clone();
    assert_eq!(last["message"], message, "{last}");
    assert!(!fixture.managed.join("0.156.1").exists() && !fixture.managed.join("active.json").exists());
    assert_eq!(leftovers(&fixture.managed), Vec::<String>::new(), "staging must not outlive a failed install");
    last
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_install_downloads_verifies_reports_progress_republishes_providers_and_is_removed() {
    let archive_bytes = archive(&package(VERSION, TARGET, VERSION));
    let fixture = fixture(Some(archive_bytes.clone()), |_| {}).await;
    let harness = &fixture.harness;

    // Before anything is installed the provider list says how to get started.
    let before = harness.registry.snapshots();
    let codex = provider(&Value::from(before), "codex").clone();
    assert_eq!((codex["installed"].clone(), codex["message"].clone()), (json!(false), json!("Set up Codex to get started.")));
    assert_eq!(codex["setup"], json!({"canAuthenticate":false,"canInstall":true}));

    let mut manager = harness.connect(&harness.manager).await;
    let mut watcher = harness.connect(&harness.manager).await;
    send(&mut watcher, json!({"_tag":"Request","id":1,"tag":"subscribeServerConfig","payload":{},"headers":[]})).await;
    let mut events = harness.connect(&harness.manager).await;
    subscribe(&mut events, 1, "codex").await;
    let idle = states_until(&mut events, 1, |_| true).await;
    assert_eq!(idle[0]["phase"], "idle");
    assert_eq!(idle[0]["totalBytes"], archive_bytes.len());
    assert_eq!(idle[0]["canRemove"], false);

    let started = rpc(&mut manager, 2, "provider.install.start", json!({"instanceId":"codex"})).await;
    let operation = success(&started)["operationId"].clone();
    assert_eq!(success(&started)["phase"], "downloading");
    assert_eq!(success(&started)["message"], "Downloading Codex.");
    let states = states_until(&mut events, 1, |state| state["phase"] == "succeeded").await;
    let phases: Vec<&str> = states.iter().map(|state| state["phase"].as_str().unwrap()).collect();
    for phase in ["downloading", "extracting", "verifying", "succeeded"] {
        assert!(phases.contains(&phase), "{phases:?}");
    }
    let done = states.last().unwrap();
    assert_eq!((done["installedVersion"].clone(), done["source"].clone(), done["canRemove"].clone()), (json!(VERSION), json!("managed"), json!(true)));
    assert_eq!(done["executablePath"], json!(fixture.managed.join("0.156.1/bin/codex")));
    assert_eq!(states.iter().find(|state| state["phase"] == "extracting").unwrap()["operationId"], operation);
    assert_eq!(std::fs::read_to_string(fixture.managed.join("active.json")).unwrap(), json!({"version":VERSION}).to_string());
    assert!(fixture.managed.join("0.156.1/.install-complete.json").is_file());
    assert_eq!(leftovers(&fixture.managed), Vec::<String>::new());
    assert_eq!(fixture.releases.count(), 1);

    // The provider list every client holds now reads the install.
    let mut seen = None;
    loop {
        let message = next_while(&mut watcher, "waiting for the provider list stream to report the install").await;
        if message["_tag"] != "Chunk" {
            continue;
        }
        send(&mut watcher, json!({"_tag":"Ack","requestId":1})).await;
        let value = &message["values"][0];
        if value["type"] == "providerStatuses" && provider(&value["payload"]["providers"], "codex")["installed"] == true {
            seen = Some(provider(&value["payload"]["providers"], "codex").clone());
            break;
        }
    }
    let seen = seen.unwrap();
    assert_eq!(seen["version"], VERSION);

    // Removal deletes the managed directory and nothing else.
    std::fs::write(harness.base.join("tools/neighbour"), "keep").unwrap();
    let removed = rpc(&mut manager, 3, "provider.install.remove", json!({"instanceId":"codex"})).await;
    assert_eq!((success(&removed)["phase"].clone(), success(&removed)["canRemove"].clone(), success(&removed)["installedVersion"].clone()), (json!("idle"), json!(false), Value::Null));
    assert!(!fixture.managed.exists());
    assert_eq!(std::fs::read_to_string(harness.base.join("tools/neighbour")).unwrap(), "keep");
    assert_eq!(provider(&Value::from(harness.registry.snapshots()), "codex")["installed"], false);

    drop((manager, watcher, events));
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_supported_codex_on_path_is_preferred_and_nothing_is_downloaded() {
    let archive_bytes = archive(&package(VERSION, TARGET, VERSION));
    let fixture = fixture(Some(archive_bytes), |_| {}).await;
    let harness = &fixture.harness;
    let mut manager = harness.connect(&harness.manager).await;
    // A local install too old for this release is not used; a supported one is.
    let local = fixture.path_dir.join("codex");
    let local_script = |version: &str, padding: &str| format!("#!/bin/sh\n# {padding}\ncase \"$1\" in --version) echo 'codex-cli {version}';; esac\nexit 0\n");
    write_executable(&local, &local_script("0.100.0", ""));
    // The state was read when the service opened, so removal re-reads the machine.
    let _ = rpc(&mut manager, 1, "provider.install.remove", json!({"instanceId":"codex"})).await;
    let started = rpc(&mut manager, 2, "provider.install.start", json!({"instanceId":"codex"})).await;
    assert_eq!(success(&started)["phase"], "downloading", "an unsupported local version must not satisfy setup");
    let mut stream = harness.connect(&harness.manager).await;
    subscribe(&mut stream, 1, "codex").await;
    states_until(&mut stream, 1, |state| state["phase"] == "succeeded").await;
    assert_eq!(fixture.releases.count(), 1);

    write_executable(&local, &local_script("0.157.0", "a supported release, and a different file"));
    let again = rpc(&mut manager, 3, "provider.install.remove", json!({"instanceId":"codex"})).await;
    // After removal the supported local install is reported, with no download.
    assert_eq!((success(&again)["phase"].clone(), success(&again)["source"].clone(), success(&again)["installedVersion"].clone()), (json!("succeeded"), json!("local"), json!("0.157.0")));
    let reused = rpc(&mut manager, 4, "provider.install.start", json!({"instanceId":"codex"})).await;
    assert_eq!(success(&reused)["source"], "local");
    assert_eq!(fixture.releases.count(), 1);
    drop((manager, stream));
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_fails_verification_installs_nothing() {
    // Wrong digest.
    let good = archive(&package(VERSION, TARGET, VERSION));
    let fixture_a = fixture(Some(good.clone()), |asset| asset.sha256 = "f".repeat(64)).await;
    let mut socket = fixture_a.harness.connect(&fixture_a.harness.manager).await;
    failed_with(&fixture_a, &mut socket, "The Codex download failed its size or SHA-256 check. Nothing was installed.").await;
    drop(socket);
    fixture_a.harness.finish().await;

    // Smaller than promised.
    let fixture_b = fixture(Some(good.clone()), |asset| asset.archive_bytes += 5).await;
    let mut socket = fixture_b.harness.connect(&fixture_b.harness.manager).await;
    failed_with(&fixture_b, &mut socket, "The Codex download failed its size or SHA-256 check. Nothing was installed.").await;
    drop(socket);
    fixture_b.harness.finish().await;

    // Larger than promised: stopped at the bound, not read to the end.
    let fixture_c = fixture(Some(good.clone()), |asset| asset.archive_bytes -= 5).await;
    let mut socket = fixture_c.harness.connect(&fixture_c.harness.manager).await;
    failed_with(&fixture_c, &mut socket, "The Codex download exceeded the expected release size.").await;
    drop(socket);
    fixture_c.harness.finish().await;

    // A missing release.
    let fixture_d = fixture(Some(good.clone()), |asset| asset.url.push_str(".missing")).await;
    let mut socket = fixture_d.harness.connect(&fixture_d.harness.manager).await;
    failed_with(&fixture_d, &mut socket, "Could not install Codex. Check disk space and directory access, then try again.").await;
    drop(socket);
    fixture_d.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_download_leaves_nothing_behind() {
    let good = archive(&package(VERSION, TARGET, VERSION));
    let fixture = fixture(Some(good.clone()), |_| {}).await;
    let url = fixture.releases.serve("/package.tgz", Route::Truncated { body: good[..good.len() / 2].to_vec(), declared: good.len() });
    // The asset was fixed when the service was configured; point it at the scripted route by size and digest.
    assert!(url.ends_with("/package.tgz"));
    let mut socket = fixture.harness.connect(&fixture.harness.manager).await;
    let mut events = fixture.harness.connect(&fixture.harness.manager).await;
    subscribe(&mut events, 1, "codex").await;
    let started = rpc(&mut socket, 2, "provider.install.start", json!({"instanceId":"codex"})).await;
    assert_eq!(success(&started)["phase"], "downloading");
    let states = states_until(&mut events, 1, |state| state["phase"] == "failed").await;
    assert_eq!(states.last().unwrap()["message"], "Could not install Codex. Check disk space and directory access, then try again.");
    assert_eq!(leftovers(&fixture.managed), Vec::<String>::new());
    assert!(!fixture.managed.join("active.json").exists());
    drop((socket, events));
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archives_with_unsafe_or_untrustworthy_contents_are_never_extracted_or_run() {
    let cases: Vec<(&str, Vec<Value>, &str)> = vec![
        ("parent traversal", [package(VERSION, TARGET, VERSION), vec![member("file", "../evil", "x", 0o644)]].concat(), "The Codex archive contains unsafe entries."),
        ("absolute path", [package(VERSION, TARGET, VERSION), vec![member("file", "/tmp/t3-evil-absolute", "x", 0o644)]].concat(), "The Codex archive contains unsafe entries."),
        ("symlink member", [package(VERSION, TARGET, VERSION), vec![member("symlink", "link", "/etc/passwd", 0o777)]].concat(), "The Codex archive contains unsafe entries."),
        ("dot-slash member", vec![member("file", "./codex-package.json", "{}", 0o644)], "The Codex archive contains unsafe entries."),
        ("binary reports another version", package(VERSION, TARGET, "9.9.9"), "The downloaded Codex executable has the wrong version."),
        ("manifest names another target", package(VERSION, "other-target", VERSION), "The downloaded Codex package does not match the expected release."),
        ("manifest names another version", package("0.156.0", TARGET, VERSION), "The downloaded Codex package does not match the expected release."),
        ("missing companion", package(VERSION, TARGET, VERSION).into_iter().filter(|entry| entry["name"] != "codex-path/rg").collect(), "Could not install Codex. Check disk space and directory access, then try again."),
        ("companion not executable", package(VERSION, TARGET, VERSION).into_iter().map(|entry| if entry["name"] == "codex-path/rg" { member("file", "codex-path/rg", "x", 0o644) } else { entry }).collect(), "The managed Codex package is incomplete. Reinstall Codex."),
    ];
    for (label, members, message) in cases {
        let bytes = archive(&members);
        let fixture = fixture(Some(bytes), |_| {}).await;
        let mut socket = fixture.harness.connect(&fixture.harness.manager).await;
        let last = failed_with(&fixture, &mut socket, message).await;
        assert_eq!(last["installedVersion"], Value::Null, "{label}");
        assert!(!fixture.harness.base.parent().unwrap().join("evil").exists(), "{label}");
        assert!(!Path::new("/tmp/t3-evil-absolute").exists(), "{label}");
        drop(socket);
        fixture.harness.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_mid_download_stops_it_and_leaves_the_previous_runtime_unchanged() {
    let good = archive(&package(VERSION, TARGET, VERSION));
    let fixture = fixture(Some(good.clone()), |_| {}).await;
    let (started, release) = (Arc::new(tokio::sync::Notify::new()), Arc::new(tokio::sync::Notify::new()));
    fixture.releases.serve("/package.tgz", Route::Gated { body: good.clone(), first: good.len() / 2, started: started.clone(), release: release.clone() });
    let mut socket = fixture.harness.connect(&fixture.harness.manager).await;
    // The subscription has a socket of its own: unary replies on a shared one would swallow its chunks unacknowledged.
    let mut events = fixture.harness.connect(&fixture.harness.manager).await;
    subscribe(&mut events, 1, "codex").await;
    let begun = rpc(&mut socket, 2, "provider.install.start", json!({"instanceId":"codex"})).await;
    let operation = success(&begun)["operationId"].clone();
    // A second start while one runs joins it.
    let joined = rpc(&mut socket, 3, "provider.install.start", json!({"instanceId":"codex"})).await;
    assert_eq!(success(&joined)["operationId"], operation);
    within("waiting for the gated download to begin streaming", started.notified()).await;
    assert!(std::fs::read_dir(&fixture.managed).unwrap().any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with(".install-")));

    // Only the current operation can be cancelled, and the install cannot be removed while it runs.
    let stale = rpc(&mut socket, 4, "provider.install.cancel", json!({"instanceId":"codex","operationId":"not-the-one"})).await;
    assert_eq!(failure(&stale), &json!({"_tag":"ProviderSetupError","instanceId":"codex","operation":"cancel","detail":"This installation is no longer current. Refresh its status before cancelling."}));
    let busy = rpc(&mut socket, 5, "provider.install.remove", json!({"instanceId":"codex"})).await;
    assert_eq!(failure(&busy)["detail"], "Stop Codex sessions and sign-in flows before removing its managed runtime.");

    let cancelled = rpc(&mut socket, 6, "provider.install.cancel", json!({"instanceId":"codex","operationId":operation})).await;
    assert_eq!((success(&cancelled)["phase"].clone(), success(&cancelled)["message"].clone()), (json!("cancelled"), json!("Installation cancelled. The previous runtime is unchanged.")));
    assert_eq!(leftovers(&fixture.managed), Vec::<String>::new());
    assert!(!fixture.managed.join("active.json").exists() && !fixture.managed.join("0.156.1").exists());
    release.notify_one();
    // Cancelling again, once it is over, is a no-op that reports the state.
    let again = rpc(&mut socket, 7, "provider.install.cancel", json!({"instanceId":"codex","operationId":operation})).await;
    assert_eq!(success(&again)["phase"], "cancelled");
    states_until(&mut events, 1, |state| state["phase"] == "cancelled").await;
    drop((socket, events));
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_managed_codex_instances_install_and_removal_never_touches_paths_in_use() {
    let good = archive(&package(VERSION, TARGET, VERSION));
    let fixture = fixture_with(Some(good), |_| {}, true).await;
    let harness = &fixture.harness;
    let mut manager = harness.connect(&harness.manager).await;
    let detail = |exit: &Value, operation: &str| {
        let error = failure(exit);
        assert_eq!((error["_tag"].as_str(), error["operation"].as_str()), (Some("ProviderSetupError"), Some(operation)), "{exit}");
        error["detail"].as_str().unwrap().to_owned()
    };
    let other = |instance: &str| json!({"instanceId":instance});
    let unavailable = "Managed installation is not available for this provider instance.";
    // Other drivers, a driver this build has no adapter for, and an instance that does not exist.
    for instance in ["claudeAgent", "agy", "nope"] {
        assert_eq!(detail(&rpc(&mut manager, 1, "provider.install.start", other(instance)).await, "install"), unavailable, "{instance}");
        assert_eq!(detail(&rpc(&mut manager, 2, "provider.install.remove", other(instance)).await, "remove-install"), unavailable);
    }
    assert_eq!(detail(&rpc(&mut manager, 3, "provider.install.cancel", json!({"instanceId":"agy","operationId":"x"})).await, "cancel-install"), unavailable);
    // Codex that is not set up as managed, or runs a custom executable.
    assert_eq!(detail(&rpc(&mut manager, 4, "provider.install.start", other("existing")).await, "install"), "Choose managed setup to install Codex in T3 Code.");
    assert!(!fixture.managed.exists());
    // Removal refuses while an instance runs a binary inside the managed runtime.
    let started = rpc(&mut manager, 5, "provider.install.start", other("codex")).await;
    assert_eq!(success(&started)["phase"], "downloading");
    let mut stream = harness.connect(&harness.manager).await;
    subscribe(&mut stream, 1, "codex").await;
    states_until(&mut stream, 1, |state| state["phase"] == "succeeded").await;
    let refused = rpc(&mut manager, 6, "provider.install.remove", other("codex")).await;
    assert_eq!(detail(&refused, "remove"), "A provider instance uses a custom path inside managed Codex. Clear that path before removing it.");
    assert!(fixture.managed.join("0.156.1/bin/codex").is_file());
    // Observing is allowed for instances that are not managed-only; a custom executable cannot be installed over.
    let custom = rpc(&mut manager, 7, "provider.install.start", other("custom")).await;
    assert_eq!(detail(&custom, "install"), "Choose managed setup to install Codex in T3 Code.");
    drop((manager, stream));
    fixture.harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn installation_methods_are_gated_by_scope_and_validate_their_payloads() {
    let fixture = fixture(Some(archive(&package(VERSION, TARGET, VERSION))), |_| {}).await;
    let harness = &fixture.harness;
    let mut operator = harness.connect(&harness.operator).await;
    for (id, tag, payload) in [(1, "provider.install.start", json!({"instanceId":"codex"})), (2, "provider.install.cancel", json!({"instanceId":"codex","operationId":"x"})), (3, "provider.install.remove", json!({"instanceId":"codex"}))] {
        let denied = rpc(&mut operator, id, tag, payload).await;
        assert_eq!(failure(&denied)["_tag"], "EnvironmentAuthorizationError", "{tag}");
        assert_eq!(failure(&denied)["requiredPermission"], "providers:manage", "{tag}");
    }
    assert_eq!(fixture.releases.count(), 0);
    assert!(!fixture.managed.exists());
    // Watching an install needs only the read scope.
    let mut reader = harness.connect(&harness.reader).await;
    subscribe(&mut reader, 1, "codex").await;
    assert_eq!(states_until(&mut reader, 1, |_| true).await[0]["phase"], "idle");
    let mut manager = harness.connect(&harness.manager).await;
    assert!(defect(&rpc(&mut manager, 1, "provider.install.start", json!({"instanceId":5})).await).as_str().is_some());
    assert!(defect(&rpc(&mut manager, 2, "provider.install.cancel", json!({"instanceId":"codex"})).await).as_str().is_some());
    drop((operator, reader, manager));
    fixture.harness.finish().await;
}
