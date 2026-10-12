//! A Chrome DevTools Protocol client: commands with replies, and the events a page raises, over the pipe a browser started with
//! `--remote-debugging-pipe` gives (NUL-delimited JSON) or over a WebSocket on loopback (one JSON message per text frame). It speaks the
//! flat session protocol of `Target.attachToTarget` with `flatten: true`, where every message of a page carries its `sessionId`.
//! (The original's client is inlined in apps/server/src/htmlRender/headlessChrome.ts; Playwright speaks the same wire for the server browser.)
//!
//! A connection is cheap to clone. Its reader and writer tasks end when the last clone is dropped or the other side closes, and a closed
//! connection fails every command still waiting and ends every event subscription.
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

/// What went wrong with a command.
#[derive(Debug, Clone, PartialEq)]
pub enum CdpError {
    /// The browser answered the command with an error.
    Remote { code: Option<i64>, message: String },
    /// The connection closed before the command was answered, or was closed already.
    Closed,
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote { message, .. } => f.write_str(message),
            Self::Closed => f.write_str("the browser connection closed"),
        }
    }
}
impl std::error::Error for CdpError {}

/// An event the browser raised, for the flat session it belongs to (none for the browser itself).
#[derive(Debug, Clone, PartialEq)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

#[derive(Debug)]
pub enum CdpConnectError {
    /// The address is not a `ws://` URL to a loopback host: the DevTools endpoint of a browser this server started.
    NotLoopback(String),
    Connect(String),
}
impl std::fmt::Display for CdpConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLoopback(url) => write!(f, "{url} is not a WebSocket address on this machine"),
            Self::Connect(message) => write!(f, "could not connect to the browser: {message}"),
        }
    }
}
impl std::error::Error for CdpConnectError {}

/// One message to the browser as the pieces of its JSON, so a large body can be shared between the small halves around it and never copied.
type Frame = Vec<Arc<[u8]>>;

type Reply = oneshot::Sender<Result<Value, CdpError>>;

enum Delivery {
    Channel(mpsc::UnboundedSender<Arc<CdpEvent>>),
    Callback(Box<dyn FnMut(&CdpEvent) + Send>),
}

struct Subscriber {
    session_id: Option<String>,
    delivery: Delivery,
}

#[derive(Default)]
struct State {
    pending: HashMap<u64, Reply>,
    subscribers: Vec<Subscriber>,
    closed: bool,
}

struct Shared {
    next_id: AtomicU64,
    outgoing: mpsc::UnboundedSender<Frame>,
    state: Mutex<State>,
    closed: watch::Sender<bool>,
}

struct AbortOnDrop(Vec<JoinHandle<()>>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub struct CdpConnection {
    shared: Arc<Shared>,
    _tasks: Arc<AbortOnDrop>,
}

/// Posts commands to a connection without keeping it alive.
#[derive(Clone)]
pub struct CdpPoster(std::sync::Weak<Shared>);

impl CdpPoster {
    pub fn post(&self, method: &str, params: Value, session_id: Option<&str>) {
        if let Some(shared) = self.0.upgrade() {
            shared.post(method, params, session_id);
        }
    }

    /// `CdpConnection::post_parts`.
    pub fn post_parts(&self, build: impl FnOnce(u64) -> Vec<Arc<[u8]>>) {
        if let Some(shared) = self.0.upgrade() {
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = shared.outgoing.send(build(id));
        }
    }
}

#[derive(Deserialize)]
struct Incoming {
    id: Option<u64>,
    method: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    params: Option<Value>,
    result: Option<Value>,
    error: Option<RemoteError>,
}

#[derive(Deserialize)]
struct RemoteError {
    code: Option<i64>,
    message: Option<String>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Routes one message from the browser: a reply to the command waiting for it, or an event to its subscribers. A message that is neither,
    /// or is not JSON, is dropped, as is a reply nobody waits for.
    fn receive(&self, message: &[u8]) {
        let Ok(incoming) = serde_json::from_slice::<Incoming>(message) else { return };
        if let Some(id) = incoming.id {
            let waiter = self.lock().pending.remove(&id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(match incoming.error {
                    Some(error) => Err(CdpError::Remote { code: error.code, message: error.message.unwrap_or_default() }),
                    None => Ok(incoming.result.unwrap_or(Value::Null)),
                });
            }
            return;
        }
        let Some(method) = incoming.method else { return };
        let event = Arc::new(CdpEvent { method, params: incoming.params.unwrap_or(Value::Null), session_id: incoming.session_id });
        self.lock().subscribers.retain_mut(|subscriber| {
            let wanted = subscriber.session_id.as_ref().is_none_or(|session| event.session_id.as_ref() == Some(session));
            if !wanted {
                return true;
            }
            match &mut subscriber.delivery {
                Delivery::Channel(events) => events.send(event.clone()).is_ok(),
                Delivery::Callback(callback) => {
                    callback(&event);
                    true
                }
            }
        });
    }

    fn post(&self, method: &str, params: Value, session_id: Option<&str>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let mut command = json!({"id": id, "method": method, "params": params});
        if let Some(session_id) = session_id {
            command["sessionId"] = json!(session_id);
        }
        let _ = self.outgoing.send(vec![Arc::from(command.to_string().into_bytes())]);
    }

    /// Fails every waiting command and ends every subscription. The first close wins.
    fn close(&self) {
        let waiting = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.subscribers.clear();
            std::mem::take(&mut state.pending)
        };
        for (_, waiter) in waiting {
            let _ = waiter.send(Err(CdpError::Closed));
        }
        self.closed.send_replace(true);
    }
}

impl CdpConnection {
    fn start<F, G>(spawn_tasks: F) -> Self
    where
        F: FnOnce(Arc<Shared>, mpsc::UnboundedReceiver<Frame>) -> G,
        G: IntoIterator<Item = JoinHandle<()>>,
    {
        let (outgoing, queued) = mpsc::unbounded_channel();
        let (closed, _) = watch::channel(false);
        let shared = Arc::new(Shared { next_id: AtomicU64::new(0), outgoing, state: Mutex::default(), closed });
        let tasks = spawn_tasks(shared.clone(), queued).into_iter().collect();
        Self { shared, _tasks: Arc::new(AbortOnDrop(tasks)) }
    }

    /// Speaks over the two ends of a browser's `--remote-debugging-pipe`: `reader` is what the browser writes (its fd 4), `writer` what it reads
    /// (its fd 3). Each message is JSON followed by a NUL.
    pub fn over_pipe<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self::start(|shared, mut queued| {
            let read = {
                let shared = shared.clone();
                tokio::spawn(async move {
                    let mut reader = reader;
                    let mut chunk = vec![0u8; 64 * 1024];
                    // A message may be split anywhere between reads, and a screenshot spans many.
                    let mut partial: Vec<u8> = Vec::new();
                    loop {
                        match reader.read(&mut chunk).await {
                            Ok(0) | Err(_) => break,
                            Ok(read) => {
                                let mut rest = &chunk[..read];
                                while let Some(end) = rest.iter().position(|byte| *byte == 0) {
                                    partial.extend_from_slice(&rest[..end]);
                                    shared.receive(&partial);
                                    partial.clear();
                                    rest = &rest[end + 1..];
                                }
                                partial.extend_from_slice(rest);
                            }
                        }
                    }
                    shared.close();
                })
            };
            let write = tokio::spawn(async move {
                let mut writer = writer;
                'frames: while let Some(frame) = queued.recv().await {
                    for part in frame {
                        if writer.write_all(&part).await.is_err() {
                            break 'frames;
                        }
                    }
                    if writer.write_all(&[0]).await.is_err() || writer.flush().await.is_err() {
                        break;
                    }
                }
                shared.close();
            });
            [read, write]
        })
    }

    /// Connects to a DevTools WebSocket endpoint on this machine, such as `ws://127.0.0.1:9222/devtools/browser/<id>`. Anything that is not
    /// a loopback address is refused before a socket is opened.
    pub async fn connect_websocket(url: &str) -> Result<Self, CdpConnectError> {
        let loopback = url::Url::parse(url).ok().is_some_and(|parsed| {
            parsed.scheme() == "ws"
                && match parsed.host() {
                    Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
                    Some(url::Host::Ipv4(address)) => address.is_loopback(),
                    Some(url::Host::Ipv6(address)) => address.is_loopback(),
                    None => false,
                }
        });
        if !loopback {
            return Err(CdpConnectError::NotLoopback(url.to_owned()));
        }
        // A screenshot reply is far larger than the library's default frame.
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default().max_message_size(Some(512 << 20)).max_frame_size(Some(512 << 20));
        let (socket, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), false).await.map_err(|error| CdpConnectError::Connect(error.to_string()))?;
        let (mut sink, mut stream) = socket.split();
        Ok(Self::start(|shared, mut queued| {
            let read = {
                let shared = shared.clone();
                tokio::spawn(async move {
                    while let Some(Ok(message)) = stream.next().await {
                        match message {
                            tokio_tungstenite::tungstenite::Message::Text(text) => shared.receive(text.as_bytes()),
                            tokio_tungstenite::tungstenite::Message::Close(_) => break,
                            _ => {}
                        }
                    }
                    shared.close();
                })
            };
            let write = tokio::spawn(async move {
                while let Some(frame) = queued.recv().await {
                    let bytes: Vec<u8> = frame.iter().flat_map(|part| part.iter().copied()).collect();
                    // Every part is a piece of one JSON text, so the whole is UTF-8.
                    let Ok(text) = String::from_utf8(bytes) else { continue };
                    if sink.send(tokio_tungstenite::tungstenite::Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                shared.close();
            });
            [read, write]
        }))
    }

    /// Runs a command and waits for its reply. `session_id` addresses a page attached with `Target.attachToTarget`; none addresses the browser.
    pub async fn send(&self, method: &str, params: Value, session_id: Option<&str>) -> Result<Value, CdpError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, answer) = oneshot::channel();
        {
            let mut state = self.shared.lock();
            if state.closed {
                return Err(CdpError::Closed);
            }
            state.pending.insert(id, reply);
        }
        let mut command = json!({"id": id, "method": method, "params": params});
        if let Some(session_id) = session_id {
            command["sessionId"] = json!(session_id);
        }
        if self.shared.outgoing.send(vec![Arc::from(command.to_string().into_bytes())]).is_err() {
            self.shared.lock().pending.remove(&id);
            return Err(CdpError::Closed);
        }
        answer.await.unwrap_or(Err(CdpError::Closed))
    }

    /// A command whose reply nobody awaits: the reply is dropped on arrival.
    pub fn post(&self, method: &str, params: Value, session_id: Option<&str>) {
        self.shared.post(method, params, session_id);
    }

    /// A handle that posts commands but does not keep the connection alive, for an event callback, which the connection itself holds.
    pub fn poster(&self) -> CdpPoster {
        CdpPoster(Arc::downgrade(&self.shared))
    }

    /// A command written from pieces of JSON that `build` lays out around the id it is given, for a body too large to copy into a value. Its
    /// reply is dropped on arrival.
    pub fn post_parts(&self, build: impl FnOnce(u64) -> Vec<Arc<[u8]>>) {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.shared.outgoing.send(build(id));
    }

    /// Every event from now on: those of one session, or all of them. The stream ends when the connection closes.
    pub fn events(&self, session_id: Option<&str>) -> mpsc::UnboundedReceiver<Arc<CdpEvent>> {
        let (events, receiver) = mpsc::unbounded_channel();
        let mut state = self.shared.lock();
        if !state.closed {
            state.subscribers.push(Subscriber { session_id: session_id.map(str::to_owned), delivery: Delivery::Channel(events) });
        }
        receiver
    }

    /// Calls `callback` for every event from now on, those of one session or all of them, on the connection's reader in the order the browser
    /// sent them and before any reply that followed them is handed to its command. A command that needs the events before it to have been seen
    /// can rely on that; the callback must be quick, and must not subscribe.
    pub fn on_event(&self, session_id: Option<&str>, callback: impl FnMut(&CdpEvent) + Send + 'static) {
        let mut state = self.shared.lock();
        if !state.closed {
            state.subscribers.push(Subscriber { session_id: session_id.map(str::to_owned), delivery: Delivery::Callback(Box::new(callback)) });
        }
    }

    pub fn is_closed(&self) -> bool {
        *self.shared.closed.borrow()
    }

    /// Resolves once the connection has closed.
    pub async fn closed(&self) {
        let mut closed = self.shared.closed.subscribe();
        let _ = closed.wait_for(|closed| *closed).await;
    }

    /// Closes the connection from this side. The browser is not told to exit.
    pub fn close(&self) {
        self.shared.close();
        for task in &self._tasks.0 {
            task.abort();
        }
    }
}
