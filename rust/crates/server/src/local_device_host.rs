//! Owned local device hub. Tool consent is enforced by DeviceService above it.
use crate::{
    device_platform::{android_sdk, host_environment, platform_availability},
    device_toolchain::{DEVICE_HUB_VERSION, DeviceToolPaths, DeviceToolchain},
    terminal_environment::Environment,
};
use futures_util::future::BoxFuture;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use t3_contracts::*;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::{oneshot, watch},
    task::JoinHandle,
};
pub type DevicePhase =
    Arc<dyn Fn(DeviceHostStatus, Option<String>) -> BoxFuture<'static, ()> + Send + Sync>;
#[derive(Debug, Clone)]
pub struct DeviceHostReady {
    pub generation: u64,
    commands: crate::device_commands::DeviceCommands,
    pub node_path: PathBuf,
    pub origin: String,
    pub pid: u32,
    pub environment: Environment,
    pub serve_sim_ax_settings: Option<PathBuf>,
    pub serve_sim_cli: Option<PathBuf>,
}
impl DeviceHostReady {
    pub async fn run_command(
        &self,
        command: &str,
        args: Vec<String>,
        timeout: Duration,
    ) -> crate::device_commands::HostCommandOutput {
        self.run_command_with_stdin(command, args, timeout, None)
            .await
    }
    pub async fn run_command_with_stdin(
        &self,
        command: &str,
        args: Vec<String>,
        timeout: Duration,
        stdin: Option<Vec<u8>>,
    ) -> crate::device_commands::HostCommandOutput {
        let command = if command == "emulator" {
            self.environment.get("ANDROID_HOME").map(|root| {
                PathBuf::from(root).join("emulator").join(if cfg!(windows) {
                    "emulator.exe"
                } else {
                    "emulator"
                })
            })
        } else {
            None
        }
        .or_else(|| crate::acp_registry_spawn::resolve_executable(command, &self.environment))
        .unwrap_or_else(|| command.into());
        self.commands
            .run_with_stdin(command, args, self.environment.clone(), timeout, stdin)
            .await
    }
}
#[derive(Clone)]
pub struct LocalDeviceHostOptions {
    pub state_dir: PathBuf,
    pub environment: Environment,
    pub platform: String,
    pub node_override: Option<PathBuf>,
    pub ready_timeout: Duration,
    #[cfg(test)]
    pub ready_published: Option<watch::Sender<Option<DeviceHostReady>>>,
}
impl LocalDeviceHostOptions {
    pub fn host(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            environment: std::env::vars().collect(),
            platform: match std::env::consts::OS {
                "macos" => "darwin",
                "windows" => "win32",
                other => other,
            }
            .into(),
            node_override: None,
            ready_timeout: Duration::from_secs(30),
            #[cfg(test)]
            ready_published: None,
        }
    }
}
struct State {
    generation: u64,
    ready: Option<DeviceHostReady>,
}
struct Job {
    stop: watch::Sender<bool>,
    handle: JoinHandle<()>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
struct Inner {
    options: LocalDeviceHostOptions,
    toolchain: DeviceToolchain,
    state: Arc<Mutex<State>>,
    start: Arc<tokio::sync::Mutex<()>>,
    jobs: Mutex<Vec<Job>>,
    cleanup: tokio::sync::Mutex<Vec<Job>>,
    closed: AtomicBool,
    stopping: AtomicBool,
}
impl Drop for Inner {
    fn drop(&mut self) {
        for job in self.jobs.get_mut().unwrap().drain(..) {
            job.stop.send_replace(true);
        }
    }
}
#[derive(Clone)]
pub struct LocalDeviceHost(Arc<Inner>);
struct Cancel(Option<watch::Sender<bool>>);
impl Drop for Cancel {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            stop.send_replace(true);
        }
    }
}
fn failure(
    reason: impl Into<String>,
    cause: Option<serde_json::Value>,
) -> DeviceHostUnavailableError {
    DeviceHostUnavailableError {
        tag: DeviceHostUnavailableErrorTag::DeviceHostUnavailableError,
        host_id: DeviceHostId::new("local").unwrap(),
        reason: reason.into(),
        cause: cause.map(Some),
    }
}
impl LocalDeviceHost {
    pub fn new(options: LocalDeviceHostOptions, toolchain: DeviceToolchain) -> Self {
        Self(Arc::new(Inner {
            options,
            toolchain,
            state: Arc::new(Mutex::new(State {
                generation: 0,
                ready: None,
            })),
            start: Arc::new(tokio::sync::Mutex::new(())),
            jobs: Mutex::new(Vec::new()),
            cleanup: tokio::sync::Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
        }))
    }
    pub fn generation(&self) -> u64 {
        self.0.state.lock().unwrap().generation
    }
    pub fn toolchain(&self) -> DeviceToolchain {
        self.0.toolchain.clone()
    }
    pub fn current(&self) -> Option<DeviceHostReady> {
        self.0.state.lock().unwrap().ready.clone()
    }
    pub async fn platform(&self, platform: DevicePlatform) -> DevicePlatformAvailability {
        platform_availability(
            platform,
            &self.0.options.environment,
            &self.0.options.platform,
        )
        .await
    }
    pub async fn summary(&self) -> DeviceHostSummary {
        let current = self.current();
        let tools = self
            .0
            .toolchain
            .versions(current.as_ref().map(|_| DEVICE_HUB_VERSION.into()), None)
            .await;
        DeviceHostSummary {
            id: DeviceHostId::new("local").unwrap(),
            kind: DeviceHostKind::Local,
            label: TrimmedNonEmptyString::new("This machine").unwrap(),
            platforms: vec![
                self.platform(DevicePlatform::Ios).await,
                self.platform(DevicePlatform::Android).await,
            ],
            tools: tools.map(Some),
            tool_inspection_error: None,
            hub_installed: self.0.toolchain.installed(DeviceToolKind::Hub).await,
            agent_device_installed: self.0.toolchain.installed(DeviceToolKind::Agent).await,
        }
    }
    pub async fn ensure_ready(
        &self,
        on_phase: DevicePhase,
    ) -> Result<DeviceHostReady, DeviceHostUnavailableError> {
        if self.0.closed.load(Ordering::Acquire) || self.0.stopping.load(Ordering::Acquire) {
            return Err(failure("Device host is shutting down.", None));
        }
        if let Some(ready) = self.current() {
            return Ok(ready);
        }
        let admission = self.0.start.clone().lock_owned().await;
        if let Some(ready) = self.current() {
            return Ok(ready);
        }
        let (stop, stopped) = watch::channel(false);
        let mut cancel = Cancel(Some(stop.clone()));
        let (reply, receive) = oneshot::channel();
        {
            let mut jobs = self.0.jobs.lock().unwrap();
            if self.0.closed.load(Ordering::Acquire) || self.0.stopping.load(Ordering::Acquire) {
                return Err(failure("Device host is shutting down.", None));
            }
            jobs.retain(|job| !job.handle.is_finished());
            for job in jobs.iter() {
                job.stop.send_replace(true);
            }
            let generation = {
                let mut state = self.0.state.lock().unwrap();
                state.generation += 1;
                state.generation
            };
            let options = self.0.options.clone();
            let toolchain = self.0.toolchain.clone();
            let state = self.0.state.clone();
            let start = self.0.start.clone();
            jobs.push(Job {
                stop,
                handle: tokio::spawn(async move {
                    supervise(
                        options, toolchain, state, start, generation, admission, stopped, on_phase,
                        reply,
                    )
                    .await;
                }),
            });
        }
        let result = receive.await.map_err(|error| {
            failure(
                "Device host startup stopped.",
                Some(json!(error.to_string())),
            )
        })?;
        if result.is_ok() {
            cancel.0 = None;
        }
        result
    }
    pub async fn stop(&self) {
        let mut cleanup = self.0.cleanup.lock().await;
        {
            let mut jobs = self.0.jobs.lock().unwrap();
            self.0.stopping.store(true, Ordering::Release);
            cleanup.extend(jobs.drain(..));
        }
        for job in cleanup.iter() {
            job.stop.send_replace(true);
        }
        while let Some(job) = cleanup.last_mut() {
            let _ = (&mut job.handle).await;
            cleanup.pop();
        }
        {
            let mut state = self.0.state.lock().unwrap();
            state.generation += 1;
            state.ready = None;
        }
        let _ = tokio::fs::remove_file(
            self.0
                .options
                .state_dir
                .join("device/agent-device/hub.json"),
        )
        .await;
        self.0.stopping.store(false, Ordering::Release);
    }
    pub async fn shutdown(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.stop().await;
    }
}
async fn drain(mut read: impl AsyncRead + Unpin) {
    let mut chunk = [0; 8192];
    loop {
        match read.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}
async fn spawn_hub(
    options: &LocalDeviceHostOptions,
    tool: &DeviceToolPaths,
    node: &Path,
    environment: &Environment,
) -> Result<(tokio::process::Child, JoinHandle<()>, DeviceHostReady), DeviceHostUnavailableError> {
    let port = {
        let socket = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| {
                failure(
                    "Device support failed during reserving the device hub port.",
                    Some(json!(error.to_string())),
                )
            })?;
        socket.local_addr().unwrap().port()
    };
    let mut env = environment.clone();
    env.insert("FORCE_COLOR".into(), "0".into());
    env.insert("NO_COLOR".into(), "1".into());
    #[cfg(unix)]
    if options.platform == "linux" && !env.contains_key("XDG_RUNTIME_DIR") {
        use std::os::unix::fs::MetadataExt;
        let uid = unsafe { libc::geteuid() };
        let runtime = format!("/run/user/{uid}");
        if let Ok(stat) = tokio::fs::metadata(&runtime).await {
            if stat.is_dir() && stat.uid() == uid {
                env.insert("XDG_RUNTIME_DIR".into(), runtime);
            }
        }
    }
    let args = vec![
        tool.entry_path.to_string_lossy().into_owned(),
        "--port".into(),
        port.to_string(),
        "--host".into(),
        "127.0.0.1".into(),
        "--hide-sidebar".into(),
        "--hide-boot-device".into(),
    ];
    let mut command =
        crate::acp_registry_spawn::command(&node.to_string_lossy(), &args, Some(&env));
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            failure(
                "Device support failed during starting the device hub.",
                Some(json!(error.to_string())),
            )
        })?;
    let pid = child.id().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let output = tokio::spawn(async move {
        tokio::join!(drain(stdout), drain(stderr));
    });
    let dist = tool
        .install_dir
        .join("node_modules/expo-device-hub/vendor/serve-sim/dist");
    let ax = dist.join("simax/serve-sim-ax-settings");
    let cli = dist.join("serve-sim.js");
    let ready = DeviceHostReady {
        generation: 0,
        commands: crate::device_commands::DeviceCommands::new(),
        node_path: node.into(),
        origin: format!("http://127.0.0.1:{port}"),
        pid,
        environment: environment.clone(),
        serve_sim_ax_settings: tokio::fs::try_exists(&ax)
            .await
            .unwrap_or(false)
            .then_some(ax),
        serve_sim_cli: tokio::fs::try_exists(&cli)
            .await
            .unwrap_or(false)
            .then_some(cli),
    };
    Ok((child, output, ready))
}
async fn close(child: &mut tokio::process::Child, output: JoinHandle<()>) {
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = output.await;
}
async fn await_ready(
    ready: &DeviceHostReady,
    timeout: Duration,
    stopped: &mut watch::Receiver<bool>,
) -> Result<(), DeviceHostUnavailableError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let check = async {
        loop {
            if client
                .get(format!("{}/readyz", ready.origin))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>Err(failure("Device hub startup was cancelled.",None)),result=tokio::time::timeout(timeout,check)=>result.map_err(|_|failure("Device support failed during waiting for the device hub to answer.",None))}
}
async fn phase(
    callback: &DevicePhase,
    status: DeviceHostStatus,
    stopped: &mut watch::Receiver<bool>,
) -> Result<(), DeviceHostUnavailableError> {
    tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>Err(failure("Device hub startup was cancelled.",None)),_=callback(status,None)=>Ok(())}
}
async fn supervise(
    options: LocalDeviceHostOptions,
    toolchain: DeviceToolchain,
    state: Arc<Mutex<State>>,
    start: Arc<tokio::sync::Mutex<()>>,
    generation: u64,
    admission: tokio::sync::OwnedMutexGuard<()>,
    mut stopped: watch::Receiver<bool>,
    on_phase: DevicePhase,
    reply: oneshot::Sender<Result<DeviceHostReady, DeviceHostUnavailableError>>,
) {
    let mut admission = Some(admission);
    let mut reply = Some(reply);
    let startup=async{
  let node=options.node_override.clone().or_else(||crate::acp_registry_spawn::resolve_executable(if options.platform=="win32"{"node.exe"}else{"node"},&options.environment)).ok_or_else(||failure("Local device support requires Node.js. Install Node.js and make sure node is on PATH, then retry.",None))?;
  if !toolchain.installed(DeviceToolKind::Hub).await{phase(&on_phase,DeviceHostStatus::Installing,&mut stopped).await?;}
  let tool=toolchain.ensure_with_stop(DeviceToolKind::Hub,stopped.clone()).await.map_err(|error|failure("Device support failed during installing device support.",serde_json::to_value(error).ok()))?;
  let sdk=android_sdk(&options.environment,&options.platform).await;let environment=host_environment(options.environment.clone(),sdk.root.as_deref(),&options.platform);
  phase(&on_phase,DeviceHostStatus::Starting,&mut stopped).await?;Ok::<_,DeviceHostUnavailableError>((node,tool,environment))
 }.await;
    let (node, tool, environment) = match startup {
        Ok(startup) => startup,
        Err(error) => {
            if let Some(reply) = reply.take() {
                let _ = reply.send(Err(error));
            }
            return;
        }
    };
    let mut backoff = 0u64;
    loop {
        if *stopped.borrow() || state.lock().unwrap().generation != generation {
            return;
        }
        let spawned = spawn_hub(&options, &tool, &node, &environment).await;
        let (mut child, output, mut ready) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(Err(error));
                }
                return;
            }
        };
        ready.generation = generation;
        let started = tokio::time::Instant::now();
        if let Err(error) = await_ready(&ready, options.ready_timeout, &mut stopped).await {
            close(&mut child, output).await;
            if let Some(reply) = reply.take() {
                let _ = reply.send(Err(error));
            }
            return;
        }
        if *stopped.borrow() || state.lock().unwrap().generation != generation {
            close(&mut child, output).await;
            return;
        }
        let record_path = options.state_dir.join("device/agent-device/hub.json");
        let contents=json!({"pid":ready.pid,"port":url::Url::parse(&ready.origin).unwrap().port().unwrap(),"entryPath":tool.entry_path}).to_string();
        let _ = tokio::task::spawn_blocking(move || {
            crate::server_secret_store::write_string_atomically(&record_path, &contents)
        })
        .await;
        if *stopped.borrow() || state.lock().unwrap().generation != generation {
            close(&mut child, output).await;
            return;
        }
        state.lock().unwrap().ready = Some(ready.clone());
        #[cfg(test)]
        if let Some(published) = &options.ready_published {
            published.send_replace(Some(ready.clone()));
        }
        let ready_commands = ready.commands.clone();
        if let Some(reply) = reply.take() {
            let _ = reply.send(Ok(ready));
        }
        drop(admission.take());
        let stopped_by_owner =
            tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>true,_=child.wait()=>false};
        close(&mut child, output).await;
        ready_commands.shutdown().await;
        {
            let mut state = state.lock().unwrap();
            if state.generation != generation {
                return;
            }
            state.ready = None;
        }
        if stopped_by_owner {
            return;
        }
        let delay = if started.elapsed() >= Duration::from_secs(60) {
            backoff = 0;
            0
        } else {
            let delay = backoff;
            backoff = if backoff == 0 {
                1000
            } else {
                (backoff * 2).min(30000)
            };
            delay
        };
        tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>return,_=tokio::time::sleep(Duration::from_millis(delay))=>{}}
        admission = Some(
            tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>return,admission=start.clone().lock_owned()=>admission},
        );
    }
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn noop() -> DevicePhase {
        Arc::new(|_, _| Box::pin(async {}))
    }
    async fn fixture(
        temp: &Path,
        held: bool,
    ) -> (
        LocalDeviceHost,
        DeviceToolchain,
        tokio::net::UnixDatagram,
        watch::Receiver<Option<DeviceHostReady>>,
    ) {
        let toolchain =
            DeviceToolchain::new(crate::device_toolchain::ToolchainOptions::new(temp.into()));
        let paths = toolchain.paths(DeviceToolKind::Hub);
        tokio::fs::create_dir_all(paths.entry_path.parent().unwrap())
            .await
            .unwrap();
        let socket_path = temp.join("hub-milestones.sock");
        let socket = tokio::net::UnixDatagram::bind(&socket_path).unwrap();
        let source = format!(
            r#"import os,sys,json,socket,http.server
socket_path={socket_path}
held={held}
args=sys.argv[1:]
port=int(args[args.index('--port')+1])
class Server(http.server.HTTPServer): allow_reuse_address=True
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  if self.path=='/readyz':
   s=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM)
   s.sendto(json.dumps({{'pid':os.getpid(),'args':args,'env':{{k:os.environ.get(k) for k in ['FORCE_COLOR','NO_COLOR','ANDROID_HOME','PATH']}}}}).encode(),socket_path)
   s.close()
   if held:
    import signal
    signal.pause()
  self.send_response(200);self.end_headers();self.wfile.write(b'ready');self.wfile.flush()
  if self.path=='/crash': os._exit(0)
server=Server(('127.0.0.1',port),Handler)
server.serve_forever()
"#,
            socket_path = serde_json::to_string(&socket_path.to_string_lossy()).unwrap(),
            held = if held { "True" } else { "False" }
        );
        tokio::fs::write(&paths.entry_path, source).await.unwrap();
        tokio::fs::write(paths.sentinel_path, format!("{DEVICE_HUB_VERSION}\n"))
            .await
            .unwrap();
        let mut options = LocalDeviceHostOptions::host(temp.join("state"));
        options.node_override = Some("/usr/bin/python3".into());
        options.environment = std::env::vars()
            .filter(|(key, _)| !key.starts_with("ANDROID_"))
            .collect();
        options.environment.insert(
            "HOME".into(),
            temp.join("home").to_string_lossy().into_owned(),
        );
        let (ready, published) = watch::channel(None);
        options.ready_published = Some(ready);
        let host = LocalDeviceHost::new(options, toolchain.clone());
        (host, toolchain, socket, published)
    }
    async fn milestone(socket: &tokio::net::UnixDatagram) -> serde_json::Value {
        let mut bytes = [0; 32768];
        let length = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        serde_json::from_slice(&bytes[..length]).unwrap()
    }
    fn reaped(pid: u32) {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    #[tokio::test]
    async fn cached_ready_hub_uses_exact_args_and_restart_replaces_owned_endpoint() {
        let temp = tempfile::tempdir().unwrap();
        let (host, tools, socket, mut published) = fixture(temp.path(), false).await;
        let (a, b) = tokio::join!(host.ensure_ready(noop()), host.ensure_ready(noop()));
        let first = a.unwrap();
        assert_eq!(first.pid, b.unwrap().pid);
        let witness = milestone(&socket).await; // Script argv excludes its entrypoint.
        assert_eq!(
            witness["args"],
            json!([
                "--port",
                url::Url::parse(&first.origin)
                    .unwrap()
                    .port()
                    .unwrap()
                    .to_string(),
                "--host",
                "127.0.0.1",
                "--hide-sidebar",
                "--hide-boot-device"
            ])
        );
        assert_eq!(witness["env"]["FORCE_COLOR"], "0");
        assert_eq!(witness["env"]["NO_COLOR"], "1");
        let record: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(temp.path().join("state/device/agent-device/hub.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(record["pid"], first.pid);
        assert_eq!(
            host.summary()
                .await
                .tools
                .unwrap()
                .unwrap()
                .hub
                .running_version
                .as_deref(),
            Some(DEVICE_HUB_VERSION)
        );
        published.borrow_and_update();
        let _ = reqwest::get(format!("{}/crash", first.origin)).await;
        tokio::time::timeout(
            Duration::from_secs(5),
            published.wait_for(|ready| ready.as_ref().is_some_and(|ready| ready.pid != first.pid)),
        )
        .await
        .unwrap()
        .unwrap();
        let replacement = host.current().unwrap();
        assert_ne!(replacement.pid, first.pid);
        reaped(first.pid);
        host.shutdown().await;
        reaped(replacement.pid);
        assert!(host.current().is_none());
        assert!(
            !temp
                .path()
                .join("state/device/agent-device/hub.json")
                .exists()
        );
        assert!(host.ensure_ready(noop()).await.is_err());
        tools.shutdown().await;
    }
    #[tokio::test]
    async fn cancelled_readiness_reaps_unpublished_hub_before_shutdown_returns() {
        let temp = tempfile::tempdir().unwrap();
        let (host, tools, socket, _) = fixture(temp.path(), true).await;
        let request = {
            let host = host.clone();
            tokio::spawn(async move { host.ensure_ready(noop()).await })
        };
        let witness = milestone(&socket).await;
        let pid = witness["pid"].as_u64().unwrap() as u32;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), host.shutdown())
            .await
            .unwrap();
        reaped(pid);
        assert!(host.current().is_none());
        assert!(
            !temp
                .path()
                .join("state/device/agent-device/hub.json")
                .exists()
        );
        tools.shutdown().await;
    }
    #[tokio::test]
    async fn held_phase_callback_is_cancelled_without_launching_any_hub() {
        let temp = tempfile::tempdir().unwrap();
        let (host, tools, _, _) = fixture(temp.path(), false).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let callback: DevicePhase = {
            let entered = entered.clone();
            Arc::new(move |_, _| {
                let entered = entered.clone();
                Box::pin(async move {
                    entered.notify_one();
                    std::future::pending::<()>().await
                })
            })
        };
        let request = {
            let host = host.clone();
            tokio::spawn(async move { host.ensure_ready(callback).await })
        };
        entered.notified().await;
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), host.shutdown())
            .await
            .unwrap();
        assert!(host.current().is_none());
        tools.shutdown().await;
    }
}
