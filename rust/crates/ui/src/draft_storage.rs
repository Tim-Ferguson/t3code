use crate::runtime::{NewThreadChoices, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use t3_client::{
    draft_storage::{DraftKind, DraftStorage, DraftTarget},
    drafts,
};

#[derive(Debug)]
enum WriterRequest {
    Save,
    #[cfg_attr(not(any(feature = "desktop", test)), allow(dead_code))]
    Flush(futures_channel::oneshot::Sender<Result<(), String>>),
}
#[derive(Debug, Clone, Default)]
pub struct DraftHandle {
    pub document: Rc<RefCell<DraftStorage>>,
    hydrated: Rc<Cell<bool>>,
    sender: Rc<RefCell<Option<futures_channel::mpsc::UnboundedSender<WriterRequest>>>>,
}

pub async fn hydrate(state: Store<UiModel>) {
    let handle = state.peek().draft_storage.clone();
    handle.hydrated.set(false);
    let result = read().await;
    let handle = state.peek().draft_storage.clone();
    match result {
        Ok((source, sidecar)) => {
            handle.document.borrow_mut().hydrate(
                source,
                sidecar,
                &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            );
            handle.hydrated.set(true);
            let document = handle.document.borrow();
            let error = document
                .recovery_error
                .clone()
                .or_else(|| document.sidecar_error.clone());
            drop(document);
            state.draft_storage_error().set(error);
            restore_destination(state);
            schedule(state);
        }
        Err(error) => state.draft_storage_error().set(Some(error)),
    }
}
async fn read() -> Result<(Option<String>, Option<String>), String> {
    #[cfg(target_arch = "wasm32")]
    {
        let storage = web_sys::window()
            .ok_or("Draft storage unavailable.")?
            .local_storage()
            .map_err(|_| "Draft storage could not be read. Saved data was preserved.")?
            .ok_or("Draft storage unavailable.")?;
        Ok((
            storage
                .get_item(drafts::SOURCE_KEY)
                .map_err(|_| "Saved drafts could not be read.")?,
            storage
                .get_item(drafts::SIDECAR_KEY)
                .map_err(|_| "Saved Rust drafts could not be read.")?,
        ))
    }
    #[cfg(not(target_arch="wasm32"))]
    dioxus::document::eval("return [window.localStorage.getItem('t3code:composer-drafts:v1'), window.localStorage.getItem('t3code:rust-composer-drafts:v1')];").join::<(Option<String>,Option<String>)>().await.map_err(|_|"Draft storage could not be read. Saved data was preserved.".into())
}
pub fn restore_destination(mut state: Store<UiModel>) {
    let Some(destination) = state.peek().destination.clone() else {
        return;
    };
    let handle = state.peek().draft_storage.clone();
    let document = handle.document.borrow();
    let mut model = state.write();
    for target in document
        .targets()
        .into_iter()
        .filter(|target| target.environment == destination.as_str())
    {
        match target.kind {
            DraftKind::Thread => {
                let choices = recovered_choices(&document, &target);
                if choices != NewThreadChoices::default() {
                    model
                        .thread_choices
                        .entry((destination.clone(), target.local_id.clone()))
                        .or_insert(choices);
                }
                if let Some(prompt) = document.prompt(&target) {
                    if let Some(record) = model.environments.records.get_mut(&destination) {
                        record
                            .cache
                            .drafts
                            .entry(target.local_id.clone())
                            .or_insert_with(|| prompt.into());
                    }
                    if model.active_thread.as_deref() == Some(&target.local_id)
                        && model.draft.is_empty()
                    {
                        model.draft = prompt.into();
                    }
                }
            }
            DraftKind::Project => {
                let key = (destination.clone(), target.local_id.clone());
                if let Some(prompt) = document.prompt(&target) {
                    model
                        .new_thread_drafts
                        .entry(key.clone())
                        .or_insert_with(|| prompt.into());
                }
                let choices = recovered_choices(&document, &target);
                if choices != NewThreadChoices::default() {
                    model.new_thread_choices.entry(key).or_insert(choices);
                }
            }
        }
    }
}
fn recovered_choices(document: &DraftStorage, target: &DraftTarget) -> NewThreadChoices {
    document
        .changes(&target)
        .and_then(|changes| changes.choices.as_ref())
        .and_then(|value| serde_json::from_value::<NewThreadChoices>(value.clone()).ok())
        .unwrap_or_else(|| {
            let draft = document.recovered(&target);
            let session = document.recovered_session(&target);
            NewThreadChoices {
                model_selection: draft.and_then(|draft| {
                    draft["activeProvider"].as_str().and_then(|active| {
                        serde_json::from_value(draft["modelSelectionByProvider"][active].clone())
                            .ok()
                    })
                }),
                runtime_mode: draft
                    .and_then(|draft| serde_json::from_value(draft["runtimeMode"].clone()).ok())
                    .or_else(|| {
                        session.and_then(|session| {
                            serde_json::from_value(session["runtimeMode"].clone()).ok()
                        })
                    }),
                environment_mode: session
                    .and_then(|session| serde_json::from_value(session["envMode"].clone()).ok()),
                base_ref: session
                    .and_then(|session| session["branch"].as_str())
                    .unwrap_or_default()
                    .into(),
            }
        })
}
pub fn edit_thread_prompt(state: Store<UiModel>, prompt: String) {
    let target = {
        let model = state.peek();
        model
            .destination
            .as_ref()
            .zip(model.active_thread.as_ref())
            .map(|(environment, thread)| {
                DraftTarget::thread(environment.to_string(), thread.clone())
            })
    };
    state.draft().set(prompt.clone());
    if let Some(target) = target {
        state
            .peek()
            .draft_storage
            .document
            .borrow_mut()
            .edit_prompt(target, prompt);
        schedule(state);
    }
}
pub fn edit_project_prompt(
    state: Store<UiModel>,
    key: (t3_contracts::EnvironmentId, String),
    prompt: String,
) {
    state
        .new_thread_drafts()
        .write()
        .insert(key.clone(), prompt.clone());
    state
        .peek()
        .draft_storage
        .document
        .borrow_mut()
        .edit_prompt(DraftTarget::project(key.0.to_string(), key.1), prompt);
    schedule(state);
}
pub fn save_choices(
    state: Store<UiModel>,
    key: &(t3_contracts::EnvironmentId, String),
    choices: &NewThreadChoices,
) {
    state
        .peek()
        .draft_storage
        .document
        .borrow_mut()
        .edit_choices(
            DraftTarget::project(key.0.to_string(), key.1.clone()),
            serde_json::to_value(choices).expect("draft choices"),
        );
    schedule(state);
}
pub fn save_thread_choices(
    state: Store<UiModel>,
    key: &(t3_contracts::EnvironmentId, String),
    choices: &NewThreadChoices,
) {
    state
        .peek()
        .draft_storage
        .document
        .borrow_mut()
        .edit_choices(
            DraftTarget::thread(key.0.to_string(), key.1.clone()),
            serde_json::to_value(choices).expect("draft choices"),
        );
    schedule(state);
}
pub fn acknowledge_content(state: Store<UiModel>, target: DraftTarget) {
    state
        .peek()
        .draft_storage
        .document
        .borrow_mut()
        .acknowledge_content(target);
    schedule(state);
}
pub fn acknowledge(state: Store<UiModel>, target: DraftTarget) {
    state
        .peek()
        .draft_storage
        .document
        .borrow_mut()
        .acknowledge(target);
    schedule(state);
}
pub fn forget(state: Store<UiModel>, environment: &str) {
    state
        .peek()
        .draft_storage
        .document
        .borrow_mut()
        .forget(environment);
    schedule(state);
}
pub fn has_unrendered(state: Store<UiModel>, target: &DraftTarget) -> bool {
    state
        .peek()
        .draft_storage
        .document
        .borrow()
        .recovered(target)
        .is_some_and(drafts::has_unrendered_content)
}
fn schedule(state: Store<UiModel>) {
    if let Some(sender) = state.peek().draft_storage.sender.borrow().as_ref() {
        let _ = sender.unbounded_send(WriterRequest::Save);
    }
}
pub fn use_writer(state: Store<UiModel>) {
    use futures_util::{
        StreamExt,
        future::{Either, select},
    };
    let receiver = use_hook(move || {
        let (sender, receiver) = futures_channel::mpsc::unbounded();
        *state.peek().draft_storage.sender.borrow_mut() = Some(sender);
        Rc::new(RefCell::new(Some(receiver)))
    });
    // Root ownership survives composer/project navigation. Native writes are
    // serial: a slow old write finishes before a newer document is serialized.
    use_future(move || {
        let mut receiver = receiver.borrow_mut().take().expect("single draft writer");
        async move {
            while let Some(request) = receiver.next().await {
                let mut completion = match request {
                    WriterRequest::Save => None,
                    WriterRequest::Flush(reply) => Some(reply),
                };
                if completion.is_none() {
                    loop {
                        let timer = delay();
                        let next = receiver.next();
                        futures_util::pin_mut!(timer, next);
                        match select(timer, next).await {
                            Either::Right((Some(WriterRequest::Save), _)) => continue,
                            Either::Right((Some(WriterRequest::Flush(reply)), _)) => {
                                completion = Some(reply);
                                break;
                            }
                            Either::Right((None, _)) => return,
                            Either::Left(_) => break,
                        }
                    }
                }
                let mut result = if state.peek().draft_storage.hydrated.get() {
                    flush(state).await
                } else {
                    Err(
                        "Draft storage has not been read yet. Unsent drafts remain in memory."
                            .into(),
                    )
                };
                if let Some(completion) = completion {
                    // A native close waits for the newest revision, including
                    // an edit delivered while a previous write was in flight.
                    while result.is_ok() && state.peek().draft_storage.document.borrow().dirty() {
                        result = flush(state).await;
                    }
                    let _ = completion.send(result);
                }
            }
        }
    });
}
async fn delay() {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(drafts::DEBOUNCE_MS as u32).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_millis(drafts::DEBOUNCE_MS)).await;
}
async fn flush(state: Store<UiModel>) -> Result<(), String> {
    let handle = state.peek().draft_storage.clone();
    let pending = handle.document.borrow().prepare_write();
    let (revision, bytes) = match pending {
        Ok(Some(pending)) => pending,
        Ok(None) => return Ok(()),
        Err(error) => {
            state.draft_storage_error().set(Some(error.clone()));
            return Err(error);
        }
    };
    let result = write(&bytes).await;
    match result {
        Ok(()) => {
            let error = {
                let mut document = handle.document.borrow_mut();
                document.write_succeeded(revision);
                document
                    .recovery_error
                    .clone()
                    .or_else(|| document.sidecar_error.clone())
            };
            state.draft_storage_error().set(error);
            Ok(())
        }
        Err(error) => {
            state.draft_storage_error().set(Some(error.clone()));
            Err(error)
        }
    }
}
async fn write(bytes: &str) -> Result<(), String> {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<tests::WriteProbe>() {
        let _ = probe.started.unbounded_send(bytes.into());
        let gate = probe.gates.borrow_mut().pop_front();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        if probe.failures.get() > 0 {
            probe.failures.set(probe.failures.get() - 1);
            return Err("Draft storage write failed. Unsent drafts remain in memory.".into());
        }
        probe.writes.borrow_mut().push(bytes.into());
        let _ = probe.completed.unbounded_send(bytes.into());
        return Ok(());
    }
    #[cfg(target_arch = "wasm32")]
    let result = write_sync(bytes);
    #[cfg(not(target_arch = "wasm32"))]
    let result = dioxus::document::eval(&format!(
        "window.localStorage.setItem('t3code:rust-composer-drafts:v1', {}); return true;",
        serde_json::to_string(&bytes).unwrap()
    ))
    .join::<bool>()
    .await
    .map(|_| ())
    .map_err(|_| "Draft storage write failed. Unsent drafts remain in memory.".into());
    result
}

#[cfg(target_arch = "wasm32")]
fn write_sync(bytes: &str) -> Result<(), String> {
    web_sys::window()
        .ok_or("Draft storage unavailable.")?
        .local_storage()
        .map_err(|_| "Draft storage unavailable.")?
        .ok_or("Draft storage unavailable.")?
        .set_item(drafts::SIDECAR_KEY, bytes)
        .map_err(|_| "Draft storage write failed. Unsent drafts remain in memory.".into())
}
#[cfg(any(feature = "desktop", test))]
fn request_flush(
    state: Store<UiModel>,
) -> Result<futures_channel::oneshot::Receiver<Result<(), String>>, String> {
    let (reply, receiver) = futures_channel::oneshot::channel();
    let sender = state
        .peek()
        .draft_storage
        .sender
        .borrow()
        .clone()
        .ok_or("Draft writer is unavailable.")?;
    sender
        .unbounded_send(WriterRequest::Flush(reply))
        .map_err(|_| "Draft writer is unavailable.")?;
    Ok(receiver)
}
/// Tao delivers native CloseRequested to this handler before destroying the
/// webview. Hide it while the serial writer commits; a failed write reopens it.
/// OS app quit and mobile lifecycle events need their own shell integration.
pub fn use_native_close_flush(state: Store<UiModel>) {
    #[cfg(all(feature = "desktop", not(target_arch = "wasm32")))]
    {
        use dioxus::desktop::{
            WindowCloseBehaviour,
            tao::event::{Event, WindowEvent},
            use_window, use_wry_event_handler,
        };
        let window = use_window();
        let preferences = try_consume_context::<crate::client_settings::Writer>();
        let fonts = try_consume_context::<crate::font_service::Fonts>();
        let themes = try_consume_context::<crate::themes::Themes>();
        let closing = use_hook(|| Rc::new(Cell::new(false)));
        use_wry_event_handler(move |event, _| {
            if !matches!(
                event,
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                }
            ) || (!state.peek().draft_storage.document.borrow().dirty()
                && !preferences
                    .as_ref()
                    .is_some_and(|writer| writer.needs_flush())
                && !fonts.as_ref().is_some_and(|fonts| fonts.commits.pending())
                && !themes.as_ref().is_some_and(|themes| themes.needs_flush()))
            {
                return;
            }
            window.set_close_behavior(WindowCloseBehaviour::WindowHides);
            if closing.replace(true) {
                return;
            }
            let window = window.clone();
            let closing = closing.clone();
            let preferences = preferences.clone();
            let fonts = fonts.clone();
            let themes = themes.clone();
            spawn(async move {
                let mut result = Ok(());
                loop {
                    if let Some(themes) = &themes {
                        result = themes.flush().await;
                    }
                    if result.is_ok() {
                        if let Some(writer) = &preferences {
                            result = if let Some(fonts) = &fonts {
                                writer.flush_after(&fonts.commits).await
                            } else {
                                writer.flush().await
                            };
                        }
                    }
                    if result.is_ok() && state.peek().draft_storage.document.borrow().dirty() {
                        result=match request_flush(state){Ok(receiver)=>receiver.await.unwrap_or_else(|_|Err("Draft writer stopped before saving. Unsent drafts remain in memory.".into())),Err(error)=>Err(error)};
                    }
                    if result.is_err()
                        || (!state.peek().draft_storage.document.borrow().dirty()
                            && !preferences
                                .as_ref()
                                .is_some_and(|writer| writer.needs_flush())
                            && !fonts.as_ref().is_some_and(|fonts| fonts.commits.pending())
                            && !themes.as_ref().is_some_and(|themes| themes.needs_flush()))
                    {
                        break;
                    }
                }
                window.set_close_behavior(WindowCloseBehaviour::WindowCloses);
                if result.is_ok() {
                    window.close();
                } else {
                    closing.set(false);
                    window.set_visible(true);
                    state.error().set(result.err());
                }
            });
        });
    }
    #[cfg(not(all(feature = "desktop", not(target_arch = "wasm32"))))]
    {
        let _ = state;
    }
}
// Source flushes the deferred writer before unload. The callback and all JSON
// serialization are Rust; only native webview storage calls use its document bridge.
pub fn use_flush_on_unload(state: Store<UiModel>) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::{JsCast, closure::Closure};
        let handle = use_hook(move || state.peek().draft_storage.clone());
        let callback = use_hook(move || {
            Rc::new(Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                if !handle.hydrated.get() {
                    return;
                }
                let pending = handle.document.borrow().prepare_write();
                if let Ok(Some((revision, bytes))) = pending {
                    if write_sync(&bytes).is_ok() {
                        handle.document.borrow_mut().write_succeeded(revision);
                    }
                }
            }))
        });
        let registered = callback.clone();
        use_hook(move || {
            if let Some(window) = web_sys::window() {
                let _ = window.add_event_listener_with_callback(
                    "beforeunload",
                    registered.as_ref().as_ref().unchecked_ref(),
                );
            }
        });
        use_drop(move || {
            if let Some(window) = web_sys::window() {
                let _ = window.remove_event_listener_with_callback(
                    "beforeunload",
                    callback.as_ref().as_ref().unchecked_ref(),
                );
            }
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;
    use std::collections::VecDeque;
    #[derive(Clone)]
    pub(super) struct WriteProbe {
        pub started: futures_channel::mpsc::UnboundedSender<String>,
        pub completed: futures_channel::mpsc::UnboundedSender<String>,
        pub gates: Rc<RefCell<VecDeque<futures_channel::oneshot::Receiver<()>>>>,
        pub writes: Rc<RefCell<Vec<String>>>,
        pub failures: Rc<Cell<usize>>,
    }
    #[derive(Clone)]
    struct Harness {
        state: Rc<RefCell<Option<Store<UiModel>>>>,
        probe: WriteProbe,
    }
    fn child(state: Store<UiModel>) -> Element {
        use_effect(move || {
            let environment = t3_contracts::EnvironmentId::new("a").unwrap();
            edit_project_prompt(
                state,
                (environment.clone(), "project".into()),
                "First edit".into(),
            );
            save_choices(
                state,
                &(environment, "project".into()),
                &NewThreadChoices {
                    runtime_mode: Some(t3_contracts::RuntimeMode::ApprovalRequired),
                    ..Default::default()
                },
            );
            state.sidebar_open().set(false); // Immediately unmount this scope.
        });
        rsx! {span {"Composer"}}
    }
    fn harness(props: Harness) -> Element {
        use_context_provider(|| props.probe.clone());
        let state = use_store(|| UiModel {
            sidebar_open: true,
            ..Default::default()
        });
        *props.state.borrow_mut() = Some(state);
        state.peek().draft_storage.hydrated.set(true);
        use_writer(state);
        rsx! {if *state.sidebar_open().read() {Child {state}}}
    }
    #[component]
    fn Child(state: Store<UiModel>) -> Element {
        child(state)
    }
    async fn milestone(
        dom: &mut VirtualDom,
        receiver: &mut futures_channel::mpsc::UnboundedReceiver<String>,
    ) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(3),async {
            loop {tokio::select! {value=receiver.next()=>return value.unwrap(),_=dom.wait_for_work()=>dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations)}}
        }).await.expect("draft writer milestone")
    }
    #[test]
    fn imported_attachment_drafts_are_held_without_dispatch_or_tombstones() {
        #[derive(Clone)]
        struct Owned {
            state: Rc<RefCell<Option<Store<UiModel>>>>,
            source: String,
        }
        fn owner(props: Owned) -> Element {
            let state = use_store(|| UiModel {
                destination: Some(t3_contracts::EnvironmentId::new("a").unwrap()),
                active_thread: Some("thread".into()),
                draft: "Unsent text".into(),
                ..Default::default()
            });
            *props.state.borrow_mut() = Some(state);
            use_hook(move || {
                state.peek().draft_storage.document.borrow_mut().hydrate(
                    Some(props.source),
                    None,
                    "2026-10-08T12:00:00.000Z",
                )
            });
            rsx! {span {"Owned drafts"}}
        }
        let file = json!({"id":"file","name":"unsent.txt","mimeType":"text/plain","sizeBytes":1});
        let source=json!({"version":9,"state":{"draftsByThreadKey":{"a:thread":{"prompt":"Unsent text","attachments":[],"files":[file.clone()]},"draft":{"prompt":"New draft","attachments":[],"files":[file]}},"draftThreadsByThreadKey":{"draft":{"threadId":"draft","environmentId":"a","projectId":"project","logicalProjectKey":"a:project"}},"logicalProjectDraftThreadKeyByLogicalProjectKey":{"a:project":"draft"}}}).to_string();
        let props = Owned {
            state: Rc::new(RefCell::new(None)),
            source: source.clone(),
        };
        let mut dom = VirtualDom::new_with_props(owner, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        let transport = crate::runtime::TransportHandle::default();
        dom.in_scope(ScopeId::APP, || {
            crate::runtime::send_message(&transport, state);
            assert!(state.error().peek().as_ref().unwrap().contains("preserved"));
            assert!(state.pending_messages().peek().is_empty());
            let selection =
                serde_json::from_value(json!({"instanceId":"codex","model":"fixture"})).unwrap();
            assert!(
                crate::runtime::launch_thread(
                    &transport,
                    state,
                    "project",
                    &selection,
                    t3_contracts::RuntimeMode::FullAccess,
                    t3_contracts::ThreadLaunchWorkspaceStrategy::Root(
                        t3_contracts::WorkspaceRoot { branch: None }
                    ),
                    Some("New draft")
                )
                .is_none()
            );
            assert!(state.error().peek().as_ref().unwrap().contains("preserved"));
            assert!(state.pending_launches().peek().is_empty());
        });
        let handle = state.peek().draft_storage.clone();
        let document = handle.document.borrow();
        assert_eq!(document.source_bytes.as_deref(), Some(source.as_str()));
        assert!(
            document
                .changes(&DraftTarget::thread("a", "thread"))
                .is_none()
        );
        assert!(
            document
                .changes(&DraftTarget::project("a", "project"))
                .is_none()
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn root_writer_survives_composer_unmount_and_serializes_slow_native_writes() {
        let (started, mut starts) = futures_channel::mpsc::unbounded();
        let (completed, mut completions) = futures_channel::mpsc::unbounded();
        let (release, gate) = futures_channel::oneshot::channel();
        let probe = WriteProbe {
            started,
            completed,
            gates: Rc::new(RefCell::new(VecDeque::from([gate]))),
            writes: Rc::new(RefCell::new(vec![])),
            failures: Rc::new(Cell::new(0)),
        };
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            probe: probe.clone(),
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let first = milestone(&mut dom, &mut starts).await;
        let state = props.state.borrow().unwrap();
        assert!(!*state.sidebar_open().peek());
        assert!(first.contains("First edit"));
        assert!(first.contains("approval-required"));
        dom.in_scope(ScopeId::APP, || {
            edit_project_prompt(
                state,
                (
                    t3_contracts::EnvironmentId::new("a").unwrap(),
                    "project".into(),
                ),
                "Latest unsent edit".into(),
            )
        });
        state
            .draft_storage_error()
            .set(Some("Old write failed".into()));
        release.send(()).unwrap();
        assert_eq!(milestone(&mut dom, &mut completions).await, first);
        let latest = milestone(&mut dom, &mut starts).await;
        assert!(latest.contains("Latest unsent edit"));
        assert_eq!(milestone(&mut dom, &mut completions).await, latest);
        assert_eq!(*probe.writes.borrow(), vec![first, latest.clone()]);
        assert!(state.draft_storage_error().peek().is_none());
        assert!(!state.peek().draft_storage.document.borrow().dirty());
        let mut reload = DraftStorage::default();
        reload.hydrate(None, Some(latest), "2026-10-08T12:00:00.000Z");
        assert_eq!(
            reload.prompt(&DraftTarget::project("a", "project")),
            Some("Latest unsent edit")
        );
        assert_eq!(
            reload
                .changes(&DraftTarget::project("a", "project"))
                .unwrap()
                .choices
                .as_ref()
                .unwrap()["runtimeMode"],
            json!("approval-required")
        );
    }
    async fn receipt(
        dom: &mut VirtualDom,
        receiver: &mut futures_channel::oneshot::Receiver<Result<(), String>>,
    ) -> Result<(), String> {
        tokio::time::timeout(std::time::Duration::from_secs(3),async {
            loop {tokio::select! {value=&mut *receiver=>return value.unwrap(),_=dom.wait_for_work()=>dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations)}}
        }).await.expect("close flush receipt")
    }
    #[tokio::test(flavor = "current_thread")]
    async fn close_receipt_waits_for_edit_arriving_during_native_write() {
        let (started, mut starts) = futures_channel::mpsc::unbounded();
        let (completed, mut completions) = futures_channel::mpsc::unbounded();
        let (release_first, first_gate) = futures_channel::oneshot::channel();
        let (release_latest, latest_gate) = futures_channel::oneshot::channel();
        let probe = WriteProbe {
            started,
            completed,
            gates: Rc::new(RefCell::new(VecDeque::from([first_gate, latest_gate]))),
            writes: Rc::new(RefCell::new(vec![])),
            failures: Rc::new(Cell::new(0)),
        };
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            probe: probe.clone(),
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let first = milestone(&mut dom, &mut starts).await;
        let state = props.state.borrow().unwrap();
        let mut close = dom.in_scope(ScopeId::APP, || {
            edit_project_prompt(
                state,
                (
                    t3_contracts::EnvironmentId::new("a").unwrap(),
                    "project".into(),
                ),
                "Before close".into(),
            );
            request_flush(state).unwrap()
        });
        assert_eq!(close.try_recv().unwrap(), None);
        release_first.send(()).unwrap();
        assert_eq!(milestone(&mut dom, &mut completions).await, first);
        let latest = milestone(&mut dom, &mut starts).await;
        assert!(latest.contains("Before close"));
        assert_eq!(close.try_recv().unwrap(), None);
        dom.in_scope(ScopeId::APP, || {
            edit_project_prompt(
                state,
                (
                    t3_contracts::EnvironmentId::new("a").unwrap(),
                    "project".into(),
                ),
                "During close write".into(),
            )
        });
        release_latest.send(()).unwrap();
        assert_eq!(milestone(&mut dom, &mut completions).await, latest);
        let final_bytes = milestone(&mut dom, &mut starts).await;
        assert!(final_bytes.contains("During close write"));
        assert_eq!(milestone(&mut dom, &mut completions).await, final_bytes);
        assert_eq!(receipt(&mut dom, &mut close).await, Ok(()));
        assert!(!state.peek().draft_storage.document.borrow().dirty());
        assert_eq!(probe.writes.borrow().last(), Some(&final_bytes));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn failed_close_flush_keeps_dirty_draft_until_successful_retry() {
        let (started, _) = futures_channel::mpsc::unbounded();
        let (completed, _) = futures_channel::mpsc::unbounded();
        let probe = WriteProbe {
            started,
            completed,
            gates: Rc::new(RefCell::new(VecDeque::new())),
            writes: Rc::new(RefCell::new(vec![])),
            failures: Rc::new(Cell::new(1)),
        };
        let props = Harness {
            state: Rc::new(RefCell::new(None)),
            probe,
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        let state = props.state.borrow().unwrap();
        // Queue a close before the normal deferred write so it bypasses debounce.
        dom.in_scope(ScopeId::APP, || {
            edit_project_prompt(
                state,
                (
                    t3_contracts::EnvironmentId::new("a").unwrap(),
                    "project".into(),
                ),
                "Unsent close draft".into(),
            )
        });
        let mut close = dom.in_scope(ScopeId::APP, || request_flush(state).unwrap());
        assert!(
            receipt(&mut dom, &mut close)
                .await
                .unwrap_err()
                .contains("write failed")
        );
        assert!(state.peek().draft_storage.document.borrow().dirty());
        assert!(state.draft_storage_error().peek().is_some());
        let mut retry = dom.in_scope(ScopeId::APP, || request_flush(state).unwrap());
        assert_eq!(receipt(&mut dom, &mut retry).await, Ok(()));
        assert!(!state.peek().draft_storage.document.borrow().dirty());
        assert!(state.draft_storage_error().peek().is_none());
    }
}
