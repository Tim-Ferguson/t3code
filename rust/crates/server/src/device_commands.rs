//! Device-host commands retain their process source through cancellation and reap.
use crate::{terminal_environment::Environment, terminal_inspector::NativeProcessTable};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};
#[derive(Debug, Clone)]
pub struct HostCommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn complete_environment_and_nonzero_command_output_are_preserved() {
        let commands = DeviceCommands::new();
        let output = commands
            .run(
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "printf '%s' \"$FIXTURE_VALUE\"; printf diagnostic >&2; exit 7".into(),
                ],
                [("FIXTURE_VALUE".into(), "isolated value".into())]
                    .into_iter()
                    .collect(),
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(output.code, 7);
        assert_eq!(output.stdout, "isolated value");
        assert_eq!(output.stderr, "diagnostic");
        commands.shutdown().await;
        assert_eq!(
            commands
                .run(
                    "/bin/sh".into(),
                    Vec::new(),
                    Default::default(),
                    Duration::from_secs(1)
                )
                .await
                .code,
            127
        );
    }
    #[tokio::test]
    async fn cancelled_command_is_reaped_before_owned_pool_shutdown_returns() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("command.sock");
        let socket = tokio::net::UnixDatagram::bind(&path).unwrap();
        let commands = DeviceCommands::new();
        let request = tokio::spawn({
            let commands = commands.clone();
            let path = path.to_string_lossy().into_owned();
            async move {
                commands.run("/usr/bin/python3".into(),vec!["-c".into(),"import os,signal,socket,sys;s=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM);s.sendto(str(os.getpid()).encode(),sys.argv[1]);signal.pause()".into(),path],Default::default(),Duration::from_secs(30)).await
            }
        });
        let mut bytes = [0; 100];
        let length = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        let pid = std::str::from_utf8(&bytes[..length])
            .unwrap()
            .parse::<i32>()
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), commands.shutdown())
            .await
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
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
pub struct DeviceCommands(Arc<Inner>);
impl std::fmt::Debug for DeviceCommands {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceCommands")
    }
}
struct Cancel(watch::Sender<bool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
fn failed(message: impl Into<String>) -> HostCommandOutput {
    HostCommandOutput {
        stdout: String::new(),
        stderr: message.into(),
        code: 127,
    }
}
impl Default for DeviceCommands {
    fn default() -> Self {
        Self::new()
    }
}
impl DeviceCommands {
    pub fn new() -> Self {
        Self(Arc::new(Inner {
            jobs: Mutex::new(Some(Vec::new())),
            shutdown: tokio::sync::Mutex::new(Vec::new()),
        }))
    }
    pub async fn run(
        &self,
        command: PathBuf,
        args: Vec<String>,
        environment: Environment,
        timeout: Duration,
    ) -> HostCommandOutput {
        let (stop, mut stopped) = watch::channel(false);
        let guard = Cancel(stop.clone());
        let (reply, receive) = oneshot::channel();
        {
            let mut jobs = self.0.jobs.lock().unwrap();
            let Some(jobs) = jobs.as_mut() else {
                return failed("Device host commands are shut down.");
            };
            jobs.retain(|job| !job.handle.is_finished());
            jobs.push(Job{stop,handle:tokio::spawn(async move{
    let source=NativeProcessTable::command_with_environment(command,args,"device command",timeout,8*1024*1024,environment);
    let outcome=tokio::select!{biased;_=stopped.wait_for(|stop|*stop)=>failed("Device command was cancelled."),result=source.output()=>match result{Ok(output)=>HostCommandOutput{stdout:String::from_utf8_lossy(&output.stdout).into_owned(),stderr:String::from_utf8_lossy(&output.stderr).into_owned(),code:output.exit_code.unwrap_or(127)},Err(error)=>failed(error.to_string())}};
    source.shutdown().await;let _=reply.send(outcome);
   })});
        }
        let output = receive
            .await
            .unwrap_or_else(|_| failed("Device command owner stopped."));
        drop(guard);
        output
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
