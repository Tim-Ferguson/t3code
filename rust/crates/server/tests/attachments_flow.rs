#![cfg(unix)]
//! Real sockets and a real Codex-protocol process: upload URL, HTTP upload, claim by a
//! message, provider input, signed asset serving, and deletion, plus the refusal paths.
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use t3_contracts::AuthEnvironmentScope;
use t3_server::{
    attachment_service::AttachmentService,
    auth::AuthService,
    execution::ExecutionService,
    launch::ThreadLaunchService,
    persistence::{Store, StoredEvent},
    project::ProjectService,
    provider_registry::ProviderRegistry,
    transport::{ApiState, router},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    directory: tempfile::TempDir,
    address: std::net::SocketAddr,
    store: Store,
    execution: ExecutionService,
    thread_id: String,
    operator: String,
    reader: String,
    record: std::path::PathBuf,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

fn png(width: u32, height: u32, filler: u8) -> Vec<u8> {
    let mut bytes = vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52,
    ];
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.resize(40, filler);
    bytes
}

async fn harness() -> Harness {
    harness_for("codex").await
}

/// `codex` runs the recording Codex fixture; `acp` runs the registry-agent fixture.
async fn harness_for(driver: &str) -> Harness {
    let directory = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(directory.path()).unwrap();
    let store = Store::open(cwd.join("rust-state.sqlite")).unwrap();
    let workspace = cwd.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":workspace}), Utc::now()).unwrap();
    let record = cwd.join("codex-requests.jsonl");
    let binary =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-provider.py");
    let acp =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp-provider.py");
    let settings: t3_contracts::ServerSettings = serde_json::from_value(json!({"providerInstances":{
        "claudeAgent":{"driver":"claudeAgent","enabled":false},
        "codex":{"driver":"codex","enabled":driver=="codex","config":{"binaryPath":binary,"homePath":cwd.join("codex-home")},"environment":[{"name":"FIXTURE_RPC_RECORD","value":record}]},
        "local-agent":{"driver":"acpRegistry","enabled":driver=="acp","config":{"enabled":true,"source":"local","commandPath":"python3","commandArgs":[acp,"2","normal"]}}}})).unwrap();
    let providers = ProviderRegistry::discover(&settings, &cwd).await.unwrap();
    let execution = ExecutionService::start(store.clone(), providers.clone());
    let thread_id = ThreadLaunchService::new(store.clone())
        .launch(json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":if driver=="acp"{"local-agent"}else{"codex"},"model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}}), Utc::now())
        .unwrap()["threadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let auth = AuthService::new(
        store.clone(),
        [7; 32],
        "t3_session_attachments".into(),
        "loopback-browser".into(),
    )
    .unwrap();
    let issue = |scopes: Vec<AuthEnvironmentScope>| {
        auth.issue_session(
            "fixture",
            "bearer-access-token",
            scopes,
            json!({"deviceType":"unknown"}),
            Utc::now(),
            chrono::Duration::hours(1),
        )
        .unwrap()
        .1
    };
    let operator = issue(vec![
        AuthEnvironmentScope::OrchestrationRead,
        AuthEnvironmentScope::OrchestrationOperate,
    ]);
    let reader = issue(vec![AuthEnvironmentScope::OrchestrationRead]);
    let state = ApiState {
        store: store.clone(),
        auth,
        environment: json!({"environmentId":"attachments-fixture","label":"Fixture","platform":{"os":"linux","arch":"x64"},"serverVersion":"test","orchestrationProtocolVersion":2,"capabilities":{"repositoryIdentity":false,"connectionProbe":true}}),
        config: None,
        settings: None,
        cors_origins: None,
        assets: None,
        providers: Some(providers),
        execution: Some(execution.clone()),
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
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    Harness {
        directory,
        address,
        store,
        execution,
        thread_id,
        operator,
        reader,
        record,
        stop: Some(stop),
        server: Some(server),
    }
}

impl Harness {
    fn attachments(&self) -> std::path::PathBuf {
        std::fs::canonicalize(self.directory.path())
            .unwrap()
            .join("attachments")
    }
    fn listing(&self, prefix: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.attachments())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .filter(|name| name.starts_with(prefix))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
    async fn connect(&self, token: &str) -> Socket {
        let mut request = format!("ws://{}/ws?orchestrationProtocol=2", self.address)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {token}").parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }
    fn url(&self, relative: &str) -> String {
        format!("http://{}{relative}", self.address)
    }
    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        self.server.take().unwrap().await.unwrap();
        self.execution.shutdown().await;
    }
}

async fn rpc(socket: &mut Socket, id: u64, tag: &str, payload: Value) -> Value {
    socket
        .send(Message::Text(
            json!({"_tag":"Request","id":id,"tag":tag,"payload":payload,"headers":[]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    loop {
        let message = socket.next().await.unwrap().unwrap();
        let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
        if value["_tag"] == "Exit" && value["requestId"] == id {
            return value["exit"].clone();
        }
    }
}
fn success(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Success", "{exit}");
    &exit["value"]
}
fn failure(exit: &Value) -> &Value {
    assert_eq!(exit["_tag"], "Failure", "{exit}");
    &exit["cause"][0]
}

async fn upload(
    harness: &Harness,
    socket: &mut Socket,
    id: u64,
    input: Value,
    bytes: Vec<u8>,
) -> Value {
    let issued = success(&rpc(socket, id, "attachments.createUploadUrl", input).await).clone();
    let response = reqwest::Client::new()
        .post(harness.url(issued["relativeUrl"].as_str().unwrap()))
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204, "{}", response.text().await.unwrap());
    issued
}

async fn settled(
    store: &Store,
    events: &mut tokio::sync::broadcast::Receiver<Vec<StoredEvent>>,
    thread: &str,
    runs: usize,
) -> Value {
    let mut last = Value::Null;
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let view = store.projection("thread", thread).unwrap().unwrap();
            last = view["runs"].clone();
            if view["runs"].as_array().unwrap().len() >= runs
                && view["runs"].as_array().unwrap().iter().all(|run| {
                    matches!(
                        run["status"].as_str(),
                        Some("completed" | "failed" | "interrupted")
                    )
                })
            {
                return view;
            }
            events.recv().await.unwrap();
        }
    })
    .await;
    outcome.unwrap_or_else(|_| panic!("the provider turn did not settle: {last}"))
}

/// The first projection with a pending runtime request.
async fn awaiting(
    store: &Store,
    events: &mut tokio::sync::broadcast::Receiver<Vec<StoredEvent>>,
    thread: &str,
) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let view = store.projection("thread", thread).unwrap().unwrap();
            if view["runtimeRequests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|request| request["status"] == "pending")
            {
                return view;
            }
            events.recv().await.unwrap();
        }
    })
    .await
    .expect("the agent never asked for approval")
}

fn resource(resource: Value) -> Value {
    json!({"resource":resource})
}

fn dispatch(thread: &str, id: &str, attachments: Value) -> Value {
    json!({"type":"message.dispatch","commandId":id,"threadId":thread,"messageId":format!("message:{id}"),"text":"Describe these","attachments":attachments,"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}})
}

#[tokio::test]
async fn upload_claim_provider_input_serve_and_delete() {
    let harness = harness().await;
    let mut socket = harness.connect(&harness.operator).await;
    let image_bytes = png(3, 5, 7);
    let pdf_bytes = b"%PDF-1.4 fixture".to_vec();

    let image = upload(
        &harness,
        &mut socket,
        1,
        json!({"name":"shot.png","mimeType":"image/png","sizeBytes":image_bytes.len()}),
        image_bytes.clone(),
    )
    .await;
    let pdf = upload(&harness, &mut socket, 2, json!({"type":"file","name":"Report é.PDF","mimeType":"application/pdf","sizeBytes":pdf_bytes.len()}), pdf_bytes.clone()).await;
    let (image_id, pdf_id) = (
        image["attachmentId"].as_str().unwrap(),
        pdf["attachmentId"].as_str().unwrap(),
    );
    assert!(image_id.starts_with("pending-") && !image_id.contains("-png"));
    assert!(pdf_id.starts_with("pending-") && pdf_id.ends_with("-pdf"));
    assert_eq!(harness.listing("pending-").len(), 2);

    // A staged upload can already be previewed before any message claims it.
    let staged = success(
        &rpc(
            &mut socket,
            3,
            "assets.createUrl",
            json!({"resource":{"_tag":"attachment","attachmentId":image_id}}),
        )
        .await,
    )
    .clone();
    assert_eq!(staged["imageDimensions"], json!({"width":3,"height":5}));
    let served = reqwest::get(harness.url(staged["relativeUrl"].as_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(served.status(), 200);
    assert_eq!(served.bytes().await.unwrap().to_vec(), image_bytes);

    // Send a message that claims both uploads.
    let mut events = harness.store.subscribe();
    let attachments = json!([
        {"type":"image","id":image_id,"name":"shot.png","mimeType":"image/png","sizeBytes":image_bytes.len()},
        {"type":"file","id":pdf_id,"name":"Report é.PDF","mimeType":"application/pdf","sizeBytes":pdf_bytes.len()},
    ]);
    let exit = rpc(
        &mut socket,
        4,
        "orchestration.dispatchCommand",
        dispatch(&harness.thread_id, "send-1", attachments.clone()),
    )
    .await;
    assert!(success(&exit)["sequence"].is_number());
    let view = settled(&harness.store, &mut events, &harness.thread_id, 1).await;
    assert_eq!(view["runs"][0]["status"], "completed");
    let message = view["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap();
    let claimed = message["attachments"].as_array().unwrap();
    assert_eq!(claimed.len(), 2);
    let (claimed_image, claimed_pdf) = (
        claimed[0]["id"].as_str().unwrap(),
        claimed[1]["id"].as_str().unwrap(),
    );
    assert!(
        claimed_image.starts_with(&format!("{}-", harness.thread_id))
            && !claimed_image.starts_with("pending-"),
        "{claimed_image}"
    );
    assert!(claimed_pdf.ends_with("-pdf"));
    assert_eq!(
        std::fs::read(harness.attachments().join(format!("{claimed_image}.png"))).unwrap(),
        image_bytes
    );
    assert_eq!(
        std::fs::read(harness.attachments().join(format!("{claimed_pdf}.pdf"))).unwrap(),
        pdf_bytes
    );
    assert_eq!(
        harness.listing("pending-").len(),
        2,
        "staged uploads remain as the retry source"
    );
    let item = view["turnItems"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "user_message")
        .unwrap();
    assert_eq!(item["attachments"], message["attachments"]);

    // The provider received the notes and the image as a data URL; the pdf is path-only.
    let recorded: Vec<Value> = std::fs::read_to_string(&harness.record)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let turn = recorded
        .iter()
        .find(|request| request["method"] == "turn/start")
        .unwrap();
    let input = turn["params"]["input"].as_array().unwrap();
    assert_eq!(input.len(), 2, "{input:?}");
    let text = input[0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Describe these\n\n[Attached image \"shot.png\" is saved at: "),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "{}.png]",
            harness.attachments().join(claimed_image).display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "[Attached file \"Report é.PDF\" is saved at: {}.pdf]",
            harness.attachments().join(claimed_pdf).display()
        )),
        "{text}"
    );
    use base64::Engine;
    assert_eq!(input[1]["type"], "image");
    assert_eq!(
        input[1]["url"],
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&image_bytes)
        )
    );

    // Serving: images inline, pdf as a sandboxed download unless a viewer asks for inline.
    let claimed_url = success(&rpc(&mut socket, 5, "assets.createUrl", resource(json!({"_tag":"attachment","attachmentId":claimed_image,"fileName":"shot.png","mimeType":"image/png"}))).await).clone();
    assert_eq!(
        claimed_url["imageDimensions"],
        json!({"width":3,"height":5})
    );
    let served = reqwest::get(harness.url(claimed_url["relativeUrl"].as_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(served.status(), 200);
    assert_eq!(served.headers()["content-type"], "image/png");
    assert_eq!(served.headers()["x-content-type-options"], "nosniff");
    assert_eq!(served.bytes().await.unwrap().to_vec(), image_bytes);
    let head = reqwest::Client::new()
        .head(harness.url(claimed_url["relativeUrl"].as_str().unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        (
            head.status().as_u16(),
            head.headers()["content-length"].to_str().unwrap()
        ),
        (200, "40")
    );

    let download = success(&rpc(&mut socket, 6, "assets.createUrl", resource(json!({"_tag":"attachment","attachmentId":claimed_pdf,"fileName":"Report é.PDF","mimeType":"application/pdf"}))).await).clone();
    let served = reqwest::get(harness.url(download["relativeUrl"].as_str().unwrap()))
        .await
        .unwrap();
    assert!(
        served.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with(
                "attachment; filename=\"Report _.PDF\"; filename*=UTF-8''Report%20%C3%A9.PDF"
            )
    );
    assert_eq!(
        served.headers()["content-security-policy"],
        "default-src 'none'; sandbox"
    );
    assert_eq!(served.bytes().await.unwrap().to_vec(), pdf_bytes);
    let inline = success(
        &rpc(
            &mut socket,
            7,
            "assets.createUrl",
            resource(
                json!({"_tag":"attachment","attachmentId":claimed_pdf,"disposition":"inline"}),
            ),
        )
        .await,
    )
    .clone();
    let served = reqwest::get(harness.url(inline["relativeUrl"].as_str().unwrap()))
        .await
        .unwrap();
    assert_eq!(served.headers()["content-type"], "application/pdf");
    assert!(!served.headers().contains_key("content-disposition"));

    // Replaying the same command id claims fresh copies that are then released as unused.
    let before = harness.listing(&format!("{}-", harness.thread_id));
    let replay = rpc(
        &mut socket,
        12,
        "orchestration.dispatchCommand",
        dispatch(&harness.thread_id, "send-1", attachments),
    )
    .await;
    assert_eq!(success(&replay)["sequence"], success(&exit)["sequence"]);
    assert_eq!(harness.listing(&format!("{}-", harness.thread_id)), before);

    // Deleting only ever removes staged uploads.
    for (id, attachment) in [
        (8, claimed_image),
        (9, image_id),
        (10, image_id),
        (11, "../../outside"),
    ] {
        assert!(
            success(
                &rpc(
                    &mut socket,
                    id,
                    "attachments.delete",
                    json!({"attachmentId":attachment})
                )
                .await
            )
            .is_null()
        );
    }
    assert!(
        harness
            .attachments()
            .join(format!("{claimed_image}.png"))
            .exists()
    );
    assert_eq!(harness.listing("pending-").len(), 1);
    assert!(harness.listing("pending-")[0].contains("-pdf"));
    // A signed URL for a deleted file stops resolving.
    assert_eq!(
        reqwest::get(harness.url(staged["relativeUrl"].as_str().unwrap()))
            .await
            .unwrap()
            .status(),
        404
    );
    // A claimed attachment keeps serving through its own URL.
    assert_eq!(
        reqwest::get(harness.url(claimed_url["relativeUrl"].as_str().unwrap()))
            .await
            .unwrap()
            .status(),
        200
    );

    socket.close(None).await.unwrap();
    harness.finish().await;
}

#[tokio::test]
async fn refused_claims_and_dispatches_leave_no_stray_files() {
    let harness = harness().await;
    let mut socket = harness.connect(&harness.operator).await;
    let bytes = png(2, 2, 1);
    let staged = upload(
        &harness,
        &mut socket,
        1,
        json!({"name":"shot.png","mimeType":"image/png","sizeBytes":bytes.len()}),
        bytes.clone(),
    )
    .await;
    let id = staged["attachmentId"].as_str().unwrap();
    let attachment = |size: usize| json!({"type":"image","id":id,"name":"shot.png","mimeType":"image/png","sizeBytes":size});

    // Stored size differs from the declared size.
    let exit = rpc(
        &mut socket,
        2,
        "orchestration.dispatchCommand",
        dispatch(
            &harness.thread_id,
            "bad-size",
            json!([attachment(bytes.len() + 1)]),
        ),
    )
    .await;
    let error = failure(&exit)["error"].clone();
    assert_eq!(error["_tag"], "OrchestrationV2DispatchCommandError");
    assert_eq!(error["commandId"], "bad-size");
    assert_eq!(
        error["message"],
        "Attachment 'shot.png' cannot be sent: stored size does not match."
    );

    // Missing upload, duplicate refs, and the 100 attachment cap.
    let gone = json!({"type":"image","id":"pending-00000000-0000-4000-8000-0000000000ee","name":"gone.png","mimeType":"image/png","sizeBytes":4});
    let error = failure(
        &rpc(
            &mut socket,
            3,
            "orchestration.dispatchCommand",
            dispatch(&harness.thread_id, "gone", json!([gone])),
        )
        .await,
    )["error"]
        .clone();
    assert_eq!(
        error["message"],
        "Attachment 'gone.png' cannot be sent: attachment not found (removed or expired)."
    );
    let error = failure(
        &rpc(
            &mut socket,
            4,
            "orchestration.dispatchCommand",
            dispatch(
                &harness.thread_id,
                "dupe",
                json!([attachment(bytes.len()), attachment(bytes.len())]),
            ),
        )
        .await,
    )["error"]
        .clone();
    assert_eq!(
        error["message"],
        "Duplicate attachment ids are not allowed."
    );
    let many: Vec<Value> = (0..101).map(|index| json!({"type":"file","id":format!("existing-{index}"),"name":"x.txt","mimeType":"text/plain","sizeBytes":1})).collect();
    let error = failure(
        &rpc(
            &mut socket,
            5,
            "orchestration.dispatchCommand",
            dispatch(&harness.thread_id, "many", json!(many)),
        )
        .await,
    )["error"]
        .clone();
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("up to 100 files")
    );

    // A command the store refuses after claiming still releases its copies.
    let error = failure(
        &rpc(
            &mut socket,
            6,
            "orchestration.dispatchCommand",
            dispatch("no-such-thread", "ghost", json!([attachment(bytes.len())])),
        )
        .await,
    )["error"]
        .clone();
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("Thread not found"),
        "{error}"
    );
    assert_eq!(harness.listing("no-such-thread-"), Vec::<String>::new());
    assert_eq!(
        harness.listing(&format!("{}-", harness.thread_id)),
        Vec::<String>::new()
    );
    assert_eq!(
        harness.listing("pending-").len(),
        1,
        "the staged upload is untouched for a corrected retry"
    );
    assert!(
        harness
            .store
            .projection("thread", &harness.thread_id)
            .unwrap()
            .unwrap()["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Launching with an upload needs a thread id to claim into.
    let launch = json!({"commandId":"launch-with-upload","projectId":"project","title":"Launched","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"},"initialMessage":{"text":"hi","attachments":[attachment(bytes.len())]}});
    let error =
        failure(&rpc(&mut socket, 7, "orchestration.launchThread", launch).await)["error"].clone();
    assert_eq!(error["_tag"], "OrchestrationV2ThreadLaunchError");
    assert_eq!(
        error["message"],
        "Uploaded attachments need a thread id at launch."
    );

    socket.close(None).await.unwrap();
    harness.finish().await;
}

#[tokio::test]
async fn signed_urls_expire_and_reject_forgery_and_wrong_sizes() {
    let harness = harness().await;
    let mut socket = harness.connect(&harness.operator).await;
    let client = reqwest::Client::new();
    let bytes = png(1, 1, 3);
    let input = json!({"name":"shot.png","mimeType":"image/png","sizeBytes":bytes.len()});
    let issued =
        success(&rpc(&mut socket, 1, "attachments.createUploadUrl", input.clone()).await).clone();
    let relative = issued["relativeUrl"].as_str().unwrap();
    assert_eq!(
        issued["expiresAt"].as_i64().unwrap() - Utc::now().timestamp_millis() <= 600_000,
        true
    );

    // Wrong declared length, forged signature, and malformed tokens all store nothing.
    let short = client
        .post(harness.url(relative))
        .body(bytes[..10].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(
        (
            short.status().as_u16(),
            short.text().await.unwrap().as_str()
        ),
        (400, "Content-Length must match the upload size.")
    );
    let (payload, signature) = relative
        .rsplit_once('/')
        .unwrap()
        .1
        .split_once('.')
        .unwrap();
    for forged in [
        format!("{payload}x.{signature}"),
        format!("{payload}.{signature}.extra"),
        "garbage".into(),
        format!("{payload}.AAAA"),
    ] {
        let response = client
            .post(harness.url(&format!("/api/attachments/upload/{forged}")))
            .body(bytes.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(
            (
                response.status().as_u16(),
                response.text().await.unwrap().as_str()
            ),
            (404, "Not Found"),
            "{forged}"
        );
    }
    assert_eq!(
        client
            .post(harness.url("/api/attachments/upload/"))
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        client
            .get(harness.url(relative))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        client
            .post(harness.url("/api/assets/token/name.png"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(harness.listing(""), Vec::<String>::new());

    // Chunked bodies are accepted when their total equals the signed size, and rejected otherwise.
    let chunks = |parts: Vec<Vec<u8>>| {
        reqwest::Body::wrap_stream(futures_util::stream::iter(
            parts.into_iter().map(Ok::<_, std::io::Error>),
        ))
    };
    let over = client
        .post(harness.url(relative))
        .body(chunks(vec![bytes.clone(), vec![0]]))
        .send()
        .await
        .unwrap();
    assert_eq!(
        (over.status().as_u16(), over.text().await.unwrap().as_str()),
        (400, "Body was 41 bytes, expected 40.")
    );
    assert_eq!(
        harness.listing(""),
        Vec::<String>::new(),
        "no part file survives a rejected upload"
    );
    let (first, second) = bytes.split_at(13);
    let stored = client
        .post(harness.url(relative))
        .body(chunks(vec![first.to_vec(), second.to_vec()]))
        .send()
        .await
        .unwrap();
    assert_eq!(stored.status(), 204);
    assert_eq!(harness.listing("").len(), 1);

    // Expired upload and asset URLs answer 404 without detail.
    let files = AttachmentService::for_store(&harness.store).unwrap();
    let past = Utc::now().timestamp_millis() - 11 * 60_000;
    let expired = files
        .issue_upload_url(&serde_json::from_value(input).unwrap(), past)
        .unwrap();
    let response = client
        .post(harness.url(expired.relative_url.0.as_str()))
        .body(bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let id = issued["attachmentId"].as_str().unwrap();
    let resource: t3_contracts::AssetResource =
        serde_json::from_value(json!({"_tag":"attachment","attachmentId":id})).unwrap();
    let old = files
        .create_asset_url(&resource, Utc::now().timestamp_millis() - 61 * 60_000)
        .unwrap();
    assert_eq!(
        reqwest::get(harness.url(old["relativeUrl"].as_str().unwrap()))
            .await
            .unwrap()
            .status(),
        404
    );
    let live = files
        .create_asset_url(&resource, Utc::now().timestamp_millis())
        .unwrap();
    let live_url = live["relativeUrl"].as_str().unwrap();
    assert_eq!(
        reqwest::get(harness.url(live_url)).await.unwrap().status(),
        200
    );
    // The token is the credential: any trailing name serves the same asset, a missing name does not.
    let token = live_url
        .strip_prefix("/api/assets/")
        .unwrap()
        .split_once('/')
        .unwrap()
        .0;
    assert_eq!(
        reqwest::get(harness.url(&format!("/api/assets/{token}/anything.png")))
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        reqwest::get(harness.url(&format!("/api/assets/{token}")))
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        reqwest::get(harness.url(&format!("/api/assets/{token}x/a.png")))
            .await
            .unwrap()
            .status(),
        404
    );
    // Unknown attachment ids and traversal attempts have no URL to issue.
    for hostile in [
        "../../etc/passwd",
        "pending-00000000-0000-4000-8000-0000000000ab",
    ] {
        let exit = rpc(
            &mut socket,
            2,
            "assets.createUrl",
            json!({"resource":{"_tag":"attachment","attachmentId":hostile}}),
        )
        .await;
        assert_eq!(
            failure(&exit)["error"]["_tag"],
            "AssetAttachmentNotFoundError",
            "{hostile}"
        );
    }

    socket.close(None).await.unwrap();
    harness.finish().await;
}

#[tokio::test]
async fn scopes_decoding_and_inline_persistence_through_the_socket() {
    let harness = harness().await;
    let mut reader = harness.connect(&harness.reader).await;
    let mut operator = harness.connect(&harness.operator).await;

    // Reads may mint asset URLs; every mutating method needs the operate scope.
    let denied = rpc(
        &mut reader,
        1,
        "attachments.createUploadUrl",
        json!({"name":"a.png","mimeType":"image/png","sizeBytes":1}),
    )
    .await;
    assert_eq!(
        failure(&denied)["error"]["requiredPermission"],
        "orchestration:operate"
    );
    for (id, tag, payload) in [
        (2, "attachments.delete", json!({"attachmentId":"pending-x"})),
        (
            3,
            "assets.persistChatAttachments",
            json!({"threadId":"t","messageId":"m","attachments":[]}),
        ),
    ] {
        assert_eq!(
            failure(&rpc(&mut reader, id, tag, payload).await)["error"]["_tag"],
            "EnvironmentAuthorizationError",
            "{tag}"
        );
    }
    let unknown = rpc(
        &mut reader,
        4,
        "assets.createUrl",
        json!({"resource":{"_tag":"attachment","attachmentId":"nothing"}}),
    )
    .await;
    assert_eq!(
        failure(&unknown)["error"]["_tag"],
        "AssetAttachmentNotFoundError"
    );

    // Undecodable payloads die as schema errors rather than reaching the service.
    for (id, payload) in [
        (
            5,
            json!({"name":"a.png","mimeType":"image/png","sizeBytes":0}),
        ),
        (
            6,
            json!({"name":"a.svg","mimeType":"image/svg+xml","sizeBytes":4}),
        ),
        (
            7,
            json!({"type":"file","name":"big.bin","mimeType":"x/y","sizeBytes":52428801}),
        ),
    ] {
        assert_eq!(
            failure(&rpc(&mut operator, id, "attachments.createUploadUrl", payload).await)["_tag"],
            "Die"
        );
    }
    assert_eq!(
        failure(
            &rpc(
                &mut operator,
                8,
                "assets.createUrl",
                json!({"resource":{"_tag":"attachment","attachmentId":"x","disposition":"nope"}})
            )
            .await
        )["_tag"],
        "Die"
    );

    // A project icon is asked of a project that exists; an unknown folder has no workspace context.
    let favicon = rpc(
        &mut operator,
        9,
        "assets.createUrl",
        json!({"resource":{"_tag":"project-favicon","cwd":"/tmp"}}),
    )
    .await;
    assert_eq!(
        failure(&favicon)["error"]["_tag"],
        "AssetWorkspaceContextNotFoundError"
    );

    // Inline base64 images persist under ids derived from thread, message, and position.
    use base64::Engine;
    let bytes = png(4, 4, 9);
    let data_url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    let image = json!({"type":"image","name":"pasted.png","mimeType":"image/png","sizeBytes":bytes.len(),"dataUrl":data_url});
    let persist = |attachments: Value| json!({"threadId":harness.thread_id,"messageId":"message-persist","attachments":attachments});
    let first = success(
        &rpc(
            &mut operator,
            10,
            "assets.persistChatAttachments",
            persist(json!([image, image])),
        )
        .await,
    )
    .clone();
    let saved = first["attachments"].as_array().unwrap();
    assert_eq!(saved.len(), 2);
    assert_ne!(saved[0]["id"], saved[1]["id"]);
    for attachment in saved {
        assert_eq!(
            std::fs::read(
                harness
                    .attachments()
                    .join(format!("{}.png", attachment["id"].as_str().unwrap()))
            )
            .unwrap(),
            bytes
        );
    }
    let again = success(
        &rpc(
            &mut operator,
            11,
            "assets.persistChatAttachments",
            persist(json!([image, image])),
        )
        .await,
    )
    .clone();
    assert_eq!(again, first, "retrying a message writes the same files");
    let mut corrupt = image.clone();
    corrupt["sizeBytes"] = json!(bytes.len() + 1);
    let error = failure(
        &rpc(
            &mut operator,
            12,
            "assets.persistChatAttachments",
            persist(json!([corrupt])),
        )
        .await,
    )["error"]
        .clone();
    assert_eq!(
        error,
        json!({"_tag":"PersistChatAttachmentsError","message":"Attachment pasted.png size does not match its payload."})
    );
    let served = success(
        &rpc(
            &mut operator,
            13,
            "assets.createUrl",
            json!({"resource":{"_tag":"attachment","attachmentId":saved[0]["id"]}}),
        )
        .await,
    )
    .clone();
    assert_eq!(served["imageDimensions"], json!({"width":4,"height":4}));

    reader.close(None).await.unwrap();
    operator.close(None).await.unwrap();
    harness.finish().await;
}

#[tokio::test]
async fn acp_turns_carry_attachment_notes_and_refuse_images_without_negotiated_support() {
    let harness = harness_for("acp").await;
    let mut socket = harness.connect(&harness.operator).await;
    let mut events = harness.store.subscribe();

    // A file attachment reaches the agent as a path note inside the text prompt. The fixture
    // agent then asks for approval of a command, which the test grants to finish the turn.
    let pdf_bytes = b"%PDF-1.4 fixture".to_vec();
    let pdf = upload(&harness, &mut socket, 1, json!({"type":"file","name":"notes.pdf","mimeType":"application/pdf","sizeBytes":pdf_bytes.len()}), pdf_bytes.clone()).await;
    let file = json!({"type":"file","id":pdf["attachmentId"],"name":"notes.pdf","mimeType":"application/pdf","sizeBytes":pdf_bytes.len()});
    let exit = rpc(
        &mut socket,
        2,
        "orchestration.dispatchCommand",
        dispatch(&harness.thread_id, "acp-file", json!([file])),
    )
    .await;
    assert!(success(&exit)["sequence"].is_number());
    let pending = awaiting(&harness.store, &mut events, &harness.thread_id).await;
    assert_eq!(pending["runs"][0]["status"], "running");
    let request = pending["runtimeRequests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|request| request["status"] == "pending")
        .unwrap();
    let answer = json!({"type":"runtime-request.respond","commandId":"approve","threadId":harness.thread_id,"requestId":request["id"],"decision":"accept"});
    assert!(
        success(&rpc(&mut socket, 3, "orchestration.dispatchCommand", answer).await)["sequence"]
            .is_number()
    );
    let view = settled(&harness.store, &mut events, &harness.thread_id, 1).await;
    assert_eq!(
        view["runs"][0]["status"], "completed",
        "{}",
        view["runs"][0]
    );
    let message = view["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap();
    assert_eq!(message["attachments"].as_array().unwrap().len(), 1);
    assert!(
        message["attachments"][0]["id"]
            .as_str()
            .unwrap()
            .ends_with("-pdf")
    );

    // The fixture agent does not advertise image prompts, so a native image cannot be delivered.
    let bytes = png(2, 2, 5);
    let image = upload(
        &harness,
        &mut socket,
        4,
        json!({"name":"shot.png","mimeType":"image/png","sizeBytes":bytes.len()}),
        bytes.clone(),
    )
    .await;
    let attachment = json!({"type":"image","id":image["attachmentId"],"name":"shot.png","mimeType":"image/png","sizeBytes":bytes.len()});
    let exit = rpc(
        &mut socket,
        5,
        "orchestration.dispatchCommand",
        dispatch(&harness.thread_id, "acp-image", json!([attachment])),
    )
    .await;
    assert!(success(&exit)["sequence"].is_number());
    let view = settled(&harness.store, &mut events, &harness.thread_id, 2).await;
    assert_eq!(view["runs"][1]["status"], "failed", "{}", view["runs"][1]);
    assert!(
        view.to_string()
            .contains("ACP driver did not negotiate image prompt support")
    );

    socket.close(None).await.unwrap();
    harness.finish().await;
}
