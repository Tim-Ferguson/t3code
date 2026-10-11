//! `server.updateServer`, `server.updateServerWithProgress` and `server.commitDesktopUpdate` (`ws.ts`, `cloud/selfUpdate.ts`).
//! A remote client asks this server to move to an exact version; how depends on who manages it, and an unconfigured build answers
//! as the original does when updates are unavailable (`Remote updates require the T3 Code background service. ...`, a
//! `ServerSelfUpdateError`).
//!
//! The service is `t3_remote::self_update::ServerSelfUpdate`, with every effect behind a trait the server supplies here:
//! * the desktop app's update runs over the telemetry control channel (`desktop_app_update.rs`);
//! * a server a launcher started (`NODE_CHANNEL_FD`, `T3_SERVICE_LAUNCHER_CONTEXT`) is updated through a pinned runtime the
//!   launcher then activates: the runtime is downloaded from the release feed, proven by its own `__service-preflight`, and the
//!   launcher is asked over its channel to switch (`launcher_ipc.rs`). A server nothing launched answers as an unconfigured build.
use crate::persistence::Store;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use t3_remote::{
    pinned_runtime::{ProcessRunner, ReleaseFetcher},
    self_update::{DesktopAppUpdate, SelfUpdateService, ServerSelfUpdate, ServerSelfUpdateConfig, ServerSelfUpdateError, ServerSelfUpdateInput, ServerSelfUpdateProgressStage, no_handoff_callback, silent_progress},
    service_launcher_client::{LauncherEvent, ServiceLauncherClient, ServiceLauncherHost},
};
use t3_contracts::ServerSelfUpdateCapability;
use tokio::sync::{broadcast, mpsc};

pub const METHODS: [&str; 2] = ["server.updateServer", "server.commitDesktopUpdate"];
pub const WITH_PROGRESS: &str = "server.updateServerWithProgress";

/// The channel of a server nothing launched.
struct NoLauncherChannel(broadcast::Sender<LauncherEvent>);
impl ServiceLauncherHost for NoLauncherChannel {
    fn connected(&self) -> bool {
        false
    }
    fn send(&self, _message: &t3_remote::service_protocol::ServiceLauncherChildMessage) -> Result<(), String> {
        Err("There is no service launcher channel.".into())
    }
    fn subscribe(&self) -> broadcast::Receiver<LauncherEvent> {
        self.0.subscribe()
    }
}

/// What the descriptor advertises about updating (`serverSelfUpdate`, `serverSelfUpdateProgress`, `desktopAppUpdate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateCapabilities {
    pub self_update: Option<ServerSelfUpdateCapability>,
    pub progress: bool,
    pub desktop_app_update: bool,
}

/// The capabilities of a server in `mode` with or without the desktop control channel, and with or without a launcher that manages
/// it. Static, as the original's: the control descriptor is known at bootstrap, and a present one means the app speaks the update
/// protocol.
pub fn capabilities(desktop_mode: bool, control_channel: bool, launcher_managed: bool) -> UpdateCapabilities {
    let self_update = t3_remote::self_update::resolve_server_self_update_capability(desktop_mode, launcher_managed);
    let desktop_app_update = self_update == Some(ServerSelfUpdateCapability::DesktopManaged) && control_channel;
    UpdateCapabilities { self_update, progress: self_update == Some(ServerSelfUpdateCapability::BootService) || desktop_app_update, desktop_app_update }
}

/// Everything a launcher-managed update does outside this process.
pub struct UpdateEffects {
    /// The channel to the launcher that started this server; `None` when nothing did.
    pub launcher: Option<Arc<dyn ServiceLauncherHost>>,
    pub runner: Arc<dyn ProcessRunner>,
    pub feed: Arc<dyn ReleaseFetcher>,
}

impl UpdateEffects {
    /// The real ones: the channel this process was started with, the host's processes and the release feed.
    pub fn host(env: &BTreeMap<String, String>) -> Self {
        let launcher = crate::launcher_ipc::IpcLauncherHost::from_environment(env).map(|host| host as Arc<dyn ServiceLauncherHost>);
        Self { launcher, runner: Arc::new(crate::launcher_ipc::HostProcessRunner), feed: Arc::new(crate::launcher_ipc::HttpReleaseFeed::new()) }
    }
}

/// The service for a server in `desktop_mode`, with the app it may drive and the real effects of a launcher-managed update. `env` is the
/// process environment.
pub fn build(desktop_mode: bool, desktop: Arc<dyn DesktopAppUpdate>, base_dir: PathBuf, db_path: String, env: &BTreeMap<String, String>) -> Result<Arc<dyn SelfUpdateService>, String> {
    build_with(desktop_mode, desktop, base_dir, db_path, env, UpdateEffects::host(env))
}

/// [`build`] with the effects chosen by the caller (a server that has already taken its launcher channel, or a test).
pub fn build_with(desktop_mode: bool, desktop: Arc<dyn DesktopAppUpdate>, base_dir: PathBuf, db_path: String, env: &BTreeMap<String, String>, effects: UpdateEffects) -> Result<Arc<dyn SelfUpdateService>, String> {
    let host = effects.launcher.unwrap_or_else(|| Arc::new(NoLauncherChannel(broadcast::channel(1).0)));
    let launcher = ServiceLauncherClient::new(host, env, env!("CARGO_PKG_VERSION")).map_err(|error| error.to_string())?;
    let config = ServerSelfUpdateConfig {
        desktop_mode,
        base_dir,
        db_path,
        platform: match std::env::consts::OS {
            "macos" => "darwin",
            "windows" => "win32",
            other => other,
        }
        .into(),
        arch: match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => other,
        }
        .into(),
        release_base_url: env.get(t3_remote::cli_release::CLI_RELEASE_BASE_URL_ENV).cloned(),
        env: env.clone(),
    };
    Ok(Arc::new(ServerSelfUpdate::new(config, desktop, Arc::new(launcher), effects.runner, effects.feed)))
}

#[derive(Default)]
struct Slot(std::sync::OnceLock<Arc<dyn SelfUpdateService>>);

/// Installs the server's update service; a server that never installed one answers as an unconfigured build does.
pub fn install(store: &Store, service: Arc<dyn SelfUpdateService>) {
    let _ = store.extension(Slot::default).0.set(service);
}

fn current(store: &Store) -> Option<Arc<dyn SelfUpdateService>> {
    store.extension(Slot::default).0.get().cloned()
}

/// `ServerSelfUpdateError` as the wire carries it.
pub fn wire(error: &ServerSelfUpdateError) -> Value {
    json!({"_tag":"ServerSelfUpdateError","reason":error.reason})
}

fn unconfigured() -> Value {
    wire(&ServerSelfUpdateError::new("Remote updates require the T3 Code background service. Run `t3 service install` on the server machine."))
}

pub async fn command(store: &Store, tag: &str, payload: Value) -> Result<Value, Value> {
    let Some(service) = current(store) else { return Err(unconfigured()) };
    match tag {
        "server.updateServer" => {
            let input: ServerSelfUpdateInput = serde_json::from_value(payload).map_err(|error| json!({"_tag":"SchemaDecodeError","message":error.to_string()}))?;
            let result = service.update(input, silent_progress(), no_handoff_callback()).await.map_err(|error| wire(&error))?;
            serde_json::to_value(result).map_err(|error| json!({"_tag":"NativeServiceError","message":error.to_string()}))
        }
        _ => {
            let request_id = payload["requestId"].as_str().unwrap_or_default().to_owned();
            match service.commit_desktop_update(&request_id, no_handoff_callback()).await {
                Ok(never) => match never {},
                Err(error) => Err(wire(&error)),
            }
        }
    }
}

/// `server.updateServerWithProgress`: a `progress` item per stage the update reaches, then one `complete` item with the result; a
/// failure ends the stream. Dropping the receiver abandons the update where it stands.
pub fn update_with_progress(store: &Store, payload: Value) -> mpsc::Receiver<Result<Value, Value>> {
    let (items, received) = mpsc::channel(8);
    let service = current(store);
    tokio::spawn(async move {
        let Some(service) = service else {
            let _ = items.send(Err(unconfigured())).await;
            return;
        };
        let input: ServerSelfUpdateInput = match serde_json::from_value(payload) {
            Ok(input) => input,
            Err(error) => {
                let _ = items.send(Err(json!({"_tag":"SchemaDecodeError","message":error.to_string()}))).await;
                return;
            }
        };
        let reporter: t3_remote::self_update::ProgressReporter = {
            let items = items.clone();
            Arc::new(move |stage: ServerSelfUpdateProgressStage| {
                let items = items.clone();
                Box::pin(async move {
                    let stage = match stage {
                        ServerSelfUpdateProgressStage::Downloading => "downloading",
                        ServerSelfUpdateProgressStage::Installing => "installing",
                    };
                    items.send(Ok(json!({"type":"progress","stage":stage}))).await.map_err(|_| ServerSelfUpdateError::new("The client stopped listening."))
                })
            })
        };
        // A client that leaves abandons the update where it stands.
        let ran = tokio::select! { ran = service.update(input, reporter, no_handoff_callback()) => ran, _ = items.closed() => return };
        let outcome = match ran {
            Ok(result) => Ok(json!({"type":"complete","result":result})),
            Err(error) => Err(wire(&error)),
        };
        let _ = items.send(outcome).await;
    });
    received
}
