//! Tells the server the window is visible, focused or idle (`server.reportClientActivity`) so its background policy
//! knows when periodic work may run (`apps/web/src/lib/backgroundActivityReporter.ts`; the rules are in
//! `t3_client::background_activity`). The page senses the window and is otherwise dumb; this component owns the timing:
//! a report after any change, debounced, and one every 25 seconds so the 45 second lease never lapses.
use crate::runtime::{self, TransportHandle, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use futures_util::StreamExt;
use serde_json::Value;
use std::{cell::RefCell, rc::Rc};
use t3_client::background_activity::{self as logic, Observation};
use t3_client::connection::ConnectionStatus;
use t3_contracts::AuthEnvironmentScope;

thread_local! {
    static RETAINED: RefCell<logic::RetainedScopes> = RefCell::new(logic::RetainedScopes::default());
    static CHANGED: RefCell<Option<futures_channel::mpsc::UnboundedSender<()>>> = const { RefCell::new(None) };
}

fn scopes_changed() {
    CHANGED.with(|slot| {
        if let Some(sender) = slot.borrow().as_ref() {
            let _ = sender.unbounded_send(());
        }
    });
}

/// A stream's hold on a scope, released when the stream is dropped.
pub(crate) struct ScopeLease(String);
impl Drop for ScopeLease {
    fn drop(&mut self) {
        if RETAINED.with(|retained| retained.borrow_mut().release(&self.0)) {
            scopes_changed();
        }
    }
}

/// The lease a stream of `method` holds on `environment` for as long as it runs, if it holds one.
pub(crate) fn retain_for_stream(environment: &str, method: &str, payload: &Value) -> Option<ScopeLease> {
    let scope = logic::scope_for_subscription(method, payload)?;
    let (key, first) = RETAINED.with(|retained| retained.borrow_mut().retain(environment, scope));
    if first {
        scopes_changed();
    }
    Some(ScopeLease(key))
}

#[cfg(test)]
pub(crate) fn retained_scopes(environment: &str) -> Vec<Value> {
    RETAINED.with(|retained| retained.borrow().scopes_of(environment))
}

/// What feeds the reporter instead of the page, for a test that has no page to sense.
#[derive(Clone)]
pub(crate) struct ObservationFeed(pub Rc<RefCell<Option<futures_channel::mpsc::UnboundedReceiver<Observation>>>>);

/// The page's sensor: one message at the start and after every change of visibility, focus, network and the first input
/// after a quiet spell, and one at the report interval. `idleMs` is the time since the last input.
const SENSOR: &str = r#"
const KEY = "t3.backgroundActivity.clientId";
const WINDOW = 45000;
let clientId = "ephemeral-browser-client";
try {
  clientId = window.localStorage.getItem(KEY) || "";
  if (!clientId) { clientId = (crypto.randomUUID ? crypto.randomUUID() : String(Math.random()).slice(2)); window.localStorage.setItem(KEY, clientId); }
} catch (error) { clientId = "ephemeral-browser-client"; }
let last = Date.now();
const send = () => dioxus.send({ clientId, visible: document.visibilityState === "visible", focused: document.hasFocus(), idleMs: Date.now() - last });
const input = () => { const recent = Date.now() - last <= WINDOW; last = Date.now(); if (!recent) send(); };
const passive = { passive: true };
document.addEventListener("visibilitychange", send);
window.addEventListener("focus", send);
window.addEventListener("blur", send);
window.addEventListener("online", send);
window.addEventListener("pointermove", input);
window.addEventListener("keydown", input);
window.addEventListener("wheel", input, passive);
window.addEventListener("touchstart", input, passive);
const timer = setInterval(send, 25000);
send();
try { await dioxus.recv(); } finally {
  clearInterval(timer);
  document.removeEventListener("visibilitychange", send);
  window.removeEventListener("focus", send);
  window.removeEventListener("blur", send);
  window.removeEventListener("online", send);
  window.removeEventListener("pointermove", input);
  window.removeEventListener("keydown", input);
  window.removeEventListener("wheel", input);
  window.removeEventListener("touchstart", input);
}
"#;

fn client_kind() -> &'static str {
    if cfg!(feature = "desktop") { "desktop-renderer" } else { "web" }
}

/// Sends one report for the connected environment, as the original does for each registered environment, and ignores
/// the answer: a refused or lost report is simply sent again at the next trigger.
pub(crate) async fn send_report(transport: &TransportHandle, state: Store<UiModel>, observation: &Observation) {
    let (environment, allowed) = {
        let model = state.peek();
        let Some(environment) = model.destination.clone() else { return };
        let allowed = model.status == ConnectionStatus::Connected && model.environments.allows(&environment, AuthEnvironmentScope::OrchestrationRead);
        (environment.to_string(), allowed)
    };
    if !allowed {
        return;
    }
    let retained = RETAINED.with(|retained| retained.borrow().scopes_of(&environment));
    let report = logic::report(&environment, observation, client_kind(), &retained, chrono::Utc::now().timestamp_millis());
    let _ = runtime::request_value_reported(transport.clone(), state, logic::METHOD, report).await;
}

/// Reports for as long as it is mounted; draws nothing.
#[component]
pub(crate) fn Reporter(state: Store<UiModel>, transport: TransportHandle) -> Element {
    // Held scopes and a new connection are both reasons to report; they arrive on one channel.
    let reasons = use_hook(|| {
        let (sender, receiver) = futures_channel::mpsc::unbounded::<()>();
        CHANGED.with(|slot| *slot.borrow_mut() = Some(sender));
        Rc::new(RefCell::new(Some(receiver)))
    });
    use_drop(|| CHANGED.with(|slot| *slot.borrow_mut() = None));
    let connection = use_memo(move || (state.destination().read().clone(), *state.status().read() == ConnectionStatus::Connected));
    use_effect(move || {
        let _ = connection();
        scopes_changed();
    });
    use_future(move || {
        let transport = transport.clone();
        let reasons = reasons.clone();
        async move {
            let Some(mut reasons) = reasons.borrow_mut().take() else { return };
            let mut observations: futures_channel::mpsc::UnboundedReceiver<Observation> = match try_consume_context::<ObservationFeed>().and_then(|feed| feed.0.borrow_mut().take()) {
                Some(feed) => feed,
                None => {
                    let (sender, receiver) = futures_channel::mpsc::unbounded();
                    spawn(async move {
                        let mut sensor = document::eval(SENSOR);
                        while let Ok(message) = sensor.recv::<Value>().await {
                            if let Some(observation) = Observation::parse(&message) {
                                let _ = sender.unbounded_send(observation);
                            }
                        }
                    });
                    receiver
                }
            };
            let mut latest: Option<Observation> = None;
            loop {
                {
                    let sensor = observations.next();
                    let reason = reasons.next();
                    futures_util::pin_mut!(sensor, reason);
                    match futures_util::future::select(sensor, reason).await {
                        futures_util::future::Either::Left((Some(observation), _)) => latest = Some(observation),
                        futures_util::future::Either::Right((Some(()), _)) => {}
                        // A closed source never reports again; the other one may.
                        futures_util::future::Either::Left((None, _)) | futures_util::future::Either::Right((None, _)) => return,
                    }
                }
                // Triggers that arrive together are answered by one report.
                runtime::delay(logic::DEBOUNCE_MS).await;
                while let Ok(observation) = observations.try_recv() {
                    latest = Some(observation);
                }
                while reasons.try_recv().is_ok() {}
                if let Some(observation) = latest.as_ref() {
                    send_report(&transport, state, observation).await;
                }
            }
        }
    });
    rsx! {}
}
