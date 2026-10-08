//! One bounded, authoritative process-table snapshot, shared across terminals.
//! Requests own cancellation; the worker always reaps its captured child before
//! completion, including when a caller drops the request future.
use crate::terminal_activity::ProcessTable;
use futures_util::future::BoxFuture;
use std::{
    fmt, io,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::{oneshot, watch},
    task::JoinHandle,
};

#[derive(Clone, Debug)]
pub struct InspectionError {
    pub command: &'static str,
    pub cause: Option<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
}
impl InspectionError {
    pub fn source(command: &'static str, cause: impl fmt::Display) -> Self {
        Self {
            command,
            cause: Some(cause.to_string()),
            exit_code: None,
            timed_out: false,
            stdout_truncated: false,
        }
    }
}
impl fmt::Display for InspectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Failed to inspect terminal subprocesses with {}",
            self.command
        )?;
        let mut details = Vec::new();
        if let Some(code) = self.exit_code {
            details.push(format!("exit code {code}"));
        }
        if self.timed_out {
            details.push("timed out".into());
        }
        if self.stdout_truncated {
            details.push("output truncated".into());
        }
        if !details.is_empty() {
            write!(f, " ({})", details.join(", "))?;
        }
        Ok(())
    }
}
impl std::error::Error for InspectionError {}
pub type ProcessTableProvider =
    Arc<dyn Fn() -> BoxFuture<'static, Result<ProcessTable, InspectionError>> + Send + Sync>;

struct Task {
    cancel: watch::Sender<bool>,
    handle: JoinHandle<()>,
}
impl Drop for Task {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}
#[derive(Clone, Debug)]
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}
struct Inner {
    tasks: Mutex<Option<Vec<Task>>>,
    spec: CommandSpec,
    shutdown: tokio::sync::Mutex<Vec<Task>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(tasks) = self.tasks.get_mut().unwrap().take() {
            for task in tasks {
                task.cancel.send_replace(true);
            }
        }
    }
}
#[derive(Clone)]
pub struct NativeProcessTable(Arc<Inner>);
#[derive(Clone)]
struct CommandSpec {
    environment: Option<crate::terminal_environment::Environment>,
    command: PathBuf,
    args: Vec<String>,
    label: &'static str,
    windows: bool,
    timeout: Duration,
    max_bytes: usize,
    #[cfg(test)]
    started: Option<Arc<(std::sync::atomic::AtomicU32, tokio::sync::Notify)>>,
    #[cfg(test)]
    finish_gate: Option<Arc<(tokio::sync::Notify, tokio::sync::Notify)>>,
}
struct Cancel(watch::Sender<bool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
impl NativeProcessTable {
    pub fn new(platform: &str) -> Self {
        let windows = platform == "win32";
        let command = if windows {
            PathBuf::from("powershell.exe")
        } else {
            ["/bin/ps", "/usr/bin/ps"]
                .into_iter()
                .find(|candidate| std::path::Path::new(candidate).exists())
                .unwrap_or("ps")
                .into()
        };
        let args = if windows {
            vec!["-NoProfile", "-NonInteractive", "-Command", "Get-CimInstance Win32_Process -ErrorAction Stop | ForEach-Object { Write-Output \"$($_.ProcessId)|$($_.ParentProcessId)|$($_.Name)\" }"]
        } else { vec!["-eo", "pid=,ppid=,comm="] }.into_iter().map(String::from).collect();
        Self::with_spec(CommandSpec {
            environment: None,
            command,
            args,
            label: if windows { "powershell" } else { "ps" },
            windows,
            timeout: Duration::from_millis(if windows { 1500 } else { 1000 }),
            max_bytes: if windows { 262_144 } else { 524_288 },
            #[cfg(test)]
            started: None,
            #[cfg(test)]
            finish_gate: None,
        })
    }
    fn with_spec(spec: CommandSpec) -> Self {
        Self(Arc::new(Inner {
            tasks: Mutex::new(Some(Vec::new())),
            spec,
            shutdown: tokio::sync::Mutex::new(Vec::new()),
        }))
    }
    /// Shares captured-child cancellation and reap ownership with process tables.
    /// Returned native diagnostics (nonzero exit/truncation) remain distinguishable
    /// from spawn/read/timeout failure; PortScanner consumes even partial output.
    pub fn command(
        command: PathBuf,
        args: Vec<String>,
        label: &'static str,
        timeout: Duration,
        max_bytes: usize,
    ) -> Self {
        Self::with_spec(CommandSpec {
            environment: None,
            command,
            args,
            label,
            windows: false,
            timeout,
            max_bytes,
            #[cfg(test)]
            started: None,
            #[cfg(test)]
            finish_gate: None,
        })
    }
    /// Complete host environment for commands whose SDK and tool PATH are
    /// resolved by an owning service. Captured-child cleanup is unchanged.
    pub fn command_with_environment(
        command: PathBuf,
        args: Vec<String>,
        label: &'static str,
        timeout: Duration,
        max_bytes: usize,
        environment: crate::terminal_environment::Environment,
    ) -> Self {
        let mut source = Self::command(command, args, label, timeout, max_bytes);
        Arc::get_mut(&mut source.0).unwrap().spec.environment = Some(environment);
        source
    }
    pub async fn snapshot(&self) -> Result<ProcessTable, InspectionError> {
        let output = self.output().await?;
        if output.exit_code != Some(0) || output.stdout_truncated {
            return Err(InspectionError {
                command: self.0.spec.label,
                cause: None,
                exit_code: output.exit_code,
                timed_out: false,
                stdout_truncated: output.stdout_truncated,
            });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(if self.0.spec.windows {
            ProcessTable::windows(&text)
        } else {
            ProcessTable::posix(&text)
        })
    }
    pub async fn output(&self) -> Result<CommandOutput, InspectionError> {
        let (result, receive) = oneshot::channel();
        let (cancel, cancelled) = watch::channel(false);
        let guard = Cancel(cancel.clone());
        {
            let mut owned = self.0.tasks.lock().unwrap();
            let tasks = owned.as_mut().ok_or_else(|| {
                InspectionError::source(self.0.spec.label, "process-table source is shut down")
            })?;
            tasks.retain(|task| !task.handle.is_finished());
            let spec = self.0.spec.clone();
            tasks.push(Task {
                cancel,
                handle: tokio::spawn(async move {
                    let outcome = run(spec, cancelled).await;
                    let _ = result.send(outcome);
                }),
            });
        }
        let result = receive
            .await
            .map_err(|cause| InspectionError::source(self.0.spec.label, cause))?;
        drop(guard);
        result
    }
    pub async fn shutdown(&self) {
        // Every concurrent caller observes full worker quiescence, rather than
        // only the first caller taking ownership of the join handles.
        let mut shutdown = self.0.shutdown.lock().await;
        shutdown.extend(self.0.tasks.lock().unwrap().take().unwrap_or_default());
        for task in shutdown.iter() {
            task.cancel.send_replace(true);
        }
        while let Some(task) = shutdown.first_mut() {
            if let Err(error) = (&mut task.handle).await {
                tracing::warn!(%error,"process-table worker failed");
            }
            shutdown.remove(0);
        }
    }
}

async fn collect(
    mut reader: impl AsyncRead + Unpin,
    max_bytes: usize,
) -> io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..count.min(remaining)]);
        truncated |= count > remaining;
    }
    Ok((retained, truncated))
}
async fn run(
    spec: CommandSpec,
    mut cancelled: watch::Receiver<bool>,
) -> Result<CommandOutput, InspectionError> {
    if *cancelled.borrow() {
        return Err(InspectionError::source(
            spec.label,
            "process inspection cancelled",
        ));
    }
    let mut builder = crate::acp_registry_spawn::command(
        &spec.command.to_string_lossy(),
        &spec.args,
        spec.environment.as_ref(),
    );
    let mut child = builder
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|cause| InspectionError::source(spec.label, cause))?;
    #[cfg(test)]
    if let Some(started) = &spec.started {
        started
            .0
            .store(child.id().unwrap(), std::sync::atomic::Ordering::SeqCst);
        started.1.notify_one();
    }
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let complete = async {
        tokio::try_join!(
            child.wait(),
            collect(stdout, spec.max_bytes),
            collect(stderr, spec.max_bytes)
        )
    };
    let outcome = tokio::select! {
        biased;
        _ = cancelled.wait_for(|value| *value) => Err(InspectionError::source(spec.label, "process inspection cancelled")),
        outcome = tokio::time::timeout(spec.timeout, complete) => match outcome {
            Err(_) => Err(InspectionError { command: spec.label, cause: None, exit_code: None, timed_out: true, stdout_truncated: false }),
            Ok(Err(cause)) => Err(InspectionError::source(spec.label, cause)),
            Ok(Ok((status,(stdout,stdout_truncated),(stderr,stderr_truncated))))=>Ok(CommandOutput{stdout,stderr,exit_code:status.code(),stdout_truncated,stderr_truncated}),
        }
    };
    if outcome.is_err() {
        // This is this command's unreaped child handle, never a matched PID.
        if let Err(error) = child.kill().await {
            tracing::debug!(%error, "process-table child already exited or could not be killed");
        }
        if let Err(error) = child.wait().await {
            tracing::warn!(%error, "failed to reap process-table child");
        }
    }
    #[cfg(test)]
    if let Some(gate) = spec.finish_gate {
        gate.0.notify_one();
        gate.1.notified().await;
    }
    outcome
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn fixture(directory: &std::path::Path, body: &str, max_bytes: usize) -> NativeProcessTable {
        let script = directory.join("process-table.py");
        std::fs::write(&script, format!("#!/usr/bin/env python3\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        NativeProcessTable::with_spec(CommandSpec {
            environment: None,
            command: script,
            args: vec![],
            label: "ps",
            windows: false,
            timeout: Duration::from_secs(3),
            max_bytes,
            started: None,
            #[cfg(test)]
            finish_gate: None,
        })
    }
    #[tokio::test]
    async fn raw_command_preserves_returned_nonzero_and_truncation_diagnostics() {
        let directory = tempfile::tempdir().unwrap();
        let source = fixture(
            directory.path(),
            "import sys;print('abcdefghijk');sys.stderr.write('diagnostic');sys.exit(7)",
            5,
        );
        let output = source.output().await.unwrap();
        assert_eq!(output.exit_code, Some(7));
        assert_eq!(output.stdout, b"abcde");
        assert_eq!(output.stderr, b"diagn");
        assert!(output.stdout_truncated && output.stderr_truncated);
        source.shutdown().await;
    }
    #[tokio::test]
    async fn cancelled_shutdown_retains_owned_join_until_next_shutdown_completes() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = fixture(directory.path(), "import signal;signal.pause()", 1024);
        let started = Arc::new((
            std::sync::atomic::AtomicU32::new(0),
            tokio::sync::Notify::new(),
        ));
        let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
        let spec = &mut Arc::get_mut(&mut source.0).unwrap().spec;
        spec.started = Some(started.clone());
        spec.finish_gate = Some(gate.clone());
        let request = tokio::spawn({
            let source = source.clone();
            async move { source.output().await }
        });
        started.1.notified().await;
        let first = tokio::spawn({
            let source = source.clone();
            async move { source.shutdown().await }
        });
        gate.0.notified().await; // Captured child has been reaped; worker is held.
        first.abort();
        assert!(matches!(first.await,Err(error) if error.is_cancelled()));
        let pid = started.0.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        assert_eq!(
            source.0.shutdown.lock().await.len(),
            1,
            "cancelled caller retained the worker join"
        );
        gate.1.notify_one();
        source.shutdown().await;
        assert!(request.await.unwrap().is_err());
        assert!(source.0.shutdown.lock().await.is_empty());
    }
    #[tokio::test]
    async fn authoritative_table_requires_success_and_full_output() {
        let directory = tempfile::tempdir().unwrap();
        let source = fixture(directory.path(), "print('100 1 sh\\n101 100 vim')", 1024);
        assert_eq!(
            source
                .snapshot()
                .await
                .unwrap()
                .inspect(
                    crate::terminal_activity::ProcessId::new(100.0).unwrap(),
                    "linux"
                )
                .child_command
                .as_deref(),
            Some("vim")
        );
        source.shutdown().await;
        let source = fixture(directory.path(), "print('100 1 sh\\n101 100 vim')", 10);
        let error = source.snapshot().await.unwrap_err();
        assert!(error.stdout_truncated);
        assert_eq!(error.exit_code, Some(0));
        source.shutdown().await;
        let source = fixture(
            directory.path(),
            "import sys;print('100 1 sh');sys.exit(7)",
            1024,
        );
        assert_eq!(source.snapshot().await.unwrap_err().exit_code, Some(7));
        source.shutdown().await;
    }
    #[tokio::test]
    async fn dropped_request_cancels_and_reaps_owned_child_before_source_shutdown_completes() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = fixture(directory.path(), "import signal;signal.pause()", 1024);
        let started = Arc::new((
            std::sync::atomic::AtomicU32::new(0),
            tokio::sync::Notify::new(),
        ));
        Arc::get_mut(&mut source.0).unwrap().spec.started = Some(started.clone());
        let task_source = source.clone();
        let request = tokio::spawn(async move { task_source.snapshot().await });
        started.1.notified().await;
        let pid = started.0.load(std::sync::atomic::Ordering::SeqCst) as i32;
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        request.abort();
        let _ = request.await;
        source.shutdown().await;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    #[tokio::test]
    async fn timeout_reaps_captured_child_before_returning_failure() {
        let directory = tempfile::tempdir().unwrap();
        let mut source = fixture(directory.path(), "import signal;signal.pause()", 1024);
        let started = Arc::new((
            std::sync::atomic::AtomicU32::new(0),
            tokio::sync::Notify::new(),
        ));
        let spec = &mut Arc::get_mut(&mut source.0).unwrap().spec;
        spec.started = Some(started.clone());
        spec.timeout = Duration::ZERO;
        assert!(source.snapshot().await.unwrap_err().timed_out);
        let pid = started.0.load(std::sync::atomic::Ordering::SeqCst) as i32;
        assert!(pid > 0);
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        source.shutdown().await;
    }
}
