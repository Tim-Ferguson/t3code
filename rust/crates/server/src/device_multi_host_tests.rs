//! The device service with more than one host: independent state per host (a port of `DeviceMultiHost.test.ts`), the SSH host against a
//! fake `ssh` and a loopback stand-in for the hub (`SshDeviceHost.test.ts`, plus the reconnect the original leaves untested), and the
//! SSH hosts settings name coming and going. The fake `ssh` is `tests/fixtures/fake-ssh-device.py`; nothing here reaches a real host.
use super::*;
use crate::{
    device_host_resolver::{DeviceHostResolver, DeviceHostResolverOptions},
    local_device_host::LocalDeviceHostOptions,
    server_secret_store::ServerSecretStore,
    server_settings::SettingsOptions,
    ssh_device_host::{HostError, SshDeviceHost, SshRunner},
    test_support::HANG_BOUND,
};
use async_trait::async_trait;
use std::{collections::HashSet, os::unix::fs::PermissionsExt, path::{Path, PathBuf}};
use t3_remote::ssh::Ssh;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn noop() -> DevicePhase {
    Arc::new(|_, _| Box::pin(async {}))
}
fn config(id: &str) -> SshDeviceHostConfig {
    serde_json::from_value(json!({"id":id,"label":format!("Host {id}"),"target":format!("{id}.example")})).unwrap()
}
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// A loopback server that answers as the remote hub and agent-device daemon do, one request per connection. Dropping it stops listening.
struct Stub {
    port: u16,
    task: tokio::task::JoinHandle<()>,
    hits: Arc<Mutex<Vec<String>>>,
}
impl Drop for Stub {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn stub(devices: Value) -> Stub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let devices = Arc::new(Mutex::new(devices));
    let hits = Arc::new(Mutex::new(Vec::new()));
    let (served, recorded) = (devices.clone(), hits.clone());
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let (devices, hits) = (served.clone(), recorded.clone());
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(count) => request.extend_from_slice(&chunk[..count]),
                    }
                }
                let head = String::from_utf8_lossy(&request).into_owned();
                let line = head.lines().next().unwrap_or_default().to_owned();
                hits.lock().unwrap().push(line.clone());
                let body = match line.split(' ').take(2).collect::<Vec<_>>()[..] {
                    ["GET", "/readyz"] | ["GET", "/health"] => "ok".to_owned(),
                    ["GET", "/api/devices"] => devices.lock().unwrap().to_string(),
                    ["POST", "/api/devices/boot"] | ["POST", "/vendor/serve-sim/grid/api/start"] => json!({"ok":true}).to_string(),
                    _ => String::new(),
                };
                let status = if body.is_empty() { "404 Not Found" } else { "200 OK" };
                let reply = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = socket.write_all(reply.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Stub { port, task, hits }
}
fn emulator(id: &str) -> Value {
    json!({"id":id,"name":"Pixel","version":"36","platform":"android","booted":true,"physical":false})
}
fn simulator(id: &str, booted: bool) -> Value {
    json!({"id":id,"name":"iPhone","version":"26","platform":"ios","booted":booted,"physical":false})
}

/// The fake `ssh` and what it answers, in a directory of the test's own.
struct Remote {
    directory: tempfile::TempDir,
    ssh: PathBuf,
    hub: Stub,
    daemon: Stub,
}
impl Remote {
    async fn new(devices: Value) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let ssh = directory.path().join("ssh");
        std::fs::write(&ssh, include_str!("../tests/fixtures/fake-ssh-device.py").replace("@@DIRECTORY@@", &directory.path().to_string_lossy())).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let remote = Self { hub: stub(devices).await, daemon: stub(json!({})).await, directory, ssh };
        remote.reply();
        std::fs::write(remote.path("commands.json"), "{}").unwrap();
        remote
    }
    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }
    /// What `start` and `agent-start` answer, pointing at the stub servers.
    fn reply(&self) {
        let version = json!({"requiredVersion":"1.0.0","installedVersions":["1.0.0"],"runningVersion":null});
        let reply = json!({
            "nodePath":"/remote/node",
            "platforms":[{"platform":"ios","available":true},{"platform":"android","available":false,"reason":"Android SDK missing."}],
            "tools":{"hub":version,"agent":version},
            "hubPort":self.hub.port,"daemonPort":self.daemon.port,"token":"remote-token","entryPath":"/remote/agent-device.mjs",
            "helpers":{"serveSimAxSettings":null,"serveSimCli":null}
        });
        std::fs::write(self.path("reply.json"), reply.to_string()).unwrap();
    }
    fn ssh(&self) -> Ssh {
        Ssh::with_executable(self.ssh.to_string_lossy().into_owned())
    }
    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("calls.jsonl")).unwrap_or_default().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }
    fn modes(&self) -> Vec<String> {
        self.calls().iter().filter(|call| call["kind"] == "bootstrap").map(|call| call["mode"].as_str().unwrap().to_owned()).collect()
    }
    /// The processes of the forwards that were started, in order.
    fn forwarders(&self) -> Vec<i32> {
        let mut pids: Vec<i32> = Vec::new();
        for line in std::fs::read_to_string(self.path("forwarders.log")).unwrap_or_default().lines() {
            let pid = line.split(' ').next().unwrap().parse().unwrap();
            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
        pids
    }
    fn running(&self) -> usize {
        self.forwarders().into_iter().filter(|pid| alive(*pid)).count()
    }
}
fn host_with(remote: &Remote, id: &str, state_dir: &Path, on_ready: Option<crate::ssh_device_host::OnAgentReady>) -> (SshDeviceHost, tokio::sync::mpsc::UnboundedReceiver<(DeviceHostStatus, Option<String>)>) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let on_status: DevicePhase = Arc::new(move |status, detail| {
        let _ = sender.send((status, detail));
        Box::pin(async {})
    });
    let host = SshDeviceHost::new(config(id), remote.ssh(), state_dir, on_ready, Some(on_status));
    // A python stand-in can take a while to start on a loaded machine.
    host.set_forward_deadline(Duration::from_secs(60));
    (host, receiver)
}
async fn status_where(receiver: &mut tokio::sync::mpsc::UnboundedReceiver<(DeviceHostStatus, Option<String>)>, wanted: impl Fn(&(DeviceHostStatus, Option<String>)) -> bool) -> (DeviceHostStatus, Option<String>) {
    tokio::time::timeout(HANG_BOUND, async {
        loop {
            let status = receiver.recv().await.expect("the host keeps reporting");
            if wanted(&status) {
                return status;
            }
        }
    })
    .await
    .expect("the awaited status arrives")
}
async fn waited(taps: &mut tokio::sync::mpsc::UnboundedReceiver<Duration>, delay: Duration) {
    let seen = tokio::time::timeout(HANG_BOUND, async {
        loop {
            let seen = taps.recv().await.expect("the host keeps waiting");
            if seen == delay {
                return seen;
            }
        }
    })
    .await
    .expect("the wait is made");
    assert_eq!(seen, delay);
}
/// Moves a paused clock past a wait the host has registered, and lets time run again.
async fn pass(delay: Duration) {
    tokio::time::pause();
    tokio::time::advance(delay).await;
    tokio::time::resume();
}
#[tokio::test]
async fn a_probe_start_and_stop_use_one_owner_and_the_forward_follows_the_host() {
    tokio::time::timeout(HANG_BOUND, async {
        let remote = Remote::new(json!({"simulators":[],"emulators":[]})).await;
        let state = tempfile::tempdir().unwrap();
        let rejected = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let on_ready: crate::ssh_device_host::OnAgentReady = {
            let rejected = rejected.clone();
            Arc::new(move |_| {
                let reject = rejected.load(std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move { if reject { Err(HostError { host_id: "test".into(), step: "configuring agent access".into(), cause: "fixture failure".into() }) } else { Ok(()) } })
            })
        };
        let (host, _statuses) = host_with(&remote, "test", state.path(), Some(on_ready));
        // The first forward dies at once, as a port that was taken in between would: the host connects again.
        std::fs::write(remote.path("fail-forward"), "").unwrap();
        // The dead forward never answers, and must not take the 15 s the original waits.
        host.set_forward_deadline(Duration::from_millis(1500));
        let ready = host.ensure_ready(noop()).await.unwrap();
        host.set_forward_deadline(Duration::from_secs(60));
        assert_eq!(remote.modes(), ["probe", "start", "start"]);
        assert_eq!(remote.running(), 1);
        assert_eq!(ready.node_path, PathBuf::from("/remote/node"));
        assert!(ready.origin.starts_with("http://127.0.0.1:"));
        let probed = crate::ssh_device_host::probe(&remote.ssh(), &config("test"), &crate::ssh_device_host::owner_for(state.path(), "test")).await.unwrap();
        assert!(probed.hub_installed);
        let owners: HashSet<_> = remote.calls().iter().filter(|call| call["kind"] == "bootstrap").map(|call| call["owner"].as_str().unwrap().to_owned()).collect();
        assert_eq!(owners.len(), 1);
        let owner = owners.into_iter().next().unwrap();
        assert!(owner.len() == 24 && owner.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(host.platform(DevicePlatform::Ios).await.available);
        assert!(host.summary().await.hub_installed);
        // Agent access whose config cannot be written leaves nothing running and stops the agent on the host.
        let failed = host.ensure_agent_ready(noop()).await.unwrap_err();
        assert_eq!(failed.host_id.as_str(), "test");
        assert_eq!(remote.modes().last().unwrap(), "stop-agent");
        assert_eq!(remote.running(), 0);
        assert!(host.current().is_none());
        rejected.store(false, std::sync::atomic::Ordering::SeqCst);
        let agent = host.ensure_agent_ready(noop()).await.unwrap();
        assert_eq!(agent.agent_device.token, "remote-token");
        assert!(agent.agent_device.base_url.starts_with("http://127.0.0.1:"));
        assert_eq!(host.current_agent().as_ref(), Some(&agent.agent_device));
        assert!(host.summary().await.agent_device_installed);
        host.stop_agent().await;
        assert!(host.current_agent().is_none());
        assert!(host.current().is_some());
        assert_eq!(remote.running(), 1);
        assert_eq!(remote.modes().iter().filter(|mode| *mode == "stop-agent").count(), 2);
        host.stop().await;
        assert_eq!(remote.running(), 0);
        assert_eq!(remote.modes().last().unwrap(), "stop");
        assert!(host.current().is_none());
        host.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_dropped_forward_reconnects_with_a_doubling_delay_and_a_new_endpoint() {
    tokio::time::timeout(HANG_BOUND, async {
        let remote = Remote::new(json!({"simulators":[],"emulators":[]})).await;
        let state = tempfile::tempdir().unwrap();
        let (host, mut statuses) = host_with(&remote, "test", state.path(), None);
        let mut waits = host.waits();
        let first = host.ensure_ready(noop()).await.unwrap();
        let old = remote.forwarders();
        assert_eq!(old.len(), 1);
        // The first reconnect fails (the host cannot be reached), the second works.
        std::fs::write(remote.path("fail-start"), "").unwrap();
        waited(&mut waits, Duration::from_secs(10)).await;
        unsafe { libc::kill(old[0], libc::SIGKILL) };
        let (_, detail) = status_where(&mut statuses, |(status, detail)| *status == DeviceHostStatus::Starting && detail.is_some()).await;
        assert_eq!(detail.as_deref(), Some("Reconnecting to device host…"));
        waited(&mut waits, Duration::from_secs(1)).await;
        assert!(host.current().is_none());
        pass(Duration::from_secs(1)).await;
        let (_, detail) = status_where(&mut statuses, |(status, _)| *status == DeviceHostStatus::Failed).await;
        assert_eq!(detail.as_deref(), Some("Device host test failed while start."));
        waited(&mut waits, Duration::from_secs(2)).await;
        std::fs::remove_file(remote.path("fail-start")).unwrap();
        pass(Duration::from_secs(2)).await;
        status_where(&mut statuses, |(status, _)| *status == DeviceHostStatus::Ready).await;
        let second = host.current().expect("the host is connected again");
        assert_ne!(second.origin, first.origin);
        assert_eq!(second.generation, first.generation + 1);
        assert!(!alive(old[0]));
        assert_eq!(remote.running(), 1);
        host.shutdown().await;
        assert_eq!(remote.running(), 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn helpers_that_stop_answering_are_noticed_on_the_health_interval() {
    tokio::time::timeout(HANG_BOUND, async {
        let mut remote = Remote::new(json!({"simulators":[],"emulators":[]})).await;
        let state = tempfile::tempdir().unwrap();
        let (host, mut statuses) = host_with(&remote, "test", state.path(), None);
        let mut waits = host.waits();
        host.ensure_ready(noop()).await.unwrap();
        waited(&mut waits, Duration::from_secs(10)).await;
        // The SSH forward stays up while the hub behind it is gone.
        remote.hub.task.abort();
        // Joining the aborted task is what drops its listener.
        assert!((&mut remote.hub.task).await.unwrap_err().is_cancelled());
        pass(Duration::from_secs(10)).await;
        let (_, detail) = status_where(&mut statuses, |(status, detail)| *status == DeviceHostStatus::Starting && detail.is_some()).await;
        assert_eq!(detail.as_deref(), Some("Reconnecting to device host…"));
        host.shutdown().await;
        assert_eq!(remote.running(), 0);
    })
    .await
    .unwrap();
}

/// A host the service can be given without anything behind it but a hub stand-in.
struct Fake {
    id: DeviceHostId,
    kind: DeviceHostKind,
    origin: String,
    offline: bool,
}
impl Fake {
    fn new(id: &str, origin: String) -> Arc<dyn DeviceHost> {
        Arc::new(Self { id: DeviceHostId::new(id).unwrap(), kind: if id == "b" { DeviceHostKind::Ssh } else { DeviceHostKind::Local }, origin, offline: id == "offline" })
    }
    fn ready(&self) -> DeviceHostReady {
        DeviceHostReady::remote(self.id.clone(), 0, SshRunner::new(Ssh::with_executable("/usr/bin/true"), config("fake")), "/node".into(), self.origin.clone(), None, None)
    }
}
#[async_trait]
impl DeviceHost for Fake {
    fn id(&self) -> DeviceHostId {
        self.id.clone()
    }
    async fn summary(&self) -> DeviceHostSummary {
        let platforms = if self.kind == DeviceHostKind::Ssh { json!([]) } else { json!([{"platform":"android","available":true}]) };
        serde_json::from_value(json!({"id":self.id,"label":self.id,"kind":self.kind,"hubInstalled":true,"agentDeviceInstalled":true,"platforms":platforms})).unwrap()
    }
    async fn platform(&self, platform: DevicePlatform) -> DevicePlatformAvailability {
        DevicePlatformAvailability { platform, available: true, reason: None }
    }
    async fn ensure_ready(&self, _: DevicePhase) -> Result<DeviceHostReady, DeviceHostUnavailableError> {
        if self.offline {
            return Err(DeviceHostUnavailableError { tag: DeviceHostUnavailableErrorTag::DeviceHostUnavailableError, host_id: self.id.clone(), reason: "Device support failed during connect.".into(), cause: None });
        }
        Ok(self.ready())
    }
    async fn ensure_agent_ready(&self, _: DevicePhase) -> Result<DeviceHostAgentReady, DeviceHostUnavailableError> {
        Ok(DeviceHostAgentReady { host: self.ready(), agent_device: self.current_agent().unwrap() })
    }
    fn current(&self) -> Option<DeviceHostReady> {
        Some(self.ready())
    }
    fn current_agent(&self) -> Option<crate::device_agent_daemon::AgentDeviceEndpoint> {
        Some(crate::device_agent_daemon::AgentDeviceEndpoint { base_url: self.origin.clone(), token: "test".into(), entry_path: "/agent-device".into(), pid: None, version: None })
    }
    fn generation(&self) -> u64 {
        0
    }
    async fn stop_agent(&self) {}
    async fn stop(&self) {}
    async fn shutdown(&self) {}
}
async fn settings_in(root: &Path) -> SettingsService {
    let mut options = SettingsOptions::file(root.join("settings.json"), ServerSecretStore::open(root.join("secrets")).unwrap());
    options.watch = false;
    SettingsService::start(options).await.unwrap()
}
/// A local host that has nothing to run devices with, so only the hosts a test adds are ever started.
fn idle_local(root: &Path) -> LocalDeviceHost {
    let mut options = LocalDeviceHostOptions::host(root.join("state"));
    options.platform = "linux".into();
    options.environment = Default::default();
    LocalDeviceHost::new(options, crate::device_toolchain::DeviceToolchain::new(crate::device_toolchain::ToolchainOptions::new(root.join("state"))))
}
fn hosts_of(state: &DeviceServiceState) -> Vec<String> {
    state.hosts.iter().map(|host| host.id.as_str().to_owned()).collect()
}
fn id(value: &str) -> DeviceHostId {
    DeviceHostId::new(value).unwrap()
}

#[tokio::test]
async fn hosts_stay_independent_when_ids_collide_and_another_host_fails() {
    tokio::time::timeout(HANG_BOUND, async {
        let root = tempfile::tempdir().unwrap();
        let settings = settings_in(root.path()).await;
        // Both hosts' hubs list a device with the same serial.
        let (hub_a, hub_b, hub_offline) = (stub(json!({"simulators":[],"emulators":[emulator("emulator-5554")]})).await, stub(json!({"simulators":[],"emulators":[emulator("emulator-5554")]})).await, stub(json!({})).await);
        let origin = |hub: &Stub| format!("http://127.0.0.1:{}", hub.port);
        let service = DeviceService::with_hosts(settings.clone(), idle_local(root.path()), vec![Fake::new("a", origin(&hub_a)), Fake::new("b", origin(&hub_b)), Fake::new("offline", origin(&hub_offline))]).await.unwrap();
        service.configure(serde_json::from_value(json!({"enabled":true,"agentAccessEnabled":true})).unwrap()).await.unwrap();
        assert!(service.agent_readiness_if_supported(Some(&id("b"))).await.unwrap().is_some());
        let listed = service.list().await.unwrap();
        let mut owners: Vec<_> = listed.devices.iter().map(|device| device.host_id.as_str().to_owned()).collect();
        owners.sort();
        assert_eq!(owners, ["a", "b"]);
        assert_eq!(listed.host_statuses[&id("offline")].status, DeviceHostStatus::Failed);
        assert_eq!(hosts_of(&listed), ["local", "a", "b", "offline"]);
        let thread = ThreadId::new("thread").unwrap();
        let open = |host: &str| serde_json::from_value::<DeviceOpenInput>(json!({"threadId":thread,"hostId":host,"deviceId":"emulator-5554","platform":"android"})).unwrap();
        for host in ["a", "b"] {
            service.open(open(host)).await.unwrap();
        }
        service.close(serde_json::from_value(json!({"threadId":thread,"hostId":"a","deviceId":"emulator-5554"})).unwrap()).await.unwrap();
        let state = service.snapshot();
        assert_eq!(state.devices.len(), 2);
        assert_eq!(state.sessions.iter().map(|session| session.host_id.as_str()).collect::<Vec<_>>(), ["b"]);
        assert_eq!(state.host_statuses[&id("a")].status, DeviceHostStatus::Ready);
        assert_eq!(state.host_statuses[&id("offline")].status, DeviceHostStatus::Failed);
        // Writing a host's agent config holds the lifecycle lock, so a host replaced meanwhile waits for the write.
        let (entered, started) = tokio::sync::oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (release, released) = std::sync::mpsc::channel();
        let released = Mutex::new(released);
        let order = Arc::new(Mutex::new(Vec::new()));
        *service.0.write_hook.lock().unwrap() = Some({
            let order = order.clone();
            Arc::new(move || {
                order.lock().unwrap().push("write started");
                entered.lock().unwrap().take().unwrap().send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
                order.lock().unwrap().push("write finished");
            })
        });
        let targeting = tokio::spawn({
            let service = service.clone();
            let device = DeviceId::new("emulator-5554").unwrap();
            let thread = thread.clone();
            async move { service.agent_target(&thread, &id("b"), &device).await }
        });
        started.await.unwrap();
        let replacing = tokio::spawn({
            let service = service.clone();
            let order = order.clone();
            let replacement = Fake::new("b", origin(&hub_b));
            async move {
                let _lifecycle = service.0.lifecycle.lock().await;
                order.lock().unwrap().push("replace");
                service.0.registry.lock().unwrap().hosts.retain(|host| host.id().as_str() != "b");
                service.0.registry.lock().unwrap().hosts.push(replacement);
                service.refresh_hosts().await;
            }
        });
        release.send(()).unwrap();
        targeting.await.unwrap().unwrap();
        replacing.await.unwrap();
        assert_eq!(*order.lock().unwrap(), ["write started", "write finished", "replace"]);
        let replaced = service.snapshot();
        assert!(replaced.sessions.is_empty());
        assert_eq!(replaced.devices.iter().map(|device| device.host_id.as_str()).collect::<Vec<_>>(), ["a"]);
        assert!(!replaced.host_statuses.contains_key(&id("b")));
        assert_eq!(hosts_of(&replaced), ["local", "a", "offline", "b"]);
        // A host that is gone takes its sessions with it, and a late status for it is dropped.
        service.open(open("b")).await.unwrap();
        service.0.registry.lock().unwrap().hosts.retain(|host| host.id().as_str() != "b");
        service.refresh_hosts().await;
        service.status(&id("b"), DeviceHostStatus::Ready, None);
        assert!(!service.snapshot().host_statuses.contains_key(&id("b")));
        assert!(service.snapshot().sessions.is_empty());
        service.agent_readiness_if_supported(Some(&id("a"))).await.unwrap();
        assert_eq!(service.snapshot().host_statuses[&id("a")].status, DeviceHostStatus::Ready);
        service.configure(serde_json::from_value(json!({"enabled":false})).unwrap()).await.unwrap();
        assert!(service.snapshot().host_statuses.is_empty());
        service.shutdown().await;
        settings.shutdown().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ssh_hosts_follow_settings_and_serve_devices_sessions_and_agent_access() {
    tokio::time::timeout(HANG_BOUND, async {
        let root = tempfile::tempdir().unwrap();
        let remote = Remote::new(json!({"simulators":[simulator("remote-sim", true)],"emulators":[]})).await;
        let settings = settings_in(root.path()).await;
        let resolver = DeviceHostResolver::new(DeviceHostResolverOptions { ssh_binary: remote.ssh.clone(), local_addresses: Arc::new(HashSet::new), ..Default::default() });
        let service = DeviceService::new(settings.clone(), idle_local(root.path())).await.unwrap();
        service.manage_ssh_hosts(resolver.clone()).await;
        assert_eq!(hosts_of(&service.snapshot()), ["local"]);
        let mut updates = service.subscribe();
        settings.update(serde_json::from_value(json!({"deviceHosts":[config("remote-a")]})).unwrap()).await.unwrap();
        // Settings name the host; the service has it, but nothing has connected to it yet.
        loop {
            let state = tokio::time::timeout(HANG_BOUND, updates.recv()).await.unwrap().unwrap();
            if hosts_of(&state) == ["local", "remote-a"] {
                break;
            }
        }
        assert_eq!(remote.running(), 0);
        let listed = service.configure(serde_json::from_value(json!({"enabled":true,"agentAccessEnabled":true})).unwrap()).await.unwrap();
        // The host connected: its hub is the forwarded one, and its devices carry its id.
        assert_eq!(listed.host_statuses[&id("remote-a")].status, DeviceHostStatus::Ready);
        assert_eq!(listed.devices.iter().map(|device| (device.host_id.as_str(), device.id.as_str())).collect::<Vec<_>>(), [("remote-a", "remote-sim")]);
        assert!(!listed.host_statuses.contains_key(&id("local")) || listed.host_statuses[&id("local")].status != DeviceHostStatus::Failed);
        assert_eq!(remote.running(), 1);
        let origin = service.current_readiness(Some(&id("remote-a"))).unwrap().origin;
        assert!(origin.starts_with("http://127.0.0.1:") && !origin.ends_with(&remote.hub.port.to_string()));
        let thread = ThreadId::new("thread").unwrap();
        let session = service.open(serde_json::from_value(json!({"threadId":thread,"hostId":"remote-a","deviceId":"remote-sim","platform":"ios"})).unwrap()).await.unwrap();
        assert_eq!(session.host_id.as_str(), "remote-a");
        assert!(remote.hub.hits.lock().unwrap().iter().any(|hit| hit == "POST /vendor/serve-sim/grid/api/start HTTP/1.1"));
        // The agent endpoint is the forwarded daemon, written for this host alone.
        let target = service.agent_target(&thread, &id("remote-a"), &DeviceId::new("remote-sim").unwrap()).await.unwrap();
        let file = crate::device_agent_target::config_path(&root.path().join("state"), "remote-a");
        assert_eq!(target[1], file.to_string_lossy());
        let written: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(written["daemonAuthToken"], "remote-token");
        assert_ne!(written["daemonBaseUrl"], format!("http://127.0.0.1:{}", remote.daemon.port));
        // Changing the host's label replaces it: its state is dropped, and the old connection ends.
        let before = remote.forwarders();
        settings.update(serde_json::from_value(json!({"deviceHosts":[{"id":"remote-a","label":"Renamed","target":"remote-a.example"}]})).unwrap()).await.unwrap();
        loop {
            let state = tokio::time::timeout(HANG_BOUND, updates.recv()).await.unwrap().unwrap();
            if state.hosts.iter().any(|host| host.label.as_str() == "Renamed") && state.sessions.is_empty() {
                assert!(state.devices.is_empty() && !state.host_statuses.contains_key(&id("remote-a")));
                break;
            }
        }
        // The reconcile that published this state is still ending the old connection; it holds this until it is done.
        drop(service.0.ssh_hosts.lock().await);
        assert!(before.iter().all(|pid| !alive(*pid)));
        // A host settings no longer name is stopped, and its agent config goes.
        service.list().await.unwrap();
        assert_eq!(remote.running(), 1);
        settings.update(serde_json::from_value(json!({"deviceHosts":[]})).unwrap()).await.unwrap();
        loop {
            let state = tokio::time::timeout(HANG_BOUND, updates.recv()).await.unwrap().unwrap();
            if hosts_of(&state) == ["local"] {
                break;
            }
        }
        drop(service.0.ssh_hosts.lock().await);
        assert_eq!(remote.running(), 0);
        assert_eq!(remote.modes().last().unwrap(), "stop");
        assert!(!file.exists());
        service.shutdown().await;
        resolver.shutdown().await;
        settings.shutdown().await;
    })
    .await
    .unwrap();
}
