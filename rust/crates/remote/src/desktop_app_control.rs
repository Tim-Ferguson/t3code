//! The control socket `t3 app` uses to ask the running desktop app to open a project (`desktopAppControl.ts` of
//! packages/shared, `cli/app.ts` and `app/DesktopAppActivation.ts` of the original).
//!
//! The CLI half is [`resolve_address`] and [`send_request`]. The desktop half is [`start_control_server`]: it binds the address with
//! the original's ownership checks and hands each request line to the shell, which answers it. The shell's part (the broker that
//! holds a request until its renderer is ready, and the window that opens the project and thread) is not here; see the README
//! of the Rust port.
//!
//! The address is under the temporary directory, in a directory only its user can enter (`t3code-<uid>`), named by a hash of the
//! state directory so a custom T3 home cannot exceed the length a Unix socket path may have. On Windows it is a named pipe,
//! which this module names but does not serve.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use t3_contracts::{DesktopAppActivationRequest, DesktopAppActivationResponse};

/// `MAX_RESPONSE_BYTES` of `cli/app.ts`, and `MAX_REQUEST_BYTES` of the desktop's socket: one line may not be longer.
pub const MAX_LINE_BYTES: usize = 64 * 1024;
/// `CLI_RESPONSE_TIMEOUT_MS`: how long `t3 app` waits for an answer. The desktop gives up on a request at 15 s, so the CLI waits two
/// seconds longer for the answer that says so.
pub const CLI_RESPONSE_TIMEOUT: Duration = Duration::from_millis(17_000);
/// `REQUEST_TIMEOUT_MS` of `DesktopAppActivation.ts`: how long the shell holds a request for its renderer.
pub const DESKTOP_REQUEST_TIMEOUT: Duration = Duration::from_millis(15_000);
/// How long a connection may sit before sending its line (`socket.setTimeout(5_000)`).
pub const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Where the desktop app listens: the socket (or pipe) and, on Unix, the directory that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlAddress {
    pub address: String,
    pub directory: Option<PathBuf>,
}

fn short_hash(value: &str) -> String {
    Sha256::digest(value.as_bytes()).iter().take(12).map(|byte| format!("{byte:02x}")).collect()
}

/// `resolveDesktopAppControlAddress`. `platform` is the Node `process.platform` name; `user_id` is the real user id (none on
/// Windows). `state_dir` is the desktop's state directory (`<T3 home>/userdata`, or `<T3 home>/dev` for a development build).
pub fn resolve_address(state_dir: &Path, platform: &str, temp_dir: &Path, user_id: Option<u32>) -> ControlAddress {
    let state_hash = short_hash(&state_dir.to_string_lossy());
    if platform == "win32" {
        return ControlAddress { address: format!("\\\\.\\pipe\\t3code-app-{state_hash}"), directory: None };
    }
    let user_key = user_id.map_or_else(|| state_hash[..12].to_owned(), |id| id.to_string());
    let directory = temp_dir.join(format!("t3code-{user_key}"));
    let address = directory.join(format!("{state_hash}.sock")).to_string_lossy().into_owned();
    ControlAddress { address, directory: Some(directory) }
}

/// Node's `os.tmpdir()` on Unix: `TMPDIR`, `TMP` or `TEMP` (the first that is set and not empty), else `/tmp`, without a trailing
/// slash. The CLI and the desktop shell must agree on it, or they resolve different addresses.
pub fn node_tmpdir(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    #[cfg(windows)]
    let (variables, fallback) = (["TEMP", "TMP"], "C:\\Windows\\Temp");
    #[cfg(not(windows))]
    let (variables, fallback) = (["TMPDIR", "TMP", "TEMP"], "/tmp");
    let found = variables.iter().find_map(|name| env(name).filter(|value| !value.is_empty())).unwrap_or_else(|| fallback.to_owned());
    let trimmed = if found.len() > 1 && (found.ends_with('/') || found.ends_with('\\') && cfg!(windows)) { &found[..found.len() - 1] } else { &found };
    PathBuf::from(trimmed)
}

/// The real user id of this process; none where there is no such thing.
pub fn current_user_id() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(unsafe { libc::getuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Why a request got no answer. `code` is the `errno` name Node would report for a failed connection (`ENOENT`, `ECONNREFUSED`...),
/// which is what `t3 app` falls back on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendError {
    pub code: Option<String>,
    pub message: String,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn errno_name(error: &std::io::Error) -> Option<String> {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::NotFound => Some("ENOENT".into()),
        ErrorKind::ConnectionRefused => Some("ECONNREFUSED".into()),
        ErrorKind::PermissionDenied => Some("EACCES".into()),
        _ => None,
    }
}

#[cfg(unix)]
type Stream = tokio::net::UnixStream;
#[cfg(unix)]
async fn connect(address: &str) -> std::io::Result<Stream> {
    tokio::net::UnixStream::connect(address).await
}
#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(windows)]
async fn connect(address: &str) -> std::io::Result<Stream> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(address)
}

/// What one attempt at an address came to.
enum Attempt {
    Answered(Result<DesktopAppActivationResponse, SendError>),
    /// Nothing there, or nothing listening: the one failure `t3 app` retries elsewhere.
    Unreachable,
}

async fn attempt(address: &str, may_fall_back: bool, request: &DesktopAppActivationRequest, timeout: Duration) -> Attempt {
    let run = async {
        match connect(address).await {
            Ok(stream) => Attempt::Answered(exchange(stream, request).await),
            Err(error) => {
                let code = errno_name(&error);
                if may_fall_back && matches!(code.as_deref(), Some("ENOENT" | "ECONNREFUSED")) {
                    Attempt::Unreachable
                } else {
                    Attempt::Answered(Err(SendError { code, message: error.to_string() }))
                }
            }
        }
    };
    tokio::time::timeout(timeout, run).await.unwrap_or_else(|_| Attempt::Answered(Err(SendError { code: None, message: "The desktop app did not respond in time.".into() })))
}

/// `sendDesktopAppActivationRequest`: one request line to `address`, the one response line back. When the address cannot be
/// connected (nothing there, or nothing listening) and a `fallback` is given, the same request goes there instead, with a deadline
/// of its own; once connected there is no fallback, whatever comes back. The answer is accepted only if it is a valid response
/// to this request's id.
pub async fn send_request(address: &str, fallback: Option<&str>, request: &DesktopAppActivationRequest, timeout: Duration) -> Result<DesktopAppActivationResponse, SendError> {
    match attempt(address, fallback.is_some(), request, timeout).await {
        Attempt::Answered(result) => result,
        Attempt::Unreachable => match attempt(fallback.unwrap_or(address), false, request, timeout).await {
            Attempt::Answered(result) => result,
            Attempt::Unreachable => unreachable!("an attempt that may not fall back always answers"),
        },
    }
}

async fn exchange(mut stream: Stream, request: &DesktopAppActivationRequest) -> Result<DesktopAppActivationResponse, SendError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let failure = |message: &str| SendError { code: None, message: message.into() };
    let mut line = serde_json::to_vec(request).map_err(|error| failure(&error.to_string()))?;
    line.push(b'\n');
    stream.write_all(&line).await.map_err(|error| SendError { code: errno_name(&error), message: error.to_string() })?;
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = stream.read(&mut chunk).await.map_err(|error| SendError { code: errno_name(&error), message: error.to_string() })?;
        if read == 0 {
            return Err(failure("The desktop app closed the connection."));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_LINE_BYTES {
            return Err(failure("The desktop app response is too large."));
        }
        let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') else { continue };
        let text = String::from_utf8_lossy(&buffer[..newline]).into_owned();
        if serde_json::from_str::<Value>(&text).is_err() {
            return Err(failure("The desktop app response is not valid JSON."));
        }
        let response = DesktopAppActivationResponse::parse(&text).map_err(|_| failure("The desktop app response is invalid."))?;
        if response.request_id() != request.request_id.as_str() {
            return Err(failure("The desktop app response did not match this request."));
        }
        return Ok(response);
    }
}

// ---- the desktop's side ---------------------------------------------------------------------------------------------

/// What the shell does with a request. `handle` answers it (after the renderer has opened the project and thread, or failed to);
/// `cancel` is told when the CLI went away before that.
#[async_trait::async_trait]
pub trait ControlHandler: Send + Sync + 'static {
    async fn handle(&self, request: DesktopAppActivationRequest) -> DesktopAppActivationResponse;
    fn cancel(&self, request_id: &str);
}

/// A running control socket. Dropping it does not close it; call [`ControlServer::close`].
#[cfg(unix)]
pub struct ControlServer {
    address: PathBuf,
    directory: PathBuf,
    user_id: Option<u32>,
    handler: std::sync::Arc<dyn ControlHandler>,
    state: std::sync::Arc<tokio::sync::Mutex<Bound>>,
    closed: std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
struct Bound {
    task: tokio::task::JoinHandle<()>,
    /// The inode of the socket file this server put at the address, so `close` removes only its own.
    inode: Option<u64>,
}

#[cfg(unix)]
fn invalid(request_id: &str, message: &str) -> DesktopAppActivationResponse {
    DesktopAppActivationResponse::failure(request_id, t3_contracts::DesktopAppActivationErrorCode::InvalidRequest, message)
}

/// `requestIdFromUnknown`: the id of a request that failed to decode, if it has a usable one.
#[cfg(unix)]
fn request_id_of(value: &Value) -> String {
    match value.get("requestId").and_then(Value::as_str) {
        Some(id) if !t3_contracts::trim_wire_string(id).is_empty() => id.to_owned(),
        _ => "invalid-request".to_owned(),
    }
}

/// `prepareUnixDirectory`: the socket's directory exists, is a real directory (not a link), belongs to this user and is closed to
/// everyone else. Returns whether it had to be made.
#[cfg(unix)]
pub fn prepare_directory(directory: &Path, user_id: Option<u32>) -> std::io::Result<bool> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let created = !directory.exists();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(format!("{} is not a directory.", directory.display())));
    }
    if user_id.is_some_and(|id| metadata.uid() != id) {
        return Err(std::io::Error::other(format!("{} is owned by another user.", directory.display())));
    }
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    Ok(created)
}

#[cfg(unix)]
fn inode_at(path: &Path) -> std::io::Result<Option<u64>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata.ino())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
async fn serve_connection(stream: tokio::net::UnixStream, handler: std::sync::Arc<dyn ControlHandler>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut reader, mut writer) = stream.into_split();
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let line = loop {
        let read = match tokio::time::timeout(CONNECTION_IDLE_TIMEOUT, reader.read(&mut chunk)).await {
            Ok(Ok(read)) if read > 0 => read,
            _ => return,
        };
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_LINE_BYTES {
            let _ = writer.write_all(format!("{}\n", serde_json::to_string(&invalid("invalid-request", "The desktop app request is too large.")).unwrap_or_default()).as_bytes()).await;
            return;
        }
        if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            break String::from_utf8_lossy(&buffer[..newline]).into_owned();
        }
    };
    let respond = |response: DesktopAppActivationResponse| format!("{}\n", serde_json::to_string(&response).unwrap_or_default());
    let parsed: Value = match serde_json::from_str(&line) {
        Ok(parsed) => parsed,
        Err(_) => {
            let _ = writer.write_all(respond(invalid("invalid-request", "The desktop app request is not valid JSON.")).as_bytes()).await;
            return;
        }
    };
    let request = match DesktopAppActivationRequest::parse(&line) {
        Ok(request) => request,
        Err(_) => {
            let _ = writer.write_all(respond(invalid(&request_id_of(&parsed), "The desktop app request is invalid.")).as_bytes()).await;
            return;
        }
    };
    let request_id = request.request_id.as_str().to_owned();
    // The peer closing before the answer is ready cancels the request.
    let answer = handler.handle(request);
    tokio::pin!(answer);
    let peer_gone = async {
        let mut ignored = [0u8; 256];
        while matches!(reader.read(&mut ignored).await, Ok(read) if read > 0) {}
    };
    tokio::select! {
        response = &mut answer => {
            let _ = writer.write_all(respond(response).as_bytes()).await;
            let _ = writer.shutdown().await;
        }
        _ = peer_gone => handler.cancel(&request_id),
    }
}

#[cfg(unix)]
async fn accept_loop(listener: tokio::net::UnixListener, handler: std::sync::Arc<dyn ControlHandler>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { return };
        tokio::spawn(serve_connection(stream, handler.clone()));
    }
}

#[cfg(unix)]
impl ControlServer {
    /// Binds a staging path in the socket's directory and moves it onto the address (`take_over`, which replaces whatever socket is
    /// there: the newest app wins) or links it there only while the address is free (`claim_free`, which fails with `AlreadyExists`
    /// otherwise). Binding the staging path rather than the address means that closing the listener unlinks only the staging path,
    /// never another app's socket.
    fn bind(&self, take_over: bool) -> std::io::Result<Bound> {
        use std::os::unix::fs::PermissionsExt;
        let staging = self.directory.join(format!("{:012x}.tmp", rand::random::<u64>() >> 16));
        let listener = std::os::unix::net::UnixListener::bind(&staging)?;
        let placed = (|| {
            listener.set_nonblocking(true)?;
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600))?;
            let inode = inode_at(&staging)?;
            if take_over {
                std::fs::rename(&staging, &self.address)?;
            } else {
                std::fs::hard_link(&staging, &self.address)?;
                std::fs::remove_file(&staging)?;
            }
            Ok::<_, std::io::Error>(inode)
        })();
        let inode = match placed {
            Ok(inode) => inode,
            Err(error) => {
                drop(listener);
                let _ = std::fs::remove_file(&staging);
                return Err(error);
            }
        };
        let listener = tokio::net::UnixListener::from_std(listener)?;
        Ok(Bound { task: tokio::spawn(accept_loop(listener, self.handler.clone())), inode })
    }

    /// Binds the address again if its socket file is gone (for example after the app that took it over quit). Never replaces a socket
    /// that exists, so two apps cannot trade the path back and forth. The shell calls this when the directory changes, or on a timer.
    pub async fn reclaim(&self) -> std::io::Result<()> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) || inode_at(Path::new(&self.address))?.is_some() {
            return Ok(());
        }
        prepare_directory(&self.directory, self.user_id)?;
        let next = match self.bind(false) {
            Ok(next) => next,
            // Another app bound the address first.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut bound = self.state.lock().await;
        let previous = std::mem::replace(&mut *bound, next);
        previous.task.abort();
        Ok(())
    }

    /// Stops listening, and removes the socket file only while it is still this server's.
    pub async fn close(&self) -> std::io::Result<()> {
        if self.closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let bound = self.state.lock().await;
        bound.task.abort();
        if bound.inode.is_some() && inode_at(Path::new(&self.address))? == bound.inode {
            match std::fs::remove_file(&self.address) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn address(&self) -> &str {
        self.address.to_str().unwrap_or_default()
    }
}

/// `startDesktopAppControlServer` for Unix: checks the directory, takes the address over and serves it. `user_id` is the user the
/// directory must belong to (`current_user_id()`).
#[cfg(unix)]
pub async fn start_control_server(address: &ControlAddress, user_id: Option<u32>, handler: std::sync::Arc<dyn ControlHandler>) -> std::io::Result<ControlServer> {
    let directory = address.directory.clone().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Unsupported, "A named pipe is not served by this module."))?;
    prepare_directory(&directory, user_id)?;
    let mut server = ControlServer {
        address: PathBuf::from(&address.address),
        directory,
        user_id,
        handler,
        state: std::sync::Arc::new(tokio::sync::Mutex::new(Bound { task: tokio::spawn(async {}), inode: None })),
        closed: std::sync::atomic::AtomicBool::new(false),
    };
    let bound = server.bind(true)?;
    server.state = std::sync::Arc::new(tokio::sync::Mutex::new(bound));
    Ok(server)
}
