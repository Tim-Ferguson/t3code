//! Server-owned live settings consumers. A stalled control pipe or provider
//! discovery cannot prevent shutdown from cancelling this subscription owner.
use crate::{
    desktop_telemetry::DesktopTelemetryReceiver,
    provider_registry::ProviderRegistry,
    server_settings::{SettingsError, SettingsService},
    terminal_manager::TerminalManager,
};
use std::path::PathBuf;
use tokio::sync::{Mutex, watch};
pub struct SettingsRuntime {
    stop: watch::Sender<bool>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl SettingsRuntime {
    pub async fn start(
        service: &SettingsService,
        registry: ProviderRegistry,
        cwd: PathBuf,
        terminals: TerminalManager,
        desktop: DesktopTelemetryReceiver,
        mode: String,
    ) -> Result<Self, SettingsError> {
        let mut updates = service.subscribe().await?;
        let mut seed = Some(updates.snapshot.clone());
        let (stop, mut stopped) = watch::channel(false);
        let (initialized, ready) = tokio::sync::oneshot::channel();
        struct Startup(watch::Sender<bool>, bool);
        impl Drop for Startup {
            fn drop(&mut self) {
                if self.1 {
                    self.0.send_replace(true);
                }
            }
        }
        let mut startup = Startup(stop.clone(), true);
        let worker = tokio::spawn(async move {
            let mut initialized = Some(initialized);
            loop {
                let next = if let Some(seed) = seed.take() {
                    seed
                } else {
                    tokio::select! {
                        biased;
                        _=stopped.wait_for(|stop|*stop)=>break,
                        next=updates.recv()=>match next {Some(next)=>next,None=>break},
                    }
                };
                terminals.update_settings(&next);
                tokio::select! {
                    biased;
                    _=stopped.wait_for(|stop|*stop)=>break,
                    _=async {
                        if registry.settings()!=next {
                            if let Err(error)=registry.reconfigure(&next,&cwd).await {tracing::warn!(error=%error,"failed to refresh providers after settings change");}
                        }
                        let intervals=crate::desktop_telemetry_bootstrap::options_for_settings(&mode,&next);
                        if let Err(error)=desktop.set_host_power_intervals(intervals.active_interval_ms,intervals.idle_interval_ms).await {tracing::warn!(error=%error,"failed to update desktop host-power intervals");}
                    }=>{},
                }
                if let Some(initialized) = initialized.take() {
                    let _ = initialized.send(());
                }
            }
        });
        ready
            .await
            .map_err(|_| SettingsError::stopped(service.settings_path()))?;
        startup.1 = false;
        Ok(Self {
            stop,
            worker: Mutex::new(Some(worker)),
        })
    }
    pub async fn shutdown(&self) {
        self.stop.send_replace(true);
        let mut worker = self.worker.lock().await;
        if let Some(handle) = worker.as_mut() {
            let _ = handle.await;
        }
        *worker = None;
    }
}
impl Drop for SettingsRuntime {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        server_secret_store::ServerSecretStore,
        server_settings::SettingsOptions,
        terminal_manager::{TerminalManagerOptions, TerminalSubscription},
    };
    use serde_json::{Value, json};
    use std::time::Duration;
    use t3_contracts::ServerSettings;
    fn settings(value: &str) -> ServerSettings {
        serde_json::from_value(json!({"providers":{"codex":{"enabled":false},"claudeAgent":{"enabled":false},"cursor":{"enabled":false},"grok":{"enabled":false},"pi":{"enabled":false},"opencode":{"enabled":false},"antigravity":{"enabled":false}},"providerInstances":{"fixture":{"driver":"codex","enabled":false,"environment":[{"name":"VALUE","value":value,"sensitive":true}]}}})).unwrap()
    }
    async fn service(directory: &std::path::Path, settings: &ServerSettings) -> SettingsService {
        let path = directory.join("settings.json");
        std::fs::write(&path, serde_json::to_vec(settings).unwrap()).unwrap();
        let mut options = SettingsOptions::file(
            path,
            ServerSecretStore::open(directory.join("secrets")).unwrap(),
        );
        options.watch = false;
        SettingsService::start(options).await.unwrap()
    }
    async fn data(stream: &mut TerminalSubscription, needle: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let event = stream.recv().await.unwrap().unwrap();
                if event["data"]
                    .as_str()
                    .is_some_and(|value| value.contains(needle))
                    || event["snapshot"]["history"]
                        .as_str()
                        .is_some_and(|value| value.contains(needle))
                {
                    break;
                }
            }
        })
        .await
        .expect("owned terminal output milestone");
    }
    #[tokio::test]
    async fn startup_seed_catches_change_after_config_snapshot_and_updates_actual_terminal_environment()
     {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let initial = settings("initial");
        let registry = ProviderRegistry::discover(&initial, directory.path())
            .await
            .unwrap();
        let service = service(directory.path(), &initial).await;
        let script = directory.path().join("shell.py");
        std::fs::write(&script,"#!/usr/bin/env python3\nimport os,sys,tty\ntty.setraw(0)\nos.write(1,b'READY\\n')\nfor line in sys.stdin:\n os.write(1,('VALUE='+os.environ.get('VALUE','')+'\\n').encode())\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut options = TerminalManagerOptions::host(directory.path().join("logs"), &initial);
        options.shell = Some(script.to_string_lossy().into());
        options.kill_grace = Duration::ZERO;
        let terminals = TerminalManager::new(options).await.unwrap();
        // This commit occurs after config/terminal creation, before observer
        // subscription. No further change can rescue an ignored seed.
        let changed = settings("seed");
        service
            .update(
                serde_json::from_value(json!({"providerInstances":changed.provider_instances}))
                    .unwrap(),
            )
            .await
            .unwrap();
        let desktop = DesktopTelemetryReceiver::new(
            crate::desktop_telemetry::DesktopTelemetryOptions::unavailable("web"),
        )
        .await;
        let runtime = SettingsRuntime::start(
            &service,
            registry.clone(),
            directory.path().into(),
            terminals.clone(),
            desktop.clone(),
            "web".into(),
        )
        .await
        .unwrap();
        let latest: Value = serde_json::to_value(registry.settings()).unwrap();
        assert_eq!(
            latest["providerInstances"]["fixture"]["environment"][0]["value"],
            "seed"
        );
        terminals.open(serde_json::from_value(json!({"threadId":"seed-thread","terminalId":"term-1","cwd":directory.path(),"providerInstanceId":"fixture"})).unwrap()).await.unwrap();
        let mut stream = terminals
            .observe(
                serde_json::from_value(json!({"threadId":"seed-thread","terminalId":"term-1"}))
                    .unwrap(),
            )
            .await
            .unwrap();
        data(&mut stream, "READY").await;
        terminals
            .write(
                serde_json::from_value(
                    json!({"threadId":"seed-thread","terminalId":"term-1","data":"ENV\n"}),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        data(&mut stream, "VALUE=seed").await;
        runtime.shutdown().await;
        terminals.shutdown().await;
        desktop.shutdown().await;
        service.shutdown().await;
    }
    #[tokio::test]
    async fn shutdown_cancels_live_interval_write_to_stalled_owned_pipe() {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
        let directory = tempfile::tempdir().unwrap();
        let initial = settings("initial");
        let registry = ProviderRegistry::discover(&initial, directory.path())
            .await
            .unwrap();
        let service = service(directory.path(), &initial).await;
        let terminals = TerminalManager::new(TerminalManagerOptions::host(
            directory.path().join("logs"),
            &initial,
        ))
        .await
        .unwrap();
        let (control, peer) = tokio::io::duplex(1);
        let mut peer = BufReader::new(peer);
        let mut options = crate::desktop_telemetry::DesktopTelemetryOptions::unavailable("desktop");
        options.control = Some((99, Box::pin(control)));
        let opening = tokio::spawn(DesktopTelemetryReceiver::new(options));
        let mut initial_frame = String::new();
        peer.read_line(&mut initial_frame).await.unwrap();
        let desktop = opening.await.unwrap();
        let starting = tokio::spawn({
            let service = service.clone();
            let terminals = terminals.clone();
            let desktop = desktop.clone();
            let cwd = directory.path().to_path_buf();
            async move {
                SettingsRuntime::start(
                    &service,
                    registry,
                    cwd,
                    terminals,
                    desktop,
                    "desktop".into(),
                )
                .await
            }
        });
        let mut seed_frame = String::new();
        tokio::time::timeout(Duration::from_secs(3), peer.read_line(&mut seed_frame))
            .await
            .unwrap()
            .unwrap();
        let seed: Value = serde_json::from_str(&seed_frame).unwrap();
        assert_eq!(seed["type"], "setHostPowerIntervals");
        assert_eq!(seed["idleIntervalMs"], 300000);
        let runtime = tokio::time::timeout(Duration::from_secs(3), starting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        service
            .update(
                serde_json::from_value(json!({"backgroundActivity":{"profile":"battery-saver"}}))
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut first = [0];
        tokio::time::timeout(Duration::from_secs(3), peer.read_exact(&mut first))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, *b"{"); // The changed control write entered, then fills the one-byte pipe.
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(runtime.shutdown(), desktop.shutdown());
        })
        .await
        .unwrap();
        terminals.shutdown().await;
        service.shutdown().await;
    }
}
