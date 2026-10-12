//! A device host is a machine with simulators or emulators on it (`device/DeviceHost.ts`). The device service only talks to this trait,
//! so the local host and any number of SSH hosts sit beside each other without the service telling them apart.
//!
//! Every ready host presents a loopback origin where expo-device-hub answers; the agent-device daemon is added only after agent access
//! is granted. For the local host both run on this machine; an SSH host forwards them here.
use crate::{
    local_device_host::{DeviceHostAgentReady, DeviceHostReady, DevicePhase, LocalDeviceHost},
    ssh_device_host::HostError,
};
use async_trait::async_trait;
use t3_contracts::*;

#[async_trait]
pub trait DeviceHost: Send + Sync {
    fn id(&self) -> DeviceHostId;
    /// What the host last reported about itself, without asking it again.
    async fn summary(&self) -> DeviceHostSummary;
    /// Asks the host which tools it has (`inspect`); the local host answers from its own state.
    async fn inspect(&self) -> Result<DeviceHostSummary, HostError> {
        Ok(self.summary().await)
    }
    async fn platform(&self, platform: DevicePlatform) -> DevicePlatformAvailability;
    /// Installs tools on first use and starts the helper processes. Idempotent: concurrent callers share one start.
    async fn ensure_ready(&self, on_phase: DevicePhase) -> Result<DeviceHostReady, DeviceHostUnavailableError>;
    /// Installs and starts agent-device after the user grants agent access.
    async fn ensure_agent_ready(&self, on_phase: DevicePhase) -> Result<DeviceHostAgentReady, DeviceHostUnavailableError>;
    /// Current endpoints when already running, without starting anything.
    fn current(&self) -> Option<DeviceHostReady>;
    fn current_agent(&self) -> Option<crate::device_agent_daemon::AgentDeviceEndpoint>;
    /// Changes whenever the host's endpoints are replaced, so a caller holding an old one can tell.
    fn generation(&self) -> u64;
    /// Stops only agent-device. Manual viewing through the hub stays available.
    async fn stop_agent(&self);
    /// Stops helpers. Devices themselves keep running; the user owns those.
    async fn stop(&self);
    /// Stops for good: what `stop` does, and nothing starts again.
    async fn shutdown(&self);
}

#[async_trait]
impl DeviceHost for LocalDeviceHost {
    fn id(&self) -> DeviceHostId {
        DeviceHostId::new("local").unwrap()
    }
    async fn summary(&self) -> DeviceHostSummary {
        LocalDeviceHost::summary(self).await
    }
    async fn platform(&self, platform: DevicePlatform) -> DevicePlatformAvailability {
        LocalDeviceHost::platform(self, platform).await
    }
    async fn ensure_ready(&self, on_phase: DevicePhase) -> Result<DeviceHostReady, DeviceHostUnavailableError> {
        LocalDeviceHost::ensure_ready(self, on_phase).await
    }
    async fn ensure_agent_ready(&self, on_phase: DevicePhase) -> Result<DeviceHostAgentReady, DeviceHostUnavailableError> {
        LocalDeviceHost::ensure_agent_ready(self, on_phase).await
    }
    fn current(&self) -> Option<DeviceHostReady> {
        LocalDeviceHost::current(self)
    }
    fn current_agent(&self) -> Option<crate::device_agent_daemon::AgentDeviceEndpoint> {
        LocalDeviceHost::current_agent(self)
    }
    fn generation(&self) -> u64 {
        LocalDeviceHost::generation(self)
    }
    async fn stop_agent(&self) {
        LocalDeviceHost::stop_agent(self).await
    }
    async fn stop(&self) {
        LocalDeviceHost::stop(self).await
    }
    async fn shutdown(&self) {
        LocalDeviceHost::shutdown(self).await
    }
}
