#![cfg(unix)]
//! The OpenCode driver: how an instance appears in the provider list and reports its state. The local executable is
//! a wrapper script at an absolute path in the test's temp directory that runs `fixtures/opencode-fake-cli.py`
//! (`--version` and `serve` on loopback); HOME and the data home stay in the temp directory, and no real `opencode`
//! runs. A server a test started is ended on every exit path through the pid the server wrote for itself.
mod opencode_support;
use serde_json::{Value, json};
use std::path::PathBuf;
use t3_server::provider_registry::ProviderRegistry;

/// Ends the server a test started if the test did not: only by the pid the server recorded.
struct PidGuard(PathBuf);
impl PidGuard {
    fn pid(&self) -> Option<i32> {
        std::fs::read_to_string(&self.0).ok()?.trim().parse().ok().filter(|pid| *pid > 1)
    }
    fn alive(&self) -> bool {
        self.pid().is_some_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }
}
impl Drop for PidGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

struct Install {
    _directory: tempfile::TempDir,
    base: PathBuf,
    wrapper: PathBuf,
    pids: PidGuard,
    death: PathBuf,
}
/// A wrapper at an absolute path that runs the fake, with the recording it serves named in its environment.
fn install(version: &str, recording: Option<&str>) -> Install {
    let directory = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(directory.path()).unwrap();
    let wrapper = base.join("bin-opencode");
    let pidfile = base.join("server.pid");
    let transcript = recording.map(|name| {
        let row: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/opencode-replay/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
        let path = base.join("transcript.json");
        std::fs::write(&path, json!({"scenario":name,"protocol":row["protocol"],"version":row["version"],"entries":row["transcript"]}).to_string()).unwrap();
        path
    });
    let death = base.join("death.fifo");
    let path = std::ffi::CString::new(death.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let mut script = format!("#!/bin/sh\nexport OPENCODE_FIXTURE_VERSION={version}\nexport OPENCODE_FIXTURE_PIDFILE={}\nexport OPENCODE_FIXTURE_DEATHPIPE={}\n", pidfile.display(), death.display());
    if let Some(transcript) = &transcript {
        script.push_str(&format!("export OPENCODE_FIXTURE_TRANSCRIPT={}\n", transcript.display()));
    }
    script.push_str(&format!("exec python3 {} \"$@\"\n", opencode_support::FAKE));
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(base.join("home")).unwrap();
    Install { _directory: directory, base, wrapper, pids: PidGuard(pidfile), death }
}
fn settings(install: &Install, instance: Value) -> t3_contracts::ServerSettings {
    let home = install.base.join("home");
    let mut opencode = json!({"driver":"opencode","environment":[{"name":"HOME","value":home},{"name":"XDG_DATA_HOME","value":home.join("share")},{"name":"OPENCODE_API_KEY","value":""}]});
    for (key, value) in instance.as_object().unwrap() {
        opencode[key] = value.clone();
    }
    serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},"opencode":opencode}})).unwrap()
}
fn snapshot(providers: &ProviderRegistry) -> Value {
    providers.snapshots().into_iter().find(|snapshot| snapshot["driver"] == "opencode").expect("an opencode snapshot")
}

#[tokio::test]
async fn opencode_is_listed_disabled_by_default_and_runs_nothing() {
    let install = install("2.0.18", None);
    // The default settings leave it disabled; a wrapper that would write its pid proves nothing ran.
    let providers = ProviderRegistry::discover(&settings(&install, json!({"config":{"binaryPath":install.wrapper}})), &install.base).await.unwrap();
    let listed = snapshot(&providers);
    assert_eq!((listed["enabled"].clone(), listed["status"].clone(), listed["installed"].clone(), listed["version"].clone()), (json!(false), json!("disabled"), json!(false), Value::Null));
    assert_eq!(listed["message"], "OpenCode is disabled in T3 Code settings.");
    assert_eq!(listed["displayName"], "OpenCode");
    assert!(install.pids.pid().is_none(), "nothing was started");
}

#[tokio::test]
async fn a_missing_executable_is_reported_as_not_installed() {
    let install = install("2.0.18", None);
    let missing = install.base.join("no-such-opencode");
    let providers = ProviderRegistry::discover(&settings(&install, json!({"enabled":true,"config":{"enabled":true,"binaryPath":missing}})), &install.base).await.unwrap();
    let listed = snapshot(&providers);
    assert_eq!((listed["status"].clone(), listed["installed"].clone()), (json!("error"), json!(false)));
    assert_eq!(listed["message"], "OpenCode CLI (`opencode`) is not installed or not on PATH.");
}

#[tokio::test]
async fn a_two_x_install_lists_its_models_with_their_reasoning_options() {
    let install = install("2.0.18", Some("opencode2_simple"));
    let death = install.death.clone();
    // Blocks until the server opens its end, then until that end closes.
    let dead = tokio::task::spawn_blocking(move || std::fs::read(death).map(|_| ()));
    let providers = ProviderRegistry::discover(&settings(&install, json!({"enabled":true,"config":{"enabled":true,"binaryPath":install.wrapper,"customModels":["local/custom"]}})), &install.base).await.unwrap();
    let listed = snapshot(&providers);
    assert_eq!((listed["status"].clone(), listed["installed"].clone(), listed["version"].clone()), (json!("ready"), json!(true), json!("2.0.18")));
    assert_eq!(listed["auth"]["status"], "authenticated");
    assert_eq!(listed["showInteractionModeToggle"], true);
    assert_eq!(listed["supportedRuntimeModes"], json!(["approval-required", "auto-accept-edits", "auto", "full-access"]));
    assert_eq!(listed["slashCommands"], json!([{"name":"compact","description":"Summarize the conversation and reduce context usage"}]));
    let models = listed["models"].as_array().unwrap();
    let space = models.iter().find(|model| model["slug"] == "opencode/space-bunny-free").expect("the server's model");
    assert_eq!(space["name"], "Space Bunny Free");
    assert_eq!(space["isCustom"], false);
    let variants = &space["capabilities"]["optionDescriptors"][0];
    assert_eq!((variants["id"].clone(), variants["currentValue"].clone()), (json!("variant"), json!("medium")));
    assert!(variants["options"].as_array().unwrap().iter().any(|option| option["id"] == "high"));
    // A custom slug is listed after the server's, with the default options.
    let custom = models.last().unwrap();
    assert_eq!((custom["slug"].clone(), custom["isCustom"].clone()), (json!("local/custom"), json!(true)));
    assert_eq!(custom["capabilities"]["optionDescriptors"][1]["id"], "agent");
    // The server it started is gone with the registry that owned it: its pipe reads to the end once it has died.
    assert!(install.pids.alive(), "the spawned server serves while the registry lives");
    drop(providers);
    tokio::time::timeout(std::time::Duration::from_secs(30), dead).await.expect("the server outlived the registry").unwrap().unwrap();
}

#[tokio::test]
async fn a_configured_server_url_is_probed_and_never_spawned() {
    let install = install("2.0.18", Some("opencode2_simple"));
    let row: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/tests/fixtures/opencode-replay/opencode2_simple.json", env!("CARGO_MANIFEST_DIR"))).unwrap()).unwrap();
    let server = opencode_support::FakeServer::start(&install.base, "opencode2_simple", "opencode2-http.sse", "2.0.18", &row["transcript"], Some("hunter2"));
    let wrong = ProviderRegistry::discover(&settings(&install, json!({"enabled":true,"config":{"enabled":true,"binaryPath":install.base.join("unused"),"serverUrl":server.url,"serverPassword":"wrong"}})), &install.base).await.unwrap();
    let listed = snapshot(&wrong);
    assert_eq!(listed["status"], "error");
    assert_eq!(listed["message"], "OpenCode server rejected authentication. Check the server URL and password.");
    assert!(!listed.to_string().contains("wrong") && !listed.to_string().contains("hunter2"));
    let right = ProviderRegistry::discover(&settings(&install, json!({"enabled":true,"config":{"enabled":true,"binaryPath":install.base.join("unused"),"serverUrl":server.url,"serverPassword":"hunter2"}})), &install.base).await.unwrap();
    let listed = snapshot(&right);
    assert_eq!((listed["status"].clone(), listed["version"].clone()), (json!("ready"), json!("2.0.18")));
    // The host's own account is never read for a server the user runs.
    assert!(listed["usageLimits"]["unavailable"]["reason"] == "unsupported" || listed["usageLimits"].is_null());
    assert!(!listed.to_string().contains("hunter2"));
    assert!(install.pids.pid().is_none(), "no process of the instance's own was started");
}

#[tokio::test]
async fn an_unreachable_configured_server_is_named_without_its_credentials() {
    let install = install("2.0.18", None);
    let providers = ProviderRegistry::discover(&settings(&install, json!({"enabled":true,"config":{"enabled":true,"binaryPath":install.base.join("unused"),"serverUrl":"http://user:secret@127.0.0.1:9/base?token=abc","serverPassword":"hunter2"}})), &install.base).await.unwrap();
    let listed = snapshot(&providers);
    assert_eq!(listed["status"], "error");
    let text = listed.to_string();
    assert!(!text.contains("secret") && !text.contains("hunter2") && !text.contains("token=abc"), "{text}");
}
