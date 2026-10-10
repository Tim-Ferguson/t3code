#![cfg(unix)]
//! The OpenCode driver for a 1.x install: how it appears in the provider list (the models and agents the server lists,
//! the capabilities its adapter declares), the advisory and update it offers within its own package, and the failures
//! it names without a credential. The executable is a wrapper at an absolute path in the test's temp directory that
//! runs `fixtures/opencode-fake-cli.py` in its 1.x mode (`--version` and `serve` on loopback); HOME and the data home
//! stay in the temp directory, and no real `opencode` runs. A server a test started is ended on every exit path through
//! the pid the server wrote for itself.
mod opencode_support;
mod provider_manage_support;
use provider_manage_support::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Ends the server a test started if the test did not: only by the pid the server recorded.
struct PidGuard(PathBuf);
impl PidGuard {
    fn pid(&self) -> Option<i32> {
        std::fs::read_to_string(&self.0).ok()?.trim().parse().ok().filter(|pid| *pid > 1)
    }
}
impl Drop for PidGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// A 1.x `opencode` at `path` that reports the version in `version_file` and serves the fake's canned inventory.
fn install(path: &Path, version_file: &Path, pidfile: &Path) {
    write_executable(path, &format!("#!/bin/sh\nexport OPENCODE_FIXTURE_GENERATION=v1\nexport OPENCODE_FIXTURE_VERSION=$(cat '{}')\nexport OPENCODE_FIXTURE_PIDFILE='{}'\nexec python3 {} \"$@\"\n", version_file.display(), pidfile.display(), opencode_support::FAKE));
}
fn opencode(binary: &Path, home: &Path, extra: Value) -> Value {
    let mut instance = json!({"driver":"opencode","enabled":true,"config":{"enabled":true,"binaryPath":binary},"environment":[{"name":"HOME","value":home},{"name":"XDG_DATA_HOME","value":home.join("share")},{"name":"OPENCODE_API_KEY","value":""}]});
    for (key, value) in extra.as_object().unwrap() {
        instance[key] = value.clone();
    }
    instance
}
async fn refreshed(harness: &Harness, id: u64) -> Value {
    let mut manager = harness.connect(&harness.manager).await;
    let reply = rpc(&mut manager, id, "server.refreshProviders", json!({"instanceId":"opencode"})).await;
    provider(&success(&reply)["providers"], "opencode").clone()
}
fn names(values: &Value, key: &str) -> Vec<String> {
    values.as_array().unwrap().iter().map(|value| value[key].as_str().unwrap().to_owned()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_x_install_is_ready_with_its_models_agents_commands_and_no_two_x_features() {
    let pids = std::sync::Arc::new(std::sync::Mutex::new(None::<PidGuard>));
    let kept = pids.clone();
    let harness = start(
        move |base| {
            let (binary, version, pidfile) = (base.join("bin/opencode"), base.join("version"), base.join("server.pid"));
            std::fs::write(&version, "1.14.39").unwrap();
            std::fs::create_dir_all(base.join("home")).unwrap();
            install(&binary, &version, &pidfile);
            *kept.lock().unwrap() = Some(PidGuard(pidfile));
            json!({"opencode":opencode(&binary, &base.join("home"), json!({"config":{"enabled":true,"binaryPath":binary,"customModels":["local/custom"]}}))})
        },
        |_, _| {},
    )
    .await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!((snapshot["installed"].clone(), snapshot["status"].clone(), snapshot["version"].clone()), (json!(true), json!("ready"), json!("1.14.39")), "{snapshot}");
    assert_eq!(snapshot["auth"], json!({"status":"authenticated","type":"opencode"}));
    assert_eq!(snapshot["message"], "1 upstream provider connected through OpenCode.");
    // 1.x has no interaction-mode toggle and offers no narrowed set of runtime modes.
    assert_eq!(snapshot["showInteractionModeToggle"], false);
    assert!(snapshot.get("supportedRuntimeModes").is_none(), "{snapshot}");
    // The models are the connected providers', each with its reasoning variants and the primary agents (not the
    // subagent the server also lists), the default being what the server and the install prefer.
    let models = snapshot["models"].as_array().unwrap();
    assert_eq!(names(&snapshot["models"], "slug"), ["openai/gpt-5.4", "openai/gpt-5.4-mini", "local/custom"]);
    let mini = models.iter().find(|model| model["slug"] == "openai/gpt-5.4-mini").unwrap();
    assert_eq!(mini["subProvider"], "OpenAI");
    let variants = &mini["capabilities"]["optionDescriptors"][0];
    assert_eq!((variants["id"].clone(), variants["currentValue"].clone()), (json!("variant"), json!("medium")));
    assert_eq!(names(&variants["options"], "id"), ["low", "medium", "high"]);
    let agents = &mini["capabilities"]["optionDescriptors"][1];
    assert_eq!((agents["id"].clone(), agents["currentValue"].clone()), (json!("agent"), json!("build")));
    assert_eq!(names(&agents["options"], "id"), ["build", "plan"]);
    // A model that advertises no variants still offers the standard levels.
    let plain = models.iter().find(|model| model["slug"] == "openai/gpt-5.4").unwrap();
    assert_eq!(names(&plain["capabilities"]["optionDescriptors"][0]["options"], "id"), ["low", "medium", "high", "xhigh"]);
    assert_eq!(snapshot["slashCommands"][0]["name"], "compact");
    assert_eq!(snapshot["slashCommands"][1], json!({"name":"init","description":"create or update AGENTS.md","input":{"hint":"$ARGUMENTS"}}));
    assert_eq!(snapshot["continuation"]["groupKey"], "opencode:instance:opencode");
    assert!(!snapshot.to_string().contains("1.x, which this build does not drive"));
    drop(harness);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_install_older_than_the_minimum_is_an_error_that_says_what_to_install() {
    let harness = start(
        |base| {
            let (binary, version, pidfile) = (base.join("bin/opencode"), base.join("version"), base.join("server.pid"));
            std::fs::write(&version, "1.14.18").unwrap();
            std::fs::create_dir_all(base.join("home")).unwrap();
            install(&binary, &version, &pidfile);
            json!({"opencode":opencode(&binary, &base.join("home"), json!({}))})
        },
        |_, _| {},
    )
    .await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!((snapshot["status"].clone(), snapshot["version"].clone()), (json!("error"), json!("1.14.18")));
    assert_eq!(snapshot["message"], "OpenCode v1.14.18 is too old. Upgrade to v1.14.19 or newer.");
    harness.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_one_x_server_is_probed_with_its_password_and_never_spawned() {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let row: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/opencode-replay/simple.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
    let server = opencode_support::FakeServer::start(&base, "simple", "opencode-sdk.sse", "1.14.39", &row["transcript"], Some("hunter2"));
    let url = server.url.clone();
    let harness = start(
        |base| {
            std::fs::create_dir_all(base.join("home")).unwrap();
            json!({
                "opencode":opencode(&base.join("unused"), &base.join("home"), json!({"config":{"enabled":true,"binaryPath":base.join("unused"),"serverUrl":url,"serverPassword":"hunter2"}})),
                "wrong":opencode(&base.join("unused"), &base.join("home"), json!({"config":{"enabled":true,"binaryPath":base.join("unused"),"serverUrl":url,"serverPassword":"nope"}})),
            })
        },
        |_, _| {},
    )
    .await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!((snapshot["status"].clone(), snapshot["version"].clone()), (json!("ready"), json!("1.14.39")), "{snapshot}");
    assert_eq!(snapshot["message"], "1 upstream provider connected through the configured OpenCode server.");
    // The host's own account is never read for a server the user runs.
    assert!(snapshot["usageLimits"]["unavailable"]["reason"] == "unsupported" || snapshot["usageLimits"].is_null());
    let mut manager = harness.connect(&harness.manager).await;
    let reply = rpc(&mut manager, 2, "server.refreshProviders", json!({"instanceId":"wrong"})).await;
    let wrong = provider(&success(&reply)["providers"], "wrong").clone();
    assert_eq!(wrong["status"], "error");
    assert_eq!(wrong["message"], "OpenCode server rejected authentication. Check the server URL and password.");
    assert!(!wrong.to_string().contains("nope") && !wrong.to_string().contains("hunter2") && !snapshot.to_string().contains("hunter2"));
    drop(manager);
    harness.finish().await;
}

/// The install is under npm's `opencode-ai` package, so an update is that package's, and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn a_one_x_install_is_advised_and_updated_within_its_own_package() {
    let registry = Registry::start(Some("1.15.2")).await;
    let url = registry.url.clone();
    let pids = std::sync::Arc::new(std::sync::Mutex::new(None::<PidGuard>));
    let kept = pids.clone();
    let harness = start(
        move |base| {
            let package = base.join("npm/lib/node_modules/opencode-ai/bin");
            std::fs::create_dir_all(&package).unwrap();
            std::fs::create_dir_all(base.join("npm/bin")).unwrap();
            std::fs::create_dir_all(base.join("home")).unwrap();
            let (version, pidfile) = (package.join("version"), base.join("server.pid"));
            std::fs::write(&version, "1.14.39").unwrap();
            install(&package.join("opencode"), &version, &pidfile);
            std::os::unix::fs::symlink("../lib/node_modules/opencode-ai/bin/opencode", base.join("npm/bin/opencode")).unwrap();
            *kept.lock().unwrap() = Some(PidGuard(pidfile));
            json!({"opencode":opencode(&base.join("npm/bin/opencode"), &base.join("home"), json!({}))})
        },
        move |base, management| {
            let tools = base.join("tools-bin");
            std::fs::create_dir_all(&tools).unwrap();
            // The fake npm installs 1.15.2 into the package and records what it was asked.
            let version = base.join("npm/lib/node_modules/opencode-ai/bin/version");
            write_executable(&tools.join("npm"), &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '%s' 1.15.2 > '{}'\n", base.join("npm.log").display(), version.display()));
            let path = tools.to_string_lossy().into_owned();
            management.configure(|options| {
                options.registry_url = url;
                options.path = Some(path);
            });
        },
    )
    .await;
    let behind = refreshed(&harness, 1).await;
    let advisory = &behind["versionAdvisory"];
    assert_eq!((advisory["status"].clone(), advisory["currentVersion"].clone(), advisory["latestVersion"].clone()), (json!("behind_latest"), json!("1.14.39"), json!("1.15.2")), "{behind}");
    assert_eq!(advisory["canUpdate"], true);
    let prefix = harness.base.join("npm");
    assert_eq!(advisory["updateCommand"], format!("npm install -g --prefix {} --allow-scripts=opencode-ai opencode-ai@latest", prefix.display()), "1.x is updated through `opencode-ai`, never `@opencode/cli`");
    let mut manager = harness.connect(&harness.manager).await;
    let updated = rpc(&mut manager, 2, "server.updateProvider", json!({"provider":"opencode"})).await;
    let opencode = provider(&success(&updated)["providers"], "opencode");
    assert_eq!(opencode["updateState"]["status"], "succeeded", "{opencode}");
    assert_eq!(opencode["version"], "1.15.2");
    assert_eq!(opencode["versionAdvisory"]["status"], "current");
    let log = std::fs::read_to_string(harness.base.join("npm.log")).unwrap();
    assert_eq!(log.lines().collect::<Vec<_>>(), [format!("install -g --prefix {} --allow-scripts=opencode-ai opencode-ai@latest", prefix.display())]);
    drop(manager);
    harness.finish().await;
}

/// A server that cannot start is explained by what it said, without the password it was given or any credential in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_cannot_start_is_explained_by_its_output_without_the_password() {
    let harness = start(
        |base| {
            let (binary, version, pidfile) = (base.join("bin/opencode"), base.join("version"), base.join("server.pid"));
            std::fs::write(&version, "1.14.39").unwrap();
            std::fs::create_dir_all(base.join("home")).unwrap();
            write_executable(&binary, &format!("#!/bin/sh\nexport OPENCODE_FIXTURE_GENERATION=v1\nexport OPENCODE_FIXTURE_VERSION=$(cat '{}')\nexport OPENCODE_FIXTURE_PIDFILE='{}'\nexport OPENCODE_FIXTURE_STARTUP_FAIL=1\nexec python3 {} \"$@\"\n", version.display(), pidfile.display(), opencode_support::FAKE));
            json!({"opencode":opencode(&binary, &base.join("home"), json!({"config":{"enabled":true,"binaryPath":binary,"serverPassword":"hunter2-secret"}}))})
        },
        |_, _| {},
    )
    .await;
    let snapshot = refreshed(&harness, 1).await;
    assert_eq!(snapshot["status"], "error");
    let message = snapshot["message"].as_str().unwrap();
    assert!(message.contains("OpenCode server exited before startup completed.") && message.contains("fatal: cannot bind the port"), "{message}");
    assert!(!snapshot.to_string().contains("hunter2-secret") && !snapshot.to_string().contains("abcdef0123456789"), "{message}");
    assert!(message.contains("[REDACTED]"), "{message}");
    harness.finish().await;
}
