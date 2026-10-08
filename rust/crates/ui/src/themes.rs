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
    self, Appearance, Catalog, Mode,
    storage::{self, Action, ReadState, Snapshot, Storage},
};
type Replies = Rc<RefCell<BTreeMap<u64, futures_channel::oneshot::Sender<Result<Value, String>>>>>;
#[allow(dead_code)]
enum Command {
    Refresh(Option<Option<String>>),
    Change(Action),
    Flush {
        pending: bool,
        reply: futures_channel::oneshot::Sender<Result<(), String>>,
    },
}
#[derive(Clone)]
pub struct Themes {
    pub snapshot: Signal<Snapshot>,
    pub catalog: Rc<Catalog>,
    pub error: Signal<Option<String>>,
    pub ready: Signal<bool>,
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
    pub fn change(&self, action: Action) {
        self.pending.set(self.pending.get() + 1);
        if self
            .commands
            .unbounded_send(Command::Change(action))
            .is_err()
        {
            self.pending.set(self.pending.get() - 1);
        }
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
    let catalog = use_hook(|| Rc::new(Catalog::default()));
    let error = use_signal(|| None);
    let ready = use_signal(|| false);
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
        error,
        ready,
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
    let actor_service = service.clone();
    use_future(move || {
        let mut rx = receiver
            .borrow_mut()
            .take()
            .expect("theme actor mounts once");
        let mut service = actor_service.clone();
        async move {
            let catalog = service.catalog.clone();
            let mut reads = ReadState::default();
            let mut storage = BrowserStorage(service.clone());
            let mut write_failure = None;
            let mut last_applied = None;
            while let Some(command) = rx.next().await {
                let force = matches!(&command, Command::Refresh(Some(None)))
                    || matches!(&command,Command::Refresh(Some(Some(key))) if key==themes::CUSTOM_KEY);
                let _change = matches!(command, Command::Change(_))
                    .then(|| ChangeGuard(service.pending.clone()));
                match command {
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
                    Command::Change(action) => {
                        match storage::apply(&mut storage, &catalog, &mut reads, action).await {
                            Ok(()) => write_failure = None,
                            Err(error) => {
                                write_failure =
                                    Some(format!("Theme choice was not saved: {error}"));
                                service.error.set(write_failure.clone());
                                continue;
                            }
                        }
                    }
                }
                let snapshot = reads
                    .snapshot(&mut storage, &catalog, service.system_dark.get())
                    .await;
                if state.peek().dark != (snapshot.resolved_theme == Appearance::Dark) {
                    state
                        .dark()
                        .set(snapshot.resolved_theme == Appearance::Dark);
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
                let apply = palette_command(&catalog, &snapshot);
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
    let catalog = themes.catalog.clone();
    let mode_service = themes.clone();
    let theme_service = themes.clone();
    rsx! {
        if let Some(error)=themes.error.read().clone(){p {class:"error-banner",role:"alert","{error}"}}
        label {"Appearance mode"}
        select {"aria-label":"Appearance mode",disabled,value:snapshot.appearance_mode.key(),onchange:move|event|{if let Some(mode)=Mode::parse(&event.value()){mode_service.change(Action::Mode(mode));}},
            for (mode,label) in [(Mode::System,"System"),(Mode::Light,"Light"),(Mode::Dark,"Dark")] {option {value:mode.key(),selected:mode==snapshot.appearance_mode,"{label}"}}
        }
        label {"Theme"}
        select {"aria-label":"Theme",disabled,value:snapshot.theme.clone(),onchange:move|event|theme_service.change(Action::Theme(event.value())),
            option {value:"system",selected:snapshot.theme=="system","T3 Code (default)"}
            if snapshot.theme=="light"||snapshot.theme=="dark"||snapshot.theme=="t3-chat-dark" {option {value:snapshot.theme.clone(),selected:true,"Legacy selection"}}
            for theme in catalog.data.builtin.iter(){option {value:theme.id.clone(),selected:theme.id==snapshot.theme,"{theme.label}"}}
        }
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
                    value = self
                        .saved
                        .get(key)
                        .map(|value| json!(value))
                        .unwrap_or(Value::Null);
                }
                Some("set") => {
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
        rsx! {if *show.read(){ThemeControls{}}}
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
}
