//! Composer attachments through the mounted UI and the real native HTTP/WebSocket
//! server: pick, upload, send, display, remove, fail, retry, reload and permissions.
//! Waits are on state or rendered controls that only the awaited step produces.
use super::*;
use crate::runtime::transport_tests::{
    api_fixture, click_control, control, input_control, rendered_text, start_fixture,
};
use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};
use t3_client::{
    attachments::{Attachment, Upload},
    draft_storage::DraftTarget,
};

#[derive(Clone)]
struct Harness {
    state: Rc<RefCell<Option<Store<UiModel>>>>,
    transport: TransportHandle,
    address: String,
    credential: String,
    initial: UiModel,
}
fn harness(props: Harness) -> Element {
    let state = use_store(|| props.initial.clone());
    crate::themes::use_themes(state);
    *props.state.borrow_mut() = Some(state);
    let transport = props.transport.clone();
    use_future(move || {
        let transport = transport.clone();
        let address = props.address.clone();
        let credential = props.credential.clone();
        async move { pair_and_connect(transport, state, address, credential).await }
    });
    rsx! {crate::Application {state,transport:props.transport}}
}

async fn until(dom: &mut VirtualDom, label: &str, ready: impl Fn(&VirtualDom) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if ready(dom) {
                return;
            }
            dom.wait_for_work().await;
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("attachment milestone {label} timed out"));
}

struct Fixture {
    directory: tempfile::TempDir,
    auth: t3_server::auth::AuthService,
    store: t3_server::persistence::Store,
    address: String,
    server: crate::runtime::fixture_server::Server,
}
impl Fixture {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut api = api_fixture(&directory, "attachments-ui");
        let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../server/tests/fixtures/codex-provider.py")
            .canonicalize()
            .unwrap();
        let settings: t3_contracts::ServerSettings = serde_json::from_value(
            json!({"providerInstances":{"claudeAgent":{"driver":"claudeAgent","enabled":false},"codex":{"driver":"codex","config":{"binaryPath":binary,"homePath":directory.path().join("codex-home")}}}}),
        )
        .unwrap();
        let native = t3_server::config::NativeConfig::from_settings(
            settings,
            directory.path(),
            directory.path(),
            &api.environment,
            &api.auth.descriptor(),
        )
        .await
        .unwrap();
        let execution = t3_server::execution::ExecutionService::start(
            api.store.clone(),
            native.providers.clone(),
        );
        api.config = Some(native.snapshot);
        api.providers = Some(native.providers);
        api.execution = Some(execution);
        let (auth, store) = (api.auth.clone(), api.store.clone());
        let (address, server) = start_fixture(api).await;
        Self {
            directory,
            auth,
            store,
            address,
            server,
        }
    }
    fn credential(&self, scopes: &[AuthEnvironmentScope]) -> String {
        self.auth
            .create_pairing_credential(scopes, chrono::Utc::now(), chrono::Duration::minutes(1))
            .unwrap()
    }
    fn connect(&self, scopes: &[AuthEnvironmentScope], initial: UiModel) -> (VirtualDom, Harness) {
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            transport: TransportHandle::default(),
            address: self.address.clone(),
            credential: self.credential(scopes),
            initial,
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        (dom, props)
    }
    fn operator(&self) -> (VirtualDom, Harness) {
        self.connect(
            &[
                AuthEnvironmentScope::OrchestrationRead,
                AuthEnvironmentScope::OrchestrationOperate,
            ],
            UiModel::default(),
        )
    }
    /// Files the server holds as uploads that no message has claimed.
    fn pending(&self) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(self.directory.path().join("attachments"))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .filter(|name| name.starts_with("pending-"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
    async fn finish(self, dom: VirtualDom) {
        drop(dom);
        self.server.await;
    }
}

/// A project with threads `thread-a` and `thread-b`, created through the operator UI.
async fn seed(dom: &mut VirtualDom, props: &Harness, root: &std::path::Path) -> Store<UiModel> {
    let state = props.state.borrow().unwrap();
    until(dom, "shell", |_| state.peek().shell.synchronized).await;
    dom.in_scope(ScopeId::APP, || request(&props.transport, state, "projects.mutate", json!({"type":"project.create","commandId":"project","projectId":"project","title":"Project","workspaceRoot":root}), RequestKind::Unary)).unwrap();
    until(dom, "project", |_| {
        state
            .peek()
            .shell
            .snapshot
            .as_ref()
            .is_some_and(|s| !s.projects.is_empty())
    })
    .await;
    for thread in ["thread-a", "thread-b"] {
        dom.in_scope(ScopeId::APP,||command(&props.transport,state,"thread.create",json!({"threadId":thread,"projectId":"project","title":thread,"modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null}))).unwrap();
    }
    until(dom, "threads", |_| {
        state
            .peek()
            .shell
            .snapshot
            .as_ref()
            .is_some_and(|s| s.threads.len() == 2)
    })
    .await;
    state
}
async fn open(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>, thread: &str) {
    dom.in_scope(ScopeId::APP, || {
        select_thread(&props.transport, state, thread.into())
    });
    until(dom, "thread", |_| {
        state.peek().thread.synchronized
            && state.peek().thread.projection.as_ref().unwrap()["thread"]["id"] == thread
    })
    .await;
}
fn target(state: Store<UiModel>, thread: &str) -> DraftTarget {
    DraftTarget::thread(
        state.peek().destination.as_ref().unwrap().to_string(),
        thread,
    )
}
fn attachments(state: Store<UiModel>, target: &DraftTarget) -> Vec<Attachment> {
    state
        .draft_attachments()
        .peek()
        .get(target)
        .cloned()
        .unwrap_or_default()
}
fn all_ready(state: Store<UiModel>, target: &DraftTarget, count: usize) -> bool {
    let list = attachments(state, target);
    list.len() == count && list.iter().all(|a| a.upload == Upload::Ready)
}

/// A header-only PNG the server can read dimensions from.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52,
    ];
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.resize(40, 7);
    bytes
}
fn pick(dom: &mut VirtualDom, label: &str, files: Vec<(&str, &str, Vec<u8>)>) {
    use dioxus_html::{SerializedFileData, SerializedFormData, SerializedFormObject};
    dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
    let (element, _) = control(dom, label).unwrap_or_else(|| panic!("missing control {label}"));
    let values = files
        .into_iter()
        .map(|(name, mime, bytes)| SerializedFormObject {
            key: "files".into(),
            text: None,
            file: Some(SerializedFileData {
                path: PathBuf::from("/picked").join(name),
                size: bytes.len() as u64,
                last_modified: 0,
                content_type: Some(mime.into()),
                contents: Some(bytes.into()),
            }),
        })
        .collect();
    let form = SerializedFormData::new(String::new(), values);
    dom.runtime().handle_event(
        "change",
        dioxus::dioxus_core::Event::new(
            Rc::new(dioxus_html::PlatformEventData::new(Box::new(form))) as Rc<dyn std::any::Any>,
            true,
        ),
        element,
    );
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn picked_files_upload_send_and_show_on_the_sent_message() {
    let fixture = Fixture::start().await;
    let (mut dom, props) = fixture.operator();
    let state = seed(&mut dom, &props, fixture.directory.path()).await;
    open(&mut dom, &props, state, "thread-a").await;
    let draft = target(state, "thread-a");
    pick(
        &mut dom,
        "Attach files",
        vec![
            ("shot.png", "image/png", png(3, 5)),
            ("notes.pdf", "application/pdf", b"%PDF-1.4 fixture".to_vec()),
        ],
    );
    // Slots are taken and chips shown before any byte has moved.
    assert!(control(&dom, "Remove shot.png").is_some() && control(&dom, "Remove notes.pdf").is_some());
    input_control(&mut dom, "Message", "Describe these");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    assert!(
        state.peek().pending_messages.is_empty(),
        "a send waits for uploads"
    );
    assert_eq!(
        state.error().peek().as_deref(),
        Some("Attachments still uploading")
    );

    until(&mut dom, "uploads", |_| all_ready(state, &draft, 2)).await;
    assert_eq!(fixture.pending().len(), 2);
    let ready = attachments(state, &draft);
    assert!(
        ready.iter().all(|a| a.bytes.is_none()),
        "bytes are released once the server has them"
    );
    // Thumbnails come from the signed server URL and know their size.
    until(&mut dom, "thumbnail", |_| {
        attachments(state, &draft)
            .iter()
            .any(|a| a.preview.as_ref().is_some_and(|p| (p.width, p.height) == (Some(3), Some(5))))
    })
    .await;
    let preview = attachments(state, &draft)[0].preview.clone().unwrap();
    assert!(preview.url.starts_with(&fixture.address), "{}", preview.url);
    let served = reqwest::get(&preview.url).await.unwrap();
    assert_eq!(served.headers()["content-type"], "image/png");

    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "sent message", |dom| {
        control(dom, "Open shot.png").is_some() && control(dom, "Download notes.pdf").is_some()
    })
    .await;
    until(&mut dom, "composer cleared", |dom| {
        attachments(state, &draft).is_empty() && control(dom, "Remove shot.png").is_none()
    })
    .await;
    let projection = fixture.store.projection("thread", "thread-a").unwrap().unwrap();
    let message = projection["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap();
    let sent = message["attachments"].as_array().unwrap();
    assert_eq!(
        sent.iter().map(|a| a["name"].as_str().unwrap()).collect::<Vec<_>>(),
        ["shot.png", "notes.pdf"]
    );
    assert_eq!(sent[1]["type"], "file");
    assert_eq!(sent[0]["sizeBytes"], 40);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn removing_before_send_deletes_the_pending_upload() {
    let fixture = Fixture::start().await;
    let (mut dom, props) = fixture.operator();
    let state = seed(&mut dom, &props, fixture.directory.path()).await;
    open(&mut dom, &props, state, "thread-a").await;
    let draft = target(state, "thread-a");
    pick(&mut dom, "Attach files", vec![("notes.pdf", "application/pdf", b"%PDF-1.4".to_vec())]);
    until(&mut dom, "upload", |_| all_ready(state, &draft, 1)).await;
    assert_eq!(fixture.pending().len(), 1);
    click_control(&mut dom, "Remove notes.pdf");
    assert!(control(&dom, "Remove notes.pdf").is_none(), "gone from the composer at once");
    // The delete's reply changes nothing on screen, so nothing wakes the runtime for it: the runtime is
    // polled once per turn until the server has deleted the file (it does so before it replies).
    // This used to pass only because every reply wrote an unchanged message map, repainting the composer.
    crate::runtime::keys_tests::poll_until(&mut dom, "the server to delete the pending upload", |_| fixture.pending().is_empty()).await;
    // Removal is saved: the draft no longer lists it after reload.
    let saved = state.peek().draft_storage.document.borrow().prepare_write().unwrap().unwrap().1;
    assert!(saved.contains("\"attachments\":[]"), "{saved}");
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn rejected_upload_is_shown_blocks_sending_and_retries() {
    let fixture = Fixture::start().await;
    let (mut dom, props) = fixture.operator();
    let state = seed(&mut dom, &props, fixture.directory.path()).await;
    open(&mut dom, &props, state, "thread-a").await;
    let draft = target(state, "thread-a");
    // A file where the attachment directory belongs makes the server refuse the bytes.
    let blocker = fixture.directory.path().join("attachments");
    std::fs::write(&blocker, "").unwrap();
    pick(&mut dom, "Attach files", vec![("notes.pdf", "application/pdf", b"%PDF-1.4".to_vec())]);
    until(&mut dom, "failure", |dom| {
        control(dom, "Retry notes.pdf").is_some()
    })
    .await;
    let list = attachments(state, &draft);
    let Upload::Failed { reason, .. } = &list[0].upload else {
        panic!("{:?}", list[0].upload)
    };
    assert!(reason.starts_with("Upload"), "{reason}");
    assert!(crate::attachments::for_send(state, &draft).unwrap_err().starts_with("Retry or remove"));
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    assert!(state.peek().pending_messages.is_empty());

    std::fs::remove_file(&blocker).unwrap();
    click_control(&mut dom, "Retry notes.pdf");
    until(&mut dom, "retried upload", |_| all_ready(state, &draft, 1)).await;
    assert!(control(&dom, "Retry notes.pdf").is_none());
    assert_eq!(fixture.pending().len(), 1);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn limits_refuse_files_with_the_original_messages() {
    let fixture = Fixture::start().await;
    let (mut dom, props) = fixture.operator();
    let state = seed(&mut dom, &props, fixture.directory.path()).await;
    open(&mut dom, &props, state, "thread-a").await;
    let draft = target(state, "thread-a");
    pick(&mut dom, "Attach files", vec![("empty.txt", "text/plain", vec![])]);
    assert_eq!(
        state.attachment_error().peek().as_ref().unwrap().1,
        "'empty.txt' is empty or could not be read."
    );
    pick(&mut dom, "Attach files", vec![("drawing.svg", "image/svg+xml", b"<svg/>".to_vec())]);
    assert!(state.attachment_error().peek().as_ref().unwrap().1.contains("is not a supported image type"));
    assert!(attachments(state, &draft).is_empty());
    assert!(
        rendered_text(&dom).contains("not a supported image type"),
        "the refusal is visible in the composer"
    );
    // A good file clears the message.
    pick(&mut dom, "Attach files", vec![("a.txt", "text/plain", b"a".to_vec())]);
    assert!(state.attachment_error().peek().is_none());
    until(&mut dom, "upload", |_| all_ready(state, &draft, 1)).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn upload_finishing_after_switching_threads_stays_with_its_own_draft() {
    let fixture = Fixture::start().await;
    let (mut dom, props) = fixture.operator();
    let state = seed(&mut dom, &props, fixture.directory.path()).await;
    open(&mut dom, &props, state, "thread-a").await;
    let (a, b) = (target(state, "thread-a"), target(state, "thread-b"));
    pick(&mut dom, "Attach files", vec![("notes.pdf", "application/pdf", b"%PDF-1.4".to_vec())]);
    assert!(!all_ready(state, &a, 1), "still uploading when the user leaves");
    open(&mut dom, &props, state, "thread-b").await;
    assert!(control(&dom, "Remove notes.pdf").is_none(), "thread B shows no file of A");
    until(&mut dom, "upload while elsewhere", |_| all_ready(state, &a, 1)).await;
    assert!(attachments(state, &b).is_empty());
    assert!(control(&dom, "Remove notes.pdf").is_none());
    // Sending on B claims nothing of A's.
    input_control(&mut dom, "Message", "Only text");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "B accepted its text", |_| state.peek().draft.is_empty()).await;
    assert_eq!(attachments(state, &a).len(), 1);
    open(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "A still has its file", |dom| control(dom, "Remove notes.pdf").is_some()).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn uploaded_attachments_survive_reload_without_uploading_again() {
    let fixture = Fixture::start().await;
    let (mut dom, props) = fixture.operator();
    let state = seed(&mut dom, &props, fixture.directory.path()).await;
    open(&mut dom, &props, state, "thread-a").await;
    let draft = target(state, "thread-a");
    pick(&mut dom, "Attach files", vec![("shot.png", "image/png", png(4, 4)), ("unfinished.bin", "application/zip", vec![1; 4])]);
    until(&mut dom, "uploads", |_| all_ready(state, &draft, 2)).await;
    let before = attachments(state, &draft);
    // Navigating away and back keeps the draft in memory.
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, None));
    open(&mut dom, &props, state, "thread-a").await;
    assert!(control(&dom, "Remove shot.png").is_some());
    let saved = state.peek().draft_storage.document.borrow().prepare_write().unwrap().unwrap().1;
    drop(dom);

    // A new window reads the saved overlay and reconnects to the same server.
    let reloaded = UiModel::default();
    reloaded.draft_storage.document.borrow_mut().hydrate(None, Some(saved), "2026-10-08T12:00:00.000Z");
    let (mut dom, props) = fixture.connect(&[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], reloaded);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.synchronized).await;
    open(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "verified", |dom| {
        all_ready(state, &draft, 2) && control(dom, "Remove shot.png").is_some()
    })
    .await;
    let after = attachments(state, &draft);
    assert_eq!(
        after.iter().map(|a| a.attachment_id.clone()).collect::<Vec<_>>(),
        before.iter().map(|a| a.attachment_id.clone()).collect::<Vec<_>>(),
        "the saved server copies are reused"
    );
    assert_eq!(fixture.pending().len(), 2, "nothing was uploaded again");
    until(&mut dom, "thumbnail after reload", |_| attachments(state, &draft)[0].preview.is_some()).await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn expired_saved_upload_asks_to_be_attached_again() {
    let fixture = Fixture::start().await;
    let (mut dom, props) = fixture.operator();
    let state = seed(&mut dom, &props, fixture.directory.path()).await;
    open(&mut dom, &props, state, "thread-a").await;
    let draft = target(state, "thread-a");
    pick(&mut dom, "Attach files", vec![("notes.pdf", "application/pdf", b"%PDF-1.4".to_vec())]);
    until(&mut dom, "upload", |_| all_ready(state, &draft, 1)).await;
    let saved = state.peek().draft_storage.document.borrow().prepare_write().unwrap().unwrap().1;
    drop(dom);
    for name in fixture.pending() {
        std::fs::remove_file(fixture.directory.path().join("attachments").join(name)).unwrap();
    }
    let reloaded = UiModel::default();
    reloaded.draft_storage.document.borrow_mut().hydrate(None, Some(saved), "2026-10-08T12:00:00.000Z");
    let (mut dom, props) = fixture.connect(&[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], reloaded);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.synchronized).await;
    open(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "expiry", |_| attachments(state, &draft).first().is_some_and(|a| a.upload == Upload::NeedsReattach)).await;
    assert!(state.error().peek().is_none(), "expiry is shown on the chip, not as a request failure");
    assert!(control(&dom, "Retry notes.pdf").is_none());
    assert!(crate::attachments::for_send(state, &draft).unwrap_err().starts_with("Attach the missing file"));
    click_control(&mut dom, "Remove notes.pdf");
    assert!(attachments(state, &draft).is_empty());
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn read_only_connection_is_not_offered_attachments() {
    let fixture = Fixture::start().await;
    let (mut operator, operator_props) = fixture.operator();
    seed(&mut operator, &operator_props, fixture.directory.path()).await;
    drop(operator);
    let (mut dom, props) = fixture.connect(&[AuthEnvironmentScope::OrchestrationRead], UiModel::default());
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.synchronized && state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.len() == 2)).await;
    open(&mut dom, &props, state, "thread-a").await;
    assert!(control(&dom, "Message").is_some());
    assert!(control(&dom, "Attach files").is_none());
    // Even a direct attempt is refused before anything is staged.
    let draft = target(state, "thread-a");
    dom.in_scope(ScopeId::APP, || {
        crate::attachments::attach(
            &props.transport,
            state,
            &draft,
            vec![crate::attachments::Incoming {
                name: "a.txt".into(),
                mime_type: "text/plain".into(),
                size: 1,
                source: crate::attachments::Source::Bytes(Rc::from(&b"a"[..])),
            }],
        )
    });
    assert!(attachments(state, &draft).is_empty());
    assert!(fixture.pending().is_empty());
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn new_thread_launch_carries_attachments_in_its_first_message() {
    let fixture = Fixture::start().await;
    let (mut dom, props) = fixture.operator();
    let state = seed(&mut dom, &props, fixture.directory.path()).await;
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, Some("project".into())));
    until(&mut dom, "new thread form", |dom| {
        !state.peek().model_options().is_empty()
            && control(dom, "Create new thread").is_some()
            && control(dom, "Attach files").is_some()
    })
    .await;
    let draft = DraftTarget::project(state.peek().destination.as_ref().unwrap().to_string(), "project");
    pick(&mut dom, "Attach files", vec![("notes.pdf", "application/pdf", b"%PDF-1.4".to_vec())]);
    until(&mut dom, "upload", |_| all_ready(state, &draft, 1)).await;
    // Attachments alone are a first message.
    click_control(&mut dom, "Create new thread");
    until(&mut dom, "launched", |_| state.peek().active_thread.as_deref().is_some_and(|id| id != "thread-a" && id != "thread-b")).await;
    until(&mut dom, "message shown", |dom| control(dom, "Download notes.pdf").is_some()).await;
    assert!(attachments(state, &draft).is_empty());
    let thread = state.peek().active_thread.clone().unwrap();
    let projection = fixture.store.projection("thread", &thread).unwrap().unwrap();
    let sent = projection["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap()["attachments"].clone();
    assert_eq!(sent.as_array().unwrap().len(), 1);
    assert_eq!(sent[0]["name"], "notes.pdf");
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn drafts_saved_by_the_original_app_load_their_attachments_instead_of_holding() {
    let fixture = Fixture::start().await;
    let (mut operator, operator_props) = fixture.operator();
    seed(&mut operator, &operator_props, fixture.directory.path()).await;
    drop(operator);
    let image = json!({"id":"img","name":"shot.png","mimeType":"image/png","sizeBytes":8,"dataUrl":"data:image/png;base64,iVBORw0KGgo="});
    let sent = json!({"id":"file-a","name":"sent.pdf","mimeType":"application/pdf","sizeBytes":9,"attachmentId":"pending-gone","environmentId":"attachments-ui"});
    let unfinished = json!({"id":"file-b","name":"unfinished.pdf","mimeType":"application/pdf","sizeBytes":9});
    let source = json!({"version":9,"state":{"draftsByThreadKey":{"attachments-ui:thread-a":{"prompt":"From the original","attachments":[image],"files":[sent,unfinished]}}}}).to_string();
    let reloaded = UiModel::default();
    reloaded.draft_storage.document.borrow_mut().hydrate(Some(source.clone()), None, "2026-10-08T12:00:00.000Z");
    let (mut dom, props) = fixture.connect(&[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate], reloaded);
    let state = props.state.borrow().unwrap();
    until(&mut dom, "shell", |_| state.peek().shell.synchronized && state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.len() == 2)).await;
    open(&mut dom, &props, state, "thread-a").await;
    let draft = target(state, "thread-a");
    assert!(!crate::draft_storage::has_unrendered(state, &draft), "attachments no longer hold the draft");
    // The image is uploaded like a fresh pick; the file whose upload is gone and the
    // one that never finished ask to be attached again.
    until(&mut dom, "restored", |_| {
        let list = attachments(state, &draft);
        list.len() == 3
            && list[0].upload == Upload::Ready
            && list[1].upload == Upload::NeedsReattach
            && list[2].upload == Upload::NeedsReattach
    })
    .await;
    assert_eq!(fixture.pending().len(), 1);
    assert!(control(&dom, "Remove sent.pdf").is_some() && control(&dom, "Remove unfinished.pdf").is_some());
    assert!(crate::attachments::for_send(state, &draft).is_err());
    click_control(&mut dom, "Remove sent.pdf");
    click_control(&mut dom, "Remove unfinished.pdf");
    assert!(crate::attachments::for_send(state, &draft).is_ok());
    // The original app's bytes were only read.
    assert_eq!(state.peek().draft_storage.document.borrow().source_bytes.as_deref(), Some(source.as_str()));
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn editing_from_a_message_returns_its_attachments_to_the_composer() {
    use super::diff_tests::git;
    let fixture = Fixture::start().await;
    // Rewinding needs a checkpoint, and checkpoints need a repository.
    let root = std::fs::canonicalize(fixture.directory.path()).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    std::fs::write(root.join(".gitignore"), "*.sqlite*\nattachments/\n").unwrap();
    git(&root, &["add", "README.md", ".gitignore"]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    let (mut dom, props) = fixture.operator();
    let state = seed(&mut dom, &props, &root).await;
    open(&mut dom, &props, state, "thread-a").await;
    let draft = target(state, "thread-a");
    pick(&mut dom, "Attach files", vec![("shot.png", "image/png", png(3, 5)), ("notes.pdf", "application/pdf", b"%PDF-1.4 fixture".to_vec())]);
    input_control(&mut dom, "Message", "Describe these");
    until(&mut dom, "uploads", |_| all_ready(state, &draft, 2)).await;
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "an offer to edit the sent message", |dom| rendered_text(dom).contains('\u{21b6}')).await;
    until(&mut dom, "composer cleared", |_| attachments(state, &draft).is_empty() && state.peek().draft.is_empty()).await;

    click_control(&mut dom, "Edit from here");
    assert!(rendered_text(&dom).contains("Your prompt and attachments return to the composer."));
    click_control(&mut dom, "Revert and keep changes");
    until(&mut dom, "the message and its files back in the composer", |_| state.peek().draft == "Describe these" && all_ready(state, &draft, 2)).await;
    let names: Vec<_> = attachments(state, &draft).iter().map(|a| a.name.clone()).collect();
    assert_eq!(names, ["shot.png", "notes.pdf"]);
    assert!(control(&dom, "Remove shot.png").is_some() && control(&dom, "Remove notes.pdf").is_some());
    assert_eq!(state.error().peek().as_deref(), None);
    fixture.finish(dom).await;
}
