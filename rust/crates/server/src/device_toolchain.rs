//! Pinned device tool acquisition. Inspection never downloads or starts tools.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use t3_contracts::{DeviceToolKind, DeviceToolVersion, DeviceToolVersions, trim_wire_string};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::{Semaphore, oneshot, watch},
    task::JoinHandle,
};
pub const DEVICE_HUB_VERSION: &str = "0.12.0";
pub const AGENT_DEVICE_VERSION: &str = "0.21.12";
const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
fn install_lock() -> Arc<Semaphore> {
    static LOCK: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(Semaphore::new(1))).clone()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInstallError {
    #[serde(rename = "_tag")]
    pub tag: String,
    pub tool: String,
    pub step: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Value>,
}
impl std::fmt::Display for ToolInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Installing {} failed while {}{}.",
            self.tool,
            self.step,
            self.exit_code
                .map(|code| format!(" (exit code {code})"))
                .unwrap_or_default()
        )
    }
}
impl std::error::Error for ToolInstallError {}
#[derive(Debug, Clone)]
pub struct DeviceToolPaths {
    pub install_dir: PathBuf,
    pub entry_path: PathBuf,
    pub sentinel_path: PathBuf,
}
fn spec(tool: DeviceToolKind) -> (&'static str, &'static str, &'static str) {
    match tool {
        DeviceToolKind::Hub => ("expo-device-hub", DEVICE_HUB_VERSION, "dist/server/cli.mjs"),
        DeviceToolKind::Agent => ("agent-device", AGENT_DEVICE_VERSION, "bin/agent-device.mjs"),
    }
}
fn paths(base: &Path, tool: DeviceToolKind, version: &str) -> DeviceToolPaths {
    let (name, _, entry) = spec(tool);
    let install_dir = base.join("tools").join(name).join(version);
    DeviceToolPaths {
        entry_path: install_dir.join("node_modules").join(name).join(entry),
        sentinel_path: install_dir.join(".install-complete"),
        install_dir,
    }
}
fn error(tool: DeviceToolKind, step: &str, cause: impl std::fmt::Display) -> ToolInstallError {
    ToolInstallError {
        tag: "DeviceToolchainInstallError".into(),
        tool: spec(tool).0.into(),
        step: step.into(),
        exit_code: None,
        cause: Some(json!(cause.to_string())),
    }
}
#[derive(Clone)]
pub struct ToolchainOptions {
    pub base_dir: PathBuf,
    pub npm: PathBuf,
    pub pnpm: PathBuf,
    pub timeout: Duration,
}
impl ToolchainOptions {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            npm: "npm".into(),
            pnpm: "pnpm".into(),
            timeout: Duration::from_secs(600),
        }
    }
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
    options: ToolchainOptions,
    jobs: Mutex<Option<Vec<Job>>>,
    shutdown: tokio::sync::Mutex<Vec<Job>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(jobs) = self.jobs.get_mut().unwrap().take() {
            for job in jobs {
                job.stop.send_replace(true);
            }
        }
    }
}
#[derive(Clone)]
pub struct DeviceToolchain(Arc<Inner>);
struct Cancel(watch::Sender<bool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
impl DeviceToolchain {
    pub fn new(options: ToolchainOptions) -> Self {
        Self(Arc::new(Inner {
            options,
            jobs: Mutex::new(Some(Vec::new())),
            shutdown: tokio::sync::Mutex::new(Vec::new()),
        }))
    }
    pub fn paths(&self, tool: DeviceToolKind) -> DeviceToolPaths {
        paths(&self.0.options.base_dir, tool, spec(tool).1)
    }
    pub async fn installed(&self, tool: DeviceToolKind) -> bool {
        installed(&self.paths(tool), spec(tool).1).await
    }
    pub async fn versions(
        &self,
        hub_running: Option<String>,
        agent_running: Option<String>,
    ) -> Option<DeviceToolVersions> {
        async fn inspect(
            base: &Path,
            tool: DeviceToolKind,
            running: Option<String>,
        ) -> std::io::Result<DeviceToolVersion> {
            let mut found = Vec::new();
            let directory = base.join("tools").join(spec(tool).0);
            match tokio::fs::read_dir(directory).await {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                Ok(mut entries) => {
                    while let Some(entry) = entries.next_entry().await? {
                        let version = entry.file_name().to_string_lossy().into_owned();
                        if !valid_version(&version) {
                            continue;
                        }
                        let path = paths(base, tool, &version);
                        let sentinel = match tokio::fs::read_to_string(&path.sentinel_path).await {
                            Ok(s) => Some(s),
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                            Err(e) => return Err(e),
                        };
                        if sentinel.as_deref().map(trim_wire_string) == Some(version.as_str())
                            && tokio::fs::try_exists(path.entry_path).await?
                        {
                            found.push(version);
                        }
                    }
                }
            }
            found.sort();
            Ok(DeviceToolVersion {
                required_version: spec(tool).1.into(),
                installed_versions: found,
                running_version: running,
            })
        }
        Some(DeviceToolVersions {
            hub: inspect(&self.0.options.base_dir, DeviceToolKind::Hub, hub_running)
                .await
                .ok()?,
            agent: inspect(
                &self.0.options.base_dir,
                DeviceToolKind::Agent,
                agent_running,
            )
            .await
            .ok()?,
        })
    }
    pub async fn ensure(&self, tool: DeviceToolKind) -> Result<DeviceToolPaths, ToolInstallError> {
        self.ensure_owned(tool, None).await
    }
    pub(crate) async fn ensure_with_stop(
        &self,
        tool: DeviceToolKind,
        stopped: watch::Receiver<bool>,
    ) -> Result<DeviceToolPaths, ToolInstallError> {
        self.ensure_owned(tool, Some(stopped)).await
    }
    async fn ensure_owned(
        &self,
        tool: DeviceToolKind,
        mut stopped: Option<watch::Receiver<bool>>,
    ) -> Result<DeviceToolPaths, ToolInstallError> {
        let (stop, installer_stopped) = watch::channel(false);
        let guard = Cancel(stop.clone());
        let (reply, mut receive) = oneshot::channel();
        {
            let mut jobs = self.0.jobs.lock().unwrap();
            let jobs = jobs.as_mut().ok_or_else(|| {
                error(tool, "running npm install", "device toolchain is shut down")
            })?;
            jobs.retain(|job| !job.handle.is_finished());
            let options = self.0.options.clone();
            jobs.push(Job {
                stop,
                handle: tokio::spawn(async move {
                    let outcome = install(options, tool, installer_stopped).await;
                    let _ = reply.send(outcome);
                }),
            });
        }
        let result = tokio::select! {
            biased;
            _ = async { match stopped.as_mut() { Some(stopped) => { let _ = stopped.wait_for(|value| *value).await; }, None => std::future::pending().await } } => {
                guard.0.send_replace(true);
                let _ = (&mut receive).await;
                return Err(error(tool, "running npm install", "installation cancelled"));
            },
            result = &mut receive => result.map_err(|cause|error(tool,"running npm install",cause))?,
        };
        drop(guard);
        result
    }
    pub async fn shutdown(&self) {
        let mut jobs = self.0.shutdown.lock().await;
        jobs.extend(self.0.jobs.lock().unwrap().take().unwrap_or_default());
        for job in jobs.iter() {
            job.stop.send_replace(true);
        }
        while let Some(job) = jobs.last_mut() {
            let _ = (&mut job.handle).await;
            jobs.pop();
        }
    }
}
fn valid_version(version: &str) -> bool {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX
        .get_or_init(|| regex::Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.-]+)?$").unwrap())
        .is_match(version)
}
async fn installed(paths: &DeviceToolPaths, version: &str) -> bool {
    tokio::fs::try_exists(&paths.entry_path)
        .await
        .unwrap_or(false)
        && tokio::fs::read_to_string(&paths.sentinel_path)
            .await
            .ok()
            .as_deref()
            .map(trim_wire_string)
            == Some(version)
}
async fn remove(path: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
async fn install(
    options: ToolchainOptions,
    tool: DeviceToolKind,
    mut stopped: watch::Receiver<bool>,
) -> Result<DeviceToolPaths, ToolInstallError> {
    let lock = install_lock();
    let _admission = tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>return Err(error(tool,"running npm install","installation cancelled")),permit=lock.acquire_owned()=>permit.unwrap()};
    let (name, version, entry) = spec(tool);
    let destination = paths(&options.base_dir, tool, version);
    if installed(&destination, version).await {
        return Ok(destination);
    }
    remove(&destination.install_dir)
        .await
        .map_err(|cause| error(tool, "removing an incomplete install", cause))?;
    let parent = destination.install_dir.parent().unwrap();
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|cause| error(tool, "preparing the install directory", cause))?;
    let staging = parent.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir(&staging)
        .await
        .map_err(|cause| error(tool, "preparing the install directory", cause))?;
    let outcome=async{
  if *stopped.borrow(){return Err(error(tool,"running npm install","installation cancelled"));}
  let args=vec!["install".into(),"--prefix".into(),staging.to_string_lossy().into_owned(),"--no-fund".into(),"--no-audit".into(),format!("{name}@{version}")];
  let output=match run(&options.npm,&args,options.timeout,&mut stopped).await{
   Err(RunError::Spawn(cause)) if cause.kind()==std::io::ErrorKind::NotFound=>{
     let mut fallback=vec!["--package=npm@11".into(),"dlx".into(),"npm".into()];fallback.extend(args);
     run(&options.pnpm,&fallback,options.timeout,&mut stopped).await
   },result=>result
  }.map_err(|cause|error(tool,"running npm install",cause))?;
  if output.code!=Some(0){return Err(ToolInstallError{tag:"DeviceToolchainInstallError".into(),tool:name.into(),step:"running npm install".into(),exit_code:output.code,cause:Some(json!({"code":output.code,"stdout":output.stdout,"stderr":output.stderr,"timedOut":false,"stdoutTruncated":false,"stderrTruncated":false,"stdoutInvalidUtf8":output.stdout_invalid_utf8,"stderrInvalidUtf8":output.stderr_invalid_utf8}))});}
  if !tokio::fs::try_exists(staging.join("node_modules").join(name).join(entry)).await.unwrap_or(false){return Err(ToolInstallError{tag:"DeviceToolchainInstallError".into(),tool:name.into(),step:"verifying the installed entry point".into(),exit_code:None,cause:None});}
  if *stopped.borrow(){return Err(error(tool,"running npm install","installation cancelled"));}
  tokio::fs::write(staging.join(".install-complete"),format!("{version}\n")).await.map_err(|cause|error(tool,"recording the completed install",cause))?;
  if let Err(cause)=tokio::fs::rename(&staging,&destination.install_dir).await{if !installed(&destination,version).await{return Err(error(tool,"publishing the install",cause));}}
  Ok(destination)
 }.await;
    // This owner retains install admission through cleanup, including caller cancellation.
    if let Err(cause) = remove(&staging).await {
        tracing::debug!(%cause,"device install staging cleanup failed");
    }
    outcome
}
struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    stdout_invalid_utf8: bool,
    stderr_invalid_utf8: bool,
}
#[derive(Debug)]
enum RunError {
    Spawn(std::io::Error),
    Io(std::io::Error),
    Timeout,
    Cancelled,
    Limit,
}
impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(error) | Self::Io(error) => error.fmt(f),
            Self::Timeout => f.write_str("process timed out"),
            Self::Cancelled => f.write_str("process cancelled"),
            Self::Limit => f.write_str("process output exceeds the byte limit"),
        }
    }
}
async fn collect(mut read: impl AsyncRead + Unpin) -> Result<Vec<u8>, RunError> {
    let mut output = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = read.read(&mut chunk).await.map_err(RunError::Io)?;
        if count == 0 {
            return Ok(output);
        }
        if count > OUTPUT_LIMIT - output.len() {
            return Err(RunError::Limit);
        }
        output.extend_from_slice(&chunk[..count]);
    }
}
async fn run(
    command: &Path,
    args: &[String],
    timeout: Duration,
    stopped: &mut watch::Receiver<bool>,
) -> Result<Output, RunError> {
    if *stopped.borrow() {
        return Err(RunError::Cancelled);
    }
    let mut command = crate::acp_registry_spawn::command(&command.to_string_lossy(), args, None);
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let outcome = tokio::select! {biased;_=stopped.wait_for(|stop|*stop)=>Err(RunError::Cancelled),result=tokio::time::timeout(timeout,async{tokio::try_join!(async {child.wait().await.map_err(RunError::Io)},collect(stdout),collect(stderr))})=>match result{Ok(result)=>result,Err(_)=>Err(RunError::Timeout)}};
    match outcome {
        Ok((status, stdout, stderr)) => Ok(Output {
            code: status.code(),
            stdout_invalid_utf8: std::str::from_utf8(&stdout).is_err(),
            stderr_invalid_utf8: std::str::from_utf8(&stderr).is_err(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }),
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(error)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    fn executable(path: &Path, source: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, format!("#!/usr/bin/env python3\n{source}")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(unix)]
    fn installer(temp: &Path) -> PathBuf {
        let path = temp.join("npm-fixture");
        let record = serde_json::to_string(&temp.join("invocations").to_string_lossy()).unwrap();
        executable(
            &path,
            &format!(
                r#"import sys,json,pathlib
args=sys.argv[1:]
with open({record},'a') as f: f.write(json.dumps(args)+'\n')
root=pathlib.Path(args[args.index('--prefix')+1])
name,version=args[-1].split('@')
entry='dist/server/cli.mjs' if name=='expo-device-hub' else 'bin/agent-device.mjs'
file=root/'node_modules'/name/entry
file.parent.mkdir(parents=True,exist_ok=True)
file.write_text('fixture')
"#
            ),
        );
        path
    }
    #[tokio::test]
    async fn inspection_requires_completed_sentinel_and_preserves_unknown_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let toolchain = DeviceToolchain::new(ToolchainOptions::new(temp.path().into()));
        for (version, sentinel) in [
            ("0.9.0", "0.9.0"),
            (DEVICE_HUB_VERSION, "wrong"),
            (".staging-123", ".staging-123"),
            ("2.0.0-beta.1", "2.0.0-beta.1"),
        ] {
            let paths = paths(temp.path(), DeviceToolKind::Hub, version);
            tokio::fs::create_dir_all(paths.entry_path.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(paths.entry_path, "").await.unwrap();
            tokio::fs::write(paths.sentinel_path, sentinel)
                .await
                .unwrap();
        }
        let versions = toolchain
            .versions(Some("0.9.0".into()), None)
            .await
            .unwrap();
        assert_eq!(versions.hub.installed_versions, ["0.9.0", "2.0.0-beta.1"]);
        assert_eq!(versions.hub.running_version.as_deref(), Some("0.9.0"));
        assert!(versions.agent.installed_versions.is_empty());
        assert!(!toolchain.installed(DeviceToolKind::Hub).await);
        let invalid = temp.path().join("file");
        tokio::fs::write(&invalid, "").await.unwrap();
        assert!(
            DeviceToolchain::new(ToolchainOptions::new(invalid))
                .versions(None, None)
                .await
                .is_none()
        );
        toolchain.shutdown().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_install_publishes_once_and_missing_npm_uses_exact_pnpm_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let mut options = ToolchainOptions::new(temp.path().into());
        options.npm = temp.path().join("missing-npm");
        options.pnpm = installer(temp.path());
        let toolchain = DeviceToolchain::new(options);
        let (a, b) = tokio::join!(
            toolchain.ensure(DeviceToolKind::Hub),
            toolchain.ensure(DeviceToolKind::Hub)
        );
        let paths = a.unwrap();
        assert_eq!(paths.entry_path, b.unwrap().entry_path);
        assert!(toolchain.installed(DeviceToolKind::Hub).await);
        assert_eq!(
            tokio::fs::read_to_string(&paths.sentinel_path)
                .await
                .unwrap(),
            format!("{DEVICE_HUB_VERSION}\n")
        );
        let calls = tokio::fs::read_to_string(temp.path().join("invocations"))
            .await
            .unwrap();
        assert_eq!(calls.lines().count(), 1);
        let args: Vec<String> = serde_json::from_str(calls.lines().next().unwrap()).unwrap();
        assert_eq!(&args[..4], ["--package=npm@11", "dlx", "npm", "install"]);
        assert_eq!(
            &args[6..],
            ["--no-fund", "--no-audit", "expo-device-hub@0.12.0"]
        );
        assert!(args[5].contains(".staging-"));
        assert_eq!(
            std::fs::read_dir(paths.install_dir.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        toolchain.shutdown().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_is_safe_and_does_not_fallback_or_publish_an_incomplete_tree() {
        let temp = tempfile::tempdir().unwrap();
        let npm = temp.path().join("npm-failure");
        executable(
            &npm,
            "import sys\nsys.stderr.write('https://private:credential@example.test/package')\nsys.exit(1)\n",
        );
        let mut options = ToolchainOptions::new(temp.path().into());
        options.npm = npm;
        options.pnpm = installer(temp.path());
        let toolchain = DeviceToolchain::new(options);
        let failure = toolchain.ensure(DeviceToolKind::Hub).await.unwrap_err();
        assert_eq!(
            failure.to_string(),
            "Installing expo-device-hub failed while running npm install (exit code 1)."
        );
        assert_eq!(
            failure.cause.as_ref().unwrap()["stderr"],
            "https://private:credential@example.test/package"
        );
        assert!(!toolchain.installed(DeviceToolKind::Hub).await);
        assert!(!temp.path().join("invocations").exists());
        assert_eq!(
            std::fs::read_dir(temp.path().join("tools/expo-device-hub"))
                .unwrap()
                .count(),
            0
        );
        toolchain.shutdown().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_install_reaps_captured_child_and_cleans_staging_before_retry() {
        let temp = tempfile::tempdir().unwrap();
        let socket_path = temp.path().join("milestone.sock");
        let socket = tokio::net::UnixDatagram::bind(&socket_path).unwrap();
        let npm = temp.path().join("npm-held");
        executable(
            &npm,
            &format!(
                "import socket,os,signal\ns=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM)\ns.sendto(str(os.getpid()).encode(),{})\nsignal.pause()\n",
                serde_json::to_string(&socket_path.to_string_lossy()).unwrap()
            ),
        );
        let mut options = ToolchainOptions::new(temp.path().into());
        options.npm = npm.clone();
        let toolchain = DeviceToolchain::new(options);
        let task = {
            let toolchain = toolchain.clone();
            tokio::spawn(async move { toolchain.ensure(DeviceToolKind::Hub).await })
        };
        let mut bytes = [0; 128];
        let size = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        let pid: i32 = std::str::from_utf8(&bytes[..size])
            .unwrap()
            .parse()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), toolchain.shutdown())
            .await
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert_eq!(
            std::fs::read_dir(temp.path().join("tools/expo-device-hub"))
                .unwrap()
                .count(),
            0
        );
        let successful = installer(temp.path());
        let mut options = ToolchainOptions::new(temp.path().into());
        options.npm = successful;
        let retry = DeviceToolchain::new(options);
        retry.ensure(DeviceToolKind::Hub).await.unwrap();
        assert!(retry.installed(DeviceToolKind::Hub).await);
        retry.shutdown().await;
    }
}
