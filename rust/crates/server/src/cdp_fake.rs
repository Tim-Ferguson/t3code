//! A scripted stand-in for a browser's DevTools endpoint, for tests: it answers the commands it receives with frames written out in advance
//! (a replay file, one rule per line) or computed by a handler, over the same two transports the real browser offers. No browser is ever
//! started. The replay frames follow the protocol's documented shapes; they were written by hand, not captured from a running Chrome.
//!
//! A replay line is `{"on": "<Domain.method>", "frames": [...]}`. When a command with that method arrives, the next unused rule for the method
//! answers (the last one answers every later command), and each of its frames is sent in order. In a frame, a string that is exactly `$id` is
//! the command's id, `$sessionId` is its session, `$params.<name>` is one of its string params and `$params.<name>.withoutFragment` is that
//! with any `#` and what follows removed. A frame `{"$hangUp": true}` ends the connection from the browser's side at that point.
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    task::JoinHandle,
};

/// A command the endpoint received.
#[derive(Debug, Clone)]
pub struct Command {
    pub id: u64,
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

type Handler = Box<dyn FnMut(&Command) -> Vec<Value> + Send>;

enum Behaviour {
    Replay(HashMap<String, Vec<Vec<Value>>>),
    Handler(Handler),
}

enum Control {
    Push(Value),
    HangUp,
}

struct Inner {
    behaviour: Mutex<Behaviour>,
    commands: Mutex<Vec<Command>>,
    control: mpsc::UnboundedSender<Control>,
    controlled: Mutex<Option<mpsc::UnboundedReceiver<Control>>>,
}

#[derive(Clone)]
pub struct FakeBrowser(Arc<Inner>);

fn substitute(frame: &Value, command: &Command) -> Value {
    match frame {
        Value::String(text) if text == "$id" => json!(command.id),
        Value::String(text) if text == "$sessionId" => json!(command.session_id),
        Value::String(text) if text.starts_with("$params.") => {
            let name = &text["$params.".len()..];
            let (name, without_fragment) = name.strip_suffix(".withoutFragment").map_or((name, false), |name| (name, true));
            let value = command.params[name].as_str().unwrap_or_default();
            json!(if without_fragment { value.split('#').next().unwrap_or_default() } else { value })
        }
        Value::Array(items) => Value::Array(items.iter().map(|item| substitute(item, command)).collect()),
        Value::Object(fields) => Value::Object(fields.iter().map(|(name, item)| (name.clone(), substitute(item, command))).collect::<Map<_, _>>()),
        other => other.clone(),
    }
}

impl FakeBrowser {
    fn new(behaviour: Behaviour) -> Self {
        let (control, controlled) = mpsc::unbounded_channel();
        Self(Arc::new(Inner { behaviour: Mutex::new(behaviour), commands: Mutex::default(), control, controlled: Mutex::new(Some(controlled)) }))
    }

    /// Answers from a replay file.
    pub fn replay(file: &str) -> Self {
        let mut rules: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
        for line in file.lines().filter(|line| !line.trim().is_empty()) {
            let rule: Value = serde_json::from_str(line).expect("a replay line is JSON");
            rules.entry(rule["on"].as_str().expect("a rule names its method").to_owned()).or_default().push(rule["frames"].as_array().expect("a rule has frames").clone());
        }
        Self::new(Behaviour::Replay(rules))
    }

    /// Answers with what `handler` returns for each command.
    pub fn with_handler(handler: impl FnMut(&Command) -> Vec<Value> + Send + 'static) -> Self {
        Self::new(Behaviour::Handler(Box::new(handler)))
    }

    /// Every command received so far, in order.
    pub fn commands(&self) -> Vec<Command> {
        self.0.commands.lock().unwrap().clone()
    }

    pub fn commands_named(&self, method: &str) -> Vec<Command> {
        self.commands().into_iter().filter(|command| command.method == method).collect()
    }

    /// Sends a frame nobody asked for.
    pub fn push(&self, frame: Value) {
        let _ = self.0.control.send(Control::Push(frame));
    }

    /// Ends the connection from the browser's side.
    pub fn hang_up(&self) {
        let _ = self.0.control.send(Control::HangUp);
    }

    fn answer(&self, command: &Command) -> Vec<Value> {
        let mut behaviour = self.0.behaviour.lock().unwrap();
        match &mut *behaviour {
            Behaviour::Handler(handler) => handler(command),
            Behaviour::Replay(rules) => match rules.get_mut(&command.method) {
                Some(queue) => {
                    let frames = if queue.len() > 1 { queue.remove(0) } else { queue[0].clone() };
                    frames.iter().map(|frame| substitute(frame, command)).collect()
                }
                None => vec![json!({"id": command.id, "error": {"code": -32601, "message": format!("'{}' wasn't found", command.method)}})],
            },
        }
    }

    /// Takes one received message: records the command and returns the frames to send for it.
    fn receive(&self, message: &[u8]) -> Vec<Value> {
        let Ok(value) = serde_json::from_slice::<Value>(message) else { return Vec::new() };
        let command = Command {
            id: value["id"].as_u64().unwrap_or_default(),
            method: value["method"].as_str().unwrap_or_default().to_owned(),
            params: value["params"].clone(),
            session_id: value["sessionId"].as_str().map(str::to_owned),
        };
        self.0.commands.lock().unwrap().push(command.clone());
        self.answer(&command)
    }

    /// The browser's end of a pipe: returns the client's ends, to hand to `CdpConnection::over_pipe`.
    pub fn pipe(&self) -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
        let (client_in, mut server_out) = tokio::io::duplex(1 << 20);
        let (mut server_in, client_out) = tokio::io::duplex(1 << 20);
        let browser = self.clone();
        let mut controlled = self.0.controlled.lock().unwrap().take().expect("the endpoint serves one connection");
        tokio::spawn(async move {
            let mut chunk = vec![0u8; 64 * 1024];
            let mut partial: Vec<u8> = Vec::new();
            loop {
                tokio::select! {
                    read = server_in.read(&mut chunk) => {
                        let Ok(read @ 1..) = read else { break };
                        let mut rest = &chunk[..read];
                        while let Some(end) = rest.iter().position(|byte| *byte == 0) {
                            partial.extend_from_slice(&rest[..end]);
                            let frames = browser.receive(&partial);
                            partial.clear();
                            rest = &rest[end + 1..];
                            for frame in frames {
                                if frame.get("$hangUp").is_some() {
                                    return;
                                }
                                if server_out.write_all(frame.to_string().as_bytes()).await.is_err() || server_out.write_all(&[0]).await.is_err() {
                                    return;
                                }
                            }
                        }
                        partial.extend_from_slice(rest);
                    }
                    control = controlled.recv() => match control {
                        Some(Control::Push(frame)) => {
                            if server_out.write_all(frame.to_string().as_bytes()).await.is_err() || server_out.write_all(&[0]).await.is_err() {
                                return;
                            }
                        }
                        Some(Control::HangUp) | None => break,
                    },
                }
            }
        });
        (client_in, client_out)
    }

    /// The browser's end of a WebSocket on loopback: returns its address, which `CdpConnection::connect_websocket` takes, and the task serving
    /// the one connection it accepts.
    pub async fn websocket(&self) -> (String, JoinHandle<()>) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let url = format!("ws://127.0.0.1:{}/devtools/browser/fake", listener.local_addr().unwrap().port());
        let browser = self.clone();
        let mut controlled = self.0.controlled.lock().unwrap().take().expect("the endpoint serves one connection");
        let task = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else { return };
            let Ok(socket) = tokio_tungstenite::accept_async(stream).await else { return };
            let (mut sink, mut messages) = socket.split();
            loop {
                tokio::select! {
                    message = messages.next() => match message {
                        Some(Ok(Message::Text(text))) => {
                            for frame in browser.receive(text.as_bytes()) {
                                if frame.get("$hangUp").is_some() {
                                    let _ = sink.send(Message::Close(None)).await;
                                    return;
                                }
                                if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Some(Ok(_)) => {}
                        _ => break,
                    },
                    control = controlled.recv() => match control {
                        Some(Control::Push(frame)) => {
                            if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                                return;
                            }
                        }
                        Some(Control::HangUp) | None => {
                            let _ = sink.send(Message::Close(None)).await;
                            break;
                        }
                    },
                }
            }
        });
        (url, task)
    }
}

/// A launcher that hands out a connection to a scripted browser instead of starting one, and remembers how it was asked.
pub struct FakeLauncher {
    pub browser: FakeBrowser,
    launches: Mutex<Vec<(std::path::PathBuf, bool, std::path::PathBuf)>>,
    shutdowns: Arc<std::sync::atomic::AtomicUsize>,
    exit_output: String,
}

struct FakeHandle {
    shutdowns: Arc<std::sync::atomic::AtomicUsize>,
    exit_output: String,
}

#[async_trait::async_trait]
impl crate::headless_chrome::BrowserHandle for FakeHandle {
    async fn exited(&self) -> crate::headless_chrome::HtmlRenderBrowserError {
        crate::headless_chrome::HtmlRenderBrowserError { reason: "the browser exited unexpectedly".into(), cause: None, output: Some(self.exit_output.clone()) }
    }
    async fn shutdown(&self) {
        self.shutdowns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl FakeLauncher {
    pub fn new(browser: FakeBrowser) -> Self {
        Self { browser, launches: Mutex::default(), shutdowns: Arc::default(), exit_output: "fake stderr".into() }
    }
    pub fn exiting_with(mut self, output: &str) -> Self {
        self.exit_output = output.into();
        self
    }
    /// What each launch was asked to start, in order: the executable, whether the sandbox was off, and the profile directory.
    pub fn launches(&self) -> Vec<(std::path::PathBuf, bool, std::path::PathBuf)> {
        self.launches.lock().unwrap().clone()
    }
    pub fn shutdowns(&self) -> usize {
        self.shutdowns.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl crate::headless_chrome::BrowserLauncher for FakeLauncher {
    async fn launch(&self, input: &crate::headless_chrome::LaunchInput) -> Result<crate::headless_chrome::LaunchedBrowser, crate::headless_chrome::HtmlRenderBrowserError> {
        self.launches.lock().unwrap().push((input.executable.clone(), input.no_sandbox, input.profile_directory.clone()));
        let (reader, writer) = self.browser.pipe();
        Ok(crate::headless_chrome::LaunchedBrowser {
            connection: crate::cdp::CdpConnection::over_pipe(reader, writer),
            handle: Arc::new(FakeHandle { shutdowns: self.shutdowns.clone(), exit_output: self.exit_output.clone() }),
        })
    }
}

/// A scripted page: answers the commands of a load, with the height `height` gives for the width the page was set to, and counts the pages open.
#[derive(Default)]
pub struct Pages {
    pub created: usize,
    pub open: usize,
    pub max_open: usize,
    pub closed: usize,
    pub widths: HashMap<String, u32>,
    pub navigations: usize,
}

pub fn page_handler(pages: Arc<Mutex<Pages>>, height: impl Fn(u32) -> Value + Send + 'static, mut console: impl FnMut() -> Vec<Value> + Send + 'static) -> impl FnMut(&Command) -> Vec<Value> + Send + 'static {
    move |command| {
        let session = command.session_id.clone();
        let reply = |result: Value| json!({"id": command.id, "sessionId": session, "result": result});
        let mut pages = pages.lock().unwrap();
        match command.method.as_str() {
            "Target.createTarget" => {
                pages.created += 1;
                pages.open += 1;
                pages.max_open = pages.max_open.max(pages.open);
                vec![json!({"id": command.id, "result": {"targetId": format!("T-{}", pages.created)}})]
            }
            "Target.attachToTarget" => vec![json!({"id": command.id, "result": {"sessionId": format!("S-{}", command.params["targetId"].as_str().unwrap().trim_start_matches("T-"))}})],
            "Target.closeTarget" => {
                pages.open -= 1;
                pages.closed += 1;
                vec![json!({"id": command.id, "result": {"success": true}})]
            }
            "Emulation.setDeviceMetricsOverride" => {
                pages.widths.insert(session.clone().unwrap(), command.params["width"].as_u64().unwrap() as u32);
                vec![reply(json!({}))]
            }
            "Page.navigate" => {
                pages.navigations += 1;
                let target = format!("T-{}", session.as_deref().unwrap().trim_start_matches("S-"));
                let url = command.params["url"].as_str().unwrap().split('#').next().unwrap().to_owned();
                vec![
                    reply(json!({"frameId": target, "loaderId": "L"})),
                    json!({"method": "Fetch.requestPaused", "sessionId": session, "params": {"requestId": format!("req-{}", command.id), "request": {"url": url}, "frameId": target, "resourceType": "Document"}}),
                ]
            }
            "Fetch.fulfillRequest" => {
                let mut frames = vec![reply(json!({}))];
                for params in console() {
                    frames.push(json!({"method": "Runtime.consoleAPICalled", "sessionId": session, "params": params}));
                }
                frames.push(json!({"method": "Page.loadEventFired", "sessionId": session, "params": {"timestamp": 1.0}}));
                frames
            }
            "Runtime.evaluate" if command.params["expression"].as_str().unwrap().contains("document.fonts.ready") => vec![reply(json!({"result": {"type": "boolean", "value": true}}))],
            "Runtime.evaluate" => vec![reply(height(pages.widths[session.as_deref().unwrap()]))],
            "Page.captureScreenshot" => vec![reply(json!({"data": "AAAA"}))],
            _ => vec![reply(json!({}))],
        }
    }
}

pub fn measured(value: f64) -> Value {
    json!({"result": {"type": "number", "value": value}})
}
