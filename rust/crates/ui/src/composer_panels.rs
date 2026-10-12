//! What sits between the conversation and the message box: the request the agent is waiting on
//! (an approval, a question), the messages queued behind the running turn, the work still running
//! in the background, the plan ready to implement, and the context meter. Each is its own
//! component reading one narrow memo of the thread, so none of them repaints on a keystroke or
//! on text streaming into the conversation.
use crate::runtime::{self, UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{connection::ConnectionStatus, requests, timeline_items as words};

/// The request being waited on, then the questions. An approval answers once: its buttons stay
/// off from the click until the server's answer (the request leaving `pending`, an error, or the
/// connection dropping, after which the projection says what is true again).
#[component]
pub(crate) fn PendingRequests(state: Store<UiModel>, transport: runtime::TransportHandle, thread_id: String, can_operate: bool) -> Element {
    let pending = use_memo(move || state.thread().read().projection.as_ref().map(requests::pending_requests).unwrap_or_default());
    let pending = pending.read();
    let count = pending.approvals.len();
    rsx! {
        if let Some(approval) = pending.approvals.first() {
            ApprovalPanel {key:"{approval.id}",approval:approval.clone(),count,state,transport:transport.clone(),thread_id:thread_id.clone(),can_operate}
        }
        for request in pending.user_inputs.iter() {
            crate::UserInputCard {key:"{request.id}",request:request.clone(),state,transport:transport.clone(),thread_id:thread_id.clone(),can_operate}
        }
    }
}

#[component]
fn ApprovalPanel(approval: requests::PendingApproval, count: usize, state: Store<UiModel>, transport: runtime::TransportHandle, thread_id: String, can_operate: bool) -> Element {
    let mut responding = use_signal(|| false);
    let mut more = use_signal(|| false);
    // Anything that says the answer did not land, or that the connection is not the one asked:
    // the card is answerable again only if the projection still lists the request.
    use_effect(move || {
        let failed = state.error().read().is_some();
        let connected = *state.status().read() == ConnectionStatus::Connected;
        if failed || !connected {
            responding.set(false);
        }
    });
    let (heading, detail_name) = words::approval_labels(&approval.kind);
    let options: Vec<(String, String, Option<String>)> = approval
        .options
        .iter()
        .filter_map(|option| Some((option["decision"].as_str()?.to_owned(), option["label"].as_str()?.to_owned(), option["warning"].as_str().map(str::to_owned))))
        .collect();
    let (primary, others): (Vec<_>, Vec<_>) = options.into_iter().partition(|(decision, ..)| words::approval_is_primary(decision));
    let answerable = can_operate && approval.live && !responding();
    let text = if !approval.live { "Provider process is gone — interrupt or restart the run to respond.".to_owned() } else { approval.detail.clone().filter(|text| !text.is_empty()).unwrap_or_else(|| heading.to_owned()) };
    let respond = {
        let transport = transport.clone();
        let thread_id = thread_id.clone();
        let id = approval.id.clone();
        move |decision: String| {
            if responding.peek().clone() {
                return;
            }
            if runtime::command(&transport, state, "runtime-request.respond", json!({"threadId":thread_id,"requestId":id,"decision":decision})).is_some() {
                responding.set(true);
            }
        }
    };
    rsx! {
        section {class:"approval-card composer-panel",role:"group","aria-label":"{heading}","data-request-kind":"{approval.kind}",
            header {class:"approval-header",
                strong {class:"approval-heading","{heading}"}
                if let Some(app)=&approval.app_name {span {class:"muted","{app}"}}
                if count > 1 {span {class:"approval-count muted","1/{count}"}}
            }
            if approval.kind == "mcp-elicitation" {span {class:"approval-detail prose","aria-label":"{detail_name}",tabindex:"0","data-approval-detail":"complete","{text}"}}
            else {code {class:"approval-detail","aria-label":"{detail_name}",tabindex:"0","data-approval-detail":"complete","{text}"}}
            div {class:"approval-actions",
                for (decision,label,warning) in primary {
                    button {key:"{decision}","aria-label":"{label}",class:if decision=="accept"{"primary"}else{""},disabled:!answerable,title:warning.clone().unwrap_or_default(),"aria-description":warning.clone().unwrap_or_default(),
                        onclick:{let mut respond=respond.clone();move |_|respond(decision.clone())},
                        if warning.is_some() {span {"aria-hidden":"true","⚠ "}}"{label}"}
                }
                if !others.is_empty() {
                    div {class:"menu-anchor",onkeydown:move |event| {if event.key()==Key::Escape && more() {event.stop_propagation();more.set(false);}},
                        button {"aria-label":"More approval options","aria-haspopup":"menu","aria-expanded":"{more()}",disabled:!can_operate||!approval.live||responding(),onclick:move |_|more.set(!more()),"⋯"}
                        if more() {div {class:"menu",role:"menu",
                            for (decision,label,warning) in others {
                                button {key:"{decision}","aria-label":"{label}",role:"menuitem",disabled:!answerable,title:warning.clone().unwrap_or_default(),"aria-description":warning.clone().unwrap_or_default(),
                                    onclick:{let mut respond=respond.clone();move |_|{more.set(false);respond(decision.clone());}},
                                    if warning.is_some() {span {"aria-hidden":"true","⚠ "}}"{label}"}
                            }
                        }}
                    }
                }
            }
        }
    }
}

/// The run being edited in the composer, and what the box held before.
#[derive(Clone, PartialEq)]
pub(crate) struct EditingRun {
    pub thread_id: String,
    pub run_id: String,
    pub previous_draft: String,
}

/// The queue above the composer: each message waiting for the running turn, to edit in the
/// composer or remove. (Reordering, resuming and steering a queued message need server commands
/// that are not there yet.)
#[component]
pub(crate) fn QueuedRuns(state: Store<UiModel>, transport: runtime::TransportHandle, thread_id: String, can_operate: bool, editing: Signal<Option<EditingRun>>, edit_hint: Option<String>, steer_hint: Option<String>) -> Element {
    let queue = use_memo(move || state.thread().read().projection.as_ref().map(words::queued_runs).unwrap_or_default());
    let mut expanded = use_signal(|| true);
    let steerable = use_memo(move || state.thread().read().projection.as_ref().is_some_and(t3_client::queue_workflow::can_promote_to_steer));
    let steering = use_memo(move || state.relations().read().steering.clone());
    let held = use_memo(move || state.thread().read().projection.as_ref().is_some_and(|projection| projection["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["status"] == "queued" && run["queueHeld"] == true))));
    let edit_thread = thread_id.clone();
    // The edited message left the queue (delivered, removed elsewhere): the edit has nothing to save into.
    use_effect(move || {
        let queue = queue.read();
        let current = editing.peek().clone();
        if let Some(current) = current {
            if current.thread_id != edit_thread {
                // Left over from another conversation: it has nothing to restore here.
                let mut editing = editing;
                editing.set(None);
            } else if !queue.iter().any(|run| run.run_id == current.run_id) {
                cancel_edit(state, editing);
            }
        }
    });
    let queue = queue.read();
    if queue.is_empty() {
        return rsx! {};
    }
    let count = queue.len();
    let edit_thread_for_click = thread_id.clone();
    let queued_label = format!("{count} queued message{}", if count == 1 { "" } else { "s" });
    let latest = queue.last().map(|run| run.run_id.clone());
    rsx! {
        section {class:"queued-runs composer-panel","aria-label":queued_label,
            header {
                button {class:"queued-toggle","aria-label":if expanded() {"Collapse queued messages"} else {"Expand queued messages"},"aria-expanded":"{expanded()}",onclick:move |_|expanded.set(!expanded()),if expanded() {"⌄"} else {"›"}}
                strong {"Queued"}
                span {class:"muted","{count}"}
                if held() {button {class:"queued-resume","aria-label":"Resume thread",disabled:!can_operate,title:"The queue is held after a stop. Resume to deliver the queued messages.",
                    onclick:{let transport=transport.clone();let thread_id=thread_id.clone();move |_|{runtime::command(&transport,state,"queue.resume",json!({"threadId":thread_id}));}},"Resume"}}
            }
            if expanded() {ul {
                for (position,run) in queue.iter().enumerate() {{
                    let is_editing = editing.read().as_ref().is_some_and(|current| current.run_id == run.run_id);
                    let attachments = run.attachments.len();
                    rsx!{li {key:"{run.run_id}","aria-current":if is_editing {"true"} else {"false"},class:if is_editing {"editing"} else {""},
                        button {class:"queued-handle","aria-label":"Reorder queued message (drag, or press the arrow keys)",disabled:!can_operate||editing.read().is_some()||count<2,
                            onkeydown:{let transport=transport.clone();let thread_id=thread_id.clone();let run_id=run.run_id.clone();let ids:Vec<String>=queue.iter().map(|run|run.run_id.clone()).collect();move |event: KeyboardEvent| {
                                let before = match event.key() {
                                    Key::ArrowUp if position>0 => Some(Some(ids[position-1].clone())),
                                    Key::ArrowDown if position+1<ids.len() => Some(ids.get(position+2).cloned()),
                                    _ => None,
                                };
                                if let Some(before)=before {event.prevent_default();runtime::command(&transport,state,"queued-run.reorder",json!({"threadId":thread_id,"runId":run_id,"beforeRunId":before}));}
                            }},"⠿"}
                        span {class:"queued-text",title:"{run.text}",
                            if is_editing {span {class:"sr-only","Editing queued message: "}}
                            "{run.text}"
                        }
                        if attachments > 0 {span {class:"muted",{format!("{attachments} attachment{}",if attachments==1{""}else{"s"})}}}
                        if is_editing {
                            button {"aria-label":"Cancel editing queued message",onclick:move |_|cancel_edit(state,editing),"Cancel"}
                        } else {
                            button {"aria-label":"Edit queued message",disabled:!can_operate||editing.read().is_some(),title:{let hint=if latest.as_deref()==Some(run.run_id.as_str()) {edit_hint.clone()} else {None};format!("Edit in the composer{}",hint.map(|hint|format!(" ({hint})")).unwrap_or_default())},
                                onclick:{let (id,text,thread)=(run.run_id.clone(),run.text.clone(),edit_thread_for_click.clone());move |_|begin_edit(state,editing,thread.clone(),id.clone(),text.clone())},"✎"}
                            button {"aria-label":"Steer queued message",class:"queued-steer",disabled:!can_operate||editing.read().is_some()||!steerable()||steering().is_some(),"aria-busy":"{steering().as_deref()==Some(run.run_id.as_str())}",
                                title:if !steerable() {"There is no running turn to steer".to_owned()} else {format!("Send as a steer instead{}",if position==0 {steer_hint.clone().map(|hint|format!(" ({hint})")).unwrap_or_default()} else {String::new()})},
                                onclick:{let transport=transport.clone();let thread_id=thread_id.clone();let id=run.run_id.clone();move |_|steer(&transport,state,&thread_id,&id)},"Steer"}
                            button {"aria-label":"Remove queued message",disabled:!can_operate||editing.read().is_some(),title:"Remove from queue",
                                onclick:{let transport=transport.clone();let thread_id=thread_id.clone();let id=run.run_id.clone();move |_|{runtime::command(&transport,state,"queued-run.cancel",json!({"threadId":thread_id,"runId":id}));}},"✕"}
                        }
                    }}
                }}
            }}
        }
    }
}

/// Sends a queued message into the turn that is running (`queued-message.promote-to-steer`). One at
/// a time: the button waits for the server's answer, and a refusal is reported.
pub(crate) fn steer(transport: &runtime::TransportHandle, state: Store<UiModel>, thread_id: &str, queued_run: &str) {
    let target = {
        let model = state.peek();
        let Some(projection) = model.thread.projection.as_ref().filter(|projection| projection["thread"]["id"] == thread_id) else { return };
        if !t3_client::queue_workflow::can_promote_to_steer(projection) {
            return;
        }
        t3_client::queue_workflow::active_run(projection).and_then(|run| run["id"].as_str()).map(str::to_owned)
    };
    let Some(target) = target else { return };
    if state.relations().peek().steering.is_some() {
        return;
    }
    let mut payload = t3_client::queue_workflow::promote_command(thread_id, queued_run, &target);
    payload["commandId"] = json!(uuid::Uuid::new_v4().to_string());
    state.relations().write().steering = Some(queued_run.to_owned());
    let owner = runtime::response_owner(transport, state);
    let transport = transport.clone();
    dioxus::dioxus_core::spawn_forever(async move {
        let result = runtime::request_value_reported(transport.clone(), state, "orchestration.dispatchCommand", payload).await;
        state.relations().write().steering = None;
        if owner.is_some() && owner == runtime::response_owner(&transport, state) {
            if let Err(message) = result {
                crate::notice::push(state, crate::notice::Level::Error, "Could not steer", if message.trim().is_empty() { "Failed to send the queued message as a steer.".to_owned() } else { message });
            }
        }
    });
}

/// Puts a queued message in the composer (its attachments stay on the message; the edit replaces
/// the text), remembering what was being written.
pub(crate) fn begin_edit(state: Store<UiModel>, mut editing: Signal<Option<EditingRun>>, thread_id: String, run_id: String, text: String) {
    if editing.peek().is_some() {
        return;
    }
    let previous = state.draft().peek().clone();
    editing.set(Some(EditingRun { thread_id, run_id, previous_draft: previous }));
    crate::draft_storage::edit_thread_prompt(state, text);
}
pub(crate) fn cancel_edit(state: Store<UiModel>, mut editing: Signal<Option<EditingRun>>) {
    let current = editing.peek().clone();
    if let Some(current) = current {
        crate::draft_storage::edit_thread_prompt(state, current.previous_draft);
        editing.set(None);
    }
}

/// A native `/goal`, which keeps the agent going across turns: its title and objective above the composer, and once the thread
/// is idle the native follow-ups (resume, clear), which are sent as the commands they are.
#[component]
pub(crate) fn GoalStrip(state: Store<UiModel>, transport: runtime::TransportHandle, thread_id: String, can_operate: bool) -> Element {
    let goal = use_memo({
        let thread_id = thread_id.clone();
        move || {
            let shell = state.shell();
            let shell = shell.read();
            let thread = shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id))?;
            let goal = serde_json::to_value(thread.goal.clone().flatten()?).ok()?;
            Some((goal, thread.status.is_active()))
        }
    });
    let Some((goal, working)) = goal.read().clone() else { return rsx! {} };
    let shown = t3_client::provider_goal::present(&goal, working);
    let title = match &shown.usage {
        Some(usage) => format!("{} \u{b7} {usage}", shown.title),
        None => shown.title.clone(),
    };
    let command = move |text: &'static str| {
        let (transport, state) = (transport.clone(), state);
        move |_| runtime::send_with(&transport, state, runtime::SendOptions { text: Some(text.to_owned()), mode: Some("auto"), ..Default::default() })
    };
    rsx! { crate::composer_banners::Item { priority: t3_client::banner_stack::Priority::Activity, section {class:"goal-strip composer-panel",role:"status","aria-label":"Goal",
        span {class:if working {"status-dot busy"} else {"status-dot"},"aria-hidden":"true"}
        strong {"{title}"}
        span {class:"muted goal-objective","{shown.objective}"}
        if !working {
            if shown.can_resume { button {"aria-label":"Resume goal",disabled:!can_operate,onclick:command("/goal resume"),"Resume"} }
            button {"aria-label":"Clear goal",disabled:!can_operate,onclick:command("/goal clear"),"Clear"}
        }
    }}}
}

/// What a settled thread still runs, with Stop. The thread is not "working": a dev server can run
/// for hours after the agent is done.
#[component]
pub(crate) fn BackgroundStrip(state: Store<UiModel>, transport: runtime::TransportHandle, thread_id: String, can_operate: bool) -> Element {
    let work = use_memo({
        let thread_id = thread_id.clone();
        move || {
            let shell = state.shell();
            let shell = shell.read();
            let thread = shell.snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id))?;
            let tasks = serde_json::to_value(&thread.pending_background_tasks).ok()?;
            words::background_work(tasks.as_array()?)
        }
    });
    // Shown for settled threads only, as the original does; a running turn has its own indicator.
    let working = use_memo({
        let thread_id = thread_id.clone();
        move || state.shell().read().snapshot.as_ref().and_then(|snapshot| snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)).is_some_and(|thread| thread.status.is_active())
    });
    let mut stopping = use_signal(|| false);
    use_effect(move || {
        // A settled thread without work, an error, or a dropped connection ends the wait.
        let done = work.read().is_none() || state.error().read().is_some() || *state.status().read() != ConnectionStatus::Connected;
        if done {
            stopping.set(false);
        }
    });
    let Some(work) = work.read().clone().filter(|_| !working()) else {
        return rsx! {};
    };
    let detail = (work.items.len() > 1 || work.items.iter().any(|item| item.child_thread_id.is_some())).then(|| work.items.iter().map(|item| item.label.clone()).collect::<Vec<_>>().join(", "));
    let strip_label = "Background work";
    rsx! { crate::composer_banners::Item { priority: t3_client::banner_stack::Priority::Activity, section {class:"background-strip composer-panel",role:"status","aria-label":strip_label,
        span {class:if work.waiting {"status-dot busy"} else {"status-dot"},"aria-hidden":"true"}
        strong {"{work.title}"}
        if let Some(detail)=detail {span {class:"muted","{detail}"}}
        button {"aria-label":"Stop background work",disabled:!can_operate||stopping(),onclick:move |_|{
            if *stopping.peek() {return;}
            if stop_background_work(&transport,state,&thread_id) {stopping.set(true);}
        },if stopping() {"Stopping..."} else {"Stop"}}
    }}}
}

/// Stop for the thread's latest work: the active run, or the latest run when only background work
/// is left (`interruptThreadTurn`). True when a stop was sent.
pub(crate) fn stop_background_work(transport: &runtime::TransportHandle, state: Store<UiModel>, thread_id: &str) -> bool {
    let run = state.peek().thread.projection.as_ref().and_then(|projection| {
        let runs = projection["runs"].as_array()?;
        runs.iter().rev().find(|run| matches!(run["status"].as_str(), Some("preparing" | "starting" | "running" | "waiting"))).or_else(|| runs.last()).and_then(|run| run["id"].as_str()).map(str::to_owned)
    });
    match run {
        Some(run) => runtime::command(transport, state, "run.interrupt", json!({"threadId":thread_id,"runId":run,"holdQueue":true})).is_some(),
        // A thread that never ran can still be watching a pull request.
        None => runtime::unwatch_pull_requests(transport, state, thread_id),
    }
}

/// "Plan ready" above the box while a plan waits to be implemented or refined.
#[component]
pub(crate) fn PlanReady(plan: words::ProposedPlan) -> Element {
    let title = words::plan_title(&plan.markdown);
    rsx! {section {class:"plan-ready composer-panel",role:"status",strong {"Plan ready"}if let Some(title)=title {span {class:"muted","{title}"}}}}
}

/// The context meter: a ring that fills as the window does, and the numbers behind it.
#[component]
pub(crate) fn ContextMeter(state: Store<UiModel>) -> Element {
    // Hidden unless the reader turned the legacy indicator on (`contextWindowMeterEnabled`).
    if !state.client_settings().read().context_window_meter_enabled {
        return rsx! {};
    }
    let window = use_memo(move || state.thread().read().projection.as_ref().and_then(words::projection_context_window));
    let mut open = use_signal(|| false);
    let Some(window) = window.read().clone() else {
        return rsx! {};
    };
    let percentage = words::format_percentage(window.used_percentage);
    let used = words::format_context_tokens(Some(window.used_tokens));
    let label = match (&percentage, window.max_tokens) {
        (Some(percentage), Some(_)) => format!("Context window {percentage} used"),
        _ => format!("Context window {used} tokens used"),
    };
    let fill = window.used_percentage.unwrap_or(0.).clamp(0., 100.);
    let hot = fill > 90.;
    let circumference = 2. * std::f64::consts::PI * 9.75;
    let offset = circumference * (1. - fill / 100.);
    rsx! {div {class:"context-meter",onkeydown:move |event| {if event.key()==Key::Escape && open() {event.stop_propagation();open.set(false);}},
        button {class:"icon-button","aria-label":"{label}","aria-expanded":"{open()}",onclick:move |_|open.set(!open()),onmouseenter:move |_|open.set(true),
            svg {view_box:"0 0 24 24",width:"20",height:"20","aria-hidden":"true",style:"transform:rotate(-90deg)",
                circle {cx:"12",cy:"12",r:"9.75",fill:"none",stroke:"currentColor","stroke-opacity":"0.24","stroke-width":"3"}
                circle {cx:"12",cy:"12",r:"9.75",fill:"none",stroke:if hot {"var(--destructive, #dc2626)"} else {"currentColor"},"stroke-width":"3","stroke-linecap":"round","stroke-dasharray":"{circumference}","stroke-dashoffset":"{offset}"}
            }
        }
        if open() {div {class:"context-popover",role:"dialog","aria-label":"Context window",onmouseleave:move |_|open.set(false),
            div {class:"context-row",strong {"Context Window"}
                if let (Some(percentage),Some(max))=(&percentage,window.max_tokens) {span {"{percentage} · {used}/{words::format_context_tokens(Some(max))}"}} else {span {"{used}"}}
            }
            if window.max_tokens.is_some() {div {class:"context-bar",role:"progressbar","aria-valuemin":"0","aria-valuemax":"100","aria-valuenow":"{fill.round()}","aria-label":"Context window usage",div {style:"width:{fill}%"}}}
            if let Some(total)=window.total_processed_tokens.filter(|total|*total>0.) {div {class:"context-row",span {"Total processed"}span {"{words::format_context_tokens(Some(total))}"}}}
            if let Some(cost)=&window.cost {div {class:"context-row",span {"Cost"}span {"{words::format_context_cost(cost.amount,&cost.currency)}"}}}
            if window.compacts_automatically == Some(true) {p {class:"muted",{words::format_compaction_message(None,window.auto_compact_threshold)}}}
        }}
    }}
}

/// "Working for 12s" at the end of the conversation while the run lasts. Only this component
/// repaints each second; the conversation above never does, and it is gone with the run.
#[component]
pub(crate) fn WorkingRow(started: Option<String>) -> Element {
    let mut now = use_signal(chrono::Utc::now);
    use_future(move || async move {
        loop {
            runtime::delay(1000).await;
            now.set(chrono::Utc::now());
        }
    });
    let label = started.as_deref().and_then(|started| words::working_elapsed(started, now())).map(|elapsed| format!("Working for {elapsed}")).unwrap_or_else(|| "Working...".into());
    rsx! {div {class:"working-row",role:"status","data-timeline-working":"true","{label}"}}
}

/// Whether a value of the projection is the settled latest run, for the plan banner.
pub(crate) fn latest_run_settled(projection: &Value) -> Option<String> {
    let run = projection["runs"].as_array()?.last()?;
    matches!(run["status"].as_str(), Some("completed" | "failed" | "cancelled" | "interrupted")).then(|| run["id"].as_str().unwrap_or_default().to_owned())
}

/// The refusal of a send whose prompt is too long (`ComposerPromptLengthValidation`). It appears when a send is refused and
/// follows the prompt from then on: it says by how much it is still over, and goes once it is not. `text` is the prompt of a
/// composer that keeps it elsewhere than in the thread draft.
#[component]
pub(crate) fn PromptLength(state: Store<UiModel>, text: Option<String>) -> Element {
    // Nothing is read of the prompt until there is something to say about it.
    let refused = state.prompt_length_error().read().is_some();
    let now = refused.then(|| t3_client::composer_submission::prompt_length_message(&text.unwrap_or_else(|| state.draft().read().clone()))).flatten();
    use_effect(use_reactive(&(refused, now.is_none()), move |(refused, fine)| {
        if refused && fine {
            state.prompt_length_error().set(None);
        }
    }));
    match now {
        Some(message) => rsx! { p { class: "error-banner", role: "alert", "data-chat-composer-validation": "prompt-length", "{message}" } },
        None => rsx! {},
    }
}
