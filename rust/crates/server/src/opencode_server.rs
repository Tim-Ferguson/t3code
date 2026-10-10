//! The OpenCode server behind one provider instance: which generation it is (`opencodeVersionProbe.ts`), how a
//! configured or spawned server is verified (`OpenCode2Server.ts`, `verifyOpenCodeServerVersion`), and the owner that
//! starts `opencode serve` on loopback on demand, lends it to sessions and stops it, with its process group, once
//! nothing has used it for a while (`OpenCodeServerOwner.ts`).
use crate::{opencode_api::{Api, ApiError}, scheduled_tasks::SchedulerClock};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, sync::watch};

/// `MINIMUM_OPENCODE_VERSION`: the oldest 1.x T3 Code drives.
pub const MINIMUM_VERSION: &str = "1.14.19";
const VERSION_TIMEOUT: Duration = Duration::from_secs(4);
const SERVER_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// `OPENCODE_SERVER_IDLE_TTL`.
pub const IDLE_TTL_MS: i64 = 30_000;
/// The startup output read while looking for the listening line.
const STARTUP_OUTPUT_BYTES: usize = 64 * 1024;
const GROUP_GRACE: Duration = Duration::from_secs(1);

/// OpenCode 1.x (`opencode-ai`, the SDK over `/global/health`) or 2.x (`@opencode/cli`, `/api/info`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generation {
    V1,
    V2,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probed {
    pub generation: Generation,
    pub version: String,
}
/// A failure with the text a status message shows. Never built from a URL, a password or process output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerError(pub String);
impl std::fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for ServerError {}
fn failed(detail: impl Into<String>) -> ServerError {
    ServerError(detail.into())
}

fn parse_triplet(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}
/// `compareSemverVersions` for release versions.
pub fn version_at_least(version: &str, minimum: &str) -> bool {
    matches!((parse_triplet(version), parse_triplet(minimum)), (Some(version), Some(minimum)) if version >= minimum)
}
fn probed(version: Option<&str>) -> Option<Probed> {
    let version = version?;
    let (major, ..) = parse_triplet(version)?;
    Some(Probed { generation: if major >= 2 { Generation::V2 } else { Generation::V1 }, version: version.to_owned() })
}
/// `opencode --version` prints `1.18.32` on 1.x and `opencode v2.0.18` on 2.x.
pub fn classify_cli_version(output: &str) -> Option<Probed> {
    probed(crate::claude_instance::parse_version(output).as_deref())
}

/// 2.x answers `/api/info` and 1.x answers `/global/health`; each serves its web UI's HTML with a 200 on the other's
/// path, so only a JSON body counts. Both answer a wrong password with a 401 on either path.
enum Classified {
    Found(Probed),
    Unauthorized,
    Other,
}
fn classify_response(path: &str, status: u16, content_type: Option<&str>, body: &str) -> Classified {
    if status == 401 {
        return Classified::Unauthorized;
    }
    let media = content_type.and_then(|value| value.split(';').next()).map(|value| value.trim().to_lowercase());
    if status != 200 || media.as_deref() != Some("application/json") {
        return Classified::Other;
    }
    let Ok(body) = serde_json::from_str::<Value>(body) else { return Classified::Other };
    let version = match path {
        // 2.x's own CLI decodes `{version, pid}`; requiring both keeps unrelated JSON out.
        "/api/info" => body["version"].as_str().filter(|_| body["pid"].is_i64() || body["pid"].is_u64()),
        _ => body["version"].as_str().filter(|_| body["healthy"] == true),
    };
    probed(version).map_or(Classified::Other, Classified::Found)
}
/// Probes a configured server: `/api/info`, then `/global/health`, keeping a path prefix and query of the URL.
pub async fn probe_server(server_url: &str, password: &str) -> Result<Probed, ServerError> {
    let parsed = url::Url::parse(server_url.trim()).ok().filter(|parsed| matches!(parsed.scheme(), "http" | "https")).ok_or_else(|| failed("The OpenCode server URL is not a valid http:// or https:// URL."))?;
    for path in ["/api/info", "/global/health"] {
        let mut target = parsed.clone();
        target.set_path(&format!("{}{path}", parsed.path().trim_end_matches('/')));
        let base = target.as_str().to_owned();
        // `Api` joins a path onto its base, so the probe URL is the base of a request for the empty path.
        let api = Api::new(&base, Some(password)).map_err(|_| failed("The OpenCode server URL is not a valid http:// or https:// URL."))?;
        let answer = match tokio::time::timeout(SERVER_PROBE_TIMEOUT, api.probe("", SERVER_PROBE_TIMEOUT)).await {
            Err(_) => return Err(failed("Timed out while checking the OpenCode server version.")),
            Ok(Err(_)) => return Err(failed("Couldn't reach the OpenCode server.")),
            Ok(Ok(answer)) => answer,
        };
        match classify_response(path, answer.0, answer.1.as_deref(), &answer.2) {
            Classified::Unauthorized => return Err(failed("401 Unauthorized: the OpenCode server rejected the password.")),
            Classified::Found(found) => return Ok(found),
            Classified::Other => {}
        }
    }
    Err(failed(format!("The server did not identify itself as OpenCode. T3 Code requires OpenCode v{MINIMUM_VERSION} or newer.")))
}
/// Runs `binary --version` for the generation and version of a local install.
pub async fn probe_binary(binary: &std::path::Path, environment: &HashMap<String, String>) -> Result<Probed, ServerError> {
    let mut command = Command::new(binary);
    command.arg("--version").env_clear().envs(environment).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let run = async {
        let mut child = command.spawn()?;
        let (mut out, mut err) = (String::new(), String::new());
        let (mut stdout, mut stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let _ = tokio::join!(stdout.read_to_string(&mut out), stderr.read_to_string(&mut err));
        child.wait().await.map(|_| format!("{out}\n{err}"))
    };
    let text = match tokio::time::timeout(VERSION_TIMEOUT, run).await {
        Err(_) => return Err(failed("OpenCode CLI version probe timed out after 4 seconds.")),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => return Err(failed("spawn opencode ENOENT")),
        Ok(Err(_)) => return Err(failed("Failed to execute OpenCode CLI.")),
        Ok(Ok(text)) => text,
    };
    classify_cli_version(&text).ok_or_else(|| failed(format!("Unable to determine OpenCode version from `opencode --version` output. T3 Code requires OpenCode v{MINIMUM_VERSION} or newer.")))
}

/// Confirms a server is an authenticated OpenCode 2 server through `/api/info` and returns its version. Details are
/// fixed text: they reach clients, and a `serverUrl` can carry credentials.
pub async fn verify_v2(api: &Api) -> Result<String, ServerError> {
    match tokio::time::timeout(SERVER_PROBE_TIMEOUT, api.call(reqwest::Method::GET, "/api/info", &[], None, Some(SERVER_PROBE_TIMEOUT))).await {
        Err(_) => Err(failed("Timed out waiting for the OpenCode server.")),
        Ok(Ok(info)) => info["version"].as_str().map(str::to_owned).ok_or_else(|| failed("The server is not an OpenCode 2 server.")),
        Ok(Err(error)) => Err(failed(match &error {
            ApiError::Transport(_) => "Could not reach the OpenCode server.".to_owned(),
            ApiError::Status { status: 401, .. } => "The OpenCode server rejected the server password.".to_owned(),
            ApiError::Status { status, .. } => format!("The OpenCode server returned HTTP {status}."),
            _ => "The server is not an OpenCode 2 server.".to_owned(),
        })),
    }
}
/// `verifyOpenCodeServerVersion`: the 1.x health check, which must report a version T3 Code supports.
pub async fn verify_v1(api: &Api) -> Result<String, ServerError> {
    let health = match tokio::time::timeout(SERVER_PROBE_TIMEOUT, api.call(reqwest::Method::GET, "/global/health", &[], None, Some(SERVER_PROBE_TIMEOUT))).await {
        Err(_) => return Err(failed("Timed out while checking the OpenCode server version.")),
        Ok(Err(ApiError::Status { status: 401, .. })) => return Err(failed("401 Unauthorized: the OpenCode server rejected the password.")),
        Ok(Err(_)) => return Err(failed("Couldn't reach the OpenCode server.")),
        Ok(Ok(health)) => health,
    };
    let required = format!("T3 Code requires OpenCode v{MINIMUM_VERSION} or newer.");
    let (true, Some(version)) = (health["healthy"] == true, health["version"].as_str()) else { return Err(failed(format!("OpenCode server returned an invalid health response. {required}"))) };
    if parse_triplet(version).is_none() {
        return Err(failed(format!("OpenCode server returned an invalid version. {required}")));
    }
    if !version_at_least(version, MINIMUM_VERSION) {
        return Err(failed(format!("OpenCode v{version} is too old. Upgrade to v{MINIMUM_VERSION} or newer.")));
    }
    Ok(version.to_owned())
}

/// What a session talks to: the API, the version, and whether T3 Code started the server.
#[derive(Clone)]
pub struct Connection {
    pub api: Api,
    pub generation: Generation,
    pub version: String,
    /// A server the user runs (`serverUrl`): it outlives T3 Code, and may not reach T3 Code's own endpoint.
    pub external: bool,
    /// For a spawned server: the process and whether it still runs. Held so the server is not stopped under a long call.
    pub lease: Option<Arc<Lease>>,
}
impl Connection {
    /// A configured server, verified.
    pub async fn external(server_url: &str, password: &str, generation: Generation) -> Result<Self, ServerError> {
        let api = Api::new(server_url, Some(password)).map_err(|_| failed("The OpenCode server URL is not a valid http:// or https:// URL."))?;
        let version = match generation {
            Generation::V2 => verify_v2(&api).await?,
            Generation::V1 => verify_v1(&api).await?,
        };
        Ok(Self { api, generation, version, external: true, lease: None })
    }
}

/// The settings a spawned server needs.
#[derive(Clone)]
pub struct SpawnConfig {
    pub binary: PathBuf,
    /// The instance's environment on top of the server's own.
    pub environment: HashMap<String, String>,
    pub generation: Generation,
    /// A password fixed by the user's 1.x `serverPassword`; absent, one is generated (2.x always requires one).
    pub password: Option<String>,
}
/// A running `opencode serve`.
pub struct Spawned {
    api: Api,
    version: String,
    generation: Generation,
    exited: watch::Receiver<bool>,
    _group: Group,
}
/// The process group the server leads: dropping it ends the group.
struct Group(Option<u32>);
impl Spawned {
    pub fn is_running(&self) -> bool {
        !*self.exited.borrow()
    }
    /// Resolves once the process has exited.
    pub async fn exit(&self) {
        let mut exited = self.exited.clone();
        let _ = exited.wait_for(|exited| *exited).await;
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        signal_group(self.0, libc::SIGTERM);
        let pid = self.0;
        // SIGKILL follows the grace if the group is still there; without a runtime there is nobody to wait.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    tokio::time::sleep(GROUP_GRACE).await;
                    signal_group(pid, libc::SIGKILL);
                });
            }
            Err(_) => signal_group(pid, libc::SIGKILL),
        }
    }
}
#[cfg(unix)]
fn signal_group(leader: Option<u32>, signal: i32) {
    if let Some(group) = leader.and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: plain signal delivery to the process group this server led; a gone group is an ignored ESRCH.
        unsafe { libc::kill(-group, signal) };
    }
}
#[cfg(not(unix))]
fn signal_group(_: Option<u32>, _: i32) {}

/// `parseServerUrlFromOutput`.
fn listening_url(output: &str) -> Option<String> {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| regex::Regex::new(r"(?i)server listening on\s+(https?://\S+)").unwrap());
    output.lines().find_map(|line| pattern.captures(line).map(|captures| captures[1].to_owned()))
}
/// The address the child announced, only if it is the loopback port T3 Code asked it to bind.
fn loopback_url(announced: &str, port: u16) -> Option<String> {
    let parsed = url::Url::parse(announced).ok()?;
    let host = parsed.host_str()?;
    (matches!(host, "127.0.0.1" | "localhost" | "[::1]") && parsed.port() == Some(port)).then(|| format!("{}://{host}:{port}", parsed.scheme()))
}
fn free_port() -> Result<u16, ServerError> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|_| failed("Failed to find available port."))?;
    listener.local_addr().map(|address| address.port()).map_err(|_| failed("Failed to find available port."))
}
fn generated_password() -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

async fn spawn(config: &SpawnConfig, directory: &std::path::Path) -> Result<Spawned, ServerError> {
    let port = free_port()?;
    let password = config.password.clone().unwrap_or_else(generated_password);
    let mut environment = config.environment.clone();
    match config.generation {
        // 2.x reads `OPENCODE_PASSWORD` over `OPENCODE_SERVER_PASSWORD`; the inherited 1.x variable is dropped so the
        // password T3 Code supplies is the only one in play.
        Generation::V2 => {
            environment.remove("OPENCODE_SERVER_PASSWORD");
            environment.insert("OPENCODE_PASSWORD".into(), password.clone());
        }
        Generation::V1 => {
            if config.password.is_some() {
                environment.insert("OPENCODE_SERVER_PASSWORD".into(), password.clone());
            }
        }
    }
    // Respect a config the caller or the inherited environment provides; setting it unconditionally would hide the
    // user's providers and models.
    environment.entry("OPENCODE_CONFIG_CONTENT".into()).or_insert_with(|| "{}".into());
    let mut command = Command::new(&config.binary);
    command.arg("serve").arg("--hostname=127.0.0.1").arg(format!("--port={port}")).env_clear().envs(&environment).current_dir(directory).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|_| failed("Failed to spawn OpenCode server process."))?;
    let pid = child.id();
    let (mut stdout, mut stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let (exited_tx, exited) = watch::channel(false);
    // The child's lifetime is its waiter's: abort the waiter and the child is killed with it.
    tokio::spawn(async move {
        let _ = child.wait().await;
        exited_tx.send_replace(true);
    });
    // Keep draining stderr; stopping the readers can block OpenCode when its pipes fill.
    tokio::spawn(async move {
        let mut sink = [0u8; 4096];
        while matches!(stderr.read(&mut sink).await, Ok(read) if read > 0) {}
    });
    let group = Group(pid);
    let ready = async {
        let mut seen = String::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = stdout.read(&mut chunk).await.map_err(|_| failed("Failed while waiting for OpenCode server startup."))?;
            if read == 0 {
                return Err(failed("OpenCode server exited before startup completed."));
            }
            seen.push_str(&String::from_utf8_lossy(&chunk[..read]));
            if let Some(url) = listening_url(&seen) {
                return Ok(url);
            }
            if seen.len() > STARTUP_OUTPUT_BYTES {
                seen.drain(..seen.len() - STARTUP_OUTPUT_BYTES);
            }
        }
    };
    let announced = match tokio::time::timeout(STARTUP_TIMEOUT, ready).await {
        Err(_) => return Err(failed("Timed out waiting for OpenCode server start after 30000ms.")),
        Ok(announced) => announced?,
    };
    // Startup output is no longer needed; later output is discarded.
    tokio::spawn(async move {
        let mut sink = [0u8; 4096];
        while matches!(stdout.read(&mut sink).await, Ok(read) if read > 0) {}
    });
    let url = loopback_url(&announced, port).ok_or_else(|| failed("The OpenCode server announced an address T3 Code did not ask for."))?;
    let api = Api::new(&url, Some(&password)).map_err(|_| failed("Failed to start the OpenCode server."))?;
    let version = match config.generation {
        Generation::V2 => verify_v2(&api).await?,
        Generation::V1 => verify_v1(&api).await?,
    };
    Ok(Spawned { api, version, generation: config.generation, exited, _group: group })
}

struct OwnerState {
    server: Option<Arc<Spawned>>,
    borrowers: usize,
    idle: Option<tokio::task::JoinHandle<()>>,
}
struct OwnerInner {
    config: SpawnConfig,
    state: Mutex<OwnerState>,
    /// One start at a time.
    starting: tokio::sync::Mutex<()>,
}
/// Owns the lazy local server one provider instance shares: started on first use, lent to each session, and stopped
/// once it has been unused for [`IDLE_TTL_MS`] on the registry's clock, or when the owner is dropped.
#[derive(Clone)]
pub struct ServerOwner(Arc<OwnerInner>);
impl ServerOwner {
    pub fn new(config: SpawnConfig) -> Self {
        Self(Arc::new(OwnerInner { config, state: Mutex::new(OwnerState { server: None, borrowers: 0, idle: None }), starting: tokio::sync::Mutex::new(()) }))
    }
    pub fn generation(&self) -> Generation {
        self.0.config.generation
    }
    /// Lends the server, starting it first when none runs (a server that died is started again).
    pub async fn lease(&self, directory: &std::path::Path, clock: Arc<dyn SchedulerClock>) -> Result<Connection, ServerError> {
        let _starting = self.0.starting.lock().await;
        let taken = {
            let mut state = self.0.state.lock().unwrap();
            if let Some(idle) = state.idle.take() {
                idle.abort();
            }
            match state.server.clone().filter(|server| server.is_running()) {
                Some(server) => {
                    state.borrowers += 1;
                    Some(server)
                }
                None => {
                    state.server = None;
                    None
                }
            }
        };
        let server = match taken {
            Some(server) => server,
            None => {
                let started = Arc::new(Box::pin(spawn(&self.0.config, directory)).await?);
                let mut state = self.0.state.lock().unwrap();
                state.server = Some(started.clone());
                state.borrowers = 1;
                started
            }
        };
        let lease = Arc::new(Lease { owner: Arc::downgrade(&self.0), server: server.clone(), clock });
        Ok(Connection { api: server.api.clone(), generation: server.generation, version: server.version.clone(), external: false, lease: Some(lease) })
    }
}
/// A borrowed server. The last lease dropped starts the idle window.
pub struct Lease {
    owner: std::sync::Weak<OwnerInner>,
    server: Arc<Spawned>,
    clock: Arc<dyn SchedulerClock>,
}
impl Lease {
    /// Resolves once the server this lease borrowed has exited.
    pub async fn exited(&self) {
        self.server.exit().await;
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let Some(owner) = self.owner.upgrade() else { return };
        let mut state = owner.state.lock().unwrap();
        if !state.server.as_ref().is_some_and(|server| Arc::ptr_eq(server, &self.server)) {
            return;
        }
        state.borrowers = state.borrowers.saturating_sub(1);
        if state.borrowers > 0 {
            return;
        }
        // The idle window must not keep the server alive on its own: it names the server by address only.
        let (clock, server, weak) = (self.clock.clone(), Arc::as_ptr(&self.server) as usize, Arc::downgrade(&owner));
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            state.server = None;
            return;
        };
        state.idle = Some(runtime.spawn(async move {
            clock.sleep_until(clock.now_ms() + IDLE_TTL_MS).await;
            let Some(owner) = weak.upgrade() else { return };
            let mut state = owner.state.lock().unwrap();
            if state.borrowers == 0 && state.server.as_ref().is_some_and(|current| Arc::as_ptr(current) as usize == server) {
                state.server = None;
                state.idle = None;
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_cli_version_names_the_generation() {
        assert_eq!(classify_cli_version("1.18.32\n"), Some(Probed { generation: Generation::V1, version: "1.18.32".into() }));
        assert_eq!(classify_cli_version("opencode v2.0.18"), Some(Probed { generation: Generation::V2, version: "2.0.18".into() }));
        assert_eq!(classify_cli_version("no version here"), None);
    }
    #[test]
    fn a_server_is_known_by_the_json_only_it_answers() {
        let json = Some("application/json; charset=utf-8");
        assert!(matches!(classify_response("/api/info", 200, json, r#"{"version":"2.0.18","pid":42}"#), Classified::Found(Probed { generation: Generation::V2, .. })));
        // 1.x serves its web UI as HTML with a 200 on 2.x's path.
        assert!(matches!(classify_response("/api/info", 200, Some("text/html"), "<html>"), Classified::Other));
        assert!(matches!(classify_response("/api/info", 200, json, r#"{"version":"2.0.18"}"#), Classified::Other));
        assert!(matches!(classify_response("/global/health", 200, json, r#"{"healthy":true,"version":"1.14.39"}"#), Classified::Found(Probed { generation: Generation::V1, .. })));
        assert!(matches!(classify_response("/global/health", 401, None, ""), Classified::Unauthorized));
    }
    #[test]
    fn only_the_loopback_port_that_was_asked_for_is_accepted() {
        assert_eq!(listening_url("opencode server listening on http://127.0.0.1:4096\n").as_deref(), Some("http://127.0.0.1:4096"));
        assert_eq!(loopback_url("http://127.0.0.1:4096", 4096).as_deref(), Some("http://127.0.0.1:4096"));
        assert_eq!(loopback_url("http://127.0.0.1:4097", 4096), None);
        assert_eq!(loopback_url("http://evil.example:4096", 4096), None);
    }
    #[test]
    fn the_minimum_version_is_enforced_for_one_x() {
        assert!(version_at_least("1.14.19", MINIMUM_VERSION));
        assert!(version_at_least("1.15.0", MINIMUM_VERSION));
        assert!(!version_at_least("1.14.18", MINIMUM_VERSION));
    }
}
