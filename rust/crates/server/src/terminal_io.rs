//! Bounded terminal I/O worker. A blocked write never owns the cancellation
//! capability, so close can terminate the child and join that same worker.
use crate::terminal_process::{OwnedPty, PtyEvent, PtySpawn, PtyTermination};
use std::{io, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};

enum Command {
    Write(String, oneshot::Sender<io::Result<()>>),
    Resize(u16, u16, oneshot::Sender<io::Result<()>>),
}
struct Client {
    commands: mpsc::Sender<Command>,
    stop: watch::Sender<bool>,
    termination: PtyTermination,
    #[cfg(test)]
    write_started: Arc<tokio::sync::Notify>,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        let _ = self.termination.signal(true);
    }
}
#[derive(Clone)]
pub struct TerminalIo(Arc<Client>);
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "Terminal I/O worker has stopped")
}
impl TerminalIo {
    pub async fn write(&self, data: String) -> io::Result<()> {
        let (send, receive) = oneshot::channel();
        self.0
            .commands
            .send(Command::Write(data, send))
            .await
            .map_err(|_| closed())?;
        receive.await.map_err(|_| closed())?
    }
    pub async fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let (send, receive) = oneshot::channel();
        self.0
            .commands
            .send(Command::Resize(cols, rows, send))
            .await
            .map_err(|_| closed())?;
        receive.await.map_err(|_| closed())?
    }
}
pub struct TerminalProcess {
    pub pid: u32,
    pub io: TerminalIo,
    pub events: mpsc::Receiver<PtyEvent>,
    worker: Option<tokio::task::JoinHandle<io::Result<()>>>,
}
impl TerminalProcess {
    pub async fn spawn(input: PtySpawn, grace: Duration) -> io::Result<Self> {
        let mut pty = tokio::task::spawn_blocking(move || OwnedPty::spawn(input))
            .await
            .map_err(|error| io::Error::other(error.to_string()))??;
        let termination = pty.termination();
        let events = pty.take_event_receiver();
        let (commands, mut receive) = mpsc::channel(32);
        let (stop, mut stopped) = watch::channel(false);
        let pid = pty.pid;
        let worker_termination = termination.clone();
        #[cfg(test)]
        let write_started = Arc::new(tokio::sync::Notify::new());
        #[cfg(test)]
        let registered = write_started.clone();
        let worker = tokio::spawn(async move {
            loop {
                let command = tokio::select! {
                    biased;
                    _ = async { let _ = stopped.wait_for(|value| *value).await; } => break,
                    command = receive.recv() => match command { Some(command) => command, None => break },
                };
                #[cfg(test)]
                if matches!(command, Command::Write(..)) {
                    registered.notify_one();
                }
                let mut job = tokio::task::spawn_blocking(move || {
                    let (result, complete) = match command {
                        Command::Write(data, complete) => (pty.write(&data), complete),
                        Command::Resize(cols, rows, complete) => (pty.resize(cols, rows), complete),
                    };
                    (pty, result, complete)
                });
                let mut stopping = false;
                let (returned, result, complete) = tokio::select! {
                    result = &mut job => result,
                    _ = async { let _ = stopped.wait_for(|value| *value).await; } => {
                        stopping = true;
                        let _ = worker_termination.signal(false);
                        match tokio::time::timeout(grace,&mut job).await {
                            Ok(result) => result,
                            Err(_) => { let _ = worker_termination.signal(true); job.await },
                        }
                    }
                }
                .map_err(|error| io::Error::other(error.to_string()))?;
                pty = returned;
                let _ = complete.send(result);
                if stopping {
                    break;
                }
            }
            // Reject queued writes, then close/reap after the session has closed
            // its event receiver. This unblocks the bounded output producer.
            receive.close();
            pty.close(grace).await
        });
        Ok(Self {
            pid,
            io: TerminalIo(Arc::new(Client {
                commands,
                stop,
                termination,
                #[cfg(test)]
                write_started,
            })),
            events,
            worker: Some(worker),
        })
    }
    pub async fn close(mut self) -> io::Result<()> {
        self.events.close();
        self.io.0.stop.send_replace(true);
        self.worker
            .take()
            .unwrap()
            .await
            .map_err(|error| io::Error::other(error.to_string()))?
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn closing_backpressured_write_terminates_child_and_joins_owned_io_worker() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("backpressured.py");
        std::fs::write(&script,"import os,signal,termios\na=termios.tcgetattr(0);a[3]&=~(termios.ECHO|termios.ICANON);termios.tcsetattr(0,termios.TCSANOW,a)\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nos.write(1,b'READY')\nwhile True: signal.pause()\n").unwrap();
        let mut process = TerminalProcess::spawn(
            PtySpawn {
                shell: "python3".into(),
                args: vec![script.to_string_lossy().into_owned()],
                cwd: directory.path().into(),
                environment: std::env::vars().collect(),
                cols: 120,
                rows: 30,
            },
            Duration::ZERO,
        )
        .await
        .unwrap();
        let mut output = String::new();
        while !output.contains("READY") {
            match tokio::time::timeout(Duration::from_secs(3), process.events.recv())
                .await
                .unwrap()
                .unwrap()
            {
                PtyEvent::Output(data) => output.push_str(&data),
                event => panic!("{event:?}"),
            }
        }
        let io = process.io.clone();
        let registration = io.0.write_started.clone();
        let pending = tokio::spawn(async move { io.write("x".repeat(65536)).await });
        tokio::time::timeout(Duration::from_secs(3), registration.notified())
            .await
            .unwrap();
        assert!(
            !pending.is_finished(),
            "the registered child does not read its terminal input"
        );
        let pid = process.pid;
        tokio::time::timeout(Duration::from_secs(3), process.close())
            .await
            .unwrap()
            .unwrap();
        assert!(pending.await.unwrap().is_err());
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}
impl Drop for TerminalProcess {
    fn drop(&mut self) {
        self.events.close();
        self.io.0.stop.send_replace(true);
        let _ = self.io.0.termination.signal(true);
        // The worker keeps ownership of its blocking operation and child until
        // cleanup completes even if the caller cancels the awaited close.
    }
}
