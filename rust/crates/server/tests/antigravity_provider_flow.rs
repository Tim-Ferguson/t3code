#![cfg(unix)]
//! The Antigravity driver in the provider list: whether it is enabled, what is installed and which version, what
//! the health check can honestly say about the account (nothing, until a session or a sign-in has proved it), the
//! skills the agent will load, and what a re-probe keeps. The health check never starts the agent: the stand-in
//! writes a launch log whenever it runs, and these tests prove the log stays empty. No real Antigravity runs, and
//! HOME is inside the temp directory.
mod antigravity_support;
use antigravity_support::{install_fake_agent, registry, settings};
use serde_json::{Value, json};
use std::path::Path;
use t3_server::provider_registry::ProviderRegistry;

fn provider(registry: &ProviderRegistry) -> Value {
    registry.snapshots().into_iter().find(|snapshot| snapshot["instanceId"] == "antigravity").unwrap()
}
fn launches(root: &Path) -> bool {
    root.join("launch.jsonl").exists()
}
fn environment(root: &Path) -> Vec<Value> {
    vec![json!({"name":"ANTIGRAVITY_FIXTURE_LAUNCH_LOG","value":root.join("launch.jsonl")})]
}
struct Fixture {
    _directory: tempfile::TempDir,
    root: std::path::PathBuf,
    workspace: std::path::PathBuf,
}
fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    Fixture { _directory: directory, root, workspace }
}

#[tokio::test(flavor = "multi_thread")]
async fn antigravity_is_listed_and_disabled_until_it_is_turned_on() {
    let fixture = fixture();
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false}}})).unwrap();
    let registry = ProviderRegistry::discover(&settings, &fixture.workspace).await.unwrap();
    let snapshot = provider(&registry);
    assert_eq!(snapshot["driver"], "antigravity");
    assert_eq!((snapshot["enabled"].clone(), snapshot["installed"].clone(), snapshot["status"].clone()), (json!(false), json!(false), json!("disabled")));
    assert_eq!(snapshot["message"], "Antigravity is disabled in T3 Code settings.");
    assert_eq!(snapshot["auth"], json!({"status":"unknown","type":"oauth-personal"}));
    assert_eq!(snapshot["setup"], json!({"canAuthenticate":true,"canInstall":true}));
    assert_eq!(snapshot["continuation"]["groupKey"], "antigravity:instance:antigravity");
    assert_eq!(snapshot["models"], json!([]));
    assert_eq!(snapshot["showInteractionModeToggle"], false);
    assert!(registry.antigravity("antigravity").is_err(), "a disabled instance cannot start a session");
    // Listed after Codex and Claude in the presentation order.
    let order: Vec<Value> = registry.snapshots().iter().map(|snapshot| snapshot["driver"].clone()).collect();
    let at = |driver: &str| order.iter().position(|candidate| *candidate == driver).unwrap();
    assert!(at("codex") < at("claudeAgent") && at("claudeAgent") < at("antigravity"), "{order:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_installed_agent_is_reported_without_starting_it_and_its_account_is_unchecked() {
    let fixture = fixture();
    let agent = install_fake_agent(&fixture.root);
    let settings = settings(&fixture.root, &agent, json!({}), environment(&fixture.root));
    let (registry, _) = registry(&settings, &fixture.root, &fixture.workspace).await;
    registry.refresh_instance("antigravity").await.unwrap();
    let snapshot = provider(&registry);
    assert_eq!((snapshot["enabled"].clone(), snapshot["installed"].clone(), snapshot["status"].clone()), (json!(true), json!(true), json!("warning")));
    // An executable outside the managed release names no version.
    assert_eq!(snapshot["version"], "unknown");
    assert_eq!(snapshot["auth"], json!({"status":"unknown","type":"oauth-personal"}));
    assert_eq!(snapshot["message"], "Antigravity is installed. Google account access is not checked yet.");
    assert_eq!(snapshot["versionAdvisory"]["status"], "unknown");
    assert_eq!(snapshot["versionAdvisory"]["canUpdate"], false);
    assert_eq!(snapshot["compatibilityAdvisory"]["status"], "unknown");
    assert!(!launches(&fixture.root), "the health check does not start the agent");
    // It can be selected: a session would run it.
    assert!(registry.antigravity("antigravity").is_ok());
    assert_eq!(registry.driver("antigravity").unwrap(), "antigravity");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_installation_and_an_incomplete_configuration_say_what_is_wrong() {
    let fixture = fixture();
    // A custom executable that is not there.
    let missing = fixture.root.join("nowhere").join("agy_acp_server.par");
    let configured = settings(&fixture.root, &missing, json!({}), environment(&fixture.root));
    let (registry, _) = registry(&configured, &fixture.root, &fixture.workspace).await;
    let snapshot = provider(&registry);
    assert_eq!((snapshot["installed"].clone(), snapshot["status"].clone()), (json!(false), json!("error")));
    assert_eq!(snapshot["message"], "The custom Antigravity executable or its localharness_external sibling is missing or not executable.");
    // An executable without its harness beside it is no better.
    let lonely = fixture.root.join("lonely");
    std::fs::create_dir_all(&lonely).unwrap();
    std::fs::write(lonely.join("agy_acp_server.par"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(lonely.join("agy_acp_server.par"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let configured = settings(&fixture.root, &lonely.join("agy_acp_server.par"), json!({}), environment(&fixture.root));
    let (registry, _) = self::registry(&configured, &fixture.root, &fixture.workspace).await;
    assert_eq!(provider(&registry)["installed"], false);
    // Nothing configured, nothing managed and nothing on the PATH.
    let empty = fixture.root.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let mut variables = environment(&fixture.root);
    variables.push(json!({"name":"PATH","value":empty}));
    let configured = settings(&fixture.root, Path::new(""), json!({"binaryPath":""}), variables);
    let (registry, _) = self::registry(&configured, &fixture.root, &fixture.workspace).await;
    let snapshot = provider(&registry);
    assert_eq!((snapshot["installed"].clone(), snapshot["message"].clone()), (json!(false), json!("Antigravity is not installed. Install it in this environment or set a custom executable path.")));
    // An API key method without a key: installed, but not usable.
    let agent = install_fake_agent(&fixture.root);
    let configured = settings(&fixture.root, &agent, json!({"authMethod":"gemini-api-key"}), environment(&fixture.root));
    let (registry, _) = self::registry(&configured, &fixture.root, &fixture.workspace).await;
    let snapshot = provider(&registry);
    assert_eq!((snapshot["installed"].clone(), snapshot["status"].clone(), snapshot["message"].clone()), (json!(true), json!("error"), json!("Enter a Gemini API key in the Antigravity provider settings.")));
    assert_eq!(snapshot["auth"]["type"], "gemini-api-key");
    for (config, message) in [
        (json!({"authMethod":"oauth-business"}), "Gemini Enterprise needs a GCP project and location in the Antigravity provider settings."),
        (json!({"authMethod":"agent-platform","gcpProject":"p"}), "Agent Platform needs an API key, or a GCP project and location, in the Antigravity provider settings."),
    ] {
        let configured = settings(&fixture.root, &agent, config, environment(&fixture.root));
        let (registry, _) = self::registry(&configured, &fixture.root, &fixture.workspace).await;
        assert_eq!(provider(&registry)["message"], message);
    }
    assert!(!launches(&fixture.root));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_managed_release_is_resolved_with_its_pinned_version() {
    let fixture = fixture();
    let release = "a".repeat(64);
    let directory = fixture.root.join("state/tools/antigravity-acp").join(format!("{}-{}", match std::env::consts::OS { "macos" => "darwin", other => other }, match std::env::consts::ARCH { "aarch64" => "arm64", "x86_64" => "x64", other => other })).join("versions").join(&release);
    std::fs::create_dir_all(&directory).unwrap();
    for (name, contents) in [("agy_acp_server.par", "#!/bin/sh\nexit 1\n"), ("localharness_external", "#!/bin/sh\nexit 1\n")] {
        std::fs::write(directory.join(name), contents).unwrap();
        std::fs::set_permissions(directory.join(name), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    std::fs::write(directory.join(".install-complete.json"), json!({"releaseId":release,"version":"1.3.0","executable":{"name":"agy_acp_server.par","bytes":17},"harness":{"name":"localharness_external","bytes":17}}).to_string()).unwrap();
    std::fs::write(directory.parent().unwrap().parent().unwrap().join("active.json"), json!({"releaseId":release}).to_string()).unwrap();
    let empty = fixture.root.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let mut variables = environment(&fixture.root);
    variables.push(json!({"name":"PATH","value":empty}));
    let configured = settings(&fixture.root, Path::new(""), json!({"binaryPath":""}), variables);
    let (registry, _) = registry(&configured, &fixture.root, &fixture.workspace).await;
    registry.refresh_instance("antigravity").await.unwrap();
    let snapshot = provider(&registry);
    assert_eq!((snapshot["installed"].clone(), snapshot["version"].clone(), snapshot["status"].clone()), (json!(true), json!("1.3.0"), json!("warning")));
    assert_eq!(snapshot["versionAdvisory"]["currentVersion"], "1.3.0");
    // A record that disagrees with the files is an incomplete installation, not a guess.
    std::fs::write(directory.join("agy_acp_server.par"), "short").unwrap();
    let snapshot = registry.refresh_instance("antigravity").await.unwrap().into_iter().find(|snapshot| snapshot["instanceId"] == "antigravity").unwrap();
    assert_eq!((snapshot["installed"].clone(), snapshot["message"].clone()), (json!(false), json!("The managed Antigravity runtime is incomplete. Reinstall it.")));
    assert!(!launches(&fixture.root));
}

#[tokio::test(flavor = "multi_thread")]
async fn skills_come_from_the_agents_own_roots_and_each_workspace_sees_its_own() {
    let fixture = fixture();
    let agent = install_fake_agent(&fixture.root);
    let write = |path: &Path, name: &str, description: &str| {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(path.join("SKILL.md"), format!("---\nname: {name}\ndescription: {description}\n---\nbody\n")).unwrap();
    };
    write(&fixture.root.join("home/.gemini/config/skills/review"), "review", "Review a change");
    write(&fixture.workspace.join(".agents/skills/local"), "local", "Only here");
    let other = fixture.root.join("other");
    write(&other.join(".agent/skills/elsewhere"), "elsewhere", "Over there");
    let settings = settings(&fixture.root, &agent, json!({}), environment(&fixture.root));
    let (registry, _) = registry(&settings, &fixture.root, &fixture.workspace).await;
    let names = |skills: &Value| skills.as_array().unwrap().iter().map(|skill| skill["name"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!(names(&provider(&registry)["skills"]), ["local", "review"]);
    let scope = |skills: &Value, name: &str| skills.as_array().unwrap().iter().find(|skill| skill["name"] == name).unwrap()["scope"].clone();
    assert_eq!((scope(&provider(&registry)["skills"], "review"), scope(&provider(&registry)["skills"], "local")), (json!("user"), json!("project")));
    registry.refresh_workspace("antigravity", other.to_str().unwrap(), true).await.unwrap();
    let snapshot = provider(&registry);
    let workspace = snapshot["workspaceSnapshots"].as_array().unwrap().iter().find(|entry| entry["cwd"] == other.to_str().unwrap()).unwrap().clone();
    assert_eq!(names(&workspace["skills"]), ["elsewhere", "review"]);
    assert!(!launches(&fixture.root));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_re_probe_keeps_what_a_session_proved_unless_the_installation_is_gone_or_the_account_signed_out() {
    let fixture = fixture();
    let agent = install_fake_agent(&fixture.root);
    let settings = settings(&fixture.root, &agent, json!({}), environment(&fixture.root));
    let (registry, _) = registry(&settings, &fixture.root, &fixture.workspace).await;
    let started = json!({"version":"1.3.0","setup":{"configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"gemini-3.8-flash-low","options":[{"value":"gemini-3.8-flash-high","name":"High"},{"value":"gemini-3.8-flash-low","name":"Low"}]}]}});
    registry.antigravity_update("antigravity", |_, snapshot, now| t3_server::antigravity_instance::session_started(snapshot, &started, None, "Google account", "oauth-personal", now));
    assert_eq!(provider(&registry)["auth"]["status"], "authenticated");
    let again = registry.refresh_instance("antigravity").await.unwrap().into_iter().find(|snapshot| snapshot["instanceId"] == "antigravity").unwrap();
    assert_eq!((again["status"].clone(), again["auth"]["status"].clone(), again.get("message").cloned()), (json!("ready"), json!("authenticated"), None));
    assert_eq!(again["models"].as_array().unwrap().len(), 2);
    // The default model is the manifest's, and the older one is folded under the legacy section only if it is not current.
    assert!(again["models"].as_array().unwrap().iter().any(|model| model["slug"] == "gemini-3.8-flash-high" && model["isDefault"] == true));
    // Sign-out forgets it, and a re-probe does not bring it back.
    registry.antigravity_update("antigravity", |_, snapshot, now| t3_server::antigravity_instance::account_cleared(snapshot, now));
    let again = registry.refresh_instance("antigravity").await.unwrap().into_iter().find(|snapshot| snapshot["instanceId"] == "antigravity").unwrap();
    assert_eq!((again["auth"]["status"].clone(), again["models"].clone()), (json!("unauthenticated"), json!([])));
    assert_eq!(again["message"], "Sign in with Google to use Antigravity.");
    // A gone installation is never covered for.
    registry.antigravity_update("antigravity", |_, snapshot, now| t3_server::antigravity_instance::session_started(snapshot, &started, None, "Google account", "oauth-personal", now));
    std::fs::remove_file(fixture.root.join("bin/agy_acp_server.par")).unwrap();
    let again = registry.refresh_instance("antigravity").await.unwrap().into_iter().find(|snapshot| snapshot["instanceId"] == "antigravity").unwrap();
    assert_eq!((again["installed"].clone(), again["status"].clone(), again["auth"]["status"].clone()), (json!(false), json!("error"), json!("unknown")));
}
