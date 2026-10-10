//! What the Antigravity flows share: a directory holding a stand-in agent and its harness at absolute paths (the
//! agent is `agy_acp_server.par`, the name Google ships), and the settings that enable an instance running it with
//! HOME and every other variable inside the test's temp directory.
#![allow(dead_code)]
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use t3_server::{provider_manage::Management, provider_registry::ProviderRegistry};

pub const CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/antigravity-fake-cli.py");

/// `<root>/bin/agy_acp_server.par` and `<root>/bin/localharness_external`, both executable.
pub fn install_fake_agent(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let agent = bin.join("agy_acp_server.par");
    std::fs::write(&agent, format!("#!/bin/sh\nexec python3 {CLI} \"$@\"\n")).unwrap();
    std::fs::write(bin.join("localharness_external"), "#!/bin/sh\nexit 0\n").unwrap();
    for name in ["agy_acp_server.par", "localharness_external"] {
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    agent
}
/// Settings with every other provider disabled and one enabled Antigravity instance running `agent`.
pub fn settings(root: &Path, agent: &Path, extra_config: Value, environment: Vec<Value>) -> t3_contracts::ServerSettings {
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut config = json!({"enabled":true,"binaryPath":agent});
    for (key, value) in extra_config.as_object().cloned().unwrap_or_default() {
        config[key] = value;
    }
    let mut variables = vec![json!({"name":"HOME","value":home})];
    variables.extend(environment);
    serde_json::from_value(json!({"providerInstances":{
        "codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false},
        "antigravity":{"driver":"antigravity","enabled":true,"config":config,"environment":variables}}}))
    .unwrap()
}
/// A registry whose state (tools, profile) lives in `<root>/state`.
pub async fn registry(settings: &t3_contracts::ServerSettings, root: &Path, cwd: &Path) -> (ProviderRegistry, PathBuf) {
    let state = root.join("state");
    std::fs::create_dir_all(state.join("tools")).unwrap();
    let management = Management::new(Some(state.join("tools")));
    (ProviderRegistry::discover_with_management(settings, cwd, None, management).await.unwrap(), state)
}
