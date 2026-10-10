//! The quiet line after the conversation's last message for a thread that is snoozed, settled or
//! just woke, with the one way out of each. Port of `threadStatusLine` in `ChatView.tsx` and
//! `ThreadStatusLine.tsx`; the choice of line and its labels are `t3_client::thread_status`.
//!
//! The line reads the shell through a memo, so only a change to what it shows repaints it. Its one
//! timer exists while the line does: it fires at the wake of a snooze (the line then goes) and
//! otherwise once a minute, which is the resolution of the labels.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    sidebar_rows::send,
};
use chrono::Utc;
use dioxus::prelude::*;
use serde_json::json;
use t3_client::thread_status::{self as logic, Line};

type Shown = (Line, Option<(String, String)>);

#[component]
pub(crate) fn StatusLine(state: Store<UiModel>, transport: TransportHandle, thread_id: String, can_operate: bool) -> Element {
    #[cfg(test)]
    if let Some(probe) = try_consume_context::<crate::RenderProbe>() {
        probe.status_line.set(probe.status_line.get() + 1);
    }
    let mut tick = use_signal(|| 0u64);
    let busy = use_signal(|| false);
    let shown = use_memo(use_reactive((&thread_id,), move |(thread_id,)| -> Option<Shown> {
        let _ = tick();
        let (snooze, settlement) = {
            let field = state.typed_config();
            let config = field.read();
            let capabilities = config.as_ref().map(|config| &config.environment.capabilities);
            (capabilities.is_some_and(|c| c.thread_snooze == Some(true)), capabilities.is_some_and(|c| c.thread_settlement == Some(true)))
        };
        let shell = state.shell();
        let shell = shell.read();
        let thread = shell.snapshot.as_ref()?.threads.iter().find(|thread| thread.id.as_str() == thread_id)?;
        let line = logic::line(thread, snooze, settlement, Utc::now())?;
        Some((line, logic::recovery_snooze(thread)))
    }));
    // The timer's key is whether the line moves by itself; the loop sleeps to the next change.
    let moving = shown.read().as_ref().is_some_and(|(line, _)| logic::next_change_ms(line, Utc::now()).is_some());
    use_resource(use_reactive((&moving,), move |(moving,)| async move {
        while moving {
            let Some(wait) = shown.peek().as_ref().and_then(|(line, _)| logic::next_change_ms(line, Utc::now())) else { return };
            runtime::delay(wait).await;
            let next = *tick.peek() + 1;
            tick.set(next);
        }
    }));
    let Some((line, recovery)) = shown.read().clone() else { return rsx! {} };
    let thread_id_for_action = thread_id.clone();
    let (icon, label, action, working, act): (&str, String, &str, &str, Box<dyn Fn()>) = match line {
        Line::Snoozed { label, .. } => ("\u{23f0}", label, "Wake now", "Waking...", Box::new(move || match &recovery {
            Some((run_id, reset_at)) => send(&transport, state, busy, "thread.metadata.update", json!({"threadId":thread_id_for_action,"limitRecovery":{"runId":run_id,"resetAt":reset_at,"snooze":false}}), "Failed to wake thread"),
            None => send(&transport, state, busy, "thread.unsnooze", json!({"threadId":thread_id_for_action,"reason":"user"}), "Failed to wake thread"),
        })),
        Line::Settled { label } => ("\u{2713}", label, "Un-settle", "Un-settling...", Box::new(move || send(&transport, state, busy, "thread.unsettle", json!({"threadId":thread_id_for_action,"reason":"user"}), "Failed to un-settle thread"))),
        Line::Woke { at } => ("\u{23f0}", "Woke from snooze".to_owned(), "Dismiss", "Dismiss", Box::new(move || send(&transport, state, busy, "thread.visit", json!({"threadId":thread_id_for_action,"visitedAt":at}), "Could not dismiss the notification"))),
    };
    rsx! { div { class: "thread-status-line", role: "status", "data-chat-thread-status-line": "true",
        span { "aria-hidden": "true", "{icon}" }
        span { class: "thread-status-label", "{label}" }
        span { "aria-hidden": "true", "\u{b7}" }
        button { r#type: "button", class: "thread-status-action", "aria-label": "{action}", disabled: !can_operate || busy(), onclick: move |_| act(), if busy() { "{working}" } else { "{action}" } }
    } }
}
