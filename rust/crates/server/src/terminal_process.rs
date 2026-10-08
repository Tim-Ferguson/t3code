//! Owned native PTY. Raw reads decode incrementally; queued output is delivered
//! before process exit. Closing stops delivery, terminates and reaps this child,
//! and joins both workers. There is no process-pattern cleanup.
use crate::terminal_utf8::TerminalUtf8Decoder;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::Duration,
};
use tokio::sync::{mpsc, watch};
#[derive(Debug, Clone)]
pub struct PtyExit {
    pub code: i64,
    pub signal: Option<i64>,
}
#[derive(Debug, Clone)]
pub enum PtyEvent {
    Output(String),
    Exited(PtyExit),
    Error(String),
}
pub struct PtySpawn {
    pub shell: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub environment: indexmap::IndexMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}
struct ChildState {
    child: Option<Box<dyn Child + Send + Sync>>,
}
struct ChildControl {
    state: Mutex<ChildState>,
    wake: Condvar,
}
impl ChildControl {
    fn signal(&self, force: bool) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if let Some(child) = state.child.as_mut() {
            #[cfg(unix)]
            {
                let pid = child
                    .process_id()
                    .ok_or_else(|| io::Error::other("PTY child has no PID"))?;
                // The handle remains unreaped under this same mutex; PID identity
                // cannot be reused while a close races with our exit waiter.
                let result = unsafe {
                    libc::kill(
                        pid as i32,
                        if force { libc::SIGKILL } else { libc::SIGTERM },
                    )
                };
                if result != 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                let _ = force;
                child.kill()?;
            }
        }
        self.wake.notify_one();
        Ok(())
    }
}
impl Drop for ChildControl {
    fn drop(&mut self) {
        if let Some(mut child) = self.state.get_mut().unwrap().child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
pub struct OwnedPty {
    pub pid: u32,
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    control: Arc<ChildControl>,
    events: mpsc::Receiver<PtyEvent>,
    workers: Vec<thread::JoinHandle<()>>,
    exited: watch::Receiver<bool>,
}
/// A signal capability tied to an unreaped, owned child. It remains usable
/// while a blocking write holds the PTY, so shutdown can unblock that write.
#[derive(Clone)]
pub struct PtyTermination(Arc<ChildControl>);
impl PtyTermination {
    pub fn signal(&self, force: bool) -> io::Result<()> {
        self.0.signal(force)
    }
}
impl OwnedPty {
    pub fn termination(&self) -> PtyTermination {
        PtyTermination(self.control.clone())
    }
    pub fn take_event_receiver(&mut self) -> mpsc::Receiver<PtyEvent> {
        let (_, empty) = mpsc::channel(1);
        std::mem::replace(&mut self.events, empty)
    }
    /// Call in the blocking pool; PTY allocation and spawn perform system I/O.
    pub fn spawn(mut input: PtySpawn) -> io::Result<Self> {
        // Original NodePtyAdapter supplies name=xterm-256color on Unix and
        // inserts TERM only when absent on Windows (ConPTY ignores name).
        #[cfg(unix)]
        input
            .environment
            .insert("TERM".into(), "xterm-256color".into());
        #[cfg(windows)]
        input
            .environment
            .entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
        let size = PtySize {
            cols: input.cols,
            rows: input.rows,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let mut command = CommandBuilder::new(input.shell);
        command.args(input.args);
        command.cwd(input.cwd);
        command.env_clear();
        for (key, value) in input.environment {
            command.env(key, value);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let control = Arc::new(ChildControl {
            state: Mutex::new(ChildState { child: Some(child) }),
            wake: Condvar::new(),
        });
        let pid = control
            .state
            .lock()
            .unwrap()
            .child
            .as_ref()
            .unwrap()
            .process_id()
            .ok_or_else(|| io::Error::other("PTY child has no PID"))?;
        let (events, receiver) = mpsc::channel(64);
        let (exited, exit_receiver) = watch::channel(false);
        let mut owned = Self {
            pid,
            master: Some(pair.master),
            writer: None,
            control: control.clone(),
            events: receiver,
            workers: vec![],
            exited: exit_receiver,
        };
        // All fallible setup after spawning is guarded by OwnedPty::drop.
        owned.writer = Some(
            owned
                .master
                .as_ref()
                .unwrap()
                .take_writer()
                .map_err(|error| io::Error::other(error.to_string()))?,
        );
        #[cfg(unix)]
        let reader = UnixReader::new(
            owned
                .master
                .as_ref()
                .unwrap()
                .as_raw_fd()
                .ok_or_else(|| io::Error::other("Native PTY has no readable descriptor"))?,
        )?;
        #[cfg(not(unix))]
        let reader = owned
            .master
            .as_ref()
            .unwrap()
            .try_clone_reader()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let (status, final_status) = std::sync::mpsc::channel();
        #[cfg(unix)]
        let (mut status_wake, reader_wake) = std::os::unix::net::UnixStream::pair()?;
        let waiter = thread::Builder::new()
            .name("t3-pty-wait".into())
            .spawn(move || {
                let result = loop {
                    let mut state = control.state.lock().unwrap();
                    let Some(child) = state.child.as_mut() else {
                        break Err(io::Error::other("PTY child was released"));
                    };
                    let result = try_exit(child.as_mut());
                    match result {
                        Ok(Some(exit)) => {
                            state.child.take();
                            break Ok(exit);
                        }
                        Err(error) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            state.child.take();
                            break Err(error);
                        }
                        Ok(None) => {
                            let _ = control
                                .wake
                                .wait_timeout(state, Duration::from_millis(10))
                                .unwrap();
                        }
                    }
                };
                let _ = status.send(result);
                exited.send_replace(true);
                #[cfg(unix)]
                {
                    let _ = status_wake.write_all(&[1]);
                }
            })?;
        owned.workers.push(waiter);
        let reader = thread::Builder::new()
            .name("t3-pty-output".into())
            .spawn(move || {
                let mut decoder = TerminalUtf8Decoder::default();
                #[cfg(unix)]
                let result = read_unix(reader, reader_wake, &mut decoder, &events);
                #[cfg(not(unix))]
                let result = read_portable(reader, &mut decoder, &events);
                if let Err(error) = result {
                    let _ = events.blocking_send(PtyEvent::Error(error.to_string()));
                }
                let tail = decoder.finish();
                if !tail.is_empty() {
                    let _ = events.blocking_send(PtyEvent::Output(tail));
                }
                // The output drain owns exit publication, so no waiter can overtake
                // raw data that was already queued/read when the child exited.
                match final_status.recv() {
                    Ok(Ok(exit)) => {
                        let _ = events.blocking_send(PtyEvent::Exited(exit));
                    }
                    Ok(Err(error)) => {
                        let _ = events.blocking_send(PtyEvent::Error(error.to_string()));
                    }
                    Err(_) => {}
                }
            })?;
        owned.workers.push(reader);
        drop(pair.slave);
        Ok(owned)
    }
    pub async fn next_event(&mut self) -> Option<PtyEvent> {
        self.events.recv().await
    }
    pub fn write(&mut self, data: &str) -> io::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("PTY is closed"))?;
        writer.write_all(data.as_bytes())?;
        writer.flush()
    }
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .as_ref()
            .ok_or_else(|| io::Error::other("PTY is closed"))?
            .resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| io::Error::other(error.to_string()))
    }
    pub async fn close(mut self, grace: Duration) -> io::Result<()> {
        self.events.close();
        self.writer.take();
        let signal = self.control.signal(false);
        if !*self.exited.borrow()
            && tokio::time::timeout(grace, self.exited.changed())
                .await
                .is_err()
        {
            self.control.signal(true)?;
        }
        self.master.take();
        let workers = std::mem::take(&mut self.workers);
        tokio::task::spawn_blocking(move || {
            for worker in workers {
                worker
                    .join()
                    .map_err(|_| io::Error::other("PTY worker panicked"))?;
            }
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(|error| io::Error::other(error.to_string()))??;
        signal
    }
}
impl Drop for OwnedPty {
    fn drop(&mut self) {
        self.events.close();
        self.writer.take();
        let _ = self.control.signal(true);
        self.master.take();
        // A dropped/cancelled close still retains explicit worker ownership;
        // join away from Tokio rather than detaching unreaped process workers.
        let workers = std::mem::take(&mut self.workers);
        if !workers.is_empty() {
            let _ = thread::Builder::new()
                .name("t3-pty-release".into())
                .spawn(move || {
                    for worker in workers {
                        let _ = worker.join();
                    }
                });
        }
    }
}
fn try_exit(child: &mut (dyn Child + Send + Sync)) -> io::Result<Option<PtyExit>> {
    #[cfg(unix)]
    if let Some(child) = child.as_any_mut().downcast_mut::<std::process::Child>() {
        use std::os::unix::process::ExitStatusExt;
        return child.try_wait().map(|status| {
            status.map(|status| PtyExit {
                code: status.code().unwrap_or(0).into(),
                signal: status.signal().map(i64::from),
            })
        });
    }
    child.try_wait().map(|status| {
        status.map(|status| PtyExit {
            code: status.exit_code().into(),
            signal: None,
        })
    })
}
#[cfg(unix)]
struct UnixReader(std::fs::File);
#[cfg(unix)]
impl UnixReader {
    fn new(fd: i32) -> io::Result<Self> {
        use std::os::fd::FromRawFd;
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(unsafe { std::fs::File::from_raw_fd(duplicate) }))
    }
}
#[cfg(unix)]
fn read_unix(
    mut reader: UnixReader,
    wake: std::os::unix::net::UnixStream,
    decoder: &mut TerminalUtf8Decoder,
    events: &mpsc::Sender<PtyEvent>,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut buffer = [0u8; 16 * 1024];
    let mut exiting = false;
    loop {
        let mut fds = [
            libc::pollfd {
                fd: reader.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let count = unsafe { libc::poll(fds.as_mut_ptr(), 2, if exiting { 0 } else { -1 }) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[1].revents != 0 {
            exiting = true;
        }
        if fds[0].revents == 0 {
            if exiting {
                return Ok(());
            }
            continue;
        }
        match reader.0.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(size) => {
                let data = decoder.feed(&buffer[..size]);
                if !data.is_empty() && events.blocking_send(PtyEvent::Output(data)).is_err() {
                    return Ok(());
                }
            }
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}
#[cfg(not(unix))]
fn read_portable(
    mut reader: Box<dyn Read + Send>,
    decoder: &mut TerminalUtf8Decoder,
    events: &mpsc::Sender<PtyEvent>,
) -> io::Result<()> {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let size = reader.read(&mut buffer)?;
        if size == 0 {
            return Ok(());
        }
        let data = decoder.feed(&buffer[..size]);
        if !data.is_empty() && events.blocking_send(PtyEvent::Output(data)).is_err() {
            return Ok(());
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    async fn spawn_script(directory: &std::path::Path, script: &str) -> OwnedPty {
        let path = directory.join("terminal.py");
        std::fs::write(&path, script).unwrap();
        let input = PtySpawn {
            shell: "python3".into(),
            args: vec![path.to_string_lossy().into_owned()],
            cwd: directory.into(),
            environment: std::env::vars()
                .filter(|(key, _)| !key.starts_with("T3CODE_") && !key.starts_with("VITE_"))
                .collect(),
            cols: 120,
            rows: 30,
        };
        tokio::task::spawn_blocking(move || OwnedPty::spawn(input))
            .await
            .unwrap()
            .unwrap()
    }
    async fn next(pty: &mut OwnedPty) -> PtyEvent {
        tokio::time::timeout(Duration::from_secs(3), pty.next_event())
            .await
            .unwrap()
            .unwrap()
    }
    async fn output_until(pty: &mut OwnedPty, needle: &str) -> String {
        let mut output = String::new();
        loop {
            match next(pty).await {
                PtyEvent::Output(data) => {
                    output.push_str(&data);
                    if output.contains(needle) {
                        return output;
                    }
                }
                event => panic!("unexpected event while waiting for {needle}: {event:?}"),
            }
        }
    }
    #[tokio::test]
    async fn actual_pty_byte_fragments_decode_unicode_and_drain_output_before_exit() {
        let directory = tempfile::tempdir().unwrap();
        let mut pty = spawn_script(
            directory.path(),
            r#"import os,sys,termios
attrs=termios.tcgetattr(0)
attrs[3]&=~termios.ECHO
termios.tcsetattr(0,termios.TCSANOW,attrs)
os.write(1,b'prefix:\xf0\x9f')
sys.stdin.readline()
os.write(1,b'\x98\x80\xe4\xb8')
sys.stdin.readline()
os.write(1,b'\xad\xfftail')
"#,
        )
        .await;
        let first = output_until(&mut pty, "prefix:").await;
        assert_eq!(first, "prefix:");
        pty.write("next\n").unwrap();
        let emoji = output_until(&mut pty, "😀").await;
        assert_eq!(emoji, "😀");
        pty.write("next\n").unwrap();
        let mut tail = String::new();
        loop {
            match next(&mut pty).await {
                PtyEvent::Output(data) => tail.push_str(&data),
                PtyEvent::Exited(exit) => {
                    assert_eq!(exit.code, 0);
                    assert_eq!(exit.signal, None);
                    break;
                }
                event => panic!("unexpected terminal event {event:?}"),
            }
        }
        assert_eq!(tail, "中�tail");
        pty.close(Duration::ZERO).await.unwrap();
    }
    #[tokio::test]
    async fn actual_pty_resize_and_input_reach_the_owned_child() {
        let directory = tempfile::tempdir().unwrap();
        let mut pty = spawn_script(
            directory.path(),
            r#"import os,sys,termios,fcntl,struct
attrs=termios.tcgetattr(0)
attrs[3]&=~termios.ECHO
termios.tcsetattr(0,termios.TCSANOW,attrs)
print('READY',flush=True)
line=sys.stdin.readline().strip()
rows,cols,_,_=struct.unpack('HHHH',fcntl.ioctl(0,termios.TIOCGWINSZ,b'\0'*8))
print(f'{line}:{cols}x{rows}',flush=True)
"#,
        )
        .await;
        output_until(&mut pty, "READY").await;
        pty.resize(98, 41).unwrap();
        pty.write("hello\n").unwrap();
        assert!(
            output_until(&mut pty, "hello:98x41")
                .await
                .contains("hello:98x41")
        );
        assert!(matches!(
            next(&mut pty).await,
            PtyEvent::Exited(PtyExit { code: 0, .. })
        ));
        pty.close(Duration::ZERO).await.unwrap();
    }
    #[tokio::test]
    async fn close_escalates_ignored_term_and_joins_reader_and_reaped_child() {
        let directory = tempfile::tempdir().unwrap();
        let mut pty=spawn_script(directory.path(),"import signal,sys\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\nprint('READY',flush=True)\nwhile True: sys.stdin.readline()\n").await;
        output_until(&mut pty, "READY").await;
        let pid = pty.pid;
        let weak = Arc::downgrade(&pty.control);
        tokio::time::timeout(Duration::from_secs(3), pty.close(Duration::ZERO))
            .await
            .unwrap()
            .unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    #[tokio::test]
    async fn dropping_pty_terminates_and_reaps_child_even_without_explicit_close() {
        let directory = tempfile::tempdir().unwrap();
        let mut pty = spawn_script(
            directory.path(),
            "import sys\nprint('READY',flush=True)\nwhile True: sys.stdin.readline()\n",
        )
        .await;
        output_until(&mut pty, "READY").await;
        let pid = pty.pid;
        let mut exited = pty.exited.clone();
        drop(pty);
        if !*exited.borrow() {
            tokio::time::timeout(Duration::from_secs(3), exited.changed())
                .await
                .unwrap()
                .unwrap();
        }
        assert!(*exited.borrow());
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}
