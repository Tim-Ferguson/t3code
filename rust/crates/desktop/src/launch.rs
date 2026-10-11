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

/// The server as a child of this process. Only the id captured at spawn is ever signalled.
pub struct ChildProcess {
    child: tokio::process::Child,
    pid: Option<u32>,
    pid_file: Option<PathBuf>,
}

impl Process for ChildProcess {
    async fn wait(&mut self) -> Exit {
        let result = self.child.wait().await;
        if let Some(file) = self.pid_file.take() {
            let _ = std::fs::remove_file(file);
        }
        match result {
            Ok(status) => Exit(describe(status)),
            Err(error) => Exit(format!("waiting for it failed: {error}")),
        }
    }
    fn terminate(&mut self) {
        // The server finishes its work on an interrupt; it does not handle the terminate signal.
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            unsafe { libc::kill(pid as i32, libc::SIGINT) };
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
}

impl ServerLauncher {
    pub fn new(config: ServerConfig) -> Self {
        Self { config, log_start: 0 }
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
        let log_for_errors = log.try_clone().map_err(|error| error.to_string())?;
        let mut command = tokio::process::Command::new(&self.config.binary);
        command.args(self.config.args()).stdin(std::process::Stdio::null()).stdout(log).stderr(log_for_errors).kill_on_drop(false);
        // Provider tools are found the way the person's own shell finds them, not through the server's
        // settings of an earlier run.
        command.env_remove("T3CODE_HOME").env_remove("T3CODE_PORT").env_remove("T3CODE_HOST").env_remove("T3CODE_MODE").env_remove("T3CODE_BOOTSTRAP_FD");
        let line = self.config.bootstrap_line(&credential);
        #[cfg(unix)]
        {
            use std::io::Write;
            let (reader, mut writer) = std::io::pipe().map_err(|error| format!("Cannot make the bootstrap channel: {error}"))?;
            writer.write_all(line.as_bytes()).map_err(|error| format!("Cannot write the bootstrap envelope: {error}"))?;
            drop(writer);
            let descriptor = std::os::fd::AsRawFd::as_raw_fd(&reader);
            // After the fork and before the exec: the read end becomes descriptor 3 (which clears close-on-exec).
            // Everything else the pipe created stays close-on-exec.
            unsafe {
                command.pre_exec(move || {
                    if descriptor == BOOTSTRAP_FD {
                        // Already in place; dup2 would leave close-on-exec set.
                        let flags = libc::fcntl(descriptor, libc::F_GETFD);
                        if flags < 0 || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    } else if libc::dup2(descriptor, BOOTSTRAP_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command.spawn().map_err(|error| format!("Cannot start {}: {error}", self.config.binary.display()))?;
            drop(reader);
            let pid = child.id();
            if let (Some(file), Some(pid)) = (&self.config.pid_file, pid) {
                let _ = std::fs::write(file, pid.to_string());
            }
            Ok(Launched { process: ChildProcess { child, pid, pid_file: self.config.pid_file.clone() }, address: self.config.address(), credential: Secret(credential) })
        }
        #[cfg(not(unix))]
        {
            let _ = line;
            Err("Starting the server needs an inherited descriptor for its bootstrap credential, which the server does not support on this platform yet.".into())
        }
    }

    async fn ready(&mut self, address: &str) {
        let Some(authority) = address.strip_prefix("http://") else { return std::future::pending().await };
        loop {
            if answers(authority).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
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
        let config = ServerConfig { binary: "/app/t3-server".into(), state_dir: "/data/server".into(), log_file: "/data/logs/server.log".into(), pid_file: None, host: "127.0.0.1".into(), port: 3775 };
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
        ServerConfig { binary: script, state_dir: directory.join("state"), log_file: directory.join("logs/server.log"), pid_file: Some(directory.join("server.pid")), host: "127.0.0.1".into(), port: 3779 }
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
