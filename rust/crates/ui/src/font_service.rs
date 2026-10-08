use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
};
#[derive(Clone, Default)]
pub struct CommitBarrier(Rc<CommitState>);
#[derive(Default)]
struct CommitState {
    pending: Cell<usize>,
    waiters: RefCell<Vec<futures_channel::oneshot::Sender<()>>>,
}
struct CommitGuard(CommitBarrier);
impl Drop for CommitGuard {
    fn drop(&mut self) {
        let state = &self.0.0;
        state.pending.set(state.pending.get() - 1);
        if state.pending.get() == 0 {
            for waiter in state.waiters.borrow_mut().drain(..) {
                let _ = waiter.send(());
            }
        }
    }
}
impl CommitBarrier {
    fn begin(&self) -> CommitGuard {
        self.0.pending.set(self.0.pending.get() + 1);
        CommitGuard(self.clone())
    }
    #[cfg_attr(not(any(feature = "desktop", test)), allow(dead_code))]
    pub fn pending(&self) -> bool {
        self.0.pending.get() != 0
    }
    #[cfg_attr(not(any(feature = "desktop", test)), allow(dead_code))]
    pub async fn wait(&self) {
        while self.pending() {
            let (tx, rx) = futures_channel::oneshot::channel();
            self.0.waiters.borrow_mut().push(tx);
            let _ = rx.await;
        }
    }
    pub fn track(
        &self,
        task: impl std::future::Future<Output = ()> + 'static,
    ) -> impl std::future::Future<Output = ()> + 'static {
        let guard = self.begin();
        async move {
            let _guard = guard;
            task.await;
        }
    }
}
type Responses =
    Rc<RefCell<BTreeMap<u64, futures_channel::oneshot::Sender<Result<Value, String>>>>>;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Enumeration {
    #[default]
    Unknown,
    Unavailable,
    Granted(Vec<String>),
}
#[derive(Clone)]
pub struct Fonts {
    pub commits: CommitBarrier,
    pub ready: Signal<bool>,
    pub error: Signal<Option<String>>,
    pub defaults: Signal<Value>,
    pub enumeration: Signal<Enumeration>,
    owner: ScopeId,
    runtime: Weak<dioxus::dioxus_core::Runtime>,
    alive: Rc<Cell<bool>>,
    loading: Signal<bool>,
    eval: Signal<Option<document::Eval>>,
    next: Rc<Cell<u64>>,
    responses: Responses,
}
struct Pending {
    id: u64,
    responses: Responses,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.responses.borrow_mut().remove(&self.id);
    }
}
impl Fonts {
    pub async fn request(&self, mut command: Value) -> Result<Value, String> {
        if !self.alive.get() {
            return Err("Font controls were closed.".into());
        }
        if !*self.ready.peek() {
            return Err("Font controls are still loading.".into());
        }
        let id = self.next.get();
        self.next.set(id + 1);
        command["id"] = id.into();
        let (tx, rx) = futures_channel::oneshot::channel();
        self.responses.borrow_mut().insert(id, tx);
        let _pending = Pending {
            id,
            responses: self.responses.clone(),
        };
        let eval = self.eval.peek().ok_or("Font controls are unavailable.")?;
        eval.send(command).map_err(|cause| cause.to_string())?;
        rx.await
            .map_err(|_| "Font request was canceled.".to_owned())?
    }
    pub fn spawn(&self, task: impl std::future::Future<Output = ()> + 'static) {
        if self.alive.get() {
            if let Some(runtime) = self.runtime.upgrade() {
                runtime.spawn(self.owner, task);
            }
        }
    }
    pub fn commit(&self, task: impl std::future::Future<Output = ()> + 'static) {
        self.spawn(self.commits.track(task));
    }
    pub fn discover(&self) {
        if !self.alive.get()
            || !*self.ready.peek()
            || *self.loading.peek()
            || *self.enumeration.peek() != Enumeration::Unknown
        {
            return;
        }
        let mut loading = self.loading;
        loading.set(true);
        let service = self.clone();
        self.spawn(async move {
            let result = service.request(json!({"type":"enumerate"})).await;
            if !service.alive.get() {
                return;
            }
            let mut enumeration = service.enumeration;
            let mut loading = service.loading;
            enumeration.set(match result {
                Ok(value) if value["status"] == "granted" => Enumeration::Granted(
                    serde_json::from_value(value["families"].clone()).unwrap_or_default(),
                ),
                _ => Enumeration::Unavailable,
            });
            loading.set(false);
        });
    }
}
pub fn use_fonts(state: Store<UiModel>) {
    let mut ready = use_signal(|| false);
    let mut error = use_signal(|| None);
    let mut defaults = use_signal(|| json!({}));
    let mut enumeration = use_signal(Enumeration::default);
    let loading = use_signal(|| false);
    let mut eval = use_signal(|| None::<document::Eval>);
    let bridge_id = use_hook(|| format!("appearance-{}", uuid::Uuid::new_v4()));
    let launch_id = bridge_id.clone();
    let next = use_hook(|| Rc::new(Cell::new(1)));
    let responses = use_hook(|| Rc::new(RefCell::new(BTreeMap::new())));
    let owner = use_hook(dioxus::dioxus_core::current_scope_id);
    let runtime = use_hook(|| Rc::downgrade(&dioxus::dioxus_core::Runtime::current()));
    let alive = use_hook(|| Rc::new(Cell::new(true)));
    let commits = use_hook(CommitBarrier::default);
    let service = Fonts {
        commits,
        owner,
        runtime,
        alive,
        ready,
        error,
        defaults,
        enumeration,
        loading,
        eval,
        next,
        responses,
    };
    use_context_provider(|| service.clone());
    let pending = service.responses.clone();
    let granted_service = service.clone();
    use_future(move || {
        let pending = pending.clone();
        let granted_service = granted_service.clone();
        let launch_id = launch_id.clone();
        async move {
            let base = crate::terminal_pane::SURFACE.to_string();
            let args =
                json!({"id":launch_id,"base":base,"wasm":format!("{base}/t3_terminal_bg.wasm")});
            let mut bridge = document::eval(&format!(
                "const args={args};\n{}",
                include_str!("../assets/appearance_abi.js")
            ));
            eval.set(Some(bridge));
            loop {
                let event: Value = match bridge.recv().await {
                    Ok(event) => event,
                    Err(cause) => {
                        error.set(Some(format!("Font controls disconnected: {cause}")));
                        break;
                    }
                };
                match event["type"].as_str() {
                    Some("ready") => {
                        defaults.set(event["defaults"].clone());
                        if event["defaults"]["enumerationSupported"] == false {
                            enumeration.set(Enumeration::Unavailable);
                        }
                        ready.set(true);
                        let lookup = granted_service.clone();
                        granted_service.spawn(async move {
                            if lookup
                                .request(json!({"type":"permission"}))
                                .await
                                .as_ref()
                                .ok()
                                .and_then(Value::as_str)
                                == Some("granted")
                            {
                                lookup.discover();
                            }
                        });
                    }
                    Some("response") => {
                        if let Some(id) = event["id"].as_u64() {
                            let response = pending.borrow_mut().remove(&id);
                            if let Some(response) = response {
                                let result = event
                                    .get("error")
                                    .and_then(Value::as_str)
                                    .map(|error| Err(error.to_owned()))
                                    .unwrap_or_else(|| Ok(event["value"].clone()));
                                let _ = response.send(result);
                            }
                        }
                    }
                    Some("error") => {
                        error.set(Some(
                            event["message"]
                                .as_str()
                                .unwrap_or("Font controls failed.")
                                .into(),
                        ));
                        break;
                    }
                    _ => {}
                }
            }
            ready.set(false);
            eval.set(None);
            pending.borrow_mut().clear();
            let _ = bridge.send(json!({"type":"dispose"}));
        }
    });
    use_effect(move || {
        let preferences = state.client_settings();
        let preferences = preferences.read();
        if *ready.read() {
            if let Some(bridge) = *eval.read() {
                let _ = bridge.send(json!({"type":"apply","settings":&*preferences}));
            }
        }
    });
    let cleanup = service.responses.clone();
    let alive = service.alive.clone();
    use_drop(move || {
        alive.set(false);
        cleanup.borrow_mut().clear();
        if let Some(bridge) = *eval.peek() {
            let _ = bridge.send(json!({"type":"dispose"}));
        }
        let args = json!({"id":bridge_id});
        document::eval(&format!(
            "const slot=window.__t3RustAppearances?.get({args}.id);if(slot)slot.disposed=true;"
        ));
    });
}
pub async fn delay(ms: u64) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(ms as u32).await;
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone)]
    struct Props {
        service: Rc<RefCell<Option<Fonts>>>,
        child: Rc<RefCell<Option<Signal<bool>>>>,
    }
    fn root(props: Props) -> Element {
        let state = use_store(UiModel::default);
        use_fonts(state);
        let fonts = use_context::<Fonts>();
        let show = use_signal(|| true);
        *props.service.borrow_mut() = Some(fonts);
        *props.child.borrow_mut() = Some(show);
        rsx! {if *show.read(){span{"Font input"}}}
    }
    #[tokio::test(flavor = "current_thread")]
    async fn app_owned_font_task_survives_row_navigation_but_is_canceled_with_app() {
        let props = Props {
            service: Default::default(),
            child: Default::default(),
        };
        let mut dom = VirtualDom::new_with_props(root, props.clone());
        dom.rebuild_in_place();
        let fonts = props.service.borrow().as_ref().unwrap().clone();
        let (tx, rx) = futures_channel::oneshot::channel();
        let (done, result) = futures_channel::oneshot::channel();
        let mut signal = fonts.loading;
        dom.in_scope(ScopeId::APP, || {
            fonts.commit(async move {
                let _ = rx.await;
                signal.set(true);
                let _ = done.send(());
            })
        });
        props.child.borrow().unwrap().set(false);
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(fonts.commits.pending());
        tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3),async{futures_util::pin_mut!(result);loop{tokio::select!{value=&mut result=>{value.unwrap();break;},_=dom.wait_for_work()=>dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations)}}}).await.unwrap();
        assert!(*fonts.loading.peek());
        assert!(!fonts.commits.pending());
        let (tx, rx) = futures_channel::oneshot::channel();
        let touched = Rc::new(Cell::new(false));
        let after = touched.clone();
        dom.in_scope(ScopeId::APP, || {
            fonts.commit(async move {
                let _ = rx.await;
                signal.set(false);
                after.set(true);
            })
        });
        assert!(fonts.commits.pending());
        drop(dom);
        assert!(!fonts.alive.get());
        assert!(!fonts.commits.pending());
        assert!(tx.send(()).is_err());
        assert!(!touched.get());
        // Escaped handles cannot touch disposed signals or register more work.
        fonts.discover();
        fonts.commit(async {});
        assert!(!fonts.commits.pending());
    }
}
