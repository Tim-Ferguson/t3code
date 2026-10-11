//! Starting the T3 Code server as this app's child (`DesktopBackendConfiguration.ts`, `DesktopBackendManager.ts`):
//! where the app keeps its files, which port, the bootstrap credential handed over a descriptor that only the
//! child inherits, and the process itself.
use crate::supervisor::{Exit, Launched, Launcher, Process, Secret};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// The port the original's desktop app starts looking at (`DEFAULT_DESKTOP_BACKEND_PORT`).
pub const FIRST_PORT: u16 = 3773;
/// The descriptor on which the server reads its bootstrap envelope (`--bootstrap-fd 3`, as the original).
pub const BOOTSTRAP_FD: i32 = 3;
/// The descriptor of the socket the server talks to this app over (`--desktop-telemetry-fd 4 --desktop-telemetry-control-fd 4`: the
/// server accepts one socket for both directions). See `control_channel`.
pub const CONTROL_FD: i32 = 4;

/// Where this app keeps what it writes. All of it lives under one directory, so that a test or a second profile
/// is a different directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppDirs {
    pub root: PathBuf,
    /// The window toolkit's browser profile (storage, caches).
    pub webview: PathBuf,
    /// The server's `--state-dir`.
    pub server_state: PathBuf,
    pub window_state: PathBuf,
    pub server_log: PathBuf,
    /// The running server's process id, for whoever needs to see or end this app's server by the id it was
    /// started with (a test, a person at a terminal). Removed when the server ends.
    pub server_pid: PathBuf,
}

impl AppDirs {
    /// `T3_UI_DATA_DIR` names the directory, else the platform's application data directory. `T3_DESKTOP_STATE_DIR`
    /// moves only the server's state.
    pub fn resolve(env: impl Fn(&str) -> Option<String>, platform: &str) -> Result<Self, String> {
        let nonempty = |name: &str| env(name).filter(|value| !value.trim().is_empty());
        let root = match nonempty("T3_UI_DATA_DIR") {
            Some(path) => PathBuf::from(path),
            None => {
                let home = nonempty("HOME").map(PathBuf::from);
                match platform {
                    "macos" => home.ok_or("HOME is not set")?.join("Library/Application Support/T3 Code (Rust)"),
                    "windows" => PathBuf::from(nonempty("APPDATA").ok_or("APPDATA is not set")?).join("T3 Code (Rust)"),
                    _ => nonempty("XDG_DATA_HOME").map(PathBuf::from).or_else(|| home.map(|home| home.join(".local/share"))).ok_or("HOME is not set")?.join("t3code-rust"),
                }
            }
        };
        let server_state = nonempty("T3_DESKTOP_STATE_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("server"));
        Ok(Self { webview: root.join("webview"), window_state: root.join("window-state.json"), server_log: root.join("logs").join("server.log"), server_pid: root.join("server.pid"), server_state, root })
    }

    /// The server's state must never be the original app's live data (`~/.t3`), whatever a variable says.
    pub fn check_state_is_not_live_data(&self, home: Option<&Path>) -> Result<(), String> {
        let Some(home) = home else { return Ok(()) };
        let live = home.join(".t3");
        let resolved = |path: &Path| {
            // Through symlinks as far as the path exists, so that a link into the live directory is seen.
            let mut existing = path;
            let mut rest = Vec::new();
            while !existing.exists() {
                match (existing.file_name(), existing.parent()) {
                    (Some(name), Some(parent)) => {
                        rest.push(name.to_owned());
                        existing = parent;
                    }
                    _ => break,
                }
            }
            let mut resolved = existing.canonicalize().unwrap_or_else(|_| existing.to_owned());
            resolved.extend(rest.iter().rev());
            resolved
        };
        let (state, live_resolved) = (resolved(&self.server_state), resolved(&live));
        if state.starts_with(&live) || state.starts_with(&live_resolved) {
            return Err(format!("The server's state directory {} is inside {}, the original app's live data. Choose another with T3_DESKTOP_STATE_DIR.", self.server_state.display(), live.display()));
        }
        Ok(())
    }
}

/// The first port from `first` that `can_listen` accepts.
pub fn find_port(first: u16, can_listen: impl Fn(u16) -> bool) -> Option<u16> {
    (first..=u16::MAX).find(|port| can_listen(*port))
}

pub fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// A bootstrap credential: 128 random bits, valid for this start of the server only.
pub fn new_credential() -> String {
    rand::random::<[u8; 16]>().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// How to start the server.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub binary: PathBuf,
    pub state_dir: PathBuf,
    pub log_file: PathBuf,
    pub pid_file: Option<PathBuf>,
    pub host: String,
    pub port: u16,
    /// The login shell's environment for the server (`shell_env`), asked for once and reused for every start;
    /// None starts the server with the environment this app has.
    pub login_shell: Option<std::sync::Arc<crate::shell_env::Cached>>,
}

impl ServerConfig {
    pub fn address(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    pub fn args(&self) -> Vec<String> {
        ["serve", "--state-dir", &self.state_dir.to_string_lossy(), "--host", &self.host, "--port", &self.port.to_string(), "--mode", "desktop", "--bootstrap-fd", &BOOTSTRAP_FD.to_string()].map(str::to_owned).into()
    }

    /// The one line the server reads from its bootstrap descriptor (`DesktopBackendBootstrap`). Provider
    /// credentials, telemetry and Tailscale are not part of it: this app asks for none of them.
    pub fn bootstrap_line(&self, credential: &str) -> String {
        let mut line = serde_json::json!({
            "mode": "desktop",
            "noBrowser": true,
            "port": self.port,
            "host": self.host,
            "desktopBootstrapToken": credential,
            "tailscaleServeEnabled": false,
            "tailscaleServePort": 443,
        })
        .to_string();
        line.push('\n');
        line
    }
}

/// Finds the server program: the one named by `T3_SERVER_BIN`, else `t3-server` beside this program (where the
/// application bundle puts it).
pub fn find_server_binary(env: impl Fn(&str) -> Option<String>, current_exe: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = env("T3_SERVER_BIN").filter(|path| !path.trim().is_empty()) {
        let path = PathBuf::from(path);
        return if path.is_file() { Ok(path) } else { Err(format!("T3_SERVER_BIN names {}, which is not a file.", path.display())) };
    }
    let name = if cfg!(windows) { "t3-server.exe" } else { "t3-server" };
    let beside = current_exe.and_then(Path::parent).map(|directory| directory.join(name));
    match beside {
        Some(path) if path.is_file() => Ok(path),
        Some(path) => Err(format!("The T3 Code server program was not found at {}. Put it there, or set T3_SERVER_BIN.", path.display())),
        None => Err("The T3 Code server program was not found. Set T3_SERVER_BIN.".into()),
    }
}

/// What the server said about itself on its first line of standard output (`announce_ready` in the server):
/// `T3_SERVER_READY {"pid":N,"host":"127.0.0.1","port":P,"origin":"http://127.0.0.1:P"}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyLine {
    pub pid: u32,
    pub port: u16,
    /// An address a client on this computer can use.
    pub origin: String,
}

/// The ready line in one line of the server's output, when that is what it is.
pub fn parse_ready_line(line: &str) -> Option<ReadyLine> {
    let json = line.trim_end().strip_prefix("T3_SERVER_READY ")?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(ReadyLine { pid: u32::try_from(value["pid"].as_u64()?).ok()?, port: u16::try_from(value["port"].as_u64()?).ok()?, origin: value["origin"].as_str()?.to_owned() })
}

/// How long a start waits for the ready line before it also asks the server over HTTP. The original's server prints
/// no such line, and a server that does not is as welcome as one that does.
pub const READY_LINE_GRACE: Duration = Duration::from_secs(3);

/// Copies the server's standard output to the log, line by line, and reports the ready line when it passes. Ends at
/// the end of the output, which is when the server ends.
async fn forward_output(stdout: tokio::process::ChildStdout, log: std::fs::File, ready: tokio::sync::oneshot::Sender<ReadyLine>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut log = tokio::fs::File::from_std(log);
    let mut ready = Some(ready);
    let mut lines = tokio::io::BufReader::new(stdout).split(b'\n');
    while let Ok(Some(mut line)) = lines.next_segment().await {
        if ready.is_some() {
            if let Some(found) = parse_ready_line(&String::from_utf8_lossy(&line)) {
                if let Some(sender) = ready.take() {
                    let _ = sender.send(found);
                }
            }
        }
        line.push(b'\n');
        if log.write_all(&line).await.is_err() {
            // The log is gone; keep reading so the server never blocks on a full pipe.
            continue;
        }
        let _ = log.flush().await;
    }
}

/// The server as a child of this process. Only the id captured at spawn is ever signalled.
pub struct ChildProcess {
    child: tokio::process::Child,
    pid: Option<u32>,
    pid_file: Option<PathBuf>,
    /// Copies its output to the log; finished when the output ends.
    output: Option<tokio::task::JoinHandle<()>>,
}

impl Process for ChildProcess {
    async fn wait(&mut self) -> Exit {
        let result = self.child.wait().await;
        // What it printed last is in the log before this returns (a program it started that kept the pipe open is not waited for).
        if let Some(output) = self.output.as_mut() {
            let _ = tokio::time::timeout(Duration::from_secs(2), output).await;
            self.output = None;
        }
        if let Some(file) = self.pid_file.take() {
            let _ = std::fs::remove_file(file);
        }
        match result {
            Ok(status) => Exit(describe(status)),
            Err(error) => Exit(format!("waiting for it failed: {error}")),
        }
    }
    fn terminate(&mut self) {
        // The server finishes its work on a terminate signal (and on an interrupt).
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            return;
        }
        let _ = self.child.start_kill();
    }
    fn kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn describe(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "no exit code".into(),
    }
}

/// Starts the real server and asks it whether it answers.
pub struct ServerLauncher {
    config: ServerConfig,
    log_start: u64,
    /// The ready line of the server the last `launch` started.
    ready: Option<tokio::sync::oneshot::Receiver<ReadyLine>>,
    /// How long `ready` waits for the line before it asks over HTTP as well.
    ready_line_grace: Duration,
    /// What the server's requests over the control channel are answered with; none starts the server without the channel.
    answers: Option<crate::control_channel::Answers>,
}

impl ServerLauncher {
    pub fn new(config: ServerConfig) -> Self {
        Self { config, log_start: 0, ready: None, ready_line_grace: READY_LINE_GRACE, answers: None }
    }

    /// Starts the server with the control channel, and answers what it asks over it as `answers` say.
    pub fn with_control_channel(mut self, answers: crate::control_channel::Answers) -> Self {
        self.answers = Some(answers);
        self
    }

    /// Changes how long a start waits for the ready line before it also polls (a test of the polling).
    pub fn with_ready_line_grace(mut self, grace: Duration) -> Self {
        self.ready_line_grace = grace;
        self
    }
}

/// The last non-empty line of `path` after `offset` bytes, trimmed of terminal colour codes.
pub fn last_line_since(path: &Path, offset: u64) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(bytes.get(offset as usize..)?).into_owned();
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let mut plain = String::new();
    let mut in_escape = false;
    for character in line.chars() {
        match (in_escape, character) {
            (false, '\u{1b}') => in_escape = true,
            (true, 'm') => in_escape = false,
            (true, _) => {}
            (false, _) => plain.push(character),
        }
    }
    Some(plain.trim().chars().take(300).collect())
}

impl Launcher for ServerLauncher {
    type Proc = ChildProcess;

    fn launch(&mut self, _attempt: u32) -> Result<Launched<ChildProcess>, String> {
        let credential = new_credential();
        std::fs::create_dir_all(&self.config.state_dir).map_err(|error| format!("Cannot create {}: {error}", self.config.state_dir.display()))?;
        if let Some(directory) = self.config.log_file.parent() {
            std::fs::create_dir_all(directory).map_err(|error| format!("Cannot create {}: {error}", directory.display()))?;
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&self.config.log_file).map_err(|error| format!("Cannot open {}: {error}", self.config.log_file.display()))?;
        self.log_start = log.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let mut command = tokio::process::Command::new(&self.config.binary);
        // Standard output is read for the ready line and copied to the log; standard error is the same pipe (below), so
        // the log keeps the order in which the server wrote.
        command.args(self.config.args()).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(false);
        // Provider tools are found the way the person's own shell finds them, not through the server's
        // settings of an earlier run.
        command.env_remove("T3CODE_HOME").env_remove("T3CODE_PORT").env_remove("T3CODE_HOST").env_remove("T3CODE_MODE").env_remove("T3CODE_BOOTSTRAP_FD");
        // A program started from the Finder has launchd's bare environment: the server gets what the person's login shell has.
        if let Some(login_shell) = &self.config.login_shell {
            command.envs(login_shell.get());
        }
        let line = self.config.bootstrap_line(&credential);
        #[cfg(unix)]
        {
            use std::io::Write;
            let (reader, mut writer) = std::io::pipe().map_err(|error| format!("Cannot make the bootstrap channel: {error}"))?;
            writer.write_all(line.as_bytes()).map_err(|error| format!("Cannot write the bootstrap envelope: {error}"))?;
            drop(writer);
            let descriptor = std::os::fd::AsRawFd::as_raw_fd(&reader);
            // The control channel: a socket whose child end becomes descriptor 4. Both ends are first moved above the low
            // numbers the child's descriptors are placed on, so placing one cannot overwrite the other.
            let channel = match &self.answers {
                Some(_) => {
                    let (app_end, child_end) = std::os::unix::net::UnixStream::pair().map_err(|error| format!("Cannot make the control channel: {error}"))?;
                    let high = |end: std::os::unix::net::UnixStream| -> Result<std::os::fd::OwnedFd, String> {
                        let moved = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&end), libc::F_DUPFD_CLOEXEC, 10) };
                        if moved < 0 {
                            return Err(format!("Cannot make the control channel: {}", std::io::Error::last_os_error()));
                        }
                        Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(moved) })
                    };
                    Some((high(app_end)?, high(child_end)?))
                }
                None => None,
            };
            let channel_child = channel.as_ref().map(|(_, child)| std::os::fd::AsRawFd::as_raw_fd(child));
            if channel.is_some() {
                command.args(["--desktop-telemetry-fd", &CONTROL_FD.to_string(), "--desktop-telemetry-control-fd", &CONTROL_FD.to_string()]);
            }
            // After the fork and before the exec: the read end becomes descriptor 3 (which clears close-on-exec).
            // Everything else the pipe created stays close-on-exec.
            unsafe {
                command.pre_exec(move || {
                    // Standard error joins standard output (already set up by now).
                    if libc::dup2(1, 2) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if descriptor == BOOTSTRAP_FD {
                        // Already in place; dup2 would leave close-on-exec set.
                        let flags = libc::fcntl(descriptor, libc::F_GETFD);
                        if flags < 0 || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    } else if libc::dup2(descriptor, BOOTSTRAP_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some(child_end) = channel_child {
                        if libc::dup2(child_end, CONTROL_FD) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().map_err(|error| format!("Cannot start {}: {error}", self.config.binary.display()))?;
            drop(reader);
            if let (Some((app_end, child_end)), Some(answers)) = (channel, self.answers.clone()) {
                // The server holds its end now; this app keeps its own and serves the channel until the server closes it.
                drop(child_end);
                let app_end: std::os::unix::net::UnixStream = app_end.into();
                app_end.set_nonblocking(true).map_err(|error| format!("Cannot use the control channel: {error}"))?;
                let stream = tokio::net::UnixStream::from_std(app_end).map_err(|error| format!("Cannot use the control channel: {error}"))?;
                tokio::spawn(crate::control_channel::serve(stream, answers));
            }
            let pid = child.id();
            if let (Some(file), Some(pid)) = (&self.config.pid_file, pid) {
                let _ = std::fs::write(file, pid.to_string());
            }
            let (announced, ready) = tokio::sync::oneshot::channel();
            self.ready = Some(ready);
            let output = child.stdout.take().map(|stdout| tokio::spawn(forward_output(stdout, log, announced)));
            Ok(Launched { process: ChildProcess { child, pid, pid_file: self.config.pid_file.clone(), output }, address: self.config.address(), credential: Secret(credential) })
        }
        #[cfg(not(unix))]
        {
            let _ = line;
            Err("Starting the server needs an inherited descriptor for its bootstrap credential, which the server does not support on this platform yet.".into())
        }
    }

    /// The server's ready line says it is bound and every service runs. A server that prints none is asked over HTTP once
    /// `ready_line_grace` has passed, until it answers its descriptor; the supervisor bounds the whole wait.
    async fn ready(&mut self, address: &str) -> Option<String> {
        let line = self.ready.take();
        let by_line = async {
            match line {
                Some(line) => line.await.ok(),
                None => None,
            }
        };
        let grace = self.ready_line_grace;
        let by_asking = async {
            let Some(authority) = address.strip_prefix("http://") else { return std::future::pending().await };
            tokio::time::sleep(grace).await;
            loop {
                if answers(authority).await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        tokio::select! {
            Some(found) = by_line => Some(found.origin).filter(|origin| origin.as_str() != address),
            () = by_asking => None,
        }
    }

    fn last_words(&mut self) -> Option<String> {
        last_line_since(&self.config.log_file, self.log_start)
    }
}

/// Whether the server at `authority` (`host:port`) answers its environment descriptor.
pub async fn answers(authority: &str) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(Ok(mut stream)) = tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(authority)).await else { return false };
    let request = format!("GET /.well-known/t3/environment HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    matches!(tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut head)).await, Ok(Ok(_))) && &head == b"HTTP/1.1 200"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
    }

    #[test]
    fn the_app_keeps_everything_under_one_directory_that_a_variable_can_move() {
        let dirs = AppDirs::resolve(env(&[("HOME", "/Users/me")]), "macos").unwrap();
        assert_eq!(dirs.root, PathBuf::from("/Users/me/Library/Application Support/T3 Code (Rust)"));
        assert_eq!(dirs.server_state, dirs.root.join("server"));
        assert_eq!(dirs.webview, dirs.root.join("webview"));
        assert_eq!(dirs.window_state, dirs.root.join("window-state.json"));
        assert_eq!(dirs.server_log, dirs.root.join("logs/server.log"));
        let moved = AppDirs::resolve(env(&[("HOME", "/Users/me"), ("T3_UI_DATA_DIR", "/tmp/t3code-rust-port-test-desktop-1")]), "macos").unwrap();
        assert_eq!(moved.server_state, PathBuf::from("/tmp/t3code-rust-port-test-desktop-1/server"));
        let state_only = AppDirs::resolve(env(&[("HOME", "/Users/me"), ("T3_DESKTOP_STATE_DIR", "/tmp/elsewhere")]), "macos").unwrap();
        assert_eq!((state_only.server_state, state_only.root), (PathBuf::from("/tmp/elsewhere"), dirs.root.clone()));
        // An empty variable is not set.
        assert_eq!(AppDirs::resolve(env(&[("HOME", "/Users/me"), ("T3_UI_DATA_DIR", "  ")]), "macos").unwrap(), dirs);
        assert_eq!(AppDirs::resolve(env(&[("HOME", "/home/me")]), "linux").unwrap().root, PathBuf::from("/home/me/.local/share/t3code-rust"));
        assert_eq!(AppDirs::resolve(env(&[("XDG_DATA_HOME", "/data")]), "linux").unwrap().root, PathBuf::from("/data/t3code-rust"));
        assert_eq!(AppDirs::resolve(env(&[("APPDATA", "C:\\Users\\me\\AppData\\Roaming")]), "windows").unwrap().root, PathBuf::from("C:\\Users\\me\\AppData\\Roaming").join("T3 Code (Rust)"));
        assert!(AppDirs::resolve(env(&[]), "macos").is_err());
    }

    #[test]
    fn the_originals_live_data_directory_is_refused_however_it_is_reached() {
        let home = tempfile::tempdir().unwrap();
        let live = home.path().join(".t3");
        std::fs::create_dir_all(live.join("userdata")).unwrap();
        let dirs = |state: &Path| AppDirs { root: PathBuf::from("/nowhere"), webview: PathBuf::new(), server_state: state.to_owned(), window_state: PathBuf::new(), server_log: PathBuf::new(), server_pid: PathBuf::new() };
        for refused in [live.clone(), live.join("userdata"), live.join("not-yet/created")] {
            let error = dirs(&refused).check_state_is_not_live_data(Some(home.path())).unwrap_err();
            assert!(error.contains("live data"), "{error}");
        }
        // Through a link that points into it.
        #[cfg(unix)]
        {
            let link = home.path().join("innocent");
            std::os::unix::fs::symlink(&live, &link).unwrap();
            assert!(dirs(&link.join("server")).check_state_is_not_live_data(Some(home.path())).is_err());
        }
        // Beside it, and under the app's own directory, is fine; so is `.t3x`, which only looks similar.
        for allowed in [home.path().join(".t3x"), home.path().join("Library/server"), PathBuf::from("/tmp/t3code-rust-port-test-desktop-2/server")] {
            assert_eq!(dirs(&allowed).check_state_is_not_live_data(Some(home.path())), Ok(()), "{allowed:?}");
        }
    }

    #[test]
    fn the_server_is_started_in_desktop_mode_on_loopback_with_the_credential_on_the_descriptor_not_in_the_arguments() {
        let config = ServerConfig { binary: "/app/t3-server".into(), state_dir: "/data/server".into(), log_file: "/data/logs/server.log".into(), pid_file: None, host: "127.0.0.1".into(), port: 3775, login_shell: None };
        assert_eq!(config.address(), "http://127.0.0.1:3775");
        assert_eq!(config.args(), ["serve", "--state-dir", "/data/server", "--host", "127.0.0.1", "--port", "3775", "--mode", "desktop", "--bootstrap-fd", "3"]);
        let credential = new_credential();
        assert_eq!(credential.len(), 32);
        assert!(credential.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(credential, new_credential());
        let line = config.bootstrap_line(&credential);
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1, "one line");
        let envelope: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(envelope["mode"], "desktop");
        assert_eq!(envelope["port"], 3775);
        assert_eq!(envelope["host"], "127.0.0.1");
        assert_eq!(envelope["desktopBootstrapToken"], credential.as_str());
        assert_eq!(envelope["noBrowser"], true);
        assert_eq!(envelope["tailscaleServeEnabled"], false);
        assert!(!config.args().iter().any(|argument| argument.contains(&credential)));
        assert!(!format!("{:?}", Secret(credential)).contains("0"), "a credential does not print itself");
    }

    #[test]
    fn the_first_free_port_from_the_start_is_taken_and_none_is_found_when_none_is() {
        assert_eq!(find_port(3773, |port| port >= 3775), Some(3775));
        assert_eq!(find_port(3773, |_| true), Some(3773));
        assert_eq!(find_port(65_534, |_| false), None);
        assert_eq!(find_port(65_535, |port| port == 65_535), Some(65_535));
    }

    #[test]
    fn the_server_program_is_the_named_one_or_the_one_beside_this_program() {
        let directory = tempfile::tempdir().unwrap();
        let beside = directory.path().join(if cfg!(windows) { "t3-server.exe" } else { "t3-server" });
        let exe = directory.path().join("t3-ui");
        assert!(find_server_binary(env(&[]), Some(&exe)).unwrap_err().contains("not found"));
        std::fs::write(&beside, b"").unwrap();
        assert_eq!(find_server_binary(env(&[]), Some(&exe)).unwrap(), beside);
        let named = directory.path().join("other-server");
        assert!(find_server_binary(env(&[("T3_SERVER_BIN", named.to_str().unwrap())]), Some(&exe)).unwrap_err().contains("not a file"));
        std::fs::write(&named, b"").unwrap();
        assert_eq!(find_server_binary(env(&[("T3_SERVER_BIN", named.to_str().unwrap())]), Some(&exe)).unwrap(), named);
        assert!(find_server_binary(env(&[]), None).is_err());
    }

    #[test]
    fn a_failure_quotes_the_last_thing_the_server_said_during_this_start_only() {
        let directory = tempfile::tempdir().unwrap();
        let log = directory.path().join("server.log");
        std::fs::write(&log, "old line from an earlier start\n").unwrap();
        let offset = std::fs::metadata(&log).unwrap().len();
        assert_eq!(last_line_since(&log, offset), None);
        std::io::Write::write_all(&mut std::fs::OpenOptions::new().append(true).open(&log).unwrap(), b"starting\n\x1b[31mError:\x1b[0m address in use\n\n").unwrap();
        assert_eq!(last_line_since(&log, offset), Some("Error: address in use".into()));
        assert_eq!(last_line_since(&directory.path().join("missing.log"), 0), None);
    }
    #[cfg(unix)]
    fn stand_in(directory: &Path, body: &str) -> ServerConfig {
        use std::os::unix::fs::PermissionsExt;
        let script = directory.join("t3-server");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        ServerConfig { binary: script, state_dir: directory.join("state"), log_file: directory.join("logs/server.log"), pid_file: Some(directory.join("server.pid")), host: "127.0.0.1".into(), port: 3779, login_shell: None }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_real_child_gets_the_envelope_on_descriptor_3_and_its_output_goes_to_the_log() {
        let directory = tempfile::tempdir().unwrap();
        // A stand-in server: logs what it was given on descriptor 3 and how it was started, then ends with 7.
        let config = stand_in(directory.path(), "read line <&3\necho \"envelope: $line\"\necho \"args: $*\"\necho \"to stderr\" >&2\nexit 7");
        let mut launcher = ServerLauncher::new(config.clone());
        let Launched { mut process, credential, address } = launcher.launch(1).unwrap();
        assert_eq!(address, "http://127.0.0.1:3779");
        let pid_file = config.pid_file.clone().unwrap();
        assert!(std::fs::read_to_string(&pid_file).unwrap().trim().parse::<u32>().is_ok(), "the id it was started with is written");
        assert_eq!(process.wait().await, Exit("exit code 7".into()));
        assert!(!pid_file.exists(), "and removed when it ends");
        let log = std::fs::read_to_string(&config.log_file).unwrap();
        assert!(log.contains(&format!("\"desktopBootstrapToken\":\"{}\"", credential.0)), "{log}");
        assert!(log.contains("args: serve --state-dir"), "{log}");
        assert!(log.contains("to stderr"), "standard error is logged too: {log}");
        // The next start appends, and its failure quotes only its own last line.
        let mut second = ServerLauncher::new(config);
        let Launched { mut process, .. } = second.launch(2).unwrap();
        process.wait().await;
        assert_eq!(second.last_words(), Some("to stderr".into()));
        assert!(std::fs::read_to_string(&second.config.log_file).unwrap().matches("envelope:").count() == 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_server_is_started_with_the_login_shells_path_asked_for_once() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        // A login shell at a temporary path, never the person's own: it adds a directory of provider tools to PATH.
        let shell = directory.path().join("fake-login-shell");
        let asked = directory.path().join("asked");
        std::fs::write(&shell, format!("#!/bin/sh\necho asked >> '{}'\nPATH='/only/the/shell/knows:/usr/bin:/bin'; export PATH\nshift\nexec /bin/sh -c \"$1\"\n", asked.display())).unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut shell_config = crate::shell_env::Config::for_process();
        shell_config.env = [("SHELL".to_owned(), shell.to_string_lossy().into_owned()), ("PATH".to_owned(), "/usr/bin:/bin".to_owned())].into();
        shell_config.fallback_shell = None;
        let mut config = stand_in(directory.path(), "echo \"path: $PATH\"");
        config.login_shell = Some(std::sync::Arc::new(crate::shell_env::Cached::new(shell_config)));
        let mut launcher = ServerLauncher::new(config.clone());
        for attempt in 1..=2 {
            let Launched { mut process, .. } = launcher.launch(attempt).unwrap();
            process.wait().await;
        }
        let log = std::fs::read_to_string(&config.log_file).unwrap();
        assert_eq!(log.matches("path: /only/the/shell/knows:").count(), 2, "{log}");
        assert_eq!(std::fs::read_to_string(&asked).unwrap().lines().count(), 1, "the shell was asked once for two starts");
    }

    #[test]
    fn the_ready_line_is_the_servers_announcement_and_nothing_else_is() {
        let line = r#"T3_SERVER_READY {"pid":4242,"host":"0.0.0.0","port":3775,"origin":"http://127.0.0.1:3775"}"#;
        assert_eq!(parse_ready_line(line), Some(ReadyLine { pid: 4242, port: 3775, origin: "http://127.0.0.1:3775".into() }));
        assert_eq!(parse_ready_line(&format!("{line}\r\n")), parse_ready_line(line));
        for other in ["", "listening on 127.0.0.1:3775", "T3_SERVER_READY", "T3_SERVER_READY not json", r#"T3_SERVER_READY {"pid":1,"port":70000,"origin":"x"}"#, r#"T3_SERVER_READY {"pid":1,"port":1}"#, r#" T3_SERVER_READY {"pid":1,"port":1,"origin":"x"}"#] {
            assert_eq!(parse_ready_line(other), None, "{other:?}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_prints_its_ready_line_is_ready_at_once_and_the_line_reaches_the_log() {
        let directory = tempfile::tempdir().unwrap();
        // Nothing listens on the server's port: only the line can say it is ready.
        let config = stand_in(directory.path(), "read line <&3\necho 'T3_SERVER_READY {\"pid\":1,\"host\":\"127.0.0.1\",\"port\":3779,\"origin\":\"http://127.0.0.1:3779\"}'\nexec sleep 30");
        let mut launcher = ServerLauncher::new(config.clone());
        let Launched { mut process, address, .. } = launcher.launch(1).unwrap();
        assert_eq!(launcher.ready(&address).await, None, "the address it was started for is the one it names");
        process.kill();
        process.wait().await;
        assert!(std::fs::read_to_string(&config.log_file).unwrap().contains("T3_SERVER_READY"), "standard output is still logged");
        assert!(!config.pid_file.unwrap().exists());
        // A server that names another address is ready there.
        let config = stand_in(directory.path(), "echo 'T3_SERVER_READY {\"pid\":1,\"host\":\"127.0.0.1\",\"port\":4001,\"origin\":\"http://127.0.0.1:4001\"}'\nexec sleep 30");
        let mut launcher = ServerLauncher::new(config);
        let Launched { mut process, address, .. } = launcher.launch(1).unwrap();
        assert_eq!(launcher.ready(&address).await, Some("http://127.0.0.1:4001".into()));
        process.kill();
        process.wait().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_without_a_ready_line_is_asked_over_http_once_the_grace_is_over() {
        use tokio::io::AsyncWriteExt;
        let directory = tempfile::tempdir().unwrap();
        // A server of the original's kind: says nothing, and answers its descriptor.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let answering = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
            }
        });
        let mut config = stand_in(directory.path(), "echo starting\nexec sleep 30");
        config.port = port;
        let mut launcher = ServerLauncher::new(config.clone()).with_ready_line_grace(Duration::ZERO);
        let Launched { mut process, address, .. } = launcher.launch(1).unwrap();
        assert_eq!(launcher.ready(&address).await, None);
        process.kill();
        process.wait().await;
        answering.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_ends_without_a_ready_line_does_not_leave_the_wait_ended() {
        let directory = tempfile::tempdir().unwrap();
        let mut launcher = ServerLauncher::new(stand_in(directory.path(), "exit 3"));
        let Launched { mut process, address, .. } = launcher.launch(1).unwrap();
        assert_eq!(process.wait().await, Exit("exit code 3".into()));
        // The line never came and never will; with a long grace the wait does not pretend otherwise.
        let waited = tokio::time::timeout(Duration::from_millis(200), launcher.ready(&address)).await;
        assert!(waited.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_will_not_stop_can_be_killed_by_its_captured_id() {
        let directory = tempfile::tempdir().unwrap();
        let config = stand_in(directory.path(), "exec sleep 30");
        let Launched { mut process, .. } = ServerLauncher::new(config).launch(1).unwrap();
        process.kill();
        assert_eq!(process.wait().await, Exit("signal 9".into()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_program_that_cannot_be_started_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = stand_in(directory.path(), "exit 0");
        config.binary = directory.path().join("missing");
        let error = ServerLauncher::new(config).launch(1).err().unwrap();
        assert!(error.contains("Cannot start"), "{error}");
    }
}

#[cfg(all(test, unix))]
mod control_channel_tests {
    use super::*;
    use crate::updates::{self, Channel, RuntimeInfo};

    /// A stand-in server that says what it read on descriptor 4: the app's hello, then the answer to an update request it makes.
    #[tokio::test]
    async fn the_server_is_started_with_the_control_channel_on_descriptor_4_and_its_update_request_is_answered() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-desktop-channel-").tempdir().unwrap();
        let script = directory.path().join("t3-server");
        let body = "read line <&3\nread hello <&4\necho \"hello: $hello\"\nprintf '{\"version\":1,\"type\":\"requestDesktopUpdate\",\"requestId\":\"r1\"}\\n' >&4\nread answer <&4\necho \"answer: $answer\"\necho \"args: $*\"\nexit 0";
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = ServerConfig { binary: script, state_dir: directory.path().join("state"), log_file: directory.path().join("logs/server.log"), pid_file: None, host: "127.0.0.1".into(), port: 3781, login_shell: None };
        let runtime = RuntimeInfo { host_arch: "arm64".into(), app_arch: "arm64".into(), running_under_arm64_translation: false };
        let reason = updates::disabled_reason(&updates::Environment { has_update_feed_config: false, is_development: false, is_packaged: true, disabled_by_env: false }).map(str::to_owned);
        let answers = crate::control_channel::Answers { state: updates::unavailable("1.0.0", &runtime, Channel::Latest), reason };
        let mut launcher = ServerLauncher::new(config.clone()).with_control_channel(answers);
        let Launched { mut process, .. } = launcher.launch(1).unwrap();
        assert_eq!(process.wait().await, Exit("exit code 0".into()));
        let log = std::fs::read_to_string(&config.log_file).unwrap();
        assert!(log.contains("\"type\":\"desktopTelemetryHello\"") && log.contains("hello: "), "{log}");
        assert!(log.contains("\"type\":\"desktopUpdateStatus\"") && log.contains("\"requestId\":\"r1\"") && log.contains("\"outcome\":\"failed\"") && log.contains("no update feed is configured"), "{log}");
        assert!(log.contains("--desktop-telemetry-fd 4 --desktop-telemetry-control-fd 4"), "{log}");
    }
}
