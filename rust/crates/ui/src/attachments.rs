//! Composer attachments: pick, paste and drop, upload to the server, remember across
//! navigation and reload, and claim by sending. Rules live in `t3_client::attachments`.
//! Every transfer is owned by the draft it started in, never by whatever is on screen,
//! so switching thread or environment mid-upload cannot attach a file to another draft.
use crate::runtime::{self, TransportHandle, UiModel, UiModelStoreExt};
use dioxus::{
    html::HasFileData,
    html::FileData,
    prelude::*,
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeSet, VecDeque},
    future::Future,
    rc::Rc,
};
use t3_client::{
    attachments::{
        self as rules, Attachment, Candidate, Kind, MAX_CONCURRENT_UPLOADS, Persisted, Preview,
        Retry, Upload,
    },
    draft_storage::{DraftTarget, SavedAttachments},
};
use t3_contracts::AuthEnvironmentScope;

/// What a failed `assets.createUrl` for a deleted or expired attachment reports.
pub const ASSET_NOT_FOUND: &str = "AssetAttachmentNotFoundError";
const VERIFY_FAILED: &str = "Uploaded file could not be verified. Retry when the server reconnects.";

thread_local! {
    /// Ids with a live task, so a remount or reconnect never starts a second one.
    static RUNNING: RefCell<BTreeSet<String>> = const { RefCell::new(BTreeSet::new()) };
    static ACTIVE_UPLOADS: Cell<usize> = const { Cell::new(0) };
    static WAITING_UPLOADS: RefCell<VecDeque<futures_channel::oneshot::Sender<()>>> = const { RefCell::new(VecDeque::new()) };
}
struct Running(String);
impl Running {
    fn claim(id: &str) -> Option<Self> {
        RUNNING
            .with(|running| running.borrow_mut().insert(id.to_owned()))
            .then(|| Self(id.to_owned()))
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.with(|running| running.borrow_mut().remove(&self.0));
    }
}
/// At most `MAX_CONCURRENT_UPLOADS` transfers run at once; the rest queue in order.
struct Permit;
impl Permit {
    async fn acquire() -> Self {
        loop {
            if ACTIVE_UPLOADS.get() < MAX_CONCURRENT_UPLOADS {
                ACTIVE_UPLOADS.set(ACTIVE_UPLOADS.get() + 1);
                return Self;
            }
            let (wake, wait) = futures_channel::oneshot::channel();
            WAITING_UPLOADS.with(|waiting| waiting.borrow_mut().push_back(wake));
            let _ = wait.await;
        }
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        ACTIVE_UPLOADS.set(ACTIVE_UPLOADS.get().saturating_sub(1));
        if let Some(wake) = WAITING_UPLOADS.with(|waiting| waiting.borrow_mut().pop_front()) {
            let _ = wake.send(());
        }
    }
}
fn spawn(task: impl Future<Output = ()> + 'static) {
    dioxus::dioxus_core::spawn_forever(task);
}

pub enum Source {
    File(FileData),
    Bytes(Rc<[u8]>),
    /// A paste or drop whose contents the platform would not hand over.
    Missing,
}
pub struct Incoming {
    pub name: String,
    pub mime_type: String,
    pub size: u64,
    pub source: Source,
}
impl Incoming {
    pub fn file(file: FileData) -> Self {
        Self {
            name: file.name(),
            mime_type: file.content_type().unwrap_or_default(),
            size: file.size(),
            source: Source::File(file),
        }
    }
}

fn entry(state: Store<UiModel>, target: &DraftTarget, id: &str) -> Option<Attachment> {
    state
        .draft_attachments()
        .peek()
        .get(target)?
        .iter()
        .find(|attachment| attachment.id == id)
        .cloned()
}
fn update<R>(
    state: Store<UiModel>,
    target: &DraftTarget,
    id: &str,
    change: impl FnOnce(&mut Attachment) -> R,
) -> Option<R> {
    state
        .draft_attachments()
        .write()
        .get_mut(target)?
        .iter_mut()
        .find(|attachment| attachment.id == id)
        .map(change)
}
fn persist(state: Store<UiModel>, target: &DraftTarget) {
    let saved = state
        .draft_attachments()
        .peek()
        .get(target)
        .map(|list| list.iter().map(Persisted::of).collect())
        .unwrap_or_default();
    crate::draft_storage::edit_attachments(state, target.clone(), saved);
}
fn set_error(state: Store<UiModel>, target: &DraftTarget, error: Option<String>) {
    let current = state.attachment_error().peek().clone();
    if current.as_ref().map(|(_, message)| message) != error.as_ref()
        || current.as_ref().map(|(owner, _)| owner) != Some(target)
    {
        state
            .attachment_error()
            .set(error.map(|message| (target.clone(), message)));
    }
}
fn fail_entry(
    state: Store<UiModel>,
    target: &DraftTarget,
    id: &str,
    reason: impl Into<String>,
    retry: Retry,
    minted: Option<String>,
) {
    let reason = reason.into();
    update(state, target, id, |attachment| {
        attachment.upload = Upload::Failed { reason, retry };
        if minted.is_some() {
            attachment.attachment_id = minted;
        }
    });
}

/// Why this connection cannot talk to `target`'s environment right now, if it cannot.
fn unavailable(
    handle: &TransportHandle,
    state: Store<UiModel>,
    environment: &str,
    scope: AuthEnvironmentScope,
) -> Option<&'static str> {
    if !runtime::connected_to(handle, state, environment) {
        return Some("Not connected");
    }
    let model = state.peek();
    let destination = model.destination.as_ref()?;
    (!model.environments.allows(destination, scope))
        .then_some("This connection cannot upload attachments.")
}

/// Fire-and-forget delete of a pending upload the draft no longer references. Without
/// a connection to its environment the server's own 24 hour sweep collects it.
pub(crate) fn delete_pending(handle: &TransportHandle, state: Store<UiModel>, environment: &str, id: &str) {
    if unavailable(handle, state, environment, AuthEnvironmentScope::OrchestrationOperate).is_some()
    {
        return;
    }
    let (handle, id) = (handle.clone(), id.to_owned());
    spawn(async move {
        let _ = runtime::request_value(handle, state, "attachments.delete", json!({"attachmentId":id})).await;
    });
}

/// Signed display/download URL for an attachment resource (valid for an hour).
pub async fn asset_url(
    handle: &TransportHandle,
    state: Store<UiModel>,
    environment: &str,
    resource: Value,
) -> Result<Preview, String> {
    let result = runtime::request_value(
        handle.clone(),
        state,
        "assets.createUrl",
        json!({"resource":resource}),
    )
    .await?;
    let relative = result["relativeUrl"]
        .as_str()
        .ok_or("The server returned an invalid asset URL.")?;
    let url = runtime::http_url(handle, state, environment, relative).ok_or("Not connected")?;
    let dimension = |key: &str| {
        result["imageDimensions"][key]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
    };
    Ok(Preview {
        url,
        width: dimension("width"),
        height: dimension("height"),
    })
}

async fn post_bytes(url: &str, mime_type: &str, bytes: &[u8]) -> Result<(), String> {
    // The signed URL is the whole credential: no cookies, no bearer token.
    let request = reqwest::Client::new()
        .post(url)
        .header("content-type", mime_type)
        .timeout(std::time::Duration::from_secs(300))
        .body(bytes.to_vec());
    #[cfg(target_arch = "wasm32")]
    let request = request.fetch_credentials_omit();
    let response = request.send().await.map_err(|_| "Upload failed".to_owned())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("Upload rejected ({})", response.status().as_u16()))
    }
}

async fn upload(handle: TransportHandle, state: Store<UiModel>, target: DraftTarget, id: String) {
    let permit = Permit::acquire().await;
    let Some(attachment) = entry(state, &target, &id).filter(|a| a.upload == Upload::Uploading)
    else {
        return;
    };
    let Some(bytes) = attachment.bytes.clone() else {
        fail_entry(state, &target, &id, "Original file is no longer available", Retry::None, None);
        return;
    };
    let environment = target.environment.as_str();
    if let Some(reason) = unavailable(
        &handle,
        state,
        environment,
        AuthEnvironmentScope::OrchestrationOperate,
    ) {
        fail_entry(state, &target, &id, reason, Retry::Upload, None);
        return;
    }
    let mut input = json!({"name":attachment.name,"mimeType":attachment.mime_type,"sizeBytes":bytes.len()});
    if attachment.kind == Kind::File {
        input["type"] = json!("file");
    }
    let minted = match runtime::request_value(handle.clone(), state, "attachments.createUploadUrl", input).await {
        Ok(minted) => minted,
        Err(_) => {
            fail_entry(state, &target, &id, "Upload could not start", Retry::Upload, None);
            return;
        }
    };
    let (Some(pending), Some(relative)) = (
        minted["attachmentId"].as_str().map(str::to_owned),
        minted["relativeUrl"].as_str(),
    ) else {
        fail_entry(state, &target, &id, "Upload could not start", Retry::Upload, None);
        return;
    };
    // Removed while the URL was being minted: nothing owns the pending upload.
    if entry(state, &target, &id).is_none() {
        delete_pending(&handle, state, environment, &pending);
        return;
    }
    let Some(url) = runtime::http_url(&handle, state, environment, relative) else {
        fail_entry(state, &target, &id, "Not connected", Retry::Upload, Some(pending));
        return;
    };
    let transfer = post_bytes(&url, &attachment.mime_type, &bytes).await;
    drop(permit);
    match transfer {
        Ok(()) => {
            let kept = update(state, &target, &id, |attachment| {
                attachment.upload = Upload::Ready;
                attachment.attachment_id = Some(pending.clone());
                attachment.bytes = None;
            })
            .is_some();
            if !kept {
                delete_pending(&handle, state, environment, &pending);
                return;
            }
            persist(state, &target);
            show_preview(&handle, state, &target, &id).await;
        }
        Err(reason) => fail_entry(state, &target, &id, reason, Retry::Upload, Some(pending)),
    }
}

/// Thumbnails come from the server copy, which also supplies the image dimensions.
async fn show_preview(handle: &TransportHandle, state: Store<UiModel>, target: &DraftTarget, id: &str) {
    let Some(attachment) = entry(state, target, id).filter(|a| a.kind == Kind::Image) else {
        return;
    };
    let Some(pending) = attachment.attachment_id else {
        return;
    };
    if let Ok(preview) = asset_url(
        handle,
        state,
        &target.environment,
        json!({"_tag":"attachment","attachmentId":pending}),
    )
    .await
    {
        update(state, target, id, |attachment| attachment.preview = Some(preview));
    }
}

/// Asks whether a saved upload still exists. Pending uploads expire, so a draft
/// restored after a day finds its file gone and asks to attach it again.
async fn verify(handle: TransportHandle, state: Store<UiModel>, target: DraftTarget, id: String) {
    let Some(attachment) = entry(state, &target, &id).filter(|a| a.upload == Upload::Verifying)
    else {
        return;
    };
    let Some(pending) = attachment.attachment_id.clone() else {
        update(state, &target, &id, |a| a.upload = Upload::NeedsReattach);
        return;
    };
    // Verification resumes when the connection does; this is not a failure.
    if unavailable(&handle, state, &target.environment, AuthEnvironmentScope::OrchestrationRead)
        .is_some()
    {
        return;
    }
    match asset_url(
        &handle,
        state,
        &target.environment,
        json!({"_tag":"attachment","attachmentId":pending}),
    )
    .await
    {
        Ok(preview) => {
            update(state, &target, &id, |attachment| {
                attachment.upload = Upload::Ready;
                if attachment.kind == Kind::Image {
                    attachment.preview = Some(preview);
                }
            });
        }
        Err(error) if error == ASSET_NOT_FOUND => {
            update(state, &target, &id, |attachment| {
                attachment.upload = Upload::NeedsReattach;
                attachment.attachment_id = None;
            });
            persist(state, &target);
        }
        Err(_) => fail_entry(state, &target, &id, VERIFY_FAILED, Retry::Verify, None),
    }
}

enum Task {
    Upload,
    Verify,
}
fn start(handle: &TransportHandle, state: Store<UiModel>, target: &DraftTarget, id: &str, task: Task) {
    let Some(running) = Running::claim(id) else {
        return;
    };
    let (handle, target, id) = (handle.clone(), target.clone(), id.to_owned());
    spawn(async move {
        let _running = running;
        match task {
            Task::Upload => upload(handle, state, target, id).await,
            Task::Verify => verify(handle, state, target, id).await,
        }
    });
}

async fn read(source: Source) -> Option<Rc<[u8]>> {
    match source {
        Source::File(file) => file.read_bytes().await.ok().map(|bytes| Rc::from(&bytes[..])),
        Source::Bytes(bytes) => Some(bytes),
        Source::Missing => None,
    }
}

/// Adds picked, pasted or dropped files to a draft. Limits are checked first, and
/// every admitted file takes its slot at once so concurrent adds see each other.
/// What the connected environment says it accepts (`capabilities.attachmentUploads`, `fileAttachments`).
pub(crate) fn limits(state: Store<UiModel>) -> rules::Limits {
    let config = state.typed_config();
    let config = config.read();
    rules::Limits::of(config.as_ref().map(|config| &config.environment.capabilities))
}

pub fn attach(
    handle: &TransportHandle,
    state: Store<UiModel>,
    target: &DraftTarget,
    incoming: Vec<Incoming>,
) {
    if incoming.is_empty() {
        return;
    }
    if let Some(reason) = unavailable(
        handle,
        state,
        &target.environment,
        AuthEnvironmentScope::OrchestrationOperate,
    ) {
        set_error(state, target, Some(reason.into()));
        return;
    }
    load(state, target);
    let existing = state.draft_attachments().peek().get(target).map_or(0, Vec::len);
    let candidates: Vec<_> = incoming
        .iter()
        .map(|file| Candidate {
            name: file.name.clone(),
            mime_type: file.mime_type.clone(),
            size_bytes: file.size,
        })
        .collect();
    let limits = limits(state);
    let (admitted, error) = rules::admit_with(existing, &candidates, &limits);
    set_error(state, target, error);
    let mut sources: Vec<_> = incoming.into_iter().map(|file| Some(file.source)).collect();
    let mut jobs = vec![];
    for file in &admitted {
        let id = uuid::Uuid::new_v4().to_string();
        state
            .draft_attachments()
            .write()
            .entry(target.clone())
            .or_default()
            .push(Attachment::new(id.clone(), file, candidates[file.index].size_bytes));
        jobs.push((id, sources[file.index].take().expect("admitted once")));
    }
    if jobs.is_empty() {
        return;
    }
    persist(state, target);
    for (id, source) in jobs {
        let Some(running) = Running::claim(&id) else {
            continue;
        };
        let (handle, target) = (handle.clone(), target.clone());
        spawn(async move {
            let _running = running;
            let name = entry(state, &target, &id).map(|a| a.name).unwrap_or_default();
            let bytes = read(source).await.filter(|bytes| !bytes.is_empty());
            let problem = match &bytes {
                None => Some(format!("'{name}' is empty or could not be read.")),
                Some(bytes) => entry(state, &target, &id).and_then(|a| {
                    let limit = if a.kind == Kind::Image { rules::MAX_IMAGE_BYTES } else { limits.file_staging_limit().unwrap_or(rules::MAX_FILE_BYTES) };
                    (bytes.len() as u64 > limit).then(|| rules::too_large_message(&name, limit))
                }),
            };
            if let Some(problem) = problem {
                remove_entry(state, &target, &id);
                persist(state, &target);
                set_error(state, &target, Some(problem));
                return;
            }
            let bytes = bytes.expect("checked above");
            let present = update(state, &target, &id, |attachment| {
                attachment.size_bytes = bytes.len() as u64;
                attachment.bytes = Some(bytes);
            });
            if present.is_some() {
                upload(handle, state, target, id).await;
            }
        });
    }
}
fn remove_entry(state: Store<UiModel>, target: &DraftTarget, id: &str) -> Option<Attachment> {
    let mut field = state.draft_attachments();
    let mut all = field.write();
    let list = all.get_mut(target)?;
    let at = list.iter().position(|attachment| attachment.id == id)?;
    Some(list.remove(at))
}

/// Removes an attachment from the draft and deletes the pending upload it owned.
pub fn remove(handle: &TransportHandle, state: Store<UiModel>, target: &DraftTarget, id: &str) {
    let Some(removed) = remove_entry(state, target, id) else {
        return;
    };
    persist(state, target);
    set_error(state, target, None);
    if let Some(pending) = removed.attachment_id {
        delete_pending(handle, state, &target.environment, &pending);
    }
}

/// Retries a failed upload (a fresh upload URL, the stale one deleted) or verification.
pub fn retry(handle: &TransportHandle, state: Store<UiModel>, target: &DraftTarget, id: &str) {
    let Some(attachment) = entry(state, target, id) else {
        return;
    };
    let Upload::Failed { retry, .. } = attachment.upload else {
        return;
    };
    match retry {
        Retry::None => {}
        Retry::Upload => {
            if let Some(stale) = attachment.attachment_id {
                delete_pending(handle, state, &target.environment, &stale);
            }
            update(state, target, id, |attachment| {
                attachment.attachment_id = None;
                attachment.upload = Upload::Uploading;
            });
            start(handle, state, target, id, Task::Upload);
        }
        Retry::Verify => {
            update(state, target, id, |attachment| attachment.upload = Upload::Verifying);
            start(handle, state, target, id, Task::Verify);
        }
    }
}

/// Brings a draft's saved attachments into memory the first time its composer opens.
pub fn load(state: Store<UiModel>, target: &DraftTarget) {
    if state.draft_attachments().peek().contains_key(target) {
        return;
    }
    let restored = {
        let model = state.peek();
        let document = model.draft_storage.document.borrow();
        match document.attachments(target) {
            SavedAttachments::None => vec![],
            SavedAttachments::Edited(saved) => saved.iter().cloned().map(Persisted::restore).collect(),
            SavedAttachments::Original(draft) => rules::restore_source(draft, &target.environment),
        }
    };
    state
        .draft_attachments()
        .write()
        .entry(target.clone())
        .or_insert(restored);
}

/// Puts saved attachments back into a draft (a restored stash entry). Each is verified against the
/// server before it counts, as a draft reopened after a reload is; the names of those that do not fit
/// under the attachment limit come back.
pub(crate) fn bring_back(handle: &TransportHandle, state: Store<UiModel>, target: &DraftTarget, saved: Vec<Persisted>) -> Vec<String> {
    load(state, target);
    let mut skipped = vec![];
    for attachment in saved {
        let list = state.draft_attachments().peek().get(target).cloned().unwrap_or_default();
        if list.iter().any(|held| held.id == attachment.id || (attachment.attachment_id.is_some() && held.attachment_id == attachment.attachment_id)) {
            continue;
        }
        if list.len() >= rules::MAX_ATTACHMENTS {
            skipped.push(attachment.name);
            continue;
        }
        state.draft_attachments().write().entry(target.clone()).or_default().push(attachment.restore());
    }
    persist(state, target);
    resume(handle, state, target);
    skipped
}

/// Starts whatever a restored or reconnecting draft still has to do.
pub fn resume(handle: &TransportHandle, state: Store<UiModel>, target: &DraftTarget) {
    load(state, target);
    let pending: Vec<_> = state
        .draft_attachments()
        .peek()
        .get(target)
        .into_iter()
        .flatten()
        .filter_map(|attachment| match attachment.upload {
            Upload::Uploading if attachment.bytes.is_some() => Some((attachment.id.clone(), Task::Upload)),
            Upload::Verifying => Some((attachment.id.clone(), Task::Verify)),
            _ => None,
        })
        .collect();
    for (id, task) in pending {
        start(handle, state, target, &id, task);
    }
}

/// The ids and wire form of a draft's attachments for a message, or why they cannot
/// be sent yet.
pub fn for_send(
    state: Store<UiModel>,
    target: &DraftTarget,
) -> Result<(Vec<String>, Vec<Value>), String> {
    let model = state.peek();
    let Some(list) = model.draft_attachments.get(target).filter(|list| !list.is_empty()) else {
        return Ok((vec![], vec![]));
    };
    if let Some(reason) = rules::block_reason_for(list, &limits(state)) {
        return Err(reason);
    }
    if let Some(error) = rules::limit_error(list) {
        return Err(error);
    }
    let wire = rules::wire(list).ok_or("Attachments are not ready to send.")?;
    Ok((list.iter().map(|attachment| attachment.id.clone()).collect(), wire))
}

/// A send claimed these uploads; they leave the draft but stay on the server.
pub fn sent(state: Store<UiModel>, target: &DraftTarget, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    if let Some(list) = state.draft_attachments().write().get_mut(target) {
        list.retain(|attachment| !ids.contains(&attachment.id));
    }
    persist(state, target);
}

fn files_of(files: Vec<FileData>) -> Vec<Incoming> {
    files.into_iter().map(Incoming::file).collect()
}

#[component]
pub fn AttachButton(
    state: Store<UiModel>,
    transport: TransportHandle,
    target: DraftTarget,
) -> Element {
    // Remounting the input clears it, so choosing the same file twice still fires.
    let mut picks = use_signal(|| 0u32);
    // Nothing to attach to on a server that takes no uploads.
    if !limits(state).attach_action() {
        return rsx! {};
    }
    rsx! {
        label { class:"attach-button", title:"Attach files",
            span { "aria-hidden":"true", "📎" }
            input { key:"{picks}", r#type:"file", multiple:true, "aria-label":"Attach files", onchange:move|event|{
                let files=event.files();
                if files.is_empty(){return;}
                attach(&transport,state,&target,files_of(files));
                picks.set(picks()+1);
            }}
        }
    }
}

/// Drop handlers for the element that should accept dragged files.
pub fn on_drop(
    transport: &TransportHandle,
    state: Store<UiModel>,
    target: DraftTarget,
) -> impl FnMut(DragEvent) + 'static {
    let transport = transport.clone();
    move |event| {
        event.prevent_default();
        attach(&transport, state, &target, files_of(event.files()));
    }
}

/// Chips for a draft's attachments, their error and the reason a send must wait.
#[component]
pub fn Shelf(
    state: Store<UiModel>,
    transport: TransportHandle,
    target: DraftTarget,
    /// Removing or retrying is held while a send that claims these uploads is in flight.
    locked: bool,
    can_operate: bool,
    /// CSS selector of the prompt box that receives pasted files.
    paste_target: &'static str,
) -> Element {
    {
        let (transport, target) = (transport.clone(), target.clone());
        use_effect(move || {
            // Reconnecting resumes uploads and verification that waited for it.
            let _ = state.status().read();
            resume(&transport, state, &target);
        });
    }
    #[cfg(not(test))]
    use_future({
        let (transport, target) = (transport.clone(), target.clone());
        move || paste_bridge(transport.clone(), state, target.clone(), paste_target)
    });
    #[cfg(test)]
    let _ = paste_target;
    let list = state
        .draft_attachments()
        .read()
        .get(&target)
        .cloned()
        .unwrap_or_default();
    let error = state
        .attachment_error()
        .read()
        .clone()
        .filter(|(owner, _)| owner == &target)
        .map(|(_, message)| message);
    let blocked = rules::block_reason_for(&list, &limits(state));
    if list.is_empty() && error.is_none() {
        return rsx! {};
    }
    rsx! {
        if let Some(error)=error {p {class:"attachment-error",role:"alert","{error}",button {r#type:"button","aria-label":"Dismiss attachment error",onclick:move|_|state.attachment_error().set(None),"×"}}}
        if !list.is_empty() {
            div { class:"attachment-shelf", "aria-label":"Attachments",
                for attachment in list.iter().cloned() { Chip { key:"{attachment.id}", state, transport:transport.clone(), target:target.clone(), attachment, locked, can_operate } }
            }
            if let Some(reason)=blocked {p {class:"attachment-hint muted","{reason}"}}
        }
    }
}

#[component]
fn Chip(
    state: Store<UiModel>,
    transport: TransportHandle,
    target: DraftTarget,
    attachment: Attachment,
    locked: bool,
    can_operate: bool,
) -> Element {
    let status = match &attachment.upload {
        Upload::Uploading => "Uploading…".to_owned(),
        Upload::Verifying => "Checking upload…".to_owned(),
        Upload::Ready => rules::format_size(attachment.size_bytes),
        Upload::Failed { reason, .. } => reason.clone(),
        Upload::NeedsReattach => "Needs to be attached again".to_owned(),
    };
    let class = match attachment.upload {
        Upload::Failed { .. } | Upload::NeedsReattach => "attachment-chip failed",
        Upload::Ready => "attachment-chip",
        _ => "attachment-chip busy",
    };
    let retryable = matches!(&attachment.upload, Upload::Failed { retry, .. } if *retry != Retry::None);
    let id = attachment.id.clone();
    let name = attachment.name.clone();
    rsx! {
        div { class:"{class}",
            div { class:"attachment-thumb",
                if let Some(preview)=attachment.preview.as_ref() {
                    img { src:"{preview.url}", alt:"{attachment.name}", width:preview.width.map(|w|w.to_string()), height:preview.height.map(|h|h.to_string()) }
                } else {span {"aria-hidden":"true",if attachment.kind==Kind::Image {"🖼"} else {"📄"}}}
            }
            div { class:"attachment-meta", span { class:"attachment-name", title:"{attachment.name}", "{attachment.name}" } small { role:if matches!(attachment.upload,Upload::Failed{..}){"alert"}else{"status"}, "{status}" } }
            if retryable {
                button { r#type:"button", "aria-label":"Retry {name}", disabled:locked || !can_operate, onclick:{let (transport,target,id)=(transport.clone(),target.clone(),id.clone());move|_|retry(&transport,state,&target,&id)}, "Retry" }
            }
            button { r#type:"button", "aria-label":"Remove {name}", disabled:locked || !can_operate, onclick:move|_|remove(&transport,state,&target,&id), "×" }
        }
    }
}

/// Long pasted text as a file of its own, named so several pastes in one draft stay distinct.
pub(crate) fn attach_pasted_text(transport: &TransportHandle, state: Store<UiModel>, target: &DraftTarget, text: &str) {
    use t3_client::text_paste::{Disposition, disposition, next_file_name};
    if disposition(text, true, false, false) != Disposition::Attachment {
        return;
    }
    let existing: Vec<String> = state.draft_attachments().peek().get(target).map(|list| list.iter().map(|attachment| attachment.name.clone()).collect()).unwrap_or_default();
    let bytes: Rc<[u8]> = Rc::from(text.as_bytes());
    attach(transport, state, target, vec![Incoming { name: next_file_name(&existing), mime_type: "text/plain".into(), size: bytes.len() as u64, source: Source::Bytes(bytes) }]);
}

/// Dioxus clipboard events carry no files, so a one-time listener on the prompt box
/// hands pasted files back through the webview bridge. As in the original, images
/// (and any files, when there is no pasted text) become attachments and the paste is
/// not inserted as text. `data-attach-paste` is on only while the connection may attach.
#[cfg(not(test))]
async fn paste_bridge(
    transport: TransportHandle,
    state: Store<UiModel>,
    target: DraftTarget,
    selector: &'static str,
) {
    let script = format!(
        r#"
const selector = {selector};
let box = null;
for (let tries = 0; tries < 60 && !(box = document.querySelector(selector)); tries++) {{
  await new Promise((resolve) => requestAnimationFrame(resolve));
}}
if (!box) return;
// A thread opened just now (a fork, a click in the sidebar) can still show the old prompt box for a frame while this runs; the
// one the new composer draws is bound instead, so a long paste there becomes a file as it does anywhere else.
await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
box = document.querySelector(selector) ?? box;
if (box.__t3Paste) box.removeEventListener("paste", box.__t3Paste);
const read = (file) => new Promise((resolve) => {{
  const reader = new FileReader();
  reader.onload = () => resolve(String(reader.result).split(",", 2)[1] ?? "");
  reader.onerror = () => resolve(null);
  reader.readAsDataURL(file);
}});
const image = (file) => file.type.startsWith("image/") || /\.(png|jpe?g|gif|webp)$/i.test(file.name);
// Shift with the platform's paste chord pastes text as it is, however long.
const mac = /Mac|iPhone|iPad/.test(navigator.platform);
if (box.__t3Chord) box.removeEventListener("keydown", box.__t3Chord, true);
box.__t3Bypass = false;
box.__t3Chord = (event) => {{
  if (event.key.toLowerCase() === "v" && event.shiftKey && !event.altKey && (mac ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey)) box.__t3Bypass = true;
}};
box.addEventListener("keydown", box.__t3Chord, true);
box.__t3Paste = async (event) => {{
  const data = event.clipboardData;
  if (!data || box.getAttribute("data-attach-paste") !== "on") return;
  const bypass = box.__t3Bypass;
  box.__t3Bypass = false;
  const files = Array.from(data.files);
  if (files.length === 0) {{
    // Long text becomes a file (the rule is `text_paste::disposition`; this only has to decide before the text is inserted).
    const text = data.getData("text/plain");
    if (!bypass && text.length > 0 && (text.length >= {fold} || new TextEncoder().encode(text).length >= {fold})) {{
      event.preventDefault();
      dioxus.send({{ text }});
    }}
    return;
  }}
  if (!files.some(image) && data.getData("text/plain").length > 0) return;
  event.preventDefault();
  const out = [];
  for (const file of files) {{
    out.push({{ name: file.name, type: file.type, size: file.size, data: file.size <= {limit} ? await read(file) : null }});
  }}
  dioxus.send(out);
}};
box.addEventListener("paste", box.__t3Paste);
await new Promise(() => {{}});
"#,
        selector = serde_json::to_string(selector).expect("selector"),
        limit = rules::MAX_FILE_BYTES,
        fold = t3_client::text_paste::THRESHOLD_BYTES
    );
    let mut bridge = dioxus::document::eval(&script);
    while let Ok(message) = bridge.recv::<Value>().await {
        if let Some(text) = message["text"].as_str() {
            attach_pasted_text(&transport, state, &target, text);
            continue;
        }
        let files: Vec<Value> = serde_json::from_value(message).unwrap_or_default();
        let incoming = files
            .into_iter()
            .map(|file| {
                let bytes = file["data"].as_str().and_then(rules::decode_base64);
                Incoming {
                    name: file["name"].as_str().unwrap_or_default().to_owned(),
                    mime_type: file["type"].as_str().unwrap_or_default().to_owned(),
                    size: file["size"].as_u64().unwrap_or_default(),
                    source: bytes.map_or(Source::Missing, |bytes| Source::Bytes(bytes.into())),
                }
            })
            .collect();
        attach(&transport, state, &target, incoming);
    }
}
