//! Plain installer commands; these never use the provider JSON-RPC transport.
use crate::acp_registry_support::RegistryError;
use indexmap::IndexMap;
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
    sync::{oneshot, watch},
};

#[cfg(windows)]
use tokio::process::Command;

const OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) struct CommandOptions {
    pub command: String,
    pub arguments: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub environment: Option<IndexMap<String, String>>,
    pub timeout: Option<Duration>,
    pub truncated_output_reason: &'static str,
}

#[derive(Debug, PartialEq)]
struct CollectedText {
    text: String,
    bytes: usize,
    truncated: bool,
    invalid_utf8: bool,
}

#[derive(Default)]
struct Collector {
    bytes: Vec<u8>,
    truncated: bool,
}
impl Collector {
    fn push(&mut self, chunk: &[u8], limit: usize) {
        if self.truncated {
            return;
        }
        let remaining = limit.saturating_sub(self.bytes.len());
        if remaining == 0 {
            self.truncated = true;
            return;
        }
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        self.truncated = chunk.len() > remaining;
    }
    fn finish(self) -> CollectedText {
        CollectedText {
            text: String::from_utf8_lossy(&self.bytes).into_owned(),
            bytes: self.bytes.len(),
            truncated: self.truncated,
            invalid_utf8: std::str::from_utf8(&self.bytes).is_err(),
        }
    }
}
async fn collect(mut reader: impl AsyncRead + Unpin) -> std::io::Result<CollectedText> {
    let mut collector = Collector::default();
    let mut buffer = [0; 16 * 1024];
    loop {
        let length = reader.read(&mut buffer).await?;
        if length == 0 {
            return Ok(collector.finish());
        }
        // Keep draining both pipes after the cap; otherwise a child can block
        // forever on its stdout/stderr and never reach the exit milestone.
        collector.push(&buffer[..length], OUTPUT_LIMIT);
    }
}
fn error(reason: &'static str, detail: impl Into<String>) -> RegistryError {
    RegistryError {
        reason,
        detail: detail.into(),
    }
}

/// The worker owns the child and installation guards from spawn through reap.
/// Dropping an awaiting caller cancels the worker, rather than dropping an
/// untracked Child or releasing the installation lock before filesystem tools stop.
pub(crate) struct RunningCommand {
    cancel: Option<oneshot::Sender<()>>,
    result: Option<oneshot::Receiver<Result<String, RegistryError>>>,
    disposed: watch::Receiver<bool>,
    #[cfg(test)]
    pid: u32,
}
impl RunningCommand {
    pub fn start(
        options: CommandOptions,
        installation: Arc<dyn Send + Sync>,
    ) -> Result<Self, RegistryError> {
        let mut command = crate::acp_registry_spawn::command(
            &options.command,
            &options.arguments,
            options.environment.as_ref(),
        );
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &options.cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|_| {
            error(
                "install_failed",
                format!(
                    "Could not run ACP Registry install command '{}'.",
                    options.command
                ),
            )
        })?;
        let pid = child.id().expect("new child has a process ID");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (cancel, cancelled) = oneshot::channel();
        let (reply, result) = oneshot::channel();
        let (disposed, completed) = watch::channel(false);
        tokio::spawn(async move {
            let outcome = {
                let collect =
                    async { tokio::try_join!(collect(stdout), collect(stderr), child.wait()) };
                tokio::pin!(collect);
                let timeout = async {
                    match options.timeout {
                        Some(duration) => tokio::time::sleep(duration).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    result = &mut collect => (Some(result), false),
                    _ = cancelled => (None, false),
                    _ = timeout => (None, true),
                }
            };
            let (outcome, timed_out) = outcome;
            let result = match outcome {
                Some(Ok((stdout, _, status))) if status.success() => {
                    if stdout.truncated {
                        Err(error(
                            options.truncated_output_reason,
                            format!(
                                "ACP Registry install command '{}' produced more output than expected.",
                                options.command
                            ),
                        ))
                    } else {
                        Ok(stdout.text)
                    }
                }
                Some(Ok((_, stderr, status))) => Err(error(
                    "install_failed",
                    format!(
                        "ACP Registry install command '{}' exited with code {}: {}",
                        options.command,
                        status
                            .code()
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "signal".into()),
                        t3_contracts::trim_wire_string(&stderr.text)
                    ),
                )),
                Some(Err(_)) | None => {
                    terminate(&mut child, pid).await;
                    Err(error(
                        "install_failed",
                        if timed_out {
                            format!(
                                "Timed out running ACP Registry install command '{}'.",
                                options.command
                            )
                        } else {
                            format!(
                                "Could not complete ACP Registry install command '{}'.",
                                options.command
                            )
                        },
                    ))
                }
            };
            // Close child handles before releasing the last installation owner.
            drop(child);
            drop(installation);
            let _ = reply.send(result);
            let _ = disposed.send(true);
        });
        Ok(Self {
            cancel: Some(cancel),
            result: Some(result),
            disposed: completed,
            #[cfg(test)]
            pid,
        })
    }
    pub async fn wait(&mut self) -> Result<String, RegistryError> {
        self.result
            .take()
            .expect("command result awaited once")
            .await
            .map_err(|_| {
                error(
                    "install_failed",
                    "ACP Registry command owner stopped unexpectedly.",
                )
            })?
    }
    pub async fn shutdown(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        let _ = self.disposed.wait_for(|disposed| *disposed).await;
    }
}
impl Drop for RunningCommand {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}
async fn terminate(child: &mut Child, pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        // PID captured at this spawn, never discovered by a process-name search.
        let _ = Command::new("taskkill")
            .args(["/pid", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn python(script: &str) -> CommandOptions {
        CommandOptions {
            command: "python3".into(),
            arguments: vec!["-c".into(), script.into()],
            cwd: None,
            environment: None,
            timeout: None,
            truncated_output_reason: "archive_invalid",
        }
    }
    #[test]
    fn original_byte_collector_fixtures() {
        for line in include_str!("../tests/fixtures/acp-registry-archives.jsonl").lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            if row["operation"] != "collect" {
                continue;
            }
            let mut collector = Collector::default();
            for chunk in row["chunks"].as_array().unwrap() {
                let bytes = chunk
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap() as u8)
                    .collect::<Vec<_>>();
                collector.push(&bytes, row["limit"].as_u64().unwrap() as usize);
            }
            let actual = collector.finish();
            assert_eq!(
                serde_json::json!({"text":actual.text,"bytes":actual.bytes,
                "truncated":actual.truncated,"invalidUtf8":actual.invalid_utf8}),
                row["output"],
                "{row}"
            );
        }
    }
    #[tokio::test]
    async fn capped_commands_drain_both_pipes_and_preserve_error_precedence() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let script = "import os,sys\nos.write(1,b'x'*(1024*1024+1))\nos.write(2,b'y'*(1024*1024+1))\nsys.exit(int(sys.argv[1]))";
            for code in [0, 7] {
                let mut options = python(script);
                options.arguments.push(code.to_string());
                let mut job = RunningCommand::start(options, Arc::new(())).unwrap();
                let result = job.wait().await.unwrap_err();
                assert_eq!(result.reason, if code == 0 { "archive_invalid" } else { "install_failed" });
                if code == 7 { assert!(result.detail.contains("exited with code 7: y")); }
                job.shutdown().await;
            }
            let mut job = RunningCommand::start(python("import os\nos.write(1,bytes([239,187,191,102,128,111]))"), Arc::new(())).unwrap();
            assert_eq!(job.wait().await.unwrap(), "\u{feff}f\u{fffd}o");
        }).await.unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn caller_cancellation_retains_install_owner_until_actual_child_is_reaped() {
        tokio::time::timeout(Duration::from_secs(10), async {
            struct Installation { released: watch::Sender<bool> }
            impl Drop for Installation { fn drop(&mut self) { let _ = self.released.send(true); } }
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("started.sock");
            let receiver = tokio::net::UnixDatagram::bind(&socket).unwrap();
            let (released, mut release) = watch::channel(false);
            let owner = Arc::new(Installation { released });
            let mut options = python("import os,signal,socket,sys\ns=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM)\ns.sendto(str(os.getpid()).encode(),sys.argv[1])\nwhile True: signal.pause()");
            options.arguments.push(socket.to_str().unwrap().into());
            let mut job = RunningCommand::start(options, owner).unwrap();
            let pid = job.pid;
            let mut disposed = job.disposed.clone();
            let waiting = tokio::spawn(async move { job.wait().await });
            let mut bytes = [0; 64];
            let length = receiver.recv(&mut bytes).await.unwrap();
            assert_eq!(std::str::from_utf8(&bytes[..length]).unwrap(), pid.to_string());
            assert!(!*release.borrow());
            waiting.abort();
            assert!(waiting.await.unwrap_err().is_cancelled());
            release.wait_for(|released| *released).await.unwrap();
            disposed.wait_for(|disposed| *disposed).await.unwrap();
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        }).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn expired_command_deadline_reaps_spawned_child_before_returning_error() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut options = python("import signal\nwhile True: signal.pause()");
            options.timeout = Some(Duration::ZERO);
            let mut job = RunningCommand::start(options, Arc::new(())).unwrap();
            let pid = job.pid;
            let failure = job.wait().await.unwrap_err();
            assert_eq!(failure.reason, "install_failed");
            assert!(
                failure
                    .detail
                    .starts_with("Timed out running ACP Registry install command")
            );
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        })
        .await
        .unwrap();
    }
}
