//! Probing a device host over SSH (`device/SshDeviceHost.ts`: `probe`, `bootstrap`, `ownerFor`; `device/sshDeviceScript.ts`).
//!
//! The probe pipes the unchanged remote script (`ssh_device_script.txt`, made by `tools/generate_ssh_device_script.mjs`) into `node` on the
//! host and reads its JSON answer: which platforms the host can run devices for, and which versions of the device tools it has. It
//! starts nothing and installs nothing. The other modes the script has (start, agent-start, stop) belong to a connected host, which this
//! server's single-host device service does not have; they are filled in by [`remote_script`] all the same.
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use t3_contracts::{DeviceHostSummary, DevicePlatformAvailability, DeviceToolVersions, SshDeviceHostConfig};
use t3_remote::ssh::{RunSshOptions, Ssh, SshTarget};

const SCRIPT: &str = include_str!("ssh_device_script.txt");
const ENVIRONMENT: &str = include_str!("ssh_device_environment.txt");

/// What a script run does on the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Probe,
    Start,
    AgentStart,
    StopAgent,
    Stop,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Probe => "probe",
            Mode::Start => "start",
            Mode::AgentStart => "agent-start",
            Mode::StopAgent => "stop-agent",
            Mode::Stop => "stop",
        }
    }
    /// An install can take a long while; a probe or a stop must not.
    fn timeout(self) -> Duration {
        Duration::from_millis(if matches!(self, Mode::Start | Mode::AgentStart) { 1_300_000 } else { 45_000 })
    }
}

/// `DeviceHostError`: a host failed at one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub host_id: String,
    pub step: String,
    pub cause: String,
}
impl std::fmt::Display for HostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Device host {} failed while {}.", self.host_id, self.step)
    }
}
impl std::error::Error for HostError {}

/// `quoteRemoteArg`: single-quoted for the remote shell.
pub fn quote_remote_arg(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// `remoteDeviceScript`: the script text the host's `node` runs, for this owner and mode.
pub fn remote_script(owner: &str, mode: Mode) -> String {
    SCRIPT.replace("\"@@OWNER@@\"", &Value::String(owner.to_owned()).to_string()).replace("\"@@MODE@@\"", &Value::String(mode.name().to_owned()).to_string())
}

/// `ownerFor`: a stable name for this environment's claim on the host, so two servers sharing a host do not stop each other's hub.
pub fn owner_for(state_dir: &Path, host_id: &str) -> String {
    let environment_id = std::fs::read_to_string(state_dir.join("rust-environment-id")).unwrap_or_else(|_| state_dir.to_string_lossy().into_owned());
    let digest = Sha256::digest(format!("{environment_id}\0{}\0{host_id}", state_dir.to_string_lossy()).as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>()[..24].to_owned()
}

fn target_for(config: &SshDeviceHostConfig) -> SshTarget {
    SshTarget { alias: config.target.as_str().to_owned(), hostname: config.target.as_str().to_owned(), username: None, port: config.port.flatten().map(|port| port.0) }
}

fn identity_args(config: &SshDeviceHostConfig) -> Vec<String> {
    match config.identity_file.as_ref().and_then(|file| file.as_ref()) {
        Some(file) => vec!["-i".into(), file.as_str().to_owned()],
        None => vec![],
    }
}

/// `bootstrap`: runs the script in one mode over SSH and returns what it printed.
pub async fn bootstrap(ssh: &Ssh, config: &SshDeviceHostConfig, owner: &str, mode: Mode) -> Result<String, HostError> {
    let command = format!("{ENVIRONMENT}command -v node >/dev/null 2>&1 || {{ echo \"Node is missing from the non-interactive SSH PATH\" >&2; exit 1; }}; exec node");
    let options = RunSshOptions {
        pre_host_args: identity_args(config),
        remote_command_args: vec!["sh".into(), "-c".into(), quote_remote_arg(&command)],
        stdin: Some(remote_script(owner, mode)),
        timeout: Some(mode.timeout()),
        batch_mode: None,
    };
    ssh.run(&target_for(config), options).await.map(|result| result.stdout).map_err(|error| HostError { host_id: config.id.as_str().to_owned(), step: mode.name().to_owned(), cause: error.to_string() })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Probe {
    #[allow(dead_code)]
    node_path: String,
    #[serde(default)]
    tools: Option<DeviceToolVersions>,
    platforms: Vec<DevicePlatformAvailability>,
}

/// `probe`: what the host can run and which device tools it has.
pub async fn probe(ssh: &Ssh, config: &SshDeviceHostConfig, owner: &str) -> Result<DeviceHostSummary, HostError> {
    let stdout = bootstrap(ssh, config, owner, Mode::Probe).await?;
    let failed = |cause: String| HostError { host_id: config.id.as_str().to_owned(), step: "reading probe result".into(), cause };
    let value: Probe = serde_json::from_str(stdout.trim()).map_err(|error| failed(error.to_string()))?;
    let installed = |tool: &t3_contracts::DeviceToolVersion| tool.installed_versions.contains(&tool.required_version);
    let (hub_installed, agent_installed) = value.tools.as_ref().map_or((false, false), |tools| (installed(&tools.hub), installed(&tools.agent)));
    let mut summary = json!({"id":config.id,"label":config.label,"kind":"ssh","hubInstalled":hub_installed,"agentDeviceInstalled":agent_installed,"platforms":value.platforms});
    if let Some(tools) = &value.tools {
        summary["tools"] = serde_json::to_value(tools).map_err(|error| failed(error.to_string()))?;
    }
    serde_json::from_value(summary).map_err(|error| failed(error.to_string()))
}
