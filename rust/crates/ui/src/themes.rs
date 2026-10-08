//! App-owned theme selection, storage receipts and OS appearance subscriptions.
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use t3_client::themes::{
    self, Appearance, Catalog, Definition, Mode,
    library::Library,
    storage::{self, Action, ReadState, Snapshot, Storage},
};
type Replies = Rc<RefCell<BTreeMap<u64, futures_channel::oneshot::Sender<Result<Value, String>>>>>;
#[allow(dead_code)]
enum Command {
    Refresh(Option<Option<String>>),
    Change(
        Choice,
        Option<futures_channel::oneshot::Sender<Result<(), String>>>,
    ),
    Environment(Vec<t3_contracts::EnvironmentTheme>),
    Preview(Option<(themes::Colors, Appearance)>),
    Save(
        t3_client::themes::editor::Draft,
        futures_channel::oneshot::Sender<Result<Value, String>>,
    ),
    Remove(
        Vec<String>,
        futures_channel::oneshot::Sender<Result<Value, String>>,
    ),
    Import(
        Definition,
        futures_channel::oneshot::Sender<Result<Value, String>>,
    ),
    ImportMany(
        Vec<Definition>,
        ImportMode,
        Option<String>,
        futures_channel::oneshot::Sender<Result<Value, String>>,
    ),
    Library(
        LibraryAction,
        futures_channel::oneshot::Sender<Result<Value, String>>,
    ),
    Flush {
        pending: bool,
        reply: futures_channel::oneshot::Sender<Result<(), String>>,
    },
}
#[derive(Clone)]
pub enum Choice {
    Set(Action),
    AssignHalf(Appearance, Option<String>),
}
#[derive(Clone)]
pub enum LibraryAction {
    Install(Definition),
    Update(Definition),
    Remove(Vec<String>),
    Replace {
        id: String,
        themes: Vec<Value>,
        expected: Option<Vec<Value>>,
    },
}
#[derive(Clone, Copy)]
pub enum ImportMode {
    New,
    Update,
    Copy,
}
#[derive(Clone)]
pub struct Themes {
    pub snapshot: Signal<Snapshot>,
    pub catalog: Signal<Rc<Catalog>>,
    pub library: Signal<Option<Library>>,
    pub error: Signal<Option<String>>,
    pub ready: Signal<bool>,
    pub editor: Signal<Option<crate::theme_library::EditorSession>>,
    commands: futures_channel::mpsc::UnboundedSender<Command>,
    eval: Signal<Option<document::Eval>>,
    replies: Replies,
    next: Rc<Cell<u64>>,
    system_dark: Rc<Cell<bool>>,
    pending: Rc<Cell<usize>>,
}
struct ChangeGuard(Rc<Cell<usize>>);
impl Drop for ChangeGuard {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}
struct Pending {
    id: u64,
    replies: Replies,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.replies.borrow_mut().remove(&self.id);
    }
}
impl Themes {
    pub fn library_change(
        &self,
        action: LibraryAction,
    ) -> impl std::future::Future<Output = Result<Value, String>> + use<> {
        let (tx, rx) = futures_channel::oneshot::channel();
        self.pending.set(self.pending.get() + 1);
        let sent = self.commands.unbounded_send(Command::Library(action, tx));
        if sent.is_err() {
            self.pending.set(self.pending.get() - 1);
        }
        async move {
            sent.map_err(|_| "Theme library controls are unavailable.".to_owned())?;
            rx.await
                .map_err(|_| "Theme library write was canceled.".to_owned())?
        }
    }
    pub fn save_editor(
        &self,
        draft: t3_client::themes::editor::Draft,
    ) -> impl std::future::Future<Output = Result<Value, String>> + use<> {
        self.transaction(move |reply| Command::Save(draft, reply))
    }
    pub fn remove_themes(
        &self,
        ids: Vec<String>,
    ) -> impl std::future::Future<Output = Result<Value, String>> + use<> {
        self.transaction(move |reply| Command::Remove(ids, reply))
    }
    pub fn import_theme(
        &self,
        theme: Definition,
    ) -> impl std::future::Future<Output = Result<Value, String>> + use<> {
        self.transaction(move |reply| Command::Import(theme, reply))
    }
    pub fn import_many(
        &self,
        themes: Vec<Definition>,
        mode: ImportMode,
        preferred: Option<String>,
    ) -> impl std::future::Future<Output = Result<Value, String>> + use<> {
        self.transaction(move |reply| Command::ImportMany(themes, mode, preferred, reply))
    }
    fn transaction<
        F: FnOnce(futures_channel::oneshot::Sender<Result<Value, String>>) -> Command,
    >(
        &self,
        command: F,
    ) -> impl std::future::Future<Output = Result<Value, String>> + use<F> {
        let (tx, rx) = futures_channel::oneshot::channel();
        self.pending.set(self.pending.get() + 1);
        let sent = self.commands.unbounded_send(command(tx));
        if sent.is_err() {
            self.pending.set(self.pending.get() - 1);
        }
        async move {
            sent.map_err(|_| "Theme controls are unavailable.".to_owned())?;
            rx.await
                .map_err(|_| "Theme transaction was canceled.".to_owned())?
        }
    }
    pub fn preview(&self, colors: Option<(themes::Colors, Appearance)>) {
        let _ = self.commands.unbounded_send(Command::Preview(colors));
    }
    pub fn retry_library(&self) {
        let _ = self
            .commands
            .unbounded_send(Command::Refresh(Some(Some(themes::CUSTOM_KEY.into()))));
    }

    pub fn choice(
        &self,
        choice: Choice,
    ) -> impl std::future::Future<Output = Result<(), String>> + use<> {
        let (tx, rx) = futures_channel::oneshot::channel();
        self.pending.set(self.pending.get() + 1);
        let sent = self
            .commands
            .unbounded_send(Command::Change(choice, Some(tx)));
        if sent.is_err() {
            self.pending.set(self.pending.get() - 1);
        }
        async move {
            sent.map_err(|_| "Theme controls are unavailable.".to_owned())?;
            rx.await
                .map_err(|_| "Theme choice was canceled.".to_owned())?
        }
    }
    pub fn change(&self, action: Action) {
        drop(self.choice(Choice::Set(action)));
    }
    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    pub fn needs_flush(&self) -> bool {
        self.pending.get() != 0
    }
    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    pub async fn flush(&self) -> Result<(), String> {
        let (tx, rx) = futures_channel::oneshot::channel();
        self.commands
            .unbounded_send(Command::Flush {
                pending: self.needs_flush(),
                reply: tx,
            })
            .map_err(|_| "Theme controls are unavailable.".to_owned())?;
        rx.await
            .map_err(|_| "Theme controls stopped before saving.".to_owned())?
    }
    async fn request(&self, mut value: Value) -> Result<Value, String> {
        let id = self.next.get();
        self.next.set(id + 1);
        value["id"] = id.into();
        let eval = self
            .eval
            .peek()
            .ok_or("Theme controls are still loading.")?;
        let (tx, rx) = futures_channel::oneshot::channel();
        self.replies.borrow_mut().insert(id, tx);
        let _pending = Pending {
            id,
            replies: self.replies.clone(),
        };
        eval.send(value).map_err(|cause| cause.to_string())?;
        rx.await
            .map_err(|_| "Theme storage request was canceled.".to_owned())?
    }
}
struct BrowserStorage(Themes);
impl Storage for BrowserStorage {
    async fn get(&mut self, key: &str) -> Result<Option<String>, String> {
        serde_json::from_value(self.0.request(json!({"type":"get","key":key})).await?)
            .map_err(|_| "Theme storage returned an invalid value.".into())
    }
    async fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.0
            .request(json!({"type":"set","key":key,"value":value}))
            .await
            .map(|_| ())
    }
    async fn remove(&mut self, key: &str) -> Result<(), String> {
        self.0
            .request(json!({"type":"remove","key":key}))
            .await
            .map(|_| ())
    }
}
pub fn use_themes(state: Store<UiModel>) {
    let snapshot = use_signal(Snapshot::default);
    let catalog = use_signal(|| Rc::new(Catalog::default()));
    let library = use_signal(|| None);
    let error = use_signal(|| None);
    let ready = use_signal(|| false);
    let editor = use_signal(|| None);
    let eval = use_signal(|| None::<document::Eval>);
    let (commands, receiver) = use_hook(|| {
        let (tx, rx) = futures_channel::mpsc::unbounded();
        (tx, Rc::new(RefCell::new(Some(rx))))
    });
    let replies = use_hook(|| Rc::new(RefCell::new(BTreeMap::new())));
    let next = use_hook(|| Rc::new(Cell::new(1)));
    let system_dark = use_hook(|| Rc::new(Cell::new(false)));
    let pending = use_hook(|| Rc::new(Cell::new(0)));
    let service = Themes {
        snapshot,
        catalog,
        library,
        error,
        ready,
        editor,
        commands,
        eval,
        replies,
        next,
        system_dark,
        pending,
    };
    use_context_provider(|| service.clone());
    let bridge_id = use_hook(|| format!("theme-{}", uuid::Uuid::new_v4()));
    let launch_id = bridge_id.clone();
    let bridge_service = service.clone();
    use_future(move || {
        let launch_id = launch_id.clone();
        let mut service = bridge_service.clone();
        async move {
            let args = json!({"id":launch_id});
            let mut bridge = document::eval(&format!(
                "const args={args};\n{}",
                include_str!("../assets/theme_abi.js")
            ));
            service.eval.set(Some(bridge));
            loop {
                let event: Value = match bridge.recv().await {
                    Ok(event) => event,
                    Err(cause) => {
                        service
                            .error
                            .set(Some(format!("Theme controls disconnected: {cause}")));
                        break;
                    }
                };
                match event["type"].as_str() {
                    Some("ready") => {
                        service
                            .system_dark
                            .set(event["dark"].as_bool().unwrap_or(false));
                        service.ready.set(true);
                        let _ = service.commands.unbounded_send(Command::Refresh(None));
                    }
                    Some("media") => {
                        service
                            .system_dark
                            .set(event["dark"].as_bool().unwrap_or(false));
                        let _ = service.commands.unbounded_send(Command::Refresh(None));
                    }
                    Some("storage") => {
                        let key = event["key"].as_str();
                        if key.is_none()
                            || [
                                themes::THEME_KEY,
                                themes::MODE_KEY,
                                themes::FOLLOW_KEY,
                                themes::HALVES_KEY,
                                themes::CUSTOM_KEY,
                            ]
                            .contains(&key.unwrap())
                        {
                            let _ = service
                                .commands
                                .unbounded_send(Command::Refresh(Some(key.map(Into::into))));
                        }
                    }
                    Some("response") => {
                        if let Some(id) = event["id"].as_u64() {
                            if let Some(reply) = service.replies.borrow_mut().remove(&id) {
                                let _ = reply.send(if let Some(error) = event["error"].as_str() {
                                    Err(error.into())
                                } else {
                                    Ok(event["value"].clone())
                                });
                            }
                        }
                    }
                    Some("error") => {
                        service.error.set(event["message"].as_str().map(Into::into));
                        break;
                    }
                    _ => {}
                }
            }
            service.ready.set(false);
            service.eval.set(None);
            service.replies.borrow_mut().clear();
        }
    });
    let environment_service = service.clone();
    let last_environment = use_hook(|| Rc::new(RefCell::new(None)));
    use_effect(move || {
        if !*environment_service.ready.read() {
            return;
        }
        let published = state
            .typed_config()
            .read()
            .as_ref()
            .and_then(|c| c.environment_themes.as_ref())
            .and_then(Option::as_ref)
            .cloned()
            .unwrap_or_default();
        if last_environment.borrow().as_ref() == Some(&published) {
            return;
        }
        *last_environment.borrow_mut() = Some(published.clone());
        let _ = environment_service
            .commands
            .unbounded_send(Command::Environment(published));
    });
    let actor_service = service.clone();
    use_future(move || {
        let mut rx = receiver
            .borrow_mut()
            .take()
            .expect("theme actor mounts once");
        let mut service = actor_service.clone();
        async move {
            let mut library = None::<Library>;
            let mut preview = None::<(themes::Colors, Appearance)>;
            let mut reads = ReadState::default();
            let mut storage = BrowserStorage(service.clone());
            let mut write_failure = None;
            let mut last_applied = None;
            while let Some(command) = rx.next().await {
                let mut force = matches!(&command, Command::Refresh(Some(None)))
                    || matches!(&command,Command::Refresh(Some(Some(key))) if key==themes::CUSTOM_KEY);
                let _change = matches!(
                    command,
                    Command::Change(..)
                        | Command::Library(..)
                        | Command::Save(..)
                        | Command::Remove(..)
                        | Command::Import(..)
                        | Command::ImportMany(..)
                )
                .then(|| ChangeGuard(service.pending.clone()));
                if !matches!(command, Command::Flush { .. }) && (library.is_none() || force) {
                    let raw = storage.get(themes::CUSTOM_KEY).await;
                    let next = Library::read(
                        &service.catalog.peek(),
                        raw.as_ref().map(|v| v.as_deref()).map_err(|_| ()),
                    );
                    publish_library(&mut service, &next);
                    library = Some(next);
                }
                let mut catalog = service.catalog.peek().clone();
                match command {
                    Command::Preview(next) => {
                        preview = next;
                        force = true;
                    }
                    Command::Environment(published) => {
                        let next = themes::environment::definitions(&catalog, &published);
                        if catalog.environment == next {
                            continue;
                        }
                        let mut next_catalog = (*catalog).clone();
                        next_catalog.environment = next;
                        catalog = Rc::new(next_catalog);
                        service.catalog.set(catalog.clone());
                        force = true;
                    }
                    Command::Library(action, reply) => {
                        let outcome =
                            mutate_library(&mut service, &mut storage, &mut library, action).await;
                        if let Err(error) = &outcome {
                            write_failure = Some(error.clone());
                            service.error.set(write_failure.clone());
                            let _ = reply.send(outcome);
                            continue;
                        }
                        write_failure = None;
                        catalog = service.catalog.peek().clone();
                        force = true;
                        let _ = reply.send(outcome);
                    }
                    Command::Save(draft, reply) => {
                        let outcome = save_editor_transaction(
                            &mut service,
                            &mut storage,
                            &mut reads,
                            &mut library,
                            draft,
                        )
                        .await;
                        write_failure = outcome.as_ref().err().cloned();
                        catalog = service.catalog.peek().clone();
                        force = true;
                        if outcome.is_ok() {
                            preview = None;
                        }
                        let _ = reply.send(outcome);
                    }
                    Command::Remove(ids, reply) => {
                        let outcome = remove_themes_transaction(
                            &mut service,
                            &mut storage,
                            &mut reads,
                            &mut library,
                            ids,
                        )
                        .await;
                        write_failure = outcome.as_ref().err().cloned();
                        catalog = service.catalog.peek().clone();
                        force = true;
                        let _ = reply.send(outcome);
                    }
                    Command::Import(theme, reply) => {
                        let outcome = import_theme_transaction(
                            &mut service,
                            &mut storage,
                            &mut reads,
                            &mut library,
                            theme,
                        )
                        .await;
                        write_failure = outcome.as_ref().err().cloned().or_else(|| {
                            outcome
                                .as_ref()
                                .ok()
                                .and_then(|v| v["activationError"].as_str())
                                .map(str::to_owned)
                        });
                        catalog = service.catalog.peek().clone();
                        force = true;
                        if outcome.is_ok() {
                            preview = None;
                        }
                        let _ = reply.send(outcome);
                    }
                    Command::ImportMany(themes, mode, preferred, reply) => {
                        let mut saved = Vec::new();
                        let mut conflicts = Vec::new();
                        let mut failures = Vec::new();
                        for theme in themes {
                            let current = service.catalog.peek().clone();
                            if matches!(mode, ImportMode::New)
                                && current.custom.iter().any(|t| t.id == theme.id)
                            {
                                conflicts.push(theme);
                                continue;
                            }
                            let label = theme.label.clone();
                            let candidate = match mode {
                                ImportMode::Copy => themes::import::versioned_copy(
                                    &current,
                                    &theme,
                                    preferred.as_deref(),
                                ),
                                ImportMode::Update => {
                                    let mut next = theme;
                                    next.collection = current
                                        .custom
                                        .iter()
                                        .find(|t| t.id == next.id)
                                        .and_then(|t| t.collection.clone())
                                        .or(next.collection);
                                    Ok(next)
                                }
                                ImportMode::New => Ok(theme),
                            };
                            let outcome = match candidate {
                                Err(error) => Err(error),
                                Ok(theme) => {
                                    mutate_library(
                                        &mut service,
                                        &mut storage,
                                        &mut library,
                                        if matches!(mode, ImportMode::Update) {
                                            LibraryAction::Update(theme)
                                        } else {
                                            LibraryAction::Install(theme)
                                        },
                                    )
                                    .await
                                }
                            };
                            match outcome {
                                Ok(value) => saved.push(value),
                                Err(error) => failures.push(format!("{label}: {error}")),
                            };
                        }
                        write_failure = (!failures.is_empty()).then(|| failures.join(" — "));
                        catalog = service.catalog.peek().clone();
                        force = true;
                        let _ = reply.send(Ok(
                            json!({"saved":saved,"conflicts":conflicts,"failures":failures}),
                        ));
                    }
                    Command::Flush { pending, reply } => {
                        let _ = reply.send(if pending {
                            write_failure.clone().map_or(Ok(()), Err)
                        } else {
                            Ok(())
                        });
                        continue;
                    }
                    Command::Refresh(key) => {
                        if let Some(key) = key {
                            reads.storage_changed(key.as_deref());
                        }
                    }
                    Command::Change(choice, reply) => {
                        preview = None;
                        match apply_choice(&mut storage, &catalog, &mut reads, choice).await {
                            Ok(()) => {
                                write_failure = None;
                                if let Some(reply) = reply {
                                    let _ = reply.send(Ok(()));
                                }
                            }
                            Err(error) => {
                                write_failure =
                                    Some(format!("Theme choice was not saved: {error}"));
                                service.error.set(write_failure.clone());
                                if let Some(reply) = reply {
                                    let _ = reply.send(Err(write_failure.clone().unwrap()));
                                }
                                force = true;
                            }
                        }
                    }
                }
                let snapshot = reads
                    .snapshot(&mut storage, &catalog, service.system_dark.get())
                    .await;
                let appearance = preview
                    .as_ref()
                    .map(|(_, mode)| *mode)
                    .unwrap_or(snapshot.resolved_theme);
                if state.peek().dark != (appearance == Appearance::Dark) {
                    state.dark().set(appearance == Appearance::Dark);
                }
                if *service.snapshot.peek() != snapshot {
                    service.snapshot.set(snapshot.clone());
                }
                if !force && last_applied.as_ref() == Some(&snapshot) {
                    service
                        .error
                        .set(reads.failure.clone().or_else(|| write_failure.clone()));
                    continue;
                }
                // Root class/id/variables are changed together by the ABI before reading CSS,
                // independent of when the Dioxus app attributes finish rendering.
                let apply = if let Some((colors, appearance)) = &preview {
                    let variables: BTreeMap<_, _> = catalog
                        .data
                        .variables
                        .iter()
                        .filter_map(|(role, variable)| {
                            colors
                                .get(role)
                                .filter(|v| themes::color::canonical(v).is_some())
                                .map(|v| (variable.clone(), v.clone()))
                        })
                        .collect();
                    json!({"type":"apply","dark":*appearance==Appearance::Dark,"paletteId":"__preview","variables":variables,"chrome":colors.get("chrome").filter(|v|themes::color::canonical(v).is_some())})
                } else {
                    palette_command(&catalog, &snapshot)
                };
                let chrome = apply["chrome"].as_str().map(str::to_owned);
                let surfaces = match service.request(apply).await {
                    Ok(value) => value,
                    Err(error) => {
                        last_applied = None;
                        service
                            .error
                            .set(Some(format!("Theme could not be applied: {error}")));
                        continue;
                    }
                };
                if let Some(color) = chrome
                    .as_deref()
                    .and_then(themes::normalize_browser_color)
                    .or_else(|| {
                        surfaces["surface"]
                            .as_str()
                            .and_then(themes::normalize_browser_color)
                    })
                    .or_else(|| {
                        surfaces["body"]
                            .as_str()
                            .and_then(themes::normalize_browser_color)
                    })
                {
                    if let Err(error) = service
                        .request(json!({"type":"chrome","color":color}))
                        .await
                    {
                        last_applied = None;
                        service
                            .error
                            .set(Some(format!("Browser theme could not be applied: {error}")));
                        continue;
                    }
                }
                sync_native(&catalog, &snapshot);
                last_applied = Some(snapshot);
                service
                    .error
                    .set(reads.failure.clone().or_else(|| write_failure.clone()));
            }
        }
    });
    let dispose_service = service.clone();
    use_drop(move || {
        if let Some(eval) = *dispose_service.eval.peek() {
            let _ = eval.send(json!({"type":"dispose"}));
        }
        document::eval(&format!(
            "const slot=window.__t3RustThemes?.get({});slot?.dispose?.();",
            json!(bridge_id)
        ));
        dispose_service.replies.borrow_mut().clear();
    });
}
async fn mutate_library(
    service: &mut Themes,
    storage: &mut BrowserStorage,
    library: &mut Option<Library>,
    action: LibraryAction,
) -> Result<Value, String> {
    let mut catalog = service.catalog.peek().clone();
    // Collection downloads must compare against a fresh read, not the cached list.
    if matches!(action, LibraryAction::Replace { .. }) {
        let raw = storage.get(themes::CUSTOM_KEY).await;
        let next = Library::read(&catalog, raw.as_ref().map(|v| v.as_deref()).map_err(|_| ()));
        publish_library(service, &next);
        *library = Some(next);
        catalog = service.catalog.peek().clone();
    }
    let current = library.as_ref().expect("library read before mutation");
    let (candidate, result) = match &action {
        LibraryAction::Install(theme) => (
            current.install(&catalog, theme).map(Some),
            serde_json::to_value(theme).unwrap(),
        ),
        LibraryAction::Update(theme) => (
            current.update(&catalog, theme).map(Some),
            serde_json::to_value(theme).unwrap(),
        ),
        LibraryAction::Remove(ids) => (current.remove(&catalog, ids), Value::Null),
        LibraryAction::Replace {
            id,
            themes,
            expected,
        } => (
            current
                .replace_collection(&catalog, id, themes, expected.as_deref())
                .map(Some),
            Value::Null,
        ),
    };
    let outcome = match candidate {
        Err(error) => Err(error),
        Ok(None) => Ok(result),
        Ok(Some(next)) => {
            let result = match &action {
                LibraryAction::Install(theme) | LibraryAction::Update(theme) => next
                    .ready()
                    .expect("ready candidate")
                    .1
                    .iter()
                    .find(|t| t.id == theme.id)
                    .map(|t| themes::library::definition_value(&catalog, t))
                    .expect("canonical mutation result"),
                LibraryAction::Replace { themes, .. } => json!(
                    themes
                        .iter()
                        .filter_map(|v| themes::library::stored_theme(&catalog, v))
                        .map(|t| themes::library::definition_value(&catalog, &t))
                        .collect::<Vec<_>>()
                ),
                LibraryAction::Remove(_) => Value::Null,
            };
            match storage
                .set(
                    themes::CUSTOM_KEY,
                    &next.bytes().expect("ready library candidate"),
                )
                .await
            {
                Err(_) => Err(format!(
                    "Failed to write the theme library to {}.",
                    themes::CUSTOM_KEY
                )),
                Ok(()) => {
                    publish_library(service, &next);
                    *library = Some(next);
                    Ok(result)
                }
            }
        }
    };
    outcome
}
async fn save_editor_transaction(
    service: &mut Themes,
    storage: &mut BrowserStorage,
    reads: &mut ReadState,
    library: &mut Option<Library>,
    draft: t3_client::themes::editor::Draft,
) -> Result<Value, String> {
    let catalog = service.catalog.peek().clone();
    let plan = themes::editor::save(&catalog, &draft)?;
    let action = if plan.created {
        LibraryAction::Install(plan.theme.clone())
    } else {
        LibraryAction::Update(plan.theme.clone())
    };
    let saved = mutate_library(service, storage, library, action).await?;
    if let Some(retired) = &plan.retired {
        if let Err(error) = mutate_library(
            service,
            storage,
            library,
            LibraryAction::Remove(vec![retired.id.clone()]),
        )
        .await
        {
            if let Some(target) = &plan.merge_target {
                let _ = mutate_library(
                    service,
                    storage,
                    library,
                    LibraryAction::Update(target.clone()),
                )
                .await;
            }
            return Err(error);
        }
    }
    if plan.created || plan.merged_appearance.is_some() {
        let catalog = service.catalog.peek().clone();
        if storage::apply(
            storage,
            &catalog,
            reads,
            Action::Theme(plan.theme.id.clone()),
        )
        .await
        .is_err()
        {
            if plan.created {
                let _ = mutate_library(
                    service,
                    storage,
                    library,
                    LibraryAction::Remove(vec![plan.theme.id.clone()]),
                )
                .await;
            } else if let Some(target) = &plan.merge_target {
                let restored = mutate_library(
                    service,
                    storage,
                    library,
                    LibraryAction::Update(target.clone()),
                )
                .await;
                if restored.is_ok() {
                    if let Some(retired) = &plan.retired {
                        let _ = mutate_library(
                            service,
                            storage,
                            library,
                            LibraryAction::Install(retired.clone()),
                        )
                        .await;
                    }
                }
            }
            return Err("Theme saved, but it could not be made active. Try again.".into());
        }
    }
    Ok(saved)
}
async fn import_theme_transaction(
    service: &mut Themes,
    storage: &mut BrowserStorage,
    reads: &mut ReadState,
    library: &mut Option<Library>,
    theme: Definition,
) -> Result<Value, String> {
    let saved = mutate_library(
        service,
        storage,
        library,
        LibraryAction::Install(theme.clone()),
    )
    .await?;
    let catalog = service.catalog.peek().clone();
    let modes = theme.modes();
    if modes.len() == 1 {
        if let Err(error) = apply_choice(
            storage,
            &catalog,
            reads,
            Choice::AssignHalf(modes[0], Some(theme.id.clone())),
        )
        .await
        {
            return Ok(json!({"theme":saved,"activationError":error}));
        }
    } else if storage::apply(storage, &catalog, reads, Action::Theme(theme.id.clone()))
        .await
        .is_err()
    {
        let _ = mutate_library(
            service,
            storage,
            library,
            LibraryAction::Remove(vec![theme.id]),
        )
        .await;
        return Err("Theme added, but it could not be selected. Try again.".into());
    }
    Ok(saved)
}
async fn remove_themes_transaction(
    service: &mut Themes,
    storage: &mut BrowserStorage,
    reads: &mut ReadState,
    library: &mut Option<Library>,
    ids: Vec<String>,
) -> Result<Value, String> {
    if ids.is_empty() {
        return Ok(Value::Null);
    }
    let catalog = service.catalog.peek().clone();
    let snapshot = reads
        .snapshot(storage, &catalog, service.system_dark.get())
        .await;
    let removes_base = catalog
        .definition(&snapshot.theme)
        .is_some_and(|t| ids.contains(&t.id));
    let raw = storage.get(themes::HALVES_KEY).await.ok().flatten();
    let halves = themes::raw_halves(raw.as_deref());
    if removes_base {
        storage::apply(
            storage,
            &catalog,
            reads,
            Action::Theme(snapshot.appearance_mode.key().into()),
        )
        .await?;
    }
    for mode in [Appearance::Light, Appearance::Dark] {
        if let Some(half) = halves.get(mode) {
            if ids.iter().any(|id| id == half) {
                storage::apply(storage, &catalog, reads, Action::Half(mode, None)).await?;
            } else if removes_base {
                storage::apply(
                    storage,
                    &catalog,
                    reads,
                    Action::Half(mode, Some(half.into())),
                )
                .await?;
            }
        }
    }
    mutate_library(service, storage, library, LibraryAction::Remove(ids)).await
}
async fn apply_choice(
    storage: &mut impl Storage,
    catalog: &Catalog,
    reads: &mut ReadState,
    choice: Choice,
) -> Result<(), String> {
    match choice {
        Choice::Set(action) => storage::apply(storage, catalog, reads, action).await,
        Choice::AssignHalf(appearance, id) => {
            let theme = reads.theme(storage, catalog).await;
            if id.is_none() {
                if let Some(base) = catalog.definition(&theme) {
                    let other = if appearance == Appearance::Light {
                        Appearance::Dark
                    } else {
                        Appearance::Light
                    };
                    let raw = storage.get(themes::HALVES_KEY).await.ok().flatten();
                    let halves = themes::raw_halves(raw.as_deref());
                    let owner = match other {
                        Appearance::Light => halves.light,
                        Appearance::Dark => halves.dark,
                    }
                    .unwrap_or_else(|| base.id.clone());
                    let mode = storage::read_mode(storage, catalog, &theme).await;
                    storage::apply(storage, catalog, reads, Action::Theme(mode.key().into()))
                        .await?;
                    if let Err(error) =
                        storage::apply(storage, catalog, reads, Action::Half(other, Some(owner)))
                            .await
                    {
                        let _ = storage::apply(storage, catalog, reads, Action::Theme(theme)).await;
                        return Err(error);
                    }
                    return Ok(());
                }
            }
            storage::apply(storage, catalog, reads, Action::Half(appearance, id)).await
        }
    }
}
fn publish_library(service: &mut Themes, library: &Library) {
    let custom = match library {
        Library::Ready { themes, .. } => themes.clone(),
        Library::Unavailable { .. } => vec![],
    };
    let current = service.catalog.peek().clone();
    if current.custom != custom {
        let mut catalog = (*current).clone();
        catalog.custom = custom;
        service.catalog.set(Rc::new(catalog));
    }
    if service.library.peek().as_ref() != Some(library) {
        service.library.set(Some(library.clone()));
    }
}
fn palette_command(catalog: &Catalog, snapshot: &Snapshot) -> Value {
    let id = catalog.half(
        &snapshot.theme,
        snapshot.theme_halves.as_ref(),
        snapshot.resolved_theme,
    );
    let definition = catalog.definition(id);
    let colors = definition.and_then(|theme| theme.colors(snapshot.resolved_theme));
    let variables: BTreeMap<_, _> = catalog
        .data
        .variables
        .iter()
        .map(|(role, variable)| {
            (
                variable.clone(),
                colors.and_then(|colors| colors.get(role)).cloned(),
            )
        })
        .collect();
    json!({"type":"apply","dark":snapshot.resolved_theme==Appearance::Dark,"paletteId":definition.map(|theme|&theme.id),"variables":variables,"chrome":colors.and_then(|colors|colors.get("chrome"))})
}
fn sync_native(catalog: &Catalog, snapshot: &Snapshot) {
    #[cfg(feature = "desktop")]
    {
        use dioxus::desktop::tao::window::Theme;
        let mode = catalog.desktop_mode(
            &snapshot.theme,
            Some(snapshot.appearance_mode == Mode::System),
            Some(snapshot.appearance_mode),
            snapshot.theme_halves.as_ref(),
        );
        if let Some(desktop) = try_consume_context::<dioxus::desktop::DesktopContext>() {
            desktop.window.set_theme(match mode {
                Mode::Light => Some(Theme::Light),
                Mode::Dark => Some(Theme::Dark),
                Mode::System => None,
            });
        }
    }
    #[cfg(not(feature = "desktop"))]
    let _ = (catalog, snapshot);
}
#[component]
pub fn ThemeControls() -> Element {
    let themes = use_context::<Themes>();
    let snapshot = themes.snapshot.read().clone();
    let disabled = !*themes.ready.read();
    let catalog = themes.catalog.read().clone();
    let mode_service = themes.clone();
    let theme_service = themes.clone();
    let mixed = snapshot.theme_halves.is_some();
    let selected = if mixed {
        "__mixed".to_owned()
    } else {
        snapshot.theme.clone()
    };
    let half_label = |mode| {
        catalog
            .definition(catalog.half(&snapshot.theme, snapshot.theme_halves.as_ref(), mode))
            .map(|t| t.label.clone())
            .unwrap_or_else(|| "T3 Code (default)".into())
    };
    let mixed_label = format!(
        "Light: {} · Dark: {}",
        half_label(Appearance::Light),
        half_label(Appearance::Dark)
    );
    let definitions: Vec<_> = catalog
        .data
        .builtin
        .iter()
        .chain(&catalog.custom)
        .chain(
            catalog
                .environment
                .iter()
                .filter(|t| !catalog.custom.iter().any(|c| c.id == t.id)),
        )
        .cloned()
        .collect();
    rsx! {
        if let Some(error)=themes.error.read().clone(){p {class:"error-banner",role:"alert","{error}"}}
        label {"Appearance mode"}
        select {"aria-label":"Appearance mode",disabled,value:snapshot.appearance_mode.key(),onchange:move|event|{if let Some(mode)=Mode::parse(&event.value()){mode_service.change(Action::Mode(mode));}},
            for (mode,label) in [(Mode::System,"System"),(Mode::Light,"Light"),(Mode::Dark,"Dark")] {option {value:mode.key(),selected:mode==snapshot.appearance_mode,"{label}"}}
        }
        label {"Theme"}
        select {"aria-label":"Theme",disabled,value:selected.clone(),onchange:move|event|theme_service.change(Action::Theme(event.value())),
            if mixed {option {value:"__mixed",selected:true,disabled:true,"{mixed_label}"}}
            option {value:"system",selected:selected=="system","T3 Code (default)"}
            if snapshot.theme=="light"||snapshot.theme=="dark"||snapshot.theme=="t3-chat-dark" {option {value:snapshot.theme.clone(),selected:!mixed,"Legacy selection"}}
            for theme in &definitions{option {value:theme.id.clone(),selected:theme.id==selected,"{theme.label}"}}

        }
        for mode in [Appearance::Light,Appearance::Dark] {
            {let owner=catalog.definition(catalog.half(&snapshot.theme,snapshot.theme_halves.as_ref(),mode)).map(|t|t.id.clone()).unwrap_or_default();let options=definitions.clone();let half_service=themes.clone();rsx!{
                label {"{mode.key()} theme"}
                select {"aria-label":format!("{} theme",mode.key()),disabled,value:owner.clone(),onchange:move|event|{let value=event.value();drop(half_service.choice(Choice::AssignHalf(mode,(!value.is_empty()).then_some(value))));},
                    option {value:"",selected:owner.is_empty(),"T3 Code (default)"}
                    for theme in &options{option {value:theme.id.clone(),selected:theme.id==owner,disabled:theme.colors(mode).is_none(),"{theme.label}"}}
                }
            }}
        }
        crate::theme_library::ThemeLibrary {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        task::{Context, Poll, Waker},
    };
    #[derive(Default)]
    struct Browser {
        saved: BTreeMap<String, String>,
        events: VecDeque<Value>,
        waker: Option<Waker>,
        held: Option<Value>,
        hold_mode: bool,
        fail_theme: bool,
        fail_theme_write: bool,
        fail_library_at: Vec<usize>,
        hold_library: bool,
        fail_library: bool,
        fail_library_read: bool,
        library_reads: usize,
        library_writes: usize,
        theme_reads: usize,
        applied: Vec<Value>,
        disposed: bool,
    }
    impl Browser {
        fn event(&mut self, event: Value) {
            self.events.push_back(event);
            if let Some(waker) = self.waker.take() {
                waker.wake();
            }
        }
        fn command(&mut self, command: Value) {
            if command["type"] == "set" && command["key"] == themes::MODE_KEY && self.hold_mode {
                self.held = Some(command);
                return;
            }
            if command["type"] == "set" && command["key"] == themes::CUSTOM_KEY && self.hold_library
            {
                self.held = Some(command);
                return;
            }
            let key = command["key"].as_str().unwrap_or("");
            let mut value = Value::Null;
            let mut error = None;
            match command["type"].as_str() {
                Some("get") => {
                    if key == themes::THEME_KEY {
                        self.theme_reads += 1;
                        if self.fail_theme {
                            error = Some("injected theme read failure");
                        }
                    }
                    if key == themes::CUSTOM_KEY {
                        self.library_reads += 1;
                        if self.fail_library_read {
                            error = Some("injected library read failure");
                        }
                    }
                    value = self
                        .saved
                        .get(key)
                        .map(|value| json!(value))
                        .unwrap_or(Value::Null);
                }
                Some("set") => {
                    if key == themes::THEME_KEY && self.fail_theme_write {
                        self.event(json!({"type":"response","id":command["id"],"error":"injected theme write failure"}));
                        return;
                    }
                    if key == themes::CUSTOM_KEY {
                        self.library_writes += 1;
                        if self.fail_library || self.fail_library_at.contains(&self.library_writes)
                        {
                            self.event(json!({"type":"response","id":command["id"],"error":"injected library write failure"}));
                            return;
                        }
                    }
                    self.saved
                        .insert(key.into(), command["value"].as_str().unwrap().into());
                }
                Some("remove") => {
                    self.saved.remove(key);
                }
                Some("apply") => {
                    self.applied.push(command.clone());
                    value = json!({"surface":"rgb(250, 250, 250)","body":"rgb(255, 255, 255)"});
                }
                Some("chrome") => {}
                Some("dispose") => {
                    self.disposed = true;
                    return;
                }
                _ => panic!("unsupported transport command {command}"),
            }
            self.event(if let Some(error) = error {
                json!({"type":"response","id":command["id"],"error":error})
            } else {
                json!({"type":"response","id":command["id"],"value":value})
            });
        }
    }
    struct Bridge(Rc<RefCell<Browser>>);
    impl document::Evaluator for Bridge {
        fn send(&self, data: Value) -> Result<(), document::EvalError> {
            self.0.borrow_mut().command(data);
            Ok(())
        }
        fn poll_recv(
            &mut self,
            context: &mut Context<'_>,
        ) -> Poll<Result<Value, document::EvalError>> {
            let mut browser = self.0.borrow_mut();
            if let Some(event) = browser.events.pop_front() {
                Poll::Ready(Ok(event))
            } else {
                browser.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }
        fn poll_join(&mut self, _: &mut Context<'_>) -> Poll<Result<Value, document::EvalError>> {
            Poll::Pending
        }
    }
    struct Document {
        owner: dioxus::signals::Owner,
        browser: Rc<RefCell<Browser>>,
    }
    impl document::Document for Document {
        fn eval(&self, script: String) -> document::Eval {
            if script.contains("Browser API transport only") {
                self.browser
                    .borrow_mut()
                    .event(json!({"type":"ready","dark":true}));
            }
            if script.contains("slot?.dispose?.()") {
                self.browser.borrow_mut().disposed = true;
            }
            document::Eval::new(
                self.owner
                    .insert(Box::new(Bridge(self.browser.clone())) as Box<dyn document::Evaluator>),
            )
        }
    }
    #[derive(Clone)]
    struct Props {
        service: Rc<RefCell<Option<Themes>>>,
        child: Rc<RefCell<Option<Signal<bool>>>>,
    }
    fn root(props: Props) -> Element {
        let state = use_store(UiModel::default);
        use_themes(state);
        *props.service.borrow_mut() = Some(use_context::<Themes>());
        let show = use_signal(|| true);
        *props.child.borrow_mut() = Some(show);
        rsx! {if *show.read(){ThemeControls{}}crate::theme_library::ThemeEditorHost{}}
    }
    fn mounted(browser: Rc<RefCell<Browser>>) -> (VirtualDom, Props) {
        let props = Props {
            service: Default::default(),
            child: Default::default(),
        };
        let mut dom = VirtualDom::new_with_props(root, props.clone());
        dom.provide_root_context(Rc::new(Document {
            owner: Default::default(),
            browser,
        }) as Rc<dyn document::Document>);
        dom.rebuild_in_place();
        (dom, props)
    }
    async fn pump(dom: &mut VirtualDom, done: impl Fn() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
                if done() {
                    break;
                }
                dom.wait_for_work().await;
            }
        })
        .await
        .expect("controller should settle");
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mounted_theme_choice_survives_navigation_and_flush_waits_for_storage_receipt() {
        let browser = Rc::new(RefCell::new(Browser::default()));
        browser
            .borrow_mut()
            .saved
            .insert(themes::THEME_KEY.into(), "t3-chat-dark".into());
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || service.snapshot.peek().theme == "t3-chat-dark").await;
        assert_eq!(service.snapshot.peek().appearance_mode, Mode::Dark);
        assert!(!service.snapshot.peek().system_dark);
        browser.borrow_mut().hold_mode = true;
        service.change(Action::Mode(Mode::System));
        props.child.borrow().unwrap().set(false);
        pump(&mut dom, || browser.borrow().held.is_some()).await;
        let done = Rc::new(Cell::new(false));
        let receipt = done.clone();
        let flush = service.clone();
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                flush.flush().await.unwrap();
                receipt.set(true);
            });
        });
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(!done.get());
        assert!(service.needs_flush());
        let held = {
            let mut browser = browser.borrow_mut();
            browser.hold_mode = false;
            browser.held.take().unwrap()
        };
        browser.borrow_mut().command(held);
        pump(&mut dom, || done.get()).await;
        assert_eq!(browser.borrow().saved[themes::MODE_KEY], "system");
        assert_eq!(service.snapshot.peek().resolved_theme, Appearance::Dark);
        browser
            .borrow_mut()
            .event(json!({"type":"media","dark":false}));
        pump(&mut dom, || {
            service.snapshot.peek().resolved_theme == Appearance::Light
        })
        .await;
        assert_eq!(service.snapshot.peek().appearance_mode, Mode::System);
        assert!(!service.needs_flush());
        drop(dom);
        assert!(browser.borrow().disposed);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mounted_storage_failure_stays_cached_until_theme_event_and_recovers_without_rewriting_source()
     {
        let browser = Rc::new(RefCell::new(Browser {
            fail_theme: true,
            ..Default::default()
        }));
        browser
            .borrow_mut()
            .saved
            .insert(themes::THEME_KEY.into(), "ocean".into());
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || service.error.peek().is_some()).await;
        assert_eq!(browser.borrow().theme_reads, 1);
        browser
            .borrow_mut()
            .event(json!({"type":"media","dark":false}));
        pump(&mut dom, || {
            service.snapshot.peek().resolved_theme == Appearance::Light
        })
        .await;
        assert_eq!(browser.borrow().theme_reads, 1);
        assert_eq!(browser.borrow().saved[themes::THEME_KEY], "ocean");
        browser.borrow_mut().fail_theme = false;
        browser
            .borrow_mut()
            .event(json!({"type":"storage","key":themes::THEME_KEY}));
        pump(&mut dom, || {
            service.snapshot.peek().theme == "ocean" && service.error.peek().is_none()
        })
        .await;
        assert_eq!(browser.borrow().theme_reads, 2);
        assert_eq!(
            browser.borrow().applied.last().unwrap()["paletteId"],
            "ocean"
        );
        drop(dom);
        assert!(browser.borrow().disposed);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn mounted_library_waits_for_receipt_across_navigation_preserves_bytes_and_returns_canonical_theme()
     {
        let raw = r#"[ {"id":"future","opaque":{"untouched":[null,1]},"colors":{"unknown":"var(--later)"}} ]"#;
        let browser = Rc::new(RefCell::new(Browser::default()));
        browser
            .borrow_mut()
            .saved
            .insert(themes::CUSTOM_KEY.into(), raw.into());
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || service.library.peek().is_some()).await;
        assert_eq!(browser.borrow().saved[themes::CUSTOM_KEY], raw);
        let mut theme=themes::library::import(&service.catalog.peek(),&json!({"version":1,"name":"Receipt theme","appearance":"light","colors":{"canvas":"red"}})).unwrap();
        theme.colors.insert("canvas".into(), "blue".into());
        browser.borrow_mut().hold_library = true;
        let save = service.library_change(LibraryAction::Install(theme.clone()));
        assert!(service.needs_flush());
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                *output.borrow_mut() = Some(save.await);
            });
        });
        props.child.borrow().unwrap().set(false);
        pump(&mut dom, || browser.borrow().held.is_some()).await;
        assert_eq!(browser.borrow().saved[themes::CUSTOM_KEY], raw);
        assert!(service.catalog.peek().custom.is_empty());
        let flushed = Rc::new(RefCell::new(None));
        let receipt = flushed.clone();
        let close = service.clone();
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                *receipt.borrow_mut() = Some(close.flush().await);
            });
        });
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(flushed.borrow().is_none());
        let held = {
            let mut browser = browser.borrow_mut();
            browser.hold_library = false;
            browser.fail_library = true;
            browser.held.take().unwrap()
        };
        browser.borrow_mut().command(held);
        pump(&mut dom, || {
            result.borrow().is_some() && flushed.borrow().is_some()
        })
        .await;
        assert!(result.borrow().as_ref().unwrap().is_err());
        assert!(flushed.borrow().as_ref().unwrap().is_err());
        assert_eq!(browser.borrow().saved[themes::CUSTOM_KEY], raw);
        assert!(service.catalog.peek().custom.is_empty());
        browser.borrow_mut().fail_library = false;
        let save = service.library_change(LibraryAction::Install(theme));
        let output = result.clone();
        *result.borrow_mut() = None;
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                *output.borrow_mut() = Some(save.await);
            });
        });
        pump(&mut dom, || {
            result.borrow().is_some() && service.error.peek().is_none()
        })
        .await;
        let returned = result.borrow().as_ref().unwrap().as_ref().unwrap().clone();
        assert_eq!(
            returned["colors"]["canvas"],
            themes::color::canonical("blue").unwrap()
        );
        let saved: Value =
            serde_json::from_str(&browser.borrow().saved[themes::CUSTOM_KEY]).unwrap();
        assert_eq!(saved[0], serde_json::from_str::<Value>(raw).unwrap()[0]);
        assert_eq!(saved[1], returned);
        assert_eq!(service.catalog.peek().custom.len(), 1);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn malformed_library_blocks_mutations_and_external_recovery_preserves_preview() {
        let browser = Rc::new(RefCell::new(Browser::default()));
        browser
            .borrow_mut()
            .saved
            .insert(themes::CUSTOM_KEY.into(), "{broken".into());
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || {
            matches!(&*service.library.peek(), Some(Library::Unavailable { .. }))
        })
        .await;
        let theme = themes::library::import(
            &service.catalog.peek(),
            &json!({"version":1,"name":"New","appearance":"dark","colors":{"canvas":"black"}}),
        )
        .unwrap();
        let save = service.library_change(LibraryAction::Install(theme.clone()));
        let result = Rc::new(Cell::new(false));
        let done = result.clone();
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                assert!(save.await.is_err());
                done.set(true);
            });
        });
        pump(&mut dom, || result.get()).await;
        assert_eq!(browser.borrow().saved[themes::CUSTOM_KEY], "{broken");
        assert_eq!(browser.borrow().library_writes, 0);
        service.preview(Some((theme.colors.clone(), Appearance::Dark)));
        pump(&mut dom, || {
            browser
                .borrow()
                .applied
                .last()
                .is_some_and(|v| v["paletteId"] == "__preview")
        })
        .await;
        browser
            .borrow_mut()
            .saved
            .insert(themes::CUSTOM_KEY.into(), json!([theme]).to_string());
        browser
            .borrow_mut()
            .event(json!({"type":"storage","key":themes::CUSTOM_KEY}));
        pump(&mut dom, || service.catalog.peek().custom.len() == 1).await;
        assert_eq!(
            browser.borrow().applied.last().unwrap()["paletteId"],
            "__preview"
        );
        assert_eq!(browser.borrow().library_writes, 0);
        service.preview(None);
        pump(&mut dom, || {
            browser
                .borrow()
                .applied
                .last()
                .is_some_and(|v| v["paletteId"] != "__preview")
        })
        .await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn actual_file_selection_guards_size_and_pairs_batch_without_activation() {
        use crate::runtime::transport_tests::{click_control, control, wait_for_rendered_text};
        fn files(dom: &mut VirtualDom, entries: Vec<Value>) {
            dioxus_html::set_event_converter(Box::new(dioxus_html::SerializedHtmlEventConverter));
            let data: dioxus_html::SerializedFormData =
                serde_json::from_value(json!({"value":"","values":entries,"valid":true})).unwrap();
            let element = control(dom, "Choose theme files").unwrap().0;
            dom.runtime().handle_event(
                "change",
                dioxus::dioxus_core::Event::new(
                    Rc::new(dioxus_html::PlatformEventData::new(Box::new(data)))
                        as Rc<dyn std::any::Any>,
                    true,
                ),
                element,
            );
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        let browser = Rc::new(RefCell::new(Browser::default()));
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || *service.ready.peek()).await;
        let initial = service.snapshot.peek().clone();
        click_control(&mut dom, "Add theme");
        files(
            &mut dom,
            vec![
                json!({"key":"files","text":null,"file":{"path":"/not-read/huge.json","size":262145,"last_modified":0,"content_type":"application/json","contents":null}}),
            ],
        );
        // File metadata rejects this before the nonexistent native path is read.
        wait_for_rendered_text(&mut dom, "this one was not read (limit 256 KB)").await;
        assert!(service.catalog.peek().custom.is_empty());
        assert_eq!(browser.borrow().library_writes, 0);
        let entries=[("Receipt Light","light","#ffffff"),("Receipt Dark","dark","#112233")].into_iter().map(|(name,mode,canvas)|{
            let text=json!({"name":name,"type":mode,"colors":{"editor.background":canvas,"editor.foreground":"#888888","button.background":"#2380ee"}}).to_string();
            json!({"key":"files","text":null,"file":{"path":format!("{name}.json"),"size":text.len(),"last_modified":0,"content_type":"application/json","contents":text.as_bytes()}})
        }).collect();
        files(&mut dom, entries);
        pump(&mut dom, || {
            service.catalog.peek().custom.len() == 1 && !service.needs_flush()
        })
        .await;
        assert_eq!(service.catalog.peek().custom[0].label, "Receipt");
        assert_eq!(service.catalog.peek().custom[0].modes().len(), 2);
        assert_eq!(service.snapshot.peek().theme, initial.theme);
        assert_eq!(service.snapshot.peek().theme_halves, initial.theme_halves);
        assert!(control(&dom, "Theme JSON").is_none());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn accepted_import_batch_survives_unmount_and_reports_partial_write_before_close_receipt()
    {
        let browser = Rc::new(RefCell::new(Browser::default()));
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || *service.ready.peek()).await;
        let make = |name| {
            themes::library::import(
                &service.catalog.peek(),
                &json!({"version":1,"name":name,"appearance":"dark","colors":{"canvas":"#123456"}}),
            )
            .unwrap()
        };
        browser.borrow_mut().hold_library = true;
        browser.borrow_mut().fail_library_at = vec![2];
        let save = service.import_many(
            vec![make("First receipt"), make("Second receipt")],
            ImportMode::New,
            None,
        );
        assert!(service.needs_flush());
        props.child.borrow().unwrap().set(false);
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let closing = Rc::new(RefCell::new(None));
        let receipt = closing.clone();
        let closer = service.clone();
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                *output.borrow_mut() = Some(save.await);
            });
            spawn(async move {
                *receipt.borrow_mut() = Some(closer.flush().await);
            });
        });
        pump(&mut dom, || browser.borrow().held.is_some()).await;
        assert!(closing.borrow().is_none());
        let held = {
            let mut browser = browser.borrow_mut();
            browser.hold_library = false;
            browser.held.take().unwrap()
        };
        browser.borrow_mut().command(held);
        pump(&mut dom, || {
            result.borrow().is_some() && closing.borrow().is_some()
        })
        .await;
        let result = result.borrow_mut().take().unwrap().unwrap();
        assert_eq!(result["saved"].as_array().unwrap().len(), 1);
        assert_eq!(result["failures"].as_array().unwrap().len(), 1);
        assert_eq!(service.catalog.peek().custom[0].id, "first-receipt");
        assert_eq!(
            serde_json::from_str::<Value>(&browser.borrow().saved[themes::CUSTOM_KEY]).unwrap()[0]
                ["id"],
            "first-receipt"
        );
        assert!(service.error.peek().is_some());
        assert!(closing.borrow().as_ref().unwrap().is_err());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn actual_vscode_import_conflict_retry_and_copy_preserve_library_and_selection() {
        use crate::runtime::transport_tests::{click_control, control, input_control};
        let opaque = json!({"id":"future","opaque":{"keep":true}});
        let browser = Rc::new(RefCell::new(Browser::default()));
        browser
            .borrow_mut()
            .saved
            .insert(themes::CUSTOM_KEY.into(), json!([opaque]).to_string());
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || *service.ready.peek()).await;
        let source = |background: &str| {
            json!({"name":"VS Code receipt","type":"dark","colors":{"editor.background":background,"editor.foreground":"#ddd","button.background":"#1984f0"}}).to_string()
        };
        click_control(&mut dom, "Add theme");
        input_control(&mut dom, "Theme JSON", &source("#112233"));
        click_control(&mut dom, "Import theme");
        pump(&mut dom, || {
            service.catalog.peek().custom.len() == 1 && !service.needs_flush()
        })
        .await;
        let original = service.catalog.peek().custom[0].clone();
        assert_eq!(
            original,
            themes::import::parse(&service.catalog.peek(), &source("#112233")).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&browser.borrow().saved[themes::CUSTOM_KEY]).unwrap()[0],
            opaque
        );
        let mut collection = original.clone();
        collection.collection = Some(json!({"id":"open-vsx:fixture","label":"Fixture"}));
        settle(
            &mut dom,
            service.library_change(LibraryAction::Update(collection)),
        )
        .await
        .unwrap();
        let selection = service.snapshot.peek().clone();
        let before = browser.borrow().saved[themes::CUSTOM_KEY].clone();
        click_control(&mut dom, "Add theme");
        input_control(&mut dom, "Theme JSON", &source("#223344"));
        click_control(&mut dom, "Import theme");
        assert!(control(&dom, "Update theme").is_some());
        assert_eq!(browser.borrow().saved[themes::CUSTOM_KEY], before);
        browser.borrow_mut().fail_library = true;
        click_control(&mut dom, "Update theme");
        pump(&mut dom, || {
            !service.needs_flush() && service.error.peek().is_some()
        })
        .await;
        assert_eq!(browser.borrow().saved[themes::CUSTOM_KEY], before);
        assert!(control(&dom, "Import theme").is_some());
        browser.borrow_mut().fail_library = false;
        click_control(&mut dom, "Import theme");
        click_control(&mut dom, "Update theme");
        pump(&mut dom, || {
            !service.needs_flush() && service.catalog.peek().custom[0].colors != original.colors
        })
        .await;
        assert!(control(&dom, "Theme JSON").is_none());
        assert_eq!(
            service.catalog.peek().custom[0].collection,
            Some(json!({"id":"open-vsx:fixture","label":"Fixture"}))
        );
        assert_ne!(service.catalog.peek().custom[0].colors, original.colors);
        assert_eq!(service.snapshot.peek().theme, selection.theme);
        assert_eq!(service.snapshot.peek().theme_halves, selection.theme_halves);
        click_control(&mut dom, "Add theme");
        input_control(&mut dom, "Theme JSON", &source("#334455"));
        click_control(&mut dom, "Import theme");
        click_control(&mut dom, "Add a copy");
        pump(&mut dom, || {
            service.catalog.peek().custom.len() == 2 && !service.needs_flush()
        })
        .await;
        assert_eq!(
            service.catalog.peek().custom[1].label,
            "VS Code receipt (1)"
        );
        assert_eq!(service.catalog.peek().custom[1].collection, None);
        assert_eq!(service.snapshot.peek().theme, selection.theme);
        assert_eq!(service.snapshot.peek().theme_halves, selection.theme_halves);
        let saved = browser.borrow().saved[themes::CUSTOM_KEY].clone();
        drop(dom);
        let (mut reloaded, props) = mounted(browser.clone());
        let restored = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut reloaded, || restored.catalog.peek().custom.len() == 2).await;
        assert_eq!(browser.borrow().saved[themes::CUSTOM_KEY], saved);
    }
    #[tokio::test(flavor = "current_thread")]
    async fn real_editor_events_preview_failed_save_retry_and_reload_lossless_library() {
        use crate::runtime::transport_tests::{click_control, control, input_control};
        let opaque = json!({"id":"future","future":{"preserve":[1,null,"yes"]}});
        let browser = Rc::new(RefCell::new(Browser::default()));
        browser
            .borrow_mut()
            .saved
            .insert(themes::CUSTOM_KEY.into(), json!([opaque]).to_string());
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || service.library.peek().is_some()).await;
        click_control(&mut dom, "Create theme");
        input_control(&mut dom, "Theme name", "Behavior");
        input_control(&mut dom, "Theme color canvas", "#123456");
        input_control(&mut dom, "Theme color accent", "#fa00fa");
        props.child.borrow().unwrap().set(false);
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(control(&dom, "Theme name").is_some());
        input_control(&mut dom, "Theme color canvas", "#123456");
        pump(&mut dom, || {
            browser
                .borrow()
                .applied
                .last()
                .is_some_and(|v| v["paletteId"] == "__preview")
        })
        .await;
        assert!(
            browser.borrow().applied.last().unwrap()["variables"]["--app-theme-canvas"]
                .as_str()
                .unwrap()
                .starts_with("oklch(")
        );
        browser.borrow_mut().fail_library = true;
        click_control(&mut dom, "Save theme");
        pump(&mut dom, || {
            service.error.peek().is_some() && !service.needs_flush()
        })
        .await;
        assert!(service.catalog.peek().custom.is_empty());
        assert!(control(&dom, "Theme name").is_some());
        assert_eq!(
            serde_json::from_str::<Value>(&browser.borrow().saved[themes::CUSTOM_KEY]).unwrap(),
            json!([opaque])
        );
        browser.borrow_mut().fail_library = false;
        click_control(&mut dom, "Save theme");
        pump(&mut dom, || {
            service.catalog.peek().custom.len() == 1 && !service.needs_flush()
        })
        .await;
        assert!(control(&dom, "Theme name").is_none());
        assert_eq!(service.catalog.peek().custom[0].label, "Behavior");
        assert_eq!(service.catalog.peek().custom[0].managed, Some(true));
        assert_eq!(
            serde_json::from_str::<Value>(&browser.borrow().saved[themes::CUSTOM_KEY]).unwrap()[0],
            opaque
        );
        let saved_colors = service.catalog.peek().custom[0].colors.clone();
        drop(dom);
        let (mut reloaded, props) = mounted(browser.clone());
        let restored = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut reloaded, || {
            restored.catalog.peek().custom.len() == 1
                && browser
                    .borrow()
                    .applied
                    .last()
                    .is_some_and(|v| v["paletteId"] == "behavior")
        })
        .await;
        assert_eq!(restored.catalog.peek().custom[0].colors, saved_colors);
    }
    async fn settle<T: 'static>(
        dom: &mut VirtualDom,
        future: impl std::future::Future<Output = Result<T, String>> + 'static,
    ) -> Result<T, String> {
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                *output.borrow_mut() = Some(future.await);
            });
        });
        pump(dom, || result.borrow().is_some()).await;
        let result = result.borrow_mut().take().unwrap();
        result
    }
    fn draft(
        service: &Themes,
        name: &str,
        appearance: Appearance,
        editing_id: Option<&str>,
    ) -> themes::editor::Draft {
        let catalog = service.catalog.peek();
        themes::editor::Draft {
            editing_id: editing_id.map(str::to_owned),
            name: name.into(),
            appearance,
            advanced: false,
            colors: [
                (
                    Appearance::Light,
                    catalog.data.standard[&Appearance::Light].clone(),
                ),
                (
                    Appearance::Dark,
                    catalog.data.standard[&Appearance::Dark].clone(),
                ),
            ]
            .into_iter()
            .collect(),
        }
    }
    #[tokio::test(flavor = "current_thread")]
    async fn create_edit_merge_and_mixed_controls_settle_against_persisted_selection() {
        use crate::runtime::transport_tests::{change_control, control};
        let browser = Rc::new(RefCell::new(Browser::default()));
        browser
            .borrow_mut()
            .saved
            .insert(themes::THEME_KEY.into(), "grove".into());
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || *service.ready.peek()).await;
        settle(
            &mut dom,
            service.save_editor(draft(&service, "Pair", Appearance::Dark, None)),
        )
        .await
        .unwrap();
        pump(&mut dom, || service.snapshot.peek().theme == "pair").await;
        assert_eq!(control(&dom, "Theme").unwrap().1.as_deref(), Some("pair"));
        assert_eq!(browser.borrow().saved[themes::THEME_KEY], "pair");
        change_control(&mut dom, "light theme", "ocean");
        pump(&mut dom, || {
            service
                .snapshot
                .peek()
                .theme_halves
                .as_ref()
                .is_some_and(|h| h.light.as_deref() == Some("ocean"))
        })
        .await;
        assert_eq!(
            control(&dom, "Theme").unwrap().1.as_deref(),
            Some("__mixed")
        );
        let raw = browser.borrow().saved[themes::HALVES_KEY].clone();
        settle(
            &mut dom,
            service.save_editor(draft(&service, "Pair", Appearance::Dark, Some("pair"))),
        )
        .await
        .unwrap();
        assert_eq!(browser.borrow().saved[themes::HALVES_KEY], raw);
        settle(
            &mut dom,
            service.save_editor(draft(&service, "Pair", Appearance::Light, None)),
        )
        .await
        .unwrap();
        pump(&mut dom, || service.snapshot.peek().theme_halves.is_none()).await;
        assert_eq!(service.catalog.peek().custom[0].modes().len(), 2);
        assert_eq!(control(&dom, "Theme").unwrap().1.as_deref(), Some("pair"));
    }
    #[tokio::test(flavor = "current_thread")]
    async fn activation_failure_and_failed_rollback_publish_actual_library_then_allow_retry() {
        let browser = Rc::new(RefCell::new(Browser::default()));
        browser
            .borrow_mut()
            .saved
            .insert(themes::THEME_KEY.into(), "grove".into());
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || *service.ready.peek()).await;
        browser.borrow_mut().fail_theme_write = true;
        browser.borrow_mut().fail_library_at = vec![2];
        let result = settle(
            &mut dom,
            service.save_editor(draft(&service, "Retained", Appearance::Dark, None)),
        )
        .await;
        assert!(result.is_err());
        pump(&mut dom, || service.error.peek().is_some()).await;
        assert_eq!(service.catalog.peek().custom[0].id, "retained");
        assert_eq!(browser.borrow().saved[themes::THEME_KEY], "grove");
        assert_eq!(
            serde_json::from_str::<Value>(&browser.borrow().saved[themes::CUSTOM_KEY]).unwrap()[0]
                ["id"],
            "retained"
        );
        browser.borrow_mut().fail_theme_write = false;
        browser.borrow_mut().fail_library_at.clear();
        settle(
            &mut dom,
            service.choice(Choice::Set(Action::Theme("retained".into()))),
        )
        .await
        .unwrap();
        pump(&mut dom, || {
            browser
                .borrow()
                .applied
                .last()
                .is_some_and(|a| a["paletteId"] == "retained")
                && service.error.peek().is_none()
        })
        .await;
        assert_eq!(service.snapshot.peek().theme, "retained");
    }
    #[tokio::test(flavor = "current_thread")]
    async fn remove_and_default_half_preserve_unpublished_other_owner_after_partial_failure() {
        let browser = Rc::new(RefCell::new(Browser::default()));
        let (mut dom, props) = mounted(browser.clone());
        let service = props.service.borrow().as_ref().unwrap().clone();
        pump(&mut dom, || *service.ready.peek()).await;
        settle(
            &mut dom,
            service.save_editor(draft(&service, "Remove", Appearance::Dark, None)),
        )
        .await
        .unwrap();
        browser.borrow_mut().saved.insert(
            themes::HALVES_KEY.into(),
            json!({"light":"published-later"}).to_string(),
        );
        settle(
            &mut dom,
            service.choice(Choice::AssignHalf(Appearance::Dark, None)),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&browser.borrow().saved[themes::HALVES_KEY]).unwrap(),
            json!({"light":"published-later"})
        );
        settle(
            &mut dom,
            service.choice(Choice::Set(Action::Theme("remove".into()))),
        )
        .await
        .unwrap();
        browser.borrow_mut().saved.insert(
            themes::HALVES_KEY.into(),
            json!({"light":"published-later","dark":"remove"}).to_string(),
        );
        browser.borrow_mut().fail_library = true;
        assert!(
            settle(&mut dom, service.remove_themes(vec!["remove".into()]))
                .await
                .is_err()
        );
        pump(&mut dom, || service.snapshot.peek().theme == "system").await;
        assert_eq!(
            serde_json::from_str::<Value>(&browser.borrow().saved[themes::HALVES_KEY]).unwrap(),
            json!({"light":"published-later"})
        );
        assert_eq!(service.catalog.peek().custom.len(), 1);
        browser.borrow_mut().fail_library = false;
        settle(&mut dom, service.remove_themes(vec!["remove".into()]))
            .await
            .unwrap();
        pump(&mut dom, || service.catalog.peek().custom.is_empty()).await;
    }
}
