//! The browser tab through the mounted UI and the real native server: the Browser row, the session list and its events, the stream the
//! viewer connects to (over a socket with a real ticket), taking and releasing control, the address bar, the menu, dialogs, downloads and
//! the page's file picker, closing and reopening, and what the tab says when the host cannot run a browser. The server's browser is a
//! double that plays a page (`preview_real_tests` runs the same panel against the real `ServerBrowser` on the scripted Chrome); everything
//! else on the server is the real thing. The script in the page (`preview_surface_abi.js`) is stood in for by a task that speaks the same
//! protocol to the same socket (`driver`): it holds the socket, acks frames and forwards what the server says, and the UI decides.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until};
use super::*;
use crate::{
    diff_panel::Gate,
    preview_panel::{PageCmd, PageEvent, TEST_PAGE},
    right_panel::MEMORY,
    runtime::{
        keys_tests::{chord, key_on, press},
        transport_tests::{click_control, click_when_enabled, control, input_when_enabled},
    },
};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use t3_client::keybindings::Dom;
use t3_server::{
    preview_manager::PreviewManager,
    preview_browser_host::PreviewBrowserHostError,
    server_browser::{LaunchCause, ServerBrowserError, UploadedFile},
    server_browser_contexts::LaunchFailure,
    server_browser_viewer::{Ack, ServerBrowserApi, ViewerOutput, ViewerQueue, ViewerStream},
    test_support::HANG_BOUND,
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as Wire;

/// What the double does when a viewer attaches.
enum Plan {
    Viewer,
    Gone,
    NeedsHost(PreviewBrowserHostError),
}

#[derive(Debug, Clone, PartialEq)]
struct Attach {
    thread_id: String,
    tab_id: String,
    max_width: u32,
    max_height: u32,
    can_operate: bool,
}

struct Stub {
    manager: Arc<PreviewManager>,
    plans: Mutex<VecDeque<Plan>>,
    attached: Mutex<Vec<Attach>>,
    queues: Mutex<Vec<Arc<ViewerQueue>>>,
    inputs: mpsc::UnboundedSender<Value>,
    acks: mpsc::UnboundedSender<()>,
    answered: Mutex<Vec<(String, Vec<(String, String, Vec<u8>)>)>>,
    download: Mutex<Option<(PathBuf, String)>>,
    served: Mutex<Vec<String>>,
}

struct Viewing {
    queue: Arc<ViewerQueue>,
    stub: Arc<Stub>,
    thread_id: String,
    tab_id: String,
}
impl Viewing {
    fn push(&self, output: ViewerOutput) {
        assert!(self.queue.offer(output).is_ok());
    }
    fn control(&self, controller: &'static str, generation: u64) {
        self.push(ViewerOutput::Control { can_operate: true, controller, generation, dialog: None });
    }
}
#[async_trait]
impl ViewerStream for Viewing {
    async fn next(&self) -> Option<ViewerOutput> {
        self.queue.next().await
    }
    /// A page that obeys: control goes to whoever takes it, and an address is gone to (the browser reports where it landed).
    async fn input(&self, message: Value) {
        let _ = self.stub.inputs.send(message.clone());
        match message["type"].as_str() {
            Some("takeControl") => self.control("you", 2),
            Some("releaseControl") => self.control("unclaimed", 3),
            Some("dialog") => self.control("you", 4),
            Some("navigate") => {
                let url = message["url"].as_str().unwrap_or_default();
                let status = json!({"threadId": self.thread_id, "tabId": self.tab_id, "navStatus": {"_tag": "Success", "url": url, "title": "Dev app"}, "canGoBack": true, "canGoForward": false});
                self.stub.manager.report_status(serde_json::from_value(status).unwrap(), true).unwrap();
            }
            _ => {}
        }
    }
    async fn close(&self) {
        self.queue.close();
    }
}
#[async_trait]
impl ServerBrowserApi for Stub {
    async fn attach_viewer(&self, thread_id: &str, tab_id: &str, max_width: u32, max_height: u32, _quality: u32, can_operate: bool) -> Result<Box<dyn ViewerStream>, ServerBrowserError> {
        self.attached.lock().unwrap().push(Attach { thread_id: thread_id.into(), tab_id: tab_id.into(), max_width, max_height, can_operate });
        match self.plans.lock().unwrap().pop_front().unwrap_or(Plan::Viewer) {
            Plan::Gone => Err(ServerBrowserError::TabNotFound { thread_id: thread_id.into(), tab_id: tab_id.into() }),
            Plan::NeedsHost(host) => Err(ServerBrowserError::Launch(LaunchCause::Browser(LaunchFailure::Host(host)))),
            Plan::Viewer => {
                let queue = Arc::new(ViewerQueue::default());
                self.queues.lock().unwrap().push(queue.clone());
                let viewing = Viewing { queue, stub: self.clone_handle(), thread_id: thread_id.into(), tab_id: tab_id.into() };
                viewing.control("unclaimed", 1);
                viewing.push(ViewerOutput::Viewport { width: 1280, height: 800 });
                Ok(Box::new(viewing))
            }
        }
    }
    async fn answer_file_chooser(&self, _thread_id: &str, _tab_id: &str, chooser_id: &str, files: Vec<UploadedFile>) -> bool {
        self.answered.lock().unwrap().push((chooser_id.into(), files.into_iter().map(|file| (file.name, file.mime_type, file.bytes)).collect()));
        true
    }
    fn open_download(&self, _thread_id: &str, _tab_id: &str, download_id: &str) -> Option<(PathBuf, String)> {
        self.served.lock().unwrap().push(download_id.into());
        self.download.lock().unwrap().clone()
    }
    async fn clear_profile(&self, _: &str) -> std::io::Result<()> {
        Ok(())
    }
}
thread_local! {
    static CURRENT: std::cell::RefCell<Option<Arc<Stub>>> = const { std::cell::RefCell::new(None) };
}
impl Stub {
    fn clone_handle(&self) -> Arc<Stub> {
        CURRENT.with(|current| current.borrow().clone().expect("the double that is serving"))
    }
    /// The last viewer to attach.
    fn viewer(&self) -> Arc<ViewerQueue> {
        self.queues.lock().unwrap().last().cloned().expect("a viewer has attached")
    }
    fn push(&self, output: ViewerOutput) {
        assert!(self.viewer().offer(output).is_ok());
    }
}

struct Browser {
    stub: Arc<Stub>,
    inputs: mpsc::UnboundedReceiver<Value>,
    acks: mpsc::UnboundedReceiver<()>,
}

/// A server that hosts browser tabs and the double behind it, and the page's stand-in.
async fn serving() -> (Fixture, Browser) {
    let fixture = Fixture::start_offering(json!({"serverBrowser": true})).await;
    let (inputs_sender, inputs) = mpsc::unbounded_channel();
    let (acks_sender, acks) = mpsc::unbounded_channel();
    let stub = Arc::new(Stub {
        manager: fixture.store.extension(PreviewManager::default),
        plans: Mutex::default(),
        attached: Mutex::default(),
        queues: Mutex::default(),
        inputs: inputs_sender,
        acks: acks_sender,
        answered: Mutex::default(),
        download: Mutex::default(),
        served: Mutex::default(),
    });
    CURRENT.with(|current| *current.borrow_mut() = Some(stub.clone()));
    t3_server::server_browser_http::install(&fixture.store, stub.clone());
    TEST_PAGE.with(|page| *page.borrow_mut() = Some(Box::new(driver)));
    (fixture, Browser { stub, inputs, acks })
}
fn done() {
    CURRENT.with(|current| *current.borrow_mut() = None);
    TEST_PAGE.with(|page| *page.borrow_mut() = None);
}

/// The page's script, as a task: it reports a size, holds the socket it is told to connect to, acks every frame, forwards the server's
/// words and its close, and sends only what the control it was last told of allows.
fn driver(events: futures_channel::mpsc::UnboundedSender<PageEvent>, mut commands: futures_channel::mpsc::UnboundedReceiver<PageCmd>) {
    let _ = events.unbounded_send(PageEvent::Size { width: 800.0, height: 600.0, ratio: 1.0 });
    tokio::spawn(async move {
        let control: Arc<Mutex<Option<Value>>> = Arc::default();
        let mut sink: Option<Arc<tokio::sync::Mutex<futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Wire>>>> = None;
        let mut reader: Option<tokio::task::JoinHandle<()>> = None;
        while let Some(command) = commands.next().await {
            match command {
                PageCmd::Connect(url) => {
                    if let Some(task) = reader.take() {
                        task.abort();
                    }
                    sink = None;
                    *control.lock().unwrap() = None;
                    match tokio_tungstenite::connect_async(url).await {
                        Err(_) => {
                            let _ = events.unbounded_send(PageEvent::Close { code: 1006, reason: String::new(), opened: false });
                        }
                        Ok((socket, _)) => {
                            let _ = events.unbounded_send(PageEvent::Open);
                            let (writer, mut stream) = socket.split();
                            let writer = Arc::new(tokio::sync::Mutex::new(writer));
                            sink = Some(writer.clone());
                            let (events, control) = (events.clone(), control.clone());
                            reader = Some(tokio::spawn(async move {
                                let mut framed = false;
                                while let Some(message) = stream.next().await {
                                    match message {
                                        Ok(Wire::Binary(_)) => {
                                            let _ = writer.lock().await.send(Wire::Text(r#"{"type":"ack"}"#.into())).await;
                                            if !framed {
                                                framed = true;
                                                let _ = events.unbounded_send(PageEvent::Frame);
                                            }
                                        }
                                        Ok(Wire::Text(text)) => {
                                            if let Ok(message) = serde_json::from_str::<Value>(text.as_str()) {
                                                if message["type"] == "control" {
                                                    *control.lock().unwrap() = Some(message);
                                                }
                                            }
                                            let _ = events.unbounded_send(PageEvent::Text(text.to_string()));
                                        }
                                        Ok(Wire::Close(frame)) => {
                                            let (code, reason) = frame.map_or((1005, String::new()), |frame| (u16::from(frame.code), frame.reason.to_string()));
                                            let _ = events.unbounded_send(PageEvent::Close { code, reason, opened: true });
                                            return;
                                        }
                                        Ok(_) => {}
                                        Err(_) => break,
                                    }
                                }
                                let _ = events.unbounded_send(PageEvent::Close { code: 1006, reason: String::new(), opened: true });
                            }));
                        }
                    }
                }
                PageCmd::Send(message) => {
                    let allowed = control.lock().unwrap().as_ref().is_some_and(|control| control["canOperate"] == true && (message["type"] == "takeControl" || control["controller"] == "you"));
                    if let (true, Some(sink)) = (allowed, sink.as_ref()) {
                        let _ = sink.lock().await.send(Wire::Text(message.to_string().into())).await;
                    }
                }
                PageCmd::Stop => break,
            }
        }
    });
}

async fn ready(fixture: &Fixture) -> (VirtualDom, super::diff_tests::Harness, Store<UiModel>) {
    MEMORY.with(|memory| memory.borrow_mut().clear());
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead, AuthEnvironmentScope::PreviewOperate]);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    dom.in_scope(ScopeId::APP, || spawn(crate::right_panel::hydrate(state)));
    crate::runtime::diff_tests::pump(&mut dom).await;
    (dom, props, state)
}

/// What the double was sent, waiting for the one named.
async fn input_of(browser: &mut Browser, kind: &str) -> Value {
    tokio::time::timeout(HANG_BOUND, async {
        loop {
            let message = browser.inputs.recv().await.expect("the double is alive");
            if message["type"] == kind {
                return message;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the page never sent a {kind} message"))
}

/// Opens the Browser tab from the add menu and waits until its stream is up and says nobody has control.
async fn open_browser(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>) {
    open_thread(dom, props, state, "thread-a").await;
    assert_eq!(press(dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(dom, "the launcher", |dom| control(dom, "Open Browser").is_some()).await;
    click_when_enabled(dom, "Open Browser").await;
    until(dom, "the browser tab", |dom| control(dom, "Search or enter URL").is_some()).await;
    until(dom, "the stream", |dom| rendered_text(dom).contains("Watching")).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_new_browser_tab_streams_the_page_hands_over_control_and_goes_where_the_address_says() {
    let (fixture, mut browser) = serving().await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_browser(&mut dom, &props, state).await;

    // The viewer attached with the ticket it was given and asked for a frame as large as its box (device pixels).
    let attached = browser.stub.attached.lock().unwrap().clone();
    assert_eq!(attached.len(), 1);
    assert_eq!((attached[0].thread_id.as_str(), attached[0].max_width, attached[0].max_height, attached[0].can_operate), ("thread-a", 800, 600, true));
    assert!(attached[0].tab_id.starts_with("tab_"), "{:?}", attached[0]);
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    assert_eq!(state.peek().right_panel.panels.get(&format!("{environment}:thread-a")).surfaces.len(), 1, "one tab for the session, not also the empty one");

    // Nobody has control: the person takes it, and the address bar (disabled until then) works.
    assert!(rendered_text(&dom).contains("Take control") || control(&dom, "Take control").is_some());
    click_when_enabled(&mut dom, "Take control").await;
    assert_eq!(input_of(&mut browser, "takeControl").await["type"], "takeControl");
    until(&mut dom, "control", |dom| rendered_text(dom).contains("You have control") && control(dom, "Release control").is_some()).await;
    input_when_enabled(&mut dom, "Search or enter URL", "localhost:5173").await;
    key_on(&mut dom, "Search or enter URL", Key::Enter);
    assert_eq!(input_of(&mut browser, "navigate").await["url"], "http://localhost:5173/");
    // The browser reported where it landed; the server's event moves the tab: its title, its address, and Back.
    until(&mut dom, "the title", |dom| control(dom, "Close Dev app tab").is_some()).await;
    until(&mut dom, "back", |dom| control(dom, "Back").is_some() && crate::runtime::transport_tests::control_attribute(dom, "Back", "disabled").and_then(|(_, value)| value).as_deref() != Some("true")).await;
    assert_eq!(crate::runtime::transport_tests::control(&dom, "Search or enter URL").and_then(|(_, value)| value).as_deref(), Some("http://localhost:5173/"));

    // An invalid address is refused where it is typed.
    input_when_enabled(&mut dom, "Search or enter URL", "ftp://files.example.com").await;
    key_on(&mut dom, "Search or enter URL", Key::Enter);
    until(&mut dom, "the refusal", |dom| rendered_text(dom).contains("Invalid preview URL (unsupported-protocol: ftp:")).await;

    // A frame is acked once it is shown; the first one ends the loading state.
    assert!(control(&dom, "Stop").is_some(), "loading until the first frame");
    let acked = browser.stub.acks.clone();
    browser.stub.push(ViewerOutput::Frame { data: vec![0xff, 0xd8, 0xff, 0xd9], ack: Ack::of(Box::pin(async move { let _ = acked.send(()); })) });
    tokio::time::timeout(HANG_BOUND, browser.acks.recv()).await.expect("the frame was acked").unwrap();
    until(&mut dom, "the first frame", |dom| control(dom, "Refresh").is_some()).await;

    // Giving control back is a message too.
    click_when_enabled(&mut dom, "Release control").await;
    input_of(&mut browser, "releaseControl").await;
    until(&mut dom, "watching again", |dom| rendered_text(dom).contains("Watching") && control(dom, "Take control").is_some()).await;
    done();
    fixture.finish(dom).await;
}

/// Opens the tab and leaves the stream to the test (a host that cannot run a browser is the answer it waits for).
async fn open_browser_tab(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>) {
    open_thread(dom, props, state, "thread-a").await;
    assert_eq!(press(dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(dom, "the launcher", |dom| control(dom, "Open Browser").is_some()).await;
    click_when_enabled(dom, "Open Browser").await;
    until(dom, "the browser tab", |dom| control(dom, "Search or enter URL").is_some()).await;
}
fn sessions(browser: &Browser) -> Vec<Value> {
    let list = browser.stub.manager.list(serde_json::from_value(json!({"threadId": "thread-a"})).unwrap());
    serde_json::to_value(list).unwrap()["sessions"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "current_thread")]
async fn the_menu_changes_the_tab_for_every_client_through_the_server() {
    let (fixture, browser) = serving().await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_browser(&mut dom, &props, state).await;
    click_control(&mut dom, "Preview menu");
    click_control(&mut dom, "Appearance Dark");
    until(&mut dom, "the server's answer", |_| sessions(&browser)[0]["colorScheme"] == "dark").await;
    // The menu closed with the choice; the tab's state is what the server says.
    click_control(&mut dom, "Preview menu");
    until(&mut dom, "dark checked", |dom| crate::runtime::transport_tests::control_attribute(dom, "Appearance Dark", "aria-checked").and_then(|(_, value)| value).as_deref() == Some("true")).await;
    click_control(&mut dom, "Zoom in");
    until(&mut dom, "110%", |dom| rendered_text(dom).contains("110%")).await;
    click_control(&mut dom, "Zoom in");
    click_control(&mut dom, "Zoom out");
    until(&mut dom, "back to 110%", |_| sessions(&browser)[0]["zoomFactor"] == 1.1).await;
    click_control(&mut dom, "Reset zoom");
    until(&mut dom, "100%", |dom| rendered_text(dom).contains("100%")).await;
    // Everything else in it is a request of its own, refused or not by the server.
    click_control(&mut dom, "Hard reload");
    click_control(&mut dom, "Preview menu");
    click_control(&mut dom, "Clear cookies");
    click_control(&mut dom, "Preview menu");
    click_control(&mut dom, "Show device toolbar");
    until(&mut dom, "a fixed size", |_| sessions(&browser)[0]["viewport"]["_tag"] == "freeform").await;
    click_control(&mut dom, "Preview menu");
    until(&mut dom, "the menu says how to give the page the panel again", |dom| control(dom, "Hide device toolbar").is_some()).await;
    assert!(!rendered_text(&dom).contains("Unable to change"), "{}", rendered_text(&dom));
    click_control(&mut dom, "Hide device toolbar");
    until(&mut dom, "fill again", |_| sessions(&browser)[0]["viewport"]["_tag"] == "fill").await;
    done();
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn closing_a_browser_tab_closes_its_session_and_reopening_brings_it_back_at_its_address() {
    let (fixture, mut browser) = serving().await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_browser(&mut dom, &props, state).await;
    click_when_enabled(&mut dom, "Take control").await;
    until(&mut dom, "control", |dom| control(dom, "Release control").is_some()).await;
    input_when_enabled(&mut dom, "Search or enter URL", "localhost:5173/app").await;
    key_on(&mut dom, "Search or enter URL", Key::Enter);
    input_of(&mut browser, "navigate").await;
    until(&mut dom, "the title", |dom| control(dom, "Close Dev app tab").is_some()).await;

    click_control(&mut dom, "Close Dev app tab");
    crate::runtime::keys_tests::poll_until(&mut dom, "the session is gone", |_| sessions(&browser).is_empty()).await;
    until(&mut dom, "the panel is gone", |dom| control(dom, "Search or enter URL").is_none()).await;
    let remembered = state.peek().right_panel.history.entries.first().and_then(|entry| entry.browser.clone());
    assert_eq!(remembered.and_then(|browser| browser.url).as_deref(), Some("http://localhost:5173/app"));

    assert_eq!(press(&mut dom, chord("t", true, false), Dom::default()).await.as_deref(), Some("view.reopenClosed"));
    until(&mut dom, "the new session", |dom| control(dom, "Search or enter URL").is_some() && sessions(&browser).len() == 1).await;
    assert_eq!(sessions(&browser)[0]["navStatus"]["url"], "http://localhost:5173/app");
    until(&mut dom, "its address", |dom| control(dom, "Search or enter URL").and_then(|(_, value)| value).as_deref() == Some("http://localhost:5173/app")).await;
    assert!(!crate::right_panel::can_reopen_peek(state), "the entry is used up");
    done();
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_session_the_agent_opens_is_a_tab_unless_it_is_not_to_be_shown() {
    let (fixture, browser) = serving().await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_browser(&mut dom, &props, state).await;
    let open = |reveal: bool| browser.stub.manager.open(serde_json::from_value(json!({"threadId": "thread-a", "runtime": "server", "url": "http://agent.test/", "reveal": reveal})).unwrap()).unwrap();
    open(false);
    let shown = open(true);
    let tab = shown.tab_id.as_str().to_owned();
    until(&mut dom, "the agent's tab", |dom| control(dom, "Close agent.test tab").is_some()).await;
    assert_eq!(crate::runtime::transport_tests::all_controls(&dom, "Close agent.test tab").len(), 1, "the hidden one is not a tab");
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    let ids = state.peek().right_panel.panels.get(&format!("{environment}:thread-a")).surfaces.iter().map(|surface| surface.id()).collect::<Vec<_>>();
    assert!(ids.contains(&format!("browser:{tab}")) && ids.len() == 2, "{ids:?}");
    done();
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_host_that_cannot_run_the_browser_says_what_to_run_and_tries_again_when_asked() {
    let (fixture, browser) = serving().await;
    browser.stub.plans.lock().unwrap().push_back(Plan::NeedsHost(PreviewBrowserHostError::Libraries { setup_command: "sudo npx t3 browser setup".into(), libraries: vec!["libnss3".into()] }));
    let (mut dom, props, state) = ready(&fixture).await;
    open_browser_tab(&mut dom, &props, state).await;
    until(&mut dom, "the setup", |dom| rendered_text(dom).contains("missing libraries its browser needs") && rendered_text(dom).contains("sudo npx t3 browser setup")).await;
    // Nothing retries on its own: the person runs the command and asks.
    assert_eq!(browser.stub.attached.lock().unwrap().len(), 1);
    click_control(&mut dom, "Try again");
    until(&mut dom, "the stream", |dom| rendered_text(dom).contains("Watching") && !rendered_text(dom).contains("missing libraries")).await;
    assert_eq!(browser.stub.attached.lock().unwrap().len(), 2);
    done();
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_tab_the_server_has_closed_says_so_and_stops() {
    let (fixture, browser) = serving().await;
    browser.stub.plans.lock().unwrap().push_back(Plan::Gone);
    let (mut dom, props, state) = ready(&fixture).await;
    open_browser_tab(&mut dom, &props, state).await;
    until(&mut dom, "gone", |dom| rendered_text(dom).contains("This browser tab was closed.")).await;
    assert_eq!(browser.stub.attached.lock().unwrap().len(), 1, "a closed tab is not retried");
    done();
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_dialog_a_file_picker_and_a_download_of_the_page_are_answered_in_the_panel() {
    use dioxus_html::{SerializedFileData, SerializedFormData, SerializedFormObject};
    let (fixture, mut browser) = serving().await;
    let report = fixture.directory.path().join("report.csv");
    std::fs::write(&report, "a,b\n").unwrap();
    *browser.stub.download.lock().unwrap() = Some((report, "text/csv".into()));
    let (mut dom, props, state) = ready(&fixture).await;
    open_browser(&mut dom, &props, state).await;
    click_when_enabled(&mut dom, "Take control").await;
    until(&mut dom, "control", |dom| control(dom, "Release control").is_some()).await;

    // A prompt waits for an answer: the field starts with the page's default.
    browser.stub.push(ViewerOutput::Control { can_operate: true, controller: "you", generation: 3, dialog: Some(json!({"type": "prompt", "message": "Name?", "defaultValue": "Ada"})) });
    until(&mut dom, "the dialog", |dom| control(dom, "Browser dialog").is_some() && control(dom, "Dialog response").and_then(|(_, value)| value).as_deref() == Some("Ada")).await;
    assert!(rendered_text(&dom).contains("Name?"));
    crate::runtime::transport_tests::input_control(&mut dom, "Dialog response", "Grace");
    click_control(&mut dom, "Accept dialog");
    let answer = input_of(&mut browser, "dialog").await;
    assert_eq!((answer["accept"].as_bool(), answer["promptText"].as_str()), (Some(true), Some("Grace")));
    browser.stub.push(ViewerOutput::Control { can_operate: true, controller: "you", generation: 4, dialog: None });
    until(&mut dom, "no dialog", |dom| control(dom, "Browser dialog").is_none()).await;

    // The page's picker: the person chooses a file on this device and it goes to the page.
    browser.stub.push(ViewerOutput::FileChooser { id: "c1".into(), multiple: false, accept: ".txt".into() });
    until(&mut dom, "the picker", |dom| control(dom, "Choose files for the page").is_some() && rendered_text(dom).contains("The page asks for a file.")).await;
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(&dom, "Choose a file").expect("the file input");
    let file = SerializedFileData { path: PathBuf::from("/picked/notes.txt"), size: 5, last_modified: 0, content_type: Some("text/plain".into()), contents: Some(b"hello".to_vec().into()) };
    let form = SerializedFormData::new(String::new(), vec![SerializedFormObject { key: "files".into(), text: None, file: Some(file) }]);
    dom.runtime().handle_event("change", dioxus::dioxus_core::Event::new(Rc::new(dioxus_html::PlatformEventData::new(Box::new(form))) as Rc<dyn std::any::Any>, true), element);
    crate::runtime::diff_tests::pump(&mut dom).await;
    crate::runtime::keys_tests::poll_until(&mut dom, "the upload", |_| !browser.stub.answered.lock().unwrap().is_empty()).await;
    assert_eq!(browser.stub.answered.lock().unwrap()[0], ("c1".to_owned(), vec![("notes.txt".to_owned(), "text/plain".to_owned(), b"hello".to_vec())]));
    until(&mut dom, "the picker is answered", |dom| control(dom, "Choose files for the page").is_none()).await;

    // A file the page downloaded is offered; saving it reads it from the environment.
    browser.stub.push(ViewerOutput::Download { id: "d1".into(), file_name: "report.csv".into(), size_bytes: 4 });
    until(&mut dom, "the download", |dom| rendered_text(dom).contains("Downloaded report.csv")).await;
    click_control(&mut dom, "Save report.csv");
    crate::runtime::keys_tests::poll_until(&mut dom, "the download to be read", |_| !browser.stub.served.lock().unwrap().is_empty()).await;
    assert_eq!(browser.stub.served.lock().unwrap().as_slice(), ["d1"]);
    crate::runtime::keys_tests::poll_until(&mut dom, "the file to be kept", |_| !crate::media_save::SAVED.with(|saved| saved.borrow().is_empty())).await;
    assert_eq!(crate::media_save::SAVED.with(|saved| saved.borrow().clone()), vec![("report.csv".to_owned(), b"a,b\n".to_vec())]);
    click_control(&mut dom, "Dismiss report.csv");
    until(&mut dom, "dismissed", |dom| !rendered_text(dom).contains("Downloaded report.csv")).await;
    done();
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn without_a_browser_host_or_the_grant_the_row_is_off_and_a_tab_says_why() {
    // A server with no browser: the row says so, and a tab that is there anyway says the same.
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert_eq!(press(&mut dom, chord("b", false, true), Dom::default()).await.as_deref(), Some("rightPanel.toggle"));
    until(&mut dom, "the launcher", |dom| control(dom, "Open a surface").is_some()).await;
    assert!(control(&dom, "Open Browser").is_none() && rendered_text(&dom).contains("Only available in the desktop app."));
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    state.right_panel().write().panels.open_preview(&format!("{environment}:thread-a"), None);
    until(&mut dom, "the line", |dom| rendered_text(dom).contains("Preview is only available in the T3 Code desktop app.")).await;
    fixture.finish(dom).await;

    // A server with one, and a client paired without the grant to control it.
    let (fixture, _browser) = serving().await;
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    state.right_panel().write().panels.open_preview(&format!("{environment}:thread-a"), None);
    until(&mut dom, "the line", |dom| rendered_text(dom).contains("Pair this client again with preview access to control browser previews.")).await;
    done();
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_browser_shortcuts_show_hide_and_drive_the_tab_that_is_shown() {
    let (fixture, mut browser) = serving().await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    // Nothing to refresh or zoom while no browser is shown: those keys are the page's own.
    assert_eq!(press(&mut dom, chord("r", false, false), Dom::default()).await, None);
    assert_eq!(press(&mut dom, chord("j", true, false), Dom::default()).await.as_deref(), Some("preview.toggle"));
    until(&mut dom, "a new tab", |dom| control(dom, "Search or enter URL").is_some()).await;
    until(&mut dom, "the stream", |dom| rendered_text(dom).contains("Watching")).await;
    click_when_enabled(&mut dom, "Take control").await;
    until(&mut dom, "control", |dom| control(dom, "Release control").is_some()).await;

    // Reload goes through the stream; zoom is the environment's, so it goes through the server.
    assert_eq!(press(&mut dom, chord("r", false, false), Dom::default()).await.as_deref(), Some("preview.refresh"));
    crate::runtime::transport_tests::drive_future(&mut dom, "the reload", input_of(&mut browser, "reload")).await;
    assert_eq!(press(&mut dom, chord("=", false, false), Dom::default()).await.as_deref(), Some("preview.zoomIn"));
    until(&mut dom, "110%", |_| sessions(&browser)[0]["zoomFactor"] == 1.1).await;
    assert_eq!(press(&mut dom, chord("-", false, false), Dom::default()).await.as_deref(), Some("preview.zoomOut"));
    until(&mut dom, "100% again", |dom| rendered_text(dom).contains("100%") || sessions(&browser)[0].get("zoomFactor").is_some_and(|zoom| *zoom == 1.0)).await;
    assert_eq!(press(&mut dom, chord("=", false, false), Dom::default()).await.as_deref(), Some("preview.zoomIn"));
    assert_eq!(press(&mut dom, chord("0", false, false), Dom::default()).await.as_deref(), Some("preview.resetZoom"));
    assert_eq!(press(&mut dom, chord("l", false, false), Dom::default()).await.as_deref(), Some("preview.focusUrl"));
    // In the composer the keys stay the composer's.
    assert_eq!(press(&mut dom, chord("r", false, false), Dom { composer_focus: true, editable_focus: true, ..Default::default() }).await, None);

    // The toggle hides the panel and brings the same tab back, not another.
    assert_eq!(press(&mut dom, chord("j", true, false), Dom::default()).await.as_deref(), Some("preview.toggle"));
    until(&mut dom, "hidden", |dom| control(dom, "Search or enter URL").is_none()).await;
    assert_eq!(sessions(&browser).len(), 1);
    assert_eq!(press(&mut dom, chord("j", true, false), Dom::default()).await.as_deref(), Some("preview.toggle"));
    until(&mut dom, "shown again", |dom| control(dom, "Search or enter URL").is_some()).await;
    assert_eq!(sessions(&browser).len(), 1);
    done();
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_browser_key_says_so_where_the_environment_hosts_none() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = ready(&fixture).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert_eq!(press(&mut dom, chord("j", true, false), Dom::default()).await.as_deref(), Some("preview.toggle"));
    until(&mut dom, "the notice", |dom| rendered_text(dom).contains("Preview is desktop-only")).await;
    assert!(rendered_text(&dom).contains("Open T3 Code in the desktop app to use the in-app preview."));
    fixture.finish(dom).await;
}

#[path = "preview_real_tests.rs"]
mod real;
