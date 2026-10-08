//! Source PortScanner discovery: owned native listener query, bounded manual
//! HTTP probes, PID-sensitive positive/negative cache and scoped polling.
use crate::{
    resource_ports::{self, LocalServer, TerminalRegistry},
    terminal_activity::ProcessId,
    terminal_inspector::{InspectionError, NativeProcessTable},
};
use futures_util::{
    future::{BoxFuture, FutureExt, Shared},
    stream::{self, StreamExt},
};
use indexmap::{IndexMap, IndexSet};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use url::Url;
/// Bind the shared native process snapshot and terminal ownership registry.
/// TerminalManager retains its source fallback when the sidecar is unavailable.
pub fn configure_terminal_tracking(
    terminal_options: &mut crate::terminal_manager::TerminalManagerOptions,
    telemetry: crate::native_telemetry::NativeTelemetryClient,
    registry: TerminalRegistry,
) {
    let primary = telemetry;
    terminal_options.process_table = Some(std::sync::Arc::new(move || {
        let primary = primary.clone();
        Box::pin(async move {
            let entries = primary.process_table().await.map_err(|error| {
                crate::terminal_inspector::InspectionError::source("resource-monitor", error)
            })?;
            Ok(crate::terminal_activity::ProcessTable::from_entries(
                entries
                    .into_iter()
                    .map(|entry| (entry.pid.0 as f64, entry.ppid.0 as f64, entry.name)),
            ))
        })
    }));
    let owners = registry.clone();
    terminal_options.register_terminal_processes =
        Some(std::sync::Arc::new(move |thread, terminal, pids| {
            let owners = owners.clone();
            Box::pin(async move {
                if let Err(error) =
                    owners.register(&thread, &terminal, pids.into_iter().map(|pid| pid.number()))
                {
                    tracing::error!(%error,"registering terminal processes failed");
                }
            })
        }));
    terminal_options.unregister_terminal = Some(std::sync::Arc::new(move |thread, terminal| {
        let registry = registry.clone();
        Box::pin(async move {
            registry.unregister(&thread, &terminal);
        })
    }));
}
pub const COMMON_DEV_PORTS: [u16; 16] = [
    3000, 3001, 3333, 4173, 4200, 4321, 5000, 5173, 5174, 5175, 5500, 8000, 8080, 8081, 8888, 9000,
];
pub type ListenerSource =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Vec<LocalServer>, InspectionError>> + Send + Sync>;
pub type WebProbe = Arc<dyn Fn(String) -> BoxFuture<'static, bool> + Send + Sync>;
pub type DiscoveryListener = Arc<dyn Fn(Vec<LocalServer>) -> BoxFuture<'static, ()> + Send + Sync>;
pub type DiscoveryClock = Arc<dyn Fn() -> i64 + Send + Sync>;
pub struct PortDiscoveryOptions {
    pub registry: TerminalRegistry,
    pub listeners: ListenerSource,
    pub probe: WebProbe,
    pub clock: DiscoveryClock,
    pub poll_interval: Duration,
    native: Option<NativeProcessTable>,
}
impl PortDiscoveryOptions {
    pub fn host(platform: &str, registry: TerminalRegistry) -> Result<Self, reqwest::Error> {
        let windows = platform == "win32";
        let (command, args, label) = if windows {
            (
                "powershell.exe",
                vec![
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Get-NetTCPConnection -State Listen -ErrorAction Stop | ForEach-Object { $processName = (Get-Process -Id $_.OwningProcess -ErrorAction SilentlyContinue).ProcessName; Write-Output \"$($_.LocalAddress)|$($_.LocalPort)|$($_.OwningProcess)|$processName\" }",
                ],
                "windows-listeners",
            )
        } else {
            (
                "lsof",
                vec!["-iTCP", "-sTCP:LISTEN", "-P", "-n", "-F", "pcn"],
                "lsof",
            )
        };
        let native = NativeProcessTable::command(
            command.into(),
            args.into_iter().map(String::from).collect(),
            label,
            Duration::from_secs(5),
            1024 * 1024,
        );
        let source = native.clone();
        let owners = registry.clone();
        let listeners = Arc::new(move || {
            let source = source.clone();
            let owners = owners.snapshot();
            async move {
                let output = source.output().await?;
                // Source consumes returned diagnostics, even nonzero/truncated.
                let stdout = String::from_utf8_lossy(&output.stdout);
                Ok(if windows {
                    resource_ports::parse_windows(&stdout, &owners)
                } else {
                    resource_ports::parse_lsof(&stdout, &owners)
                })
            }
            .boxed()
        });
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()?;
        let probe = Arc::new(move |raw: String| {
            let http = http.clone();
            async move {
                let Ok(url) = Url::parse(&raw) else {
                    return false;
                };
                // Node fetch rejects embedded credentials before sending a
                // request; reqwest otherwise synthesizes BasicAuth here.
                if !url.username().is_empty()
                    || url.password().is_some_and(|password| !password.is_empty())
                {
                    return false;
                }
                let Ok(response) = http.get(url).send().await else {
                    return false;
                };
                resource_ports::web_response(
                    response.status().as_u16(),
                    response
                        .headers()
                        .get("location")
                        .and_then(|value| value.to_str().ok()),
                    response
                        .headers()
                        .get("content-type")
                        .and_then(|value| value.to_str().ok()),
                )
            }
            .boxed()
        });
        Ok(Self {
            registry,
            listeners,
            probe,
            clock: Arc::new(|| chrono::Utc::now().timestamp_millis()),
            poll_interval: Duration::from_secs(3),
            native: Some(native),
        })
    }
    pub fn custom(
        registry: TerminalRegistry,
        listeners: ListenerSource,
        probe: WebProbe,
        clock: DiscoveryClock,
    ) -> Self {
        Self {
            registry,
            listeners,
            probe,
            clock,
            poll_interval: Duration::from_secs(3),
            native: None,
        }
    }
}
#[derive(Debug, thiserror::Error)]
#[error("Port discovery is stopped")]
pub struct DiscoveryStopped;
#[derive(Clone)]
struct Probe {
    pid: Option<ProcessId>,
    is_web: bool,
    expires: i64,
}
struct Group {
    server: LocalServer,
    urls: Vec<String>,
    configured_key: Option<String>,
}
struct Snapshot {
    discovered: Vec<LocalServer>,
    configured: HashMap<String, LocalServer>,
}
struct Subscription {
    urls: Vec<String>,
    last: Vec<LocalServer>,
    publish: DiscoveryListener,
}
#[derive(Default)]
struct State {
    retains: usize,
    subscriptions: IndexMap<uuid::Uuid, Subscription>,
}
struct Service {
    options: PortDiscoveryOptions,
    state: Mutex<State>,
    cache: Mutex<HashMap<String, Probe>>,
    scan: tokio::sync::Mutex<()>,
    stop: watch::Sender<bool>,
}
struct Inner {
    service: Arc<Service>,
    worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    shutdown: tokio::sync::Mutex<()>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.service.stop.send_replace(true);
    }
}
#[derive(Clone)]
pub struct PortDiscovery(Arc<Inner>);
pub struct PortRetention {
    service: Arc<Service>,
}
impl Drop for PortRetention {
    fn drop(&mut self) {
        let mut state = self.service.state.lock().unwrap();
        state.retains = state.retains.saturating_sub(1);
    }
}
pub struct PortCallbackSubscription {
    service: Arc<Service>,
    id: uuid::Uuid,
}
impl Drop for PortCallbackSubscription {
    fn drop(&mut self) {
        self.service
            .state
            .lock()
            .unwrap()
            .subscriptions
            .shift_remove(&self.id);
    }
}
pub struct PortSubscription {
    service: Arc<Service>,
    id: uuid::Uuid,
    receiver: mpsc::UnboundedReceiver<Vec<LocalServer>>,
    stopped: watch::Receiver<bool>,
}
impl Drop for PortSubscription {
    fn drop(&mut self) {
        self.service
            .state
            .lock()
            .unwrap()
            .subscriptions
            .shift_remove(&self.id);
    }
}
impl PortSubscription {
    pub async fn recv(&mut self) -> Option<Vec<LocalServer>> {
        tokio::select! {biased;_=self.stopped.wait_for(|stop|*stop)=>None,item=self.receiver.recv()=>item}
    }
}
impl PortDiscovery {
    pub fn new(options: PortDiscoveryOptions) -> Self {
        let (stop, mut stopped) = watch::channel(false);
        let service = Arc::new(Service {
            options,
            state: Mutex::new(State::default()),
            cache: Mutex::new(HashMap::new()),
            scan: tokio::sync::Mutex::new(()),
            stop,
        });
        let poll = service.clone();
        let worker = tokio::spawn(async move {
            loop {
                if *stopped.borrow() {
                    break;
                }
                if poll.state.lock().unwrap().retains > 0 {
                    let _ = poll.poll().await;
                }
                tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>break,_=tokio::time::sleep(poll.options.poll_interval)=>{}}
            }
            if let Some(native) = &poll.options.native {
                native.shutdown().await;
            }
        });
        Self(Arc::new(Inner {
            service,
            worker: tokio::sync::Mutex::new(Some(worker)),
            shutdown: tokio::sync::Mutex::new(()),
        }))
    }
    /// Shared service clock for source callback timestamps and cache deadlines.
    pub fn clock(&self) -> DiscoveryClock {
        self.0.service.options.clock.clone()
    }
    pub fn registry(&self) -> TerminalRegistry {
        self.0.service.options.registry.clone()
    }
    pub async fn scan(&self, urls: &[String]) -> Result<Vec<LocalServer>, DiscoveryStopped> {
        let urls = resource_ports::configured_urls(urls);
        let snapshot = self.0.service.snapshot(&urls).await?;
        Ok(project(&snapshot, &urls))
    }
    pub fn subscribe(
        &self,
        urls: &[String],
        initial: Vec<LocalServer>,
    ) -> Result<PortSubscription, DiscoveryStopped> {
        let service = self.0.service.clone();
        let id = uuid::Uuid::new_v4();
        let (send, receiver) = mpsc::unbounded_channel();
        let mut state = service.state.lock().unwrap();
        if *service.stop.borrow() {
            return Err(DiscoveryStopped);
        }
        state.subscriptions.insert(
            id,
            Subscription {
                urls: resource_ports::configured_urls(urls),
                last: initial,
                publish: Arc::new(move |servers| {
                    let send = send.clone();
                    async move {
                        let _ = send.send(servers);
                    }
                    .boxed()
                }),
            },
        );
        drop(state);
        Ok(PortSubscription {
            stopped: service.stop.subscribe(),
            service,
            id,
            receiver,
        })
    }
    pub fn subscribe_callback(
        &self,
        urls: &[String],
        initial: Vec<LocalServer>,
        publish: DiscoveryListener,
    ) -> Result<PortCallbackSubscription, DiscoveryStopped> {
        let service = self.0.service.clone();
        let id = uuid::Uuid::new_v4();
        let mut state = service.state.lock().unwrap();
        if *service.stop.borrow() {
            return Err(DiscoveryStopped);
        }
        state.subscriptions.insert(
            id,
            Subscription {
                urls: resource_ports::configured_urls(urls),
                last: initial,
                publish,
            },
        );
        drop(state);
        Ok(PortCallbackSubscription { service, id })
    }
    pub async fn retain(&self) -> Result<PortRetention, DiscoveryStopped> {
        let service = self.0.service.clone();
        let first = {
            let mut state = service.state.lock().unwrap();
            if *service.stop.borrow() {
                return Err(DiscoveryStopped);
            }
            let first = state.retains == 0;
            state.retains += 1;
            first
        };
        let retention = PortRetention {
            service: service.clone(),
        };
        if first {
            service.poll().await?;
        }
        Ok(retention)
    }
    /// Completes when shutdown begins, including while a consumer awaits output.
    pub async fn closed(&self) {
        let mut stopped = self.0.service.stop.subscribe();
        let _ = stopped.wait_for(|stop| *stop).await;
    }
    pub async fn shutdown(&self) {
        let _serial = self.0.shutdown.lock().await;
        self.0.service.stop.send_replace(true);
        let mut worker = self.0.worker.lock().await;
        if let Some(worker) = worker.as_mut() {
            if let Err(error) = worker.await {
                tracing::error!(%error,"port discovery worker failed");
            }
        }
        worker.take();
        // Public scan callers observe stop cancellation; their native output
        // requests have already been reaped by the worker's owned-source join.
    }
}
impl Service {
    async fn snapshot(&self, urls: &[String]) -> Result<Snapshot, DiscoveryStopped> {
        let mut stopped = self.stop.subscribe();
        tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>Err(DiscoveryStopped),result=self.snapshot_active(urls)=>Ok(result)}
    }
    async fn snapshot_active(&self, urls: &[String]) -> Snapshot {
        let _scan = self.scan.lock().await;
        let servers = match (self.options.listeners)().await {
            Ok(servers) => servers,
            Err(error) => {
                tracing::debug!(%error,"preview listener query failed; falling back to common-port probes");
                fallback().await
            }
        };
        let mut groups = vec![];
        let servers_by_key = servers
            .iter()
            .map(|server| {
                (
                    resource_ports::server_key(&server.host, server.port),
                    server.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        let mut configured_keys = IndexSet::new();
        for raw in urls {
            let url = Url::parse(raw).expect("normalized configured URL");
            let port = resource_ports::url_port(&url);
            let key = resource_ports::cache_key(raw).unwrap();
            if !configured_keys.insert(key.clone()) {
                continue;
            }
            let server = servers_by_key
                .get(&resource_ports::server_key(url.host_str().unwrap(), port))
                .cloned()
                .unwrap_or(LocalServer {
                    host: url.host_str().unwrap().into(),
                    port,
                    url: raw.clone(),
                    process_name: None,
                    pid: None,
                    terminal: None,
                });
            groups.push(Group {
                server,
                urls: vec![raw.clone()],
                configured_key: Some(key),
            });
        }
        for server in servers {
            let urls = vec![
                format!("http://{}:{}", server.host, server.port),
                format!("https://{}:{}", server.host, server.port),
            ];
            groups.push(Group {
                server,
                urls,
                configured_key: None,
            });
        }
        let now = (self.options.clock)();
        let cached = self.cache.lock().unwrap().clone();
        type ProbeFuture = Shared<BoxFuture<'static, (Probe, bool)>>;
        let batch = Mutex::new(HashMap::<(String, Option<ProcessId>), ProbeFuture>::new());
        let mut probed = stream::iter(groups.into_iter().enumerate().map(|(index, group)| {
            let cached = &cached;
            let batch = &batch;
            async move {
                let mut probes = vec![];
                let mut visible = None;
                for url in &group.urls {
                    let key = resource_ports::cache_key(url).unwrap();
                    let identity = (key.clone(), group.server.pid);
                    let pending = {
                        let mut batch = batch.lock().unwrap();
                        batch
                            .entry(identity)
                            .or_insert_with(|| {
                                if let Some(probe) = cached.get(&key).filter(|probe| {
                                    probe.pid == group.server.pid && probe.expires > now
                                }) {
                                    let probe = probe.clone();
                                    async move { (probe, false) }.boxed().shared()
                                } else {
                                    let probe = self.options.probe.clone();
                                    let url = url.clone();
                                    let pid = group.server.pid;
                                    async move {
                                        (
                                            Probe {
                                                pid,
                                                is_web: probe(url).await,
                                                expires: 0,
                                            },
                                            true,
                                        )
                                    }
                                    .boxed()
                                    .shared()
                                }
                            })
                            .clone()
                    };
                    let (probe, fresh) = pending.await;
                    let is_web = probe.is_web;
                    probes.push((key, probe, fresh));
                    if is_web {
                        visible = Some(url.clone());
                        break;
                    }
                }
                (index, group, probes, visible)
            }
        }))
        .buffer_unordered(16)
        .collect::<Vec<_>>()
        .await;
        probed.sort_by_key(|(index, _, _, _)| *index);
        let completed = (self.options.clock)();
        let mut next = cached
            .into_iter()
            .filter(|(_, probe)| probe.expires > completed)
            .collect::<HashMap<_, _>>();
        let mut snapshot = Snapshot {
            discovered: vec![],
            configured: HashMap::new(),
        };
        for (_, group, probes, visible) in probed {
            for (key, mut probe, fresh) in probes {
                if fresh {
                    probe.expires = completed.saturating_add(15000);
                }
                next.insert(key, probe);
            }
            if let Some(url) = visible {
                let server = LocalServer {
                    url,
                    ..group.server
                };
                if let Some(key) = group.configured_key {
                    snapshot.configured.insert(key, server);
                } else {
                    snapshot.discovered.push(server);
                }
            }
        }
        *self.cache.lock().unwrap() = next;
        snapshot
    }
    async fn poll(&self) -> Result<(), DiscoveryStopped> {
        let urls = self
            .state
            .lock()
            .unwrap()
            .subscriptions
            .values()
            .flat_map(|subscription| subscription.urls.iter().cloned())
            .collect::<IndexSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let snapshot = self.snapshot(&urls).await?;
        let notifications = {
            let mut state = self.state.lock().unwrap();
            let mut notifications = vec![];
            for subscription in state.subscriptions.values_mut() {
                let next = project(&snapshot, &subscription.urls);
                if next != subscription.last {
                    subscription.last = next.clone();
                    notifications.push((subscription.publish.clone(), next));
                }
            }
            notifications
        };
        let mut stopped = self.stop.subscribe();
        for (publish, next) in notifications {
            tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>return Err(DiscoveryStopped),_=publish(next)=>{}}
        }
        Ok(())
    }
}
fn project(snapshot: &Snapshot, urls: &[String]) -> Vec<LocalServer> {
    let mut visible = IndexMap::new();
    for raw in resource_ports::configured_urls(urls) {
        let url = Url::parse(&raw).unwrap();
        let key =
            resource_ports::server_key(url.host_str().unwrap(), resource_ports::url_port(&url));
        if visible.contains_key(&key) {
            continue;
        }
        if let Some(server) = snapshot
            .configured
            .get(&resource_ports::cache_key(&raw).unwrap())
        {
            visible.insert(
                key,
                LocalServer {
                    url: raw,
                    ..server.clone()
                },
            );
        }
    }
    for server in &snapshot.discovered {
        visible
            .entry(resource_ports::server_key(&server.host, server.port))
            .or_insert_with(|| server.clone());
    }
    let mut servers = visible.into_values().collect::<Vec<_>>();
    servers.sort_by_key(|server| server.port);
    servers
}
async fn fallback() -> Vec<LocalServer> {
    stream::iter(COMMON_DEV_PORTS.into_iter().map(|port| async move {
        async fn listening(port: u16, host: &str) -> bool {
            tokio::time::timeout(
                Duration::from_millis(250),
                tokio::net::TcpStream::connect((host, port)),
            )
            .await
            .is_ok_and(|stream| stream.is_ok())
        }
        async fn available(port: u16, host: &str) -> bool {
            match tokio::net::TcpListener::bind((host, port)).await {
                Ok(_) => true,
                Err(error) => error.kind() == std::io::ErrorKind::AddrNotAvailable,
            }
        }
        let (ipv4, ipv6) = tokio::join!(listening(port, "127.0.0.1"), listening(port, "::1"));
        let busy = if ipv4 || ipv6 {
            true
        } else {
            let (ipv4, ipv6) = tokio::join!(available(port, "127.0.0.1"), available(port, "::1"));
            !(ipv4 && ipv6)
        };
        busy.then(|| LocalServer {
            host: "localhost".into(),
            port,
            url: format!("http://localhost:{port}"),
            process_name: None,
            pid: None,
            terminal: None,
        })
    }))
    .buffered(16)
    .filter_map(|server| async move { server })
    .collect()
    .await
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    fn server(port: u16, pid: f64) -> LocalServer {
        LocalServer {
            host: "localhost".into(),
            port,
            url: format!("http://localhost:{port}"),
            pid: ProcessId::new(pid),
            process_name: Some("fixture".into()),
            terminal: None,
        }
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn native_table_terminal_registry_and_host_listener_discovery_share_owned_lifetimes() {
        use crate::{
            native_telemetry::{NativeTelemetryClient, NativeTelemetryOptions},
            terminal_manager::{TerminalManager, TerminalManagerOptions},
        };
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("terminal.pid");
        let port_file = directory.path().join("terminal.port");
        let child_pid_file = directory.path().join("http.pid");
        let child_script = directory.path().join("http-child.py");
        std::fs::write(&child_script,format!("import http.server,os\nserver=http.server.HTTPServer(('127.0.0.1',0),http.server.SimpleHTTPRequestHandler)\nopen({},'w').write(str(os.getpid()))\nopen({},'w').write(str(server.server_port))\nprint('READY',flush=True)\nserver.serve_forever()\n",serde_json::to_string(&child_pid_file).unwrap(),serde_json::to_string(&port_file).unwrap())).unwrap();
        let shell = directory.path().join("terminal-http.sh");
        // The shell waits/reaps its actual HTTP subprocess on group termination.
        std::fs::write(&shell,format!("#!/bin/sh\necho $$ > {}\npython3 -u {} &\nchild=$!\ntrap 'kill \"$child\" 2>/dev/null; wait \"$child\"; exit' TERM\nwait \"$child\"\n",serde_json::to_string(&pid_file).unwrap(),serde_json::to_string(&child_script).unwrap())).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
        let monitor = directory.path().join("resource-monitor.py");
        let fixture = include_str!("../tests/fixtures/resource-monitor.py")
            .replace("__MODE__", "normal")
            .replace("__LOG__", "")
            .replace(
                "processes=[dict(pid=os.getpid(),ppid=os.getppid(),name=\"fixture\")]",
                &format!(
                    "processes=[dict(pid=int(open({}).read()),ppid=1,name='sh'),dict(pid=int(open({}).read()),ppid=int(open({}).read()),name='python3')]",
                    serde_json::to_string(&pid_file).unwrap(),serde_json::to_string(&child_pid_file).unwrap(),serde_json::to_string(&pid_file).unwrap()
                ),
            );
        std::fs::write(&monitor, fixture).unwrap();
        std::fs::set_permissions(&monitor, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut owner_options = NativeTelemetryOptions::host(directory.path().to_owned(), None);
        owner_options.binary.overrides = vec![monitor];
        owner_options.binary.directories.clear();
        let telemetry = NativeTelemetryClient::new(owner_options);
        let registry = TerminalRegistry::default();
        let mut options = TerminalManagerOptions::host(
            directory.path().join("logs"),
            &t3_contracts::ServerSettings::default(),
        );
        options.shell = Some(shell.to_string_lossy().into_owned());
        options.kill_grace = Duration::from_secs(1);
        configure_terminal_tracking(&mut options, telemetry.clone(), registry.clone());
        let registered = Arc::new(tokio::sync::Notify::new());
        let register = options.register_terminal_processes.take().unwrap();
        options.register_terminal_processes = Some(Arc::new({
            let registered = registered.clone();
            move |thread, terminal, pids| {
                let registered = registered.clone();
                let register = register.clone();
                async move {
                    let has_child = pids.len() > 1;
                    register(thread, terminal, pids).await;
                    if has_child {
                        registered.notify_one();
                    }
                }
                .boxed()
            }
        }));
        let manager = TerminalManager::new(options).await.unwrap();
        let snapshot = manager.open(serde_json::from_value(serde_json::json!({"threadId":"discovery-thread","terminalId":"terminal","cwd":directory.path()})).unwrap()).await.unwrap();
        let pid = snapshot.pid.unwrap().0;
        let mut output = manager
            .observe(
                serde_json::from_value(
                    serde_json::json!({"threadId":"discovery-thread","terminalId":"terminal"}),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let mut ready = String::new();
        while !ready.contains("READY") {
            let event = tokio::time::timeout(Duration::from_secs(5), output.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            ready.push_str(
                event["snapshot"]["history"]
                    .as_str()
                    .or_else(|| event["data"].as_str())
                    .unwrap_or_default(),
            );
        }

        tokio::time::timeout(Duration::from_secs(5), registered.notified())
            .await
            .unwrap();
        assert_eq!(
            registry
                .snapshot()
                .get(&ProcessId::new(pid as f64).unwrap())
                .unwrap()
                .thread_id
                .as_str(),
            "discovery-thread"
        );
        let native_table = telemetry.process_table().await.unwrap();
        assert_eq!(native_table.len(), 2);
        assert_eq!(native_table[0].pid.0, pid);
        let discovery = PortDiscovery::new(
            PortDiscoveryOptions::host(
                if cfg!(target_os = "macos") {
                    "darwin"
                } else {
                    "linux"
                },
                registry.clone(),
            )
            .unwrap(),
        );
        let port: u16 = std::fs::read_to_string(&port_file)
            .unwrap()
            .parse()
            .unwrap();
        let servers = discovery
            .scan(&[format!("http://127.0.0.1:{port}/")])
            .await
            .unwrap();
        let found = servers
            .iter()
            .find(|server| server.port == port)
            .expect("owned HTTP terminal missing from native listener query");
        let http_pid: u64 = std::fs::read_to_string(&child_pid_file)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(found.pid.unwrap().number(), http_pid as f64);
        assert_eq!(found.terminal.as_ref().unwrap().terminal_id, "terminal");
        manager
            .close(
                serde_json::from_value(
                    serde_json::json!({"threadId":"discovery-thread","terminalId":"terminal"}),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(registry.snapshot().is_empty());
        manager.shutdown().await;
        discovery.shutdown().await;
        let child = telemetry.health().hello.unwrap().sidecar_pid.0;
        telemetry.shutdown().await;
        for pid in [pid, child] {
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
    #[tokio::test]
    async fn cache_positive_negative_pid_identity_and_batch_memo_follow_source() {
        let servers = Arc::new(Mutex::new(vec![server(5234, 10.0), server(6234, 11.0)]));
        let calls = Arc::new(AtomicUsize::new(0));
        let now = Arc::new(AtomicI64::new(0));
        let options = PortDiscoveryOptions::custom(
            TerminalRegistry::default(),
            Arc::new({
                let servers = servers.clone();
                move || {
                    let servers = servers.lock().unwrap().clone();
                    async move { Ok(servers) }.boxed()
                }
            }),
            Arc::new({
                let calls = calls.clone();
                move |url| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async move { url.contains(":5234") }.boxed()
                }
            }),
            Arc::new({
                let now = now.clone();
                move || now.load(Ordering::SeqCst)
            }),
        );
        let discovery = PortDiscovery::new(options);
        let urls = vec![
            "http://localhost:5234/#one".into(),
            "http://localhost:5234/#two".into(),
        ];
        let first = discovery.scan(&urls).await.unwrap();
        assert_eq!(first.len(), 1);
        assert!(first[0].url.ends_with("#one"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        now.store(14999, Ordering::SeqCst);
        discovery.scan(&urls).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        servers.lock().unwrap()[0].pid = ProcessId::new(12.0);
        assert_eq!(
            discovery.scan(&urls).await.unwrap()[0].pid,
            ProcessId::new(12.0)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        now.store(15000, Ordering::SeqCst);
        discovery.scan(&urls).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            6,
            "expired negative http and https reprobe; current PID positive remains cached"
        );
        discovery.shutdown().await;
    }
    #[tokio::test(start_paused = true)]
    async fn retain_cancellation_and_held_callback_release_shutdown_without_leaking_polling() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let queries = Arc::new(AtomicUsize::new(0));
        let options = PortDiscoveryOptions::custom(
            TerminalRegistry::default(),
            Arc::new({
                let queries = queries.clone();
                move || {
                    queries.fetch_add(1, Ordering::SeqCst);
                    async { Ok(vec![server(5234, 12.0)]) }.boxed()
                }
            }),
            Arc::new(|_| async { true }.boxed()),
            Arc::new(|| 0),
        );
        let discovery = PortDiscovery::new(options);
        let subscription = discovery
            .subscribe_callback(
                &[],
                vec![],
                Arc::new({
                    let entered = entered.clone();
                    move |_| {
                        let entered = entered.clone();
                        async move {
                            entered.notify_one();
                            std::future::pending::<()>().await;
                        }
                        .boxed()
                    }
                }),
            )
            .unwrap();
        let acquire = tokio::spawn({
            let discovery = discovery.clone();
            async move { discovery.retain().await }
        });
        entered.notified().await;
        assert_eq!(discovery.0.service.state.lock().unwrap().retains, 1);
        acquire.abort();
        assert!(matches!(acquire.await,Err(error) if error.is_cancelled()));
        assert_eq!(discovery.0.service.state.lock().unwrap().retains, 0);
        let before = queries.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(9)).await;
        assert_eq!(
            queries.load(Ordering::SeqCst),
            before,
            "idle scanner performs no native queries"
        );
        let poll = tokio::spawn({
            let service = discovery.0.service.clone();
            async move {
                service
                    .state
                    .lock()
                    .unwrap()
                    .subscriptions
                    .values_mut()
                    .next()
                    .unwrap()
                    .last
                    .clear();
                service.poll().await
            }
        });
        entered.notified().await;
        discovery.shutdown().await;
        assert!(poll.await.unwrap().is_err());
        drop(subscription);
        assert!(
            discovery
                .0
                .service
                .state
                .lock()
                .unwrap()
                .subscriptions
                .is_empty()
        );
    }
    #[tokio::test]
    async fn probe_concurrency_releases_completed_slots_without_waiting_for_first_group() {
        let entered = Arc::new(AtomicUsize::new(0));
        let enough = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Notify::new());
        let options = PortDiscoveryOptions::custom(
            TerminalRegistry::default(),
            Arc::new(|| async { Ok(vec![]) }.boxed()),
            Arc::new({
                let entered = entered.clone();
                let enough = enough.clone();
                let gate = gate.clone();
                move |url| {
                    let entered = entered.clone();
                    let enough = enough.clone();
                    let gate = gate.clone();
                    async move {
                        if entered.fetch_add(1, Ordering::SeqCst) + 1 == 20 {
                            enough.notify_one();
                        }
                        if url.contains("/first") {
                            gate.notified().await;
                        }
                        true
                    }
                    .boxed()
                }
            }),
            Arc::new(|| 0),
        );
        let discovery = PortDiscovery::new(options);
        let urls = (0..20)
            .map(|index| {
                format!(
                    "http://localhost:5234/{}",
                    if index == 0 {
                        "first".into()
                    } else {
                        index.to_string()
                    }
                )
            })
            .collect::<Vec<_>>();
        let scan = tokio::spawn({
            let discovery = discovery.clone();
            async move { discovery.scan(&urls).await }
        });
        tokio::time::timeout(Duration::from_secs(3), enough.notified())
            .await
            .unwrap();
        gate.notify_one();
        let servers = scan.await.unwrap().unwrap();
        assert_eq!(servers.len(), 1);
        assert!(
            servers[0].url.ends_with("/first"),
            "source input order determines configured preference"
        );
        discovery.shutdown().await;
    }
    #[tokio::test]
    async fn real_http_classification_manual_redirect_and_credential_rejection_match_fetch() {
        use axum::{
            Router,
            http::{HeaderMap, HeaderValue, StatusCode},
            routing::get,
        };
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let application = Router::new().route(
            "/{kind}",
            get(
                move |axum::extract::Path(kind): axum::extract::Path<String>| {
                    let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, Ordering::SeqCst);
                        let mut headers = HeaderMap::new();
                        let status = match kind.as_str() {
                            "html" => {
                                headers.insert(
                                    "content-type",
                                    HeaderValue::from_static("Text/HTML; charset=UTF-8"),
                                );
                                StatusCode::OK
                            }
                            "xhtml" => {
                                headers.insert(
                                    "content-type",
                                    HeaderValue::from_static("application/xhtml+xml"),
                                );
                                StatusCode::OK
                            }
                            "redirect" => {
                                headers.insert(
                                    "location",
                                    HeaderValue::from_static(
                                        "https://external.invalid/never-follow",
                                    ),
                                );
                                StatusCode::FOUND
                            }
                            "empty" => {
                                headers
                                    .insert("content-type", HeaderValue::from_static("text/html"));
                                StatusCode::NO_CONTENT
                            }
                            "reset" => {
                                headers
                                    .insert("content-type", HeaderValue::from_static("text/html"));
                                StatusCode::RESET_CONTENT
                            }
                            "json" => {
                                headers.insert(
                                    "content-type",
                                    HeaderValue::from_static("application/json"),
                                );
                                StatusCode::OK
                            }
                            _ => StatusCode::NOT_FOUND,
                        };
                        (status, headers, "fixture")
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let host = tokio::spawn(async move {
            axum::serve(listener, application)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let mut options =
            PortDiscoveryOptions::host("darwin", TerminalRegistry::default()).unwrap();
        options.listeners = Arc::new(|| async { Ok(vec![]) }.boxed());
        let probe = options.probe.clone();
        for (path, expected) in [
            ("html", true),
            ("xhtml", true),
            ("redirect", true),
            ("empty", false),
            ("reset", false),
            ("json", false),
            ("missing", false),
        ] {
            assert_eq!(
                probe(format!("http://localhost:{port}/{path}")).await,
                expected,
                "{path}"
            );
        }
        let count = requests.load(Ordering::SeqCst);
        assert!(!probe(format!("http://user:pass@localhost:{port}/html")).await);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            count,
            "fetch rejects credentials before network"
        );
        let discovery = PortDiscovery::new(options);
        let urls = vec![format!("http://localhost:{port}/html")];
        assert_eq!(discovery.scan(&urls).await.unwrap()[0].url, urls[0]);
        discovery.shutdown().await;
        stop.send(()).unwrap();
        host.await.unwrap();
    }
}
