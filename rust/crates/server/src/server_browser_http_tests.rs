//! The stream, download and upload routes (apps/server/src/preview/ServerBrowserStream.test.ts) against a browser the test plays, over a real
//! server on loopback with real authentication: who may attach, what a viewer sends and gets, and the answers when the browser is off.
use super::*;
use crate::{
    auth::AuthService,
    persistence::Store,
    preview_browser_host::PreviewBrowserHostError,
    server_browser::{LaunchCause, ServerBrowserError},
    server_browser_contexts::LaunchFailure,
    server_browser_viewer::{ViewerQueue, ViewerStream},
    test_support::HANG_BOUND,
    transport::{ApiState, router},
};
use async_trait::async_trait;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message as ClientMessage, client::IntoClientRequest};

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, future).await.expect("the awaited event happened")
}

#[derive(Debug, Clone, PartialEq)]
struct Attach {
    thread_id: String,
    tab_id: String,
    max_width: u32,
    max_height: u32,
    quality: u32,
    can_operate: bool,
}

enum Plan {
    /// A viewer that already has this output queued.
    Viewer(Vec<ViewerOutput>),
    Gone,
    NeedsHost(PreviewBrowserHostError),
}

struct Stub {
    plan: Mutex<Option<Plan>>,
    attached: Mutex<Vec<Attach>>,
    inputs: mpsc::UnboundedSender<Value>,
    download: Mutex<Option<(PathBuf, String)>>,
    chooser_answer: bool,
    answered: Mutex<Vec<(String, String, String, Vec<(String, String, Vec<u8>)>)>>,
}

struct StubViewer {
    queue: Arc<ViewerQueue>,
    inputs: mpsc::UnboundedSender<Value>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ViewerStream for StubViewer {
    async fn next(&self) -> Option<ViewerOutput> {
        self.queue.next().await
    }
    async fn input(&self, message: Value) {
        let _ = self.inputs.send(message);
    }
    async fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.queue.close();
    }
}

#[async_trait]
impl ServerBrowserApi for Stub {
    async fn attach_viewer(&self, thread_id: &str, tab_id: &str, max_width: u32, max_height: u32, quality: u32, can_operate: bool) -> Result<Box<dyn ViewerStream>, ServerBrowserError> {
        self.attached.lock().unwrap().push(Attach { thread_id: thread_id.into(), tab_id: tab_id.into(), max_width, max_height, quality, can_operate });
        match self.plan.lock().unwrap().take().expect("a plan") {
            Plan::Viewer(outputs) => {
                let queue = Arc::new(ViewerQueue::default());
                for output in outputs {
                    assert!(queue.offer(output).is_ok());
                }
                Ok(Box::new(StubViewer { queue, inputs: self.inputs.clone(), closed: Arc::default() }))
            }
            Plan::Gone => Err(ServerBrowserError::TabNotFound { thread_id: thread_id.into(), tab_id: tab_id.into() }),
            Plan::NeedsHost(host) => Err(ServerBrowserError::Launch(LaunchCause::Browser(LaunchFailure::Host(host)))),
        }
    }
    async fn answer_file_chooser(&self, thread_id: &str, tab_id: &str, chooser_id: &str, files: Vec<UploadedFile>) -> bool {
        self.answered.lock().unwrap().push((thread_id.into(), tab_id.into(), chooser_id.into(), files.into_iter().map(|file| (file.name, file.mime_type, file.bytes)).collect()));
        self.chooser_answer
    }
    fn open_download(&self, thread_id: &str, tab_id: &str, download_id: &str) -> Option<(PathBuf, String)> {
        (thread_id == "thread" && tab_id == "tab" && download_id == "file").then(|| self.download.lock().unwrap().clone()).flatten()
    }
    async fn clear_profile(&self, _: &str) -> std::io::Result<()> {
        Ok(())
    }
}

struct Server {
    address: std::net::SocketAddr,
    stub: Arc<Stub>,
    inputs: mpsc::UnboundedReceiver<Value>,
    reader: String,
    operator: String,
    operate_only: String,
    directory: tempfile::TempDir,
    _server: tokio::task::JoinHandle<()>,
}

async fn server(installed: bool, plan: Plan, chooser_answer: bool) -> Server {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("rust-state.sqlite")).unwrap();
    let auth = AuthService::new(store.clone(), [46; 32], "preview_stream_fixture".into(), "loopback-browser".into()).unwrap();
    let grant = |scopes| auth.issue_session("fixture", "bearer-access-token", scopes, json!({"deviceType":"unknown"}), Utc::now(), chrono::Duration::hours(1)).unwrap().1;
    let reader = grant(vec![AuthEnvironmentScope::OrchestrationRead]);
    let operator = grant(vec![AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let operate_only = grant(vec![AuthEnvironmentScope::OrchestrationOperate]);
    let (inputs_sender, inputs) = mpsc::unbounded_channel();
    let download_path = directory.path().join("report.csv");
    std::fs::write(&download_path, b"a,b\n1,2\n").unwrap();
    let stub = Arc::new(Stub {
        plan: Mutex::new(Some(plan)),
        attached: Mutex::default(),
        inputs: inputs_sender,
        download: Mutex::new(Some((download_path, "my report.csv".into()))),
        chooser_answer,
        answered: Mutex::default(),
    });
    if installed {
        install(&store, stub.clone());
    }
    let state = ApiState {
        store,
        auth,
        environment: json!({"environmentId":"preview-stream","label":"Preview stream","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{}}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: None,
        execution: None,
        workspace: None,
        terminals: None,
        discovery: None,
        resource_telemetry: None,
        host_resources: None,
        background: None,
        device_hosts: None,
        devices: None,
        provider_auth: None,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = router(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { address, stub, inputs, reader, operator, operate_only, directory, _server: task }
}

fn request(address: std::net::SocketAddr, query: &str, token: Option<&str>) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut request = format!("ws://{address}/api/preview-stream/ws?{query}").into_client_request().unwrap();
    if let Some(token) = token {
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    }
    request
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn http(server: &Server, path: &str) -> String {
    format!("http://{}{path}", server.address)
}

#[tokio::test]
async fn with_the_browser_off_every_route_is_a_plain_not_found_before_any_credential_is_read() {
    let server = server(false, Plan::Gone, true).await;
    let websocket = tokio_tungstenite::connect_async(request(server.address, "threadId=thread&tabId=tab", None)).await;
    match websocket {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 404),
        other => panic!("{other:?}"),
    }
    for (method, path) in [("GET", "/api/preview-stream/download?threadId=thread&tabId=tab&id=file"), ("POST", "/api/preview-stream/upload?threadId=thread&tabId=tab&chooser=c"), ("GET", "/api/preview-stream/anything")] {
        let response = client().request(method.parse().unwrap(), http(&server, path)).send().await.unwrap();
        assert_eq!(response.status(), 404, "{method} {path}");
        assert_eq!(response.text().await.unwrap(), "Not Found");
    }
    // Even with a credential that would have been enough.
    let response = client().get(http(&server, "/api/preview-stream/download?threadId=thread&tabId=tab&id=file")).bearer_auth(&server.operator).send().await.unwrap();
    assert_eq!(response.status(), 404);
    assert!(server.stub.attached.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_viewer_gets_frames_and_state_and_only_an_operator_may_send_input() {
    let mutations = vec![
        json!({"type": "resize", "width": 390, "height": 844}),
        json!({"type": "mouse", "action": "down", "x": 10, "y": 10, "button": "left", "buttons": 1}),
        json!({"type": "key", "action": "down", "key": "Enter"}),
        json!({"type": "navigate", "url": "https://example.com"}),
        json!({"type": "takeControl"}),
    ];
    for (name, interactive, expect_input) in [("read only", true, false), ("operator", true, true), ("operator opted out", false, false)] {
        let mut server = server(true, Plan::Gone, true).await;
        let (acked, mut acks) = mpsc::unbounded_channel();
        let frame = vec![255, 216, 255, 217];
        *server.stub.plan.lock().unwrap() = Some(Plan::Viewer(vec![
            ViewerOutput::Viewport { width: 1280, height: 800 },
            ViewerOutput::Frame { data: frame.clone(), ack: Ack::of(Box::pin(async move { let _ = acked.send(()); })) },
        ]));
        let token = if name == "read only" { server.reader.clone() } else { server.operator.clone() };
        let query = format!("threadId=thread&tabId=tab&maxWidth=1000&quality=55{}", if interactive { "" } else { "&interactive=false" });
        let (mut socket, _) = within(tokio_tungstenite::connect_async(request(server.address, &query, Some(&token)))).await.unwrap();
        let mut texts = Vec::new();
        let received = within(async {
            loop {
                match socket.next().await.unwrap().unwrap() {
                    ClientMessage::Text(text) => texts.push(serde_json::from_str::<Value>(text.as_str()).unwrap()),
                    ClientMessage::Binary(bytes) => break bytes.to_vec(),
                    _ => {}
                }
            }
        })
        .await;
        assert_eq!((received, texts), (frame, vec![json!({"type": "viewport", "width": 1280, "height": 800})]), "{name}");
        for message in &mutations {
            socket.send(ClientMessage::Text(message.to_string().into())).await.unwrap();
        }
        socket.send(ClientMessage::Text("malformed input".into())).await.unwrap();
        // The ack follows every mutation on the socket, so the frame's release also proves they were all read.
        socket.send(ClientMessage::Text(json!({"type": "ack"}).to_string().into())).await.unwrap();
        within(acks.recv()).await.unwrap();
        if expect_input {
            let mut got = Vec::new();
            for _ in 0..mutations.len() + 1 {
                got.push(within(server.inputs.recv()).await.unwrap());
            }
            // Each is forwarded in its own task, so only the set is certain; a message that is not JSON arrives as null.
            for message in mutations.iter().chain([&Value::Null]) {
                assert!(got.contains(message), "{name}: {message}");
            }
        } else {
            assert!(server.inputs.try_recv().is_err(), "{name}");
        }
        assert_eq!(
            *server.stub.attached.lock().unwrap(),
            [Attach { thread_id: "thread".into(), tab_id: "tab".into(), max_width: 1000, max_height: 800, quality: 55, can_operate: expect_input }],
            "{name}"
        );
    }
}

#[tokio::test]
async fn an_unauthorized_connection_is_refused_before_a_viewer_attaches() {
    let server = server(true, Plan::Gone, true).await;
    for (token, status) in [(None, 401), (Some(server.operate_only.as_str()), 403)] {
        match tokio_tungstenite::connect_async(request(server.address, "threadId=thread&tabId=tab", token)).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), status),
            other => panic!("{other:?}"),
        }
    }
    assert!(server.stub.attached.lock().unwrap().is_empty());
    // A request that names no tab is not found, even with a credential.
    match tokio_tungstenite::connect_async(request(server.address, "threadId=thread", Some(&server.reader))).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 404),
        other => panic!("{other:?}"),
    }
    assert!(server.stub.attached.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_tab_that_is_gone_is_a_close_code_clients_stop_on() {
    let server = server(true, Plan::Gone, true).await;
    let (mut socket, _) = within(tokio_tungstenite::connect_async(request(server.address, "threadId=thread&tabId=tab", Some(&server.reader)))).await.unwrap();
    match within(socket.next()).await.unwrap().unwrap() {
        ClientMessage::Close(Some(frame)) => assert_eq!((u16::from(frame.code), frame.reason.as_str()), (4404, "tab closed")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_host_that_needs_setup_tells_the_viewer_the_command_that_fixes_it() {
    for (error, need) in [
        (PreviewBrowserHostError::Sandbox { setup_command: "sudo t3 browser setup".into() }, "sandbox"),
        (PreviewBrowserHostError::Libraries { setup_command: "sudo t3 browser setup".into(), libraries: vec!["libnss3.so".into()] }, "libraries"),
    ] {
        let server = server(true, Plan::NeedsHost(error), true).await;
        let (mut socket, _) = within(tokio_tungstenite::connect_async(request(server.address, "threadId=thread&tabId=tab", Some(&server.reader)))).await.unwrap();
        match within(socket.next()).await.unwrap().unwrap() {
            ClientMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), PREVIEW_STREAM_HOST_SETUP_CLOSE_CODE);
                let reason: t3_contracts::PreviewStreamHostSetup = serde_json::from_str(frame.reason.as_str()).unwrap();
                assert_eq!(serde_json::to_value(reason).unwrap(), json!({"need": need, "command": "sudo t3 browser setup"}));
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn a_download_is_served_to_a_reader_as_an_attachment_and_to_no_one_else() {
    let server = server(true, Plan::Gone, true).await;
    let path = "/api/preview-stream/download?threadId=thread&tabId=tab&id=file";
    assert_eq!(client().get(http(&server, path)).send().await.unwrap().status(), 401);
    assert_eq!(client().get(http(&server, path)).bearer_auth(&server.operate_only).send().await.unwrap().status(), 403);
    let served = client().get(http(&server, path)).bearer_auth(&server.reader).send().await.unwrap();
    assert_eq!(served.status(), 200);
    let disposition = served.headers()["content-disposition"].to_str().unwrap().to_owned();
    assert!(disposition.starts_with("attachment;") && disposition.contains("my report.csv"), "{disposition}");
    assert_eq!(served.bytes().await.unwrap().as_ref(), b"a,b\n1,2\n");
    // Another tab's file, or one that does not exist, is not found.
    for other in ["/api/preview-stream/download?threadId=thread&tabId=other&id=file", "/api/preview-stream/download?threadId=thread&tabId=tab&id=missing", "/api/preview-stream/download"] {
        assert_eq!(client().get(http(&server, other)).bearer_auth(&server.reader).send().await.unwrap().status(), 404, "{other}");
    }
    assert!(server.directory.path().join("report.csv").exists());
}

fn multipart(parts: &[(&str, &str, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "----t3boundary";
    let mut body = Vec::new();
    for (name, file_name, bytes) in parts {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: {name}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[tokio::test]
async fn an_upload_answers_the_pages_picker_for_an_operator_and_reports_a_picker_that_is_gone() {
    for (answered, status) in [(true, 204), (false, 409)] {
        let server = server(true, Plan::Gone, answered).await;
        let path = "/api/preview-stream/upload?threadId=thread&tabId=tab&chooser=picker-1";
        let (content_type, body) = multipart(&[("text/plain", "a.txt", b"alpha"), ("image/png", "b.png", &[1, 2, 3])]);
        let upload = |token: Option<&str>| {
            let request = client().post(http(&server, path)).header("content-type", &content_type).body(body.clone());
            match token {
                Some(token) => request.bearer_auth(token),
                None => request,
            }
        };
        assert_eq!(upload(None).send().await.unwrap().status(), 401);
        assert_eq!(upload(Some(&server.reader)).send().await.unwrap().status(), 403);
        assert!(server.stub.answered.lock().unwrap().is_empty());
        let response = upload(Some(&server.operator)).send().await.unwrap();
        assert_eq!(response.status(), status);
        if !answered {
            assert_eq!(response.text().await.unwrap(), "The page's file picker is no longer open.");
        }
        let answers = server.stub.answered.lock().unwrap().clone();
        assert_eq!(
            answers,
            [("thread".to_owned(), "tab".to_owned(), "picker-1".to_owned(), vec![("a.txt".to_owned(), "text/plain".to_owned(), b"alpha".to_vec()), ("b.png".to_owned(), "image/png".to_owned(), vec![1, 2, 3])])]
        );
    }
}

#[tokio::test]
async fn no_files_cancels_the_pick_and_too_many_are_refused() {
    let server = server(true, Plan::Gone, true).await;
    let path = "/api/preview-stream/upload?threadId=thread&tabId=tab&chooser=picker-1";
    let (content_type, body) = multipart(&[]);
    let cancel = client().post(http(&server, path)).header("content-type", &content_type).body(body).bearer_auth(&server.operator).send().await.unwrap();
    assert_eq!(cancel.status(), 204);
    assert_eq!(server.stub.answered.lock().unwrap()[0].3, Vec::new());
    let many: Vec<(&str, &str, &[u8])> = (0..UPLOAD_MAX_FILES + 1).map(|_| ("text/plain", "f.txt", &b"x"[..])).collect();
    let (content_type, body) = multipart(&many);
    let refused = client().post(http(&server, path)).header("content-type", &content_type).body(body).bearer_auth(&server.operator).send().await.unwrap();
    assert_eq!(refused.status(), 413);
    assert_eq!(server.stub.answered.lock().unwrap().len(), 1);
    // A GET to the upload route is refused as the original's handler refuses it, and a method no route has is not found.
    assert_eq!(client().get(http(&server, path)).bearer_auth(&server.operator).send().await.unwrap().status(), 405);
    assert_eq!(client().put(http(&server, path)).bearer_auth(&server.operator).send().await.unwrap().status(), 404);
}
