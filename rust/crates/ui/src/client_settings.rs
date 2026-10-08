//! Browser-local preferences: serialized storage IO at the persistent App scope.
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::Value;
use std::{cell::RefCell, rc::Rc};
use t3_client::client_preferences::Preferences;
const KEY: &str = "t3code:client-settings:v1";
enum Request {
    Save,
    Flush(futures_channel::oneshot::Sender<Result<(), String>>),
}
#[derive(Clone)]
pub struct Writer {
    pub document: Signal<Preferences>,
    wake: futures_channel::mpsc::UnboundedSender<Request>,
}
pub fn use_writer(state: Store<UiModel>) {
    let mut document = use_signal(Preferences::default);
    let (wake, receiver) = use_hook(|| {
        let (tx, rx) = futures_channel::mpsc::unbounded::<Request>();
        (tx, Rc::new(RefCell::new(Some(rx))))
    });
    let writer = Writer { document, wake };
    use_flush_on_unload(writer.clone());
    use_context_provider(|| writer);
    use_future(move || {
        let mut receiver = receiver
            .borrow_mut()
            .take()
            .expect("preferences writer mounts once");
        async move {
            while let Some(request) = receiver.next().await {
                let mut result = Ok(());
                loop {
                    let Some(receipt) = document.peek().write_receipt() else {
                        if document.peek().needs_flush() {
                            result=Err(document.peek().read_error.clone().unwrap_or_else(||"Client preferences have not been read yet. Changes remain in memory.".into()));
                        }
                        break;
                    };
                    match write(receipt.bytes.clone()).await {
                        Ok(()) => {
                            document.write().acknowledge(&receipt);
                            publish_error(state, &document.peek());
                        }
                        Err(error) => {
                            document.write().failed_write(error);
                            publish_error(state, &document.peek());
                            result = Err(document.peek().write_error.clone().unwrap());
                            break;
                        }
                    }
                }
                if let Request::Flush(reply) = request {
                    let _ = reply.send(result);
                }
            }
        }
    });
}
fn use_flush_on_unload(writer: Writer) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::{JsCast, closure::Closure};
        let mut document = writer.document;
        let callback = use_hook(move || {
            Rc::new(Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let receipt = document.peek().write_receipt();
                if let Some(receipt) = receipt {
                    let saved = web_sys::window()
                        .and_then(|window| window.local_storage().ok().flatten())
                        .is_some_and(|storage| storage.set_item(KEY, &receipt.bytes).is_ok());
                    if saved {
                        document.write().acknowledge(&receipt);
                    }
                }
            }))
        });
        let register = callback.clone();
        use_hook(move || {
            if let Some(window) = web_sys::window() {
                let _ = window.add_event_listener_with_callback(
                    "beforeunload",
                    register.as_ref().as_ref().unchecked_ref(),
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
    let _ = writer;
}

impl Writer {
    pub fn patch(&self, state: Store<UiModel>, patch: Value) {
        let mut document = self.document;
        let result = document.write().patch(patch);
        if let Err(error) = result {
            state
                .client_settings_error()
                .set(Some(format!("Client preference was not changed: {error}")));
            return;
        }
        if document.peek().hydrated() {
            state
                .client_settings()
                .set(document.peek().snapshot().clone());
        }
        publish_error(state, &document.peek());
        let _ = self.wake.unbounded_send(Request::Save);
    }
    #[cfg_attr(not(any(feature = "desktop", test)), allow(dead_code))]
    pub fn needs_flush(&self) -> bool {
        self.document.peek().needs_flush()
    }
    #[cfg_attr(not(any(feature = "desktop", test)), allow(dead_code))]
    pub async fn flush(&self) -> Result<(), String> {
        let (tx, rx) = futures_channel::oneshot::channel();
        self.wake
            .unbounded_send(Request::Flush(tx))
            .map_err(|_| "Client preference writer is unavailable.".to_owned())?;
        rx.await.unwrap_or_else(|_| {
            Err("Client preference writer stopped before saving. Changes remain in memory.".into())
        })
    }
    #[cfg_attr(not(any(feature = "desktop", test)), allow(dead_code))]
    pub async fn flush_after(
        &self,
        commits: &crate::font_service::CommitBarrier,
    ) -> Result<(), String> {
        loop {
            commits.wait().await;
            self.flush().await?;
            if !commits.pending() && !self.needs_flush() {
                return Ok(());
            }
        }
    }
    pub fn retry(&self) {
        let _ = self.wake.unbounded_send(Request::Save);
    }
}
fn publish_error(state: Store<UiModel>, document: &Preferences) {
    state.client_settings_error().set(
        document
            .read_error
            .clone()
            .or_else(|| document.write_error.clone()),
    );
}
pub async fn hydrate(state: Store<UiModel>) {
    let writer = try_consume_context::<Writer>();
    if let Some(writer) = writer {
        let mut document = writer.document;
        if document.peek().hydrated() {
            writer.retry();
            return;
        }
        let generation = document.write().begin_hydration();
        let raw = read_raw().await;
        let result = document.write().hydrate(
            generation,
            raw.as_ref().map(|raw| raw.as_deref()).map_err(Clone::clone),
        );
        if matches!(result, Ok(true)) {
            state
                .client_settings()
                .set(document.peek().snapshot().clone());
        }
        publish_error(state, &document.peek());
        writer.retry();
    } else {
        // Non-app harnesses can hydrate without mounting a persistence writer.
        match read_raw().await.and_then(|raw|raw.map(|raw|serde_json::from_str(&raw).map_err(|_|"Saved client preferences are invalid. Saved preferences have been preserved.".to_owned())).unwrap_or_else(||Ok(Default::default()))){
            Ok(settings)=>{state.client_settings().set(settings);state.client_settings_error().set(None);},
            Err(error)=>state.client_settings_error().set(Some(error)),
        }
    }
}
async fn read_raw() -> Result<Option<String>, String> {
    #[cfg(target_arch = "wasm32")]
    {
        let storage = web_sys::window()
            .and_then(|window| window.local_storage().ok().flatten())
            .ok_or("Client storage is unavailable. Saved preferences have been preserved.")?;
        storage.get_item(KEY).map_err(|_| {
            "Could not read client storage. Saved preferences have been preserved.".into()
        })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let args = serde_json::json!({"key":KEY});
        document::eval(&format!("return window.localStorage.getItem({args}.key);"))
            .join()
            .await
            .map_err(|_| {
                "Could not read client storage. Saved preferences have been preserved.".into()
            })
    }
}
async fn write(bytes: String) -> Result<(), String> {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<tests::Probe>() {
        return probe.write(bytes).await;
    }

    #[cfg(target_arch = "wasm32")]
    {
        let storage = web_sys::window()
            .and_then(|window| window.local_storage().ok().flatten())
            .ok_or("Client storage is unavailable. Your changes remain in memory.")?;
        storage.set_item(KEY, &bytes).map_err(|_| {
            "Could not save client preferences. Your changes remain in memory; retry saving.".into()
        })
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let args = serde_json::json!({"key":KEY,"bytes":bytes});
        document::eval(&format!(
            "window.localStorage.setItem({args}.key,{args}.bytes);return true;"
        ))
        .join::<bool>()
        .await
        .map(|_| ())
        .map_err(|_| {
            "Could not save client preferences. Your changes remain in memory; retry saving.".into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::{cell::Cell, collections::VecDeque};
    #[derive(Clone)]
    pub(super) struct Probe {
        started: futures_channel::mpsc::UnboundedSender<String>,
        gates: Rc<RefCell<VecDeque<futures_channel::oneshot::Receiver<()>>>>,
        bytes: Rc<RefCell<Vec<String>>>,
        fail: Rc<Cell<bool>>,
    }
    impl Probe {
        pub async fn write(&self, bytes: String) -> Result<(), String> {
            let _ = self.started.unbounded_send(bytes.clone());
            let gate = self.gates.borrow_mut().pop_front();
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            if self.fail.replace(false) {
                return Err("quota".into());
            }
            self.bytes.borrow_mut().push(bytes);
            Ok(())
        }
    }
    #[derive(Clone)]
    struct Props {
        probe: Probe,
        state: Rc<RefCell<Option<Store<UiModel>>>>,
        writer: Rc<RefCell<Option<Writer>>>,
    }
    fn harness(props: Props) -> Element {
        use_context_provider(|| props.probe.clone());
        let state = use_store(UiModel::default);
        use_writer(state);
        let mut writer = use_context::<Writer>();
        use_hook(|| {
            let generation = writer.document.write().begin_hydration();
            writer
                .document
                .write()
                .hydrate(generation, Ok(Some(r#"{"unknown":{"keep":true}}"#)))
                .unwrap();
        });
        *props.state.borrow_mut() = Some(state);
        *props.writer.borrow_mut() = Some(writer);
        rsx! {div{"Settings owner"}}
    }
    fn fixture() -> (
        VirtualDom,
        Props,
        futures_channel::mpsc::UnboundedReceiver<String>,
    ) {
        let (tx, rx) = futures_channel::mpsc::unbounded();
        let props = Props {
            probe: Probe {
                started: tx,
                gates: Default::default(),
                bytes: Default::default(),
                fail: Default::default(),
            },
            state: Default::default(),
            writer: Default::default(),
        };
        let mut dom = VirtualDom::new_with_props(harness, props.clone());
        dom.rebuild_in_place();
        (dom, props, rx)
    }
    async fn drive<T>(dom: &mut VirtualDom, future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(3),async{futures_util::pin_mut!(future);loop{tokio::select!{value=&mut future=>return value,_=dom.wait_for_work()=>dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations)}}}).await.unwrap()
    }
    #[tokio::test(flavor = "current_thread")]
    async fn serial_close_receipt_waits_for_latest_edit_and_retry_preserves_unknown_fields() {
        let (mut dom, props, mut started) = fixture();
        let state = props.state.borrow().unwrap();
        let writer = props.writer.borrow().as_ref().unwrap().clone();
        let (tx, rx) = futures_channel::oneshot::channel();
        props.probe.gates.borrow_mut().push_back(rx);
        dom.in_scope(ScopeId::APP, || {
            writer.patch(state, serde_json::json!({"fontSizeCode":16}))
        });
        let first = drive(&mut dom, started.next()).await.unwrap();
        assert!(first.contains("unknown"));
        dom.in_scope(ScopeId::APP, || {
            writer.patch(state, serde_json::json!({"fontSizeCode":18}))
        });
        let (done, receipt) = futures_channel::oneshot::channel();
        let flush = writer.clone();
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                let _ = done.send(flush.flush().await);
            });
        });
        tx.send(()).unwrap();
        let second = drive(&mut dom, started.next()).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&second).unwrap()["fontSizeCode"],
            18
        );
        assert_eq!(drive(&mut dom, receipt).await.unwrap(), Ok(()));
        assert_eq!(*props.probe.bytes.borrow(), vec![first, second]);
        assert!(!writer.needs_flush());
        props.probe.fail.set(true);
        dom.in_scope(ScopeId::APP, || {
            writer.patch(state, serde_json::json!({"fontSizeCode":14}))
        });
        drive(&mut dom, started.next()).await;
        while writer.document.peek().write_error.is_none() {
            drive(&mut dom, std::future::ready(())).await;
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        }
        assert!(writer.needs_flush());
        let (done, receipt) = futures_channel::oneshot::channel();
        let flush = writer.clone();
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                let _ = done.send(flush.flush().await);
            });
        });
        assert_eq!(drive(&mut dom, receipt).await.unwrap(), Ok(()));
        assert!(state.client_settings_error().peek().is_none());
        assert_eq!(
            serde_json::from_str::<Value>(props.probe.bytes.borrow().last().unwrap()).unwrap()["unknown"],
            serde_json::json!({"keep":true})
        );
    }
    #[tokio::test(flavor = "current_thread")]
    async fn close_waits_for_delayed_font_probe_before_taking_preferences_receipt() {
        let (mut dom, props, mut started) = fixture();
        let state = props.state.borrow().unwrap();
        let writer = props.writer.borrow().as_ref().unwrap().clone();
        let barrier = crate::font_service::CommitBarrier::default();
        let (probe, measured) = futures_channel::oneshot::channel();
        let commit = writer.clone();
        // Registration happens synchronously at blur, even before task firstpoll.
        let pending = barrier.track(async move {
            let _ = measured.await;
            commit.patch(state, serde_json::json!({"fontFamilyCode":"Menlo"}));
        });
        assert!(barrier.pending());
        dom.in_scope(ScopeId::APP, || {
            spawn(pending);
        });
        assert!(!writer.needs_flush());
        let (done, mut receipt) = futures_channel::oneshot::channel();
        let close_barrier = barrier.clone();
        let flush = writer.clone();
        dom.in_scope(ScopeId::APP, || {
            spawn(async move {
                let _ = done.send(flush.flush_after(&close_barrier).await);
            });
        });
        dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
        assert!(receipt.try_recv().unwrap().is_none());
        assert!(props.probe.bytes.borrow().is_empty());
        probe.send(()).unwrap();
        let bytes = drive(&mut dom, started.next()).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&bytes).unwrap()["fontFamilyCode"],
            "Menlo"
        );
        assert_eq!(drive(&mut dom, receipt).await.unwrap(), Ok(()));
        assert!(!barrier.pending());
        assert!(!writer.needs_flush());
    }
}
