//! Consent, discovery, device boot/session state and scoped subscriptions.
use crate::{
    local_device_host::{DeviceHostReady, DevicePhase, LocalDeviceHost},
    server_settings::SettingsService,
};
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use t3_contracts::*;
use tokio::sync::{mpsc, watch};
pub const DEVICE_HUB_ROUTE_PREFIX: &str = "/api/device-hub";
struct State {
    snapshot: DeviceServiceState,
    listeners: BTreeMap<u64, mpsc::UnboundedSender<DeviceServiceState>>,
    next_listener: u64,
}
struct Inner {
    host: LocalDeviceHost,
    settings: SettingsService,
    state: Mutex<State>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
    writes: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    write_cleanup: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    #[cfg(test)]
    write_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    target_ready_hook: Mutex<Option<Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>>>,
    stopped: watch::Sender<bool>,
    client: reqwest::Client,
}
#[derive(Clone)]
pub struct DeviceService(Arc<Inner>);
pub struct DeviceSubscription {
    pub snapshot: DeviceServiceState,
    receiver: mpsc::UnboundedReceiver<DeviceServiceState>,
    stopped: watch::Receiver<bool>,
    owner: Weak<Inner>,
    id: u64,
}
impl DeviceSubscription {
    pub async fn recv(&mut self) -> Option<DeviceServiceState> {
        tokio::select! {biased;_=self.stopped.wait_for(|stopped|*stopped)=>None,value=self.receiver.recv()=>value}
    }
}
impl Drop for DeviceSubscription {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.state.lock().unwrap().listeners.remove(&self.id);
        }
    }
}
struct BootingCleanup {
    owner: Weak<Inner>,
    host: DeviceHostId,
    device: DeviceId,
}
impl Drop for BootingCleanup {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            DeviceService(owner).publish(|state| {
                if let Some(Some(entries)) = state.booting_devices.as_mut() {
                    entries.retain(|entry| entry.host_id != self.host || entry.id != self.device);
                }
            });
        }
    }
}
fn host_error(host_id: DeviceHostId, reason: impl Into<String>) -> DeviceError {
    DeviceError::DeviceHostUnavailableError(DeviceHostUnavailableError {
        tag: DeviceHostUnavailableErrorTag::DeviceHostUnavailableError,
        host_id,
        reason: reason.into(),
        cause: None,
    })
}
fn operation_error(
    operation: &str,
    reason: DeviceOperationFailureReason,
    cause: Value,
) -> DeviceError {
    DeviceError::DeviceOperationError(DeviceOperationError {
        tag: DeviceOperationErrorTag::DeviceOperationError,
        operation: operation.into(),
        reason,
        exit_code: None,
        cause,
    })
}
fn not_found(host_id: DeviceHostId, device_id: DeviceId) -> DeviceError {
    DeviceError::DeviceNotFoundError(DeviceNotFoundError {
        tag: DeviceNotFoundErrorTag::DeviceNotFoundError,
        host_id,
        device_id,
    })
}
fn local() -> DeviceHostId {
    DeviceHostId::new("local").unwrap()
}
impl DeviceService {
    pub async fn new(
        settings: SettingsService,
        host: LocalDeviceHost,
    ) -> Result<Self, DeviceError> {
        let current = settings.snapshot().await.map_err(|error| {
            operation_error(
                "settings",
                DeviceOperationFailureReason::SettingsFailed,
                json!(error.to_string()),
            )
        })?;
        let summary = host.summary().await;
        let (stopped, _) = watch::channel(false);
        Ok(Self(Arc::new(Inner {
            host,
            settings,
            state: Mutex::new(State {
                snapshot: DeviceServiceState {
                    supports_host_retry: Some(Some(true)),
                    supports_tool_update: Some(Some(true)),
                    supports_tool_inspection: Some(Some(true)),
                    hosts: vec![summary],
                    host_status: if current.enable_device_support {
                        DeviceHostStatus::Idle
                    } else {
                        DeviceHostStatus::Disabled
                    },
                    host_status_detail: None,
                    host_statuses: BTreeMap::new(),
                    devices: Vec::new(),
                    sessions: Vec::new(),
                    booting_devices: None,
                    onboarding_completed: current.device_onboarding_completed,
                    agent_access_enabled: current.enable_agent_device_access,
                    hub_base_path: DEVICE_HUB_ROUTE_PREFIX.into(),
                    revision: SafeInt(0),
                },
                listeners: BTreeMap::new(),
                next_listener: 0,
            }),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            writes: Mutex::new(Vec::new()),
            write_cleanup: tokio::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            write_hook: Mutex::new(None),
            #[cfg(test)]
            target_ready_hook: Mutex::new(None),
            stopped,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        })))
    }
    pub fn snapshot(&self) -> DeviceServiceState {
        self.0.state.lock().unwrap().snapshot.clone()
    }
    pub fn subscribe(&self) -> DeviceSubscription {
        let (receiver, id, snapshot) = {
            let mut state = self.0.state.lock().unwrap();
            let (sender, receiver) = mpsc::unbounded_channel();
            state.next_listener += 1;
            let id = state.next_listener;
            state.listeners.insert(id, sender);
            (receiver, id, state.snapshot.clone())
        };
        DeviceSubscription {
            snapshot,
            receiver,
            stopped: self.0.stopped.subscribe(),
            owner: Arc::downgrade(&self.0),
            id,
        }
    }
    fn publish(&self, update: impl FnOnce(&mut DeviceServiceState)) -> DeviceServiceState {
        let mut state = self.0.state.lock().unwrap();
        if *self.0.stopped.borrow() {
            return state.snapshot.clone();
        }
        update(&mut state.snapshot);
        state.snapshot.revision = SafeInt(state.snapshot.revision.0 + 1);
        let snapshot = state.snapshot.clone();
        state
            .listeners
            .retain(|_, sender| sender.send(snapshot.clone()).is_ok());
        snapshot
    }
    fn status(&self, status: DeviceHostStatus, detail: Option<String>) {
        self.publish(|state| {
            state.host_status = status;
            state.host_status_detail = detail.clone().map(Some);
            state.host_statuses.insert(
                local(),
                DeviceHostState {
                    status,
                    detail: detail.map(Some),
                },
            );
        });
    }
    fn resolve_host(&self, id: Option<&DeviceHostId>) -> Result<(), DeviceError> {
        if id.is_some_and(|id| id.as_str() != "local") {
            return Err(host_error(id.unwrap().clone(), "Unknown host."));
        }
        if *self.0.stopped.borrow() {
            return Err(host_error(local(), "Device service is shutting down."));
        }
        Ok(())
    }
    async fn enabled(&self) -> Result<bool, DeviceError> {
        Ok(self
            .0
            .settings
            .snapshot()
            .await
            .map_err(|error| {
                operation_error(
                    "settings",
                    DeviceOperationFailureReason::SettingsFailed,
                    json!(error.to_string()),
                )
            })?
            .enable_device_support)
    }
    pub async fn readiness(
        &self,
        id: Option<&DeviceHostId>,
    ) -> Result<DeviceHostReady, DeviceError> {
        self.resolve_host(id)?;
        let _lifecycle = self.0.lifecycle.lock().await;
        if !self.enabled().await? {
            return Err(host_error(
                local(),
                "Device support is off. Enable it in the Device panel before installing or starting device tools.",
            ));
        }
        let owner = Arc::downgrade(&self.0);
        let callback: DevicePhase = Arc::new(move |status, detail| {
            let owner = owner.clone();
            Box::pin(async move {
                if let Some(owner) = owner.upgrade() {
                    DeviceService(owner).status(status, detail);
                }
            })
        });
        match self.0.host.ensure_ready(callback).await {
            Ok(ready) => {
                self.resolve_host(id)?;
                if self.0.host.generation() != ready.generation {
                    return Err(host_error(
                        local(),
                        "Host configuration changed. Retry the operation.",
                    ));
                }
                self.status(DeviceHostStatus::Ready, None);
                Ok(ready)
            }
            Err(error) => {
                self.status(DeviceHostStatus::Failed, Some(error.reason.clone()));
                Err(DeviceError::DeviceHostUnavailableError(error))
            }
        }
    }
    pub async fn inspect(&self) -> DeviceServiceState {
        let summary = self.0.host.summary().await;
        self.publish(|state| {
            state.hosts = vec![summary];
        })
    }
    pub async fn list(&self) -> Result<DeviceServiceState, DeviceError> {
        self.resolve_host(None)?;
        if !self.enabled().await? {
            return Ok(self.snapshot());
        }
        let summary = self.0.host.summary().await;
        if !summary.platforms.iter().any(|platform| platform.available) {
            return Ok(self.snapshot());
        }
        match self.readiness(None).await {
            Ok(ready) => {
                if let Err(error) = self.refresh(&ready).await {
                    self.status(DeviceHostStatus::Failed, Some(error_message(&error)));
                }
            }
            Err(_) => {}
        }
        Ok(self.snapshot())
    }
    pub async fn update_tool(
        &self,
        tool: DeviceToolKind,
    ) -> Result<DeviceServiceState, DeviceError> {
        self.resolve_host(None)?;
        let _lifecycle = self.0.lifecycle.lock().await;
        self.resolve_host(None)?;
        self.0
            .host
            .toolchain()
            .ensure(tool)
            .await
            .map_err(|error| {
                operation_error(
                    "update device tool",
                    DeviceOperationFailureReason::CommandFailed,
                    serde_json::to_value(error).unwrap(),
                )
            })?;
        Ok(self.inspect().await)
    }
    pub async fn retry_host(&self, id: DeviceHostId) -> Result<DeviceServiceState, DeviceError> {
        self.resolve_host(Some(&id))?;
        if self
            .0
            .settings
            .snapshot()
            .await
            .map_err(|error| {
                operation_error(
                    "settings",
                    DeviceOperationFailureReason::SettingsFailed,
                    json!(error.to_string()),
                )
            })?
            .enable_agent_device_access
        {
            self.agent_readiness_if_supported(Some(&id)).await?;
        }
        self.list().await
    }
    pub async fn agent_readiness_if_supported(
        &self,
        id: Option<&DeviceHostId>,
    ) -> Result<Option<crate::local_device_host::DeviceHostAgentReady>, DeviceError> {
        let _lifecycle = self.0.lifecycle.lock().await;
        self.resolve_host(id)?;
        let settings = self.0.settings.snapshot().await.map_err(|error| {
            operation_error(
                "settings",
                DeviceOperationFailureReason::SettingsFailed,
                json!(error.to_string()),
            )
        })?;
        if !settings.enable_device_support || !settings.enable_agent_device_access {
            return Ok(None);
        }
        let summary = self.0.host.summary().await;
        if !summary.platforms.iter().any(|platform| platform.available) {
            return Ok(None);
        }
        let owner = Arc::downgrade(&self.0);
        let callback: DevicePhase = Arc::new(move |status, detail| {
            let owner = owner.clone();
            Box::pin(async move {
                if let Some(owner) = owner.upgrade() {
                    DeviceService(owner).status(status, detail);
                }
            })
        });
        match self.0.host.ensure_agent_ready(callback).await {
            Ok(ready) => {
                self.resolve_host(id)?;
                if self.0.host.generation() != ready.host.generation {
                    return Err(host_error(
                        local(),
                        "Host configuration changed. Retry the operation.",
                    ));
                }
                let summary = self.0.host.summary().await;
                self.publish(|state| state.hosts = vec![summary]);
                self.status(DeviceHostStatus::Ready, None);
                Ok(Some(ready))
            }
            Err(error) => {
                self.status(DeviceHostStatus::Failed, Some(error.reason.clone()));
                Err(DeviceError::DeviceHostUnavailableError(error))
            }
        }
    }
    pub async fn configure(
        &self,
        input: DeviceConfigureInput,
    ) -> Result<DeviceServiceState, DeviceError> {
        let enabled = input.enabled.flatten();
        let agent = input.agent_access_enabled.flatten();
        let onboarding = input.onboarding_completed.flatten();
        {
            let _lifecycle = self.0.lifecycle.lock().await;
            self.resolve_host(None)?;
            let mut patch = serde_json::Map::new();
            if let Some(value) = enabled {
                patch.insert("enableDeviceSupport".into(), json!(value));
            }
            if let Some(value) = agent {
                patch.insert("enableAgentDeviceAccess".into(), json!(value));
            }
            if let Some(value) = onboarding {
                patch.insert("deviceOnboardingCompleted".into(), json!(value));
            }
            let settings = self
                .0
                .settings
                .update(serde_json::from_value(Value::Object(patch)).unwrap())
                .await
                .map_err(|error| {
                    operation_error(
                        "configure",
                        DeviceOperationFailureReason::SettingsFailed,
                        json!(error.to_string()),
                    )
                })?;
            if !settings.enable_device_support {
                self.0.host.stop().await;
            } else if agent == Some(false) {
                self.0.host.stop_agent().await;
            }
            self.publish(|state| {
                state.host_status = if settings.enable_device_support {
                    DeviceHostStatus::Idle
                } else {
                    DeviceHostStatus::Disabled
                };
                state.host_status_detail = None;
                state.host_statuses.clear();
                if !settings.enable_device_support {
                    state.devices.clear();
                    state.sessions.clear();
                    state.booting_devices = Some(Some(Vec::new()));
                }
                state.agent_access_enabled = settings.enable_agent_device_access;
                state.onboarding_completed = settings.device_onboarding_completed;
            });
        }
        if agent == Some(true) && self.enabled().await? {
            self.agent_readiness_if_supported(None).await?;
        }
        self.list().await
    }
    pub async fn agent_target(
        &self,
        thread: &ThreadId,
        host: &DeviceHostId,
        device: &DeviceId,
    ) -> Result<Vec<String>, DeviceError> {
        let ready=self.agent_readiness_if_supported(Some(host)).await?.ok_or_else(||host_error(host.clone(),"Agent device access requires enabled device support, agent access, and an available simulator platform on this host."))?;
        #[cfg(test)]
        {
            let hook = { self.0.target_ready_hook.lock().unwrap().clone() };
            if let Some(hook) = hook {
                hook().await;
            }
        }
        let admission = self.0.lifecycle.clone().lock_owned().await;
        self.resolve_host(Some(host))?;
        let settings = self.0.settings.snapshot().await.map_err(|error| {
            operation_error(
                "configure agent",
                DeviceOperationFailureReason::SettingsFailed,
                json!(error.to_string()),
            )
        })?;
        if !settings.enable_device_support
            || !settings.enable_agent_device_access
            || self.0.host.current_agent().as_ref() != Some(&ready.agent_device)
        {
            return Err(host_error(
                host.clone(),
                "Agent device access requires enabled device support, agent access, and an available simulator platform on this host.",
            ));
        }
        if self.0.host.generation() != ready.host.generation {
            return Err(host_error(
                host.clone(),
                "Host configuration changed. Retry the operation.",
            ));
        }
        let file = crate::device_agent_target::config_path(self.0.host.state_dir(), host.as_str());
        let session =
            crate::device_agent_target::session(thread.as_str(), host.as_str(), device.as_str());
        let (reply, receive) = tokio::sync::oneshot::channel();
        {
            let mut jobs = self.0.writes.lock().unwrap();
            self.resolve_host(Some(host))?;
            jobs.retain(|job| !job.is_finished());
            #[cfg(test)]
            let hook = self.0.write_hook.lock().unwrap().clone();
            jobs.push(tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some(hook) = hook {
                    hook();
                }
                let result = crate::device_agent_target::write_config(&file, &ready.agent_device)
                    .map(|_| {
                        vec![
                            "--config".into(),
                            file.to_string_lossy().into_owned(),
                            "--session".into(),
                            session,
                        ]
                    })
                    .map_err(|error| {
                        operation_error(
                            "configure agent",
                            DeviceOperationFailureReason::SettingsFailed,
                            json!(error.to_string()),
                        )
                    });
                drop(admission);
                let _ = reply.send(result);
            }));
        }
        receive.await.unwrap_or_else(|error| {
            Err(operation_error(
                "configure agent",
                DeviceOperationFailureReason::SettingsFailed,
                json!(error.to_string()),
            ))
        })
    }
    async fn hub_json<T: serde::de::DeserializeOwned>(
        &self,
        ready: &DeviceHostReady,
        path: &str,
        body: Option<Value>,
        operation: &str,
        timeout: Duration,
    ) -> Result<T, DeviceError> {
        let request = if let Some(body) = body {
            self.0
                .client
                .post(format!("{}{path}", ready.origin))
                .json(&body)
        } else {
            self.0.client.get(format!("{}{path}", ready.origin))
        };
        let response_future = async {
            let response = request
                .timeout(timeout)
                .send()
                .await
                .map_err(|error| error.to_string())?
                .error_for_status()
                .map_err(|error| error.to_string())?;
            let value = response
                .json::<Value>()
                .await
                .map_err(|error| error.to_string())?;
            if !value.is_object() {
                return Err("Expected a device hub object response.".into());
            }
            serde_json::from_value(value).map_err(|error| error.to_string())
        };
        let result = tokio::select! {biased;_=self.closed()=>Err("Device service is shutting down.".into()),result=response_future=>result};
        result.map_err(|error| {
            operation_error(
                operation,
                DeviceOperationFailureReason::RequestFailed,
                json!(error),
            )
        })
    }
    async fn fetch_devices(
        &self,
        ready: &DeviceHostReady,
    ) -> Result<(Vec<DeviceSummary>, Option<String>), DeviceError> {
        #[derive(Deserialize)]
        struct HubDevice {
            id: String,
            name: String,
            version: String,
            platform: DevicePlatform,
            booted: bool,
            physical: bool,
        }
        #[derive(Deserialize)]
        struct HubError {
            message: String,
        }
        #[derive(Deserialize)]
        struct HubList {
            simulators: Vec<HubDevice>,
            emulators: Vec<HubDevice>,
            errors: Option<Vec<HubError>>,
        }
        let list: HubList = self
            .hub_json(ready, "/api/devices", None, "list", Duration::from_secs(15))
            .await?;
        let detail = list
            .errors
            .map(|errors| {
                errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|detail| !detail.is_empty());
        let mut devices = Vec::new();
        for device in list.simulators.into_iter().chain(list.emulators) {
            devices.push(DeviceSummary {
                host_id: local(),
                id: DeviceId::new(device.id).map_err(|error| {
                    operation_error(
                        "list",
                        DeviceOperationFailureReason::RequestFailed,
                        json!(error.to_string()),
                    )
                })?,
                name: TrimmedNonEmptyString::new(device.name).map_err(|error| {
                    operation_error(
                        "list",
                        DeviceOperationFailureReason::RequestFailed,
                        json!(error.to_string()),
                    )
                })?,
                version: device.version,
                platform: device.platform,
                booted: device.booted,
                physical: device.physical,
            });
        }
        if self
            .0
            .host
            .platform(DevicePlatform::Android)
            .await
            .available
        {
            let output = ready
                .run_command(
                    "emulator",
                    vec!["-list-avds".into()],
                    Duration::from_secs(20),
                )
                .await;
            if output.code != 0 {
                let mut error = operation_error(
                    "list",
                    DeviceOperationFailureReason::CommandFailed,
                    json!({"code":output.code,"stdout":output.stdout,"stderr":output.stderr}),
                );
                if let DeviceError::DeviceOperationError(ref mut error) = error {
                    error.exit_code = Some(Some(output.code.into()));
                }
                return Err(error);
            }
            for name in output
                .stdout
                .lines()
                .map(trim_wire_string)
                .filter(|name| !name.is_empty())
            {
                if !devices.iter().any(|device| {
                    device.platform == DevicePlatform::Android && device.name.as_str() == name
                }) {
                    devices.push(DeviceSummary {
                        host_id: local(),
                        id: DeviceId::new(name).map_err(|error| {
                            operation_error(
                                "list",
                                DeviceOperationFailureReason::RequestFailed,
                                json!(error.to_string()),
                            )
                        })?,
                        name: TrimmedNonEmptyString::new(name).unwrap(),
                        version: "Android".into(),
                        platform: DevicePlatform::Android,
                        booted: false,
                        physical: false,
                    });
                }
            }
        }
        Ok((devices, detail))
    }
    async fn refresh(&self, ready: &DeviceHostReady) -> Result<DeviceServiceState, DeviceError> {
        let (devices, detail) = self.fetch_devices(ready).await?;
        let summary = self.0.host.summary().await;
        let _lifecycle = self.0.lifecycle.lock().await;
        if !self.enabled().await?
            || *self.0.stopped.borrow()
            || self.0.host.generation() != ready.generation
        {
            return Ok(self.snapshot());
        }
        Ok(self.publish(|state| {
            state.hosts = vec![summary];
            state.devices = devices;
            state.host_status_detail = detail.clone().map(Some);
            state.host_statuses.insert(
                local(),
                DeviceHostState {
                    status: DeviceHostStatus::Ready,
                    detail: detail.map(Some),
                },
            );
        }))
    }
    pub async fn open(&self, input: DeviceOpenInput) -> Result<DeviceSession, DeviceError> {
        self.resolve_host(input.host_id.as_ref().and_then(Option::as_ref))?;
        let availability = self.0.host.platform(input.platform).await;
        if !availability.available {
            return Err(DeviceError::DevicePlatformUnavailableError(
                DevicePlatformUnavailableError {
                    tag: DevicePlatformUnavailableErrorTag::DevicePlatformUnavailableError,
                    host_id: local(),
                    platform: input.platform,
                    reason: availability
                        .reason
                        .flatten()
                        .unwrap_or_else(|| "Platform toolchain missing.".into()),
                },
            ));
        }
        let ready = self.readiness(None).await?;
        let mut state = self.refresh(&ready).await?;
        let mut device = state
            .devices
            .iter()
            .find(|device| device.id == input.device_id)
            .cloned()
            .ok_or_else(|| not_found(local(), input.device_id.clone()))?;
        if !device.booted && input.boot.flatten() != Some(false) {
            let booting = DeviceBootingDevice {
                host_id: device.host_id.clone(),
                id: device.id.clone(),
                platform: device.platform,
                name: device.name.clone(),
                version: device.version.clone(),
                booted: device.booted,
                physical: device.physical,
                thread_id: input.thread_id.clone(),
            };
            self.publish(|state| {
                let mut entries = state.booting_devices.take().flatten().unwrap_or_default();
                entries.retain(|entry| entry.host_id != booting.host_id || entry.id != booting.id);
                entries.push(booting);
                state.booting_devices = Some(Some(entries));
            });
            let cleanup = BootingCleanup {
                owner: Arc::downgrade(&self.0),
                host: device.host_id.clone(),
                device: device.id.clone(),
            };
            let result = self.boot(&ready, &device).await;
            drop(cleanup);
            let booted_id = result?;
            state = self.refresh(&ready).await?;
            device = state
                .devices
                .iter()
                .find(|candidate| candidate.id == booted_id)
                .or_else(|| {
                    state
                        .devices
                        .iter()
                        .find(|candidate| candidate.id == device.id)
                })
                .cloned()
                .ok_or_else(|| not_found(local(), booted_id))?;
        } else if device.platform == DevicePlatform::Ios && device.booted {
            let _: HubAction = self
                .hub_json(
                    &ready,
                    "/vendor/serve-sim/grid/api/start",
                    Some(json!({"udid":device.id})),
                    "attach stream",
                    Duration::from_secs(180),
                )
                .await?;
        }
        let session = DeviceSession {
            thread_id: input.thread_id,
            host_id: local(),
            device_id: device.id,
            platform: device.platform,
            opened_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        {
            let _lifecycle = self.0.lifecycle.lock().await;
            self.resolve_host(None)?;
            if self.0.host.generation() != ready.generation {
                return Err(host_error(
                    local(),
                    "Host configuration changed. Retry the operation.",
                ));
            }
            if !self.enabled().await? {
                return Err(host_error(
                    local(),
                    "Device support was turned off while the device was opening.",
                ));
            }
            self.publish(|state| {
                state.sessions.retain(|existing| {
                    existing.thread_id != session.thread_id
                        || existing.host_id != session.host_id
                        || existing.device_id != session.device_id
                });
                state.sessions.push(session.clone());
            });
        }
        Ok(session)
    }
    async fn boot(
        &self,
        ready: &DeviceHostReady,
        device: &DeviceSummary,
    ) -> Result<DeviceId, DeviceError> {
        let result: HubAction = self
            .hub_json(
                ready,
                "/api/devices/boot",
                Some(json!({"platform":device.platform,"id":device.id,"name":device.name})),
                "boot",
                Duration::from_secs(180),
            )
            .await?;
        if !result.ok {
            let text = result.error.clone().unwrap_or_default();
            let reason = if regex::Regex::new(
                "(?i)insufficient.*(?:disk|space)|not enough.*(?:disk|space)|no space left",
            )
            .unwrap()
            .is_match(&text)
            {
                DeviceBootFailureReason::DiskSpace
            } else if regex::Regex::new("(?i)timed? out|timeout")
                .unwrap()
                .is_match(&text)
            {
                DeviceBootFailureReason::Timeout
            } else {
                DeviceBootFailureReason::LaunchFailed
            };
            return Err(DeviceError::DeviceBootError(DeviceBootError {
                tag: DeviceBootErrorTag::DeviceBootError,
                host_id: local(),
                device_id: device.id.clone(),
                reason,
                cause: json!({"ok":false,"error":result.error}),
            }));
        }
        if device.platform == DevicePlatform::Ios {
            let _: HubAction = self
                .hub_json(
                    ready,
                    "/vendor/serve-sim/grid/api/start",
                    Some(json!({"udid":device.id})),
                    "attach stream",
                    Duration::from_secs(180),
                )
                .await?;
        }
        DeviceId::new(
            result
                .serial
                .or(result.id)
                .unwrap_or_else(|| device.id.to_string()),
        )
        .map_err(|error| {
            operation_error(
                "boot",
                DeviceOperationFailureReason::RequestFailed,
                json!(error.to_string()),
            )
        })
    }
    async fn resolve_device(
        &self,
        host_id: Option<&DeviceHostId>,
        device_id: &DeviceId,
    ) -> Result<(DeviceHostReady, DeviceSummary), DeviceError> {
        self.resolve_host(host_id)?;
        let ready = self.readiness(host_id).await?;
        let find = |state: DeviceServiceState| {
            state
                .devices
                .into_iter()
                .find(|device| device.host_id == local() && &device.id == device_id)
        };
        let device = match find(self.snapshot()) {
            Some(device) => Some(device),
            None => find(self.refresh(&ready).await?),
        }
        .ok_or_else(|| not_found(local(), device_id.clone()))?;
        Ok((ready, device))
    }
    fn action_helpers(ready: &DeviceHostReady) -> crate::device_actions::Helpers {
        crate::device_actions::Helpers {
            node_path: ready.node_path.to_string_lossy().into_owned(),
            serve_sim_ax_settings: ready
                .serve_sim_ax_settings
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            serve_sim_cli: ready
                .serve_sim_cli
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        }
    }
    fn action_runner(
        ready: DeviceHostReady,
    ) -> impl Fn(
        crate::device_actions::ActionCommand,
    ) -> BoxFuture<'static, crate::device_commands::HostCommandOutput>
    + Send
    + Sync {
        move |command| {
            let ready = ready.clone();
            Box::pin(async move {
                ready
                    .run_command_with_stdin(
                        &command.command,
                        command.args,
                        Duration::from_secs(20),
                        command.stdin.map(String::into_bytes),
                    )
                    .await
            })
        }
    }
    /// Media routes inspect an existing endpoint and never install or start tools.
    pub fn current_readiness(&self, host_id: Option<&DeviceHostId>) -> Option<DeviceHostReady> {
        self.resolve_host(host_id).ok()?;
        self.0.host.current()
    }
    pub async fn screenshot(
        &self,
        host_id: Option<&DeviceHostId>,
        device_id: &DeviceId,
    ) -> Result<(DeviceSummary, Vec<u8>), DeviceError> {
        let (ready, device) = self.resolve_device(host_id, device_id).await?;
        let prefix = if device.platform == DevicePlatform::Ios {
            "serve-sim"
        } else {
            "serve-emu"
        };
        let encoded_id = device
            .id
            .as_str()
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
                    char::from(byte).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect::<String>();
        let capture = async {
            self.0
                .client
                .post(format!(
                    "{}/vendor/{prefix}/api/screenshot?device={encoded_id}",
                    ready.origin
                ))
                .timeout(Duration::from_secs(20))
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await
                .map(|bytes| bytes.to_vec())
        };
        let png = tokio::select! {
            biased;
            _ = self.closed() => Err(operation_error("screenshot", DeviceOperationFailureReason::RequestFailed, json!("Device service is shutting down."))),
            result = tokio::time::timeout(Duration::from_secs(20), capture) => match result {
                Ok(result) => result.map_err(|error| operation_error("screenshot", DeviceOperationFailureReason::RequestFailed, json!(error.to_string()))),
                Err(error) => Err(operation_error("screenshot", DeviceOperationFailureReason::RequestFailed, json!(error.to_string()))),
            },
        }?;
        Ok((device, png))
    }
    pub async fn detail(&self, input: DeviceDetailInput) -> Result<DeviceDetail, DeviceError> {
        let (ready, device) = self
            .resolve_device(
                input.host_id.as_ref().and_then(Option::as_ref),
                &input.device_id,
            )
            .await?;
        let (settings, foreground_app) = crate::device_detail::read_detail(
            device.platform,
            device.id.as_str(),
            &Self::action_helpers(&ready),
            &Self::action_runner(ready),
        )
        .await;
        Ok(DeviceDetail {
            host_id: local(),
            device_id: device.id,
            settings,
            foreground_app,
            read_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
    }
    pub async fn action(&self, input: DeviceActionInput) -> Result<DeviceDetail, DeviceError> {
        let value = serde_json::to_value(&input).expect("typed device action");
        let device_id: DeviceId =
            serde_json::from_value(value["deviceId"].clone()).expect("device action ID");
        let (ready, device) = self.resolve_device(input.host_id(), &device_id).await?;
        crate::device_actions::run_action(
            device.platform,
            &input,
            &Self::action_helpers(&ready),
            &Self::action_runner(ready),
        )
        .await?;
        self.detail(DeviceDetailInput {
            host_id: Some(Some(local())),
            device_id: device.id,
        })
        .await
    }
    pub async fn close(&self, input: DeviceCloseInput) -> Result<(), DeviceError> {
        let host = input.host_id.flatten();
        let device = input.device_id.flatten();
        let closing = self
            .snapshot()
            .sessions
            .into_iter()
            .filter(|session| {
                session.thread_id == input.thread_id
                    && host.as_ref().is_none_or(|id| id == &session.host_id)
                    && device.as_ref().is_none_or(|id| id == &session.device_id)
            })
            .collect::<Vec<_>>();
        if closing.is_empty() {
            return Ok(());
        }
        self.publish(|state| {
            state.sessions.retain(|session| !closing.contains(session));
        });
        if input.shutdown.flatten() == Some(true) {
            for session in closing {
                self.shutdown_device(DeviceShutdownInput {
                    host_id: Some(Some(session.host_id)),
                    device_id: session.device_id,
                    platform: session.platform,
                })
                .await?;
            }
        }
        Ok(())
    }
    pub async fn shutdown_device(&self, input: DeviceShutdownInput) -> Result<(), DeviceError> {
        self.resolve_host(input.host_id.as_ref().and_then(Option::as_ref))?;
        let ready = self.readiness(None).await?;
        let path = if input.platform == DevicePlatform::Ios {
            "/vendor/serve-sim/grid/api/shutdown"
        } else {
            "/api/devices/shutdown"
        };
        let body = if input.platform == DevicePlatform::Ios {
            json!({"udid":input.device_id})
        } else {
            json!({"platform":input.platform,"id":input.device_id})
        };
        let result: Result<HubAction, DeviceError> = self
            .hub_json(
                &ready,
                path,
                Some(body),
                "shutdown",
                Duration::from_secs(15),
            )
            .await;
        match result {
            Ok(result) if result.ok => {}
            result => {
                let original = match result {
                    Err(error) => error,
                    Ok(result) => operation_error(
                        "shutdown",
                        DeviceOperationFailureReason::HubRejected,
                        json!({"ok":result.ok,"error":result.error}),
                    ),
                };
                if input.platform != DevicePlatform::Ios
                    || !self.fetch_devices(&ready).await.is_ok_and(|(devices, _)| {
                        devices
                            .iter()
                            .any(|device| device.id == input.device_id && !device.booted)
                    })
                {
                    return Err(original);
                }
            }
        }
        self.publish(|state| {
            for device in &mut state.devices {
                if device.id == input.device_id {
                    device.booted = false;
                }
            }
            state.sessions.retain(|session| {
                session.host_id != local() || session.device_id != input.device_id
            });
        });
        let _ = self.refresh(&ready).await;
        Ok(())
    }
    pub fn sessions_for_thread(&self, thread: &ThreadId) -> Vec<DeviceSession> {
        self.snapshot()
            .sessions
            .into_iter()
            .filter(|session| &session.thread_id == thread)
            .collect()
    }
    pub async fn closed(&self) {
        let mut stopped = self.0.stopped.subscribe();
        let _ = stopped.wait_for(|stopped| *stopped).await;
    }
    pub async fn shutdown(&self) {
        self.0.stopped.send_replace(true);
        self.0.host.shutdown().await;
        self.0.host.toolchain().shutdown().await;
        let mut writes = self.0.write_cleanup.lock().await;
        writes.extend(self.0.writes.lock().unwrap().drain(..));
        while let Some(job) = writes.last_mut() {
            let _ = (&mut *job).await;
            writes.pop();
        }
        self.0.state.lock().unwrap().listeners.clear();
    }
}
#[derive(Deserialize)]
struct HubAction {
    ok: bool,
    id: Option<String>,
    serial: Option<String>,
    error: Option<String>,
}
fn error_message(error: &DeviceError) -> String {
    match error {
        DeviceError::DeviceHostUnavailableError(error) => error.reason.clone(),
        _ => "Device discovery failed.".into(),
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::{
        device_toolchain::{DEVICE_HUB_VERSION, DeviceToolchain, ToolchainOptions},
        local_device_host::LocalDeviceHostOptions,
        server_secret_store::ServerSecretStore,
        server_settings::SettingsOptions,
    };
    use std::{os::unix::fs::PermissionsExt, path::Path};
    pub(crate) struct Fixture {
        pub(crate) service: DeviceService,
        pub(crate) settings: SettingsService,
        pub(crate) socket: tokio::net::UnixDatagram,
        profile: std::path::PathBuf,
    }
    pub(crate) async fn fixture(root: &Path) -> Fixture {
        fixture_with_agent_stop_gate(root, false).await
    }
    async fn fixture_with_agent_stop_gate(root: &Path, held: bool) -> Fixture {
        let tools = DeviceToolchain::new(ToolchainOptions::new(root.into()));
        let paths = tools.paths(DeviceToolKind::Hub);
        tokio::fs::create_dir_all(paths.entry_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &paths.entry_path,
            include_str!("../tests/fixtures/device-hub.py"),
        )
        .await
        .unwrap();
        tokio::fs::write(paths.sentinel_path, format!("{DEVICE_HUB_VERSION}\n"))
            .await
            .unwrap();
        let agent_paths = tools.paths(DeviceToolKind::Agent);
        tokio::fs::create_dir_all(agent_paths.entry_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &agent_paths.entry_path,
            include_str!("../tests/fixtures/device-agent.py"),
        )
        .await
        .unwrap();
        tokio::fs::write(
            agent_paths.sentinel_path,
            format!("{}\n", crate::device_toolchain::AGENT_DEVICE_VERSION),
        )
        .await
        .unwrap();
        let ax = paths
            .install_dir
            .join("node_modules/expo-device-hub/vendor/serve-sim/dist/simax/serve-sim-ax-settings");
        tokio::fs::create_dir_all(ax.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(ax, b"fixture helper").await.unwrap();
        let compressed_data = root.join("compressed.jsonl");
        tokio::fs::write(
            &compressed_data,
            include_str!("../tests/fixtures/device-proxy-compression.jsonl"),
        )
        .await
        .unwrap();
        let command_data = root.join("commands.json");
        tokio::fs::write(&command_data, b"{}").await.unwrap();
        let socket_path = root.join("events.sock");
        let socket = tokio::net::UnixDatagram::bind(&socket_path).unwrap();
        let profile = root.join("profile.json");
        tokio::fs::write(&profile, b"{}").await.unwrap();
        let bin = root.join("bin");
        tokio::fs::create_dir_all(&bin).await.unwrap();
        let xcrun = bin.join("xcrun");
        tokio::fs::write(&xcrun, include_str!("../tests/fixtures/device-command.py"))
            .await
            .unwrap();
        tokio::fs::set_permissions(xcrun, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();
        let mut options = LocalDeviceHostOptions::host(root.join("state"));
        options.platform = "darwin".into();
        options.node_override = Some("/usr/bin/python3".into());
        options.environment = [
            ("HOME", root.join("home")),
            ("PATH", bin),
            ("FIXTURE_PROFILE", profile.clone()),
            ("FIXTURE_COMMAND_DATA", command_data),
            ("FIXTURE_COMPRESSED_DATA", compressed_data),
            ("FIXTURE_MILESTONES", socket_path),
            ("FIXTURE_LOG", root.join("agent-calls.jsonl")),
            ("FIXTURE_HOLD_START", root.join("agent-hold")),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value.to_string_lossy().into_owned()))
        .collect();
        if held {
            options.environment.insert(
                "FIXTURE_STOP_GATE".into(),
                root.join("agent-stop.sock").to_string_lossy().into_owned(),
            );
        }
        let host = LocalDeviceHost::new(options, tools);
        let secrets = ServerSecretStore::open(root.join("secrets")).unwrap();
        let mut options = SettingsOptions::file(root.join("settings.json"), secrets);
        options.watch = false;
        let settings = SettingsService::start(options).await.unwrap();
        let service = DeviceService::new(settings.clone(), host).await.unwrap();
        Fixture {
            service,
            settings,
            socket,
            profile,
        }
    }
    async fn path(socket: &tokio::net::UnixDatagram, expected: &str) -> u32 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut bytes = [0; 4096];
                let count = socket.recv(&mut bytes).await.unwrap();
                let message: Value = serde_json::from_slice(&bytes[..count]).unwrap();
                if message["path"] == expected && message["held"] == true {
                    return message["pid"].as_u64().unwrap() as u32;
                }
            }
        })
        .await
        .unwrap()
    }
    fn reaped(pid: u32) {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    async fn agent_event(socket: &tokio::net::UnixDatagram, event: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut bytes = [0; 4096];
                let size = socket.recv(&mut bytes).await.unwrap();
                let value: Value = serde_json::from_slice(&bytes[..size]).unwrap();
                if value["event"] == event {
                    return;
                }
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn canceled_agent_stop_preserves_cleanup_admission_before_restarting_agent() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let fixture = fixture_with_agent_stop_gate(root.path(), true).await;
            fixture
                .service
                .configure(
                    serde_json::from_value(json!({"enabled":true,"agentAccessEnabled":true}))
                        .unwrap(),
                )
                .await
                .unwrap();
            let hub_pid = fixture.service.current_readiness(None).unwrap().pid;
            let stop = tokio::spawn({
                let host = fixture.service.0.host.clone();
                async move { host.stop_agent().await }
            });
            agent_event(&fixture.socket, "stop").await;
            stop.abort();
            assert!(stop.await.unwrap_err().is_cancelled());
            let restart = tokio::spawn({
                let service = fixture.service.clone();
                async move { service.agent_readiness_if_supported(None).await }
            });
            drop(
                tokio::net::UnixStream::connect(root.path().join("agent-stop.sock"))
                    .await
                    .unwrap(),
            );
            let ready = restart.await.unwrap().unwrap().unwrap();
            assert_eq!(ready.host.pid, hub_pid);
            let calls: Vec<Value> = std::fs::read_to_string(root.path().join("agent-calls.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call["args"][0].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["devices", "daemon", "devices"]
            );
            let shutdown = tokio::spawn({
                let service = fixture.service.clone();
                async move { service.shutdown().await }
            });
            agent_event(&fixture.socket, "stop").await;
            drop(
                tokio::net::UnixStream::connect(root.path().join("agent-stop.sock"))
                    .await
                    .unwrap(),
            );
            shutdown.await.unwrap();
            fixture.settings.shutdown().await;
            reaped(hub_pid);
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn canceled_host_stop_completes_owned_cleanup_before_reusable_startup() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let fixture = fixture_with_agent_stop_gate(root.path(), true).await;
            fixture
                .service
                .configure(
                    serde_json::from_value(json!({"enabled":true,"agentAccessEnabled":true}))
                        .unwrap(),
                )
                .await
                .unwrap();
            let old_pid = fixture.service.current_readiness(None).unwrap().pid;
            let stop = tokio::spawn({
                let host = fixture.service.0.host.clone();
                async move { host.stop().await }
            });
            agent_event(&fixture.socket, "stop").await;
            stop.abort();
            assert!(stop.await.unwrap_err().is_cancelled());
            let retry = tokio::spawn({
                let host = fixture.service.0.host.clone();
                async move { host.stop().await }
            });
            drop(
                tokio::net::UnixStream::connect(root.path().join("agent-stop.sock"))
                    .await
                    .unwrap(),
            );
            retry.await.unwrap();
            reaped(old_pid);
            let new_pid = fixture.service.readiness(None).await.unwrap().pid;
            assert_ne!(old_pid, new_pid);
            fixture.service.shutdown().await;
            fixture.settings.shutdown().await;
            reaped(new_pid);
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn agent_consent_starts_owned_daemon_and_disabling_agent_preserves_viewing_hub() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let fixture = fixture(root.path()).await;
            assert!(
                fixture
                    .service
                    .agent_readiness_if_supported(None)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(!root.path().join("agent-calls.jsonl").exists());
            let thread = ThreadId::new("agent-thread").unwrap();
            let device = DeviceId::new("fixture-ios").unwrap();
            assert!(matches!(
                fixture
                    .service
                    .agent_target(&thread, &local(), &device)
                    .await,
                Err(DeviceError::DeviceHostUnavailableError(_))
            ));
            let state = fixture
                .service
                .configure(
                    serde_json::from_value(json!({"enabled":true,"agentAccessEnabled":true}))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(state.agent_access_enabled);
            let ready = fixture
                .service
                .agent_readiness_if_supported(None)
                .await
                .unwrap()
                .unwrap();
            let pid = ready.host.pid;
            assert_eq!(ready.agent_device.base_url, "http://127.0.0.1:12345");
            assert_eq!(ready.agent_device.token, "isolated token");
            let target = fixture
                .service
                .agent_target(&thread, &local(), &device)
                .await
                .unwrap();
            assert_eq!(target[0], "--config");
            assert_eq!(target[2], "--session");
            let configuration: Value =
                serde_json::from_slice(&std::fs::read(&target[1]).unwrap()).unwrap();
            assert_eq!(
                configuration,
                json!({"daemonBaseUrl":"http://127.0.0.1:12345","daemonAuthToken":"isolated token"})
            );
            let other = fixture
                .service
                .agent_target(
                    &ThreadId::new("other-agent-thread").unwrap(),
                    &local(),
                    &device,
                )
                .await
                .unwrap();
            assert_eq!(target[1], other[1]);
            assert_ne!(target[3], other[3]);
            let state = fixture
                .service
                .configure(serde_json::from_value(json!({"agentAccessEnabled":false})).unwrap())
                .await
                .unwrap();
            assert!(!state.agent_access_enabled);
            assert_eq!(fixture.service.current_readiness(None).unwrap().pid, pid);
            assert!(
                fixture
                    .service
                    .agent_readiness_if_supported(None)
                    .await
                    .unwrap()
                    .is_none()
            );
            let rows: Vec<Value> = std::fs::read_to_string(root.path().join("agent-calls.jsonl"))
                .unwrap()
                .lines()
                .map(|row| serde_json::from_str(row).unwrap())
                .collect();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0]["args"], json!(["devices", "--json"]));
            assert_eq!(rows[1]["args"][0], "daemon");
            fixture.service.shutdown().await;
            fixture.settings.shutdown().await;
            reaped(pid);
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn canceled_target_write_retains_lifecycle_and_shutdown_awaits_atomic_publication() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let fixture = fixture(root.path()).await;
            fixture
                .service
                .configure(
                    serde_json::from_value(json!({"enabled":true,"agentAccessEnabled":true}))
                        .unwrap(),
                )
                .await
                .unwrap();
            let (entered, started) = tokio::sync::oneshot::channel();
            let entered = Mutex::new(Some(entered));
            let (release, released) = std::sync::mpsc::channel();
            let released = Mutex::new(released);
            *fixture.service.0.write_hook.lock().unwrap() = Some(Arc::new(move || {
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            }));
            let request = tokio::spawn({
                let service = fixture.service.clone();
                async move {
                    service
                        .agent_target(
                            &ThreadId::new("cancelled-target").unwrap(),
                            &local(),
                            &DeviceId::new("fixture-ios").unwrap(),
                        )
                        .await
                }
            });
            started.await.unwrap();
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            assert!(fixture.service.0.lifecycle.try_lock().is_err());
            let shutdown = tokio::spawn({
                let service = fixture.service.clone();
                async move { service.shutdown().await }
            });
            release.send(()).unwrap();
            shutdown.await.unwrap();
            let file = crate::device_agent_target::config_path(
                fixture.service.0.host.state_dir(),
                "local",
            );
            let configuration: Value =
                serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
            assert_eq!(configuration["daemonAuthToken"], "isolated token");
            assert!(fixture.service.0.lifecycle.try_lock().is_ok());
            assert!(fixture.service.0.write_cleanup.lock().await.is_empty());
            fixture.settings.shutdown().await;
        })
        .await
        .unwrap();
    }
    fn open(thread: &str) -> DeviceOpenInput {
        serde_json::from_value(json!({"threadId":thread,"deviceId":"fixture-ios","platform":"ios"}))
            .unwrap()
    }
    async fn enable(fixture: &Fixture) {
        fixture
            .service
            .configure(
                serde_json::from_value(json!({"enabled":true,"onboardingCompleted":true})).unwrap(),
            )
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn screenshot_posts_both_vendor_routes_encodes_device_id_and_cancels_held_body() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let fixture = fixture(root.path()).await;
            assert!(fixture.service.current_readiness(None).is_none());
            enable(&fixture).await;
            let ios = DeviceId::new("fixture-ios").unwrap();
            let (device, png) = fixture.service.screenshot(None, &ios).await.unwrap();
            assert_eq!(device.id, ios);
            assert_eq!(png, b"\x89PNG\r\n\x1a\nfixture screenshot");
            let android=DeviceId::new("fixture /?&👋").unwrap();
            tokio::fs::write(&fixture.profile,serde_json::to_vec(&json!({"androidId":android})).unwrap()).await.unwrap();
            let (device,png)=fixture.service.screenshot(None,&android).await.unwrap();
            assert_eq!(device.platform,DevicePlatform::Android);assert_eq!(device.id,android);assert_eq!(png,b"\x89PNG\r\n\x1a\nfixture screenshot");
            let route = "/vendor/serve-emu/api/screenshot?device=fixture%20%2F%3F%26%F0%9F%91%8B";
            tokio::fs::write(&fixture.profile,serde_json::to_vec(&json!({"androidId":android,"bodyHeld":route})).unwrap()).await.unwrap();
            let request = tokio::spawn({let service=fixture.service.clone();async move {service.screenshot(None,&android).await}});
            let pid=path(&fixture.socket,route).await;
            fixture.service.shutdown().await;
            assert!(matches!(request.await.unwrap(), Err(DeviceError::DeviceOperationError(error)) if error.operation=="screenshot" && error.reason==DeviceOperationFailureReason::RequestFailed));
            reaped(pid);assert!(fixture.service.current_readiness(None).is_none());
            fixture.settings.shutdown().await;
        }).await.unwrap();
    }
    #[tokio::test]
    async fn screenshot_deadline_covers_body_after_headers_and_reaps_host_on_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let fixture = fixture(root.path()).await;
        enable(&fixture).await;
        fixture.service.list().await.unwrap();
        let route = "/vendor/serve-sim/api/screenshot?device=fixture-ios";
        tokio::fs::write(
            &fixture.profile,
            serde_json::to_vec(&json!({"bodyHeld":route})).unwrap(),
        )
        .await
        .unwrap();
        let request = tokio::spawn({
            let service = fixture.service.clone();
            async move {
                service
                    .screenshot(None, &DeviceId::new("fixture-ios").unwrap())
                    .await
            }
        });
        let pid = path(&fixture.socket, route).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(20)).await;
        tokio::time::resume();
        let result = tokio::time::timeout(Duration::from_secs(5), request).await;
        fixture.service.shutdown().await;
        let error = result
            .expect("screenshot deadline must finish after clock advance")
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(error,DeviceError::DeviceOperationError(error) if error.operation=="screenshot" && error.reason==DeviceOperationFailureReason::RequestFailed)
        );
        reaped(pid);
        fixture.settings.shutdown().await;
    }
    #[tokio::test]
    async fn consent_discovery_boot_and_shared_sessions_use_owned_hub() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = fixture(temp.path()).await;
        assert_eq!(
            fixture.service.list().await.unwrap().host_status,
            DeviceHostStatus::Disabled
        );
        fixture.service.inspect().await;
        assert!(fixture.service.0.host.current().is_none());
        let mut updates = fixture.service.subscribe();
        enable(&fixture).await;
        assert!(updates.recv().await.is_some());
        let first = fixture.service.open(open("thread-a")).await.unwrap();
        let second = fixture.service.open(open("thread-b")).await.unwrap();
        assert_eq!(first.device_id, second.device_id);
        assert_eq!(fixture.service.snapshot().sessions.len(), 2);
        assert!(
            fixture
                .settings
                .snapshot()
                .await
                .unwrap()
                .device_onboarding_completed
        );
        fixture
            .service
            .close(serde_json::from_value(json!({"threadId":"thread-a"})).unwrap())
            .await
            .unwrap();
        assert_eq!(fixture.service.snapshot().sessions, vec![second]);
        // A confirmed shutdown survives the subsequent best-effort discovery failure.
        tokio::fs::write(&fixture.profile, b"{\"fail\":\"/api/devices\"}")
            .await
            .unwrap();
        fixture
            .service
            .shutdown_device(
                serde_json::from_value(json!({"deviceId":"fixture-ios","platform":"ios"})).unwrap(),
            )
            .await
            .unwrap();
        assert!(fixture.service.snapshot().sessions.is_empty());
        assert!(!fixture.service.snapshot().devices[0].booted);
        let pid = fixture.service.0.host.current().unwrap().pid;
        fixture.service.shutdown().await;
        reaped(pid);
        assert!(updates.recv().await.is_none());
        fixture.settings.shutdown().await;
    }
    #[tokio::test]
    async fn cancelled_boot_clears_marker_and_shutdown_reaps_pending_hub() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = fixture(temp.path()).await;
        enable(&fixture).await;
        tokio::fs::write(&fixture.profile, b"{\"held\":\"/api/devices/boot\"}")
            .await
            .unwrap();
        let request = tokio::spawn({
            let service = fixture.service.clone();
            async move { service.open(open("thread-a")).await }
        });
        let pid = path(&fixture.socket, "/api/devices/boot").await;
        assert_eq!(
            fixture
                .service
                .snapshot()
                .booting_devices
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .len(),
            1
        );
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(
            fixture
                .service
                .snapshot()
                .booting_devices
                .unwrap()
                .unwrap()
                .is_empty()
        );
        fixture.service.shutdown().await;
        reaped(pid);
        fixture.settings.shutdown().await;
    }
    #[tokio::test]
    async fn shutdown_cancels_held_discovery_and_refuses_late_state_or_session() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = fixture(temp.path()).await;
        enable(&fixture).await;
        tokio::fs::write(&fixture.profile, b"{\"held\":\"/api/devices\"}")
            .await
            .unwrap();
        let request = tokio::spawn({
            let service = fixture.service.clone();
            async move { service.open(open("thread-a")).await }
        });
        let pid = path(&fixture.socket, "/api/devices").await;
        fixture.service.shutdown().await;
        reaped(pid);
        let revision = fixture.service.snapshot().revision;
        assert!(request.await.unwrap().is_err());
        assert_eq!(fixture.service.snapshot().revision, revision);
        assert!(fixture.service.snapshot().sessions.is_empty());
        assert!(fixture.service.readiness(None).await.is_err());
        fixture.settings.shutdown().await;
    }
    #[tokio::test]
    async fn held_prior_host_boot_cannot_publish_after_disable_and_reenable() {
        let temp = tempfile::tempdir().unwrap();
        let fixture = fixture(temp.path()).await;
        enable(&fixture).await;
        tokio::fs::write(&fixture.profile, b"{\"held\":\"/api/devices/boot\"}")
            .await
            .unwrap();
        let request = tokio::spawn({
            let service = fixture.service.clone();
            async move { service.open(open("thread-old")).await }
        });
        let pid = path(&fixture.socket, "/api/devices/boot").await;
        let generation = fixture.service.0.host.generation();
        fixture
            .service
            .configure(serde_json::from_value(json!({"enabled":false})).unwrap())
            .await
            .unwrap();
        reaped(pid);
        tokio::fs::write(&fixture.profile, b"{}").await.unwrap();
        enable(&fixture).await;
        assert_ne!(generation, fixture.service.0.host.generation());
        assert!(request.await.unwrap().is_err());
        assert!(fixture.service.snapshot().sessions.is_empty());
        assert!(
            fixture
                .service
                .snapshot()
                .booting_devices
                .unwrap()
                .unwrap()
                .is_empty()
        );
        fixture.service.shutdown().await;
        fixture.settings.shutdown().await;
    }

    #[tokio::test]
    async fn revoked_agent_consent_between_readiness_and_target_admission_cannot_publish_endpoint()
    {
        tokio::time::timeout(Duration::from_secs(15), async {
            let root = tempfile::tempdir().unwrap();
            let fixture = fixture(root.path()).await;
            fixture
                .service
                .configure(
                    serde_json::from_value(json!({"enabled":true,"agentAccessEnabled":true}))
                        .unwrap(),
                )
                .await
                .unwrap();
            let hub_pid = fixture.service.current_readiness(None).unwrap().pid;
            let (entered, started) = tokio::sync::oneshot::channel();
            let entered = Arc::new(Mutex::new(Some(entered)));
            let (release, released) = watch::channel(false);
            *fixture.service.0.target_ready_hook.lock().unwrap() = Some(Arc::new(move || {
                let entered = entered.clone();
                let mut released = released.clone();
                Box::pin(async move {
                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                    let _ = released.wait_for(|value| *value).await;
                })
            }));
            let target = tokio::spawn({
                let service = fixture.service.clone();
                async move {
                    service
                        .agent_target(
                            &ThreadId::new("revoked-thread").unwrap(),
                            &local(),
                            &DeviceId::new("fixture-ios").unwrap(),
                        )
                        .await
                }
            });
            started.await.unwrap();
            fixture
                .service
                .configure(serde_json::from_value(json!({"agentAccessEnabled":false})).unwrap())
                .await
                .unwrap();
            assert_eq!(
                fixture.service.current_readiness(None).unwrap().pid,
                hub_pid
            );
            release.send_replace(true);
            assert!(matches!(
                target.await.unwrap(),
                Err(DeviceError::DeviceHostUnavailableError(_))
            ));
            assert!(
                !crate::device_agent_target::config_path(
                    fixture.service.0.host.state_dir(),
                    "local"
                )
                .exists()
            );
            fixture.service.shutdown().await;
            fixture.settings.shutdown().await;
            reaped(hub_pid);
        })
        .await
        .unwrap();
    }
}
